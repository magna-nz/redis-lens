// The sample data's phase 4 routes, driven through the same `createReal`, adapters and
// confirm flow the page uses: Browse › Streams (before and after an analysis, off for a
// limited user, a cluster, XACK, XAUTOCLAIM and XTRIM through the write gate), Browse ›
// Search (the module on demo-redis, not loaded elsewhere, refused for a limited user; the
// three query commands, pages, a field, FT.CREATE and FT.DROPINDEX) and Hot keys ranked from
// a Monitor run.
import test from 'node:test';
import assert from 'node:assert/strict';
import { createApi, createReal, streamWriteText } from '../api.js';
import { createMockServer, hexOf } from '../mock.js';
import { queryStart } from '../views/search.js';

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function session(t, name) {
  const server = createMockServer();
  const api = createApi(createReal({ ...server, mock: true }));
  t.after(() => api.stop());
  const states = [];
  const asked = [];
  const answers = [];
  const progress = [];
  api.on('state', (s) => states.push(s));
  api.on('progress', (p) => progress.push(p));
  api.setConfirmHandler(async (c) => {
    asked.push(c);
    return answers.length ? answers.shift() : { typed: c.expect ?? '' };
  });
  api.start();
  await api.connect(name.startsWith('docker:') ? { docker: name.slice(7) } : { name });
  for (let i = 0; i < 40 && !(states.length && states[states.length - 1].liveState === 'ready' && states[states.length - 1].connected); i += 1) await wait(50);
  return { api, server, states, asked, answers, progress };
}

async function until(fn, tries = 120) {
  for (let i = 0; i < tries; i += 1) {
    const v = await fn();
    if (v) return v;
    await wait(100);
  }
  throw new Error('waited too long');
}

async function analyzed(s) {
  await s.api.startAnalysis('quick');
  await until(() => s.progress.find((p) => p.kind === 'done'));
  await until(async () => (await s.api.getState()).liveState === 'ready');
}

async function refusal(p) {
  try {
    await p;
  } catch (err) {
    return [err.status ?? null, err.message];
  }
  throw new Error('it was not refused');
}

const ORDERS = hexOf('events:orders');

test('streams: the TUI\'s words before an analysis, then the stream it found, with its groups', async (t) => {
  const s = await session(t, 'docker:demo-redis');
  const before = await s.api.getStreams();
  assert.equal(before.status, 'no_report');
  assert.equal(before.text, 'Streams are found by an analysis: it samples the keys and lists the streams among them.');
  await analyzed(s);
  const l = await s.api.getStreams();
  assert.equal(l.status, 'ready');
  assert.equal(l.cluster, false);
  assert.equal(l.writes, null);
  assert.deepEqual(l.rows.map((r) => r.key.name), ['events:orders']);
  const r = l.rows[0];
  assert.equal(r.length, 96000);
  assert.equal(r.verdict, 'stuck');
  assert.deepEqual(r.verdicts.map((v) => v.label), ['stuck', 'lagging']);
  assert.deepEqual(r.groups.map((g) => g.name.name), ['billing', 'shipping']);
  const billing = r.groups[0];
  assert.equal(billing.pending, 8);
  assert.equal(billing.readers[0].stuck, true);
  assert.equal(billing.claim.to.name, 'worker-2', 'the claim form starts with the most recently active consumer');
  assert.equal(r.groups[1].lagging, true);
});

test('streams: XACK an entry of the pending list shown, XAUTOCLAIM, XTRIM asked for, each through the gate', async (t) => {
  const s = await session(t, 'docker:demo-redis');
  await analyzed(s);
  const l = await s.api.getStreams();
  const g = l.rows[0].groups[0];
  const p = await s.api.getPending({ key: ORDERS, group: g.name.id });
  assert.equal(p.pending.entries.length, 8);
  const first = p.pending.entries[0];
  assert.deepEqual(await refusal(s.api.ackEntry({ key: ORDERS, group: g.name.id, id: '1-0' })), [422, 'That entry is not in the pending list shown: list the pending entries again.']);
  assert.deepEqual(await refusal(s.api.ackEntry({ key: ORDERS, group: g.name.id, id: 'x' })), [422, 'That is not a stream entry ID, such as 1700000000000-0.']);
  const ack = await s.api.ackEntry({ key: ORDERS, group: g.name.id, id: first.id });
  assert.equal(s.asked.length, 0, 'an XACK off prod is sent at once');
  assert.equal(streamWriteText('ack', ack, { id: first.id }), `Acknowledged ${first.id}.`);
  // Read again as the page does after a write: at once (`now=1`).
  assert.equal((await s.api.getStreams({ now: true })).rows[0].groups[0].pending, 7);

  assert.deepEqual(await refusal(s.api.claimEntries({ key: ORDERS, group: g.name.id, to: '', minIdleMs: '0', count: '10' })), [422, 'Type the consumer that takes the entries.']);
  assert.deepEqual(await refusal(s.api.claimEntries({ key: ORDERS, group: g.name.id, to: 'w9', minIdleMs: '0', count: '0' })), [422, 'The count is a whole number, 1 or more.']);
  const claim = await s.api.claimEntries({ key: ORDERS, group: g.name.id, to: 'worker-2', minIdleMs: '60000', count: '100' });
  assert.equal(streamWriteText('claim', claim, { to: 'worker-2' }), 'Claimed 5 entries for worker-2.');
  assert.deepEqual(await refusal(s.api.claimEntries({ key: ORDERS, group: hexOf('nobody'), to: 'w', minIdleMs: '0', count: '1' })), [404, "That group is not one of the stream's: read the streams again."]);

  assert.deepEqual(await refusal(s.api.trimStream({ key: ORDERS, by: 'MINID', value: 'soon' })), [422, 'MINID takes an entry ID, such as 1700000000000-0.']);
  const trim = await s.api.trimStream({ key: ORDERS, by: 'MAXLEN', value: '50000' });
  assert.equal(s.asked.length, 1, 'XTRIM is destructive: asked');
  assert.equal(s.asked[0].text, "Send XTRIM events:orders MAXLEN ~ 50000 to demo-redis? This can't be undone.");
  assert.equal(streamWriteText('trim', trim), 'Trimmed 46000 entries.', 'as the TUI says it: the count ungrouped');
  assert.equal((await s.api.getStreams()).rows[0].length, 50000);
  s.answers.push(null);
  assert.equal((await refusal(s.api.trimStream({ key: ORDERS, by: 'MAXLEN', value: '1' })))[1], 'Cancelled: nothing was sent.');
  assert.equal((await s.api.getStreams()).rows[0].length, 50000, 'cancelled: nothing trimmed');
  assert.deepEqual(await refusal(s.api.trimStream({ key: hexOf('events:other'), by: 'MAXLEN', value: '1' })), [404, 'That stream is not in the list: read the streams again, or analyze again to update it.']);
});

test('streams: a read-only profile lists, and sends nothing; a limited user sees the view off', async (t) => {
  const s = await session(t, 'prod-cache-01');
  await analyzed(s);
  const l = await s.api.getStreams();
  assert.equal(l.writes, 'Not sent: prod-cache-01 is read-only. Turn writes on in Connections.');
  const g = l.rows[0].groups[0];
  const p = await s.api.getPending({ key: ORDERS, group: g.name.id });
  assert.deepEqual(await refusal(s.api.ackEntry({ key: ORDERS, group: g.name.id, id: p.pending.entries[0].id })), [422, l.writes]);
  assert.deepEqual(await refusal(s.api.trimStream({ key: ORDERS, by: 'MAXLEN', value: '1' })), [422, l.writes]);
  assert.equal(s.asked.length, 0, 'nothing is asked: the gate refuses first');

  const lim = await session(t, 'limited-user');
  await analyzed(lim);
  assert.equal((await lim.api.getStreams()).off, 'The server refused XINFO for this user, so this view is off.');
  assert.equal((await lim.api.getPending({ key: ORDERS, group: hexOf('billing') })).off, 'The server refused XINFO for this user, so this view is off.');
});

test('streams on a cluster: each stream says the node it was read on', async (t) => {
  const s = await session(t, 'demo-cluster');
  await analyzed(s);
  const l = await s.api.getStreams();
  assert.equal(l.cluster, true);
  assert.match(l.rows[0].endpoint, /^127\.0\.0\.1:700[123]$/);
  assert.equal(typeof l.rows[0].node, 'number');
});

test('search: the indexes of demo-redis, the three commands as the TUI sends them, pages and a field', async (t) => {
  const s = await session(t, 'docker:demo-redis');
  const l = await s.api.getSearch();
  assert.equal(l.module, 'Redis Search');
  assert.deepEqual(l.indexes.map((i) => i.name.name), ['idx:products', 'idx:flags']);
  assert.equal(l.indexes[1].building, true);
  const idx = l.indexes[0].name.id;
  assert.deepEqual(await refusal(s.api.runQuery({ index: idx, cmd: 'explain' })), [422, 'Run a query first: FT.EXPLAIN uses the query last run.']);
  assert.deepEqual(await refusal(s.api.runQuery({ index: idx, cmd: 'search', query: 'x'.repeat(1001) })), [422, 'A query is at most 1,000 characters.']);
  const q = await s.api.runQuery({ index: idx, cmd: 'search', query: '*' });
  assert.equal(q.results.total, 2420);
  assert.equal(q.results.kept, 50);
  assert.equal(q.results.docs.length, 10);
  const next = await s.api.getResults({ run: q.results.run, from: 10 });
  assert.equal(next.from, 10);
  assert.equal(next.docs[0].index, 10);
  const f = await s.api.getResultField({ run: q.results.run, doc: 0, field: 0 });
  assert.equal(f.name, 'name');
  assert.equal(f.shown.lines[0], 'Trail shoe 0');
  const explain = await s.api.runQuery({ index: idx, cmd: 'explain' });
  assert.deepEqual(explain.text.lines, ['<WILDCARD>']);
  const profile = await s.api.runQuery({ index: idx, cmd: 'profile' });
  assert.equal(profile.results.run, q.results.run + 1);
  assert.equal(profile.text.lines[0], 'Shards');
  assert.deepEqual(await refusal(s.api.getResults({ run: q.results.run, from: 0 })), [409, "Those results are not the last query's: run it again."]);
  const priced = await s.api.runQuery({ index: idx, cmd: 'search', query: '@price:[0 60]' });
  assert.ok(priced.results.total > 0 && priced.results.total < 2420);
  assert.match((await refusal(s.api.runQuery({ index: idx, cmd: 'search', query: '@price:[0' })))[1], /^FT\.SEARCH failed: Syntax error/);
  const again = await s.api.getSearch();
  assert.equal(again.lastQuery, '@price:[0', 'the last query is the one typed last');
  assert.equal(queryStart(again), '@price:[0', 'the query box starts from it');
});

test('search: FT.CREATE from the form (sent), FT.DROPINDEX asked for; the form\'s refusals in the TUI\'s words', async (t) => {
  const s = await session(t, 'docker:demo-redis');
  await s.api.getSearch();
  assert.deepEqual(await refusal(s.api.createIndex({ name: 'p4 idx', on: 'HASH', prefixes: '', fields: 'title TEXT' })), [422, 'An index name has no spaces.']);
  assert.deepEqual(await refusal(s.api.createIndex({ name: 'p4idx', on: 'HASH', prefixes: '', fields: 'title WORDS' })), [422, "A field's type is TEXT, TAG, NUMERIC or GEO, not WORDS."]);
  assert.deepEqual(await refusal(s.api.createIndex({ name: 'p4idx', on: 'JSON', prefixes: '', fields: 'title TEXT' })), [422, 'On JSON a field is a path such as $.title: title TEXT.']);
  const made = await s.api.createIndex({ name: 'p4idx', on: 'HASH', prefixes: 'p4p:', fields: 'title TEXT, price NUMERIC' });
  assert.equal(made.sent, true);
  assert.equal(s.asked.length, 0, 'FT.CREATE off prod is sent at once');
  const l = await s.api.getSearch();
  const idx = l.indexes.find((i) => i.name.name === 'p4idx');
  assert.deepEqual(idx.schema.map((f) => `${f.name} ${f.type}`), ['title TEXT', 'price NUMERIC']);
  await s.api.dropIndex({ index: idx.name.id });
  assert.equal(s.asked[0].text, "Send FT.DROPINDEX p4idx to demo-redis? This can't be undone.");
  assert.equal((await s.api.getSearch()).indexes.some((i) => i.name.name === 'p4idx'), false);
  assert.deepEqual(await refusal(s.api.dropIndex({ index: idx.name.id })), [404, 'That index is not in the list: read the indexes again.']);
});

test('search: off without the module and refused for a limited user, in the TUI\'s words; nothing is sent', async (t) => {
  const s = await session(t, 'docker:demo-valkey');
  const l = await s.api.getSearch();
  assert.equal(l.off, 'FT._LIST is not available: the Search module is not loaded.');
  assert.equal(l.noModule, true);
  assert.match(l.detail, /valkey-search/);
  assert.deepEqual(await refusal(s.api.createIndex({ name: 'a', on: 'HASH', prefixes: '', fields: 'a TEXT' })), [422, "Not available: the server has no Search module, so indexes can't be created or dropped."]);
  assert.equal((await s.api.runQuery({ index: hexOf('a'), cmd: 'search', query: '*' })).off, l.off);
  const lim = await session(t, 'limited-user');
  const off = await lim.api.getSearch();
  assert.equal(off.off, 'The server refused FT._LIST for this user, so this view is off.');
  assert.equal(off.noModule, false);
  const ro = await session(t, 'prod-cache-01');
  assert.equal((await ro.api.getSearch()).noModule, true);
});

test('hot keys from a Monitor run: none, then ranking, then the keys it named most', async (t) => {
  const s = await session(t, 'docker:demo-redis');
  await analyzed(s);
  assert.match((await s.api.getHotKeys()).off, /LFU/);
  const none = await s.api.getHotKeysMonitor();
  assert.equal(none.status, 'none');
  assert.equal(none.text, 'Or rank keys by how often Monitor sees them: Server › Monitor.');
  await s.api.startMonitor();
  assert.match(s.asked[0].text, /^MONITOR shows every command/);
  await wait(2600);
  await s.api.stopMonitor();
  const pending = await s.api.getHotKeysMonitor();
  assert.equal(pending.status, 'pending');
  const ready = await until(async () => {
    const m = await s.api.getHotKeysMonitor();
    return m.status === 'ready' ? m : null;
  });
  const r = ready.ranking;
  assert.match(r.title, /^from Monitor, \d+ s, [\d,]+ lines, at \d\d:\d\d UTC$/);
  assert.ok(r.rows.length > 0);
  assert.equal(r.rows.reduce((n, x) => n + x.count, 0), r.lines, 'each line names one key');
  assert.ok(r.rows[0].count >= r.rows[r.rows.length - 1].count, 'most named first');
  assert.match(r.rows[0].named, / · named (once|twice|\d+ times) in [\d,]+ lines$/);
  assert.match(ready.text, /^Ranked by how often the last Monitor run named each key/);
  const lfu = await session(t, 'prod-cache-01');
  await analyzed(lfu);
  assert.equal((await lfu.api.getHotKeysMonitor()).status, 'lfu', 'with LFU counters Hot keys ranks by them');
});
