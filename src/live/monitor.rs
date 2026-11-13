//! The Monitor panel's run (spec "3 Server", "Monitor (P2)"): reading `MONITOR` lines on
//! a connection of its own (`conn::open_monitor`), and ranking the keys they name for
//! Hot keys (spec "4 Hot keys").
//!
//! A run stops after [`MAX_TIME`] or [`MAX_LINES`] lines, whichever comes first
//! ([`forward`]). Every line is kept as an [`Entry`]: its parts, with the credentials
//! already replaced by [`command::MASK`] ([`command::secrets`]), so a password the
//! server did not redact is never held, and cut to [`MAX_LINE_BYTES`], so a large value
//! cannot fill memory. Lines are values (spec "Values (P2)"): on screen only, never in a
//! snapshot, a report, a log, error text or `Debug` output (an [`Entry`] prints its
//! argument count only).

use std::collections::HashMap;
use std::time::Duration;

use futures::{Stream, StreamExt};
use redis::Value;

use super::command::{self, MonitorLine, Secret};
use super::server::{CommandKeys, keys_in};

/// A run stops after this many lines.
pub const MAX_LINES: usize = 10_000;
/// A run stops after this long.
pub const MAX_TIME: Duration = Duration::from_secs(30);
/// The most bytes of a line's arguments kept; the rest are counted in [`Entry::cut`].
pub const MAX_LINE_BYTES: usize = 1024;
/// A line with no argument boundary (one huge argument) longer than this is dropped.
const MAX_SINGLE_ARG_LINE: usize = 64 * 1024;
/// Lines are passed on at most this often.
pub const BATCH_EVERY: Duration = Duration::from_millis(100);
/// Hot keys show this many keys from a run.
pub const TOP_KEYS: usize = 50;

/// When a run stops by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub lines: usize,
    pub time: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            lines: MAX_LINES,
            time: MAX_TIME,
        }
    }
}

/// One `MONITOR` line as kept: credentials masked, arguments past [`MAX_LINE_BYTES`] cut.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// `Debug` shows the time, database and argument count only.
    pub line: MonitorLine,
    /// Arguments left out because the line was too long.
    pub cut: u64,
}

impl Entry {
    /// The command and arguments as one line to draw: `command::masked_line`.
    pub fn command_line(&self) -> String {
        command::masked_line(&self.line.args)
    }

    /// The line's time, seconds since the epoch, as the server wrote it.
    pub fn unix(&self) -> Option<i64> {
        self.line.time.split('.').next()?.parse().ok()
    }

    /// Milliseconds past the second.
    pub fn millis(&self) -> u32 {
        let frac = self.line.time.split_once('.').map_or("", |(_, f)| f);
        let digits: String = frac.chars().take(3).collect();
        format!("{digits:0<3}").parse().unwrap_or(0)
    }
}

/// Why a run ended by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// [`Limits::time`] passed.
    TimeUp,
    /// [`Limits::lines`] lines arrived.
    LineCap,
    /// The connection was lost.
    Lost,
    /// Whoever took the lines is gone.
    Gone,
}

/// The byte positions in `line` where one quoted argument ends and the next starts: the
/// `"` of each `" "`. The server quotes every argument and escapes every `"` inside one
/// (`\"`), so `" "` is found only between arguments.
fn boundaries(line: &str) -> impl Iterator<Item = usize> + '_ {
    line.as_bytes()
        .windows(3)
        .enumerate()
        .filter(|(_, w)| *w == b"\" \"")
        .map(|(i, _)| i)
}

/// Reads one line of `MONITOR`: `None` for anything else (`OK`). Long lines are cut at
/// an argument boundary; credentials are masked.
pub fn read_line(text: &str) -> Option<Entry> {
    let (kept, cut) = if text.len() <= MAX_LINE_BYTES {
        (text, 0)
    } else {
        let all: Vec<usize> = boundaries(text).collect();
        let at = all
            .iter()
            .rev()
            .find(|b| **b < MAX_LINE_BYTES)
            .or(all.first())
            .copied();
        match at {
            Some(b) => {
                let after = all.iter().filter(|x| **x > b).count() as u64;
                (&text[..=b], after + 1)
            }
            None if text.len() <= MAX_SINGLE_ARG_LINE => (text, 0),
            None => return None,
        }
    };
    let mut line = command::parse_monitor(kept)?;
    mask(&mut line.args);
    Some(Entry { line, cut })
}

/// Replaces the credentials in `args` with the mask, as `masked_line` draws them.
fn mask(args: &mut [Vec<u8>]) {
    for (i, s) in command::secrets(args).into_iter().enumerate() {
        match s {
            Some(Secret::Whole) => args[i] = command::MASK.as_bytes().to_vec(),
            Some(Secret::Rule) => {
                let first = args[i].first().copied().unwrap_or(b'>');
                let mut v = vec![first];
                v.extend_from_slice(command::MASK.as_bytes());
                args[i] = v;
            }
            None => {}
        }
    }
}

fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::SimpleString(s) => Some(s.clone()),
        Value::BulkString(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Value::VerbatimString { text, .. } => Some(text.clone()),
        _ => None,
    }
}

/// Reads `lines` until a limit is reached or the connection is lost, passing the lines
/// on to `batch` at most every [`BATCH_EVERY`], oldest first; what is left is passed on
/// before it returns. Never passes on more than `limits.lines` lines in all.
pub async fn forward<S>(
    lines: &mut S,
    limits: Limits,
    mut batch: impl FnMut(Vec<Entry>) -> bool,
) -> End
where
    S: Stream<Item = Value> + Unpin,
{
    let deadline = tokio::time::Instant::now() + limits.time;
    let mut every = tokio::time::interval(BATCH_EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pending: Vec<Entry> = Vec::new();
    let mut count = 0usize;
    let mut flush = |pending: &mut Vec<Entry>| pending.is_empty() || batch(std::mem::take(pending));
    if limits.lines == 0 {
        return End::LineCap;
    }
    loop {
        tokio::select! {
            v = lines.next() => {
                let Some(v) = v else {
                    return if flush(&mut pending) { End::Lost } else { End::Gone };
                };
                if let Some(e) = text_of(&v).as_deref().and_then(read_line) {
                    pending.push(e);
                    count += 1;
                    if count >= limits.lines {
                        return if flush(&mut pending) { End::LineCap } else { End::Gone };
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                return if flush(&mut pending) { End::TimeUp } else { End::Gone };
            }
            _ = every.tick() => {
                if !flush(&mut pending) {
                    return End::Gone;
                }
            }
        }
    }
}

/// Keys ranked by how often the lines of a Monitor run name them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ranking {
    /// Up to [`TOP_KEYS`] key names, as text (bytes that are not UTF-8 become `�`), with
    /// how many times they appear; most first, then by name.
    pub keys: Vec<(String, u64)>,
    /// Lines in the run.
    pub lines: u64,
    /// Lines whose command's keys could not be placed: its key specs are unknown.
    pub unknown: u64,
}

/// Ranks the keys `entries` name, placed by the key specs in `known`.
pub fn rank(entries: &[Entry], known: &HashMap<String, CommandKeys>) -> Ranking {
    let mut counts: HashMap<&[u8], u64> = HashMap::new();
    let mut unknown = 0;
    for e in entries {
        let args = &e.line.args;
        match keys_in(known, args) {
            Some(pos) => {
                for p in pos {
                    *counts.entry(args[p].as_slice()).or_default() += 1;
                }
            }
            None => unknown += 1,
        }
    }
    let mut keys: Vec<(String, u64)> = counts
        .into_iter()
        .map(|(k, n)| (String::from_utf8_lossy(k).into_owned(), n))
        .collect();
    keys.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    keys.truncate(TOP_KEYS);
    Ranking {
        keys,
        lines: entries.len() as u64,
        unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::server::{Begin, Find, KeySpec};

    fn simple(s: &str) -> Value {
        Value::SimpleString(s.to_string())
    }

    #[test]
    fn lines_are_read_and_credentials_masked_on_arrival() {
        const MARK: &str = "hunter2-monitor";
        let e = read_line(&format!(
            r#"1700000000.123456 [0 127.0.0.1:5] "AUTH" "u" "{MARK}""#
        ))
        .unwrap();
        assert_eq!(
            e.line.args,
            [b"AUTH".to_vec(), b"***".to_vec(), b"***".to_vec()]
        );
        assert_eq!(e.command_line(), "AUTH *** ***");
        assert_eq!((e.unix(), e.millis()), (Some(1_700_000_000), 123));
        let e = read_line(&format!(
            r#"1.5 [0 lua] "ACL" "SETUSER" "bob" ">{MARK}" "CONFIG""#
        ))
        .unwrap();
        assert_eq!(e.command_line(), "ACL SETUSER bob >*** CONFIG");
        assert!(!crate::value::leaks(&format!("{e:?}"), MARK));
        let e = read_line(&format!(r#"1.5 [0 x:1] "SET" "k" "{MARK}""#)).unwrap();
        // A value is kept (on screen only) but never in Debug.
        assert!(e.command_line().contains(MARK));
        assert!(!crate::value::leaks(&format!("{e:?}"), MARK));
        assert!(read_line("OK").is_none());
        assert_eq!(read_line("2 [0 x:1] \"PING\"").unwrap().millis(), 0);
    }

    #[test]
    fn long_lines_are_cut_at_an_argument() {
        let big = "v".repeat(5000);
        let text = format!(r#"1.0 [0 a:1] "MSET" "k1" "{big}" "k2" "x" "k3" "y""#);
        let e = read_line(&text).unwrap();
        assert_eq!(e.line.args, [b"MSET".to_vec(), b"k1".to_vec()]);
        assert_eq!(e.cut, 5);
        let quote = format!(r#"1.0 [0 a:1] "SET" "a\" \"b" "{big}""#);
        let e = read_line(&quote).unwrap();
        assert_eq!(e.line.args, [b"SET".to_vec(), b"a\" \"b".to_vec()]);
        assert_eq!(e.cut, 1);
        assert!(read_line(&format!(r#"1.0 [0 a:1] "{}""#, "x".repeat(70_000))).is_none());
    }

    fn get_spec() -> CommandKeys {
        CommandKeys::Specs(vec![KeySpec {
            begin: Begin::Index(1),
            find: Find::Range {
                last: 0,
                step: 1,
                limit: 0,
            },
        }])
    }

    #[test]
    fn keys_are_ranked_and_unknown_commands_counted() {
        let lines: Vec<Entry> = [
            r#"1.0 [0 a:1] "GET" "user:1""#,
            r#"1.0 [0 a:1] "GET" "user:2""#,
            r#"1.0 [0 a:1] "get" "user:1""#,
            r#"1.0 [0 a:1] "PING""#,
            r#"1.0 [0 a:1] "WHAT" "x""#,
        ]
        .iter()
        .map(|l| read_line(l).unwrap())
        .collect();
        let mut known = HashMap::new();
        known.insert("get".into(), get_spec());
        known.insert("ping".into(), CommandKeys::Specs(vec![]));
        let r = rank(&lines, &known);
        assert_eq!(r.keys, [("user:1".into(), 2), ("user:2".into(), 1)]);
        assert_eq!((r.lines, r.unknown), (5, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_stops_at_its_line_cap() {
        let mut s = futures::stream::iter(
            std::iter::once(simple("OK"))
                .chain((0..50).map(|i| simple(&format!(r#"1.0 [0 a:1] "GET" "k{i}""#)))),
        )
        .chain(futures::stream::pending());
        let mut got = Vec::new();
        let end = forward(
            &mut s,
            Limits {
                lines: 7,
                time: MAX_TIME,
            },
            |b| {
                got.extend(b);
                true
            },
        )
        .await;
        assert_eq!(end, End::LineCap);
        assert_eq!(got.len(), 7);
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_stops_after_its_time_and_when_lost() {
        let mut quiet = futures::stream::pending::<Value>();
        let start = tokio::time::Instant::now();
        let end = forward(&mut quiet, Limits::default(), |_| true).await;
        assert_eq!(end, End::TimeUp);
        assert_eq!(start.elapsed(), MAX_TIME);
        let mut lost = futures::stream::iter(vec![simple(r#"1.0 [0 a:1] "GET" "k""#)]);
        let mut got = 0;
        let end = forward(&mut lost, Limits::default(), |b| {
            got += b.len();
            true
        })
        .await;
        assert_eq!((end, got), (End::Lost, 1));
        let mut more = futures::stream::iter(vec![simple(r#"1.0 [0 a:1] "GET" "k""#)])
            .chain(futures::stream::pending());
        let end = forward(&mut more, Limits::default(), |_| false).await;
        assert_eq!(end, End::Gone);
    }
}
