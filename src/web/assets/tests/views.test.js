// The pure pieces of the views: Overview's lines, the Connections card, the
// slow log's wording, and the Monitor run's bookkeeping.
import test from 'node:test';
import assert from 'node:assert/strict';
import { spanText, memorySub, keysSub, persistenceText, analysisWhen, offLines } from '../views/overview.js';
import { monitorNew, monitorBegin, monitorApply, monitorLeft, monitorFollow, slowlogSettingsText, slowEntryFacts } from '../views/server.js';
import { profileStatus, accessText, tagChanges } from '../views/connections.js';

test('a live tile says how far back its bars go', () => {
  assert.equal(spanText(40, 1, true), 'Last 40 seconds');
  assert.equal(spanText(40, 2, true), 'Last 80 seconds');
  assert.equal(spanText(3, 2, true), 'Last 6 seconds');
  assert.equal(spanText(0, 2, true), 'Live');
  assert.equal(spanText(40, null, true), 'Live', 'no made-up span when the interval is unknown');
  assert.equal(spanText(40, 2, false), 'Live figures paused');
});

test('the memory tile names the limit and the policy, or says there is no limit', () => {
  assert.equal(memorySub(24 * 1024 ** 3, 'allkeys-lfu'), 'of 24G · allkeys-lfu');
  assert.equal(memorySub(null, 'noeviction'), 'no limit set · noeviction');
  assert.equal(memorySub(null, null), 'no limit set');
});

test('the keys tile leaves out a part whose figure is missing', () => {
  assert.equal(keysSub({ ttlShare: 0.31 }, 0), '31% have a TTL · database 0');
  assert.equal(keysSub({ ttlShare: null }, 0), 'database 0');
  assert.equal(keysSub({ ttlShare: null }, null), '—');
});

test('persistence in words, with a failed save or write marked', () => {
  assert.equal(persistenceText({ rdbOk: true, aofOn: false, aofOk: true }), 'RDB ok · AOF off');
  assert.equal(persistenceText({ rdbOk: true, aofOn: true, aofOk: true }), 'RDB ok · AOF ok');
  assert.equal(persistenceText({ rdbOk: false, aofOn: true, aofOk: false }), '✗ RDB save failed · ✗ AOF write failed');
  assert.equal(persistenceText({ rdbOk: null, aofOn: null, aofOk: null }), null);
  assert.equal(persistenceText(null), null);
});

test('the last analysis: today, or the date on another day', () => {
  const now = Date.UTC(2026, 10, 15, 14, 0, 0);
  assert.equal(analysisWhen(Date.UTC(2026, 10, 15, 9, 12, 40), now), 'today 09:12 UTC');
  assert.equal(analysisWhen(Date.UTC(2026, 10, 12, 23, 5, 0), now), '2026-11-12 23:05 UTC');
});

test('"Turned off, and why": the gaps, each refused check, the provider notice; no line twice', () => {
  const data = {
    figuresFailed: null,
    gaps: ['MEMORY STATS is denied for this user (ACL +memory|stats), so dataset.bytes is missing'],
    checks: [
      { name: 'SCAN', status: 'allowed', why: null },
      { name: 'MEMORY STATS', status: 'denied', why: 'NOPERM MEMORY STATS is not allowed for this user' },
      { name: 'FT._LIST', status: 'skipped', why: 'the Search module is not loaded' },
    ],
    provider: { label: 'Azure', notice: 'Azure Managed Redis and Enterprise tiers don\'t support MEMORY STATS, so dataset.bytes may be missing.' },
  };
  const conn = { providerNotice: data.provider.notice, refused: ['SET', 'DEL'] };
  assert.deepEqual(offLines(data, conn), [
    'MEMORY STATS is denied for this user (ACL +memory|stats), so dataset.bytes is missing',
    'MEMORY STATS: NOPERM MEMORY STATS is not allowed for this user',
    data.provider.notice,
    'Refused by the server this session: SET, DEL.',
  ]);
  assert.deepEqual(offLines({ figuresFailed: 'INFO or DBSIZE could not be read', gaps: [], checks: [], provider: null }, { providerNotice: null, refused: [] }), ['INFO or DBSIZE could not be read']);
  assert.deepEqual(offLines(null, null), []);
});

// ---- Connections ----

const profile = { name: 'demo', docker: false, env: 'LOCAL', readOnly: false };
const base = { connecting: null, failed: null, connected: false, lost: null, conn: null };

test('a card says what the server\'s state says about its connection', () => {
  assert.equal(profileStatus(profile, base).status, 'not-open');
  const conn = { name: 'demo', docker: false };
  assert.equal(profileStatus(profile, { ...base, conn, connecting: { name: 'demo' } }).status, 'connecting');
  assert.deepEqual(profileStatus(profile, { ...base, conn, failed: 'Connection refused (os error 61)' }), { status: 'failed', text: 'Connection refused (os error 61)' });
  assert.equal(profileStatus(profile, { ...base, conn, connected: true }).status, 'connected');
  assert.equal(profileStatus(profile, { ...base, conn, connected: true }, true).status, 'limited');
  assert.deepEqual(profileStatus(profile, { ...base, conn, connected: true, lost: 'The live connection is closed.' }), { status: 'failed', text: 'The live connection is closed.' });
  assert.equal(profileStatus(profile, { ...base, conn: { name: 'other', docker: false }, connected: true }).status, 'not-open');
  assert.equal(profileStatus(profile, { ...base, conn: { name: 'demo', docker: true }, connected: true }).status, 'not-open', 'a container of the same name is another thing');
});

test('why writes are on or off, in words', () => {
  assert.equal(accessText({ readOnly: true, env: 'PROD' }), 'Read-only, the default for production');
  assert.equal(accessText({ readOnly: true, env: 'STAGE' }), 'Read-only');
  assert.equal(accessText({ readOnly: false, env: 'PROD' }), 'Writes on (each one asks first)');
  assert.equal(accessText({ readOnly: false, env: 'LOCAL' }), 'Writes on (the default outside production)');
  assert.equal(accessText({ readOnly: false, env: 'LOCAL' }, 2), 'Writes on (2 refused by the server)');
});

test('tags to add and to remove', () => {
  assert.deepEqual(tagChanges(['cache', 'eu'], ['cache', 'team-a']), { addTags: ['team-a'], removeTags: ['eu'] });
  assert.deepEqual(tagChanges(['a'], ['a']), { addTags: [], removeTags: [] });
  assert.deepEqual(tagChanges([], ['x', 'y']), { addTags: ['x', 'y'], removeTags: [] });
});

// ---- Slow log ----

test('what the slow log keeps, in words', () => {
  assert.equal(slowlogSettingsText({ thresholdMicros: 10000, maxLen: 128 }, null), 'Commands slower than 10 ms are logged. The last 128 are kept.');
  assert.equal(slowlogSettingsText({ thresholdMicros: 0, maxLen: 128 }, null), 'Every command is logged (the threshold is 0). The last 128 are kept.');
  assert.match(slowlogSettingsText({ thresholdMicros: -1, maxLen: 128 }, null), /switched off on the server/);
  assert.equal(slowlogSettingsText(null, 'the server refused CONFIG GET for this user'), 'The threshold and length are unknown: the server refused CONFIG GET for this user.');
  assert.equal(slowlogSettingsText(null, null), 'The threshold and length are unknown.');
});

test('under a slow-log command: its keys, its client, what was cut, the note', () => {
  assert.equal(slowEntryFacts({ keys: ['queue:emails'], moreKeys: 0, client: '10.0.4.17:51122', clientName: null, argsCut: 0, note: 'Reads a whole list.' }), 'key queue:emails · from 10.0.4.17:51122 · Reads a whole list.');
  assert.equal(slowEntryFacts({ keys: ['a', 'b'], moreKeys: 3, client: '10.0.4.17:51122', clientName: 'worker', argsCut: 12, note: null }), 'keys a, b +3 more · from 10.0.4.17:51122 (worker) · … 12 more arguments');
  assert.equal(slowEntryFacts({ keys: [], moreKeys: 0, client: null, clientName: null, argsCut: 0, note: null }), '');
});

// ---- Monitor ----

const ready = (run, startedAt) => ({ run, kind: 'ready', startedAt, maxSecs: 30, maxLines: 10000, lines: [], stopped: null });
const lines = (run, n) => ({ run, kind: 'lines', lines: Array.from({ length: n }, (_, i) => ({ command: `GET k${i}` })), stopped: null });
const closed = (run, stopped) => ({ run, kind: 'closed', lines: [], stopped });

test('monitor: Start begins a run; ready sets the clock by the server\'s start time', () => {
  const m = monitorNew();
  monitorBegin(m, { run: 6, maxSecs: 30, maxLines: 10000 }, 1000);
  assert.equal(m.running, true);
  assert.equal(monitorLeft(m, 1000), 30);
  monitorApply(m, ready(6, 3000), 3500);
  assert.equal(monitorLeft(m, 3000), 30, 'counted from when the server started it');
  assert.equal(monitorLeft(m, 3000 + 12400), 18);
  assert.equal(monitorLeft(m, 3000 + 99000), 0, 'never below zero');
});

test('monitor: lines are kept, counted and capped; closing keeps them and says why', () => {
  const m = monitorNew();
  monitorBegin(m, { run: 1, maxSecs: 30, maxLines: 10000 }, 0);
  monitorApply(m, lines(1, 2));
  assert.equal(m.kept, 2);
  monitorApply(m, lines(1, 2500));
  assert.equal(m.lines.length, 2000, 'the page keeps the newest 2,000');
  assert.equal(m.lines[1999].command, 'GET k2499');
  assert.equal(m.kept, 2502, 'and counts them all');
  monitorApply(m, closed(1, 'MONITOR stopped by itself: its time was up.'));
  assert.equal(m.running, false);
  assert.equal(m.stopped, 'MONITOR stopped by itself: its time was up.');
  assert.equal(m.lines.length, 2000);
  assert.equal(monitorLeft(m), 0);
});

test('monitor: events of another run are ignored; a new run starts with an empty list', () => {
  const m = monitorNew();
  monitorBegin(m, { run: 1, maxSecs: 30, maxLines: 10000 }, 0);
  monitorApply(m, lines(1, 3));
  monitorApply(m, lines(9, 5));
  assert.equal(m.kept, 3);
  monitorApply(m, closed(9, 'MONITOR stopped.'));
  assert.equal(m.running, true, 'the close of an older run does not stop this one');
  monitorApply(m, closed(1, 'MONITOR stopped.'));
  monitorApply(m, ready(2, 5000), 5000);
  assert.equal(m.run, 2);
  assert.deepEqual(m.lines, []);
  assert.equal(m.kept, 0);
  assert.equal(m.stopped, null);
  assert.equal(m.running, true);
});

test('monitor: the state names a run this page did not start, ends one it missed, and resets on another connection', () => {
  const m = monitorNew();
  const state = (name, monitor) => ({ conn: name ? { name, docker: false } : null, monitor });
  monitorFollow(m, state('demo', { run: 4, startedAt: 10000, maxSecs: 30, maxLines: 10000 }), 15000);
  assert.equal(m.run, 4);
  assert.equal(m.running, true);
  assert.equal(monitorLeft(m, 15000), 25, 'a reload in the middle of a run picks up its clock');
  monitorFollow(m, state('demo', null));
  assert.equal(m.running, false);
  assert.equal(m.stopped, 'MONITOR stopped.');
  monitorApply(m, lines(4, 2));
  monitorFollow(m, state('other', null));
  assert.equal(m.run, null);
  assert.deepEqual(m.lines, []);
});

test('the server asks before Monitor, so the page asks nothing of its own; Overview reads no panel of its own', async () => {
  const { readFileSync } = await import('node:fs');
  const read = (rel) => readFileSync(new URL(`../${rel}`, import.meta.url), 'utf8');
  assert.equal(/confirmDialog/.test(read('views/server.js')), false, 'Monitor is asked about once, by the server');
  const overview = read('views/overview.js');
  assert.equal(/getSlowlog|getClients/.test(overview), false, 'the findings come with GET /api/overview');
});
