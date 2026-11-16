// Browse › Keys in three panes, and its state in the address: building and reading the
// query, what a route change reads, which panes show at a width, the breadcrumb, and the
// other views' routes as they were.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import {
  NO_PARAMS, parseBrowseQuery, browseQuery, browseHref, cleanParams, sameParams, opensAnything, shownParams, linkFrom,
  routeLoads, paneLayout, revealPane, crumbsModel, nameOfId, hrefFor, blank, follow, PANES, WIDE, NARROW, MAX_FIND,
} from '../views/browse.js';
import { parseRoute } from '../ui.js';

const read = (rel) => readFileSync(new URL(`../${rel}`, import.meta.url), 'utf8');
const hex = (s) => Buffer.from(s, 'utf8').toString('hex');
const params = (o = {}) => ({ ...NO_PARAMS, ...o });

// ---- The query: built and read back ----

test('the query round-trips: a binary key name, a pattern id of reserved characters, Find text with *?[]&=#%', () => {
  const binary = Buffer.from([0x00, 0xff, 0x1b, 0x26, 0x3d, 0x23, 0x25, 0x80]).toString('hex');
  const cases = [
    params(),
    params({ p: hex('user:{id}:cart') }),
    params({ p: hex('a&b=c#d%e?f/g h'), k: binary }),
    params({ k: binary }),
    params({ find: '*?[]&=#%' }),
    params({ find: ' cart & co ?x=1#y %41 ' }),
    params({ find: 'é:日本:ü', node: 0 }),
    params({ p: hex('session:{uuid}'), k: hex('session:7f3a'), find: 'sess*', node: 12, sort: 'keys' }),
    params({ sort: 'no_ttl' }),
  ];
  for (const c of cases) {
    const q = browseQuery(c);
    assert.deepEqual(parseBrowseQuery(q), c, q);
    assert.deepEqual(parseBrowseQuery(`?${q}`), c, 'a leading ? is read the same');
    // The query holds only what URLs may carry raw: no #, no space, no & or = in a value.
    assert.doesNotMatch(q, /[#\s%](?![0-9A-F]{2})/);
    for (const part of q ? q.split('&') : []) assert.equal(part.split('=').length, 2, part);
    // Through the router too.
    const route = parseRoute(browseHref(c), ['browse', 'overview'], 'overview');
    assert.equal(route.name, 'browse');
    assert.equal(route.sub, null);
    assert.deepEqual(parseBrowseQuery(route.query), c);
  }
  assert.equal(browseHref(params()), '#/browse');
  assert.equal(browseHref(params({ k: '00ff' })), '#/browse?k=00ff');
  assert.equal(browseQuery(params({ p: '6162', k: '63', find: 'x', node: 2, sort: 'name' })), 'p=6162&k=63&find=x&node=2&sort=name', 'a fixed order');
});

test('a malformed or unknown part is left out, never thrown on', () => {
  const bad = [
    'p=xyz', 'p=abc', 'p=6', 'k=%E0%A4%A', 'k=%zz', 'find=', 'find=%20%20', 'find=%E0%A4%A', 'node=-1', 'node=1.5', 'node=abc', 'node=12345',
    'sort=bogus', 'sort=constructor', 'constructor=1', '__proto__=61', 'toString=x', '&&&', '=', '==', 'p', 'k=', '%', '?',
  ];
  for (const q of bad) assert.deepEqual(parseBrowseQuery(q), NO_PARAMS, q);
  assert.deepEqual(parseBrowseQuery(null), NO_PARAMS);
  assert.deepEqual(parseBrowseQuery(undefined), NO_PARAMS);
  assert.deepEqual(parseBrowseQuery(42), NO_PARAMS);
  // Good parts beside bad ones are kept; the first of a name wins; hex is read in lower case.
  assert.deepEqual(parseBrowseQuery('x=1&p=6162&p=63&k=4A4b&k=ZZ&node=3&sort=memory&find=a%26b'), params({ p: '6162', k: '4a4b', node: 3, sort: 'memory', find: 'a&b' }));
  assert.deepEqual(parseBrowseQuery('p=zz&p=6162'), params(), 'a bad first one is not replaced by a later one');
  // Too long: an id past the server's limit, a Find past the page's.
  assert.equal(parseBrowseQuery(`k=${'ab'.repeat(65537)}`).k, null);
  assert.equal(parseBrowseQuery(`k=${'ab'.repeat(65536)}`).k.length, 131072);
  assert.equal(parseBrowseQuery(`find=${'a'.repeat(MAX_FIND + 1)}`).find, null);
  // What can't be put in an address is not built either.
  assert.equal(browseQuery({ p: 'not hex', k: 61, find: '   ', node: -2, sort: 'nope' }), '');
  assert.deepEqual(cleanParams({ p: 'ABCD', k: '61', find: 'x', node: 1.5 }), params({ k: '61', find: 'x' }));
});

test('the same state is the same; only a pattern, key, Find or node opens anything', () => {
  assert.ok(sameParams(params({ k: '61' }), { k: '61', p: null }));
  assert.ok(!sameParams(params({ k: '61' }), params({ k: '62' })));
  assert.equal(opensAnything(params()), false);
  assert.equal(opensAnything(params({ sort: 'keys' })), false, 'a sort alone opens nothing');
  for (const o of [{ p: '61' }, { k: '61' }, { find: 'a' }, { node: 0 }]) assert.equal(opensAnything(params(o)), true);
});

test('what Browse shows, as an address names it', () => {
  const s = blank();
  assert.deepEqual(shownParams(s), NO_PARAMS);
  Object.assign(s, { picked: { id: '6162', name: 'ab' }, valueId: '63', finding: 'x*', node: 1, sort: 'keys' });
  assert.deepEqual(shownParams(s), params({ p: '6162', k: '63', find: 'x*', node: 1, sort: 'keys' }));
});

test('another area\'s jump: a pattern drops the Find and the key; a key and Find keep the rest', () => {
  const base = params({ p: '6162', k: '63', find: 'x*', node: 2, sort: 'keys' });
  assert.equal(linkFrom(base, { p: '64' }), '#/browse?p=64&node=2&sort=keys');
  assert.equal(linkFrom(base, { k: '65' }), '#/browse?p=6162&k=65&find=x*&node=2&sort=keys');
  assert.equal(linkFrom(params({ p: '6162' }), { find: 'cart' }), '#/browse?p=6162&find=cart', 'Find keeps the pattern to go back to');
  assert.equal(linkFrom(base, { node: 0 }), '#/browse?p=6162&k=63&find=x*&node=0&sort=keys', 'a node alone keeps the rest');
  assert.equal(linkFrom(null, { k: '61' }), '#/browse?k=61');
  // Browse holds nothing for a session it has not seen: nothing is carried over.
  const state = { conn: { name: 'demo', docker: false }, analysis: { run: 1 } };
  assert.equal(hrefFor({ k: '61' }, state), '#/browse?k=61');
  assert.equal(hrefFor({ p: '61', node: 3 }, null), '#/browse?p=61&node=3');
});

// ---- What a route change reads ----

test('a route change reads only what changed: the value for a key, the keys for a pattern, nothing for the same', () => {
  const who = 'demo|false|1';
  const at = (o) => ({ who, params: params(o) });
  const none = { tree: false, keys: false, clearList: false, value: false, clearValue: false };
  const P = '757365723a7b69647d3a63617274';
  const K = '757365723a31303239333a63617274';
  // The same state: nothing.
  assert.deepEqual(routeLoads(at({ p: P, k: K, sort: 'memory' }), at({ p: P, k: K, sort: 'memory' })), none);
  assert.deepEqual(routeLoads(at({}), at({})), none);
  // Another key: the value only.
  assert.deepEqual(routeLoads(at({ p: P }), at({ p: P, k: K })), { ...none, value: true });
  assert.deepEqual(routeLoads(at({ p: P, k: '61' }), at({ p: P, k: K })), { ...none, value: true });
  // Back from the key: the value goes, nothing is read.
  assert.deepEqual(routeLoads(at({ p: P, k: K }), at({ p: P })), { ...none, clearValue: true });
  // Another pattern: its keys only (the open key closes; nothing is read for it).
  assert.deepEqual(routeLoads(at({ p: '61' }), at({ p: P })), { ...none, keys: true });
  assert.deepEqual(routeLoads(at({ p: '61', k: K }), at({ p: P })), { ...none, keys: true, clearValue: true });
  // Back from the pattern: the list goes, nothing is read.
  assert.deepEqual(routeLoads(at({ p: P }), at({})), { ...none, clearList: true });
  // Find: its list; Clear search: the pattern's again; the pattern under a Find is not read.
  assert.deepEqual(routeLoads(at({ p: P }), at({ p: P, find: 'cart' })), { ...none, keys: true });
  assert.deepEqual(routeLoads(at({ p: P, find: 'cart' }), at({ p: P })), { ...none, keys: true });
  assert.deepEqual(routeLoads(at({ find: 'cart' }), at({ p: P, find: 'cart' })), none);
  assert.deepEqual(routeLoads(at({ find: 'cart' }), at({ find: 'carts' })), { ...none, keys: true });
  // The sort and node: the tree only; an address without a sort keeps the one shown.
  assert.deepEqual(routeLoads(at({ p: P, sort: 'memory' }), at({ p: P })), none);
  assert.deepEqual(routeLoads(at({ sort: 'memory' }), at({ sort: 'keys' })), { ...none, tree: true });
  assert.deepEqual(routeLoads(at({ node: 1 }), at({})), { ...none, tree: true });
  assert.deepEqual(routeLoads(at({}), at({ node: 0 })), { ...none, tree: true });
  // Another connection or run, or nothing shown before: everything the address names.
  assert.deepEqual(routeLoads({ who: 'other|false|1', params: params({ p: P, k: K }) }, at({ p: P, k: K })), { ...none, tree: true, keys: true, value: true });
  assert.deepEqual(routeLoads({ who: 'demo|false|0', params: params({ p: P, k: K }) }, at({ p: P, k: K })), { ...none, tree: true, keys: true, value: true });
  assert.deepEqual(routeLoads(null, at({ k: K })), { ...none, tree: true, value: true });
  assert.deepEqual(routeLoads(null, at({})), { ...none, tree: true });
});

test('following a new connection or run forgets what the address named, and the layout\'s anchor', () => {
  const store = blank();
  const state = (name, run) => ({ conn: { name, docker: false }, analysis: { run } });
  follow(store, state('a', 1));
  Object.assign(store, { picked: { id: '61' }, valueId: '62', finding: 'x', anchor: 'value', sort: 'keys' });
  assert.equal(follow(store, state('a', 2)), 'run');
  assert.deepEqual(shownParams(store), params({ sort: 'keys' }), 'the sort stays, as before');
  assert.equal(store.anchor, 'patterns');
  Object.assign(store, { picked: { id: '61' }, anchor: 'keys' });
  assert.equal(follow(store, state('b', 2)), 'reset');
  assert.deepEqual(shownParams(store), NO_PARAMS);
});

// ---- The panes ----

test('the panes shown: three when wide, the anchor and its right-hand pane when medium, one when narrow', () => {
  for (const anchor of PANES) assert.deepEqual(paneLayout(1232, anchor), PANES, `wide, ${anchor}`);
  assert.deepEqual(paneLayout(WIDE, 'value'), PANES);
  assert.deepEqual(paneLayout(892, 'patterns'), ['patterns', 'keys']);
  assert.deepEqual(paneLayout(892, 'keys'), ['keys', 'value']);
  assert.deepEqual(paneLayout(892, 'value'), ['keys', 'value'], 'the last pane shows with the one on its left');
  assert.deepEqual(paneLayout(WIDE - 1, 'patterns'), ['patterns', 'keys']);
  assert.deepEqual(paneLayout(NARROW, 'value'), ['keys', 'value']);
  for (const anchor of PANES) assert.deepEqual(paneLayout(676, anchor), [anchor], `narrow, ${anchor}`);
  assert.deepEqual(paneLayout(NARROW - 1, 'keys'), ['keys']);
  assert.deepEqual(paneLayout(0, 'value'), ['value']);
  assert.deepEqual(paneLayout(676, 'nonsense'), ['patterns'], 'an unknown anchor is the first pane');
  // Opening a pane that is hidden anchors on it; one that shows keeps the anchor.
  assert.equal(revealPane(1232, 'patterns', 'value'), 'patterns');
  assert.equal(revealPane(892, 'patterns', 'keys'), 'patterns', 'a pattern opened beside the patterns: they stay');
  assert.equal(revealPane(892, 'patterns', 'value'), 'value');
  assert.equal(revealPane(892, 'keys', 'patterns'), 'patterns', 'Back up to the patterns shows them');
  assert.equal(revealPane(676, 'patterns', 'keys'), 'keys');
  assert.equal(revealPane(676, 'value', 'keys'), 'keys');
});

test('the breadcrumb: each step, which one ← and → reach, and which are hidden', () => {
  const list = { kind: 'pattern', name: 'user:{id}:cart', literal: false };
  const wide = crumbsModel({ list, key: 'user:10293:cart', shown: PANES, anchor: 'patterns' });
  assert.deepEqual(wide.map((c) => [c.pane, c.kind, c.label]), [['patterns', 'root', 'Patterns'], ['keys', 'pattern', 'user:{id}:cart'], ['value', 'key', 'user:10293:cart']]);
  assert.deepEqual(wide.map((c) => c.current), [false, false, true], 'the last step is where you are');
  assert.ok(wide.every((c) => !c.back && !c.next && !c.off), 'all shown: nothing to reach beyond them');
  assert.equal(wide[1].literal, false);
  // Two panes, keys and value: Patterns is to the left, ← reaches it.
  const medium = crumbsModel({ list, key: 'user:10293:cart', shown: ['keys', 'value'], anchor: 'value' });
  assert.deepEqual(medium.map((c) => [c.back, c.next, c.off]), [[true, false, true], [false, false, false], [false, false, false]]);
  // One pane: the patterns; the open pattern's keys are to the right, → reaches them.
  const narrow = crumbsModel({ list, key: null, shown: ['patterns'], anchor: 'patterns' });
  assert.deepEqual(narrow.map((c) => [c.pane, c.back, c.next, c.off]), [['patterns', false, false, false], ['keys', false, true, true]]);
  const middle = crumbsModel({ list, key: 'k', shown: ['keys'], anchor: 'keys' });
  assert.deepEqual(middle.map((c) => [c.back, c.next]), [[true, false], [false, false], [false, true]]);
  // Find, and nothing open.
  const find = crumbsModel({ list: { kind: 'find', text: '*cart*' }, key: 'user:1:cart' });
  assert.deepEqual(find.map((c) => c.label), ['Patterns', 'Find: *cart*', 'user:1:cart']);
  assert.deepEqual(crumbsModel({}).map((c) => [c.label, c.current]), [['Patterns', true]]);
  assert.deepEqual(crumbsModel({ list: { kind: 'pattern', name: 'config:{id}', literal: true } })[1].literal, true);
});

test('a name from an address, before the server names it: its text when plain, else its hex', () => {
  assert.equal(nameOfId(hex('user:1:cart')), 'user:1:cart');
  assert.equal(nameOfId('00ff'), '0x00ff');
  assert.equal(nameOfId('1b5b33316d'), '0x1b5b33316d', 'a control character is never shown as it is');
});

// ---- The other views' routes, as before ----

/** parseRoute as it was before the query part. */
function oldParseRoute(hash, names, fallback) {
  const parts = String(hash ?? '').replace(/^#\/?/, '').split('/').filter(Boolean);
  const name = names.includes(parts[0]) ? parts[0] : fallback;
  return { name, sub: name === parts[0] ? parts[1] ?? null : null };
}

test('the other views\' routes parse exactly as before', () => {
  const names = ['overview', 'browse', 'analyze', 'server', 'pubsub', 'connections'];
  const hashes = [
    '', '#', '#/', '#/overview', '#/analyze', '#/analyze/big_keys', '#/analyze/expiry', '#/server/slowlog', '#/server/clients', '#/browse',
    '#/browse/streams', '#/browse/search', '#/pubsub', '#/connections', '#/nope', '#/nope/x', '#token=abc', 'overview', '#/server/slowlog/extra',
    '#//analyze', null, undefined,
  ];
  for (const hash of hashes) {
    for (const fallback of ['overview', 'connections']) {
      assert.deepEqual(parseRoute(hash, names, fallback), oldParseRoute(hash, names, fallback), String(hash));
    }
  }
  assert.deepEqual(parseRoute('#/browse?p=61&k=62', names, 'overview'), { name: 'browse', sub: null, query: 'p=61&k=62' });
  assert.deepEqual(parseRoute('#/browse/streams?x=1', names, 'overview'), { name: 'browse', sub: 'streams', query: 'x=1' });
  assert.deepEqual(parseRoute('#/browse?', names, 'overview'), { name: 'browse', sub: null });
  assert.deepEqual(parseRoute('#/browse?find=a%2Fb', names, 'overview'), { name: 'browse', sub: null, query: 'find=a%2Fb' }, 'a / in the query is not a path');
  assert.deepEqual(parseRoute('#/nope?p=61', names, 'overview'), { name: 'overview', sub: null }, 'an unknown area keeps no query');
  assert.deepEqual(parseRoute('#?p=61', names, 'connections'), { name: 'connections', sub: null });
});

// ---- The wiring (app.js and browse.js; they need a page, so their source is checked) ----

test('the jumps from other areas are addresses, and a change inside Browse does not mount it again', () => {
  const app = read('app.js');
  assert.match(app, /findKey\(text\) \{[\s\S]*?ctx\.navigate\(browse\.hrefFor\(\{ find: q \}, store\.state\)\)/);
  assert.match(app, /openKey\(name\) \{\n\s+ctx\.navigate\(browse\.hrefFor\(\{ k: name\.id \}, store\.state\)\);/);
  assert.match(app, /openPattern\(name, node = null\) \{[\s\S]*?ctx\.navigate\(browse\.hrefFor\(change, store\.state\)\);/);
  assert.doesNotMatch(app, /requestFind|requestKey|requestPattern/);
  // Same screen, same session: the view is told the route; it is not torn down.
  assert.match(app, /mounted\.key === key\) \{\n[^\n]*\n[^\n]*\n\s+if \(typeof VIEWS\[route\.name\]\.onRoute === 'function'\) VIEWS\[route\.name\]\.onRoute\(route\);\n\s+return;/);
  assert.match(app, /window\.history\.replaceState\(window\.history\.state, '', hash\)/);
  const browse = read('views/browse.js');
  assert.doesNotMatch(browse, /pending/);
  // Opening is a step in the history; the route change does the read.
  assert.match(browse, /onclick: \(\) => go\(\{ p: r\.pattern\.id, k: null, find: null \}\)/);
  assert.match(browse, /onclick: \(\) => go\(\{ k: r\.id \}\)/);
  // Nothing reads as the selection moves or a loaded list is filtered: the filters only draw.
  const filters = browse.match(/oninput: \(\) => \{[\s\S]*?\n\s+\},/g) || [];
  assert.equal(filters.length, 2);
  for (const f of filters) assert.doesNotMatch(f, /ctx\.api|go\(|syncUrl|navigate/);
  // The address never holds a connection, a credential or a value.
  assert.doesNotMatch(browseQuery({ p: '61', k: '62', find: 'x', node: 1, sort: 'keys', conn: 'demo', password: 'x', value: 'y' }), /conn|password|value/);
});

test('three panes fill the window: no height caps on Browse › Keys, each pane scrolls inside itself', () => {
  const css = read('app.css');
  assert.doesNotMatch(css, /\.browse \.tree|browse-col\.keys > \.list|max-height: 560px/);
  assert.match(css, /body\.fill-view \.app \{ height: 100vh; height: 100dvh; min-height: 0; \}/);
  assert.match(css, /\.pane-scroll \{ position: relative; flex: 1 1 auto; min-height: 0; overflow-y: auto;/);
  assert.match(css, /\.pane\.pane-off \{ display: none; \}/);
  assert.match(css, /\.pane\.pats \{ flex: 26 1 0;[^}]*\}\n\.pane\.keys \{ flex: 32 1 0;[^}]*\}\n\.pane\.value \{ flex: 42 1 0;/);
});
