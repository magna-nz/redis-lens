//! The nodes the live task talks to (spec "Cluster live views (P4)"): one for a
//! standalone server, one per shard of a cluster (the node discovery chose for it), each
//! with its own connection, or none when discovery could not use it, and its own
//! command budget, shared with the scan of that node.
//!
//! The slot map starts as discovery read it and remembers every `MOVED` followed, for
//! the session. No node is ever added after discovery: a redirect to an address that
//! is not in the table is not followed ([`super::MOVING`]).

use std::sync::{Arc, Mutex, MutexGuard};

use crate::budget::Budget;
use crate::conn::slot::{SLOTS, key_slot};
use crate::model::{Endpoint, NodeRole};

use super::{LiveError, To};

/// What the views know of one node: from discovery, so before the scan finishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfo {
    /// The node ID the cluster announced (empty when unknown).
    pub id: String,
    /// The address as the cluster announced it.
    pub endpoint: Endpoint,
    pub role: NodeRole,
    /// The shard's slot ranges, inclusive, ascending.
    pub slots: Vec<(u16, u16)>,
    /// Discovery has a connection to it.
    pub connected: bool,
}

/// One node of the table: its connection (`None` when discovery could not use it) and
/// its budget.
pub struct Node<C> {
    pub info: NodeInfo,
    pub conn: Option<C>,
    pub budget: Arc<Budget>,
}

/// The node table of one live task.
pub struct Table<C> {
    nodes: Vec<Node<C>>,
    /// False for a standalone server: every request goes to the one node.
    cluster: bool,
    state: Mutex<State>,
}

struct State {
    /// The index of the node that serves each slot, [`NO_NODE`] when none does.
    owner: Vec<u16>,
    /// Nodes whose connection was lost.
    lost: Vec<bool>,
}

const NO_NODE: u16 = u16::MAX;

/// Why a key's request went nowhere: no node of the table serves its slot.
pub const NO_OWNER: &str =
    "no connected node serves that key's slot; rescan to read the cluster's slots again";

/// Why a request to `ep` was not sent: discovery has no connection to it.
pub fn not_connected(ep: &Endpoint) -> String {
    format!("{ep} is not connected (discovery could not use it), so nothing is sent to it")
}

/// Why a request to `ep` failed: its connection was lost.
pub fn lost(ep: &Endpoint) -> String {
    format!("the connection to {ep} was lost; rescan to connect again")
}

impl<C: Clone> Table<C> {
    /// A standalone server: one node, which every request goes to.
    pub fn single(conn: C, budget: Arc<Budget>) -> Self {
        let info = NodeInfo {
            id: String::new(),
            endpoint: Endpoint {
                host: String::new(),
                port: 0,
                tls: false,
            },
            role: NodeRole::Primary,
            slots: Vec::new(),
            connected: true,
        };
        Self {
            nodes: vec![Node {
                info,
                conn: Some(conn),
                budget,
            }],
            cluster: false,
            state: Mutex::new(State {
                owner: Vec::new(),
                lost: vec![false],
            }),
        }
    }

    /// A cluster's nodes, in slot order, as discovery left them.
    pub fn cluster(nodes: Vec<Node<C>>) -> Self {
        let mut owner = vec![NO_NODE; usize::from(SLOTS)];
        for (i, n) in nodes.iter().enumerate() {
            let Ok(i) = u16::try_from(i) else { break };
            for &(a, b) in &n.info.slots {
                for s in a..=b.min(SLOTS - 1) {
                    owner[usize::from(s)] = i;
                }
            }
        }
        let lost = vec![false; nodes.len()];
        Self {
            nodes,
            cluster: true,
            state: Mutex::new(State { owner, lost }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Every update is a plain assignment, so a panic elsewhere leaves it consistent.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn is_cluster(&self) -> bool {
        self.cluster
    }

    /// The node `to` goes to. A standalone table sends everything to its one node.
    pub fn pick(&self, to: &To) -> Result<usize, LiveError> {
        if !self.cluster {
            return Ok(0);
        }
        match to {
            To::Node(i) if *i < self.nodes.len() => Ok(*i),
            To::Node(_) => Err(LiveError::Failed(
                "that node is not part of this session; rescan".to_string(),
            )),
            To::Key(k) => self.owner_of(key_slot(k)),
        }
    }

    /// The node that serves `slot`, as the slot map has it now.
    pub fn owner_of(&self, slot: u16) -> Result<usize, LiveError> {
        match self.state().owner.get(usize::from(slot)) {
            Some(&i) if i != NO_NODE => Ok(usize::from(i)),
            _ => Err(LiveError::Failed(NO_OWNER.to_string())),
        }
    }

    /// Node `i`'s connection and budget, or why nothing can be sent to it.
    pub fn conn(&self, i: usize) -> Result<(C, Arc<Budget>), LiveError> {
        let n = self
            .nodes
            .get(i)
            .ok_or_else(|| LiveError::Failed(NO_OWNER.to_string()))?;
        let Some(c) = &n.conn else {
            return Err(LiveError::Failed(not_connected(&n.info.endpoint)));
        };
        if self.state().lost.get(i).copied().unwrap_or(false) {
            return Err(LiveError::Failed(lost(&n.info.endpoint)));
        }
        Ok((c.clone(), Arc::clone(&n.budget)))
    }

    /// The node at `host:port` as announced, when the table has a connection to it.
    /// `host` empty means "the host of node `from`" (how a server without a known
    /// endpoint answers).
    pub fn find(&self, host: &str, port: u16, from: usize) -> Option<usize> {
        let host = if host.is_empty() {
            self.nodes.get(from)?.info.endpoint.host.as_str()
        } else {
            host
        };
        let host = host.trim_start_matches('[').trim_end_matches(']');
        self.nodes.iter().position(|n| {
            n.conn.is_some()
                && n.info.endpoint.port == port
                && n.info
                    .endpoint
                    .host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .eq_ignore_ascii_case(host)
        })
    }

    /// A `MOVED` was followed: `slot` is served by node `to` from now on.
    pub fn moved(&self, slot: u16, to: usize) {
        if let (Some(o), Ok(to)) = (
            self.state().owner.get_mut(usize::from(slot)),
            u16::try_from(to),
        ) {
            *o = to;
        }
    }

    /// Node `i`'s connection was lost. True when no node is left.
    pub fn mark_lost(&self, i: usize) -> bool {
        let mut s = self.state();
        if let Some(l) = s.lost.get_mut(i) {
            *l = true;
        }
        self.nodes
            .iter()
            .zip(&s.lost)
            .all(|(n, &lost)| lost || n.conn.is_none())
    }

    /// Node `i`'s announced address.
    pub fn endpoint(&self, i: usize) -> Option<&Endpoint> {
        self.nodes.get(i).map(|n| &n.info.endpoint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(port: u16, slots: Vec<(u16, u16)>, conn: Option<u8>) -> Node<u8> {
        Node {
            info: NodeInfo {
                id: format!("id{port}"),
                endpoint: Endpoint {
                    host: "10.0.0.1".into(),
                    port,
                    tls: false,
                },
                role: NodeRole::Primary,
                slots,
                connected: conn.is_some(),
            },
            conn,
            budget: Budget::new(None),
        }
    }

    fn three() -> Table<u8> {
        Table::cluster(vec![
            node(7000, vec![(0, 5460)], Some(0)),
            node(7001, vec![(5461, 10922)], Some(1)),
            node(7002, vec![(10923, 16383)], None),
        ])
    }

    #[test]
    fn keys_go_to_their_slot_owner() {
        let t = three();
        // "foo" is slot 12182, "bar" 5061.
        assert_eq!(t.pick(&To::Key(b"bar".to_vec())), Ok(0));
        assert_eq!(t.pick(&To::Key(b"foo".to_vec())), Ok(2));
        assert_eq!(t.pick(&To::Node(1)), Ok(1));
        assert!(t.pick(&To::Node(3)).is_err());
        // Node 2 has no connection: say so, with its address.
        assert_eq!(
            t.conn(2).err(),
            Some(LiveError::Failed(not_connected(t.endpoint(2).unwrap())))
        );
        assert_eq!(t.conn(1).map(|(c, _)| c), Ok(1));
    }

    #[test]
    fn a_standalone_table_sends_everything_to_its_node() {
        let t = Table::single(9u8, Budget::new(None));
        assert_eq!(t.pick(&To::Key(b"foo".to_vec())), Ok(0));
        assert_eq!(t.pick(&To::Node(4)), Ok(0));
    }

    #[test]
    fn uncovered_slots_have_no_owner() {
        let t = Table::cluster(vec![node(7000, vec![(0, 100)], Some(0))]);
        assert_eq!(
            t.pick(&To::Key(b"foo".to_vec())),
            Err(LiveError::Failed(NO_OWNER.into()))
        );
    }

    #[test]
    fn moved_updates_the_slot_map() {
        let t = three();
        t.moved(12182, 1);
        assert_eq!(t.pick(&To::Key(b"foo".to_vec())), Ok(1));
    }

    #[test]
    fn only_connected_nodes_are_found() {
        let t = three();
        assert_eq!(t.find("10.0.0.1", 7001, 0), Some(1));
        assert_eq!(t.find("", 7001, 0), Some(1));
        assert_eq!(t.find("10.0.0.1", 7002, 0), None, "not connected");
        assert_eq!(t.find("10.0.0.9", 7001, 0), None, "unknown address");
    }

    #[test]
    fn lost_nodes() {
        let t = three();
        assert!(!t.mark_lost(0));
        assert_eq!(
            t.conn(0).err(),
            Some(LiveError::Failed(lost(t.endpoint(0).unwrap())))
        );
        assert!(t.mark_lost(1), "node 2 never had a connection");
    }
}
