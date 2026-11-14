//! The tree the structured formats are drawn as: PHP `serialize()`, Python Pickle,
//! Protobuf and Java serialization. A flat list of lines, each with its depth, with
//! bounded size: at most [`MAX_NODES`] lines and [`MAX_TREE_BYTES`] bytes of text, each
//! string at most [`MAX_STR`] bytes. Past either cap the parsers still read (to tell a
//! valid value from an invalid one) but add nothing, and the tree says it was cut.
//!
//! Lines hold text as the parsers wrote it, control characters included: they are
//! escaped when drawn ([`super::render`] through the line sink) or made one line
//! ([`Tree::compact`], escaped by its callers).

use std::fmt::Write as _;

/// How deep a parser nests. Deeper parts are not drawn (Pickle, Protobuf, Java) or make
/// the value invalid (PHP).
pub(super) const MAX_DEPTH: usize = 32;
/// The most lines a tree holds: the renderer's own line cap.
pub(super) const MAX_NODES: usize = super::MAX_LINES;
/// The most bytes of text a tree holds.
pub(super) const MAX_TREE_BYTES: usize = super::MAX_DECOMPRESSED;
/// The most bytes of one string or byte string shown; the rest is cut with `…`.
pub(super) const MAX_STR: usize = 1024;

/// Lines of a structured value, each `(depth, text)`.
pub(super) struct Tree {
    lines: Vec<(usize, String)>,
    bytes: usize,
    max_bytes: usize,
    /// A line was left out because a cap was reached.
    cut: bool,
}

impl Tree {
    /// A tree with the full caps, for drawing and copying.
    pub fn new() -> Self {
        Self::with_limit(MAX_TREE_BYTES)
    }

    /// A tree that keeps at most `max_bytes` of text (and [`MAX_NODES`] lines): for a
    /// one-line preview, or 0 to only check that a value parses.
    pub fn with_limit(max_bytes: usize) -> Self {
        Self {
            lines: Vec::new(),
            bytes: 0,
            max_bytes: max_bytes.min(MAX_TREE_BYTES),
            cut: false,
        }
    }

    /// True once nothing more is kept.
    pub fn full(&self) -> bool {
        self.cut
    }

    /// Adds a line at `depth`. Past a cap it is left out and the tree is marked cut.
    pub fn push(&mut self, depth: usize, mut text: String) {
        if self.cut {
            return;
        }
        if self.lines.len() >= MAX_NODES {
            self.cut = true;
            return;
        }
        if self.bytes + text.len() > self.max_bytes {
            // What fits of the line is kept, so a short preview still shows its start.
            self.cut = true;
            let mut room = self.max_bytes - self.bytes;
            if room == 0 {
                return;
            }
            while !text.is_char_boundary(room) {
                room -= 1;
            }
            text.truncate(room);
        }
        self.bytes += text.len();
        self.lines.push((depth, text));
    }

    pub fn lines(&self) -> &[(usize, String)] {
        &self.lines
    }

    pub fn is_cut(&self) -> bool {
        self.cut
    }

    /// The tree on one line: a node's children in `{…}` after it, siblings separated
    /// by `, `. Stops once longer than `limit` bytes. Not escaped.
    pub fn compact(&self, limit: usize) -> String {
        let mut out = String::new();
        let Some(&(first, _)) = self.lines.first() else {
            return out;
        };
        let mut prev = first;
        let mut open = 0usize;
        for (k, (d, text)) in self.lines.iter().enumerate() {
            let d = (*d).max(first);
            if k > 0 {
                if d > prev {
                    out.push_str(" {");
                    open += 1;
                } else {
                    let close = (prev - d).min(open);
                    for _ in 0..close {
                        out.push('}');
                    }
                    open -= close;
                    out.push_str(", ");
                }
            }
            out.push_str(text);
            prev = d;
            if out.len() > limit {
                return out;
            }
        }
        for _ in 0..open {
            out.push('}');
        }
        if self.cut {
            out.push_str(" …");
        }
        out
    }
}

/// `b` as text: valid UTF-8 as it is, other bytes as `\xNN`, cut at [`MAX_STR`] bytes
/// with `…`.
pub(super) fn bytes_text(b: &[u8]) -> String {
    let cut = b.len() > MAX_STR;
    let b = &b[..b.len().min(MAX_STR)];
    let mut s = String::with_capacity(b.len());
    for chunk in b.utf8_chunks() {
        s.push_str(chunk.valid());
        for x in chunk.invalid() {
            let _ = write!(s, "\\x{x:02x}");
        }
    }
    if cut {
        s.push('…');
    }
    s
}

/// `b` as a byte string: printable ASCII as it is, other bytes as `\xNN`, cut at
/// [`MAX_STR`] bytes with `…`.
pub(super) fn ascii_text(b: &[u8]) -> String {
    let cut = b.len() > MAX_STR;
    let mut s = String::new();
    for &x in &b[..b.len().min(MAX_STR)] {
        if (0x20..0x7f).contains(&x) {
            s.push(x as char);
        } else {
            let _ = write!(s, "\\x{x:02x}");
        }
    }
    if cut {
        s.push('…');
    }
    s
}

/// The first `n` bytes of `b` as hex pairs, with `…` when there are more.
pub(super) fn hex_head(b: &[u8], n: usize) -> String {
    let mut s = String::new();
    for (k, x) in b.iter().take(n).enumerate() {
        if k > 0 {
            s.push(' ');
        }
        let _ = write!(s, "{x:02x}");
    }
    if b.len() > n {
        s.push_str(" …");
    }
    s
}

/// Adds a hex dump of `tail` (at most 256 bytes, 16 a line) under a line saying how
/// much was not read from `offset`: what a partial reader shows past what it
/// understands.
pub(super) fn hex_tail(out: &mut Tree, depth: usize, offset: usize, tail: &[u8]) {
    out.push(
        depth,
        format!(
            "not read: {} bytes from byte {offset}",
            crate::format::grouped(tail.len() as u64)
        ),
    );
    let (lines, _) = tail[..tail.len().min(256)].as_chunks::<16>();
    let mut shown = 0;
    for line in lines {
        out.push(depth + 1, hex_head(line, 16));
        shown += 16;
    }
    let rest = &tail[shown..tail.len().min(256)];
    if !rest.is_empty() {
        out.push(depth + 1, hex_head(rest, 16));
    }
    if tail.len() > 256 {
        out.push(depth + 1, "…".to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(lines: &[(usize, &str)]) -> Tree {
        let mut t = Tree::new();
        for (d, s) in lines {
            t.push(*d, (*s).to_string());
        }
        t
    }

    #[test]
    fn compact_nests_children_in_braces() {
        let t = tree(&[
            (0, "array(2)"),
            (1, "[0] => int(1)"),
            (1, "[1] => array(1)"),
            (2, "[0] => NULL"),
        ]);
        assert_eq!(
            t.compact(1000),
            "array(2) {[0] => int(1), [1] => array(1) {[0] => NULL}}"
        );
        let t = tree(&[(0, "a"), (1, "b"), (2, "c"), (0, "d")]);
        assert_eq!(t.compact(1000), "a {b {c}}, d");
        assert_eq!(tree(&[]).compact(10), "");
    }

    #[test]
    fn caps_hold() {
        let mut t = Tree::with_limit(10);
        t.push(0, "12345".into());
        t.push(0, "1234é6".into());
        assert!(t.full());
        t.push(0, "1".into());
        // The line that did not fit is kept up to the cap, on a character.
        assert_eq!(t.lines().len(), 2);
        assert_eq!(t.lines()[1].1, "1234");
        let mut t = Tree::with_limit(0);
        t.push(0, "x".into());
        assert!(t.lines().is_empty() && t.full());
        let mut t = Tree::new();
        for _ in 0..MAX_NODES + 5 {
            t.push(0, "x".into());
        }
        assert_eq!(t.lines().len(), MAX_NODES);
        assert!(t.is_cut());
    }

    #[test]
    fn text_helpers() {
        assert_eq!(bytes_text(b"ok\xff"), "ok\\xff");
        assert_eq!(ascii_text(b"a\x00\xe9"), "a\\x00\\xe9");
        assert_eq!(
            bytes_text(&[b'a'; MAX_STR + 1]).chars().count(),
            MAX_STR + 1
        );
        assert_eq!(hex_head(&[1, 2, 3], 2), "01 02 …");
        let mut t = Tree::new();
        hex_tail(&mut t, 0, 4, &[0xab; 300]);
        assert_eq!(t.lines()[0].1, "not read: 300 bytes from byte 4");
        assert_eq!(t.lines().len(), 1 + 16 + 1);
    }
}
