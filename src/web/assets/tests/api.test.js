// The adapters: the server's JSON → the view-models the views consume. The sample JSON
// here is the shape api.js assumes; when the server's real shapes land, these samples and
// the `// ADAPT:` functions change together.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  ROUTES, METHODS, addRequest, updateRequest, adaptState, adaptConnections, adaptTestResult, adaptOverview,
  adaptFigures, adaptClients, adaptSlowlog, adaptMonitor, adaptChannels, adaptSubscription, adaptTail,
  adaptPublished, adaptReply, adaptConfirm, adaptNotice, adaptError, wantsMock, createReal, TEXT,
} from '../api.js';

test('state: connected, with nodes and the checks that are off', () => {
  const s = adaptState({
    connected: true,
    live: false,
    connection: {
      name: 'prod-cache-01', env: 'prod', address: 'cache-01.prod.internal:6379', read_only: true, server: 'Redis', version: '7.4.1',
      topology: 'cluster', tls: true, picked_node: 'n2', shard_channels: true, monitor_seconds: 30,
      checks_off: [{ name: 'MEMORY STATS', why: 'MEMORY STATS is denied for this user.' }],
      nodes: [{ id: 'n1', address: '10.0.0.1:6379', role: 'primary', slots: '0-8191' }, { id: 'n2', address: '10.0.0.2:6379', role: 'primary', slots: '8192-16383' }],
    },
  });
  assert.equal(s.connected, true);
  assert.equal(s.live, false);
  assert.equal(s.conn.env, 'PROD');
  assert.equal(s.conn.readOnly, true);
  assert.equal(s.conn.pickedNode, 'n2');
  assert.equal(s.conn.nodes.length, 2);
  assert.deepEqual(s.conn.checksOff, [{ name: 'MEMORY STATS', why: 'MEMORY STATS is denied for this user.' }]);
});

test('state: empty, connecting and lost', () => {
  assert.deepEqual(adaptState({}), { connected: false, connecting: null, conn: null, live: true, lost: null });
  assert.deepEqual(adaptState(null), { connected: false, connecting: null, conn: null, live: true, lost: null });
  const c = adaptState({ connecting: { name: 'demo', text: 'Connecting to demo (127.0.0.1:6390) and checking permissions…' } });
  assert.equal(c.connecting.name, 'demo');
  assert.equal(c.connected, false);
  assert.equal(adaptState({ lost: 'the connection to the server was lost' }).lost, 'the connection to the server was lost');
});

test('state: a connection is read-only unless the server says writes are on', () => {
  assert.equal(adaptState({ connected: true, connection: { name: 'x' } }).conn.readOnly, true);
  assert.equal(adaptState({ connected: true, connection: { name: 'x', read_only: false } }).conn.readOnly, false);
  assert.equal(adaptState({ connected: true, connection: { name: 'x', env: 'nonsense' } }).conn.env, 'OTHER');
  assert.equal(adaptState({ connected: true, connection: { name: 'x' } }).conn.monitorSeconds, 30);
});

test('connections: profiles and Docker containers, and no credential field survives', () => {
  const c = adaptConnections({
    profiles: [{
      name: 'prod', env: 'prod', address: 'h:6379', tls: true, tags: ['cache', '', 7], read_only: true, access: 'read-only, the default for production',
      sign_in: 'password or ACL user, kept in the OS keychain', status: 'limited', status_text: 'no sizes',
      username: 'alice', password: 'hunter2', ssh: 'bastion.example:22',
    }],
    docker: [{ name: 'docker:demo-redis', env: 'local', address: '127.0.0.1:6390', container: 'demo-redis', image: 'redis:8', read_only: false, status: 'bogus' }],
    docker_note: null,
  });
  assert.equal(c.profiles.length, 1);
  const p = c.profiles[0];
  assert.deepEqual(p.tags, ['cache']);
  assert.equal(p.status, 'limited');
  assert.equal(p.statusText, 'no sizes');
  assert.equal(p.signIn, 'password or ACL user, kept in the OS keychain');
  assert.equal(JSON.stringify(c).includes('hunter2'), false, 'a password the server should never send is dropped');
  assert.equal(JSON.stringify(c).includes('alice'), false, 'and so is a username');
  assert.equal(c.docker[0].docker, true);
  assert.equal(c.docker[0].status, 'not-open', 'an unknown status reads as not open');
  assert.deepEqual(adaptConnections({}), { profiles: [], docker: [], dockerNote: null });
});

test('a test result lists each check and why it is off', () => {
  const r = adaptTestResult({ ok: false, summary: 'Limited', checks: [{ name: 'SCAN', ok: true }, { name: 'MEMORY USAGE', ok: false, why: 'NOPERM MEMORY USAGE is not allowed for this user' }], notes: ['Through SSH bastion b:22.'] });
  assert.equal(r.ok, false);
  assert.deepEqual(r.checks, [{ name: 'SCAN', ok: true, why: null }, { name: 'MEMORY USAGE', ok: false, why: 'NOPERM MEMORY USAGE is not allowed for this user' }]);
  assert.deepEqual(r.notes, ['Through SSH bastion b:22.']);
});

test('the add request carries the password once, in the body, and only what was filled in', () => {
  const body = addRequest({
    name: 'cache-02', url: 'rediss://app@cache-02:6380/0', env: 'PROD', password: 'hunter2', tags: ['a'], writes: null,
    tls: true, provider: '', sshHost: '', sentinelAddresses: [], tokenCommand: [], preferReplica: false, forceStandalone: false,
  });
  assert.deepEqual(body, { name: 'cache-02', url: 'rediss://app@cache-02:6380/0', tags: ['a'], env: 'prod', password: 'hunter2', tls: true });
  assert.equal(JSON.stringify(body).split('hunter2').length - 1, 1);
});

test('the add request: advanced fields', () => {
  const body = addRequest({
    name: 'n', url: 'redis://h', env: '', password: '', tags: [], writes: false,
    provider: 'memorydb', sshHost: 'ops@bastion:2222', sshIdentity: '~/.ssh/id', sshKnownHosts: '',
    sentinelAddresses: ['s1:26379', 's2:26379'], sentinelMaster: 'mymaster', sentinelPassword: 'sp',
    tokenCommand: ['aws', 'token', 'my cache'], tokenCommandConfirmed: true, preferReplica: true, forceStandalone: true,
  });
  assert.equal('password' in body, false, 'an empty password is not sent');
  assert.equal('env' in body, false, 'no environment means the server guesses it');
  assert.equal(body.writes, false);
  assert.equal(body.provider, 'memorydb');
  assert.deepEqual(body.ssh, { host: 'ops@bastion:2222', identity: '~/.ssh/id', known_hosts: null });
  assert.deepEqual(body.sentinel, { addresses: ['s1:26379', 's2:26379'], master: 'mymaster', password: 'sp' });
  assert.deepEqual(body.token_command, ['aws', 'token', 'my cache']);
  assert.equal(body.token_command_confirmed, true);
  assert.equal(body.prefer_replica, true);
  assert.equal(body.force_standalone, true);
});

test('the update request sends only what changed', () => {
  assert.deepEqual(updateRequest({ writes: true }), { writes: true });
  assert.deepEqual(updateRequest({ env: 'STAGE', tags: ['x'] }), { env: 'stage', tags: ['x'] });
  assert.deepEqual(updateRequest({}), {});
});

test('figures: missing readings stay unknown, and a zero limit means no limit', () => {
  const f = adaptFigures({ at: 1000, ops_per_sec: 12400.5, clients: 214, used_memory: 16000000000, max_memory: 0, hit_rate: 0.97 });
  assert.equal(f.opsPerSec, 12400.5);
  assert.equal(f.maxMemory, null);
  assert.equal(f.evictedPerSec, null);
  assert.equal(f.expiredPerSec, null);
  const empty = adaptFigures({ at: 5 });
  assert.equal(empty.opsPerSec, null);
  assert.equal(empty.clients, null);
  assert.equal(empty.usedMemory, null);
});

test('overview: figures, shards, findings and notes', () => {
  const o = adaptOverview({
    address: 'h:6379 · TLS',
    server: { kind: 'Valkey', version: '8.0.1', topology: 'cluster', uptime_secs: 86400, role: 'primary', replicas: 1, persistence: 'RDB ok', policy: 'allkeys-lfu' },
    memory: { used: 1000, max: 0 },
    keys: { total: 7920114, ttl_share: 0.31, data_bytes: null, overhead_bytes: null, db: 0 },
    figures: { ops_per_sec: 5 },
    history: { ops: [1, 2, 'x'], clients: [3], every_secs: 2 },
    shards: [{ id: 'n1', slots: '0-5460', address: 'a:1', role: 'primary', ops_per_sec: 10, used_memory: 5, keys: 9, picked: true }, { id: 'n2', status: 'no-answer' }],
    findings: [{ text: 'KEYS took 812 ms at 14:01 UTC.', where: 'slowlog' }, { text: '', where: 'clients' }, { text: 'Other', where: 'elsewhere' }],
    notes: ['Data and overhead are missing: MEMORY STATS is denied for this user.'],
  });
  assert.equal(o.server.kind, 'Valkey');
  assert.equal(o.server.uptimeSecs, 86400);
  assert.equal(o.memory.max, null);
  assert.equal(o.keys.ttlShare, 0.31);
  assert.equal(o.keys.dataBytes, null);
  assert.deepEqual(o.history, { ops: [1, 2, null], clients: [3], everySecs: 2 });
  assert.equal(o.shards[0].picked, true);
  assert.equal(o.shards[1].status, 'no-answer');
  assert.equal(o.shards[1].opsPerSec, null);
  assert.deepEqual(o.findings, [{ text: 'KEYS took 812 ms at 14:01 UTC.', where: 'slowlog' }, { text: 'Other', where: null }]);
  assert.equal(o.notes.length, 1);
});

test('overview: an empty reply invents nothing', () => {
  const o = adaptOverview({});
  assert.equal(o.keys.total, null);
  assert.equal(o.memory.used, null);
  assert.equal(o.server.version, null);
  assert.equal(o.figures, null);
  assert.deepEqual(o.shards, []);
  assert.deepEqual(o.findings, []);
});

test('clients, slow log and monitor', () => {
  const c = adaptClients({ total: 3, groups: [{ by: 'address', name: '10.0.9.0/24', connections: 3, oldest_secs: 100, idle_secs: 4000, last_command: 'PING', memory: 60000, flagged: true }, {}] });
  assert.equal(c.groups[0].by, 'address');
  assert.equal(c.groups[0].idleSecs, 4000);
  assert.equal(c.groups[0].flagged, true);
  assert.equal(c.groups[1].name, '(no name)');
  assert.equal(c.groups[1].connections, null);
  assert.equal(adaptClients({ off: 'Idle clients are off: the server refused CLIENT LIST' }).off, 'Idle clients are off: the server refused CLIENT LIST');

  const s = adaptSlowlog({ threshold: 'slower than 10 ms', entries: [{ at: 1, micros: 912000, command: 'LRANGE queue:emails 0 -1', keys: ['queue:emails'], client: '10.0.4.17:51122', note: 'Reads a whole list.' }] });
  assert.equal(s.entries[0].micros, 912000);
  assert.deepEqual(s.entries[0].keys, ['queue:emails']);

  const m = adaptMonitor({ running: true, left: 27, lines: [{ at: 5, db: 0, client: '10.0.4.11:4000', command: 'GET a' }], kept: 1 });
  assert.equal(m.running, true);
  assert.equal(m.left, 27);
  assert.equal(m.lines[0].command, 'GET a');
  assert.equal(adaptMonitor({ running: false, stopped: 'MONITOR stopped after 30 s.' }).stopped, 'MONITOR stopped after 30 s.');
});

test('channels, subscription, tail and publish', () => {
  const ch = adaptChannels({ total: 2, patterns: 1, node: '127.0.0.1:7001', channels: [{ name: 'orders.created', subscribers: 12 }, { name: 'shard.orders', shard: true, node: '127.0.0.1:7002' }] });
  assert.equal(ch.channels[0].shard, false);
  assert.equal(ch.channels[1].shard, true);
  assert.equal(ch.channels[1].subscribers, null, 'a refused NUMSUB leaves the count unknown');
  assert.deepEqual(adaptSubscription({ name: 'orders.*', kind: 'pattern' }), { name: 'orders.*', kind: 'pattern' });
  assert.equal(adaptSubscription({ target: 'a' }).kind, 'channel');
  const t = adaptTail({ messages: [{ at: 7, channel: 'c', body: '<img src=x onerror=alert(1)>' }, { channel: 'c', body: 5 }], skipped: 3 });
  assert.equal(t.messages[0].body, '<img src=x onerror=alert(1)>', 'text from Redis is data and is kept as text');
  assert.equal(t.messages[1].body, '');
  assert.equal(t.skipped, 3);
  assert.deepEqual(adaptPublished({ receivers: 12 }), { receivers: 12, text: null });
});

test('a command reply: the four views', () => {
  const r = adaptReply({
    input: 'HGETALL user:1', node: '127.0.0.1:7002', micros: 420, kind: 'reply',
    views: { text: '1) "a"\n2) "1"', table: { columns: ['#', 'field', 'value'], rows: [[1, 'a', '1']] }, json: '{"a":"1"}', raw: '[array] *2' },
  });
  assert.equal(r.views.table.rows[0][0], '1', 'cells are text');
  assert.equal(r.micros, 420);
  assert.equal(adaptReply({ kind: 'refused', views: { text: 'Not sent: prod is read-only.' } }).kind, 'refused');
  const bare = adaptReply({ text: 'PONG' });
  assert.equal(bare.kind, 'reply');
  assert.equal(bare.views.text, 'PONG');
  assert.equal(bare.views.table, null);
  assert.equal(bare.views.json, null);
});

test('a command reply: views sent as lines, a drawn table, milliseconds and an error flag', () => {
  const r = adaptReply({ text: ['1) "a"', '2) "1"'], table: ['#  field  value', '1  a      1'], json: ['{', '  "a": "1"', '}'], raw: ['[array] *2'], error: false, took_ms: 0.42 });
  assert.equal(r.views.text, '1) "a"\n2) "1"');
  assert.equal(r.views.table, '#  field  value\n1  a      1');
  assert.equal(r.views.json, '{\n  "a": "1"\n}');
  assert.equal(r.micros, 420);
  assert.equal(r.kind, 'reply');
  assert.equal(adaptReply({ text: ['ERR syntax error'], error: true }).kind, 'error');
  assert.equal(adaptReply({ views: { text: 'x', table: [1, 2] } }).views.table, null, 'a table that is neither shape is unknown');
});

test('confirm and notice', () => {
  assert.deepEqual(adaptConfirm({ confirm: { id: 'a1', kind: 'type', text: 'Delete user:1?', expect: 'user:1' } }), { id: 'a1', kind: 'type', text: 'Delete user:1?', expect: 'user:1', title: null });
  assert.equal(adaptConfirm({ confirm: { id: 'a2', kind: 'something' } }).kind, 'ask');
  assert.equal(adaptConfirm({ confirm: { id: 'a3', kind: 'something', expect: 'prod' } }).kind, 'type', 'a name to type is never downgraded to a plain Yes');
  assert.deepEqual(adaptNotice({ text: 'Saved.', kind: 'ok' }), { text: 'Saved.', kind: 'ok' });
  assert.equal(adaptNotice({ text: 'x', kind: 'loud' }).kind, 'info');
});

test('errors: fixed wording only', () => {
  assert.equal(adaptError(400, { error: 'Not sent: the name typed did not match.' }).message, 'Not sent: the name typed did not match.');
  assert.equal(adaptError(500, { message: 'panic at src/web', stack: '…' }).message, TEXT.failed);
  assert.equal(adaptError(500, null).message, TEXT.failed);
  assert.equal(adaptError(401, { error: 'whatever' }).locked, true);
  assert.equal(adaptError(403, null).message, TEXT.forbidden);
});

test('routes: names are escaped into the path, and every method exists on the real implementation', () => {
  assert.deepEqual(ROUTES.update('docker:demo redis/x'), ['PATCH', '/api/connections/docker%3Ademo%20redis%2Fx']);
  assert.deepEqual(ROUTES.confirm('../state'), ['POST', '/api/confirm/..%2Fstate']);
  const real = createReal({ fetchFn: async () => ({ status: 200, ok: true, json: async () => ({}) }) });
  for (const name of METHODS) assert.equal(typeof real[name], 'function', name);
  for (const [name, route] of Object.entries(ROUTES)) {
    const [method, path] = route('x');
    assert.match(method, /^(GET|POST|PATCH|DELETE)$/, name);
    assert.match(path, /^\/api\//, name);
  }
});

test('?mock picks the sample data', () => {
  assert.equal(wantsMock('?mock'), true);
  assert.equal(wantsMock('?mock=1&x=2'), true);
  assert.equal(wantsMock('?x=mock'), false);
  assert.equal(wantsMock(''), false);
  assert.equal(wantsMock(undefined), false);
});
