//! Human-readable numbers shared by the TUI and the `scan` command.

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
pub fn count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=9_999 => format!("{:.1}K", n as f64 / 1e3),
        10_000..=999_999 => format!("{}K", (n as f64 / 1e3).round() as u64),
        _ => format!("{:.2}M", n as f64 / 1e6),
    }
}

/// A count with thousands separators: `85,964`.
pub fn grouped(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
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
}
