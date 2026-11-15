// The sample data's phase 3 routes, driven through the same `createReal`, adapters and
// confirm flow the page uses: each edit per type, add key, expiry, rename, delete, the
// write gate (read-only, prod's questions, a command the server refuses, an answer from
// another run), and bulk actions from the dry run to the summary, stopped and cancelled.
import test from 'node:test';
import assert from 'node:assert/strict';
import { createApi, createReal } from '../api.js';
import { createMockServer, hexOf } from '../mock.js';

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** Connect, wait for the live connection; `answers` is what the confirm dialog says. */
async function session(t, name, server = createMockServer()) {
  const backend = createReal({ ...server, mock: true });
  const api = createApi(backend);
  t.after(() => api.stop());
  const states = [];
  const bulks = [];
  const asked = [];
  const answers = [];
  api.on('state', (s) => states.push(s));
  api.on('bulk', (b) => bulks.push(b.bulk));
  api.setConfirmHandler(async (c) => {
    asked.push(c);
    const a = answers.length ? answers.shift() : { typed: c.expect ?? '' };
    return a;
  });
  api.start();
  await api.connect(name.startsWith('docker:') ? { docker: name.slice(7) } : { name });
  for (let i = 0; i < 40 && !(states.length && states[states.length - 1].liveState === 'ready' && states[states.length - 1].connected); i += 1) await wait(50);
  return { api, server, states, bulks, asked, answers };
}

async function until(fn, tries = 100) {
  for (let i = 0; i < tries; i += 1) {
    const v = await fn();
    if (v) return v;
    await wait(100);
  }
  throw new Error('waited too long');
}

async function raw(server, method, path, body) {
  const r = await server.fetchFn(path, { method, headers: { Authorization: `Bearer ${server.token}` }, body: body === undefined ? undefined : JSON.stringify(body) });
  return [r.status, await r.json()];
}

/** The text of an error a call threw. */
async function refusal(p) {
  try {
    await p;
  } catch (err) {
    return [err.status ?? null, err.message];
  }
  throw new Error('it was not refused');
}

const open = (api, key) => api.openValue({ key: hexOf(key) });
const lastBulk = (bulks, stage) => until(() => {
  const b = bulks[bulks.length - 1];
  return b && (!stage || b.stage === stage) ? b : null;
});

test('a hash: set a field, add one, a field TTL, delete one; each reply has the value read again', async (t) => {
  const { api, asked } = await session(t, 'docker:demo-redis');
  const v = await open(api, 'cache:product:51007');
  assert.equal(v.kind, 'hash');
  assert.equal(v.rev, 0);
  assert.equal(v.writing, false);
  const name = v.items[0];
  assert.equal(name.name, 'name');

  const e = await api.editValue({ view: v.view, rev: v.rev, index: 0, value: 'Road shoe' });
  assert.equal(asked.length, 0, 'an edit off prod is sent at once');
  assert.equal(e.sent, true);
  assert.equal(e.done, true);
  assert.equal(e.message, 'Saved.');
  assert.equal(e.value.rev, 1);
  assert.equal(e.value.items[0].value, 'Road shoe');
  assert.equal(e.row.type, 'hash');

  assert.deepEqual(await refusal(api.editValue({ view: v.view, rev: 0, index: 0, value: 'x' })), [409, 'The value was read again since the page showed it: look at it again, then try once more.']);
  const same = await api.editValue({ view: v.view, rev: 1, index: 0, value: 'Road shoe' });
  assert.deepEqual([same.sent, same.done, same.message, same.value], [false, false, 'Not changed, so nothing was sent.', null]);

  const taken = await api.addItem({ view: v.view, field: 'price', value: '1' });
  assert.deepEqual([taken.sent, taken.done, taken.message], [true, false, 'Not added: the field exists already. Edit it instead.']);
  const added = await api.addItem({ view: v.view, field: 'colour', value: 'red' });
  assert.equal(added.message, 'Added.');
  assert.equal(added.value.length, 7);
  assert.equal(added.row.length, 7);
  assert.ok(added.value.items.some((i) => i.name === 'colour' && i.value === 'red'));

  const ft = await api.setTtl({ view: v.view, ttl: '1h', field: 1, rev: added.value.rev });
  assert.equal(ft.message, 'Field TTL set.');
  assert.equal(ft.value.items[1].ttl, '1h 0m');

  const removed = await api.removeItem({ view: v.view, rev: ft.value.rev, index: 6 });
  assert.equal(asked.length, 1, 'a delete asks');
  assert.equal(asked[0].kind, 'ask');
  assert.equal(asked[0].text, "Send HDEL cache:product:51007 colour to demo-redis? This can't be undone.");
  assert.equal(removed.message, 'Deleted.');
  assert.equal(removed.value.length, 6);
  assert.deepEqual(await refusal(api.removeItem({ view: v.view, rev: removed.value.rev, index: 99 })), [422, 'There is no such item in what is loaded.']);
});

test('a string: set it (text or hex), the TTL set and removed, rename, delete', async (t) => {
  const { api, asked, server } = await session(t, 'docker:demo-redis');
  const v = await open(api, 'rl:203.0.0.7:login');
  assert.equal(v.kind, 'string');
  const e = await api.editValue({ view: v.view, rev: 0, index: 0, value: '42' });
  assert.equal(e.value.value.copy, '42');
  const hx = await api.editValue({ view: v.view, rev: e.value.rev, index: 0, value: { hex: '00ff' } });
  assert.equal(hx.value.value.detected, 'hex');
  assert.deepEqual(await raw(server, 'POST', '/api/value/edit', { view: v.view, rev: hx.value.rev, index: 0, value: { hex: '0' } }), [400, { error: 'a value given as hex must be an even number of hex digits' }]);

  const ttl = await api.setTtl({ view: v.view, ttl: '1h30m' });
  assert.equal(ttl.message, 'TTL set to 1h 30m.');
  assert.equal(ttl.row.ttl, '1h 30m');
  assert.equal(ttl.value.rev, hx.value.rev, 'the key TTL alone does not read the value again');
  const gone = await api.setTtl({ view: v.view, ttl: '-' });
  assert.equal(gone.message, 'TTL removed: the key no longer expires.');
  assert.equal(gone.row.ttlSecs, null);
  const again = await api.setTtl({ view: v.view, ttl: 'persist' });
  assert.deepEqual([again.done, again.message], [false, 'The key had no TTL.']);
  assert.deepEqual(await refusal(api.setTtl({ view: v.view, ttl: '0' })), [422, 'A TTL of 0 would delete the key: delete the key instead.']);
  assert.deepEqual(await refusal(api.setTtl({ view: v.view, ttl: 'soon' })), [422, 'Type a TTL such as 90s, 10m, 24h or 7d, or - to remove it.']);
  assert.deepEqual(await refusal(api.setTtl({ view: v.view, ttl: '1h', field: 0, rev: ttl.value.rev })), [422, 'Only a hash field has a TTL of its own.']);

  const taken = await api.renameKey({ view: v.view, name: 'session:5f3a91c' });
  assert.deepEqual([taken.done, taken.message], [false, 'Not renamed: session:5f3a91c exists already, and a rename never overwrites a key.']);
  const moved = await api.renameKey({ view: v.view, name: 'p3test:login' });
  assert.equal(moved.message, 'Renamed to p3test:login.');
  assert.equal(moved.key.name, 'p3test:login');
  assert.equal(moved.row.name, 'p3test:login');
  assert.equal((await open(api, 'rl:203.0.0.7:login')).gone, true, 'the old name is gone');
  const reopened = await open(api, 'p3test:login');
  assert.equal(reopened.value.detected, 'hex', 'the new name holds what was written');

  const del = await api.deleteKey({ view: reopened.view });
  assert.equal(asked.length, 1);
  assert.equal(asked[0].text, "Send UNLINK p3test:login to demo-redis? This can't be undone.");
  assert.deepEqual([del.closed, del.message, del.value, del.row], [true, 'Deleted p3test:login.', null, null]);
  assert.deepEqual(await refusal(api.setTtl({ view: reopened.view, ttl: '1h' })), [404, 'That value is no longer open (a newer one replaced it, or the session changed): open it again.']);
  assert.equal((await open(api, 'p3test:login')).gone, true);
});

test('lists, sets, sorted sets and streams: each edit the server offers, and the refusals', async (t) => {
  const { api } = await session(t, 'docker:demo-valkey');
  const list = await open(api, 'queue:emails');
  const ls = await api.editValue({ view: list.view, rev: 0, index: 2, value: '{"to":"a@example.com"}' });
  assert.equal(ls.value.items[2].value, '{"to":"a@example.com"}');
  const push = await api.addItem({ view: list.view, value: 'tail' });
  assert.equal(push.value.length, list.length + 1, 'RPUSH goes past what is loaded');
  const lrem = await api.removeItem({ view: list.view, rev: push.value.rev, index: 0 });
  assert.equal(lrem.value.length, list.length);

  const set = await open(api, 'tags:1000');
  const sr = await api.editValue({ view: set.view, rev: 0, index: 0, value: 'green' });
  assert.equal(sr.value.items[0].value, 'green');
  const dup = await api.addItem({ view: set.view, value: 'blue' });
  assert.deepEqual([dup.done, dup.message], [false, 'Not added: the member exists already.']);

  const z = await open(api, 'user:88120:feed');
  assert.deepEqual(await refusal(api.editValue({ view: z.view, rev: 0, index: 0, value: 'high' })), [422, 'Type the score as a number, such as 1.5, or inf or -inf.']);
  const zs = await api.editValue({ view: z.view, rev: 0, index: 0, value: '2.5' });
  assert.equal(zs.value.items[0].value, '2.5');
  const za = await api.addItem({ view: z.view, value: 'post:1', score: '-inf' });
  assert.equal(za.message, 'Added.');

  const s = await open(api, 'events:orders');
  assert.deepEqual(await refusal(api.editValue({ view: s.view, rev: 0, index: 0, value: 'x' })), [422, "A stream entry can't be edited: add an entry, or delete this one."]);
  assert.deepEqual(await refusal(api.addItem({ view: s.view, value: 'x' })), [422, 'Type the field.']);
  const xa = await api.addItem({ view: s.view, field: 'kind', value: 'refunded' });
  assert.deepEqual(xa.value.items[0].fields, [{ name: 'kind', value: 'refunded' }], 'the new entry is the newest');
  const xd = await api.removeItem({ view: s.view, rev: xa.value.rev, index: 0 });
  assert.equal(xd.value.length, s.length);

  const str = await open(api, 'cfg:flags');
  assert.deepEqual(await refusal(api.addItem({ view: str.view, value: 'x' })), [422, 'Adding works on a hash, list, set, sorted set or stream: edit this value instead.']);
  assert.deepEqual(await refusal(api.removeItem({ view: str.view, rev: 0, index: 0 })), [422, 'Removing works on a field, element, member or entry: delete the key instead.']);

  const h = await open(api, 'cache:product:51007');
  assert.deepEqual(await refusal(api.setTtl({ view: h.view, ttl: '1h', field: 0, rev: 0 })), [422, 'Field TTLs need Redis 7.4 or later, or Valkey 9.0 or later.']);
});

test('a set that loses its last member is gone, as the server deletes it', async (t) => {
  const { api } = await session(t, 'docker:demo-redis');
  const made = await api.addKey({ name: 'p3test:one', type: 'set', value: 'only' });
  const r = await api.removeItem({ view: made.value.view, rev: 0, index: 0 });
  assert.equal(r.value.gone, true);
  assert.equal(r.row, null);
});

test('add a key of each type; a taken name is not overwritten; the list it names must be open', async (t) => {
  const { api } = await session(t, 'docker:demo-redis');
  const tree = await until(async () => {
    const b = await api.getBrowse();
    return b.rows.length ? b : null;
  });
  const pattern = tree.rows.find((r) => r.kind === 'pattern');
  const l = await api.getKeys({ pattern: pattern.pattern.id });
  const cases = [
    { name: 'p3test:s', type: 'string', value: 's' },
    { name: 'p3test:h', type: 'hash', field: 'f', value: 'v' },
    { name: 'p3test:l', type: 'list', value: 'e' },
    { name: 'p3test:set', type: 'set', value: 'm' },
    { name: 'p3test:z', type: 'zset', value: 'm', score: '2' },
    { name: 'p3test:x', type: 'stream', field: 'f', value: 'v' },
  ];
  for (const c of cases) {
    const w = await api.addKey({ list: l.list, ...c });
    assert.equal(w.message, `Added ${c.name}.`);
    assert.equal(w.row.type, c.type);
    assert.equal(w.value.type, c.type);
    assert.equal(w.value.kind, c.type);
    assert.equal(w.value.rev, 0);
  }
  const x = await open(api, 'p3test:z');
  assert.deepEqual(x.items.map((i) => [i.name, i.value]), [['m', '2']]);
  const taken = await api.addKey({ name: 'p3test:s', type: 'string', value: 'over' });
  assert.deepEqual([taken.done, taken.message, taken.reply.kind, taken.row, taken.value], [false, 'Not added: p3test:s exists already, and adding a key never overwrites one.', 'exists', null, null]);
  assert.deepEqual(await refusal(api.addKey({ list: 999, name: 'p3test:t', type: 'string', value: 'v' })), [404, 'That key list is gone (a newer one replaced it, or the session changed): open it again.']);
  assert.deepEqual(await refusal(api.addKey({ name: 'p3test:t', type: 'hash', value: 'v' })), [422, 'Type the field.']);
  assert.deepEqual(await refusal(api.addKey({ name: 'p3test:t', type: 'zset', value: 'v', score: 'x' })), [422, 'Type the score as a number, such as 1.5, or inf or -inf.']);
});

test('a read-only profile refuses every write first, with the gate words, and sends nothing', async (t) => {
  const { api, asked, server } = await session(t, 'prod-cache-01');
  const ro = 'Not sent: prod-cache-01 is read-only. Turn writes on in Connections.';
  const v = await open(api, 'cache:product:51007');
  for (const call of [
    api.editValue({ view: v.view, rev: 0, index: 0, value: 'x' }),
    api.addItem({ view: v.view, field: 'a', value: 'b' }),
    api.setTtl({ view: v.view, ttl: '1h' }),
    api.renameKey({ view: v.view, name: 'p3test:x' }),
    api.removeItem({ view: v.view, rev: 0, index: 0 }),
    api.deleteKey({ view: v.view }),
    api.addKey({ name: 'p3test:x', type: 'string', value: 'v' }),
    api.startBulk({ pattern: hexOf('tags:{id}'), action: 'delete' }),
    api.runBulk(1),
  ]) assert.deepEqual(await refusal(call), [422, ro]);
  assert.equal(asked.length, 0);
  assert.deepEqual(await raw(server, 'POST', '/api/value/edit', { view: 999, rev: 0, index: 0, value: 'x' }), [422, { error: ro }], 'read-only is refused before the view is looked at');
});

test('prod with writes on: an edit asks, a delete wants the key typed, a bulk the pattern', async (t) => {
  const server = createMockServer();
  const { api, asked, answers, bulks } = await session(t, 'prod-cache-01', server);
  await api.updateConnection('prod-cache-01', { writes: true });
  assert.equal(asked[0].kind, 'type', 'writes on for prod: its name typed');
  const v = await open(api, 'tags:1000');
  const e = await api.editValue({ view: v.view, rev: 0, index: 0, value: 'p3' });
  assert.equal(asked[1].kind, 'ask');
  assert.equal(asked[1].text, 'Send SREM tags:1000 red and SADD tags:1000 p3 (in MULTI/EXEC) to prod-cache-01?');
  assert.equal(e.message, 'Saved.');

  answers.push({ typed: 'tags' });
  assert.deepEqual(await refusal(api.deleteKey({ view: v.view })), [422, 'Not sent: the name typed did not match.']);
  assert.equal(asked[2].kind, 'type');
  assert.equal(asked[2].expect, 'tags:1000');
  assert.equal(asked[2].text, "Type tags:1000 to send UNLINK tags:1000 to prod-cache-01. This can't be undone.");
  const del = await api.deleteKey({ view: v.view });
  assert.equal(del.closed, true);

  const started = await api.startBulk({ pattern: hexOf('tags:{id}'), action: 'expire', ttl: '2h' });
  assert.equal(started.bulk.stage, 'counting');
  assert.equal(started.bulk.prod, true);
  const counted = await lastBulk(bulks, 'counted');
  const run = await api.runBulk(counted.id);
  const q = asked[asked.length - 1];
  assert.equal(q.kind, 'type');
  assert.equal(q.expect, 'tags:{id}');
  assert.match(q.text, /^Type tags:\{id\} to start: Set a TTL on every key of the pattern tags:\{id\} on prod-cache-01\. Dry run: /);
  assert.equal(run.bulk.stage, 'running');
  await api.stopBulk();
  const end = await lastBulk(bulks, 'done');
  assert.match(end.lines[0], /^Stopped after the batch: set a TTL of 2h on [\d,]+ keys of tags:\{id\}\.$/);
});

test('a command the server refuses for this user: Not written once, then Not sent and in state.refused', async (t) => {
  const { api, states, bulks } = await session(t, 'limited-user');
  const v = await open(api, 'cache:product:51007');
  assert.deepEqual(await refusal(api.editValue({ view: v.view, rev: 0, index: 0, value: 'x' })), [422, 'Not written: the server refused HSET for this user.']);
  const s = await until(() => states.find((x) => x.conn && x.conn.refused.includes('HSET')));
  assert.ok(s);
  assert.deepEqual(await refusal(api.editValue({ view: v.view, rev: 0, index: 0, value: 'x' })), [422, 'Not sent: the server refused HSET for this user.']);
  assert.equal((await api.addItem({ view: v.view, field: 'n', value: '1' })).message, 'Added.', 'HSETNX is another command');

  // A bulk delete: counted, asked, then the first batch is refused, and UNLINK is off.
  await api.startBulk({ pattern: hexOf('tags:{id}'), action: 'delete' });
  const counted = await lastBulk(bulks, 'counted');
  await api.runBulk(counted.id);
  const end = await lastBulk(bulks, 'done');
  assert.equal(end.error, true);
  assert.equal(end.lines[0], 'Stopped: the server refused UNLINK for this user. Before that, deleted 0 keys of tags:{id}.');
  assert.deepEqual(await refusal(api.startBulk({ pattern: hexOf('tags:{id}'), action: 'delete' })), [422, 'Not sent: the server refused UNLINK for this user.']);
  assert.deepEqual(await refusal(api.deleteKey({ view: v.view })), [422, 'Not sent: the server refused UNLINK for this user.']);
});

test('an answer from before a database switch is refused: the connection changed', async (t) => {
  const { api, server } = await session(t, 'docker:demo-redis');
  const v = await open(api, 'tags:1000');
  const [status, q] = await raw(server, 'POST', '/api/value/delete', { view: v.view });
  assert.equal(status, 409);
  await api.switchDb(1);
  assert.deepEqual(await raw(server, 'POST', `/api/confirm/${q.confirm.id}`, {}), [409, { error: 'Not sent: the connection changed since the question was asked.' }]);
});

test('a bulk expire: the dry run counts, the question, progress by batch, the summary', async (t) => {
  const { api, asked, bulks } = await session(t, 'docker:demo-redis');
  const started = await api.startBulk({ pattern: hexOf('user:{id}:cart'), action: 'expire', ttl: '2h' });
  assert.deepEqual([started.bulk.stage, started.bulk.ttl, started.bulk.ttlSecs, started.bulk.title], ['counting', '2h', 7200, 'Set a TTL on every key of the pattern']);
  assert.deepEqual(await refusal(api.startBulk({ pattern: hexOf('tags:{id}'), action: 'delete' })), [409, 'A bulk action is running already: wait for it to end, or stop it first.']);
  assert.deepEqual(await refusal(api.runBulk(started.bulk.id)), [409, 'The dry run has not finished: wait for its count.']);
  // While it counts or runs, the key writes, a database switch and an analysis wait.
  const busy = [409, 'A bulk action is running: wait for it to end, or stop it first.'];
  const v = await open(api, 'tags:1000');
  assert.deepEqual(await refusal(api.deleteKey({ view: v.view })), busy);
  assert.deepEqual(await refusal(api.addKey({ name: 'p3test:k', type: 'string', value: 'v' })), busy);
  assert.deepEqual(await refusal(api.switchDb(1)), busy);
  assert.deepEqual(await refusal(api.startAnalysis('quick')), busy);
  const counted = await lastBulk(bulks, 'counted');
  assert.ok(counted.count.matched > 0);
  assert.equal(counted.count.toChange, counted.count.matched - counted.count.withTtl);
  assert.match(counted.summary[0], /^Dry run: [\d,]+ keys of the pattern, [\d,]+ with a TTL already\.$/);
  assert.equal(counted.count.otherPattern, 0, 'no other pattern matches user:*:cart');
  assert.equal(counted.summary.length, 2);
  const running = await api.runBulk(counted.id);
  assert.equal(asked[0].kind, 'ask');
  assert.match(asked[0].text, /^Set a TTL on every key of the pattern user:\{id\}:cart on demo-redis\? Dry run: .* PERSIST on the keys in the key file undoes it\. Sends: EXPIRE <key> 7200 NX for each key of SCAN MATCH user:\*:cart, 500 at a time\.$/);
  assert.equal(running.bulk.stage, 'running');
  assert.deepEqual(await refusal(api.runBulk(counted.id)), [409, 'That bulk action has started already.']);
  const end = await lastBulk(bulks, 'done');
  assert.ok(bulks.some((b) => b && b.stage === 'running' && b.run.seen > 0 && b.run.seen < counted.count.matched), 'progress by batch');
  assert.equal(end.run.changed, counted.count.toChange);
  assert.match(end.lines[0], /^Done: set a TTL of 2h on [\d,]+ keys of user:\{id\}:cart\.$/);
  assert.equal(end.lines[end.lines.length - 1], 'PERSIST on those keys undoes it.');
  assert.ok(end.lines.some((l) => l.startsWith('Key names: ')));
  const cleared = await api.stopBulk();
  assert.equal(cleared.bulk, null);
  assert.equal((await api.getBulk()).bulk, null);
});

test('a bulk delete: stopped while counting, cancelled when counted, the question cancelled, stopped while running, done', async (t) => {
  const { api, bulks, answers } = await session(t, 'docker:demo-redis');
  const pattern = hexOf('tags:{id}');
  await api.startBulk({ pattern, action: 'delete' });
  const stopped = await api.stopBulk();
  assert.deepEqual([stopped.bulk.stage, stopped.text, stopped.bulk.lines], ['done', 'Dry run stopped. Nothing was sent.', ['Dry run stopped. Nothing was sent.']]);

  await api.startBulk({ pattern, action: 'delete' });
  await lastBulk(bulks, 'counted');
  const cancelled = await api.stopBulk();
  assert.equal(cancelled.text, 'Bulk action cancelled. Nothing was sent.');

  const b = await api.startBulk({ pattern, action: 'delete' });
  await lastBulk(bulks, 'counted');
  answers.push(null);
  assert.equal((await refusal(api.runBulk(b.bulk.id)))[1], 'Cancelled: nothing was sent.');
  assert.deepEqual((await lastBulk(bulks, 'done')).lines, ['Cancelled: nothing was sent.']);

  const c = await api.startBulk({ pattern, action: 'delete' });
  await lastBulk(bulks, 'counted');
  await api.runBulk(c.bulk.id);
  const stopping = await api.stopBulk();
  assert.deepEqual([stopping.text, stopping.bulk.run.stopping], ['Stopping after the batch on its way.', true]);
  const half = await lastBulk(bulks, 'done');
  assert.match(half.lines[0], /^Stopped after the batch: deleted [\d,]+ keys of tags:\{id\}\.$/);

  const d = await api.startBulk({ pattern, action: 'delete' });
  const counted = await lastBulk(bulks, 'counted');
  assert.equal(counted.id, d.bulk.id);
  await api.runBulk(d.bulk.id);
  const done = await lastBulk(bulks, 'done');
  assert.match(done.lines[0], /^Done: deleted [\d,]+ keys of tags:\{id\}\.$/);
  const tree = await api.getBrowse();
  assert.ok(!JSON.stringify(tree.rows).includes('tags:{id}'), 'the pattern is gone');
});

test('a bulk expire with nothing to change ends at the count, and the run is refused', async (t) => {
  const { api, bulks } = await session(t, 'docker:demo-redis');
  const b = await api.startBulk({ pattern: hexOf('session:{id}'), action: 'expire' });
  assert.equal(b.bulk.ttl, '24h', 'the TTL defaults to 24h');
  const end = await lastBulk(bulks, 'done');
  assert.match(end.lines[0], /^Every key of session:\{id\} \([\d,]+\) has a TTL already\. Nothing was sent\.$/);
  assert.deepEqual(await refusal(api.runBulk(b.bulk.id)), [422, 'Nothing to run: the dry run found nothing to change.']);
  assert.deepEqual(await refusal(api.startBulk({ pattern: hexOf('tags:{id}'), action: 'expire', ttl: 'soon' })), [422, 'not a duration: use 90s, 10m, 24h or 7d']);
  assert.deepEqual(await refusal(api.startBulk({ pattern: hexOf('tags:{id}'), action: 'expire', ttl: '0' })), [422, 'the TTL must be at least 1s']);
  assert.deepEqual(await refusal(api.startBulk({ pattern: hexOf('nope:{id}'), action: 'delete' })), [404, 'There is no such pattern in the patterns shown: read them again.']);
});

test('a bulk on a cluster counts per primary', async (t) => {
  const { api, bulks } = await session(t, 'demo-cluster');
  await api.startBulk({ pattern: hexOf('tags:{id}'), action: 'delete' });
  const counted = await lastBulk(bulks, 'counted');
  assert.equal(counted.nodes.length, 3);
  assert.equal(counted.nodes.reduce((n, x) => n + x.matched, 0), counted.count.matched);
  assert.deepEqual(counted.nodes.map((n) => n.endpoint), ['127.0.0.1:7001', '127.0.0.1:7002', '127.0.0.1:7003']);
});
