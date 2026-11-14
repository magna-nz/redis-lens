//! Live requests for the Search view. Owner: wave B. No writes.
//!
//! Phase 1 reads `FT._LIST` and `FT.INFO`. Phase 2 adds the queries, on one index each
//! (spec "6 Search"):
//!
//! - `FT.SEARCH <index> <query> LIMIT 0 50`, the results held as [`Raw`];
//! - `FT.EXPLAIN <index> <query>`, the plan as escaped lines ([`Text`]);
//! - `FT.PROFILE <index> SEARCH QUERY <query> LIMIT 0 50`, the results and the profile
//!   part as an indented tree of escaped lines.
//!
//! No `FT.CREATE`, no `FT.DROPINDEX`. Redis Search (built into Redis 8) and
//! valkey-search answer `FT.INFO` with different fields; a field the module does not
//! return is `None` here, and the view leaves it out with a reason instead of showing an
//! error. valkey-search has no `FT.EXPLAIN` and no `FT.PROFILE`: they fail as
//! [`LiveError::Unsupported`].
//!
//! Every command takes one from the budget. Errors are redis-lens's own words
//! ([`error_for`]), with one exception for the three query commands
//! ([`query_error`]): a plain error about the query (a syntax error, an unknown index)
//! keeps the server's message, escaped and cut at [`QUERY_ERROR_CHARS`] characters,
//! because it describes the query the user typed, not the user. `NOPERM` never does.

use std::fmt;
use std::time::{Duration, Instant};

use redis::{ErrorKind, RedisError, RedisResult, ServerErrorKind, Value};

use super::streams::{field, pairs, text, uint};
use super::{LiveConn, LiveError, error_for, is_unknown_command, query, query_pipe};
use crate::budget::Budget;
use crate::value::{self, Raw};

/// The most indexes whose `FT.INFO` is read.
pub const MAX_INDEXES: usize = 50;
/// The most results a query returns: `LIMIT 0 50`.
pub const MAX_RESULTS: usize = 50;
/// The most characters of the server's message about a query that are kept.
pub const QUERY_ERROR_CHARS: usize = 200;
/// The most lines of an explain or profile text that are kept.
pub const MAX_TEXT_LINES: usize = 2_000;

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// `FT._LIST`, then `FT.INFO` for each index.
    Indexes,
    /// `FT.SEARCH <index> <query> LIMIT 0 50`. `query` is sent as one argument, as it is.
    Search { index: String, query: String },
    /// `FT.EXPLAIN <index> <query>`.
    Explain { index: String, query: String },
    /// `FT.PROFILE <index> SEARCH QUERY <query> LIMIT 0 50`.
    Profile { index: String, query: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Indexes(IndexList),
    Results(Results),
    Explain(Text),
    Profile { results: Results, profile: Text },
}

/// What a query found. Values only as [`Raw`], so `Debug` never shows one.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Results {
    /// How many documents match, as the module counts them: often more than `docs`.
    pub total: u64,
    /// At most [`MAX_RESULTS`], in the module's order.
    pub docs: Vec<Doc>,
    /// The command's round trip, measured here (views never read the clock).
    pub took: Duration,
}

/// One result: the key and the fields the module returned. A JSON index returns the
/// document as the one field `$`.
#[derive(Debug, Clone, PartialEq)]
pub struct Doc {
    pub key: String,
    pub fields: Vec<(Raw, Raw)>,
}

/// Lines of text from the server that are safe to draw: each has gone through
/// [`value::escape`]. They can echo data (a profile names the terms and ranges it
/// read), so `Debug` shows only how many there are.
#[derive(Clone, PartialEq, Default)]
pub struct Text(Vec<String>);

impl Text {
    /// Escapes every line, keeping at most [`MAX_TEXT_LINES`].
    pub fn new<S: AsRef<str>>(lines: impl IntoIterator<Item = S>) -> Self {
        Self(
            lines
                .into_iter()
                .take(MAX_TEXT_LINES)
                .map(|l| value::escape(l.as_ref()))
                .collect(),
        )
    }

    pub fn lines(&self) -> &[String] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Text(<{} lines>)", self.0.len())
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct IndexList {
    /// By name. At most [`MAX_INDEXES`].
    pub indexes: Vec<IndexInfo>,
    /// How many indexes `FT._LIST` named.
    pub total: usize,
    /// Why an index has no details, in redis-lens's own words.
    pub notes: Vec<String>,
}

/// What `FT.INFO` says about one index.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct IndexInfo {
    pub name: String,
    /// What it indexes: `HASH` or `JSON`.
    pub on: Option<String>,
    pub prefixes: Vec<String>,
    pub docs: Option<u64>,
    /// Memory the index uses. Redis Search reports it, valkey-search does not.
    pub bytes: Option<u64>,
    /// True while the index is still being built over the existing keys.
    pub indexing: Option<bool>,
    /// Share of the keys indexed so far, 0.0 to 1.0.
    pub progress: Option<f64>,
    /// Documents the module failed to index.
    pub failures: Option<u64>,
    pub schema: Vec<FieldInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldInfo {
    /// The field name in a hash, or the JSON path.
    pub identifier: String,
    /// The name queries use (`AS`); the identifier when none was given.
    pub name: String,
    /// `TEXT`, `TAG`, `NUMERIC`, `GEO`, `VECTOR` …
    pub ty: String,
    /// The options that are on, such as `SORTABLE`.
    pub flags: Vec<String>,
}

/// Runs one request. Every command goes through `budget`.
pub async fn run(
    conn: &mut impl LiveConn,
    budget: &Budget,
    req: Request,
) -> Result<Reply, LiveError> {
    match req {
        Request::Indexes => indexes(conn, budget).await.map(Reply::Indexes),
        Request::Search { index, query } => {
            let mut c = redis::cmd("FT.SEARCH");
            c.arg(&index)
                .arg(&query)
                .arg("LIMIT")
                .arg(0)
                .arg(MAX_RESULTS);
            let (v, took) = timed(conn, budget, "FT.SEARCH", &c).await?;
            let mut results = parse_results(&v);
            results.took = took;
            Ok(Reply::Results(results))
        }
        Request::Explain { index, query } => {
            let mut c = redis::cmd("FT.EXPLAIN");
            c.arg(&index).arg(&query);
            let (v, _) = timed(conn, budget, "FT.EXPLAIN", &c).await?;
            Ok(Reply::Explain(parse_explain(&v)))
        }
        Request::Profile { index, query } => {
            let mut c = redis::cmd("FT.PROFILE");
            c.arg(&index)
                .arg("SEARCH")
                .arg("QUERY")
                .arg(&query)
                .arg("LIMIT")
                .arg(0)
                .arg(MAX_RESULTS);
            let (v, took) = timed(conn, budget, "FT.PROFILE", &c).await?;
            let (mut results, profile) = parse_profile(&v).ok_or_else(|| {
                LiveError::Failed("FT.PROFILE came back in an unexpected shape".into())
            })?;
            results.took = took;
            Ok(Reply::Profile { results, profile })
        }
    }
}

/// Takes one command from `budget`, then sends `cmd` and measures its round trip (the
/// wait for the budget not included). Like [`query`], except that a failure is read by
/// [`query_error`].
async fn timed(
    conn: &mut impl LiveConn,
    budget: &Budget,
    name: &str,
    cmd: &redis::Cmd,
) -> Result<(Value, Duration), LiveError> {
    budget.take(1).await;
    let start = Instant::now();
    let r: RedisResult<Value> = cmd.query_async(conn).await;
    let took = start.elapsed();
    r.map(|v| (v, took)).map_err(|e| query_error(name, &e))
}

/// A failure of `FT.SEARCH`, `FT.EXPLAIN` or `FT.PROFILE` (`command`). `NOPERM`, an
/// unknown command, a timeout and a dropped connection read as everywhere else
/// ([`error_for`]). A plain error (`ERR …`, or a code of the module's own such as
/// `SEARCH_SYNTAX`) is about the query: its message is kept, without a leading
/// upper-case code, escaped with [`value::escape`] and cut at [`QUERY_ERROR_CHARS`]
/// characters. It never names the user: `NOPERM` is the only error that does.
pub fn query_error(command: &str, e: &RedisError) -> LiveError {
    let plain = matches!(
        e.kind(),
        ErrorKind::Server(ServerErrorKind::ResponseError) | ErrorKind::Extension
    );
    if !plain || is_unknown_command(e) {
        return error_for(command, e);
    }
    let code = e.code().unwrap_or("");
    let detail = e.detail().unwrap_or("");
    let is_code = !code.is_empty()
        && code
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    let msg = if is_code {
        detail.to_string()
    } else {
        format!("{code} {detail}")
    };
    let msg = value::escape(msg.trim());
    if msg.is_empty() {
        return LiveError::Failed(format!("{command} failed"));
    }
    LiveError::Failed(format!(
        "{command} failed: {}",
        cut_chars(&msg, QUERY_ERROR_CHARS)
    ))
}

/// `s` cut to `max` characters, the last one `…` when it was cut.
fn cut_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

async fn indexes(conn: &mut impl LiveConn, budget: &Budget) -> Result<IndexList, LiveError> {
    let listed: Value = query(conn, budget, "FT._LIST", &redis::cmd("FT._LIST")).await?;
    let mut names = parse_names(&listed);
    names.sort();
    let mut out = IndexList {
        total: names.len(),
        ..IndexList::default()
    };
    names.truncate(MAX_INDEXES);
    if names.is_empty() {
        return Ok(out);
    }
    let mut pipe = redis::pipe();
    pipe.ignore_errors();
    for n in &names {
        pipe.cmd("FT.INFO").arg(n.as_slice());
    }
    let replies: Vec<RedisResult<Value>> = query_pipe(conn, budget, "FT.INFO", &pipe).await?;
    for (name, reply) in names.iter().zip(replies) {
        let name = String::from_utf8_lossy(name).into_owned();
        match reply {
            Ok(v) => out.indexes.push(parse_info(&name, &v)),
            Err(e) => match error_for("FT.INFO", &e) {
                LiveError::Closed => return Err(LiveError::Closed),
                // The user may list indexes and not read them: the names stay.
                err @ (LiveError::Denied(_) | LiveError::Unsupported(_)) => {
                    let note = err.to_string();
                    if !out.notes.contains(&note) {
                        out.notes.push(note);
                    }
                    out.indexes.push(IndexInfo {
                        name,
                        ..IndexInfo::default()
                    });
                }
                // Dropped since `FT._LIST`, or any other failure of this one index.
                LiveError::Failed(_) => {}
            },
        }
    }
    Ok(out)
}

/// The bytes of a string-like reply; other replies give no bytes.
fn raw(v: &Value) -> Raw {
    match v {
        Value::BulkString(b) => Raw::new(b.clone()),
        Value::SimpleString(s) => Raw::from(s.as_str()),
        Value::VerbatimString { text, .. } => Raw::from(text.as_str()),
        Value::Int(i) => Raw::from(i.to_string().as_str()),
        Value::Double(d) => Raw::from(d.to_string().as_str()),
        _ => Raw::default(),
    }
}

/// `(name, value)` pairs of a flat array or a map, as bytes.
fn raw_pairs(v: &Value) -> Vec<(Raw, Raw)> {
    match v {
        Value::Map(m) => m.iter().map(|(k, v)| (raw(k), raw(v))).collect(),
        Value::Array(items) => items
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[k, v]| (raw(k), raw(v)))
            .collect(),
        _ => Vec::new(),
    }
}

/// Reads the reply of `FT.SEARCH`: RESP2 `[total, key, [field, value …], key, …]`, or
/// the RESP3 map with `total_results` and `results`. Keeps at most [`MAX_RESULTS`].
pub fn parse_results(v: &Value) -> Results {
    let mut out = Results::default();
    match v {
        Value::Array(items) => {
            out.total = items.first().and_then(uint).unwrap_or(0);
            let rest = items.get(1..).unwrap_or(&[]);
            let mut i = 0;
            while i < rest.len() {
                let Some(key) = text(&rest[i]) else {
                    i += 1;
                    continue;
                };
                i += 1;
                let fields = match rest.get(i) {
                    Some(f @ (Value::Array(_) | Value::Map(_))) => {
                        i += 1;
                        raw_pairs(f)
                    }
                    _ => Vec::new(),
                };
                out.docs.push(Doc { key, fields });
            }
        }
        Value::Map(_) => {
            out.total = field(v, "total_results").and_then(uint).unwrap_or(0);
            if let Some(Value::Array(results)) = field(v, "results") {
                for r in results {
                    let Some(key) = field(r, "id").and_then(text) else {
                        continue;
                    };
                    let fields = field(r, "extra_attributes")
                        .map(raw_pairs)
                        .unwrap_or_default();
                    out.docs.push(Doc { key, fields });
                }
            }
        }
        _ => {}
    }
    out.docs.truncate(MAX_RESULTS);
    out
}

/// Reads the reply of `FT.EXPLAIN`: the plan as text, one line per line.
pub fn parse_explain(v: &Value) -> Text {
    let all = match v {
        Value::Array(items) => items.iter().filter_map(text).collect::<Vec<_>>().join("\n"),
        v => text(v).unwrap_or_default(),
    };
    let mut lines: Vec<&str> = all.split('\n').map(|l| l.trim_end_matches('\r')).collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    Text::new(lines)
}

/// Reads the reply of `FT.PROFILE … SEARCH`: the results and the profile. RESP2 answers
/// `[results, profile]`, RESP3 a map with `Results` and `Profile`.
pub fn parse_profile(v: &Value) -> Option<(Results, Text)> {
    let (results, profile) = match v {
        Value::Array(items) if items.len() >= 2 => (&items[0], &items[1]),
        Value::Map(_) => (field(v, "Results")?, field(v, "Profile")?),
        _ => return None,
    };
    Some((parse_results(results), tree(profile)))
}

/// Any reply as an indented tree of lines: `name: value` for the pairs of a flat array
/// or a map, `- ` before each composite item of a list, two more spaces per level.
/// Every string is escaped. At most [`MAX_TEXT_LINES`] lines, 32 levels deep.
pub fn tree(v: &Value) -> Text {
    let mut out = Vec::new();
    tree_into(v, 0, &mut out);
    out.truncate(MAX_TEXT_LINES);
    // Every piece was escaped as it was added.
    Text(out)
}

/// A one-line form of a reply that is not a list or a map, escaped.
fn scalar(v: &Value) -> Option<String> {
    Some(match v {
        Value::BulkString(b) => escaped_bytes(b),
        Value::SimpleString(s) => value::escape(s),
        Value::VerbatimString { text, .. } => value::escape(text),
        Value::Int(i) => i.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Nil => "(nil)".to_string(),
        Value::Okay => "OK".to_string(),
        Value::ServerError(_) => "(error)".to_string(),
        _ => return None,
    })
}

/// Bytes as text that is safe to draw: invalid UTF-8 as `\xNN`.
fn escaped_bytes(b: &[u8]) -> String {
    let mut out = String::new();
    for chunk in b.utf8_chunks() {
        out.push_str(&value::escape(chunk.valid()));
        for x in chunk.invalid() {
            out.push_str(&format!("\\x{x:02x}"));
        }
    }
    out
}

fn children(v: &Value) -> Option<Vec<&Value>> {
    match v {
        Value::Array(items) | Value::Set(items) | Value::Push { data: items, .. } => {
            Some(items.iter().collect())
        }
        Value::Map(m) => Some(m.iter().flat_map(|(k, v)| [k, v]).collect()),
        Value::Attribute { data, .. } => children(data),
        _ => None,
    }
}

/// A flat array reads as pairs when it has an even, non-zero length and every name is
/// a short text.
fn as_pairs(v: &Value) -> Option<Vec<(String, &Value)>> {
    if let Value::Map(m) = v {
        return Some(
            m.iter()
                .map(|(k, v)| (scalar(k).unwrap_or_default(), v))
                .collect(),
        );
    }
    let Value::Array(items) = v else {
        return None;
    };
    if items.is_empty() || items.len() % 2 != 0 {
        return None;
    }
    let named = items.as_chunks::<2>().0.iter().all(|[k, _]| {
        matches!(k, Value::BulkString(_) | Value::SimpleString(_))
            && scalar(k).is_some_and(|s| !s.is_empty() && s.chars().count() <= 48)
    });
    named.then(|| {
        items
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[k, v]| (scalar(k).unwrap_or_default(), v))
            .collect()
    })
}

/// A list of scalars short enough to sit after its name: `a, b`.
fn inline(v: &Value) -> Option<String> {
    if let Some(s) = scalar(v) {
        return Some(s);
    }
    let items = children(v)?;
    if items.is_empty() {
        return Some("(empty)".to_string());
    }
    let parts: Option<Vec<String>> = items.iter().map(|i| scalar(i)).collect();
    let s = parts?.join(", ");
    (s.chars().count() <= 60 && as_pairs(v).is_none()).then_some(s)
}

fn tree_into(v: &Value, depth: usize, out: &mut Vec<String>) {
    if out.len() >= MAX_TEXT_LINES {
        return;
    }
    let pad = "  ".repeat(depth);
    if depth >= 32 {
        out.push(format!("{pad}…"));
        return;
    }
    if let Some(s) = scalar(v) {
        out.push(format!("{pad}{s}"));
        return;
    }
    if let Some(pairs) = as_pairs(v) {
        for (name, val) in pairs {
            match inline(val) {
                Some(s) => out.push(format!("{pad}{name}: {s}")),
                None => {
                    out.push(format!("{pad}{name}"));
                    tree_into(val, depth + 1, out);
                }
            }
        }
        return;
    }
    for item in children(v).unwrap_or_default() {
        if let Some(s) = scalar(item) {
            out.push(format!("{pad}{s}"));
            continue;
        }
        let start = out.len();
        tree_into(item, depth + 1, out);
        // The item's first line starts with `- ` at this level.
        if let Some(first) = out.get_mut(start) {
            let inner = format!("{pad}  ");
            if let Some(rest) = first.strip_prefix(&inner) {
                *first = format!("{pad}- {rest}");
            }
        }
    }
}

/// The names in the reply of `FT._LIST`, as the server sent them.
pub fn parse_names(v: &Value) -> Vec<Vec<u8>> {
    let items = match v {
        Value::Array(items) | Value::Set(items) => items,
        _ => return Vec::new(),
    };
    items
        .iter()
        .filter_map(|c| match c {
            Value::BulkString(b) => Some(b.clone()),
            Value::SimpleString(s) => Some(s.clone().into_bytes()),
            _ => None,
        })
        .collect()
}

/// Options that are on when listed alone (Redis Search) or followed by `1` (valkey-search
/// lists every option with `0` or `1`).
const FLAGS: [&str; 9] = [
    "SORTABLE",
    "NOINDEX",
    "NOSTEM",
    "NO_STEM",
    "CASESENSITIVE",
    "UNF",
    "WITHSUFFIXTRIE",
    "WITH_SUFFIX_TRIE",
    "INDEXMISSING",
];

/// Reads the reply of `FT.INFO` of Redis Search or valkey-search.
pub fn parse_info(name: &str, v: &Value) -> IndexInfo {
    let get = |n: &str| field(v, n);
    let definition = get("index_definition");
    let in_def = |n: &str| definition.and_then(|d| field(d, n));
    let prefixes = match in_def("prefixes") {
        Some(Value::Array(items)) => items.iter().filter_map(text).collect(),
        _ => Vec::new(),
    };
    // Redis Search: `indexing` is 1 while it builds and `percent_indexed` is 0 to 1.
    // valkey-search: `backfill_in_progress` and `backfill_complete_percent`, a text.
    let indexing = get("indexing")
        .or_else(|| get("backfill_in_progress"))
        .and_then(uint)
        .map(|n| n != 0);
    let progress = get("percent_indexed")
        .or_else(|| get("backfill_complete_percent"))
        .and_then(float)
        .map(|p| p.clamp(0.0, 1.0));
    // `Index Errors` also counts failures of JSON documents; `hash_indexing_failures` is
    // all valkey-search has.
    let failures = get("Index Errors")
        .and_then(|e| field(e, "indexing failures"))
        .or_else(|| get("hash_indexing_failures"))
        .and_then(uint);
    IndexInfo {
        name: get("index_name")
            .and_then(text)
            .unwrap_or_else(|| name.to_string()),
        on: in_def("key_type").and_then(text),
        prefixes,
        docs: get("num_docs").and_then(uint),
        bytes: index_bytes(v),
        indexing,
        progress,
        failures,
        schema: match get("attributes") {
            Some(Value::Array(items)) => items.iter().filter_map(parse_field).collect(),
            _ => Vec::new(),
        },
    }
}

/// The memory of the index in bytes: `total_index_memory_sz_mb` when the module reports
/// it, else the sum of its size fields. `None` when it reports none (valkey-search).
fn index_bytes(v: &Value) -> Option<u64> {
    const MB: f64 = 1024.0 * 1024.0;
    let to_bytes = |mb: f64| (mb.max(0.0) * MB).round() as u64;
    if let Some(mb) = field(v, "total_index_memory_sz_mb").and_then(float) {
        return Some(to_bytes(mb));
    }
    let parts: Vec<f64> = [
        "inverted_sz_mb",
        "vector_index_sz_mb",
        "offset_vectors_sz_mb",
        "doc_table_size_mb",
        "sortable_values_size_mb",
        "key_table_size_mb",
    ]
    .iter()
    .filter_map(|n| field(v, n).and_then(float))
    .collect();
    (!parts.is_empty()).then(|| to_bytes(parts.iter().sum()))
}

fn float(v: &Value) -> Option<f64> {
    match v {
        Value::Double(d) => Some(*d).filter(|d| d.is_finite()),
        Value::Int(i) => Some(*i as f64),
        Value::BulkString(_) | Value::SimpleString(_) | Value::VerbatimString { .. } => text(v)?
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|d| d.is_finite()),
        _ => None,
    }
}

/// One entry of `attributes`: `identifier x attribute y type T [options …]`. The options
/// are not all pairs (Redis Search lists `SORTABLE` alone), so the entry is read token by
/// token.
fn parse_field(v: &Value) -> Option<FieldInfo> {
    let Value::Array(tokens) = v else {
        // RESP3 may answer a map.
        let p = pairs(v);
        if p.is_empty() {
            return None;
        }
        let get = |n: &str| p.iter().find(|(k, _)| k == n).and_then(|(_, v)| text(v));
        let identifier = get("identifier")?;
        return Some(FieldInfo {
            name: get("attribute").unwrap_or_else(|| identifier.clone()),
            ty: get("type").unwrap_or_default(),
            flags: Vec::new(),
            identifier,
        });
    };
    let t: Vec<String> = tokens.iter().map(|t| text(t).unwrap_or_default()).collect();
    let mut identifier = None;
    let mut attribute = None;
    let mut ty = None;
    let mut flags = Vec::new();
    let mut i = 0;
    while i < t.len() {
        let upper = t[i].to_ascii_uppercase();
        let next = t.get(i + 1);
        match upper.as_str() {
            "IDENTIFIER" if next.is_some() => {
                identifier = next.cloned();
                i += 2;
            }
            "ATTRIBUTE" if next.is_some() => {
                attribute = next.cloned();
                i += 2;
            }
            "TYPE" if next.is_some() && ty.is_none() => {
                ty = next.map(|s| s.to_ascii_uppercase());
                i += 2;
            }
            f if FLAGS.contains(&f) => {
                // `NO_STEM 0` is off, `NO_STEM 1` and a lone `SORTABLE` are on.
                match next.map(String::as_str) {
                    Some("0") => i += 2,
                    Some("1") => {
                        flags.push(upper);
                        i += 2;
                    }
                    _ => {
                        flags.push(upper);
                        i += 1;
                    }
                }
            }
            _ => i += 1,
        }
    }
    let identifier = identifier?;
    Some(FieldInfo {
        name: attribute.unwrap_or_else(|| identifier.clone()),
        ty: ty.unwrap_or_default(),
        flags,
        identifier,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn b(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    fn arr(items: Vec<Value>) -> Value {
        Value::Array(items)
    }

    fn flat(items: Vec<(&str, Value)>) -> Value {
        arr(items.into_iter().flat_map(|(k, v)| [b(k), v]).collect())
    }

    /// `FT.INFO` as Redis 8 answers it (shortened).
    fn redis_info() -> Value {
        flat(vec![
            ("index_name", b("idx:docs")),
            ("index_options", arr(vec![])),
            (
                "index_definition",
                flat(vec![
                    ("key_type", b("HASH")),
                    ("prefixes", arr(vec![b("doc:"), b("item:")])),
                    ("default_score", b("1")),
                ]),
            ),
            (
                "attributes",
                arr(vec![
                    arr(vec![
                        b("identifier"),
                        b("title"),
                        b("attribute"),
                        b("title"),
                        b("type"),
                        b("TEXT"),
                        b("WEIGHT"),
                        b("1"),
                        b("SORTABLE"),
                    ]),
                    arr(vec![
                        b("identifier"),
                        b("$.price"),
                        b("attribute"),
                        b("price"),
                        b("type"),
                        b("NUMERIC"),
                    ]),
                ]),
            ),
            ("num_docs", b("12")),
            ("inverted_sz_mb", b("1.0")),
            ("total_index_memory_sz_mb", b("2.5")),
            ("hash_indexing_failures", b("9")),
            ("indexing", b("1")),
            ("percent_indexed", b("0.25")),
            (
                "Index Errors",
                flat(vec![
                    ("indexing failures", Value::Int(3)),
                    ("last indexing error", b("N/A")),
                ]),
            ),
        ])
    }

    /// `FT.INFO` as valkey-search answers it (shortened).
    fn valkey_info() -> Value {
        flat(vec![
            ("index_name", b("idx:docs")),
            (
                "index_definition",
                flat(vec![
                    ("key_type", b("JSON")),
                    ("prefixes", arr(vec![b("doc:")])),
                ]),
            ),
            (
                "attributes",
                arr(vec![
                    arr(vec![
                        b("identifier"),
                        b("title"),
                        b("attribute"),
                        b("title"),
                        b("user_indexed_memory"),
                        b("5"),
                        b("type"),
                        b("TEXT"),
                        b("WITH_SUFFIX_TRIE"),
                        b("0"),
                        b("NO_STEM"),
                        b("1"),
                        b("WEIGHT"),
                        b("1"),
                    ]),
                    arr(vec![
                        b("identifier"),
                        b("tags"),
                        b("attribute"),
                        b("tags"),
                        b("type"),
                        b("TAG"),
                        b("SEPARATOR"),
                        b(","),
                        b("CASESENSITIVE"),
                        b("0"),
                        b("size"),
                        b("1"),
                    ]),
                ]),
            ),
            ("num_docs", Value::Int(7)),
            ("hash_indexing_failures", Value::Int(2)),
            ("backfill_in_progress", Value::Int(0)),
            ("backfill_complete_percent", b("1.000000")),
            ("state", b("ready")),
        ])
    }

    #[test]
    fn redis_search_reply() {
        let i = parse_info("x", &redis_info());
        assert_eq!(i.name, "idx:docs");
        assert_eq!(i.on.as_deref(), Some("HASH"));
        assert_eq!(i.prefixes, ["doc:", "item:"]);
        assert_eq!(i.docs, Some(12));
        assert_eq!(i.bytes, Some(2_621_440));
        assert_eq!(i.indexing, Some(true));
        assert_eq!(i.progress, Some(0.25));
        // `Index Errors` wins over `hash_indexing_failures`.
        assert_eq!(i.failures, Some(3));
        assert_eq!(
            i.schema,
            [
                FieldInfo {
                    identifier: "title".into(),
                    name: "title".into(),
                    ty: "TEXT".into(),
                    flags: vec!["SORTABLE".into()],
                },
                FieldInfo {
                    identifier: "$.price".into(),
                    name: "price".into(),
                    ty: "NUMERIC".into(),
                    flags: vec![],
                },
            ]
        );
    }

    #[test]
    fn valkey_search_reply_leaves_out_what_it_lacks() {
        let i = parse_info("x", &valkey_info());
        assert_eq!(i.on.as_deref(), Some("JSON"));
        assert_eq!(i.docs, Some(7));
        assert_eq!(i.bytes, None, "valkey-search reports no index size");
        assert_eq!(i.indexing, Some(false));
        assert_eq!(i.progress, Some(1.0));
        assert_eq!(i.failures, Some(2));
        assert_eq!(i.schema.len(), 2);
        assert_eq!(i.schema[0].ty, "TEXT");
        // `WITH_SUFFIX_TRIE 0` is off, `NO_STEM 1` is on.
        assert_eq!(i.schema[0].flags, ["NO_STEM"]);
        assert!(i.schema[1].flags.is_empty(), "{:?}", i.schema[1].flags);
    }

    #[test]
    fn size_falls_back_to_the_sum_of_the_parts() {
        let v = flat(vec![
            ("inverted_sz_mb", b("1")),
            ("doc_table_size_mb", b("0.5")),
        ]);
        assert_eq!(index_bytes(&v), Some(1_572_864));
        assert_eq!(index_bytes(&flat(vec![])), None);
    }

    #[test]
    fn a_bare_reply_gives_the_name_and_nothing_else() {
        let i = parse_info("only", &arr(vec![]));
        assert_eq!(i.name, "only");
        assert_eq!(i.docs, None);
        assert!(i.schema.is_empty() && i.prefixes.is_empty());
    }

    #[test]
    fn resp3_attributes_are_read_from_maps() {
        let m = Value::Map(vec![
            (b("identifier"), b("a")),
            (b("attribute"), b("alias")),
            (b("type"), b("TAG")),
        ]);
        let f = parse_field(&m).unwrap();
        assert_eq!(
            (f.identifier.as_str(), f.name.as_str(), f.ty.as_str()),
            ("a", "alias", "TAG")
        );
    }

    #[test]
    fn names() {
        let v = arr(vec![b("a"), Value::Nil, b("b")]);
        assert_eq!(parse_names(&v), vec![b"a".to_vec(), b"b".to_vec()]);
        assert!(parse_names(&Value::Nil).is_empty());
    }

    fn value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Nil),
            any::<i64>().prop_map(Value::Int),
            proptest::collection::vec(any::<u8>(), 0..10).prop_map(Value::BulkString),
            prop_oneof![
                Just("identifier"),
                Just("attribute"),
                Just("type"),
                Just("SORTABLE"),
                Just("NO_STEM"),
                Just("0"),
                Just("1"),
                Just("attributes"),
                Just("num_docs"),
                Just("Index Errors"),
                Just("index_definition"),
                Just("prefixes"),
            ]
            .prop_map(|s| Value::BulkString(s.as_bytes().to_vec())),
            any::<f64>().prop_map(Value::Double),
        ];
        leaf.prop_recursive(4, 64, 8, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..8).prop_map(Value::Array),
                proptest::collection::vec((inner.clone(), inner), 0..4).prop_map(Value::Map),
            ]
        })
    }

    proptest! {
        #[test]
        fn parsers_never_panic(v in value()) {
            let i = parse_info("n", &v);
            if let Some(p) = i.progress {
                prop_assert!((0.0..=1.0).contains(&p));
            }
            let _ = parse_names(&v);
            let _ = parse_field(&v);
            let r = parse_results(&v);
            prop_assert!(r.docs.len() <= MAX_RESULTS);
            let _ = parse_profile(&v);
            for t in [parse_explain(&v), tree(&v)] {
                prop_assert!(t.len() <= MAX_TEXT_LINES);
                for l in t.lines() {
                    prop_assert!(!l.chars().any(char::is_control), "{l:?}");
                }
            }
        }
    }

    #[test]
    fn search_results_in_resp2() {
        // `FT.SEARCH idx * LIMIT 0 50` on a hash index and a JSON one.
        let v = arr(vec![
            Value::Int(1_204),
            b("product:1"),
            arr(vec![b("title"), b("red shoe"), b("price"), b("10")]),
            b("session:1"),
            arr(vec![b("$"), b(r#"{"user":{"id":"u1"}}"#)]),
            // A key without fields (NOCONTENT-like) still counts.
            b("bare"),
        ]);
        let r = parse_results(&v);
        assert_eq!(r.total, 1_204);
        let keys: Vec<_> = r.docs.iter().map(|d| d.key.as_str()).collect();
        assert_eq!(keys, ["product:1", "session:1", "bare"]);
        assert_eq!(r.docs[0].fields[1].1.as_bytes(), b"10");
        assert_eq!(r.docs[1].fields[0].0.as_bytes(), b"$");
        assert!(r.docs[2].fields.is_empty());
        assert!(!format!("{r:?}").contains("red shoe"));
    }

    #[test]
    fn search_results_in_resp3() {
        let m = |items: Vec<(&str, Value)>| {
            Value::Map(items.into_iter().map(|(k, v)| (b(k), v)).collect())
        };
        let v = m(vec![
            ("attributes", arr(vec![])),
            ("format", Value::SimpleString("STRING".into())),
            (
                "results",
                arr(vec![m(vec![
                    ("id", b("product:1")),
                    (
                        "extra_attributes",
                        m(vec![("title", b("red shoe")), ("price", b("10"))]),
                    ),
                    ("values", arr(vec![])),
                ])]),
            ),
            ("total_results", Value::Int(1)),
            ("warning", arr(vec![])),
        ]);
        let r = parse_results(&v);
        assert_eq!(r.total, 1);
        assert_eq!(r.docs[0].key, "product:1");
        assert_eq!(r.docs[0].fields.len(), 2);
        // FT.PROFILE in RESP3: `Results` and `Profile`.
        let p = m(vec![
            ("Results", v),
            ("Profile", m(vec![("Total profile time", b("0.1"))])),
        ]);
        let (r, t) = parse_profile(&p).unwrap();
        assert_eq!(r.docs.len(), 1);
        assert_eq!(t.lines(), ["Total profile time: 0.1"]);
    }

    #[test]
    fn at_most_50_results_are_kept() {
        let mut items = vec![Value::Int(500)];
        for i in 0..80 {
            items.push(b(&format!("k:{i}")));
            items.push(arr(vec![b("f"), b("v")]));
        }
        let r = parse_results(&arr(items));
        assert_eq!((r.total, r.docs.len()), (500, MAX_RESULTS));
    }

    #[test]
    fn explain_and_tree_lines_are_escaped() {
        let t = parse_explain(&b("INTERSECT {\r\n  @t:\x1b[2Jx\n}\n\n"));
        assert_eq!(t.lines(), ["INTERSECT {", "  @t:\\x1b[2Jx", "}"]);
        assert_eq!(format!("{t:?}"), "Text(<3 lines>)");
        let v = arr(vec![
            b("Iterators profile"),
            arr(vec![
                b("Term"),
                b("a\x1bb"),
                b("Child iterators"),
                arr(vec![
                    arr(vec![b("Type"), b("TAG")]),
                    arr(vec![b("Type"), b("NUMERIC")]),
                ]),
            ]),
            b("Warning"),
            arr(vec![b("None")]),
            b("Raw"),
            Value::BulkString(vec![0xff, b'x']),
        ]);
        assert_eq!(
            tree(&v).lines(),
            [
                "Iterators profile",
                "  Term: a\\x1bb",
                "  Child iterators",
                "    - Type: TAG",
                "    - Type: NUMERIC",
                "Warning: None",
                "Raw: \\xffx",
            ]
        );
        // A deep reply stops at 32 levels.
        let mut deep = b("x");
        for _ in 0..100 {
            deep = arr(vec![deep]);
        }
        assert!(tree(&deep).len() < 40);
    }

    fn server_error(wire: &str) -> RedisError {
        let Value::ServerError(e) = redis::parse_redis_value(wire.as_bytes()).unwrap() else {
            panic!("not an error: {wire}");
        };
        RedisError::from(e)
    }

    #[test]
    fn query_errors_keep_the_message_about_the_query_only() {
        let failed = |wire: &str| query_error("FT.SEARCH", &server_error(wire));
        assert_eq!(
            failed("-SEARCH_SYNTAX Syntax error at offset 7 near price\r\n"),
            LiveError::Failed("FT.SEARCH failed: Syntax error at offset 7 near price".into())
        );
        assert_eq!(
            failed("-ERR Unknown Index name\r\n"),
            LiveError::Failed("FT.SEARCH failed: Unknown Index name".into())
        );
        // valkey-search starts with a plain word: it is part of the message.
        assert_eq!(
            failed("-Invalid query string syntax\r\n"),
            LiveError::Failed("FT.SEARCH failed: Invalid query string syntax".into())
        );
        // NOPERM and unknown commands never keep the server's words.
        assert_eq!(
            failed("-NOPERM User zed-ro has no permissions to run the 'ft.search' command\r\n"),
            LiveError::Denied("FT.SEARCH".into())
        );
        assert_eq!(
            query_error(
                "FT.EXPLAIN",
                &server_error("-ERR unknown command 'FT.EXPLAIN', with args beginning with: \r\n")
            ),
            LiveError::Unsupported("FT.EXPLAIN".into())
        );
        // Other known kinds read as everywhere else.
        assert_eq!(
            failed("-LOADING Redis is loading the dataset in memory\r\n"),
            LiveError::Failed("FT.SEARCH failed".into())
        );
        // Escaped and cut at 200 characters.
        let long = format!("-SEARCH_SYNTAX near \x1b[2J{}\r\n", "y".repeat(500));
        let LiveError::Failed(why) = failed(&long) else {
            panic!();
        };
        let msg = why.strip_prefix("FT.SEARCH failed: ").unwrap();
        assert!(msg.starts_with("near \\x1b[2Jyyy"), "{msg}");
        assert_eq!(msg.chars().count(), QUERY_ERROR_CHARS);
        assert!(msg.ends_with('…') && !msg.contains('\x1b'));
    }
}
