//! PHP `serialize()` output as a tree, in `var_dump` style: `array(2)`, `[0] =>
//! int(5)`, `["name"] => string(3) "bob"`, `object(User) (1)`, …
//!
//! A reader for untrusted bytes: it never allocates by a length the input claims, nests
//! at most [`MAX_DEPTH`] deep (deeper is invalid), and stops adding lines at the tree's
//! caps while it goes on checking the rest.

use super::tree::{MAX_DEPTH, Tree, bytes_text};

/// The bytes are one complete PHP array or object (`a:`, `O:` or `C:`), nothing after
/// it: what detection calls PHP.
pub(super) fn is_container(b: &[u8]) -> bool {
    matches!(b, [b'a' | b'O' | b'C', b':', ..]) && render(b, &mut Tree::with_limit(0))
}

/// Writes the value `b` holds into `out`. True when `b` is exactly one valid value.
pub(super) fn render(b: &[u8], out: &mut Tree) -> bool {
    let mut p = Parser { b, i: 0 };
    p.value(0, String::new(), out).is_some() && p.i == b.len()
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn expect(&mut self, c: u8) -> Option<()> {
        (self.peek()? == c).then(|| self.i += 1)
    }

    /// Bytes up to (not including) `end`, which is consumed. At most 400 bytes: no
    /// number or float is longer.
    fn until(&mut self, end: u8) -> Option<&'a [u8]> {
        let rest = &self.b[self.i..];
        let n = rest.iter().take(400).position(|&c| c == end)?;
        self.i += n + 1;
        Some(&rest[..n])
    }

    /// A non-negative decimal ending at `end`.
    fn len(&mut self, end: u8) -> Option<usize> {
        let t = self.until(end)?;
        if t.is_empty() || t.len() > 19 || !t.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(t).ok()?.parse().ok()
    }

    /// `<n>:"<n bytes>"`, the `"` around them consumed.
    fn quoted(&mut self) -> Option<&'a [u8]> {
        let n = self.len(b':')?;
        self.expect(b'"')?;
        let end = self.i.checked_add(n)?;
        let s = self.b.get(self.i..end)?;
        self.i = end;
        self.expect(b'"')?;
        Some(s)
    }

    /// One value, its line `prefix` followed by what it is.
    fn value(&mut self, depth: usize, prefix: String, out: &mut Tree) -> Option<()> {
        if depth > MAX_DEPTH {
            return None;
        }
        let tag = self.peek()?;
        self.i += 1;
        if tag == b'N' {
            self.expect(b';')?;
            out.push(depth, format!("{prefix}NULL"));
            return Some(());
        }
        self.expect(b':')?;
        match tag {
            b'b' => {
                let v = match self.until(b';')? {
                    b"0" => "false",
                    b"1" => "true",
                    _ => return None,
                };
                out.push(depth, format!("{prefix}bool({v})"));
            }
            b'i' => {
                let t = self.until(b';')?;
                let digits = t.strip_prefix(b"-").unwrap_or(t);
                if digits.is_empty() || digits.len() > 20 || !digits.iter().all(u8::is_ascii_digit)
                {
                    return None;
                }
                out.push(depth, format!("{prefix}int({})", bytes_text(t)));
            }
            b'd' => {
                let t = self.until(b';')?;
                let ok = matches!(t, b"INF" | b"-INF" | b"NAN")
                    || (!t.is_empty()
                        && t.iter().any(u8::is_ascii_digit)
                        && t.iter().all(|c| c.is_ascii_digit() || b"+-.eE".contains(c)));
                if !ok {
                    return None;
                }
                out.push(depth, format!("{prefix}float({})", bytes_text(t)));
            }
            b's' => {
                let s = self.quoted()?;
                self.expect(b';')?;
                out.push(
                    depth,
                    format!("{prefix}string({}) \"{}\"", s.len(), bytes_text(s)),
                );
            }
            b'E' => {
                let s = self.quoted()?;
                self.expect(b';')?;
                let text = bytes_text(s).replacen(':', "::", 1);
                out.push(depth, format!("{prefix}enum({text})"));
            }
            b'r' | b'R' => {
                let n = self.len(b';')?;
                let amp = if tag == b'R' { "&" } else { "" };
                out.push(depth, format!("{prefix}{amp}reference to value {n}"));
            }
            b'a' => {
                let n = self.len(b':')?;
                self.expect(b'{')?;
                out.push(depth, format!("{prefix}array({n})"));
                for _ in 0..n {
                    let key = self.key()?;
                    self.value(depth + 1, format!("{key} => "), out)?;
                }
                self.expect(b'}')?;
            }
            b'O' => {
                let class = bytes_text(self.quoted()?);
                self.expect(b':')?;
                let n = self.len(b':')?;
                self.expect(b'{')?;
                out.push(depth, format!("{prefix}object({class}) ({n})"));
                for _ in 0..n {
                    let key = self.prop()?;
                    self.value(depth + 1, format!("{key} => "), out)?;
                }
                self.expect(b'}')?;
            }
            b'C' => {
                let class = bytes_text(self.quoted()?);
                self.expect(b':')?;
                let n = self.len(b':')?;
                self.expect(b'{')?;
                let end = self.i.checked_add(n)?;
                let data = self.b.get(self.i..end)?;
                self.i = end;
                self.expect(b'}')?;
                out.push(
                    depth,
                    format!(
                        "{prefix}object({class}) custom string({n}) \"{}\"",
                        bytes_text(data)
                    ),
                );
            }
            _ => return None,
        }
        Some(())
    }

    /// An array key: `[0]` or `["name"]`.
    fn key(&mut self) -> Option<String> {
        let tag = self.peek()?;
        self.i += 1;
        self.expect(b':')?;
        match tag {
            b'i' => {
                let t = self.until(b';')?;
                let digits = t.strip_prefix(b"-").unwrap_or(t);
                if digits.is_empty() || digits.len() > 20 || !digits.iter().all(u8::is_ascii_digit)
                {
                    return None;
                }
                Some(format!("[{}]", bytes_text(t)))
            }
            b's' => {
                let s = self.quoted()?;
                self.expect(b';')?;
                Some(format!("[\"{}\"]", bytes_text(s)))
            }
            _ => None,
        }
    }

    /// An object property name, with its visibility the way `var_dump` writes it:
    /// `["name"]`, `["name":protected]`, `["name":"Class":private]`.
    fn prop(&mut self) -> Option<String> {
        if self.peek()? != b's' {
            return self.key();
        }
        self.i += 1;
        self.expect(b':')?;
        let s = self.quoted()?;
        self.expect(b';')?;
        Some(match s {
            [0, b'*', 0, name @ ..] => format!("[\"{}\":protected]", bytes_text(name)),
            [0, rest @ ..] => match rest.iter().position(|&c| c == 0) {
                Some(k) => format!(
                    "[\"{}\":\"{}\":private]",
                    bytes_text(&rest[k + 1..]),
                    bytes_text(&rest[..k])
                ),
                None => format!("[\"{}\"]", bytes_text(s)),
            },
            _ => format!("[\"{}\"]", bytes_text(s)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(b: &[u8]) -> Vec<String> {
        let mut t = Tree::new();
        assert!(render(b, &mut t), "{}", String::from_utf8_lossy(b));
        t.lines()
            .iter()
            .map(|(d, s)| format!("{}{s}", "  ".repeat(*d)))
            .collect()
    }

    #[test]
    fn scalars() {
        assert_eq!(lines(b"N;"), ["NULL"]);
        assert_eq!(lines(b"b:1;"), ["bool(true)"]);
        assert_eq!(lines(b"i:-42;"), ["int(-42)"]);
        assert_eq!(lines(b"d:0.5;"), ["float(0.5)"]);
        assert_eq!(lines(b"d:-INF;"), ["float(-INF)"]);
        assert_eq!(lines(b"s:5:\"h\"llo\";"), ["string(5) \"h\"llo\""]);
        assert_eq!(lines("s:2:\"é\";".as_bytes()), ["string(2) \"é\""]);
        assert_eq!(lines(b"E:11:\"Suit:Hearts\";"), ["enum(Suit::Hearts)"]);
    }

    #[test]
    fn arrays_and_objects() {
        let b = br#"a:3:{i:0;s:3:"foo";s:4:"tags";a:1:{i:0;s:1:"x";}s:3:"obj";O:4:"User":3:{s:4:"name";s:3:"bob";s:6:"\0*\0age";i:42;s:10:"\0User\0pass";N;}}"#;
        let b = String::from_utf8_lossy(b).replace("\\0", "\0");
        assert_eq!(
            lines(b.as_bytes()),
            [
                "array(3)",
                "  [0] => string(3) \"foo\"",
                "  [\"tags\"] => array(1)",
                "    [0] => string(1) \"x\"",
                "  [\"obj\"] => object(User) (3)",
                "    [\"name\"] => string(3) \"bob\"",
                "    [\"age\":protected] => int(42)",
                "    [\"pass\":\"User\":private] => NULL",
            ]
        );
        assert!(is_container(b.as_bytes()));
        assert_eq!(
            lines(b"C:11:\"ArrayObject\":5:{x:i:0}"),
            ["object(ArrayObject) custom string(5) \"x:i:0\""]
        );
        assert_eq!(
            lines(b"a:2:{i:0;r:1;i:1;R:1;}"),
            [
                "array(2)",
                "  [0] => reference to value 1",
                "  [1] => &reference to value 1"
            ]
        );
    }

    #[test]
    fn invalid_values_are_refused() {
        for bad in [
            &b""[..],
            b"a:1:{i:0;}",
            b"s:10:\"short\";",
            b"s:99999999999999999999:\"x\";",
            b"a:2:{i:0;N;}",
            b"i:12",
            b"i:1x;",
            b"b:2;",
            b"N;N;",
            b"a:1:{a:0:{}N;}",
            b"x:1;",
        ] {
            assert!(
                !render(bad, &mut Tree::new()),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        // Only containers are detected.
        assert!(!is_container(b"i:5;"));
        assert!(!is_container(b"s:1:\"a\";"));
        assert!(is_container(b"a:0:{}"));
    }

    #[test]
    fn deep_nesting_is_refused_without_overflowing() {
        let mut b = String::new();
        for _ in 0..10_000 {
            b.push_str("a:1:{i:0;");
        }
        b.push_str("N;");
        for _ in 0..10_000 {
            b.push('}');
        }
        assert!(!render(b.as_bytes(), &mut Tree::new()));
        let mut ok = String::new();
        for _ in 0..MAX_DEPTH {
            ok.push_str("a:1:{i:0;");
        }
        ok.push_str("N;");
        ok.push_str(&"}".repeat(MAX_DEPTH));
        assert!(render(ok.as_bytes(), &mut Tree::new()));
    }

    #[test]
    fn a_huge_count_claim_does_not_allocate() {
        assert!(!render(b"a:9999999999999999:{}", &mut Tree::new()));
    }
}
