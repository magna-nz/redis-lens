// redis-lens dashboard: sample data, used when the address has `?mock`.
//
// It stands in for the redis-lens process at the HTTP level: `createMockServer()` returns
// a `fetch` that answers the routes of docs/dashboard-api.md with the same JSON, status
// codes and event stream the real server sends, and the `token` it signs in with. Like the
// real server, every /api/* route answers 401 without `Authorization: Bearer <token>`, and
// /api/events is a byte stream of Server-Sent Events (each event split in two at a varying
// place, so the page's stream parser is exercised as a network would). So `?mock` runs the
// very same code as the real thing. The data is the approved mockup's.
//
// Release builds of redis-lens do not serve this file (debug builds only).
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
/** The sample servers' session token. It signs in to nothing real. */
export const MOCK_TOKEN = 'a'.repeat(64);

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
      version: '7.2.5', scale: 0.08, policy: 'noeviction', lfu: false, denied: { 'MEMORY STATS': noperm('MEMORY STATS') }, configDenied: true, analysisFails: true,
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
      // Writes this user's ACL refuses: found when one is sent, then off for the session.
      writeDenied: ['HSET', 'UNLINK'],
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

// ---------- the keyspace of the sample data (Browse and Analyze) ----------

const MIB = 1024 ** 2;
const utf8 = new TextEncoder();

/** The hex id that names a key in a request: the hex of its bytes. */
export function hexOf(raw) {
  return Array.from(utf8.encode(raw), (b) => b.toString(16).padStart(2, '0')).join('');
}

function fromHex(id) {
  if (typeof id !== 'string' || !/^(?:[0-9a-f]{2})*$/.test(id)) return null;
  const bytes = new Uint8Array((id.match(/../g) || []).map((x) => parseInt(x, 16)));
  return new TextDecoder().decode(bytes);
}

/** A name as redis-lens sends it: escaped for the screen, the hex id, and the name to copy
 *  when escaping changed nothing. */
export function nameOf(raw) {
  const name = raw.replace(/[\x00-\x1f\x7f]/g, (c) => `\\x${c.charCodeAt(0).toString(16).padStart(2, '0')}`);
  return { copy: name === raw ? raw : null, id: hexOf(raw), name };
}

/** A TTL in the TUI's words: `6d 10h`, `59m 59s`, `never`. */
export function ttlText(secs) {
  if (secs === null || secs === undefined) return 'never';
  const d = Math.floor(secs / 86400);
  const hh = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  if (d) return `${d}d ${hh}h`;
  if (hh) return `${hh}h ${m}m`;
  if (m) return `${m}m ${s}s`;
  return `${s}s`;
}

/** A Redis glob as a regular expression: `*`, `?`, `[...]` and `\` escapes. */
export function globRe(glob) {
  const lit = (c) => c.replace(/[.*+?^${}()|[\]\\/]/g, '\\$&');
  let re = '';
  for (let i = 0; i < glob.length; i += 1) {
    const c = glob[i];
    if (c === '*') re += '.*';
    else if (c === '?') re += '.';
    else if (c === '\\' && i + 1 < glob.length) {
      i += 1;
      re += lit(glob[i]);
    } else if (c === '[') {
      const end = glob.indexOf(']', i + 2);
      if (end < 0) re += '\\[';
      else {
        const body = glob.slice(i + 1, end);
        const neg = body.startsWith('^');
        re += `[${neg ? '^' : ''}${(neg ? body.slice(1) : body).replace(/[\\\]]/g, '\\$&')}]`;
        i = end;
      }
    } else re += lit(c);
  }
  return new RegExp(`^${re}$`, 's');
}

// Each pattern of the sample keyspace: its keys at scale 1, their mean size, TTL (seconds,
// or null for none) and the share without a TTL, and how to name its i-th key.
const SPACE = [
  { name: 'user:{id}:cart', glob: 'user:*:cart', type: 'hash', keys: 2410000, avg: 1290, ttl: 554400, noTtl: 0.01, enc: 'listpack', unit: 'fields', len: 4, hot: 20, key: (i) => `user:${10293 + i * 7}:cart`, value: 'cart', group: 'user:' },
  { name: 'session:{id}', glob: 'session:*', type: 'string', keys: 1980000, avg: 820, ttl: 1790, noTtl: 0, enc: 'raw', unit: 'bytes', len: 612, hot: 60, key: (i) => `session:${((0x5f3a91c + i * 40503) >>> 0).toString(16)}`, value: 'session', grows: true },
  { name: 'cache:product:{id}', glob: 'cache:product:*', type: 'hash', keys: 1210000, avg: 2100, ttl: 3540, noTtl: 0.04, enc: 'listpack', unit: 'fields', len: 6, hot: 30, key: (i) => `cache:product:${51007 + i}`, value: 'product' },
  { name: 'rl:{ip}:login', glob: 'rl:*:login', type: 'string', keys: 880000, avg: 64, ttl: 52, noTtl: 0, enc: 'int', unit: 'bytes', len: 2, hot: 180, key: (i) => `rl:203.0.${(i >> 8) & 255}.${(7 + i) & 255}:login`, value: 'counter' },
  { name: 'user:{id}:feed', glob: 'user:*:feed', type: 'zset', keys: 640000, avg: 3400, ttl: null, noTtl: 1, enc: 'listpack', unit: 'members', len: 40, hot: 10, key: (i) => `user:${88120 + i * 3}:feed`, value: 'feed', group: 'user:' },
  { name: 'tmp:import:{id}', glob: 'tmp:import:*', type: 'string', keys: 420000, avg: 5200, ttl: null, noTtl: 1, enc: 'raw', unit: 'bytes', len: 5120, hot: 0, key: (i) => `tmp:import:${20260901 + i}`, value: 'blob', grows: true, warn: 'Every key never expires, and the pattern grows: temporary data should carry a TTL.' },
  { name: 'tags:{id}', glob: 'tags:*', type: 'set', keys: 380000, avg: 310, ttl: null, noTtl: 1, enc: 'listpack', unit: 'members', len: 5, hot: 4, key: (i) => `tags:${1000 + i}`, value: 'tags' },
  { name: 'queue:emails', glob: 'queue:emails', literal: true, type: 'list', keys: 1, avg: 92 * MIB, ttl: null, noTtl: 1, enc: 'quicklist', unit: 'elements', len: 412000, hot: 140, key: () => 'queue:emails', value: 'queue' },
  { name: 'events:orders', glob: 'events:orders', literal: true, type: 'stream', keys: 1, avg: 18 * MIB, ttl: null, noTtl: 1, enc: 'stream', unit: 'entries', len: 96000, hot: 90, key: () => 'events:orders', value: 'stream' },
  { name: 'bf:signups', glob: 'bf:signups', literal: true, type: 'MBbloom--', keys: 1, avg: Math.round(2.4 * MIB), ttl: null, noTtl: 1, enc: null, unit: null, len: null, hot: 70, key: () => 'bf:signups', value: 'bloom' },
  { name: 'cfg:flags', glob: 'cfg:flags', literal: true, type: 'string', keys: 1, avg: 410, ttl: null, noTtl: 1, enc: 'raw', unit: 'bytes', len: 342, hot: 120, key: () => 'cfg:flags', value: 'json' },
  // A key whose name holds an escape sequence: shown escaped, asked for by its hex id, and
  // never copied (a pasted copy would name another key).
  { name: 'evil\x1b[31mkey', glob: null, literal: true, type: 'string', keys: 1, avg: 56, ttl: null, noTtl: 1, enc: 'embstr', unit: 'bytes', len: 1, hot: 2, key: () => 'evil\x1b[31mkey', value: 'v' },
];
// The other databases of a standalone server.
const OTHER_DBS = {
  1: [{ name: 'job:{id}', glob: 'job:*', type: 'hash', keys: 41, fixed: true, avg: 240, ttl: null, noTtl: 1, enc: 'listpack', unit: 'fields', len: 3, hot: 3, key: (i) => `job:${700 + i}`, value: 'product' }],
  3: [{ name: 'lock:{id}', glob: 'lock:*', type: 'string', keys: 3, fixed: true, avg: 72, ttl: 30, noTtl: 0, enc: 'embstr', unit: 'bytes', len: 8, hot: 1, key: (i) => `lock:order:${9 + i}`, value: 'counter' }],
};
const TTL_LABELS = ['none', '<1m', '<1h', '<1d', '<7d', '<30d', '≥30d'];
const FORMATS = ['text', 'hex', 'JSON', 'MessagePack', 'PHP', 'Pickle', 'Protobuf', 'Java'];
const COPY_REFUSED = 'The value has control characters, so it is not copied as text: show it as hex to copy it.';
const BAD_FORMAT = 'the format must be one of text, hex, json, msgpack, php, pickle, protobuf or java';
const NO_REPORT = 'No analysis yet: analyze the keys first.';
const NO_VIEW = 'That value is no longer open (a newer one replaced it, or the session changed): open it again.';
const NO_LIST = 'That key list is gone (a newer one replaced it, or the session changed): open it again.';
const NOT_CONNECTED = 'Not connected: connect first.';
const BIG_KEY = 64 * MIB;
const HOT_BIG = 10 * MIB;
const HOT_FREQ = 100;
const scanCommand = (glob) => (glob ? `redis-cli --scan --pattern "${glob}"` : null);
const snapFile = (at) => `${new Date(at * 1000).toISOString().replace(/[-:]/g, '').slice(0, 15)}Z.json`;
const snapUtc = (at) => `${new Date(at * 1000).toISOString().replace('T', ' ').slice(0, 19)} UTC`;
const short = (n) => (n >= GIB ? `${(n / GIB).toFixed(1)}G` : n >= MIB ? `${(n / MIB).toFixed(1)}M` : n >= 1024 ? `${(n / 1024).toFixed(1)}K` : `${n}B`);
const pct = (r) => `${Math.round(r * 100)}%`;

/** A value in a format, as redis-lens draws it: header, lines, what to copy. */
function render(raw, want = null) {
  const binary = /[\x00-\x08\x0e-\x1f\x7f-\xff]/.test(raw);
  let json = null;
  try {
    const v = JSON.parse(raw);
    if (v && typeof v === 'object') json = v;
  } catch {
    // Not JSON.
  }
  const detected = binary ? 'hex' : json ? 'JSON' : 'text';
  const label = want ? FORMATS.find((f) => f.toLowerCase() === want.toLowerCase() || (want.toLowerCase() === 'msgpack' && f === 'MessagePack')) : detected;
  const bytes = raw.length;
  const escaped = raw.replace(/[\x00-\x08\x0b-\x1f\x7f-\xff]/g, (c) => `\\x${c.charCodeAt(0).toString(16).padStart(2, '0')}`);
  const hexLines = () => {
    const out = [];
    for (let at = 0; at < Math.min(bytes, 512); at += 16) {
      const chunk = raw.slice(at, at + 16);
      const hx = Array.from(chunk, (c) => c.charCodeAt(0).toString(16).padStart(2, '0')).join(' ');
      out.push(`${at.toString(16).padStart(8, '0')}  ${hx.padEnd(47)}  ${chunk.replace(/[^\x20-\x7e]/g, '.')}`);
    }
    return out;
  };
  let lines;
  let copy = raw;
  let copyRefused = null;
  let note = null;
  if (label === 'hex') {
    lines = hexLines();
    copy = Array.from(raw, (c) => c.charCodeAt(0).toString(16).padStart(2, '0')).join('');
    if (bytes > 512) note = 'Shown: the first 512 bytes in hex. Copy takes them all.';
  } else if (label === 'JSON' && json) lines = JSON.stringify(json, null, 2).split('\n');
  else {
    if (label !== 'text') note = `This value is not ${label}: it is shown as text.`;
    lines = escaped.split('\n');
    if (binary) {
      copy = null;
      copyRefused = COPY_REFUSED;
    }
  }
  const header = want ? `value · ${bytes.toLocaleString('en-US')} bytes · shown as ${label}` : `value · ${detected} · ${bytes.toLocaleString('en-US')} bytes · auto-detected`;
  return { bytes, compression: null, copy, copy_refused: copyRefused, detected, format: label, formats: FORMATS, header, lines, note, picked: want ? label.toLowerCase() : null, truncated: false };
}

/** The value of key `i` of a pattern: items, or the string. */
function valueOf(spec, i, raw) {
  const seed = (i * 2654435761) >>> 0;
  switch (spec.value) {
    case 'cart': return { kind: 'hash', length: 3 + (i % 3), item: (n) => (n < 2 + (i % 3) ? [`sku:${44812 + n * 31 + (i % 50)}`, JSON.stringify({ qty: 1 + ((seed >> n) % 3), price: [19.99, 4.5, 129, 7.25][n % 4] })] : ['coupon', 'WINTER10']) };
    case 'product': return { kind: 'hash', length: 6, item: (n) => [['name', 'price', 'stock', 'currency', 'updated_at', 'image'][n], [`Trail shoe ${i % 90}`, String(49 + (i % 70)), String(seed % 400), 'EUR', '2026-11-14T08:12:40Z', `/img/p/${51007 + i}.jpg`][n]] };
    case 'session': return { kind: 'string', raw: JSON.stringify({ user_id: 10293 + i * 7, roles: ['buyer'], csrf: seed.toString(36), cart: `user:${10293 + i * 7}:cart`, expires: '2026-11-16T10:00:00Z', ua: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 14_6)' }) };
    case 'counter': return { kind: 'string', raw: String(1 + (seed % 30)) };
    case 'feed': return { kind: 'zset', length: 40, item: (n) => [`post:${900100 + ((seed + n * 977) % 90000)}`, String(1794745502 - n * 3600)] };
    case 'blob': return { kind: 'string', raw: `\x1f\x8b\x08\x00${Array.from({ length: 1020 }, (_, n) => String.fromCharCode((seed + n * 131) % 256)).join('')}` };
    case 'tags': return { kind: 'set', length: 5, item: (n) => [null, ['red', 'blue', 'sale', 'new', 'eu'][n]] };
    case 'queue': return { kind: 'list', length: spec.len, page: 500, item: (n) => [null, JSON.stringify({ to: `user${10293 + n * 7}@example.com`, template: n % 3 ? 'receipt' : 'welcome' })] };
    case 'stream': return { kind: 'stream', length: spec.len, page: 200, entry: (n) => [`1794745${String(502183 - n * 61).padStart(6, '0')}-0`, [['kind', n % 4 ? 'paid' : 'created'], ['order', String(770100 - n)], ['amount', `${(12 + (n % 80)).toFixed(2)}`]]] };
    case 'bloom': return { kind: 'module', module: { label: 'Bloom filter', info: [{ name: 'Capacity', value: '1000000' }, { name: 'Size', value: '2516288' }, { name: 'Number of filters', value: '1' }, { name: 'Number of items inserted', value: '412880' }, { name: 'Expansion rate', value: '2' }], top: [], stats: [], samples: [], missing: [], unavailable: null } };
    case 'json': return { kind: 'string', raw: JSON.stringify({ checkout_v2: true, search_beta: false, regions: ['eu', 'us'], max_cart_items: 50, banner: { text: 'Winter sale', until: '2026-12-01' } }) };
    default: return { kind: 'string', raw: raw === 'evil\x1b[31mkey' ? 'v' : 'ok' };
  }
}

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
        analysis: analysisState(), monitor_limits: limits, nodes: [], picked: 0, polling,
        refused: [], sentinel: null, server: null, status: 'idle', tail: null, target: null, writes: false, writes_off: null,
      };
    }
    const p = s.profile;
    const up = s.status === 'connected';
    return {
      analysis: analysisState(), connected_at_unix: s.connectedAt, connection: { ...connection(p), db: s.db }, docker: s.docker, error: s.error, features, live: s.live, live_error: s.liveError,
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
  /** The `overview` event: GET /api/overview's body without `live` and `shards`. */
  const emitOverview = () => {
    if (!session || session.status !== 'connected') return;
    const { live: _live, shards: _shards, ...rest } = overview();
    emit('overview', rest);
  };

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
    if (tick % 15 === 0) for (const sub of Array.from(sources)) sub.send(': keep-alive\n\n');
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

  /** One open `/api/events` stream: SSE text as bytes. The first event is the state,
   *  as on the real server; a keep-alive comment goes out now and then. */
  let splitSeq = 0;
  function openStream(signal) {
    const encoder = new TextEncoder();
    let controller = null;
    let closed = false;
    const sub = {
      deliver(name, data) {
        sub.send(`event: ${name}\ndata: ${JSON.stringify(data)}\n\n`);
      },
      send(text) {
        if (closed) return;
        const bytes = encoder.encode(text);
        splitSeq += 1;
        const cut = (splitSeq * 7) % bytes.length;
        if (cut) {
          controller.enqueue(bytes.slice(0, cut));
          controller.enqueue(bytes.slice(cut));
        } else controller.enqueue(bytes);
      },
      /** The stream ends (redis-lens went away for a moment): the page tries again. */
      end() {
        if (closed) return;
        drop();
        controller.close();
      },
    };
    function drop() {
      closed = true;
      sources.delete(sub);
      if (!sources.size && timer) {
        clearInterval(timer);
        timer = null;
      }
    }
    const body = new ReadableStream({
      start(c) {
        controller = c;
        sources.add(sub);
        if (!timer) timer = setInterval(onTick, 1000);
        setTimeout(() => sub.deliver('state', state()), 0);
      },
      cancel() {
        drop();
      },
    });
    if (signal) {
      signal.addEventListener('abort', () => {
        if (closed) return;
        drop();
        try {
          controller.error(new Error('aborted'));
        } catch {
          // Already closed.
        }
      });
    }
    return new Response(body, { status: 200, headers: { 'Content-Type': 'text/event-stream' } });
  }

  // ---------- confirms ----------

  function ask(question, run, { keep = false, cancel = null } = {}) {
    confirmSeq += 1;
    const id = `${String(confirmSeq).padStart(4, '0')}${'0'.repeat(28)}`;
    confirms.set(id, { question, run, keep, cancel });
    return [409, { confirm: { expect: question.expect ?? null, id, kind: question.kind, text: question.text } }];
  }

  /** A new run: every pending question goes but a write's, whose answer is then refused. */
  function dropQuestions() {
    for (const [id, c] of Array.from(confirms)) if (!c.keep) confirms.delete(id);
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
      analysis: up ? overviewAnalysis() : null, figures: up ? figures(p) : null, figures_failed: null, findings,
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
  const line = (shown, status, text, replies = [], times = 1, dropped = 0) => [200, {
    dropped, dropped_note: dropped ? `(the first ${dropped} replies were dropped: the output keeps at most 4 MiB)` : null,
    line: shown, node: null, replies, route_note: null, sent: replies.length + dropped, status, text, times,
  }];
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
    if (upper === 'MOCK DROPPED') {
      // A repeat whose replies went past the 4 MiB the output keeps.
      const kept = [0, 1].map(() => one(['"…"'], ['"…"'], ['"…"'], ['[bulk string] …']));
      return line(text, 'ok', null, kept, 5, 3);
    }
    if (upper === 'MOCK LINK DOWN') {
      setTimeout(() => {
        for (const s of Array.from(sources)) s.end();
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

  // ---------- Browse, Analyze and snapshots ----------

  const PAGE = 1000;
  const SORTS = ['memory', 'keys', 'avg_size', 'no_ttl', 'name'];
  const STILL_CONNECTING = 'Still connecting: wait until the connection is ready.';
  const STOPPED_TEXT = 'This analysis was stopped early, so its results are partial and are not saved. Analyze again to save one.';
  const ANALYSIS_RUNS = 'An analysis is running: its patterns replace the key names.';
  let runSeq = 0;

  // Saved snapshots by profile. prod-cache-01 has three from the last two days, so its
  // Changes show a trend.
  const snaps = new Map();
  {
    const now = unix();
    const at = [now - 2 * 86400 - 1800, now - 86400 - 600, now - 3 * 3600];
    const grew = [0.9, 0.95, 0.985];
    snaps.set('prod-cache-01', at.map((t, n) => ({
      at: t, file: snapFile(t), sampled: 100000, complete: false, usedMemory: Math.round(15.02 * GIB * (0.955 + n * 0.015)),
      rows: SPACE.filter((spec) => n === 2 || spec.glob).map((spec) => {
        const keys = Math.round((spec.literal ? 1 : spec.keys) * (spec.grows ? grew[n] : 1));
        return { name: spec.name, keys, bytes: Math.round(keys * spec.avg) };
      }),
    })));
  }

  const snapName = () => session.profile.name;
  const snapsOf = () => snaps.get(snapName()) || [];
  const isCluster = (p) => p.sim.nodes.length > 0;
  const sizesOn = (p) => !p.sim.denied['MEMORY USAGE'];
  const freqOn = (p) => p.sim.lfu && !p.sim.denied['OBJECT FREQ'];
  const running = () => !!session && ['connecting', 'sampling', 'stopping'].includes(session.an.status);
  const keep = (map, id, value, cap) => {
    map.set(id, value);
    while (map.size > cap) map.delete(map.keys().next().value);
  };

  function freshAnalysis() {
    runSeq += 1;
    return { status: 'none', run: runSeq, depth: null, sampled: null, target: null, startedAt: null, error: null, note: null, report: null, stop: false };
  }

  /** The patterns of the database shown, with their key counts. */
  function space() {
    const p = session.profile;
    const specs = (session.db === 0 ? SPACE : OTHER_DBS[session.db] || []).filter((spec) => !session.wiped.has(spec.name));
    return specs.map((spec) => ({ spec, count: spec.literal ? 1 : spec.fixed ? spec.keys : Math.max(1, Math.round(spec.keys * p.sim.scale)) }));
  }
  const dbsize = () => space().reduce((n, x) => n + x.count, 0);

  function keyRow(spec, i, node = null) {
    const p = session.profile;
    const never = spec.ttl === null || i % 100 < spec.noTtl * 100;
    const ttl = never ? null : spec.ttl - ((i * 97) % Math.max(1, Math.floor(spec.ttl / 2)));
    const bytes = spec.literal ? spec.avg : Math.round(spec.avg * (0.55 + ((i * 37) % 90) / 100));
    const nodes = p.sim.nodes.length;
    return {
      ...nameOf(spec.key(i)), bytes: sizesOn(p) ? bytes : null, encoding: spec.enc, freq: freqOn(p) ? Math.min(255, spec.hot + ((i * 13) % 9)) : null,
      length: spec.len === null ? null : spec.literal ? spec.len : Math.max(1, Math.round(spec.len * (0.6 + ((i * 7) % 80) / 100))), length_unit: spec.unit,
      node: nodes ? node ?? (spec.literal ? spec.name.length % nodes : i % nodes) : null, ttl: ttlText(ttl), ttl_secs: ttl, type: spec.type,
    };
  }

  const refusals = (p) => [
    p.sim.denied['MEMORY USAGE'] ? 'MEMORY USAGE is refused for this user, so the keys have no sizes.' : null,
    p.sim.lfu && p.sim.denied['OBJECT FREQ'] ? 'OBJECT FREQ is refused for this user, so the keys have no LFU counters.' : null,
  ].filter(Boolean);

  // ---------- the pattern tree ----------

  function names() {
    if (!session.names) {
      const n = { pages: 0, read: 0, reading: true };
      session.names = n;
      const mine = connSeq;
      const step = () => {
        if (!session || connSeq !== mine || session.names !== n) return;
        const total = dbsize();
        n.pages += 1;
        n.read = Math.min(total, n.pages * PAGE);
        if (n.read >= total || n.pages >= 8) n.reading = false;
        else later(180).then(step);
      };
      later(180).then(step);
    }
    return session.names;
  }

  const bucket = (ttl) => (ttl < 60 ? 1 : ttl < 3600 ? 2 : ttl < 86400 ? 3 : ttl < 7 * 86400 ? 4 : ttl < 30 * 86400 ? 5 : 6);

  /** A pattern's row from an analysis (`r`), narrowed to a node when given. */
  function patternStats(r, spec, count, node = null) {
    const p = session.profile;
    const nodes = p.sim.nodes.length;
    let k = count;
    if (node !== null && nodes) k = spec.literal ? (spec.name.length % nodes === node ? 1 : 0) : Math.round(count / nodes);
    const est = Math.round(k * (spec.grows ? r.growth : 1));
    const sampled = r.complete ? est : Math.min(est, Math.max(est ? 1 : 0, Math.round(est * r.fraction)));
    const bytes = r.sizes ? Math.round(est * spec.avg) : null;
    const noTtl = Math.round(sampled * spec.noTtl);
    const hist = TTL_LABELS.map(() => 0);
    hist[0] = noTtl;
    if (spec.ttl !== null) hist[bucket(spec.ttl)] += sampled - noTtl;
    const big = r.sizes && spec.avg >= BIG_KEY;
    const base = r.base ? r.base.rows.find((x) => x.name === spec.name) : undefined;
    return {
      avg_bytes: r.sizes ? spec.avg : null, change: r.base ? changeOf(base, { keys: est, bytes }) : null, command: scanCommand(spec.glob), est_bytes: bytes, est_keys: est,
      examples: (spec.literal ? [0] : [0, 1, 2]).map((i) => keyRow(spec, i)), kind: 'pattern', literal: !!spec.literal, marked: !!spec.warn || big, names_seen: null,
      no_ttl: noTtl, no_ttl_ratio: sampled ? noTtl / sampled : null, pattern: nameOf(spec.name),
      reason: spec.warn || (big ? `${short(spec.avg)} in one key: a key this big blocks the server while it is read or deleted whole.` : null),
      sampled_bytes: r.sizes ? Math.round(sampled * spec.avg) : null, sampled_keys: sampled, share: null,
      size_max: r.sizes ? Math.round(spec.avg * (spec.literal ? 1 : 1.45)) : null, size_p50: r.sizes ? Math.round(spec.avg * (spec.literal ? 1 : 0.95)) : null, size_p99: r.sizes ? Math.round(spec.avg * (spec.literal ? 1 : 1.4)) : null,
      ttl_hist: hist, ttl_verdict: spec.noTtl >= 1 ? 'never expires' : spec.noTtl > 0 ? 'mostly expires' : 'every key expires', type: spec.type,
      verdict: spec.warn ? 'look' : big ? 'big' : null, warning: !!spec.warn,
    };
  }

  function changeOf(b, a) {
    if (!b) return { alarming: (a.bytes ?? 0) >= MIB, cause: 'new', cause_words: 'new', delta_bytes: a.bytes, delta_keys: a.keys };
    const dk = a.keys - b.keys;
    const dbytes = a.bytes === null || b.bytes === null ? null : a.bytes - b.bytes;
    const cause = dk === 0 && !dbytes ? 'unchanged' : dk > 0 ? 'more_keys' : dk < 0 ? 'fewer_keys' : dbytes > 0 ? 'bigger_keys' : 'smaller_keys';
    const words = { unchanged: 'unchanged', more_keys: 'more keys', fewer_keys: 'fewer keys', bigger_keys: 'bigger keys', smaller_keys: 'smaller keys' }[cause];
    return { alarming: (dbytes ?? 0) > 0 && b.bytes > 0 && dbytes / b.bytes >= 0.03, cause, cause_words: words, delta_bytes: dbytes, delta_keys: dk };
  }

  /** Sort pattern rows and put the patterns that share a prefix in one group. */
  function tree(items, sort) {
    const by = { memory: (r) => -(r.est_bytes ?? -1), keys: (r) => -r.est_keys, avg_size: (r) => -(r.avg_bytes ?? -1), no_ttl: (r) => -(r.no_ttl_ratio ?? -1), name: null }[sort];
    items.sort((x, y) => (by ? by(x.row) - by(y.row) : 0) || x.row.pattern.name.localeCompare(y.row.pattern.name));
    const out = [];
    const done = new Set();
    for (const { row, spec } of items) {
      const members = spec.group ? items.filter((x) => x.spec.group === spec.group) : [];
      if (members.length < 2) {
        out.push(row);
        continue;
      }
      if (done.has(spec.group)) continue;
      done.add(spec.group);
      const kids = members.map((x) => x.row);
      const sum = (f) => (kids.every((c) => typeof c[f] === 'number') ? kids.reduce((n, c) => n + c[f], 0) : null);
      const noTtl = sum('no_ttl');
      const sampled = sum('sampled_keys');
      out.push({
        children: kids, est_bytes: sum('est_bytes'), est_keys: sum('est_keys'), key: spec.group, kind: 'group', label: spec.group, marked: kids.some((c) => c.marked),
        no_ttl: noTtl, no_ttl_ratio: noTtl !== null && sampled ? noTtl / sampled : null, reason: null, sampled_keys: sampled, share: sum('share'), type: null, verdict: null,
      });
    }
    return out;
  }

  function analysisRows(r, node) {
    const items = space().map(({ spec, count }) => ({ spec, row: patternStats(r, spec, count, node) })).filter((x) => x.row.est_keys > 0);
    const total = items.reduce((n, x) => n + (r.sizes ? x.row.est_bytes : x.row.est_keys), 0) || 1;
    for (const x of items) x.row.share = (r.sizes ? x.row.est_bytes : x.row.est_keys) / total;
    return items;
  }

  function browse(q) {
    const p = session.profile;
    const a = session.an;
    const sort = q.get('sort');
    if (sort !== null && !SORTS.includes(sort)) return [400, { error: 'the sort must be one of memory, keys, avg_size, no_ttl or name' }];
    let node = null;
    if (q.has('node')) {
      node = /^\d+$/.test(q.get('node')) ? Number(q.get('node')) : -1;
      if (!a.report || !isCluster(p) || node < 0 || node >= p.sim.nodes.length) return [400, { error: "That node is not one of this analysis's nodes." }];
    }
    const total = dbsize();
    const base = { names: null, node: null, nodes: [], partial: null, previous: false, rows: [], sizes: false, sort: 'keys', sorts: ['keys', 'name'], source: 'none', total, ttl_labels: TTL_LABELS, why: null };
    const sorted = (r) => {
      const s = sort && (r.sizes || !['memory', 'avg_size'].includes(sort)) ? sort : r.sizes ? 'memory' : 'keys';
      return { sizes: r.sizes, sort: s, sorts: r.sizes ? SORTS : ['keys', 'no_ttl', 'name'] };
    };
    if (a.report) {
      const r = a.report;
      return [200, {
        ...base, ...sorted(r), node, nodes: isCluster(p) ? p.sim.nodes.map(([endpoint], index) => ({ endpoint, index, role: 'primary', sampled_keys: Math.round(r.sampled / p.sim.nodes.length), status: index === 2 ? 'partial' : 'scanned' })) : [],
        previous: running(), rows: tree(analysisRows(r, node), sorted(r).sort), source: 'analysis',
        why: running() ? 'An analysis is running: these are the last one\'s patterns until it is done.' : 'These patterns come from the analysis: analyze again to read them anew.',
      }];
    }
    if (running()) {
      if (!a.sampled) return [200, { ...base, why: 'An analysis is starting: its patterns come once it samples keys.' }];
      const r = { sizes: sizesOn(p), complete: false, fraction: a.sampled / Math.max(1, total), growth: 1, base: null };
      return [200, { ...base, ...sorted(r), partial: { sampled: a.sampled, target: a.target }, rows: tree(analysisRows(r, null), sorted(r).sort), source: 'partial', why: ANALYSIS_RUNS }];
    }
    if (!live()) return [200, { ...base, why: 'Key names are read once the live connection is open.' }];
    const n = names();
    const items = [];
    for (const { spec, count } of space()) {
      const seen = spec.literal ? (n.read >= total || n.pages >= 2 ? 1 : 0) : Math.round(count * (n.read / Math.max(1, total)));
      if (!seen) continue;
      items.push({
        spec,
        row: {
          avg_bytes: null, change: null, command: scanCommand(spec.glob), est_bytes: null, est_keys: n.read >= total || spec.literal ? seen : count, examples: [], kind: 'pattern', literal: !!spec.literal,
          marked: false, names_seen: seen, no_ttl: null, no_ttl_ratio: null, pattern: nameOf(spec.name), reason: null, sampled_bytes: null, sampled_keys: seen, share: seen / Math.max(1, n.read),
          size_max: null, size_p50: null, size_p99: null, ttl_hist: null, ttl_verdict: null, type: null, verdict: null, warning: false,
        },
      });
    }
    return [200, {
      ...base, sort: sort === 'name' ? 'name' : 'keys', source: 'names', rows: tree(items, sort === 'name' ? 'name' : 'keys'),
      names: {
        complete: n.read >= total, dbsize: total, done: !n.reading, error: null, first_pages: 8, missing: [], names_read: n.read,
        nodes_done: isCluster(p) ? [Math.min(p.sim.nodes.length, Math.ceil((n.read / Math.max(1, total)) * p.sim.nodes.length)), p.sim.nodes.length] : null,
        pages: n.pages, reading: n.reading, refused: [], skipped: [],
      },
    }];
  }

  async function moreNames(body) {
    if (!known(body, ['sort'])) return [400, { error: SHAPE }];
    if (session.an.report || running()) return [409, { error: 'The patterns come from the analysis: there are no more names to read.' }];
    const n = names();
    if (n.reading) return [409, { error: 'Still reading key names: wait for it.' }];
    const total = dbsize();
    if (n.read >= total) return [409, { error: `Every key name is read: ${total.toLocaleString('en-US')}.` }];
    await later(250);
    n.pages += 1;
    n.read = Math.min(total, n.read + PAGE);
    return browse(new URLSearchParams(body.sort ? { sort: body.sort } : {}));
  }

  // ---------- key lists ----------

  function listReply(l, from) {
    const p = session.profile;
    const rows = [];
    for (let j = from; j < l.loaded; j += 1) {
      const row = l.row(j);
      const own = session.store.get(fromHex(row.id));
      if (!own) rows.push(row);
      else if (!own.gone) rows.push({ ...row, ...rowOf(own.spec, own.i, fromHex(row.id)) });
    }
    const done = l.loaded >= l.total;
    const nodes = p.sim.nodes.length;
    return {
      cluster: nodes ? { at: done ? nodes - 1 : Math.min(nodes - 1, l.pages - 1), missing: [], narrowed: l.node, nodes_done: done ? nodes : Math.min(l.pages, nodes - 1), walk: p.sim.nodes.map((_, n) => n) } : null,
      capped: false, done, error: null, find: l.find, freq: freqOn(p), glob: l.glob, kind: l.kind, lfu: p.sim.lfu, list: l.id, loaded: l.loaded, max_rows: 50000, new_from: from, other_pattern: 0, page_keys: PAGE, pages: l.pages,
      pattern: l.pattern, refused: l.refused, rows, sizes: sizesOn(p), unreadable: 0,
    };
  }

  function newList(fields) {
    session.listSeq += 1;
    const l = { id: session.listSeq, pages: 1, refused: refusals(session.profile), find: null, pattern: null, glob: null, node: null, ...fields };
    l.loaded = Math.min(PAGE, l.total);
    keep(session.lists, l.id, l, 16);
    return l;
  }

  function patternList(q) {
    const id = q.get('pattern');
    if (!id || fromHex(id) === null) return [400, { error: 'a pattern is named by its hex id: ?pattern=757365723a2a' }];
    const p = session.profile;
    let node = null;
    if (q.has('node')) {
      node = /^\d+$/.test(q.get('node')) ? Number(q.get('node')) : -1;
      if (!isCluster(p) || node < 0 || node >= p.sim.nodes.length) return [400, { error: "That node is not one of this analysis's nodes." }];
    }
    const x = space().find(({ spec }) => hexOf(spec.name) === id);
    if (!x) return [404, { error: 'There is no such pattern in the patterns shown: read them again.' }];
    const nodes = p.sim.nodes.length;
    const narrowed = node !== null && nodes;
    const total = Math.min(50000, narrowed ? (x.spec.literal ? Number(x.spec.name.length % nodes === node) : Math.ceil(x.count / nodes)) : x.count);
    const l = newList({ kind: 'pattern', pattern: nameOf(x.spec.name), glob: x.spec.glob ? nameOf(x.spec.glob).name : nameOf(x.spec.name).name, node, total, row: (j) => keyRow(x.spec, narrowed && !x.spec.literal ? node + j * nodes : j, narrowed ? node : null) });
    return [200, listReply(l, 0)];
  }

  function findList(q) {
    if (!q.has('q')) return [400, { error: 'give the name or pattern to find: ?q=user' }];
    const text = q.get('q').trim();
    if (!text) return [422, { error: 'Type a name or a pattern to find.' }];
    if (text.length > 512) return [422, { error: 'A name to find has at most 512 characters.' }];
    const glob = /[*?[]/.test(text) ? text : `*${text.replace(/\\/g, '\\\\')}*`;
    if ((glob.match(/\*/g) || []).length > 8) return [422, { error: 'A pattern may have at most 8 * wildcards, so matching it stays cheap for the server.' }];
    const re = globRe(glob);
    const hits = [];
    for (const { spec, count } of space()) {
      for (let i = 0; i < Math.min(count, 4000) && hits.length < 50000; i += 1) if (re.test(spec.key(i))) hits.push([spec, i]);
    }
    const l = newList({ kind: 'find', find: { glob: nameOf(glob).name, text: nameOf(text).name }, glob: nameOf(glob).name, total: hits.length, row: (j) => keyRow(hits[j][0], hits[j][1]) });
    return [200, listReply(l, 0)];
  }

  function moreList(q) {
    if (!/^\d+$/.test(q.get('list') || '')) return [400, { error: 'give the key list: ?list=1' }];
    const l = session.lists.get(Number(q.get('list')));
    if (!l) return [404, { error: NO_LIST }];
    if (l.loaded >= l.total) return [409, { error: 'Every key of this list is loaded.' }];
    const from = l.loaded;
    l.loaded = Math.min(l.total, l.loaded + PAGE);
    l.pages += 1;
    // Like the real server: only the rows that are new, from `new_from` on.
    return [200, listReply(l, from)];
  }

  // ---------- values ----------

  function findKey(raw) {
    const own = session.store.get(raw);
    if (own) return own.gone ? null : [own.spec, own.i];
    for (const { spec, count } of space()) {
      if (spec.literal) {
        if (spec.key(0) === raw) return [spec, 0];
        continue;
      }
      if (!globRe(spec.glob).test(raw)) continue;
      for (let i = 0; i < Math.min(count, 50000); i += 1) if (spec.key(i) === raw) return [spec, i];
    }
    return null;
  }

  function viewReply(v) {
    const p = session.profile;
    const base = {
      capped: false, cluster: isCluster(p), command: null, cut: null, done: true, error: null, field_ttls: false, field_ttls_off: null, gone: false, items: [], key: nameOf(v.raw), kind: 'other',
      length: null, loaded: 0, loading: false, max_items: 10000, message: null, meta: null, module: null, more: false, notice: null, pages: v.pages, pattern: null, refused: [], rev: v.rev, type: null, value: null, view: v.id,
      writing: !!v.writing,
    };
    if (!v.spec) return { ...base, gone: true, notice: 'The key no longer exists: it expired or was deleted.' };
    const val = v.value;
    const out = { ...base, kind: val.kind, type: v.spec.type, meta: rowOf(v.spec, v.i, v.raw), pattern: nameOf(v.spec.name), refused: refusals(p) };
    const fieldTtls = p.sim.flavor === 'redis' && p.sim.version >= '7.4';
    if (val.kind === 'string') return { ...out, command: 'GETRANGE', loaded: 1, value: render(val.raw), notice: 'The value is loaded.' };
    if (val.kind === 'module') return { ...out, command: 'BF.INFO', loaded: 1, module: val.module, notice: 'The summary is loaded.' };
    const done = v.loaded >= val.length;
    const capped = !done && v.loaded >= 10000;
    const items = [];
    for (let n = 0; n < v.loaded; n += 1) {
      if (val.kind === 'stream') {
        const [id, fields] = val.entry(n);
        items.push({ at: null, fields: fields.map(([name, value]) => ({ name, value })), index: n, name: id, ttl: null, value: null });
      } else {
        const [name, value, fieldTtl] = val.item(n);
        items.push({ at: null, fields: null, index: n, name, ttl: val.kind === 'hash' && fieldTtls ? ttlText(fieldTtl ?? null) : null, value });
      }
    }
    const n = (x) => x.toLocaleString('en-US');
    return {
      ...out, capped, command: { hash: 'HSCAN', list: 'LRANGE', set: 'SSCAN', zset: 'ZRANGE', stream: 'XREVRANGE' }[val.kind], done: done || capped, items, length: val.length, loaded: v.loaded, more: !done && !capped,
      field_ttls: val.kind === 'hash' && fieldTtls,
      field_ttls_off: val.kind === 'hash' && !fieldTtls ? `Field TTLs need Redis 7.4 or later: ${p.sim.flavor === 'valkey' ? 'Valkey' : 'Redis'} ${p.sim.version} has none.` : null,
      notice: done ? `All ${n(val.length)} items are loaded.` : capped ? `The first ${n(v.loaded)} of ${n(val.length)} items are loaded: at most 10,000 are held.` : `${n(v.loaded)} of ${n(val.length)} items are loaded.`,
    };
  }

  function openView(q) {
    const raw = fromHex(q.get('key'));
    if (!q.get('key') || raw === null) return [400, { error: 'a key is named by the hex of its bytes: ?key=6b6579' }];
    const found = findKey(raw);
    session.viewSeq += 1;
    const v = { id: session.viewSeq, raw, spec: found ? found[0] : null, i: found ? found[1] : 0, loaded: 0, pages: 1, value: null, rev: 0, writing: false };
    if (found) {
      const own = session.store.get(raw);
      v.value = own ? storedValue(own) : valueOf(v.spec, v.i, raw);
      v.loaded = v.value.length ? Math.min(v.value.length, v.value.page || v.value.length) : 1;
    }
    keep(session.views, v.id, v, 8);
    return [200, viewReply(v)];
  }

  function moreView(q) {
    if (!/^\d+$/.test(q.get('view') || '')) return [400, { error: 'give the open value: ?view=1' }];
    const v = session.views.get(Number(q.get('view')));
    if (!v) return [404, { error: NO_VIEW }];
    const val = v.value;
    if (!val || !val.length || v.loaded >= Math.min(val.length, 10000)) return [409, { error: 'Every item of this value is loaded.' }];
    v.loaded = Math.min(val.length, 10000, v.loaded + (val.page || val.length));
    v.pages += 1;
    return [200, viewReply(v)];
  }

  function itemOf(q) {
    if (!/^\d+$/.test(q.get('view') || '') || (q.has('index') && !/^\d+$/.test(q.get('index')))) return [400, { error: 'give the open value and the item: ?view=1&index=0' }];
    const format = q.get('format');
    if (format !== null && format.toLowerCase() !== 'msgpack' && !FORMATS.some((f) => f.toLowerCase() === format.toLowerCase())) return [400, { error: BAD_FORMAT }];
    const v = session.views.get(Number(q.get('view')));
    if (!v) return [404, { error: NO_VIEW }];
    const val = v.value;
    const index = Number(q.get('index') || 0);
    const noItem = [422, { error: 'There is no such item in what is loaded.' }];
    if (!val || val.kind === 'module') return noItem;
    let raw;
    let name = null;
    let field = null;
    if (val.kind === 'string') {
      if (index) return noItem;
      raw = val.raw;
    } else {
      if (index >= v.loaded) return noItem;
      if (val.kind === 'stream') {
        if (!q.has('field')) return [422, { error: 'A stream entry has several fields: open one field to see or copy its value.' }];
        field = Number(q.get('field'));
        const fields = val.entry(index)[1];
        if (!fields[field]) return noItem;
        [name, raw] = fields[field];
      } else [name, raw] = val.item(index);
    }
    return [200, { field, index, name, shown: render(raw, format) }];
  }

  // ---------- writes (phase 3) ----------
  //
  // What the session wrote is kept over the generated keyspace in `session.store`, by key:
  // the value as it is now (its items materialised, at most 10,000 of them), its TTL, or
  // `gone`. Every write goes through `gate()`, as the server's one write gate: read-only
  // first, a command the server refused, then the question prod or a delete asks.

  const CHANGED = 'Not sent: the connection changed since the question was asked.';
  const STALE = 'The value was read again since the page showed it: look at it again, then try once more.';
  const WRITING = 'The last write has not answered yet.';
  const KEY_GONE = 'The key no longer exists.';
  const NO_ITEM = 'There is no such item in what is loaded.';
  const BAD_HEX = 'a value given as hex must be an even number of hex digits';
  const BAD_TTL = 'Type a TTL such as 90s, 10m, 24h or 7d, or - to remove it.';
  const BAD_SCORE = 'Type the score as a number, such as 1.5, or inf or -inf.';
  const ZERO_TTL = 'A TTL of 0 would delete the key: delete the key instead.';
  const BULK_GONE = 'That bulk action is no longer open: start it again from its pattern.';
  const BULK_BUSY = 'A bulk action is running already: wait for it to end, or stop it first.';
  // What every key write, database switch, analysis, connect and disconnect gets while a
  // bulk action counts or runs.
  const BULK_RUNS = 'A bulk action is running: wait for it to end, or stop it first.';
  const UNDO = " This can't be undone.";
  const NEW_TYPES = ['string', 'hash', 'list', 'set', 'zset', 'stream'];
  const UNITS = { string: 'bytes', hash: 'fields', list: 'elements', set: 'members', zset: 'members', stream: 'entries' };
  const ENCODINGS = { string: 'embstr', hash: 'listpack', list: 'quicklist', set: 'listpack', zset: 'listpack', stream: 'stream' };
  let bulkSeq = 0;
  const isErr = (x) => Array.isArray(x);
  const n0 = (x) => x.toLocaleString('en-US');

  /** A value field of a request: text as typed, or `{hex}` as its bytes. */
  function bytesOf(x) {
    if (typeof x === 'string') return x;
    if (x && typeof x === 'object' && !Array.isArray(x) && Object.keys(x).length === 1 && typeof x.hex === 'string') {
      const h = x.hex.toLowerCase();
      if (!/^(?:[0-9a-f]{2})*$/.test(h)) return [400, { error: BAD_HEX }];
      return (h.match(/../g) || []).map((b) => String.fromCharCode(parseInt(b, 16))).join('');
    }
    return [400, { error: SHAPE }];
  }

  /** A TTL as typed: seconds, `persist` for `-`, or an error (what was typed is never echoed). */
  function ttlOf(x, zero = ZERO_TTL, bad = BAD_TTL) {
    if (typeof x === 'number') return Number.isInteger(x) && x > 0 ? x : x === 0 ? [422, { error: zero }] : [422, { error: bad }];
    if (typeof x !== 'string') return [400, { error: SHAPE }];
    const t = x.trim().toLowerCase();
    if (['-', 'persist', 'none'].includes(t)) return 'persist';
    if (!/^(?:\d+[smhdw]?)+$/.test(t)) return [422, { error: bad }];
    const unit = { s: 1, m: 60, h: 3600, d: 86400, w: 604800 };
    let secs = 0;
    for (const [, n, u] of t.matchAll(/(\d+)([smhdw]?)/g)) secs += Number(n) * unit[u || 's'];
    return secs > 0 ? secs : [422, { error: zero }];
  }

  /** A TTL as the TUI's message gives it: `1h 30m`, `1h 00m`, `5m 07s`. */
  function ttlSet(secs) {
    const d = Math.floor(secs / 86400);
    const hh = Math.floor((secs % 86400) / 3600);
    const m = Math.floor((secs % 3600) / 60);
    const s = secs % 60;
    const two = (x) => String(x).padStart(2, '0');
    if (d) return `${d}d ${two(hh)}h`;
    if (hh) return `${hh}h ${two(m)}m`;
    if (m) return `${m}m ${two(s)}s`;
    return `${s}s`;
  }

  const scoreOf = (x) => (typeof x === 'string' && /^\s*(?:[+-]?inf|[+-]?(?:\d+\.?\d*|\.\d+)(?:e[+-]?\d+)?)\s*$/i.test(x) ? x.trim().toLowerCase() : null);
  /** A value inside a question: short, and never with control characters. */
  const said = (raw) => (raw === null || raw === undefined ? '' : /[\x00-\x1f\x7f-\xff]/.test(raw) ? `<${n0(raw.length)} bytes>` : raw.length > 40 ? `${raw.slice(0, 40)}…` : raw);

  /** The key `raw` as this session wrote it, made the first time it is written. */
  function owned(v) {
    let s = session.store.get(v.raw);
    if (s && !s.gone) return s;
    const val = v.value;
    s = { spec: v.spec, i: v.i, kind: val.kind, ttl: keyRow(v.spec, v.i).ttl_secs, page: val.page, tail: 0 };
    if (val.kind === 'string') s.raw = val.raw;
    else if (val.kind === 'module') s.module = val.module;
    else {
      const n = Math.min(val.length, 10000);
      s.items = Array.from({ length: n }, (_, k) => (val.kind === 'stream' ? val.entry(k) : [...val.item(k)]));
      s.tail = val.length - n;
    }
    session.store.set(v.raw, s);
    return s;
  }

  /** The value a stored key holds now, in `valueOf`'s shape. */
  function storedValue(s) {
    if (s.kind === 'string') return { kind: 'string', raw: s.raw };
    if (s.kind === 'module') return { kind: 'module', module: s.module };
    return { kind: s.kind, length: s.items.length + s.tail, page: s.page, item: (n) => s.items[n], entry: (n) => s.items[n] };
  }

  /** A key's row, with what this session wrote. */
  function rowOf(spec, i, raw = spec.key(i)) {
    const row = keyRow(spec, i);
    const s = session.store.get(raw);
    if (!s || s.gone) return row;
    const length = s.kind === 'string' ? s.raw.length : s.kind === 'module' ? row.length : s.items.length + s.tail;
    return { ...row, ...nameOf(raw), length, ttl: ttlText(s.ttl), ttl_secs: s.ttl, type: s.spec.type };
  }

  /** A made-up pattern entry for a key this session added. */
  const newSpec = (raw, type) => ({
    name: raw, glob: null, literal: true, type, keys: 1, avg: 64, ttl: null, noTtl: 1, enc: ENCODINGS[type], unit: UNITS[type], len: 1, hot: 0, key: () => raw, value: null,
  });

  /** The server refusing a write for this user, once: then the command is off. */
  function refusedOnce(cmds) {
    const cmd = cmds.find((c) => (session.profile.sim.writeDenied || []).includes(c));
    if (!cmd) return null;
    if (!session.refused.includes(cmd)) {
      session.refused.push(cmd);
      emitState();
    }
    return [422, { error: `Not written: the server refused ${cmd} for this user.` }];
  }

  /**
   * The one write gate. `op`: `cmds` (the commands sent), `text` (the command as the
   * question gives it), `destructive`, `typed` (what prod types for a destructive one) and,
   * for a bulk action, `ask`/`type` (its own questions). `send()` is the write.
   */
  function gate(op, send) {
    const p = session.profile;
    if (p.read_only) return [422, { error: readOnly(p) }];
    const off = op.cmds.find((c) => session.refused.includes(c));
    if (off) return [422, { error: `Not sent: the server refused ${off} for this user.` }];
    const conn = connSeq;
    const run = session.an.run;
    const answer = () => {
      if (!session || connSeq !== conn || session.an.run !== run) return [409, { error: CHANGED }];
      if (session.profile.read_only) return [422, { error: readOnly(session.profile) }];
      return send();
    };
    if (op.ask) {
      const question = p.env === 'prod' ? { kind: 'type', expect: op.typed, text: op.type } : { kind: 'ask', text: op.ask };
      return ask(question, answer, { keep: true, cancel: op.cancel });
    }
    if (p.env === 'prod') {
      if (op.destructive) return ask({ kind: 'type', expect: op.typed, text: `Type ${op.typed} to send ${op.text} to ${p.name}.${UNDO}` }, answer, { keep: true });
      return ask({ kind: 'ask', text: `Send ${op.text} to ${p.name}?` }, answer, { keep: true });
    }
    if (op.destructive) return ask({ kind: 'ask', text: `Send ${op.text} to ${p.name}?${UNDO}` }, answer, { keep: true });
    return send();
  }

  /** The open value a write names, after the read-only rule and the body's shape. */
  function writeView(body, fields, { rev = false, index = false } = {}) {
    if (!known(body, fields) || !Number.isInteger(body.view)) return [400, { error: SHAPE }];
    if ((rev && !Number.isInteger(body.rev)) || (index && !Number.isInteger(body.index))) return [400, { error: SHAPE }];
    if (session.profile.read_only) return [422, { error: readOnly(session.profile) }];
    if (bulkBusy()) return [409, { error: BULK_RUNS }];
    if (!live()) return [409, { error: NOT_LIVE }];
    const v = session.views.get(body.view);
    if (!v) return [404, { error: NO_VIEW }];
    if (v.writing) return [409, { error: WRITING }];
    if (rev && body.rev !== v.rev) return [409, { error: STALE }];
    if (!v.spec) return [422, { error: KEY_GONE }];
    if (index && v.value.kind !== 'string' && v.value.kind !== 'module' && (body.index < 0 || body.index >= v.loaded)) return [422, { error: NO_ITEM }];
    return v;
  }

  /** Sends a write: one at a time per value, refused by the server for a denied command. */
  async function sending(v, cmds, apply) {
    v.writing = true;
    try {
      await later(80);
      if (!session || !session.views.has(v.id)) return [409, { error: NOT_LIVE }];
      return refusedOnce(cmds) || apply();
    } finally {
      v.writing = false;
    }
  }

  /** A write's reply: the value read again (when `reread`), the row read again. */
  function written(v, { done = true, message, reply, reread = true }) {
    if (reread && done) {
      const s = session.store.get(v.raw);
      v.rev += 1;
      if (s && s.gone) v.spec = null;
      else if (s) {
        v.value = storedValue(s);
        v.loaded = v.value.length ? Math.min(v.value.length, v.value.page || v.value.length) : v.value.kind === 'string' || v.value.kind === 'module' ? 1 : 0;
        v.pages = 1;
      }
    }
    return [200, { closed: false, done, key: nameOf(v.raw), message, reply, row: v.spec ? rowOf(v.spec, v.i, v.raw) : null, sent: true, value: viewReply(v) }];
  }

  /** A collection that lost its last item is deleted by the server: the key goes. */
  function emptied(v, s) {
    if (s.items && s.items.length + s.tail === 0) {
      session.store.set(v.raw, { gone: true });
      return true;
    }
    return false;
  }

  function editValue(body) {
    const v = writeView(body, ['view', 'rev', 'index', 'value'], { rev: true });
    if (isErr(v)) return v;
    const kind = v.value.kind;
    const index = body.index ?? 0;
    if (!Number.isInteger(index)) return [400, { error: SHAPE }];
    if (kind === 'stream') return [422, { error: "A stream entry can't be edited: add an entry, or delete this one." }];
    if (kind === 'module') return [422, { error: `redis-lens does not edit values of type ${v.spec.type}.` }];
    if (kind === 'string' ? index !== 0 : index < 0 || index >= v.loaded) return [422, { error: NO_ITEM }];
    const value = bytesOf(body.value);
    if (isErr(value)) return value;
    const score = kind === 'zset' ? scoreOf(value) : null;
    if (kind === 'zset' && score === null) return [422, { error: BAD_SCORE }];
    const key = nameOf(v.raw).name;
    const before = kind === 'string' ? v.value.raw : v.value.item(index);
    const now = kind === 'string' ? before : before[1];
    if ((kind === 'zset' ? String(Number(now)) === String(Number(score)) : now === value)) {
      return [200, { closed: false, done: false, key: nameOf(v.raw), message: 'Not changed, so nothing was sent.', reply: null, sent: false }];
    }
    const op = {
      string: { cmds: ['SET'], text: `SET ${key} ${said(value)} KEEPTTL (in MULTI/EXEC after WATCH, only if unchanged)` },
      hash: { cmds: ['HSET'], text: `HSET ${key} ${said(before[0])} ${said(value)}` },
      list: { cmds: ['LSET'], text: `LSET ${key} ${index} ${said(value)} (only while it is the element loaded)` },
      set: { cmds: ['SREM', 'SADD'], text: `SREM ${key} ${said(before[1])} and SADD ${key} ${said(value)} (in MULTI/EXEC)` },
      zset: { cmds: ['ZADD'], text: `ZADD ${key} XX ${score} ${said(before[0])}` },
    }[kind];
    return gate(op, () => sending(v, op.cmds, () => {
      const s = owned(v);
      if (kind === 'string') s.raw = value;
      else s.items[index][1] = kind === 'zset' ? score : value;
      return written(v, { message: 'Saved.', reply: kind === 'hash' || kind === 'zset' ? { kind: 'int', value: 0 } : { kind: 'ok' } });
    }));
  }

  function addItem(body) {
    const v = writeView(body, ['view', 'field', 'value', 'score']);
    if (isErr(v)) return v;
    const kind = v.value.kind;
    if (!['hash', 'list', 'set', 'zset', 'stream'].includes(kind)) return [422, { error: 'Adding works on a hash, list, set, sorted set or stream: edit this value instead.' }];
    const value = bytesOf(body.value);
    if (isErr(value)) return value;
    const field = body.field === undefined ? null : bytesOf(body.field);
    if (isErr(field)) return field;
    if ((kind === 'hash' || kind === 'stream') && !field) return [422, { error: 'Type the field.' }];
    const score = kind === 'zset' ? scoreOf(body.score) : null;
    if (kind === 'zset' && score === null) return [422, { error: BAD_SCORE }];
    const key = nameOf(v.raw).name;
    const op = {
      hash: { cmds: ['HSETNX'], text: `HSETNX ${key} ${said(field)} ${said(value)}` },
      list: { cmds: ['RPUSH'], text: `RPUSH ${key} ${said(value)}` },
      set: { cmds: ['SADD'], text: `SADD ${key} ${said(value)}` },
      zset: { cmds: ['ZADD'], text: `ZADD ${key} NX ${score} ${said(value)}` },
      stream: { cmds: ['XADD'], text: `XADD ${key} * ${said(field)} ${said(value)}` },
    }[kind];
    return gate(op, () => sending(v, op.cmds, () => {
      const s = owned(v);
      const exists = kind === 'hash' ? (it) => it[0] === field : kind === 'set' ? (it) => it[1] === value : kind === 'zset' ? (it) => it[0] === value : () => false;
      if (s.items.some(exists)) {
        return written(v, { done: false, message: kind === 'hash' ? 'Not added: the field exists already. Edit it instead.' : 'Not added: the member exists already.', reply: { kind: 'int', value: 0 } });
      }
      if (kind === 'hash') s.items.push([field, value]);
      else if (kind === 'set') s.items.push([null, value]);
      else if (kind === 'zset') s.items.push([value, score]);
      else if (kind === 'list') {
        if (s.tail) s.tail += 1;
        else s.items.push([null, value]);
      } else s.items.unshift([`${Date.now()}-0`, [[field, value]]]);
      return written(v, { message: 'Added.', reply: { kind: 'int', value: kind === 'list' ? s.items.length + s.tail : 1 } });
    }));
  }

  function setTtl(body) {
    const v = writeView(body, ['view', 'ttl', 'field', 'rev'], { rev: body.field !== undefined });
    if (isErr(v)) return v;
    const ttl = ttlOf(body.ttl);
    if (isErr(ttl)) return ttl;
    const key = nameOf(v.raw).name;
    if (body.field !== undefined) {
      if (!Number.isInteger(body.field)) return [400, { error: SHAPE }];
      if (v.value.kind !== 'hash') return [422, { error: 'Only a hash field has a TTL of its own.' }];
      const p = session.profile;
      if (!(p.sim.flavor === 'redis' && p.sim.version >= '7.4')) return [422, { error: 'Field TTLs need Redis 7.4 or later, or Valkey 9.0 or later.' }];
      if (body.field < 0 || body.field >= v.loaded) return [422, { error: NO_ITEM }];
      const name = v.value.item(body.field)[0];
      const op = ttl === 'persist' ? { cmds: ['HPERSIST'], text: `HPERSIST ${key} FIELDS 1 ${said(name)}` } : { cmds: ['HEXPIRE'], text: `HEXPIRE ${key} ${ttl} FIELDS 1 ${said(name)}` };
      return gate(op, () => sending(v, op.cmds, () => {
        const s = owned(v);
        s.items[body.field][2] = ttl === 'persist' ? null : ttl;
        return written(v, { message: ttl === 'persist' ? 'Field TTL removed.' : 'Field TTL set.', reply: { kind: 'int', value: 1 } });
      }));
    }
    const op = ttl === 'persist' ? { cmds: ['PERSIST'], text: `PERSIST ${key}` } : { cmds: ['EXPIRE'], text: `EXPIRE ${key} ${ttl}` };
    return gate(op, () => sending(v, op.cmds, () => {
      const s = owned(v);
      if (ttl === 'persist') {
        const had = s.ttl !== null;
        s.ttl = null;
        return written(v, { done: had, message: had ? 'TTL removed: the key no longer expires.' : 'The key had no TTL.', reply: { kind: 'int', value: had ? 1 : 0 }, reread: false });
      }
      s.ttl = ttl;
      return written(v, { message: `TTL set to ${ttlSet(ttl)}.`, reply: { kind: 'int', value: 1 }, reread: false });
    }));
  }

  function renameKey(body) {
    const v = writeView(body, ['view', 'name']);
    if (isErr(v)) return v;
    if (typeof body.name !== 'string') return [400, { error: SHAPE }];
    if (!body.name) return [422, { error: 'Type the new name.' }];
    const to = body.name;
    const key = nameOf(v.raw).name;
    const op = { cmds: ['RENAMENX'], text: `RENAMENX ${key} ${nameOf(to).name}` };
    return gate(op, () => sending(v, op.cmds, () => {
      if (to === v.raw || findKey(to)) {
        return written(v, { done: false, message: `Not renamed: ${nameOf(to).name} exists already, and a rename never overwrites a key.`, reply: { kind: 'int', value: 0 }, reread: false });
      }
      const s = owned(v);
      session.store.set(v.raw, { gone: true });
      session.store.set(to, s);
      v.raw = to;
      return written(v, { message: `Renamed to ${nameOf(to).name}.`, reply: { kind: 'int', value: 1 }, reread: false });
    }));
  }

  function removeItem(body) {
    const v = writeView(body, ['view', 'rev', 'index'], { rev: true, index: true });
    if (isErr(v)) return v;
    const kind = v.value.kind;
    if (!['hash', 'list', 'set', 'zset', 'stream'].includes(kind)) return [422, { error: 'Removing works on a field, element, member or entry: delete the key instead.' }];
    const key = nameOf(v.raw).name;
    const it = kind === 'stream' ? v.value.entry(body.index) : v.value.item(body.index);
    const op = {
      hash: { cmds: ['HDEL'], text: `HDEL ${key} ${said(it[0])}` },
      list: { cmds: ['LSET', 'LREM'], text: `LSET ${key} ${body.index} <marker> then LREM ${key} 1 <marker> (only while it is the element loaded)` },
      set: { cmds: ['SREM'], text: `SREM ${key} ${said(it[1])}` },
      zset: { cmds: ['ZREM'], text: `ZREM ${key} ${said(it[0])}` },
      stream: { cmds: ['XDEL'], text: `XDEL ${key} ${it[0]}` },
    }[kind];
    return gate({ ...op, destructive: true, typed: key }, () => sending(v, op.cmds, () => {
      const s = owned(v);
      s.items.splice(body.index, 1);
      if (!emptied(v, s)) return written(v, { message: 'Deleted.', reply: { kind: 'int', value: 1 } });
      // The last item: the server deleted the key, so the value read again is gone.
      v.rev += 1;
      v.spec = null;
      return [200, { closed: false, done: true, key: nameOf(v.raw), message: 'Deleted.', reply: { kind: 'int', value: 1 }, row: null, sent: true, value: viewReply(v) }];
    }));
  }

  function deleteKey(body) {
    const v = writeView(body, ['view']);
    if (isErr(v)) return v;
    const key = nameOf(v.raw).name;
    return gate({ cmds: ['UNLINK'], text: `UNLINK ${key}`, destructive: true, typed: key }, () => sending(v, ['UNLINK'], () => {
      session.store.set(v.raw, { gone: true });
      session.views.delete(v.id);
      return [200, { closed: true, done: true, key: nameOf(v.raw), message: `Deleted ${key}.`, reply: { kind: 'int', value: 1 }, row: null, sent: true, value: null }];
    }));
  }

  function addKey(body) {
    if (!known(body, ['list', 'name', 'type', 'field', 'value', 'score']) || typeof body.name !== 'string' || typeof body.type !== 'string') return [400, { error: SHAPE }];
    if (body.list !== undefined && !Number.isInteger(body.list)) return [400, { error: SHAPE }];
    if (!NEW_TYPES.includes(body.type)) return [400, { error: 'the type must be string, hash, list, set, zset or stream' }];
    const p = session.profile;
    if (p.read_only) return [422, { error: readOnly(p) }];
    if (bulkBusy()) return [409, { error: BULK_RUNS }];
    if (!live()) return [409, { error: NOT_LIVE }];
    if (body.list !== undefined && !session.lists.has(body.list)) return [404, { error: NO_LIST }];
    const raw = body.name;
    if (!raw) return [422, { error: "Type the new key's name." }];
    const type = body.type;
    const value = bytesOf(body.value);
    if (isErr(value)) return value;
    const field = body.field === undefined ? null : bytesOf(body.field);
    if (isErr(field)) return field;
    if ((type === 'hash' || type === 'stream') && !field) return [422, { error: 'Type the field.' }];
    const score = type === 'zset' ? scoreOf(body.score) : null;
    if (type === 'zset' && score === null) return [422, { error: BAD_SCORE }];
    const key = nameOf(raw).name;
    const cmd = { string: 'SET', hash: 'HSET', list: 'RPUSH', set: 'SADD', zset: 'ZADD', stream: 'XADD' }[type];
    const op = {
      cmds: [cmd],
      text: {
        string: `SET ${key} ${said(value)} NX`, hash: `HSET ${key} ${said(field)} ${said(value)}`, list: `RPUSH ${key} ${said(value)}`, set: `SADD ${key} ${said(value)}`,
        zset: `ZADD ${key} ${score} ${said(value)}`, stream: `XADD ${key} * ${said(field)} ${said(value)}`,
      }[type],
    };
    return gate(op, async () => {
      await later(80);
      if (!session) return [409, { error: NOT_LIVE }];
      const refused = refusedOnce(op.cmds);
      if (refused) return refused;
      if (findKey(raw)) {
        return [200, { closed: false, done: false, key: nameOf(raw), message: `Not added: ${key} exists already, and adding a key never overwrites one.`, reply: { kind: 'exists' }, row: null, sent: true, value: null }];
      }
      const spec = newSpec(raw, type);
      const s = { spec, i: 0, kind: type, ttl: null, page: undefined, tail: 0 };
      if (type === 'string') s.raw = value;
      else if (type === 'hash') s.items = [[field, value]];
      else if (type === 'list' || type === 'set') s.items = [[null, value]];
      else if (type === 'zset') s.items = [[value, score]];
      else s.items = [[`${Date.now()}-0`, [[field, value]]]];
      session.store.set(raw, s);
      session.viewSeq += 1;
      const v = { id: session.viewSeq, raw, spec, i: 0, loaded: 1, pages: 1, value: storedValue(s), rev: 0, writing: false };
      keep(session.views, v.id, v, 8);
      return [200, { closed: false, done: true, key: nameOf(raw), message: `Added ${key}.`, reply: type === 'string' ? { kind: 'ok' } : { kind: 'int', value: 1 }, row: rowOf(spec, 0, raw), sent: true, value: viewReply(v) }];
    });
  }

  // ---------- bulk actions ----------

  const BULK_FIELDS = ['action', 'at', 'count', 'error', 'glob', 'id', 'lines', 'nodes', 'pattern', 'prod', 'profile', 'run', 'stage', 'summary', 'title', 'ttl', 'ttl_secs'];
  const bulkBody = (b) => (b ? JSON.parse(JSON.stringify(Object.fromEntries(BULK_FIELDS.map((f) => [f, b[f]])))) : null);
  const emitBulk = () => emit('bulk', { bulk: bulkBody(session && session.bulk) });
  const bulkBusy = () => !!session && !!session.bulk && ['counting', 'running'].includes(session.bulk.stage);
  const bulkFile = (b) => `~/.local/share/redis-lens/bulk/${b.profile}/${new Date().toISOString().replace(/[-:]/g, '').slice(0, 15)}Z-${b.action}.txt`;

  function bulkSummary(b) {
    const c = b.count;
    const lines = b.action === 'delete'
      ? [`Dry run: ${n0(c.matched)} keys of the pattern will be deleted with UNLINK.`]
      : [`Dry run: ${n0(c.matched)} keys of the pattern, ${n0(c.with_ttl)} with a TTL already.`, `${n0(c.to_change)} keys without a TTL get ${b.ttl}; the others are left alone.`];
    if (c.other_pattern) lines.push(`${n0(c.other_pattern)} other keys matched the glob but belong to other patterns: left alone.`);
    return lines;
  }

  function startBulk(body) {
    if (!known(body, ['pattern', 'action', 'ttl']) || typeof body.pattern !== 'string') return [400, { error: SHAPE }];
    if (!['expire', 'delete'].includes(body.action)) return [400, { error: 'the action must be expire or delete' }];
    if (body.action === 'delete' && body.ttl !== undefined) return [400, { error: SHAPE }];
    const p = session.profile;
    const cmd = body.action === 'delete' ? 'UNLINK' : 'EXPIRE';
    if (p.read_only) return [422, { error: readOnly(p) }];
    if (session.refused.includes(cmd)) return [422, { error: `Not sent: the server refused ${cmd} for this user.` }];
    if (bulkBusy()) return [409, { error: BULK_BUSY }];
    let secs = null;
    let ttl = null;
    if (body.action === 'expire') {
      const t = body.ttl ?? '24h';
      secs = ttlOf(t, 'the TTL must be at least 1s', 'not a duration: use 90s, 10m, 24h or 7d');
      if (secs === 'persist') secs = [422, { error: 'not a duration: use 90s, 10m, 24h or 7d' }];
      if (isErr(secs)) return secs;
      ttl = typeof t === 'number' ? `${t}s` : t.trim();
    }
    if (!live()) return [409, { error: NOT_LIVE }];
    const x = space().find(({ spec }) => hexOf(spec.name) === body.pattern);
    if (!x) return [404, { error: 'There is no such pattern in the patterns shown: read them again.' }];
    if (!x.spec.glob) return [422, { error: 'This pattern has no glob to walk: open its key and delete it there.' }];
    const re = globRe(x.spec.glob);
    const other = space().filter((y) => y.spec !== x.spec && re.test(y.spec.key(0))).reduce((n, y) => n + y.count, 0);
    const nodes = p.sim.nodes;
    bulkSeq += 1;
    const b = {
      id: bulkSeq, pattern: nameOf(x.spec.name), glob: nameOf(x.spec.glob).name, action: body.action, ttl, ttl_secs: secs,
      title: body.action === 'expire' ? 'Set a TTL on every key of the pattern' : 'Delete every key of the pattern', profile: p.name, prod: p.env === 'prod',
      stage: 'counting', count: { matched: 0, other_pattern: 0, to_change: 0, unreadable: 0, with_ttl: 0 }, summary: [],
      nodes: nodes.map(([endpoint], node) => ({ endpoint, matched: 0, node })), at: nodes.length ? 0 : null, run: null, lines: [], error: false,
      spec: x.spec, total: x.count, other, ranOn: session.an.run, conn: connSeq, stop: false,
    };
    session.bulk = b;
    const steps = 6;
    let step = 0;
    const tickCount = () => {
      if (!session || session.bulk !== b || b.stage !== 'counting') return;
      step += 1;
      const share = step / steps;
      const matched = Math.round(b.total * share);
      const withTtl = Math.round(matched * (1 - b.spec.noTtl));
      Object.assign(b.count, { matched, with_ttl: withTtl, other_pattern: Math.round(b.other * share), to_change: b.action === 'delete' ? matched : matched - withTtl });
      if (nodes.length) {
        b.at = Math.min(nodes.length - 1, Math.floor(share * nodes.length));
        b.nodes.forEach((n, k) => { n.matched = Math.round(matched / nodes.length) + (k === 0 ? matched % nodes.length : 0); });
      }
      if (step >= steps) {
        b.summary = bulkSummary(b).filter(Boolean);
        if (!b.count.to_change) {
          b.stage = 'done';
          b.lines = [b.action === 'expire' ? `Every key of ${b.pattern.name} (${n0(b.count.matched)}) has a TTL already. Nothing was sent.` : `No key of ${b.pattern.name} is left. Nothing was sent.`];
        } else b.stage = 'counted';
        if (nodes.length) b.at = null;
      }
      emitBulk();
      if (b.stage === 'counting') later(200).then(tickCount);
    };
    later(200).then(tickCount);
    return [202, { bulk: bulkBody(b) }];
  }

  function bulkEnd(b, lines, error = false) {
    b.stage = 'done';
    b.lines = lines;
    b.error = error;
    if (b.nodes.length) b.at = null;
    emitBulk();
  }

  function bulkDone(b, head) {
    const r = b.run;
    const verb = b.action === 'delete' ? `deleted ${n0(r.changed)} keys of ${b.pattern.name}` : `set a TTL of ${b.ttl} on ${n0(r.changed)} keys of ${b.pattern.name}`;
    const lines = [head(verb)];
    if (b.action === 'expire' && r.skipped) lines.push(`${n0(r.skipped)} keys left alone: a TTL already, or gone.`);
    if (r.failed) lines.push(`${n0(r.failed)} keys failed.`);
    lines.push(b.nodes.length ? `Key names, each after its node and a tab: ${r.file}` : `Key names: ${r.file}`);
    if (b.action === 'expire') lines.push('PERSIST on those keys undoes it.');
    return lines;
  }

  function runBulk(body) {
    if (!known(body, ['bulk']) || !Number.isInteger(body.bulk)) return [400, { error: SHAPE }];
    const p = session.profile;
    if (p.read_only) return [422, { error: readOnly(p) }];
    const b = session.bulk;
    if (!b || b.id !== body.bulk) return [404, { error: BULK_GONE }];
    if (b.stage === 'counting') return [409, { error: 'The dry run has not finished: wait for its count.' }];
    if (b.stage === 'running') return [409, { error: 'That bulk action has started already.' }];
    if (b.stage === 'done') return b.run || b.count.to_change ? [404, { error: BULK_GONE }] : [422, { error: 'Nothing to run: the dry run found nothing to change.' }];
    if (b.ranOn !== session.an.run || b.conn !== connSeq) return [409, { error: CHANGED }];
    const cmd = b.action === 'delete' ? 'UNLINK' : 'EXPIRE';
    const sends = b.action === 'delete' ? 'UNLINK <key>' : `EXPIRE <key> ${b.ttl_secs} NX`;
    const nodes = b.nodes.length ? ` It runs on each of the ${b.nodes.length} primaries in turn, in slot order.` : '';
    const undo = b.action === 'delete' ? UNDO : ' PERSIST on the keys in the key file undoes it.';
    const what = `${b.summary.join(' ')}${nodes}${undo} Sends: ${sends} for each key of SCAN MATCH ${b.glob}, 500 at a time.`;
    return gate({
      cmds: [cmd], typed: b.pattern.name,
      ask: `${b.title} ${b.pattern.name} on ${p.name}? ${what}`,
      type: `Type ${b.pattern.name} to start: ${b.title} ${b.pattern.name} on ${p.name}. ${what}`,
      cancel: () => {
        if (session && session.bulk === b && b.stage === 'counted') bulkEnd(b, [CANCELLED]);
      },
    }, () => {
      if (!session || session.bulk !== b || b.stage !== 'counted') return [410, { error: BULK_GONE }];
      b.stage = 'running';
      b.run = { changed: 0, failed: 0, file: null, moved: 0, seen: 0, sent: 0, skipped: 0, stopping: false, unreadable: 0 };
      if (b.nodes.length) b.at = 0;
      const total = b.count.matched;
      const batch = Math.max(500, Math.ceil(total / 12 / 500) * 500);
      const tickRun = () => {
        if (!session || session.bulk !== b || b.stage !== 'running') return;
        const r = b.run;
        r.file = r.file || bulkFile(b);
        if (session.profile.read_only) {
          bulkEnd(b, bulkDone(b, (verb) => `Stopped: ${session.profile.name} is now read-only. Before that, ${verb}.`));
          return;
        }
        if (b.ranOn !== session.an.run || b.conn !== connSeq) {
          bulkEnd(b, bulkDone(b, (verb) => `Stopped: the connection changed. Before that, ${verb}.`));
          return;
        }
        if ((session.profile.sim.writeDenied || []).includes(cmd)) {
          r.failed = Math.min(total, batch);
          r.seen = r.failed;
          if (!session.refused.includes(cmd)) {
            session.refused.push(cmd);
            emitState();
          }
          bulkEnd(b, bulkDone(b, (verb) => `Stopped: the server refused ${cmd} for this user. Before that, ${verb}.`), true);
          return;
        }
        const n = Math.min(batch, total - r.seen);
        const change = Math.round(((r.seen + n) * b.count.to_change) / Math.max(1, total)) - r.changed;
        r.seen += n;
        r.changed += change;
        r.sent += change;
        r.skipped = r.seen - r.changed;
        if (b.nodes.length) b.at = Math.min(b.nodes.length - 1, Math.floor((r.seen / Math.max(1, total)) * b.nodes.length));
        if (r.seen >= total) {
          if (b.action === 'delete') session.wiped.add(b.spec.name);
          bulkEnd(b, bulkDone(b, (verb) => `Done: ${verb}.`));
          return;
        }
        if (r.stopping) {
          bulkEnd(b, bulkDone(b, (verb) => `Stopped after the batch: ${verb}.`));
          return;
        }
        emitBulk();
        later(250).then(tickRun);
      };
      later(250).then(tickRun);
      return [202, { bulk: bulkBody(b) }];
    });
  }

  function stopBulk() {
    const b = session.bulk;
    if (!b) return [200, { bulk: null }];
    if (b.stage === 'counting') {
      b.summary = [];
      bulkEnd(b, ['Dry run stopped. Nothing was sent.']);
      return [200, { bulk: bulkBody(b), text: 'Dry run stopped. Nothing was sent.' }];
    }
    if (b.stage === 'counted') {
      bulkEnd(b, ['Bulk action cancelled. Nothing was sent.']);
      return [200, { bulk: bulkBody(b), text: 'Bulk action cancelled. Nothing was sent.' }];
    }
    if (b.stage === 'running') {
      b.run.stopping = true;
      emitBulk();
      return [200, { bulk: bulkBody(b), text: 'Stopping after the batch on its way.' }];
    }
    session.bulk = null;
    emitBulk();
    return [200, { bulk: null }];
  }

  // ---------- the analysis ----------

  function analysisState() {
    if (!session) return { db: 0, depth: null, error: null, note: null, previous: false, report: null, run: 0, sampled: null, started_unix: null, status: 'none', target: null };
    const a = session.an;
    return {
      db: session.db, depth: a.depth, error: a.error, note: a.note, previous: running() && !!a.report, report: a.report ? reportJson(a.report) : null,
      run: a.run, sampled: a.sampled, started_unix: a.startedAt, status: a.status, target: a.target,
    };
  }

  const reportJson = (r) => ({
    age_secs: Math.max(0, unix() - r.takenAt), cluster: r.cluster, complete: r.complete, patterns: space().length, previous: running(), sampled_keys: r.sampled,
    sizes_available: r.sizes, stopped: r.stopped, taken_at_unix: r.takenAt,
  });

  function estimates() {
    const p = session.profile;
    const total = dbsize();
    // A production profile's budget: the analysis keeps under it.
    const budget = p.env === 'prod' ? 4000 : null;
    return ['quick', 'deep'].map((depth) => {
      const limit = depth === 'quick' ? 10000 : 100000;
      const keys = Math.min(limit, total);
      const commands = keys * (2 + Number(sizesOn(p)) + Number(freqOn(p))) + Math.ceil(total / PAGE) + 3;
      return {
        about: budget ? `at most ${budget.toLocaleString('en-US')} commands a second` : 'as fast as the server answers', commands, depth, duration_secs: budget ? Math.ceil(commands / budget) : null,
        freq: freqOn(p), keys, per_sec: budget, sample_limit: limit, shards: p.sim.nodes.length, sizes: sizesOn(p),
      };
    });
  }

  function startAnalysis(body) {
    if (!known(body, ['depth'])) return [400, { error: SHAPE }];
    const depth = body.depth ?? 'quick';
    if (!['quick', 'deep'].includes(depth)) return [400, { error: 'the depth must be quick or deep' }];
    if (running()) return [409, { error: 'An analysis is already running.' }];
    if (bulkBusy()) return [409, { error: BULK_RUNS }];
    if (!live()) return [409, { error: STILL_CONNECTING }];
    const p = session.profile;
    const a = session.an;
    const est = estimates().find((e) => e.depth === depth);
    runSeq += 1;
    Object.assign(a, { run: runSeq, status: 'connecting', depth, sampled: null, target: null, startedAt: unix(), error: null, note: null, stop: false });
    // A new run: the key lists, open values and pending questions go (a write's question
    // stays, and its answer is refused as asked in another run).
    dropQuestions();
    session.lists.clear();
    session.views.clear();
    session.names = null;
    const run = a.run;
    const mine = connSeq;
    const alive = () => !!session && connSeq === mine && session.an === a && a.run === run;
    // As on the real server, an analysis opens its connection again: the state goes
    // through connecting and waiting for a moment.
    session.status = 'connecting';
    session.live = 'waiting';
    emitState();
    emit('progress', { depth, kind: 'started', run, sample_limit: est.sample_limit });

    const stopped = () => {
      const kept = a.sampled > 0;
      if (kept) {
        const total = dbsize();
        a.report = { takenAt: unix(), sampled: a.sampled, complete: false, stopped: true, sizes: sizesOn(p), cluster: isCluster(p), fraction: a.sampled / Math.max(1, total), growth: 1 + 0.01 * (run % 7), base: null };
      }
      Object.assign(a, { status: a.report ? 'done' : 'none', sampled: null, target: null, stop: false });
      const n = kept ? a.report.sampled.toLocaleString('en-US') : '0';
      emit('progress', { kept, kind: 'stopped', run, text: kept ? `Analysis stopped: the ${n} keys it sampled are shown. They are partial, so they are not saved.` : 'Analysis stopped before it sampled any key. Nothing changed.' });
      emitState();
      emitOverview();
    };
    const done = () => {
      const total = dbsize();
      a.report = { takenAt: unix(), sampled: a.sampled, complete: a.sampled >= total, stopped: false, sizes: sizesOn(p), cluster: isCluster(p), fraction: a.sampled / Math.max(1, total), growth: 1 + 0.01 * (run % 7), base: undefined };
      Object.assign(a, { status: 'done', sampled: null, target: null });
      emit('progress', { complete: a.report.complete, kind: 'done', patterns: space().length, run, sampled_keys: a.report.sampled, stopped: false, taken_at_unix: a.report.takenAt });
      emitState();
      later(300).then(() => {
        if (!alive()) return;
        const r = a.report;
        const older = snapsOf().filter((x) => x.at < r.takenAt).sort((x, y) => y.at - x.at);
        r.base = older[0] || null;
        emit('progress', { kind: 'compared', run });
        emitOverview();
      });
    };
    const failed = () => {
      Object.assign(a, { status: a.report ? 'done' : 'none', sampled: null, target: null, stop: false });
      a.error = 'The analysis failed: the connection to the server broke.';
      emit('progress', { error: a.error, kind: 'failed', run });
      emitState();
    };

    later(350).then(() => {
      if (!alive()) return;
      session.status = 'connected';
      later(150).then(() => {
        if (!alive()) return;
        session.live = 'ready';
        emitState();
      });
      if (a.stop) {
        stopped();
        return;
      }
      Object.assign(a, { status: 'sampling', sampled: 0, target: est.keys });
      emitState();
      const steps = depth === 'deep' ? 24 : 12;
      let step = 0;
      const next = () => {
        if (!alive()) return;
        if (a.stop) {
          stopped();
          return;
        }
        step += 1;
        a.sampled = Math.min(a.target, Math.round((a.target * step) / steps));
        if (p.sim.analysisFails && step === Math.round(steps * 0.4)) {
          failed();
          return;
        }
        emit('progress', { kind: 'sampling', patterns: Math.min(space().length, step), run, sampled: a.sampled, stopping: false, target: a.target });
        emitState();
        if (step >= steps) done();
        else later(250).then(next);
      };
      later(250).then(next);
    });
    return [202, { depth, estimate: est, run, sample_limit: est.sample_limit }];
  }

  function stopAnalysis(body) {
    if (!known(body, [])) return [400, { error: SHAPE }];
    if (!running()) return [409, { error: 'No analysis is running.' }];
    const a = session.an;
    const first = !a.stop;
    a.stop = true;
    a.status = 'stopping';
    emit('progress', { kind: 'stopping', run: a.run });
    emitState();
    return [200, { stopping: true, text: first ? 'Stopping the analysis. What it sampled so far is kept.' : 'Stopping: the analysis ends after its current batch.' }];
  }

  /** The sampled keys the panels rank: a few of each pattern, the big ones whole. */
  function sampledKeys() {
    const out = [];
    for (const { spec, count } of space()) for (let i = 0; i < Math.min(count, spec.literal ? 1 : 8); i += 1) out.push({ spec, key: keyRow(spec, i) });
    return out;
  }
  const endpointOf = (k) => (k.node === null ? null : session.profile.sim.nodes[k.node][0]);

  function bigKeys() {
    const r = session.an.report;
    if (!r.sizes) return { off: 'MEMORY USAGE is refused for this user, so no key has a size to rank.' };
    const rows = sampledKeys().sort((x, y) => y.key.bytes - x.key.bytes).slice(0, 50).map(({ spec, key }) => ({
      big: key.bytes >= BIG_KEY, encoding: spec.enc, key, length: key.length, length_unit: spec.unit, node: endpointOf(key), pattern: nameOf(spec.name),
    }));
    const stats = analysisRows(r, null);
    const total = stats.reduce((n, x) => n + x.row.est_bytes, 0);
    const never = stats.reduce((n, x) => n + x.row.est_bytes * x.spec.noTtl, 0);
    const recommendations = rows.filter((x) => x.big).map((x) => ({ hint: 'Read it in ranges, delete it with UNLINK, or split it.', severity: 'warn', text: `${x.key.name} is ${short(x.key.bytes)}: a key this big blocks the server while it is read or deleted whole.` }));
    for (const x of stats) if (x.spec.warn) recommendations.push({ hint: 'Temporary data should carry a TTL.', severity: 'note', text: `${x.row.pattern.name}: 100% of its keys never expire.` });
    if (total) recommendations.push({ hint: 'Fine for data that must stay; caches should expire.', severity: 'info', text: `${pct(never / total)} of memory never expires: ${short(Math.round(never))} of ${short(total)}.` });
    return { big_key_bytes: BIG_KEY, empty: rows.length ? null : 'No keys were sampled.', facts_read: true, notes: [], recommendations, report: reportJson(r), rows };
  }

  function expiryPanel() {
    const r = session.an.report;
    const stats = analysisRows(r, null);
    const status = (spec) => `${spec.noTtl >= 1 ? 'persistent' : spec.noTtl > 0 ? 'mostly expiring' : 'expiring'} · ${spec.grows ? 'growing' : 'stable'}`;
    const explain = (spec, row) => {
      const lines = [];
      if (spec.noTtl >= 1) lines.push(`All ${row.est_keys.toLocaleString('en-US')} keys never expire${spec.grows ? ', and the pattern grew since the last snapshot' : ''}.`);
      else if (spec.noTtl > 0) lines.push(`${pct(spec.noTtl)} of its keys never expire; the others expire within ${ttlText(spec.ttl)}.`);
      else lines.push(`Every key expires, within ${ttlText(spec.ttl)}.`);
      if (spec.warn) lines.push(spec.warn);
      return lines;
    };
    const rows = stats.map(({ spec, row }) => ({
      command: row.command, est_bytes: row.est_bytes, est_keys: row.est_keys, explain: explain(spec, row), no_ttl: row.no_ttl, no_ttl_ratio: row.no_ttl_ratio, pattern: row.pattern,
      sampled_keys: row.sampled_keys, status: status(spec), ttl_hist: row.ttl_hist, warning: row.warning,
    })).sort((x, y) => (y.warning - x.warning) || ((y.est_bytes ?? 0) - (x.est_bytes ?? 0)));
    let freed = null;
    if (r.sizes) {
      const total = stats.reduce((n, x) => n + x.row.est_bytes, 0);
      const within = [['1 h', 3600], ['1 d', 86400], ['7 d', 7 * 86400], ['30 d', 30 * 86400]].map(([label, secs]) => ({
        bytes: Math.round(stats.reduce((n, x) => n + (x.spec.ttl !== null && x.spec.ttl <= secs ? x.row.est_bytes * (1 - x.spec.noTtl) : 0), 0)), label,
      }));
      const never = Math.round(stats.reduce((n, x) => n + x.row.est_bytes * (x.spec.ttl === null ? 1 : x.spec.noTtl), 0));
      freed = { later: Math.max(0, total - never - within[within.length - 1].bytes), never, never_share: total ? never / total : 0, total, within };
    }
    return { empty: rows.length ? null : 'No keys were sampled.', freed, freed_off: r.sizes ? null : 'MEMORY USAGE is refused for this user, so there are no memory figures.', report: reportJson(r), rows, ttl_labels: TTL_LABELS };
  }

  /** The current analysis as a snapshot's figures. */
  function current(r) {
    return {
      at: r.takenAt, sampled: r.sampled, complete: r.complete, usedMemory: server(session.profile).used_memory,
      rows: analysisRows(r, null).map(({ spec, row }) => ({ name: spec.name, keys: row.est_keys, bytes: row.est_bytes })),
    };
  }

  function diff(b, a, history = null) {
    const all = Array.from(new Set(b.rows.map((x) => x.name).concat(a.rows.map((x) => x.name))));
    const days = history && history.length ? Math.max(1 / 24, (a.at - history[0].at) / 86400) : null;
    const rows = all.map((name) => {
      const x = b.rows.find((r) => r.name === name);
      const y = a.rows.find((r) => r.name === name);
      const change = !y ? { alarming: false, cause: 'gone', cause_words: 'gone', delta_bytes: x.bytes === null ? null : -x.bytes, delta_keys: -x.keys } : changeOf(x, y);
      let trend = null;
      if (history && history.length >= 2) {
        const values = history.map((h0) => {
          const at = h0.rows.find((r) => r.name === name);
          return at ? at.bytes ?? at.keys : null;
        }).concat([y ? y.bytes ?? y.keys : null]);
        const first = values.find((v) => v !== null);
        const last = values[values.length - 1];
        trend = { growth: first !== null && last !== null && first !== undefined ? { per_day: (last - first) / days, ratio_per_day: first ? (last - first) / first / days : null } : null, values };
      }
      return { bytes_after: y ? y.bytes : null, bytes_before: x ? x.bytes : null, change, keys_after: y ? y.keys : null, keys_before: x ? x.keys : null, partial: false, pattern: nameOf(name), trend };
    }).sort((p, q) => Math.abs(q.change.delta_bytes ?? q.change.delta_keys) - Math.abs(p.change.delta_bytes ?? p.change.delta_keys));
    return {
      after_sampled_keys: a.sampled, after_taken_at_unix: a.at, after_utc: snapUtc(a.at), before_sampled_keys: b.sampled, before_taken_at_unix: b.at, before_utc: snapUtc(b.at),
      empty: rows.length ? null : 'Neither analysis sampled any keys.', note: b.complete === false || a.complete === false ? 'Both are samples: the figures are estimates scaled from the keys sampled.' : null, rows,
      trend_points: history && history.length >= 2 ? history.map((h0) => ({ complete: !!h0.complete, sampled_keys: h0.sampled, taken_at_unix: h0.at })) : null,
      used_memory_after: a.usedMemory, used_memory_before: b.usedMemory,
    };
  }

  function changesPanel() {
    const r = session.an.report;
    const report = reportJson(r);
    if (r.base === undefined) return { report, status: 'pending', text: 'Looking for an earlier snapshot…' };
    if (r.stopped) return { report, status: 'none', text: 'This analysis was stopped early, so it is not compared with a snapshot.' };
    if (!r.base) return { report, status: 'none', text: `No earlier snapshot for ${snapName()}. Save one; the next analysis compares against it.` };
    const history = snapsOf().filter((x) => x.at < r.takenAt).sort((x, y) => x.at - y.at);
    return { ...diff(r.base, current(r), history), report, status: 'diff' };
  }

  function hotKeys() {
    const p = session.profile;
    const r = session.an.report;
    if (p.sim.denied['OBJECT FREQ']) return { off: 'OBJECT FREQ is refused for this user, so there are no hot keys.' };
    if (!p.sim.lfu) return { off: `The maxmemory policy is ${p.sim.policy}: without an LFU policy the server keeps no access counters, so there are no hot keys.` };
    const rows = sampledKeys().sort((x, y) => y.key.freq - x.key.freq).slice(0, 50).map(({ spec, key }) => ({
      hot_and_big: (key.bytes ?? 0) >= HOT_BIG && key.freq >= HOT_FREQ, key, node: endpointOf(key), pattern: nameOf(spec.name),
    }));
    return { empty: rows.length ? null : 'No sampled key has an LFU counter above 0 yet.', hot_big_bytes: HOT_BIG, hot_freq: HOT_FREQ, report: reportJson(r), rows };
  }

  function nodesPanel() {
    const p = session.profile;
    const r = session.an.report;
    if (!isCluster(p)) return { off: 'Not a cluster: this analysis read one standalone server.' };
    const total = dbsize();
    const count = p.sim.nodes.length;
    const rows = nodesOf(p).map((n, i) => ({
      complete: i !== 2, endpoint: n.endpoint, id: n.id, index: i, keys: Math.round((total / count) * (0.96 + i * 0.04)), memory: Math.round((server(p).used_memory / count) * (0.97 + i * 0.03)),
      note: i === 2 ? 'Its sample stopped at the command budget, so its figures are partial.' : null, patterns: space().length - (i === 2 ? 1 : 0), primary_id: n.id, role: 'primary',
      sampled_keys: Math.round((r.sampled / count) * (i === 2 ? 0.8 : 1.1)), sizes_available: r.sizes, slot_count: n.slot_count, slots: n.slots, status: i === 2 ? 'partial' : 'scanned', version: p.sim.version,
    }));
    return { empty: null, report: reportJson(r), rows };
  }

  function slotsPanel(q) {
    const p = session.profile;
    const order = q.get('order') ?? 'keys';
    if (!['keys', 'cpu', 'net_in', 'net_out'].includes(order)) return [400, { error: 'the order must be one of keys, cpu, net_in or net_out' }];
    if (!isCluster(p)) return [200, { off: 'Not a cluster: this is one standalone server.' }];
    if (!session.slots || q.get('refresh')) session.slots = { seed: tick };
    const seed = session.slots.seed;
    const nodes = nodesOf(p);
    const fig = (slot) => ({ keys: 120 + ((slot * 7919 + seed) % 260), cpu_usec: 5000 + ((slot * 104729 + seed * 13) % 90000), net_in: 20000 + ((slot * 15485863) % 400000), net_out: 90000 + ((slot * 32452843 + seed) % 2000000), memory: null });
    const nodeOf = (slot) => nodes.findIndex((n) => slot >= n.slots[0][0] && slot <= n.slots[0][1]);
    const by = { keys: 'keys', cpu: 'cpu_usec', net_in: 'net_in', net_out: 'net_out' }[order];
    const hottest = [8412, 1203, 15011, 5462, 3300, 12800, 9001, 61, 14000, 7777, 2200, 10500]
      .map((slot) => ({ slot, node: nodeOf(slot), ...fig(slot) }))
      .sort((x, y) => y[by] - x[by]).slice(0, 10)
      .map((x) => ({ ...x, endpoint: nodes[x.node].endpoint, sampled_keys: x.slot === 8412 && session.an.report ? [{ key: nameOf('user:10293:cart'), kinds: 'hash', pattern: nameOf('user:{id}:cart') }] : [] }));
    const ranges = nodes.map((n, i) => ({
      cpu_usec: 1200000 + i * 90000 + seed, end: n.slots[0][1], endpoint: n.endpoint, error: null, keys: Math.round(dbsize() / nodes.length), memory: null,
      net_in: 41000000 + i * 1200000, net_out: 380000000 + i * 9000000, node: i, start: n.slots[0][0], status: 'done',
    }));
    return [200, { cpu_and_network_zero: false, empty: null, hottest, memory_column: p.sim.flavor === 'redis', note: null, order, ranges }];
  }

  /** Overview's analysis part: null until there is a report, and while one runs. */
  function overviewAnalysis() {
    const r = session.an.report;
    if (!r || running()) return null;
    const stats = analysisRows(r, null);
    const by = (x) => (r.sizes ? x.row.est_bytes : x.row.est_keys);
    const total = stats.reduce((n, x) => n + by(x), 0);
    const findings = [];
    if (r.sizes) {
      for (const { spec, key } of sampledKeys()) {
        if (key.bytes >= BIG_KEY) findings.push({ area: 'Analyze', panel: 'Big keys', screen: 'big_keys', severity: 'warn', text: `${key.name} is ${short(key.bytes)}: a big key.` });
      }
    }
    for (const x of stats) if (x.spec.warn) findings.push({ area: 'Analyze', panel: 'Expiry', screen: 'expiry', severity: 'note', text: `${x.row.pattern.name}: 100% of its keys never expire.` });
    if (r.base) {
      for (const row of diff(r.base, current(r)).rows) {
        if (row.change.alarming && row.change.cause !== 'new') findings.push({ area: 'Analyze', panel: 'Changes', screen: 'changes', severity: 'warn', text: `${row.pattern.name} grew ${pct((row.change.delta_bytes ?? 0) / (row.bytes_before || 1))} since the last snapshot.` });
      }
    }
    if (freqOn(session.profile)) {
      const hot = sampledKeys().sort((x, y) => y.key.freq - x.key.freq)[0];
      if (hot) findings.push({ area: 'Analyze', panel: 'Hot keys', screen: 'hot_keys', severity: 'info', text: `${hot.key.name} is the most-read sampled key: LFU counter ${hot.key.freq}.` });
    }
    const never = r.sizes && total ? stats.reduce((n, x) => n + x.row.est_bytes * (x.spec.ttl === null ? 1 : x.spec.noTtl), 0) / total : null;
    return {
      changes: r.base === undefined ? 'pending' : r.base ? 'diff' : 'none', complete: r.complete, findings, never_expires_share: never,
      note: r.stopped ? 'Stopped early: these figures come from part of the sample.' : null, patterns: stats.length, sampled_keys: r.sampled, sizes_available: r.sizes, stopped: r.stopped,
      taken_at_unix: r.takenAt,
      top_patterns: stats.slice().sort((x, y) => by(y) - by(x)).slice(0, 5).map((x) => ({ est_bytes: x.row.est_bytes, est_keys: x.row.est_keys, pattern: x.row.pattern, share: total ? by(x) / total : null })),
    };
  }

  // ---------- snapshots and databases ----------

  function snapshotList() {
    const r = session.an.report;
    const list = snapsOf().slice().sort((x, y) => y.at - x.at);
    const saveOff = !r ? 'Nothing to save yet: analyze the keys first.' : running() ? 'An analysis is running: save it when it finishes.' : r.stopped ? STOPPED_TEXT : null;
    return {
      can_save: !saveOff, profile: snapName(), save_off: saveOff, saved: !!r && list.some((s) => s.at === r.takenAt),
      snapshots: list.map((s) => ({ current: !!r && s.at === r.takenAt, file: s.file, taken_at_unix: s.at, taken_at_utc: snapUtc(s.at) })),
    };
  }

  function saveSnapshot(body) {
    if (!known(body, [])) return [400, { error: SHAPE }];
    const r = session.an.report;
    if (running()) return [409, { error: 'An analysis is running: save it when it finishes.' }];
    if (!r) return [409, { error: 'Nothing to save yet: analyze the keys first.' }];
    if (r.stopped) return [409, { error: STOPPED_TEXT }];
    const list = snaps.get(snapName()) || [];
    let s = list.find((x) => x.at === r.takenAt);
    if (!s) {
      s = { ...current(r), file: snapFile(r.takenAt) };
      list.push(s);
      snaps.set(snapName(), list);
    }
    return [201, { file: s.file, sampled_keys: r.sampled, taken_at_unix: r.takenAt, taken_at_utc: snapUtc(r.takenAt), text: `Saved snapshot ${s.file}` }];
  }

  function compareSnapshots(q) {
    const names = [q.get('a'), q.get('b')];
    if (names.some((n) => !n || !(n === 'current' || /^\d{8}T\d{6}Z\.json$/.test(n)))) return [400, { error: 'a snapshot is named by its file name, such as 20261115T091500Z.json, or current' }];
    if (names[0] === names[1]) return [422, { error: 'Pick two different snapshots to compare.' }];
    const r = session.an.report;
    const sides = [];
    for (const n of names) {
      if (n === 'current') {
        if (!r) return [409, { error: NO_REPORT }];
        sides.push(current(r));
      } else {
        const s = snapsOf().find((x) => x.file === n);
        if (!s) return [404, { error: 'There is no such snapshot of this connection.' }];
        sides.push(s);
      }
    }
    return [200, diff(sides[0], sides[1])];
  }

  function dbs() {
    const p = session.profile;
    if (isCluster(p)) return { off: 'A cluster only has database 0.' };
    const db0 = SPACE.reduce((n, spec) => n + (spec.literal ? 1 : Math.max(1, Math.round(spec.keys * p.sim.scale))), 0);
    const rows = [{ db: 0, keys: db0, expires: Math.round(db0 * 0.31) }, { db: 1, keys: 41, expires: 0 }, { db: 3, keys: 3, expires: 3 }];
    return {
      current: session.db, note: 'Picking a database is for this session. The saved profile is unchanged.', rows: rows.map((r) => ({ ...r, current: r.db === session.db })),
      saved_as: p.name, unread: p.sim.denied['MEMORY USAGE'] ? 'CONFIG GET databases is refused for this user, so only the databases with keys are listed.' : null,
    };
  }

  function switchDb(body) {
    if (!known(body, ['db']) || !Number.isInteger(body.db)) return [400, { error: SHAPE }];
    if (isCluster(session.profile)) return [422, { error: 'A cluster only has database 0.' }];
    if (running()) return [409, { error: 'An analysis is running: wait for it to finish, then pick a database.' }];
    if (bulkBusy()) return [409, { error: BULK_RUNS }];
    if (!live()) return [409, { error: 'The connection is not ready yet: wait for it, then pick a database.' }];
    if (![0, 1, 3].includes(body.db)) return [422, { error: 'Pick a database the list shows (read the list first): it shows the databases with keys.' }];
    if (body.db === session.db) return [200, { db: body.db, run: null, switched: false, text: null }];
    // A new run: what was read of the other database goes, pending questions too.
    Object.assign(session, { db: body.db, an: freshAnalysis(), names: null, slots: null });
    session.lists.clear();
    session.views.clear();
    dropQuestions();
    emitState();
    later(200).then(() => emitOverview());
    return [202, { db: body.db, run: session.an.run, switched: true, text: `Switched to db ${body.db} for this session. The saved profile is unchanged.` }];
  }

  const P2 = ['/api/dbs', '/api/db', '/api/browse', '/api/browse/more', '/api/keys', '/api/keys/more', '/api/find', '/api/value', '/api/value/more', '/api/value/item',
    '/api/analysis', '/api/analysis/big_keys', '/api/analysis/expiry', '/api/analysis/changes', '/api/analysis/hot_keys', '/api/analysis/nodes', '/api/analysis/slots',
    '/api/snapshots', '/api/snapshots/compare',
    '/api/value/edit', '/api/value/add', '/api/value/ttl', '/api/value/rename', '/api/value/remove', '/api/value/delete', '/api/keys/add', '/api/bulk', '/api/bulk/run'];

  /** Phase 2's routes, or null for another route. */
  async function phase2(method, path, query, body) {
    if (!P2.includes(path)) return null;
    if (!session) return [409, { error: NOT_CONNECTED }];
    if (session.status === 'failed') return [409, { error: 'The connect failed: connect again first.' }];
    const q = new URLSearchParams(query);
    const k = `${method} ${path}`;
    const reading = !live() ? [409, { error: STILL_CONNECTING }] : null;
    const panel = (fn) => {
      if (!session.an.report) return [409, { error: NO_REPORT }];
      const out = fn();
      return [200, out];
    };
    switch (k) {
      case 'GET /api/dbs': return [200, dbs()];
      case 'POST /api/db': return switchDb(body);
      case 'GET /api/browse': return browse(q);
      case 'POST /api/browse/more': return reading || moreNames(body);
      case 'GET /api/keys': return reading || patternList(q);
      case 'GET /api/find': return live() ? findList(q) : [409, { error: 'Still connecting: find a key once connected.' }];
      case 'GET /api/keys/more': return reading || moreList(q);
      case 'GET /api/value': return reading || openView(q);
      case 'GET /api/value/more': return reading || moreView(q);
      case 'GET /api/value/item': return reading || itemOf(q);
      case 'GET /api/analysis': return [200, { analysis: analysisState(), estimates: session.status === 'connected' || running() ? estimates() : [], running: running() }];
      case 'POST /api/analysis': return startAnalysis(body);
      case 'DELETE /api/analysis': return stopAnalysis(body);
      case 'GET /api/analysis/big_keys': return panel(bigKeys);
      case 'GET /api/analysis/expiry': return panel(expiryPanel);
      case 'GET /api/analysis/changes': return panel(changesPanel);
      case 'GET /api/analysis/hot_keys': return panel(hotKeys);
      case 'GET /api/analysis/nodes': return panel(nodesPanel);
      case 'GET /api/analysis/slots': return slotsPanel(q);
      case 'GET /api/snapshots': return [200, snapshotList()];
      case 'POST /api/snapshots': return saveSnapshot(body);
      case 'GET /api/snapshots/compare': return compareSnapshots(q);
      case 'POST /api/value/edit': return editValue(body);
      case 'POST /api/value/add': return addItem(body);
      case 'POST /api/value/ttl': return setTtl(body);
      case 'POST /api/value/rename': return renameKey(body);
      case 'POST /api/value/remove': return removeItem(body);
      case 'POST /api/value/delete': return deleteKey(body);
      case 'POST /api/keys/add': return addKey(body);
      case 'GET /api/bulk': return [200, { bulk: bulkBody(session.bulk) }];
      case 'POST /api/bulk': return startBulk(body);
      case 'DELETE /api/bulk': return stopBulk();
      case 'POST /api/bulk/run': return runBulk(body);
      default: return [405, { error: 'method not allowed' }];
    }
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
    session = {
      profile: p, docker: !!target.docker, status: 'connecting', error: null, live: 'waiting', liveError: null, connectedAt: null, picked: 0, refused: [], tail: null, monitor: null,
      db: p.db, an: freshAnalysis(), names: null, lists: new Map(), views: new Map(), listSeq: 0, viewSeq: 0, slots: null,
      store: new Map(), wiped: new Set(), bulk: null,
    };
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
        // What the connect read for Overview (its findings) lands after the live
        // connection is ready, as on the real server.
        later(300).then(() => {
          if (session && connSeq === mine) emitOverview();
        });
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
    if ((key === 'POST /api/connect' || key === 'POST /api/disconnect') && bulkBusy()) return [409, { error: BULK_RUNS }];
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
      if (method === 'DELETE') {
        if (held.cancel) held.cancel();
        return [200, { cancelled: true, text: CANCELLED }];
      }
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
      emitOverview();
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

    const p2 = await phase2(method, path, query, body);
    if (p2) return p2;

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
      if (body.line.length > 8192) return [413, { error: 'Not sent: a line holds at most 8,192 characters.' }];
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

  /** Like the real server: one `Authorization: Bearer <token>` header, any case of the scheme. */
  function signedIn(init) {
    const headers = init.headers || {};
    const values = Object.keys(headers).filter((k) => k.toLowerCase() === 'authorization').map((k) => headers[k]);
    return values.length === 1 && /^bearer /i.test(values[0]) && values[0].slice(7) === MOCK_TOKEN;
  }

  async function fetchFn(path, init = {}) {
    if (String(path).startsWith('/api/') && !signedIn(init)) {
      await later(20);
      return { status: 401, ok: false, body: null, json: async () => ({ error: 'not signed in' }) };
    }
    if (path === '/api/events' && (init.method || 'GET') === 'GET') return openStream(init.signal);
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

  return { fetchFn, token: MOCK_TOKEN };
}
