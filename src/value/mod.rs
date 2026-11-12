//! Values on screen (spec "Values (P2)", "Key detail (P2)"): the one renderer every
//! screen that shows a value uses — key values, hash fields, stream entries, Pub/Sub
//! messages, Search results, Monitor lines and Slow log arguments.
//!
//! Pure code: no IO, no clock. Three rules hold for everything here:
//!
//! - **On screen only.** A value is held as [`Raw`], whose `Debug` prints only its
//!   length, and which has no `Serialize`. Nothing here formats a value into an error,
//!   a log line or a snapshot. [`Rendered`] and [`Decoded`] redact their `Debug` too.
//! - **Terminal safe.** Every string this module returns for drawing has gone through
//!   [`escape`]: control characters, escape sequences and bidirectional overrides are
//!   drawn as escapes (`\x1b`), never sent to the terminal raw.
//! - **Bounded.** Decompression stops at [`MAX_DECOMPRESSED`] bytes, and the renderers
//!   at [`MAX_LINES`] lines, whatever the input.
//!
//! The flow: [`decode`] undoes compression (gzip, zlib, LZ4 frame, zstd) and detects
//! the format (JSON, MessagePack, UTF-8 text, otherwise hex); [`render`] draws it in the
//! detected format or the one `f` picked; [`preview`] is the one-line form for list
//! cells; [`copy_text`] is what `y` copies.

use std::fmt;
use std::fmt::Write as _;

mod compress;
mod json;
mod lines;
mod memo;
#[cfg(test)]
mod tests;

/// Test helper: `text` (some `Debug`, notice or snapshot output) holds `mark` in any
/// form a value could leak in: as text, as the decimal list a derived `Debug` of its
/// bytes prints, or as hex digits.
#[cfg(test)]
pub(crate) fn leaks(text: &str, mark: &str) -> bool {
    let b = mark.as_bytes();
    let decimal = b.iter().map(u8::to_string).collect::<Vec<_>>().join(", ");
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    text.contains(mark) || text.contains(&decimal) || text.contains(&hex)
}

use lines::Sink;
pub use memo::Memo;

/// The most bytes a decompression produces. Anything past it is dropped and
/// [`Decoded::truncated`] is set, so a small compressed value can't fill memory.
pub const MAX_DECOMPRESSED: usize = 1024 * 1024;

/// The most lines [`render`] returns. Past it the last line says how many were left
/// out.
pub const MAX_LINES: usize = 10_000;

/// The widest escape [`escape`] draws one character as (`\u{202e}`). A line from
/// [`render`] is at most `max(width, MAX_ESCAPE_WIDTH)` columns wide: an escape is never
/// split across lines, so one wider than `width` gets a line of its own.
pub const MAX_ESCAPE_WIDTH: usize = 8;

/// A value read from the server: bytes, shown on screen and nowhere else.
///
/// Its `Debug` prints the length only (`Raw(<12 bytes>)`), so a value never reaches
/// `Debug` output, logs or test failure messages by accident. It has no `Serialize` on
/// purpose: values never go into snapshots or reports. Keep both that way.
#[derive(Clone, Default)]
pub struct Raw {
    bytes: Vec<u8>,
    /// Tells this value apart for [`Memo`]: set once when it is made, never 0 for a
    /// value made from bytes, kept by clones. A `Raw` never changes after it is made,
    /// so the same id always means the same bytes.
    id: u64,
}

/// The next [`Raw`] id. Starts at 1: 0 is the empty `Raw::default()`.
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Raw {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// See [`Memo`].
    fn id(&self) -> u64 {
        self.id
    }
}

/// Equal when the bytes are: the id only names one copy for [`Memo`].
impl PartialEq for Raw {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for Raw {}

impl From<Vec<u8>> for Raw {
    fn from(b: Vec<u8>) -> Self {
        Self::new(b)
    }
}

impl From<&[u8]> for Raw {
    fn from(b: &[u8]) -> Self {
        Self::new(b.to_vec())
    }
}

impl From<&str> for Raw {
    fn from(s: &str) -> Self {
        Self::new(s.as_bytes().to_vec())
    }
}

impl fmt::Debug for Raw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Raw(<{} bytes>)", self.bytes.len())
    }
}

/// How a value is drawn. `f` steps through them in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    Text,
    Hex,
    Json,
    MsgPack,
}

impl Format {
    pub const ALL: [Format; 4] = [Format::Text, Format::Hex, Format::Json, Format::MsgPack];

    /// For the panel title and the notice: `text`, `hex`, `JSON`, `MessagePack`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Hex => "hex",
            Self::Json => "JSON",
            Self::MsgPack => "MessagePack",
        }
    }

    /// The next format for `f`: text → hex → JSON → MessagePack → text.
    pub fn next(self) -> Self {
        match self {
            Self::Text => Self::Hex,
            Self::Hex => Self::Json,
            Self::Json => Self::MsgPack,
            Self::MsgPack => Self::Text,
        }
    }
}

/// A compression [`decode`] undid. Recognised by its magic bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Compression {
    Gzip,
    Zlib,
    Lz4,
    Zstd,
}

impl Compression {
    /// For the panel: `gzip`, `zlib`, `LZ4`, `zstd`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Zlib => "zlib",
            Self::Lz4 => "LZ4",
            Self::Zstd => "zstd",
        }
    }
}

/// A value after decompression, with its detected format. `Debug` leaves the bytes out.
#[derive(Clone, PartialEq)]
pub struct Decoded {
    /// The compression that was undone, if any. `None` also when the bytes looked
    /// compressed but did not decompress: they are then shown as they are.
    pub compression: Option<Compression>,
    /// Decompression stopped at [`MAX_DECOMPRESSED`].
    pub truncated: bool,
    /// What the (decompressed) bytes look like: JSON, MessagePack, text or hex.
    pub detected: Format,
    bytes: Vec<u8>,
}

impl Decoded {
    /// The bytes after decompression.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl fmt::Debug for Decoded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Decoded")
            .field("compression", &self.compression)
            .field("truncated", &self.truncated)
            .field("detected", &self.detected)
            .field("len", &self.bytes.len())
            .finish()
    }
}

/// Undoes compression and detects the format (spec "Formats"): gzip, zlib, LZ4 frame and
/// zstd first; then JSON, MessagePack, UTF-8 text, otherwise hex.
///
/// Only one layer of compression is undone. Bytes that start with a compression's
/// magic but do not decompress are kept as they are, with `compression: None`.
/// Detection: JSON when the bytes (trimmed of ASCII whitespace) are one complete JSON
/// object or array (a bare number, string or word is text); MessagePack when they are
/// not UTF-8 and are exactly one MessagePack map or array; text when they are UTF-8
/// (when decompression was cut, an incomplete character at the end is allowed);
/// otherwise hex.
pub fn decode(raw: &[u8]) -> Decoded {
    let (compression, bytes, truncated) = match compress::decompress(raw) {
        Some((c, bytes, truncated)) => (Some(c), bytes, truncated),
        None => (None, raw.to_vec(), false),
    };
    let detected = detect(&bytes, truncated);
    Decoded {
        compression,
        truncated,
        detected,
        bytes,
    }
}

fn detect(bytes: &[u8], truncated: bool) -> Format {
    match std::str::from_utf8(bytes) {
        Ok(s) if json::is_json_document(s) => Format::Json,
        Ok(_) => Format::Text,
        Err(_) if json::is_msgpack_container(bytes) => Format::MsgPack,
        // Cut in the middle of a character by the decompression cap.
        Err(e) if truncated && e.error_len().is_none() => Format::Text,
        Err(_) => Format::Hex,
    }
}

/// The format `d` is drawn or copied in when `format` is asked for, and the note when
/// that is not `format`. Text and hex always work; JSON needs one complete JSON value
/// (any, not only an object or array), MessagePack exactly one MessagePack value.
fn resolve(d: &Decoded, format: Format) -> (Format, Option<&'static str>) {
    let utf8 = std::str::from_utf8(&d.bytes).ok();
    let ok = match format {
        Format::Text | Format::Hex => true,
        Format::Json => utf8.is_some_and(json::is_json),
        Format::MsgPack => json::msgpack_value(&d.bytes).is_some(),
    };
    if ok {
        return (format, None);
    }
    let note = match (format, utf8.is_some()) {
        (Format::Json, true) => "not valid JSON, shown as text",
        (Format::Json, false) => "not valid JSON, shown as hex",
        (_, true) => "not valid MessagePack, shown as text",
        (_, false) => "not valid MessagePack, shown as hex",
    };
    let used = if utf8.is_some() {
        Format::Text
    } else {
        Format::Hex
    };
    (used, Some(note))
}

/// A value drawn in one format: lines that are safe to draw. `Debug` leaves them out.
#[derive(Clone, PartialEq)]
pub struct Rendered {
    /// The format the lines are in. Not always the one asked for: see `note`.
    pub format: Format,
    /// Why the format asked for could not be used and what was used instead, in
    /// redis-lens's own words, e.g. "not valid JSON, shown as hex".
    pub note: Option<&'static str>,
    lines: Vec<String>,
}

impl Rendered {
    /// Every line has gone through [`escape`]: no control characters.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }
}

impl fmt::Debug for Rendered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rendered")
            .field("format", &self.format)
            .field("note", &self.note)
            .field("lines", &self.lines.len())
            .finish()
    }
}

/// Draws `d` as `format` in lines at most `width` display columns wide (at least 1;
/// longer lines are wrapped), over as many lines as it needs (at most [`MAX_LINES`]):
/// - text: the UTF-8 text split at newlines, invalid bytes as `\xNN`;
/// - hex: offset, hex and the printable ASCII beside it, 16, 8 or 4 bytes a line,
///   the most that fit in `width`;
/// - JSON: pretty-printed with two-space indents;
/// - MessagePack: decoded and drawn like pretty JSON.
///
/// When `d` isn't valid in `format` (JSON asked for bytes that are not JSON, say), it
/// is drawn as text if it is UTF-8, else as hex, and [`Rendered::note`] says so.
///
/// Every line has gone through [`escape`], and is at most `max(width,`
/// [`MAX_ESCAPE_WIDTH`]`)` columns wide. Hex of an empty value has no lines.
pub fn render(d: &Decoded, format: Format, width: usize) -> Rendered {
    let (used, note) = resolve(d, format);
    let mut sink = Sink::new(width);
    match used {
        Format::Text => {
            for line in d.bytes.split(|&b| b == b'\n') {
                sink.bytes(line);
                sink.break_line();
            }
        }
        Format::Hex => {
            let n = lines::hex_bytes_per_line(width);
            for (k, chunk) in d.bytes.chunks(n).enumerate() {
                sink.str(&lines::hex_line(k * n, chunk, n));
                sink.break_line();
            }
        }
        Format::Json => {
            // `resolve` checked it is UTF-8 JSON.
            let s = std::str::from_utf8(&d.bytes).unwrap_or("");
            pretty_lines(s, &mut sink);
        }
        Format::MsgPack => {
            if let Some(v) = json::msgpack_value(&d.bytes) {
                let mut s = String::new();
                json::msgpack_to_json(&v, &mut s, usize::MAX);
                pretty_lines(&s, &mut sink);
            }
        }
    }
    Rendered {
        format: used,
        note,
        lines: sink.finish(),
    }
}

/// Pretty JSON into `sink`, a line per line, with two-space indents.
fn pretty_lines(json_text: &str, sink: &mut Sink) {
    struct Pretty<'a>(&'a mut Sink);
    impl json::JsonOut for Pretty<'_> {
        fn text(&mut self, s: &str) {
            self.0.str(s);
        }
        fn newline(&mut self, depth: usize) {
            self.0.break_line();
            self.0.spaces(2 * depth);
        }
    }
    json::reformat(json_text, true, &mut Pretty(sink));
    sink.break_line();
}

/// Compact one-line JSON for `json_text`, cut once longer than `limit` bytes.
fn compact_json(json_text: &str, limit: usize) -> String {
    let mut out = json::Compact {
        out: String::new(),
        limit,
    };
    json::reformat(json_text, false, &mut out);
    out.out
}

/// Compact one-line JSON for a MessagePack value, cut once longer than `limit` bytes.
fn compact_msgpack(bytes: &[u8], limit: usize) -> Option<String> {
    let v = json::msgpack_value(bytes)?;
    let mut s = String::new();
    json::msgpack_to_json(&v, &mut s, limit);
    // Through the reformatter for the same escapes as JSON gets.
    Some(compact_json(&s, limit))
}

fn hex_digits(bytes: &[u8], sep: &str) -> String {
    let mut s = String::with_capacity(bytes.len() * (2 + sep.len()));
    for (k, b) in bytes.iter().enumerate() {
        if k > 0 {
            s.push_str(sep);
        }
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// One line for a list cell: the value in its detected format, compact (JSON and
/// MessagePack on one line, text with newlines as `\n`, hex as pairs of digits), at
/// most `max_chars` characters with `…` at the end when cut. Terminal safe.
///
/// Text and hex convert only as much of the value as fits; JSON is still checked
/// whole, to detect it.
pub fn preview(raw: &[u8], max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let d = decode(raw);
    // Enough input or output bytes for more than `max_chars` characters: a character
    // is at most 4 bytes, and every byte of input gives at least a character.
    let limit = max_chars.saturating_add(1).saturating_mul(4);
    let s = match d.detected {
        Format::Json => escape(&compact_json(
            std::str::from_utf8(&d.bytes).unwrap_or(""),
            limit,
        )),
        Format::MsgPack => escape(&compact_msgpack(&d.bytes, limit).unwrap_or_default()),
        Format::Text => {
            let prefix = &d.bytes[..d.bytes.len().min(limit)];
            let mut s = String::new();
            for chunk in prefix.utf8_chunks() {
                for c in chunk.valid().chars() {
                    match escaped_len(c) {
                        Some(_) => push_escaped(c, &mut s),
                        None => s.push(c),
                    }
                }
                for b in chunk.invalid() {
                    let _ = write!(s, "\\x{b:02x}");
                }
            }
            s
        }
        Format::Hex => {
            let n = d.bytes.len().min(max_chars / 3 + 2);
            hex_digits(&d.bytes[..n], " ")
        }
    };
    cut(s, max_chars)
}

/// `s` cut to `max_chars` characters, the last one `…` when it was cut.
fn cut(s: String, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        None => s,
        Some(_) => {
            let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
            out.push('…');
            out
        }
    }
}

/// Why `y` can't copy a value as shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyRefusal {
    /// The text has control characters (a newline included): a pasted newline would
    /// run what follows it. Switching to hex (`f`) copies it.
    ControlCharacters,
}

/// What `y` copies for `d` shown as `format` (spec "Clipboard": in the format shown):
/// - text: the text itself, refused when it has control characters;
/// - hex: the hex digits, no spaces;
/// - JSON and MessagePack: the value as compact one-line JSON (which escapes control
///   characters inside strings);
///
/// falling back the way [`render`] does when `d` isn't valid in `format`.
///
/// Text that is not valid UTF-8 is refused too (as [`CopyRefusal::ControlCharacters`]):
/// it could only be copied with its escapes, which is not the value. What is returned
/// never has a control character or a bidirectional control.
pub fn copy_text(d: &Decoded, format: Format) -> Result<String, CopyRefusal> {
    let (used, _) = resolve(d, format);
    match used {
        Format::Text => match std::str::from_utf8(&d.bytes) {
            Ok(s) if !s.chars().any(|c| c.is_control() || is_bidi_control(c)) => Ok(s.to_string()),
            _ => Err(CopyRefusal::ControlCharacters),
        },
        Format::Hex => Ok(hex_digits(&d.bytes, "")),
        Format::Json => Ok(compact_json(
            std::str::from_utf8(&d.bytes).unwrap_or(""),
            usize::MAX,
        )),
        Format::MsgPack => Ok(compact_msgpack(&d.bytes, usize::MAX).unwrap_or_default()),
    }
}

/// Makes `s` safe to draw on one line: `\t`, `\n` and `\r` become those two-character
/// escapes; every other C0 control character and DEL becomes `\xNN` (ESC is `\x1b`);
/// C1 controls (U+0080 to U+009F) and the bidirectional controls (U+061C, U+200E,
/// U+200F, U+202A to U+202E, U+2066 to U+2069) become `\u{…}`. Everything else, `\`
/// included, is kept as it is.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match escaped_len(c) {
            Some(_) => push_escaped(c, &mut out),
            None => out.push(c),
        }
    }
    out
}

/// How long `c`'s escape is, or `None` when [`escape`] keeps it as it is. Escapes are
/// ASCII, so this is also their width.
fn escaped_len(c: char) -> Option<usize> {
    match c {
        '\t' | '\n' | '\r' => Some(2),
        c if (c as u32) < 0x20 || c == '\x7f' => Some(4),
        c if ('\u{80}'..='\u{9f}').contains(&c) || is_bidi_control(c) => {
            // `\u{` + hex digits + `}`
            let digits = if (c as u32) < 0x100 {
                2
            } else if (c as u32) < 0x1000 {
                3
            } else {
                4
            };
            Some(4 + digits)
        }
        _ => None,
    }
}

/// Pushes the escape for `c`, which [`escaped_len`] said has one.
fn push_escaped(c: char, out: &mut String) {
    match c {
        '\t' => out.push_str("\\t"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        c if (c as u32) < 0x20 || c == '\x7f' => {
            let _ = write!(out, "\\x{:02x}", c as u32);
        }
        c => {
            let _ = write!(out, "\\u{{{:x}}}", c as u32);
        }
    }
}

/// The Unicode bidirectional formatting characters, which can reorder what a terminal
/// draws around them.
pub(crate) fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}
