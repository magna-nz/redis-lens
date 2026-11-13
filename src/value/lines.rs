//! Building the lines [`render`](super::render) returns: every character escaped,
//! wrapped at the width, cut at [`MAX_LINES`].

use std::fmt::Write as _;

use unicode_width::UnicodeWidthChar;

use super::{MAX_LINES, escaped_len, push_escaped};

/// Collects escaped, wrapped lines. Past [`MAX_LINES`] it only counts them.
pub(super) struct Sink {
    width: usize,
    lines: Vec<String>,
    /// Every line, kept or not.
    total: usize,
    cur: String,
    cur_width: usize,
}

impl Sink {
    pub fn new(width: usize) -> Self {
        Self {
            width: width.max(1),
            lines: Vec::new(),
            total: 0,
            cur: String::new(),
            cur_width: 0,
        }
    }

    fn storing(&self) -> bool {
        self.lines.len() < MAX_LINES
    }

    /// Starts a new line when `w` more columns would not fit. A unit is never split:
    /// one wider than the width gets a line of its own.
    fn make_room(&mut self, w: usize) {
        if self.cur_width > 0 && self.cur_width + w > self.width {
            self.break_line();
        }
        self.cur_width += w;
    }

    /// One character, escaped when it has to be.
    pub fn ch(&mut self, c: char) {
        match escaped_len(c) {
            Some(w) => {
                self.make_room(w);
                if self.storing() {
                    push_escaped(c, &mut self.cur);
                }
            }
            None => {
                self.make_room(c.width().unwrap_or(0));
                if self.storing() {
                    self.cur.push(c);
                }
            }
        }
    }

    pub fn str(&mut self, s: &str) {
        for c in s.chars() {
            self.ch(c);
        }
    }

    /// `n` spaces. Once the lines are full, only counted, without a loop per space.
    pub fn spaces(&mut self, mut n: usize) {
        while n > 0 && self.storing() {
            self.ch(' ');
            n -= 1;
        }
        if n == 0 {
            return;
        }
        if self.cur_width > 0 && self.cur_width >= self.width {
            self.break_line();
        }
        let room = self.width - self.cur_width.min(self.width);
        if n <= room {
            self.cur_width += n;
            return;
        }
        // Fill this line, then whole lines of `width`, then what is left.
        let rest = n - room;
        let more_lines = rest.div_ceil(self.width);
        self.total += more_lines;
        self.cur.clear();
        self.cur_width = rest - (more_lines - 1) * self.width;
    }

    /// A byte that is not part of valid UTF-8, as `\xNN`.
    pub fn invalid_byte(&mut self, b: u8) {
        self.make_room(4);
        if self.storing() {
            let _ = write!(self.cur, "\\x{b:02x}");
        }
    }

    /// Text that may not be UTF-8: invalid bytes as `\xNN`.
    pub fn bytes(&mut self, bytes: &[u8]) {
        for chunk in bytes.utf8_chunks() {
            self.str(chunk.valid());
            for &b in chunk.invalid() {
                self.invalid_byte(b);
            }
        }
    }

    /// Ends the current line, even when it is empty.
    pub fn break_line(&mut self) {
        if self.storing() {
            self.lines.push(std::mem::take(&mut self.cur));
        } else {
            self.cur.clear();
        }
        self.total += 1;
        self.cur_width = 0;
    }

    /// The lines; past [`MAX_LINES`] the last one says how many were left out. Call
    /// [`Sink::break_line`] first for the line in progress.
    pub fn finish(mut self) -> Vec<String> {
        if self.total > MAX_LINES {
            self.lines.truncate(MAX_LINES - 1);
            let left_out = self.total - (MAX_LINES - 1);
            let msg = format!("… {left_out} more lines not shown");
            // ASCII after the first character, which is one column wide.
            self.lines.push(msg.chars().take(self.width).collect());
        }
        self.lines
    }
}

/// Bytes a hex line holds for `width`: 16 if a 16-byte line fits, else 8, else 4.
pub(super) fn hex_bytes_per_line(width: usize) -> usize {
    [16, 8]
        .into_iter()
        .find(|&n| hex_line_len(n) <= width)
        .unwrap_or(4)
}

/// The length of a full `hexdump -C` style line of `n` bytes: offset, two spaces,
/// `xx ` per byte (an extra space after the eighth of sixteen), a space, `|ascii|`.
pub(super) fn hex_line_len(n: usize) -> usize {
    8 + 2 + 3 * n + usize::from(n == 16) + 1 + n + 2
}

/// One `hexdump -C` style line for `chunk`, which starts at `offset`, padded as if it
/// held `n` bytes.
pub(super) fn hex_line(offset: usize, chunk: &[u8], n: usize) -> String {
    let mut s = String::with_capacity(hex_line_len(n));
    let _ = write!(s, "{offset:08x}  ");
    for k in 0..n {
        match chunk.get(k) {
            Some(b) => {
                let _ = write!(s, "{b:02x} ");
            }
            None => s.push_str("   "),
        }
        if n == 16 && k == 7 {
            s.push(' ');
        }
    }
    s.push_str(" |");
    s.extend(chunk.iter().map(|&b| {
        if (0x20..=0x7e).contains(&b) {
            b as char
        } else {
            '.'
        }
    }));
    s.push('|');
    s
}
