// The small pure pieces of the views: Overview's tile lines and the Monitor countdown.
import test from 'node:test';
import assert from 'node:assert/strict';
import { spanText, memorySub, keysSub } from '../views/overview.js';
import { monitorApply, monitorLeft } from '../views/server.js';

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
  assert.equal(keysSub({ ttlShare: 0.31, db: 0 }), '31% have a TTL · database 0');
  assert.equal(keysSub({ ttlShare: null, db: 0 }), 'database 0');
  assert.equal(keysSub({ ttlShare: null, db: null }), '—');
});

const fresh = () => ({ running: false, endsAt: 0, lines: [], kept: 0, node: null, stopped: null, conn: null });

test('monitor: a run counts down from what the server says', () => {
  const m = fresh();
  monitorApply(m, { running: true, left: 30, lines: [] }, 1000);
  assert.equal(m.running, true);
  assert.equal(monitorLeft(m, 1000), 30);
  assert.equal(monitorLeft(m, 1000 + 12400), 18);
  assert.equal(monitorLeft(m, 1000 + 99000), 0, 'never below zero');
  monitorApply(m, { running: true, left: 9, lines: [{ at: 1, command: 'GET a' }] }, 22000);
  assert.equal(monitorLeft(m, 22000), 9, 'the server corrects the clock');
});

test('monitor: lines are kept, counted and capped; stopping keeps them', () => {
  const m = fresh();
  monitorApply(m, { running: true, left: 10, lines: [] }, 0);
  monitorApply(m, { running: true, left: 9, lines: [{ command: 'GET a' }, { command: 'GET b' }] }, 1000);
  assert.equal(m.lines.length, 2);
  assert.equal(m.kept, 2);
  monitorApply(m, { running: true, left: 8, kept: 9000, lines: Array.from({ length: 2500 }, (_, i) => ({ command: `GET k${i}` })) }, 2000);
  assert.equal(m.lines.length, 2000, 'the page keeps the newest 2,000');
  assert.equal(m.lines[1999].command, 'GET k2499');
  assert.equal(m.kept, 9000, 'the count is the server’s');
  monitorApply(m, { running: false, stopped: 'MONITOR stopped after 30 s.', lines: [] }, 3000);
  assert.equal(m.running, false);
  assert.equal(m.stopped, 'MONITOR stopped after 30 s.');
  assert.equal(m.lines.length, 2000);
  assert.equal(monitorLeft(m, 3000), 0);
});

test('monitor: a new run starts with an empty list', () => {
  const m = fresh();
  monitorApply(m, { running: true, left: 10, lines: [{ command: 'old' }] }, 0);
  monitorApply(m, { running: false, lines: [] }, 1000);
  assert.equal(m.stopped, 'MONITOR stopped.');
  monitorApply(m, { running: true, left: 10, lines: [{ command: 'new' }] }, 2000);
  assert.deepEqual(m.lines.map((l) => l.command), ['new']);
  assert.equal(m.kept, 1);
  assert.equal(m.stopped, null);
});
