// The sample-data implementation behind `?mock`, driven through the same api wrapper the
// page uses: the views' shapes, the write gate's questions, and that a password given to
// the add form is not kept anywhere.
import test from 'node:test';
import assert from 'node:assert/strict';
import { ApiError, ConfirmRequired, METHODS, createApi } from '../api.js';
import { createMock, maskedAddress, sampleMessage, sampleSeries } from '../mock.js';

function mockApi() {
  const backend = createMock({ ApiError, ConfirmRequired });
  const api = createApi(backend);
  return { api, backend };
}

test('the mock has every method the real implementation has', () => {
  const { backend } = mockApi();
  for (const name of METHODS.concat(['answerConfirm', 'cancelConfirm', 'open', 'close', 'reopen'])) assert.equal(typeof backend[name], 'function', name);
  assert.equal(backend.mock, true);
});

test('before a connect: nothing connected, the saved connections and the Docker containers', async () => {
  const { api } = mockApi();
  const state = await api.getState();
  assert.equal(state.connected, false);
  assert.equal(state.conn, null);
  const list = await api.listConnections();
  assert.deepEqual(list.profiles.map((p) => p.name), ['prod-cache-01', 'staging-cache', 'demo-cluster']);
  assert.deepEqual(list.docker.map((p) => p.name), ['docker:demo-redis', 'docker:demo-valkey']);
  assert.equal(list.profiles[0].readOnly, true, 'production is read-only by default');
  assert.ok(list.profiles.every((p) => p.status === 'not-open'));
  await assert.rejects(api.getOverview(), /Connect first/);
});

test('connect goes through connecting to connected and reaches state subscribers', async () => {
  const { api } = mockApi();
  const seen = [];
  api.on('state', (s) => seen.push(s.connecting ? 'connecting' : s.connected ? 'connected' : 'idle'));
  api.start();
  const state = await api.connect('prod-cache-01');
  api.stop();
  assert.equal(state.connected, true);
  assert.equal(state.conn.name, 'prod-cache-01');
  assert.equal(state.conn.readOnly, true);
  assert.deepEqual(seen, ['connecting', 'connected']);
  const overview = await api.getOverview();
  assert.equal(overview.keys.total, 7920114);
  assert.equal(overview.history.ops.length, 40);
  assert.equal(overview.shards.length, 0, 'no shard rows on a standalone server');
  const list = await api.listConnections();
  assert.equal(list.profiles[0].status, 'connected');
});

test('a cluster has shard rows and a picked node that can be changed', async () => {
  const { api } = mockApi();
  const state = await api.connect('demo-cluster');
  assert.equal(state.conn.nodes.length, 3);
  assert.equal(state.conn.pickedNode, 'n1');
  const picked = await api.pickNode('n3');
  assert.equal(picked.conn.pickedNode, 'n3');
  const overview = await api.getOverview();
  assert.deepEqual(overview.shards.map((s) => s.picked), [false, false, true]);
});

test('a refused check leaves its figures unknown instead of inventing them', async () => {
  const { api } = mockApi();
  await api.connect('staging-cache');
  const overview = await api.getOverview();
  assert.equal(overview.keys.dataBytes, null);
  assert.equal(overview.keys.overheadBytes, null);
  assert.ok(overview.notes.some((n) => n.includes('MEMORY STATS')));
  const result = await api.testConnection('staging-cache');
  assert.equal(result.ok, false);
  assert.ok(result.checks.some((c) => c.name === 'MEMORY STATS' && !c.ok && c.why));
});

test('a write on a read-only connection is refused in fixed wording, and nothing asks', async () => {
  const { api } = mockApi();
  api.setConfirmHandler(async () => assert.fail('a read-only connection never asks'));
  await api.connect('prod-cache-01');
  const reply = await api.runCommand('SET a b');
  assert.equal(reply.kind, 'refused');
  assert.equal(reply.views.text, 'Not sent: prod-cache-01 is read-only. Turn writes on in Connections.');
  await assert.rejects(api.publish({ channel: 'orders.created', message: 'x' }), /read-only/);
});

test('turning writes on for production asks for the name; a wrong name sends nothing', async () => {
  const { api } = mockApi();
  const asked = [];
  api.setConfirmHandler(async (c) => { asked.push(c); return { typed: 'prod-cache-1' }; });
  await assert.rejects(api.updateConnection('prod-cache-01', { writes: true }), /did not match/);
  assert.equal(asked[0].kind, 'type');
  assert.equal(asked[0].expect, 'prod-cache-01');
  assert.equal((await api.listConnections()).profiles[0].readOnly, true);

  api.setConfirmHandler(async (c) => ({ typed: c.expect }));
  const saved = await api.updateConnection('prod-cache-01', { writes: true });
  assert.equal(saved.readOnly, false);

  api.setConfirmHandler(async () => assert.fail('turning writes off never asks'));
  assert.equal((await api.updateConnection('prod-cache-01', { writes: false })).readOnly, true);
});

test('a destructive command asks; Cancel sends nothing; a question can be answered once', async () => {
  const { api, backend } = mockApi();
  await api.connect('staging-cache');
  api.setConfirmHandler(async () => null);
  await assert.rejects(api.runCommand('DEL user:10293:cart'), (err) => err.cancelled === true);
  let id = null;
  api.setConfirmHandler(async (c) => { id = c.id; assert.equal(c.kind, 'ask'); return { typed: '' }; });
  const reply = await api.runCommand('DEL user:10293:cart');
  assert.equal(reply.views.text, '(integer) 1');
  await assert.rejects(backend.answerConfirm(id, ''), /expired/);
});

test('MONITOR always asks first, read-only connections too', async () => {
  const { api } = mockApi();
  await api.connect('prod-cache-01');
  let asked = null;
  api.setConfirmHandler(async (c) => { asked = c; return { typed: c.expect }; });
  const run = await api.startMonitor();
  assert.equal(asked.kind, 'type', 'production types the name');
  assert.equal(run.running, true);
  await api.stopMonitor();
});

test('subscribing: channel, pattern, and a pattern cannot be shard-subscribed', async () => {
  const { api } = mockApi();
  await api.connect('demo-cluster');
  assert.deepEqual(await api.subscribe({ target: 'orders.created', shard: false }), { name: 'orders.created', kind: 'channel' });
  assert.deepEqual(await api.subscribe({ target: 'orders.*', shard: false }), { name: 'orders.*', kind: 'pattern' });
  assert.deepEqual(await api.subscribe({ target: 'shard.orders', shard: true }), { name: 'shard.orders', kind: 'shard' });
  await assert.rejects(api.subscribe({ target: 'orders.*', shard: true }), /pattern/);
  const channels = await api.getChannels();
  assert.ok(channels.channels.some((c) => c.shard));
  await api.unsubscribe();
});

test('adding a connection keeps no password: only where it lives is said', async () => {
  const { api } = mockApi();
  const form = { name: 'cache-02', url: 'rediss://app@cache-02.prod.internal:6380/0', env: 'PROD', password: 'hunter2-s3cret', tags: ['cache'], writes: null, tls: true, sentinelAddresses: ['s1:26379'], sentinelMaster: 'mymaster', sentinelPassword: 'sentinel-s3cret' };
  const added = await api.addConnection(form);
  assert.deepEqual(added, { name: 'cache-02' });
  const everything = JSON.stringify([await api.listConnections(), await api.getState(), await api.testConnection('cache-02')]);
  assert.equal(everything.includes('hunter2-s3cret'), false);
  assert.equal(everything.includes('sentinel-s3cret'), false);
  const profile = (await api.listConnections()).profiles.find((p) => p.name === 'cache-02');
  assert.equal(profile.address, 'cache-02.prod.internal:6380', 'no user in the address shown');
  assert.equal(profile.readOnly, true, 'production defaults to read-only');
  assert.match(profile.signIn, /keychain/);
  await assert.rejects(api.addConnection(form), /already exists/);
});

test('a token command is saved only once it was confirmed', async () => {
  const { api } = mockApi();
  const form = { name: 'tok', url: 'redis://h:6379', env: '', tags: [], tokenCommand: ['aws', 'token'] };
  await assert.rejects(api.addConnection({ ...form, tokenCommandConfirmed: false }), /not confirmed/);
  await api.addConnection({ ...form, tokenCommandConfirmed: true });
  assert.match((await api.listConnections()).profiles.find((p) => p.name === 'tok').signIn, /token from a command/);
});

test('removing the connected connection disconnects', async () => {
  const { api } = mockApi();
  await api.connect('staging-cache');
  await api.removeConnection('staging-cache');
  assert.equal((await api.getState()).connected, false);
  assert.equal((await api.listConnections()).profiles.some((p) => p.name === 'staging-cache'), false);
  await assert.rejects(api.removeConnection('docker:demo-redis'), /no saved connection/);
});

test('a lost connection is said in state, and a new connect clears it', async () => {
  const { api } = mockApi();
  const states = [];
  api.on('state', (s) => states.push(s));
  api.start();
  await api.connect('staging-cache');
  await api.runCommand('MOCK LOST');
  await new Promise((resolve) => setTimeout(resolve, 300));
  const last = states[states.length - 1];
  assert.equal(last.connected, false);
  assert.equal(last.lost, 'The connection to the server was lost.');
  const again = await api.connect('staging-cache');
  api.stop();
  assert.equal(again.connected, true);
  assert.equal(again.lost, null);
});

test('the address shown never holds a user or a password', () => {
  assert.equal(maskedAddress('rediss://app:s3cret@cache-02.prod.internal:6380/0'), 'cache-02.prod.internal:6380');
  assert.equal(maskedAddress('redis://h'), 'h');
  assert.equal(maskedAddress('h:6380,password=s3cret,ssl=True'), 'h:6380');
});

test('the sample series and messages are the mockup’s, and repeatable', () => {
  assert.deepEqual(sampleSeries(40, 12.4, 1.6, 7), sampleSeries(40, 12.4, 1.6, 7));
  assert.equal(sampleSeries(40, 214, 9, 3).length, 40);
  assert.equal(sampleMessage('orders.created', 0), '{"order":"A-20417","items":1,"total":19.99}');
  assert.equal(sampleMessage('cache.invalidate', 3), 'cache:product:44815');
});
