//! Markdown report. Key names and patterns are escaped so they can't break a table cell,
//! open a link or inject HTML into a PR comment.

use std::fmt::Write;

use super::{NODE_HEADS, Row, TOP_PATTERNS, change_text, changes, facts, node_cells, risks, rows};
use crate::diff::ReportDiff;
use crate::format;
use crate::model::ScanReport;
use crate::scan::context::context_summary;
use crate::snapshot::format_utc;

/// Escape other text (server facts, reasons): `\ | ` < > [ ] * _ ~` get a backslash,
/// `&` becomes `&amp;`, and line breaks and other control characters become spaces.
/// Pattern and key names go through [`code`] instead.
pub(crate) fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '|' | '`' | '<' | '>' | '[' | ']' | '*' | '_' | '~' => {
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

/// A pattern or key name as an inline code span, so nothing in it is read as Markdown:
/// no emphasis, link, auto-link (`https://…`) or HTML. Key names come from whoever writes
/// keys. The fence is one backtick longer than the longest run of backticks in the name,
/// with a space inside when the name starts or ends with one. Control characters become
/// spaces. In a table cell (`in_table`), `|` is written `\|`, which GFM reads as a pipe
/// inside the span instead of a cell break.
pub(crate) fn code(s: &str, in_table: bool) -> String {
    let mut clean: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if in_table {
        clean = clean.replace('|', "\\|");
    }
    if clean.trim().is_empty() {
        return "(empty)".to_string();
    }
    let mut longest = 0;
    let mut run = 0;
    for c in clean.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(longest + 1);
    let pad = if clean.starts_with('`') || clean.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{clean}{pad}{fence}")
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

    if let Some(nodes) = &report.nodes {
        let _ = writeln!(out, "\n## Nodes\n");
        let heads: Vec<String> = NODE_HEADS
            .iter()
            .map(|h| {
                let mut c = h.chars();
                c.next()
                    .map(|f| f.to_string() + &c.as_str().to_lowercase())
                    .unwrap_or_default()
            })
            .collect();
        let _ = writeln!(
            out,
            "| {} |\n|---|---|---|---|---:|---:|---:|---|",
            heads.join(" | ")
        );
        for n in nodes {
            let cells: Vec<String> = node_cells(n).iter().map(|c| esc(c)).collect();
            let _ = writeln!(out, "| {} |", cells.join(" | "));
        }
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
                code(&p.pattern, true),
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
                code(&r.stats.pattern, true),
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
                    code(&c.pattern, true),
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
        let keys: Vec<String> = r
            .stats
            .examples
            .iter()
            .map(|k| code(&k.key, false))
            .collect();
        let _ = writeln!(
            out,
            "- {}: {}",
            code(&r.stats.pattern, false),
            keys.join(", ")
        );
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
    fn snapshot_cluster_nodes() {
        insta::assert_snapshot!(render(&cluster_report(), None, false));
    }

    #[test]
    fn nodes_section_only_on_clusters() {
        let (_, after) = before_after();
        assert!(!render(&after, None, false).contains("## Nodes"));
        assert!(render(&cluster_report(), None, false).contains("## Nodes"));
    }

    #[test]
    fn hostile_node_endpoint_and_note_are_escaped() {
        let mut r = cluster_report();
        let n = &mut r.nodes.as_mut().unwrap()[0];
        n.endpoint.host = "<b>x|y".into();
        n.note = Some("a|b\nc".into());
        let out = render(&r, None, false);
        assert!(out.contains("\\<b\\>x\\|y:7000"), "{out}");
        assert!(out.contains("a\\|b c"), "{out}");
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
        // Every hostile name is inside a code span, where Markdown is not read: remove
        // the spans and no HTML, link or emphasis syntax from a name is left.
        let mut rest = out.clone();
        for p in &r.patterns {
            rest = rest.replace(&code(&p.pattern, true), "");
            rest = rest.replace(&code(&p.pattern, false), "");
            for k in &p.examples {
                rest = rest.replace(&code(&k.key, false), "");
            }
        }
        for bad in ["<script>", "](e)", "<f>", "=cmd()", "line1", "\tTAB"] {
            assert!(!rest.contains(bad), "{bad:?} outside a code span:\n{rest}");
        }
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
    }

    #[test]
    fn names_become_code_spans() {
        assert_eq!(code("user:{id}", true), "`user:{id}`");
        assert_eq!(code("https://x.io", false), "`https://x.io`");
        assert_eq!(code("*a*_b_", false), "`*a*_b_`");
        assert_eq!(code("a`b", false), "``a`b``");
        assert_eq!(code("`a", false), "`` `a ``");
        assert_eq!(code("a|b", true), "`a\\|b`");
        assert_eq!(code("a|b", false), "`a|b`");
        assert_eq!(code("a\nb", false), "`a b`");
        assert_eq!(code("", false), "(empty)");
        assert_eq!(esc("*a*_b_~"), "\\*a\\*\\_b\\_\\~");
    }
}
