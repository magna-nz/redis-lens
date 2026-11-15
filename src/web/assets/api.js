// redis-lens dashboard: the data layer.
//
// The views never call `fetch`. They call the one `api` object exported at the bottom and
// subscribe to its events. `api` talks to the redis-lens process that served this page
// (the routes and replies are in docs/dashboard-api.md). With `?mock` in the address it
// talks to mock.js instead, which answers the same routes with the same JSON, so the
// adapters below are the one path for both. mock.js is loaded only then, and only debug
// builds serve it: a release build says so and shows nothing made up.
//
// SIGN-IN: redis-lens opens `/#token=<64 hex>`. `initApi` takes the token out of the
// address at once (history.replaceState), keeps it in memory and in sessionStorage (this
// tab only, so a reload stays signed in), and every /api/* request sends it as
// `Authorization: Bearer <token>`, the event stream included: that stream is read with
// fetch and parsed here (`createSseParser`), because EventSource cannot send a header. A
// 401 forgets the token and locks the page. The token is never logged, put in a URL the
// page builds, put in an error text or in localStorage.
//
// =======================================================================================
// VIEW-MODEL SHAPES (what the views consume; `null` always means "unknown", drawn as "—")
// =======================================================================================
//
// State         { status: 'idle'|'connecting'|'connected'|'failed', connected: bool,
//                 connecting: { name, text } | null, failed: string | null (why, as sent),
//                 conn: Conn | null, liveState: 'waiting'|'ready'|'closed',
//                 lost: string | null (the live connection closed), tail: { id, kind, name,
//                 node, ready } | null, monitor: { run, node, startedAt, maxLines, maxSecs }
//                 | null, features: { entraId: bool, version }, polling: Polling,
//                 live: bool (the live figures are polled: not paused),
//                 monitorLimits: { maxLines, maxSecs } }
// Polling       { paused: bool, intervalSecs, intervals: [number] }   (shared by every tab)
// Conn          Profile plus { docker: bool, readOnly: bool, writesOff: string|null,
//                 server: 'Redis'|'Valkey'|null, version, topology: 'standalone'|'cluster'|
//                 'sentinel'|null, policy, target, via: string|null (Sentinel answer),
//                 refused: [string] (writes the server refused this session),
//                 nodes: [Node], pickedNode: id|null, connectedAt (unix seconds) }
// Node          { id (the index as text), index, address, role, slots: '0-5460', connected }
// Profile       { name, env: 'PROD'|'STAGE'|'LOCAL'|'OTHER', address (never a credential),
//                 url (masked), tls, db, auth, signIn: string (where the credentials live),
//                 tags: [string], readOnly: bool, provider, providerNotice, ssh (bastion
//                 host, never the user), sentinel, sentinelTls, tokenCommand: bool,
//                 standalone, preferReplica, docker: bool, container, image }
// Connections   { profiles: [Profile], connected: name|null }
// Docker        { containers: [Profile] }   (docker: true; name is the container's name)
// TestResult    { ok: bool, summary, checks: [Check], notes: [string] }
// Check         { name, status: 'allowed'|'denied'|'skipped', ok: bool, why: string|null }
// Overview      { status, server: { kind, version, topology, policy, lfu }, memory: { used,
//                 max }, keys: { total, ttlShare, withTtl, dataBytes, overheadBytes,
//                 bigKeys }, figuresFailed: string|null, gaps: [string], checks: [Check],
//                 provider: { label, notice }|null, lastAnalysis: { at (ms), sampledKeys }
//                 |null, target, figures: Figures|null, shards: [Shard],
//                 findings: [{ text, severity: 'warn'|'note'|'info', screen, label
//                 ('Server › Slow log'), where: 'slowlog'|'clients'|null (the page's
//                 own panel for it, when this phase has one) }] }
// Shard         { id, index, slots, address, role, opsPerSec, usedMemory, keys,
//                 status: 'ok'|'not-connected'|'no-answer', note }
// Figures       { at (ms), node: id|null, error: string|null, opsPerSec, opsNow, clients,
//                 blocked, usedMemory, maxMemory, hitRate (0..1), evictedPerSec,
//                 expiredPerSec, netIn, netOut, cpuUser, cpuSys, fragmentation, uptimeSecs,
//                 role, replicas, linkUp, policy, rdbOk, aofOn, aofOk, keys,
//                 shards: [{ id, failed, opsPerSec, usedMemory, keys, role }] | null }
// Clients       { off: string|null, node, total, connections: [{ id, addr, name, lib,
//                 ageSecs, idleSecs, cmd, db, memory, own: bool }], groups: [{ by:
//                 'library'|'address', name, connections, oldestSecs, idleSecs, idleConns,
//                 lastCommand, memory, flagged: bool, members: [index into connections] }] }
// Slowlog       { off: string|null, node, settings: { thresholdMicros, maxLen }|null,
//                 settingsOff: string|null, keysOff: string|null, entries: [{ id, at (ms),
//                 micros, name, command (one masked line), keys: [string], moreKeys,
//                 client, clientName, argsCut, note }] }
// MonitorRun    { run, maxLines, maxSecs }                      (the reply to Start)
// MonitorEvent  { run, kind: 'ready'|'lines'|'closed', startedAt, maxLines, maxSecs,
//                 lines: [{ at (ms), db, client, command, cut }], end, error,
//                 stopped: string|null (a sentence, for kind closed) }
// Channels      { off: string|null, node, total, patterns, channels: [{ name, subscribers,
//                 shard: bool, node }], shardOff: string|null, notes: [string] }
// Subscription  { id, name, kind: 'channel'|'pattern'|'shard', node }
// TailEvent     { id, kind: 'ready'|'messages'|'closed', messages: [{ at (ms), channel,
//                 body, bytes, pattern }], skipped: number, error: string|null }
// Published     { receivers: number|null }
// Reply         { input (masked line), node: string|null, routeNote: string|null,
//                 kind: 'reply'|'refused'|'error', text: string|null, times, sent,
//                 dropped: number, droppedNote: string|null (the 4 MiB output cap),
//                 replies: [{ views: { text, table, json, raw } (each a string or null),
//                 micros, error: bool, cut: bool }] }
// Names         { off: string|null, names: [string] }   ('config', 'config|get', ...)
// Help          { name, summary, since, complexity, arguments, subcommands: [Help] } | null
// Confirm       { id, kind: 'ask'|'type', text, expect, title: string|null }
// Notice        { text, kind: 'info' }
//
// Browse, Analyze and snapshots (phase 2):
// Name          { name (escaped, to show), id (hex of the bytes: what names it in a request),
//                 copy: string|null (the name as it is, when escaping changed nothing) }
// Analysis      { status: 'none'|'connecting'|'sampling'|'stopping'|'done', running: bool, run,
//                 depth: 'quick'|'deep'|null, sampled, target, startedAt (unix), error, note,
//                 previous: bool, db, report: Report|null }          (State.analysis)
// Report        { takenAt (unix), ageSecs, sampledKeys, patterns, complete, stopped, sizes,
//                 cluster, previous }
// Progress      { run, kind: 'started'|'sampling'|'stopping'|'done'|'stopped'|'failed'|
//                 'compared', depth, sampleLimit, sampled, target, patterns, stopping,
//                 sampledKeys, complete, stopped, takenAt, kept, text, error }
// Dbs           { off, current, rows: [{ db, keys, expires, current }], unread, savedAs, note }
// Browse        { source: 'names'|'partial'|'analysis'|'none', why, sort, sorts, sizes, node,
//                 nodes: [{ index, endpoint, role, status, sampledKeys }], previous, partial:
//                 { sampled, target }|null, names: Names|null, total, ttlLabels, rows: [Row] }
// Names         { pages, firstPages, namesRead, dbsize, complete, done, reading, error,
//                 refused, skipped, missing: [endpoint], nodesDone: [read, of]|null }
// Row           PatternRow | { kind: 'group', label, key, type, estKeys, estBytes, sampledKeys,
//                 share, noTtl, noTtlRatio, verdict, marked, reason, children: [PatternRow] }
// PatternRow    { kind: 'pattern', pattern: Name, type, estKeys, estBytes, avgBytes,
//                 sampledKeys, sampledBytes, share, noTtl, noTtlRatio, ttlHist: [n]|null,
//                 ttlVerdict, verdict, reason, warning, p50, p99, max, examples: [KeyRow],
//                 command, marked, change: Change|null, namesSeen, literal }
// Change        { deltaBytes, deltaKeys, cause, words, alarming }
// KeyRow        Name plus { type, ttl ('never', '59m 59s'), ttlSecs, bytes, encoding, length,
//                 lengthUnit, freq, node }
// KeyList       { list, kind: 'pattern'|'find', pattern: Name|null, find: { text, glob }|null,
//                 glob, rows: [KeyRow] (from newFrom on), loaded, newFrom, pages, pageKeys,
//                 done, capped, maxRows, error,
//                 refused: [string], otherPattern, unreadable, sizes, lfu, freq, cluster:
//                 { walk, at, nodesDone, missing: [endpoint], narrowed }|null }
// Value         { view, key: Name|null, type, kind, command, meta: KeyRow|null, pattern,
//                 loading, pages, loaded, length, done, more, capped, gone, error, notice,
//                 refused, fieldTtls, fieldTtlsOff, items: [{ index, name, value, ttl, at,
//                 fields: [{ name, value }]|null }], value: Shown|null, cut, message,
//                 module: Module|null, cluster, maxItems }
// Shown         { format, detected, picked, formats, bytes, compression, truncated, note,
//                 header, lines: [string], copy: string|null, copyRefused }
// Item          { index, field, name, shown: Shown }
// Module        { label, info, top, stats: [{ name, value }], samples: [{ at (ms), value }],
//                 missing: [string], unavailable }
// AnalysisInfo  { analysis: Analysis, estimates: [Estimate], running }
// Estimate      { depth, sampleLimit, keys, commands, perSec, durationSecs, about, shards,
//                 sizes, freq }
// BigKeys       { off, report, empty, rows: [{ key: KeyRow, pattern: Name, length, lengthUnit,
//                 encoding, big, endpoint }], recommendations: [{ severity, text, hint }],
//                 factsRead, notes, bigKeyBytes }
// Expiry        { off, report, empty, rows: [{ pattern, estKeys, estBytes, sampledKeys, noTtl,
//                 noTtlRatio, ttlHist, status, warning, explain: [string], command }],
//                 ttlLabels, freed: { within: [{ label, bytes }], later, never, total,
//                 neverShare }|null, freedOff }
// Diff          { off, report, status: 'pending'|'none'|'failed'|'diff', text, before, after:
//                 { at, utc, sampledKeys, usedMemory }, rows: [{ pattern, keysBefore,
//                 keysAfter, bytesBefore, bytesAfter, change, partial, trend: { values,
//                 perDay, ratioPerDay }|null }], empty, note, trendPoints }
// HotKeys       { off, report, empty, rows: [{ key: KeyRow, pattern, endpoint, hotAndBig }],
//                 hotBigBytes, hotFreq }
// Nodes         { off, report, empty, rows: [{ index, id, primaryId, endpoint, role, version,
//                 status, complete, keys, sampledKeys, patterns, memory, slots (text),
//                 slotCount, sizes, note }] }
// Slots         { off, ranges: [{ node, endpoint, start, end, status, error, keys, cpuUsec,
//                 netIn, netOut, memory }], hottest: [{ slot, node, endpoint, keys, cpuUsec,
//                 netIn, netOut, memory, sampled: [{ key: Name, pattern: Name, kinds }] }],
//                 order, memoryColumn, cpuAndNetworkZero, note, empty }
// Snapshots     { profile, snapshots: [{ file, takenAt, utc, current }], canSave, saveOff,
//                 saved }
// Saved         { file, takenAt, utc, sampledKeys, text }
//
// =======================================================================================
// METHODS (all async; each can reject with ApiError, whose `message` is the server's own
// fixed wording, or a fixed line of the page's when the server sent none)
// =======================================================================================
//
//   getState()                        → State         GET    /api/state
//   listConnections()                 → Connections   GET    /api/connections
//   listDocker()                      → Docker        GET    /api/docker
//   addConnection(form)               → { name, message, profile }   POST /api/connections
//   updateConnection(name, changes)   → { profile, message }   PATCH /api/connections/<name>
//   removeConnection(name)            → { message }   DELETE /api/connections/<name>
//   testConnection(name)              → TestResult    POST   /api/connections/<name>/test
//   connect({ name } | { docker })    → State         POST   /api/connect      (202)
//   disconnect()                      → State         POST   /api/disconnect
//   pickNode(index)                   → State         POST   /api/node
//   setPolling({ paused, intervalSecs }) → Polling    POST   /api/polling
//   setSessionWrites(on)              → { profile, message }   POST /api/session/writes
//   getOverview()                     → Overview      GET    /api/overview
//   getClients()                      → Clients       GET    /api/server/clients
//   getSlowlog()                      → Slowlog       GET    /api/server/slowlog
//   startMonitor()                    → MonitorRun    POST   /api/server/monitor (asks, then 202)
//   stopMonitor()                     → { stopped }   DELETE /api/server/monitor
//   getChannels()                     → Channels      GET    /api/pubsub/channels
//   subscribe({ name, kind })         → Subscription  POST   /api/pubsub/subscribe (202)
//   unsubscribe()                     → { stopped }   DELETE /api/pubsub/subscribe
//   publish({ channel, message, shard }) → Published  POST   /api/pubsub/publish
//   runCommand(line, { pipeline })    → Reply         POST   /api/cmdline
//   getCommandNames()                 → Names         GET    /api/cmdline/commands
//   getCommandHelp(name)              → Help | null   GET    /api/cmdline/help?name=
//   getDbs()                          → Dbs           GET    /api/dbs
//   switchDb(db)                      → { switched, db, run, text }   POST /api/db (202)
//   getBrowse({ sort, node })         → Browse        GET    /api/browse
//   moreNames({ sort })               → Browse        POST   /api/browse/more
//   getKeys({ pattern (id), node })   → KeyList       GET    /api/keys
//   findKeys(text)                    → KeyList       GET    /api/find?q=
//   moreKeys(list)                    → KeyList       GET    /api/keys/more?list=
//   openValue({ key (id), type })     → Value         GET    /api/value
//   moreValue(view)                   → Value         GET    /api/value/more?view=
//   getItem({ view, index, field, format }) → Item    GET    /api/value/item
//   getAnalysis()                     → AnalysisInfo  GET    /api/analysis
//   startAnalysis(depth)              → { run, depth, sampleLimit, estimate }   POST (202)
//   stopAnalysis()                    → { stopping, text }   DELETE /api/analysis
//   getBigKeys() getExpiry() getChanges() getHotKeys() getNodes() getSlots({ order, refresh })
//                                     → the panel     GET    /api/analysis/<panel>
//   saveSnapshot()                    → Saved         POST   /api/snapshots (201)
//   listSnapshots()                   → Snapshots     GET    /api/snapshots
//   compareSnapshots(a, b)            → Diff          GET    /api/snapshots/compare
//
// A write the gate wants confirmed comes back as HTTP 409 `{ "confirm": … }`. The wrapper
// here opens the confirm dialog (through the handler app.js registers), answers with
// `POST /api/confirm/<id>` and returns that reply as the method's result; Cancel sends
// `DELETE /api/confirm/<id>` and rejects with `ApiError.cancelled`.
// A panel whose feature is off answers 200 `{ "off": why }`: its view-model carries `off`.
//
// EVENTS: `api.on(name, fn)` returns a function that unsubscribes.
//   state (State) · figures (Figures) · overview (Overview without `figures` and
//   `shards`, which come with `figures`) · tail (TailEvent) · monitor (MonitorEvent) ·
//   progress (Progress, an analysis) · notice (Notice) · link ({ up: bool }, the page's own line to redis-lens) ·
//   locked ({}): the session is missing or wrong (HTTP 401) · live ({ live: bool }):
//   raised by app.js when the state's `live` changes, for the views that draw figures.

// ---------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------

/** A failure with wording that is safe to show: redis-lens's own, never Redis's. */
export class ApiError extends Error {
  constructor(message, { status = 0, cancelled = false, locked = false, network = false, notLive = false } = {}) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.cancelled = cancelled;
    this.locked = locked;
    this.network = network;
    this.notLive = notLive;
  }
}

/** A write that needs an answer first. `adapt` turns the confirmed reply into the
 *  view-model the method returns. */
export class ConfirmRequired extends Error {
  constructor(confirm, adapt = (x) => x) {
    super('confirmation needed');
    this.name = 'ConfirmRequired';
    this.confirm = confirm;
    this.adapt = adapt;
  }
}

export const TEXT = {
  cancelled: 'Cancelled: nothing was sent.',
  network: 'Lost contact with redis-lens. It may have been closed in the terminal.',
  locked: 'This page is locked. Open the dashboard from redis-lens: press ctrl-o in the terminal, or run redis-lens --dashboard.',
  forbidden: 'redis-lens refused the request. Open the dashboard again from the terminal.',
  failed: 'redis-lens could not do that.',
  tooMany: 'Too many questions in a row, so nothing was sent.',
  liveClosed: 'The live connection is closed.',
  connectFailed: 'The connect failed.',
  noMock: 'The sample data (?mock) comes only with development builds of redis-lens, so there is none here. Open the dashboard without ?mock to see your servers.',
};

/** Where the credentials of a saved connection live, by the server's `auth` word. */
export const SIGN_IN = {
  none: 'no password',
  keychain: 'password or ACL user, kept in the OS keychain',
  command: 'a token from a command; the username is kept in the OS keychain',
  'entra-id': 'Entra ID, from your developer tools (az login)',
};

/** What Docker discovery found out about a container's sign-in. */
export const DOCKER_SIGN_IN = {
  password: 'password for the default user, from the container, kept in memory only',
  none: 'no password',
  required: 'a password is required and was not found: save it as a connection with credentials',
  unknown: 'unknown: the container did not answer',
};

/** Why a Monitor run ended, by the server's `end` word. */
export const MONITOR_END = {
  stopped: 'MONITOR stopped.',
  line_cap: 'MONITOR stopped by itself: it reached its line limit.',
  time_up: 'MONITOR stopped by itself: its time was up.',
  lost: 'MONITOR stopped: the connection to the server was lost.',
  failed: 'MONITOR stopped.',
};

// ---------------------------------------------------------------------------------------
// Adapters: the server's JSON → the view-models above. Pure functions, tested in
// tests/api.test.js against replies captured from the real server. Unknown stays unknown
// (null): nothing is invented here.
// ---------------------------------------------------------------------------------------

const str = (v) => (typeof v === 'string' && v !== '' ? v : null);
const num = (v) => (typeof v === 'number' && Number.isFinite(v) ? v : null);
const arr = (v) => (Array.isArray(v) ? v : []);
const obj = (v) => (v && typeof v === 'object' && !Array.isArray(v) ? v : {});
const strings = (v) => arr(v).filter((s) => typeof s === 'string' && s !== '');
const oneOf = (v, allowed, fallback) => (allowed.includes(v) ? v : fallback);
const ENVS = ['PROD', 'STAGE', 'LOCAL', 'OTHER'];
const env = (v) => {
  const e = String(v ?? '').toUpperCase();
  return ENVS.includes(e) ? e : 'OTHER';
};
const flavor = (v) => (v === 'redis' ? 'Redis' : v === 'valkey' ? 'Valkey' : null);
const joined = (v) => {
  if (typeof v === 'string') return v;
  if (Array.isArray(v) && v.every((line) => typeof line === 'string')) return v.join('\n');
  return null;
};

/** Slot ranges as text: `0-5460`, several comma-joined, after three `+N more`. */
export function fmtSlots(ranges) {
  const parts = arr(ranges)
    .filter((r) => Array.isArray(r) && num(r[0]) !== null && num(r[1]) !== null)
    .map(([a, b]) => (a === b ? String(a) : `${a}-${b}`));
  if (!parts.length) return null;
  if (parts.length <= 3) return parts.join(', ');
  return `${parts.slice(0, 3).join(', ')} +${parts.length - 3} more`;
}

/** One line of the permission check: `{ label, status, reason }`. */
export function adaptCheck(j) {
  const o = obj(j);
  const status = oneOf(o.status, ['allowed', 'denied', 'skipped'], 'skipped');
  return { name: str(o.label) ?? 'check', status, ok: status === 'allowed', why: str(o.reason) };
}

/** A cluster node of `state.nodes` (and of each shard row). */
export function adaptNode(j) {
  const o = obj(j);
  const index = num(o.index) ?? 0;
  return { id: String(index), index, address: str(o.endpoint) ?? '', role: str(o.role), slots: fmtSlots(o.slots), connected: o.connected !== false };
}

/** A saved connection (`Connection` in the API). It has no credential field to read:
 *  `auth` is only a word for where they live. */
export function adaptProfile(j) {
  const o = obj(j);
  const auth = oneOf(o.auth, Object.keys(SIGN_IN), 'none');
  return {
    name: str(o.name) ?? '',
    env: env(o.env_label ?? o.env),
    address: str(o.endpoint),
    url: str(o.url),
    tls: o.tls === true,
    db: num(o.db),
    auth,
    signIn: SIGN_IN[auth],
    tags: strings(o.tags),
    readOnly: o.writes === true ? false : o.read_only !== false,
    provider: str(o.provider),
    providerNotice: str(o.provider_notice),
    ssh: str(o.ssh),
    sentinel: str(o.sentinel),
    sentinelTls: o.sentinel_tls === true,
    tokenCommand: o.token_command === true,
    standalone: o.standalone === true,
    preferReplica: o.prefer_replica === true,
    docker: false,
    container: null,
    image: null,
  };
}

/** A container Docker discovery found (`instances[]` of GET /api/docker). */
export function adaptContainer(j) {
  const o = obj(j);
  const auth = oneOf(o.auth, Object.keys(DOCKER_SIGN_IN), 'unknown');
  const url = str(o.url);
  return {
    ...adaptProfile({ name: o.container, env: 'local', endpoint: o.endpoint, url, tls: /^rediss:/i.test(url || ''), writes: true }),
    auth,
    signIn: DOCKER_SIGN_IN[auth],
    docker: true,
    container: str(o.container),
    image: str(o.image),
  };
}

/** GET /api/state, the `state` event, and the reply of connect / disconnect / node. */
export function adaptState(j) {
  const o = obj(j);
  const status = oneOf(o.status, ['idle', 'connecting', 'connected', 'failed'], 'idle');
  const liveState = oneOf(o.live, ['waiting', 'ready', 'closed'], 'waiting');
  const server = obj(o.server);
  const answer = o.sentinel ? obj(o.sentinel) : null;
  const nodes = arr(o.nodes).map(adaptNode);
  const monitor = o.monitor ? obj(o.monitor) : null;
  const tail = o.tail ? obj(o.tail) : null;
  const liveError = str(o.live_error);
  const limits = obj(o.monitor_limits);
  const conn = o.connection
    ? {
      ...adaptProfile(o.connection),
      docker: o.docker === true,
      readOnly: o.writes !== true,
      writesOff: str(o.writes_off),
      server: flavor(server.flavor),
      version: str(server.version),
      topology: str(server.mode),
      policy: str(server.policy),
      target: str(o.target),
      via: answer ? `master ${str(answer.master) ?? '?'} at ${str(answer.primary) ?? '?'}, asked of Sentinel ${str(answer.sentinel) ?? '?'}` : null,
      refused: strings(o.refused),
      nodes,
      pickedNode: nodes.length ? String(num(o.picked) ?? 0) : null,
      connectedAt: num(o.connected_at_unix),
    }
    : null;
  const polling = adaptPolling(o.polling);
  return {
    status,
    connected: status === 'connected' && !!conn,
    connecting: status === 'connecting' && conn
      ? { name: conn.name, text: `Connecting to ${conn.name} (${conn.address ?? '?'}) and checking permissions…` }
      : null,
    failed: status === 'failed' ? str(o.error) ?? TEXT.connectFailed : null,
    conn,
    liveState,
    lost: status === 'connected' && liveState === 'closed' ? (liveError ? `${TEXT.liveClosed.slice(0, -1)}: ${liveError}` : TEXT.liveClosed) : null,
    tail: tail ? { id: num(tail.id), kind: oneOf(tail.kind, ['channel', 'pattern', 'shard'], 'channel'), name: str(tail.name) ?? '', node: num(tail.node), ready: tail.ready === true } : null,
    monitor: monitor ? { run: num(monitor.run), node: num(monitor.node), startedAt: num(monitor.started_at_unix_ms), maxLines: num(monitor.max_lines), maxSecs: num(monitor.max_secs) } : null,
    features: { entraId: obj(o.features).entra_id === true, version: str(obj(o.features).version) },
    polling,
    live: !polling.paused,
    monitorLimits: { maxLines: num(limits.max_lines) ?? 10000, maxSecs: num(limits.max_secs) ?? 30 },
    analysis: adaptAnalysis(o.analysis),
  };
}

/** `state.polling` and the reply of POST /api/polling: how the live figures are polled. */
export function adaptPolling(j) {
  const o = obj(j);
  const intervals = arr(o.intervals).filter((n) => num(n) !== null);
  return { paused: o.paused === true, intervalSecs: num(o.interval_secs) ?? 2, intervals: intervals.length ? intervals : [1, 2, 3, 5, 10, 15, 30, 60] };
}

/** The body of POST /api/polling: `{ paused, interval_secs }`, either or both. */
export function pollingRequest(change) {
  const body = {};
  if (change.paused !== undefined) body.paused = change.paused === true;
  if (change.intervalSecs !== undefined) body.interval_secs = Number(change.intervalSecs);
  return body;
}

/** POST /api/session/writes: `{ connection, message }` for a saved connection,
 *  `{ message, state }` for a Docker container (its writes change for the session only). */
export function adaptSessionWrites(j) {
  const o = obj(j);
  const connection = o.connection || obj(o.state).connection;
  return { profile: connection ? adaptProfile(connection) : null, message: str(o.message) };
}

/** GET /api/connections. */
export function adaptConnections(j) {
  const o = obj(j);
  return { profiles: arr(o.connections).map(adaptProfile), connected: str(o.connected) };
}

/** GET /api/docker. */
export function adaptDocker(j) {
  return { containers: arr(obj(j).instances).map(adaptContainer) };
}

/** POST /api/connections/<name>/test. */
export function adaptTestResult(j) {
  const o = obj(j);
  const checks = arr(o.checks).map(adaptCheck);
  const denied = checks.filter((c) => c.status === 'denied').length;
  const notes = [];
  if (str(o.at)) notes.push(`Connected to ${o.at}. A test reads no key.`);
  if (str(o.sentinel)) notes.push(o.sentinel);
  return {
    ok: denied === 0,
    summary: denied === 0
      ? 'Every check passed.'
      : 'Limited: some checks were refused, so the panels that need them are off.',
    checks,
    notes,
  };
}

/** POST /api/connections → 201 `{ message, connection }`. */
export function adaptAdded(j) {
  const o = obj(j);
  const profile = o.connection ? adaptProfile(o.connection) : null;
  return { name: profile ? profile.name : null, message: str(o.message), profile };
}

/** PATCH /api/connections/<name> → `{ message, connection }`. */
export function adaptUpdated(j) {
  const o = obj(j);
  return { profile: o.connection ? adaptProfile(o.connection) : null, message: str(o.message) };
}

/** The body of POST /api/connections, from the add form. The passwords are sent here
 *  and nowhere else; the form drops them as soon as this call returns. The server takes
 *  no unknown field, so only what was filled in is sent. */
export function addRequest(form) {
  const body = { name: form.name, url: form.url };
  if (form.tags && form.tags.length) body.tags = form.tags;
  if (form.env) body.env = String(form.env).toLowerCase();
  if (form.writes === true || form.writes === false) body.writes = form.writes;
  if (form.password) body.password = form.password;
  if (form.provider) body.provider = form.provider;
  if (form.sshHost) {
    body.ssh = form.sshHost;
    if (form.sshIdentity) body.ssh_identity = form.sshIdentity;
    if (form.sshKnownHosts) body.ssh_known_hosts = form.sshKnownHosts;
  }
  if (form.sentinelAddresses && form.sentinelAddresses.length) {
    body.sentinel = form.sentinelAddresses.join(',');
    body.sentinel_master = form.sentinelMaster;
    if (form.sentinelTls) body.sentinel_tls = true;
    if (form.sentinelUser) body.sentinel_user = form.sentinelUser;
    if (form.sentinelPassword) body.sentinel_password = form.sentinelPassword;
  }
  if (form.tokenCommand && form.tokenCommand.length) body.token_command = form.tokenCommand;
  if (form.entra) body.entra = true;
  if (form.preferReplica) body.prefer_replica = true;
  if (form.standalone) body.standalone = true;
  if (form.force) body.force = true;
  return body;
}

/** The body of PATCH /api/connections/<name>: `{ writes, tag, untag, standalone }`, only
 *  what changed. */
export function updateRequest(changes) {
  const body = {};
  if (changes.writes !== undefined) body.writes = changes.writes === true;
  if (changes.addTags && changes.addTags.length) body.tag = changes.addTags;
  if (changes.removeTags && changes.removeTags.length) body.untag = changes.removeTags;
  if (changes.standalone !== undefined) body.standalone = changes.standalone === true;
  return body;
}

/** The `figures` event (`{ node, info, rates, shards? }` or `{ node, error }`) and
 *  Overview's `live`. */
export function adaptFigures(j) {
  const o = obj(j);
  const info = obj(o.info);
  const rates = obj(o.rates);
  const node = num(o.node);
  return {
    at: Date.now(),
    node: node === null ? null : String(node),
    error: str(o.error),
    opsPerSec: num(rates.ops),
    opsNow: num(info.ops_now),
    clients: num(rates.clients) ?? num(info.clients),
    blocked: num(rates.blocked) ?? num(info.blocked),
    usedMemory: num(rates.used_memory) ?? num(info.used_memory),
    maxMemory: num(info.maxmemory) || null,
    hitRate: num(rates.hit_rate),
    evictedPerSec: num(rates.evicted),
    expiredPerSec: num(rates.expired),
    netIn: num(rates.net_in),
    netOut: num(rates.net_out),
    cpuUser: num(rates.cpu_user),
    cpuSys: num(rates.cpu_sys),
    fragmentation: num(rates.fragmentation) ?? num(info.fragmentation),
    uptimeSecs: num(info.uptime_secs),
    role: str(info.role),
    replicas: num(info.replicas),
    linkUp: typeof info.link_up === 'boolean' ? info.link_up : null,
    policy: str(info.policy),
    rdbOk: typeof info.rdb_ok === 'boolean' ? info.rdb_ok : null,
    aofOn: typeof info.aof_on === 'boolean' ? info.aof_on : null,
    aofOk: typeof info.aof_ok === 'boolean' ? info.aof_ok : null,
    keys: num(info.keys),
    shards: Array.isArray(o.shards)
      ? o.shards.map((s) => {
        const x = obj(s);
        const live = obj(x.info);
        return { id: String(num(x.node) ?? 0), failed: x.failed === true, opsPerSec: num(live.ops_now), usedMemory: num(live.used_memory), keys: num(live.keys), role: str(live.role) };
      })
      : null,
  };
}

/** One shard row of Overview on a cluster: `{ node, figures, note, live, failed }`. */
export function adaptShard(j) {
  const o = obj(j);
  const node = adaptNode(o.node);
  const live = obj(o.live);
  const figures = obj(o.figures);
  return {
    id: node.id,
    index: node.index,
    slots: node.slots,
    address: node.address || null,
    role: str(live.role) ?? node.role,
    opsPerSec: num(live.ops_now),
    usedMemory: num(live.used_memory) ?? num(obj(figures.server).used_memory),
    keys: num(live.keys) ?? num(figures.keys),
    status: !node.connected ? 'not-connected' : o.failed === true ? 'no-answer' : 'ok',
    note: str(o.note),
  };
}

/** GET /api/overview. */
export function adaptOverview(j) {
  const o = obj(j);
  const figures = o.figures ? obj(o.figures) : null;
  const server = obj((figures && figures.server) || o.server);
  const provider = o.provider ? obj(o.provider) : null;
  const last = o.last_analysis ? obj(o.last_analysis) : null;
  const taken = last ? num(last.taken_at_unix) : null;
  return {
    status: oneOf(o.status, ['connecting', 'connected', 'failed'], 'connected'),
    server: { kind: flavor(server.flavor), version: str(server.version), topology: str(server.mode), policy: str(server.policy), lfu: server.lfu === true },
    memory: { used: num(server.used_memory), max: num(server.maxmemory) || null },
    keys: {
      total: figures ? num(figures.keys) : num(server.keys),
      ttlShare: figures ? num(figures.ttl_share) : null,
      withTtl: figures ? num(figures.with_ttl) : null,
      dataBytes: figures ? num(figures.dataset_bytes) : null,
      overheadBytes: figures ? num(figures.overhead_bytes) : null,
      bigKeys: figures ? num(figures.big_keys) : null,
    },
    figuresFailed: str(o.figures_failed),
    gaps: figures ? strings(figures.gaps) : [],
    checks: arr(o.perms).map(adaptCheck),
    provider: provider ? { label: str(provider.label), notice: str(provider.notice) } : null,
    lastAnalysis: last && taken !== null ? { at: taken * 1000, sampledKeys: num(last.sampled_keys) } : null,
    target: str(o.target),
    figures: o.live ? adaptFigures(o.live) : null,
    shards: arr(o.shards).map(adaptShard),
    findings: arr(o.findings).map(adaptFinding).filter((f) => f.text),
    analysis: o.analysis ? adaptOverviewAnalysis(o.analysis) : null,
  };
}

/** Overview's analysis part (`overview.analysis`): null until an analysis has a report, and
 *  while one runs. */
export function adaptOverviewAnalysis(j) {
  const o = obj(j);
  return {
    takenAt: num(o.taken_at_unix),
    sampledKeys: num(o.sampled_keys),
    patterns: num(o.patterns),
    complete: o.complete === true,
    stopped: o.stopped === true,
    sizes: o.sizes_available === true,
    findings: arr(o.findings).map(adaptFinding).filter((f) => f.text),
    topPatterns: arr(o.top_patterns).map((t) => {
      const x = obj(t);
      return { pattern: adaptName(x.pattern), estKeys: num(x.est_keys), estBytes: num(x.est_bytes), share: num(x.share) };
    }),
    neverExpiresShare: num(o.never_expires_share),
    changes: oneOf(o.changes, ['pending', 'none', 'failed', 'diff'], null),
    note: str(o.note),
  };
}

/** Where a finding's `screen` is looked into on this page (a route under `#/`): the TUI's
 *  screens the dashboard has. Streams and Search are not in it yet. */
export const FINDING_WHERE = {
  slow_log: 'server/slowlog', clients: 'server/clients', monitor: 'server/monitor', pubsub: 'pubsub', connections: 'connections',
  keys: 'browse', big_keys: 'analyze/big_keys', expiry: 'analyze/expiry', changes: 'analyze/changes', hot_keys: 'analyze/hot_keys',
  nodes: 'analyze/nodes', slots: 'analyze/slots',
};

/** One of Overview's findings "from the server": `{ severity, text, screen, area, panel }`.
 *  `where` is this page's route for it (`server/slowlog`), when the page has the screen. */
export function adaptFinding(j) {
  const o = obj(j);
  const screen = str(o.screen);
  return {
    text: str(o.text) ?? '',
    severity: oneOf(o.severity, ['warn', 'note', 'info'], 'note'),
    screen,
    label: [str(o.area), str(o.panel)].filter(Boolean).join(' › ') || null,
    where: FINDING_WHERE[screen] ?? null,
  };
}

/** GET /api/server/clients: `{ connections, groups, node }` or `{ off }`. */
export function adaptClients(j) {
  const o = obj(j);
  const connections = arr(o.connections).map((c) => {
    const x = obj(c);
    return {
      id: num(x.id), addr: str(x.addr) ?? '', name: str(x.name), lib: str(x.lib), ageSecs: num(x.age), idleSecs: num(x.idle),
      cmd: str(x.cmd), db: num(x.db), memory: num(x.mem), own: x.own === true,
    };
  });
  return {
    off: str(o.off),
    node: num(o.node),
    total: str(o.off) ? null : connections.length,
    connections,
    groups: arr(o.groups).map((g) => {
      const x = obj(g);
      return {
        by: x.kind === 'addr' ? 'address' : 'library',
        name: str(x.label) ?? '(no name)',
        connections: num(x.connections),
        oldestSecs: num(x.oldest_age),
        idleSecs: num(x.longest_idle),
        idleConns: num(x.idle_conns),
        lastCommand: str(x.top_cmd),
        memory: num(x.mem),
        flagged: x.flagged === true,
        members: arr(x.members).filter((m) => num(m) !== null),
      };
    }),
  };
}

/** GET /api/server/slowlog: `{ entries, settings, settings_off, keys_off, node }` or
 *  `{ off }`. Each `line` is one line the server already masked. */
export function adaptSlowlog(j) {
  const o = obj(j);
  const settings = o.settings ? obj(o.settings) : null;
  return {
    off: str(o.off),
    node: num(o.node),
    settings: settings ? { thresholdMicros: num(settings.threshold_micros), maxLen: num(settings.max_len) } : null,
    settingsOff: str(o.settings_off),
    keysOff: str(o.keys_off),
    entries: arr(o.entries).map((e) => {
      const x = obj(e);
      const unix = num(x.unix);
      return {
        id: num(x.id), at: unix === null ? null : unix * 1000, micros: num(x.micros), name: str(x.command), command: str(x.line) ?? str(x.command) ?? '',
        keys: strings(x.keys), moreKeys: num(x.more_keys) ?? 0, client: str(x.client), clientName: str(x.client_name), argsCut: num(x.args_cut) ?? 0, note: str(x.note),
      };
    }),
  };
}

/** POST /api/server/monitor → 202 `{ run, max_lines, max_secs }`. */
export function adaptMonitorRun(j) {
  const o = obj(j);
  return { run: num(o.run), maxLines: num(o.max_lines), maxSecs: num(o.max_secs) };
}

/** The `monitor` event: `{ run, kind: ready|lines|closed, ... }`. */
export function adaptMonitorEvent(j) {
  const o = obj(j);
  const kind = oneOf(o.kind, ['ready', 'lines', 'closed'], 'lines');
  const end = str(o.end);
  const error = str(o.error);
  let stopped = null;
  if (kind === 'closed') stopped = error ? `MONITOR stopped: ${error}.` : MONITOR_END[end] ?? MONITOR_END.stopped;
  return {
    run: num(o.run),
    kind,
    startedAt: num(o.at_unix_ms),
    maxLines: num(o.max_lines),
    maxSecs: num(o.max_secs),
    lines: arr(o.lines).map((l) => {
      const x = obj(l);
      const unix = num(x.unix);
      return { at: unix === null ? null : unix * 1000 + (num(x.millis) ?? 0), db: str(x.db) ?? (num(x.db) === null ? null : String(x.db)), client: str(x.client), command: str(x.line) ?? '', cut: num(x.cut) ?? 0 };
    }),
    end,
    error,
    stopped,
  };
}

/** GET /api/pubsub/channels. Shard channels (one list per node) are folded into the one
 *  list, marked `shard`, each with its node. */
export function adaptChannels(j) {
  const o = obj(j);
  const shards = obj(o.shards);
  const notes = strings(o.notes);
  const channels = arr(o.channels).map((c) => ({ name: str(obj(c).name) ?? '', subscribers: num(obj(c).subscribers), shard: false, node: null }));
  const shardOff = str(shards.off);
  let shardNodesOff = 0;
  const shardNodes = arr(shards.nodes);
  for (const n of shardNodes) {
    const x = obj(n);
    if (str(x.off)) {
      shardNodesOff += 1;
      if (!notes.includes(`Shard channels: ${x.off}`)) notes.push(`Shard channels: ${x.off}`);
    }
    for (const note of strings(x.notes)) if (!notes.includes(note)) notes.push(note);
    for (const c of arr(x.channels)) channels.push({ name: str(obj(c).name) ?? '', subscribers: num(obj(c).subscribers), shard: true, node: num(x.node) });
  }
  return {
    off: str(o.off),
    node: num(o.node),
    total: num(o.total_channels),
    patterns: num(o.patterns),
    channels,
    // Shard channels can be subscribed to when the server has them and at least one node answered.
    shardOff: shardOff ?? (shardNodes.length && shardNodesOff === shardNodes.length ? 'no node could list its shard channels' : null),
    notes,
  };
}

/** POST /api/pubsub/subscribe → 202 `{ tail, kind, node }`. */
export function adaptSubscription(j, name = '') {
  const o = obj(j);
  return { id: num(o.tail), name, kind: oneOf(o.kind, ['channel', 'pattern', 'shard'], 'channel'), node: num(o.node) };
}

/** The `tail` event: `{ tail, kind: ready|messages|closed, messages, dropped, error }`. */
export function adaptTailEvent(j) {
  const o = obj(j);
  return {
    id: num(o.tail),
    kind: oneOf(o.kind, ['ready', 'messages', 'closed'], 'messages'),
    messages: arr(o.messages).map((m) => {
      const x = obj(m);
      return { at: num(x.at_unix_ms), channel: str(x.channel) ?? '', body: typeof x.preview === 'string' ? x.preview : '', bytes: num(x.bytes), pattern: str(x.pattern) };
    }),
    skipped: num(o.dropped) ?? 0,
    error: str(o.error),
  };
}

/** POST /api/pubsub/publish → `{ sent, reply: { kind: 'int', value } }`. */
export function adaptPublished(j) {
  const reply = obj(obj(j).reply);
  return { receivers: reply.kind === 'int' ? num(reply.value) : null };
}

/** POST /api/cmdline → `{ line, status, text, node, route_note, times, sent, replies }`.
 *  The server draws each reply in its four views, as lines; the page only shows them. */
export function adaptReply(j) {
  const o = obj(j);
  const node = str(o.node) ?? (num(o.node) === null ? null : String(o.node));
  return {
    input: str(o.line),
    node,
    routeNote: str(o.route_note),
    kind: o.status === 'failed' ? 'error' : o.status === 'note' ? 'refused' : 'reply',
    text: str(o.text),
    times: num(o.times) ?? 1,
    sent: num(o.sent) ?? 0,
    dropped: num(o.dropped) ?? 0,
    droppedNote: str(o.dropped_note),
    replies: arr(o.replies).map((r) => {
      const x = obj(r);
      const took = num(x.took_ms);
      return {
        views: { text: joined(x.text) ?? '', table: joined(x.table), json: joined(x.json), raw: joined(x.raw) },
        micros: took === null ? null : took * 1000,
        error: x.error === true,
        cut: x.cut === true,
      };
    }),
  };
}

/** GET /api/cmdline/commands → `{ names }` or `{ off }`. */
export function adaptNames(j) {
  const o = obj(j);
  return { off: str(o.off), names: strings(o.names) };
}

/** GET /api/cmdline/help → `{ name, summary, since, complexity, arguments, subcommands }`. */
export function adaptHelp(j) {
  const o = obj(j);
  if (!str(o.name)) return null;
  return {
    name: o.name,
    summary: str(o.summary),
    since: str(o.since),
    complexity: str(o.complexity),
    arguments: str(o.arguments),
    subcommands: arr(o.subcommands).map(adaptHelp).filter(Boolean),
  };
}

/** The `confirm` object of an HTTP 409 reply. */
export function adaptConfirm(j) {
  const o = obj(obj(j).confirm ?? j);
  const expect = str(o.expect) ?? '';
  // An unknown kind that names something to type is the stricter question, never a plain Yes.
  const kind = o.kind === 'type' || (o.kind !== 'ask' && expect) ? 'type' : 'ask';
  return { id: str(o.id) ?? '', kind, text: (str(o.text) ?? '').trim(), expect, title: str(o.title) };
}

/** The `notice` event: `{ text }`. */
export function adaptNotice(j) {
  return { text: str(obj(j).text) ?? '', kind: 'info' };
}

// ---------------------------------------------------------------------------------------
// Browse, Analyze and snapshots (phase 2). The same rules: unknown stays unknown, and
// every text from Redis arrives escaped by redis-lens and is drawn as text.
// ---------------------------------------------------------------------------------------

/** A text that may be empty (a key name, a value): only a non-string is unknown. */
const text = (v) => (typeof v === 'string' ? v : null);
const bool = (v) => (typeof v === 'boolean' ? v : null);
const nums = (v) => (Array.isArray(v) ? v.map((n) => num(n)) : null);

/** A key or pattern name: the escaped text to show, the hex id that names it in a request
 *  (never the text), and the name to copy when escaping changed nothing. */
export function adaptName(j) {
  const o = obj(j);
  const id = typeof o.id === 'string' && /^(?:[0-9a-f]{2})*$/.test(o.id) ? o.id : '';
  return { name: text(o.name) ?? '', id, copy: text(o.copy) };
}

/** A key with what is known of it: a key list's row, a sampled key, a value's facts. */
export function adaptKeyRow(j) {
  const o = obj(j);
  return {
    ...adaptName(o),
    type: str(o.type),
    ttl: str(o.ttl),
    ttlSecs: num(o.ttl_secs),
    bytes: num(o.bytes),
    encoding: str(o.encoding),
    length: num(o.length),
    lengthUnit: str(o.length_unit),
    freq: num(o.freq),
    node: num(o.node),
  };
}

/** A report's summary (`state.analysis.report`, each panel's `report`). */
export function adaptReport(j) {
  const o = obj(j);
  return {
    takenAt: num(o.taken_at_unix),
    ageSecs: num(o.age_secs),
    sampledKeys: num(o.sampled_keys),
    patterns: num(o.patterns),
    complete: o.complete === true,
    stopped: o.stopped === true,
    sizes: o.sizes_available === true,
    cluster: o.cluster === true,
    previous: o.previous === true,
  };
}

const RUNNING = ['connecting', 'sampling', 'stopping'];

/** `state.analysis`: the analysis and its report. */
export function adaptAnalysis(j) {
  const o = obj(j);
  const status = oneOf(o.status, ['none', 'connecting', 'sampling', 'stopping', 'done'], 'none');
  return {
    status,
    running: RUNNING.includes(status),
    run: num(o.run) ?? 0,
    depth: oneOf(o.depth, ['quick', 'deep'], null),
    sampled: num(o.sampled),
    target: num(o.target),
    startedAt: num(o.started_unix),
    error: str(o.error),
    note: str(o.note),
    previous: o.previous === true,
    db: num(o.db) ?? 0,
    report: o.report ? adaptReport(o.report) : null,
  };
}

/** The `progress` event of an analysis. */
export function adaptProgress(j) {
  const o = obj(j);
  return {
    run: num(o.run),
    kind: oneOf(o.kind, ['started', 'sampling', 'stopping', 'done', 'stopped', 'failed', 'compared'], 'sampling'),
    depth: oneOf(o.depth, ['quick', 'deep'], null),
    sampleLimit: num(o.sample_limit),
    sampled: num(o.sampled),
    target: num(o.target),
    patterns: num(o.patterns),
    stopping: o.stopping === true,
    sampledKeys: num(o.sampled_keys),
    complete: o.complete === true,
    stopped: o.stopped === true,
    takenAt: num(o.taken_at_unix),
    kept: bool(o.kept),
    text: str(o.text),
    error: str(o.error),
  };
}

/** GET /api/dbs: the databases with their key counts, or `{ off }` on a cluster. */
export function adaptDbs(j) {
  const o = obj(j);
  return {
    off: str(o.off),
    current: num(o.current) ?? 0,
    rows: arr(o.rows).map((r) => {
      const x = obj(r);
      return { db: num(x.db) ?? 0, keys: num(x.keys), expires: num(x.expires), current: x.current === true };
    }),
    unread: str(o.unread),
    savedAs: str(o.saved_as),
    note: str(o.note),
  };
}

/** POST /api/db: 202 `{ db, run, switched: true, text }`, 200 `{ switched: false }`. */
export function adaptDbSwitch(j) {
  const o = obj(j);
  return { switched: o.switched === true, db: num(o.db), run: num(o.run), text: str(o.text) };
}

export function adaptChange(j) {
  if (!j) return null;
  const o = obj(j);
  return { deltaBytes: num(o.delta_bytes), deltaKeys: num(o.delta_keys), cause: str(o.cause), words: str(o.cause_words) ?? str(o.cause), alarming: o.alarming === true };
}

/** A pattern row of the Browse tree. */
export function adaptPatternRow(j) {
  const o = obj(j);
  return {
    kind: 'pattern',
    pattern: adaptName(o.pattern),
    type: str(o.type),
    estKeys: num(o.est_keys),
    estBytes: num(o.est_bytes),
    avgBytes: num(o.avg_bytes),
    sampledKeys: num(o.sampled_keys),
    sampledBytes: num(o.sampled_bytes),
    share: num(o.share),
    noTtl: num(o.no_ttl),
    noTtlRatio: num(o.no_ttl_ratio),
    ttlHist: nums(o.ttl_hist),
    ttlVerdict: str(o.ttl_verdict),
    verdict: str(o.verdict),
    reason: str(o.reason),
    warning: o.warning === true,
    p50: num(o.size_p50),
    p99: num(o.size_p99),
    max: num(o.size_max),
    examples: arr(o.examples).map(adaptKeyRow),
    command: str(o.command),
    marked: o.marked === true,
    change: adaptChange(o.change),
    namesSeen: num(o.names_seen),
    literal: bool(o.literal),
  };
}

/** A row of the tree: a pattern, or a group of patterns that share a prefix. */
export function adaptTreeRow(j) {
  const o = obj(j);
  if (o.kind !== 'group') return adaptPatternRow(o);
  return {
    kind: 'group',
    label: str(o.label) ?? '',
    key: str(o.key) ?? str(o.label) ?? '',
    type: str(o.type),
    estKeys: num(o.est_keys),
    estBytes: num(o.est_bytes),
    sampledKeys: num(o.sampled_keys),
    share: num(o.share),
    noTtl: num(o.no_ttl),
    noTtlRatio: num(o.no_ttl_ratio),
    verdict: str(o.verdict),
    marked: o.marked === true,
    reason: str(o.reason),
    children: arr(o.children).map(adaptPatternRow),
  };
}

const SORTS = ['memory', 'keys', 'avg_size', 'no_ttl', 'name'];

/** GET /api/browse and POST /api/browse/more: the pattern tree. */
export function adaptBrowse(j) {
  const o = obj(j);
  const n = o.names ? obj(o.names) : null;
  const partial = o.partial ? obj(o.partial) : null;
  const done = n ? arr(n.nodes_done) : [];
  return {
    source: oneOf(o.source, ['names', 'partial', 'analysis', 'none'], 'none'),
    why: str(o.why),
    sort: oneOf(o.sort, SORTS, 'keys'),
    sorts: arr(o.sorts).filter((s) => SORTS.includes(s)),
    sizes: o.sizes === true,
    node: num(o.node),
    nodes: arr(o.nodes).map((x) => {
      const r = obj(x);
      return { index: num(r.index) ?? 0, endpoint: str(r.endpoint) ?? '', role: str(r.role), status: str(r.status), sampledKeys: num(r.sampled_keys) };
    }),
    previous: o.previous === true,
    partial: partial ? { sampled: num(partial.sampled), target: num(partial.target) } : null,
    names: n ? {
      pages: num(n.pages) ?? 0,
      firstPages: num(n.first_pages),
      namesRead: num(n.names_read) ?? 0,
      dbsize: num(n.dbsize),
      complete: n.complete === true,
      done: n.done === true,
      reading: n.reading === true,
      error: str(n.error),
      refused: strings(n.refused),
      skipped: strings(n.skipped),
      missing: strings(n.missing),
      nodesDone: done.length === 2 && num(done[0]) !== null && num(done[1]) !== null ? [done[0], done[1]] : null,
    } : null,
    total: num(o.total),
    ttlLabels: strings(o.ttl_labels),
    rows: arr(o.rows).map(adaptTreeRow),
  };
}

/** GET /api/keys, GET /api/find and GET /api/keys/more: a key list. */
export function adaptKeyList(j) {
  const o = obj(j);
  const find = o.find ? obj(o.find) : null;
  const cluster = o.cluster ? obj(o.cluster) : null;
  return {
    list: num(o.list),
    kind: o.kind === 'find' ? 'find' : 'pattern',
    pattern: o.pattern ? adaptName(o.pattern) : null,
    find: find ? { text: text(find.text) ?? '', glob: text(find.glob) ?? '' } : null,
    glob: text(o.glob),
    rows: arr(o.rows).map(adaptKeyRow),
    loaded: num(o.loaded) ?? 0,
    newFrom: num(o.new_from) ?? 0,
    pages: num(o.pages) ?? 0,
    pageKeys: num(o.page_keys) ?? 1000,
    done: o.done === true,
    capped: o.capped === true,
    maxRows: num(o.max_rows),
    error: str(o.error),
    refused: strings(o.refused),
    otherPattern: num(o.other_pattern) ?? 0,
    unreadable: num(o.unreadable) ?? 0,
    sizes: o.sizes === true,
    lfu: o.lfu === true,
    freq: o.freq === true,
    cluster: cluster ? {
      walk: arr(cluster.walk).filter((n) => num(n) !== null),
      at: num(cluster.at),
      nodesDone: num(cluster.nodes_done),
      missing: strings(cluster.missing),
      narrowed: num(cluster.narrowed),
    } : null,
  };
}

/** A value drawn in one format (`value` of a string or document, `shown` of an item). */
export function adaptShown(j) {
  const o = obj(j);
  return {
    format: str(o.format) ?? 'text',
    detected: str(o.detected),
    picked: str(o.picked),
    formats: strings(o.formats),
    bytes: num(o.bytes),
    compression: str(o.compression),
    truncated: o.truncated === true,
    note: str(o.note),
    header: str(o.header) ?? '',
    lines: arr(o.lines).filter((l) => typeof l === 'string'),
    copy: text(o.copy),
    copyRefused: str(o.copy_refused),
  };
}

const pairs = (v) => arr(v).map((p) => ({ name: text(obj(p).name) ?? '', value: text(obj(p).value) ?? '' }));

/** A module type's summary (time series, Bloom, Top-K, ...). */
export function adaptModule(j) {
  const o = obj(j);
  return {
    label: str(o.label),
    info: pairs(o.info),
    top: pairs(o.top),
    stats: pairs(o.stats),
    samples: arr(o.samples).map((s) => ({ at: num(obj(s).ts_ms), value: text(obj(s).value) ?? '' })),
    missing: strings(o.missing),
    unavailable: str(o.unavailable),
  };
}

const KINDS = ['string', 'hash', 'list', 'set', 'zset', 'stream', 'json', 'vector', 'array', 'module', 'other'];

/** GET /api/value and GET /api/value/more: an open value. */
export function adaptValue(j) {
  const o = obj(j);
  return {
    view: num(o.view),
    key: o.key ? adaptName(o.key) : null,
    type: str(o.type),
    kind: oneOf(o.kind, KINDS, 'other'),
    command: str(o.command),
    meta: o.meta ? adaptKeyRow(o.meta) : null,
    pattern: o.pattern ? adaptName(o.pattern) : null,
    loading: o.loading === true,
    pages: num(o.pages) ?? 0,
    loaded: num(o.loaded) ?? 0,
    length: num(o.length),
    done: o.done === true,
    more: o.more === true,
    capped: o.capped === true,
    gone: o.gone === true,
    error: str(o.error),
    notice: str(o.notice),
    refused: strings(o.refused),
    fieldTtls: o.field_ttls === true,
    fieldTtlsOff: str(o.field_ttls_off),
    items: arr(o.items).map((i) => {
      const x = obj(i);
      return {
        index: num(x.index) ?? 0,
        name: text(x.name),
        value: text(x.value),
        ttl: str(x.ttl),
        at: num(x.at),
        fields: Array.isArray(x.fields) ? pairs(x.fields) : null,
      };
    }),
    value: o.value ? adaptShown(o.value) : null,
    cut: str(o.cut),
    message: str(o.message),
    module: o.module ? adaptModule(o.module) : null,
    cluster: o.cluster === true,
    maxItems: num(o.max_items) ?? 10000,
  };
}

/** GET /api/value/item: one item, whole, in a format. */
export function adaptItem(j) {
  const o = obj(j);
  return { index: num(o.index), field: num(o.field), name: text(o.name), shown: adaptShown(o.shown) };
}

export function adaptEstimate(j) {
  const o = obj(j);
  return {
    depth: oneOf(o.depth, ['quick', 'deep'], 'quick'),
    sampleLimit: num(o.sample_limit),
    keys: num(o.keys),
    commands: num(o.commands),
    perSec: num(o.per_sec),
    durationSecs: num(o.duration_secs),
    about: str(o.about),
    shards: num(o.shards) ?? 0,
    sizes: o.sizes === true,
    freq: o.freq === true,
  };
}

/** GET /api/analysis: the analysis, whether one runs, and what each depth would cost. */
export function adaptAnalysisInfo(j) {
  const o = obj(j);
  return { analysis: adaptAnalysis(o.analysis), estimates: arr(o.estimates).map(adaptEstimate), running: o.running === true };
}

/** POST /api/analysis → 202 `{ run, depth, sample_limit, estimate }`. */
export function adaptAnalysisStart(j) {
  const o = obj(j);
  return { run: num(o.run), depth: oneOf(o.depth, ['quick', 'deep'], 'quick'), sampleLimit: num(o.sample_limit), estimate: o.estimate ? adaptEstimate(o.estimate) : null };
}

const panel = (o) => ({ off: str(o.off), report: o.report ? adaptReport(o.report) : null, empty: str(o.empty) });

/** GET /api/analysis/big_keys. */
export function adaptBigKeys(j) {
  const o = obj(j);
  return {
    ...panel(o),
    rows: arr(o.rows).map((r) => {
      const x = obj(r);
      return { key: adaptKeyRow(x.key), pattern: adaptName(x.pattern), length: num(x.length), lengthUnit: str(x.length_unit), encoding: str(x.encoding), big: x.big === true, endpoint: str(x.endpoint) ?? str(x.node) };
    }),
    recommendations: arr(o.recommendations).map((r) => {
      const x = obj(r);
      return { severity: oneOf(x.severity, ['warn', 'note', 'info'], 'note'), text: str(x.text) ?? '', hint: str(x.hint) };
    }).filter((r) => r.text),
    factsRead: o.facts_read === true,
    notes: strings(o.notes),
    bigKeyBytes: num(o.big_key_bytes),
  };
}

/** GET /api/analysis/expiry. */
export function adaptExpiry(j) {
  const o = obj(j);
  const freed = o.freed ? obj(o.freed) : null;
  return {
    ...panel(o),
    rows: arr(o.rows).map((r) => {
      const x = obj(r);
      return {
        pattern: adaptName(x.pattern), estKeys: num(x.est_keys), estBytes: num(x.est_bytes), sampledKeys: num(x.sampled_keys), noTtl: num(x.no_ttl),
        noTtlRatio: num(x.no_ttl_ratio), ttlHist: nums(x.ttl_hist), status: str(x.status), warning: x.warning === true, explain: strings(x.explain), command: str(x.command),
      };
    }),
    ttlLabels: strings(o.ttl_labels),
    freed: freed ? {
      within: arr(freed.within).map((w) => ({ label: str(obj(w).label) ?? '', bytes: num(obj(w).bytes) })),
      later: num(freed.later),
      never: num(freed.never),
      total: num(freed.total),
      neverShare: num(freed.never_share),
    } : null,
    freedOff: str(o.freed_off),
  };
}

/** GET /api/analysis/changes and GET /api/snapshots/compare: one analysis against another. */
export function adaptDiff(j) {
  const o = obj(j);
  const side = (p) => ({ at: num(o[`${p}_taken_at_unix`]), utc: str(o[`${p}_utc`]), sampledKeys: num(o[`${p}_sampled_keys`]), usedMemory: num(o[`used_memory_${p}`]) });
  const status = o.status === undefined && Array.isArray(o.rows) ? 'diff' : oneOf(o.status, ['pending', 'none', 'failed', 'diff'], 'none');
  return {
    ...panel(o),
    status,
    text: str(o.text),
    before: side('before'),
    after: side('after'),
    rows: arr(o.rows).map((r) => {
      const x = obj(r);
      const t = x.trend ? obj(x.trend) : null;
      const g = t && t.growth ? obj(t.growth) : null;
      return {
        pattern: adaptName(x.pattern), keysBefore: num(x.keys_before), keysAfter: num(x.keys_after), bytesBefore: num(x.bytes_before), bytesAfter: num(x.bytes_after),
        change: adaptChange(x.change), partial: x.partial === true,
        trend: t ? { values: nums(t.values) || [], perDay: g ? num(g.per_day) : null, ratioPerDay: g ? num(g.ratio_per_day) : null } : null,
      };
    }),
    note: str(o.note),
    trendPoints: Array.isArray(o.trend_points) ? o.trend_points.map((p) => ({ at: num(obj(p).taken_at_unix), complete: obj(p).complete === true, sampledKeys: num(obj(p).sampled_keys) })) : null,
  };
}

/** GET /api/analysis/hot_keys, or `{ off }` without LFU counters. */
export function adaptHotKeys(j) {
  const o = obj(j);
  return {
    ...panel(o),
    rows: arr(o.rows).map((r) => {
      const x = obj(r);
      return { key: adaptKeyRow(x.key), pattern: adaptName(x.pattern), endpoint: str(x.endpoint) ?? str(x.node), hotAndBig: x.hot_and_big === true };
    }),
    hotBigBytes: num(o.hot_big_bytes),
    hotFreq: num(o.hot_freq),
  };
}

/** GET /api/analysis/nodes, or `{ off }` on a standalone server. */
export function adaptNodes(j) {
  const o = obj(j);
  return {
    ...panel(o),
    rows: arr(o.rows).map((r) => {
      const x = obj(r);
      return {
        index: num(x.index) ?? 0, id: str(x.id), primaryId: str(x.primary_id), endpoint: str(x.endpoint) ?? '', role: str(x.role), version: str(x.version),
        status: oneOf(x.status, ['scanned', 'partial', 'failed'], 'failed'), complete: x.complete === true, keys: num(x.keys), sampledKeys: num(x.sampled_keys),
        patterns: num(x.patterns), memory: num(x.memory), slots: fmtSlots(x.slots), slotCount: num(x.slot_count), sizes: x.sizes_available === true, note: str(x.note),
      };
    }),
  };
}

const slotFigures = (x) => ({ keys: num(x.keys), cpuUsec: num(x.cpu_usec), netIn: num(x.net_in), netOut: num(x.net_out), memory: num(x.memory) });

/** GET /api/analysis/slots, or `{ off }`. */
export function adaptSlots(j) {
  const o = obj(j);
  return {
    off: str(o.off),
    empty: str(o.empty),
    ranges: arr(o.ranges).map((r) => {
      const x = obj(r);
      return { node: num(x.node), endpoint: str(x.endpoint) ?? '', start: num(x.start), end: num(x.end), status: str(x.status), error: str(x.error), ...slotFigures(x) };
    }),
    hottest: arr(o.hottest).map((r) => {
      const x = obj(r);
      return {
        slot: num(x.slot), node: num(x.node), endpoint: str(x.endpoint) ?? '', ...slotFigures(x),
        sampled: arr(x.sampled_keys).map((k) => ({ key: adaptName(obj(k).key), pattern: adaptName(obj(k).pattern), kinds: str(obj(k).kinds) })),
      };
    }),
    order: oneOf(o.order, ['keys', 'cpu', 'net_in', 'net_out'], 'keys'),
    memoryColumn: o.memory_column === true,
    cpuAndNetworkZero: o.cpu_and_network_zero === true,
    note: str(o.note),
  };
}

/** GET /api/snapshots: this connection's snapshots, newest first. */
export function adaptSnapshots(j) {
  const o = obj(j);
  return {
    profile: str(o.profile),
    snapshots: arr(o.snapshots).map((s) => {
      const x = obj(s);
      return { file: str(x.file) ?? '', takenAt: num(x.taken_at_unix), utc: str(x.taken_at_utc), current: x.current === true };
    }).filter((s) => s.file),
    canSave: o.can_save === true,
    saveOff: str(o.save_off),
    saved: o.saved === true,
  };
}

/** POST /api/snapshots → 201. */
export function adaptSaved(j) {
  const o = obj(j);
  return { file: str(o.file), takenAt: num(o.taken_at_unix), utc: str(o.taken_at_utc), sampledKeys: num(o.sampled_keys), text: str(o.text) };
}

/** A failed reply → the sentence to show. Only the server's `error` field, which is
 *  redis-lens's own wording, is ever shown; any other body is replaced by a fixed line. */
export function adaptError(status, j) {
  const fixed = str(obj(j).error);
  if (status === 401) return new ApiError(TEXT.locked, { status, locked: true });
  if (status === 403) return new ApiError(TEXT.forbidden, { status });
  if (fixed) return new ApiError(fixed, { status, notLive: status === 409 });
  return new ApiError(TEXT.failed, { status, notLive: status === 409 });
}

// ---------------------------------------------------------------------------------------
// The confirm flow
// ---------------------------------------------------------------------------------------

/** Run `run`; when it needs a confirmation, ask (the dialog), then answer or cancel.
 *  `ask(confirm)` resolves to `{ typed }` or null. A confirmed reply can itself ask again
 *  (a second question: a token command, then writes on production), up to a small limit. */
export async function runWithConfirm(run, backend, ask) {
  let attempt = run;
  for (let round = 0; round < 4; round += 1) {
    try {
      return await attempt();
    } catch (err) {
      if (!(err instanceof ConfirmRequired)) throw err;
      const { confirm, adapt } = err;
      const answer = await ask(confirm);
      if (!answer) {
        try {
          await backend.cancelConfirm(confirm.id);
        } catch {
          // The pending write expires by itself; nothing was sent either way.
        }
        throw new ApiError(TEXT.cancelled, { cancelled: true });
      }
      attempt = () => backend.answerConfirm(confirm, answer.typed ?? '', adapt);
    }
  }
  throw new ApiError(TEXT.tooMany);
}

// ---------------------------------------------------------------------------------------
// The implementation
// ---------------------------------------------------------------------------------------

const seg = (s) => encodeURIComponent(String(s));

/** A query string from the parts that are set: `?pattern=6b6579&node=0`, or ''. */
export function query(parts) {
  const out = Object.entries(parts)
    .filter(([, v]) => v !== null && v !== undefined && v !== '')
    .map(([k, v]) => `${seg(k)}=${seg(v)}`);
  return out.length ? `?${out.join('&')}` : '';
}

/** Every route in one place (docs/dashboard-api.md). */
export const ROUTES = {
  state: () => ['GET', '/api/state'],
  connections: () => ['GET', '/api/connections'],
  add: () => ['POST', '/api/connections'],
  update: (name) => ['PATCH', `/api/connections/${seg(name)}`],
  remove: (name) => ['DELETE', `/api/connections/${seg(name)}`],
  test: (name) => ['POST', `/api/connections/${seg(name)}/test`],
  docker: () => ['GET', '/api/docker'],
  connect: () => ['POST', '/api/connect'],
  disconnect: () => ['POST', '/api/disconnect'],
  node: () => ['POST', '/api/node'],
  polling: () => ['POST', '/api/polling'],
  sessionWrites: () => ['POST', '/api/session/writes'],
  overview: () => ['GET', '/api/overview'],
  clients: () => ['GET', '/api/server/clients'],
  slowlog: () => ['GET', '/api/server/slowlog'],
  monitorStart: () => ['POST', '/api/server/monitor'],
  monitorStop: () => ['DELETE', '/api/server/monitor'],
  channels: () => ['GET', '/api/pubsub/channels'],
  subscribe: () => ['POST', '/api/pubsub/subscribe'],
  unsubscribe: () => ['DELETE', '/api/pubsub/subscribe'],
  publish: () => ['POST', '/api/pubsub/publish'],
  command: () => ['POST', '/api/cmdline'],
  commandNames: () => ['GET', '/api/cmdline/commands'],
  commandHelp: (name) => ['GET', `/api/cmdline/help?name=${seg(name)}`],
  dbs: () => ['GET', '/api/dbs'],
  db: () => ['POST', '/api/db'],
  browse: (q = {}) => ['GET', `/api/browse${query({ sort: q.sort, node: q.node })}`],
  browseMore: () => ['POST', '/api/browse/more'],
  keys: (pattern, node = null) => ['GET', `/api/keys${query({ pattern, node })}`],
  find: (text) => ['GET', `/api/find${query({ q: text })}`],
  keysMore: (list) => ['GET', `/api/keys/more${query({ list })}`],
  value: (key, type = null) => ['GET', `/api/value${query({ key, type })}`],
  valueMore: (view) => ['GET', `/api/value/more${query({ view })}`],
  valueItem: (q = {}) => ['GET', `/api/value/item${query({ view: q.view, index: q.index, field: q.field, format: q.format, width: q.width })}`],
  analysis: () => ['GET', '/api/analysis'],
  analysisStart: () => ['POST', '/api/analysis'],
  analysisStop: () => ['DELETE', '/api/analysis'],
  bigKeys: () => ['GET', '/api/analysis/big_keys'],
  expiry: () => ['GET', '/api/analysis/expiry'],
  changes: () => ['GET', '/api/analysis/changes'],
  hotKeys: () => ['GET', '/api/analysis/hot_keys'],
  nodes: () => ['GET', '/api/analysis/nodes'],
  slots: (q = {}) => ['GET', `/api/analysis/slots${query({ order: q.order, refresh: q.refresh ? 1 : null })}`],
  snapshots: () => ['GET', '/api/snapshots'],
  snapshotSave: () => ['POST', '/api/snapshots'],
  snapshotCompare: (a, b) => ['GET', `/api/snapshots/compare${query({ a, b })}`],
  confirm: (id) => ['POST', `/api/confirm/${seg(id)}`],
  confirmCancel: (id) => ['DELETE', `/api/confirm/${seg(id)}`],
  events: () => ['GET', '/api/events'],
};

const EVENT_ADAPTERS = {
  state: adaptState,
  figures: adaptFigures,
  overview: adaptOverview,
  tail: adaptTailEvent,
  monitor: adaptMonitorEvent,
  progress: adaptProgress,
  notice: adaptNotice,
};

// ---------------------------------------------------------------------------------------
// The session token
// ---------------------------------------------------------------------------------------

const TOKEN = /^[0-9a-f]{64}$/i;
export const TOKEN_KEY = 'redis-lens.session';

/** The token in a sign-in address: `#token=<64 hex>`. `present` says whether the hash
 *  tried to carry one at all (then it is taken out of the address even when it is not a
 *  token's shape, and `token` is null). */
export function tokenFromHash(hash) {
  const h = String(hash ?? '');
  if (!/^#token=/i.test(h)) return { present: false, token: null };
  const value = h.slice('#token='.length);
  return { present: true, token: TOKEN.test(value) ? value.toLowerCase() : null };
}

/** Where the token is kept: in memory, and in `storage` (the tab's sessionStorage) so a
 *  reload stays signed in. Any storage error is ignored: memory is enough for this page.
 *  Nothing here ever logs the token or puts it anywhere else. */
export function createTokenStore(storage = null) {
  let memory = null;
  let loaded = false;
  return {
    get() {
      if (!loaded) {
        loaded = true;
        try {
          const kept = storage ? storage.getItem(TOKEN_KEY) : null;
          if (typeof kept === 'string' && TOKEN.test(kept)) memory = kept;
        } catch {
          // No storage (a private window, a policy): memory only.
        }
      }
      return memory;
    },
    set(token) {
      loaded = true;
      memory = token;
      try {
        if (storage) storage.setItem(TOKEN_KEY, token);
      } catch {
        // Memory only.
      }
    },
    clear() {
      loaded = true;
      memory = null;
      try {
        if (storage) storage.removeItem(TOKEN_KEY);
      } catch {
        // Nothing kept there, or nothing to do.
      }
    },
  };
}

/** Take a sign-in token out of the address bar: a token there replaces a kept one (a
 *  malformed one clears it, so the page locks rather than use an old session), and the
 *  hash goes from the address and from history at once. */
export function takeTokenFromAddress(win, tokens) {
  const found = tokenFromHash(win.location.hash);
  if (!found.present) return false;
  if (found.token) tokens.set(found.token);
  else tokens.clear();
  try {
    win.history.replaceState(win.history.state, '', `${win.location.pathname}${win.location.search}`);
  } catch {
    // Cannot happen on this origin; the token is kept either way.
  }
  return true;
}

// ---------------------------------------------------------------------------------------
// The event stream: Server-Sent Events read with fetch
// ---------------------------------------------------------------------------------------

/** A parser for the text of an SSE stream, as the HTML standard reads it: lines end with
 *  CRLF, LF or CR (a CR at the end of a chunk waits for the next one, which may start with
 *  its LF); `event:` names the event, each `data:` adds a line, `:` starts a comment (the
 *  keep-alives); an empty line sends the event, when it has data. One leading space after
 *  the colon is dropped; a leading BOM is dropped. `push` takes text (decode the bytes with
 *  a streaming TextDecoder first, so a character split between chunks stays whole); `end`
 *  drops an event left unfinished, as a browser does. */
export function createSseParser(onEvent, onComment = () => {}) {
  let buffer = '';
  let first = true;
  let name = '';
  let data = [];

  function line(text) {
    if (text === '') {
      if (data.length) onEvent({ event: name || 'message', data: data.join('\n') });
      name = '';
      data = [];
      return;
    }
    if (text[0] === ':') {
      onComment(text.slice(1));
      return;
    }
    const colon = text.indexOf(':');
    const field = colon < 0 ? text : text.slice(0, colon);
    let value = colon < 0 ? '' : text.slice(colon + 1);
    if (value[0] === ' ') value = value.slice(1);
    if (field === 'event') name = value;
    else if (field === 'data') data.push(value);
    // `id` and `retry` are not used by redis-lens; other fields are ignored, as the standard says.
  }

  function scan(final) {
    let at = 0;
    for (;;) {
      let end = -1;
      for (let i = at; i < buffer.length; i += 1) {
        const c = buffer.charCodeAt(i);
        if (c === 10 || c === 13) {
          end = i;
          break;
        }
      }
      if (end < 0) break;
      // A CR as the last thing seen may be the first half of a CRLF split over two chunks.
      if (buffer.charCodeAt(end) === 13 && end === buffer.length - 1 && !final) break;
      line(buffer.slice(at, end));
      at = end + 1;
      if (buffer.charCodeAt(end) === 13 && buffer.charCodeAt(at) === 10) at += 1;
    }
    buffer = buffer.slice(at);
  }

  return {
    push(text) {
      let t = String(text);
      if (first && t.length) {
        first = false;
        if (t.charCodeAt(0) === 0xfeff) t = t.slice(1);
      }
      buffer += t;
      scan(false);
    },
    end() {
      scan(true);
      buffer = '';
      name = '';
      data = [];
    },
  };
}

/** Bytes in, events out: a streaming UTF-8 decoder in front of the parser. */
export function createSseDecoder(onEvent, onComment) {
  const decoder = new TextDecoder('utf-8');
  const parser = createSseParser(onEvent, onComment);
  return {
    push(bytes) {
      parser.push(decoder.decode(bytes, { stream: true }));
    },
    end() {
      parser.push(decoder.decode());
      parser.end();
    },
  };
}

/** How long to wait before the next try, after `attempt` failed ones in a row. */
export const STREAM_BACKOFF = [1000, 2000, 4000, 8000, 15000];

/** The one event stream: `fetch` with the token, read as it arrives. It tries again with
 *  backoff when the stream ends or fails, stops for good on 401 (`onLocked`), and stops
 *  at once (aborting the request) on `stop()`. */
export function createEventStream({
  fetchFn, url, headers, onEvent, onOpen = () => {}, onDown = () => {}, onLocked = () => {},
  setTimer = (fn, ms) => setTimeout(fn, ms), clearTimer = (t) => clearTimeout(t), backoff = STREAM_BACKOFF,
}) {
  let controller = null;
  let timer = null;
  let failures = 0;
  let running = false;

  function again() {
    if (!running) return;
    onDown();
    const wait = backoff[Math.min(failures, backoff.length - 1)];
    failures += 1;
    timer = setTimer(() => {
      timer = null;
      connect();
    }, wait);
  }

  async function connect() {
    if (!running) return;
    const mine = new AbortController();
    controller = mine;
    let res;
    try {
      res = await fetchFn(url, { method: 'GET', headers: headers(), credentials: 'omit', cache: 'no-store', signal: mine.signal });
    } catch {
      if (!mine.signal.aborted) again();
      return;
    }
    if (mine.signal.aborted) return;
    if (res.status === 401) {
      running = false;
      onLocked();
      return;
    }
    if (!res.ok || !res.body || typeof res.body.getReader !== 'function') {
      again();
      return;
    }
    failures = 0;
    onOpen();
    const decoder = createSseDecoder(onEvent);
    const reader = res.body.getReader();
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        if (value) decoder.push(value);
      }
      decoder.end();
    } catch {
      // Aborted (stop), or the connection broke: told below.
    }
    if (mine.signal.aborted || controller !== mine) return;
    again();
  }

  function stop() {
    running = false;
    if (timer !== null) clearTimer(timer);
    timer = null;
    if (controller) controller.abort();
    controller = null;
  }

  return {
    start() {
      stop();
      running = true;
      failures = 0;
      connect();
    },
    stop,
    get running() {
      return running;
    },
  };
}

// ---------------------------------------------------------------------------------------
// The implementation
// ---------------------------------------------------------------------------------------

/** The implementation that talks to the redis-lens process. `fetchFn` is passed in by the
 *  tests and by `?mock` (mock.js answers it in the browser). `tokens` is where the session
 *  token is kept (`token` gives one for a store of its own, as the mock does). */
export function createReal({ fetchFn = (...a) => fetch(...a), tokens = null, token = null, mock = false, timers = undefined } = {}) {
  const keep = tokens || createTokenStore(null);
  if (token) keep.set(token);
  let emit = () => {};
  let locked = false;

  function headers(extra = {}) {
    const out = { Accept: 'application/json', 'X-Redis-Lens': '1', ...extra };
    const t = keep.get();
    if (t) out.Authorization = `Bearer ${t}`;
    return out;
  }

  function lock() {
    keep.clear();
    if (locked) return;
    locked = true;
    emit('locked', {});
  }

  async function request([method, path], body, adapt = (x) => x) {
    const init = { method, credentials: 'omit', cache: 'no-store', headers: headers() };
    if (method !== 'GET') {
      init.headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(body ?? {});
    }
    let res;
    try {
      res = await fetchFn(path, init);
    } catch {
      throw new ApiError(TEXT.network, { network: true });
    }
    let json = null;
    try {
      json = await res.json();
    } catch {
      json = null;
    }
    if (res.status === 401) {
      lock();
      throw adaptError(401, json);
    }
    if (res.status === 409 && json && json.confirm) throw new ConfirmRequired(adaptConfirm(json), adapt);
    if (!res.ok) throw adaptError(res.status, json);
    return adapt(json ?? {});
  }

  const stream = createEventStream({
    fetchFn,
    url: ROUTES.events()[1],
    headers: () => headers({ Accept: 'text/event-stream' }),
    onEvent: ({ event, data }) => {
      const adapt = EVENT_ADAPTERS[event];
      if (!adapt) return;
      let parsed;
      try {
        parsed = JSON.parse(data);
      } catch {
        return;
      }
      emit(event, adapt(parsed));
    },
    onOpen: () => emit('link', { up: true }),
    onDown: () => emit('link', { up: false }),
    onLocked: lock,
    ...(timers || {}),
  });

  return {
    mock,
    /** Where events go (the api's bus), stream or not: a 401 on any request locks. */
    bind(emitFn) {
      emit = emitFn;
    },
    open(emitFn) {
      emit = emitFn;
      if (!locked) stream.start();
    },
    close: () => stream.stop(),
    reopen: () => {
      if (!locked) stream.start();
    },

    getState: () => request(ROUTES.state(), null, adaptState),
    listConnections: () => request(ROUTES.connections(), null, adaptConnections),
    listDocker: () => request(ROUTES.docker(), null, adaptDocker),
    addConnection: (form) => request(ROUTES.add(), addRequest(form), adaptAdded),
    updateConnection: (name, changes) => request(ROUTES.update(name), updateRequest(changes), adaptUpdated),
    removeConnection: (name) => request(ROUTES.remove(name), {}, (j) => ({ message: str(obj(j).message) })),
    testConnection: (name) => request(ROUTES.test(name), {}, adaptTestResult),
    connect: (target) => request(ROUTES.connect(), target && target.docker ? { docker: target.docker } : { name: target && target.name }, adaptState),
    disconnect: () => request(ROUTES.disconnect(), {}, adaptState),
    pickNode: (index) => request(ROUTES.node(), { index: Number(index) }, adaptState),
    setPolling: (change) => request(ROUTES.polling(), pollingRequest(change), adaptPolling),
    setSessionWrites: (on) => request(ROUTES.sessionWrites(), { on: on === true }, adaptSessionWrites),
    getOverview: () => request(ROUTES.overview(), null, adaptOverview),
    getClients: () => request(ROUTES.clients(), null, adaptClients),
    getSlowlog: () => request(ROUTES.slowlog(), null, adaptSlowlog),
    startMonitor: () => request(ROUTES.monitorStart(), {}, adaptMonitorRun),
    stopMonitor: () => request(ROUTES.monitorStop(), {}, (j) => ({ stopped: obj(j).stopped === true })),
    getChannels: () => request(ROUTES.channels(), null, adaptChannels),
    subscribe: ({ name, kind }) => request(ROUTES.subscribe(), { name, kind: kind || 'auto' }, (j) => adaptSubscription(j, name)),
    unsubscribe: () => request(ROUTES.unsubscribe(), {}, (j) => ({ stopped: obj(j).stopped === true })),
    publish: ({ channel, message, shard }) => request(ROUTES.publish(), { channel, message, shard: shard === true }, adaptPublished),
    runCommand: (line, options = {}) => request(ROUTES.command(), { line, pipeline: options.pipeline === true }, adaptReply),
    getCommandNames: () => request(ROUTES.commandNames(), null, adaptNames),
    getCommandHelp: (name) => request(ROUTES.commandHelp(name), null, adaptHelp).catch((err) => {
      // 404: the server has no docs for that name. That is an answer, not a failure.
      if (err.status === 404) return null;
      throw err;
    }),

    getDbs: () => request(ROUTES.dbs(), null, adaptDbs),
    switchDb: (db) => request(ROUTES.db(), { db: Number(db) }, adaptDbSwitch),
    getBrowse: (q = {}) => request(ROUTES.browse(q), null, adaptBrowse),
    moreNames: (q = {}) => request(ROUTES.browseMore(), q.sort ? { sort: q.sort } : {}, adaptBrowse),
    getKeys: ({ pattern, node = null }) => request(ROUTES.keys(pattern, node), null, adaptKeyList),
    findKeys: (text) => request(ROUTES.find(text), null, adaptKeyList),
    moreKeys: (list) => request(ROUTES.keysMore(list), null, adaptKeyList),
    openValue: ({ key, type = null }) => request(ROUTES.value(key, type), null, adaptValue),
    moreValue: (view) => request(ROUTES.valueMore(view), null, adaptValue),
    getItem: (q) => request(ROUTES.valueItem(q), null, adaptItem),
    getAnalysis: () => request(ROUTES.analysis(), null, adaptAnalysisInfo),
    startAnalysis: (depth) => request(ROUTES.analysisStart(), { depth: depth === 'deep' ? 'deep' : 'quick' }, adaptAnalysisStart),
    stopAnalysis: () => request(ROUTES.analysisStop(), {}, (j) => ({ stopping: obj(j).stopping === true, text: str(obj(j).text) })),
    getBigKeys: () => request(ROUTES.bigKeys(), null, adaptBigKeys),
    getExpiry: () => request(ROUTES.expiry(), null, adaptExpiry),
    getChanges: () => request(ROUTES.changes(), null, adaptDiff),
    getHotKeys: () => request(ROUTES.hotKeys(), null, adaptHotKeys),
    getNodes: () => request(ROUTES.nodes(), null, adaptNodes),
    getSlots: (q = {}) => request(ROUTES.slots(q), null, adaptSlots),
    saveSnapshot: () => request(ROUTES.snapshotSave(), {}, adaptSaved),
    listSnapshots: () => request(ROUTES.snapshots(), null, adaptSnapshots),
    compareSnapshots: (a, b) => request(ROUTES.snapshotCompare(a, b), null, adaptDiff),

    answerConfirm: (confirm, typed, adapt) => request(ROUTES.confirm(confirm.id), confirm.kind === 'type' ? { typed } : {}, adapt),
    cancelConfirm: (id) => request(ROUTES.confirmCancel(id), {}, () => ({})),
  };
}

// ---------------------------------------------------------------------------------------
// The api object
// ---------------------------------------------------------------------------------------

export const METHODS = [
  'getState', 'listConnections', 'listDocker', 'addConnection', 'updateConnection', 'removeConnection',
  'testConnection', 'connect', 'disconnect', 'pickNode', 'setPolling', 'setSessionWrites', 'getOverview', 'getClients', 'getSlowlog',
  'startMonitor', 'stopMonitor', 'getChannels', 'subscribe', 'unsubscribe', 'publish', 'runCommand',
  'getCommandNames', 'getCommandHelp',
  'getDbs', 'switchDb', 'getBrowse', 'moreNames', 'getKeys', 'findKeys', 'moreKeys', 'openValue', 'moreValue', 'getItem',
  'getAnalysis', 'startAnalysis', 'stopAnalysis', 'getBigKeys', 'getExpiry', 'getChanges', 'getHotKeys', 'getNodes', 'getSlots',
  'saveSnapshot', 'listSnapshots', 'compareSnapshots',
];

/** The methods whose reply is the new State: it is also sent to `state` subscribers, so
 *  the page has one path for it whether it came as a reply or as an event. */
const STATE_METHODS = new Set(['getState', 'connect', 'disconnect', 'pickNode']);

/** Wrap the implementation with the event bus and the confirm flow. `useBackend` swaps
 *  the implementation (initApi does, once it knows whether `?mock` is wanted). */
export function createApi(initial) {
  let backend = initial;
  const listeners = new Map();
  let ask = async () => null;
  /** How many states the event stream has delivered. A reply that carries a state is
   *  passed on only when the stream delivered none while the request was under way:
   *  otherwise the stream's is the newer one (a connect that fails at once is told on the
   *  stream before its own "connecting" reply arrives). */
  let streamed = 0;

  function emit(name, data) {
    for (const fn of Array.from(listeners.get(name) || [])) {
      try {
        fn(data);
      } catch (err) {
        // One view's mistake must not stop the others. Only the error's name is noted:
        // an event can carry values, and values are never logged.
        console.error(`redis-lens: a "${name}" handler failed (${err && err.name})`);
      }
    }
  }

  const relay = (name, data) => {
    if (name === 'state') streamed += 1;
    emit(name, data);
  };
  if (typeof backend.bind === 'function') backend.bind(relay);

  const api = {
    get mock() {
      return backend.mock === true;
    },
    useBackend(next) {
      backend.close();
      backend = next;
      if (typeof backend.bind === 'function') backend.bind(relay);
    },
    on(name, fn) {
      if (!listeners.has(name)) listeners.set(name, new Set());
      listeners.get(name).add(fn);
      return () => listeners.get(name).delete(fn);
    },
    /** Register the confirm dialog: `fn(confirm)` resolves to `{ typed }` or null. */
    setConfirmHandler(fn) {
      ask = fn;
    },
    /** Open the event stream. */
    start() {
      backend.open(relay);
    },
    /** Open the event stream again now (the Reconnect button). */
    restart() {
      backend.reopen();
    },
    stop() {
      backend.close();
    },
    /** Raise a page-side event (app.js: `live`, the Live / Paused switch). */
    emit,
  };
  for (const name of METHODS) {
    api[name] = async (...args) => {
      const before = streamed;
      const result = await runWithConfirm(() => backend[name](...args), backend, (c) => ask(c));
      if (STATE_METHODS.has(name) && streamed === before) emit('state', result);
      return result;
    };
  }
  return api;
}

/** Whether the address asks for the mock: `?mock` (with or without a value). */
export function wantsMock(search) {
  try {
    return new URLSearchParams(search || '').has('mock');
  } catch {
    return false;
  }
}

function tabStorage(win) {
  try {
    return win.sessionStorage;
  } catch {
    return null;
  }
}

/** A sign-in address opened in a tab that already shows the page changes only the hash,
 *  so the page is not loaded again: take the token out of the address now (kept for this
 *  tab, as at boot). Returns whether there was one, so the page can start over with it. */
export function acceptSignIn(win) {
  return takeTokenFromAddress(win, createTokenStore(tabStorage(win)));
}

/** The one data layer the views use. It has no implementation of use until `initApi`. */
export const api = createApi(createReal());

/** Boot: take the sign-in token out of the address, then talk to redis-lens, or to the
 *  sample servers with `?mock` (loaded only then; a release build does not serve them, and
 *  then the page says so instead of showing anything). Returns `{ mock, error }`. */
export async function initApi(win, { importMock = () => import('./mock.js') } = {}) {
  const tokens = createTokenStore(tabStorage(win));
  takeTokenFromAddress(win, tokens);
  if (!wantsMock(win.location.search)) {
    api.useBackend(createReal({ tokens }));
    return { mock: false, error: null };
  }
  try {
    const { createMockServer } = await importMock();
    const server = createMockServer({ entra: new URLSearchParams(win.location.search).has('entra') });
    api.useBackend(createReal({ fetchFn: server.fetchFn, token: server.token, mock: true }));
    return { mock: true, error: null };
  } catch {
    return { mock: true, error: TEXT.noMock };
  }
}
