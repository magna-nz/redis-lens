// The confirm flow: the dialog's state machine, and the wrapper that answers or cancels a
// write the gate wants confirmed (HTTP 409 → POST or DELETE /api/confirm/<id>).
import test from 'node:test';
import assert from 'node:assert/strict';
import { confirmStart, confirmStep, confirmCanSend, CONFIRM_MISMATCH } from '../ui.js';
import { ApiError, ConfirmRequired, TEXT, createApi, createReal, runWithConfirm } from '../api.js';

test('kind ask: Yes sends, Cancel cancels', () => {
  const s = confirmStart({ id: 'c1', kind: 'ask', text: 'Send it?' });
  assert.equal(s.kind, 'ask');
  assert.equal(confirmCanSend(s), true);
  assert.equal(confirmStep(s, { type: 'send' }).status, 'confirmed');
  assert.equal(confirmStep(s, { type: 'cancel' }).status, 'cancelled');
});

test('kind type: sends only when the text typed is exactly the name shown', () => {
  let s = confirmStart({ id: 'c2', kind: 'type', text: 'Turn on writes?', expect: 'prod-cache-01' });
  assert.equal(confirmCanSend(s), false);
  s = confirmStep(s, { type: 'send' });
  assert.equal(s.status, 'open');
  assert.equal(s.error, CONFIRM_MISMATCH);
  s = confirmStep(s, { type: 'input', value: 'prod-cache-0' });
  assert.equal(s.error, null, 'typing clears the message');
  assert.equal(confirmCanSend(s), false);
  s = confirmStep(s, { type: 'input', value: 'PROD-CACHE-01' });
  assert.equal(confirmCanSend(s), false, 'the case must match');
  s = confirmStep(s, { type: 'input', value: 'prod-cache-01 ' });
  assert.equal(confirmCanSend(s), false, 'no trimming');
  s = confirmStep(s, { type: 'input', value: 'prod-cache-01' });
  assert.equal(confirmCanSend(s), true);
  s = confirmStep(s, { type: 'send' });
  assert.equal(s.status, 'confirmed');
  assert.equal(s.typed, 'prod-cache-01');
});

test('a typed question with nothing to type can never be sent', () => {
  const s = confirmStart({ id: 'c3', kind: 'type', text: '?', expect: '' });
  assert.equal(confirmCanSend(s), false);
  assert.equal(confirmStep(s, { type: 'send' }).status, 'open');
});

test('an unknown kind with a name to type is treated as type, not as a plain Yes', () => {
  assert.equal(confirmStart({ kind: 'weird', expect: 'x' }).kind, 'type');
  assert.equal(confirmStart({ kind: 'weird' }).kind, 'ask');
});

test('a finished question ignores further steps', () => {
  const done = confirmStep(confirmStart({ kind: 'ask' }), { type: 'cancel' });
  assert.equal(confirmStep(done, { type: 'send' }).status, 'cancelled');
});

function fakeBackend() {
  const calls = [];
  return {
    calls,
    async answerConfirm(confirm, typed, adapt) {
      calls.push(['answer', confirm.id, typed]);
      return adapt({ done: true });
    },
    async cancelConfirm(id) {
      calls.push(['cancel', id]);
      return {};
    },
  };
}

test('a write that needs no confirmation just returns', async () => {
  const backend = fakeBackend();
  const result = await runWithConfirm(async () => 'ok', backend, async () => assert.fail('should not ask'));
  assert.equal(result, 'ok');
  assert.deepEqual(backend.calls, []);
});

test('a confirmed write is answered with what was typed, and its reply is adapted', async () => {
  const backend = fakeBackend();
  const confirm = { id: 'p1', kind: 'type', text: 'Delete?', expect: 'user:1' };
  const result = await runWithConfirm(
    async () => { throw new ConfirmRequired(confirm, (j) => ({ adapted: j.done })); },
    backend,
    async (c) => { assert.equal(c, confirm); return { typed: 'user:1' }; },
  );
  assert.deepEqual(result, { adapted: true });
  assert.deepEqual(backend.calls, [['answer', 'p1', 'user:1']]);
});

test('a cancelled write tells the server and rejects as cancelled', async () => {
  const backend = fakeBackend();
  await assert.rejects(
    runWithConfirm(async () => { throw new ConfirmRequired({ id: 'p2', kind: 'ask', text: '?' }); }, backend, async () => null),
    (err) => err instanceof ApiError && err.cancelled === true && err.message === TEXT.cancelled,
  );
  assert.deepEqual(backend.calls, [['cancel', 'p2']]);
});

test('a cancel still rejects as cancelled when telling the server fails', async () => {
  const backend = fakeBackend();
  backend.cancelConfirm = async () => { throw new ApiError('gone'); };
  await assert.rejects(
    runWithConfirm(async () => { throw new ConfirmRequired({ id: 'p3', kind: 'ask', text: '?' }); }, backend, async () => null),
    (err) => err.cancelled === true,
  );
});

test('other errors pass through untouched, and nothing is asked', async () => {
  const boom = new ApiError('Not sent: prod is read-only.');
  await assert.rejects(runWithConfirm(async () => { throw boom; }, fakeBackend(), async () => assert.fail('should not ask')), (err) => err === boom);
});

test('a second question after the first is asked too, and endless questions stop', async () => {
  const backend = fakeBackend();
  let asked = 0;
  backend.answerConfirm = async (confirm) => {
    if (confirm.id === 'q1') throw new ConfirmRequired({ id: 'q2', kind: 'ask', text: 'Sure?' });
    return 'sent';
  };
  const result = await runWithConfirm(async () => { throw new ConfirmRequired({ id: 'q1', kind: 'ask', text: '?' }); }, backend, async () => { asked += 1; return { typed: '' }; });
  assert.equal(result, 'sent');
  assert.equal(asked, 2);

  backend.answerConfirm = async () => { throw new ConfirmRequired({ id: 'again', kind: 'ask', text: '?' }); };
  await assert.rejects(
    runWithConfirm(async () => { throw new ConfirmRequired({ id: 'q1', kind: 'ask', text: '?' }); }, backend, async () => ({ typed: '' })),
    (err) => err.message === TEXT.tooMany,
  );
});

// ---- The real implementation against a fake fetch ----

function fakeFetch(script) {
  const seen = [];
  const fn = async (path, init) => {
    seen.push({ path, method: init.method, headers: init.headers, credentials: init.credentials, body: init.body === undefined ? undefined : JSON.parse(init.body) });
    const [status, json] = script(path, init, seen.length);
    return { status, ok: status >= 200 && status < 300, json: async () => json };
  };
  fn.seen = seen;
  return fn;
}

test('409 with a confirm opens the question, then POSTs the typed name to /api/confirm/<id>', async () => {
  const fetchFn = fakeFetch((path) => {
    if (path === '/api/cmdline') return [409, { confirm: { expect: 'demo-prod', id: 'abc 1', kind: 'type', text: 'KEYS is @dangerous: it can slow the server down. Type the profile name demo-prod to send it: KEYS user:*' } }];
    if (path === '/api/confirm/abc%201') return [200, { line: 'KEYS user:*', node: null, replies: [{ cut: false, error: false, json: ['[]'], raw: ['[array] *0'], table: ['(empty array)'], text: ['(empty array)'], took_ms: 0.5 }], route_note: null, sent: 1, status: 'ok', text: null, times: 1 }];
    return [500, {}];
  });
  const api = createApi(createReal({ fetchFn }));
  const questions = [];
  api.setConfirmHandler(async (c) => { questions.push(c); return { typed: 'demo-prod' }; });
  const reply = await api.runCommand('KEYS user:*');
  assert.equal(reply.replies[0].views.text, '(empty array)', 'the confirmed reply goes through the same adapter');
  assert.equal(questions.length, 1);
  assert.equal(questions[0].kind, 'type');
  assert.equal(questions[0].expect, 'demo-prod');
  assert.deepEqual(fetchFn.seen.map((c) => [c.method, c.path, c.body]), [
    ['POST', '/api/cmdline', { line: 'KEYS user:*', pipeline: false }],
    ['POST', '/api/confirm/abc%201', { typed: 'demo-prod' }],
  ]);
});

test('a yes/no question is answered with an empty body', async () => {
  const fetchFn = fakeFetch((path) => (path === '/api/pubsub/publish'
    ? [409, { confirm: { expect: null, id: 'y1', kind: 'ask', text: 'Send PUBLISH orders "x" to demo-prod?' } }]
    : [200, { reply: { kind: 'int', value: 3 }, sent: true }]));
  const api = createApi(createReal({ fetchFn }));
  api.setConfirmHandler(async () => ({ typed: '' }));
  assert.deepEqual(await api.publish({ channel: 'orders', message: 'x' }), { receivers: 3 });
  assert.deepEqual(fetchFn.seen.map((c) => [c.method, c.path, c.body]), [
    ['POST', '/api/pubsub/publish', { channel: 'orders', message: 'x', shard: false }],
    ['POST', '/api/confirm/y1', {}],
  ]);
});

test('Cancel sends DELETE /api/confirm/<id> and nothing else', async () => {
  const fetchFn = fakeFetch((path, init) => (init.method === 'DELETE' ? [200, { cancelled: true, text: 'Cancelled: nothing was sent.' }] : [409, { confirm: { id: 'x9', kind: 'ask', text: 'Publish?' } }]));
  const api = createApi(createReal({ fetchFn }));
  api.setConfirmHandler(async () => null);
  await assert.rejects(api.publish({ channel: 'c', message: 'm' }), (err) => err.cancelled === true);
  assert.deepEqual(fetchFn.seen.map((c) => [c.method, c.path]), [['POST', '/api/pubsub/publish'], ['DELETE', '/api/confirm/x9']]);
});

test('a 409 without a confirm is "not connected", in the server\'s words, and asks nothing', async () => {
  const api = createApi(createReal({ fetchFn: fakeFetch(() => [409, { error: 'not connected: connect first, or the live connection is closed' }]) }));
  api.setConfirmHandler(async () => assert.fail('nothing to ask'));
  await assert.rejects(api.getClients(), (err) => err.notLive === true && err.message === 'not connected: connect first, or the live connection is closed');
});

test('a question that expired (410) and a wrong name (400) show the server\'s words', async () => {
  const gone = createApi(createReal({ fetchFn: fakeFetch((path) => (path === '/api/cmdline' ? [409, { confirm: { id: 'g1', kind: 'ask', text: '?' } }] : [410, { error: 'that question is no longer open: it was answered, cancelled or expired' }])) }));
  gone.setConfirmHandler(async () => ({ typed: '' }));
  await assert.rejects(gone.runCommand('DEL a'), (err) => err.status === 410 && /no longer open/.test(err.message));
});

test('an accepted connect (202) is a success, and the request names a profile or a container', async () => {
  const fetchFn = fakeFetch(() => [202, { status: 'connecting', connection: { name: 'demo', env: 'local', env_label: 'LOCAL', endpoint: 'h:1' }, live: 'waiting', writes: true }]);
  const api = createApi(createReal({ fetchFn }));
  const state = await api.connect({ name: 'demo' });
  assert.equal(state.connecting.name, 'demo');
  await api.connect({ docker: 'demo-valkey-1' });
  await api.pickNode(2);
  await api.subscribe({ name: 'orders.*' });
  await api.subscribe({ name: 'shard.x', kind: 'shard' });
  assert.deepEqual(fetchFn.seen.map((c) => [c.path, c.body]), [
    ['/api/connect', { name: 'demo' }],
    ['/api/connect', { docker: 'demo-valkey-1' }],
    ['/api/node', { index: 2 }],
    ['/api/pubsub/subscribe', { name: 'orders.*', kind: 'auto' }],
    ['/api/pubsub/subscribe', { name: 'shard.x', kind: 'shard' }],
  ]);
});

test('help that the server has no docs for (404) is no help, not an error', async () => {
  const api = createApi(createReal({ fetchFn: fakeFetch((path) => (path.endsWith('name=nosuch') ? [404, { error: 'the server has no docs for that name' }] : [502, { error: 'the server did not answer' }])) }));
  assert.equal(await api.getCommandHelp('nosuch'), null);
  await assert.rejects(api.getCommandHelp('get'), (err) => err.status === 502);
});

test('every request carries the session cookie and the header; every non-GET a JSON body', async () => {
  const fetchFn = fakeFetch(() => [200, {}]);
  const api = createApi(createReal({ fetchFn }));
  await api.getClients();
  await api.disconnect();
  await api.stopMonitor();
  const [get, post, del] = fetchFn.seen;
  for (const call of fetchFn.seen) {
    assert.equal(call.credentials, 'same-origin');
    assert.equal(call.headers['X-Redis-Lens'], '1');
  }
  assert.equal(get.method, 'GET');
  assert.equal(get.body, undefined);
  assert.equal(post.headers['Content-Type'], 'application/json');
  assert.deepEqual(post.body, {});
  assert.equal(del.method, 'DELETE');
  assert.deepEqual(del.body, {}, 'a DELETE has a JSON body too, as the server requires');
});

test('401 raises the locked event and a locked error', async () => {
  const api = createApi(createReal({ fetchFn: fakeFetch(() => [401, null]) }));
  let locked = 0;
  api.on('locked', () => { locked += 1; });
  api.start();
  await assert.rejects(api.getState(), (err) => err.locked === true && err.status === 401);
  assert.equal(locked, 1);
});

test('a failed request shows only the fixed wording the server sent, or a fixed fallback', async () => {
  const withError = createApi(createReal({ fetchFn: fakeFetch(() => [400, { error: 'Not sent: prod is read-only.' }]) }));
  await assert.rejects(withError.getOverview(), (err) => err.message === 'Not sent: prod is read-only.' && err.status === 400);
  const noBody = createApi(createReal({ fetchFn: fakeFetch(() => [500, { detail: 'NOPERM user alice', trace: 'x' }]) }));
  await assert.rejects(noBody.getOverview(), (err) => err.message === TEXT.failed);
  const forbidden = createApi(createReal({ fetchFn: fakeFetch(() => [403, null]) }));
  await assert.rejects(forbidden.getOverview(), (err) => err.message === TEXT.forbidden);
});

test('a network failure says redis-lens may have been closed', async () => {
  const api = createApi(createReal({ fetchFn: async () => { throw new TypeError('Failed to fetch'); } }));
  await assert.rejects(api.getState(), (err) => err.network === true && err.message === TEXT.network);
});

test('replies that are the new State also reach state subscribers', async () => {
  const api = createApi(createReal({ fetchFn: fakeFetch(() => [200, { status: 'connected', connection: { name: 'demo', env: 'local', read_only: false, writes: true }, live: 'ready', writes: true }]) }));
  const states = [];
  api.on('state', (s) => states.push(s));
  await api.connect({ name: 'demo' });
  assert.equal(states.length, 1);
  assert.equal(states[0].conn.name, 'demo');
  const off = api.on('figures', () => assert.fail('unsubscribed'));
  off();
});

test('a reply\'s state is dropped when the stream delivered a newer one while the request ran', async () => {
  // A connect to a closed port: the stream says "connecting" then "failed" before the
  // request's own reply ("connecting") arrives. The page must end on "failed".
  const sources = [];
  class FakeSource {
    constructor() { this.listeners = {}; this.readyState = 1; sources.push(this); }
    addEventListener(name, fn) { this.listeners[name] = fn; }
    close() { this.readyState = 2; }
  }
  const state = (status, error = null) => ({ status, error, connection: { name: 'zz', env: 'other', env_label: 'OTHER', endpoint: '127.0.0.1:6399' }, live: 'waiting', writes: true });
  let release;
  const fetchFn = async () => {
    await new Promise((resolve) => { release = resolve; });
    return { status: 202, ok: true, json: async () => state('connecting') };
  };
  const api = createApi(createReal({ fetchFn, EventSourceFn: FakeSource }));
  const seen = [];
  api.on('state', (s) => seen.push(s.status));
  api.start();
  const pending = api.connect({ name: 'zz' });
  await new Promise((resolve) => setTimeout(resolve, 5));
  sources[0].listeners.state({ data: JSON.stringify(state('connecting')) });
  sources[0].listeners.state({ data: JSON.stringify(state('failed', 'Connection refused (os error 61)')) });
  release();
  await pending;
  api.stop();
  assert.deepEqual(seen, ['connecting', 'failed']);
});

test('events from the stream are adapted and delivered; a broken handler does not stop the others', async () => {
  const sources = [];
  class FakeSource {
    constructor(url) { this.url = url; this.listeners = {}; this.readyState = 1; sources.push(this); }
    addEventListener(name, fn) { this.listeners[name] = fn; }
    close() { this.readyState = 2; }
  }
  const api = createApi(createReal({ fetchFn: fakeFetch(() => [200, {}]), EventSourceFn: FakeSource }));
  const got = [];
  const realError = console.error;
  console.error = () => {};
  try {
    api.on('figures', () => { throw new Error('view bug'); });
    api.on('figures', (f) => got.push(f));
    api.on('link', (l) => got.push(l));
    api.start();
    assert.equal(sources[0].url, '/api/events');
    sources[0].onopen();
    sources[0].listeners.figures({ data: JSON.stringify({ node: 0, info: { clients: 14 }, rates: { ops: 1200, clients: 14 } }) });
    sources[0].listeners.figures({ data: 'not json' });
  } finally {
    console.error = realError;
  }
  assert.deepEqual(got[0], { up: true });
  assert.equal(got[1].opsPerSec, 1200);
  assert.equal(got[1].clients, 14);
  assert.equal(got.length, 2, 'a malformed event is dropped');
  api.stop();
  assert.equal(sources[0].readyState, 2);
});
