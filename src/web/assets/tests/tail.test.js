// The Pub/Sub tail: the filter, the buffer (cap, pause, clear, rate), its status line,
// and how it follows the server's events and state.
import test from 'node:test';
import assert from 'node:assert/strict';
import { TailBuffer, tailFilter, tailMatches } from '../ui.js';
import { isPattern, tailStatus, tailNew, tailBegin, tailApply, tailFollow } from '../views/pubsub.js';

const msg = (body, channel = 'orders.created', at = 0) => ({ at, channel, body });

test('the filter matches the message or the channel, whatever the case', () => {
  const m = msg('{"order":"A-20417","carrier":"Courier"}', 'orders.shipped');
  assert.equal(tailMatches(m, 'courier'), true);
  assert.equal(tailMatches(m, 'SHIPPED'), true);
  assert.equal(tailMatches(m, 'refund'), false);
  assert.equal(tailMatches(m, ''), true);
  assert.equal(tailMatches(m, '   '), true);
});

test('the filter keeps the order', () => {
  const all = [msg('a1', 'x'), msg('b1', 'x'), msg('a2', 'x'), msg('c1', 'has-a')];
  assert.deepEqual(tailFilter(all, 'a').map((m) => m.body), ['a1', 'a2', 'c1'], 'c1 matches by its channel');
  assert.deepEqual(tailFilter(all, 'zzz'), []);
});

test('the buffer keeps the newest messages up to its cap', () => {
  const b = new TailBuffer(5);
  b.push([1, 2, 3].map((n) => msg(`m${n}`)));
  b.push([4, 5, 6, 7].map((n) => msg(`m${n}`)));
  assert.deepEqual(b.kept.map((m) => m.body), ['m3', 'm4', 'm5', 'm6', 'm7']);
});

test('pause freezes what is shown while messages still arrive and are kept', () => {
  const b = new TailBuffer(100);
  b.push([msg('one')]);
  b.pause();
  b.push([msg('two'), msg('three')]);
  assert.equal(b.paused, true);
  assert.equal(b.newWhilePaused, 2);
  assert.deepEqual(b.visible().rows.map((m) => m.body), ['one']);
  assert.equal(b.kept.length, 3);
  b.resume();
  assert.equal(b.paused, false);
  assert.deepEqual(b.visible().rows.map((m) => m.body), ['one', 'two', 'three']);
});

test('clear empties the list and the tail keeps running', () => {
  const b = new TailBuffer(100);
  b.push([msg('one')], 4);
  b.clear();
  assert.equal(b.kept.length, 0);
  assert.equal(b.skipped, 0);
  b.push([msg('two')]);
  assert.deepEqual(b.visible().rows.map((m) => m.body), ['two']);
});

test('visible applies the filter and shows only the newest rows', () => {
  const b = new TailBuffer(100);
  b.push(Array.from({ length: 10 }, (_, i) => msg(i % 2 ? `odd ${i}` : `even ${i}`)));
  const v = b.visible('odd', 3);
  assert.deepEqual(v.rows.map((m) => m.body), ['odd 5', 'odd 7', 'odd 9']);
  assert.equal(v.matching, 5);
  assert.equal(v.kept, 10);
});

test('skipped messages are counted', () => {
  const b = new TailBuffer(10);
  b.push([msg('a')], 120);
  b.push([msg('b')], 30);
  assert.equal(b.skipped, 150);
});

test('the rate is messages per second over the last five seconds', () => {
  const b = new TailBuffer(100);
  assert.equal(b.rate(1000), 0);
  b.push([msg('a'), msg('b')], 0, 10000);
  assert.equal(b.rate(10000), 2, 'over at least one second');
  b.push([msg('c'), msg('d')], 0, 12000);
  assert.equal(b.rate(12000), 2, '4 messages over 2 s');
  assert.equal(b.rate(30000), 0, 'old arrivals fall out of the window');
});

test('the status line says the rate and what is kept, or that it is paused', () => {
  const b = new TailBuffer(2000);
  b.push([msg('a'), msg('b'), msg('c')], 0, 5000);
  assert.equal(tailStatus(b, 5000), '3 msg/s · 3 kept (last 2,000)');
  b.pause();
  b.push([msg('d')], 2, 5500);
  assert.equal(tailStatus(b, 5500), 'paused · 1 new · 4 kept (last 2,000) · 2 skipped, too many at once');
});

test('text with * ? or [ is a pattern', () => {
  assert.equal(isPattern('orders.*'), true);
  assert.equal(isPattern('user:?'), true);
  assert.equal(isPattern('h[ae]llo'), true);
  assert.equal(isPattern('orders.created'), false);
  assert.equal(isPattern('__keyevent@0__:expired'), false);
});

// ---- Following the server ----

const event = (id, kind, extra = {}) => ({ id, kind, messages: [], skipped: 0, error: null, ...extra });

test('a tail: ready, messages, and a close that says why', () => {
  const t = tailNew();
  tailBegin(t, { id: 5, name: 'orders.*', kind: 'pattern', node: 0 });
  assert.equal(t.ready, false);
  tailApply(t, event(5, 'ready'));
  assert.equal(t.ready, true);
  tailApply(t, event(5, 'messages', { messages: [msg('one'), msg('two')], skipped: 4 }), 1000);
  assert.deepEqual(t.buffer.kept.map((m) => m.body), ['one', 'two']);
  assert.equal(t.buffer.skipped, 4);
  tailApply(t, event(5, 'closed', { error: 'the connection to the server was lost' }));
  assert.equal(t.stopped, 'the connection to the server was lost');
  assert.equal(t.buffer.kept.length, 2, 'what was shown stays on screen');
});

test('messages of a tail that was replaced are ignored', () => {
  const t = tailNew();
  tailBegin(t, { id: 5, name: 'a', kind: 'channel', node: 0 });
  tailBegin(t, { id: 6, name: 'b', kind: 'channel', node: 0 });
  tailApply(t, event(5, 'messages', { messages: [msg('late')] }));
  tailApply(t, event(5, 'closed'));
  assert.equal(t.buffer.kept.length, 0);
  assert.equal(t.stopped, null, 'the close of the old tail does not stop the new one');
  tailApply(t, event(6, 'messages', { messages: [msg('mine')] }));
  assert.equal(t.buffer.kept.length, 1);
  const none = tailNew();
  tailApply(none, event(1, 'messages', { messages: [msg('x')] }));
  assert.equal(none.sub, null, 'no tail, nothing kept');
});

test('the state names a tail this page did not start, ends one that is gone, and resets on another connection', () => {
  const t = tailNew();
  const state = (name, tail) => ({ conn: name ? { name, docker: false } : null, tail });
  tailFollow(t, state('demo', { id: 7, kind: 'pattern', name: 'orders.*', node: 0, ready: true }));
  assert.deepEqual(t.sub, { id: 7, name: 'orders.*', kind: 'pattern', node: 0 });
  assert.equal(t.ready, true);
  tailApply(t, event(7, 'messages', { messages: [msg('kept')] }));
  tailFollow(t, state('demo', { id: 7, kind: 'pattern', name: 'orders.*', node: 0, ready: true }));
  assert.equal(t.buffer.kept.length, 1, 'the same tail again changes nothing');
  tailFollow(t, state('demo', null));
  assert.ok(t.stopped);
  tailFollow(t, state('other', null));
  assert.equal(t.sub, null);
  assert.equal(t.buffer.kept.length, 0);
});
