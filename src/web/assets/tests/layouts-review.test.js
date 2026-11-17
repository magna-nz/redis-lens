// The rules the layouts review asked for, each as a pure function: what Enter does in the
// ask to analyze, whether a dialog's answer may still send, what a list shows when the row
// picked is gone, and whether a poll tick reads. Then where the views use them.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import {
  pollTick, replyFits, answerSends, placeOf, connectStarted, glyphParts, GLYPH_ROLES, STALE_TEXT, STALE_START_TEXT, listPick,
} from '../ui.js';
import { PANE_LABELS } from '../keys.js';
import { askKey, nodeFacts } from '../views/analyze.js';
import { tilesModel, whereLine, offRows, offRowText, offLines, moreFigures, FINDING_GONE } from '../views/overview.js';
import { pickRow, pickGone, STREAM_GONE } from '../views/streams.js';
import { INDEX_GONE } from '../views/search.js';
import { adaptStreams } from '../api.js';

const read = (rel) => readFileSync(new URL(`../${rel}`, import.meta.url), 'utf8');

// ---------------------------------------------------------------------------------------
// 1. Enter in the ask to analyze
// ---------------------------------------------------------------------------------------

test('the ask to analyze: Enter starts only on the Analyze button, cancels on Cancel, and does nothing on a choice', () => {
  assert.equal(askKey('analyze', 'Enter'), 'start');
  assert.equal(askKey('cancel', 'Enter'), 'cancel');
  assert.equal(askKey('choice', 'Enter'), 'none', 'Enter on Quick or Deep never submits');
  assert.equal(askKey(null, 'Enter'), 'none');
  assert.equal(askKey('analyze', ' '), 'none', 'Space is the browser\'s: a button clicks itself');
  assert.equal(askKey('analyze', 'a'), 'none');
});

test('the ask opens with the focus on Cancel, Quick picked, and no form to submit', () => {
  const src = read('views/analyze.js');
  const ask = src.slice(src.indexOf('export async function askAnalyze'), src.indexOf('// The view'));
  assert.doesNotMatch(ask, /h\('form'/, 'no form: Enter on a radio has nothing to submit');
  assert.doesNotMatch(ask, /type: 'submit'/);
  assert.match(ask, /text: 'Cancel', 'data-autofocus': true/);
  assert.doesNotMatch(ask, /'data-autofocus': o\.checked/, 'the focus is not on a choice');
  assert.match(ask, /askKey\(/);
  assert.match(ask, /checked: o\.checked/, 'the choice keeps choiceModel\'s pick (Quick unless asked otherwise)');
});

// ---------------------------------------------------------------------------------------
// 2. A dialog opened for one session never sends to another
// ---------------------------------------------------------------------------------------

const st = ({ name = 'demo', docker = false, db = 0, connected = true, at = 100 } = {}) => ({
  conn: { name, docker, connectedAt: at }, connected, analysis: { db, run: 1, status: 'none', running: false },
});

test('a dialog\'s answer sends only on the session it was opened for', () => {
  const opened = placeOf(st(), 1);
  assert.equal(answerSends(opened, placeOf(st(), 1)), true);
  assert.equal(answerSends(opened, placeOf(st({ name: 'other' }), 1)), false, 'another connection');
  assert.equal(answerSends(opened, placeOf(st(), 2)), false, 'a reconnect (another connect of this page)');
  assert.equal(answerSends(opened, placeOf(st({ db: 3 }), 1)), false, 'another database');
  assert.equal(answerSends(opened, placeOf({ conn: null }, 1)), false, 'disconnected');
  assert.equal(answerSends(null, null), false, 'opened with no connection: nothing to send to');
  assert.equal(connectStarted(st({ at: 100 }), st({ at: 200 })), true, 'a connect at another time counts');
  assert.equal(STALE_TEXT, 'The connection changed: nothing was sent.');
  assert.equal(STALE_START_TEXT, 'The connection changed: nothing was started.');
});

test('the dialogs that send when answered are marked with the session', () => {
  const ui = read('ui.js');
  // openDialog: the mark at open, closing by itself on a change, and no answer after one.
  assert.match(ui, /const mark = session \? sessionMark\(\) : null;/);
  assert.match(ui, /if \(!answerSends\(mark, now\) && dlg\.open\) \{\n\s+stale = true;\n\s+close\(null\);/);
  assert.match(ui, /if \(session && \(stale \|\| \(result !== null && !holds\(\)\)\)\) \{\n\s+result = null;/);
  assert.match(ui, /export function confirmDialog\(confirm, \{ session = false, onStale = null \} = \{\}\)/);
  // Each one that sends.
  const analyze = read('views/analyze.js');
  assert.equal((analyze.match(/onStale: \(\) => ctx\.notify\(STALE_START_TEXT, 'info'\)/g) || []).length, 2, 'askAnalyze and the head\'s ask');
  assert.match(analyze, /title: 'Analyze the keys\?',\n\s+\/\/[^\n]*\n\s+\/\/[^\n]*\n\s+session: true,/);
  const browse = read('views/browse.js');
  const write = browse.slice(browse.indexOf('function writeDialog'), browse.indexOf('function learnRefusal'));
  assert.match(write, /session: true,/);
  assert.match(write, /if \(!holds\(\)\) \{\n\s+close\(null\);\n\s+ctx\.notify\(STALE_TEXT, 'info'\);\n\s+return;/, 'checked again before each send');
  assert.match(write, /if \(alive && same\) done\(result\);/);
  assert.match(read('app.js'), /api\.setConfirmHandler\(\(confirm\) => confirmDialog\(confirm, \{ session: sessionMark\(\) !== null, onStale:/);
  // Browse's history marks and the dialogs count the same connects.
  assert.match(browse, /noteState\(s\);\n\s+connects = connectCount\(\);/);
  // The inline forms (Streams' claim and trim, Search's create) go at once on another session.
  assert.match(read('views/streams.js'), /if \(store\.who !== before\) draw\(\);/);
  assert.match(read('views/search.js'), /if \(store\.who !== before\) \{\n(\s+\/\/[^\n]*\n)+\s+draw\(\);\n\s+load\(\);/);
});

// ---------------------------------------------------------------------------------------
// 3. The row picked is gone: said, never replaced
// ---------------------------------------------------------------------------------------

const streams = adaptStreams({ status: 'ready', rows: [{ key: { name: 'a', id: '61' } }, { key: { name: 'b', id: '62' } }] });

test('Streams: a stream that left the list is said, and no other stream is shown or open', () => {
  assert.equal(pickRow(streams, null).key.name, 'a', 'nothing picked yet: the first');
  assert.equal(pickRow(streams, '62').key.name, 'b');
  assert.equal(pickRow(streams, 'ff'), null);
  assert.equal(pickGone(streams, 'ff'), true);
  assert.equal(pickGone(streams, '62'), false);
  assert.equal(pickGone(streams, null), false);
  assert.match(STREAM_GONE, /no longer in the list: its forms are closed/);
  const src = read('views/streams.js');
  assert.match(src, /if \(gone && \(store\.pending \|\| store\.entries \|\| store\.claim\)\) \{/, 'its pending list, entries and claim form close');
  assert.match(src, /row \? detailCard\(row\) : gone \? goneCard\(\) : null/);
});

test('Search and Overview: a gone index or finding is said, and nothing takes its place', () => {
  const search = read('views/search.js');
  assert.match(search, /const at = listPick\(l\.indexes\.map\(\(i\) => \(\{ key: i\.name\.id \}\)\), store\.picked\);/);
  assert.match(search, /picked \? detailCard\(l, picked\) : at\.gone \? goneCard\(\) : null/);
  assert.doesNotMatch(search, /\|\| l\.indexes\[0\]/, 'no fallback to the first index');
  assert.match(INDEX_GONE, /no longer listed/);
  const overview = read('views/overview.js');
  assert.match(overview, /const at = listPick\(rowsNow, picked\);/);
  assert.doesNotMatch(overview, /picked = rowsNow\.length \? rowsNow\[0\]\.key : null/);
  assert.match(overview, /if \(pickedGone\) return;/, 'the list\'s cursor does not pick another finding by itself');
  assert.match(FINDING_GONE, /no longer reported/);
  // The rule they share.
  assert.deepEqual(listPick([{ key: 'x' }], 'gone'), { key: 'gone', index: -1, gone: true });
});

// ---------------------------------------------------------------------------------------
// 13. One poll at a time
// ---------------------------------------------------------------------------------------

test('a poll tick reads only when no read is out; a slow reply is still drawn', () => {
  assert.equal(pollTick({ first: true }), 'read');
  assert.equal(pollTick({ inFlight: true, first: true }), 'skip');
  assert.equal(pollTick({ inFlight: true }), 'skip', 'the previous read is still out: skip, do not drop it');
  assert.equal(pollTick({}), 'read');
  assert.equal(pollTick({ live: false }), 'skip', 'paused');
  assert.equal(pollTick({ hidden: true }), 'skip', 'the tab is hidden');
  // No sequence: a reply is dropped only for another connection, node or panel.
  assert.equal(replyFits({ who: 'a|n1|slowlog' }, { who: 'a|n1|slowlog' }), true);
  assert.equal(replyFits({ who: 'a|n1|slowlog' }, { who: 'a|n2|slowlog' }), false);
  const server = read('views/server.js');
  assert.match(server, /if \(pollTick\(\{ inFlight, first, live: ctx\.state\(\)\.live, hidden: document\.hidden \}\) !== 'read'\) return;/);
  assert.match(server, /\} finally \{\n\s+inFlight = false;/);
  assert.doesNotMatch(server, /seq \+= 1/);
  assert.match(server, /return c \? `\$\{c\.name\}\|\$\{c\.docker\}\|\$\{c\.connectedAt\}\|\$\{c\.pickedNode\}\|\$\{tab\}` : null;/);
});

// ---------------------------------------------------------------------------------------
// The other fixes
// ---------------------------------------------------------------------------------------

test('Nodes: the same panes and fill as Slow log; its marks in their colour roles', () => {
  const analyze = read('views/analyze.js');
  assert.match(analyze, /paneCard\(\{ id: 'node', label: 'The node', cls: 'lv-detail' \}\)/);
  assert.match(analyze, /paneCard\(\{ id: 'nodes', [^\n]*cls: 'lv-list' \}\)/);
  assert.match(analyze, /const unfill = tab === 'nodes' \? fillWhile\(\) : \(\) => \{\};/);
  assert.match(analyze, /view\.classList\.add\('lv-view'\);\n\s+body\.classList\.add\('lv-area'\);/);
  assert.match(analyze, /cell: \(r\) => glyphText\(NODE_STATE\[r\.status\] \|\| DASH\)/);
  assert.match(analyze, /h\('dd', \{ class: 'mono' \}, glyphText\(v\)\)/);
  assert.match(analyze, /glyphText\(`▲ hot and big/);
  assert.match(read('views/search.js'), /glyphText\(`▲ \$\{i\.progressText \|\| 'building'\}`\)/);
  assert.deepEqual(glyphParts('▲ partial'), { glyph: '▲', cls: 'glyph', rest: 'partial' });
  assert.deepEqual(glyphParts('✗ failed'), { glyph: '✗', cls: 'glyph bad', rest: 'failed' });
  assert.deepEqual(glyphParts('✓ analyzed'), { glyph: '✓', cls: 'glyph good', rest: 'analyzed' });
  assert.deepEqual(glyphParts('plain'), { glyph: null, cls: null, rest: 'plain' });
  assert.equal(GLYPH_ROLES['▲'], 'glyph');
  assert.equal(Object.fromEntries(nodeFacts({ status: 'failed' })).State, '✗ failed');
});

test('Overview\'s head says where the server is again, and the memory card links to Analyze', () => {
  assert.equal(whereLine({ address: '10.0.0.5:6379', tls: true, target: 'the primary' }), '10.0.0.5:6379 · TLS · reading the primary');
  assert.equal(whereLine({ address: '10.0.0.5:6379', tls: false, target: null }), '10.0.0.5:6379');
  assert.equal(whereLine(null), '');
  const more = Object.fromEntries(moreFigures({ conn: { address: 'h:1', tls: true } }));
  assert.equal(more['Server at'], 'h:1 · TLS', 'still reachable when a narrow window hides it from the head');
  const src = read('views/overview.js');
  assert.match(src, /factsWhere\.textContent = where \? ` · \$\{where\}` : '';/);
  assert.match(src, /h\('a', \{ href: '#\/analyze', text: 'Every panel of the analysis is on Analyze\.' \}\)/);
  assert.match(read('app.css'), /@media \(max-width: 1280px\) \{ \.ov-facts-where \{ display: none; \} \}/);
});

test('Turned off, and why: only an off reason says "Off:"', () => {
  const data = { figuresFailed: null, gaps: ['no INFO keysizes'], checks: [{ name: 'SLOWLOG', status: 'denied', why: null }], provider: { notice: 'A managed service: some commands are renamed.' } };
  const rows = offRows(data, { providerNotice: null, refused: ['CONFIG'] });
  assert.deepEqual(rows.map((r) => [r.kind, offRowText(r)]), [
    ['off', 'Off: no INFO keysizes'],
    ['off', 'Off: SLOWLOG: refused for this user'],
    ['notice', 'A managed service: some commands are renamed.'],
    ['refused', 'Refused by the server this session: CONFIG.'],
  ]);
  assert.deepEqual(offLines(data, { providerNotice: null, refused: [] }), ['no INFO keysizes', 'SLOWLOG: refused for this user', 'A managed service: some commands are renamed.']);
  assert.doesNotMatch(read('views/overview.js'), /text: `Off: \$\{l\}`/);
});

test('the Clients tile shows the blocked count and that the figures are paused', () => {
  const f = { clients: 214, blocked: 3 };
  assert.equal(tilesModel({ f, live: true })[2].sub, '3 blocked');
  assert.equal(tilesModel({ f, live: false })[2].sub, '3 blocked · paused');
  assert.equal(tilesModel({ f: { clients: 2, blocked: null }, live: false })[2].sub, 'Live figures paused');
});

test('stale lines go with a failed read; the connections table keys its rows; the new panes have names', () => {
  const server = read('views/server.js');
  const slow = server.slice(server.indexOf('function slowPanel'), server.indexOf('// ---------- Monitor'));
  assert.match(slow, /last = null;\n\s+sideNote\.textContent = '';/, 'the settings line goes with a failed read');
  assert.match(server, /cls: 'conns', rowKey: \(c\) => \(c\.id !== null \? `c:\$\{c\.id\}` : `a:\$\{c\.addr\}`\)/);
  const overview = read('views/overview.js');
  assert.match(overview, /rowsNow = \[\];\n\s+fill\(findList\);\n\s+fill\(findBox\);/, 'the picked finding\'s box goes with a failed read');
  for (const id of ['conns', 'entry', 'node', 'nodes']) assert.ok(PANE_LABELS[id], id);
});
