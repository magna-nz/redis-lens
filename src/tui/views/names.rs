//! Browse › Keys before an analysis: the pattern tree from key names alone (brief,
//! decision 3; spec "1 Patterns"). Pure state: `App` sends the requests it hands out
//! and gives the answers back.
//!
//! The names are read with the engine's pager ([`crate::live::names`]): one
//! `SCAN cursor COUNT 1000` a page, through the command budget, on a cluster every
//! primary in turn ([`Walk`]). Nothing else about a key is read. The first time
//! Browse › Keys shows, [`FIRST_PAGES`] pages are read one after the other; `n` reads
//! one more. Only one page is ever in flight, so a node's cursor is never sent twice.
//!
//! The patterns are inferred with [`NamePatterns`], exactly as an analysis infers
//! them, and each pattern's keys are estimated as (its names / names read) × `DBSIZE`.
//! A pattern that names one key (its only name is the pattern itself: no part of it
//! varies, braces of a hash tag such as `{user1}.following` included) counts the names
//! seen, never scaled. The rows carry no type, size or TTL (`no_ttl` 0 means unknown): the draw
//! code never shows those, nor a `▲`, for them.

use crate::live::names::{Ask, Page, Reply, Walk};
use crate::live::nodes::NodeInfo;
use crate::live::{LiveError, LiveReply, ReqId};
use crate::model::PatternStats;
use crate::pattern::names::NamePatterns;

/// Pages read on their own the first time Browse › Keys shows: about 8,000 names.
pub const FIRST_PAGES: u64 = 8;

/// Why the names can't be read when the server refused `SCAN`.
pub const SCAN_REFUSED: &str =
    "The server refused SCAN for this user, so no key names can be read.";

/// Why a cluster node's key names are left out after its page failed twice (the
/// address goes before it).
pub const NODE_SKIPPED: &str =
    "did not answer SCAN twice, so its key names are left out and the other nodes go on.";

/// The names read so far and where the walk is.
#[derive(Debug, Clone, Default)]
pub struct NamesState {
    /// `None` until the reading starts.
    walk: Option<Walk>,
    tree: NamePatterns,
    /// Pages answered.
    pub pages: u64,
    /// Pages to read before stopping: [`FIRST_PAGES`], then one more per `n`.
    want: u64,
    /// The request in flight, and what it asked, to send again when the connection it
    /// went on was replaced.
    pending: Option<(ReqId, Ask)>,
    /// A page to ask again: it failed, or its connection went.
    retry: Option<Ask>,
    /// Why the last page failed, in redis-lens's own words.
    pub error: Option<String>,
    /// Nodes whose `SCAN` was refused, left out of the walk.
    pub refused: Vec<usize>,
    /// Nodes of a cluster left out because a page failed again when `n` asked for it
    /// again ([`NODE_SKIPPED`]): the other nodes' walk goes on without them.
    pub skipped: Vec<usize>,
    /// The cluster node whose last page failed, once: a second failure leaves it out.
    failed_node: Option<usize>,
    /// The rows: one per pattern, largest estimate first. Rebuilt after every page.
    pub stats: Vec<PatternStats>,
    /// Distinct names seen per pattern, by the order of `stats`.
    seen: Vec<u64>,
    /// The `DBSIZE` the estimates scale to.
    dbsize: u64,
}

impl NamesState {
    /// True once the reading started (the first page was asked for).
    pub fn started(&self) -> bool {
        self.walk.is_some()
    }

    /// Starts the walk over `nodes` (the session's node table, empty on a standalone
    /// server), reading [`FIRST_PAGES`] pages. `dbsize`: the key count to scale to.
    pub fn start(&mut self, nodes: &[NodeInfo], dbsize: u64) {
        *self = Self {
            walk: Some(Walk::cluster(nodes, None)),
            want: FIRST_PAGES,
            dbsize,
            ..Self::default()
        };
    }

    /// The next page to ask for, if one should be: nothing is in flight, the walk is
    /// not done, and fewer than the wanted pages were read. The caller sends it and
    /// calls [`Self::sent`].
    pub fn next_page(&mut self) -> Option<Ask> {
        if self.pending.is_some() || self.pages >= self.want {
            return None;
        }
        if let Some(ask) = self.retry.take() {
            return Some(ask);
        }
        self.walk.as_mut()?.next_ask()
    }

    /// The page `ask` went out as request `id`. `None` (not sent: the live connection
    /// is not open) keeps it to ask again.
    pub fn sent(&mut self, id: Option<ReqId>, ask: Ask) {
        match id {
            Some(id) => self.pending = Some((id, ask)),
            None => self.retry = Some(ask),
        }
    }

    /// True while a page is asked for.
    pub fn is_reading(&self) -> bool {
        self.pending.is_some()
    }

    /// True when `id` is the page in flight.
    pub fn waits_for(&self, id: ReqId) -> bool {
        self.pending.as_ref().is_some_and(|(p, _)| *p == id)
    }

    /// The answer to the page in flight. A refused `SCAN` leaves its node out and says
    /// why; any other failure stops the reading until `n` asks again.
    pub fn reply(&mut self, result: Result<LiveReply, LiveError>) {
        let Some((_, ask)) = self.pending.take() else {
            return;
        };
        match result {
            Ok(LiveReply::Names(Reply::Page(page))) => self.take(ask.node, &page),
            Ok(_) => {}
            Err(LiveError::Denied(_)) => {
                if let Some(w) = self.walk.as_mut() {
                    w.skip(ask.node);
                }
                self.refused.push(ask.node);
                self.error = Some(SCAN_REFUSED.to_string());
                self.rebuild();
            }
            // A cluster node that fails again after `n` asked once more is left out, so
            // one shard that does not answer does not stall every other shard's walk.
            Err(LiveError::Failed(_) | LiveError::Unsupported(_))
                if self.failed_node == Some(ask.node)
                    && self.walk.as_ref().is_some_and(Walk::is_cluster) =>
            {
                if let Some(w) = self.walk.as_mut() {
                    w.skip(ask.node);
                }
                self.skipped.push(ask.node);
                self.failed_node = None;
                self.error = None;
                self.rebuild();
            }
            Err(e) => {
                self.failed_node = Some(ask.node);
                self.retry = Some(ask);
                // Stops here: `n` tries again.
                self.want = self.pages;
                self.error = Some(match e {
                    LiveError::Closed => "The live connection is closed.".to_string(),
                    e => format!("Could not read the key names: {e}."),
                });
            }
        }
    }

    fn take(&mut self, node: usize, page: &Page) {
        if self.failed_node == Some(node) {
            self.failed_node = None;
        }
        if let Some(w) = self.walk.as_mut() {
            w.take(node, page);
        }
        self.tree.add(page.names.iter().cloned());
        self.tree.add_unreadable(page.unreadable);
        self.pages += 1;
        self.error = None;
        self.rebuild();
    }

    /// The live connection the page in flight went on was replaced (a new run): it will
    /// never be answered, so it is asked again on the next connection.
    pub fn lost(&mut self) {
        if let Some((_, ask)) = self.pending.take() {
            self.retry = Some(ask);
        }
    }

    /// `n`: one more page. `Err` says why there is none to read.
    pub fn more(&mut self) -> Result<(), String> {
        if !self.started() {
            return Err("Key names are read once the live connection is open.".to_string());
        }
        if self.pending.is_some() {
            return Err("Still reading key names…".to_string());
        }
        if self.done() && self.retry.is_none() {
            return Err(format!(
                "Every key name is read: {}.",
                crate::format::grouped(self.names_read())
            ));
        }
        self.error = None;
        self.want = self.pages + 1;
        Ok(())
    }

    /// Every node was walked to the end, or left out.
    pub fn done(&self) -> bool {
        self.walk.as_ref().is_some_and(Walk::done)
    }

    /// Every name was read: the counts are exact, not estimates.
    pub fn complete(&self) -> bool {
        self.walk.as_ref().is_some_and(Walk::complete)
    }

    /// Nodes of the cluster left out because discovery has no connection to them.
    pub fn missing(&self) -> &[usize] {
        self.walk.as_ref().map_or(&[], Walk::missing)
    }

    /// `(done, of)` nodes on a cluster; `None` on a standalone server.
    pub fn nodes_done(&self) -> Option<(usize, usize)> {
        let w = self.walk.as_ref()?;
        w.is_cluster().then(|| w.nodes_done())
    }

    /// Names read: distinct names plus those that are not UTF-8.
    pub fn names_read(&self) -> u64 {
        self.tree.names_read()
    }

    /// The `DBSIZE` the estimates scale to.
    pub fn dbsize(&self) -> u64 {
        self.dbsize
    }

    /// Distinct names seen of the pattern at `idx` of [`Self::stats`].
    pub fn seen(&self, idx: usize) -> u64 {
        self.seen.get(idx).copied().unwrap_or(0)
    }

    /// True for a pattern that names one key: its only name read is the pattern
    /// itself. Decided from the names, not from the pattern's characters: a key name
    /// with a hash tag (`{user1}.following`, `cfg{prod}`) keeps its braces as they are.
    pub fn is_literal(&self, pattern: &str) -> bool {
        self.seen_of(pattern) == 1 && self.tree.contains(pattern)
    }

    /// Distinct names seen of `pattern`.
    pub fn seen_of(&self, pattern: &str) -> u64 {
        self.stats
            .iter()
            .position(|p| p.pattern == pattern)
            .map_or(0, |i| self.seen(i))
    }

    /// The rows anew from the names: the estimates scaled to `DBSIZE` unless every
    /// name was read, and a pattern without placeholders counted as seen.
    fn rebuild(&mut self) {
        let complete = self.complete();
        let rows = self.tree.patterns(self.dbsize, complete);
        self.seen = rows.iter().map(|p| p.names_seen).collect();
        self.stats = rows
            .iter()
            .map(|p| {
                let mut s = p.as_pattern_stats();
                if p.names_seen == 1 && self.tree.contains(&p.pattern) {
                    s.est_keys = p.names_seen;
                }
                s
            })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{Dest, LiveRequest};

    fn page(names: &[&str], cursor: u64) -> Result<LiveReply, LiveError> {
        Ok(LiveReply::Names(Reply::Page(Page {
            names: names.iter().map(|s| s.to_string()).collect(),
            cursor,
            unreadable: 0,
        })))
    }

    #[test]
    fn reads_the_first_pages_one_at_a_time_then_waits_for_n() {
        let mut s = NamesState::default();
        assert!(s.next_page().is_none(), "nothing before the start");
        s.start(&[], 1_000);
        for i in 0..FIRST_PAGES {
            let ask = s.next_page().expect("a page");
            assert_eq!(ask.dest, Dest::node(0));
            assert!(matches!(ask.req, LiveRequest::Names(_)));
            s.sent(Some(ReqId(i)), ask);
            assert!(s.next_page().is_none(), "one page in flight at a time");
            let name = format!("user:{i}");
            s.reply(page(&[name.as_str()], 7 + i));
        }
        assert_eq!(s.pages, FIRST_PAGES);
        assert!(s.next_page().is_none(), "stops after the first pages");
        s.more().unwrap();
        let ask = s.next_page().expect("n reads one more");
        let LiveRequest::Names(crate::live::names::Request::Page { cursor, .. }) = ask.req else {
            panic!("a names page");
        };
        assert_eq!(
            cursor,
            7 + FIRST_PAGES - 1,
            "the walk goes on from its cursor"
        );
    }

    #[test]
    fn estimates_scale_to_dbsize_and_a_literal_counts_as_seen() {
        let mut s = NamesState::default();
        s.start(&[], 1_000);
        let ask = s.next_page().unwrap();
        s.sent(Some(ReqId(1)), ask);
        s.reply(page(&["user:1", "user:2", "user:3", "queue:emails"], 9));
        let row =
            |s: &NamesState, p: &str| s.stats.iter().find(|r| r.pattern == p).unwrap().clone();
        assert_eq!(s.names_read(), 4);
        assert_eq!(row(&s, "user:{id}").est_keys, 750, "3 of 4 names × 1,000");
        assert_eq!(s.seen_of("user:{id}"), 3);
        assert_eq!(row(&s, "queue:emails").est_keys, 1, "one key, never scaled");
        assert!(s.is_literal("queue:emails"));
        assert!(!s.is_literal("user:{id}"));
        assert!(!s.complete());
        // The walk ends: every name read, so the counts are exact.
        let ask = s.next_page().unwrap();
        s.sent(Some(ReqId(2)), ask);
        s.reply(page(&["user:4"], 0));
        assert!(s.done() && s.complete());
        assert_eq!(row(&s, "user:{id}").est_keys, 4);
        assert!(s.more().unwrap_err().contains("Every key name is read: 5"));
    }

    /// A key name with a hash tag keeps its braces: it is still one key, never scaled.
    /// A pattern the names collapsed into is scaled even when one name was read.
    #[test]
    fn a_hash_tagged_name_is_one_key_not_a_pattern() {
        let mut s = NamesState::default();
        s.start(&[], 1_000);
        let ask = s.next_page().unwrap();
        s.sent(Some(ReqId(1)), ask);
        s.reply(page(
            &[
                "{user1}.following",
                "cfg{prod}",
                "user:1",
                "user:2",
                "order:7",
            ],
            9,
        ));
        let row =
            |s: &NamesState, p: &str| s.stats.iter().find(|r| r.pattern == p).unwrap().clone();
        for literal in ["{user1}.following", "cfg{prod}"] {
            assert!(s.is_literal(literal), "{literal}: {:?}", s.stats);
            assert_eq!(row(&s, literal).est_keys, 1, "{literal}");
        }
        assert!(!s.is_literal("user:{id}"));
        assert_eq!(row(&s, "user:{id}").est_keys, 400);
        // One name read, but its pattern varies: estimated.
        assert!(!s.is_literal("order:{id}"), "{:?}", s.stats);
        assert_eq!(row(&s, "order:{id}").est_keys, 200);
    }

    fn nodes(n: usize) -> Vec<NodeInfo> {
        (0..n)
            .map(|i| NodeInfo {
                id: format!("n{i}"),
                endpoint: crate::model::Endpoint {
                    host: format!("10.0.1.{}", 11 + i),
                    port: 6379,
                    tls: false,
                },
                role: crate::model::NodeRole::Primary,
                slots: vec![(i as u16 * 100, i as u16 * 100 + 99)],
                connected: true,
            })
            .collect()
    }

    /// A cluster node whose page fails, and fails again when `n` asks once more, is
    /// left out with a reason; the other nodes' walk goes on. A standalone server's
    /// failing page is asked again on every `n`, as before.
    #[test]
    fn a_cluster_node_that_fails_twice_is_left_out_and_the_walk_goes_on() {
        let mut s = NamesState::default();
        s.start(&nodes(2), 1_000);
        let mut id = 0;
        let mut ask = |s: &mut NamesState| {
            let a = s.next_page()?;
            id += 1;
            s.sent(Some(ReqId(id)), a.clone());
            Some(a)
        };
        let first = ask(&mut s).unwrap();
        s.reply(Err(LiveError::Failed("timeout".into())));
        assert!(s.next_page().is_none(), "stops until n");
        assert!(s.error.is_some());
        s.more().unwrap();
        assert_eq!(ask(&mut s), Some(first.clone()), "n asks the same page");
        s.reply(Err(LiveError::Failed("timeout".into())));
        assert_eq!(s.skipped, [first.node]);
        assert_eq!(s.error, None);
        // The other node is read on.
        let other = ask(&mut s).expect("the other node's page");
        assert_ne!(other.node, first.node);
        s.reply(page(&["user:1", "user:2"], 0));
        assert!(s.done() && !s.complete(), "a node left out: never complete");
        assert!(NODE_SKIPPED.contains("left out"));

        // Standalone: never left out, `n` asks again.
        let mut s = NamesState::default();
        s.start(&[], 10);
        for i in 0..3 {
            let a = s.next_page().unwrap();
            s.sent(Some(ReqId(i)), a);
            s.reply(Err(LiveError::Failed("timeout".into())));
            assert!(s.skipped.is_empty());
            s.more().unwrap();
        }
        assert!(s.next_page().is_some());
        // A closed connection never leaves a node out.
        let mut s = NamesState::default();
        s.start(&nodes(2), 10);
        for i in 0..3 {
            let a = s.next_page().unwrap();
            s.sent(Some(ReqId(i)), a);
            s.reply(Err(LiveError::Closed));
            s.more().unwrap();
        }
        assert!(s.skipped.is_empty());
    }

    #[test]
    fn a_refused_scan_leaves_its_node_out_and_says_why() {
        let mut s = NamesState::default();
        s.start(&[], 10);
        let ask = s.next_page().unwrap();
        s.sent(Some(ReqId(1)), ask);
        s.reply(Err(LiveError::Denied("SCAN".into())));
        assert_eq!(s.error.as_deref(), Some(SCAN_REFUSED));
        assert_eq!(s.refused, [0]);
        assert!(s.done() && !s.complete());
        assert!(s.next_page().is_none());
    }

    #[test]
    fn a_failed_page_is_asked_again_on_n_and_a_lost_one_at_once() {
        let mut s = NamesState::default();
        s.start(&[], 10);
        let first = s.next_page().unwrap();
        s.sent(Some(ReqId(1)), first.clone());
        s.reply(Err(LiveError::Failed("timeout".into())));
        assert!(s.error.as_deref().unwrap().contains("timeout"));
        assert!(s.next_page().is_none(), "stops until n");
        s.more().unwrap();
        assert_eq!(s.next_page(), Some(first.clone()), "the same page again");
        s.sent(Some(ReqId(2)), first.clone());
        s.lost();
        assert!(!s.is_reading());
        assert_eq!(
            s.next_page(),
            Some(first),
            "asked again on the new connection"
        );
    }
}
