//! CSV report: a header row, then one row per pattern, RFC 4180 quoting and CRLF line ends.
//! Text cells that start with `= + - @`, a tab or a carriage return get a leading `'`, so
//! a spreadsheet doesn't run a key name as a formula. Numbers we compute are written as is.

use std::fmt::Write;

use super::rows;
use crate::diff::ReportDiff;
use crate::model::ScanReport;

/// Quote a field when it holds a comma, quote, CR or LF.
fn quote(s: &str) -> String {
    if s.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// A text cell: neutralise formulas, then quote.
pub(crate) fn text(s: &str) -> String {
    if s.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        quote(&format!("'{s}"))
    } else {
        quote(s)
    }
}

pub fn render(report: &ScanReport, diff: Option<&ReportDiff>, examples: bool) -> String {
    let mut out = String::new();
    out.push_str(
        "pattern,type,sampled_keys,est_keys,est_bytes,avg_bytes,share_pct,no_ttl_pct,verdict,delta_bytes,delta_keys,change",
    );
    if examples {
        out.push_str(",examples");
    }
    out.push_str("\r\n");
    let opt = |v: Option<u64>| v.map_or(String::new(), |n| n.to_string());
    for r in rows(report, diff) {
        let p = r.stats;
        let _ = write!(
            out,
            "{},{},{},{},{},{},{:.2},{:.2},{},{},{},{}",
            text(&p.pattern),
            p.key_type.map_or("mixed", |t| t.as_str()),
            p.sampled_keys,
            p.est_keys,
            opt(p.est_bytes),
            opt(r.avg_bytes),
            r.share * 100.0,
            p.no_ttl_ratio() * 100.0,
            crate::check::verdict_word(r.verdict),
            r.change
                .and_then(|c| c.delta_bytes)
                .map_or(String::new(), |d| d.to_string()),
            r.change.map_or(String::new(), |c| c.delta_keys.to_string()),
            r.change
                .map_or(String::new(), |c| c.cause.words().to_string()),
        );
        if examples {
            let keys: Vec<&str> = p.examples.iter().map(|k| k.key.as_str()).collect();
            let _ = write!(out, ",{}", text(&keys.join(" ")));
        }
        out.push_str("\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;

    #[test]
    fn quoting_follows_rfc_4180() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("a,b"), "\"a,b\"");
        assert_eq!(quote("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(quote("a\nb"), "\"a\nb\"");
        assert_eq!(quote("a\rb"), "\"a\rb\"");
    }

    #[test]
    fn formula_prefixes_are_neutralised() {
        for s in ["=cmd()", "+1", "-2", "@SUM(A1)", "\tx", "\rx"] {
            assert!(text(s).trim_matches('"').starts_with('\''), "{s:?}");
        }
        assert_eq!(text("=1,2"), "\"'=1,2\"");
        assert_eq!(text("user:{id}"), "user:{id}");
        assert_eq!(text("a=b"), "a=b");
        assert_eq!(text(""), "");
    }

    #[test]
    fn snapshot_with_diff() {
        let (before, after) = before_after();
        let d = crate::diff::diff(&before, &after);
        insta::assert_snapshot!(render(&after, Some(&d), false));
    }

    #[test]
    fn snapshot_without_baseline_with_examples() {
        let (_, after) = before_after();
        insta::assert_snapshot!(render(&after, None, true));
    }

    #[test]
    fn examples_column_only_on_request() {
        let (_, after) = before_after();
        let plain = render(&after, None, false);
        assert!(!plain.contains("examples") && !plain.contains("user:1001:cart"));
        assert!(render(&after, None, true).contains("examples"));
    }

    #[test]
    fn hostile_names_cannot_become_formulas() {
        let out = render(&hostile(), None, true);
        // A record separator is CRLF; the embedded newline is inside quotes.
        assert!(out.contains("\"line1\nline2\""));
        for starter in ["'=cmd()", "'+SUM(1,1)", "'@x", "'-2+3"] {
            assert!(
                out.contains(starter) || out.contains(&format!("\"{starter}")),
                "{starter}\n{out}"
            );
        }
        // No cell starts with an active formula character after a comma or at a line start.
        let bytes = out.as_bytes();
        for i in 0..bytes.len() {
            let at_start = i == 0 || bytes[i - 1] == b',' || bytes[i - 1] == b'\n';
            if at_start && i + 1 < bytes.len() && matches!(bytes[i], b'=' | b'+' | b'@') {
                panic!("formula start at {i}: {out}");
            }
        }
    }

    #[test]
    fn no_sizes_leaves_cells_empty() {
        let r = report(vec![stats("a", 10, 1, None, &[])], false, 0);
        let out = render(&r, None, false);
        let row = out.lines().nth(1).unwrap();
        assert!(row.starts_with("a,hash,10,100,,,"), "{row}");
    }
}
