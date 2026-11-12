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

/// The most bytes a decompression produces. Anything past it is dropped and
/// [`Decoded::truncated`] is set, so a small compressed value can't fill memory.
pub const MAX_DECOMPRESSED: usize = 1024 * 1024;

/// The most lines [`render`] returns. Past it the last line says how many were left
/// out.
pub const MAX_LINES: usize = 10_000;

/// A value read from the server: bytes, shown on screen and nowhere else.
///
/// Its `Debug` prints the length only (`Raw(<12 bytes>)`), so a value never reaches
/// `Debug` output, logs or test failure messages by accident. It has no `Serialize` on
/// purpose: values never go into snapshots or reports. Keep both that way.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Raw(Vec<u8>);

impl Raw {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Vec<u8>> for Raw {
    fn from(b: Vec<u8>) -> Self {
        Self(b)
    }
}

impl From<&[u8]> for Raw {
    fn from(b: &[u8]) -> Self {
        Self(b.to_vec())
    }
}

impl From<&str> for Raw {
    fn from(s: &str) -> Self {
        Self(s.as_bytes().to_vec())
    }
}

impl fmt::Debug for Raw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Raw(<{} bytes>)", self.0.len())
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
pub fn decode(raw: &[u8]) -> Decoded {
    // Contract placeholder: no decompression yet; text when UTF-8, else hex.
    Decoded {
        compression: None,
        truncated: false,
        detected: if std::str::from_utf8(raw).is_ok() {
            Format::Text
        } else {
            Format::Hex
        },
        bytes: raw.to_vec(),
    }
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

/// Draws `d` as `format`, over as many lines as it needs (at most [`MAX_LINES`]):
/// - text: the UTF-8 text split at newlines, invalid bytes as `\xNN`;
/// - hex: 16 bytes a line, offset, hex and the printable ASCII beside it;
/// - JSON: pretty-printed with two-space indents;
/// - MessagePack: decoded and drawn like pretty JSON.
///
/// When `d` isn't valid in `format` (JSON asked for bytes that are not JSON, say), it
/// is drawn as text if it is UTF-8, else as hex, and [`Rendered::note`] says so.
pub fn render(d: &Decoded, format: Format) -> Rendered {
    // Contract placeholder.
    let _ = format;
    let lines = match std::str::from_utf8(&d.bytes) {
        Ok(s) => s.split('\n').map(escape).collect(),
        Err(_) => vec![d.bytes.iter().map(|b| format!("{b:02x}")).collect()],
    };
    Rendered {
        format: d.detected,
        note: None,
        lines,
    }
}

/// One line for a list cell: the value in its detected format, compact (JSON and
/// MessagePack on one line, text with newlines as `\n`, hex as pairs of digits), at
/// most `max_chars` characters with `…` at the end when cut. Terminal safe.
pub fn preview(raw: &[u8], max_chars: usize) -> String {
    // Contract placeholder.
    let s = escape(&String::from_utf8_lossy(raw));
    if s.chars().count() <= max_chars {
        return s;
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
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
pub fn copy_text(d: &Decoded, format: Format) -> Result<String, CopyRefusal> {
    // Contract placeholder.
    let _ = format;
    match std::str::from_utf8(&d.bytes) {
        Ok(s) if !s.chars().any(char::is_control) => Ok(s.to_string()),
        Ok(_) => Err(CopyRefusal::ControlCharacters),
        Err(_) => Ok(d.bytes.iter().map(|b| format!("{b:02x}")).collect()),
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
        match c {
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c == '\x7f' => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c if ('\u{80}'..='\u{9f}').contains(&c) || is_bidi_control(c) => {
                out.push_str(&format!("\\u{{{:x}}}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// The Unicode bidirectional formatting characters, which can reorder what a terminal
/// draws around them.
fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_debug_never_shows_the_value() {
        let r = Raw::from("s3cret-value");
        assert_eq!(format!("{r:?}"), "Raw(<12 bytes>)");
        let d = decode(r.as_bytes());
        assert!(!format!("{d:?}").contains("s3cret"));
        assert!(!format!("{:?}", render(&d, Format::Text)).contains("s3cret"));
    }

    #[test]
    fn escape_draws_controls_as_escapes() {
        assert_eq!(escape("a\x1b[2Jb"), "a\\x1b[2Jb");
        assert_eq!(escape("l1\nl2\tx\r"), "l1\\nl2\\tx\\r");
        assert_eq!(escape("\u{9b}31m"), "\\u{9b}31m");
        assert_eq!(escape("ab\u{202e}cd"), "ab\\u{202e}cd");
        assert_eq!(escape("plain \\ text é"), "plain \\ text é");
    }
}
