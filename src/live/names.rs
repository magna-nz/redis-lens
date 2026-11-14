//! Pages of key names for Browse before an analysis: `SCAN cursor [MATCH glob] COUNT
//! 1000`, one call per page, through the command budget, and nothing else about a key.
//! Never `KEYS`. Owner: scanner (TUI redesign).
//!
//! A [`Request::Page`] runs on one node: a `SCAN` cursor belongs to the node that gave
//! it. On a cluster the caller walks every shard's node with a [`Walk`], which hands out
//! the next request and its [`Dest`] (one cursor per node, sent with [`Follow::Never`]
//! so a redirect is never followed) and says when every node is done. Feed the names
//! to [`crate::pattern::names::NamePatterns`] to build the pattern tree.

use super::nodes::NodeInfo;
use super::{Dest, Follow, LiveConn, LiveError, LiveRequest, query};
use crate::budget::Budget;

/// The `COUNT` hint of each `SCAN`: about this many names a page.
pub const NAMES_COUNT: u64 = 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// One `SCAN cursor [MATCH glob] COUNT 1000`: the names it returns and the next
    /// cursor. One command, taken from the budget before it is sent.
    Page {
        /// Where the walk goes on; 0 to start.
        cursor: u64,
        /// `MATCH` this glob, if any.
        glob: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Page(Page),
}

/// One page of names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// The names `SCAN` returned, in its order. `SCAN` may return a name again on a
    /// later page (or twice in one): the caller dedups
    /// ([`crate::pattern::names::NamePatterns`] does).
    pub names: Vec<String>,
    /// Where the walk goes on; 0 when this node is done.
    pub cursor: u64,
    /// Names that are not UTF-8: left out of `names`, but keys all the same (count them
    /// with [`crate::pattern::names::NamePatterns::add_unreadable`]).
    pub unreadable: u64,
}

/// Runs one request. Its one command goes through `budget`.
pub async fn run(
    conn: &mut impl LiveConn,
    budget: &Budget,
    req: Request,
) -> Result<Reply, LiveError> {
    match req {
        Request::Page { cursor, glob } => page(conn, budget, cursor, glob.as_deref())
            .await
            .map(Reply::Page),
    }
}

async fn page(
    conn: &mut impl LiveConn,
    budget: &Budget,
    cursor: u64,
    glob: Option<&str>,
) -> Result<Page, LiveError> {
    let mut cmd = redis::cmd("SCAN");
    cmd.arg(cursor);
    if let Some(g) = glob {
        cmd.arg("MATCH").arg(g);
    }
    cmd.arg("COUNT").arg(NAMES_COUNT);
    let (next, raw): (u64, Vec<Vec<u8>>) = query(conn, budget, "SCAN", &cmd).await?;
    let mut names = Vec::with_capacity(raw.len());
    let mut unreadable = 0;
    for r in raw {
        match String::from_utf8(r) {
            Ok(n) => names.push(n),
            Err(_) => unreadable += 1,
        }
    }
    Ok(Page {
        names,
        cursor: next,
        unreadable,
    })
}

/// A request the [`Walk`] hands out: send `req` to `dest`, then give the page back with
/// [`Walk::take`] and `node`.
#[derive(Debug, Clone, PartialEq)]
pub struct Ask {
    /// The node it is for: an index into the session's node table (0 on a standalone
    /// server).
    pub node: usize,
    pub dest: Dest,
    pub req: LiveRequest,
}

/// Where each node's walk is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeWalk {
    /// The cursor to send next (0 before the first page).
    At(u64),
    /// The cursor came back to 0.
    Done,
    /// Left out after a failure ([`Walk::skip`]).
    Skipped,
}

/// The walk over the names of a standalone server or of every node of a cluster, one
/// page at a time.
///
/// On a cluster the nodes take turns, one page each in slot order, so the names read
/// so far come from every shard alike and the estimate from them stays fair while the
/// walk goes on. Nodes without a connection are left out ([`Walk::missing`]): the walk
/// then never covers the whole keyspace ([`Walk::complete`] is false).
///
/// Pure state: it sends nothing itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Walk {
    /// Node index and its state, in walk order.
    nodes: Vec<(usize, NodeWalk)>,
    /// Nodes left out: no connection.
    missing: Vec<usize>,
    cluster: bool,
    glob: Option<String>,
    /// Where the next turn starts in `nodes`.
    turn: usize,
}

impl Walk {
    /// A standalone server: one node, sent to [`Dest::node`]`(0)`.
    pub fn standalone(glob: Option<String>) -> Self {
        Self {
            nodes: vec![(0, NodeWalk::At(0))],
            missing: Vec::new(),
            cluster: false,
            glob,
            turn: 0,
        }
    }

    /// A cluster: every connected node of `nodes` (the session's node table, one node
    /// per shard, in slot order), each sent to [`Dest::node`]`(i)` with
    /// [`Follow::Never`]. An empty table is a standalone server.
    pub fn cluster(nodes: &[NodeInfo], glob: Option<String>) -> Self {
        if nodes.is_empty() {
            return Self::standalone(glob);
        }
        let (walk, missing): (Vec<usize>, Vec<usize>) =
            (0..nodes.len()).partition(|&i| nodes[i].connected);
        Self {
            nodes: walk.into_iter().map(|i| (i, NodeWalk::At(0))).collect(),
            missing,
            cluster: true,
            glob,
            turn: 0,
        }
    }

    /// The next page to ask for, from the next node whose turn it is and that is not
    /// done; `None` when every node is done (or skipped). Ask again only after the page
    /// came back ([`Walk::take`]) or failed: two requests for one node at once would
    /// send its cursor twice.
    pub fn next_ask(&mut self) -> Option<Ask> {
        let n = self.nodes.len();
        for k in 0..n {
            let at = (self.turn + k) % n;
            if let (node, NodeWalk::At(cursor)) = self.nodes[at] {
                self.turn = (at + 1) % n;
                let dest = if self.cluster {
                    Dest::node(node).follow(Follow::Never)
                } else {
                    Dest::node(0)
                };
                return Some(Ask {
                    node,
                    dest,
                    req: LiveRequest::Names(Request::Page {
                        cursor,
                        glob: self.glob.clone(),
                    }),
                });
            }
        }
        None
    }

    /// The page `node` sent back: its cursor goes on, or the node is done.
    pub fn take(&mut self, node: usize, page: &Page) {
        if let Some((_, state)) = self.nodes.iter_mut().find(|(i, _)| *i == node)
            && matches!(state, NodeWalk::At(_))
        {
            *state = if page.cursor == 0 {
                NodeWalk::Done
            } else {
                NodeWalk::At(page.cursor)
            };
        }
    }

    /// Leaves `node` out of the rest of the walk (its request failed and is not tried
    /// again): the walk can still end, but is not [`Walk::complete`].
    pub fn skip(&mut self, node: usize) {
        if let Some((_, state)) = self.nodes.iter_mut().find(|(i, _)| *i == node)
            && *state != NodeWalk::Done
        {
            *state = NodeWalk::Skipped;
        }
    }

    /// Every node is done or skipped: [`Walk::next_ask`] has nothing more.
    pub fn done(&self) -> bool {
        self.nodes
            .iter()
            .all(|(_, s)| matches!(s, NodeWalk::Done | NodeWalk::Skipped))
    }

    /// Every name was read: every node walked to the end, none skipped or missing. Pass
    /// it to [`crate::pattern::names::NamePatterns::patterns`].
    pub fn complete(&self) -> bool {
        self.missing.is_empty() && self.nodes.iter().all(|(_, s)| *s == NodeWalk::Done)
    }

    /// `(nodes walked to the end, nodes in the walk)`: `(1, 1)` or `(0, 1)` on a
    /// standalone server.
    pub fn nodes_done(&self) -> (usize, usize) {
        let done = self
            .nodes
            .iter()
            .filter(|(_, s)| *s == NodeWalk::Done)
            .count();
        (done, self.nodes.len())
    }

    /// Nodes of the cluster left out because discovery has no connection to them.
    pub fn missing(&self) -> &[usize] {
        &self.missing
    }

    /// True on a cluster.
    pub fn is_cluster(&self) -> bool {
        self.cluster
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Endpoint, NodeRole};

    fn page(names: &[&str], cursor: u64) -> Page {
        Page {
            names: names.iter().map(|s| s.to_string()).collect(),
            cursor,
            unreadable: 0,
        }
    }

    fn info(i: usize, connected: bool) -> NodeInfo {
        NodeInfo {
            id: format!("n{i}"),
            endpoint: Endpoint {
                host: "10.0.0.1".into(),
                port: 7000 + i as u16,
                tls: false,
            },
            role: NodeRole::Primary,
            slots: Vec::new(),
            connected,
        }
    }

    fn cursor_of(a: &Ask) -> u64 {
        match &a.req {
            LiveRequest::Names(Request::Page { cursor, .. }) => *cursor,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_standalone_walk_follows_the_cursor_to_zero() {
        let mut w = Walk::standalone(None);
        assert!(!w.is_cluster());
        let a = w.next_ask().unwrap();
        assert_eq!(
            (a.node, a.dest.clone(), cursor_of(&a)),
            (0, Dest::node(0), 0)
        );
        w.take(0, &page(&["a"], 17));
        let a = w.next_ask().unwrap();
        assert_eq!(cursor_of(&a), 17);
        assert_eq!(w.nodes_done(), (0, 1));
        w.take(0, &page(&["b"], 0));
        assert!(w.done() && w.complete());
        assert!(w.next_ask().is_none());
        assert_eq!(w.nodes_done(), (1, 1));
        // A late page for a finished node changes nothing.
        w.take(0, &page(&[], 5));
        assert!(w.done());
    }

    #[test]
    fn a_cluster_walk_takes_turns_and_never_follows_redirects() {
        let nodes = [info(0, true), info(1, false), info(2, true), info(3, true)];
        let mut w = Walk::cluster(&nodes, Some("user:*".into()));
        assert!(w.is_cluster());
        assert_eq!(w.missing(), [1]);
        let mut order = Vec::new();
        for _ in 0..3 {
            let a = w.next_ask().unwrap();
            assert_eq!(a.dest, Dest::node(a.node).follow(Follow::Never));
            assert!(matches!(
                &a.req,
                LiveRequest::Names(Request::Page { glob: Some(g), .. }) if g == "user:*"
            ));
            order.push(a.node);
            // Node 2 is done after its first page; the others go on.
            w.take(a.node, &page(&["x"], if a.node == 2 { 0 } else { 9 }));
        }
        assert_eq!(order, [0, 2, 3]);
        // Node 2 is skipped from now on, and the cursors go on.
        let a = w.next_ask().unwrap();
        assert_eq!((a.node, cursor_of(&a)), (0, 9));
        w.take(0, &page(&[], 0));
        let a = w.next_ask().unwrap();
        assert_eq!(a.node, 3);
        w.take(3, &page(&[], 0));
        assert!(w.done());
        assert_eq!(w.nodes_done(), (3, 3));
        // Node 1 was never read: not complete.
        assert!(!w.complete());
        assert!(w.next_ask().is_none());
    }

    #[test]
    fn a_skipped_node_ends_the_walk_but_not_completely() {
        let nodes = [info(0, true), info(1, true)];
        let mut w = Walk::cluster(&nodes, None);
        let a = w.next_ask().unwrap();
        w.skip(a.node);
        let b = w.next_ask().unwrap();
        assert_eq!(b.node, 1);
        w.take(1, &page(&[], 0));
        assert!(w.done());
        assert!(!w.complete());
        // Skipping a finished node keeps it done.
        w.skip(1);
        assert_eq!(w.nodes_done(), (1, 2));
        // An empty table is a standalone server.
        assert!(!Walk::cluster(&[], None).is_cluster());
        // No connected node: done at once, never complete.
        let w = Walk::cluster(&[info(0, false)], None);
        assert!(w.done() && !w.complete());
    }

    /// A connection that answers each `SCAN` with the next scripted page and records
    /// the commands.
    struct Fake {
        log: Vec<Vec<String>>,
        pages: Vec<(u64, Vec<Vec<u8>>)>,
    }

    impl redis::aio::ConnectionLike for Fake {
        fn req_packed_command<'a>(
            &'a mut self,
            cmd: &'a redis::Cmd,
        ) -> redis::RedisFuture<'a, redis::Value> {
            let words: Vec<String> = cmd
                .args_iter()
                .map(|a| match a {
                    redis::Arg::Simple(b) => String::from_utf8_lossy(b).into_owned(),
                    _ => String::new(),
                })
                .collect();
            self.log.push(words);
            let (next, names) = self.pages.remove(0);
            let v = redis::Value::Array(vec![
                redis::Value::BulkString(next.to_string().into_bytes()),
                redis::Value::Array(names.into_iter().map(redis::Value::BulkString).collect()),
            ]);
            Box::pin(async move { Ok(v) })
        }

        fn req_packed_commands<'a>(
            &'a mut self,
            _pipe: &'a redis::Pipeline,
            _offset: usize,
            _count: usize,
        ) -> redis::RedisFuture<'a, Vec<redis::Value>> {
            panic!("the names pager sends no pipeline")
        }

        fn get_db(&self) -> i64 {
            0
        }
    }

    #[tokio::test]
    async fn a_page_is_one_scan_through_the_budget() {
        let mut c = Fake {
            log: Vec::new(),
            pages: vec![
                (
                    42,
                    vec![b"user:1".to_vec(), vec![0xff, 0x01], b"user:2".to_vec()],
                ),
                (0, vec![b"cart:9".to_vec()]),
            ],
        };
        let b = Budget::new(None);
        let r = run(
            &mut c,
            &b,
            Request::Page {
                cursor: 0,
                glob: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            r,
            Reply::Page(Page {
                names: vec!["user:1".into(), "user:2".into()],
                cursor: 42,
                unreadable: 1,
            })
        );
        let r = run(
            &mut c,
            &b,
            Request::Page {
                cursor: 42,
                glob: Some("cart:*".into()),
            },
        )
        .await
        .unwrap();
        assert!(matches!(r, Reply::Page(Page { cursor: 0, .. })));
        assert_eq!(b.taken(), 2);
        assert_eq!(
            c.log,
            [
                vec!["SCAN", "0", "COUNT", "1000"],
                vec!["SCAN", "42", "MATCH", "cart:*", "COUNT", "1000"],
            ]
        );
    }
}
