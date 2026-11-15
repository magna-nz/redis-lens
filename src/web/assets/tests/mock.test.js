// The sample data behind `?mock`: a stand-in for the redis-lens process that answers the
// same routes with the same JSON. These tests drive it through the same `createReal` and
// adapters the page uses, so they also check that the page speaks the API as documented
// (the mock refuses a body with a field the real server would refuse).
import test from 'node:test';
import assert from 'node:assert/strict';
import { METHODS, createApi, createReal } from '../api.js';
import { createMockServer, maskedAddress, sampleMessage } from '../mock.js';

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function mockApi(options) {
  const server = createMockServer(options);
  const backend = createReal({ ...server, mock: true });
  const api = createApi(backend);
  return { api, backend, server };
}

/** Connect and wait until the live connection is ready, as the page does through state. */
async function connected(api, target) {
  const states = [];
  const off = api.on('state', (s) => states.push(s));
  api.start();
  await api.connect(target);
  for (let i = 0; i < 40 && !(states.length && states[states.length - 1].connected && states[states.length - 1].liveState === 'ready'); i += 1) await wait(50);
  off();
  return states;
}

test('every method the page calls is answered by a route', () => {
  const { backend } = mockApi();
  for (const name of METHODS.concat(['answerConfirm', 'cancelConfirm', 'open', 'close', 'reopen'])) assert.equal(typeof backend[name], 'function', name);
  assert.equal(backend.mock, true);
});

test('before a connect: idle, the saved connections and the Docker containers', async () => {
  const { api } = mockApi();
  const state = await api.getState();
  assert.equal(state.status, 'idle');
  assert.equal(state.conn, null);
  const list = await api.listConnections();
  assert.deepEqual(list.profiles.map((p) => p.name), ['prod-cache-01', 'staging-cache', 'demo-cluster', 'limited-user']);
  assert.equal(list.connected, null);
  assert.equal(list.profiles[0].readOnly, true, 'production is read-only by default');
  assert.deepEqual((await api.listDocker()).containers.map((p) => [p.name, p.docker]), [['demo-redis', true], ['demo-valkey', true], ['legacy-redis', true]]);
  assert.equal(state.live, true);
  assert.equal(state.polling.intervalSecs, 2, 'the figures are read every 2 s to start with');
  assert.deepEqual(state.monitorLimits, { maxLines: 10000, maxSecs: 30 }, 'known before a run starts');
  await assert.rejects(api.getClients(), (err) => err.notLive === true);
  await assert.rejects(api.getOverview(), (err) => err.status === 409);
});

test('connect goes through connecting to connected, then the live connection is ready', async () => {
  const { api } = mockApi();
  const states = await connected(api, { name: 'prod-cache-01' });
  api.stop();
  assert.deepEqual(Array.from(new Set(states.map((s) => `${s.status}/${s.liveState}`))), ['idle/waiting', 'connecting/waiting', 'connected/waiting', 'connected/ready']);
  const last = states[states.length - 1];
  assert.equal(last.conn.name, 'prod-cache-01');
  assert.equal(last.conn.readOnly, true);
  assert.equal(last.conn.server, 'Redis');
  const overview = await api.getOverview();
  assert.equal(overview.keys.total, 7920114);
  assert.equal(overview.shards.length, 0, 'no shard rows on a standalone server');
  assert.ok(overview.lastAnalysis, 'this profile has a saved analysis');
  assert.deepEqual(overview.findings.map((f) => [f.severity, f.where, f.label]), [['note', 'server/slowlog', 'Server › Slow log'], ['warn', 'server/clients', 'Server › Clients']]);
  assert.equal((await api.listConnections()).connected, 'prod-cache-01');
});

test('a connect to a closed port fails, in the server\'s words, and can be disconnected', async () => {
  const { api } = mockApi();
  await api.addConnection({ name: 'closed', url: 'redis://127.0.0.1:6399', tags: [] });
  const states = [];
  api.on('state', (s) => states.push(s));
  api.start();
  await api.connect({ name: 'closed' });
  await wait(900);
  api.stop();
  const last = states[states.length - 1];
  assert.equal(last.status, 'failed');
  assert.equal(last.failed, 'Connection refused (os error 61)');
  assert.equal(last.connected, false);
  await assert.rejects(api.testConnection('closed'), (err) => err.status === 502 && err.message === 'Connection refused (os error 61)');
  await assert.rejects(api.connect({ name: 'nope' }), (err) => err.status === 404);
  assert.equal((await api.disconnect()).status, 'idle');
});

test('a connect that cannot authenticate ends failed, never connected', async () => {
  const { api } = mockApi();
  const states = [];
  api.on('state', (s) => states.push(s));
  api.start();
  await api.connect({ docker: 'legacy-redis' });
  await wait(900);
  api.stop();
  const last = states[states.length - 1];
  assert.equal(last.status, 'failed');
  assert.equal(last.connected, false);
  assert.match(last.failed, /^the server asks for a password, and this connection has none/);
  assert.equal(states.some((s) => s.connected), false);
});

test('pausing stops the figures; the interval spaces them; both are in the state for every tab', async () => {
  const { api } = mockApi();
  const figures = [];
  const states = [];
  api.on('figures', (f) => figures.push(f));
  await connected(api, { name: 'staging-cache' });
  api.on('state', (s) => states.push(s));
  assert.deepEqual(await api.setPolling({ paused: true }), { paused: true, intervalSecs: 2, intervals: [1, 2, 3, 5, 10, 15, 30, 60] });
  await wait(100);
  const before = figures.length;
  await wait(2300);
  assert.equal(figures.length, before, 'paused: no figures arrive');
  assert.equal(states[states.length - 1].live, false);
  const fast = await api.setPolling({ paused: false, intervalSecs: 1 });
  assert.equal(fast.intervalSecs, 1);
  await wait(2300);
  api.stop();
  assert.ok(figures.length - before >= 2, 'every second again');
  assert.equal(states[states.length - 1].polling.intervalSecs, 1);
  await assert.rejects(api.setPolling({ intervalSecs: 4 }), (err) => err.status === 422 && /must be one of/.test(err.message));
});

test('live figures arrive as events: no rate on the first poll, rates after', async () => {
  const { api } = mockApi();
  const figures = [];
  api.on('figures', (f) => figures.push(f));
  await api.setPolling({ intervalSecs: 1 });
  await connected(api, { name: 'staging-cache' });
  await wait(2300);
  api.stop();
  assert.ok(figures.length >= 2);
  assert.equal(figures[0].opsPerSec, null, 'a rate needs two polls');
  assert.equal(typeof figures[0].opsNow, 'number');
  assert.equal(typeof figures[1].opsPerSec, 'number');
  assert.equal(figures[1].maxMemory > 0, true);
});

test('a cluster has nodes, shard rows and a picked node that can be changed', async () => {
  const { api } = mockApi();
  const states = await connected(api, { name: 'demo-cluster' });
  const conn = states[states.length - 1].conn;
  assert.equal(conn.topology, 'cluster');
  assert.deepEqual(conn.nodes.map((n) => [n.id, n.address, n.slots]), [['0', '127.0.0.1:7001', '0-5460'], ['1', '127.0.0.1:7002', '5461-10922'], ['2', '127.0.0.1:7003', '10923-16383']]);
  assert.equal(conn.pickedNode, '0');
  const picked = await api.pickNode(2);
  assert.equal(picked.conn.pickedNode, '2');
  const overview = await api.getOverview();
  assert.deepEqual(overview.shards.map((s) => [s.id, s.status]), [['0', 'ok'], ['1', 'ok'], ['2', 'ok']]);
  await assert.rejects(api.pickNode(9), (err) => err.status === 422 && err.message === 'there is no such node');
  const channels = await api.getChannels();
  assert.deepEqual(channels.channels.filter((c) => c.shard).map((c) => [c.name, c.node]), [['shard.orders', 1], ['shard.presence', 2]]);
  api.stop();
});

test('the slow log says why its settings are missing in every reading, not only the first', async () => {
  const { api } = mockApi();
  await connected(api, { name: 'staging-cache' });
  for (let i = 0; i < 2; i += 1) {
    const slow = await api.getSlowlog();
    assert.equal(slow.off, null);
    assert.equal(slow.settings, null);
    assert.equal(slow.settingsOff, 'the server refused CONFIG GET for this user');
    assert.ok(slow.entries.length > 0);
  }
  api.stop();
});

test('a limited user: panels are off with the reason, and nothing is invented', async () => {
  const { api } = mockApi();
  await connected(api, { name: 'limited-user' });
  const overview = await api.getOverview();
  assert.equal(overview.keys.dataBytes, null);
  assert.ok(overview.checks.filter((c) => c.status === 'denied').length >= 5);
  assert.equal((await api.getClients()).off, 'the server refused CLIENT LIST for this user');
  assert.equal((await api.getSlowlog()).off, 'the server refused SLOWLOG for this user');
  assert.equal((await api.getChannels()).off, 'the server refused PUBSUB CHANNELS for this user');
  assert.equal((await api.getCommandNames()).off, 'the server refused COMMAND LIST for this user');
  const ping = await api.runCommand('PING');
  assert.equal(ping.kind, 'error');
  assert.equal(ping.text, 'the server refused COMMAND INFO for this user');
  const result = await api.testConnection('limited-user');
  assert.equal(result.ok, false);
  // A refused subscribe and a refused MONITOR are told as events.
  api.setConfirmHandler(async (c) => ({ typed: c.expect }));
  const events = [];
  api.on('tail', (e) => events.push(`tail ${e.kind} ${e.error || ''}`));
  api.on('monitor', (e) => events.push(`monitor ${e.kind} ${e.stopped || ''}`));
  await api.subscribe({ name: 'x' });
  await api.startMonitor();
  await wait(500);
  api.stop();
  assert.ok(events.includes('tail closed the server refused SUBSCRIBE on that channel for this user'));
  assert.ok(events.includes('monitor closed MONITOR stopped: the server refused MONITOR for this user.'));
});

test('a write on a read-only connection is refused in fixed wording, and nothing asks', async () => {
  const { api } = mockApi();
  api.setConfirmHandler(async () => assert.fail('a read-only connection never asks about a write'));
  await connected(api, { name: 'prod-cache-01' });
  api.stop();
  const reply = await api.runCommand('SET a b');
  assert.equal(reply.kind, 'refused');
  assert.match(reply.text, /^Not sent: prod-cache-01 is read-only\./);
  await assert.rejects(api.publish({ channel: 'orders.created', message: 'x' }), (err) => err.status === 422 && /read-only/.test(err.message));
});

test('turning writes on for production asks for the name; a wrong name sends nothing', async () => {
  const { api } = mockApi();
  const asked = [];
  api.setConfirmHandler(async (c) => { asked.push(c); return { typed: 'prod-cache-1' }; });
  await assert.rejects(api.updateConnection('prod-cache-01', { writes: true }), /does not match/);
  assert.equal(asked[0].kind, 'type');
  assert.equal(asked[0].expect, 'prod-cache-01');
  assert.equal((await api.listConnections()).profiles[0].readOnly, true);

  api.setConfirmHandler(async (c) => ({ typed: c.expect }));
  const saved = await api.updateConnection('prod-cache-01', { writes: true });
  assert.equal(saved.profile.readOnly, false);
  assert.match(saved.message, /^Writes are on for prod-cache-01/);

  api.setConfirmHandler(async () => assert.fail('turning writes off never asks'));
  assert.equal((await api.updateConnection('prod-cache-01', { writes: false })).profile.readOnly, true);
});

test('tags are added and removed, and standalone is set', async () => {
  const { api } = mockApi();
  const done = await api.updateConnection('prod-cache-01', { addTags: ['team-a'], removeTags: ['eu'], standalone: true });
  assert.deepEqual(done.profile.tags, ['cache', 'team-a']);
  assert.equal(done.profile.standalone, true);
  await assert.rejects(api.updateConnection('nope', { writes: false }), (err) => err.status === 404);
});

test('a destructive command asks; Cancel sends nothing; a question can be answered once', async () => {
  const { api, backend } = mockApi();
  await connected(api, { name: 'staging-cache' });
  api.stop();
  api.setConfirmHandler(async () => null);
  await assert.rejects(api.runCommand('DEL user:10293:cart'), (err) => err.cancelled === true);
  let question = null;
  api.setConfirmHandler(async (c) => { question = c; return { typed: '' }; });
  const reply = await api.runCommand('DEL user:10293:cart');
  assert.equal(question.kind, 'ask');
  assert.equal(question.text, 'Send DEL user:10293:cart to staging-cache?');
  assert.equal(reply.replies[0].views.text, '(integer) 1');
  await assert.rejects(backend.answerConfirm(question, ''), (err) => err.status === 410);
});

test('the writes switch of the session: off at once, on after a question; a container for the session only', async () => {
  const { api } = mockApi();
  const states = [];
  await connected(api, { name: 'staging-cache' });
  api.on('state', (s) => states.push(s));
  api.setConfirmHandler(async () => assert.fail('turning writes off never asks'));
  const off = await api.setSessionWrites(false);
  assert.equal(off.profile.readOnly, true);
  assert.match(off.message, /^Writes are off for staging-cache/);
  await wait(50);
  assert.equal(states[states.length - 1].conn.readOnly, true, 'the state follows');
  assert.match((await api.runCommand('SET a b')).text, /read-only\. Turn writes on in Connections\.$/);
  let asked = null;
  api.setConfirmHandler(async (c) => { asked = c; return { typed: '' }; });
  assert.equal((await api.setSessionWrites(true)).profile.readOnly, false);
  assert.equal(asked.kind, 'ask');
  assert.equal((await api.listConnections()).profiles.find((p) => p.name === 'staging-cache').readOnly, false, 'a saved connection is saved');

  await connected(api, { docker: 'demo-valkey' });
  api.setConfirmHandler(async () => assert.fail('turning writes off never asks'));
  const container = await api.setSessionWrites(false);
  assert.match(container.message, /in this session only/);
  assert.equal(container.profile.readOnly, true);
  api.stop();
  await api.disconnect();
  await assert.rejects(api.setSessionWrites(true), (err) => err.status === 409);
});

test('the command line: views, a repeat, an error reply, refusals and masking', async () => {
  const { api } = mockApi();
  await connected(api, { name: 'staging-cache' });
  api.stop();
  const hash = await api.runCommand('HGETALL user:10293:cart');
  assert.match(hash.replies[0].views.text, /^1\) "sku:44812"/);
  assert.match(hash.replies[0].views.table, /^# {2}field/);
  assert.equal(JSON.parse(hash.replies[0].views.json).coupon, 'WINTER10');
  assert.match(hash.replies[0].views.raw, /^\[array\] \*6/);
  const repeat = await api.runCommand('3 GET rl:203.0.113.7:login');
  assert.equal(repeat.times, 3);
  assert.equal(repeat.replies.length, 3);
  assert.equal((await api.runCommand('GET')).replies[0].error, true);
  assert.equal((await api.runCommand('SUBSCRIBE x')).kind, 'refused');
  const auth = await api.runCommand('AUTH alice hunter2');
  assert.equal(auth.input, 'AUTH *** ***');
  assert.equal(JSON.stringify(auth).includes('hunter2'), false);
  assert.match((await api.runCommand('NOPE')).text, /has no command NOPE/);
});

test('command names for completion, and help by name', async () => {
  const { api } = mockApi();
  await connected(api, { name: 'staging-cache' });
  api.stop();
  const names = await api.getCommandNames();
  assert.ok(names.names.includes('config|get'));
  assert.equal((await api.getCommandHelp('get')).arguments, 'key');
  assert.equal((await api.getCommandHelp('config')).subcommands.length, 2);
  assert.equal((await api.getCommandHelp('config|get')).name, 'config|get');
  assert.equal(await api.getCommandHelp('nosuch'), null);
});

test('the tail: ready, then messages; a published message arrives; stop closes it', async () => {
  const { api } = mockApi();
  const states = await connected(api, { name: 'staging-cache' });
  assert.equal(states[states.length - 1].conn.readOnly, false);
  const events = [];
  api.on('tail', (e) => events.push(e));
  const sub = await api.subscribe({ name: 'orders.*' });
  assert.equal(sub.kind, 'pattern');
  await wait(400);
  const sent = await api.publish({ channel: 'orders.created', message: 'hello' });
  assert.equal(typeof sent.receivers, 'number');
  await wait(1200);
  assert.deepEqual(await api.unsubscribe(), { stopped: true });
  await wait(100);
  api.stop();
  assert.equal(events[0].kind, 'ready');
  assert.equal(events[0].id, sub.id);
  const bodies = events.filter((e) => e.kind === 'messages').flatMap((e) => e.messages.map((m) => m.body));
  assert.ok(bodies.includes('hello'));
  assert.equal(events[events.length - 1].kind, 'closed');
  assert.equal(events[events.length - 1].error, null);
  await assert.rejects(api.subscribe({ name: 'a*', kind: 'shard' }), (err) => err.status === 422);
});

test('Monitor always asks first: yes or no, or the name typed on production; Cancel starts nothing', async () => {
  const { api } = mockApi();
  await connected(api, { name: 'prod-cache-01' });
  const events = [];
  api.on('monitor', (e) => events.push(e));
  api.setConfirmHandler(async () => null);
  await assert.rejects(api.startMonitor(), (err) => err.cancelled === true);
  let asked = null;
  api.setConfirmHandler(async (c) => { asked = c; return { typed: 'prod-cache-1' }; });
  await assert.rejects(api.startMonitor(), (err) => err.status === 422 && err.message === 'MONITOR not started: the name typed did not match.');
  assert.equal(asked.kind, 'type', 'a read-only production connection is asked too, by name');
  assert.equal(asked.expect, 'prod-cache-01');
  assert.match(asked.text, /It stops after 30 s or 10,000 lines\./);
  await wait(400);
  assert.deepEqual(events, [], 'nothing started');
  api.setConfirmHandler(async (c) => ({ typed: c.expect }));
  const run = await api.startMonitor();
  assert.equal(typeof run.run, 'number');
  await assert.rejects(api.startMonitor(), (err) => err.status === 422 && /already running/.test(err.message));
  await api.stopMonitor();
  api.stop();
});

test('Monitor: ready with its limits, lines, then stopped', async () => {
  const { api } = mockApi();
  await connected(api, { name: 'staging-cache' });
  const events = [];
  api.on('monitor', (e) => events.push(e));
  let asked = null;
  api.setConfirmHandler(async (c) => { asked = c; return { typed: '' }; });
  const run = await api.startMonitor();
  assert.equal(asked.kind, 'ask');
  assert.deepEqual([run.maxSecs, run.maxLines], [30, 10000]);
  await wait(1500);
  assert.deepEqual(await api.stopMonitor(), { stopped: true });
  await wait(100);
  api.stop();
  assert.equal(events[0].kind, 'ready');
  assert.equal(events[0].run, run.run);
  assert.ok(events.some((e) => e.kind === 'lines' && e.lines.length === 3));
  assert.equal(events[events.length - 1].stopped, 'MONITOR stopped.');
});

test('adding a connection keeps no password: only where it lives is said', async () => {
  const { api } = mockApi();
  const form = {
    name: 'cache-02', url: 'rediss://app@cache-02.prod.internal:6380/0', env: 'PROD', password: 'hunter2-s3cret', tags: ['cache'], writes: null,
    sentinelAddresses: ['s1:26379', 's2:26379'], sentinelMaster: 'mymaster', sentinelUser: 'sen', sentinelPassword: 'sentinel-s3cret', sentinelTls: true,
    sshHost: 'ops@bastion:2222', preferReplica: true, standalone: true,
  };
  const added = await api.addConnection(form);
  assert.equal(added.name, 'cache-02');
  assert.match(added.message, /^Saved cache-02 \(cache-02\.prod\.internal:6380, PROD\)\./);
  const everything = JSON.stringify([added, await api.listConnections(), await api.getState(), await api.testConnection('cache-02')]);
  assert.equal(everything.includes('hunter2-s3cret'), false);
  assert.equal(everything.includes('sentinel-s3cret'), false);
  assert.equal(everything.includes('ops@'), false, 'the SSH user is a credential too');
  const profile = (await api.listConnections()).profiles.find((p) => p.name === 'cache-02');
  assert.equal(profile.address, 'cache-02.prod.internal:6380', 'no user in the address shown');
  assert.equal(profile.readOnly, true, 'production defaults to read-only');
  assert.equal(profile.tls, true);
  assert.equal(profile.ssh, 'bastion:2222');
  assert.equal(profile.sentinel, 'mymaster via s1:26379 (+1)');
  assert.equal(profile.sentinelTls, true);
  assert.equal(profile.preferReplica, true);
  assert.equal(profile.standalone, true);
  assert.match(profile.signIn, /keychain/);
  await assert.rejects(api.addConnection(form), /already exists/);
  assert.equal((await api.addConnection({ ...form, force: true })).name, 'cache-02', 'replaced when asked to');
});

test('a token command is asked about before it is saved; writes on production need the name typed', async () => {
  const { api } = mockApi();
  const form = { name: 'tok', url: 'redis://h:6379', env: 'PROD', tags: [], writes: true, tokenCommand: ['get-token', '--scope', 'redis'] };
  api.setConfirmHandler(async () => null);
  await assert.rejects(api.addConnection(form), (err) => err.cancelled === true);
  assert.equal((await api.listConnections()).profiles.some((p) => p.name === 'tok'), false);
  const asked = [];
  api.setConfirmHandler(async (c) => { asked.push(c); return { typed: c.expect }; });
  await api.addConnection(form);
  assert.deepEqual(asked.map((c) => c.kind), ['ask', 'type'], 'first the command, then the name');
  assert.match(asked[0].text, /get-token --scope redis/);
  assert.equal(asked[1].expect, 'tok');
  const saved = (await api.listConnections()).profiles.find((p) => p.name === 'tok');
  assert.equal(saved.tokenCommand, true);
  assert.equal(saved.readOnly, false);
  assert.match(saved.signIn, /token from a command/);
});

test('Entra ID is offered only by a build that has it', async () => {
  assert.equal((await mockApi().api.getState()).features.entraId, false);
  const { api } = mockApi({ entra: true });
  assert.equal((await api.getState()).features.entraId, true);
  await api.addConnection({ name: 'az', url: 'rediss://x.redis.azure.net:10000', tags: [], entra: true });
  assert.match((await api.listConnections()).profiles.find((p) => p.name === 'az').signIn, /Entra ID/);
  await assert.rejects(mockApi().api.addConnection({ name: 'az', url: 'rediss://x:1', tags: [], entra: true }), /Entra ID/);
});

test('the overview event: the findings arrive after the live connection is ready, and with another node', async () => {
  const { api } = mockApi();
  const events = [];
  api.on('overview', (o) => events.push(o));
  await connected(api, { name: 'prod-cache-01' });
  for (let i = 0; i < 20 && !events.length; i += 1) await wait(50);
  assert.equal(events.length, 1);
  assert.deepEqual(events[0].findings.map((f) => f.where), ['server/slowlog', 'server/clients']);
  assert.equal(events[0].figures, null, 'no live figures in the event: they come with figures');
  assert.deepEqual(events[0].shards, [], 'no shard rows in the event');
  await api.connect({ name: 'demo-cluster' });
  for (let i = 0; i < 40 && events.length < 2; i += 1) await wait(50);
  await api.pickNode(1);
  for (let i = 0; i < 20 && events.length < 3; i += 1) await wait(50);
  assert.ok(events.length >= 3, 'picking a node sends it again');
  api.stop();
});

test('the command line: the 4 MiB notice, and a line too long', async () => {
  const { api } = mockApi();
  await connected(api, { name: 'demo-cluster' });
  api.stop();
  const big = await api.runCommand('MOCK DROPPED');
  assert.equal(big.dropped, 3);
  assert.equal(big.droppedNote, '(the first 3 replies were dropped: the output keeps at most 4 MiB)');
  assert.equal((await api.runCommand('PING')).droppedNote, null);
  await assert.rejects(api.runCommand(`GET ${'k'.repeat(8200)}`), (err) => err.status === 413 && err.message === 'Not sent: a line holds at most 8,192 characters.');
});

test('like the real server, the mock refuses a body with a field it does not know', async () => {
  const { server } = mockApi();
  const headers = { Authorization: `Bearer ${server.token}` };
  const post = async (path, body) => (await server.fetchFn(path, { method: 'POST', headers, body: JSON.stringify(body) })).status;
  assert.equal(await post('/api/connections', { name: 'a', url: 'redis://h', tls: true }), 400);
  assert.equal(await post('/api/connections', { name: 'a', url: 'redis://h', token_command_confirmed: true }), 400);
  assert.equal(await post('/api/connect', { name: 'prod-cache-01', extra: 1 }), 400);
  assert.equal(await post('/api/connect', { name: 'prod-cache-01', docker: 'demo-redis' }), 400);
  assert.equal(await post('/api/polling', { live: false }), 400);
  assert.equal(await post('/api/session/writes', { writes: true }), 400);
  assert.equal((await server.fetchFn('/api/connections/prod-cache-01', { method: 'PATCH', headers, body: JSON.stringify({ env: 'stage' }) })).status, 400);
});

test('removing a connection; a Docker container connects by its name', async () => {
  const { api } = mockApi();
  assert.deepEqual(await api.removeConnection('staging-cache'), { message: 'Removed staging-cache.' });
  assert.equal((await api.listConnections()).profiles.some((p) => p.name === 'staging-cache'), false);
  await assert.rejects(api.removeConnection('staging-cache'), (err) => err.status === 404 && err.message === 'there is no saved connection by that name');
  // Removing the connection the session is on closes it: no disconnect is needed.
  const during = await connected(api, { name: 'prod-cache-01' });
  assert.equal(during[during.length - 1].connected, true);
  const after = [];
  api.on('state', (s) => after.push(s));
  await api.removeConnection('prod-cache-01');
  await wait(50);
  assert.equal(after[after.length - 1].status, 'idle');
  assert.equal((await api.getState()).conn, null);
  const states = await connected(api, { docker: 'demo-valkey' });
  api.stop();
  const conn = states[states.length - 1].conn;
  assert.equal(conn.docker, true);
  assert.equal(conn.server, 'Valkey');
  assert.equal((await api.listConnections()).connected, null, 'a container is not a saved connection');
});

test('the address shown never holds a user or a password', () => {
  assert.equal(maskedAddress('rediss://app:s3cret@cache-02.prod.internal:6380/0'), 'cache-02.prod.internal:6380');
  assert.equal(maskedAddress('redis://h'), 'h:6379');
  assert.equal(maskedAddress('h:6380,password=s3cret,ssl=True'), 'h:6380');
});

test('the sample messages are the mockup\'s', () => {
  assert.equal(sampleMessage('orders.created', 0), '{"order":"A-20417","items":1,"total":19.99}');
  assert.equal(sampleMessage('cache.invalidate', 3), 'cache:product:44815');
});
