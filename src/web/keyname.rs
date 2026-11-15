//! How the dashboard names a key (or a pattern) to the page and back.
//!
//! A key name is bytes. The live layers read names as UTF-8 text (a name that is not
//! UTF-8 is counted as unreadable and never listed), but a name can still hold control
//! characters or bidirectional controls, which must never reach the page as they are.
//! So every name goes out three ways ([`Name`]):
//!
//! - `name`: for display, escaped with [`crate::value::escape`] as the TUI escapes it;
//! - `id`: the name's bytes in lowercase hex, lossless: the page sends it back to name
//!   the key (or pattern) in a request, never the display text;
//! - `copy`: the name as it is, for the clipboard, only when it has no character that
//!   escaping changes; `null` otherwise (copying the escaped text would name another key).

use axum::http::StatusCode;
use serde::Serialize;

use crate::web::api::Refused;

/// Why a key or pattern id was refused.
pub const BAD_ID: &str = "the key id is not the hex the dashboard gave for a key";

/// The longest id taken: names of up to 64 KiB.
pub const MAX_ID: usize = 128 * 1024;

/// A name as the page gets it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Name {
    /// Escaped for display.
    pub name: String,
    /// The bytes in lowercase hex: send this back.
    pub id: String,
    /// The text to copy, when it is safe to put on the clipboard as it is.
    pub copy: Option<String>,
}

impl Name {
    pub fn of(raw: &str) -> Self {
        let name = crate::value::escape(raw);
        let copy = (name == raw).then(|| raw.to_string());
        Self {
            name,
            id: id(raw),
            copy,
        }
    }
}

/// A server string (a field name, a member) as one safe line, as the TUI's key detail
/// shows it: valid UTF-8 escaped by [`crate::value::escape`], other bytes as `\xNN`.
pub fn bytes_text(raw: &[u8]) -> String {
    let mut out = String::new();
    for chunk in raw.utf8_chunks() {
        out.push_str(&crate::value::escape(chunk.valid()));
        for b in chunk.invalid() {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    out
}

/// `raw`'s bytes in lowercase hex.
pub fn id(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() * 2);
    for b in raw.as_bytes() {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0xf)]));
    }
    out
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// The name an [`id`] stands for. Refuses (400) anything that is not even-length hex of
/// UTF-8 bytes, or longer than [`MAX_ID`].
pub fn decode(id: &str) -> Result<String, Refused> {
    let bad = || Refused::new(StatusCode::BAD_REQUEST, BAD_ID);
    if !id.len().is_multiple_of(2) || id.len() > MAX_ID {
        return Err(bad());
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let mut bytes = Vec::with_capacity(id.len() / 2);
    for pair in id.as_bytes().chunks(2) {
        let (Some(hi), Some(lo)) = (digit(pair[0]), digit(pair[1])) else {
            return Err(bad());
        };
        bytes.push(hi << 4 | lo);
    }
    String::from_utf8(bytes).map_err(|_| bad())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn a_plain_name_is_shown_and_copied_as_it_is() {
        let n = Name::of("user:1:cart");
        assert_eq!(n.name, "user:1:cart");
        assert_eq!(n.copy.as_deref(), Some("user:1:cart"));
        assert_eq!(n.id, "757365723a313a63617274");
        assert_eq!(decode(&n.id).unwrap(), "user:1:cart");
    }

    #[test]
    fn a_name_with_control_characters_is_escaped_and_not_copied() {
        let raw = "evil\x1b[2Jname\n\u{202e}x";
        let n = Name::of(raw);
        assert!(!n.name.contains('\x1b') && !n.name.contains('\n'));
        assert!(!n.name.contains('\u{202e}'));
        assert!(n.name.contains("\\x1b"), "{}", n.name);
        assert_eq!(n.copy, None);
        // The id still names the key exactly.
        assert_eq!(decode(&n.id).unwrap(), raw);
    }

    #[test]
    fn ids_that_are_not_hex_of_utf8_are_refused() {
        for bad in ["a", "zz", "0g", "ff", "c3", "fffe"] {
            let e = decode(bad).unwrap_err();
            assert_eq!(e.status, StatusCode::BAD_REQUEST, "{bad}");
            assert_eq!(e.text, BAD_ID);
        }
        assert_eq!(decode("").unwrap(), "");
        assert_eq!(decode("4A4b").unwrap(), "JK");
        assert!(decode(&"61".repeat(MAX_ID)).is_err());
    }

    proptest! {
        #[test]
        fn every_name_comes_back_from_its_id(raw in ".{0,64}") {
            let n = Name::of(&raw);
            prop_assert_eq!(decode(&n.id).unwrap(), raw.clone());
            prop_assert!(n.id.bytes().all(|b| HEX.contains(&b)));
            prop_assert!(!n.name.chars().any(char::is_control));
            if let Some(c) = &n.copy {
                prop_assert_eq!(c, &raw);
            }
        }
    }
}
