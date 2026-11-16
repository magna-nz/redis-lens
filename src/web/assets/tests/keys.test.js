// The keys (docs/ui-redesign.md §5, §7, §8, §12): the one key table and everything built
// from it: the dispatcher, list navigation, the digits, the hints, the `?` sheet, Go to and
// the reasons a bar of buttons shows when some are off.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import {
  AREAS, ACTS, KEYS, actsOf, conflicts, keyOf, keyLabel, ariaKey, dispatch, hintFor, entryFor, listMove, paneStep,
  slashTarget, selectedRow, screenOf, hashOf, areaHash, screenList, panelsOf, allScreens, onScreen, sheetModel,
  screenActions, gotoModel, gotoFlat, gotoStep, matchRank, reasonLines, readOnlyLine,
} from '../keys.js';
import { PANELS as BROWSE_PANELS, readOnlyText, writeOff } from '../views/browse.js';
import { PANELS as ANALYZE_PANELS } from '../views/analyze.js';
import { TABS as SERVER_TABS } from '../views/server.js';

const read = (rel) => readFileSync(new URL(`../${rel}`, import.meta.url), 'utf8');
const sources = ['index.html', 'app.js', ...readdirSync(new URL('../views/', import.meta.url)).map((f) => `views/${f}`)];

/** Every action id a source puts on a control: `'data-act': …`, `act: '…'`, `data-act="…"`. */
function carried() {
  const out = new Map();
  const add = (id, file) => {
    if (!out.has(id)) out.set(id, new Set());
    out.get(id).add(file);
  };
  for (const file of sources) {
    const src = read(file);
    for (const m of src.matchAll(/'data-act': ([^,}\n]*)/g)) {
      // `cond ? 'a' : 'b'`: the values are the branches, not what the condition compares.
      const values = m[1].includes('?') ? m[1].slice(m[1].indexOf('?')) : m[1];
      for (const q of values.matchAll(/'([a-z][\w.]*)'/g)) add(q[1], file);
    }
    for (const m of src.matchAll(/\bact: '([a-z][\w.]*)'/g)) add(m[1], file);
    for (const m of src.matchAll(/data-act="([a-z][\w.]*)"/g)) add(m[1], file);
  }
  return out;
}

/** The keys of app.js's APP handler map. */
function appHandlers() {
  const app = read('app.js');
  const start = app.indexOf('const APP = {');
  assert.ok(start >= 0, 'app.js has the APP handler map');
  const block = app.slice(start, app.indexOf('\n};', start));
  return Array.from(block.matchAll(/^\s+'([^']+)':/gm), (m) => m[1]);
}

// ---- The table ----

test('every key of the table runs an action that has a handler, and every handler has a key', () => {
  const used = new Set(KEYS.flatMap(actsOf));
  for (const act of used) assert.ok(ACTS[act], `${act} is an action`);
  for (const act of Object.keys(ACTS)) assert.ok(used.has(act), `${act} has a key in the table`);
  // The app's own: a handler in app.js for each, and none without an action.
  const app = Object.keys(ACTS).filter((a) => ACTS[a].run === 'app').sort();
  assert.deepEqual(appHandlers().sort(), app);
  // The pressed ones: a control on the page carries each, and no control carries an id the
  // table does not know.
  const on = carried();
  for (const act of Object.keys(ACTS).filter((a) => ACTS[a].run === 'press')) assert.ok(on.has(act), `a control carries data-act="${act}"`);
  for (const [act, files] of on) assert.ok(ACTS[act], `${[...files].join(', ')}: data-act="${act}" is not in the key table`);
});

test('no two entries claim the same key in the same place', () => {
  assert.deepEqual(conflicts(), []);
  // The check itself finds a clash: a second `d` on Browse › Keys' value pane, or a global
  // `w`, collides; the same key in another pane or on another screen does not.
  const extra = (e) => conflicts(KEYS.concat([{ name: 'Extra', act: 'refresh', ...e }]));
  assert.equal(extra({ keys: ['d'], screens: ['browse/keys'], panes: ['value'] }).length, 1);
  assert.deepEqual(extra({ keys: ['d'], screens: ['browse/keys'], panes: ['value'] })[0].key, 'd');
  assert.ok(extra({ keys: ['w'], screens: ['server/clients'] }).length >= 1);
  assert.ok(extra({ keys: ['r'], screens: ['analyze/*'] }).length >= 1, 'analyze/* covers Slots, which has r');
  assert.deepEqual(extra({ keys: ['d'], screens: ['server/clients'] }), []);
  assert.deepEqual(extra({ keys: ['j'], screens: ['overview'] }), [], 'j is a list key; elsewhere it is free');
});

test('every write is a press of its button, never a handler of its own', () => {
  const writes = Object.entries(ACTS).filter(([, a]) => a.write);
  assert.ok(writes.length >= 15);
  for (const [id, a] of writes) assert.equal(a.run, 'press', id);
  for (const id of ['key.delete', 'pattern.bulkDelete', 'item.delete', 'search.drop', 'stream.trim', 'stream.ack', 'publish']) assert.equal(ACTS[id].write, true, id);
  assert.equal(ACTS['key.delete'].sends, 'UNLINK');
  assert.equal(ACTS['stream.ack'].in, 'row', 'an ack acts on its own row only');
  assert.equal(ACTS['conn.remove'].in, 'row');
});

test('the areas and panels are the views\' own', () => {
  const ids = (a) => AREAS.find((x) => x.id === a).panels.map((p) => p.id);
  assert.deepEqual(ids('browse'), BROWSE_PANELS.map((p) => p.id));
  assert.deepEqual(ids('analyze'), ANALYZE_PANELS.map((p) => p.id));
  assert.deepEqual(AREAS.find((x) => x.id === 'analyze').panels.filter((p) => p.cluster).map((p) => p.id), ANALYZE_PANELS.filter((p) => p.cluster).map((p) => p.id));
  assert.deepEqual(ids('server'), SERVER_TABS.map((p) => p.id));
  assert.deepEqual(AREAS.map((a) => a.digit), ['1', '2', '3', '4', '5', '0']);
  // The rail shows the same digits.
  const nav = read('index.html');
  for (const a of AREAS) assert.match(nav, new RegExp(`data-nav="${a.id}" aria-keyshortcuts="${a.digit}"`));
});

test('keys the brief names but the dashboard has no action for, or the browser keeps, are not bound', () => {
  const bound = new Set(KEYS.flatMap((e) => e.keys));
  for (const k of ['ctrl+t', 'ctrl+l', 'ctrl+f', 'v', '.', 'q', 'ctrl+c', '<', '>', 'Tab']) assert.equal(bound.has(k), false, k);
  // Clients `d` (end a connection) and Slow log `e` / `D`: the dashboard has no such action.
  for (const [key, screen] of [['d', 'server/clients'], ['e', 'server/slowlog'], ['D', 'server/slowlog']]) {
    assert.equal(dispatch(key, { screen }), null, `${key} on ${screen}`);
    assert.equal(dispatch(key, { screen, pane: screen.split('/')[1], inList: true }), null, `${key} on ${screen}, in its list`);
  }
});

// ---- Key names ----

test('a key press gets the table\'s name for it', () => {
  assert.equal(keyOf({ key: 'd' }), 'd');
  assert.equal(keyOf({ key: 'D', shiftKey: true }), 'D');
  assert.equal(keyOf({ key: ' ' }), 'Space');
  assert.equal(keyOf({ key: 'k', ctrlKey: true }), 'ctrl+k');
  assert.equal(keyOf({ key: 'K', metaKey: true, shiftKey: true }), 'meta+k');
  assert.equal(keyOf({ key: 'k', ctrlKey: true, metaKey: true }), null);
  assert.equal(keyOf({ key: 'd', altKey: true }), null);
  assert.equal(keyOf({ key: 'ArrowDown', ctrlKey: true }), null);
  assert.equal(keyOf({}), null);
  assert.deepEqual(['ArrowDown', 'Escape', 'Space', 'ctrl+k', 'meta+k', 'D', 'PageDown'].map(keyLabel), ['↓', 'esc', 'space', 'ctrl-k', 'cmd-k', 'D', 'PgDn']);
  assert.deepEqual(['D', 'ctrl+k', '+', 'd', '?'].map(ariaKey), ['Shift+D', 'Control+K', 'Plus', 'd', '?']);
});

// ---- The dispatcher ----

const act = (key, c) => {
  const e = dispatch(key, c);
  return e ? actsOf(e).join('|') : null;
};

test('on Browse › Keys, lower case acts on the selected row and upper case on what it belongs to', () => {
  const value = { screen: 'browse/keys', pane: 'value', inList: true };
  const keys = { screen: 'browse/keys', pane: 'keys', inList: true };
  const patterns = { screen: 'browse/keys', pane: 'patterns', inList: true };
  assert.equal(act('d', value), 'item.delete');
  assert.equal(act('D', value), 'key.delete');
  assert.equal(act('d', keys), 'key.delete');
  assert.equal(act('D', keys), 'pattern.bulkDelete');
  assert.equal(act('d', patterns), 'pattern.bulkDelete');
  assert.equal(act('t', value), 'item.ttl');
  assert.equal(act('T', value), 'key.ttl');
  assert.equal(act('t', keys), 'key.ttl');
  assert.equal(act('T', keys), 'pattern.bulkTtl');
  assert.equal(act('t', patterns), 'pattern.bulkTtl');
  assert.equal(act('y', value), 'copy.value');
  assert.equal(act('Y', value), 'copy.name');
  assert.equal(act('y', patterns), 'copy.pattern');
  assert.equal(act('Y', patterns), 'copy.command');
  assert.equal(act('e', value), 'item.edit|value.edit');
  assert.equal(act('+', value), 'item.add');
  assert.equal(act('+', keys), 'key.add');
  assert.equal(act('R', keys), 'key.rename');
  assert.equal(act('f', value), 'format');
  assert.equal(act('m', keys), 'more');
  assert.equal(act('s', patterns), 'sort');
  // With no pane focused, a pane's keys do nothing; the screen's do.
  assert.equal(act('d', { screen: 'browse/keys' }), null);
  assert.equal(act('m', { screen: 'browse/keys' }), 'more');
  // The pane-bound ones say which row they need open.
  assert.deepEqual(dispatch('d', keys).open, ['keys']);
  assert.deepEqual(dispatch('d', patterns).open, ['patterns']);
});

test('each screen\'s own verbs, and nothing on the screens that lack them', () => {
  assert.equal(act('c', { screen: 'browse/streams' }), 'stream.claim');
  assert.equal(act('A', { screen: 'browse/streams' }), 'stream.ack|stream.pending');
  assert.equal(act('-', { screen: 'browse/streams' }), 'stream.trim');
  assert.equal(act('x', { screen: 'browse/search' }), 'search.explain');
  assert.equal(act('X', { screen: 'browse/search' }), 'search.profile');
  assert.equal(act('+', { screen: 'browse/search' }), 'search.create');
  assert.equal(act('d', { screen: 'browse/search', pane: 'indexes', inList: true }), 'search.drop');
  assert.equal(act('+', { screen: 'pubsub' }), 'sub.add');
  assert.equal(act('S', { screen: 'pubsub' }), 'sub.shard');
  assert.equal(act('P', { screen: 'pubsub' }), 'publish');
  assert.equal(act('+', { screen: 'connections' }), 'conn.add');
  assert.equal(act('e', { screen: 'connections', pane: 'saved', inList: true }), 'conn.edit');
  assert.equal(act('d', { screen: 'connections', pane: 'saved', inList: true }), 'conn.remove');
  assert.equal(act('r', { screen: 'browse/streams' }), 'refresh');
  assert.equal(act('r', { screen: 'overview' }), null, 'Overview has nothing to read again');
  assert.equal(act('a', { screen: 'overview' }), 'analyze');
  assert.equal(act('a', { screen: 'analyze/expiry' }), 'analyze');
  assert.equal(act('c', { screen: 'browse/keys' }), null);
  assert.equal(act('x', { screen: 'pubsub' }), null);
});

test('everywhere: areas, Go to, the sheet, the command line, writes, the database, nodes, pause, snapshot', () => {
  for (const screen of allScreens()) {
    const c = { screen };
    assert.equal(act('1', c), 'area.overview');
    assert.equal(act('0', c), 'area.connections');
    assert.equal(act('ctrl+k', c), 'goto');
    assert.equal(act('meta+k', c), 'goto');
    assert.equal(act('/', c), 'filter');
    assert.equal(act('?', c), 'sheet');
    assert.equal(act(':', c), 'cmdline');
    assert.equal(act('w', c), 'writes');
    assert.equal(act('b', c), 'db');
    assert.equal(act('[', c), 'node.prev');
    assert.equal(act(']', c), 'node.next');
    assert.equal(act('Space', c), 'pause|live');
    assert.equal(act('ctrl+s', c), 'snapshot');
  }
});

test('no key runs in a text box, behind a dialog or menu, or in the command-line drawer', () => {
  const everyKey = Array.from(new Set(KEYS.flatMap((e) => e.keys)));
  for (const screen of allScreens()) {
    for (const pane of [null, 'value', 'keys', 'patterns', 'saved', 'streams']) {
      for (const key of everyKey) {
        const base = { screen, pane, inList: true, onRow: true };
        assert.equal(dispatch(key, { ...base, inInput: true }), null, `${key} in an input on ${screen}`);
        assert.equal(dispatch(key, { ...base, dialog: 'dialog' }), null, `${key} behind a dialog on ${screen}`);
        assert.equal(dispatch(key, { ...base, dialog: 'menu' }), null, `${key} with a menu open on ${screen}`);
        assert.equal(dispatch(key, { ...base, dialog: 'drawer' }), null, `${key} in the drawer on ${screen}`);
      }
    }
  }
  // So no write is ever fired behind a question: the confirm is a dialog.
  for (const key of ['d', 'D', 't', 'T', 'R', 'e', '+', 'w', 'P', '-', 'c', 'A']) {
    assert.equal(dispatch(key, { screen: 'browse/keys', pane: 'value', inList: true, dialog: 'dialog' }), null);
  }
});

test('list keys need a list; Enter and Space stay the browser\'s on a button; Tab is never taken', () => {
  assert.equal(act('j', { screen: 'overview' }), null);
  assert.equal(act('ArrowDown', { screen: 'overview' }), null, 'arrows scroll the page outside a list');
  assert.equal(act('j', { screen: 'overview', pane: 'findings', inList: true }), 'list.down');
  assert.equal(act('ArrowUp', { screen: 'overview', pane: 'findings', inList: true }), 'list.up');
  for (const [k, a] of [['PageDown', 'list.pageDown'], ['PageUp', 'list.pageUp'], ['Home', 'list.home'], ['g', 'list.home'], ['End', 'list.end'], ['G', 'list.end'], ['l', 'list.open'], ['ArrowRight', 'list.open'], ['h', 'list.back'], ['ArrowLeft', 'list.back'], ['Escape', 'list.back']]) {
    assert.equal(act(k, { screen: 'browse/keys', pane: 'keys', inList: true }), a, k);
  }
  // Enter on a button or link presses it as ever (a row's button opens the row); on a row
  // that is not a button (a table row), the keys open it.
  assert.equal(act('Enter', { screen: 'browse/keys', pane: 'keys', inList: true, native: true, onRow: true }), null);
  assert.equal(act('Enter', { screen: 'server/slowlog', pane: 'slowlog', inList: true, native: false, onRow: true }), 'list.open');
  // Space on a button that is not a row presses it; on a row, or nothing, it pauses.
  assert.equal(act('Space', { screen: 'pubsub', native: true, onRow: false }), null);
  assert.equal(act('Space', { screen: 'pubsub', pane: 'channels', inList: true, native: true, onRow: true }), 'pause|live');
  assert.equal(act('Space', { screen: 'pubsub' }), 'pause|live');
  for (const k of ['Tab', 'Shift', 'Control']) assert.equal(dispatch(k, { screen: 'browse/keys', pane: 'keys', inList: true }), null, k);
});

// ---- Lists, panes, digits ----

test('moving in a list: steps, bounds, pages, ends', () => {
  assert.equal(listMove(0, 5, 'list.down'), 1);
  assert.equal(listMove(4, 5, 'list.down'), 4, 'stays on the last row');
  assert.equal(listMove(0, 5, 'list.up'), 0, 'stays on the first row');
  assert.equal(listMove(3, 5, 'list.up'), 2);
  assert.equal(listMove(-1, 5, 'list.down'), 0, 'none selected: the first');
  assert.equal(listMove(-1, 5, 'list.up'), 0);
  assert.equal(listMove(2, 100, 'list.pageDown', 10), 12);
  assert.equal(listMove(95, 100, 'list.pageDown', 10), 99);
  assert.equal(listMove(12, 100, 'list.pageUp', 10), 2);
  assert.equal(listMove(5, 100, 'list.pageUp', 10), 0);
  assert.equal(listMove(-1, 100, 'list.pageDown', 10), 10);
  assert.equal(listMove(40, 100, 'list.home'), 0);
  assert.equal(listMove(40, 100, 'list.end'), 99);
  assert.equal(listMove(9, 3, 'list.down'), 2, 'a list that got shorter');
  assert.equal(listMove(0, 0, 'list.down'), -1, 'an empty list');
  assert.equal(listMove(1, 1, 'list.end'), 0);
});

test('the selected row after a redraw: the one kept, else the open one, else the first', () => {
  const rows = [{ key: 'a', open: false }, { key: 'b', open: true }, { key: 'c', open: false }];
  assert.equal(selectedRow(rows, 'c'), 2);
  assert.equal(selectedRow(rows, 'gone'), 1);
  assert.equal(selectedRow(rows, null), 1);
  assert.equal(selectedRow([{ key: 'a' }, { key: 'b' }], null), 0);
  assert.equal(selectedRow([], 'a'), -1);
});

test('panes to the left and right', () => {
  assert.equal(paneStep(3, 0, 1), 1);
  assert.equal(paneStep(3, 2, 1), -1);
  assert.equal(paneStep(3, 0, -1), -1);
  assert.equal(paneStep(3, 1, -1), 0);
  assert.equal(paneStep(3, -1, 1), -1, 'no pane focused: no step');
});

test('`/` filters the focused list, on Search the query, else opens Go to', () => {
  assert.equal(slashTarget({ paneFilter: true, screenFilter: true }), 'pane');
  assert.equal(slashTarget({ paneFilter: false, screenFilter: true }), 'screen');
  assert.equal(slashTarget({}), 'goto');
  // Search's query box is outside every pane; the list filters are inside theirs.
  const search = read('views/search.js');
  assert.match(search, /placeholder: '\* \(every document\)', 'data-act': 'filter'/);
  assert.doesNotMatch(search.slice(search.indexOf('function queryCard'), search.indexOf('function resultsBox')), /'data-pane'/);
  const browse = read('views/browse.js');
  assert.match(browse, /id: 'browse-pat-filter'[^\n]*'data-act': 'filter'/);
  assert.match(browse, /id: 'browse-key-filter'[^\n]*'data-act': 'filter'/);
});

test('a digit goes to its area; the area\'s digit again goes to its next panel', () => {
  const r = (name, sub = null) => ({ name, sub });
  assert.equal(areaHash('2', r('overview')), '#/browse');
  assert.equal(areaHash('2', r('browse')), '#/browse/streams');
  assert.equal(areaHash('2', r('browse', 'streams')), '#/browse/search');
  assert.equal(areaHash('2', r('browse', 'search')), '#/browse', 'wraps to Keys');
  assert.equal(areaHash('4', r('server', 'clients')), '#/server/slowlog');
  assert.equal(areaHash('4', r('server', 'monitor')), '#/server/clients');
  assert.equal(areaHash('4', r('server')), '#/server/slowlog', 'no panel named: the first is shown');
  assert.equal(areaHash('3', r('analyze', 'hot_keys')), '#/analyze/snapshots', 'Nodes and Slots only on a cluster');
  assert.equal(areaHash('3', r('analyze', 'hot_keys'), { cluster: true }), '#/analyze/nodes');
  assert.equal(areaHash('3', r('analyze', 'snapshots')), '#/analyze/big_keys');
  assert.equal(areaHash('1', r('overview')), '#/overview');
  assert.equal(areaHash('5', r('browse', 'search')), '#/pubsub');
  assert.equal(areaHash('0', r('pubsub')), '#/connections');
  assert.equal(areaHash('9', r('overview')), null);
  assert.equal(screenOf(r('browse')), 'browse/keys');
  assert.equal(screenOf(r('browse', 'nope')), 'browse/keys');
  assert.equal(screenOf(r('server', 'slowlog')), 'server/slowlog');
  assert.equal(screenOf(r('analyze')), 'analyze/big_keys');
  assert.equal(screenOf(r('pubsub', 'x')), 'pubsub');
  assert.equal(hashOf('browse', 'keys'), '#/browse');
  assert.deepEqual(panelsOf('analyze').map((p) => p.id), ['big_keys', 'expiry', 'changes', 'hot_keys', 'snapshots']);
  assert.ok(onScreen(['analyze/*'], 'analyze/slots'));
  assert.equal(onScreen(['analyze/*'], 'overview'), false);
});

// ---- Hints ----

test('a control\'s hint is the key that runs it there, in its pane', () => {
  assert.deepEqual(hintFor('key.delete', 'browse/keys', 'value'), { label: 'D', aria: 'Shift+D' });
  assert.deepEqual(hintFor('key.delete', 'browse/keys', 'keys'), { label: 'd', aria: 'd' });
  assert.deepEqual(hintFor('pattern.bulkDelete', 'browse/keys', 'keys'), { label: 'D', aria: 'Shift+D' });
  assert.deepEqual(hintFor('goto', 'overview'), { label: 'ctrl-k', aria: 'Control+K Meta+K' });
  assert.deepEqual(hintFor('live', 'server/clients'), { label: 'space', aria: 'Space' });
  assert.deepEqual(hintFor('search.drop', 'browse/search', 'index'), { label: 'd', aria: 'd' });
  assert.equal(hintFor('key.delete', 'browse/keys', null), null, 'outside its panes, no key');
  assert.equal(hintFor('refresh', 'overview'), null);
  assert.equal(entryFor('copy.value', 'browse/keys', 'value').keys[0], 'y');
  // The hint is drawn from an attribute (the control's text, which the views use to put the
  // focus back, never changes) and is left out of the accessible name.
  const css = read('app.css');
  assert.match(css, /body\.keys-on \[data-hint\]::after \{\n {2}content: attr\(data-hint\);\n {2}content: attr\(data-hint\) \/ "";/);
  const app = read('app.js');
  assert.match(app, /el\.setAttribute\('aria-keyshortcuts', hint\.aria\)/);
  assert.match(app, /document\.body\.classList\.add\('keys-on'\)/);
});

// ---- The `?` sheet ----

const names = (groups, id) => (groups.find((g) => g.id === id) || { rows: [] }).rows.map((r) => `${r.keys.join(' ')} ${r.name}`);

test('the sheet for Browse › Keys with the value focused', () => {
  const g = sheetModel({ screen: 'browse/keys', pane: 'value' });
  assert.deepEqual(g.map((x) => x.id), ['pane', 'screen', 'everywhere']);
  assert.equal(g[0].label, 'This pane: Value');
  const pane = names(g, 'pane');
  for (const line of ['y Copy the value', 'Y Copy the key name', 'd Delete the selected item', 'D Delete the key', 'T Set the key\'s expiry', 'f Show the value as another format', 'R Rename the key']) assert.ok(pane.includes(line), line);
  assert.ok(names(g, 'screen').includes('d Delete the key (in Keys)'));
  assert.ok(names(g, 'screen').includes('m Load more'));
  const everywhere = names(g, 'everywhere');
  assert.ok(everywhere.includes('ctrl-k cmd-k Go to'));
  assert.ok(everywhere.includes('j ↓ Next row (in a list)'));
  assert.ok(everywhere.includes('2 Browse; again: Keys, Streams, Search'));
  const del = g[0].rows.find((r) => r.name === 'Delete the key');
  assert.equal(del.sends, 'UNLINK', 'a write says what it sends');
  // The filter: by key or by words.
  assert.deepEqual(names(sheetModel({ screen: 'browse/keys', pane: 'value', filter: 'D' }), 'pane'), ['D Delete the key']);
  assert.ok(names(sheetModel({ screen: 'browse/keys', pane: 'value', filter: 'copy' }), 'pane').every((n) => /copy/i.test(n)));
  assert.deepEqual(sheetModel({ screen: 'browse/keys', pane: 'value', filter: 'nothing like it' }), []);
});

test('the sheet for the Slow log and for Connections', () => {
  const slow = sheetModel({ screen: 'server/slowlog', pane: 'slowlog' });
  assert.deepEqual(slow.map((x) => x.id), ['everywhere'], 'the Slow log has no verbs of its own in the dashboard');
  assert.ok(names(slow, 'everywhere').includes('4 Server; again: Clients, Slow log, Monitor'));
  assert.equal(names(slow, 'everywhere').some((n) => /^e |^D /.test(n)), false);
  const conns = sheetModel({ screen: 'connections', pane: 'saved' });
  assert.deepEqual(names(conns, 'screen'), ['r Read again', '+ Add a connection', 'e Edit the connection', 'd Remove the connection']);
  assert.ok(names(conns, 'everywhere').includes('w Writes on or off'));
});

// ---- Go to ----

const held = () => ({
  patterns: ['user:{id}:cart', 'session:{uuid}', 'cart:{id}', 'queue:emails'],
  keys: ['user:10402:cart', 'cart:1002', 'session:abc'],
  screens: screenList({ cluster: false }),
  actions: [
    { name: 'Copy the pattern', keys: ['y'], off: null },
    { name: 'Delete every key of the pattern', keys: ['d'], off: 'Read-only: nothing is written to local. Turn writes on in Connections.' },
  ],
});

test('Go to: patterns, then keys and the Find row, then screens, then actions', () => {
  const g = gotoModel('cart', held());
  assert.deepEqual(g.map((x) => x.id), ['patterns', 'keys']);
  assert.deepEqual(g[0].items.map((i) => i.label), ['cart:{id}', 'user:{id}:cart'], 'a name that starts with it first');
  assert.deepEqual(g[1].items.map((i) => i.kind), ['key', 'key', 'find']);
  assert.equal(g[1].items[2].label, 'Find every key matching *cart*');
  assert.equal(g[1].items[2].text, 'cart');
  assert.equal(gotoModel('user:*:cart', held()).find((x) => x.id === 'keys').items.at(-1).label, 'Find every key matching user:*:cart', 'a glob is used as typed');
  const p = gotoModel('pat', held());
  assert.deepEqual(p.map((x) => x.id), ['keys', 'actions']);
  assert.deepEqual(p[1].items.map((i) => [i.label, i.off]), [
    ['Copy the pattern', null],
    ['Delete every key of the pattern', 'Read-only: nothing is written to local. Turn writes on in Connections.'],
  ], 'an action that is off stays, with its reason');
  const s = gotoModel('slow', held());
  assert.deepEqual(s.find((x) => x.id === 'screens').items.map((i) => [i.label, i.keys.join(' ')]), [['Server › Slow log', '4 4']]);
  assert.deepEqual(gotoModel('browse', held()).find((x) => x.id === 'screens').items.map((i) => i.label), ['Browse › Keys', 'Browse › Streams', 'Browse › Search']);
  // Nothing typed: the screens and the actions, no Find row.
  const none = gotoModel('', held());
  assert.deepEqual(none.map((x) => x.id), ['screens', 'actions']);
  assert.equal(none[0].items.length, 12);
});

test('Go to: `:` is a command for the command line, typed in and not run; `/` finds keys', () => {
  assert.deepEqual(gotoModel(':  HGETALL user:1 ', held()), [{ id: 'command', label: 'Command line', items: [{ kind: 'command', label: 'Run in the command line', detail: 'HGETALL user:1', text: 'HGETALL user:1' }] }]);
  assert.equal(gotoModel(':', held())[0].items[0].text, '');
  const f = gotoModel('/1002', held());
  assert.deepEqual(f.map((x) => x.id), ['keys']);
  assert.deepEqual(f[0].items.map((i) => i.label), ['cart:1002', 'Find every key matching *1002*']);
  // The command line only gets the text: app.js opens the drawer and sets the line, it does not run it.
  const app = read('app.js');
  assert.match(app, /setCommandLine\(true\);\n\s+if \(commandLine\) commandLine\.setLine\(item\.text\);/);
  assert.doesNotMatch(read('views/cmdline.js').slice(read('views/cmdline.js').indexOf('function setLine'), read('views/cmdline.js').indexOf('return { focus')), /run\(/);
});

test('Go to: no credential reaches a result, and typing asks the server for nothing', () => {
  // Go to is given names only; even sources full of secrets give nothing but names.
  const secretPattern = { id: 'aa', name: 'user:{id}', password: 'hunter2', url: 'redis://alice:hunter2@h' };
  const secretKey = { id: 'bb', name: 'user:1', user: 'alice', auth: 'token-xyz' };
  const g = gotoModel('user', { patterns: [secretPattern.name], keys: [secretKey.name], screens: screenList(), actions: [] });
  const text = JSON.stringify(g);
  for (const bad of ['hunter2', 'alice', 'token-xyz', 'redis://']) assert.equal(text.includes(bad), false, bad);
  for (const item of gotoFlat(g)) {
    for (const k of Object.keys(item)) assert.ok(['kind', 'label', 'ref', 'text', 'keys', 'off', 'detail', 'group'].includes(k), k);
  }
  // app.js builds Go to's sources from Browse's names, the screens and the actions only, and
  // the dialog's input asks nothing: the only request is Find, once its row is chosen.
  const app = read('app.js');
  const body = app.slice(app.indexOf('function openGoto'), app.indexOf('function findKeys'));
  assert.doesNotMatch(body, /api\./, 'Go to sends no request itself');
  assert.match(app, /model: \{ patterns: b\.patterns\.map\(\(p\) => p\.name\), keys: b\.keys\.map\(\(k\) => k\.name\)/);
  assert.match(body, /else if \(item\.kind === 'find'\) findKeys\(item\.text\);/);
});

test('Go to: arrows and Tab move through the results and their groups', () => {
  const g = gotoModel('cart', held());
  const flat = gotoFlat(g);
  assert.equal(flat.length, 5);
  assert.equal(gotoStep(g, 0, 'down'), 1);
  assert.equal(gotoStep(g, 4, 'down'), 0, 'wraps');
  assert.equal(gotoStep(g, 0, 'up'), 4);
  assert.equal(gotoStep(g, 0, 'group'), 2, 'Tab: the first key');
  assert.equal(gotoStep(g, 3, 'group'), 0, 'from the last group back to the first');
  assert.equal(gotoStep(g, 3, 'groupBack'), 0);
  assert.equal(gotoStep(g, 0, 'groupBack'), 2);
  assert.equal(gotoStep([], 0, 'down'), -1);
  assert.equal(matchRank('cart:{id}', 'cart'), 1);
  assert.equal(matchRank('user:{id}:cart', 'cart'), 2);
  assert.equal(matchRank('Cart', 'cart'), 0);
  assert.equal(matchRank('xcartx', 'cart'), 3);
  assert.equal(matchRank('queue', 'cart'), null);
});

test('Go to\'s actions are this screen\'s, once each, the focused pane\'s first', () => {
  const a = screenActions('browse/keys', 'keys');
  const del = a.filter((x) => x.name === 'Delete the key');
  assert.equal(del.length, 1);
  assert.deepEqual(del[0].keys, ['d'], 'in the key list the key is d');
  assert.deepEqual(screenActions('browse/keys', 'value').find((x) => x.name === 'Delete the key').keys, ['D']);
  assert.ok(a.some((x) => x.name === 'Writes on or off'));
  assert.equal(a.some((x) => /row/.test(x.name) && x.acts.some((id) => id.startsWith('list.'))), false, 'no moving keys');
  assert.equal(a.some((x) => x.acts.includes('goto')), false);
  assert.deepEqual(screenActions('connections').map((x) => x.name).filter((n) => /connection/.test(n)), ['Add a connection', 'Edit the connection', 'Remove the connection']);
});

// ---- Why a button is off, in words ----

test('a read-only connection shows one line under a bar of writes, with the key that turns writes on', () => {
  const conn = { name: 'local', env: 'LOCAL', readOnly: true, refused: [] };
  const ro = readOnlyText(conn);
  const offs = [writeOff(conn, ['SET']), writeOff(conn, ['EXPIRE']), writeOff(conn, ['UNLINK']), null];
  assert.deepEqual(reasonLines(offs, ro), ['Edits are off: this connection is read-only. Turn writes on (w).']);
  assert.equal(readOnlyLine(), 'Edits are off: this connection is read-only. Turn writes on (w).');
  // Other reasons stay as they are, once each, after the read-only line.
  assert.deepEqual(reasonLines([ro, 'redis-lens does not edit vector sets.', ro, 'redis-lens does not edit vector sets.'], ro), [
    'Edits are off: this connection is read-only. Turn writes on (w).', 'redis-lens does not edit vector sets.',
  ]);
  // Writes on: only the buttons that are off for their own reason say so.
  const on = { name: 'local', env: 'LOCAL', readOnly: false, refused: ['RENAMENX'] };
  assert.deepEqual(reasonLines([writeOff(on, ['SET']), writeOff(on, ['RENAMENX'])], readOnlyText(on)), ['Off: the server refused RENAMENX for this user.']);
  assert.deepEqual(reasonLines([null, null], ro), []);
  // Browse draws them under the bars of the value, the selected item, the key list's head
  // and the bulk actions, as text (not only in titles).
  const browse = read('views/browse.js');
  assert.equal((browse.match(/whyLines\(/g) || []).length, 5);
  assert.match(browse, /whyLines\(buttons\.map\(\(b\) => \(b\.disabled \? b\.title : null\)\)\)/);
});
