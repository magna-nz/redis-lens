// The phase 3 adapters and request bodies, on replies the real server gave (captured from
// the phase 3 integration runs; the key file's temporary directory is shortened to
// `<data dir>`), and the routes as the page sends them.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  ROUTES, METHODS, adaptWrite, adaptBulk, adaptBulkReply, adaptValue, adaptConfirm,
  bytesBody, editRequest, addItemRequest, ttlRequest, addKeyRequest, bulkRequest, createReal,
} from '../api.js';

// POST /api/value/add {"field":"age","value":"42","view":2} → 200 (Redis).
const ADDED = {
  closed: false, done: true, key: { copy: 'user:1', id: '757365723a31', name: 'user:1' }, message: 'Added.', reply: { kind: 'int', value: 1 },
  row: { bytes: null, copy: 'user:1', encoding: 'listpack', freq: null, id: '757365723a31', length: 3, length_unit: 'fields', name: 'user:1', node: null, ttl: 'never', ttl_secs: null, type: 'hash' },
  sent: true,
  value: {
    capped: false, cluster: false, command: 'HSCAN', cut: null, done: true, error: null, field_ttls: true, field_ttls_off: null, gone: false,
    items: [
      { at: null, fields: null, index: 0, name: 'name', ttl: 'never', value: 'bob' },
      { at: null, fields: null, index: 1, name: 'mail', ttl: 'never', value: 'a@x' },
      { at: null, fields: null, index: 2, name: 'age', ttl: 'never', value: '42' },
    ],
    key: { copy: 'user:1', id: '757365723a31', name: 'user:1' }, kind: 'hash', length: 3, loaded: 3, loading: false, max_items: 10000, message: null,
    meta: { bytes: null, copy: 'user:1', encoding: 'listpack', freq: null, id: '757365723a31', length: 3, length_unit: 'bytes', name: 'user:1', node: null, ttl: 'never', ttl_secs: null, type: 'hash' },
    module: null, more: false, notice: 'All 3 items are loaded.', pages: 1, pattern: null, refused: [], rev: 2, type: 'hash', value: null, view: 2, writing: false,
  },
};

// The answer to POST /api/value/delete's question → 200.
const DELETED = { closed: true, done: true, key: { copy: 'rank:1', id: '72616e6b3a31', name: 'rank:1' }, message: 'Deleted rank:1.', reply: { kind: 'int', value: 1 }, row: null, sent: true, value: null };

// POST /api/value/edit with the value it holds already → 200, nothing sent.
const UNCHANGED = { closed: false, done: false, key: { copy: 'str:a', id: '7374723a61', name: 'str:a' }, message: 'Not changed, so nothing was sent.', reply: null, sent: false };

// The `bulk` event at the end of a bulk delete on a three-primary cluster.
const CLUSTER_DONE = {
  bulk: {
    action: 'delete', at: 2, count: { matched: 300, other_pattern: 0, to_change: 300, unreadable: 0, with_ttl: 0 }, error: false, glob: 'item:*', id: 1,
    lines: ['Done: deleted 300 keys of item:{id}.', 'Key names, each after its node and a tab: <data dir>/bulk/cl-08a25a07b54a/20261115T145619Z-delete.txt'],
    nodes: [{ endpoint: '127.0.0.1:52168', matched: 98, node: 0 }, { endpoint: '127.0.0.1:52169', matched: 107, node: 1 }, { endpoint: '127.0.0.1:52170', matched: 95, node: 2 }],
    pattern: { copy: 'item:{id}', id: '6974656d3a7b69647d', name: 'item:{id}' }, prod: false, profile: 'cl',
    run: { changed: 300, failed: 0, file: '<data dir>/bulk/cl-08a25a07b54a/20261115T145619Z-delete.txt', moved: 0, seen: 300, sent: 300, skipped: 0, stopping: false, unreadable: 0 },
    stage: 'done', summary: ['Dry run: 300 keys of the pattern will be deleted with UNLINK.'], title: 'Delete every key of the pattern', ttl: null, ttl_secs: null,
  },
};

// POST /api/bulk {"action":"expire","pattern":"636172743a7b69647d","ttl":"2h"} → 202.
const COUNTING = {
  bulk: {
    action: 'expire', at: null, count: { matched: 0, other_pattern: 0, to_change: 0, unreadable: 0, with_ttl: 0 }, error: false, glob: 'cart:*', id: 5, lines: [], nodes: [],
    pattern: { copy: 'cart:{id}', id: '636172743a7b69647d', name: 'cart:{id}' }, prod: false, profile: 'local', run: null, stage: 'counting', summary: [],
    title: 'Set a TTL on every key of the pattern', ttl: '2h', ttl_secs: 7200,
  },
};

test('a write reply: the message, the reply, the row and the value read again', () => {
  const w = adaptWrite(ADDED);
  assert.deepEqual([w.sent, w.done, w.closed, w.message], [true, true, false, 'Added.']);
  assert.deepEqual(w.reply, { kind: 'int', value: 1 });
  assert.deepEqual(w.key, { name: 'user:1', id: '757365723a31', copy: 'user:1' });
  assert.equal(w.row.length, 3);
  assert.equal(w.row.lengthUnit, 'fields');
  assert.equal(w.value.rev, 2);
  assert.equal(w.value.writing, false);
  assert.equal(w.value.view, 2);
  assert.deepEqual(w.value.items.map((i) => [i.name, i.value]), [['name', 'bob'], ['mail', 'a@x'], ['age', '42']]);
});

test('a deleted key: closed, no row and no value; an unchanged edit: not sent', () => {
  const d = adaptWrite(DELETED);
  assert.deepEqual([d.closed, d.row, d.value, d.message, d.key.name], [true, null, null, 'Deleted rank:1.', 'rank:1']);
  const u = adaptWrite(UNCHANGED);
  assert.deepEqual([u.sent, u.done, u.reply, u.value, u.row], [false, false, null, null, null]);
  assert.equal(adaptWrite({ value: { '...': '...' } }).value, null, 'a value body without its view is not taken as one');
  assert.equal(adaptWrite(null).message, null);
});

test('a value body names its rev and whether a write is on its way', () => {
  assert.deepEqual([adaptValue({ view: 1 }).rev, adaptValue({ view: 1 }).writing], [0, false]);
  assert.deepEqual([adaptValue({ view: 1, rev: 7, writing: true }).rev, adaptValue({ view: 1, rev: 7, writing: true }).writing], [7, true]);
});

test('a bulk body: the dry run, the run by node, the lines', () => {
  const b = adaptBulk(CLUSTER_DONE.bulk);
  assert.deepEqual([b.id, b.action, b.stage, b.glob, b.profile, b.prod, b.at, b.error], [1, 'delete', 'done', 'item:*', 'cl', false, 2, false]);
  assert.equal(b.pattern.name, 'item:{id}');
  assert.equal(b.pattern.id, '6974656d3a7b69647d');
  assert.deepEqual(b.count, { matched: 300, withTtl: 0, otherPattern: 0, unreadable: 0, toChange: 300 });
  assert.deepEqual(b.nodes.map((n) => [n.node, n.endpoint, n.matched]), [[0, '127.0.0.1:52168', 98], [1, '127.0.0.1:52169', 107], [2, '127.0.0.1:52170', 95]]);
  assert.deepEqual(b.run, { seen: 300, sent: 300, changed: 300, skipped: 0, failed: 0, unreadable: 0, moved: 0, file: '<data dir>/bulk/cl-08a25a07b54a/20261115T145619Z-delete.txt', stopping: false });
  assert.equal(b.lines.length, 2);
  assert.equal(b.ttl, null);
  const c = adaptBulkReply(COUNTING);
  assert.deepEqual([c.bulk.stage, c.bulk.run, c.bulk.ttl, c.bulk.ttlSecs, c.bulk.summary, c.text], ['counting', null, '2h', 7200, [], null]);
});

test('a bulk reply of null is no bulk action; odd shapes fall back safely', () => {
  assert.deepEqual(adaptBulkReply({ bulk: null }), { bulk: null, text: null });
  assert.equal(adaptBulkReply({ bulk: null, text: 'x' }).text, 'x');
  const odd = adaptBulk({ stage: 'exploded', action: 'flush', count: null, run: 3, nodes: 'x', lines: [1, 'a'] });
  assert.equal(odd.stage, 'done', 'an unknown stage is an ended one: nothing more is offered');
  assert.equal(odd.action, 'delete');
  assert.deepEqual(odd.count, { matched: 0, withTtl: 0, otherPattern: 0, unreadable: 0, toChange: 0 });
  assert.deepEqual(odd.nodes, []);
  assert.equal(odd.title, 'Delete every key of the pattern');
});

test('a bulk question on prod: the pattern to type', () => {
  const c = adaptConfirm({ confirm: { expect: 'job:{id}', id: 'abc', kind: 'type', text: 'Type job:{id} to start: Delete every key of the pattern job:{id} on prod1.' } });
  assert.deepEqual([c.kind, c.expect, c.id], ['type', 'job:{id}', 'abc']);
});

test('the request bodies: text or hex bytes, and only the fields given', () => {
  assert.equal(bytesBody('hi'), 'hi');
  assert.deepEqual(bytesBody({ hex: '00 FF' }), { hex: '00ff' });
  assert.equal(bytesBody(undefined), '');
  assert.deepEqual(editRequest({ view: '3', rev: '2', index: 1, value: 'v' }), { view: 3, rev: 2, index: 1, value: 'v' });
  assert.deepEqual(editRequest({ view: 3, rev: 0, value: 'v' }), { view: 3, rev: 0, index: 0, value: 'v' });
  assert.deepEqual(addItemRequest({ view: 1, value: 'm' }), { view: 1, value: 'm' });
  assert.deepEqual(addItemRequest({ view: 1, field: 'f', value: { hex: '01' } }), { view: 1, field: 'f', value: { hex: '01' } });
  assert.deepEqual(addItemRequest({ view: 1, value: 'm', score: 1.5 }), { view: 1, value: 'm', score: '1.5' });
  assert.deepEqual(ttlRequest({ view: 1, ttl: ' 1h ' }), { view: 1, ttl: '1h' });
  assert.deepEqual(ttlRequest({ view: 1, ttl: '-', field: 0, rev: 4 }), { view: 1, ttl: '-', field: 0, rev: 4 });
  assert.deepEqual(addKeyRequest({ name: 'k', type: 'string', value: 'v' }), { name: 'k', type: 'string', value: 'v' });
  assert.deepEqual(addKeyRequest({ list: 2, name: 'k', type: 'zset', value: 'm', score: '2' }), { name: 'k', type: 'zset', value: 'm', list: 2, score: '2' });
  assert.deepEqual(bulkRequest({ pattern: 'ab', action: 'delete', ttl: '2h' }), { pattern: 'ab', action: 'delete' }, 'a delete takes no TTL');
  assert.deepEqual(bulkRequest({ pattern: 'ab', action: 'expire', ttl: ' 2h ' }), { pattern: 'ab', action: 'expire', ttl: '2h' });
  assert.deepEqual(bulkRequest({ pattern: 'ab', action: 'expire', ttl: '' }), { pattern: 'ab', action: 'expire' }, 'the server defaults to 24h');
});

test('the routes and methods of phase 3', () => {
  assert.deepEqual(ROUTES.valueEdit(), ['POST', '/api/value/edit']);
  assert.deepEqual(ROUTES.valueAdd(), ['POST', '/api/value/add']);
  assert.deepEqual(ROUTES.valueTtl(), ['POST', '/api/value/ttl']);
  assert.deepEqual(ROUTES.valueRename(), ['POST', '/api/value/rename']);
  assert.deepEqual(ROUTES.valueRemove(), ['POST', '/api/value/remove']);
  assert.deepEqual(ROUTES.valueDelete(), ['POST', '/api/value/delete']);
  assert.deepEqual(ROUTES.keyAdd(), ['POST', '/api/keys/add']);
  assert.deepEqual(ROUTES.bulk(), ['GET', '/api/bulk']);
  assert.deepEqual(ROUTES.bulkStart(), ['POST', '/api/bulk']);
  assert.deepEqual(ROUTES.bulkRun(), ['POST', '/api/bulk/run']);
  assert.deepEqual(ROUTES.bulkStop(), ['DELETE', '/api/bulk']);
  for (const m of ['editValue', 'addItem', 'setTtl', 'renameKey', 'removeItem', 'deleteKey', 'addKey', 'getBulk', 'startBulk', 'runBulk', 'stopBulk']) assert.ok(METHODS.includes(m), m);
});

test('what the page sends for each write, as the server takes it', async () => {
  const sent = [];
  const fetchFn = async (path, init) => {
    sent.push([init.method, path, init.body ? JSON.parse(init.body) : null]);
    return { status: 200, ok: true, json: async () => ({ bulk: null }) };
  };
  const r = createReal({ fetchFn, token: 'a'.repeat(64) });
  await r.renameKey({ view: '4', name: 'p3test:b' });
  await r.removeItem({ view: 4, rev: '1', index: '2' });
  await r.deleteKey({ view: 4 });
  await r.runBulk('9');
  await r.stopBulk();
  await r.getBulk();
  assert.deepEqual(sent, [
    ['POST', '/api/value/rename', { view: 4, name: 'p3test:b' }],
    ['POST', '/api/value/remove', { view: 4, rev: 1, index: 2 }],
    ['POST', '/api/value/delete', { view: 4 }],
    ['POST', '/api/bulk/run', { bulk: 9 }],
    ['DELETE', '/api/bulk', {}],
    ['GET', '/api/bulk', null],
  ]);
});
