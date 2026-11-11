//! Self-contained HTML report: inline CSS and a small inline sort script, no external
//! requests (a Content-Security-Policy meta tag forbids them). Every value that comes from
//! the server or from key names is escaped with [`esc`].

use std::fmt::Write;

use super::{Row, TOP_PATTERNS, change_text, changes, facts, risks, rows};
use crate::diff::ReportDiff;
use crate::format;
use crate::model::ScanReport;
use crate::scan::context::context_summary;
use crate::snapshot::format_utc;

/// Escape `& < > " '`, and turn control characters into spaces.
pub(crate) fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

const STYLE: &str = "body{font:14px/1.45 system-ui,sans-serif;margin:2rem auto;max-width:64rem;padding:0 1rem;color:#1b1f23}\
h1{font-size:1.4rem}h2{font-size:1.1rem;margin-top:2rem}\
table{border-collapse:collapse;width:100%}th,td{padding:.3rem .6rem;border-bottom:1px solid #e1e4e8;text-align:left}\
th{cursor:pointer;background:#f6f8fa;white-space:nowrap}td.n,th.n{text-align:right}\
.bar{background:#e1e4e8;width:8rem;height:.7rem;border-radius:2px}.bar span{display:block;height:100%;background:#0366d6;border-radius:2px}\
dl{display:grid;grid-template-columns:max-content 1fr;gap:.2rem 1rem}dt{font-weight:600}dd{margin:0}\
.warn{color:#b31d28;font-weight:600}.muted{color:#586069}code{font-family:ui-monospace,monospace}";

const SCRIPT: &str = "document.querySelectorAll('table.sortable th').forEach(function(th,i){th.addEventListener('click',function(){\
var b=th.closest('table').tBodies[0],rs=Array.prototype.slice.call(b.rows),\
d=th.dataset.dir=th.dataset.dir==='asc'?'desc':'asc';\
rs.sort(function(x,y){var a=x.cells[i].dataset.v||x.cells[i].textContent,c=y.cells[i].dataset.v||y.cells[i].textContent,\
na=parseFloat(a),nc=parseFloat(c),r=(isNaN(na)||isNaN(nc))?a.localeCompare(c):na-nc;return d==='asc'?r:-r;});\
rs.forEach(function(r){b.appendChild(r);});});});";

pub fn render(report: &ScanReport, diff: Option<&ReportDiff>, examples: bool) -> String {
    let mut out = String::new();
    let rows = rows(report, diff);
    let _ = write!(
        out,
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'\">\n\
<title>redis-lens report: {}</title>\n<style>{STYLE}</style>\n</head>\n<body>\n",
        esc(&report.profile)
    );
    let _ = writeln!(out, "<h1>redis-lens report: {}</h1>", esc(&report.profile));
    out.push_str("<dl>\n");
    for (label, value) in facts(report) {
        if label == "Profile" {
            continue;
        }
        let _ = writeln!(out, "<dt>{label}</dt><dd>{}</dd>", esc(&value));
    }
    for line in context_summary(report) {
        let _ = writeln!(out, "<dt>Server</dt><dd>{}</dd>", esc(&line));
    }
    out.push_str("</dl>\n");

    out.push_str("<h2>Top patterns</h2>\n");
    if rows.is_empty() {
        out.push_str("<p>No keys were sampled.</p>\n");
    } else {
        out.push_str(
            "<table class=\"sortable\">\n<thead><tr><th>Pattern</th><th>Type</th><th class=\"n\">Keys</th>\
<th class=\"n\">Memory</th><th>Share</th><th class=\"n\">Avg</th><th class=\"n\">No TTL</th></tr></thead>\n<tbody>\n",
        );
        for r in rows.iter().take(TOP_PATTERNS) {
            let p = r.stats;
            let _ = writeln!(
                out,
                "<tr><td>{}</td><td>{}</td><td class=\"n\" data-v=\"{}\">{}</td><td class=\"n\" data-v=\"{}\">{}</td>\
<td data-v=\"{:.4}\"><div class=\"bar\"><span style=\"width:{:.1}%\"></span></div> {}</td>\
<td class=\"n\" data-v=\"{}\">{}</td><td class=\"n\" data-v=\"{:.4}\">{}</td></tr>",
                esc(&p.pattern),
                p.key_type.map_or("mixed", |t| t.as_str()),
                p.est_keys,
                format::count(p.est_keys),
                p.est_bytes.unwrap_or(0),
                p.est_bytes.map_or("—".to_string(), format::bytes),
                r.share,
                (r.share * 100.0).clamp(0.0, 100.0),
                format::percent(r.share),
                r.avg_bytes.unwrap_or(0),
                r.avg_bytes.map_or("—".to_string(), format::bytes),
                p.no_ttl_ratio(),
                format::percent(p.no_ttl_ratio()),
            );
        }
        out.push_str("</tbody>\n</table>\n");
        if rows.len() > TOP_PATTERNS {
            let _ = writeln!(
                out,
                "<p class=\"muted\">{} more patterns are in the CSV report or <code>--json</code>.</p>",
                rows.len() - TOP_PATTERNS
            );
        }
    }

    out.push_str("<h2>TTL risks</h2>\n");
    let flagged = risks(&rows);
    if flagged.is_empty() {
        out.push_str("<p>No pattern is flagged.</p>\n");
    } else {
        out.push_str(
            "<table class=\"sortable\">\n<thead><tr><th>Pattern</th><th>Verdict</th><th class=\"n\">No TTL</th><th>Why</th></tr></thead>\n<tbody>\n",
        );
        for (r, why) in flagged {
            let _ = writeln!(
                out,
                "<tr><td>{}</td><td class=\"warn\">{}</td><td class=\"n\" data-v=\"{:.4}\">{}</td><td>{}</td></tr>",
                esc(&r.stats.pattern),
                r.verdict.label(),
                r.stats.no_ttl_ratio(),
                format::percent(r.stats.no_ttl_ratio()),
                esc(&why)
            );
        }
        out.push_str("</tbody>\n</table>\n");
    }

    if let Some(d) = diff {
        let _ = writeln!(
            out,
            "<h2>Changes since {}</h2>",
            esc(&format_utc(d.before_taken_at_unix))
        );
        let _ = writeln!(
            out,
            "<p>Memory used: {} → {} ({}).</p>",
            format::bytes(d.used_memory_before),
            format::bytes(d.used_memory_after),
            super::signed_bytes(crate::diff::sub(d.used_memory_after, d.used_memory_before))
        );
        let ch = changes(d);
        if ch.is_empty() {
            out.push_str("<p>No pattern changed.</p>\n");
        } else {
            out.push_str(
                "<table class=\"sortable\">\n<thead><tr><th>Pattern</th><th class=\"n\">Change</th><th>Cause</th></tr></thead>\n<tbody>\n",
            );
            for c in ch {
                let _ = writeln!(
                    out,
                    "<tr><td>{}</td><td class=\"n\" data-v=\"{}\">{}</td><td>{}</td></tr>",
                    esc(&c.pattern),
                    c.delta_bytes.unwrap_or(c.delta_keys),
                    change_text(c),
                    c.cause.words()
                );
            }
            out.push_str("</tbody>\n</table>\n");
        }
    }

    if examples {
        example_section(&mut out, &rows);
    }
    let _ = write!(out, "<script>{SCRIPT}</script>\n</body>\n</html>\n");
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
    out.push_str("<h2>Example keys</h2>\n<ul>\n");
    for r in with {
        let keys: Vec<String> = r
            .stats
            .examples
            .iter()
            .map(|k| format!("<code>{}</code>", esc(&k.key)))
            .collect();
        let _ = writeln!(
            out,
            "<li>{}: {}</li>",
            esc(&r.stats.pattern),
            keys.join(", ")
        );
    }
    out.push_str("</ul>\n");
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;

    #[test]
    fn escapes_all_five() {
        assert_eq!(esc("&<>\"'"), "&amp;&lt;&gt;&quot;&#39;");
        assert_eq!(esc("a\nb"), "a b");
    }

    #[test]
    fn snapshot_with_diff_and_examples() {
        let (before, after) = before_after();
        let d = crate::diff::diff(&before, &after);
        insta::assert_snapshot!(render(&after, Some(&d), true));
    }

    #[test]
    fn snapshot_without_baseline() {
        let (_, after) = before_after();
        insta::assert_snapshot!(render(&after, None, false));
    }

    #[test]
    fn self_contained() {
        let (before, after) = before_after();
        let d = crate::diff::diff(&before, &after);
        let out = render(&after, Some(&d), true);
        for needle in ["http://", "https://", "src=", "href=", "@import", "url("] {
            assert!(!out.contains(needle), "{needle}");
        }
        assert!(out.contains("<style>") && out.contains("<script>"));
        assert!(out.contains("class=\"bar\""));
    }

    #[test]
    fn examples_only_on_request() {
        let (_, after) = before_after();
        assert!(!render(&after, None, false).contains("user:1001:cart"));
        assert!(render(&after, None, true).contains("user:1001:cart"));
    }

    #[test]
    fn hostile_names_are_escaped() {
        let out = render(&hostile(), None, true);
        // The only tags in the document are ours: the one <script> is the sort script.
        assert_eq!(out.matches("<script").count(), 1, "{out}");
        assert!(out.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(out.contains("q&quot;uote&#39;s &amp; amp"));
        assert!(!out.contains("<f>"));
        assert!(!out.contains("<script>alert"));
    }

    #[test]
    fn hostile_profile_name_in_title_and_heading() {
        let mut r = hostile();
        r.profile = "</title><script>x()</script>".into();
        let out = render(&r, None, false);
        assert_eq!(out.matches("<script").count(), 1);
        assert!(out.contains("&lt;/title&gt;&lt;script&gt;"));
    }
}
