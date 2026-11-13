//! JSON and MessagePack as text: validating JSON, re-indenting it without parsing it
//! into a map (so key order, duplicate keys and big numbers stay as they are), and
//! writing a MessagePack value as JSON.

use std::fmt::Write as _;

use super::is_bidi_control;

/// How deep MessagePack may nest, the same as serde_json's limit for JSON.
const MSGPACK_MAX_DEPTH: usize = 128;

/// How deep JSON may nest: the same as MessagePack, about serde_json's own limit for
/// parsing (which it does not apply when it only validates). It also bounds the
/// indent of a pretty-printed line.
const JSON_MAX_DEPTH: usize = 128;

/// `s` (trimmed of ASCII whitespace) is one complete JSON value, nested at most
/// [`JSON_MAX_DEPTH`] deep.
pub(super) fn is_json(s: &str) -> bool {
    let t = s.trim_ascii();
    !t.is_empty()
        && nesting_depth(t) <= JSON_MAX_DEPTH
        && serde_json::from_str::<serde::de::IgnoredAny>(t).is_ok()
}

/// The deepest nesting of `{` and `[` in `s`, outside strings.
fn nesting_depth(s: &str) -> usize {
    let (mut depth, mut max, mut in_string, mut escaped) = (0usize, 0usize, false, false);
    for &b in s.as_bytes() {
        if in_string {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                max = max.max(depth);
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

/// `s` is a JSON object or array: what detection calls JSON.
pub(super) fn is_json_document(s: &str) -> bool {
    matches!(s.trim_ascii_start().as_bytes().first(), Some(b'{' | b'[')) && is_json(s)
}

/// The MessagePack value `bytes` hold, when they hold exactly one, with no bytes left.
pub(super) fn msgpack_value(bytes: &[u8]) -> Option<rmpv::Value> {
    let mut rd = bytes;
    let v = rmpv::decode::read_value_with_max_depth(&mut rd, MSGPACK_MAX_DEPTH).ok()?;
    rd.is_empty().then_some(v)
}

/// `bytes` are a MessagePack map or array: what detection calls MessagePack (it also
/// checks that they are not UTF-8).
pub(super) fn is_msgpack_container(bytes: &[u8]) -> bool {
    matches!(
        msgpack_value(bytes),
        Some(rmpv::Value::Array(_) | rmpv::Value::Map(_))
    )
}

/// Where [`reformat`] writes.
pub(super) trait JsonOut {
    fn text(&mut self, s: &str);
    /// A line break, then the indent for `depth` (pretty output only).
    fn newline(&mut self, depth: usize);
    /// Stop: enough has been written.
    fn full(&self) -> bool {
        false
    }
}

/// Compact JSON into a string, stopping once it is longer than `limit` bytes.
pub(super) struct Compact {
    pub out: String,
    pub limit: usize,
}

impl JsonOut for Compact {
    fn text(&mut self, s: &str) {
        self.out.push_str(s);
    }
    fn newline(&mut self, _depth: usize) {}
    fn full(&self) -> bool {
        self.out.len() > self.limit
    }
}

/// Re-indents `json` (two spaces, like `serde_json::to_string_pretty`) or compacts it.
/// Strings are copied as they are, except that control characters and bidirectional
/// controls in them become `\uXXXX` escapes, so the output is still JSON and has none.
/// Never panics, whatever `json` holds; valid JSON gives valid JSON.
pub(super) fn reformat(json: &str, pretty: bool, out: &mut impl JsonOut) {
    let b = json.as_bytes();
    let mut i = 0;
    let mut depth = 0usize;
    while i < b.len() && !out.full() {
        match b[i] {
            b' ' | b'\t' | b'\n' | b'\r' | 0x0c => i += 1,
            open @ (b'{' | b'[') => {
                let close = if open == b'{' { "}" } else { "]" };
                let open = if open == b'{' { "{" } else { "[" };
                let mut j = i + 1;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < b.len() && b[j] == close.as_bytes()[0] {
                    out.text(open);
                    out.text(close);
                    i = j + 1;
                } else {
                    out.text(open);
                    depth += 1;
                    if pretty {
                        out.newline(depth);
                    }
                    i += 1;
                }
            }
            close @ (b'}' | b']') => {
                depth = depth.saturating_sub(1);
                if pretty {
                    out.newline(depth);
                }
                out.text(if close == b'}' { "}" } else { "]" });
                i += 1;
            }
            b',' => {
                out.text(",");
                if pretty {
                    out.newline(depth);
                }
                i += 1;
            }
            b':' => {
                out.text(if pretty { ": " } else { ":" });
                i += 1;
            }
            b'"' => {
                let mut j = i + 1;
                while j < b.len() && b[j] != b'"' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                let end = (j + 1).min(b.len());
                // `end` is past a `"` or the end of the text: a char boundary.
                let s = json.get(i..end).unwrap_or("");
                sanitized_string(s, out);
                i = end;
            }
            _ => {
                let mut j = i + 1;
                while j < b.len() && !b"{}[],:\" \t\n\r\x0c".contains(&b[j]) {
                    j += 1;
                }
                // Stops at an ASCII byte or the end: a char boundary. `i` is one too,
                // as every arm before stops on one.
                let token = json.get(i..j).unwrap_or("");
                sanitized_string(token, out);
                i = j;
            }
        }
    }
}

/// Writes `s`, with control and bidi characters as `\uXXXX` escapes.
fn sanitized_string(s: &str, out: &mut impl JsonOut) {
    let mut start = 0;
    for (k, c) in s.char_indices() {
        if c.is_control() || is_bidi_control(c) {
            out.text(&s[start..k]);
            out.text(&format!("\\u{:04x}", c as u32));
            start = k + c.len_utf8();
        }
    }
    out.text(&s[start..]);
}

/// Writes `s` as a JSON string, escaping what JSON requires and also every control
/// and bidi character.
fn push_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() || is_bidi_control(c) => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn hex_string(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn push_float(f: f64, finite_json: String, out: &mut String) {
    if f.is_nan() {
        push_json_string("NaN", out);
    } else if f.is_infinite() {
        push_json_string(if f > 0.0 { "Infinity" } else { "-Infinity" }, out);
    } else {
        out.push_str(&finite_json);
    }
}

/// Writes `v` as compact JSON, stopping (and leaving it cut) once `out` is longer than
/// `limit` bytes:
/// - binary as a string of hex digits;
/// - an extension as the string `ext(<type>, <hex>)`;
/// - a string that is not UTF-8 with replacement characters;
/// - a map key that is not a string as a string of its compact JSON;
/// - NaN and the infinities as the strings `NaN`, `Infinity`, `-Infinity`.
pub(super) fn msgpack_to_json(v: &rmpv::Value, out: &mut String, limit: usize) {
    to_json(v, out, limit, false);
}

/// [`msgpack_to_json`]. `in_key` is set inside a map key that is not a string, which is
/// drawn as a string of its own compact JSON: a container key inside it becomes `"…"`,
/// so a key is escaped once, never once per level (each level would double its size).
fn to_json(v: &rmpv::Value, out: &mut String, limit: usize, in_key: bool) {
    use rmpv::Value;
    if out.len() > limit {
        return;
    }
    match v {
        Value::Nil => out.push_str("null"),
        Value::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Integer(n) => {
            let _ = write!(out, "{n}");
        }
        Value::F32(f) => push_float(
            f64::from(*f),
            serde_json::to_string(f).unwrap_or_else(|_| "null".into()),
            out,
        ),
        Value::F64(f) => push_float(
            *f,
            serde_json::to_string(f).unwrap_or_else(|_| "null".into()),
            out,
        ),
        Value::String(s) => match s.as_str() {
            Some(s) => push_json_string(s, out),
            None => push_json_string(&String::from_utf8_lossy(s.as_bytes()), out),
        },
        Value::Binary(b) => push_json_string(&hex_string(b), out),
        Value::Array(items) => {
            out.push('[');
            for (k, item) in items.iter().enumerate() {
                if out.len() > limit {
                    return;
                }
                if k > 0 {
                    out.push(',');
                }
                to_json(item, out, limit, in_key);
            }
            out.push(']');
        }
        Value::Map(entries) => {
            out.push('{');
            for (k, (key, val)) in entries.iter().enumerate() {
                if out.len() > limit {
                    return;
                }
                if k > 0 {
                    out.push(',');
                }
                match key {
                    Value::String(s) if s.as_str().is_some() => {
                        to_json(key, out, limit, in_key);
                    }
                    Value::Array(_) | Value::Map(_) if in_key => out.push_str("\"…\""),
                    _ => {
                        let mut key_text = String::new();
                        to_json(key, &mut key_text, limit.saturating_sub(out.len()), true);
                        push_json_string(&key_text, out);
                    }
                }
                out.push(':');
                to_json(val, out, limit, in_key);
            }
            out.push('}');
        }
        Value::Ext(ty, data) => {
            push_json_string(&format!("ext({ty}, {})", hex_string(data)), out);
        }
    }
}
