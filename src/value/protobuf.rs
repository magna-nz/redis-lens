//! Protobuf without a schema: the wire format as a tree of field numbers and wire
//! types, like `protoc --decode_raw`. Never detected on its own (any bytes can look like
//! a few fields): shown only when picked with `f`.
//!
//! A length-delimited field is drawn as a string when it is printable UTF-8, else as a
//! nested message when its bytes parse completely as fields, else as a string when it
//! is UTF-8 at all, else as bytes. Groups are drawn like messages. Nesting stops at
//! [`MAX_DEPTH`], past which a field's bytes are shown as bytes.
//!
//! Every check of a level is a single pass, and each level is checked once, so the
//! work is at most the input times [`MAX_DEPTH`].

use super::tree::{MAX_DEPTH, Tree, bytes_text, hex_head};

/// The bytes are a whole number of valid fields: what `f` needs to show Protobuf.
pub(super) fn valid(b: &[u8]) -> bool {
    scan(b, 0).is_some()
}

/// Writes the fields of `b` into `out`. False when `b` is not valid (nothing written).
pub(super) fn render(b: &[u8], out: &mut Tree) -> bool {
    if !valid(b) {
        return false;
    }
    if b.is_empty() {
        out.push(0, "empty message".to_string());
        return true;
    }
    emit(b, 0, out);
    true
}

enum Wire<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    /// The bytes between a start group and its end group.
    Group(&'a [u8]),
    Fixed32(u32),
}

struct Fields<'a> {
    b: &'a [u8],
    i: usize,
    depth: usize,
}

fn varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut v: u64 = 0;
    for k in 0..10 {
        let byte = *b.get(*i)?;
        *i += 1;
        if k == 9 && byte > 1 {
            return None;
        }
        v |= u64::from(byte & 0x7f) << (7 * k);
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// A tag: the field number and the wire type.
fn tag(b: &[u8], i: &mut usize) -> Option<(u32, u8)> {
    let t = varint(b, i)?;
    let field = u32::try_from(t >> 3).ok()?;
    if field == 0 || field > (1 << 29) - 1 {
        return None;
    }
    Some((field, (t & 7) as u8))
}

impl<'a> Fields<'a> {
    /// The next field, `None` at the end; `Err` when the bytes are not valid here.
    fn next_field(&mut self) -> Result<Option<(u32, Wire<'a>)>, ()> {
        if self.i >= self.b.len() {
            return Ok(None);
        }
        let (field, wt) = tag(self.b, &mut self.i).ok_or(())?;
        let w = match wt {
            0 => Wire::Varint(varint(self.b, &mut self.i).ok_or(())?),
            1 => Wire::Fixed64(u64::from_le_bytes(
                self.take(8)?.try_into().map_err(|_| ())?,
            )),
            2 => {
                let n = varint(self.b, &mut self.i).ok_or(())?;
                Wire::Bytes(self.take(usize::try_from(n).map_err(|_| ())?)?)
            }
            3 => {
                // Up to the matching end group, checking the fields between.
                if self.depth >= MAX_DEPTH {
                    return Err(());
                }
                let start = self.i;
                let mut inner = Fields {
                    b: self.b,
                    i: self.i,
                    depth: self.depth + 1,
                };
                loop {
                    let at = inner.i;
                    let (f, t) = tag(self.b, &mut { at }).ok_or(())?;
                    if t == 4 {
                        if f != field {
                            return Err(());
                        }
                        let mut end = at;
                        tag(self.b, &mut end).ok_or(())?;
                        self.i = end;
                        break Wire::Group(&self.b[start..at]);
                    }
                    inner.next_field()?.ok_or(())?;
                }
            }
            5 => Wire::Fixed32(u32::from_le_bytes(
                self.take(4)?.try_into().map_err(|_| ())?,
            )),
            _ => return Err(()),
        };
        Ok(Some((field, w)))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ()> {
        let end = self.i.checked_add(n).ok_or(())?;
        let s = self.b.get(self.i..end).ok_or(())?;
        self.i = end;
        Ok(s)
    }
}

/// How many fields `b` holds when it is a whole number of valid fields at `depth`.
fn scan(b: &[u8], depth: usize) -> Option<usize> {
    let mut f = Fields { b, i: 0, depth };
    let mut n = 0;
    while f.next_field().ok()?.is_some() {
        n += 1;
    }
    Some(n)
}

/// Printable: UTF-8 with no control characters but tabs and line breaks.
fn printable(b: &[u8]) -> Option<&str> {
    let s = std::str::from_utf8(b).ok()?;
    (!s.chars()
        .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r')))
    .then_some(s)
}

fn emit(b: &[u8], depth: usize, out: &mut Tree) {
    let mut f = Fields { b, i: 0, depth };
    while let Ok(Some((field, w))) = f.next_field() {
        if out.full() {
            return;
        }
        match w {
            Wire::Varint(v) => {
                let signed = if v > i64::MAX as u64 {
                    format!(" (int64 {})", v as i64)
                } else {
                    String::new()
                };
                out.push(depth, format!("{field}: varint {v}{signed}"));
            }
            Wire::Fixed64(v) => out.push(
                depth,
                format!("{field}: fixed64 {v} (double {:?})", f64::from_bits(v)),
            ),
            Wire::Fixed32(v) => out.push(
                depth,
                format!("{field}: fixed32 {v} (float {:?})", f32::from_bits(v)),
            ),
            Wire::Group(inner) => {
                let n = scan(inner, depth + 1).unwrap_or(0);
                out.push(depth, format!("{field}: group ({n} fields)"));
                emit(inner, depth + 1, out);
            }
            Wire::Bytes(c) => {
                if c.is_empty() {
                    out.push(depth, format!("{field}: \"\""));
                } else if let Some(s) = printable(c) {
                    out.push(depth, format!("{field}: \"{}\"", bytes_text(s.as_bytes())));
                } else if let Some(n) = (depth < MAX_DEPTH).then(|| scan(c, depth + 1)).flatten() {
                    out.push(depth, format!("{field}: message ({n} fields)"));
                    emit(c, depth + 1, out);
                } else if std::str::from_utf8(c).is_ok() {
                    out.push(depth, format!("{field}: \"{}\"", bytes_text(c)));
                } else {
                    out.push(
                        depth,
                        format!("{field}: bytes ({}) {}", c.len(), hex_head(c, 32)),
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(b: &[u8]) -> Vec<String> {
        let mut t = Tree::new();
        assert!(render(b, &mut t), "{b:?}");
        t.lines()
            .iter()
            .map(|(d, s)| format!("{}{s}", "  ".repeat(*d)))
            .collect()
    }

    #[test]
    fn the_wire_format_as_a_tree() {
        // 1: 150, 2: "testing", 3: {1: 1, 2: "\x01x"}, 4: fixed32 42, 5: fixed64 1,
        // 6: bytes ff fe, 7: -1 as int64.
        let mut b = vec![0x08, 0x96, 0x01, 0x12, 0x07];
        b.extend(b"testing");
        b.extend([0x1a, 0x06, 0x08, 0x01, 0x12, 0x02, 0x01, b'x']);
        b.extend([0x25, 42, 0, 0, 0]);
        b.extend([0x29, 1, 0, 0, 0, 0, 0, 0, 0]);
        b.extend([0x32, 0x02, 0xff, 0xfe]);
        b.push(0x38);
        b.extend([0xff; 9]);
        b.push(0x01);
        assert_eq!(
            lines(&b),
            [
                "1: varint 150",
                "2: \"testing\"",
                "3: message (2 fields)",
                "  1: varint 1",
                "  2: \"\u{1}x\"",
                "4: fixed32 42 (float 5.9e-44)",
                "5: fixed64 1 (double 5e-324)",
                "6: bytes (2) ff fe",
                "7: varint 18446744073709551615 (int64 -1)",
            ]
        );
    }

    #[test]
    fn groups_must_close_with_their_own_number() {
        // 1: group {2: 5}
        let b = [0x0b, 0x10, 0x05, 0x0c];
        assert_eq!(lines(&b), ["1: group (1 fields)", "  2: varint 5"]);
        assert!(!valid(&[0x0b, 0x10, 0x05, 0x14]));
        assert!(!valid(&[0x0b, 0x10, 0x05]));
        assert!(!valid(&[0x0c]));
    }

    #[test]
    fn invalid_wire_data_is_refused() {
        for bad in [
            &[0x08][..],
            &[0x12, 0x05, 0x01],
            &[0x0e, 0x00],
            &[0x00, 0x00],
            &[
                0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f,
            ],
            &[0x12, 0xff, 0xff, 0xff, 0xff, 0x0f],
        ] {
            assert!(!valid(bad), "{bad:?}");
            assert!(!render(bad, &mut Tree::new()));
        }
        assert_eq!(lines(&[]), ["empty message"]);
    }

    #[test]
    fn deep_messages_and_groups_stay_bounded() {
        // A message nested 200 deep: drawn to MAX_DEPTH, the rest as bytes.
        let mut b = vec![0x08, 0x01];
        for _ in 0..200 {
            let mut outer = vec![0x0a];
            let mut n = b.len();
            loop {
                let byte = (n & 0x7f) as u8;
                n >>= 7;
                if n == 0 {
                    outer.push(byte);
                    break;
                }
                outer.push(byte | 0x80);
            }
            outer.extend(b);
            b = outer;
        }
        let l = lines(&b);
        assert!(l.len() <= MAX_DEPTH + 2, "{}", l.len());
        assert!(l.last().unwrap().contains("bytes"));
        // Groups 10,000 deep are refused, not followed.
        let mut g = vec![0x0b; 10_000];
        g.extend(vec![0x0c; 10_000]);
        assert!(!valid(&g));
    }
}
