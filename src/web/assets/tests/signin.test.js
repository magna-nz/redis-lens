// Sign-in without a cookie: the token from the address, where it is kept, the Bearer
// header, and the event stream read with fetch (the SSE parser, reconnects, 401, stop).
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  createSseParser, createSseDecoder, createEventStream, createTokenStore, tokenFromHash, takeTokenFromAddress,
  TOKEN_KEY, createApi, createReal, initApi, api as pageApi, TEXT, STREAM_BACKOFF,
} from '../api.js';
import { createMockServer, MOCK_TOKEN } from '../mock.js';

const TOKEN = '0123456789abcdef'.repeat(4);
const tick = (ms = 5) => new Promise((resolve) => setTimeout(resolve, ms));

// ---------- the SSE parser ----------

/** A sample stream with everything the parser must handle: comments (keep-alives), a
 *  multi-line data field, CRLF and lone CR line ends, a field without a colon, no space
 *  after the colon, an event without data, an unknown field, characters of 2, 3 and 4
 *  bytes, and a last event that never ends (dropped). */
const SAMPLE = ': hello\n\n'
  + 'event: state\ndata: {"status":"idle"}\n\n'
  + 'event: notice\r\ndata: {"text":"ünïcödé → ✓ 🙂"}\r\n\r\n'
  + 'event: tail\rdata: line one\rdata: line two\r\r'
  + 'event:figures\ndata:{"n":1}\nid: 7\nretry: 1000\nfoo: bar\n\n'
  + 'event: empty\n\n'
  + 'data\n\n'
  + ': keep-alive\n\n'
  + 'event: last\ndata: never sent';
const EXPECTED = [
  { event: 'state', data: '{"status":"idle"}' },
  { event: 'notice', data: '{"text":"ünïcödé → ✓ 🙂"}' },
  { event: 'tail', data: 'line one\nline two' },
  { event: 'figures', data: '{"n":1}' },
  { event: 'message', data: '' },
];

function parseBytes(chunks) {
  const events = [];
  const comments = [];
  const d = createSseDecoder((e) => events.push(e), (c) => comments.push(c));
  for (const c of chunks) d.push(c);
  d.end();
  return { events, comments };
}

test('sse: a whole stream in one chunk', () => {
  const { events, comments } = parseBytes([new TextEncoder().encode(SAMPLE)]);
  assert.deepEqual(events, EXPECTED);
  assert.deepEqual(comments, [' hello', ' keep-alive']);
});

test('sse: split in two at every byte, characters and CRLFs included, the events are the same', () => {
  const bytes = new TextEncoder().encode(SAMPLE);
  for (let i = 0; i <= bytes.length; i += 1) {
    const { events } = parseBytes([bytes.slice(0, i), bytes.slice(i)]);
    assert.deepEqual(events, EXPECTED, `split at byte ${i}`);
  }
});

test('sse: one byte at a time, and in three pieces at every pair of places', () => {
  const bytes = new TextEncoder().encode(SAMPLE);
  assert.deepEqual(parseBytes(Array.from(bytes, (b) => Uint8Array.of(b))).events, EXPECTED);
  for (let i = 0; i < bytes.length; i += 3) {
    for (let j = i; j < bytes.length; j += 5) {
      assert.deepEqual(parseBytes([bytes.slice(0, i), bytes.slice(i, j), bytes.slice(j)]).events, EXPECTED, `split at ${i} and ${j}`);
    }
  }
});

test('sse: a CR at the end of a chunk waits for a possible LF', () => {
  const events = [];
  const p = createSseParser((e) => events.push(e));
  p.push('data: a\r');
  p.push('\n\r');
  assert.equal(events.length, 0, 'CRLF then a lone CR held back: the blank line is not known yet');
  p.push('\n');
  assert.deepEqual(events, [{ event: 'message', data: 'a' }]);
});

test('sse: a leading BOM is dropped, and only one space after the colon', () => {
  const events = [];
  const p = createSseParser((e) => events.push(e));
  p.push('﻿event: x\ndata:  two spaces\n\n');
  assert.deepEqual(events, [{ event: 'x', data: ' two spaces' }]);
});

// ---------- the event stream ----------

function fakeTimers() {
  const pending = [];
  return {
    pending,
    setTimer: (fn, ms) => { const t = { fn, ms }; pending.push(t); return t; },
    clearTimer: (t) => { const i = pending.indexOf(t); if (i >= 0) pending.splice(i, 1); },
    async fire() { const t = pending.shift(); t.fn(); await tick(); },
  };
}

/** A fetch whose answers are scripted per call: 'stream' (open, the test can end it),
 *  'throw' (network down), or an HTTP status. */
function scriptedFetch(script) {
  const calls = [];
  const fetchFn = async (url, init) => {
    const what = script[calls.length] ?? 'stream';
    const call = { url, init, controller: null, aborted: false };
    calls.push(call);
    if (init.signal) init.signal.addEventListener('abort', () => { call.aborted = true; try { call.controller && call.controller.error(new Error('abort')); } catch { /* closed */ } });
    if (what === 'throw') throw new TypeError('Failed to fetch');
    if (typeof what === 'number') return new Response('{"error":"x"}', { status: what });
    return new Response(new ReadableStream({ start(c) { call.controller = c; } }), { status: 200 });
  };
  return { fetchFn, calls };
}

test('stream: opens with the headers, reconnects with backoff when it ends or fails, stops on 401', async () => {
  const timers = fakeTimers();
  const { fetchFn, calls } = scriptedFetch(['stream', 'throw', 503, 'stream', 401]);
  const log = [];
  const s = createEventStream({
    fetchFn, url: '/api/events', headers: () => ({ Authorization: `Bearer ${TOKEN}` }),
    onEvent: (e) => log.push(`event ${e.event}`), onOpen: () => log.push('open'), onDown: () => log.push('down'), onLocked: () => log.push('locked'),
    setTimer: timers.setTimer, clearTimer: timers.clearTimer,
  });
  s.start();
  await tick();
  assert.equal(calls[0].init.headers.Authorization, `Bearer ${TOKEN}`);
  assert.equal(calls[0].init.credentials, 'omit');
  calls[0].controller.enqueue(new TextEncoder().encode('event: state\ndata: {}\n\n'));
  calls[0].controller.close();
  await tick();
  assert.deepEqual(log, ['open', 'event state', 'down']);
  assert.equal(timers.pending[0].ms, STREAM_BACKOFF[0]);
  await timers.fire(); // network down
  assert.equal(timers.pending[0].ms, STREAM_BACKOFF[1], 'the wait grows');
  await timers.fire(); // 503
  assert.equal(timers.pending[0].ms, STREAM_BACKOFF[2]);
  await timers.fire(); // open again: the wait starts over
  assert.equal(log.filter((l) => l === 'open').length, 2);
  calls[3].controller.error(new Error('connection reset'));
  await tick();
  assert.equal(timers.pending[0].ms, STREAM_BACKOFF[0], 'after a good stream the backoff starts over');
  await timers.fire(); // 401
  assert.equal(log[log.length - 1], 'locked');
  assert.equal(timers.pending.length, 0, 'no more tries after 401');
  assert.equal(s.running, false);
});

test('stream: stop aborts the request and cancels a pending try', async () => {
  const timers = fakeTimers();
  const { fetchFn, calls } = scriptedFetch(['stream', 'throw']);
  const s = createEventStream({ fetchFn, url: '/api/events', headers: () => ({}), onEvent: () => {}, setTimer: timers.setTimer, clearTimer: timers.clearTimer });
  s.start();
  await tick();
  s.stop();
  await tick();
  assert.equal(calls[0].aborted, true);
  assert.equal(timers.pending.length, 0, 'an aborted stream does not try again');
  s.start();
  await tick(); // throws
  assert.equal(timers.pending.length, 1);
  s.stop();
  assert.equal(timers.pending.length, 0);
});

test('stream: the backoff stops growing at its last step', () => {
  assert.deepEqual(STREAM_BACKOFF, [1000, 2000, 4000, 8000, 15000]);
});

// ---------- the token ----------

function fakeStorage({ broken = false } = {}) {
  const data = new Map();
  const fail = () => { throw new Error('SecurityError'); };
  return {
    data,
    getItem: (k) => (broken ? fail() : data.has(k) ? data.get(k) : null),
    setItem: (k, v) => (broken ? fail() : data.set(k, String(v))),
    removeItem: (k) => (broken ? fail() : data.delete(k)),
  };
}

function fakeWindow(href, storage = fakeStorage()) {
  const url = new URL(href);
  const win = {
    location: { pathname: url.pathname, search: url.search, hash: url.hash },
    history: { state: null, replaced: [], replaceState(state, title, to) { win.history.replaced.push(to); const u = new URL(to, href); win.location.pathname = u.pathname; win.location.search = u.search; win.location.hash = u.hash; } },
    sessionStorage: storage,
    localStorage: { setItem: () => assert.fail('localStorage is never used for the token') },
  };
  return win;
}

test('token: only #token=<64 hex> is a token; anything else under #token= is taken out but not kept', () => {
  assert.deepEqual(tokenFromHash(`#token=${TOKEN}`), { present: true, token: TOKEN });
  assert.deepEqual(tokenFromHash(`#token=${TOKEN.toUpperCase()}`), { present: true, token: TOKEN });
  assert.deepEqual(tokenFromHash('#token=abc'), { present: true, token: null });
  assert.deepEqual(tokenFromHash(`#token=${TOKEN}&x`), { present: true, token: null });
  assert.deepEqual(tokenFromHash('#/overview'), { present: false, token: null });
  assert.deepEqual(tokenFromHash(''), { present: false, token: null });
});

test('token: taken from the address at once, kept in sessionStorage, the address keeps ?mock but loses the hash', () => {
  const storage = fakeStorage();
  const win = fakeWindow(`http://127.0.0.1:7391/?x=1#token=${TOKEN}`, storage);
  const tokens = createTokenStore(storage);
  assert.equal(takeTokenFromAddress(win, tokens), true);
  assert.deepEqual(win.history.replaced, ['/?x=1']);
  assert.equal(win.location.hash, '');
  assert.equal(tokens.get(), TOKEN);
  assert.equal(storage.data.get(TOKEN_KEY), TOKEN);
  // A reload: a new store reads it back.
  assert.equal(createTokenStore(storage).get(), TOKEN);
});

test('token: one in the address replaces a kept one; a malformed one clears it; none leaves it', () => {
  const storage = fakeStorage();
  storage.setItem(TOKEN_KEY, 'f'.repeat(64));
  const tokens = createTokenStore(storage);
  takeTokenFromAddress(fakeWindow(`http://127.0.0.1:7391/#token=${TOKEN}`), tokens);
  assert.equal(tokens.get(), TOKEN);
  assert.equal(takeTokenFromAddress(fakeWindow('http://127.0.0.1:7391/#/server'), tokens), false);
  assert.equal(tokens.get(), TOKEN);
  takeTokenFromAddress(fakeWindow('http://127.0.0.1:7391/#token=nope'), tokens);
  assert.equal(tokens.get(), null);
  assert.equal(storage.data.has(TOKEN_KEY), false);
});

test('token: storage that throws (a private window, a policy) falls back to memory', () => {
  const tokens = createTokenStore(fakeStorage({ broken: true }));
  assert.equal(tokens.get(), null);
  tokens.set(TOKEN);
  assert.equal(tokens.get(), TOKEN);
  tokens.clear();
  assert.equal(tokens.get(), null);
  const none = createTokenStore(null);
  none.set(TOKEN);
  assert.equal(none.get(), TOKEN);
});

test('token: a value in storage that is not a token is ignored', () => {
  const storage = fakeStorage();
  storage.setItem(TOKEN_KEY, 'x');
  assert.equal(createTokenStore(storage).get(), null);
});

test('a 401 forgets the kept token and locks; the stream does too', async () => {
  const storage = fakeStorage();
  const tokens = createTokenStore(storage);
  tokens.set(TOKEN);
  const fetchFn = async () => new Response('{"error":"not signed in"}', { status: 401 });
  const api = createApi(createReal({ fetchFn, tokens }));
  let locked = 0;
  api.on('locked', () => { locked += 1; });
  await assert.rejects(api.getState(), (err) => err.locked && err.message === TEXT.locked);
  assert.equal(tokens.get(), null);
  assert.equal(storage.data.has(TOKEN_KEY), false);
  api.start(); // locked already: the stream is not opened again
  await tick();
  assert.equal(locked, 1);

  const tokens2 = createTokenStore(fakeStorage());
  tokens2.set(TOKEN);
  const api2 = createApi(createReal({ fetchFn, tokens: tokens2 }));
  let locked2 = 0;
  api2.on('locked', () => { locked2 += 1; });
  api2.start();
  await tick(20);
  assert.equal(locked2, 1, 'a 401 on the stream locks');
  assert.equal(tokens2.get(), null);
});

// ---------- boot, and the mock under the same contract ----------

test('initApi: takes the token, then talks to redis-lens; the token never goes to localStorage', async () => {
  const win = fakeWindow(`http://127.0.0.1:7391/#token=${TOKEN}`);
  const mode = await initApi(win);
  assert.deepEqual(mode, { mock: false, error: null });
  assert.equal(win.location.hash, '');
  assert.equal(win.sessionStorage.data.get(TOKEN_KEY), TOKEN);
  assert.equal(pageApi.mock, false);
});

test('initApi with ?mock: a build without mock.js says so, and nothing is made up', async () => {
  const win = fakeWindow('http://127.0.0.1:7391/?mock');
  const mode = await initApi(win, { importMock: () => Promise.reject(new TypeError('Failed to fetch dynamically imported module')) });
  assert.deepEqual(mode, { mock: true, error: TEXT.noMock });
});

test('initApi with ?mock: the sample servers, signed in with their own token', async () => {
  const win = fakeWindow('http://127.0.0.1:7391/?mock');
  const mode = await initApi(win, { importMock: async () => ({ createMockServer }) });
  assert.deepEqual(mode, { mock: true, error: null });
  assert.equal(pageApi.mock, true);
  assert.equal((await pageApi.getState()).status, 'idle');
  assert.equal(win.sessionStorage.data.has(TOKEN_KEY), false, 'the mock\'s token is not kept');
});

test('the mock answers 401 without the Bearer token, as the real server does, the stream included', async () => {
  const server = createMockServer();
  assert.equal(server.token, MOCK_TOKEN);
  const without = createApi(createReal({ fetchFn: server.fetchFn }));
  await assert.rejects(without.getState(), (err) => err.status === 401 && err.locked);
  assert.equal((await server.fetchFn('/api/events', { method: 'GET', headers: {} })).status, 401);
  assert.equal((await server.fetchFn('/api/state', { method: 'GET', headers: { Authorization: 'Bearer wrong' } })).status, 401);
  assert.equal((await server.fetchFn('/api/state', { method: 'GET', headers: { authorization: `bearer ${MOCK_TOKEN}` } })).status, 200, 'the scheme in any case');
  const signed = createApi(createReal({ fetchFn: server.fetchFn, token: server.token }));
  const states = [];
  signed.on('state', (s) => states.push(s.status));
  signed.start();
  await tick(50);
  signed.stop();
  assert.deepEqual(states, ['idle'], 'the stream starts with the state');
});

test('a sign-in address opened in a tab already on the page: the token is taken and kept', async () => {
  const { acceptSignIn } = await import('../api.js');
  const win = fakeWindow('http://127.0.0.1:7391/#/connections');
  assert.equal(acceptSignIn(win), false, 'a route is not a sign-in');
  win.location.hash = `#token=${TOKEN}`;
  assert.equal(acceptSignIn(win), true);
  assert.equal(win.location.hash, '');
  assert.equal(win.sessionStorage.data.get(TOKEN_KEY), TOKEN);
  win.location.hash = '#token=bad';
  assert.equal(acceptSignIn(win), true);
  assert.equal(win.sessionStorage.data.has(TOKEN_KEY), false, 'a wrong one clears the kept one');
});
