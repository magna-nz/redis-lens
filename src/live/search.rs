//! Live requests for the Search view. Owner: wave B. Phase 1: no writes, no values.
//!
//! `FT._LIST` and `FT.INFO` only: no query, no `FT.CREATE`, no `FT.DROPINDEX`. Redis
//! Search (built into Redis 8) and valkey-search answer `FT.INFO` with different fields;
//! a field the module does not return is `None` here, and the view leaves it out with a
//! reason instead of showing an error.

use redis::{RedisResult, Value};

use super::streams::{field, pairs, text, uint};
use super::{Conn, LiveError, error_for, query, query_pipe};
use crate::budget::Budget;

/// The most indexes whose `FT.INFO` is read.
pub const MAX_INDEXES: usize = 50;

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// `FT._LIST`, then `FT.INFO` for each index.
    Indexes,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Indexes(IndexList),
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
pub async fn run(conn: &mut Conn, budget: &Budget, req: Request) -> Result<Reply, LiveError> {
    match req {
        Request::Indexes => indexes(conn, budget).await.map(Reply::Indexes),
    }
}

async fn indexes(conn: &mut Conn, budget: &Budget) -> Result<IndexList, LiveError> {
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
        }
    }
}
