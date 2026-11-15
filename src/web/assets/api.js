// redis-lens dashboard: the data layer.
//
// The views never call `fetch`. They call the one `api` object exported at the bottom and
// subscribe to its events. `api` is the real implementation (the redis-lens process that
// served this page) or, when the address has `?mock`, the sample data in mock.js.
//
// =======================================================================================
// VIEW-MODEL SHAPES (what the views consume; `null` always means "unknown", drawn as "—")
// =======================================================================================
//
// State            { connected: bool, connecting: { name, text } | null, conn: Conn | null,
//                    live: bool, lost: string | null }
//   `live` is whether the live figures are being polled (the Live / Paused switch).
//   `lost` is a fixed sentence when the connection to the Redis server was lost.
// Conn             { name, env: 'PROD'|'STAGE'|'LOCAL'|'OTHER', address (masked), readOnly: bool,
//                    server: 'Redis'|'Valkey'|null, version, topology: 'standalone'|'cluster'|
//                    'sentinel'|null, tls: bool, target: string|null, providerNotice: string|null,
//                    checksOff: [{ name, why }], nodes: [{ id, address, role, slots }],
//                    pickedNode: id|null, shardChannels: bool, monitorSeconds: number,
//                    monitorOff: string|null }
// Connections      { profiles: [Profile], docker: [Profile], dockerNote: string|null }
// Profile          { name, env, address (masked), docker: bool, container, image,
//                    tls: bool, tags: [string], readOnly: bool, access: string (why, in words),
//                    signIn: string (where the credentials live; never a credential),
//                    providerNotice, ssh: string|null (bastion host, never the user),
//                    sentinel: string|null, status: 'not-open'|'connecting'|'connected'|
//                    'limited'|'failed', statusText: string|null }
// TestResult       { ok: bool, summary: string, checks: [{ name, ok: bool, why: string|null }],
//                    notes: [string] }
// Overview         { address, server: { kind, version, topology, uptimeSecs, role, replicas,
//                    persistence, policy }, memory: { used, max }, keys: { total, ttlShare,
//                    dataBytes, overheadBytes, db }, figures: Figures|null,
//                    history: { ops: [number], clients: [number], everySecs: number|null },
//                    (the readings so far, oldest first, and how often one is taken)
//                    shards: [Shard], findings: [{ text, where: 'slowlog'|'clients'|null }],
//                    notes: [string] }
// Shard            { id, slots, address, role, opsPerSec, usedMemory, keys, picked: bool,
//                    status: 'ok'|'not-connected'|'no-answer' }
// Figures          { at (ms), node: id|null, opsPerSec, clients, usedMemory, maxMemory,
//                    hitRate (0..1), evictedPerSec, expiredPerSec }
// Clients          { off: string|null, total, groups: [{ by: 'library'|'address', name,
//                    connections, oldestSecs, idleSecs, lastCommand, memory, flagged: bool }],
//                    notes: [string] }
// Slowlog          { off: string|null, threshold: string|null, entries: [{ at (ms), micros,
//                    command (one masked line), keys: [string], client, note }], notes: [string] }
// Monitor          { running: bool, left (s)|null, lines: [{ at (ms), db, client, command }],
//                    kept: number|null, node: string|null, stopped: string|null }   (event)
// Channels         { off: string|null, total, patterns: number|null, node: string|null,
//                    channels: [{ name, subscribers: number|null, shard: bool, node }],
//                    notes: [string] }
// Subscription     { name, kind: 'channel'|'pattern'|'shard' }
// Tail             { messages: [{ at (ms), channel, body }], skipped: number,
//                    stopped: string|null }                                          (event)
// Published        { receivers: number|null, text: string|null }
// Reply            { input (masked line), node: string|null, micros: number|null,
//                    kind: 'reply'|'error'|'refused'|'note',
//                    views: { text, table: { columns: [string], rows: [[string]] }|string|null,
//                             json: string|null, raw: string|null } }
//                    (a table the server already drew arrives as a string)
// Confirm          { id, kind: 'ask'|'type', text, expect, title: string|null }
// Notice           { text, kind: 'info'|'ok'|'error' }                               (event)
//
// =======================================================================================
// METHODS (all async; each can reject with ApiError, whose `message` is fixed wording)
// =======================================================================================
//
//   getState()                         → State          GET    /api/state
//   listConnections()                  → Connections    GET    /api/connections
//   addConnection(form)                → { name }       POST   /api/connections
//   updateConnection(name, changes)    → Profile        PATCH  /api/connections/<name>
//   removeConnection(name)             → {}             DELETE /api/connections/<name>
//   testConnection(name)               → TestResult     POST   /api/connections/<name>/test
//   discoverDocker()                   → Connections    POST   /api/connections/discover
//   connect(name)                      → State          POST   /api/connect
//   disconnect()                       → State          POST   /api/disconnect
//   setLive(bool)                      → State          POST   /api/live
//   pickNode(id)                       → State          POST   /api/node
//   getOverview()                      → Overview       GET    /api/overview
//   getClients()                       → Clients        GET    /api/server/clients
//   getSlowlog()                       → Slowlog        GET    /api/server/slowlog
//   startMonitor()                     → Monitor        POST   /api/server/monitor  (asks first)
//   stopMonitor()                      → {}             DELETE /api/server/monitor
//   getChannels()                      → Channels       GET    /api/pubsub/channels
//   subscribe({ target, shard })       → Subscription   POST   /api/pubsub/subscribe
//   unsubscribe()                      → {}             DELETE /api/pubsub/subscribe
//   publish({ channel, message, shard }) → Published    POST   /api/pubsub/publish
//   runCommand(line)                   → Reply          POST   /api/command
//
// A write the gate wants confirmed comes back as HTTP 409 `{ "confirm": … }`. The wrapper
// here opens the confirm dialog (through the handler app.js registers), answers with
// `POST /api/confirm/<id>` and returns that reply as the method's result; Cancel sends
// `DELETE /api/confirm/<id>` and rejects with `ApiError.cancelled`.
//
// EVENTS: `api.on(name, fn)` returns a function that unsubscribes.
//   state (State) · figures (Figures) · tail (Tail) · monitor (Monitor) · progress (as sent,
//   used from phase 2) · notice (Notice) · link ({ up: bool }, the page's own line to
//   redis-lens) · locked ({}): the session is missing or wrong (HTTP 401).
//
// The real endpoint shapes are not final. Every place that reads the server's JSON is one
// small function marked `// ADAPT:` below; the routes are in the one ROUTES table.

import { createMock } from './mock.js';

// ---------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------

/** A failure with wording that is safe to show: redis-lens's own, never the server's. */
export class ApiError extends Error {
  constructor(message, { status = 0, cancelled = false, locked = false, network = false } = {}) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.cancelled = cancelled;
    this.locked = locked;
    this.network = network;
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
};

// ---------------------------------------------------------------------------------------
// Adapters: the server's JSON → the view-models above. Pure functions, tested in
// tests/api.test.js. Unknown stays unknown (null): nothing is invented here.
// ---------------------------------------------------------------------------------------

const str = (v) => (typeof v === 'string' && v !== '' ? v : null);
const num = (v) => (typeof v === 'number' && Number.isFinite(v) ? v : null);
const arr = (v) => (Array.isArray(v) ? v : []);
const obj = (v) => (v && typeof v === 'object' && !Array.isArray(v) ? v : {});
const strings = (v) => arr(v).filter((s) => typeof s === 'string' && s !== '');
/** The first of several names a field may have (snake_case from the server, or camelCase). */
const pick = (o, ...names) => {
  for (const n of names) if (o[n] !== undefined && o[n] !== null) return o[n];
  return null;
};
const ENVS = ['PROD', 'STAGE', 'LOCAL', 'OTHER'];
const env = (v) => {
  const e = String(v ?? '').toUpperCase();
  return ENVS.includes(e) ? e : 'OTHER';
};
const oneOf = (v, allowed, fallback) => (allowed.includes(v) ? v : fallback);

// ADAPT: one permission check or feature that is off: { name, why }.
export function adaptCheck(j) {
  const o = obj(j);
  return { name: str(pick(o, 'name', 'check', 'command')) ?? 'check', ok: pick(o, 'ok', 'allowed') === true, why: str(pick(o, 'why', 'reason', 'note')) };
}

// ADAPT: a cluster node in the node picker.
export function adaptNode(j) {
  const o = obj(j);
  return { id: str(pick(o, 'id', 'address')) ?? '', address: str(o.address) ?? '', role: str(o.role), slots: str(o.slots) };
}

// ADAPT: the connected connection inside State.
export function adaptConn(j) {
  const o = obj(j);
  return {
    name: str(o.name) ?? '',
    env: env(o.env),
    address: str(o.address),
    readOnly: pick(o, 'read_only', 'readOnly') !== false,
    server: str(pick(o, 'server', 'kind')),
    version: str(o.version),
    topology: str(o.topology),
    tls: o.tls === true,
    target: str(o.target),
    providerNotice: str(pick(o, 'provider_notice', 'providerNotice')),
    checksOff: arr(pick(o, 'checks_off', 'checksOff')).map(adaptCheck).map(({ name, why }) => ({ name, why })),
    nodes: arr(o.nodes).map(adaptNode),
    pickedNode: str(pick(o, 'picked_node', 'pickedNode')),
    shardChannels: pick(o, 'shard_channels', 'shardChannels') === true,
    monitorSeconds: num(pick(o, 'monitor_seconds', 'monitorSeconds')) ?? 30,
    monitorOff: str(pick(o, 'monitor_off', 'monitorOff')),
  };
}

// ADAPT: GET /api/state, the `state` event, and the reply of connect / disconnect / live / node.
export function adaptState(j) {
  const o = obj(j);
  const connecting = obj(o.connecting);
  const conn = pick(o, 'connection', 'conn');
  return {
    connected: o.connected === true && !!conn,
    connecting: o.connecting ? { name: str(connecting.name) ?? '', text: str(connecting.text) } : null,
    conn: conn ? adaptConn(conn) : null,
    live: o.live !== false,
    lost: str(o.lost),
  };
}

// ADAPT: one saved profile or Docker container on Connections. No credential is read here
// because none is sent: `sign_in` is a sentence about where they live.
export function adaptProfile(j) {
  const o = obj(j);
  return {
    name: str(o.name) ?? '',
    env: env(o.env),
    address: str(o.address),
    docker: o.docker === true,
    container: str(o.container),
    image: str(o.image),
    tls: o.tls === true,
    tags: strings(o.tags),
    readOnly: pick(o, 'read_only', 'readOnly') !== false,
    access: str(o.access),
    signIn: str(pick(o, 'sign_in', 'signIn')),
    providerNotice: str(pick(o, 'provider_notice', 'providerNotice')),
    ssh: str(o.ssh),
    sentinel: str(o.sentinel),
    status: oneOf(o.status, ['not-open', 'connecting', 'connected', 'limited', 'failed'], 'not-open'),
    statusText: str(pick(o, 'status_text', 'statusText')),
  };
}

// ADAPT: GET /api/connections and POST /api/connections/discover.
export function adaptConnections(j) {
  const o = obj(j);
  return {
    profiles: arr(o.profiles).map(adaptProfile),
    docker: arr(o.docker).map((d) => ({ ...adaptProfile(d), docker: true })),
    dockerNote: str(pick(o, 'docker_note', 'dockerNote')),
  };
}

// ADAPT: POST /api/connections/<name>/test.
export function adaptTestResult(j) {
  const o = obj(j);
  return { ok: o.ok === true, summary: str(o.summary) ?? '', checks: arr(o.checks).map(adaptCheck), notes: strings(o.notes) };
}

// ADAPT: POST /api/connections (the reply names what was saved).
export function adaptAdded(j) {
  return { name: str(obj(j).name) };
}

// ADAPT: the body of POST /api/connections, from the add form. The password is sent here
// and nowhere else; the form drops it as soon as this call returns.
export function addRequest(form) {
  const body = { name: form.name, url: form.url, tags: form.tags || [] };
  if (form.env) body.env = String(form.env).toLowerCase();
  if (form.writes === true || form.writes === false) body.writes = form.writes;
  if (form.password) body.password = form.password;
  if (form.tls) body.tls = true;
  if (form.provider) body.provider = form.provider;
  if (form.sshHost) body.ssh = { host: form.sshHost, identity: form.sshIdentity || null, known_hosts: form.sshKnownHosts || null };
  if (form.sentinelAddresses && form.sentinelAddresses.length) {
    body.sentinel = { addresses: form.sentinelAddresses, master: form.sentinelMaster };
    if (form.sentinelPassword) body.sentinel.password = form.sentinelPassword;
  }
  if (form.tokenCommand && form.tokenCommand.length) {
    body.token_command = form.tokenCommand;
    body.token_command_confirmed = form.tokenCommandConfirmed === true;
  }
  if (form.preferReplica) body.prefer_replica = true;
  if (form.forceStandalone) body.force_standalone = true;
  return body;
}

// ADAPT: the body of PATCH /api/connections/<name>. Only what changed is sent.
export function updateRequest(changes) {
  const body = {};
  if (changes.env !== undefined) body.env = String(changes.env).toLowerCase();
  if (changes.tags !== undefined) body.tags = changes.tags;
  if (changes.writes !== undefined) body.writes = changes.writes === true;
  return body;
}

// ADAPT: the `figures` event and Overview's latest figures.
export function adaptFigures(j) {
  const o = obj(j);
  return {
    at: num(o.at) ?? Date.now(),
    node: str(o.node),
    opsPerSec: num(pick(o, 'ops_per_sec', 'opsPerSec')),
    clients: num(o.clients),
    usedMemory: num(pick(o, 'used_memory', 'usedMemory')),
    maxMemory: num(pick(o, 'max_memory', 'maxMemory')) || null,
    hitRate: num(pick(o, 'hit_rate', 'hitRate')),
    evictedPerSec: num(pick(o, 'evicted_per_sec', 'evictedPerSec')),
    expiredPerSec: num(pick(o, 'expired_per_sec', 'expiredPerSec')),
  };
}

// ADAPT: one shard row of Overview on a cluster.
export function adaptShard(j) {
  const o = obj(j);
  return {
    id: str(pick(o, 'id', 'address')) ?? '',
    slots: str(o.slots),
    address: str(o.address),
    role: str(o.role),
    opsPerSec: num(pick(o, 'ops_per_sec', 'opsPerSec')),
    usedMemory: num(pick(o, 'used_memory', 'usedMemory')),
    keys: num(o.keys),
    picked: o.picked === true,
    status: oneOf(o.status, ['ok', 'not-connected', 'no-answer'], 'ok'),
  };
}

// ADAPT: GET /api/overview.
export function adaptOverview(j) {
  const o = obj(j);
  const server = obj(o.server);
  const memory = obj(o.memory);
  const keys = obj(o.keys);
  const history = obj(o.history);
  return {
    address: str(o.address),
    server: {
      kind: str(pick(server, 'kind', 'server')),
      version: str(server.version),
      topology: str(server.topology),
      uptimeSecs: num(pick(server, 'uptime_secs', 'uptimeSecs')),
      role: str(server.role),
      replicas: num(server.replicas),
      persistence: str(server.persistence),
      policy: str(pick(server, 'policy', 'maxmemory_policy')),
    },
    memory: { used: num(memory.used), max: num(memory.max) || null },
    keys: {
      total: num(keys.total),
      ttlShare: num(pick(keys, 'ttl_share', 'ttlShare')),
      dataBytes: num(pick(keys, 'data_bytes', 'dataBytes')),
      overheadBytes: num(pick(keys, 'overhead_bytes', 'overheadBytes')),
      db: num(keys.db),
    },
    figures: o.figures ? adaptFigures(o.figures) : null,
    history: { ops: arr(history.ops).map(num), clients: arr(history.clients).map(num), everySecs: num(pick(history, 'every_secs', 'everySecs')) },
    shards: arr(o.shards).map(adaptShard),
    findings: arr(o.findings).map((f) => ({ text: str(obj(f).text) ?? '', where: oneOf(obj(f).where, ['slowlog', 'clients'], null) })).filter((f) => f.text),
    notes: strings(o.notes),
  };
}

// ADAPT: GET /api/server/clients.
export function adaptClients(j) {
  const o = obj(j);
  return {
    off: str(o.off),
    total: num(o.total),
    groups: arr(o.groups).map((g) => {
      const x = obj(g);
      return {
        by: oneOf(x.by, ['library', 'address'], 'library'),
        name: str(x.name) ?? '(no name)',
        connections: num(x.connections),
        oldestSecs: num(pick(x, 'oldest_secs', 'oldestSecs')),
        idleSecs: num(pick(x, 'idle_secs', 'idleSecs')),
        lastCommand: str(pick(x, 'last_command', 'lastCommand')),
        memory: num(x.memory),
        flagged: x.flagged === true,
      };
    }),
    notes: strings(o.notes),
  };
}

// ADAPT: GET /api/server/slowlog. `command` is one line the server already masked.
export function adaptSlowlog(j) {
  const o = obj(j);
  return {
    off: str(o.off),
    threshold: str(o.threshold),
    entries: arr(o.entries).map((e) => {
      const x = obj(e);
      return { at: num(x.at), micros: num(x.micros), command: str(x.command) ?? '', keys: strings(x.keys), client: str(x.client), note: str(x.note) };
    }),
    notes: strings(o.notes),
  };
}

// ADAPT: the `monitor` event and the reply of POST /api/server/monitor.
export function adaptMonitor(j) {
  const o = obj(j);
  return {
    running: o.running === true,
    left: num(pick(o, 'left', 'seconds_left', 'seconds')),
    lines: arr(o.lines).map((l) => {
      const x = obj(l);
      return { at: num(x.at), db: num(x.db), client: str(x.client), command: str(x.command) ?? '' };
    }),
    kept: num(o.kept),
    node: str(o.node),
    stopped: str(o.stopped),
  };
}

// ADAPT: GET /api/pubsub/channels.
export function adaptChannels(j) {
  const o = obj(j);
  return {
    off: str(o.off),
    total: num(o.total),
    patterns: num(o.patterns),
    node: str(o.node),
    channels: arr(o.channels).map((c) => {
      const x = obj(c);
      return { name: str(x.name) ?? '', subscribers: num(x.subscribers), shard: x.shard === true, node: str(x.node) };
    }),
    notes: strings(o.notes),
  };
}

// ADAPT: POST /api/pubsub/subscribe.
export function adaptSubscription(j) {
  const o = obj(j);
  return { name: str(pick(o, 'name', 'target')) ?? '', kind: oneOf(o.kind, ['channel', 'pattern', 'shard'], 'channel') };
}

// ADAPT: the `tail` event.
export function adaptTail(j) {
  const o = obj(j);
  return {
    messages: arr(o.messages).map((m) => {
      const x = obj(m);
      return { at: num(x.at), channel: str(x.channel) ?? '', body: typeof x.body === 'string' ? x.body : '' };
    }),
    skipped: num(o.skipped) ?? 0,
    stopped: str(o.stopped),
  };
}

// ADAPT: POST /api/pubsub/publish.
export function adaptPublished(j) {
  const o = obj(j);
  return { receivers: num(o.receivers), text: str(o.text) };
}

// ADAPT: POST /api/command. The server draws the reply; the page only shows it. Each view
// may come as one string or as a list of lines; the table view as `{ columns, rows }` or,
// already drawn, as lines of text.
export function adaptReply(j) {
  const o = obj(j);
  const views = o.views ? obj(o.views) : o;
  const lines = (v) => {
    if (typeof v === 'string') return v;
    if (Array.isArray(v) && v.every((line) => typeof line === 'string')) return v.join('\n');
    return null;
  };
  const t = views.table;
  const table = t && typeof t === 'object' && !Array.isArray(t) && Array.isArray(t.rows)
    ? { columns: arr(t.columns).map(String), rows: t.rows.map((r) => arr(r).map(String)) }
    : lines(t);
  const took = num(pick(o, 'took_ms', 'tookMs'));
  return {
    input: str(o.input),
    node: str(o.node),
    micros: num(o.micros) ?? (took !== null ? took * 1000 : null),
    kind: oneOf(o.kind, ['reply', 'error', 'refused', 'note'], o.error === true ? 'error' : 'reply'),
    views: { text: lines(views.text) ?? '', table, json: lines(views.json), raw: lines(views.raw) },
  };
}

// ADAPT: the `confirm` object of an HTTP 409 reply.
export function adaptConfirm(j) {
  const o = obj(obj(j).confirm ?? j);
  const expect = str(o.expect) ?? '';
  // An unknown kind that names something to type is the stricter question, never a plain Yes.
  const kind = o.kind === 'type' || (o.kind !== 'ask' && expect) ? 'type' : 'ask';
  return { id: str(o.id) ?? '', kind, text: str(o.text) ?? '', expect, title: str(o.title) };
}

// ADAPT: the `notice` event.
export function adaptNotice(j) {
  const o = obj(j);
  return { text: str(o.text) ?? '', kind: oneOf(o.kind, ['info', 'ok', 'error'], 'info') };
}

// ADAPT: a failed reply → the sentence to show. Only the server's `error` field, which is
// redis-lens's fixed wording, is ever shown; any other body is replaced by a fixed line.
export function adaptError(status, j) {
  const fixed = str(obj(j).error);
  if (status === 401) return new ApiError(TEXT.locked, { status, locked: true });
  if (fixed) return new ApiError(fixed, { status });
  if (status === 403) return new ApiError(TEXT.forbidden, { status });
  return new ApiError(TEXT.failed, { status });
}

// ---------------------------------------------------------------------------------------
// The confirm flow, shared by the real implementation and the mock
// ---------------------------------------------------------------------------------------

/** Run `run`; when it needs a confirmation, ask (the dialog), then answer or cancel.
 *  `ask(confirm)` resolves to `{ typed }` or null. A confirmed reply can itself ask again
 *  (a second question), up to a small limit. */
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
      attempt = () => backend.answerConfirm(confirm.id, answer.typed ?? '', adapt);
    }
  }
  throw new ApiError(TEXT.tooMany);
}

// ---------------------------------------------------------------------------------------
// The real implementation
// ---------------------------------------------------------------------------------------

const seg = (s) => encodeURIComponent(String(s));

// ADAPT: every route in one place.
export const ROUTES = {
  state: () => ['GET', '/api/state'],
  connections: () => ['GET', '/api/connections'],
  add: () => ['POST', '/api/connections'],
  update: (name) => ['PATCH', `/api/connections/${seg(name)}`],
  remove: (name) => ['DELETE', `/api/connections/${seg(name)}`],
  test: (name) => ['POST', `/api/connections/${seg(name)}/test`],
  discover: () => ['POST', '/api/connections/discover'],
  connect: () => ['POST', '/api/connect'],
  disconnect: () => ['POST', '/api/disconnect'],
  live: () => ['POST', '/api/live'],
  node: () => ['POST', '/api/node'],
  overview: () => ['GET', '/api/overview'],
  clients: () => ['GET', '/api/server/clients'],
  slowlog: () => ['GET', '/api/server/slowlog'],
  monitorStart: () => ['POST', '/api/server/monitor'],
  monitorStop: () => ['DELETE', '/api/server/monitor'],
  channels: () => ['GET', '/api/pubsub/channels'],
  subscribe: () => ['POST', '/api/pubsub/subscribe'],
  unsubscribe: () => ['DELETE', '/api/pubsub/subscribe'],
  publish: () => ['POST', '/api/pubsub/publish'],
  command: () => ['POST', '/api/command'],
  confirm: (id) => ['POST', `/api/confirm/${seg(id)}`],
  confirmCancel: (id) => ['DELETE', `/api/confirm/${seg(id)}`],
  events: () => ['GET', '/api/events'],
};

const EVENT_ADAPTERS = {
  state: adaptState,
  figures: adaptFigures,
  tail: adaptTail,
  monitor: adaptMonitor,
  progress: (j) => j,
  notice: adaptNotice,
};

/** The implementation that talks to the redis-lens process. `fetchFn` and `EventSourceFn`
 *  can be passed in so the tests can run it without a browser. */
export function createReal({ fetchFn = (...a) => fetch(...a), EventSourceFn = null } = {}) {
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
    mock: false,
    open(emitFn) {
      emit = emitFn;
      openEvents();
    },
    close: closeEvents,
    reopen: openEvents,

    getState: () => request(ROUTES.state(), null, adaptState),
    listConnections: () => request(ROUTES.connections(), null, adaptConnections),
    addConnection: (form) => request(ROUTES.add(), addRequest(form), adaptAdded),
    updateConnection: (name, changes) => request(ROUTES.update(name), updateRequest(changes), adaptProfile),
    removeConnection: (name) => request(ROUTES.remove(name), {}, () => ({})),
    testConnection: (name) => request(ROUTES.test(name), {}, adaptTestResult),
    discoverDocker: () => request(ROUTES.discover(), {}, adaptConnections),
    connect: (name) => request(ROUTES.connect(), { name }, adaptState),
    disconnect: () => request(ROUTES.disconnect(), {}, adaptState),
    setLive: (live) => request(ROUTES.live(), { live: live === true }, adaptState),
    pickNode: (node) => request(ROUTES.node(), { node }, adaptState),
    getOverview: () => request(ROUTES.overview(), null, adaptOverview),
    getClients: () => request(ROUTES.clients(), null, adaptClients),
    getSlowlog: () => request(ROUTES.slowlog(), null, adaptSlowlog),
    startMonitor: () => request(ROUTES.monitorStart(), {}, adaptMonitor),
    stopMonitor: () => request(ROUTES.monitorStop(), {}, () => ({})),
    getChannels: () => request(ROUTES.channels(), null, adaptChannels),
    subscribe: ({ target, shard }) => request(ROUTES.subscribe(), { target, shard: shard === true }, adaptSubscription),
    unsubscribe: () => request(ROUTES.unsubscribe(), {}, () => ({})),
    publish: ({ channel, message, shard }) => request(ROUTES.publish(), { channel, message, shard: shard === true }, adaptPublished),
    runCommand: (line) => request(ROUTES.command(), { line }, adaptReply),

    answerConfirm: (id, typed, adapt) => request(ROUTES.confirm(id), { typed }, adapt),
    cancelConfirm: (id) => request(ROUTES.confirmCancel(id), {}, () => ({})),
  };
}

// ---------------------------------------------------------------------------------------
// The api object
// ---------------------------------------------------------------------------------------

export const METHODS = [
  'getState', 'listConnections', 'addConnection', 'updateConnection', 'removeConnection',
  'testConnection', 'discoverDocker', 'connect', 'disconnect', 'setLive', 'pickNode',
  'getOverview', 'getClients', 'getSlowlog', 'startMonitor', 'stopMonitor', 'getChannels',
  'subscribe', 'unsubscribe', 'publish', 'runCommand',
];

/** The methods whose reply is the new State: it is also sent to `state` subscribers, so
 *  the page has one path for it whether it came as a reply or as an event. */
const STATE_METHODS = new Set(['getState', 'connect', 'disconnect', 'setLive', 'pickNode']);

/** Wrap an implementation (real or mock) with the event bus and the confirm flow. */
export function createApi(backend) {
  const listeners = new Map();
  let ask = async () => null;

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
      backend.open(emit);
    },
    /** Open the event stream again now (the Reconnect button). */
    restart() {
      backend.reopen();
    },
    stop() {
      backend.close();
    },
  };
  for (const name of METHODS) {
    api[name] = async (...args) => {
      const result = await runWithConfirm(() => backend[name](...args), backend, (c) => ask(c));
      if (STATE_METHODS.has(name)) emit('state', result);
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
export const api = createApi(
  inBrowser && wantsMock(window.location.search)
    ? createMock({ ApiError, ConfirmRequired })
    : createReal(),
);
