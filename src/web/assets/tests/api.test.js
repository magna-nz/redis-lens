// The adapters: the server's JSON → the view-models the views consume. The sample JSON
// here is what the real server sends: taken from docs/dashboard-api.md and from replies
// captured from the dev dashboard against the demo Redis and Valkey.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  ROUTES, METHODS, SIGN_IN, DOCKER_SIGN_IN, TEXT, addRequest, updateRequest, fmtSlots, adaptState, adaptConnections, adaptDocker,
  adaptTestResult, adaptAdded, adaptUpdated, adaptOverview, adaptFigures, adaptClients, adaptSlowlog, adaptMonitorRun, adaptMonitorEvent,
  adaptChannels, adaptSubscription, adaptTailEvent, adaptPublished, adaptReply, adaptNames, adaptHelp, adaptConfirm, adaptNotice,
  adaptError, adaptPolling, pollingRequest, adaptSessionWrites, adaptFinding, wantsMock, createReal,
} from '../api.js';

const CONNECTION = {
  auth: 'keychain', db: 0, endpoint: '127.0.0.1:6390', env: 'local', env_label: 'LOCAL', name: 'demo-redis', prefer_replica: false, provider: null,
  provider_notice: null, read_only: false, sentinel: null, sentinel_tls: false, ssh: null, standalone: false, tags: ['team-a'], tls: false,
  token_command: false, url: 'redis://127.0.0.1:6390/0', writes: true,
};
const SERVER = { flavor: 'redis', keys: 85963, lfu: true, maxmemory: 805306368, mode: 'standalone', policy: 'allkeys-lfu', used_memory: 218425448, version: '7.4.12' };
const IDLE = {
  connected_at_unix: null, connection: null, docker: false, error: null, features: { entra_id: false, version: '0.1.0' }, live: 'waiting', live_error: null,
  monitor: null, monitor_limits: { max_lines: 10000, max_secs: 30 }, nodes: [], picked: 0,
  polling: { interval_secs: 2, intervals: [1, 2, 3, 5, 10, 15, 30, 60], paused: false },
  refused: [], sentinel: null, server: null, status: 'idle', tail: null, target: null, writes: false, writes_off: null,
};
const CONNECTED = { ...IDLE, connected_at_unix: 1794725725, connection: CONNECTION, live: 'ready', refused: ['SET'], server: SERVER, status: 'connected', target: 'primary', writes: true };

test('state: idle', () => {
  const s = adaptState(IDLE);
  assert.equal(s.status, 'idle');
  assert.equal(s.connected, false);
  assert.equal(s.conn, null);
  assert.equal(s.connecting, null);
  assert.equal(s.failed, null);
  assert.equal(s.lost, null);
  assert.deepEqual(s.features, { entraId: false, version: '0.1.0' });
  assert.equal(adaptState(null).status, 'idle', 'an empty reply is idle, not a crash');
});

test('state: connected, with what the server is and the writes it refused', () => {
  const s = adaptState(CONNECTED);
  assert.equal(s.connected, true);
  assert.equal(s.liveState, 'ready');
  const c = s.conn;
  assert.equal(c.name, 'demo-redis');
  assert.equal(c.env, 'LOCAL');
  assert.equal(c.address, '127.0.0.1:6390');
  assert.equal(c.server, 'Redis');
  assert.equal(c.version, '7.4.12');
  assert.equal(c.topology, 'standalone');
  assert.equal(c.target, 'primary');
  assert.equal(c.readOnly, false);
  assert.deepEqual(c.refused, ['SET']);
  assert.deepEqual(c.tags, ['team-a']);
  assert.equal(c.signIn, SIGN_IN.keychain);
  assert.equal(c.pickedNode, null, 'no node picker on a standalone server');
  assert.equal(c.connectedAt, 1794725725);
  assert.equal(c.docker, false);
});

test('state: connecting, failed, and a closed live connection', () => {
  const connecting = adaptState({ ...IDLE, connection: CONNECTION, status: 'connecting', writes: true });
  assert.equal(connecting.connected, false);
  assert.deepEqual(connecting.connecting, { name: 'demo-redis', text: 'Connecting to demo-redis (127.0.0.1:6390) and checking permissions…' });
  const failed = adaptState({ ...IDLE, connection: { ...CONNECTION, name: 'zz-closed' }, status: 'failed', error: 'Connection refused (os error 61)' });
  assert.equal(failed.connected, false);
  assert.equal(failed.failed, 'Connection refused (os error 61)');
  assert.equal(failed.conn.name, 'zz-closed');
  assert.equal(adaptState({ ...IDLE, connection: CONNECTION, status: 'failed' }).failed, TEXT.connectFailed);
  assert.equal(adaptState({ ...CONNECTED, live: 'closed' }).lost, 'The live connection is closed.');
  assert.equal(adaptState({ ...CONNECTED, live: 'closed', live_error: 'the connection to the server was lost' }).lost, 'The live connection is closed: the connection to the server was lost');
  assert.equal(adaptState({ ...CONNECTED, live: 'waiting' }).lost, null);
});

test('state: how the live figures are polled, and when a Monitor run stops by itself', () => {
  const s = adaptState(IDLE);
  assert.deepEqual(s.polling, { paused: false, intervalSecs: 2, intervals: [1, 2, 3, 5, 10, 15, 30, 60] });
  assert.equal(s.live, true);
  assert.deepEqual(s.monitorLimits, { maxLines: 10000, maxSecs: 30 });
  const paused = adaptState({ ...CONNECTED, polling: { interval_secs: 5, intervals: [1, 2, 3, 5, 10, 15, 30, 60], paused: true }, monitor_limits: { max_lines: 500, max_secs: 10 } });
  assert.equal(paused.live, false);
  assert.equal(paused.polling.intervalSecs, 5);
  assert.deepEqual(paused.monitorLimits, { maxLines: 500, maxSecs: 10 });
  assert.equal('monitorSeconds' in paused.conn, false, 'the limits are the state\'s, not the connection\'s');
  const old = adaptState({ status: 'idle' });
  assert.deepEqual(old.polling, { paused: false, intervalSecs: 2, intervals: [1, 2, 3, 5, 10, 15, 30, 60] }, 'a reply without it falls back to the defaults');
  assert.deepEqual(old.monitorLimits, { maxLines: 10000, maxSecs: 30 });
});

test('state: a connect that cannot authenticate is failed, with the server\'s fixed reason', () => {
  const s = adaptState({ ...IDLE, connection: { ...CONNECTION, name: 'locked', auth: 'none' }, status: 'failed', live: 'closed', error: 'the server asks for a password, and this connection has none: add the connection again with its password' });
  assert.equal(s.connected, false);
  assert.match(s.failed, /^the server asks for a password/);
  assert.equal(s.lost, null, 'a failed connect is not a lost connection');
});

test('polling: the reply and the request', () => {
  assert.deepEqual(adaptPolling({ interval_secs: 5, intervals: [1, 2, 3], paused: true }), { paused: true, intervalSecs: 5, intervals: [1, 2, 3] });
  assert.deepEqual(pollingRequest({ paused: true }), { paused: true });
  assert.deepEqual(pollingRequest({ intervalSecs: '10' }), { interval_secs: 10 });
  assert.deepEqual(pollingRequest({ paused: false, intervalSecs: 1 }), { paused: false, interval_secs: 1 });
  assert.deepEqual(pollingRequest({}), {});
});

test('session writes: a saved connection answers with it, a container with the state', () => {
  const saved = adaptSessionWrites({ connection: { ...CONNECTION, read_only: true, writes: false }, message: 'Writes are off for demo-redis: it is read-only.' });
  assert.equal(saved.profile.readOnly, true);
  assert.equal(saved.message, 'Writes are off for demo-redis: it is read-only.');
  const container = adaptSessionWrites({ message: 'Writes are on for redis-lens-demo-valkey-1 in this session only (a container has no saved profile).', state: { ...CONNECTED, docker: true, connection: { ...CONNECTION, name: 'redis-lens-demo-valkey-1' } } });
  assert.equal(container.profile.name, 'redis-lens-demo-valkey-1');
  assert.equal(container.profile.readOnly, false);
  assert.match(container.message, /in this session only/);
  assert.deepEqual(adaptSessionWrites({}), { profile: null, message: null });
});

test('state: a read-only connection says why writes are off', () => {
  const s = adaptState({ ...CONNECTED, connection: { ...CONNECTION, name: 'demo-prod', env: 'prod', env_label: 'PROD', read_only: true, writes: false }, writes: false, writes_off: 'Not sent: demo-prod is read-only. Writes are turned on with w on 0 Connections.' });
  assert.equal(s.conn.readOnly, true);
  assert.equal(s.conn.env, 'PROD');
  assert.match(s.conn.writesOff, /read-only/);
  assert.equal(adaptState({ ...CONNECTED, writes: undefined }).conn.readOnly, true, 'read-only unless the server says writes are on');
});

test('state: a cluster has nodes and a picked one; a Docker connect is marked', () => {
  const s = adaptState({
    ...CONNECTED, docker: true, picked: 1,
    server: { ...SERVER, mode: 'cluster', flavor: 'valkey' },
    nodes: [
      { index: 0, id: 'aaa', endpoint: '127.0.0.1:7001', role: 'primary', slots: [[0, 5460]], slot_count: 5461, connected: true },
      { index: 1, id: 'bbb', endpoint: '127.0.0.1:7002', role: 'primary', slots: [[5461, 10922]], slot_count: 5462, connected: true },
      { index: 2, id: 'ccc', endpoint: '127.0.0.1:7003', role: 'replica', slots: [[10923, 16000], [16001, 16001], [16100, 16200], [16300, 16383]], slot_count: 5300, connected: false },
    ],
  });
  assert.equal(s.conn.docker, true);
  assert.equal(s.conn.server, 'Valkey');
  assert.equal(s.conn.topology, 'cluster');
  assert.equal(s.conn.pickedNode, '1');
  assert.deepEqual(s.conn.nodes.map((n) => [n.id, n.index, n.address, n.role, n.slots, n.connected]), [
    ['0', 0, '127.0.0.1:7001', 'primary', '0-5460', true],
    ['1', 1, '127.0.0.1:7002', 'primary', '5461-10922', true],
    ['2', 2, '127.0.0.1:7003', 'replica', '10923-16000, 16001, 16100-16200 +1 more', false],
  ]);
});

test('state: the tail, the Monitor run and a Sentinel answer', () => {
  const s = adaptState({
    ...CONNECTED,
    tail: { id: 5, kind: 'pattern', name: 'orders.*', node: 0, ready: true },
    monitor: { max_lines: 10000, max_secs: 30, node: 0, run: 6, started_at_unix_ms: 1794725990573 },
    sentinel: { sentinel: '10.0.0.1:26379', master: 'mymaster', primary: '10.0.0.7:6379' },
  });
  assert.deepEqual(s.tail, { id: 5, kind: 'pattern', name: 'orders.*', node: 0, ready: true });
  assert.deepEqual(s.monitor, { run: 6, node: 0, startedAt: 1794725990573, maxLines: 10000, maxSecs: 30 });
  assert.equal(s.conn.via, 'master mymaster at 10.0.0.7:6379, asked of Sentinel 10.0.0.1:26379');
});

test('slot ranges as text', () => {
  assert.equal(fmtSlots([[0, 5460]]), '0-5460');
  assert.equal(fmtSlots([[0, 100], [200, 200]]), '0-100, 200');
  assert.equal(fmtSlots([[0, 1], [2, 3], [4, 5], [6, 7], [8, 9]]), '0-1, 2-3, 4-5 +2 more');
  assert.equal(fmtSlots([]), null);
  assert.equal(fmtSlots(undefined), null);
});

test('connections: saved profiles, with words for where the credentials live and nothing else', () => {
  const c = adaptConnections({
    connected: 'demo-redis',
    connections: [
      CONNECTION,
      { ...CONNECTION, name: 'demo-prod', env: 'prod', env_label: 'PROD', read_only: true, writes: false, auth: 'command', token_command: true, tags: [], ssh: 'bastion:22', sentinel: 'mymaster via 10.0.0.1:26379 (+2)', sentinel_tls: true, provider: 'Azure', provider_notice: 'Azure Managed Redis and Enterprise tiers don\'t support MEMORY STATS, so dataset.bytes may be missing.', username: 'alice', password: 'hunter2' },
      { ...CONNECTION, name: 'nope', auth: 'none', tls: true, db: 2 },
    ],
  });
  assert.equal(c.connected, 'demo-redis');
  assert.equal(c.profiles.length, 3);
  const prod = c.profiles[1];
  assert.equal(prod.readOnly, true);
  assert.equal(prod.env, 'PROD');
  assert.equal(prod.signIn, SIGN_IN.command);
  assert.equal(prod.tokenCommand, true);
  assert.equal(prod.ssh, 'bastion:22');
  assert.equal(prod.sentinel, 'mymaster via 10.0.0.1:26379 (+2)');
  assert.equal(prod.sentinelTls, true);
  assert.equal(prod.provider, 'Azure');
  assert.match(prod.providerNotice, /MEMORY STATS/);
  assert.equal(c.profiles[2].signIn, SIGN_IN.none);
  assert.equal(c.profiles[2].tls, true);
  assert.equal(c.profiles[2].db, 2);
  assert.equal(JSON.stringify(c).includes('hunter2'), false, 'a password the server should never send is dropped');
  assert.equal(JSON.stringify(c).includes('alice'), false, 'and so is a username');
  assert.deepEqual(adaptConnections({}), { profiles: [], connected: null });
});

test('docker: containers, by what discovery found out about their sign-in', () => {
  const d = adaptDocker({ instances: [
    { auth: 'required', container: 'redis-lens-demo-redis-1', endpoint: '127.0.0.1:6390', image: 'redis:7.4-alpine', url: 'redis://127.0.0.1:6390/0' },
    { auth: 'none', container: 'redis-lens-demo-valkey-1', endpoint: '127.0.0.1:6391', image: 'valkey/valkey:8.1-alpine', url: 'redis://127.0.0.1:6391/0' },
    { auth: 'weird', container: 'x', endpoint: 'h:1', image: 'redis', url: 'rediss://h:1/0' },
  ] });
  assert.deepEqual(d.containers.map((c) => [c.name, c.docker, c.address, c.image, c.env]), [
    ['redis-lens-demo-redis-1', true, '127.0.0.1:6390', 'redis:7.4-alpine', 'LOCAL'],
    ['redis-lens-demo-valkey-1', true, '127.0.0.1:6391', 'valkey/valkey:8.1-alpine', 'LOCAL'],
    ['x', true, 'h:1', 'redis', 'LOCAL'],
  ]);
  assert.equal(d.containers[0].signIn, DOCKER_SIGN_IN.required);
  assert.equal(d.containers[1].signIn, DOCKER_SIGN_IN.none);
  assert.equal(d.containers[2].signIn, DOCKER_SIGN_IN.unknown);
  assert.equal(d.containers[2].tls, true);
  assert.deepEqual(adaptDocker({}), { containers: [] });
});

test('a test result lists each check, and counts only refusals against it', () => {
  const r = adaptTestResult({
    at: '127.0.0.1:6390', connected: true, connection: CONNECTION, sentinel: null,
    checks: [
      { label: 'SCAN', reason: null, status: 'allowed' },
      { label: 'MEMORY USAGE', reason: 'NOPERM MEMORY USAGE is not allowed for this user', status: 'denied' },
      { label: 'OBJECT FREQ', reason: 'the maxmemory policy is not LFU', status: 'skipped' },
    ],
  });
  assert.equal(r.ok, false);
  assert.match(r.summary, /^Limited/);
  assert.deepEqual(r.checks, [
    { name: 'SCAN', status: 'allowed', ok: true, why: null },
    { name: 'MEMORY USAGE', status: 'denied', ok: false, why: 'NOPERM MEMORY USAGE is not allowed for this user' },
    { name: 'OBJECT FREQ', status: 'skipped', ok: false, why: 'the maxmemory policy is not LFU' },
  ]);
  assert.deepEqual(r.notes, ['Connected to 127.0.0.1:6390. A test reads no key.']);
  const fine = adaptTestResult({ at: 'h:1', checks: [{ label: 'SCAN', status: 'allowed' }, { label: 'FT._LIST', status: 'skipped', reason: 'the Search module is not loaded' }], sentinel: 'Sentinel 10.0.0.1:26379 says mymaster is at 10.0.0.7:6379.' });
  assert.equal(fine.ok, true, 'a check that was skipped is not a refusal');
  assert.equal(fine.summary, 'Every check passed.');
  assert.equal(fine.notes.length, 2);
});

test('added, updated: the message and the saved connection', () => {
  const added = adaptAdded({ connection: { ...CONNECTION, name: 'staging', auth: 'keychain' }, message: 'Saved staging (10.0.0.5:6379, STAGE). Credentials are in the OS keychain. Tags: team-b.' });
  assert.equal(added.name, 'staging');
  assert.match(added.message, /^Saved staging/);
  const updated = adaptUpdated({ connection: { ...CONNECTION, read_only: true, writes: false }, message: 'Writes are off for prod-cache: it is read-only.' });
  assert.equal(updated.profile.readOnly, true);
  assert.equal(updated.message, 'Writes are off for prod-cache: it is read-only.');
  assert.deepEqual(adaptAdded({}), { name: null, message: null, profile: null });
});

test('the add request carries the password once, and only fields the server takes', () => {
  const body = addRequest({
    name: 'cache-02', url: 'rediss://app@cache-02:6380/0', env: 'PROD', password: 'hunter2', tags: ['a'], writes: null,
    provider: '', sshHost: '', sentinelAddresses: [], tokenCommand: [], entra: false, preferReplica: false, standalone: false,
  });
  assert.deepEqual(body, { name: 'cache-02', url: 'rediss://app@cache-02:6380/0', tags: ['a'], env: 'prod', password: 'hunter2' });
  assert.equal(JSON.stringify(body).split('hunter2').length - 1, 1);
});

test('the add request: every advanced field, named as the server names it', () => {
  const body = addRequest({
    name: 'n', url: 'redis://h', env: '', password: '', tags: [], writes: false,
    provider: 'memorydb', sshHost: 'ops@bastion:2222', sshIdentity: '~/.ssh/id', sshKnownHosts: '',
    sentinelAddresses: ['s1:26379', 's2:26379'], sentinelMaster: 'mymaster', sentinelUser: 'sen', sentinelPassword: 'sp', sentinelTls: true,
    tokenCommand: ['aws', 'token', 'my cache'], entra: true, preferReplica: true, standalone: true,
  });
  assert.deepEqual(body, {
    name: 'n', url: 'redis://h', writes: false, provider: 'memorydb', ssh: 'ops@bastion:2222', ssh_identity: '~/.ssh/id',
    sentinel: 's1:26379,s2:26379', sentinel_master: 'mymaster', sentinel_tls: true, sentinel_user: 'sen', sentinel_password: 'sp',
    token_command: ['aws', 'token', 'my cache'], entra: true, prefer_replica: true, standalone: true,
  });
  const allowed = ['name', 'url', 'env', 'password', 'tags', 'writes', 'provider', 'ssh', 'ssh_identity', 'ssh_known_hosts', 'sentinel', 'sentinel_master',
    'sentinel_tls', 'sentinel_user', 'sentinel_password', 'token_command', 'entra', 'standalone', 'prefer_replica', 'force'];
  for (const key of Object.keys(body)) assert.ok(allowed.includes(key), `${key} is a field the server takes`);
});

test('the update request: writes, tags to add and remove, standalone; only what changed', () => {
  assert.deepEqual(updateRequest({ writes: true }), { writes: true });
  assert.deepEqual(updateRequest({ addTags: ['x'], removeTags: ['y'], standalone: false }), { tag: ['x'], untag: ['y'], standalone: false });
  assert.deepEqual(updateRequest({ addTags: [], removeTags: [] }), {});
});

const FIGURES = {
  info: { aof_ok: true, aof_on: false, blocked: 0, clients: 2, commands: 70, evicted: 0, expired: 0, fragmentation: 9.8, hits: 4, keys: 1, link_up: null, maxmemory: 0, misses: 0, ops_now: 0, policy: 'noeviction', rdb_ok: true, replicas: 0, role: 'primary', uptime_secs: 2, used_memory: 1543912 },
  node: 0,
  rates: { blocked: 0, clients: 2, cpu_sys: 0.18385597476726265, cpu_user: 0.21549676955619976, evicted: 0.0, expired: 0.0, fragmentation: 9.8, hit_rate: null, net_in: 13.973852588174143, net_out: 5577.563447336936, ops: 0.9981323277267246, used_memory: 1543912 },
};

test('figures: the event as the server sends it', () => {
  const f = adaptFigures(FIGURES);
  assert.equal(f.node, '0');
  assert.equal(f.error, null);
  assert.equal(f.opsPerSec, 0.9981323277267246);
  assert.equal(f.opsNow, 0);
  assert.equal(f.clients, 2);
  assert.equal(f.usedMemory, 1543912);
  assert.equal(f.maxMemory, null, 'a zero limit means no limit');
  assert.equal(f.hitRate, null, 'no hit rate yet stays unknown');
  assert.equal(f.evictedPerSec, 0);
  assert.equal(f.uptimeSecs, 2);
  assert.equal(f.role, 'primary');
  assert.equal(f.replicas, 0);
  assert.equal(f.linkUp, null);
  assert.equal(f.rdbOk, true);
  assert.equal(f.aofOn, false);
  assert.equal(f.policy, 'noeviction');
  assert.equal(f.cpuUser, 0.21549676955619976);
  assert.equal(f.shards, null);
});

test('figures: the first poll has no rates, a refused INFO is an error, a cluster has shard readings', () => {
  const first = adaptFigures({ ...FIGURES, rates: { ...FIGURES.rates, ops: null, evicted: null, cpu_user: null } });
  assert.equal(first.opsPerSec, null, 'a rate needs two polls: never a made-up first value');
  assert.equal(first.opsNow, 0);
  const refused = adaptFigures({ node: 0, error: 'the server refused INFO for this user' });
  assert.equal(refused.error, 'the server refused INFO for this user');
  assert.equal(refused.clients, null);
  const cluster = adaptFigures({ ...FIGURES, node: 1, shards: [{ node: 0, info: { ...FIGURES.info, ops_now: 12, keys: 900 }, failed: false }, { node: 1, info: null, failed: true }] });
  assert.equal(cluster.node, '1');
  assert.deepEqual(cluster.shards, [
    { id: '0', failed: false, opsPerSec: 12, usedMemory: 1543912, keys: 900, role: 'primary' },
    { id: '1', failed: true, opsPerSec: null, usedMemory: null, keys: null, role: null },
  ]);
});

const OVERVIEW = {
  figures: {
    big_keys: null, dataset_bytes: null,
    gaps: ['This server has no INFO keysizes (Redis 8 and later have it), so there is no exact size histogram', 'MEMORY STATS is denied for this user (ACL +memory|stats), so dataset.bytes is missing'],
    keys: 85963, overhead_bytes: null, server: SERVER, ttl_share: 0.8330444493561183, with_ttl: 71611,
  },
  figures_failed: null,
  findings: [{ area: 'Server', panel: 'Slow log', screen: 'slow_log', severity: 'note', text: 'EVAL took 1.3 s at 06:52 UTC.' }],
  last_analysis: null, live: null,
  perms: [
    { label: 'SCAN', reason: null, status: 'allowed' },
    { label: 'MEMORY STATS', reason: 'NOPERM MEMORY STATS is not allowed for this user', status: 'denied' },
    { label: 'FT._LIST', reason: 'the Search module is not loaded', status: 'skipped' },
  ],
  provider: null, server: SERVER, shards: [], status: 'connected', target: 'primary',
};

test('overview: as captured from the demo Redis', () => {
  const o = adaptOverview(OVERVIEW);
  assert.deepEqual(o.server, { kind: 'Redis', version: '7.4.12', topology: 'standalone', policy: 'allkeys-lfu', lfu: true });
  assert.deepEqual(o.memory, { used: 218425448, max: 805306368 });
  assert.equal(o.keys.total, 85963);
  assert.equal(o.keys.withTtl, 71611);
  assert.equal(o.keys.dataBytes, null, 'MEMORY STATS was refused: unknown, not zero');
  assert.equal(o.keys.overheadBytes, null);
  assert.equal(o.keys.bigKeys, null);
  assert.equal(o.gaps.length, 2);
  assert.deepEqual(o.checks.map((c) => c.status), ['allowed', 'denied', 'skipped']);
  assert.equal(o.figures, null);
  assert.equal(o.lastAnalysis, null);
  assert.deepEqual(o.shards, []);
  assert.deepEqual(o.findings, [{ text: 'EVAL took 1.3 s at 06:52 UTC.', severity: 'note', screen: 'slow_log', label: 'Server › Slow log', where: 'slowlog' }]);
});

test('findings: the server\'s words; a link only to a panel this page has', () => {
  assert.deepEqual(adaptFinding({ area: 'Server', panel: 'Clients', screen: 'clients', severity: 'warn', text: '212 connections from 10.0.9.17 idle for over an hour.' }),
    { text: '212 connections from 10.0.9.17 idle for over an hour.', severity: 'warn', screen: 'clients', label: 'Server › Clients', where: 'clients' });
  const big = adaptFinding({ area: 'Analyze', panel: 'Big keys', screen: 'big_keys', severity: 'warn', text: '3 big keys, counted by the server (INFO keysizes).' });
  assert.equal(big.where, null, 'Analyze arrives later: named, not linked');
  assert.equal(big.label, 'Analyze › Big keys');
  assert.equal(adaptFinding({ text: 'x', severity: 'loud' }).severity, 'note');
  assert.deepEqual(adaptOverview({ ...OVERVIEW, findings: [{ text: '' }, { text: 'kept', screen: 'slow_log' }] }).findings.map((f) => f.text), ['kept']);
  assert.deepEqual(adaptOverview({ ...OVERVIEW, findings: undefined }).findings, []);
});

test('overview: a failed connect, a provider, a last analysis, live figures', () => {
  const failed = adaptOverview({ figures: null, figures_failed: null, last_analysis: null, live: null, perms: [], provider: null, server: null, shards: [], status: 'failed', target: null });
  assert.equal(failed.status, 'failed');
  assert.equal(failed.keys.total, null);
  assert.equal(failed.memory.used, null);
  assert.equal(failed.server.kind, null);
  const o = adaptOverview({
    ...OVERVIEW, figures: null, figures_failed: "INFO or DBSIZE could not be read, so the server's figures are missing",
    provider: { label: 'ElastiCache Serverless', notice: 'ElastiCache Serverless restricts MEMORY and OBJECT, so sizes and hot keys are missing; counts and TTLs still work.' },
    last_analysis: { taken_at_unix: 1794700000, sampled_keys: 100000 }, live: { node: 0, info: FIGURES.info, rates: null },
  });
  assert.match(o.figuresFailed, /figures are missing/);
  assert.equal(o.keys.total, 85963, 'the server block still says how many keys');
  assert.equal(o.provider.label, 'ElastiCache Serverless');
  assert.deepEqual(o.lastAnalysis, { at: 1794700000000, sampledKeys: 100000 });
  assert.equal(o.figures.clients, 2);
  assert.equal(o.figures.opsPerSec, null);
});

test('overview: shard rows of a cluster', () => {
  const node = (index, connected = true) => ({ index, id: `id${index}`, endpoint: `127.0.0.1:700${index + 1}`, role: 'primary', slots: [[index * 5461, index * 5461 + 5460]], slot_count: 5461, connected });
  const o = adaptOverview({
    ...OVERVIEW,
    shards: [
      { node: node(0), figures: { ...OVERVIEW.figures, keys: 300 }, note: null, live: { ...FIGURES.info, ops_now: 40, keys: 310, used_memory: 5000, role: 'primary' }, failed: false },
      { node: node(1), figures: { ...OVERVIEW.figures, keys: 200 }, note: null, live: null, failed: true },
      { node: node(2, false), figures: null, note: '127.0.0.1:7003 is not connected (discovery could not use it), so nothing is sent to it', live: null, failed: false },
    ],
  });
  assert.deepEqual(o.shards.map((s) => [s.id, s.slots, s.address, s.status, s.opsPerSec, s.usedMemory, s.keys]), [
    ['0', '0-5460', '127.0.0.1:7001', 'ok', 40, 5000, 310],
    ['1', '5461-10921', '127.0.0.1:7002', 'no-answer', null, 218425448, 200],
    ['2', '10922-16382', '127.0.0.1:7003', 'not-connected', null, null, null],
  ]);
  assert.match(o.shards[2].note, /not connected/);
});

test('clients: connections and groups; a refused list is off', () => {
  const c = adaptClients({
    connections: [
      { addr: '192.168.65.1:29502', age: 2, cmd: 'set', db: 0, id: 3, idle: 2, lib: 'redis-rs 1.8.1', mem: 22400, name: '', own: false },
      { addr: '192.168.65.1:18752', age: 2, cmd: 'client|list', db: 0, id: 7, idle: 0, lib: 'redis-rs 1.8.1', mem: 23450, name: '', own: true },
    ],
    groups: [
      { connections: 2, flagged: false, idle_conns: 0, kind: 'lib', label: 'redis-rs 1.8.1', longest_idle: 2, mem: 45850, members: [0, 1], oldest_age: 2, top_cmd: 'client|list' },
      { connections: 2, flagged: true, idle_conns: 1, kind: 'addr', label: '192.168.65.0/24', longest_idle: 4000, mem: 45850, members: [0, 1], oldest_age: 2, top_cmd: 'client|list' },
    ],
    node: 0,
  });
  assert.equal(c.off, null);
  assert.equal(c.total, 2);
  assert.deepEqual(c.connections[1], { id: 7, addr: '192.168.65.1:18752', name: null, lib: 'redis-rs 1.8.1', ageSecs: 2, idleSecs: 0, cmd: 'client|list', db: 0, memory: 23450, own: true });
  assert.deepEqual(c.groups.map((g) => [g.by, g.name, g.connections, g.oldestSecs, g.idleSecs, g.idleConns, g.lastCommand, g.memory, g.flagged, g.members]), [
    ['library', 'redis-rs 1.8.1', 2, 2, 2, 0, 'client|list', 45850, false, [0, 1]],
    ['address', '192.168.65.0/24', 2, 2, 4000, 1, 'client|list', 45850, true, [0, 1]],
  ]);
  const off = adaptClients({ off: 'the server refused CLIENT LIST for this user' });
  assert.equal(off.off, 'the server refused CLIENT LIST for this user');
  assert.equal(off.total, null);
  assert.deepEqual(off.groups, []);
});

test('slow log: entries with a masked line, the settings or why they are missing', () => {
  const s = adaptSlowlog({
    entries: [
      { args_cut: 0, client: '192.168.65.1:29502', client_name: '', command: 'CONFIG SET', id: 70, keys: [], line: 'CONFIG SET masterauth ***', micros: 7, more_keys: 0, note: null, unix: 1794724962 },
      { args_cut: 3, client: '10.0.4.17:51122', client_name: 'worker', command: 'LRANGE', id: 69, keys: ['queue:emails'], line: 'LRANGE queue:emails 0 -1', micros: 912000, more_keys: 2, note: 'Reads a whole list.', unix: 1794724961 },
    ],
    keys_off: null, node: 0, settings: { max_len: 128, threshold_micros: 0 }, settings_off: null,
  });
  assert.deepEqual(s.settings, { thresholdMicros: 0, maxLen: 128 });
  assert.deepEqual(s.entries[0], { id: 70, at: 1794724962000, micros: 7, name: 'CONFIG SET', command: 'CONFIG SET masterauth ***', keys: [], moreKeys: 0, client: '192.168.65.1:29502', clientName: null, argsCut: 0, note: null });
  assert.equal(s.entries[1].clientName, 'worker');
  assert.equal(s.entries[1].moreKeys, 2);
  assert.equal(s.entries[1].argsCut, 3);
  const refused = adaptSlowlog({ entries: [], keys_off: 'the server refused COMMAND INFO for this user', node: 0, settings: null, settings_off: 'the server refused CONFIG GET for this user' });
  assert.equal(refused.settings, null);
  assert.equal(refused.settingsOff, 'the server refused CONFIG GET for this user');
  assert.equal(refused.keysOff, 'the server refused COMMAND INFO for this user');
  assert.equal(adaptSlowlog({ off: 'the server refused SLOWLOG for this user' }).off, 'the server refused SLOWLOG for this user');
});

test('monitor: the start reply and the three kinds of event', () => {
  assert.deepEqual(adaptMonitorRun({ max_lines: 10000, max_secs: 30, run: 1 }), { run: 1, maxLines: 10000, maxSecs: 30 });
  const ready = adaptMonitorEvent({ at_unix_ms: 1794724962129, kind: 'ready', max_lines: 10000, max_secs: 30, run: 1 });
  assert.equal(ready.kind, 'ready');
  assert.equal(ready.startedAt, 1794724962129);
  assert.equal(ready.maxSecs, 30);
  assert.equal(ready.stopped, null);
  const lines = adaptMonitorEvent({ kind: 'lines', lines: [{ client: '192.168.65.1:29502', cut: 0, db: '0', line: 'SET user:2 y', millis: 129, unix: 1794724962 }, { client: '127.0.0.1:49406', cut: 2, db: '0', line: 'AUTH *** ***', millis: 934, unix: null }], run: 1 });
  assert.deepEqual(lines.lines, [
    { at: 1794724962129, db: '0', client: '192.168.65.1:29502', command: 'SET user:2 y', cut: 0 },
    { at: null, db: '0', client: '127.0.0.1:49406', command: 'AUTH *** ***', cut: 2 },
  ]);
  assert.equal(adaptMonitorEvent({ end: 'stopped', error: null, kind: 'closed', run: 1 }).stopped, 'MONITOR stopped.');
  assert.match(adaptMonitorEvent({ end: 'time_up', error: null, kind: 'closed', run: 1 }).stopped, /its time was up/);
  assert.match(adaptMonitorEvent({ end: 'line_cap', error: null, kind: 'closed', run: 1 }).stopped, /line limit/);
  assert.equal(adaptMonitorEvent({ end: 'failed', error: 'the server refused MONITOR for this user', kind: 'closed', run: 4 }).stopped, 'MONITOR stopped: the server refused MONITOR for this user.');
});

test('channels: classic and shard channels in one list, each shard channel with its node', () => {
  const ch = adaptChannels({
    channels: [{ name: 'orders', subscribers: 1 }], node: 0, notes: [], patterns: 0,
    shards: { nodes: [{ channels: [], node: 0, notes: [], total_channels: 0 }, { channels: [{ name: 'shard.orders', subscribers: 3 }], node: 1, notes: ['SHARDNUMSUB was refused'], total_channels: 1 }], off: null },
    total_channels: 1,
  });
  assert.deepEqual(ch.channels, [{ name: 'orders', subscribers: 1, shard: false, node: null }, { name: 'shard.orders', subscribers: 3, shard: true, node: 1 }]);
  assert.equal(ch.total, 1);
  assert.equal(ch.patterns, 0);
  assert.equal(ch.shardOff, null);
  assert.deepEqual(ch.notes, ['SHARDNUMSUB was refused']);
});

test('channels: shard channels off for the server, or refused on every node; the whole list off', () => {
  const old = adaptChannels({ channels: [], node: 0, notes: [], patterns: 0, shards: { nodes: [], off: 'shard channels need Redis 7.0 or later' }, total_channels: 0 });
  assert.equal(old.shardOff, 'shard channels need Redis 7.0 or later');
  const refused = adaptChannels({ channels: [], node: 0, notes: [], patterns: 0, shards: { nodes: [{ node: 0, off: 'the server refused PUBSUB SHARDCHANNELS for this user' }], off: null }, total_channels: 0 });
  assert.ok(refused.shardOff);
  assert.deepEqual(refused.notes, ['Shard channels: the server refused PUBSUB SHARDCHANNELS for this user']);
  assert.equal(adaptChannels({ off: 'the server refused PUBSUB CHANNELS for this user' }).off, 'the server refused PUBSUB CHANNELS for this user');
});

test('subscribe, tail events and publish', () => {
  assert.deepEqual(adaptSubscription({ kind: 'pattern', node: 0, tail: 2 }, 'orders.*'), { id: 2, name: 'orders.*', kind: 'pattern', node: 0 });
  assert.deepEqual(adaptTailEvent({ kind: 'ready', tail: 2 }), { id: 2, kind: 'ready', messages: [], skipped: 0, error: null });
  const t = adaptTailEvent({ dropped: 3, kind: 'messages', messages: [{ at_unix_ms: 1794724962243, bytes: 9, channel: 'orders', pattern: null, preview: '<img src=x onerror=alert(1)>' }], tail: 3 });
  assert.equal(t.id, 3);
  assert.equal(t.skipped, 3);
  assert.deepEqual(t.messages, [{ at: 1794724962243, channel: 'orders', body: '<img src=x onerror=alert(1)>', bytes: 9, pattern: null }], 'text from Redis is data and stays text');
  assert.equal(adaptTailEvent({ error: 'the server refused SUBSCRIBE on that channel for this user', kind: 'closed', tail: 3 }).error, 'the server refused SUBSCRIBE on that channel for this user');
  assert.equal(adaptTailEvent({ error: null, kind: 'closed', tail: 5 }).error, null);
  assert.deepEqual(adaptPublished({ reply: { kind: 'int', value: 1 }, sent: true }), { receivers: 1 });
  assert.deepEqual(adaptPublished({ sent: true }), { receivers: null });
});

const REPLY = { cut: false, error: false, json: ['"x"'], raw: ['[bulk string] $1 "x"'], table: ['"x"'], text: ['"x"'], took_ms: 0.418625 };

test('a command reply: the four views as lines, the masked line, how long it took', () => {
  const r = adaptReply({ line: 'GET user:1', node: null, replies: [REPLY], route_note: null, sent: 1, status: 'ok', text: null, times: 1 });
  assert.equal(r.kind, 'reply');
  assert.equal(r.input, 'GET user:1');
  assert.equal(r.replies.length, 1);
  assert.deepEqual(r.replies[0].views, { text: '"x"', table: '"x"', json: '"x"', raw: '[bulk string] $1 "x"' });
  assert.equal(Math.round(r.replies[0].micros), 419);
  const keys = adaptReply({ line: 'KEYS user:*', node: '127.0.0.1:7002', replies: [{ ...REPLY, text: ['1) "user:2"', '2) "user:1"'], table: ['#  value', '1  user:2', '2  user:1'] }], route_note: 'sent to the picked node', sent: 1, status: 'ok', text: null, times: 1 });
  assert.equal(keys.replies[0].views.text, '1) "user:2"\n2) "user:1"');
  assert.equal(keys.replies[0].views.table, '#  value\n1  user:2\n2  user:1');
  assert.equal(keys.node, '127.0.0.1:7002');
  assert.equal(keys.routeNote, 'sent to the picked node');
});

test('a command reply: a repeat, a note, a failure and an error from the server', () => {
  const repeat = adaptReply({ line: '2 GET user:1', node: null, replies: [REPLY, REPLY], route_note: null, sent: 2, status: 'ok', text: null, times: 2 });
  assert.equal(repeat.times, 2);
  assert.equal(repeat.replies.length, 2);
  const note = adaptReply({ line: 'SET user:3 z', node: null, replies: [], route_note: null, sent: 0, status: 'note', text: 'Not sent: prod-cache is read-only. Writes are turned on with w on 0 Connections.', times: 1 });
  assert.equal(note.kind, 'refused');
  assert.match(note.text, /read-only/);
  assert.deepEqual(note.replies, []);
  const failed = adaptReply({ line: 'SET a b', status: 'failed', text: 'the server refused SET for this user', node: null, route_note: null, times: 1, sent: 0, replies: [] });
  assert.equal(failed.kind, 'error');
  const error = adaptReply({ line: 'GET', status: 'ok', text: null, replies: [{ ...REPLY, text: ["(error) ERR wrong number of arguments for 'get' command"], error: true }], times: 1, sent: 1 });
  assert.equal(error.kind, 'reply');
  assert.equal(error.replies[0].error, true);
  assert.equal(adaptReply({ line: 'AUTH *** ***', status: 'note', text: 'Not sent: it would change the connection every view shares.', replies: [] }).input, 'AUTH *** ***');
});

test('command names and help', () => {
  assert.deepEqual(adaptNames({ names: ['acl', 'acl|cat', 'get'] }), { off: null, names: ['acl', 'acl|cat', 'get'] });
  assert.deepEqual(adaptNames({ off: 'the server refused COMMAND LIST for this user' }), { off: 'the server refused COMMAND LIST for this user', names: [] });
  const help = adaptHelp({ arguments: 'key', complexity: 'O(1)', name: 'get', since: '1.0.0', subcommands: [], summary: 'Returns the string value of a key.' });
  assert.deepEqual(help, { name: 'get', summary: 'Returns the string value of a key.', since: '1.0.0', complexity: 'O(1)', arguments: 'key', subcommands: [] });
  const acl = adaptHelp({ name: 'acl', summary: 'A container for Access List Control commands.', since: '6.0.0', complexity: 'Depends on subcommand.', arguments: '', subcommands: [{ name: 'acl|cat', summary: 'Lists the ACL categories.', since: '6.0.0', complexity: 'O(1)', arguments: '[category]', subcommands: [] }] });
  assert.equal(acl.arguments, null);
  assert.equal(acl.subcommands[0].name, 'acl|cat');
  assert.equal(adaptHelp({ off: 'this server has no COMMAND DOCS' }), null);
});

test('confirm and notice', () => {
  assert.deepEqual(adaptConfirm({ confirm: { expect: 'prod-cache', id: '06edda37fc81534abda0233773a0cce6', kind: 'type', text: 'prod-cache is a prod profile. Type its name to turn writes on: ' } }),
    { id: '06edda37fc81534abda0233773a0cce6', kind: 'type', text: 'prod-cache is a prod profile. Type its name to turn writes on:', expect: 'prod-cache', title: null });
  assert.deepEqual(adaptConfirm({ confirm: { expect: null, id: '6c79', kind: 'ask', text: 'KEYS is @dangerous: it can slow the server down. Send it? KEYS user:*' } }),
    { id: '6c79', kind: 'ask', text: 'KEYS is @dangerous: it can slow the server down. Send it? KEYS user:*', expect: '', title: null });
  assert.equal(adaptConfirm({ confirm: { id: 'a3', kind: 'something', expect: 'prod' } }).kind, 'type', 'a name to type is never downgraded to a plain Yes');
  assert.deepEqual(adaptNotice({ text: 'The live connection closed.' }), { text: 'The live connection closed.', kind: 'info' });
});

test('errors: the server\'s own wording, or a fixed line', () => {
  const notLive = adaptError(409, { error: 'not connected: connect first, or the live connection is closed' });
  assert.equal(notLive.message, 'not connected: connect first, or the live connection is closed');
  assert.equal(notLive.notLive, true);
  assert.equal(adaptError(422, { error: 'Not sent: demo-prod is read-only. Writes are turned on with w on 0 Connections.' }).notLive, false);
  assert.equal(adaptError(410, { error: 'that question is no longer open: it was answered, cancelled or expired' }).status, 410);
  assert.equal(adaptError(502, { error: 'Connection refused (os error 61)' }).message, 'Connection refused (os error 61)');
  assert.equal(adaptError(500, { message: 'panic at src/web', stack: '…' }).message, TEXT.failed);
  assert.equal(adaptError(500, null).message, TEXT.failed);
  assert.equal(adaptError(401, { error: 'not signed in' }).locked, true);
  assert.equal(adaptError(403, { error: 'forbidden' }).message, TEXT.forbidden);
});

test('routes: as docs/dashboard-api.md lists them, with names escaped into the path', () => {
  assert.deepEqual(ROUTES.docker(), ['GET', '/api/docker']);
  assert.deepEqual(ROUTES.command(), ['POST', '/api/cmdline']);
  assert.deepEqual(ROUTES.commandNames(), ['GET', '/api/cmdline/commands']);
  assert.deepEqual(ROUTES.commandHelp('config|get'), ['GET', '/api/cmdline/help?name=config%7Cget']);
  assert.deepEqual(ROUTES.update('demo redis/x'), ['PATCH', '/api/connections/demo%20redis%2Fx']);
  assert.deepEqual(ROUTES.confirm('../state'), ['POST', '/api/confirm/..%2Fstate']);
  assert.deepEqual(ROUTES.polling(), ['POST', '/api/polling']);
  assert.deepEqual(ROUTES.sessionWrites(), ['POST', '/api/session/writes']);
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

test('a command line reply: the replies dropped past 4 MiB, and the TUI\'s notice for them', () => {
  const r = adaptReply({ line: '500 GET k', status: 'ok', replies: [], times: 500, sent: 500, dropped: 120, dropped_note: '(the first 120 replies were dropped: the output keeps at most 4 MiB)' });
  assert.equal(r.dropped, 120);
  assert.equal(r.droppedNote, '(the first 120 replies were dropped: the output keeps at most 4 MiB)');
  const plain = adaptReply({ line: 'PING', status: 'ok', replies: [] });
  assert.equal(plain.dropped, 0);
  assert.equal(plain.droppedNote, null);
});
