// The chrome around the views (docs/ui-redesign.md §1, §12): the context bar's models (the
// connection menu, the access button, the database and node controls, Live), the
// placeholder colouring of pattern names, and the command-line drawer.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import {
  PLACEHOLDERS, splitPlaceholders, connMenuModel, connButtonModel, accessModel, dbModel, nodeModel, liveModel, parseRoute,
  menuFocus, dbListKey, dbListReply,
} from '../ui.js';
import { adaptProfile, adaptContainer, adaptState, adaptConnections, adaptDocker } from '../api.js';
import { drawerStep, drawerEscapes } from '../views/cmdline.js';
import { loadDocker, dockerKnown } from '../views/connections.js';
import { createMockServer } from '../mock.js';
import { createApi, createReal } from '../api.js';

const read = (rel) => readFileSync(new URL(`../${rel}`, import.meta.url), 'utf8');

// ---- Placeholders ----

test('the six placeholders, and only those, are split out of a pattern name', () => {
  assert.deepEqual(PLACEHOLDERS, ['id', 'uuid', 'ulid', 'hash', 'ip', 'var']);
  assert.deepEqual(splitPlaceholders('user:{id}:cart'), [{ text: 'user:', ph: false }, { text: '{id}', ph: true }, { text: ':cart', ph: false }]);
  assert.deepEqual(splitPlaceholders('{uuid}'), [{ text: '{uuid}', ph: true }]);
  assert.deepEqual(splitPlaceholders('rl:{ip}:{var}'), [{ text: 'rl:', ph: false }, { text: '{ip}', ph: true }, { text: ':', ph: false }, { text: '{var}', ph: true }]);
  assert.deepEqual(splitPlaceholders('tmp:import:{ulid}:{hash}').filter((p) => p.ph).map((p) => p.text), ['{ulid}', '{hash}']);
  // Braces that are not one of the six stay text: a cluster hash tag, a near miss.
  assert.deepEqual(splitPlaceholders('cart:{user1000}:items'), [{ text: 'cart:{user1000}:items', ph: false }]);
  assert.deepEqual(splitPlaceholders('{ID}:{ids}:{ id }'), [{ text: '{ID}:{ids}:{ id }', ph: false }]);
  assert.deepEqual(splitPlaceholders('queue:emails'), [{ text: 'queue:emails', ph: false }]);
  assert.deepEqual(splitPlaceholders(''), []);
  assert.deepEqual(splitPlaceholders(null), []);
});

test('a literal row is never split: it is a key name, not a pattern', () => {
  assert.deepEqual(splitPlaceholders('config:{id}', true), [{ text: 'config:{id}', ph: false }]);
  // Browse passes it wherever its data knows: the tree row, and through the tree the value's
  // Pattern line and the bulk title. (Analysis rows carry no literal flag: an analysis
  // names patterns, never single keys.)
  const browse = read('views/browse.js');
  assert.match(browse, /patternName\(r\.pattern\.name, \{ literal: r\.literal === true/);
  assert.match(browse, /patternName\(v\.pattern\.name, \{ literal: literalOf\(v\.pattern\) \}\)/);
  assert.match(browse, /patternName\(b\.pattern\.name, \{ literal: literalOf\(b\.pattern\) \}\)/);
});

test('the parts give the name back exactly', () => {
  for (const name of ['a{id}{id}b', '{id}', 'x{uuid', 'session:{uuid}', '{hash}{ip}{var}{ulid}', 'é:{id}:ü']) {
    assert.equal(splitPlaceholders(name).map((p) => p.text).join(''), name);
  }
});

// ---- The connection menu ----

const idle = { status: 'idle', connected: false, connecting: null, failed: null, conn: null, lost: null, analysis: { running: false, db: 0, run: 0 } };

const savedJson = {
  name: 'prod-cache-01', env: 'prod', env_label: 'PROD', endpoint: 'cache.example.internal:6379', url: 'rediss://alice@cache.example.internal:6379/2',
  tls: true, db: 2, auth: 'keychain', tags: ['team-a'], read_only: true, writes: false,
};
const dockerJson = { container: 'demo-redis', endpoint: '127.0.0.1:6390', url: 'redis://bob@127.0.0.1:6390', image: 'redis:7.4', auth: 'none' };

test('the menu lists saved connections and Docker containers with name, environment, address and target', () => {
  const saved = adaptProfile(savedJson);
  const box = adaptContainer(dockerJson);
  const m = connMenuModel([saved], [box], idle);
  assert.deepEqual(m.saved, [{
    key: 'saved:prod-cache-01', name: 'prod-cache-01', env: 'PROD', address: 'cache.example.internal:6379 · TLS · db 2',
    docker: false, target: { name: 'prod-cache-01' }, current: false, status: null,
  }]);
  assert.deepEqual(m.docker, [{
    key: 'docker:demo-redis', name: 'demo-redis', env: 'LOCAL', address: '127.0.0.1:6390 · Docker',
    docker: true, target: { docker: 'demo-redis' }, current: false, status: null,
  }]);
});

test('no user name, password or URL reaches the menu', () => {
  const m = connMenuModel([adaptProfile(savedJson)], [adaptContainer(dockerJson)], idle);
  for (const item of m.saved.concat(m.docker)) {
    for (const key of Object.keys(item)) assert.doesNotMatch(key, /user|pass|secret|url|auth|token/i, key);
    const text = JSON.stringify(item);
    assert.doesNotMatch(text, /alice|bob|rediss?:\/\//, text);
  }
});

test('the menu marks the connection the session is on, and how it is', () => {
  const saved = adaptProfile(savedJson);
  const box = adaptContainer(dockerJson);
  const conn = { name: 'demo-redis', docker: true };
  const on = connMenuModel([saved], [box], { ...idle, connected: true, conn });
  assert.equal(on.docker[0].current, true);
  assert.equal(on.docker[0].status, 'connected');
  assert.equal(on.saved[0].current, false);
  // A saved connection of the same name as a container is another thing.
  const same = connMenuModel([{ ...saved, name: 'demo-redis' }], [], { ...idle, connected: true, conn });
  assert.equal(same.saved[0].current, false);
  const connecting = connMenuModel([saved], [], { ...idle, conn: { name: 'prod-cache-01', docker: false }, connecting: { name: 'prod-cache-01' } });
  assert.equal(connecting.saved[0].status, 'connecting');
  const failed = connMenuModel([saved], [], { ...idle, conn: { name: 'prod-cache-01', docker: false }, failed: 'refused' });
  assert.equal(failed.saved[0].status, 'failed');
  assert.deepEqual(connMenuModel(null, undefined, idle), { saved: [], docker: [] });
});

test('the connection button: connected, connecting, failed, not connected', () => {
  const conn = { name: 'demo', env: 'LOCAL', docker: false, readOnly: false };
  assert.deepEqual(connButtonModel({ ...idle, connected: true, conn }), { dot: 'ok', name: 'demo', env: 'LOCAL', note: null, aria: 'Connected to demo, LOCAL. Switch connection.' });
  assert.equal(connButtonModel({ ...idle, connected: true, conn, lost: 'closed' }).dot, 'busy');
  assert.equal(connButtonModel({ ...idle, conn, analysis: { running: true, db: 0 } }).note, 'analyzing…');
  assert.deepEqual(connButtonModel({ ...idle, conn, connecting: { name: 'demo' } }).note, 'connecting…');
  assert.equal(connButtonModel({ ...idle, conn, failed: 'refused' }).dot, 'bad');
  assert.deepEqual(connButtonModel(idle), { dot: null, name: null, env: null, note: 'Not connected', aria: 'Not connected. Pick a connection.' });
});

// ---- Access ----

test('the access button: read-only, writes on, and nothing without a connection', () => {
  const ro = accessModel({ ...idle, connected: true, conn: { name: 'prod', readOnly: true, writesOff: 'prod is read-only, the default for production.' } });
  assert.equal(ro.show, true);
  assert.equal(ro.label, 'Read-only');
  assert.equal(ro.on, false);
  assert.equal(ro.next, true, 'a click asks to turn writes on');
  assert.equal(ro.tone, 'warn');
  assert.match(ro.title, /^prod is read-only, the default for production\. Click to turn writes on: it asks first\.$/);
  const on = accessModel({ ...idle, connected: true, conn: { name: 'demo', readOnly: false } });
  assert.equal(on.label, 'Writes on');
  assert.equal(on.next, false);
  assert.equal(on.tone, 'danger');
  assert.match(on.aria, /Turn writes off/);
  assert.deepEqual(accessModel(idle), { show: false });
  assert.deepEqual(accessModel({ ...idle, conn: { name: 'demo', readOnly: false }, connecting: { name: 'demo' } }), { show: false });
  assert.deepEqual(accessModel({ ...idle, conn: { name: 'demo', readOnly: false }, failed: 'refused' }), { show: false });
  assert.equal(accessModel({ ...idle, conn: { name: 'demo', readOnly: true }, analysis: { running: true, db: 0 } }).show, true, 'while an analysis runs');
});

// ---- Database and node ----

test('the database control shows db N from the state, and is off on a cluster', () => {
  const conn = { name: 'demo', docker: false, topology: 'standalone', nodes: [] };
  assert.deepEqual(dbModel({ ...idle, connected: true, conn, analysis: { running: false, db: 3 } }), { show: true, db: 3, label: 'db 3', off: null });
  assert.equal(dbModel({ ...idle, connected: true, conn, analysis: null }).label, 'db 0');
  const cluster = { ...conn, topology: 'cluster', nodes: [{ id: '0' }, { id: '1' }] };
  assert.deepEqual(dbModel({ ...idle, connected: true, conn: cluster }), { show: true, db: 0, label: 'db 0', off: 'A cluster only has database 0.' });
  assert.deepEqual(dbModel(idle), { show: false });
});

test('the node select lists the nodes with the picked one selected, on a cluster only', () => {
  const nodes = [{ id: '0', address: '10.0.1.11:6379' }, { id: '1', address: '10.0.1.12:6379' }, { id: '2', address: '10.0.1.13:6379' }];
  const m = nodeModel({ ...idle, connected: true, conn: { nodes, pickedNode: '1' } });
  assert.equal(m.picked, '1');
  assert.deepEqual(m.options, [
    { value: '0', text: '10.0.1.11:6379 · 1 of 3', selected: false },
    { value: '1', text: '10.0.1.12:6379 · 2 of 3', selected: true },
    { value: '2', text: '10.0.1.13:6379 · 3 of 3', selected: false },
  ]);
  assert.equal(nodeModel({ ...idle, connected: true, conn: { nodes: [nodes[0]], pickedNode: '0' } }), null);
  assert.equal(nodeModel({ ...idle, connected: true, conn: { nodes: [], pickedNode: null } }), null);
  assert.equal(nodeModel({ ...idle, conn: { nodes, pickedNode: '0' } }), null, 'not before the connect is done');
});

test('the models read the state the server sends', () => {
  const state = adaptState({
    status: 'connected', live: 'ready', docker: false, writes: false, writes_off: 'read-only',
    connection: { name: 'c', env: 'stage', endpoint: 'h:1', url: 'redis://carol@h:1' },
    server: { mode: 'cluster' }, nodes: [{ index: 0, endpoint: 'a:1' }, { index: 1, endpoint: 'b:1' }], picked: 1,
    analysis: { db: 0, status: 'none' }, polling: { interval_secs: 5, intervals: [1, 5], paused: true },
  });
  assert.equal(accessModel(state).label, 'Read-only');
  assert.equal(dbModel(state).off, 'A cluster only has database 0.');
  assert.deepEqual(nodeModel(state).options.map((o) => o.selected), [false, true]);
  const live = liveModel(state);
  assert.equal(live.label, 'Paused');
  assert.equal(live.every, 5);
  assert.deepEqual(live.options, [{ value: '1', text: 'every 1 s' }, { value: '5', text: 'every 5 s' }]);
  assert.equal(liveModel(adaptState({ status: 'idle' })).enabled, false);
});

// ---- Menu focus and the database list ----

test('a menu focuses the current item when it opens, so enter keeps the current database', () => {
  const dbs = [{ key: 'db0' }, { key: 'db1' }, { key: 'db3', current: true }];
  assert.equal(menuFocus(dbs), 2, 'on db 3 the focus starts on db 3, not db 0');
  assert.equal(menuFocus([{ key: 'db0' }, { key: 'db1' }]), 0, 'no current item: the first');
  assert.equal(menuFocus([{ key: 'db0', disabled: true }, { key: 'db1' }]), 1, 'a disabled item is skipped');
  assert.equal(menuFocus([{ key: 'a', current: true, disabled: true }, { key: 'b' }]), 1);
  assert.equal(menuFocus([]), -1);
  // A redraw (the list read again) before the user moved: back to the current one.
  assert.equal(menuFocus(dbs, { had: 'db0', moved: false }), 2);
  // Once the user moved, the item they are on keeps the focus.
  assert.equal(menuFocus(dbs, { had: 'db1', moved: true }), 1);
  assert.equal(menuFocus(dbs, { had: 'gone', moved: true }), 2, 'their item went: the current one');
  assert.equal(menuFocus(dbs, { had: null, moved: true }), -1, 'focus they put elsewhere stays there');
});

test('a database list is kept only for the session and run it was asked for', () => {
  const s = (name, run, docker = false) => ({ conn: { name, docker }, analysis: { run } });
  assert.equal(dbListKey(s('demo', 3)), 'demo|false|3');
  assert.equal(dbListKey(s('demo', 3, true)), 'demo|true|3');
  assert.equal(dbListKey({ conn: null }), null);
  const dbs = { rows: [{ db: 0 }] };
  assert.deepEqual(dbListReply('demo|false|3', 'demo|false|3', dbs), { who: 'demo|false|3', dbs, error: null });
  assert.deepEqual(dbListReply('demo|false|3', 'demo|false|3', null, 'refused'), { who: 'demo|false|3', dbs: null, error: 'refused' });
  // The key is the one taken when the request was sent: a reply after a switch (a new run)
  // or another connect is dropped, never stamped with the new key.
  assert.equal(dbListReply(dbListKey(s('demo', 3)), dbListKey(s('demo', 4)), dbs), null);
  assert.equal(dbListReply(dbListKey(s('demo', 3)), dbListKey(s('other', 3)), dbs), null);
  assert.equal(dbListReply(null, null, dbs), null);
  const app = read('app.js');
  assert.match(app, /draw\(known && known\.dbs, known && known\.error, true\)/, 'a kept error is drawn, not dropped');
  const browse = read('views/browse.js');
  assert.ok(browse.indexOf('const key = ctx.dbListKey') < browse.indexOf('store.dbs = await ctx.api.getDbs()'), 'Browse takes the key before it asks');
});

// ---- Docker discovery is shared ----

test('the menu and Connections share one Docker discovery', async () => {
  let asked = 0;
  const api = { listDocker: async () => { asked += 1; return { containers: [{ name: 'x' }] }; } };
  const [a, b] = await Promise.all([loadDocker(api), loadDocker(api)]);
  assert.equal(asked, 1, 'two at once ask once');
  assert.equal(a, b);
  await loadDocker(api);
  assert.equal(asked, 1, 'later ones reuse what was found');
  assert.equal(dockerKnown(), a);
  await loadDocker(api, true);
  assert.equal(asked, 2, 'Look again asks again');
});

test('a late answer to an older Docker ask never replaces a newer one', async () => {
  const replies = [];
  const api = { listDocker: () => new Promise((resolve) => replies.push(resolve)) };
  const first = loadDocker(api, true);
  const again = loadDocker(api, true);
  const newer = { containers: [{ name: 'new' }] };
  const older = { containers: [{ name: 'old' }] };
  replies[1](newer);
  assert.equal(await again, newer);
  replies[0](older);
  assert.equal(await first, newer, 'the older caller gets the newest answer');
  assert.equal(dockerKnown(), newer, 'and the kept answer is still the newest');
  // The other order: the older one first, while the newer is still asking.
  const a = loadDocker(api, true);
  const b = loadDocker(api, true);
  const latest = { containers: [{ name: 'latest' }] };
  replies[2]({ containers: [{ name: 'stale' }] });
  replies[3](latest);
  assert.equal(await a, latest);
  assert.equal(await b, latest);
  assert.equal(dockerKnown(), latest);
});

test('in ?mock: the menu lists the sample connections and containers, without credentials', async () => {
  const server = createMockServer();
  const api = createApi(createReal({ ...server, mock: true }));
  const state = await api.getState();
  const list = await api.listConnections();
  const docker = await api.listDocker();
  const m = connMenuModel(list.profiles, docker.containers, state);
  assert.deepEqual(m.saved.map((i) => i.name), ['prod-cache-01', 'staging-cache', 'demo-cluster', 'limited-user']);
  assert.deepEqual(m.docker.map((i) => [i.name, i.target]), [['demo-redis', { docker: 'demo-redis' }], ['demo-valkey', { docker: 'demo-valkey' }], ['legacy-redis', { docker: 'legacy-redis' }]]);
  assert.doesNotMatch(JSON.stringify(m), /password|username|:\/\//i);
  assert.equal(typeof adaptConnections, 'function');
  assert.equal(typeof adaptDocker, 'function');
});

// ---- The command-line drawer ----

test('the drawer opens and closes without touching the route', () => {
  const hash = '#/server/slowlog';
  const before = parseRoute(hash, ['overview', 'server'], 'overview');
  let s = { open: false };
  s = drawerStep(s, 'toggle');
  assert.deepEqual(s, { open: true, focus: 'input' });
  s = drawerStep(s, 'open');
  assert.deepEqual(s, { open: true, focus: 'input' });
  s = drawerStep(s, 'close', true);
  assert.deepEqual(s, { open: false, focus: 'toggle' }, 'the focus goes back to the rail when it was inside');
  assert.deepEqual(drawerStep({ open: true }, 'toggle', false), { open: false, focus: null });
  assert.deepEqual(drawerStep({ open: false }, 'close'), { open: false, focus: null });
  for (const step of [drawerStep({ open: false }, 'open'), drawerStep({ open: true }, 'close')]) {
    assert.deepEqual(Object.keys(step).sort(), ['focus', 'open'], 'a step holds no route');
  }
  assert.deepEqual(parseRoute(hash, ['overview', 'server'], 'overview'), before);
});

test('escape closes the drawer from anywhere inside it, once', () => {
  const key = (k, more = {}) => ({ key: k, defaultPrevented: false, ctrlKey: false, metaKey: false, altKey: false, shiftKey: false, ...more });
  assert.equal(drawerEscapes(key('Escape')), true, 'from Clear, Close, the pills or the output');
  assert.equal(drawerEscapes(key('Escape', { defaultPrevented: true })), false, 'the input handled it already');
  assert.equal(drawerEscapes(key('Escape', { ctrlKey: true })), false);
  assert.equal(drawerEscapes(key('Enter')), false);
  assert.match(read('views/cmdline.js'), /root\.addEventListener\('keydown', \(e\) => \{\n\s+if \(drawerEscapes\(e\)\)/);
});

test('opening the drawer scrolls nothing and changes no address', () => {
  const app = read('app.js');
  const body = app.slice(app.indexOf('function setCommandLine'), app.indexOf('\n}\n', app.indexOf('function setCommandLine')));
  assert.doesNotMatch(body, /scrollIntoView|location|navigate|history\./);
  assert.match(read('views/cmdline.js'), /focus: \(\) => input\.focus\(\{ preventScroll: true \}\)/);
  const html = read('index.html');
  assert.match(html, /<section class="cmd-drawer" id="cmdline" aria-label="Command line" hidden><\/section>\n<\/main>/, 'the drawer is outside the view, in the main column');
});

// ---- The markup ----

test('the context bar and the rail', () => {
  const html = read('index.html');
  const bar = html.slice(html.indexOf('<header class="topbar">'), html.indexOf('</header>'));
  const order = ['class="brand"', 'id="conn-btn"', 'id="access-btn"', 'id="db-btn"', 'id="node-select"', 'id="find-form"', 'id="live-toggle"', 'id="poll-interval"', 'id="theme-toggle"'];
  let at = -1;
  for (const mark of order) {
    const i = bar.indexOf(mark);
    assert.ok(i > at, `${mark} comes next in the top bar`);
    at = i;
  }
  assert.match(bar, /<button class="btn ctx-btn" id="conn-btn" type="button" aria-haspopup="true" aria-expanded="false" aria-controls="conn-menu">/);
  assert.match(bar, /<label for="node-select">Node<\/label>/);
  assert.match(bar, /<label class="sr-only" for="poll-interval">/);
  assert.doesNotMatch(bar, /field poll/, 'the interval is a visible select, not a transparent one');
  assert.doesNotMatch(html, /nav-note|served-at/, 'the long note left the rail');
  assert.doesNotMatch(bar, /title="Served by/, 'no tooltip on the brand, which no keyboard reaches');
  assert.match(read('views/connections.js'), /'Served by the redis-lens running in your terminal at '/, 'Connections says it in text');
  assert.match(read('app.js'), /class: 'glyph bad', 'aria-hidden': 'true', text: '✗'/, 'the banner\'s ✗ is the danger colour');
  const nav = html.slice(html.indexOf('<nav class="nav"'), html.indexOf('</nav>'));
  // Each rail entry's key is its `data-hint` (the one way keys are shown), with its label.
  const hints = Array.from(nav.matchAll(/aria-keyshortcuts="([^"]+)" data-hint="([^"]+)">\n<svg[^\n]*\n<span>([^<]+)<\/span>/g), (m) => {
    assert.equal(m[1], m[2], 'the hint is the key');
    return [m[3], m[2]];
  });
  assert.deepEqual(hints, [['Overview', '1'], ['Browse', '2'], ['Analyze', '3'], ['Server', '4'], ['Pub/Sub', '5'], ['Connections', '0'], ['Command line', ':']]);
  assert.ok(nav.indexOf('nav-sep') > nav.indexOf('data-nav="pubsub"') && nav.indexOf('nav-sep') < nav.indexOf('data-nav="connections"'), 'a rule before Connections');
  assert.doesNotMatch(html, /key-hint/, 'no second kind of hint in the markup');
  assert.doesNotMatch(read('app.js'), /key-hint/, 'nor drawn by the chrome');
  assert.match(read('app.css'), /body\.keys-on \[data-hint\]::after \{/, 'the key hints wait for keys-on');
});

test('the views no longer draw a node or database picker of their own', () => {
  for (const file of ['views/overview.js', 'views/server.js', 'views/pubsub.js', 'views/search.js', 'views/browse.js']) {
    const src = read(file);
    assert.doesNotMatch(src, /nodePicker/, file);
  }
  assert.doesNotMatch(read('views/browse.js'), /id: 'browse-db'/);
  assert.match(read('views/browse.js'), /'Patterns of'/, 'Browse keeps its "Patterns of" filter');
  assert.doesNotMatch(read('ui.js'), /export function nodePicker/);
});

test('destructive buttons carry the danger class', () => {
  const browse = read('views/browse.js');
  assert.match(browse, /writeButton\('Delete key', [^\n]*danger: true/);
  assert.match(browse, /writeButton\(e\.remove\.label, [^\n]*danger: true/);
  assert.match(browse, /writeButton\('Delete every key…', [^\n]*danger: true/);
  assert.match(read('views/connections.js'), /class: 'btn quiet danger', text: 'Remove'/);
  assert.match(read('views/search.js'), /class: 'btn danger', text: 'Drop index'/);
  assert.match(read('views/streams.js'), /class: 'btn danger', text: 'Trim'/);
});
