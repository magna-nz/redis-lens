// The pure pieces of Browse's writes: which edits each type offers, why a write is off
// (read-only, a command the server refused, the value's state), how a value is edited as
// text or as hex, the key list after a write, and a bulk action's words.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  EDITS, KEY_CMDS, NEW_KEY_TYPES, readOnlyText, writeOff, editOff, cleanHex, hexOfText, textOfHex, editStart, valueFrom,
  replaceRow, dropRow, addRow, itemAt, newKeyName, itemLabel, bulkProgress, bulkRunLabel, valueFacts,
} from '../views/browse.js';

const conn = (extra = {}) => ({ name: 'local', env: 'LOCAL', readOnly: false, refused: [], ...extra });
const value = (extra = {}) => ({ kind: 'hash', type: 'hash', gone: false, loading: false, writing: false, cut: null, value: null, items: [], ...extra });
const row = (id, extra = {}) => ({ id, name: id, ttl: 'never', ...extra });
const list = (ids) => ({ list: 1, loaded: ids.length, rows: ids.map((id) => row(id)) });
const bulk = (extra = {}) => ({
  action: 'delete', ttl: null, stage: 'running', count: { matched: 1200, withTtl: 0, otherPattern: 0, unreadable: 0, toChange: 1200 },
  run: { seen: 600, sent: 600, changed: 600, skipped: 0, failed: 0, unreadable: 0, moved: 0, file: null, stopping: false }, ...extra,
});

test('the edits each type offers, as the TUI offers them; a stream entry is not edited', () => {
  assert.deepEqual(Object.keys(EDITS).sort(), ['hash', 'json', 'list', 'set', 'stream', 'string', 'zset']);
  assert.deepEqual(EDITS.string.edit.cmds, ['WATCH', 'SET'], 'a string edit is sent after WATCH: a refused WATCH turns it off');
  assert.equal(writeOff(conn({ refused: ['WATCH'] }), EDITS.string.edit.cmds), 'Off: the server refused WATCH for this user.');
  assert.deepEqual(EDITS.hash.add.cmds, ['HSETNX'], 'adding a field never overwrites one');
  assert.deepEqual(EDITS.hash.fieldTtl.cmds, ['HEXPIRE']);
  assert.deepEqual(EDITS.list.remove.cmds, ['WATCH', 'LSET', 'LREM']);
  assert.deepEqual(EDITS.set.edit.cmds, ['SREM', 'SADD']);
  assert.equal(EDITS.zset.edit.label, 'Edit score');
  assert.equal(EDITS.stream.edit, undefined);
  assert.equal(EDITS.stream.noEdit, "A stream entry can't be edited: add an entry, or delete this one.");
  assert.equal(EDITS.string.add, undefined, 'a string is edited, not added to');
  assert.deepEqual(KEY_CMDS.delete, ['UNLINK']);
  assert.deepEqual(KEY_CMDS.rename, ['RENAMENX'], 'a rename never overwrites a key');
  assert.deepEqual(NEW_KEY_TYPES.map(([t]) => t), ['string', 'hash', 'list', 'set', 'zset', 'stream']);
});

test('why a write is off: no connection, read-only (prod says so), a refused command, the value', () => {
  assert.equal(writeOff(null), 'Connect first.');
  assert.equal(writeOff(conn({ readOnly: true })), 'Read-only: nothing is written to local. Turn writes on in Connections.');
  assert.equal(writeOff(conn({ readOnly: true, env: 'PROD', name: 'p' })), 'Read-only: this is a production connection, so nothing is written to it. Turn writes on in Connections.');
  assert.equal(readOnlyText(null), 'Connect first.');
  assert.equal(writeOff(conn({ refused: ['HSET'] }), ['HSET']), 'Off: the server refused HSET for this user.');
  assert.equal(writeOff(conn({ refused: ['hset'] }), ['HSET']), 'Off: the server refused HSET for this user.', 'case does not matter');
  assert.equal(writeOff(conn({ refused: ['HSET'] }), ['HSETNX']), null, 'another command is still on');
  assert.equal(writeOff(conn(), ['HSET'], value({ gone: true })), 'The key no longer exists.');
  assert.equal(writeOff(conn(), ['HSET'], value({ loading: true })), 'Still loading the value: wait for it.');
  assert.equal(writeOff(conn(), ['HSET'], value({ writing: true })), 'The last write has not answered yet.');
  assert.equal(writeOff(conn(), ['HSET'], value()), null);
});

test('why a value itself is not edited here: its type, a stream, a document not read, a cut string', () => {
  assert.equal(editOff(value({ kind: 'module', type: 'MBbloom--' })), 'redis-lens does not edit values of type MBbloom--.');
  assert.equal(editOff(value({ kind: 'vector' })), 'redis-lens does not edit vector sets.');
  assert.equal(editOff(value({ kind: 'stream' })), EDITS.stream.noEdit);
  assert.equal(editOff(value({ kind: 'json', value: null })), "The document is not read, so it can't be edited here.");
  assert.match(editOff(value({ kind: 'string', cut: 'the first 64 KiB' })), /^Editing is off: only the first 64 KiB of this string is loaded/);
  assert.equal(editOff(value({ kind: 'string' })), null);
  assert.equal(editOff(value({ kind: 'zset' })), null);
});

test('hex as typed: spaces allowed, whole bytes only', () => {
  assert.equal(cleanHex('00 FF\n0a'), '00ff0a');
  assert.equal(cleanHex('0'), null);
  assert.equal(cleanHex('zz'), null);
  assert.equal(cleanHex(''), '');
  assert.equal(hexOfText('hé'), '68c3a9');
});

test('a value is edited as text only when a text box holds it faithfully, else as hex', () => {
  assert.equal(textOfHex(hexOfText('line one\n\ttwo')), 'line one\n\ttwo');
  assert.equal(textOfHex(hexOfText('café ☕')), 'café ☕');
  for (const bad of ['1b5b33316d', '0d0a', 'ff', 'c3', 'e280a8', 'e280ae', '7f', 'c280']) assert.equal(textOfHex(bad), null, bad);
  assert.equal(textOfHex('efbbbf61'), null, 'a byte order mark is kept, and it is not typable text');
  assert.deepEqual(editStart(hexOfText('ok')), { hex: false, text: 'ok' });
  assert.deepEqual(editStart('00ff'), { hex: true, text: '00ff' });
});

test('what a value box sends: the text, the bytes as hex, or why not', () => {
  assert.deepEqual(valueFrom(false, 'abc'), { value: 'abc' });
  assert.deepEqual(valueFrom(false, ''), { value: '' });
  assert.deepEqual(valueFrom(true, '00 ff'), { value: { hex: '00ff' } });
  assert.deepEqual(valueFrom(true, 'abc'), { error: 'Hex digits come in pairs, 0 to 9 and a to f, two for each byte.' });
});

test('a key list after a write: the row read again, a deleted key dropped, a new key first', () => {
  const l = list(['a', 'b', 'c']);
  const r = replaceRow(l, 'b', row('b', { ttl: '1h 0m' }));
  assert.equal(r.rows[1].ttl, '1h 0m');
  assert.equal(l.rows[1].ttl, 'never', 'the list given is not changed');
  assert.equal(replaceRow(l, 'x', row('x')), l);
  const renamed = replaceRow(l, 'b', row('b2'));
  assert.deepEqual(renamed.rows.map((x) => x.id), ['a', 'b2', 'c'], 'a rename keeps the row in its place');
  const d = dropRow(l, 'a');
  assert.deepEqual([d.rows.map((x) => x.id), d.loaded], [['b', 'c'], 2]);
  assert.equal(dropRow(l, 'x'), l);
  const n = addRow(l, row('n'));
  assert.deepEqual([n.rows.map((x) => x.id), n.loaded], [['n', 'a', 'b', 'c'], 4]);
  assert.equal(addRow(l, row('a', { ttl: '5s' })).rows[0].ttl, '5s', 'listed already: replaced');
  assert.equal(addRow(null, row('n')), null);
});

test('the item an index names after a write, and what an item is called', () => {
  const v = value({ items: [{ index: 0, name: 'a', value: '1' }, { index: 1, name: 'b', value: '2' }] });
  assert.equal(itemAt(v, 1).name, 'b');
  assert.equal(itemAt(v, 5).name, 'b', 'past the end: the last one');
  assert.equal(itemAt(value(), 0), null);
  assert.equal(itemLabel(v, v.items[0]), 'field a');
  assert.equal(itemLabel(value({ kind: 'list' }), { index: 3, name: null, value: 'x' }), 'element #3');
  assert.equal(itemLabel(value({ kind: 'set' }), { index: 0, name: null, value: 'red' }), 'member red');
  assert.equal(itemLabel(value({ kind: 'stream' }), { index: 0, name: '1-0', value: null }), 'entry 1-0');
  assert.equal(newKeyName('user:{id}:cart'), 'user:');
  assert.equal(newKeyName('queue:emails'), 'queue:emails');
});

test("a bulk action's progress and its button", () => {
  assert.deepEqual(bulkProgress(bulk({ stage: 'counting', run: null, count: { matched: 1500, withTtl: 0, otherPattern: 0, unreadable: 0, toChange: 0 } })), { pct: null, text: 'Dry run: 1,500 keys of the pattern counted so far. Nothing is written.' });
  assert.equal(bulkProgress(bulk({ stage: 'counted', run: null })), null);
  assert.deepEqual(bulkProgress(bulk()), { pct: 50, text: '600 keys deleted · 600 of about 1,200 looked at.' });
  const ex = bulkProgress(bulk({ action: 'expire', ttl: '2h', run: { seen: 1200, sent: 1150, changed: 1150, skipped: 50, failed: 2, unreadable: 0, moved: 0, file: null, stopping: false } }));
  assert.deepEqual(ex, { pct: 100, text: '1,150 keys given a TTL of 2h · 1,200 of about 1,200 looked at · 50 left alone · 2 failed.' });
  assert.equal(bulkProgress(null), null);
  assert.equal(bulkRunLabel(bulk()), 'Delete 1,200 keys…');
  assert.equal(bulkRunLabel(bulk({ action: 'expire', count: { matched: 1, withTtl: 0, otherPattern: 0, unreadable: 0, toChange: 1 } })), 'Set the TTL on 1 key…');
});

test('a key just added says 1 field, 1 member, 1 entry: never "1 fields"', () => {
  assert.equal(valueFacts({ kind: 'hash', meta: { length: 1, lengthUnit: 'fields', ttl: 'never' } }), '1 field · never expires');
  assert.equal(valueFacts({ kind: 'zset', meta: { length: 1, lengthUnit: 'members' } }), '1 member');
  assert.equal(valueFacts({ kind: 'stream', meta: { length: 1, lengthUnit: 'entries' } }), '1 entry');
  assert.equal(valueFacts({ kind: 'list', meta: { length: 2, lengthUnit: 'elements' } }), '2 elements');
});
