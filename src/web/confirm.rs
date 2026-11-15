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

/// How often [`prune_on_timer`] drops the questions past their time, so an action that
/// holds a password (a connection to add) does not outlive its question by long.
pub const PRUNE_EVERY: Duration = Duration::from_secs(10);

/// The pending actions, by id.
#[derive(Debug)]
pub struct Pending<A> {
    map: HashMap<String, (Instant, A)>,
    ttl: Duration,
}

impl<A> Default for Pending<A> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            ttl: TTL,
        }
    }
}

impl<A> Pending<A> {
    /// Questions that wait `ttl` instead of [`TTL`]: for tests.
    #[cfg(test)]
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            map: HashMap::new(),
            ttl,
        }
    }

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
        (now.saturating_duration_since(at) <= self.ttl).then_some(action)
    }

    /// `f` of the action of `id`, without taking it; `None` when there is no such id.
    pub fn peek_with<R>(&self, id: &str, f: impl FnOnce(&A) -> R) -> Option<R> {
        self.map.get(id).map(|(_, a)| f(a))
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

    /// Drops the questions past their time, and their actions with them.
    pub fn prune(&mut self, now: Instant) {
        let ttl = self.ttl;
        self.map
            .retain(|_, (at, _)| now.saturating_duration_since(*at) <= ttl);
    }
}

/// Prunes `pending` every `every` until `stop` turns true or nothing else holds it.
pub async fn prune_on_timer<A>(
    pending: std::sync::Weak<std::sync::Mutex<Pending<A>>>,
    every: Duration,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = stop.wait_for(|v| *v) => return,
        }
        let Some(p) = pending.upgrade() else { return };
        // Dropped outside the lock: an action's own drop never runs under it.
        let expired = {
            let mut g = p.lock().unwrap_or_else(|e| e.into_inner());
            let before = std::mem::take(&mut g.map);
            let now = Instant::now();
            let ttl = g.ttl;
            let (keep, gone): (HashMap<_, _>, HashMap<_, _>) = before
                .into_iter()
                .partition(|(_, (at, _))| now.saturating_duration_since(*at) <= ttl);
            g.map = keep;
            gone
        };
        drop(expired);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S7: the timer drops a question past its time, and the action it held (a password)
    /// goes with it, with no request coming in; it stops with the dashboard.
    #[tokio::test]
    async fn the_timer_drops_expired_questions_and_what_they_held() {
        struct Held(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Held {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let p = std::sync::Arc::new(std::sync::Mutex::new(Pending::with_ttl(
            Duration::from_millis(100),
        )));
        let fresh = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        p.lock()
            .unwrap()
            .insert(Held(dropped.clone()), Instant::now())
            .unwrap();
        let (tx, rx) = tokio::sync::watch::channel(false);
        let timer = tokio::spawn(prune_on_timer(
            std::sync::Arc::downgrade(&p),
            Duration::from_millis(20),
            rx,
        ));
        let t0 = Instant::now();
        while !dropped.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(t0.elapsed() < Duration::from_secs(5), "not pruned");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(p.lock().unwrap().is_empty());
        // One still in time stays.
        p.lock()
            .unwrap()
            .insert(
                Held(fresh.clone()),
                Instant::now() + Duration::from_secs(60),
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(p.lock().unwrap().len(), 1);
        assert!(!fresh.load(std::sync::atomic::Ordering::SeqCst));
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), timer)
            .await
            .expect("the timer stops with the dashboard")
            .unwrap();
    }

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
