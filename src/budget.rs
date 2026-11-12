//! The command budget one connected profile shares between its scan and its live views
//! (spec "Shared command budget").
//!
//! Each [`Budget::take`] books the next `n / per_sec` seconds of the budget's time line
//! and returns when that slot ends. A booking starts where the previous one ended, so
//! bookings never overlap and the commands of every caller together stay at or below
//! `per_sec` on average, however many callers there are. After an idle gap a booking may
//! start at most [`MAX_BACKDATE`] in the past: idle time does not build up a burst.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

/// How far in the past a booking may start after an idle gap. Covers the time between
/// a caller's last slot ending and its next `take`, so a steady caller is not slowed
/// down by its own work between commands, without letting idle time add up.
pub const MAX_BACKDATE: Duration = Duration::from_millis(10);

/// Commands per second for one connected profile. Everything that sends commands for
/// that profile (the scan, the live connection's polls and lookups) takes from the same
/// `Budget`, so together they stay at or below `per_sec` on average.
#[derive(Debug)]
pub struct Budget {
    /// `None` for no limit; never `Some(0)`.
    per_sec: Option<u32>,
    created: Instant,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    /// Where the last booking ended: the earliest start of the next one.
    next: Instant,
    /// Commands counted so far.
    taken: u64,
}

impl Budget {
    /// `None` and `Some(0)` mean no limit.
    pub fn new(per_sec: Option<u32>) -> Arc<Self> {
        let created = Instant::now();
        Arc::new(Self {
            per_sec: per_sec.filter(|n| *n > 0),
            created,
            state: Mutex::new(State {
                next: created,
                taken: 0,
            }),
        })
    }

    /// The limit, `None` for none.
    pub fn per_sec(&self) -> Option<u32> {
        self.per_sec
    }

    /// Commands counted so far, by every caller.
    pub fn taken(&self) -> u64 {
        self.lock().taken
    }

    /// Waits until `n` more commands fit the budget, then counts them as sent. A pipeline
    /// of `n` commands takes `n`.
    pub async fn take(&self, n: u32) {
        let end = {
            let mut s = self.lock();
            s.taken += u64::from(n);
            let Some(rate) = self.per_sec else {
                return;
            };
            let now = Instant::now();
            let earliest = now
                .checked_sub(MAX_BACKDATE)
                .map_or(self.created, |t| t.max(self.created));
            let start = s.next.max(earliest);
            let end = start + slot(n, rate);
            s.next = end;
            end
        };
        tokio::time::sleep_until(end).await;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // The state stays consistent whatever panicked while holding the lock: every
        // update is a plain assignment.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// `n / rate` seconds, rounded up to the nanosecond so the rate is never exceeded.
fn slot(n: u32, rate: u32) -> Duration {
    let nanos = (u128::from(n) * 1_000_000_000).div_ceil(u128::from(rate));
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn sequential_takes_keep_the_rate() {
        for (rate, n) in [(100u32, 1_000u32), (5_000, 20_000), (7, 50)] {
            let b = Budget::new(Some(rate));
            let t0 = Instant::now();
            for _ in 0..n {
                b.take(1).await;
            }
            let took = t0.elapsed();
            let min = Duration::from_secs_f64(f64::from(n) / f64::from(rate));
            assert!(took >= min, "rate {rate}: {took:?} < {min:?}");
            // And it does not wait far longer than needed.
            assert!(took <= min + Duration::from_millis(1), "{took:?}");
            assert_eq!(b.taken(), u64::from(n));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_batch_counts_in_full() {
        let b = Budget::new(Some(10));
        let t0 = Instant::now();
        b.take(25).await;
        assert_eq!(t0.elapsed(), Duration::from_millis(2_500));
        assert_eq!(b.taken(), 25);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_callers_share_the_rate_without_overlapping() {
        let b = Budget::new(Some(100));
        let t0 = Instant::now();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..10 {
            let b = Arc::clone(&b);
            tasks.spawn(async move {
                let mut ends = Vec::new();
                for _ in 0..50 {
                    b.take(1).await;
                    ends.push(Instant::now());
                }
                ends
            });
        }
        let mut ends: Vec<Instant> = tasks.join_all().await.into_iter().flatten().collect();
        // 500 commands at 100 a second: 5 s for all callers together, not per caller.
        let took = t0.elapsed();
        assert!(took >= Duration::from_secs(5), "{took:?}");
        assert!(took <= Duration::from_millis(5_010), "{took:?}");
        assert_eq!(b.taken(), 500);
        // One slot per command: no two end within a slot of each other.
        ends.sort();
        for w in ends.windows(2) {
            assert!(
                w[1] - w[0] >= Duration::from_millis(10),
                "{:?}",
                w[1] - w[0]
            );
        }
        // In any window of one second, at most 100 commands end.
        for (i, t) in ends.iter().enumerate() {
            let within = ends[i..]
                .iter()
                .take_while(|e| **e < *t + Duration::from_secs(1))
                .count();
            assert!(within <= 100, "{within} commands within a second");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn idle_time_does_not_build_a_burst() {
        let b = Budget::new(Some(100));
        b.take(1).await;
        tokio::time::sleep(Duration::from_secs(60)).await;
        // After a minute idle, 100 commands still take about a second: only 10 ms of
        // the gap may be used.
        let t0 = Instant::now();
        for _ in 0..100 {
            b.take(1).await;
        }
        let took = t0.elapsed();
        assert!(took >= Duration::from_millis(990), "{took:?}");
        assert!(took <= Duration::from_secs(1), "{took:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn no_booking_starts_before_the_budget_was_created() {
        // Created, then used at once: the first slot starts at creation, not 10 ms
        // before it.
        let b = Budget::new(Some(100));
        let t0 = Instant::now();
        b.take(1).await;
        assert_eq!(t0.elapsed(), Duration::from_millis(10));
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_limit_take_never_waits() {
        for rate in [None, Some(0)] {
            let b = Budget::new(rate);
            assert_eq!(b.per_sec(), None);
            let t0 = Instant::now();
            for _ in 0..1_000 {
                b.take(1_000).await;
            }
            assert_eq!(t0.elapsed(), Duration::ZERO);
            assert_eq!(b.taken(), 1_000_000);
        }
        assert_eq!(Budget::new(Some(42)).per_sec(), Some(42));
    }

    #[test]
    fn slots_round_up() {
        assert_eq!(slot(1, 3), Duration::from_nanos(333_333_334));
        assert_eq!(slot(3, 3), Duration::from_secs(1));
        assert_eq!(slot(0, 3), Duration::ZERO);
        assert_eq!(slot(u32::MAX, 1), Duration::from_secs(u64::from(u32::MAX)));
    }
}
