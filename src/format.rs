//! Human-readable numbers shared by the TUI and the commands: sizes, counts, durations,
//! percentages, ratios and sparklines.

use std::time::Duration;

/// Bytes as `512B`, `3.1K`, `41K`, `1.2M`, `14.8G` (powers of 1024, one decimal below 10).
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["K", "M", "G", "T", "P"];
    if n < 1024 {
        return format!("{n}B");
    }
    let mut v = n as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if v < 10.0 {
        let s = format!("{v:.1}");
        // 9.96 rounds to "10.0": show it as 10 in the same unit instead.
        if s == "10.0" {
            format!("10{}", UNITS[unit])
        } else {
            format!("{s}{}", UNITS[unit])
        }
    } else {
        format!("{}{}", v.round() as u64, UNITS[unit])
    }
}

/// Counts as `9`, `212`, `1.5K`, `48K`, `1.84M` (powers of 1000).
///
/// The unit is picked after rounding, so `9_999` is `10K` rather than `10.0K` and
/// `999_999` is `1.00M` rather than `1000K`. Halves round up.
pub fn count(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    let tenths_k = n.saturating_add(50) / 100;
    if tenths_k < 100 {
        return format!("{}.{}K", tenths_k / 10, tenths_k % 10);
    }
    let k = n.saturating_add(500) / 1_000;
    if k < 1_000 {
        return format!("{k}K");
    }
    let hundredths_m = n.saturating_add(5_000) / 10_000;
    format!("{}.{:02}M", hundredths_m / 100, hundredths_m % 100)
}

/// A count with thousands separators: `85,964`.
pub fn grouped(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A TTL or age as `38s`, `24m 10s`, `11h 04m`, `6d 23h`.
pub fn duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3_599 => format!("{}m {:02}s", s / 60, s % 60),
        3_600..=86_399 => format!("{}h {:02}m", s / 3_600, (s % 3_600) / 60),
        _ => format!("{}d {:02}h", s / 86_400, (s % 86_400) / 3_600),
    }
}

/// A ratio from 0.0 to 1.0 as a whole percentage, with `<1%` for small non-zero values.
pub fn percent(ratio: f64) -> String {
    if ratio > 0.0 && ratio < 0.01 {
        "<1%".to_string()
    } else {
        format!("{}%", (ratio * 100.0).round() as u64)
    }
}

/// A pattern such as `user:{id}:cart` as a `SCAN MATCH` glob: `user:*:cart`.
pub fn glob(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut in_placeholder = false;
    for c in pattern.chars() {
        match c {
            '{' if !in_placeholder => {
                in_placeholder = true;
                out.push('*');
            }
            '}' if in_placeholder => in_placeholder = false,
            _ if in_placeholder => {}
            _ => out.push(c),
        }
    }
    out
}

/// The block characters of a sparkline, lowest first.
pub const SPARK_LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// A sparkline cell for a missing point (a pattern not in that snapshot).
pub const SPARK_GAP: char = '·';

/// One character per value, scaled from the smallest to the largest present value: `▁`
/// for the smallest, `█` for the largest, `·` for a gap. When every present value is the
/// same the line is flat at `▄`. Scaling from the minimum rather than from zero makes a
/// slow, steady leak visible; the growth rate says how big it is.
pub fn sparkline(values: &[Option<u64>]) -> String {
    let present = values.iter().flatten();
    let (Some(&min), Some(&max)) = (present.clone().min(), present.max()) else {
        return values.iter().map(|_| SPARK_GAP).collect();
    };
    values
        .iter()
        .map(|v| match v {
            None => SPARK_GAP,
            Some(_) if max == min => SPARK_LEVELS[3],
            Some(v) => {
                let r = (v - min) as f64 / (max - min) as f64;
                SPARK_LEVELS[((r * 7.0).round() as usize).min(7)]
            }
        })
        .collect()
}

/// A signed percentage for small rates: `+2.1%`, `-0.4%`, `0.0%`, and whole numbers from
/// 10% up: `+12%`. `ratio` is a fraction (0.021 is 2.1%).
pub fn signed_percent1(ratio: f64) -> String {
    let pct = ratio * 100.0;
    let text = if pct.abs() >= 9.95 {
        format!("{}%", pct.abs().round())
    } else {
        format!("{:.1}%", pct.abs())
    };
    if text == "0.0%" {
        text
    } else if pct > 0.0 {
        format!("+{text}")
    } else {
        format!("-{text}")
    }
}

/// `n` formatted by `fmt` with its sign: `+94M`, `-1.2K`, `0B`. Rounds to whole units
/// first, so it suits rates such as bytes per day.
pub fn signed_with(n: f64, fmt: impl Fn(u64) -> String) -> String {
    let r = n.round();
    if r == 0.0 || !r.is_finite() {
        return fmt(0);
    }
    // `as u64` saturates, so huge rates stay finite text.
    let text = fmt(r.abs() as u64);
    if r > 0.0 {
        format!("+{text}")
    } else {
        format!("-{text}")
    }
}

/// A ratio such as right / left: `×1.00`, `×0.12`, `×12.5`, `×340`, `×<0.01`.
pub fn ratio(r: f64) -> String {
    if !r.is_finite() {
        return "—".to_string();
    }
    if r > 0.0 && r < 0.005 {
        "×<0.01".to_string()
    } else if r < 9.995 {
        format!("×{r:.2}")
    } else if r < 99.95 {
        format!("×{r:.1}")
    } else {
        format!("×{}", r.round() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_formatting() {
        assert_eq!(bytes(0), "0B");
        assert_eq!(bytes(1023), "1023B");
        assert_eq!(bytes(1024), "1.0K");
        assert_eq!(bytes(3_174), "3.1K");
        assert_eq!(bytes(10_200), "10K");
        assert_eq!(bytes(41_984), "41K");
        assert_eq!(bytes(21_063_468), "20M");
        assert_eq!(bytes(15_891_378_995), "15G");
        assert_eq!(bytes(10_238), "10K");
    }

    #[test]
    fn count_formatting() {
        assert_eq!(count(9), "9");
        assert_eq!(count(212), "212");
        assert_eq!(count(1_462), "1.5K");
        assert_eq!(count(48_000), "48K");
        assert_eq!(count(1_840_000), "1.84M");
    }

    #[test]
    fn count_unit_boundaries_round_first() {
        assert_eq!(count(999), "999");
        assert_eq!(count(1_000), "1.0K");
        assert_eq!(count(9_949), "9.9K");
        assert_eq!(count(9_950), "10K");
        assert_eq!(count(9_999), "10K");
        assert_eq!(count(10_000), "10K");
        assert_eq!(count(999_499), "999K");
        assert_eq!(count(999_500), "1.00M");
        assert_eq!(count(999_999), "1.00M");
        assert_eq!(count(1_000_000), "1.00M");
        assert_eq!(count(1_004_999), "1.00M");
        assert_eq!(count(1_005_000), "1.01M");
        assert!(count(u64::MAX).ends_with('M'));
    }

    #[test]
    fn grouped_formatting() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(85_964), "85,964");
        assert_eq!(grouped(1_234_567), "1,234,567");
    }

    #[test]
    fn duration_formatting() {
        let s = Duration::from_secs;
        assert_eq!(duration(s(38)), "38s");
        assert_eq!(duration(s(24 * 60 + 10)), "24m 10s");
        assert_eq!(duration(s(11 * 3600 + 4 * 60)), "11h 04m");
        assert_eq!(duration(s(6 * 86_400 + 23 * 3600)), "6d 23h");
    }

    #[test]
    fn percent_formatting() {
        assert_eq!(percent(0.0), "0%");
        assert_eq!(percent(0.004), "<1%");
        assert_eq!(percent(0.31), "31%");
        assert_eq!(percent(1.0), "100%");
    }

    #[test]
    fn glob_from_pattern() {
        assert_eq!(glob("user:{id}:cart"), "user:*:cart");
        assert_eq!(glob("rl:{ip}:{id}"), "rl:*:*");
        assert_eq!(glob("queue:emails"), "queue:emails");
    }
    #[test]
    fn sparkline_shapes() {
        // Empty, one point, flat, rising, gaps.
        assert_eq!(sparkline(&[]), "");
        assert_eq!(sparkline(&[Some(5)]), "▄");
        assert_eq!(sparkline(&[Some(7), Some(7), Some(7)]), "▄▄▄");
        assert_eq!(sparkline(&[Some(0), Some(0)]), "▄▄");
        assert_eq!(sparkline(&(0..8).map(Some).collect::<Vec<_>>()), "▁▂▃▄▅▆▇█");
        assert_eq!(sparkline(&[Some(10), None, Some(20)]), "▁·█");
        assert_eq!(sparkline(&[None, None]), "··");
        assert_eq!(sparkline(&[None, Some(3), None]), "·▄·");
        assert_eq!(sparkline(&[Some(30), Some(20), Some(10)]), "█▅▁");
        // Huge values don't overflow.
        assert_eq!(sparkline(&[Some(0), Some(u64::MAX)]), "▁█");
        assert_eq!(sparkline(&[Some(u64::MAX), Some(u64::MAX)]), "▄▄");
    }

    #[test]
    fn small_signed_percentages() {
        assert_eq!(signed_percent1(0.021), "+2.1%");
        assert_eq!(signed_percent1(-0.004), "-0.4%");
        assert_eq!(signed_percent1(0.0), "0.0%");
        assert_eq!(signed_percent1(0.0001), "0.0%");
        assert_eq!(signed_percent1(-0.0001), "0.0%");
        assert_eq!(signed_percent1(0.12), "+12%");
        assert_eq!(signed_percent1(-1.5), "-150%");
        assert_eq!(signed_percent1(0.0996), "+10%");
    }

    #[test]
    fn signed_rates() {
        assert_eq!(signed_with(98_566_144.0, bytes), "+94M");
        assert_eq!(signed_with(-1_234.0, count), "-1.2K");
        assert_eq!(signed_with(0.4, bytes), "0B");
        assert_eq!(signed_with(-0.4, count), "0");
        assert_eq!(signed_with(f64::NAN, count), "0");
        assert!(signed_with(1e30, bytes).starts_with('+'));
    }

    #[test]
    fn ratios() {
        assert_eq!(ratio(1.0), "×1.00");
        assert_eq!(ratio(0.123), "×0.12");
        assert_eq!(ratio(12.46), "×12.5");
        assert_eq!(ratio(340.2), "×340");
        assert_eq!(ratio(0.001), "×<0.01");
        assert_eq!(ratio(0.0), "×0.00");
        assert_eq!(ratio(9.999), "×10.0");
        assert_eq!(ratio(f64::INFINITY), "—");
        assert_eq!(ratio(f64::NAN), "—");
    }
}
