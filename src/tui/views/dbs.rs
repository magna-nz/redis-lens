//! State of the database list (`b`, spec "Database switch (P4)"). It is not a view of
//! its own: `App` owns the list as an overlay (`tui/dbs.rs` handles its keys and its one
//! request), and `ui/dbs.rs` draws it.

use crate::live::ReqId;
use crate::model::KeyspaceInfo;

/// Why `b` does nothing, in redis-lens's own words.
pub const OFF_CLUSTER: &str = "A cluster only has database 0.";
pub const OFF_NOT_READY: &str =
    "The connection is not ready yet: wait for it, then press b for the databases.";
pub const OFF_SCANNING: &str = "A scan is running: wait for it to finish, then press b.";
pub const OFF_NOT_CONNECTED: &str = "Not connected: connect first (0 Connections, then enter).";
pub const OFF_CLOSED: &str = "The live connection is closed: rescan (r on Patterns) first.";

/// Shown in the list when `INFO` was refused (`NOPERM`). Never the server's text.
pub const DENIED: &str =
    "The server refused INFO for this user: only the current database is listed.";
/// Shown in the list when `INFO keyspace` failed some other way.
pub const UNREAD: &str = "The databases could not be read: only the current database is listed.";
/// `s` on a Docker container.
pub const NO_PROFILE: &str =
    "A container has no saved profile, so nothing can be saved. enter switches for this session.";

/// Where the list is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// `INFO keyspace` is on its way.
    Loading(ReqId),
    /// Read.
    Ready,
    /// Not read: why (one of [`DENIED`], [`UNREAD`]).
    Unread(&'static str),
}

/// One database of the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub db: u32,
    /// Keys and expires, when read.
    pub counts: Option<(u64, u64)>,
}

/// The list on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbList {
    /// The database the session reads now.
    pub current: u32,
    pub rows: Vec<Row>,
    pub sel: usize,
    pub stage: Stage,
    /// The profile `s` would save to; `None` for a Docker container.
    pub saved_as: Option<String>,
    pub prod: bool,
    /// `s` was pressed on this database and the save question is open.
    pub ask: Option<u32>,
    /// A line for the list (`s` on a container).
    pub note: Option<&'static str>,
}

impl DbList {
    /// A list that waits for request `req`: the current database only until it answers.
    pub fn loading(current: u32, saved_as: Option<String>, prod: bool, req: ReqId) -> Self {
        Self {
            current,
            rows: vec![Row {
                db: current,
                counts: None,
            }],
            sel: 0,
            stage: Stage::Loading(req),
            saved_as,
            prod,
            ask: None,
            note: None,
        }
    }

    /// The reply: the current database and every database with keys. The selection
    /// stays on the current database.
    pub fn fill(&mut self, infos: &[KeyspaceInfo]) {
        let mut rows: Vec<Row> = infos
            .iter()
            .map(|k| Row {
                db: k.db,
                counts: Some((k.keys, k.expires)),
            })
            .collect();
        if !rows.iter().any(|r| r.db == self.current) {
            rows.push(Row {
                db: self.current,
                counts: Some((0, 0)),
            });
        }
        rows.sort_by_key(|r| r.db);
        rows.dedup_by_key(|r| r.db);
        self.sel = rows.iter().position(|r| r.db == self.current).unwrap_or(0);
        self.rows = rows;
        self.stage = Stage::Ready;
    }

    /// The read failed: the current database stays the only row.
    pub fn unread(&mut self, why: &'static str) {
        self.stage = Stage::Unread(why);
        self.sel = 0;
    }

    /// The selected database.
    pub fn selected(&self) -> u32 {
        self.rows.get(self.sel).map_or(self.current, |r| r.db)
    }

    pub fn waits_for(&self, id: ReqId) -> bool {
        self.stage == Stage::Loading(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(db: u32, keys: u64, expires: u64) -> KeyspaceInfo {
        KeyspaceInfo {
            db,
            keys,
            expires,
            ..KeyspaceInfo::default()
        }
    }

    #[test]
    fn the_current_database_is_listed_even_without_keys() {
        let mut l = DbList::loading(5, None, false, ReqId(1));
        assert_eq!(l.rows.len(), 1);
        l.fill(&[info(3, 7, 2), info(0, 4, 1)]);
        assert_eq!(l.rows.iter().map(|r| r.db).collect::<Vec<_>>(), [0, 3, 5]);
        assert_eq!(l.rows[2].counts, Some((0, 0)));
        assert_eq!(l.selected(), 5);
        assert_eq!(l.stage, Stage::Ready);
    }

    #[test]
    fn the_current_database_is_not_listed_twice() {
        let mut l = DbList::loading(0, None, false, ReqId(1));
        l.fill(&[info(0, 4, 1), info(1, 2, 0)]);
        assert_eq!(l.rows.len(), 2);
        assert_eq!(l.rows[0].counts, Some((4, 1)));
    }

    #[test]
    fn an_unread_list_keeps_only_the_current_database() {
        let mut l = DbList::loading(2, None, false, ReqId(1));
        l.unread(DENIED);
        assert_eq!(l.rows.len(), 1);
        assert_eq!(l.selected(), 2);
        assert!(!l.waits_for(ReqId(1)));
    }
}
