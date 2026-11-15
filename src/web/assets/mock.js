// redis-lens dashboard: sample data, used when the address has `?mock`.
//
// It stands in for the redis-lens process: the same methods as the real implementation in
// api.js, answering with view-models directly (see the shapes at the top of api.js), live
// figures that tick once a second, a fake Pub/Sub tail and a fake Monitor. The data is the
// approved mockup's. Nothing here talks to a network, and nothing a form sends (a password
// least of all) is kept: the mock only notes whether one was given.

const GIB = 1024 ** 3;
const MIB = 1024 ** 2;

/** The mockup's repeatable pseudo-random series. */
export function sampleSeries(n, base, amp, seed) {
  const out = [];
  let x = seed * 9301 + 49297;
  for (let i = 0; i < n; i += 1) {
    x = (x * 9301 + 49297) % 233280;
    out.push(base + (x / 233280 - 0.5) * 2 * amp + Math.sin(i / 5) * amp * 0.6);
  }
  return out;
}

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

const KEYCHAIN = 'password or ACL user, kept in the OS keychain';

function profiles() {
  return [
    {
      name: 'prod-cache-01', env: 'PROD', address: 'cache-01.prod.internal:6379', tls: true, tags: ['cache', 'eu'],
      readOnly: true, signIn: KEYCHAIN, server: 'Redis', version: '7.4.1', topology: 'standalone', scale: 1,
      policy: 'allkeys-lfu', checksOff: [],
    },
    {
      name: 'staging-cache', env: 'STAGE', address: 'staging.internal:6379', tls: false, tags: ['cache'],
      readOnly: false, signIn: KEYCHAIN, server: 'Redis', version: '7.2.5', topology: 'standalone', scale: 0.08,
      policy: 'noeviction',
      checksOff: [
        { name: 'MEMORY STATS', why: 'MEMORY STATS is denied for this user, so data and overhead are missing.' },
        { name: 'OBJECT FREQ', why: 'The maxmemory policy is not LFU, so there are no hot keys.' },
      ],
    },
    {
      name: 'demo-cluster', env: 'LOCAL', address: '127.0.0.1:7001', tls: false, tags: [],
      readOnly: false, signIn: 'no password', server: 'Valkey', version: '8.0.1', topology: 'cluster', scale: 0.3,
      policy: 'allkeys-lru', checksOff: [{ name: 'OBJECT FREQ', why: 'The maxmemory policy is not LFU, so there are no hot keys.' }],
      nodes: [
        { id: 'n1', address: '127.0.0.1:7001', role: 'primary', slots: '0-5460' },
        { id: 'n2', address: '127.0.0.1:7002', role: 'primary', slots: '5461-10922' },
        { id: 'n3', address: '127.0.0.1:7003', role: 'primary', slots: '10923-16383' },
      ],
    },
  ];
}

function containers() {
  return [
    {
      name: 'docker:demo-redis', env: 'LOCAL', address: '127.0.0.1:6390', docker: true, container: 'demo-redis', image: 'redis:8',
      tls: false, tags: [], readOnly: false, signIn: 'password for the default user, from the container, kept in memory only',
      server: 'Redis', version: '8.0.2', topology: 'standalone', scale: 0.002, policy: 'noeviction', checksOff: [],
    },
    {
      name: 'docker:demo-valkey', env: 'LOCAL', address: '127.0.0.1:6391', docker: true, container: 'demo-valkey', image: 'valkey/valkey:8',
      tls: false, tags: [], readOnly: false, signIn: 'no password', server: 'Valkey', version: '8.0.1', topology: 'standalone',
      scale: 0.002, policy: 'noeviction', checksOff: [],
    },
  ];
}

function accessText(p) {
  if (p.readOnly) return p.env === 'PROD' ? 'Read-only, the default for production' : 'Read-only';
  if (p.env === 'PROD') return 'Writes on, each one asks first';
  return p.docker ? 'Writes on, for this session' : 'Writes on (the default outside production)';
}

/** The address a connection string points at, with any user and password left out. */
export function maskedAddress(url) {
  const s = String(url ?? '').trim();
  const uri = /^rediss?:\/\/(.*)$/i.exec(s);
  if (uri) {
    const authority = uri[1].split(/[/?#]/)[0];
    return authority.slice(authority.lastIndexOf('@') + 1);
  }
  return s.split(',')[0].trim();
}

const WRITES_ON_TEXT = 'With writes on, key edits, deletes and write commands reach the server. Deletes, bulk actions and @dangerous commands still ask each time. The setting is saved (read_only = false).';

export function createMock({ ApiError, ConfirmRequired }) {
  let emit = () => {};
  let timer = null;
  let tick = 0;
  let confirmSeq = 0;
  const pending = new Map();

  const saved = profiles();
  let docker = containers();
  let connected = null;
  let connecting = null;
  let live = true;
  let pickedNode = null;
  let ops = [];
  let clients = [];
  let sub = null;
  let monitorLeft = 0;
  let monitorKept = 0;
  let lost = null;

  const copy = (v) => JSON.parse(JSON.stringify(v));
  const later = (value, ms = 150) => new Promise((resolve) => setTimeout(() => resolve(copy(value)), ms));
  const find = (name) => saved.concat(docker).find((p) => p.name === name) || null;
  const nowClock = () => Date.now();
  const todayAt = (hh, mm, ss) => {
    const d = new Date();
    return Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate(), hh, mm, ss);
  };

  function needConfirm(confirm, run) {
    confirmSeq += 1;
    const id = `mock-${confirmSeq}`;
    pending.set(id, { expect: confirm.kind === 'type' ? confirm.expect : null, run });
    throw new ConfirmRequired({ id, title: null, expect: '', ...confirm });
  }

  /** The question a write asks, by the table in spec.md "Write permissions". */
  function askFor(p, { title, text, destructive = false, dangerous = false, expect = null }) {
    const prod = p.env === 'PROD';
    if (prod && (destructive || dangerous)) return { kind: 'type', expect: expect || p.name, title, text };
    return { kind: 'ask', title, text };
  }

  function reseed() {
    const scale = connected ? connected.scale : 1;
    const shift = pickedNode ? pickedNode.charCodeAt(1) % 5 : 0;
    ops = sampleSeries(40, 12400 * scale, 1600 * scale, 7 + shift).map((v) => Math.max(1, v));
    clients = sampleSeries(40, Math.max(4, 214 * scale), Math.max(1, 9 * scale), 3 + shift).map((v) => Math.max(1, Math.round(v)));
  }

  function connVM(p) {
    const nodes = p.nodes || [];
    return {
      name: p.name, env: p.env, address: p.address, readOnly: p.readOnly, server: p.server, version: p.version,
      topology: p.topology, tls: p.tls, target: null, providerNotice: p.providerNotice || null,
      checksOff: p.checksOff, nodes, pickedNode: nodes.length ? pickedNode : null,
      shardChannels: true, monitorSeconds: 10, monitorOff: null,
    };
  }

  const state = () => ({ connected: !!connected, connecting, conn: connected ? connVM(connected) : null, live, lost });

  function profileVM(p) {
    let status = 'not-open';
    if (connecting && connecting.name === p.name) status = 'connecting';
    else if (connected && connected.name === p.name) status = p.checksOff.length ? 'limited' : 'connected';
    return {
      name: p.name, env: p.env, address: p.address, docker: p.docker === true, container: p.container || null, image: p.image || null,
      tls: p.tls, tags: p.tags, readOnly: p.readOnly, access: accessText(p), signIn: p.signIn,
      providerNotice: p.providerNotice || null, ssh: p.ssh || null, sentinel: p.sentinel || null, status, statusText: null,
    };
  }

  const connections = () => ({
    profiles: saved.map(profileVM),
    docker: docker.map(profileVM),
    dockerNote: docker.length ? null : 'No Redis or Valkey containers are running in Docker.',
  });

  function figures() {
    const p = connected;
    return {
      at: nowClock(), node: pickedNode, opsPerSec: ops[ops.length - 1], clients: clients[clients.length - 1],
      usedMemory: 15.02 * GIB * p.scale, maxMemory: p.policy === 'noeviction' && p.scale < 0.01 ? null : 24 * GIB * p.scale,
      hitRate: 0.97, evictedPerSec: p.policy === 'noeviction' ? 0 : Math.round(3 * p.scale), expiredPerSec: Math.round(120 * p.scale),
    };
  }

  function overview() {
    const p = connected;
    if (!p) throw new ApiError('Connect first: open Connections and pick a connection.');
    const f = figures();
    const refusedStats = p.checksOff.some((c) => c.name === 'MEMORY STATS');
    const notes = p.checksOff.map((c) => c.why);
    if (p.providerNotice) notes.push(p.providerNotice);
    const shards = (p.nodes || []).map((n, i) => ({
      id: n.id, slots: n.slots, address: n.address, role: n.role, opsPerSec: Math.round((12400 * p.scale) / 3 + i * 40),
      usedMemory: (15.02 * GIB * p.scale) / 3 + i * 40 * MIB, keys: Math.round((7920114 * p.scale) / 3) + i * 1200,
      picked: n.id === pickedNode, status: 'ok',
    }));
    return {
      address: `${p.address}${p.tls ? ' · TLS' : ''}`,
      server: {
        kind: p.server, version: p.version, topology: p.topology, uptimeSecs: Math.round(1052400 * Math.max(p.scale, 0.05)),
        role: 'primary', replicas: p.scale === 1 ? 2 : 0, persistence: p.scale === 1 ? 'RDB ok · AOF off' : 'RDB ok', policy: p.policy,
      },
      memory: { used: f.usedMemory, max: f.maxMemory },
      keys: {
        total: Math.round(7920114 * p.scale), ttlShare: 0.31, dataBytes: refusedStats ? null : 13 * GIB * p.scale,
        overheadBytes: refusedStats ? null : 2.1 * GIB * p.scale, db: 0,
      },
      figures: f,
      history: { ops: ops.slice(), clients: clients.slice(), everySecs: 1 },
      shards,
      findings: p.scale === 1
        ? [
          { text: 'LRANGE took 912 ms at 13:58 UTC.', where: 'slowlog' },
          { text: '38 connections from 10.0.9.17 idle for over an hour.', where: 'clients' },
        ]
        : [],
      notes,
    };
  }

  function clientGroups() {
    const s = connected.scale;
    const n = (v) => Math.max(1, Math.round(v * Math.max(s, 0.03)));
    const libs = [['redis-py', 96, 'GET'], ['node-redis', 71, 'HGETALL'], ['go-redis', 38, 'EVALSHA'], ['(no name)', 7, 'PING'], ['redis-cli', 2, 'CLIENT']];
    const groups = libs.map(([name, c, cmd], i) => ({
      by: 'library', name, connections: n(c), oldestSecs: 86400 * (12 - i * 2), idleSecs: 40 + i * 310, lastCommand: cmd, memory: n(c) * 22800, flagged: false,
    }));
    groups.push(
      { by: 'address', name: '10.0.4.0/24', connections: n(176), oldestSecs: 86400 * 12, idleSecs: 1240, lastCommand: 'GET', memory: n(176) * 22800, flagged: false },
      { by: 'address', name: '10.0.9.0/24', connections: n(38), oldestSecs: 86400 * 3, idleSecs: 5400, lastCommand: 'PING', memory: n(38) * 22800, flagged: s === 1 },
    );
    return {
      off: null, total: groups.filter((g) => g.by === 'library').reduce((sum, g) => sum + g.connections, 0), groups,
      notes: s === 1 ? ['38 of them have been idle for 1h 00m or more. Idle clients hold memory and a connection each.'] : [],
    };
  }

  function slowlog() {
    return {
      off: null,
      threshold: 'Commands slower than 10 ms are logged; the last 128 are kept.',
      entries: [
        { at: todayAt(13, 58, 41), micros: 912000, command: 'LRANGE queue:emails 0 -1', keys: ['queue:emails'], client: '10.0.4.17:51122', note: 'Reads a whole list. That it was slow is the hint that the key is big.' },
        { at: todayAt(13, 41, 7), micros: 184000, command: 'ZRANGE user:88120:feed 0 -1 WITHSCORES', keys: ['user:88120:feed'], client: '10.0.4.23:40718', note: 'Reads a whole sorted set.' },
        { at: todayAt(12, 20, 15), micros: 96000, command: 'HGETALL cache:product:51007', keys: ['cache:product:51007'], client: '10.0.4.31:38806', note: 'Reads a whole hash.' },
        { at: todayAt(11, 2, 33), micros: 41000, command: 'SCAN 0 MATCH tmp:import:* COUNT 10000', keys: [], client: '10.0.4.17:51122', note: null },
      ],
      notes: [],
    };
  }

  function channels() {
    const cluster = connected.topology === 'cluster';
    const node = cluster ? (connected.nodes.find((n) => n.id === pickedNode) || connected.nodes[0]).address : null;
    const list = CHANNELS.map(([name, subscribers]) => ({ name, subscribers: Math.max(1, Math.round(subscribers * Math.max(connected.scale, 0.2))), shard: false, node: null }));
    if (cluster) {
      list.push(
        { name: 'shard.orders', subscribers: 3, shard: true, node: '127.0.0.1:7002' },
        { name: 'shard.presence', subscribers: 1, shard: true, node: '127.0.0.1:7003' },
      );
    }
    return { off: null, total: list.length, patterns: 2, node, channels: list, notes: [] };
  }

  function stopSession() {
    sub = null;
    monitorLeft = 0;
    monitorKept = 0;
  }

  function onTick() {
    tick += 1;
    if (connected && live) {
      const scale = connected.scale;
      ops = ops.slice(1).concat(Math.max(1, (12400 + (Math.random() - 0.5) * 3600) * scale));
      clients = clients.slice(1).concat(Math.max(1, Math.round((214 + (Math.random() - 0.5) * 18) * Math.max(scale, 0.02))));
      emit('figures', figures());
    }
    if (monitorLeft > 0) {
      monitorLeft -= 1;
      const at = nowClock();
      const lines = [0, 1, 2].map((i) => ({
        at, db: 0, client: `10.0.4.${11 + (((tick + i) * 7) % 40)}:${40000 + ((tick * 13 + i * 7) % 20000)}`,
        command: MONITOR_COMMANDS[(tick * 3 + i) % MONITOR_COMMANDS.length],
      }));
      monitorKept += lines.length;
      emit('monitor', {
        running: monitorLeft > 0, left: monitorLeft, lines, kept: monitorKept, node: null,
        stopped: monitorLeft > 0 ? null : 'MONITOR stopped by itself after 10 s.',
      });
    }
    if (sub) {
      const names = sub.kind === 'pattern' ? sub.matches : [sub.name];
      const count = sub.kind === 'pattern' ? 2 : 1;
      const messages = [];
      for (let i = 0; i < count; i += 1) {
        const channel = names[(tick + i) % names.length];
        messages.push({ at: nowClock(), channel, body: sampleMessage(channel, tick + i) });
      }
      emit('tail', { messages, skipped: 0, stopped: null });
    }
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

  function reply(input, kind, text, extra = {}) {
    return {
      input, node: connected && connected.topology === 'cluster' ? (connected.nodes.find((n) => n.id === pickedNode) || connected.nodes[0]).address : null,
      micros: kind === 'reply' ? 180 + ((tick * 37) % 400) : null, kind,
      views: { text, table: extra.table || null, json: extra.json || null, raw: extra.raw || null },
    };
  }

  const WRITES = ['SET', 'DEL', 'UNLINK', 'EXPIRE', 'HSET', 'LPUSH', 'FLUSHALL', 'FLUSHDB', 'INCR', 'PUBLISH'];
  const DESTRUCTIVE = ['DEL', 'UNLINK', 'FLUSHALL', 'FLUSHDB'];

  function runCommand(line) {
    const p = connected;
    if (!p) throw new ApiError('The command line needs a connection. Open Connections and pick one.');
    const text = String(line).trim();
    const upper = text.toUpperCase();
    const words = text.split(/\s+/);
    const word = upper.split(/\s+/)[0];
    // Two sample-data-only lines, to see the "connection lost" banners without a real fault.
    if (upper === 'MOCK LOST') {
      setTimeout(() => {
        connected = null;
        stopSession();
        lost = 'The connection to the server was lost.';
        emit('state', copy(state()));
      }, 200);
      return reply(text, 'note', 'The sample data now loses its connection to the server.');
    }
    if (upper === 'MOCK LINK DOWN') {
      setTimeout(() => emit('link', { up: false }), 200);
      return reply(text, 'note', 'The sample data now loses its line to redis-lens.');
    }
    if (upper === 'PING') return reply(text, 'reply', 'PONG', { json: '"PONG"', raw: '[simple string] +PONG' });
    if (upper === 'DBSIZE') {
      const n = Math.round(7920114 * p.scale);
      return reply(text, 'reply', `(integer) ${n}`, { json: String(n), raw: `[integer] :${n}` });
    }
    if (upper === 'TTL USER:10293:CART') return reply(text, 'reply', '(integer) 532800', { json: '532800', raw: '[integer] :532800' });
    if (upper === 'INFO MEMORY') {
      const body = 'used_memory_human:15.02G\nmaxmemory_human:24.00G\nmaxmemory_policy:allkeys-lfu';
      return reply(text, 'reply', body, { json: JSON.stringify(body), raw: `[bulk string] $${body.length} ${JSON.stringify(body)}` });
    }
    if (upper === 'GET RL:203.0.113.7:LOGIN') return reply(text, 'reply', '"17"', { json: '"17"', raw: '[bulk string] $2 "17"' });
    if (upper === 'HGETALL USER:10293:CART') {
      const pairs = [['sku:44812', '{"qty":2,"price":19.99}'], ['sku:44813', '{"qty":1,"price":4.50}'], ['coupon', 'WINTER10']];
      const flat = pairs.flat();
      return reply(text, 'reply', flat.map((v, i) => `${i + 1}) ${JSON.stringify(v)}`).join('\n'), {
        table: { columns: ['#', 'field', 'value'], rows: pairs.map(([f, v], i) => [String(i + 1), f, v]) },
        json: JSON.stringify(Object.fromEntries(pairs), null, 2),
        raw: `[array] *${flat.length}\n${flat.map((v) => `  [bulk string] $${v.length} ${JSON.stringify(v)}`).join('\n')}`,
      });
    }
    if (word === 'KEYS') {
      return needConfirm(askFor(p, {
        title: 'Send KEYS?', dangerous: true,
        text: `Send it to ${p.name}?\nKEYS walks every key in one go and blocks the server while it does.\n\nSends: ${text}`,
      }), () => {
        const keys = ['user:10293:cart', 'user:28648:cart', 'user:51120:cart'];
        return reply(text, 'reply', keys.map((k, i) => `${i + 1}) ${JSON.stringify(k)}`).join('\n'), {
          table: { columns: ['#', 'value'], rows: keys.map((k, i) => [String(i + 1), k]) },
          json: JSON.stringify(keys, null, 2),
          raw: `[array] *${keys.length}\n${keys.map((k) => `  [bulk string] $${k.length} ${JSON.stringify(k)}`).join('\n')}`,
        });
      });
    }
    if (['SUBSCRIBE', 'PSUBSCRIBE', 'SSUBSCRIBE', 'UNSUBSCRIBE', 'PUNSUBSCRIBE', 'SUNSUBSCRIBE'].includes(word)) return reply(text, 'refused', 'Not sent: subscribing would take over this connection. Use Pub/Sub.');
    if (word === 'MONITOR') return reply(text, 'refused', 'Not sent: MONITOR would take over this connection. Use Server › Monitor.');
    if (word === 'SELECT') return reply(text, 'refused', 'Not sent: SELECT would switch the database under every view.');
    if (['MULTI', 'EXEC', 'DISCARD', 'WATCH', 'UNWATCH'].includes(word)) return reply(text, 'refused', 'Not sent: a transaction or a watch left open would hold up every view.');
    if (['AUTH', 'HELLO', 'RESET', 'QUIT'].includes(word)) return reply(`${words[0]} ***`, 'refused', `Not sent: ${word} would change the connection every view shares.`);
    if (['EVAL', 'EVAL_RO', 'EVALSHA', 'EVALSHA_RO', 'FCALL', 'FCALL_RO', 'SCRIPT', 'FUNCTION'].includes(word)) return reply(text, 'refused', 'Not sent: scripts and functions are not sent from here.');
    if (WRITES.includes(word)) {
      if (p.readOnly) return reply(text, 'refused', `Not sent: ${p.name} is read-only. Turn writes on in Connections.`);
      const destructive = DESTRUCTIVE.includes(word);
      const done = () => reply(text, 'reply', word === 'SET' || word.startsWith('FLUSH') ? 'OK' : '(integer) 1', { json: word === 'SET' || word.startsWith('FLUSH') ? '"OK"' : '1' });
      if (!destructive && p.env !== 'PROD') return done();
      const key = words.length === 2 ? words[1] : null;
      return needConfirm(askFor(p, {
        title: `Send ${word}?`, destructive, expect: key,
        text: `Send it to ${p.name}?${destructive ? "\nThis can't be undone." : ''}\n\nSends: ${text}`,
      }), done);
    }
    return reply(text, 'note', 'The sample data only answers PING, DBSIZE, INFO memory, TTL user:10293:cart, GET rl:203.0.113.7:login, HGETALL user:10293:cart and KEYS user:*:cart. Writes such as DEL show the questions.');
  }

  const backend = {
    mock: true,
    open(emitFn) {
      emit = emitFn;
      if (!timer) timer = setInterval(onTick, 1000);
      setTimeout(() => emit('link', { up: true }), 0);
    },
    close() {
      if (timer) clearInterval(timer);
      timer = null;
    },
    reopen() {
      backend.close();
      backend.open(emit);
    },

    async getState() {
      return later(state(), 60);
    },
    async listConnections() {
      return later(connections());
    },
    async addConnection(form) {
      const name = String(form.name || '').trim();
      if (find(name)) throw new ApiError(`A connection named ${name} already exists.`);
      const address = maskedAddress(form.url);
      if (!address) throw new ApiError('The connection string could not be read: missing host.');
      const hasPassword = typeof form.password === 'string' && form.password.length > 0;
      const hasToken = Array.isArray(form.tokenCommand) && form.tokenCommand.length > 0;
      if (hasToken && !form.tokenCommandConfirmed) throw new ApiError('The token command was not confirmed, so nothing was saved.');
      const env = String(form.env || 'OTHER').toUpperCase();
      saved.push({
        name, env, address, tls: form.tls === true, tags: form.tags || [],
        readOnly: env === 'PROD' ? form.writes !== true : form.writes === false,
        signIn: hasToken ? 'a token from a command; the username is kept in the OS keychain' : hasPassword ? KEYCHAIN : 'no password',
        ssh: form.sshHost ? form.sshHost.slice(form.sshHost.lastIndexOf('@') + 1) : null,
        sentinel: form.sentinelAddresses && form.sentinelAddresses.length ? `master ${form.sentinelMaster} through ${form.sentinelAddresses.length} Sentinel${form.sentinelAddresses.length === 1 ? '' : 's'}` : null,
        server: 'Redis', version: '7.4.1', topology: 'standalone', scale: 0.01, policy: 'noeviction', checksOff: [],
      });
      return later({ name }, 400);
    },
    async updateConnection(name, changes) {
      const p = find(name);
      if (!p) throw new ApiError(`There is no connection named ${name}.`);
      const apply = () => {
        if (changes.env !== undefined) p.env = String(changes.env).toUpperCase();
        if (changes.tags !== undefined) p.tags = changes.tags;
        if (changes.writes !== undefined) p.readOnly = !changes.writes;
        if (connected && connected.name === name) emit('state', copy(state()));
        return profileVM(p);
      };
      if (changes.writes === true && p.readOnly) {
        const prod = (changes.env !== undefined ? String(changes.env).toUpperCase() : p.env) === 'PROD';
        return needConfirm({
          kind: prod ? 'type' : 'ask', expect: p.name, title: `Turn on writes for ${p.name}?`,
          text: `${prod ? 'This is a PROD profile. ' : ''}${WRITES_ON_TEXT}`,
        }, apply);
      }
      return later(apply());
    },
    async removeConnection(name) {
      const at = saved.findIndex((p) => p.name === name);
      if (at < 0) throw new ApiError(`There is no saved connection named ${name}.`);
      if (connected && connected.name === name) {
        connected = null;
        stopSession();
        emit('state', copy(state()));
      }
      saved.splice(at, 1);
      return later({});
    },
    async testConnection(name) {
      const p = find(name);
      if (!p) throw new ApiError(`There is no connection named ${name}.`);
      const off = new Map(p.checksOff.map((c) => [c.name, c.why]));
      const checks = ['SCAN', 'TYPE and PTTL', 'MEMORY USAGE', 'INFO', 'OBJECT FREQ', 'MEMORY STATS'].map((c) => ({ name: c, ok: !off.has(c), why: off.get(c) || null }));
      const ok = checks.every((c) => c.ok);
      return later({
        ok,
        summary: ok ? 'Every check passed.' : 'Limited: some checks were refused, so the panels that need them are off.',
        checks,
        notes: [`${p.server} ${p.version}, ${p.topology}.`, 'A test reads no key: it asks what this user may do, and nothing else.'],
      }, 700);
    },
    async discoverDocker() {
      docker = containers().map((c) => {
        const old = docker.find((d) => d.name === c.name);
        return old || c;
      });
      return later(connections(), 500);
    },
    async connect(name) {
      const p = find(name);
      if (!p) throw new ApiError(`There is no connection named ${name}.`);
      stopSession();
      connected = null;
      lost = null;
      connecting = { name: p.name, text: `Connecting to ${p.name} (${p.address}) and checking permissions…` };
      emit('state', copy(state()));
      await later(null, 700);
      connecting = null;
      connected = p;
      pickedNode = p.nodes ? p.nodes[0].id : null;
      reseed();
      return copy(state());
    },
    async disconnect() {
      connected = null;
      connecting = null;
      stopSession();
      return later(state());
    },
    async setLive(value) {
      live = value === true;
      return later(state(), 60);
    },
    async pickNode(id) {
      if (connected && connected.nodes && connected.nodes.some((n) => n.id === id)) {
        pickedNode = id;
        reseed();
      }
      return later(state(), 60);
    },
    async getOverview() {
      return later(overview());
    },
    async getClients() {
      if (!connected) throw new ApiError('Connect first: open Connections and pick a connection.');
      return later(clientGroups());
    },
    async getSlowlog() {
      if (!connected) throw new ApiError('Connect first: open Connections and pick a connection.');
      return later(slowlog());
    },
    async startMonitor() {
      const p = connected;
      if (!p) throw new ApiError('Connect first: open Connections and pick a connection.');
      return needConfirm({
        kind: p.env === 'PROD' ? 'type' : 'ask', expect: p.name, title: `Send MONITOR to ${p.name}?`,
        text: 'MONITOR runs on a connection of its own, and the server copies every command it runs to it. That slows a busy server.\n\nIt stops after 10 s or 10,000 lines, or when you press Stop. The lines stay in memory.',
      }, () => {
        monitorLeft = 10;
        monitorKept = 0;
        return { running: true, left: 10, lines: [], kept: 0, node: null, stopped: null };
      });
    },
    async stopMonitor() {
      if (monitorLeft > 0) {
        monitorLeft = 0;
        emit('monitor', { running: false, left: 0, lines: [], kept: monitorKept, node: null, stopped: 'MONITOR stopped.' });
      }
      return later({}, 60);
    },
    async getChannels() {
      if (!connected) throw new ApiError('Connect first: open Connections and pick a connection.');
      return later(channels());
    },
    async subscribe({ target, shard }) {
      if (!connected) throw new ApiError('Connect first: open Connections and pick a connection.');
      const name = String(target || '').trim();
      if (!name) throw new ApiError('Type a channel or a pattern to subscribe to.');
      const pattern = /[*?[]/.test(name);
      if (shard && pattern) throw new ApiError("A pattern can't be shard-subscribed: type one channel name.");
      const kind = shard ? 'shard' : pattern ? 'pattern' : 'channel';
      sub = { name, kind, matches: pattern ? globMatches(name) : [name] };
      return later({ name, kind });
    },
    async unsubscribe() {
      sub = null;
      return later({}, 60);
    },
    async publish({ channel, message }) {
      const p = connected;
      if (!p) throw new ApiError('Connect first: open Connections and pick a connection.');
      if (p.readOnly) throw new ApiError(`Not sent: ${p.name} is read-only. Turn writes on in Connections.`);
      const send = () => {
        const known = CHANNELS.find((c) => c[0] === channel);
        const receivers = known ? known[1] : 0;
        if (sub && sub.matches.includes(channel)) emit('tail', { messages: [{ at: nowClock(), channel, body: message }], skipped: 0, stopped: null });
        return { receivers, text: null };
      };
      if (p.env === 'PROD') {
        return needConfirm({ kind: 'ask', title: 'Publish this message?', text: `Send it to ${p.name}?\n\nSends: PUBLISH ${channel} ${message}` }, send);
      }
      return later(send());
    },
    async runCommand(line) {
      return later(runCommand(line), 120);
    },

    async answerConfirm(id, typed) {
      const p = pending.get(id);
      pending.delete(id);
      if (!p) throw new ApiError('That question expired, so nothing was sent.');
      if (p.expect !== null && typed !== p.expect) throw new ApiError('Not sent: the name typed did not match.');
      return later(p.run());
    },
    async cancelConfirm(id) {
      pending.delete(id);
      return later({}, 30);
    },
  };
  return backend;
}
