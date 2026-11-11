//! `scan --check`: rules over a finished [`ScanReport`] (and, for growth, the diff against
//! a baseline snapshot), so a CI job can gate a deploy on the exit code.
//!
//! Everything here is pure: no server commands, no I/O except reading a rules file.
//!
//! Grammar of one rule: `<metric><op><value>[,min-keys=N]`.
//!
//! | metric        | scope        | value                          | ops                      |
//! |---------------|--------------|--------------------------------|--------------------------|
//! | `no-ttl-pct`  | per pattern  | percent, `20` or `20%`         | `< <= > >= = !=`         |
//! | `growth`      | per pattern  | percent vs. the baseline       | `< <= > >= = !=`         |
//! | `pattern-bytes` | per pattern | size, `500MiB`                | `< <= > >= = !=`         |
//! | `verdict`     | per pattern  | `ok`, `mixed`, `persistent-growing`, `persistent-stable`, `short-lived` | `= !=` |
//! | `total-bytes` | whole scan   | size                           | `< <= > >= = !=`         |
//!
//! A rule states what must hold: `no-ttl-pct<20` fails every pattern whose share of keys
//! without a TTL is 20% or more.

use std::path::Path;

use serde::Deserialize;

use crate::diff::{Cause, ReportDiff};
use crate::format;
use crate::model::{PatternStats, ScanReport};
use crate::verdict::{TtlVerdict, ttl_verdict};

/// A per-pattern rule skips patterns with fewer sampled keys than this unless the rule
/// (`,min-keys=N`) or the rules file (`min_keys`) says otherwise.
pub const DEFAULT_MIN_KEYS: u64 = 50;

/// How many offending patterns one failing rule lists before "and N more".
const MAX_LISTED: usize = 10;

/// A rule that did not parse. The message names the rule.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid check rule '{rule}': {message}")]
pub struct RuleError {
    pub rule: String,
    pub message: String,
}

fn bad(rule: &str, message: impl Into<String>) -> RuleError {
    RuleError {
        rule: rule.to_string(),
        message: message.into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

impl Op {
    fn holds(self, actual: f64, limit: f64) -> bool {
        const EPS: f64 = 1e-9;
        match self {
            Op::Lt => actual < limit,
            Op::Le => actual <= limit,
            Op::Gt => actual > limit,
            Op::Ge => actual >= limit,
            Op::Eq => (actual - limit).abs() < EPS,
            Op::Ne => (actual - limit).abs() >= EPS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    NoTtlPct,
    Growth,
    PatternBytes,
    Verdict,
    TotalBytes,
}

/// The verdict names a rule can compare with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictName {
    /// `ok`: also matches the short-lived verdict, which the TTL view labels `ok · short-lived`.
    Ok,
    ShortLived,
    Mixed,
    PersistentGrowing,
    PersistentStable,
}

impl VerdictName {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ok" => Self::Ok,
            "short-lived" => Self::ShortLived,
            "mixed" => Self::Mixed,
            "persistent-growing" => Self::PersistentGrowing,
            "persistent-stable" => Self::PersistentStable,
            _ => return None,
        })
    }

    fn matches(self, v: TtlVerdict) -> bool {
        match self {
            Self::Ok => matches!(v, TtlVerdict::Ok | TtlVerdict::ShortLived),
            Self::ShortLived => v == TtlVerdict::ShortLived,
            Self::Mixed => v == TtlVerdict::MixedLikelyBug,
            Self::PersistentGrowing => v == TtlVerdict::PersistentGrowing,
            Self::PersistentStable => v == TtlVerdict::PersistentStable,
        }
    }
}

/// The name a `verdict` rule uses for a verdict, also used in the CSV report.
pub fn verdict_word(v: TtlVerdict) -> &'static str {
    match v {
        TtlVerdict::Ok => "ok",
        TtlVerdict::ShortLived => "short-lived",
        TtlVerdict::MixedLikelyBug => "mixed",
        TtlVerdict::PersistentGrowing => "persistent-growing",
        TtlVerdict::PersistentStable => "persistent-stable",
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Limit {
    Number(f64),
    Verdict(VerdictName),
}

/// One parsed rule.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    /// The rule as written (trimmed), for output.
    pub text: String,
    pub metric: Metric,
    pub op: Op,
    limit: Limit,
    /// Patterns with fewer sampled keys are not judged. Unused by `total-bytes`.
    pub min_keys: u64,
}

impl Rule {
    /// True when judging the rule needs a baseline snapshot.
    pub fn needs_baseline(&self) -> bool {
        matches!(self.metric, Metric::Growth | Metric::Verdict)
    }
}

/// Parse a size such as `500MiB`, `1.5GB`, `2048` or `64K` into bytes. `KiB`/`MiB`/`GiB`/`TiB`
/// and the bare `K`/`M`/`G`/`T` are powers of 1024 (the units `scan` prints); `KB`/`MB`/`GB`/`TB`
/// are powers of 1000.
fn parse_size(s: &str) -> Option<f64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kib" => 1024.0,
        "m" | "mib" => 1024.0_f64.powi(2),
        "g" | "gib" => 1024.0_f64.powi(3),
        "t" | "tib" => 1024.0_f64.powi(4),
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        _ => return None,
    };
    let v = n * mult;
    v.is_finite().then_some(v)
}

fn parse_percent(s: &str, allow_negative: bool) -> Option<f64> {
    let t = s.trim();
    let t = t.strip_suffix('%').unwrap_or(t).trim();
    let v: f64 = t.parse().ok()?;
    (v.is_finite() && (allow_negative || v >= 0.0)).then_some(v)
}

/// Parse one rule. `default_min_keys` applies when the rule has no `,min-keys=N`.
pub fn parse_rule(text: &str, default_min_keys: u64) -> Result<Rule, RuleError> {
    let text = text.trim();
    let (main, options) = match text.split_once(',') {
        Some((m, o)) => (m, Some(o)),
        None => (text, None),
    };
    let op_at = main
        .find(['<', '>', '=', '!'])
        .ok_or_else(|| bad(text, "no operator; use one of < <= > >= = !="))?;
    let (metric_text, rest) = main.split_at(op_at);
    let metric_text = metric_text.trim();
    let (op, op_len) = if rest.starts_with("<=") {
        (Op::Le, 2)
    } else if rest.starts_with(">=") {
        (Op::Ge, 2)
    } else if rest.starts_with("!=") {
        (Op::Ne, 2)
    } else if rest.starts_with('<') {
        (Op::Lt, 1)
    } else if rest.starts_with('>') {
        (Op::Gt, 1)
    } else if rest.starts_with('=') {
        (Op::Eq, 1)
    } else {
        return Err(bad(
            text,
            "'!' must be followed by '=' (the operator is !=)",
        ));
    };
    let value_text = rest[op_len..].trim();
    if value_text.is_empty() {
        return Err(bad(text, "missing value after the operator"));
    }
    let metric = match metric_text {
        "no-ttl-pct" => Metric::NoTtlPct,
        "growth" => Metric::Growth,
        "pattern-bytes" => Metric::PatternBytes,
        "verdict" => Metric::Verdict,
        "total-bytes" => Metric::TotalBytes,
        "" => return Err(bad(text, "missing metric before the operator")),
        other => {
            return Err(bad(
                text,
                format!(
                    "unknown metric '{other}'; use no-ttl-pct, growth, pattern-bytes, verdict or total-bytes"
                ),
            ));
        }
    };
    let limit = match metric {
        Metric::NoTtlPct => Limit::Number(
            parse_percent(value_text, false)
                .ok_or_else(|| bad(text, format!("'{value_text}' is not a percentage")))?,
        ),
        Metric::Growth => Limit::Number(
            parse_percent(value_text, true)
                .ok_or_else(|| bad(text, format!("'{value_text}' is not a percentage")))?,
        ),
        Metric::PatternBytes | Metric::TotalBytes => Limit::Number(
            parse_size(value_text)
                .ok_or_else(|| bad(text, format!("'{value_text}' is not a size, e.g. 500MiB")))?,
        ),
        Metric::Verdict => {
            if !matches!(op, Op::Eq | Op::Ne) {
                return Err(bad(text, "verdict only works with = and !="));
            }
            Limit::Verdict(VerdictName::parse(value_text).ok_or_else(|| {
                bad(
                    text,
                    format!(
                        "unknown verdict '{value_text}'; use ok, short-lived, mixed, persistent-growing or persistent-stable"
                    ),
                )
            })?)
        }
    };
    let mut min_keys = default_min_keys;
    if let Some(opts) = options {
        let opts = opts.trim();
        let Some(n) = opts.strip_prefix("min-keys=") else {
            return Err(bad(
                text,
                format!("unknown option '{opts}'; the only option is min-keys=N"),
            ));
        };
        if metric == Metric::TotalBytes {
            return Err(bad(text, "min-keys does not apply to total-bytes"));
        }
        min_keys = n
            .trim()
            .parse()
            .map_err(|_| bad(text, format!("'{}' is not a whole number", n.trim())))?;
    }
    Ok(Rule {
        text: text.to_string(),
        metric,
        op,
        limit,
        min_keys,
    })
}

/// The `[check]` table of a rules file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckTable {
    #[serde(default)]
    rules: Vec<String>,
    min_keys: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesFile {
    check: Option<CheckTable>,
}

/// Parse the text of a rules file: TOML with `[check]`, `rules = [...]` and an optional
/// `min_keys = N` default. Returns the rule strings and the default.
pub fn parse_rules_file(text: &str) -> Result<(Vec<String>, Option<u64>), String> {
    let file: RulesFile = toml::from_str(text).map_err(|e| e.to_string())?;
    let table = file.check.ok_or_else(|| "no [check] table".to_string())?;
    Ok((table.rules, table.min_keys))
}

/// All rules for a run: the file's first, then the `--check` flags. `min_keys` from the
/// file is the default for every rule that doesn't carry its own.
pub fn load_rules(flags: &[String], file: Option<&Path>) -> anyhow::Result<Vec<Rule>> {
    let mut texts: Vec<String> = Vec::new();
    let mut default = DEFAULT_MIN_KEYS;
    if let Some(path) = file {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("could not read rules file {}: {e}", path.display()))?;
        let (rules, min_keys) = parse_rules_file(&text)
            .map_err(|e| anyhow::anyhow!("invalid rules file {}: {e}", path.display()))?;
        texts.extend(rules);
        if let Some(n) = min_keys {
            default = n;
        }
    }
    texts.extend(flags.iter().cloned());
    if texts.is_empty() {
        anyhow::bail!("no check rules given");
    }
    texts
        .iter()
        .map(|t| parse_rule(t, default).map_err(anyhow::Error::from))
        .collect()
}

/// One pattern that broke a rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offender {
    pub pattern: String,
    /// The measured value as shown, e.g. `34%`.
    pub value: String,
    pub sampled_keys: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Every judged pattern held. `checked` counts them; `note` explains anything left out.
    Pass {
        checked: usize,
        note: Option<String>,
    },
    Fail {
        offenders: Vec<Offender>,
        note: Option<String>,
    },
    /// The rule could not be judged, with the reason. A skip unless `--strict`.
    NotJudged(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evaluation {
    pub rule: String,
    pub outcome: Outcome,
}

/// Judge `rule` against `report`. `baseline` is the diff of the newest earlier snapshot
/// against `report`, when there is one.
pub fn evaluate(rule: &Rule, report: &ScanReport, baseline: Option<&ReportDiff>) -> Outcome {
    match rule.metric {
        Metric::TotalBytes => total_bytes(rule, report),
        _ => per_pattern(rule, report, baseline),
    }
}

/// Judge every rule.
pub fn evaluate_all(
    rules: &[Rule],
    report: &ScanReport,
    baseline: Option<&ReportDiff>,
) -> Vec<Evaluation> {
    rules
        .iter()
        .map(|r| Evaluation {
            rule: r.text.clone(),
            outcome: evaluate(r, report, baseline),
        })
        .collect()
}

fn number(rule: &Rule) -> f64 {
    match rule.limit {
        Limit::Number(n) => n,
        Limit::Verdict(_) => 0.0,
    }
}

fn total_bytes(rule: &Rule, report: &ScanReport) -> Outcome {
    if !report.sizes_available {
        return Outcome::NotJudged("no MEMORY USAGE".to_string());
    }
    let total: u64 = report.patterns.iter().filter_map(|p| p.est_bytes).sum();
    let mut notes = Vec::new();
    if let Some(g) = &report.match_glob {
        notes.push(format!("MATCH {g}: matching keys only"));
    }
    if !report.complete {
        notes.push("estimated from a partial scan".to_string());
    }
    let note = (!notes.is_empty()).then(|| notes.join(", "));
    if rule.op.holds(total as f64, number(rule)) {
        Outcome::Pass { checked: 1, note }
    } else {
        Outcome::Fail {
            offenders: vec![Offender {
                pattern: "(whole scan)".to_string(),
                value: format::bytes(total),
                sampled_keys: report.sampled_keys,
            }],
            note,
        }
    }
}

/// Why one pattern could not be measured for a rule.
enum Skip {
    NoSizes,
    NoEarlierSize,
}

fn per_pattern(rule: &Rule, report: &ScanReport, baseline: Option<&ReportDiff>) -> Outcome {
    if matches!(rule.metric, Metric::PatternBytes | Metric::Growth) && !report.sizes_available {
        return Outcome::NotJudged("no MEMORY USAGE".to_string());
    }
    let verdict_needs_history =
        matches!(rule.limit, Limit::Verdict(VerdictName::PersistentGrowing));
    if baseline.is_none() && (rule.metric == Metric::Growth || verdict_needs_history) {
        return Outcome::NotJudged("no baseline snapshot".to_string());
    }
    let eligible: Vec<&PatternStats> = report
        .patterns
        .iter()
        .filter(|p| p.sampled_keys >= rule.min_keys)
        .collect();
    let mut offenders = Vec::new();
    let mut checked = 0usize;
    let mut no_sizes = 0usize;
    let mut no_earlier = 0usize;
    for p in &eligible {
        match measure(rule, p, baseline) {
            Ok((holds, shown)) => {
                checked += 1;
                if !holds {
                    offenders.push(Offender {
                        pattern: p.pattern.clone(),
                        value: shown,
                        sampled_keys: p.sampled_keys,
                    });
                }
            }
            Err(Skip::NoSizes) => no_sizes += 1,
            Err(Skip::NoEarlierSize) => no_earlier += 1,
        }
    }
    let mut notes = Vec::new();
    if eligible.is_empty() {
        notes.push(format!(
            "no pattern has {} or more sampled keys",
            format::grouped(rule.min_keys)
        ));
    }
    if no_sizes > 0 {
        notes.push(format!("{no_sizes} without a size left out"));
    }
    if no_earlier > 0 {
        notes.push(format!(
            "{no_earlier} with no comparable earlier size left out"
        ));
    }
    if checked == 0 && !eligible.is_empty() {
        let reason = if no_earlier > 0 {
            "no pattern has a comparable size in the baseline"
        } else {
            "no pattern has a size"
        };
        return Outcome::NotJudged(reason.to_string());
    }
    let note = (!notes.is_empty()).then(|| notes.join(", "));
    if offenders.is_empty() {
        Outcome::Pass { checked, note }
    } else {
        Outcome::Fail { offenders, note }
    }
}

/// Measure one pattern: whether the rule holds and the value to show if it doesn't.
fn measure(
    rule: &Rule,
    p: &PatternStats,
    baseline: Option<&ReportDiff>,
) -> Result<(bool, String), Skip> {
    match rule.metric {
        Metric::NoTtlPct => {
            let pct = p.no_ttl_ratio() * 100.0;
            Ok((
                rule.op.holds(pct, number(rule)),
                format::percent(p.no_ttl_ratio()),
            ))
        }
        Metric::PatternBytes => {
            let b = p.est_bytes.ok_or(Skip::NoSizes)?;
            Ok((rule.op.holds(b as f64, number(rule)), format::bytes(b)))
        }
        Metric::Growth => {
            let change = baseline.and_then(|d| d.pattern(&p.pattern));
            let c = change.ok_or(Skip::NoEarlierSize)?;
            if matches!(c.cause, Cause::New | Cause::Unknown) {
                return Err(Skip::NoEarlierSize);
            }
            let before = c
                .bytes_before
                .filter(|b| *b > 0)
                .ok_or(Skip::NoEarlierSize)?;
            let delta = c.delta_bytes.ok_or(Skip::NoEarlierSize)?;
            let pct = delta as f64 / before as f64 * 100.0;
            let shown = if pct >= 0.0 {
                format!("+{pct:.0}%")
            } else {
                format!("{pct:.0}%")
            };
            Ok((rule.op.holds(pct, number(rule)), shown))
        }
        Metric::Verdict => {
            let Limit::Verdict(want) = rule.limit else {
                unreachable!("verdict rules carry a verdict")
            };
            let change = baseline.and_then(|d| d.pattern(&p.pattern));
            let v = ttl_verdict(p, change);
            let is = want.matches(v);
            let holds = if rule.op == Op::Eq { is } else { !is };
            Ok((holds, verdict_word(v).to_string()))
        }
        Metric::TotalBytes => unreachable!("handled by total_bytes"),
    }
}

/// Counts for the summary line. Under `--strict` the not-judged rules count as failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
}

pub fn summarize(evals: &[Evaluation], strict: bool) -> Summary {
    let mut s = Summary {
        passed: 0,
        failed: 0,
        skipped: 0,
    };
    for e in evals {
        match &e.outcome {
            Outcome::Pass { .. } => s.passed += 1,
            Outcome::Fail { .. } => s.failed += 1,
            Outcome::NotJudged(_) if strict => s.failed += 1,
            Outcome::NotJudged(_) => s.skipped += 1,
        }
    }
    s
}

/// Pattern names come from whoever writes keys: no control characters (terminal escapes)
/// may reach the terminal.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect()
}

/// The lines to print: one per rule (with the offending patterns indented under a failure),
/// then a summary.
pub fn render_lines(evals: &[Evaluation], strict: bool) -> Vec<String> {
    let mut lines = Vec::new();
    for e in evals {
        let rule = clean(&e.rule);
        match &e.outcome {
            Outcome::Pass { checked, note } => {
                let mut s = format!(
                    "PASS {rule}  ({} pattern{} checked",
                    checked,
                    if *checked == 1 { "" } else { "s" }
                );
                if let Some(n) = note {
                    s.push_str(&format!("; {}", clean(n)));
                }
                s.push(')');
                lines.push(s);
            }
            Outcome::Fail { offenders, note } => {
                let first = &offenders[0];
                let mut head = format!("FAIL {rule}  {}", offender(first));
                if let Some(n) = note {
                    head.push_str(&format!(" [{}]", clean(n)));
                }
                lines.push(head);
                for o in offenders.iter().skip(1).take(MAX_LISTED - 1) {
                    lines.push(format!("     {}", offender(o)));
                }
                if offenders.len() > MAX_LISTED {
                    lines.push(format!(
                        "     ... and {} more",
                        offenders.len() - MAX_LISTED
                    ));
                }
            }
            Outcome::NotJudged(reason) if strict => {
                lines.push(format!(
                    "FAIL {rule}  (not judged: {}; --strict)",
                    clean(reason)
                ));
            }
            Outcome::NotJudged(reason) => {
                lines.push(format!("SKIP {rule}  ({})", clean(reason)));
            }
        }
    }
    let s = summarize(evals, strict);
    lines.push(format!(
        "{} passed, {} failed, {} skipped",
        s.passed, s.failed, s.skipped
    ));
    lines
}

fn offender(o: &Offender) -> String {
    if o.pattern == "(whole scan)" {
        format!("(whole scan) {}", clean(&o.value))
    } else {
        format!(
            "{} {} ({} sampled)",
            clean(&o.pattern),
            clean(&o.value),
            format::grouped(o.sampled_keys)
        )
    }
}

/// How many rules failed, counting not-judged ones under `--strict`.
pub fn failure_count(evals: &[Evaluation], strict: bool) -> usize {
    summarize(evals, strict).failed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::diff;
    use crate::model::{KeyType, ServerFlavor, ServerInfo, ServerMode};

    pub(crate) fn pat(name: &str, sampled: u64, no_ttl: u64, bytes: Option<u64>) -> PatternStats {
        PatternStats {
            pattern: name.to_string(),
            key_type: Some(KeyType::String),
            sampled_keys: sampled,
            est_keys: sampled,
            sampled_bytes: bytes,
            est_bytes: bytes,
            no_ttl,
            ttl_hist: {
                let mut h = [0; 7];
                h[0] = no_ttl;
                h[3] = sampled - no_ttl;
                h
            },
            size_p50: None,
            size_p99: None,
            size_max: None,
            examples: vec![],
        }
    }

    pub(crate) fn rep(patterns: Vec<PatternStats>, sizes: bool, complete: bool) -> ScanReport {
        ScanReport {
            profile: "p".into(),
            taken_at_unix: 1_000,
            server: ServerInfo {
                flavor: ServerFlavor::Redis,
                version: "7.4.2".into(),
                mode: ServerMode::Standalone,
                used_memory: 1,
                maxmemory: 0,
                maxmemory_policy: "noeviction".into(),
                total_keys: 10_000,
            },
            sampled_keys: patterns.iter().map(|p| p.sampled_keys).sum(),
            patterns,
            sizes_available: sizes,
            hot_keys: vec![],
            complete,
            match_glob: None,
            server_context: None,
        }
    }

    fn rule(s: &str) -> Rule {
        parse_rule(s, DEFAULT_MIN_KEYS).unwrap_or_else(|e| panic!("{e}"))
    }

    fn eval(s: &str, r: &ScanReport, b: Option<&ReportDiff>) -> Outcome {
        evaluate(&rule(s), r, b)
    }

    fn fails(o: &Outcome) -> Vec<&str> {
        match o {
            Outcome::Fail { offenders, .. } => {
                offenders.iter().map(|o| o.pattern.as_str()).collect()
            }
            other => panic!("not a failure: {other:?}"),
        }
    }

    #[test]
    fn parses_every_metric_and_op() {
        for (text, metric, op) in [
            ("no-ttl-pct<20", Metric::NoTtlPct, Op::Lt),
            ("no-ttl-pct<=20%", Metric::NoTtlPct, Op::Le),
            ("no-ttl-pct>1", Metric::NoTtlPct, Op::Gt),
            ("no-ttl-pct>=1", Metric::NoTtlPct, Op::Ge),
            ("no-ttl-pct=0", Metric::NoTtlPct, Op::Eq),
            ("no-ttl-pct!=0", Metric::NoTtlPct, Op::Ne),
            ("growth<25%", Metric::Growth, Op::Lt),
            ("pattern-bytes<=500MiB", Metric::PatternBytes, Op::Le),
            ("total-bytes>1GiB", Metric::TotalBytes, Op::Gt),
            ("verdict=mixed", Metric::Verdict, Op::Eq),
            ("verdict!=persistent-growing", Metric::Verdict, Op::Ne),
            ("  no-ttl-pct < 20 ", Metric::NoTtlPct, Op::Lt),
        ] {
            let r = rule(text);
            assert_eq!((r.metric, r.op), (metric, op), "{text}");
        }
    }

    #[test]
    fn size_units() {
        assert_eq!(parse_size("500MiB"), Some(500.0 * 1048576.0));
        assert_eq!(parse_size("1.5GiB"), Some(1.5 * 1073741824.0));
        assert_eq!(parse_size("2048"), Some(2048.0));
        assert_eq!(parse_size("64k"), Some(65536.0));
        assert_eq!(parse_size("1GB"), Some(1e9));
        assert_eq!(parse_size("10B"), Some(10.0));
        assert_eq!(parse_size("12XB"), None);
        assert_eq!(parse_size("MiB"), None);
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn parse_errors_name_the_rule() {
        for (text, needle) in [
            ("nonsense", "no operator"),
            ("<20", "missing metric"),
            ("no-ttl-pct<", "missing value"),
            ("ttl<20", "unknown metric 'ttl'"),
            ("no-ttl-pct<abc", "not a percentage"),
            ("no-ttl-pct<-5", "not a percentage"),
            ("pattern-bytes<12XB", "not a size"),
            ("verdict<mixed", "only works with"),
            ("verdict=bad", "unknown verdict"),
            ("no-ttl-pct!20", "'!' must be followed"),
            ("no-ttl-pct<20,max-keys=3", "unknown option"),
            ("no-ttl-pct<20,min-keys=x", "not a whole number"),
            ("total-bytes<1GiB,min-keys=3", "does not apply"),
        ] {
            let e = parse_rule(text, 50).unwrap_err();
            assert!(e.message.contains(needle), "{text}: {e}");
            assert!(e.to_string().contains(text.trim()), "{text}: {e}");
        }
    }

    #[test]
    fn min_keys_option_and_default() {
        assert_eq!(rule("no-ttl-pct<20").min_keys, 50);
        assert_eq!(rule("no-ttl-pct<20,min-keys=7").min_keys, 7);
        assert_eq!(
            parse_rule("no-ttl-pct<20", 3).unwrap().min_keys,
            3,
            "file default"
        );
        assert_eq!(
            parse_rule("no-ttl-pct<20,min-keys=9", 3).unwrap().min_keys,
            9
        );
    }

    #[test]
    fn no_ttl_pct_passes_and_fails() {
        let r = rep(
            vec![
                pat("a", 100, 34, Some(10)),
                pat("b", 100, 10, Some(10)),
                pat("c", 100, 20, Some(10)),
            ],
            true,
            true,
        );
        // Exactly 20% does not satisfy "< 20".
        assert_eq!(fails(&eval("no-ttl-pct<20", &r, None)), ["a", "c"]);
        assert_eq!(fails(&eval("no-ttl-pct<=20", &r, None)), ["a"]);
        assert_eq!(fails(&eval("no-ttl-pct>20", &r, None)), ["b", "c"]);
        assert_eq!(fails(&eval("no-ttl-pct>=20", &r, None)), ["b"]);
        assert_eq!(fails(&eval("no-ttl-pct=10", &r, None)), ["a", "c"]);
        assert_eq!(fails(&eval("no-ttl-pct!=10", &r, None)), ["b"]);
        assert_eq!(
            eval("no-ttl-pct<50", &r, None),
            Outcome::Pass {
                checked: 3,
                note: None
            }
        );
    }

    #[test]
    fn min_keys_floor_skips_small_patterns() {
        let r = rep(
            vec![pat("big", 100, 0, Some(1)), pat("small", 10, 10, Some(1))],
            true,
            true,
        );
        // The small pattern is 100% without TTL but below the default floor of 50.
        assert_eq!(
            eval("no-ttl-pct<20", &r, None),
            Outcome::Pass {
                checked: 1,
                note: None
            }
        );
        assert_eq!(
            fails(&eval("no-ttl-pct<20,min-keys=10", &r, None)),
            ["small"]
        );
        // Nothing reaches the floor: passes, and says so.
        match eval("no-ttl-pct<20,min-keys=500", &r, None) {
            Outcome::Pass { checked: 0, note } => {
                assert!(note.unwrap().contains("500"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn pattern_bytes_and_total_bytes() {
        let r = rep(
            vec![
                pat("a", 100, 0, Some(600 * 1048576)),
                pat("b", 100, 0, Some(100 * 1048576)),
            ],
            true,
            true,
        );
        assert_eq!(fails(&eval("pattern-bytes<500MiB", &r, None)), ["a"]);
        assert_eq!(fails(&eval("pattern-bytes>=500MiB", &r, None)), ["b"]);
        assert_eq!(fails(&eval("pattern-bytes=100MiB", &r, None)), ["a"]);
        assert_eq!(fails(&eval("pattern-bytes!=100MiB", &r, None)), ["b"]);
        assert_eq!(fails(&eval("pattern-bytes<=100MiB", &r, None)), ["a"]);
        assert_eq!(fails(&eval("pattern-bytes>100MiB", &r, None)), ["b"]);
        assert_eq!(
            fails(&eval("total-bytes<700MiB", &r, None)),
            ["(whole scan)"]
        );
        assert!(matches!(
            eval("total-bytes<=700MiB", &r, None),
            Outcome::Pass { .. }
        ));
        assert!(matches!(
            eval("total-bytes>=700MiB", &r, None),
            Outcome::Pass { .. }
        ));
        assert!(matches!(
            eval("total-bytes=700MiB", &r, None),
            Outcome::Pass { .. }
        ));
        assert!(matches!(
            eval("total-bytes!=700MiB", &r, None),
            Outcome::Fail { .. }
        ));
        assert!(matches!(
            eval("total-bytes>1MiB", &r, None),
            Outcome::Pass { .. }
        ));
    }

    #[test]
    fn total_bytes_notes_partial_and_match() {
        let mut r = rep(vec![pat("a", 100, 0, Some(10))], true, false);
        r.match_glob = Some("a:*".into());
        match eval("total-bytes<1MiB", &r, None) {
            Outcome::Pass { note, .. } => {
                let n = note.unwrap();
                assert!(n.contains("MATCH a:*") && n.contains("partial"), "{n}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn size_rules_are_not_judged_without_sizes() {
        let r = rep(vec![pat("a", 100, 50, None)], false, true);
        for text in ["pattern-bytes<1MiB", "total-bytes<1MiB", "growth<10%"] {
            assert_eq!(
                eval(text, &r, None),
                Outcome::NotJudged("no MEMORY USAGE".into()),
                "{text}"
            );
        }
        // TTL rules don't need sizes.
        assert!(matches!(
            eval("no-ttl-pct<20", &r, None),
            Outcome::Fail { .. }
        ));
    }

    fn baseline_pair(before_bytes: u64, after_bytes: u64) -> (ScanReport, ReportDiff) {
        let before = rep(vec![pat("a", 100, 0, Some(before_bytes))], true, true);
        let mut after = rep(vec![pat("a", 100, 0, Some(after_bytes))], true, true);
        after.taken_at_unix = 2_000;
        let d = diff(&before, &after);
        (after, d)
    }

    #[test]
    fn growth_against_a_baseline() {
        let (after, d) = baseline_pair(1_000_000, 1_300_000);
        assert_eq!(fails(&eval("growth<25%", &after, Some(&d))), ["a"]);
        assert!(matches!(
            eval("growth<35%", &after, Some(&d)),
            Outcome::Pass { checked: 1, .. }
        ));
        match eval("growth<25%", &after, Some(&d)) {
            Outcome::Fail { offenders, .. } => assert_eq!(offenders[0].value, "+30%"),
            other => panic!("{other:?}"),
        }
        let (shrunk, d2) = baseline_pair(1_000_000, 500_000);
        assert!(matches!(
            eval("growth<25%", &shrunk, Some(&d2)),
            Outcome::Pass { .. }
        ));
        assert!(matches!(
            eval("growth<-10%", &shrunk, Some(&d2)),
            Outcome::Pass { .. }
        ));
    }

    #[test]
    fn growth_without_a_baseline_is_not_judged() {
        let (after, _) = baseline_pair(1, 2);
        assert_eq!(
            eval("growth<25%", &after, None),
            Outcome::NotJudged("no baseline snapshot".into())
        );
    }

    #[test]
    fn growth_skips_new_patterns() {
        let before = rep(vec![pat("old", 100, 0, Some(1_000_000))], true, true);
        let after = rep(
            vec![
                pat("old", 100, 0, Some(1_000_000)),
                pat("new", 100, 0, Some(9_000_000)),
            ],
            true,
            true,
        );
        let d = diff(&before, &after);
        match eval("growth<25%", &after, Some(&d)) {
            Outcome::Pass { checked, note } => {
                assert_eq!(checked, 1);
                assert!(note.unwrap().contains("left out"));
            }
            other => panic!("{other:?}"),
        }
        // Only a new pattern: nothing comparable.
        let only_new = rep(vec![pat("new", 100, 0, Some(9))], true, true);
        let d2 = diff(&before, &only_new);
        assert!(matches!(
            eval("growth<25%", &only_new, Some(&d2)),
            Outcome::NotJudged(_)
        ));
    }

    #[test]
    fn growth_not_judged_when_baseline_lacks_sizes() {
        let mut before = rep(vec![pat("a", 100, 0, None)], false, true);
        before.taken_at_unix = 1;
        let after = rep(vec![pat("a", 100, 0, Some(5))], true, true);
        let d = diff(&before, &after);
        assert!(matches!(
            eval("growth<25%", &after, Some(&d)),
            Outcome::NotJudged(_)
        ));
    }

    #[test]
    fn verdict_rules() {
        let r = rep(
            vec![
                pat("mixedp", 100, 50, Some(1)),
                pat("okp", 100, 0, Some(1)),
                pat("persist", 100, 100, Some(1)),
            ],
            true,
            true,
        );
        assert_eq!(fails(&eval("verdict!=mixed", &r, None)), ["mixedp"]);
        assert_eq!(fails(&eval("verdict=ok", &r, None)), ["mixedp", "persist"]);
        assert_eq!(
            fails(&eval("verdict!=persistent-stable", &r, None)),
            ["persist"]
        );
        // The verdict a failure shows is the verdict word.
        match eval("verdict!=mixed", &r, None) {
            Outcome::Fail { offenders, .. } => assert_eq!(offenders[0].value, "mixed"),
            other => panic!("{other:?}"),
        }
        // "growing" can't be judged without history.
        assert_eq!(
            eval("verdict!=persistent-growing", &r, None),
            Outcome::NotJudged("no baseline snapshot".into())
        );
    }

    #[test]
    fn verdict_persistent_growing_with_baseline() {
        let before = rep(vec![pat("leak", 100, 100, Some(1_000_000))], true, true);
        let mut after = rep(vec![pat("leak", 100, 100, Some(9_000_000))], true, true);
        after.taken_at_unix = 2_000;
        let d = diff(&before, &after);
        assert_eq!(
            fails(&eval("verdict!=persistent-growing", &after, Some(&d))),
            ["leak"]
        );
        assert!(matches!(
            eval(
                "verdict!=persistent-growing",
                &before,
                Some(&diff(&before, &before))
            ),
            Outcome::Pass { .. }
        ));
    }

    #[test]
    fn strict_turns_not_judged_into_failures() {
        let r = rep(vec![pat("a", 100, 0, None)], false, true);
        let evals = evaluate_all(
            &[rule("pattern-bytes<1MiB"), rule("no-ttl-pct<20")],
            &r,
            None,
        );
        assert_eq!(failure_count(&evals, false), 0);
        assert_eq!(failure_count(&evals, true), 1);
        let loose = render_lines(&evals, false);
        assert!(loose[0].starts_with("SKIP pattern-bytes<1MiB  (no MEMORY USAGE)"));
        assert!(
            loose
                .last()
                .unwrap()
                .contains("1 passed, 0 failed, 1 skipped")
        );
        let strict = render_lines(&evals, true);
        assert!(strict[0].starts_with("FAIL pattern-bytes<1MiB  (not judged: no MEMORY USAGE"));
        assert!(
            strict
                .last()
                .unwrap()
                .contains("1 passed, 1 failed, 0 skipped")
        );
    }

    #[test]
    fn output_lines() {
        let r = rep(
            vec![
                pat("user:{id}:cart", 1204, 409, Some(10)),
                pat("ok", 100, 0, Some(1)),
            ],
            true,
            true,
        );
        let evals = evaluate_all(&[rule("no-ttl-pct<20"), rule("no-ttl-pct<90")], &r, None);
        let lines = render_lines(&evals, false);
        assert_eq!(
            lines[0],
            "FAIL no-ttl-pct<20  user:{id}:cart 34% (1,204 sampled)"
        );
        assert_eq!(lines[1], "PASS no-ttl-pct<90  (2 patterns checked)");
        assert_eq!(lines[2], "1 passed, 1 failed, 0 skipped");
    }

    #[test]
    fn many_offenders_are_capped() {
        let pats: Vec<_> = (0..15)
            .map(|i| pat(&format!("p{i}"), 100, 100, Some(1)))
            .collect();
        let r = rep(pats, true, true);
        let lines = render_lines(&evaluate_all(&[rule("no-ttl-pct<20")], &r, None), false);
        assert_eq!(lines.len(), 1 + 9 + 1 + 1);
        assert!(lines[10].contains("and 5 more"));
    }

    #[test]
    fn terminal_escapes_in_patterns_are_neutralised() {
        let r = rep(vec![pat("a\x1b[31m\nb", 100, 100, Some(1))], true, true);
        let lines = render_lines(&evaluate_all(&[rule("no-ttl-pct<20")], &r, None), false);
        assert!(
            lines
                .iter()
                .all(|l| !l.contains('\x1b') && !l.contains('\n'))
        );
    }

    #[test]
    fn rules_file() {
        let (rules, min) = parse_rules_file(
            "[check]\nrules = [\"no-ttl-pct<20\", \"growth<25%,min-keys=5\"]\nmin_keys = 80\n",
        )
        .unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(min, Some(80));
        let (rules, min) = parse_rules_file("[check]\nrules = []\n").unwrap();
        assert!(rules.is_empty() && min.is_none());
        assert!(parse_rules_file("").unwrap_err().contains("[check]"));
        assert!(
            parse_rules_file("rules = []").is_err(),
            "rules sit under [check]"
        );
        assert!(parse_rules_file("[check]\nrulez = []").is_err());
        assert!(parse_rules_file("[check]\nrules = [1]").is_err());
        assert!(parse_rules_file("not toml [[").is_err());
    }

    #[test]
    fn load_rules_combines_file_and_flags() {
        let dir = std::env::temp_dir().join(format!("redis-lens-check-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rules.toml");
        std::fs::write(
            &path,
            "[check]\nrules = [\"verdict!=mixed\"]\nmin_keys = 80\n",
        )
        .unwrap();
        let rules = load_rules(&["no-ttl-pct<20".to_string()], Some(&path)).unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].metric, Metric::Verdict);
        assert!(rules.iter().all(|r| r.min_keys == 80));
        // A flag's own min-keys still wins.
        let rules = load_rules(&["no-ttl-pct<20,min-keys=3".to_string()], Some(&path)).unwrap();
        assert_eq!(rules[1].min_keys, 3);

        let e = load_rules(&["bogus".to_string()], None).unwrap_err();
        assert!(e.to_string().contains("'bogus'"), "{e}");
        let e = load_rules(&[], Some(&dir.join("missing.toml"))).unwrap_err();
        assert!(e.to_string().contains("missing.toml"), "{e}");
        std::fs::write(&path, "[check]\nrules = [\"nope\"]\n").unwrap();
        let e = load_rules(&[], Some(&path)).unwrap_err();
        assert!(e.to_string().contains("'nope'"), "{e}");
        std::fs::write(&path, "[check]\n").unwrap();
        assert!(load_rules(&[], Some(&path)).is_err(), "no rules at all");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exit_codes() {
        use crate::cli::{ChecksFailed, GateError, exit_code};
        let failed: anyhow::Error = ChecksFailed(2).into();
        assert_eq!(exit_code(&failed), 1);
        let broken: anyhow::Error = GateError(anyhow::anyhow!("connection refused")).into();
        assert_eq!(exit_code(&broken), 2);
        let parse: anyhow::Error = GateError(parse_rule("x", 1).unwrap_err().into()).into();
        assert_eq!(exit_code(&parse), 2);
        assert_eq!(exit_code(&anyhow::anyhow!("other")), 1);
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn parser_never_panics(s in ".{0,60}") {
                let _ = parse_rule(&s, 50);
                let _ = parse_size(&s);
                let _ = parse_rules_file(&s);
            }

            #[test]
            fn parser_never_panics_on_rule_shaped_text(
                m in "(no-ttl-pct|growth|pattern-bytes|verdict|total-bytes|x)",
                op in "(<|<=|>|>=|=|!=|!|==)",
                v in "[0-9a-zA-Z.%-]{0,12}",
                o in "(|,min-keys=[0-9]{0,4}|,x|,)",
            ) {
                let text = format!("{m}{op}{v}{o}");
                if let Ok(r) = parse_rule(&text, 50) {
                    // Whatever parses can be evaluated.
                    let report = rep(vec![pat("a", 100, 10, Some(5))], true, true);
                    let _ = evaluate(&r, &report, None);
                }
            }
        }
    }
}
