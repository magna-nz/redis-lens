// redis-lens dashboard: sample data, used when the address has `?mock`.
//
// It stands in for the redis-lens process at the HTTP level: `createMockServer()` returns
// a `fetch` and an `EventSource` that answer the routes of docs/dashboard-api.md with the
// same JSON, status codes and events the real server sends. So `?mock` runs the very same
// adapters and views as the real thing. The data is the approved mockup's.
//
// Nothing here talks to a network, and nothing a form sends is kept: a password least of
// all (the mock only notes whether one was given). Like the real server it refuses a body
// with a field it does not know, so a page that sends a wrong field fails here too.

const GIB = 1024 ** 3;
const NOT_LIVE = 'not connected: connect first, or the live connection is closed';
const SHAPE = 'the request body is not the JSON this route takes';
const GONE = 'that question is no longer open: it was answered, cancelled or expired';
const CANCELLED = 'Cancelled: nothing was sent.';
const PERMS = ['SCAN', 'TYPE · TTL', 'MEMORY USAGE', 'OBJECT FREQ', 'INFO', 'MEMORY STATS', 'SLOWLOG', 'CLIENT LIST', 'XINFO', 'PUBSUB', 'FT._LIST', 'ACL DRYRUN'];
const noperm = (cmd) => `NOPERM ${cmd} is not allowed for this user`;

/** A sample Pub/Sub message for a channel, from the mockup. */
export function sampleMessage(channel, n) {
  if (channel === 'orders.created') return `{"order":"A-${20417 + n}","items":${1 + (n % 4)},"total":${(19.99 * (1 + (n % 4))).toFixed(2)}}`;
  if (channel === 'orders.shipped') return `{"order":"A-${20391 + n}","carrier":"courier"}`;
  if (channel === 'cache.invalidate') return `cache:product:${44812 + (n % 7)}`;
  if (channel === 'presence.eu') return `{"user":${10293 + ((n * 17) % 900)},"state":"online"}`;
  return `rl:203.0.113.${7 + (n % 80)}:login`;
}

const MONITOR_COMMANDS = [
  'GET session:c41f0e9a-2b7d-4c1e-9a53-0f6d1e8b7a42',
  'HGETALL user:10293:cart',
  'INCR rl:203.0.113.7:login',
  'ZADD user:88120:feed 1794720000 post:88213',
  'EXPIRE cache:search:1b7d44f2 3600',
  'HGET cache:product:44812 price',
  'LPUSH queue:emails {"template":"order-shipped"}',
];

const CHANNELS = [['orders.created', 12], ['orders.shipped', 8], ['cache.invalidate', 31], ['presence.eu', 4], ['__keyevent@0__:expired', 2]];

const COMMAND_NAMES = [
  'acl', 'acl|cat', 'acl|getuser', 'acl|list', 'acl|setuser', 'acl|whoami', 'client', 'client|info', 'client|list', 'cluster', 'cluster|info',
  'cluster|nodes', 'cluster|shards', 'command', 'command|docs', 'command|info', 'config', 'config|get', 'config|set', 'dbsize', 'del', 'exists',
  'expire', 'flushdb', 'get', 'hget', 'hgetall', 'hset', 'incr', 'info', 'keys', 'lpush', 'lrange', 'memory', 'memory|stats', 'memory|usage',
  'object', 'object|encoding', 'object|freq', 'ping', 'pttl', 'publish', 'pubsub', 'pubsub|channels', 'pubsub|numsub', 'scan', 'set', 'slowlog',
  'slowlog|get', 'slowlog|len', 'ttl', 'type', 'unlink', 'zadd', 'zrange',
];

const help = (name, summary, complexity, args, subcommands = []) => ({ name, summary, since: '1.0.0', complexity, arguments: args, subcommands });
const HELP = {
  get: help('get', 'Returns the string value of a key.', 'O(1)', 'key'),
  set: help('set', 'Sets the string value of a key, ignoring its type. The key is created if it doesn’t exist.', 'O(1)', 'key value [NX | XX] [GET] [EX seconds | PX milliseconds | KEEPTTL]'),
  del: help('del', 'Deletes one or more keys.', 'O(N) where N is the number of keys that will be removed.', 'key [key ...]'),
  hgetall: help('hgetall', 'Returns all fields and values in a hash.', 'O(N) where N is the size of the hash.', 'key'),
  ping: help('ping', 'Returns the server’s liveliness response.', 'O(1)', '[message]'),
  dbsize: help('dbsize', 'Returns the number of keys in the currently-selected database.', 'O(1)', ''),
  ttl: help('ttl', 'Returns the expiration time in seconds of a key.', 'O(1)', 'key'),
  info: help('info', 'Returns information and statistics about the server.', 'O(1)', '[section [section ...]]'),
  keys: help('keys', 'Returns all key names that match a pattern.', 'O(N) with N being the number of keys in the database.', 'pattern'),
  publish: help('publish', 'Posts a message to a channel.', 'O(N+M) where N is the number of clients subscribed to the receiving channel.', 'channel message'),
  config: help('config', 'A container for server configuration commands.', 'Depends on subcommand.', '', [
    help('config|get', 'Returns the effective values of configuration parameters.', 'O(N) when N is the number of configuration parameters provided', 'parameter [parameter ...]'),
    help('config|set', 'Sets configuration parameters in-flight.', 'O(N) when N is the number of configuration parameters provided', 'parameter value [parameter value ...]'),
  ]),
};

function profile(name, env, endpoint, extra, sim) {
  return {
    name, env, endpoint, tls: false, db: 0, auth: 'keychain', read_only: env === 'prod', tags: [], provider: null, ssh: null, sentinel: null,
    sentinel_tls: false, token_command: false, standalone: false, prefer_replica: false, ...extra,
    sim: { flavor: 'redis', version: '7.4.1', mode: 'standalone', scale: 1, policy: 'allkeys-lfu', lfu: true, denied: {}, nodes: [], analysed: false, ...sim },
  };
}

function savedProfiles() {
  return [
    profile('prod-cache-01', 'prod', 'cache-01.prod.internal:6379', { tls: true, tags: ['cache', 'eu'] }, { analysed: true }),
    profile('staging-cache', 'stage', 'staging.internal:6379', { tags: ['cache'] }, {
      version: '7.2.5', scale: 0.08, policy: 'noeviction', lfu: false, denied: { 'MEMORY STATS': noperm('MEMORY STATS') }, configDenied: true,
    }),
    profile('demo-cluster', 'local', '127.0.0.1:7001', { auth: 'none' }, {
      flavor: 'valkey', version: '8.0.1', mode: 'cluster', scale: 0.3, policy: 'allkeys-lru', lfu: false,
      nodes: [['127.0.0.1:7001', 0, 5460], ['127.0.0.1:7002', 5461, 10922], ['127.0.0.1:7003', 10923, 16383]],
    }),
    profile('limited-user', 'stage', 'staging.internal:6379', {}, {
      version: '7.2.5', scale: 0.08,
      denied: {
        'MEMORY USAGE': noperm('MEMORY USAGE'), 'OBJECT FREQ': noperm('OBJECT FREQ'), 'MEMORY STATS': noperm('MEMORY STATS'), SLOWLOG: noperm('SLOWLOG LEN'),
        'CLIENT LIST': noperm('CLIENT LIST'), XINFO: noperm('XINFO'), PUBSUB: noperm('PUBSUB NUMPAT'),
      },
    }),
  ];
}

function containers() {
  return [
    { container: 'demo-redis', image: 'redis:8', endpoint: '127.0.0.1:6390', auth: 'password', sim: { flavor: 'redis', version: '8.0.2', scale: 0.002, policy: 'noeviction', lfu: false } },
    { container: 'demo-valkey', image: 'valkey/valkey:8', endpoint: '127.0.0.1:6391', auth: 'none', sim: { flavor: 'valkey', version: '8.0.1', scale: 0.002, policy: 'noeviction', lfu: false } },
    { container: 'legacy-redis', image: 'redis:6.2', endpoint: '127.0.0.1:6392', auth: 'required', sim: { flavor: 'redis', version: '6.2.14', scale: 0.002, policy: 'noeviction', lfu: false } },
  ];
}

/** The address a connection string points at, with any user and password left out. */
export function maskedAddress(url) {
  const s = String(url ?? '').trim();
  const uri = /^rediss?:\/\/(.*)$/i.exec(s);
  let hostPort;
  if (uri) {
    const authority = uri[1].split(/[/?#]/)[0];
    hostPort = authority.slice(authority.lastIndexOf('@') + 1);
  } else {
    hostPort = s.split(',')[0].trim();
  }
  if (!hostPort) return '';
  return /:\d+$/.test(hostPort) ? hostPort : `${hostPort}:6379`;
}

const ADD_FIELDS = ['name', 'url', 'env', 'password', 'tags', 'writes', 'provider', 'ssh', 'ssh_identity', 'ssh_known_hosts', 'sentinel', 'sentinel_master',
  'sentinel_tls', 'sentinel_user', 'sentinel_password', 'token_command', 'entra', 'standalone', 'prefer_replica', 'force'];

export function createMockServer({ entra = false } = {}) {
  const saved = savedProfiles();
  const docker = containers();
  const sources = new Set();
  const confirms = new Map();
  let confirmSeq = 0;
  let session = null;
  let connSeq = 0;
  let idSeq = 0;
  let tick = 0;
  let timer = null;
  let lastOps = null;
  const polling = { paused: false, interval_secs: 2, intervals: [1, 2, 3, 5, 10, 15, 30, 60] };
  const limits = { max_lines: 10000, max_secs: 30 };

  const features = { entra_id: entra, version: '0.1.0' };
  const later = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const label = (e) => e.toUpperCase();
  const unix = () => Math.floor(Date.now() / 1000);

  function connection(p) {
    const scheme = p.tls ? 'rediss' : 'redis';
    return {
      auth: p.auth, db: p.db, endpoint: p.endpoint, env: p.env, env_label: label(p.env), name: p.name, prefer_replica: p.prefer_replica,
      provider: p.provider, provider_notice: null, read_only: p.read_only, sentinel: p.sentinel, sentinel_tls: p.sentinel_tls, ssh: p.ssh,
      standalone: p.standalone, tags: p.tags, tls: p.tls, token_command: p.token_command, url: `${scheme}://${p.endpoint}/${p.db}`, writes: !p.read_only,
    };
  }

  const nodesOf = (p) => p.sim.nodes.map(([endpoint, a, b], index) => ({ index, id: `${'0123456789abcdef'[index + 10]}`.repeat(40), endpoint, role: 'primary', slots: [[a, b]], slot_count: b - a + 1, connected: true }));

  function server(p) {
    const s = p.sim;
    return {
      flavor: s.flavor, version: s.version, mode: s.mode, used_memory: Math.round(15.02 * GIB * s.scale),
      maxmemory: s.policy === 'noeviction' && s.scale < 0.01 ? 0 : Math.round(24 * GIB * s.scale), policy: s.policy, lfu: s.lfu, keys: Math.round(7920114 * s.scale),
    };
  }

  function state() {
    const s = session;
    if (!s) {
      return {
        connected_at_unix: null, connection: null, docker: false, error: null, features, live: 'waiting', live_error: null, monitor: null,
        monitor_limits: limits, nodes: [], picked: 0, polling,
        refused: [], sentinel: null, server: null, status: 'idle', tail: null, target: null, writes: false, writes_off: null,
      };
    }
    const p = s.profile;
    const up = s.status === 'connected';
    return {
      connected_at_unix: s.connectedAt, connection: connection(p), docker: s.docker, error: s.error, features, live: s.live, live_error: s.liveError,
      monitor: s.monitor ? { run: s.monitor.run, node: s.picked, started_at_unix_ms: s.monitor.startedAt, max_lines: 10000, max_secs: 30 } : null,
      monitor_limits: limits, nodes: up ? nodesOf(p) : [], picked: s.picked, polling, refused: s.refused, sentinel: null, server: up ? server(p) : null, status: s.status,
      tail: s.tail ? { id: s.tail.id, kind: s.tail.kind, name: s.tail.name, node: s.picked, ready: s.tail.ready } : null,
      target: up ? (p.sim.mode === 'cluster' ? `cluster · ${p.sim.nodes.length} shards` : 'primary') : null,
      writes: !p.read_only, writes_off: p.read_only ? readOnly(p) : null,
    };
  }

  const readOnly = (p) => `Not sent: ${p.name} is read-only. Turn writes on in Connections.`;
  const live = () => session && session.status === 'connected' && session.live === 'ready';

  // ---------- events ----------

  function emit(name, data) {
    for (const s of Array.from(sources)) s.deliver(name, data);
  }
  const emitState = () => emit('state', state());

  function info(p, node) {
    const s = p.sim;
    const share = s.nodes.length ? 1 / s.nodes.length : 1;
    const ops = Math.max(1, Math.round((12400 + (Math.random() - 0.5) * 3600) * s.scale * share + node * 40));
    return {
      aof_ok: true, aof_on: false, blocked: 0, clients: Math.max(1, Math.round((214 + (Math.random() - 0.5) * 18) * Math.max(s.scale, 0.02) * share)),
      commands: 166000 + tick * ops, evicted: s.policy === 'noeviction' ? 0 : tick * 3, expired: tick * 120, fragmentation: 1.07, hits: 15000 + tick * 97, keys: Math.round(7920114 * s.scale * share) + node * 1200,
      link_up: null, maxmemory: server(p).maxmemory, misses: 400 + tick * 3, ops_now: ops, policy: s.policy, rdb_ok: true, replicas: s.scale === 1 ? 2 : 0, role: 'primary',
      uptime_secs: Math.round(1052400 * Math.max(s.scale, 0.05)) + tick, used_memory: Math.round(server(p).used_memory * share) + node * 40 * 1024 * 1024,
    };
  }

  function onTick() {
    tick += 1;
    const s = session;
    // The live figures: at the polling interval, and not at all while paused.
    if (live() && !polling.paused && tick % polling.interval_secs === 0) {
      const p = s.profile;
      const i = info(p, s.picked);
      const data = {
        node: s.picked,
        info: i,
        rates: {
          blocked: 0, clients: i.clients, cpu_sys: lastOps === null ? null : 0.11, cpu_user: lastOps === null ? null : 0.72, evicted: lastOps === null ? null : (p.sim.policy === 'noeviction' ? 0 : 3 * p.sim.scale),
          expired: lastOps === null ? null : 120 * p.sim.scale, fragmentation: 1.07, hit_rate: lastOps === null ? null : 0.97, net_in: lastOps === null ? null : 7400 * p.sim.scale, net_out: lastOps === null ? null : 572000 * p.sim.scale,
          ops: lastOps === null ? null : i.ops_now + (Math.random() - 0.5) * 0.02 * i.ops_now, used_memory: i.used_memory,
        },
      };
      if (p.sim.nodes.length && (tick % 5 === 0 || lastOps === null || polling.interval_secs >= 5)) data.shards = p.sim.nodes.map((_, n) => ({ node: n, info: info(p, n), failed: false }));
      lastOps = i.ops_now;
      emit('figures', data);
    }
    if (s && s.monitor && s.monitor.startedAt) {
      const now = Date.now();
      if (now - s.monitor.startedAt >= 30000) closeMonitor('time_up');
      else {
        const lines = [0, 1, 2].map((n) => ({
          client: `10.0.4.${11 + (((tick + n) * 7) % 40)}:${40000 + ((tick * 13 + n * 7) % 20000)}`, cut: 0, db: '0',
          line: MONITOR_COMMANDS[(tick * 3 + n) % MONITOR_COMMANDS.length], millis: now % 1000, unix: Math.floor(now / 1000),
        }));
        emit('monitor', { kind: 'lines', lines, run: s.monitor.run });
      }
    }
    if (s && s.tail && s.tail.ready) {
      const names = s.tail.matches;
      const messages = [];
      for (let n = 0; n < (s.tail.kind === 'pattern' ? 2 : 1); n += 1) {
        const channel = names[(tick + n) % names.length];
        const preview = sampleMessage(channel, tick + n);
        messages.push({ at_unix_ms: Date.now(), bytes: preview.length, channel, pattern: s.tail.kind === 'pattern' ? s.tail.name : null, preview });
      }
      emit('tail', { dropped: 0, kind: 'messages', messages, tail: s.tail.id });
    }
  }

  function closeMonitor(end, error = null) {
    if (!session || !session.monitor) return false;
    const run = session.monitor.run;
    session.monitor = null;
    emit('monitor', { end, error, kind: 'closed', run });
    emitState();
    return true;
  }

  function closeTail(error = null) {
    if (!session || !session.tail) return false;
    const id = session.tail.id;
    session.tail = null;
    emit('tail', { error, kind: 'closed', tail: id });
    emitState();
    return true;
  }

  class MockEventSource {
    constructor() {
      this.readyState = 0;
      this.listeners = {};
      this.onopen = null;
      this.onerror = null;
      sources.add(this);
      if (!timer) timer = setInterval(onTick, 1000);
      setTimeout(() => {
        if (this.readyState === 2) return;
        this.readyState = 1;
        if (this.onopen) this.onopen();
        this.deliver('state', state());
      }, 0);
    }

    addEventListener(name, fn) {
      this.listeners[name] = fn;
    }

    deliver(name, data) {
      if (this.readyState === 1 && this.listeners[name]) this.listeners[name]({ data: JSON.stringify(data) });
    }

    close() {
      this.readyState = 2;
      sources.delete(this);
      if (!sources.size && timer) {
        clearInterval(timer);
        timer = null;
      }
    }
  }

  // ---------- confirms ----------

  function ask(question, run) {
    confirmSeq += 1;
    const id = `${String(confirmSeq).padStart(4, '0')}${'0'.repeat(28)}`;
    confirms.set(id, { question, run });
    return [409, { confirm: { expect: question.expect ?? null, id, kind: question.kind, text: question.text } }];
  }

  // ---------- routes ----------

  const known = (body, fields) => Object.keys(body).every((k) => fields.includes(k));
  const find = (name) => saved.find((p) => p.name === name) || null;

  function perms(p) {
    return PERMS.map((name) => {
      if (p.sim.denied[name]) return { label: name, reason: p.sim.denied[name], status: 'denied' };
      if (name === 'FT._LIST') return { label: name, reason: 'the Search module is not loaded', status: 'skipped' };
      if (name === 'ACL DRYRUN') return { label: name, reason: 'checked by the server on use', status: 'skipped' };
      if (name === 'OBJECT FREQ' && !p.sim.lfu) return { label: name, reason: 'the maxmemory policy is not LFU, so there are no hot keys', status: 'skipped' };
      return { label: name, reason: null, status: 'allowed' };
    });
  }

  function figures(p, share = 1) {
    const refused = !!p.sim.denied['MEMORY STATS'];
    const sv = server(p);
    const gaps = ['This server has no INFO keysizes (Redis 8 and later have it), so there is no exact size histogram'];
    if (refused) gaps.push('MEMORY STATS is denied for this user (ACL +memory|stats), so dataset.bytes is missing');
    return {
      big_keys: null, dataset_bytes: refused ? null : Math.round(13 * GIB * p.sim.scale * share), gaps, keys: Math.round(sv.keys * share),
      overhead_bytes: refused ? null : Math.round(2.1 * GIB * p.sim.scale * share), server: { ...sv, used_memory: Math.round(sv.used_memory * share), keys: Math.round(sv.keys * share) },
      ttl_share: 0.31, with_ttl: Math.round(sv.keys * share * 0.31),
    };
  }

  function overview() {
    const p = session.profile;
    const up = session.status === 'connected';
    const nodes = up ? nodesOf(p) : [];
    const findings = [];
    if (up && p.sim.scale === 1) {
      findings.push(
        { area: 'Server', panel: 'Slow log', screen: 'slow_log', severity: 'note', text: 'LRANGE took 912 ms at 13:58 UTC.' },
        { area: 'Server', panel: 'Clients', screen: 'clients', severity: 'warn', text: '38 connections from 10.0.9.17 idle for over an hour.' },
      );
    }
    return {
      figures: up ? figures(p) : null, figures_failed: null, findings,
      last_analysis: up && p.sim.analysed ? { taken_at_unix: unix() - 3 * 3600, sampled_keys: 100000 } : null,
      live: null, perms: up ? perms(p) : [], provider: null, server: up ? server(p) : null,
      shards: nodes.map((node) => ({ node, figures: figures(p, 1 / nodes.length), note: null, live: null, failed: false })),
      status: session.status, target: state().target,
    };
  }

  function clients() {
    const p = session.profile;
    const n = (v) => Math.max(1, Math.round(v * Math.max(p.sim.scale, 0.03)));
    const libs = [['redis-py 5.0.8', 96, 'get'], ['node-redis 4.7.0', 71, 'hgetall'], ['go-redis 9.6.1', 38, 'evalsha'], ['(no name)', 7, 'ping'], ['redis-cli', 2, 'client|list']];
    const connections = [];
    let idle = n(38);
    libs.forEach(([lib, count, cmd], li) => {
      for (let i = 0; i < n(count); i += 1) {
        const stale = p.sim.scale === 1 && li === 0 && idle > 0;
        if (stale) idle -= 1;
        connections.push({
          id: 100 + connections.length, addr: stale ? `10.0.9.17:${42000 + i}` : `10.0.4.${11 + (i % 40)}:${51000 + connections.length}`, name: lib === 'redis-cli' ? '' : `app-${li}`,
          lib, age: 86400 * (12 - li * 2) - i * 60, idle: stale ? 5400 + i * 20 : 40 + li * 310 + (i % 7), cmd, mem: 22800, db: 0, own: false,
        });
      }
    });
    connections.push({ id: 100 + connections.length, addr: '127.0.0.1:53211', name: '', lib: 'redis-rs 1.8.1', age: 60 + tick, idle: 0, cmd: 'client|list', mem: 23450, db: 0, own: true });
    const group = (kind, label_, members) => {
      const own = members.map((m) => connections[m]);
      const count = {};
      for (const c of own) count[c.cmd] = (count[c.cmd] || 0) + 1;
      const idleConns = own.filter((c) => c.idle >= 3600).length;
      return {
        connections: own.length, flagged: idleConns > 0, idle_conns: idleConns, kind, label: label_, longest_idle: Math.max(...own.map((c) => c.idle)),
        mem: own.reduce((sum, c) => sum + c.mem, 0), members, oldest_age: Math.max(...own.map((c) => c.age)), top_cmd: Object.keys(count).sort((a, b) => count[b] - count[a])[0],
      };
    };
    const by = (key) => {
      const map = new Map();
      connections.forEach((c, i) => {
        const k = key(c);
        if (!map.has(k)) map.set(k, []);
        map.get(k).push(i);
      });
      return Array.from(map.entries()).sort((a, b) => b[1].length - a[1].length);
    };
    const groups = by((c) => c.lib).map(([k, m]) => group('lib', k, m)).concat(by((c) => `${c.addr.split('.').slice(0, 3).join('.')}.0/24`).map(([k, m]) => group('addr', k, m)));
    return { connections, groups, node: session.picked };
  }

  function slowlog() {
    const day = Math.floor(unix() / 86400) * 86400;
    const at = (h, m, s) => day + h * 3600 + m * 60 + s;
    const entry = (id, when, micros, command, line, keys, client, note) => ({ args_cut: 0, client, client_name: '', command, id, keys, line, micros, more_keys: 0, note, unix: when });
    return {
      entries: [
        entry(41, at(13, 58, 41), 912000, 'LRANGE', 'LRANGE queue:emails 0 -1', ['queue:emails'], '10.0.4.17:51122', 'Reads a whole list. That it was slow is the hint that the key is big.'),
        entry(40, at(13, 41, 7), 184000, 'ZRANGE', 'ZRANGE user:88120:feed 0 -1 WITHSCORES', ['user:88120:feed'], '10.0.4.23:40718', 'Reads a whole sorted set.'),
        entry(39, at(12, 20, 15), 96000, 'HGETALL', 'HGETALL cache:product:51007', ['cache:product:51007'], '10.0.4.31:38806', 'Reads a whole hash.'),
        entry(38, at(11, 2, 33), 41000, 'SCAN', 'SCAN 0 MATCH tmp:import:* COUNT 10000', [], '10.0.4.17:51122', null),
      ],
      // Like the real server, the reason the settings are missing comes in every reply.
      keys_off: null, node: session.picked,
      settings: session.profile.sim.configDenied ? null : { max_len: 128, threshold_micros: 10000 },
      settings_off: session.profile.sim.configDenied ? 'the server refused CONFIG GET for this user' : null,
    };
  }

  function channels() {
    const p = session.profile;
    const list = CHANNELS.map(([name, subscribers]) => ({ name, subscribers: Math.max(1, Math.round(subscribers * Math.max(p.sim.scale, 0.2))) }));
    const cluster = p.sim.nodes.length > 0;
    const nodes = cluster
      ? p.sim.nodes.map((_, node) => ({ channels: node === 1 ? [{ name: 'shard.orders', subscribers: 3 }] : node === 2 ? [{ name: 'shard.presence', subscribers: 1 }] : [], node, notes: [], total_channels: node ? 1 : 0 }))
      : [{ channels: [], node: 0, notes: [], total_channels: 0 }];
    return { channels: list, node: session.picked, notes: [], patterns: 2, shards: { nodes, off: null }, total_channels: list.length };
  }

  function globMatches(pattern) {
    const re = new RegExp(`^${pattern.replace(/[.+^$()|{}\\]/g, '\\$&').replace(/\*/g, '.*').replace(/\?/g, '.')}$`);
    const names = CHANNELS.map((c) => c[0]).filter((n) => {
      try {
        return re.test(n);
      } catch {
        return false;
      }
    });
    return names.length ? names : [pattern.replace(/[*?[\]]/g, '') || 'events'];
  }

  // ---------- the command line ----------

  const one = (text, table = text, json = text, raw = text) => ({ cut: false, error: false, json, raw, table, text, took_ms: 0.18 + ((tick * 37) % 400) / 1000 });
  const line = (shown, status, text, replies = [], times = 1) => [200, { line: shown, node: null, replies, route_note: null, sent: replies.length, status, text, times }];
  const WRITES = ['SET', 'DEL', 'UNLINK', 'EXPIRE', 'HSET', 'LPUSH', 'FLUSHALL', 'FLUSHDB', 'INCR', 'PUBLISH'];
  const DESTRUCTIVE = ['DEL', 'UNLINK', 'FLUSHALL', 'FLUSHDB'];

  function command(text) {
    const p = session.profile;
    const upper = text.toUpperCase();
    const words = text.split(/\s+/);
    const word = upper.split(/\s+/)[0];
    const prod = p.env === 'prod';
    if (upper === 'MOCK LOST') {
      setTimeout(() => {
        if (!session) return;
        session.live = 'closed';
        session.liveError = 'the connection to the server was lost';
        session.tail = null;
        session.monitor = null;
        emit('notice', { text: 'The live connection closed: the connection to the server was lost.' });
        emitState();
      }, 200);
      return line(text, 'note', 'The sample data now loses its connection to the server.');
    }
    if (upper === 'MOCK LINK DOWN') {
      setTimeout(() => {
        for (const s of Array.from(sources)) {
          s.readyState = 0;
          if (s.onerror) s.onerror();
        }
      }, 200);
      return line(text, 'note', 'The sample data now loses its line to redis-lens.');
    }
    if (p.sim.denied.SLOWLOG) return line(text, 'failed', 'the server refused COMMAND INFO for this user');
    if (upper === 'PING') return line(text, 'ok', null, [one(['PONG'], ['PONG'], ['"PONG"'], ['[simple string] +PONG'])]);
    if (upper === 'DBSIZE') {
      const n = server(p).keys;
      return line(text, 'ok', null, [one([`(integer) ${n}`], [`(integer) ${n}`], [String(n)], [`[integer] :${n}`])]);
    }
    if (upper === 'TTL USER:10293:CART') return line(text, 'ok', null, [one(['(integer) 532800'], ['(integer) 532800'], ['532800'], ['[integer] :532800'])]);
    if (upper === 'GET RL:203.0.113.7:LOGIN') return line(text, 'ok', null, [one(['"17"'], ['"17"'], ['"17"'], ['[bulk string] $2 "17"'])]);
    if (/^\d+ GET RL:203\.0\.113\.7:LOGIN$/.test(upper)) {
      const times = Math.min(100, Number(words[0]));
      return line(text, 'ok', null, Array.from({ length: times }, () => one(['"17"'], ['"17"'], ['"17"'], ['[bulk string] $2 "17"'])), times);
    }
    if (upper === 'GET') return line(text, 'ok', null, [{ ...one(["(error) ERR wrong number of arguments for 'get' command"]), error: true }]);
    if (upper === 'HGETALL USER:10293:CART') {
      const pairs = [['sku:44812', '{"qty":2,"price":19.99}'], ['sku:44813', '{"qty":1,"price":4.50}'], ['coupon', 'WINTER10']];
      const flat = pairs.flat();
      return line(text, 'ok', null, [one(
        flat.map((v, i) => `${i + 1}) ${JSON.stringify(v)}`),
        ['#  field      value'].concat(pairs.map(([f, v], i) => `${i + 1}  ${f.padEnd(9)}  ${v}`)),
        JSON.stringify(Object.fromEntries(pairs), null, 2).split('\n'),
        [`[array] *${flat.length}`].concat(flat.map((v) => `  [bulk string] $${v.length} ${JSON.stringify(v)}`)),
      )]);
    }
    if (word === 'KEYS') {
      const keys = ['user:10293:cart', 'user:28648:cart', 'user:51120:cart'];
      const done = () => line(text, 'ok', null, [one(
        keys.map((k, i) => `${i + 1}) ${JSON.stringify(k)}`), ['#  value'].concat(keys.map((k, i) => `${i + 1}  ${k}`)),
        JSON.stringify(keys, null, 2).split('\n'), [`[array] *${keys.length}`].concat(keys.map((k) => `  [bulk string] $${k.length} ${JSON.stringify(k)}`)),
      )]);
      return prod
        ? ask({ kind: 'type', expect: p.name, text: `KEYS is @dangerous: it can slow the server down. Type the profile name ${p.name} to send it: ${text}` }, done)
        : ask({ kind: 'ask', text: `KEYS is @dangerous: it can slow the server down. Send it? ${text}` }, done);
    }
    if (['SUBSCRIBE', 'PSUBSCRIBE', 'SSUBSCRIBE', 'UNSUBSCRIBE', 'PUNSUBSCRIBE', 'SUNSUBSCRIBE'].includes(word)) return line(text, 'note', 'Not sent here: use the Pub/Sub screen to subscribe.');
    if (word === 'MONITOR') return line(text, 'note', 'Not sent here: use Monitor on the Server screen.');
    if (word === 'SELECT') return line(text, 'note', 'Not sent here: SELECT would switch the database under every view.');
    if (['AUTH', 'HELLO'].includes(word)) return line(`${words[0]}${' ***'.repeat(Math.max(1, words.length - 1))}`, 'note', 'Not sent: it would change the connection every view shares.');
    if (['RESET', 'QUIT', 'MULTI', 'EXEC', 'DISCARD', 'WATCH', 'UNWATCH'].includes(word)) return line(text, 'note', 'Not sent: it would change the connection every view shares.');
    if (['EVAL', 'EVAL_RO', 'EVALSHA', 'EVALSHA_RO', 'FCALL', 'FCALL_RO', 'SCRIPT', 'FUNCTION'].includes(word)) return line(text, 'note', `Not sent: ${word} runs a script, whose commands the command line can't check.`);
    if (WRITES.includes(word)) {
      if (p.read_only) return line(text, 'note', readOnly(p));
      const destructive = DESTRUCTIVE.includes(word);
      const ok = word === 'SET' || word.startsWith('FLUSH');
      const done = () => line(text, 'ok', null, [ok ? one(['OK'], ['OK'], ['"OK"'], ['[simple string] +OK']) : one(['(integer) 1'], ['(integer) 1'], ['1'], ['[integer] :1'])]);
      // The table in spec.md "Write permissions": production asks about every write, and
      // wants a name typed for one that deletes.
      if (prod && destructive) {
        const expect = words.length === 2 ? words[1] : p.name;
        return ask({ kind: 'type', expect, text: `${word} can't be undone. Type ${expect} to send it to ${p.name}: ${text}` }, done);
      }
      if (prod || destructive) return ask({ kind: 'ask', text: `Send ${text} to ${p.name}?` }, done);
      return done();
    }
    return line(text, 'note', `Not sent: the server has no command ${word}.`);
  }

  // ---------- the router ----------

  async function connect(target) {
    if (!known(target, ['name', 'docker']) || (!!target.name === !!target.docker)) return [400, { error: 'give one of name and docker' }];
    let p;
    let locked = false;
    if (target.name) p = find(target.name);
    else {
      const c = docker.find((d) => d.container === target.docker);
      if (c) p = profile(c.container, 'local', c.endpoint, { auth: 'none', read_only: false }, c.sim);
      locked = !!c && c.auth === 'required';
    }
    if (!p) return [404, { error: 'there is no saved connection by that name' }];
    connSeq += 1;
    const mine = connSeq;
    confirms.clear();
    lastOps = null;
    session = { profile: p, docker: !!target.docker, status: 'connecting', error: null, live: 'waiting', liveError: null, connectedAt: null, picked: 0, refused: [], tail: null, monitor: null };
    emitState();
    later(600).then(() => {
      if (!session || connSeq !== mine) return;
      if (p.endpoint.endsWith(':6399')) {
        session.status = 'failed';
        session.error = 'Connection refused (os error 61)';
        emitState();
        return;
      }
      if (locked) {
        // A connect that opens but can't authenticate is failed, never connected.
        session.status = 'failed';
        session.live = 'closed';
        session.error = 'the server asks for a password, and this connection has none: add the connection again with its password';
        emitState();
        return;
      }
      session.status = 'connected';
      session.connectedAt = unix();
      emitState();
      later(150).then(() => {
        if (!session || connSeq !== mine) return;
        session.live = 'ready';
        emitState();
      });
    });
    return [202, state()];
  }

  function add(body) {
    if (!known(body, ADD_FIELDS) || typeof body.name !== 'string' || typeof body.url !== 'string') return [400, { error: SHAPE }];
    const step = (tokenOk, writesOk) => {
      const name = body.name.trim();
      if (!name) return [400, { error: 'a profile needs a name' }];
      if (/^docker:/i.test(name)) return [400, { error: 'profile names starting with docker: are reserved for discovered containers' }];
      if (find(name) && !body.force) return [400, { error: `a profile named ${name} already exists: tick Replace to overwrite it` }];
      const endpoint = maskedAddress(body.url);
      if (!endpoint) return [400, { error: 'the connection string could not be read: missing host' }];
      const tokens = Array.isArray(body.token_command) ? body.token_command : [];
      if (tokens.length && body.password) return [400, { error: 'a token command and a password can’t be used together' }];
      if (body.entra && !features.entra_id) return [400, { error: 'this build has no Entra ID sign-in (the entra-id feature is off)' }];
      const e = body.env || (/prod/i.test(endpoint) ? 'prod' : /^(127\.|localhost)/.test(endpoint) ? 'local' : 'other');
      if (!['prod', 'stage', 'local', 'other'].includes(e)) return [400, { error: SHAPE }];
      if (tokens.length && !tokenOk) {
        return ask({ kind: 'ask', text: `This profile will run this command at every connect and send what it prints as the password: ${tokens.join(' ')}. It is run directly, not through a shell. Save it?` }, () => step(true, writesOk));
      }
      if (e === 'prod' && body.writes === true && !writesOk) {
        return ask({ kind: 'type', expect: name, text: `${name} is a prod profile. Type its name to turn writes on.` }, () => step(tokenOk, true));
      }
      // A password is never kept: only where it would live is noted.
      const hasSecret = typeof body.password === 'string' && body.password.length > 0;
      const user = /^rediss?:\/\/[^/@]+@/i.test(body.url);
      const sentinels = typeof body.sentinel === 'string' && body.sentinel ? body.sentinel.split(',') : [];
      const at = saved.findIndex((x) => x.name === name);
      const p = profile(name, e, endpoint, {
        tls: /^rediss:/i.test(body.url) || /ssl\s*=\s*true/i.test(body.url), tags: Array.isArray(body.tags) ? body.tags : [],
        auth: body.entra ? 'entra-id' : tokens.length ? 'command' : hasSecret || user ? 'keychain' : 'none',
        read_only: e === 'prod' ? body.writes !== true : body.writes === false, provider: body.provider || null,
        ssh: body.ssh ? String(body.ssh).slice(String(body.ssh).lastIndexOf('@') + 1) : null,
        sentinel: sentinels.length ? `${body.sentinel_master} via ${sentinels[0].trim()}${sentinels.length > 1 ? ` (+${sentinels.length - 1})` : ''}` : null,
        sentinel_tls: body.sentinel_tls === true, token_command: tokens.length > 0, standalone: body.standalone === true, prefer_replica: body.prefer_replica === true,
      }, { scale: 0.01, policy: 'noeviction', lfu: false });
      if (at >= 0) saved[at] = p;
      else saved.push(p);
      const kept = p.auth === 'keychain' ? ' Credentials are in the OS keychain.' : '';
      return [201, { connection: connection(p), message: `Saved ${name} (${endpoint}, ${label(e)}).${kept}` }];
    };
    return step(false, false);
  }

  function set(name, body) {
    if (!known(body, ['writes', 'tag', 'untag', 'standalone'])) return [400, { error: SHAPE }];
    const p = find(name);
    if (!p) return [404, { error: 'there is no saved connection by that name' }];
    const apply = () => {
      if (body.writes !== undefined) p.read_only = !body.writes;
      for (const t of body.tag || []) if (!p.tags.includes(t)) p.tags.push(t);
      p.tags = p.tags.filter((t) => !(body.untag || []).includes(t));
      if (body.standalone !== undefined) p.standalone = body.standalone === true;
      if (session && session.profile === p) emitState();
      let message = `Saved ${name}.`;
      if (body.writes === true) message = `Writes are on for ${name}. Each write still asks as the table in the docs says.`;
      else if (body.writes === false) message = `Writes are off for ${name}: it is read-only.`;
      return [200, { connection: connection(p), message }];
    };
    if (body.writes === true && p.read_only) {
      return p.env === 'prod'
        ? ask({ kind: 'type', expect: p.name, text: `${p.name} is a prod profile. Type its name to turn writes on.`, mismatch: [400, `writes stay off: the name given does not match ${p.name}`] }, apply)
        : ask({ kind: 'ask', text: `Turn writes on for ${p.name}? Each write still asks before it is sent.` }, apply);
    }
    return apply();
  }

  async function route(method, fullPath, body) {
    const [path, query = ''] = fullPath.split('?');
    const parts = path.split('/').filter(Boolean).map(decodeURIComponent);
    const key = `${method} /${parts.slice(0, 3).join('/')}`;
    if (parts[0] !== 'api') return [404, { error: 'not found' }];

    if (path === '/api/state' && method === 'GET') return [200, state()];
    if (parts[1] === 'connections') {
      if (parts.length === 2 && method === 'GET') return [200, { connected: session && !session.docker && session.status !== 'failed' ? session.profile.name : null, connections: saved.map(connection) }];
      if (parts.length === 2 && method === 'POST') return add(body);
      if (parts.length === 3 && method === 'PATCH') return set(parts[2], body);
      if (parts.length === 3 && method === 'DELETE') {
        const at = saved.findIndex((p) => p.name === parts[2]);
        if (at < 0) return [404, { error: 'there is no saved connection by that name' }];
        const [gone] = saved.splice(at, 1);
        if (session && session.profile === gone) {
          connSeq += 1;
          session = null;
          confirms.clear();
          emitState();
        }
        return [200, { message: `Removed ${parts[2]}.` }];
      }
      if (parts.length === 4 && parts[3] === 'test' && method === 'POST') {
        const p = find(parts[2]);
        if (!p) return [404, { error: 'there is no saved connection by that name' }];
        await later(500);
        if (p.endpoint.endsWith(':6399')) return [502, { error: 'Connection refused (os error 61)' }];
        return [200, { at: p.endpoint, checks: perms(p), connected: true, connection: connection(p), sentinel: null }];
      }
    }
    if (key === 'GET /api/docker') {
      await later(300);
      return [200, { instances: docker.map((d) => ({ auth: d.auth, container: d.container, endpoint: d.endpoint, image: d.image, url: `redis://${d.endpoint}/0` })) }];
    }
    if (key === 'POST /api/connect') return connect(body);
    if (key === 'POST /api/disconnect') {
      session = null;
      confirms.clear();
      emitState();
      return [200, state()];
    }
    if (parts[1] === 'confirm' && parts.length === 3) {
      const held = confirms.get(parts[2]);
      confirms.delete(parts[2]);
      if (!held) return [410, { error: GONE }];
      if (method === 'DELETE') return [200, { cancelled: true, text: CANCELLED }];
      if (held.question.kind === 'type' && body.typed !== held.question.expect) {
        const [status, error] = held.question.mismatch || [422, 'Not sent: the name typed did not match.'];
        return [status, { error }];
      }
      return held.run();
    }
    if (key === 'POST /api/node') {
      if (!known(body, ['index'])) return [400, { error: SHAPE }];
      if (!live() || !Number.isInteger(body.index) || body.index < 0 || body.index >= session.profile.sim.nodes.length) return [422, { error: 'there is no such node' }];
      session.picked = body.index;
      lastOps = null;
      emitState();
      return [200, state()];
    }
    if (key === 'GET /api/overview') return session ? [200, overview()] : [409, { error: NOT_LIVE }];
    if (key === 'POST /api/polling') {
      if (!known(body, ['paused', 'interval_secs'])) return [400, { error: SHAPE }];
      if (body.interval_secs !== undefined && !polling.intervals.includes(body.interval_secs)) return [422, { error: 'the interval must be one of 1, 2, 3, 5, 10, 15, 30 or 60 seconds' }];
      if (body.paused !== undefined) polling.paused = body.paused === true;
      if (body.interval_secs !== undefined) polling.interval_secs = body.interval_secs;
      emitState();
      return [200, polling];
    }
    if (path === '/api/session/writes' && method === 'POST') {
      if (!known(body, ['on']) || typeof body.on !== 'boolean') return [400, { error: SHAPE }];
      if (!session) return [409, { error: NOT_LIVE }];
      const sp = session.profile;
      if (!session.docker) return set(sp.name, { writes: body.on });
      const apply = () => {
        sp.read_only = !body.on;
        emitState();
        return [200, { message: `Writes are ${body.on ? 'on' : 'off'} for ${sp.name} in this session only (a container has no saved profile).`, state: state() }];
      };
      if (body.on && sp.read_only) return ask({ kind: 'ask', text: `Turn writes on for ${sp.name} in this session? Each write still asks as the write table says.` }, apply);
      return apply();
    }

    // Everything below reads the live connection.
    if (!live()) return [409, { error: NOT_LIVE }];
    const p = session.profile;
    if (key === 'GET /api/server/clients') return [200, p.sim.denied['CLIENT LIST'] ? { off: 'the server refused CLIENT LIST for this user' } : clients()];
    if (key === 'GET /api/server/slowlog') return [200, p.sim.denied.SLOWLOG ? { off: 'the server refused SLOWLOG for this user' } : slowlog()];
    if (key === 'POST /api/server/monitor') {
      if (session.monitor) return [422, { error: 'MONITOR is already running: stop it first.' }];
      // Monitor always asks first, read-only connections too; production types the name.
      const about = 'MONITOR shows every command the server runs, other users\' values included, and slows the server. It stops after 30 s or 10,000 lines.';
      const question = p.env === 'prod'
        ? { kind: 'type', expect: p.name, text: `${about} Type ${p.name} to start it on ${p.name}.`, mismatch: [422, 'MONITOR not started: the name typed did not match.'] }
        : { kind: 'ask', text: `${about} Start it on ${p.name}?` };
      return ask(question, () => startMonitor(p));
    }
    if (key === 'DELETE /api/server/monitor') return [200, { stopped: closeMonitor('stopped') }];
    if (key === 'GET /api/pubsub/channels') return [200, p.sim.denied.PUBSUB ? { off: 'the server refused PUBSUB CHANNELS for this user' } : channels()];
    if (key === 'POST /api/pubsub/subscribe') return subscribe(p, body);
    if (key === 'DELETE /api/pubsub/subscribe') return [200, { stopped: closeTail() }];
    if (key === 'POST /api/pubsub/publish') return publish(p, body);
    if (key === 'POST /api/cmdline') {
      if (typeof body.line !== 'string') return [400, { error: SHAPE }];
      return command(body.line.trim());
    }
    if (path === '/api/cmdline/commands' && method === 'GET') return [200, p.sim.denied.SLOWLOG ? { off: 'the server refused COMMAND LIST for this user' } : { names: COMMAND_NAMES }];
    if (path === '/api/cmdline/help' && method === 'GET') {
      const name = (new URLSearchParams(query).get('name') || '').toLowerCase();
      if (!name) return [400, { error: "give the command's name: ?name=GET" }];
      const [top, sub] = name.split('|');
      const found = sub ? ((HELP[top] || {}).subcommands || []).find((s) => s.name === name) : HELP[top];
      return found ? [200, found] : [404, { error: 'the server has no docs for that name' }];
    }
    return [404, { error: 'not found' }];
  }

  function startMonitor(p) {
    if (!live() || session.profile !== p) return [422, { error: NOT_LIVE }];
    idSeq += 1;
    const run = idSeq;
    session.monitor = { run, startedAt: null };
    emitState();
    later(250).then(() => {
      if (!session || !session.monitor || session.monitor.run !== run) return;
      if (p.sim.denied['CLIENT LIST']) {
        closeMonitor('failed', 'the server refused MONITOR for this user');
        return;
      }
      session.monitor.startedAt = Date.now();
      emitState();
      emit('monitor', { at_unix_ms: session.monitor.startedAt, kind: 'ready', max_lines: 10000, max_secs: 30, run });
    });
    return [202, { max_lines: 10000, max_secs: 30, run }];
  }

  function subscribe(p, body) {
    if (!known(body, ['name', 'kind']) || typeof body.name !== 'string') return [400, { error: SHAPE }];
    const name = body.name;
    const pattern = /[*?[]/.test(name);
    const wanted = body.kind || 'auto';
    if (!['auto', 'channel', 'pattern', 'shard'].includes(wanted)) return [400, { error: 'kind is auto, channel, pattern or shard' }];
    if (!name) return [422, { error: 'Type the channel or pattern to subscribe to.' }];
    if (wanted === 'shard' && pattern) return [422, { error: "A pattern can't be shard-subscribed: type one shard channel's name." }];
    const kind = wanted === 'auto' ? (pattern ? 'pattern' : 'channel') : wanted;
    if (session.tail) emit('tail', { error: null, kind: 'closed', tail: session.tail.id });
    idSeq += 1;
    const id = idSeq;
    session.tail = { id, kind, name, ready: false, matches: kind === 'pattern' ? globMatches(name) : [name] };
    emitState();
    later(200).then(() => {
      if (!session || !session.tail || session.tail.id !== id) return;
      if (p.sim.denied.PUBSUB) {
        closeTail(kind === 'pattern' ? 'the server refused PSUBSCRIBE on that pattern for this user' : 'the server refused SUBSCRIBE on that channel for this user');
        return;
      }
      session.tail.ready = true;
      emitState();
      emit('tail', { kind: 'ready', tail: id });
    });
    return [202, { kind, node: session.picked, tail: id }];
  }

  function publish(p, body) {
    if (!known(body, ['channel', 'message', 'shard']) || typeof body.channel !== 'string' || typeof body.message !== 'string') return [400, { error: SHAPE }];
    if (!body.channel) return [422, { error: 'Type the channel to publish to.' }];
    if (p.read_only) return [422, { error: readOnly(p) }];
    const send = () => {
      const found = CHANNELS.find((c) => c[0] === body.channel);
      let receivers = found ? found[1] : 0;
      if (session && session.tail && session.tail.ready && session.tail.matches.includes(body.channel)) {
        receivers += 1;
        emit('tail', { dropped: 0, kind: 'messages', messages: [{ at_unix_ms: Date.now(), bytes: body.message.length, channel: body.channel, pattern: session.tail.kind === 'pattern' ? session.tail.name : null, preview: body.message }], tail: session.tail.id });
      }
      return [200, { reply: { kind: 'int', value: receivers }, sent: true }];
    };
    if (p.env === 'prod') return ask({ kind: 'ask', text: `Send PUBLISH ${body.channel} ${JSON.stringify(body.message)} to ${p.name}?` }, send);
    return send();
  }

  async function fetchFn(path, init = {}) {
    let body = {};
    try {
      body = init.body ? JSON.parse(init.body) : {};
    } catch {
      body = null;
    }
    await later(60);
    const [status, json] = body === null || typeof body !== 'object' ? [400, { error: SHAPE }] : await route(init.method || 'GET', path, body);
    const copy = JSON.parse(JSON.stringify(json));
    return { status, ok: status >= 200 && status < 300, json: async () => copy };
  }

  return { fetchFn, EventSourceFn: MockEventSource };
}
