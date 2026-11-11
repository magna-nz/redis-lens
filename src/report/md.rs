//! Markdown report. Key names and patterns are escaped so they can't break a table cell,
//! open a link or inject HTML into a PR comment.

use std::fmt::Write;

use super::{Row, TOP_PATTERNS, change_text, changes, facts, risks, rows};
use crate::diff::ReportDiff;
use crate::format;
use crate::model::ScanReport;
use crate::scan::context::context_summary;
use crate::snapshot::format_utc;

/// Escape text from the server or from key names: `\ | ` < > [ ]` get a backslash, `&`
/// becomes `&amp;`, and line breaks and other control characters become spaces.
pub(crate) fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '|' | '`' | '<' | '>' | '[' | ']' => {
                out.push('\\');
                out.push(c);
            }
            '&' => out.push_str("&amp;"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

pub fn render(report: &ScanReport, diff: Option<&ReportDiff>, examples: bool) -> String {
    let mut out = String::new();
    let rows = rows(report, diff);
    let _ = writeln!(out, "# redis-lens report: {}\n", esc(&report.profile));
    for (label, value) in facts(report) {
        if label == "Profile" {
            continue;
        }
        let _ = writeln!(out, "- **{label}:** {}", esc(&value));
    }
    for line in context_summary(report) {
        let _ = writeln!(out, "- {}", esc(&line));
    }

    let _ = writeln!(out, "\n## Top patterns\n");
    if rows.is_empty() {
        let _ = writeln!(out, "No keys were sampled.");
    } else {
        let _ = writeln!(
            out,
            "| Pattern | Type | Keys | Memory | Share | Avg | No TTL |\n|---|---|---:|---:|---:|---:|---:|"
        );
        for r in rows.iter().take(TOP_PATTERNS) {
            let p = r.stats;
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} | {} |",
                esc(&p.pattern),
                p.key_type.map_or("mixed", |t| t.as_str()),
                format::count(p.est_keys),
                p.est_bytes.map_or("—".to_string(), format::bytes),
                format::percent(r.share),
                r.avg_bytes.map_or("—".to_string(), format::bytes),
                format::percent(p.no_ttl_ratio()),
            );
        }
        if rows.len() > TOP_PATTERNS {
            let _ = writeln!(
                out,
                "\n{} more patterns are in the CSV report or `--json`.",
                rows.len() - TOP_PATTERNS
            );
        }
    }

    let _ = writeln!(out, "\n## TTL risks\n");
    let flagged = risks(&rows);
    if flagged.is_empty() {
        let _ = writeln!(out, "No pattern is flagged.");
    } else {
        let _ = writeln!(
            out,
            "| Pattern | Verdict | No TTL | Why |\n|---|---|---:|---|"
        );
        for (r, why) in flagged {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} |",
                esc(&r.stats.pattern),
                r.verdict.label(),
                format::percent(r.stats.no_ttl_ratio()),
                esc(&why)
            );
        }
    }

    if let Some(d) = diff {
        let _ = writeln!(
            out,
            "\n## Changes since {}\n",
            format_utc(d.before_taken_at_unix)
        );
        let _ = writeln!(
            out,
            "Memory used: {} → {} ({}).\n",
            format::bytes(d.used_memory_before),
            format::bytes(d.used_memory_after),
            super::signed_bytes(crate::diff::sub(d.used_memory_after, d.used_memory_before))
        );
        let ch = changes(d);
        if ch.is_empty() {
            let _ = writeln!(out, "No pattern changed.");
        } else {
            let _ = writeln!(out, "| Pattern | Change | Cause |\n|---|---:|---|");
            for c in ch {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} |",
                    esc(&c.pattern),
                    change_text(c),
                    c.cause.words()
                );
            }
        }
    }

    if examples {
        example_section(&mut out, &rows);
    }
    out
}

fn example_section(out: &mut String, rows: &[Row<'_>]) {
    let with: Vec<&Row<'_>> = rows
        .iter()
        .take(TOP_PATTERNS)
        .filter(|r| !r.stats.examples.is_empty())
        .collect();
    if with.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n## Example keys\n");
    for r in with {
        let keys: Vec<String> = r.stats.examples.iter().map(|k| esc(&k.key)).collect();
        let _ = writeln!(out, "- {}: {}", esc(&r.stats.pattern), keys.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;

    #[test]
    fn escapes_markdown_syntax() {
        assert_eq!(esc("a|b"), "a\\|b");
        assert_eq!(esc("`x`"), "\\`x\\`");
        assert_eq!(esc("<b>&"), "\\<b\\>&amp;");
        assert_eq!(esc("[l](u)"), "\\[l\\](u)");
        assert_eq!(esc("a\nb\r\tc"), "a b  c");
        assert_eq!(esc("\\|"), "\\\\\\|");
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
    fn snapshot_without_sizes() {
        let r = report(vec![stats("a:{id}", 100, 50, None, &[])], false, 0);
        insta::assert_snapshot!(render(&r, None, false));
    }

    #[test]
    fn examples_only_on_request() {
        let (_, after) = before_after();
        assert!(!render(&after, None, false).contains("user:1001:cart"));
        assert!(render(&after, None, true).contains("user:1001:cart"));
    }

    #[test]
    fn hostile_names_cannot_break_out() {
        let r = hostile();
        let out = render(&r, None, true);
        // No raw HTML tag survives.
        assert!(!out.contains("<script>"), "{out}");
        assert!(out.contains("\\<script\\>alert(1)\\</script\\>"));
        // Table rows stay one line each: a header and one row per pattern.
        let table_rows = out.lines().filter(|l| l.starts_with("| ")).count();
        assert_eq!(table_rows, 1 + r.patterns.len(), "{out}");
        // Pipes are escaped, so a cell can't be split.
        for l in out.lines().filter(|l| l.starts_with("| ")) {
            let bare = l.replace("\\|", "");
            let unescaped = bare.matches('|').count();
            assert_eq!(unescaped, 8, "{l}");
        }
        assert!(out.contains("line1 line2"));
        assert!(!out.contains("](e)") || out.contains("\\](e)"));
    }
}
