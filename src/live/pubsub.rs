//! Live requests for the Pub/Sub view. Owner: wave B. No writes: nothing here publishes.
//!
//! - **Counts** (phase 1): `PUBSUB CHANNELS`, `PUBSUB NUMSUB` and `PUBSUB NUMPAT`, on the
//!   live connection. They read no payload.
//! - **Shard channels** (P4): `PUBSUB SHARDCHANNELS` (the first [`MAX_CHANNELS`]) and
//!   `PUBSUB SHARDNUMSUB` in batches of 100, on one node (Redis 7.0 and later, Valkey).
//!   On a cluster the view sends one request to each node.
//! - **The live tail** (phase 2): [`subscribe`] sends `SUBSCRIBE` or `PSUBSCRIBE` on a
//!   connection of its own (`conn::open_subscriber`, RESP3), and [`parse_push`] turns
//!   what it pushes into [`TailMessage`]s. A payload is a value ([`Raw`]): shown on
//!   screen, never in `Debug`, errors or logs (spec "Values").

use redis::{PushInfo, PushKind, RedisResult, Value};

use super::streams::{text, uint};
use super::{CONNECTION_LOST, LiveConn, LiveError, error_for, query_pipe};
use crate::budget::Budget;
use crate::conn::ConnError;
use crate::value::Raw;

/// The most channels listed (spec "5 Pub/Sub": the first 1,000).
pub const MAX_CHANNELS: usize = 1_000;
/// Channels per `PUBSUB NUMSUB` command.
const NUMSUB_BATCH: usize = 100;

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// The active channels with their subscriber counts, and the pattern count.
    Counts,
    /// The node's active shard channels with their subscriber counts.
    ShardCounts,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Counts(Counts),
    ShardCounts(ShardCounts),
}

/// What one node says of its shard channels.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ShardCounts {
    /// The first [`MAX_CHANNELS`] shard channels, most subscribers first, then by name.
    pub channels: Vec<Channel>,
    /// How many are active on the node.
    pub total_channels: usize,
    /// Why a count is missing, in redis-lens's own words.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Counts {
    /// The first [`MAX_CHANNELS`] channels, most subscribers first, then by name.
    pub channels: Vec<Channel>,
    /// How many channels are active. More than `channels.len()` when the list was cut.
    pub total_channels: usize,
    /// `PUBSUB NUMPAT`: patterns subscribed with `PSUBSCRIBE`. `None` when refused.
    pub patterns: Option<u64>,
    /// Why a count is missing, in redis-lens's own words.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    pub name: String,
    /// Subscribers to this channel by name (`SUBSCRIBE`; pattern subscribers are not
    /// counted). `None` when `PUBSUB NUMSUB` was refused.
    pub subscribers: Option<u64>,
}

/// Runs one request. Every command goes through `budget`.
pub async fn run(
    conn: &mut impl LiveConn,
    budget: &Budget,
    req: Request,
) -> Result<Reply, LiveError> {
    match req {
        Request::Counts => counts(conn, budget).await.map(Reply::Counts),
        Request::ShardCounts => shard_counts(conn, budget).await.map(Reply::ShardCounts),
    }
}

/// `PUBSUB SHARDCHANNELS`, then `PUBSUB SHARDNUMSUB` in batches of 100. A refused or
/// unknown `SHARDCHANNELS` fails the request (the view leaves the list out and says
/// why); a refused `SHARDNUMSUB` leaves the counts out with a note.
async fn shard_counts(conn: &mut impl LiveConn, budget: &Budget) -> Result<ShardCounts, LiveError> {
    let mut cmd = redis::cmd("PUBSUB");
    cmd.arg("SHARDCHANNELS");
    let channels: Value = super::query(conn, budget, "PUBSUB SHARDCHANNELS", &cmd).await?;
    let mut names = parse_channels(&channels);
    let mut out = ShardCounts {
        total_channels: names.len(),
        ..ShardCounts::default()
    };
    names.truncate(MAX_CHANNELS);
    let subs = numsub(conn, budget, "SHARDNUMSUB", &names, &mut out.notes).await?;
    out.channels = merge(&names, subs.as_deref());
    Ok(out)
}

/// `PUBSUB <sub> …` for `names` in batches of [`NUMSUB_BATCH`], in one pipeline. `None`
/// when a batch was refused, with the reason in `notes`.
async fn numsub(
    conn: &mut impl LiveConn,
    budget: &Budget,
    sub: &str,
    names: &[Vec<u8>],
    notes: &mut Vec<String>,
) -> Result<Option<Vec<(String, u64)>>, LiveError> {
    let mut subs: Option<Vec<(String, u64)>> = Some(Vec::new());
    if names.is_empty() {
        return Ok(subs);
    }
    let name = format!("PUBSUB {sub}");
    let pipe = numsub_pipe(sub, names);
    let replies: Vec<RedisResult<Value>> = query_pipe(conn, budget, &name, &pipe).await?;
    for r in replies {
        match r {
            Ok(v) => {
                if let Some(s) = subs.as_mut() {
                    s.extend(parse_numsub(&v));
                }
            }
            Err(e) => match error_for(&name, &e) {
                LiveError::Closed => return Err(LiveError::Closed),
                other => {
                    if subs.is_some() {
                        notes.push(other.to_string());
                    }
                    subs = None;
                }
            },
        }
    }
    Ok(subs)
}

/// One `PUBSUB <sub> c1 … c100` per batch of [`NUMSUB_BATCH`] names.
pub fn numsub_pipe(sub: &str, names: &[Vec<u8>]) -> redis::Pipeline {
    let mut pipe = redis::pipe();
    pipe.ignore_errors();
    for batch in names.chunks(NUMSUB_BATCH) {
        let mut c = redis::cmd("PUBSUB");
        c.arg(sub);
        for n in batch {
            c.arg(n.as_slice());
        }
        pipe.add_command(c);
    }
    pipe
}

/// Why the shard channels are left out on `server`: they need Redis 7.0 or later
/// (every Valkey has them). `None` when they are read.
pub fn shards_off(server: &crate::model::ServerInfo) -> Option<String> {
    use crate::model::ServerFlavor;
    let new_enough = match server.flavor {
        ServerFlavor::Valkey => true,
        ServerFlavor::Redis | ServerFlavor::Unknown => {
            super::bulk::major_minor(&server.version).is_some_and(|v| v >= (7, 0))
        }
    };
    (!new_enough).then(|| {
        format!(
            "Shard channels need Redis 7.0 or later (this is {}), so they are left out.",
            crate::value::escape(&server.version)
        )
    })
}

async fn counts(conn: &mut impl LiveConn, budget: &Budget) -> Result<Counts, LiveError> {
    let mut pipe = redis::pipe();
    pipe.ignore_errors();
    pipe.cmd("PUBSUB").arg("CHANNELS");
    pipe.cmd("PUBSUB").arg("NUMPAT");
    let mut replies: Vec<RedisResult<Value>> =
        query_pipe(conn, budget, "PUBSUB CHANNELS", &pipe).await?;
    if replies.len() != 2 {
        return Err(LiveError::Failed("PUBSUB CHANNELS failed".into()));
    }
    let numpat = replies.pop().expect("two replies");
    let channels = replies.pop().expect("two replies");

    let channels = channels.map_err(|e| error_for("PUBSUB CHANNELS", &e))?;
    let mut out = Counts::default();
    let mut names = parse_channels(&channels);
    out.total_channels = names.len();
    names.truncate(MAX_CHANNELS);

    match numpat {
        Ok(v) => out.patterns = uint(&v),
        Err(e) => match error_for("PUBSUB NUMPAT", &e) {
            LiveError::Closed => return Err(LiveError::Closed),
            other => out.notes.push(other.to_string()),
        },
    }

    let subs = numsub(conn, budget, "NUMSUB", &names, &mut out.notes).await?;
    out.channels = merge(&names, subs.as_deref());
    Ok(out)
}

/// The channel names of a `PUBSUB CHANNELS` reply, as the server sent them.
pub fn parse_channels(v: &Value) -> Vec<Vec<u8>> {
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

/// The `(channel, subscribers)` pairs of a `PUBSUB NUMSUB` reply (a flat array in RESP2,
/// a map in RESP3).
pub fn parse_numsub(v: &Value) -> Vec<(String, u64)> {
    match v {
        Value::Map(m) => m
            .iter()
            .filter_map(|(k, n)| Some((text(k)?, uint(n)?)))
            .collect(),
        Value::Array(items) => items
            .as_chunks::<2>()
            .0
            .iter()
            .filter_map(|[k, n]| Some((text(k)?, uint(n)?)))
            .collect(),
        _ => Vec::new(),
    }
}

/// `names` with their counts from `subs`, most subscribers first, then by name.
/// `subs == None` leaves the counts out.
pub fn merge(names: &[Vec<u8>], subs: Option<&[(String, u64)]>) -> Vec<Channel> {
    let mut out: Vec<Channel> = names
        .iter()
        .map(|n| {
            let name = String::from_utf8_lossy(n).into_owned();
            let subscribers = subs.map(|s| {
                s.iter()
                    .find_map(|(c, k)| (*c == name).then_some(*k))
                    .unwrap_or(0)
            });
            Channel { name, subscribers }
        })
        .collect();
    out.sort_by(|a, b| {
        b.subscribers
            .cmp(&a.subscribers)
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

/// The most messages the live tail keeps, and the most one batch carries (spec "5
/// Pub/Sub").
pub const TAIL_KEPT: usize = 2_000;

/// What the live tail subscribes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subscription {
    /// `SUBSCRIBE` to one channel by name.
    Channel(String),
    /// `PSUBSCRIBE` to a glob-style pattern.
    Pattern(String),
    /// `SSUBSCRIBE` to one shard channel by name, on its slot's owner.
    Shard(String),
}

/// Why a pattern can't be subscribed to as a shard channel.
pub const SHARD_PATTERN: &str =
    "a pattern can't be shard-subscribed: SSUBSCRIBE takes channel names only";

impl Subscription {
    /// What `s` typed means: a pattern when it has `*`, `?` or `[`, otherwise a channel.
    /// `None` when it is empty.
    pub fn parse(text: &str) -> Option<Self> {
        if text.is_empty() {
            None
        } else if text.contains(['*', '?', '[']) {
            Some(Self::Pattern(text.to_string()))
        } else {
            Some(Self::Channel(text.to_string()))
        }
    }

    /// The channel or pattern, as typed.
    pub fn name(&self) -> &str {
        match self {
            Self::Channel(n) | Self::Pattern(n) | Self::Shard(n) => n,
        }
    }

    /// What `S` typed means: a shard channel, or why it can't be one (a pattern).
    pub fn parse_shard(text: &str) -> Result<Option<Self>, &'static str> {
        match Self::parse(text) {
            None => Ok(None),
            Some(Self::Pattern(_)) => Err(SHARD_PATTERN),
            Some(_) => Ok(Some(Self::Shard(text.to_string()))),
        }
    }

    pub fn is_shard(&self) -> bool {
        matches!(self, Self::Shard(_))
    }

    pub fn is_pattern(&self) -> bool {
        matches!(self, Self::Pattern(_))
    }

    /// `SUBSCRIBE` or `PSUBSCRIBE`.
    pub fn command(&self) -> &'static str {
        match self {
            Self::Channel(_) => "SUBSCRIBE",
            Self::Pattern(_) => "PSUBSCRIBE",
            Self::Shard(_) => "SSUBSCRIBE",
        }
    }

    /// Why the server refused it, in redis-lens's own words: an ACL without the command,
    /// or whose channel rules (`resetchannels`) leave this channel or pattern out. Never
    /// the server's text, which names the user.
    pub fn denied(&self) -> String {
        match self {
            Self::Channel(_) => "the server refused SUBSCRIBE on that channel for this user".into(),
            Self::Pattern(_) => {
                "the server refused PSUBSCRIBE on that pattern for this user".into()
            }
            Self::Shard(_) => {
                "the server refused SSUBSCRIBE on that shard channel for this user".into()
            }
        }
    }
}

/// Characters of a message's one-line preview.
pub const PREVIEW_CHARS: usize = 300;
/// The bytes of a payload a [`TailMessage`] keeps once its preview is made: up to 4,000
/// messages are held, and only the preview is drawn.
pub const PAYLOAD_KEPT: usize = 4096;

/// One message the tail received.
///
/// The payload is a value ([`Raw`]: its `Debug` shows only the length). The channel and
/// pattern are names, like the channel names of the counts; they are drawn escaped.
/// [`TailMessage::new`] makes the preview and cuts the payload to [`PAYLOAD_KEPT`] where
/// the message is read (in the tail's own task), so a batch waiting for the screen
/// holds no more than that per message. `Debug` shows neither.
#[derive(Clone, PartialEq, Eq)]
pub struct TailMessage {
    /// When redis-lens received it: UTC, milliseconds since the Unix epoch.
    pub at_unix_ms: i64,
    /// The channel it was published to.
    pub channel: String,
    /// The pattern it matched, for a `PSUBSCRIBE`.
    pub pattern: Option<String>,
    /// The start of the payload, at most [`PAYLOAD_KEPT`] bytes.
    pub payload: Raw,
    /// The payload on one line, made from all of it ([`crate::value::preview`]:
    /// terminal safe).
    preview: String,
}

impl TailMessage {
    pub fn new(
        at_unix_ms: i64,
        channel: String,
        pattern: Option<String>,
        payload: Vec<u8>,
    ) -> Self {
        let preview = crate::value::preview(&payload, PREVIEW_CHARS);
        let mut payload = payload;
        payload.truncate(PAYLOAD_KEPT);
        Self {
            at_unix_ms,
            channel,
            pattern,
            payload: Raw::new(payload),
            preview,
        }
    }

    /// The payload on one line, escaped.
    pub fn preview(&self) -> &str {
        &self.preview
    }
}

impl std::fmt::Debug for TailMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TailMessage")
            .field("at_unix_ms", &self.at_unix_ms)
            .field("channel", &self.channel)
            .field("pattern", &self.pattern)
            .field("payload", &self.payload)
            .finish_non_exhaustive()
    }
}

/// What one push of the subscriber connection means for the tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Push {
    Message(TailMessage),
    /// The connection is gone.
    Lost,
    /// Subscribe confirmations and anything else: nothing to show.
    Other,
}

/// Reads one push: `message` is `[channel, payload]`, `pmessage` is `[pattern, channel,
/// payload]`. A message of any other shape is [`Push::Other`].
pub fn parse_push(push: PushInfo, at_unix_ms: i64) -> Push {
    let bytes = |v: Value| match v {
        Value::BulkString(b) => Some(b),
        Value::SimpleString(s) => Some(s.into_bytes()),
        _ => None,
    };
    let name = |b: Vec<u8>| String::from_utf8_lossy(&b).into_owned();
    let mut data = push.data.into_iter();
    let (pattern, channel, payload) = match (push.kind, data.len()) {
        (PushKind::Disconnection, _) => return Push::Lost,
        (PushKind::Message | PushKind::SMessage, 2) => (None, data.next(), data.next()),
        (PushKind::PMessage, 3) => (data.next(), data.next(), data.next()),
        _ => return Push::Other,
    };
    let (Some(channel), Some(payload)) = (channel.and_then(bytes), payload.and_then(bytes)) else {
        return Push::Other;
    };
    Push::Message(TailMessage::new(
        at_unix_ms,
        name(channel),
        pattern.and_then(bytes).map(name),
        payload,
    ))
}

/// Sends `SUBSCRIBE` or `PSUBSCRIBE` for `to` on `conn`, a connection from
/// `conn::open_subscriber`, taking one command from `budget`. The messages then arrive
/// on that connection's push receiver. The error is in redis-lens's own words.
pub async fn subscribe(
    conn: &mut redis::aio::MultiplexedConnection,
    budget: &Budget,
    to: &Subscription,
) -> Result<(), String> {
    budget.take(1).await;
    let sent = match to {
        Subscription::Channel(c) => conn.subscribe(c.as_str()).await,
        Subscription::Pattern(p) => conn.psubscribe(p.as_str()).await,
        // redis-rs has no `ssubscribe` on this connection; over RESP3 the server's
        // confirmation is a push that answers the command (one per channel).
        Subscription::Shard(c) => redis::cmd("SSUBSCRIBE")
            .arg(c.as_str())
            .query_async::<Value>(conn)
            .await
            .map(|_| ()),
    };
    sent.map_err(|e| match error_for(to.command(), &e) {
        LiveError::Denied(_) => to.denied(),
        LiveError::Closed => CONNECTION_LOST.to_string(),
        other => other.to_string(),
    })
}

/// The server refused RESP3 (`HELLO 3`), which the tail's connection needs.
pub const RESP3_REFUSED: &str =
    "the server refused RESP3 (HELLO 3), which the live tail needs to receive messages";
/// The server refused `HELLO` for this user while the tail's connection was set up.
pub const HELLO_DENIED: &str = "the server refused HELLO for this user";

/// A fixed text for opening the tail's connection failing in a way particular to it
/// (RESP3 refused, `HELLO` refused). `None` for every other failure, which is reported
/// like any connect error.
pub fn open_error(e: &ConnError) -> Option<&'static str> {
    let ConnError::Redis(e) = e else {
        return None;
    };
    if e.kind() == redis::ErrorKind::RESP3NotSupported || e.code() == Some("NOPROTO") {
        Some(RESP3_REFUSED)
    } else if e.kind() == redis::ErrorKind::Server(redis::ServerErrorKind::NoPerm) {
        Some(HELLO_DENIED)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn push(kind: PushKind, data: Vec<Value>) -> PushInfo {
        PushInfo { kind, data }
    }

    #[test]
    fn a_pattern_has_a_glob_character() {
        assert_eq!(Subscription::parse(""), None);
        assert_eq!(
            Subscription::parse("orders:eu"),
            Some(Subscription::Channel("orders:eu".into()))
        );
        for p in ["orders:*", "news?", "a[bc]"] {
            assert_eq!(
                Subscription::parse(p),
                Some(Subscription::Pattern(p.into()))
            );
        }
        assert_eq!(Subscription::Pattern("x*".into()).command(), "PSUBSCRIBE");
        assert_eq!(Subscription::Channel("x".into()).command(), "SUBSCRIBE");
    }

    #[test]
    fn pushes_become_messages() {
        let m = parse_push(
            push(
                PushKind::Message,
                vec![b("news"), Value::BulkString(vec![0, 0xff])],
            ),
            7,
        );
        assert_eq!(
            m,
            Push::Message(TailMessage::new(7, "news".into(), None, vec![0, 0xff]))
        );
        let p = parse_push(
            push(PushKind::PMessage, vec![b("n*"), b("news"), b("hi")]),
            8,
        );
        let Push::Message(p) = p else { panic!("{p:?}") };
        assert_eq!(
            (p.pattern.as_deref(), p.channel.as_str()),
            (Some("n*"), "news")
        );
        assert_eq!(p.payload.as_bytes(), b"hi");
        assert_eq!(
            parse_push(push(PushKind::Disconnection, vec![]), 0),
            Push::Lost
        );
        assert_eq!(
            parse_push(push(PushKind::Subscribe, vec![b("news"), Value::Int(1)]), 0),
            Push::Other
        );
        // The wrong shape is not a message.
        assert_eq!(
            parse_push(push(PushKind::Message, vec![b("news")]), 0),
            Push::Other
        );
        assert_eq!(
            parse_push(push(PushKind::Message, vec![b("a"), Value::Nil]), 0),
            Push::Other
        );
    }

    #[test]
    fn a_shard_message_is_a_message_of_its_channel() {
        let m = parse_push(push(PushKind::SMessage, vec![b("orders"), b("hi")]), 9);
        assert_eq!(
            m,
            Push::Message(TailMessage::new(9, "orders".into(), None, b"hi".to_vec()))
        );
    }

    #[test]
    fn a_shard_subscription_takes_channel_names_only() {
        assert_eq!(Subscription::parse_shard(""), Ok(None));
        assert_eq!(
            Subscription::parse_shard("orders:eu"),
            Ok(Some(Subscription::Shard("orders:eu".into())))
        );
        for p in ["orders:*", "news?", "[ab]"] {
            assert_eq!(Subscription::parse_shard(p), Err(SHARD_PATTERN), "{p}");
        }
        let s = Subscription::Shard("orders".into());
        assert!(s.is_shard());
        assert!(!Subscription::parse("orders").unwrap().is_shard());
    }

    #[test]
    fn shard_numsub_is_asked_in_batches_of_100() {
        let names: Vec<Vec<u8>> = (0..250).map(|i| format!("ch:{i}").into_bytes()).collect();
        let pipe = numsub_pipe("SHARDNUMSUB", &names);
        let cmds: Vec<Vec<Vec<u8>>> = pipe
            .cmd_iter()
            .map(|c| {
                c.args_iter()
                    .map(|a| match a {
                        redis::Arg::Simple(b) => b.to_vec(),
                        _ => Vec::new(),
                    })
                    .collect()
            })
            .collect();
        assert_eq!(
            cmds.iter().map(Vec::len).collect::<Vec<_>>(),
            [102, 102, 52]
        );
        for c in &cmds {
            assert_eq!(
                (&c[0][..], &c[1][..]),
                (&b"PUBSUB"[..], &b"SHARDNUMSUB"[..])
            );
        }
        assert_eq!(cmds[2][51], b"ch:249");
        assert_eq!(numsub_pipe("NUMSUB", &[]).cmd_iter().count(), 0);
    }

    #[test]
    fn shard_channels_are_left_out_before_redis_7_only() {
        use crate::model::ServerFlavor;
        let mut s = crate::model::ServerInfo {
            flavor: ServerFlavor::Redis,
            version: String::new(),
            mode: crate::model::ServerMode::Standalone,
            used_memory: 0,
            maxmemory: 0,
            maxmemory_policy: String::new(),
            total_keys: 0,
        };
        for (flavor, version, off) in [
            (ServerFlavor::Redis, "6.2.14", true),
            (ServerFlavor::Redis, "7.0.0", false),
            (ServerFlavor::Redis, "7.4.2", false),
            (ServerFlavor::Valkey, "7.2.5", false),
            (ServerFlavor::Valkey, "8.1.0", false),
        ] {
            s.flavor = flavor;
            s.version = version.into();
            assert_eq!(shards_off(&s).is_some(), off, "{version}");
        }
        s.flavor = ServerFlavor::Redis;
        s.version = "6.2.14".into();
        assert_eq!(
            shards_off(&s).unwrap(),
            "Shard channels need Redis 7.0 or later (this is 6.2.14), so they are left out."
        );
    }

    #[test]
    fn a_message_debug_shows_no_payload() {
        let Push::Message(m) = parse_push(
            push(PushKind::Message, vec![b("news"), b("s3cr3t-p4yload")]),
            1,
        ) else {
            panic!()
        };
        assert!(!crate::value::leaks(&format!("{m:?}"), "s3cr3t-p4yload"));
    }

    #[test]
    fn open_errors_in_fixed_words() {
        let resp3 = redis::RedisError::from((
            redis::ErrorKind::RESP3NotSupported,
            "Redis Server doesn't support HELLO command therefore resp3 cannot be used",
        ));
        assert_eq!(open_error(&ConnError::Redis(resp3)), Some(RESP3_REFUSED));
        let noperm = redis::RedisError::from((
            redis::ErrorKind::Server(redis::ServerErrorKind::NoPerm),
            "NOPERM",
            "User zed-ro has no permissions to run the 'hello' command".to_string(),
        ));
        assert_eq!(open_error(&ConnError::Redis(noperm)), Some(HELLO_DENIED));
        let io = redis::RedisError::from(std::io::Error::other("refused"));
        assert_eq!(open_error(&ConnError::Redis(io)), None);
        assert_eq!(
            Subscription::Channel("x".into()).denied(),
            "the server refused SUBSCRIBE on that channel for this user"
        );
    }

    proptest! {
        #[test]
        fn pushes_never_panic(kind in 0u8..4, items in proptest::collection::vec(
            prop_oneof![
                Just(Value::Nil),
                any::<i64>().prop_map(Value::Int),
                proptest::collection::vec(any::<u8>(), 0..8).prop_map(Value::BulkString),
            ],
            0..5,
        )) {
            let kind = match kind {
                0 => PushKind::Message,
                1 => PushKind::PMessage,
                2 => PushKind::Subscribe,
                _ => PushKind::Disconnection,
            };
            let _ = parse_push(PushInfo { kind, data: items }, 0);
        }
    }

    fn b(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    #[test]
    fn channels_keep_their_bytes() {
        let v = Value::Array(vec![
            b("news"),
            Value::BulkString(vec![0xff, b'x']),
            Value::Nil,
        ]);
        assert_eq!(parse_channels(&v), vec![b"news".to_vec(), vec![0xff, b'x']]);
        assert!(parse_channels(&Value::Nil).is_empty());
    }

    #[test]
    fn numsub_reads_flat_arrays_and_maps() {
        let flat = Value::Array(vec![b("a"), Value::Int(2), b("b"), Value::Int(0)]);
        assert_eq!(parse_numsub(&flat), vec![("a".into(), 2), ("b".into(), 0)]);
        let map = Value::Map(vec![(b("a"), Value::Int(2))]);
        assert_eq!(parse_numsub(&map), vec![("a".into(), 2)]);
        // A trailing name without a count is ignored.
        let odd = Value::Array(vec![b("a"), Value::Int(2), b("b")]);
        assert_eq!(parse_numsub(&odd), vec![("a".into(), 2)]);
    }

    #[test]
    fn merge_ranks_by_subscribers_then_name() {
        let names = vec![b"b".to_vec(), b"a".to_vec(), b"c".to_vec()];
        let subs = vec![("a".into(), 1), ("b".into(), 3), ("c".into(), 1)];
        let m = merge(&names, Some(&subs));
        let order: Vec<_> = m.iter().map(|c| (c.name.as_str(), c.subscribers)).collect();
        assert_eq!(order, [("b", Some(3)), ("a", Some(1)), ("c", Some(1))]);
        // Counts refused: names only, in name order.
        let m = merge(&names, None);
        let order: Vec<_> = m.iter().map(|c| (c.name.as_str(), c.subscribers)).collect();
        assert_eq!(order, [("a", None), ("b", None), ("c", None)]);
        // A channel missing from the counts has none.
        assert_eq!(merge(&names[..1], Some(&[]))[0].subscribers, Some(0));
    }

    proptest! {
        #[test]
        fn parsers_never_panic(items in proptest::collection::vec(
            prop_oneof![
                Just(Value::Nil),
                any::<i64>().prop_map(Value::Int),
                proptest::collection::vec(any::<u8>(), 0..8).prop_map(Value::BulkString),
            ],
            0..12,
        )) {
            let v = Value::Array(items);
            let names = parse_channels(&v);
            let subs = parse_numsub(&v);
            let merged = merge(&names, Some(&subs));
            prop_assert_eq!(merged.len(), names.len());
        }

        #[test]
        fn merge_is_sorted(counts in proptest::collection::vec(0u64..5, 0..20)) {
            let names: Vec<Vec<u8>> =
                (0..counts.len()).map(|i| format!("c{i:02}").into_bytes()).collect();
            let subs: Vec<(String, u64)> =
                names.iter().zip(&counts).map(|(n, k)| (String::from_utf8(n.clone()).unwrap(), *k)).collect();
            let m = merge(&names, Some(&subs));
            for w in m.windows(2) {
                prop_assert!(w[0].subscribers >= w[1].subscribers);
            }
        }
    }
}
