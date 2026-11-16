// The pure pieces of Browse and Analyze: the tree's filter and lines, key lists after "Load
// more", a value's facts and items, following the session, and Analyze's words for an
// estimate, a run and a change.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  filterRows, keysText, treeExact, treeFoot, treeSource, listFoot, mergeList, listNotes, valueFacts, itemColumns, itemCells, valueFoot, blank, follow,
} from '../views/browse.js';
import { panelsFor, estimateText, statusText, failedText, progressPercent, changePercent, freedBars } from '../views/analyze.js';
import { analysisLine, CHANGES_TEXT } from '../views/overview.js';

const pat = (name, extra = {}) => ({ kind: 'pattern', pattern: { name, id: '00', copy: name }, estKeys: 10, literal: false, ...extra });
const group = (label, children) => ({ kind: 'group', label, key: label, children });

test('the tree filter keeps a matching pattern, a matching group whole, or only the matching patterns of a group', () => {
  const rows = [group('user:', [pat('user:{id}:cart'), pat('user:{id}:feed')]), pat('session:{id}'), pat('tags:{id}')];
  assert.equal(filterRows(rows, ''), rows);
  assert.deepEqual(filterRows(rows, 'SESSION').map((r) => r.pattern.name), ['session:{id}']);
  assert.equal(filterRows(rows, 'user:')[0].children.length, 2, 'the group label matches: all its patterns');
  const feed = filterRows(rows, 'feed');
  assert.equal(feed.length, 1);
  assert.deepEqual(feed[0].children.map((c) => c.pattern.name), ['user:{id}:feed']);
  assert.equal(rows[0].children.length, 2, 'the rows given are not changed');
  assert.deepEqual(filterRows(rows, 'nothing'), []);
});

test('a pattern\'s keys: estimated with ~, exact without, one key for a literal name', () => {
  assert.equal(keysText(pat('a', { estKeys: 8700 }), false), '~8.7K keys');
  assert.equal(keysText(pat('a', { estKeys: 60 }), true), '60 keys');
  assert.equal(keysText(pat('a', { estKeys: 1 }), true), '1 key');
  assert.equal(keysText(pat('queue:emails', { literal: true, estKeys: 1 }), false), 'one key');
  assert.equal(keysText(pat('a', { estKeys: null }), false), '—');
});

test('the tree says where its rows come from and how far it got', () => {
  const names = { source: 'names', names: { complete: false, namesRead: 8000, dbsize: 15845, nodesDone: null, done: true, missing: [], refused: [], skipped: [], reading: false } };
  assert.equal(treeExact(names, null), false);
  assert.equal(treeFoot(names, null), '8,000 names read of about 15,845.');
  assert.equal(treeFoot({ ...names, names: { ...names.names, reading: true } }, null), '8,000 names read of about 15,845. Reading more…');
  const all = { source: 'names', names: { ...names.names, complete: true, namesRead: 74 } };
  assert.equal(treeExact(all, null), true);
  assert.equal(treeFoot(all, null), 'All 74 names read.');
  const cluster = { source: 'names', names: { ...names.names, nodesDone: [2, 3], missing: ['127.0.0.1:7003'] } };
  assert.equal(treeFoot(cluster, null), '8,000 names read of about 15,845. 2 of 3 nodes done. Some nodes are left out.');
  assert.equal(treeFoot({ source: 'partial', partial: { sampled: 2500, target: 10000 } }, null), 'Partial results: 2,500 of 10,000 keys sampled so far.');
  const analysed = { source: 'analysis', node: 1, nodes: [{ endpoint: 'a:1' }, { endpoint: 'b:2' }] };
  assert.equal(treeFoot(analysed, { sampledKeys: 10000 }), 'From 10,000 sampled keys, on node b:2 only.');
  assert.equal(treeExact(analysed, { complete: true }), true);
  assert.equal(treeSource(names), 'Names only, no analysis yet: counts, no sizes or TTLs.');
  assert.equal(treeSource({ source: 'analysis', previous: true }), 'From the previous analysis, while a new one runs.');
  assert.equal(treeSource({ source: 'none', why: 'An analysis is starting: its patterns come once it samples keys.' }), 'An analysis is starting: its patterns come once it samples keys.');
});

test('a key list\'s line: loaded so far, every key, or what a find matched', () => {
  assert.equal(listFoot({ kind: 'pattern', loaded: 1000, done: false }), '1,000 loaded so far.');
  assert.equal(listFoot({ kind: 'pattern', loaded: 60, done: true }), 'Every key is loaded: 60.');
  assert.equal(listFoot({ kind: 'find', loaded: 0, done: true, glob: '*nope*' }), 'No key matches *nope*.');
  assert.equal(listFoot({ kind: 'find', loaded: 1, done: true }), '1 key matches.');
  assert.equal(listFoot({ kind: 'find', loaded: 0, done: false }), 'No key matched yet: scan further to look at more names.');
});

test('"Load more" keeps every row once: new rows from new_from are appended, a whole list replaces', () => {
  const row = (n) => ({ name: `k${n}`, id: String(n) });
  const old = { list: 4, rows: [row(0), row(1), row(2)], loaded: 3, newFrom: 0 };
  const onlyNew = { list: 4, rows: [row(3), row(4)], loaded: 5, newFrom: 3 };
  assert.deepEqual(mergeList(old, onlyNew).rows.map((r) => r.name), ['k0', 'k1', 'k2', 'k3', 'k4']);
  const whole = { list: 4, rows: [row(0), row(1), row(2), row(3)], loaded: 4, newFrom: 3 };
  assert.equal(mergeList(old, whole), whole, 'the whole list: taken as it is');
  const overlap = { list: 4, rows: [row(2), row(3)], loaded: 4, newFrom: 2 };
  assert.deepEqual(mergeList(old, overlap).rows.map((r) => r.name), ['k0', 'k1', 'k2', 'k3'], 'rows from new_from replace the ones held from there');
  const other = { list: 5, rows: [row(9)], loaded: 3, newFrom: 2 };
  assert.equal(mergeList(old, other), other, 'another list is never merged');
  assert.equal(mergeList(null, onlyNew), onlyNew);
});

test('a key list\'s notes: its error, what was refused, what was left out', () => {
  const l = { error: null, refused: ['MEMORY USAGE is refused for this user, so the keys have no sizes.'], otherPattern: 2, unreadable: 1, cluster: { missing: ['127.0.0.1:7003'] } };
  assert.deepEqual(listNotes(l), [
    'MEMORY USAGE is refused for this user, so the keys have no sizes.',
    '2 keys matched the pattern\'s glob but belong to another pattern, so they are not listed.',
    '1 name is not UTF-8, so not listed.',
    'Not read: 127.0.0.1:7003 (no connection to it).',
  ]);
  assert.deepEqual(listNotes(null), []);
});

test('a full key list says so and offers no more', () => {
  const l = { error: null, refused: [], otherPattern: 0, unreadable: 0, cluster: null, capped: true, done: false, maxRows: 50000 };
  assert.deepEqual(listNotes(l), ['This list holds 50,000 keys or more, so no more are loaded. Find a narrower name to see others.']);
  assert.deepEqual(listNotes({ ...l, capped: false }), []);
});

test('a value\'s facts, items and line', () => {
  const v = { kind: 'hash', length: 3, loaded: 3, items: [{ index: 0, name: 'sku:1', value: '{"qty":2}', ttl: 'never' }], fieldTtls: true, notice: 'All 3 items are loaded.', meta: { length: 3, lengthUnit: 'fields', bytes: 1290, ttl: '6d 10h', encoding: 'listpack', freq: 20 } };
  assert.equal(valueFacts(v), '3 fields · 1.3K in memory · expires in 6d 10h · stored as listpack · LFU counter 20');
  assert.equal(valueFacts({ kind: 'string', length: null, meta: { length: 11, lengthUnit: 'bytes', ttl: 'never' } }, '127.0.0.1:7002'), '11 bytes · never expires · on node 127.0.0.1:7002');
  assert.deepEqual(itemColumns(v), ['Field', 'Value', 'TTL']);
  assert.deepEqual(itemCells(v, v.items[0]), ['sku:1', '{"qty":2}', 'never']);
  assert.deepEqual(itemColumns({ kind: 'zset' }), ['Member', 'Score']);
  assert.deepEqual(itemCells({ kind: 'stream' }, { name: '1-0', fields: [{ name: 'kind', value: 'paid' }, { name: 'order', value: '7' }] }), ['1-0', 'kind=paid  order=7']);
  assert.deepEqual(itemCells({ kind: 'list' }, { index: 4, value: 'j5' }), ['4', 'j5']);
  assert.equal(valueFoot(v), 'All 3 items are loaded.');
  assert.equal(valueFoot({ ...v, length: 412000, loaded: 500, notice: null }), '500 of 412,000 loaded.');
});

test('following the session: another connection starts afresh; a new run drops the list and the value only', () => {
  const store = blank();
  const state = (name, run) => ({ conn: { name, docker: false }, analysis: { run } });
  assert.equal(follow(store, state('a', 1)), 'reset');
  store.filter = 'user';
  store.sort = 'keys';
  store.list = { list: 1 };
  store.value = { view: 2 };
  assert.equal(follow(store, state('a', 1)), null);
  assert.equal(follow(store, state('a', 2)), 'run', 'an analysis or a database switch is a new run');
  assert.equal(store.list, null);
  assert.equal(store.value, null);
  assert.equal(store.filter, 'user', 'the filter stays');
  assert.equal(store.sort, 'keys');
  assert.equal(follow(store, state('b', 2)), 'reset');
  assert.equal(store.filter, '');
  // What another area asks for is an address now (browse-route.test.js), not kept here.
  assert.equal('pending' in store, false);
});

test('Analyze: Nodes and Slots only on a cluster', () => {
  assert.deepEqual(panelsFor(false).map((p) => p.id), ['big_keys', 'expiry', 'changes', 'hot_keys', 'snapshots']);
  assert.deepEqual(panelsFor(true).map((p) => p.id), ['big_keys', 'expiry', 'changes', 'hot_keys', 'nodes', 'slots', 'snapshots']);
});

test('the estimate is said before an analysis starts, with what is off', () => {
  const e = { depth: 'quick', sampleLimit: 10000, keys: 10000, commands: 40016, durationSecs: 12, about: 'at most 4,000 commands a second', shards: 0, sizes: true, freq: true };
  assert.equal(estimateText(e), 'A quick analysis samples 10,000 keys (at most 10,000) with about 40,016 commands, about 12s, at most 4,000 commands a second. It reads each key\'s type, TTL and size through the command budget, never a value.');
  const cluster = estimateText({ ...e, depth: 'deep', sampleLimit: 100000, shards: 3, durationSecs: null, about: null, sizes: false, freq: false });
  assert.match(cluster, /^A deep analysis samples 10,000 keys \(at most 100,000\) across 3 shards with about 40,016 commands\./);
  assert.match(cluster, /Sizes are off: MEMORY USAGE is not allowed/);
  assert.match(cluster, /Hot keys stays empty/);
  assert.match(estimateText(null), /never a value/);
});

test('the analysis in one line, and its progress', () => {
  assert.equal(statusText(null), 'No analysis yet.');
  assert.equal(statusText({ status: 'none', report: null, error: null, note: null }), 'No analysis yet.');
  assert.equal(statusText({ status: 'none', report: null, error: 'the connection was lost', note: null }), 'The last analysis failed: the connection was lost');
  assert.equal(statusText({ status: 'none', report: null, error: 'The analysis failed: the server refused a command it needs.', note: null }), 'The last analysis failed: the server refused a command it needs.');
  assert.equal(failedText('The analysis failed: the connection to the server broke.'), 'The analysis failed: the connection to the server broke.');
  assert.equal(failedText('Wrong password.'), 'The analysis failed: Wrong password.');
  assert.equal(statusText({ status: 'connecting' }), 'Connecting to analyze…');
  assert.equal(statusText({ status: 'sampling', sampled: 2500, target: 10000 }), 'Analyzing: 2,500 of 10,000 keys sampled.');
  assert.equal(statusText({ status: 'stopping' }), 'Stopping: what it sampled so far is kept.');
  const r = { takenAt: Date.UTC(2026, 10, 15, 9, 12) / 1000, sampledKeys: 10000, ageSecs: 120, complete: false, stopped: true };
  assert.equal(statusText({ status: 'done', report: r }), 'Analyzed at 09:12 UTC · 10,000 keys sampled · 2m 00s ago · stopped early: partial, not saved');
  assert.equal(statusText({ status: 'done', report: { ...r, stopped: false, complete: true, ageSecs: null } }), 'Analyzed at 09:12 UTC · 10,000 keys sampled · every key looked at');
  assert.equal(progressPercent({ sampled: 2500, target: 10000 }), 25);
  assert.equal(progressPercent({ sampled: 5, target: 0 }), 0);
  assert.equal(progressPercent({ sampled: null, target: 10 }), 0);
  assert.equal(progressPercent({ sampled: 20, target: 10 }), 100);
});

test('a change as a share of what was there: new, gone, or a signed percentage', () => {
  assert.equal(changePercent({ change: { cause: 'new' } }), 'new');
  assert.equal(changePercent({ change: { cause: 'gone' } }), 'gone');
  assert.equal(changePercent({ bytesBefore: 1000, change: { cause: 'bigger_keys', deltaBytes: 120 } }), '+12%');
  assert.equal(changePercent({ bytesBefore: null, keysBefore: 200, change: { cause: 'fewer_keys', deltaKeys: -50 } }), '−25%');
  assert.equal(changePercent({ bytesBefore: 0, change: { cause: 'more_keys', deltaBytes: 3 } }), '—');
  assert.equal(changePercent({ change: null }), '—');
});

test('the memory expiry frees: a bar each, scaled to the biggest, with its share', () => {
  const bars = freedBars({ within: [{ label: '1 h', bytes: 4240 }, { label: '1 d', bytes: 4240 }], never: 5640, total: 9880 });
  assert.deepEqual(bars.map((b) => [b.label, b.pct, b.never]), [['within 1 h', 75, false], ['within 1 d', 75, false], ['never', 100, true]]);
  assert.ok(Math.abs(bars[2].share - 5640 / 9880) < 1e-9);
  assert.deepEqual(freedBars(null), []);
});

test('Overview: the analysis line and the changes link', () => {
  const now = Date.UTC(2026, 10, 15, 14, 0, 0);
  const a = { takenAt: Date.UTC(2026, 10, 15, 9, 12) / 1000, sampledKeys: 74, complete: true, stopped: false, neverExpiresShare: 0.57 };
  assert.equal(analysisLine(a, now), 'From the analysis of today 09:12 UTC · 74 keys sampled · every key looked at · 57% of the memory never expires.');
  assert.equal(analysisLine({ ...a, complete: false, stopped: true, neverExpiresShare: null }, now), 'From the analysis of today 09:12 UTC · 74 keys sampled · stopped early.');
  assert.equal(analysisLine(null), null);
  assert.deepEqual(Object.keys(CHANGES_TEXT).sort(), ['diff', 'failed', 'none', 'pending']);
});
