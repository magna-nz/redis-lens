//! Signed changes and their colours, shared by the Patterns Δ column and the Diff view.

use ratatui::style::Style;

use crate::diff::{Cause, PatternDiff};
use crate::format;

use super::style;

/// Growth of at least this many bytes is shown in red.
pub const BIG_GROWTH: i64 = 300 * 1024 * 1024;

/// The change of one pattern, or the sum over a group's children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    /// Bytes when both reports had sizes, keys otherwise.
    pub value: i64,
    pub bytes: bool,
    /// Every pattern in it is new since the snapshot.
    pub new: bool,
    /// Every pattern in it is unchanged.
    pub unchanged: bool,
}

impl Change {
    pub fn of(p: &PatternDiff) -> Self {
        let (value, bytes) = match p.delta_bytes {
            Some(b) => (b, true),
            None => (p.delta_keys, false),
        };
        Self {
            value,
            bytes,
            new: p.cause == Cause::New,
            unchanged: p.cause == Cause::Unchanged,
        }
    }

    /// The sum of `parts`, or `None` when there are none.
    pub fn sum(parts: impl IntoIterator<Item = Change>) -> Option<Self> {
        parts.into_iter().reduce(|a, b| Self {
            value: a.value.saturating_add(b.value),
            bytes: a.bytes && b.bytes,
            new: a.new && b.new,
            unchanged: a.unchanged && b.unchanged,
        })
    }

    /// `+640M`, `-12M`, `+1.5K` (keys without sizes), or `·` for no change at all.
    pub fn text(self) -> String {
        if self.value == 0 {
            "·".to_string()
        } else if self.bytes {
            signed_bytes(self.value)
        } else {
            signed(self.value, format::count)
        }
    }

    /// New, or grown by at least [`BIG_GROWTH`] bytes.
    pub fn is_alarming(self) -> bool {
        self.new || (self.bytes && self.value >= BIG_GROWTH)
    }

    /// Red for new patterns and growth of at least 300M, amber for other growth,
    /// teal for shrinkage, dim when unchanged.
    pub fn style(self) -> Style {
        if self.is_alarming() {
            style::warn()
        } else if self.unchanged || self.value == 0 {
            style::dim()
        } else if self.value > 0 {
            style::amber()
        } else {
            style::ok()
        }
    }
}

/// `n` formatted by `fmt`, with `+` or `-` in front unless it is zero.
pub fn signed(n: i64, fmt: impl Fn(u64) -> String) -> String {
    match n {
        0 => fmt(0),
        n if n > 0 => format!("+{}", fmt(n as u64)),
        n => format!("-{}", fmt(n.unsigned_abs())),
    }
}

pub fn signed_bytes(n: i64) -> String {
    signed(n, format::bytes)
}

/// `after / before - 1`, or `None` when there is nothing to compare with.
pub fn ratio_change(before: Option<u64>, after: Option<u64>) -> Option<f64> {
    match (before, after) {
        (Some(b), Some(a)) if b > 0 => Some(a as f64 / b as f64 - 1.0),
        _ => None,
    }
}

/// `+38%`, `-4%`, `0%`.
pub fn signed_percent(r: f64) -> String {
    let n = (r * 100.0).round() as i64;
    if n > 0 {
        format!("+{n}%")
    } else {
        format!("{n}%")
    }
}

pub fn cause_words(c: Cause) -> &'static str {
    c.words()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: i64 = 1024 * 1024;

    fn change(value: i64, bytes: bool, new: bool, unchanged: bool) -> Change {
        Change {
            value,
            bytes,
            new,
            unchanged,
        }
    }

    #[test]
    fn text_and_colours() {
        let c = change(640 * MB, true, false, false);
        assert_eq!(c.text(), "+640M");
        assert_eq!(c.style(), style::warn());
        assert_eq!(change(200 * MB, true, false, false).style(), style::amber());
        assert_eq!(change(-12 * MB, true, false, false).text(), "-12M");
        assert_eq!(change(-12 * MB, true, false, false).style(), style::ok());
        assert_eq!(change(MB, true, true, false).style(), style::warn());
        assert_eq!(change(50_000, true, false, true).style(), style::dim());
        assert_eq!(change(0, true, false, true).text(), "·");
        assert_eq!(change(1_500, false, false, false).text(), "+1.5K");
        // Without sizes there is no byte threshold.
        assert_eq!(
            change(400 * MB, false, false, false).style(),
            style::amber()
        );
    }

    #[test]
    fn sums() {
        let s =
            Change::sum([change(10, true, true, false), change(-4, true, false, true)]).unwrap();
        assert_eq!(s, change(6, true, false, false));
        assert_eq!(Change::sum([]), None);
    }

    #[test]
    fn percents() {
        assert_eq!(signed_percent(0.384), "+38%");
        assert_eq!(signed_percent(-0.04), "-4%");
        assert_eq!(signed_percent(0.001), "0%");
        assert_eq!(ratio_change(Some(100), Some(150)), Some(0.5));
        assert_eq!(ratio_change(None, Some(1)), None);
        assert_eq!(ratio_change(Some(0), Some(1)), None);
    }
}
