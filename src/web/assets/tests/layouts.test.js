// Screens that use their height (docs/ui-redesign.md §10) and the consistency sweep: the
// models Overview, Slow log, Clients and Nodes are drawn from, the rule that a reply or a
// poll is drawn only while it still matches what is shown, and where each screen's one
// read-only line goes.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import {
  listPick, replyFits, READ_ONLY_TEXT, WRITES_ON_TEXT, FILL_WIDE, writesReason, pollTick, answerSends, placeOf,
  connectStarted, glyphParts, STALE_TEXT, STALE_START_TEXT,
} from '../ui.js';
import { PANE_LABELS } from '../keys.js';
import {
  tilesModel, moreFigures, factsLine, analysisAge, findingsModel, findingBox, rightCards, whereLine, offRows, offRowText, FINDING_GONE,
} from '../views/overview.js';
import { slowModel, slowDetail, slowKeyOpens, clientsModel, groupFacts } from '../views/server.js';
import { nodesModel, nodeFacts, choiceModel, DEPTHS, askKey } from '../views/analyze.js';
import { readOnlyPlace as browsePlace, itemReasons, PANES } from '../views/browse.js';
import { readOnlyPlace as streamsPlace, pickRow as streamPickRow, pickGone as streamPickGone, STREAM_GONE } from '../views/streams.js';
import { readOnlyPlace as searchPlace, INDEX_GONE } from '../views/search.js';
import { readOnlyPlace as pubsubPlace, PUBLISH_OFF } from '../views/pubsub.js';

const read = (rel) => readFileSync(new URL(`../${rel}`, import.meta.url), 'utf8');
const G = 1024 ** 3;

const figures = {
  usedMemory: 15 * G, maxMemory: 24 * G, opsPerSec: 12400, opsNow: 12000, clients: 214, blocked: 0, hitRate: 0.97,
  evictedPerSec: 0.5, expiredPerSec: 5, fragmentation: 1.07, uptimeSecs: 1006, role: 'master', replicas: 1, linkUp: null,
  policy: 'allkeys-lfu', rdbOk: true, aofOn: false, aofOk: null, keys: 7920114, netIn: 2048, netOut: 4096, cpuUser: 0.12, cpuSys: 0.04,
};
const overview = (over = {}) => ({
  server: { kind: 'Redis', version: '7.4.2', topology: 'standalone', policy: 'allkeys-lfu' },
  memory: { used: 15 * G, max: 24 * G },
  keys: { total: 7920114, ttlShare: 0.31, dataBytes: 13 * G, overheadBytes: 2 * G },
  figuresFailed: null, gaps: [], checks: [], provider: null, lastAnalysis: null, figures: null, shards: [],
  findings: [], analysis: null,
  ...over,
});
const series = { ops: [1, 2, 3], clients: [4, 5], everySecs: 2 };

// ---------------------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------------------

test('the six tiles from a figures event: value, one-line sub, the memory meter, bars only where the page keeps a series', () => {
  const t = tilesModel({ f: figures, data: overview(), conn: { db: 0 }, series, live: true });
  assert.deepEqual(t.map((x) => x.id), ['memory', 'ops', 'clients', 'hits', 'evicted', 'frag']);
  const by = Object.fromEntries(t.map((x) => [x.id, x]));
  assert.deepEqual([by.memory.value, by.memory.sub, by.memory.meter], ['15G', 'of 24G · 63%', 63]);
  assert.deepEqual([by.ops.value, by.ops.sub, by.ops.bars], ['12.4K', 'Last 6 seconds', [1, 2, 3]]);
  assert.deepEqual([by.clients.value, by.clients.sub, by.clients.bars], ['214', '0 blocked', [4, 5]]);
  assert.deepEqual([by.hits.value, by.hits.meter], ['97%', 97]);
  assert.equal(by.evicted.value, '0.5 / 5');
  assert.deepEqual([by.frag.value, by.frag.sub], ['1.07', '13G data · 2.0G overhead']);
  for (const id of ['memory', 'hits', 'evicted', 'frag']) assert.equal(by[id].bars, null, `${id}: no series is kept for it, so no bars are made up`);
});

test('the tiles with figures missing: each shows — and no made-up meter', () => {
  const none = tilesModel({});
  for (const x of none) assert.equal(x.value, '—', x.id);
  assert.equal(none[0].sub, '—', 'nothing known: not even "no limit set"');
  assert.equal(none[0].meter, null);
  const partial = tilesModel({ f: { usedMemory: null, maxMemory: null, opsPerSec: null, opsNow: null, clients: null, hitRate: null, evictedPerSec: null, expiredPerSec: 3, fragmentation: null }, data: overview({ memory: { used: null, max: null }, keys: {} }), series });
  const by = Object.fromEntries(partial.map((x) => [x.id, x]));
  assert.equal(by.memory.value, '—');
  assert.equal(by.hits.meter, null);
  assert.equal(by.evicted.value, '— / 3', 'one of the two is known');
  assert.equal(by.frag.sub, 'RSS ÷ used memory');
});

test('the memory tile with no limit set: says so, and has no meter', () => {
  const t = tilesModel({ f: { ...figures, maxMemory: null }, data: overview({ memory: { used: 15 * G, max: null } }) });
  assert.equal(t[0].value, '15G');
  assert.equal(t[0].sub, 'no limit set');
  assert.equal(t[0].meter, null);
  assert.equal(tilesModel({ data: overview({ memory: { used: 2 * G, max: 8 * G } }) })[0].sub, 'of 8.0G · 25%', 'from GET /api/overview before the first figures');
});

test('the facts line and More figures keep every fact the old grid showed', () => {
  assert.equal(factsLine(overview(), figures, {}), 'Redis 7.4.2 · standalone · up 16m 46s · master · 1 replica · RDB ok · AOF off · allkeys-lfu');
  assert.equal(factsLine(null, null, {}), '—');
  assert.equal(factsLine(null, { ...figures, linkUp: false, replicas: null, role: 'slave' }, { server: 'Valkey', version: '8.1' }).split(' · ').slice(0, 3).join(' · '), 'Valkey 8.1 · up 16m 46s · slave');
  const more = Object.fromEntries(moreFigures({ f: figures, data: overview(), conn: { db: 0, via: 'sentinel 10.0.0.1:26379' } }));
  for (const label of ['Keys', 'Data', 'Overhead', 'Network in', 'Network out', 'CPU user', 'CPU system', 'Blocked clients', 'Evicted / sec', 'Expired / sec', 'Sentinel']) assert.ok(label in more, label);
  assert.equal(more['Network in'], '2.0K/s');
  assert.equal(more['With a TTL'], '31% have a TTL · database 0');
  assert.ok(!('Sentinel' in Object.fromEntries(moreFigures({ f: figures, data: overview(), conn: {} }))));
});

test('the analysis age beside Analyze', () => {
  const now = Date.UTC(2026, 10, 17, 15, 0, 0);
  const a = { takenAt: Date.UTC(2026, 10, 17, 14, 2, 0) / 1000, sampledKeys: 100000, findings: [], topPatterns: [] };
  assert.equal(analysisAge(overview({ analysis: a }), { running: false }, now), '100,000 keys sampled today 14:02 UTC');
  assert.equal(analysisAge(overview({ lastAnalysis: { at: Date.UTC(2026, 10, 16, 9, 0, 0), sampledKeys: 10 } }), null, now), 'Last analysis 2026-11-16 09:00 UTC (a saved snapshot)');
  assert.equal(analysisAge(overview(), null, now), 'No analysis yet');
  assert.equal(analysisAge(overview(), { running: true, status: 'sampling', sampled: 10, target: 100 }, now), 'Analyzing: 10 of 100 keys sampled.');
});

const finding = (severity, text, screen = null) => ({ severity, text, label: screen ? 'Server › Slow log' : null, where: screen });

test('things to look at: the server\'s and the analysis\'s findings, worst first, each said once', () => {
  const data = overview({
    findings: [finding('note', 'LRANGE took 912 ms', 'server/slowlog'), finding('warn', '38 connections idle', 'server/clients')],
    analysis: { sampledKeys: 74, findings: [finding('info', 'hot key'), finding('warn', 'big key', 'analyze/big_keys'), finding('note', 'LRANGE took 912 ms', 'server/slowlog')], topPatterns: [] },
  });
  const rows = findingsModel(data);
  assert.deepEqual(rows.map((r) => [r.text, r.from]), [
    ['38 connections idle', 'server'], ['big key', 'analysis'], ['LRANGE took 912 ms', 'server'], ['hot key', 'analysis'],
  ]);
  assert.deepEqual(rows.map((r) => r.warn), [true, true, false, false]);
  assert.equal(rows[0].href, '#/server/clients');
  assert.equal(rows[3].href, null, 'a finding with no screen has no link');
  assert.deepEqual(findingsModel(null), []);
  // Two findings with the same text keep two keys.
  const twice = findingsModel(overview({ findings: [finding('note', 'same'), finding('note', 'same')] }));
  assert.deepEqual(twice.map((r) => r.key), ['same', 'same#2']);
});

test('the picked finding\'s box: only what the server sent for it, and where it leads', () => {
  const data = overview({ findings: [finding('warn', '38 connections idle', 'server/clients')], analysis: { sampledKeys: 74, findings: [finding('note', 'big key')], topPatterns: [] } });
  const [server, sampled] = findingsModel(data);
  assert.deepEqual(findingBox(server, data), {
    title: '38 connections idle', warn: true, source: 'From what the server reports: no key was read for it.',
    link: { href: '#/server/clients', text: 'Look into it on Server › Slow log' },
  });
  assert.equal(findingBox(sampled, data).source, 'From the analysis of 74 keys.');
  assert.equal(findingBox(sampled, data).link, null);
  assert.equal(findingBox(null, data), null);
});

test('the right column before, during and after an analysis', () => {
  const now = Date.UTC(2026, 10, 17, 15, 0, 0);
  const before = rightCards({ analysis: null, run: { running: false }, last: { at: Date.UTC(2026, 10, 17, 9, 0, 0), sampledKeys: 1000 }, now });
  assert.equal(before.memory.state, 'prompt');
  assert.match(before.memory.text, /Analyze reads one, after showing what it costs/);
  assert.equal(before.memory.last, 'Last analysis: today 09:00 UTC, 1,000 keys (a saved snapshot).');
  assert.equal(before.freed, null);
  assert.equal(before.slowest, null, 'the page holds no slow-log entries outside Server › Slow log');

  const during = rightCards({ analysis: null, run: { running: true, status: 'sampling', sampled: 25, target: 100, depth: 'deep' }, now });
  assert.deepEqual([during.memory.state, during.memory.pct, during.memory.depth], ['running', 25, 'Deep analysis']);
  assert.equal(during.freed, null);

  const a = {
    takenAt: now / 1000, sampledKeys: 100, complete: true, stopped: false, sizes: true, note: null, changes: 'none', neverExpiresShare: 0.76,
    topPatterns: [
      { pattern: { name: 'session:{uuid}', id: '01' }, estKeys: 10, estBytes: 6 * G, share: 0.46 },
      { pattern: { name: 'cache:{id}', id: '02' }, estKeys: 5, estBytes: 3 * G, share: 0.23 },
    ],
  };
  const after = rightCards({ analysis: a, run: { running: false }, now });
  assert.equal(after.memory.state, 'done');
  assert.equal(after.memory.title, 'Where the memory is');
  assert.deepEqual(after.memory.rows.map((r) => [r.pattern.name, r.pct, r.share, r.size]), [['session:{uuid}', 100, '46%', '6.0G'], ['cache:{id}', 50, '23%', '3.0G']]);
  assert.deepEqual(after.memory.changes, { text: 'No earlier snapshot to compare with: Analyze › Snapshots saves one.', href: '#/analyze/snapshots' });
  assert.equal(after.freed.rows, null, 'Expiry not read: only the share the analysis holds');
  assert.match(after.freed.line, /^76% of the memory never expires/);

  const freed = { within: [{ label: '1 hour', bytes: G }], never: 3 * G, total: 4 * G };
  const held = rightCards({ analysis: a, run: { running: false }, freed, now });
  assert.deepEqual(held.freed.rows.map((r) => [r.label, r.never, r.share, r.size]), [['within 1 hour', false, '25%', '1.0G'], ['never', true, '75%', '3.0G']]);
  assert.equal(held.freed.total, '4.0G');

  const keysOnly = rightCards({ analysis: { ...a, sizes: false }, run: null, now });
  assert.equal(keysOnly.memory.title, 'Keys by pattern');
  assert.equal(rightCards({ analysis: { ...a, topPatterns: [] }, now }).memory.state, 'empty');
});

test('Overview has one Analyze button, with the key, and asks with the quick and deep choice', () => {
  const src = read('views/overview.js');
  assert.equal((src.match(/'data-act': 'analyze'/g) || []).length, 1);
  assert.doesNotMatch(src, /Analyze 10,000 keys|Analyze 100,000 keys/);
  assert.match(src, /onclick: \(\) => askAnalyze\(ctx\)/);
  assert.doesNotMatch(src, /getSlowlog|getClients|getExpiry/, 'Overview reads nothing of its own for the cards');
  const options = choiceModel({ estimates: [{ depth: 'quick', keys: 10000, sampleLimit: 10000, commands: 30000, sizes: true, freq: true }, { depth: 'deep', keys: 100000, sampleLimit: 100000, commands: 300000, sizes: true, freq: true }] });
  assert.deepEqual(options.map((o) => [o.depth, o.label, o.checked]), [['quick', DEPTHS.quick, true], ['deep', DEPTHS.deep, false]]);
  assert.match(options[0].text, /^A quick analysis samples 10,000 keys/);
  assert.match(options[1].text, /^A deep analysis samples 100,000 keys/);
  assert.equal(choiceModel(null, 'deep')[1].checked, true);
  assert.match(choiceModel(null)[0].text, /never a value/, 'no estimate: the general words');
});

// ---------------------------------------------------------------------------------------
// Lists with their detail beside them
// ---------------------------------------------------------------------------------------

test('listPick: the first row by default, the picked one by its key in any order, and a gone one is never swapped', () => {
  const rows = (...keys) => keys.map((key) => ({ key }));
  assert.deepEqual(listPick(rows('a', 'b'), null), { key: 'a', index: 0, gone: false });
  assert.deepEqual(listPick([], null), { key: null, index: -1, gone: false });
  assert.deepEqual(listPick(rows('a', 'b', 'c'), 'b'), { key: 'b', index: 1, gone: false });
  assert.deepEqual(listPick(rows('c', 'b', 'a'), 'b'), { key: 'b', index: 1, gone: false }, 'a reordered reading keeps it');
  assert.deepEqual(listPick(rows('a', 'c'), 'b'), { key: 'b', index: -1, gone: true });
});

test('a reply or a poll is drawn only while it matches what is shown', () => {
  assert.equal(replyFits({ seq: 3, who: 'n1' }, { seq: 3, who: 'n1' }), true);
  assert.equal(replyFits({ seq: 2, who: 'n1' }, { seq: 3, who: 'n1' }), false, 'a slow poll overtaken by a newer one');
  assert.equal(replyFits({ seq: 3, who: 'n1' }, { seq: 3, who: 'n2' }), false, 'read for another node or session');
  assert.equal(replyFits({ seq: 3, who: 'n1', pick: 'a' }, { seq: 3, who: 'n1', pick: 'b' }), false, 'asked for a row no longer picked');
  assert.equal(replyFits({ seq: 3, who: 'n1', pick: 'a' }, { seq: 3, who: 'n1', pick: 'a' }), true);
  assert.equal(replyFits(null, { seq: 1 }), false);
  // The Server panels and Overview read through it, or a sequence of their own.
  assert.match(read('views/server.js'), /if \(alive && replyFits\(sent, \{ who: who\(\) \}\)\) draw\(data\);/);
  assert.match(read('views/overview.js'), /if \(alive && mine === readSeq\) take\(next\);/);
});

const entry = (id, over = {}) => ({ id, at: Date.UTC(2026, 10, 17, 13, 58, 41), micros: 912000, name: 'LRANGE', command: 'LRANGE queue:emails 0 -1', keys: ['queue:emails'], moreKeys: 0, client: '10.0.4.17:51122', clientName: 'worker', argsCut: 0, note: null, ...over });

test('Slow log: the list and the entry picked, kept across a poll that adds entries', () => {
  const first = { entries: [entry(41, { note: 'Reads a whole list.' }), entry(40), entry(39)] };
  const m = slowModel(first, null);
  assert.deepEqual(m.rows.map((r) => [r.key, r.flag]), [['s:41', true], ['s:40', false], ['s:39', false]]);
  assert.equal(m.key, 's:41');
  const picked = slowModel(first, 's:40');
  assert.equal(picked.sel.id, 40);
  const polled = slowModel({ entries: [entry(42), entry(41), entry(40), entry(39)] }, 's:40');
  assert.equal(polled.sel.id, 40, 'a newer entry on top does not move the pick');
  const rolled = slowModel({ entries: [entry(43), entry(42)] }, 's:40');
  assert.deepEqual([rolled.sel, rolled.gone, rolled.key], [null, true, 's:40'], 'an entry that rolled out is said, not replaced');
  assert.deepEqual(slowModel({ off: 'refused' }, null).rows, []);
  assert.deepEqual(slowDetail(entry(41, { argsCut: 3 })), [
    ['When', '2026-11-17 13:58:41 UTC'], ['Took', '912 ms'], ['Command', 'LRANGE'],
    ['Arguments', 'LRANGE queue:emails 0 -1 … 3 more arguments'], ['Client', '10.0.4.17:51122 (worker)'],
  ]);
  assert.equal(slowKeyOpens('queue:emails'), true);
  assert.equal(slowKeyOpens('bin\\x00key'), false, 'a key shown with escapes is not the text it shows');
  assert.equal(slowKeyOpens(''), false);
});

const group = (by, name, members, over = {}) => ({ by, name, connections: members.length, oldestSecs: 74, idleSecs: 0, idleConns: 0, lastCommand: 'get', memory: 23000, flagged: false, members, ...over });
const conns = [{ addr: '10.0.4.1:1' }, { addr: '10.0.4.2:2' }, { addr: '10.0.9.17:3' }, { addr: '127.0.0.1:4', own: true }];

test('Clients: two headed sections of one list, the picked group\'s members, kept across a poll that reorders or removes it', () => {
  const data = { total: 4, connections: conns, groups: [group('address', '10.0.4.0/24', [0, 1]), group('library', 'redis-py', [0, 2]), group('address', '127.0.0.0/24', [3])] };
  const m = clientsModel(data, null);
  assert.deepEqual(m.sections.map((s) => [s.title, s.rows.map((r) => r.key), s.most]), [
    ['By library', ['library:redis-py'], 2], ['By address', ['address:10.0.4.0/24', 'address:127.0.0.0/24'], 2],
  ]);
  assert.equal(m.key, 'library:redis-py', 'the first row of the first section');
  assert.deepEqual(m.members.map((c) => c.addr), ['10.0.4.1:1', '10.0.9.17:3']);
  const picked = clientsModel(data, 'address:127.0.0.0/24');
  assert.deepEqual(picked.members.map((c) => c.addr), ['127.0.0.1:4']);
  // The next poll lists the groups in another order and the members at other places.
  const reordered = { total: 4, connections: [conns[3], conns[0], conns[1], conns[2]], groups: [group('address', '127.0.0.0/24', [0]), group('library', 'redis-py', [1, 3]), group('address', '10.0.4.0/24', [1, 2])] };
  const kept = clientsModel(reordered, 'address:127.0.0.0/24');
  assert.equal(kept.key, 'address:127.0.0.0/24');
  assert.deepEqual(kept.members.map((c) => c.addr), ['127.0.0.1:4'], 'the members come from the same reading as the group');
  // A poll without the group: it stays picked and is said to be gone.
  const removed = clientsModel({ total: 2, connections: conns, groups: [group('library', 'redis-py', [0, 2])] }, 'address:127.0.0.0/24');
  assert.deepEqual([removed.sel, removed.gone, removed.name, removed.members], [null, true, '127.0.0.0/24', []]);
  assert.deepEqual(clientsModel({ off: 'refused' }, 'x').sections, []);
  assert.equal(groupFacts(group('address', 'x', [], { oldestSecs: 3700, idleSecs: 61, memory: null, lastCommand: null })), 'oldest 1h 01m · longest idle 1m 01s');
});

test('Nodes: the shards and the picked node, kept by its index', () => {
  const d = { rows: [
    { index: 0, endpoint: '10.0.1.11:6379', role: 'primary', version: '7.4.2', status: 'scanned', keys: 100, memory: G, sampledKeys: 10, patterns: 4, slots: '0-5460', slotCount: 5461, note: null },
    { index: 1, endpoint: '10.0.1.12:6379', role: 'primary', version: null, status: 'partial', keys: null, memory: null, sampledKeys: 5, patterns: null, slots: '5461-10922', slotCount: 5462, note: 'Its sample stopped at the command budget.' },
  ] };
  const m = nodesModel(d, null);
  assert.deepEqual(m.rows.map((r) => r.key), ['n:0', 'n:1']);
  assert.equal(m.sel.endpoint, '10.0.1.11:6379');
  assert.equal(nodesModel({ rows: [...d.rows].reverse() }, 'n:1').sel.endpoint, '10.0.1.12:6379');
  assert.equal(nodesModel({ rows: [d.rows[0]] }, 'n:1').gone, true);
  assert.deepEqual(Object.fromEntries(nodeFacts(d.rows[1])), {
    Role: 'primary', Version: '—', Slots: '5461-10922', 'Slot count': '5,462', State: '▲ partial', Keys: '—', Memory: '—', 'Sampled keys': '5', Patterns: '—',
  });
});

test('the list-and-detail screens read nothing when a row is picked, and Monitor\'s start still goes through the server\'s question', () => {
  const server = read('views/server.js');
  const pickBodies = server.split('ui.follow(').slice(1).map((s) => s.slice(0, s.indexOf('});')));
  assert.equal(pickBodies.length, 2);
  for (const body of pickBodies) assert.doesNotMatch(body, /ctx\.api/);
  assert.doesNotMatch(server, /confirmDialog/);
  assert.match(server, /await ctx\.api\.startMonitor\(\)/);
  const analyze = read('views/analyze.js');
  const nodes = analyze.slice(analyze.indexOf('function drawNodes'), analyze.indexOf('function drawSlots'));
  assert.doesNotMatch(nodes, /ctx\.api/);
  assert.equal(FILL_WIDE, '(min-width: 1100px)');
  assert.match(read('app.css'), /@media \(min-width: 1100px\) \{\n {2}\.lv \{ flex-wrap: nowrap; \}/, 'side by side from the same width');
});

// ---------------------------------------------------------------------------------------
// The consistency sweep
// ---------------------------------------------------------------------------------------

test('each screen\'s one read-only line has one place', () => {
  assert.equal(browsePlace({ valueOpen: true, shown: PANES }), 'value');
  assert.equal(browsePlace({ valueOpen: false, shown: PANES }), 'keys');
  assert.equal(streamsPlace({ shown: true }), 'stream');
  assert.equal(streamsPlace({ shown: false }), 'streams');
  assert.equal(streamsPlace(), 'streams');
  assert.equal(searchPlace({ shown: true }), 'index');
  assert.equal(searchPlace({ shown: false }), 'indexes');
  assert.equal(pubsubPlace(), 'tail');
  assert.equal(PUBLISH_OFF, 'Publishing is off: this connection is read-only.');
  assert.equal(READ_ONLY_TEXT, 'Edits are off: this connection is read-only.');
  assert.equal(WRITES_ON_TEXT, 'Turn writes on (w)');
  // Each draws it with the button that turns writes on, as Browse does, and nowhere else;
  // Streams and Search with the server's own reason (it can say why the profile is
  // read-only), without the sentence that sends the user to Connections.
  for (const file of ['views/streams.js', 'views/search.js']) {
    const src = read(file);
    assert.match(src, /c && c\.readOnly \? writesOffLine\(ctx, writesReason\(off\)\) : notice\(off, 'info'\)/, file);
    assert.doesNotMatch(src, /readOnlyNote\(/, `${file}: not the server's sentence in a second place`);
  }
  assert.equal(writesReason('Not sent: prod is read-only. Turn writes on in Connections.'), 'Not sent: prod is read-only.');
  assert.equal(writesReason('prod is read-only: the profile says so (read_only = true).'), 'prod is read-only: the profile says so (read_only = true).');
  assert.equal(writesReason(null), READ_ONLY_TEXT);
  assert.equal(writesReason('Turn writes on in Connections.'), READ_ONLY_TEXT);
  assert.match(read('views/pubsub.js'), /writesOffLine\(ctx, PUBLISH_OFF\)/);
  assert.match(read('ui.js'), /text: WRITES_ON_TEXT, onclick: \(\) => ctx\.toggleWrites\(\)/);
  // The command line's button names no key: `w` does nothing while the focus is in it.
  assert.match(read('views/cmdline.js'), /text: 'Turn writes on', hidden: true[^\n]*ctx\.toggleWrites\(\)/);
  assert.doesNotMatch(read('views/cmdline.js'), /Turn writes on \(w\)/);
});

test('no screen still says "in Connections" as the way to turn writes on', () => {
  const files = ['app.js', 'ui.js', 'keys.js', 'index.html', ...readdirSync(new URL('../views/', import.meta.url)).map((f) => `views/${f}`)];
  // ui.js's `writesReason` names the sentence only to take it off the server's reason.
  const shown = (file) => {
    const src = read(file);
    if (file !== 'ui.js') return src;
    const at = src.indexOf('export function writesReason');
    return src.slice(0, at) + src.slice(src.indexOf('\n}\n', at));
  };
  for (const file of files) assert.doesNotMatch(shown(file), /[Tt]urn writes on in Connections/, file);
});

test('a reason is shown once in a pane: the selected item\'s bar leaves out what the value\'s notes say', () => {
  const ttl = 'Field TTLs need Redis 7.4 or later: Valkey 8.1 has none.';
  assert.deepEqual(itemReasons(['Read-only', ttl, null], [ttl]), ['Read-only', null, null]);
  assert.deepEqual(itemReasons([ttl], [null]), [ttl], 'not in the notes: it stays under the bar');
  assert.match(read('views/browse.js'), /whyLines\(itemReasons\(buttons\.map\(\(b\) => \(b\.disabled \? b\.title : null\)\), \[v\.fieldTtlsOff, \.\.\.v\.refused\]\)\)/);
});

test('primary actions hidden in a closed form get a visible button with their key in the pane head', () => {
  const streams = read('views/streams.js');
  assert.match(streams, /text: 'Claim…', 'data-act': 'stream\.claim'/);
  assert.match(streams, /class: 'btn danger', text: 'Trim…', 'data-act': 'stream\.trim'/);
  assert.doesNotMatch(streams, /h\('summary', \{[^\n]*'data-act'/, 'the form\'s summary is not a second control for the key');
  assert.match(streams, /class: 'btn danger', text: 'Trim' \}/, 'the form itself is unchanged');
  const search = read('views/search.js');
  assert.match(search, /text: 'New index…', 'data-act': 'search\.create'/);
  assert.doesNotMatch(search, /h\('summary', \{[^\n]*'data-act'/);
  // Search's formats are pills with the key once, on their label, as Browse's "Show as".
  assert.match(search, /h\('span', \{ class: 'lbl hint-label', 'data-hint-of': 'format', text: 'Show as' \}\)/);
  assert.doesNotMatch(search, /h\('select', \{ class: 'field', id, 'data-act': 'format'/);
});

test('long explanations sit behind "About this"; warnings are drawn in the warning colour', () => {
  assert.match(read('views/pubsub.js'), /aboutThis\('Messages are shown, never saved\.'\)/);
  assert.match(read('views/pubsub.js'), /aboutThis\('Counts read no payloads\.'\)/);
  assert.doesNotMatch(read('views/pubsub.js'), /h\('p', \{ class: 'muted small', text: 'Messages are shown, never saved\.' \}\)/);
  assert.match(read('views/server.js'), /aboutThis\('Arguments are shown with credentials masked, on screen only\.'\)/);
  const analyze = read('views/analyze.js');
  assert.doesNotMatch(analyze, /`\$\{r\.big \? '▲ ' : ''\}/, 'the ▲ is not plain text in the figure');
  assert.match(analyze, /h\('span', \{ class: 'glyph', 'aria-hidden': 'true', text: '▲ ' \}\)/);
  assert.match(analyze, /const patternButton = [^\n]*patternName\(p\.name, \{ cls: '' \}\)/, 'placeholders in their own colour');
});
