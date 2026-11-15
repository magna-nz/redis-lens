// Phase 4's adapters, fed replies the real server gave in tests/web_p4_it.rs (Redis 8.10
// and valkey-bundle 9.1, a cluster, a limited user; lists cut to what the test needs), and
// the routes and request bodies the page sends.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  ROUTES, METHODS, createReal, adaptStreams, adaptPending, adaptStreamWrite, streamWriteText, claimRequest, trimRequest,
  adaptSearch, adaptQuery, adaptSearchResults, adaptSearchField, createIndexRequest, adaptMonitorHot,
} from '../api.js';

const W1 = { copy: 'w1', id: '7731', name: 'w1' };
const BILLING = { copy: 'billing', id: '62696c6c696e67', name: 'billing' };
const ORDERS = { copy: 'p4s:orders', id: '7034733a6f7264657273', name: 'p4s:orders' };

const STREAMS = {
  cluster: false, error: null, found: 3, note: null, shown: 2, status: 'ready', writes: 'Not sent: ro is read-only. Turn writes on in Connections.',
  rows: [
    {
      bytes: 9704, endpoint: null, entries_added: 6, first_id: '1794764710058-0', gone: false, group_count: 1, groups_text: null, key: ORDERS, lag: 2,
      last_id: '1794764710210-0', left_out: null, length: 6, node: null, oldest: '0s', oldest_age_ms: 219, pending: 4, verdict: 'ok', verdicts: [],
      groups: [{
        claim: { count: 100, min_idle_ms: 60000, to: W1 }, consumers: 2, lag: 2, lagging: false, last_delivered_id: '1794764710059-2', name: BILLING, pending: 4,
        readers: [
          { idle: '0s', idle_ms: 218, inactive: '0s', inactive_ms: 218, name: W1, pending: 3, stuck: false },
          { idle: '0s', idle_ms: 218, inactive: null, inactive_ms: null, name: { copy: 'w2', id: '7732', name: 'w2' }, pending: 1, stuck: false },
        ],
      }],
    },
    {
      bytes: 8536, endpoint: null, entries_added: 250, first_id: '1794764710061-0', gone: false, group_count: 0, groups: [], groups_text: 'No consumer groups: nothing reads this stream.',
      key: { copy: 'p4s:big', id: '7034733a626967', name: 'p4s:big' }, lag: null, last_id: '1794764710127-3', left_out: null, length: 250, node: null, oldest: '0s', oldest_age_ms: 216,
      pending: 0, verdict: 'no readers', verdicts: [{ label: 'no readers', text: 'No readers: no consumer group reads this stream.' }],
    },
  ],
};

test('streams: the list as the server sends it, groups and consumers included', () => {
  const l = adaptStreams(STREAMS);
  assert.equal(l.off, null);
  assert.equal(l.status, 'ready');
  assert.equal(l.found, 3);
  assert.equal(l.shown, 2);
  assert.equal(l.writes, 'Not sent: ro is read-only. Turn writes on in Connections.');
  const [orders, big] = l.rows;
  assert.deepEqual(orders.key, { name: 'p4s:orders', id: '7034733a6f7264657273', copy: 'p4s:orders' });
  assert.equal(orders.length, 6);
  assert.equal(orders.lastId, '1794764710210-0');
  assert.equal(orders.oldest, '0s');
  const g = orders.groups[0];
  assert.equal(g.name.id, '62696c6c696e67');
  assert.equal(g.lag, 2);
  assert.equal(g.lastDeliveredId, '1794764710059-2');
  assert.deepEqual(g.claim, { to: { name: 'w1', id: '7731', copy: 'w1' }, minIdleMs: 60000, count: 100 });
  assert.equal(g.readers[1].inactive, null, 'Redis before 7.2 has no inactive time');
  assert.equal(g.readers[0].pending, 3);
  assert.equal(big.lag, null, 'no group: no lag');
  assert.deepEqual(big.groups, []);
  assert.equal(big.groupsText, 'No consumer groups: nothing reads this stream.');
  assert.deepEqual(big.verdicts, [{ label: 'no readers', text: 'No readers: no consumer group reads this stream.' }]);
});

test('streams: before an analysis, none found, off, and a limited user without XINFO GROUPS', () => {
  const before = adaptStreams({ analysis_running: false, rows: [], status: 'no_report', text: 'Streams are found by an analysis: it samples the keys and lists the streams among them.' });
  assert.equal(before.status, 'no_report');
  assert.equal(before.text, 'Streams are found by an analysis: it samples the keys and lists the streams among them.');
  assert.equal(before.analysisRunning, false);
  const none = adaptStreams({ hint: 'It samples keys, so it may miss some; a Deep analysis samples more.', rows: [], status: 'none', text: 'The analysis found no streams.' });
  assert.equal(none.status, 'none');
  assert.match(none.hint, /Deep analysis/);
  const off = adaptStreams({ off: 'The server refused XINFO for this user, so this view is off.' });
  assert.equal(off.off, 'The server refused XINFO for this user, so this view is off.');
  assert.deepEqual(off.rows, []);
  const limited = adaptStreams({
    ...STREAMS,
    rows: [{ ...STREAMS.rows[0], group_count: null, groups: null, groups_text: 'XINFO GROUPS is not available for this user.', lag: null, pending: null, left_out: 'Left out: the server refused XINFO GROUPS for this user.' }],
  });
  assert.equal(limited.rows[0].groups, null);
  assert.equal(limited.rows[0].groupsText, 'XINFO GROUPS is not available for this user.');
  assert.equal(limited.rows[0].leftOut, 'Left out: the server refused XINFO GROUPS for this user.');
  const gone = adaptStreams({ ...STREAMS, rows: [{ key: ORDERS, node: 1, endpoint: '127.0.0.1:50806', gone: true, text: 'The key is gone, or no longer a stream.' }] });
  assert.equal(gone.rows[0].gone, true);
  assert.equal(gone.rows[0].endpoint, '127.0.0.1:50806');
  assert.equal(gone.rows[0].text, 'The key is gone, or no longer a stream.');
});

test('streams: a pending list, and a stream gone by the time it is read', () => {
  const p = adaptPending({
    pending: {
      consumer: null, error: null, group: BILLING, key: ORDERS, max: 100,
      entries: [{ consumer: W1, deliveries: 1, id: '1794764710058-0', idle: '0s', idle_ms: 141 }, { consumer: { copy: 'w2', id: '7732', name: 'w2' }, deliveries: 3, id: '1794764710059-2', idle: '2m', idle_ms: 120000 }],
    },
    row: STREAMS.rows[0],
  });
  assert.equal(p.pending.entries.length, 2);
  assert.deepEqual(p.pending.entries[1], { id: '1794764710059-2', consumer: { name: 'w2', id: '7732', copy: 'w2' }, idleMs: 120000, idle: '2m', deliveries: 3 });
  assert.equal(p.pending.max, 100);
  assert.equal(p.row.key.id, ORDERS.id);
  const gone = adaptPending({ pending: null, row: null, text: 'The key is gone, or no longer a stream.' });
  assert.equal(gone.pending, null);
  assert.equal(gone.text, 'The key is gone, or no longer a stream.');
});

test('stream writes: the replies in the TUI\'s words', () => {
  const ack = adaptStreamWrite({ reply: { kind: 'int', value: 1 }, sent: true });
  assert.equal(streamWriteText('ack', ack, { id: '1794764710058-0' }), 'Acknowledged 1794764710058-0.');
  assert.equal(streamWriteText('ack', adaptStreamWrite({ reply: { kind: 'int', value: 0 }, sent: true }), { id: '1-0' }), '1-0 was not pending any more.');
  const claim = adaptStreamWrite({ reply: { deleted: 0, ids: ['1794764710059-0', '1794764710059-1', '1794764710059-2'], kind: 'claimed' }, sent: true });
  assert.deepEqual(claim.reply, { kind: 'claimed', value: null, ids: ['1794764710059-0', '1794764710059-1', '1794764710059-2'], deleted: 0 });
  assert.equal(streamWriteText('claim', claim, { to: 'w2' }), 'Claimed 3 entries for w2.');
  const deleted = adaptStreamWrite({ reply: { deleted: 1, ids: ['1-0', '2-0'], kind: 'claimed' }, sent: true });
  assert.equal(streamWriteText('claim', deleted, { to: 'b-2' }), 'Claimed 2 entries for b-2. 1 entry no longer in the stream left the pending list.');
  assert.equal(streamWriteText('claim', adaptStreamWrite({ reply: { deleted: 0, ids: ['1-0'], kind: 'claimed' }, sent: true }), { to: 'x' }), 'Claimed 1 entry for x.');
  assert.equal(streamWriteText('trim', adaptStreamWrite({ reply: { kind: 'int', value: 200 }, sent: true })), 'Trimmed 200 entries.');
  assert.equal(streamWriteText('trim', adaptStreamWrite({ reply: { kind: 'int', value: 0 }, sent: true })), 'Trimmed 0 entries: with ~ the server removes whole blocks only.');
  assert.equal(streamWriteText('trim', adaptStreamWrite({ sent: true, reply: { kind: 'ok' } })), 'Done.');
});

test('stream writes: the forms are sent as typed, the hex ids as given', () => {
  assert.deepEqual(claimRequest({ key: ORDERS.id, group: BILLING.id, to: ' w2 ', minIdleMs: '0', count: '10' }), { key: ORDERS.id, group: BILLING.id, to: ' w2 ', min_idle_ms: '0', count: '10' });
  assert.deepEqual(trimRequest({ key: ORDERS.id, by: 'MINID', value: '1-0' }), { key: ORDERS.id, by: 'MINID', value: '1-0' });
  assert.equal(trimRequest({ key: 'ab', by: 'anything', value: 5 }).by, 'MAXLEN');
  assert.equal(trimRequest({ key: 'ab', by: 'MAXLEN', value: 5 }).value, '5');
});

const INDEX = {
  building: false, bytes: 9264, docs: 3, failures: 0, fields_text: null, indexing: false, info: true, info_text: null, left_out: null,
  name: { copy: 'p4idx', id: '7034696478', name: 'p4idx' }, on: 'HASH', prefixes: ['p4p:'], progress: 1.0, progress_text: '100%',
  schema: [{ flags: [], name: 'title', path: null, type: 'TEXT' }, { flags: ['SORTABLE'], name: 'price', path: null, type: 'NUMERIC' }],
};
const RESULTS = {
  cmd: 'search', count: '3 matches', empty: null, from: 0, index: INDEX.name, kept: 3, max: 50, page: 10, query: '@price:[0 100]', run: 1, took: '1 ms', total: 3,
  docs: [
    { field_count: 2, fields: [{ name: 'title', value: 'red shoe' }, { name: 'price', value: '10' }], index: 0, key: { copy: 'p4p:1', id: '7034703a31', name: 'p4p:1' } },
    { field_count: 2, fields: [{ name: 'title', value: 'blue hat' }, { name: 'price', value: '30' }], index: 2, key: { copy: 'p4p:3', id: '7034703a33', name: 'p4p:3' } },
  ],
};

test('search: the list of Redis 8 and of valkey-search, and the empty list', () => {
  const l = adaptSearch({
    busy: null, cmd_off: { explain: null, profile: null, search: null }, empty: null, error: null, first_of: null, indexes: [INDEX], last_query: '@price:[0 100]',
    last_query_copy: '@price:[0 100]', module: 'Redis Search', node: null, node_title: null, notes: [], results: RESULTS, total: 1, writes: null,
  });
  assert.equal(l.off, null);
  assert.equal(l.module, 'Redis Search');
  assert.equal(l.indexes[0].name.id, '7034696478');
  assert.equal(l.indexes[0].docs, 3);
  assert.deepEqual(l.indexes[0].prefixes, ['p4p:']);
  assert.deepEqual(l.indexes[0].schema[1], { name: 'price', path: null, type: 'NUMERIC', flags: ['SORTABLE'] });
  assert.equal(l.lastQuery, '@price:[0 100]');
  assert.equal(l.results.run, 1);
  const valkey = adaptSearch({ ...l, module: 'valkey-search', cmd_off: { explain: 'The server does not support FT.EXPLAIN.', profile: null, search: null }, indexes: [{ ...INDEX, bytes: null, building: true, progress: 0, progress_text: '0%', left_out: 'left out: size (valkey-search does not report it)' }], last_query: null, results: null });
  assert.equal(valkey.indexes[0].bytes, null);
  assert.equal(valkey.indexes[0].building, true);
  assert.equal(valkey.indexes[0].leftOut, 'left out: size (valkey-search does not report it)');
  assert.equal(valkey.cmdOff.explain, 'The server does not support FT.EXPLAIN.');
  assert.equal(valkey.lastQuery, null);
  const empty = adaptSearch({ busy: null, cmd_off: {}, empty: { text: 'Redis Search is loaded; nothing was created with FT.CREATE yet.', title: 'The module has no indexes.' }, error: null, first_of: null, indexes: [], last_query: null, module: 'Redis Search', node: null, notes: [], results: null, total: 0, writes: null });
  assert.deepEqual(empty.empty, { title: 'The module has no indexes.', text: 'Redis Search is loaded; nothing was created with FT.CREATE yet.' });
});

test('search: off without the module, and refused for the user', () => {
  const none = adaptSearch({ detail: 'FT._LIST is unknown, so there is no Redis Search (built into Redis 8) or valkey-search to list indexes from.', no_module: true, off: 'FT._LIST is not available: the Search module is not loaded.' });
  assert.equal(none.off, 'FT._LIST is not available: the Search module is not loaded.');
  assert.equal(none.noModule, true);
  assert.match(none.detail, /built into Redis 8/);
  const refused = adaptSearch({ detail: null, no_module: false, off: 'The server refused FT._LIST for this user, so this view is off.' });
  assert.equal(refused.noModule, false);
  assert.equal(refused.detail, null);
});

test('search: a query\'s results, FT.EXPLAIN\'s text, FT.PROFILE\'s, a command off, and a field', () => {
  const q = adaptQuery({ cmd: 'search', results: RESULTS, text: null });
  assert.equal(q.off, null);
  assert.equal(q.results.count, '3 matches');
  assert.equal(q.results.docs[1].index, 2);
  assert.deepEqual(q.results.docs[1].fields, [{ name: 'title', value: 'blue hat' }, { name: 'price', value: '30' }]);
  assert.equal(q.text, null);
  const explain = adaptQuery({ cmd: 'explain', index: INDEX.name, query: 'red', results: null, text: { empty: null, lines: ['@title:UNION {', '  @title:red', '  @title:+red(expanded)', '}'] } });
  assert.equal(explain.cmd, 'explain');
  assert.equal(explain.results, null);
  assert.equal(explain.text.lines.length, 4);
  const profile = adaptQuery({ cmd: 'profile', index: INDEX.name, query: 'red', results: { ...RESULTS, cmd: 'profile', run: 2 }, text: { empty: null, lines: ['Shards', '  - Total profile time: 0.739417'] } });
  assert.equal(profile.results.cmd, 'profile');
  assert.equal(profile.results.run, 2);
  assert.equal(profile.text.lines[0], 'Shards');
  const off = adaptQuery({ off: 'The server does not support FT.PROFILE.' });
  assert.equal(off.off, 'The server does not support FT.PROFILE.');
  assert.equal(adaptSearchResults(null), null);
  const f = adaptSearchField({ doc: 2, field: 0, key: { copy: 'p4p:3', id: '7034703a33', name: 'p4p:3' }, name: 'title', run: 1, shown: { bytes: 8, copy: 'blue hat', detected: 'text', format: 'text', formats: ['text', 'hex'], header: 'value · text · 8 bytes · auto-detected', lines: ['blue hat'], truncated: false } });
  assert.equal(f.name, 'title');
  assert.deepEqual(f.shown.lines, ['blue hat']);
  assert.equal(f.shown.copy, 'blue hat');
});

test('search: the create form as typed', () => {
  assert.deepEqual(createIndexRequest({ name: 'p4idx', on: 'HASH', prefixes: 'p4p:', fields: 'title TEXT, price NUMERIC' }), { name: 'p4idx', on: 'HASH', prefixes: 'p4p:', fields: 'title TEXT, price NUMERIC' });
  assert.equal(createIndexRequest({ name: 'x', on: 'json?', fields: 'a TEXT' }).on, 'HASH');
  assert.equal(createIndexRequest({ name: 'x', on: 'JSON', fields: '$.a AS a TEXT' }).prefixes, '');
});

test('hot keys from Monitor: none yet, ranking, ready', () => {
  const none = adaptMonitorHot({ ranking: null, status: 'none', text: 'Or rank keys by how often Monitor sees them: Server › Monitor.' });
  assert.equal(none.status, 'none');
  assert.equal(none.ranking, null);
  assert.equal(adaptMonitorHot({ ranking: null, status: 'pending', text: 'Ranking the keys of the last Monitor run…' }).status, 'pending');
  const ready = adaptMonitorHot({
    status: 'ready', text: "Ranked by how often the last Monitor run named each key, because the analysis has no LFU counters: a short sample, not the server's own counters.",
    ranking: {
      at_unix: 1794764707, empty: null, endpoint: null, keys_note: null, lines: 10, node: 0, secs: 0, title: 'from Monitor, 0 s, 10 lines, at 17:45 UTC', unknown: 0, unknown_text: null,
      rows: [
        { count: 6, key: { copy: 'p4h:a', id: '7034683a61', name: 'p4h:a' }, named: 'p4h:a · named 6 times in 10 lines', pattern: { copy: 'p4h:a', id: '7034683a61', name: 'p4h:a' }, share: 0.6 },
        { count: 1, key: { copy: 'p4h:c', id: '7034683a63', name: 'p4h:c' }, named: 'p4h:c · named once in 10 lines', pattern: { copy: 'p4h:c', id: '7034683a63', name: 'p4h:c' }, share: 0.1 },
      ],
    },
  });
  assert.equal(ready.ranking.title, 'from Monitor, 0 s, 10 lines, at 17:45 UTC');
  assert.equal(ready.ranking.lines, 10);
  assert.equal(ready.ranking.rows[0].count, 6);
  assert.equal(ready.ranking.rows[1].named, 'p4h:c · named once in 10 lines');
  assert.equal(ready.ranking.rows[0].key.id, '7034683a61');
  assert.equal(adaptMonitorHot({ status: 'loud' }).status, 'none');
});

test('phase 4 routes: paths, query strings and methods', () => {
  assert.deepEqual(ROUTES.streams(), ['GET', '/api/streams']);
  assert.deepEqual(ROUTES.streams({ refresh: true }), ['GET', '/api/streams?refresh=1']);
  assert.deepEqual(ROUTES.streams({ now: true }), ['GET', '/api/streams?now=1']);
  assert.deepEqual(ROUTES.streams({ refresh: true, now: true }), ['GET', '/api/streams?refresh=1'], 'a refresh reads now as well');
  assert.deepEqual(ROUTES.streamPending({ key: 'ab', group: 'cd' }), ['GET', '/api/streams/pending?key=ab&group=cd']);
  assert.deepEqual(ROUTES.streamPending({ key: 'ab', group: 'cd', consumer: 'ef' }), ['GET', '/api/streams/pending?key=ab&group=cd&consumer=ef']);
  assert.deepEqual(ROUTES.streamAck(), ['POST', '/api/streams/ack']);
  assert.deepEqual(ROUTES.streamClaim(), ['POST', '/api/streams/claim']);
  assert.deepEqual(ROUTES.streamTrim(), ['POST', '/api/streams/trim']);
  assert.deepEqual(ROUTES.search({ refresh: true }), ['GET', '/api/search?refresh=1']);
  assert.deepEqual(ROUTES.searchQuery(), ['POST', '/api/search/query']);
  assert.deepEqual(ROUTES.searchResults({ run: 3, from: 10 }), ['GET', '/api/search/results?run=3&from=10']);
  assert.deepEqual(ROUTES.searchField({ run: 3, doc: 0, field: 1 }), ['GET', '/api/search/field?run=3&doc=0&field=1']);
  assert.deepEqual(ROUTES.searchCreate(), ['POST', '/api/search/create']);
  assert.deepEqual(ROUTES.searchDrop(), ['POST', '/api/search/drop']);
  assert.deepEqual(ROUTES.hotKeysMonitor(), ['GET', '/api/analysis/hot_keys/monitor']);
  const real = createReal();
  for (const m of ['getStreams', 'getPending', 'ackEntry', 'claimEntries', 'trimStream', 'getSearch', 'runQuery', 'getResults', 'getResultField', 'createIndex', 'dropIndex', 'getHotKeysMonitor']) {
    assert.ok(METHODS.includes(m), `${m} is wrapped with the confirm flow`);
    assert.equal(typeof real[m], 'function', `${m} is implemented`);
  }
});

test('a query is sent as typed for FT.SEARCH, and without one for FT.EXPLAIN and FT.PROFILE', async () => {
  const sent = [];
  const fetchFn = async (url, init) => {
    sent.push({ url, method: init.method, body: init.body ? JSON.parse(init.body) : null });
    return new Response(JSON.stringify({ cmd: 'search', results: RESULTS, text: null }), { status: 200, headers: { 'Content-Type': 'application/json' } });
  };
  const real = createReal({ fetchFn, token: 'b'.repeat(64) });
  await real.runQuery({ index: '7034696478', cmd: 'search', query: '@price:[0 100]' });
  await real.runQuery({ index: '7034696478', cmd: 'explain', query: 'ignored' });
  await real.ackEntry({ key: ORDERS.id, group: BILLING.id, id: '1-0' });
  await real.dropIndex({ index: '7034696478' });
  assert.deepEqual(sent.map((s) => [s.method, s.url]), [['POST', '/api/search/query'], ['POST', '/api/search/query'], ['POST', '/api/streams/ack'], ['POST', '/api/search/drop']]);
  assert.deepEqual(sent[0].body, { index: '7034696478', cmd: 'search', query: '@price:[0 100]' });
  assert.deepEqual(sent[1].body, { index: '7034696478', cmd: 'explain' });
  assert.deepEqual(sent[2].body, { key: ORDERS.id, group: BILLING.id, id: '1-0' });
  assert.deepEqual(sent[3].body, { index: '7034696478' });
});
