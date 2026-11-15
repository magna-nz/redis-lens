// The sample data's Browse, Analyze, snapshot and database routes, driven through the same
// `createReal` and adapters the page uses: the tree from names, key lists and "Load more",
// Find, every value kind, an analysis start to end (and stopped, and failed), the panels
// with their `off` reasons, snapshots, and a database switch.
import test from 'node:test';
import assert from 'node:assert/strict';
import { createApi, createReal } from '../api.js';
import { createMockServer, hexOf, globRe, nameOf, ttlText } from '../mock.js';
import { mergeList } from '../views/browse.js';

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function mockApi() {
  const server = createMockServer();
  const backend = createReal({ ...server, mock: true });
  return { api: createApi(backend), backend, server };
}

/** Connect, wait for the live connection, and keep the events coming until `stop` (or the
 *  test ends: a failed assertion must not leave the event stream open). */
async function session(t, name) {
  const m = mockApi();
  t.after(() => m.api.stop());
  const states = [];
  const progress = [];
  m.api.on('state', (s) => states.push(s));
  m.api.on('progress', (p) => progress.push(p));
  m.api.start();
  await m.api.connect(name.startsWith('docker:') ? { docker: name.slice(7) } : { name });
  for (let i = 0; i < 40 && !(states.length && states[states.length - 1].liveState === 'ready' && states[states.length - 1].connected); i += 1) await wait(50);
  return { ...m, states, progress };
}

async function until(fn, tries = 80) {
  for (let i = 0; i < tries; i += 1) {
    const v = await fn();
    if (v) return v;
    await wait(100);
  }
  throw new Error('waited too long');
}

/** A request as the page never sends it: raw, to check the refusals. */
async function raw(server, method, path, body) {
  const r = await server.fetchFn(path, { method, headers: { Authorization: `Bearer ${server.token}` }, body: body === undefined ? undefined : JSON.stringify(body) });
  return [r.status, await r.json()];
}

test('the helpers: hex ids, escaped names, TTL words and globs as Redis reads them', () => {
  assert.equal(hexOf('cart:{id}'), '636172743a7b69647d');
  assert.deepEqual(nameOf('evil\x1b[31mkey'), { copy: null, id: '6576696c1b5b33316d6b6579', name: 'evil\\x1b[31mkey' });
  assert.deepEqual([ttlText(null), ttlText(45), ttlText(3599), ttlText(7200), ttlText(554400)], ['never', '45s', '59m 59s', '2h 0m', '6d 10h']);
  assert.ok(globRe('user:*:cart').test('user:10293:cart'));
  assert.ok(!globRe('user:*:cart').test('user:10293:feed'));
  assert.ok(globRe('rl:203.0.?.*').test('rl:203.0.0.7:login'));
  assert.ok(globRe('tags:[0-9]*').test('tags:1000'));
  assert.ok(!globRe('tags:[^1]*').test('tags:1000'));
  assert.ok(globRe('a\\*b').test('a*b'));
  assert.ok(!globRe('a.b').test('axb'), 'a dot is a dot');
});

test('before an analysis the tree comes from key names, read a page at a time', async (t) => {
  const { api, server } = await session(t, 'docker:demo-redis');
  const first = await api.getBrowse();
  assert.equal(first.source, 'names');
  assert.equal(first.names.reading, true, 'the first look starts reading names');
  const read = await until(async () => {
    const x = await api.getBrowse();
    return x.names.done ? x : null;
  });
  api.stop();
  assert.equal(read.names.complete, false, 'a big keyspace: the first pages only');
  assert.equal(read.names.namesRead, 8000);
  assert.equal(read.sizes, false);
  const group = read.rows.find((r) => r.kind === 'group');
  assert.equal(group.label, 'user:');
  assert.deepEqual(group.children.map((c) => c.pattern.name).sort(), ['user:{id}:cart', 'user:{id}:feed']);
  assert.ok(read.rows.some((r) => r.kind === 'pattern' && r.pattern.name === 'evil\\x1b[31mkey' && r.pattern.copy === null && r.literal));
  const more = await api.moreNames({});
  assert.equal(more.names.namesRead, 9000, 'Read 1,000 more names');
  const byName = await api.getBrowse({ sort: 'name' });
  assert.equal(byName.sort, 'name');
  const [status, body] = await raw(server, 'GET', '/api/browse?sort=size');
  assert.deepEqual([status, body.error], [400, 'the sort must be one of memory, keys, avg_size, no_ttl or name']);
});

test('a pattern\'s keys a page at a time: "Load more" sends only the new rows, from new_from', async (t) => {
  const { api, server } = await session(t, 'docker:demo-redis');
  api.stop();
  const cart = hexOf('user:{id}:cart');
  const first = await api.getKeys({ pattern: cart });
  assert.equal(first.loaded, 1000);
  assert.equal(first.rows.length, 1000);
  assert.equal(first.done, false);
  assert.equal(first.rows[0].name, 'user:10293:cart');
  assert.equal(first.rows[0].type, 'hash');
  const next = await api.moreKeys(first.list);
  assert.equal(next.newFrom, 1000);
  assert.equal(next.rows.length, 1000);
  const merged = mergeList(first, next);
  assert.equal(merged.rows.length, 2000);
  assert.equal(new Set(merged.rows.map((r) => r.id)).size, 2000, 'every key once');
  let l = merged;
  while (!l.done) l = mergeList(l, await api.moreKeys(l.list));
  assert.equal(l.rows.length, l.loaded);
  await assert.rejects(api.moreKeys(l.list), (err) => err.status === 409 && err.message === 'Every key of this list is loaded.');
  await assert.rejects(api.moreKeys(999), (err) => err.status === 404);
  assert.equal((await raw(server, 'GET', '/api/keys'))[0], 400, 'no pattern');
  assert.equal((await raw(server, 'GET', '/api/keys?pattern=xyz'))[0], 400, 'not hex');
  assert.equal((await raw(server, 'GET', '/api/keys/more?list=abc'))[0], 400);
  assert.equal((await raw(server, 'GET', `/api/keys?pattern=${hexOf('nope:*')}`))[0], 404);
});

test('Find: text finds names that contain it, a pattern is used as typed, and the refusals are the server\'s', async (t) => {
  const { api, server } = await session(t, 'docker:demo-redis');
  api.stop();
  const evil = await api.findKeys('evil');
  assert.deepEqual(evil.find, { text: 'evil', glob: '*evil*' });
  assert.equal(evil.rows.length, 1);
  assert.equal(evil.rows[0].name, 'evil\\x1b[31mkey', 'shown escaped');
  assert.equal(evil.rows[0].copy, null);
  const v = await api.openValue({ key: evil.rows[0].id });
  assert.equal(v.key.name, 'evil\\x1b[31mkey', 'asked for by its hex id, the value opens');
  assert.equal(v.value.lines[0], 'v');
  const glob = await api.findKeys('user:1029?:cart');
  assert.deepEqual(glob.rows.map((r) => r.name), ['user:10293:cart']);
  await assert.rejects(api.findKeys('*a*b*c*d*e*f*g*h*i*'), (err) => err.status === 422 && /at most 8 \* wildcards/.test(err.message));
  await assert.rejects(api.findKeys('   '), (err) => err.status === 422 && err.message === 'Type a name or a pattern to find.');
  assert.equal((await raw(server, 'GET', '/api/find'))[0], 400);
  const none = await api.findKeys('no-such-key');
  assert.equal(none.loaded, 0);
  assert.equal(none.done, true);
});

test('every value kind: items, renderings, paging, a module and a key that is gone', async (t) => {
  const { api, server } = await session(t, 'prod-cache-01');
  api.stop();
  const open = (name) => api.openValue({ key: hexOf(name) });
  const hash = await open('user:10293:cart');
  assert.equal(hash.kind, 'hash');
  assert.equal(hash.fieldTtls, true, 'Redis 7.4 has field TTLs');
  assert.ok(hash.items.length >= 3);
  const list = await open('queue:emails');
  assert.deepEqual([list.kind, list.length, list.loaded, list.more], ['list', 412000, 500, true]);
  const more = await api.moreValue(list.view);
  assert.equal(more.loaded, 1000);
  assert.equal(more.items.length, 1000, 'the reply holds every item loaded');
  const stream = await open('events:orders');
  assert.equal(stream.kind, 'stream');
  await assert.rejects(api.getItem({ view: stream.view, index: 0 }), (err) => err.status === 422 && /open one field/.test(err.message));
  const field = await api.getItem({ view: stream.view, index: 0, field: 1 });
  assert.equal(field.name, 'order');
  const zset = await open('user:88120:feed');
  assert.equal(zset.kind, 'zset');
  assert.match(zset.items[0].value, /^\d+$/);
  const set = await open('tags:1000');
  assert.deepEqual(set.items.map((i) => i.value), ['red', 'blue', 'sale', 'new', 'eu']);
  const json = await open('cfg:flags');
  assert.equal(json.value.detected, 'JSON');
  assert.ok(json.value.lines.length > 3, 'drawn as indented JSON');
  assert.ok(json.value.formats.includes('hex'));
  const blob = await open('tmp:import:20260901');
  assert.equal(blob.value.detected, 'hex');
  assert.match(blob.value.lines[0], /^00000000 {2}1f 8b 08 00/);
  const asText = await api.getItem({ view: blob.view, index: 0, format: 'text' });
  assert.equal(asText.shown.copy, null, 'control characters: not copied as text');
  assert.equal(asText.shown.copyRefused, 'The value has control characters, so it is not copied as text: show it as hex to copy it.');
  const asHex = await api.getItem({ view: blob.view, index: 0, format: 'hex' });
  assert.match(asHex.shown.copy, /^1f8b0800/);
  await assert.rejects(api.getItem({ view: blob.view, index: 0, format: 'yaml' }), (err) => err.status === 400);
  const bloom = await open('bf:signups');
  assert.equal(bloom.kind, 'module');
  assert.equal(bloom.module.info[0].name, 'Capacity');
  const gone = await open('user:1:cart');
  assert.equal(gone.gone, true);
  assert.equal((await raw(server, 'GET', '/api/value?key=zz'))[0], 400);
  await assert.rejects(api.moreValue(9999), (err) => err.status === 404);
});

test('an analysis: the estimate first, live progress, the panels, a snapshot saved and compared', async (t) => {
  const { api, states, progress } = await session(t, 'prod-cache-01');
  const info = await api.getAnalysis();
  assert.deepEqual(info.estimates.map((e) => [e.depth, e.sampleLimit, e.keys]), [['quick', 10000, 10000], ['deep', 100000, 100000]]);
  assert.equal(info.estimates[0].about, 'at most 4,000 commands a second', 'production keeps under its budget');
  await assert.rejects(api.getBigKeys(), (err) => err.status === 409 && err.message === 'No analysis yet: analyze the keys first.');
  const started = await api.startAnalysis('quick');
  assert.equal(started.depth, 'quick');
  await assert.rejects(api.startAnalysis('quick'), (err) => err.status === 409 && err.message === 'An analysis is already running.');
  await until(() => progress.some((p) => p.kind === 'compared'));
  const kinds = Array.from(new Set(progress.map((p) => p.kind)));
  assert.deepEqual(kinds, ['started', 'sampling', 'done', 'compared']);
  const statuses = Array.from(new Set(states.map((s) => s.analysis.status)));
  assert.deepEqual(statuses, ['none', 'connecting', 'sampling', 'done']);
  assert.ok(states.some((s) => s.status === 'connecting' && s.analysis.status === 'connecting'), 'the analysis opens its connection again, as on the real server');
  const sampling = progress.filter((p) => p.kind === 'sampling');
  assert.ok(sampling.every((p, i) => i === 0 || p.sampled >= sampling[i - 1].sampled), 'progress only goes forward');
  const big = await api.getBigKeys();
  assert.equal(big.rows[0].key.name, 'queue:emails');
  assert.equal(big.rows[0].big, true);
  assert.ok(big.recommendations.some((r) => r.severity === 'warn'));
  const expiry = await api.getExpiry();
  assert.equal(expiry.rows[0].warning, true, 'the worrying pattern first');
  assert.equal(expiry.freed.within.length, 4);
  const hot = await api.getHotKeys();
  assert.equal(hot.off, null);
  assert.ok(hot.rows[0].key.freq >= hot.rows[1].key.freq);
  assert.equal((await api.getNodes()).off, 'Not a cluster: this analysis read one standalone server.');
  assert.equal((await api.getSlots({})).off, 'Not a cluster: this is one standalone server.');
  const changes = await api.getChanges();
  assert.equal(changes.status, 'diff', 'this profile has snapshots from the last two days');
  assert.ok(changes.trendPoints.length >= 2);
  const tmp = changes.rows.find((r) => r.pattern.name === 'tmp:import:{id}');
  assert.equal(tmp.change.alarming, true);
  assert.ok(tmp.trend.values.length >= 3);
  const evil = changes.rows.find((r) => r.pattern.name === 'evil\\x1b[31mkey');
  assert.equal(evil.change.cause, 'unchanged', 'the newest snapshot has it');
  const tree = await api.getBrowse();
  assert.equal(tree.source, 'analysis');
  assert.equal(tree.sizes, true);
  assert.equal(tree.sort, 'memory');
  const ov = await api.getOverview();
  assert.ok(ov.analysis.topPatterns.length > 0);
  assert.ok(ov.analysis.findings.some((f) => f.where === 'analyze/big_keys'));
  assert.equal(ov.analysis.changes, 'diff');
  const saved = await api.saveSnapshot();
  assert.match(saved.text, /^Saved snapshot \d{8}T\d{6}Z\.json$/);
  const list = await api.listSnapshots();
  assert.equal(list.snapshots[0].current, true, 'newest first, the one shown marked');
  assert.equal(list.saved, true);
  assert.equal(list.snapshots.length, 4);
  const oldest = list.snapshots[list.snapshots.length - 1].file;
  const cmp = await api.compareSnapshots(oldest, 'current');
  assert.equal(cmp.status, 'diff');
  assert.ok(cmp.rows.some((r) => r.change.cause === 'new'), 'the escape-sequence key is new since the oldest snapshot');
  await assert.rejects(api.compareSnapshots(oldest, oldest), (err) => err.status === 422);
  await assert.rejects(api.compareSnapshots('19990101T000000Z.json', 'current'), (err) => err.status === 404);
  api.stop();
});

test('stopping an analysis keeps what it sampled, partial and not saved', async (t) => {
  const { api, progress } = await session(t, 'prod-cache-01');
  await api.startAnalysis('deep');
  await until(() => progress.some((p) => p.kind === 'sampling' && p.sampled > 0));
  const r = await api.stopAnalysis();
  assert.deepEqual([r.stopping, r.text], [true, 'Stopping the analysis. What it sampled so far is kept.']);
  const stopped = await until(() => progress.find((p) => p.kind === 'stopped'));
  api.stop();
  assert.equal(stopped.kept, true);
  assert.match(stopped.text, /partial, so they are not saved/);
  const state = await api.getState();
  assert.equal(state.analysis.report.stopped, true);
  const list = await api.listSnapshots();
  assert.equal(list.canSave, false);
  assert.match(list.saveOff, /stopped early/);
  await assert.rejects(api.saveSnapshot(), (err) => err.status === 409);
  await assert.rejects(api.stopAnalysis(), (err) => err.status === 409 && err.message === 'No analysis is running.');
});

test('a failed analysis says why, and the panels stay empty', async (t) => {
  const { api, progress } = await session(t, 'staging-cache');
  await api.startAnalysis('quick');
  const failed = await until(() => progress.find((p) => p.kind === 'failed'));
  api.stop();
  assert.equal(failed.error, 'The analysis failed: the connection to the server broke.');
  const state = await api.getState();
  assert.equal(state.analysis.status, 'none');
  assert.equal(state.analysis.running, false);
  assert.equal(state.analysis.error, failed.error);
  assert.equal(state.analysis.report, null);
  await assert.rejects(api.getExpiry(), (err) => err.status === 409);
});

test('a cluster: no databases, Nodes and Slots with rows, hot keys off without an LFU policy', async (t) => {
  const { api, progress } = await session(t, 'demo-cluster');
  assert.equal((await api.getDbs()).off, 'A cluster only has database 0.');
  await assert.rejects(api.switchDb(1), (err) => err.status === 422);
  await api.startAnalysis('quick');
  await until(() => progress.some((p) => p.kind === 'compared'));
  api.stop();
  const nodes = await api.getNodes();
  assert.equal(nodes.rows.length, 3);
  assert.deepEqual(nodes.rows.map((r) => r.status), ['scanned', 'scanned', 'partial']);
  assert.match(nodes.rows[2].note, /partial/);
  const slots = await api.getSlots({ order: 'cpu' });
  assert.equal(slots.order, 'cpu');
  assert.ok(slots.hottest[0].cpuUsec >= slots.hottest[1].cpuUsec);
  assert.equal(slots.ranges.length, 3);
  assert.equal(slots.memoryColumn, false, 'Valkey has no memory column');
  const hot = await api.getHotKeys();
  assert.equal(hot.off, 'The maxmemory policy is allkeys-lru: without an LFU policy the server keeps no access counters, so there are no hot keys.');
  const big = await api.getBigKeys();
  assert.ok(big.rows.some((r) => r.endpoint === '127.0.0.1:7001'), 'a row names its node by endpoint');
  const tree = await api.getBrowse({ node: 1 });
  assert.equal(tree.node, 1);
  assert.equal(tree.nodes.length, 3);
  const narrowed = await api.getKeys({ pattern: hexOf('user:{id}:cart'), node: 2 });
  assert.ok(narrowed.rows.every((r) => r.node === 2));
  assert.equal(narrowed.cluster.narrowed, 2);
});

test('a user without MEMORY USAGE or OBJECT FREQ: those panels are off, with the reason', async (t) => {
  const { api, progress } = await session(t, 'limited-user');
  await api.startAnalysis('quick');
  await until(() => progress.some((p) => p.kind === 'compared'));
  api.stop();
  assert.equal((await api.getBigKeys()).off, 'MEMORY USAGE is refused for this user, so no key has a size to rank.');
  assert.equal((await api.getHotKeys()).off, 'OBJECT FREQ is refused for this user, so there are no hot keys.');
  const expiry = await api.getExpiry();
  assert.equal(expiry.freed, null);
  assert.equal(expiry.freedOff, 'MEMORY USAGE is refused for this user, so there are no memory figures.');
  const keys = await api.getKeys({ pattern: hexOf('session:{id}') });
  assert.equal(keys.rows[0].bytes, null, 'no size: unknown, not zero');
  assert.deepEqual(keys.refused, ['MEMORY USAGE is refused for this user, so the keys have no sizes.', 'OBJECT FREQ is refused for this user, so the keys have no LFU counters.']);
  assert.equal((await api.getDbs()).unread, 'CONFIG GET databases is refused for this user, so only the databases with keys are listed.');
});

test('a database switch is a new run: lists, values and a pending question go', async (t) => {
  const { api, backend, states } = await session(t, 'staging-cache');
  const dbs = await api.getDbs();
  assert.deepEqual(dbs.rows.map((r) => [r.db, r.current]), [[0, true], [1, false], [3, false]]);
  const l = await api.getKeys({ pattern: hexOf('session:{id}') });
  let switched = null;
  api.setConfirmHandler(async () => {
    switched = await backend.switchDb(1);
    return { typed: '' };
  });
  await assert.rejects(api.runCommand('DEL user:10293:cart'), (err) => err.status === 410, 'the question was dropped by the switch');
  assert.deepEqual([switched.switched, switched.db], [true, 1]);
  assert.equal(switched.text, 'Switched to db 1 for this session. The saved profile is unchanged.');
  await assert.rejects(api.moreKeys(l.list), (err) => err.status === 404, 'the old list is gone');
  await wait(100);
  api.stop();
  assert.equal(states[states.length - 1].conn.db, 1);
  const again = await api.switchDb(1);
  assert.equal(again.switched, false);
  await assert.rejects(api.switchDb(2), (err) => err.status === 422 && /Pick a database the list shows/.test(err.message));
  const tree = await until(async () => {
    const x = await api.getBrowse();
    return x.names && x.names.done ? x : null;
  });
  assert.equal(tree.names.complete, true);
  assert.deepEqual(tree.rows.map((r) => r.pattern.name), ['job:{id}']);
  await assert.rejects(api.moreNames({}), (err) => err.status === 409 && err.message === 'Every key name is read: 41.');
});

test('like the real server, the new routes refuse a body field they do not know and a bad query', async (t) => {
  const { api, server } = await session(t, 'docker:demo-valkey');
  api.stop();
  assert.equal((await raw(server, 'POST', '/api/db', { db: 1, extra: true }))[0], 400);
  assert.equal((await raw(server, 'POST', '/api/db', { db: '1' }))[0], 400);
  assert.equal((await raw(server, 'POST', '/api/analysis', { depth: 'quick', limit: 5 }))[0], 400);
  assert.equal((await raw(server, 'POST', '/api/analysis', { depth: 'huge' }))[0], 400);
  assert.equal((await raw(server, 'DELETE', '/api/analysis', { now: true }))[0], 400);
  assert.equal((await raw(server, 'POST', '/api/browse/more', { all: true }))[0], 400);
  assert.equal((await raw(server, 'POST', '/api/snapshots', { name: 'x' }))[0], 400);
  assert.equal((await raw(server, 'GET', '/api/analysis/slots?order=size'))[0], 400);
  assert.equal((await raw(server, 'GET', '/api/snapshots/compare?a=../etc&b=current'))[0], 400);
  assert.equal((await raw(server, 'GET', '/api/value/item?view=x'))[0], 400);
  const [status, body] = await raw(server, 'GET', '/api/browse?node=1');
  assert.deepEqual([status, body.error], [400, 'That node is not one of this analysis\'s nodes.']);
});

test('before a connect the new routes say to connect first', async () => {
  const { api } = mockApi();
  await assert.rejects(api.getBrowse(), (err) => err.status === 409 && err.message === 'Not connected: connect first.');
  await assert.rejects(api.getAnalysis(), (err) => err.status === 409);
  const state = await api.getState();
  assert.equal(state.analysis.status, 'none');
});
