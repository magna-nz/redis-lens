// redis-lens dashboard: the data layer.
//
// The views never call `fetch`. They call the one `api` object exported at the bottom and
// subscribe to its events. `api` talks to the redis-lens process that served this page
// (the routes and replies are in docs/dashboard-api.md). With `?mock` in the address it
// talks to mock.js instead, which answers the same routes with the same JSON, so the
// adapters below are the one path for both.
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
//                 replies: [{ views: { text, table, json, raw } (each a string or null),
//                 micros, error: bool, cut: bool }] }
// Names         { off: string|null, names: [string] }   ('config', 'config|get', ...)
// Help          { name, summary, since, complexity, arguments, subcommands: [Help] } | null
// Confirm       { id, kind: 'ask'|'type', text, expect, title: string|null }
// Notice        { text, kind: 'info' }
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
//
// A write the gate wants confirmed comes back as HTTP 409 `{ "confirm": … }`. The wrapper
// here opens the confirm dialog (through the handler app.js registers), answers with
// `POST /api/confirm/<id>` and returns that reply as the method's result; Cancel sends
// `DELETE /api/confirm/<id>` and rejects with `ApiError.cancelled`.
// A panel whose feature is off answers 200 `{ "off": why }`: its view-model carries `off`.
//
// EVENTS: `api.on(name, fn)` returns a function that unsubscribes.
//   state (State) · figures (Figures) · tail (TailEvent) · monitor (MonitorEvent) ·
//   notice (Notice) · link ({ up: bool }, the page's own line to redis-lens) ·
//   locked ({}): the session is missing or wrong (HTTP 401) · live ({ live: bool }):
//   raised by app.js when the state's `live` changes, for the views that draw figures.

import { createMockServer } from './mock.js';

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
  };
}

/** One of Overview's findings "from the server": `{ severity, text, screen, area, panel }`.
 *  `where` is this page's panel for it, when this phase has one. */
export function adaptFinding(j) {
  const o = obj(j);
  const screen = str(o.screen);
  return {
    text: str(o.text) ?? '',
    severity: oneOf(o.severity, ['warn', 'note', 'info'], 'note'),
    screen,
    label: [str(o.area), str(o.panel)].filter(Boolean).join(' › ') || null,
    where: { slow_log: 'slowlog', clients: 'clients' }[screen] ?? null,
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
  confirm: (id) => ['POST', `/api/confirm/${seg(id)}`],
  confirmCancel: (id) => ['DELETE', `/api/confirm/${seg(id)}`],
  events: () => ['GET', '/api/events'],
};

const EVENT_ADAPTERS = {
  state: adaptState,
  figures: adaptFigures,
  tail: adaptTailEvent,
  monitor: adaptMonitorEvent,
  notice: adaptNotice,
};

/** The implementation that talks to the redis-lens process. `fetchFn` and `EventSourceFn`
 *  are passed in by the tests, and by `?mock` (mock.js answers them in the browser). */
export function createReal({ fetchFn = (...a) => fetch(...a), EventSourceFn = null, mock = false } = {}) {
  let emit = () => {};
  let source = null;
  let retry = null;
  let locked = false;

  async function request([method, path], body, adapt = (x) => x) {
    const init = { method, credentials: 'same-origin', cache: 'no-store', headers: { Accept: 'application/json', 'X-Redis-Lens': '1' } };
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
      locked = true;
      emit('locked', {});
      throw adaptError(401, json);
    }
    if (res.status === 409 && json && json.confirm) throw new ConfirmRequired(adaptConfirm(json), adapt);
    if (!res.ok) throw adaptError(res.status, json);
    return adapt(json ?? {});
  }

  function openEvents() {
    const Source = EventSourceFn || (typeof EventSource === 'function' ? EventSource : null);
    if (!Source || locked) return;
    closeEvents();
    const es = new Source(ROUTES.events()[1]);
    source = es;
    for (const [name, adapt] of Object.entries(EVENT_ADAPTERS)) {
      es.addEventListener(name, (e) => {
        let data;
        try {
          data = JSON.parse(e.data);
        } catch {
          return;
        }
        emit(name, adapt(data));
      });
    }
    es.onopen = () => emit('link', { up: true });
    es.onerror = () => {
      emit('link', { up: false });
      // The browser retries by itself unless the stream was closed for good (an HTTP
      // error such as 401). Then find out which, and try again a little later.
      if (es.readyState === 2 && source === es) {
        request(ROUTES.state(), null, adaptState).then(
          () => {
            retry = setTimeout(openEvents, 2000);
          },
          (err) => {
            if (!err.locked) retry = setTimeout(openEvents, 4000);
          },
        );
      }
    };
  }

  function closeEvents() {
    if (retry) clearTimeout(retry);
    retry = null;
    if (source) source.close();
    source = null;
  }

  return {
    mock,
    open(emitFn) {
      emit = emitFn;
      openEvents();
    },
    close: closeEvents,
    reopen: openEvents,

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
];

/** The methods whose reply is the new State: it is also sent to `state` subscribers, so
 *  the page has one path for it whether it came as a reply or as an event. */
const STATE_METHODS = new Set(['getState', 'connect', 'disconnect', 'pickNode']);

/** Wrap the implementation with the event bus and the confirm flow. */
export function createApi(backend) {
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

  const api = {
    mock: backend.mock === true,
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
      backend.open((name, data) => {
        if (name === 'state') streamed += 1;
        emit(name, data);
      });
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

const inBrowser = typeof window !== 'undefined' && typeof window.location !== 'undefined';

/** The one data layer the views use. */
export const api = createApi(createReal(
  inBrowser && wantsMock(window.location.search)
    // `?mock&entra` shows the page as a build with Entra ID sign-in would.
    ? { ...createMockServer({ entra: new URLSearchParams(window.location.search).has('entra') }), mock: true }
    : {},
));
