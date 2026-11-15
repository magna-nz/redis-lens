//! Questions the dashboard waits for an answer to (spec "Web dashboard", Confirmations):
//! a write the gate answers with Ask or Type, a token command to save, writes to turn
//! on. The server holds the pending action under a random id, single use, for at most
//! [`TTL`]; the browser answers with `POST /api/confirm/<id>` or cancels with `DELETE`.
//! Whatever runs on the answer checks again (the write gate grants its permit then).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::auth::hex;

/// How long a question waits for its answer.
pub const TTL: Duration = Duration::from_secs(120);

/// The pending actions, by id.
#[derive(Debug)]
pub struct Pending<A> {
    map: HashMap<String, (Instant, A)>,
}

impl<A> Default for Pending<A> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
        }
    }
}

impl<A> Pending<A> {
    /// Keeps `action` until [`Self::take`] or [`TTL`] after `now`; returns its id: 16
    /// random bytes in hex. Questions past their time go first.
    pub fn insert(&mut self, action: A, now: Instant) -> Result<String, getrandom::Error> {
        self.prune(now);
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes)?;
        let id = hex(&bytes);
        self.map.insert(id.clone(), (now, action));
        Ok(id)
    }

    /// The action of `id`, once: removed whether or not it is still in time. `None` when
    /// there is no such question, or when it waited longer than [`TTL`].
    pub fn take(&mut self, id: &str, now: Instant) -> Option<A> {
        let (at, action) = self.map.remove(id)?;
        self.prune(now);
        (now.saturating_duration_since(at) <= TTL).then_some(action)
    }

    /// Forgets every question (the connection they were about was replaced).
    pub fn clear(&mut self) {
        self.map.clear();
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn prune(&mut self, now: Instant) {
        self.map
            .retain(|_, (at, _)| now.saturating_duration_since(*at) <= TTL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_is_taken_once() {
        let mut p = Pending::default();
        let now = Instant::now();
        let id = p.insert("publish", now).unwrap();
        assert_eq!(id.len(), 32);
        assert_eq!(p.take(&id, now), Some("publish"));
        assert_eq!(p.take(&id, now), None);
        assert!(p.is_empty());
    }

    #[test]
    fn ids_are_random_and_unknown_ids_get_nothing() {
        let mut p = Pending::default();
        let now = Instant::now();
        let a = p.insert(1, now).unwrap();
        let b = p.insert(2, now).unwrap();
        assert_ne!(a, b);
        assert_eq!(p.take("0".repeat(32).as_str(), now), None);
        assert_eq!(p.len(), 2);
    }

    #[test]
    fn a_question_expires_after_two_minutes() {
        let mut p = Pending::default();
        let t0 = Instant::now();
        let late = p.insert("late", t0).unwrap();
        let in_time = p.insert("in time", t0).unwrap();
        assert_eq!(p.take(&in_time, t0 + TTL), Some("in time"));
        assert_eq!(p.take(&late, t0 + TTL + Duration::from_millis(1)), None);
        // An expired one is gone, also for a later look.
        assert_eq!(p.take(&late, t0), None);
        // Inserting prunes what is past its time.
        p.insert("old", t0).unwrap();
        p.insert("new", t0 + TTL + Duration::from_secs(1)).unwrap();
        assert_eq!(p.len(), 1);
    }
}
