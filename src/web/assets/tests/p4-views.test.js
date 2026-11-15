// The pure pieces of phase 4's views: Browse's panels, Streams' lines and the claim form's
// defaults, Search's result lines, pages and why a command is off, and the Hot keys line
// that links Monitor.
import test from 'node:test';
import assert from 'node:assert/strict';
import { PANELS, panelOf } from '../views/browse.js';
import { countText, groupLine, claimDefaults, claimFrom, rereadAfter, pollDue, listSubtitle, entryText, pickRow, APPROXIMATE, POLL_MS } from '../views/streams.js';
import { resultsLine, pageText, pageSteps, commandOff, fieldText, queryStart, MAX_QUERY, COMMANDS } from '../views/search.js';
import { monitorLinkParts } from '../views/analyze.js';
import { adaptStreams } from '../api.js';

test('Browse has the TUI\'s panels Keys · Streams · Search, each at its own address', () => {
  assert.deepEqual(PANELS.map((p) => [p.label, p.href]), [['Keys', '#/browse'], ['Streams', '#/browse/streams'], ['Search', '#/browse/search']]);
  assert.equal(panelOf(null), 'keys');
  assert.equal(panelOf('streams'), 'streams');
  assert.equal(panelOf('search'), 'search');
  assert.equal(panelOf('elsewhere'), 'keys', 'an unknown panel is Keys');
});

const group = (over = {}) => ({
  name: { name: 'billing', id: '62696c6c696e67', copy: 'billing' }, consumers: 2, pending: 8, lastDeliveredId: '1794745501085-0', lag: 18, lagging: false,
  readers: [], claim: { to: { name: 'worker-2', id: '776f726b65722d32', copy: 'worker-2' }, minIdleMs: 60000, count: 100 }, ...over,
});

test('streams: a group in one line, a lag that is not known as a dash, lagging marked', () => {
  assert.equal(groupLine(group()), '2 consumers · 8 pending · lag 18 · last delivered 1794745501085-0');
  assert.equal(groupLine(group({ consumers: 1, lag: null, lastDeliveredId: '' })), '1 consumer · 8 pending · lag —');
  assert.equal(groupLine(group({ lag: 24210, lagging: true })), '2 consumers · 8 pending · lag 24,210 ▲ · last delivered 1794745501085-0');
  assert.equal(countText(null), '—');
  assert.equal(countText(96000), '96,000');
});

test('streams: the claim form as the TUI fills it, and blank for a name that is not typed as it is', () => {
  assert.deepEqual(claimDefaults(group()), { to: 'worker-2', minIdleMs: '60000', count: '100' });
  assert.equal(claimDefaults(group({ claim: { to: { name: 'w\\x1b', id: '771b', copy: null }, minIdleMs: 60000, count: 100 } })).to, '', 'an escaped name is not offered to send');
  assert.equal(claimDefaults(group({ claim: { to: null, minIdleMs: 1, count: 5 } })).to, '');
  assert.equal(APPROXIMATE, 'Approximate (~): the server may keep a few more.');
  assert.equal(POLL_MS, 5000);
});

const reader = (name, idleMs, copy = name) => ({ name: { name, id: Buffer.from(name).toString('hex'), copy }, pending: 1, idleMs });

test('streams: from a consumer\'s or a pending entry\'s row the claim form is the TUI\'s: that consumer left out, its idle time', () => {
  const g = group({ readers: [reader('w1', 90_500), reader('w2', 4_000), reader('w3', 4_000), reader('w4', 1_200)] });
  // From w4, the most recently active: the next most recently active, the first of a tie.
  assert.deepEqual(claimDefaults(g, { name: g.readers[3].name, idleMs: 1_200 }), { to: 'w2', minIdleMs: '1000', count: '100' });
  // From w1 (a consumer row or one of its pending entries): w4, w1's idle in whole seconds.
  assert.deepEqual(claimDefaults(g, { name: g.readers[0].name, idleMs: 90_500 }), { to: 'w4', minIdleMs: '90000', count: '100' });
  // An entry's consumer gone from the readers: every reader may take them.
  assert.equal(claimDefaults(g, { name: { name: 'gone', id: '676f6e65', copy: 'gone' }, idleMs: 0 }).to, 'w4');
  // The only consumer, readers refused, or a name not typed as it is: blank.
  assert.equal(claimDefaults(group({ readers: [reader('w1', 5)] }), { name: reader('w1', 5).name, idleMs: 5 }).to, '');
  assert.equal(claimDefaults(group({ readers: null }), { name: reader('w1', 5).name, idleMs: 5 }).to, '');
  assert.equal(claimDefaults(group({ readers: [reader('w1', 5), reader('w\\x1b', 1, null)] }), { name: reader('w1', 5).name, idleMs: 5 }).to, '');
  assert.equal(claimFrom({ name: { name: 'w1' } }), 'From w1: entries idle at least min idle go to the new consumer.');
  assert.equal(claimFrom(null), 'Entries idle at least min idle go to the new consumer.');
});

test('streams: after a write the pending list shown is read again, else the list at once; the poll stops while the entries are open', () => {
  const shown = { key: '61', group: { name: 'g', id: '67' }, consumer: null };
  assert.equal(rereadAfter(shown, '61'), 'pending');
  assert.equal(rereadAfter(shown, '62'), 'list', 'another stream\'s pending list');
  assert.equal(rereadAfter(null, '61'), 'list');
  const on = { live: true, liveState: 'ready', hidden: false, list: { off: null }, entriesOpen: false };
  assert.equal(pollDue(on), true);
  assert.equal(pollDue({ ...on, entriesOpen: true }), false, 'the entries are open');
  assert.equal(pollDue({ ...on, live: false }), false, 'paused');
  assert.equal(pollDue({ ...on, hidden: true }), false);
  assert.equal(pollDue({ ...on, liveState: 'connecting' }), false);
  assert.equal(pollDue({ ...on, list: null }), false);
  assert.equal(pollDue({ ...on, list: { off: 'x' } }), false);
});

test('streams: the subtitle, an entry\'s fields on a line, and which row is shown', () => {
  const ready = adaptStreams({
    status: 'ready', cluster: true, found: 7, shown: 2,
    rows: [{ key: { name: 'a', id: '61' }, endpoint: '10.0.0.1:6379' }, { key: { name: 'b', id: '62' }, endpoint: '10.0.0.2:6379' }],
  });
  assert.equal(listSubtitle(ready), '2 streams of 7 from the analysis, each read on its node (2 nodes).');
  assert.equal(listSubtitle(adaptStreams({ status: 'ready', found: 1, shown: 1, rows: [{ key: { name: 'a', id: '61' } }] })), '1 stream from the analysis.');
  assert.equal(listSubtitle(adaptStreams({ status: 'no_report', rows: [] })), 'Streams are listed after an analysis.');
  assert.equal(listSubtitle(adaptStreams({ off: 'x' })), 'Streams the analysis found.');
  assert.equal(entryText({ fields: [{ name: 'kind', value: 'paid' }, { name: 'order', value: '770100' }] }), 'kind=paid · order=770100');
  assert.equal(entryText({ fields: null }), '');
  assert.equal(pickRow(ready, '62').key.name, 'b');
  assert.equal(pickRow(ready, 'ff').key.name, 'a', 'a stream gone from the list: the first one');
  assert.equal(pickRow(adaptStreams({ status: 'ready', rows: [] }), null), null);
});

const results = (over = {}) => ({ run: 1, cmd: 'search', query: '*', total: 2420, count: '2,420 matches', took: '2 ms', kept: 50, max: 50, from: 0, page: 10, docs: Array.from({ length: 10 }, (_, i) => ({ index: i })), ...over });

test('search: the results line, the page, and its steps', () => {
  assert.equal(resultsLine(results()), '2,420 matches · 2 ms · the first 50 kept (LIMIT 0 50)');
  assert.equal(resultsLine(results({ total: 3, kept: 3, count: '3 matches', took: '<1 ms' })), '3 matches · <1 ms');
  assert.equal(resultsLine(null), '');
  assert.equal(pageText(results()), '1–10 of 50');
  assert.equal(pageText(results({ from: 40 })), '41–50 of 50');
  assert.equal(pageText(results({ kept: 0, docs: [] })), '');
  assert.deepEqual(pageSteps(results()), { prev: null, next: 10 });
  assert.deepEqual(pageSteps(results({ from: 20 })), { prev: 10, next: 30 });
  assert.deepEqual(pageSteps(results({ from: 40 })), { prev: 30, next: null });
  assert.deepEqual(pageSteps(results({ kept: 3, docs: [{}, {}, {}] })), { prev: null, next: null });
});

test('search: why a command\'s button is off', () => {
  const list = { off: null, cmdOff: { search: null, explain: null, profile: null }, busy: null, lastQuery: null };
  assert.equal(commandOff(list, 'search'), null);
  assert.equal(commandOff(list, 'explain'), 'Run a query first: FT.EXPLAIN uses the query last run.');
  assert.equal(commandOff({ ...list, lastQuery: '*' }, 'profile'), null);
  assert.equal(commandOff({ ...list, lastQuery: '*', cmdOff: { explain: 'The server does not support FT.EXPLAIN.' } }, 'explain'), 'The server does not support FT.EXPLAIN.');
  assert.equal(commandOff({ ...list, busy: 'FT.SEARCH' }, 'search'), 'FT.SEARCH is still running: wait for its answer.');
  assert.equal(commandOff({ off: 'x' }, 'search'), 'The Search panel is off.');
  assert.equal(MAX_QUERY, 1000);
  assert.deepEqual(COMMANDS, { search: 'FT.SEARCH', explain: 'FT.EXPLAIN', profile: 'FT.PROFILE' });
});

test('search: the query box starts from the query last run, when it can be typed as it is', () => {
  assert.equal(queryStart({ off: null, lastQuery: '@title:shoe', lastQueryCopy: '@title:shoe' }), '@title:shoe');
  assert.equal(queryStart({ off: null, lastQuery: 'a\\x1b', lastQueryCopy: null }), '');
  assert.equal(queryStart({ off: null, lastQuery: null, lastQueryCopy: null }), '');
  assert.equal(queryStart({ off: 'x' }), '');
  assert.equal(queryStart(null), '');
});

test('search: a schema field in one line', () => {
  assert.equal(fieldText({ name: 'title', path: null, type: 'TEXT', flags: ['SORTABLE'] }), 'title TEXT SORTABLE');
  assert.equal(fieldText({ name: 'banner', path: '$.banner.text', type: 'TEXT', flags: [] }), '$.banner.text AS banner TEXT');
});

test('hot keys: the line that links Server › Monitor', () => {
  assert.deepEqual(monitorLinkParts('Or rank keys by how often Monitor sees them: Server › Monitor.'), { before: 'Or rank keys by how often Monitor sees them: ', link: 'Server › Monitor', after: '.' });
  assert.deepEqual(monitorLinkParts('Ranking…'), { before: 'Ranking…', link: null, after: '' });
  assert.deepEqual(monitorLinkParts(null), { before: '', link: null, after: '' });
});
