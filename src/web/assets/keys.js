// redis-lens dashboard: the keys (docs/ui-redesign.md §5, §7, §8, §12).
//
// One table of every key the page answers to. The global key handler (app.js), the hints on
// buttons, the `?` sheet and the Actions group of Go to are all built from it, so none of
// them can name a key that does nothing, or miss one that does.
//
// Pure: no DOM, no globals at import time (it imports only ui.js's pure maskCommand).
// tests/keys.test.js imports it in Node. The decisions app.js acts on (what Enter does on a
// row, whether a key may fire now, Go to's default row) are functions here, on plain data.
//
// A key never does more than its button: an action is either "press" (the page presses the
// control that carries `data-act="<act>"`, exactly as a click does, so a write goes through
// the server's write gate and its question as a click would) or "app" (a handler of the
// page's own: moving in a list, an area, Go to, the sheet). Nothing here reads from or
// writes to a server.

import { maskCommand } from './ui.js';

// ---------------------------------------------------------------------------------------
// Areas, panels and screens
// ---------------------------------------------------------------------------------------

/** The areas with their digits, and the panels each has, in order (the TUI's sub-tabs). */
export const AREAS = [
  { id: 'overview', label: 'Overview', digit: '1', panels: null },
  { id: 'browse', label: 'Browse', digit: '2', panels: [{ id: 'keys', label: 'Keys' }, { id: 'streams', label: 'Streams' }, { id: 'search', label: 'Search' }] },
  {
    id: 'analyze', label: 'Analyze', digit: '3', panels: [
      { id: 'big_keys', label: 'Big keys' }, { id: 'expiry', label: 'Expiry' }, { id: 'changes', label: 'Changes' }, { id: 'hot_keys', label: 'Hot keys' },
      { id: 'nodes', label: 'Nodes', cluster: true }, { id: 'slots', label: 'Slots', cluster: true }, { id: 'snapshots', label: 'Snapshots' },
    ],
  },
  { id: 'server', label: 'Server', digit: '4', panels: [{ id: 'clients', label: 'Clients' }, { id: 'slowlog', label: 'Slow log' }, { id: 'monitor', label: 'Monitor' }] },
  { id: 'pubsub', label: 'Pub/Sub', digit: '5', panels: null },
  { id: 'connections', label: 'Connections', digit: '0', panels: null },
];

const AREA = Object.fromEntries(AREAS.map((a) => [a.id, a]));

/** The panels of an area that are shown: Nodes and Slots only on a cluster. */
export function panelsOf(area, cluster = false) {
  const a = AREA[area];
  if (!a || !a.panels) return [];
  return a.panels.filter((p) => !p.cluster || cluster);
}

/** A screen's address. Browse › Keys is `#/browse`, as the rail links it. */
export function hashOf(area, panel = null) {
  if (!panel || (area === 'browse' && panel === 'keys')) return `#/${area}`;
  return `#/${area}/${panel}`;
}

/** The screen a route shows: `overview`, `browse/keys`, `server/slowlog`. A panel the route
 *  does not name is the area's first, as the views fall back. */
export function screenOf(route) {
  const name = route && route.name;
  const a = AREA[name];
  if (!a) return 'connections';
  if (!a.panels) return a.id;
  const sub = route.sub;
  const p = a.panels.find((x) => x.id === sub) || a.panels[0];
  return `${a.id}/${p.id}`;
}

/** Every screen, as Go to lists them: label, address and the keys that reach it from
 *  another area (the digit, then the digit again for each next panel). */
export function screenList({ cluster = false } = {}) {
  const out = [];
  for (const a of AREAS) {
    if (!a.panels) {
      out.push({ id: a.id, label: a.label, hash: hashOf(a.id), keys: [a.digit] });
      continue;
    }
    panelsOf(a.id, cluster).forEach((p, i) => {
      out.push({ id: `${a.id}/${p.id}`, label: `${a.label} › ${p.label}`, hash: hashOf(a.id, p.id), keys: new Array(i + 1).fill(a.digit) });
    });
  }
  return out;
}

/** Where a digit goes: the area, or, when that area is shown already, its next panel
 *  (wrapping). Null for a digit that names no area. */
export function areaHash(digit, route, { cluster = false } = {}) {
  const a = AREAS.find((x) => x.digit === digit);
  if (!a) return null;
  if (!route || route.name !== a.id || !a.panels) return hashOf(a.id);
  const ids = panelsOf(a.id, cluster).map((p) => p.id);
  const now = screenOf(route).split('/')[1];
  const at = ids.indexOf(now);
  return hashOf(a.id, ids[(at + 1) % ids.length]);
}

// ---------------------------------------------------------------------------------------
// The actions and the key table
// ---------------------------------------------------------------------------------------

/** Every action a key runs. `run`: `press` (the control carrying `data-act` with this id is
 *  pressed, as a click) or `app` (a handler in app.js). `in`: where the control is looked
 *  for: `row` (only in the selected row), `pane` (the focused pane, else the screen),
 *  `view` (the focused pane first, then the screen) or `page` (anywhere, the top bar
 *  included). `write`: it writes to the server (through the write gate, as its button does);
 *  `sends`: the command, where the page knows it before it is pressed. */
export const ACTS = {
  // Moving in a list (app.js).
  'list.down': { run: 'app', name: 'Next row' },
  'list.up': { run: 'app', name: 'Previous row' },
  'list.pageDown': { run: 'app', name: 'A page down' },
  'list.pageUp': { run: 'app', name: 'A page up' },
  'list.home': { run: 'app', name: 'First row' },
  'list.end': { run: 'app', name: 'Last row' },
  'list.open': { run: 'app', name: 'Open the row, or go to the pane on the right' },
  'list.back': { run: 'app', name: 'Back, or go to the pane on the left' },
  // Areas (app.js).
  'area.overview': { run: 'app', name: 'Overview' },
  'area.browse': { run: 'app', name: 'Browse; again: its next panel' },
  'area.analyze': { run: 'app', name: 'Analyze; again: its next panel' },
  'area.server': { run: 'app', name: 'Server; again: its next panel' },
  'area.pubsub': { run: 'app', name: 'Pub/Sub' },
  'area.connections': { run: 'app', name: 'Connections' },
  // The page's own (app.js).
  filter: { run: 'app', name: 'Filter the focused list (on Search: the query); elsewhere Go to' },
  goto: { run: 'app', name: 'Go to' },
  sheet: { run: 'app', name: 'Keys' },
  cmdline: { run: 'app', name: 'Command line' },
  'node.prev': { run: 'app', name: 'Previous node' },
  'node.next': { run: 'app', name: 'Next node' },
  // The top bar.
  writes: { run: 'press', in: 'page', name: 'Writes on or off' },
  db: { run: 'press', in: 'page', name: 'Database list' },
  live: { run: 'press', in: 'page', name: 'Pause or resume the live figures' },
  // Every screen that has them.
  pause: { run: 'press', in: 'view', name: 'Pause or resume the tail' },
  snapshot: { run: 'press', in: 'view', name: 'Save a snapshot' },
  refresh: { run: 'press', in: 'view', name: 'Read again' },
  more: { run: 'press', in: 'pane', name: 'Load more' },
  sort: { run: 'press', in: 'view', name: 'Sort' },
  analyze: { run: 'press', in: 'view', name: 'Analyze the keys' },
  // Browse › Keys.
  format: { run: 'press', in: 'pane', name: 'Show the value as another format' },
  'copy.value': { run: 'press', in: 'pane', name: 'Copy the value' },
  'copy.name': { run: 'press', in: 'pane', name: 'Copy the key name' },
  'copy.pattern': { run: 'press', in: 'view', name: 'Copy the pattern' },
  'copy.command': { run: 'press', in: 'view', name: 'Copy the pattern\'s scan command' },
  'item.edit': { run: 'press', in: 'pane', name: 'Edit the selected item', write: true },
  'value.edit': { run: 'press', in: 'pane', name: 'Edit the value', write: true },
  'item.add': { run: 'press', in: 'pane', name: 'Add a field, element, member or entry', write: true },
  'item.ttl': { run: 'press', in: 'pane', name: 'Set the TTL of the selected field', write: true, sends: 'HEXPIRE' },
  'item.delete': { run: 'press', in: 'pane', name: 'Delete the selected item', write: true },
  'key.add': { run: 'press', in: 'view', name: 'Add a key', write: true },
  'key.ttl': { run: 'press', in: 'view', name: 'Set the key\'s expiry', write: true, sends: 'EXPIRE' },
  'key.rename': { run: 'press', in: 'view', name: 'Rename the key', write: true, sends: 'RENAMENX' },
  'key.delete': { run: 'press', in: 'view', name: 'Delete the key', write: true, sends: 'UNLINK' },
  'pattern.bulkTtl': { run: 'press', in: 'view', name: 'Set a TTL on every key of the pattern (dry run first)', write: true, sends: 'EXPIRE … NX' },
  'pattern.bulkDelete': { run: 'press', in: 'view', name: 'Delete every key of the pattern (dry run first)', write: true, sends: 'UNLINK' },
  // Browse › Streams.
  'stream.claim': { run: 'press', in: 'view', name: 'Claim pending entries', write: true, sends: 'XAUTOCLAIM' },
  'stream.ack': { run: 'press', in: 'row', name: 'Acknowledge the pending entry', write: true, sends: 'XACK' },
  'stream.pending': { run: 'press', in: 'view', name: 'List the pending entries' },
  'stream.trim': { run: 'press', in: 'view', name: 'Trim the stream', write: true, sends: 'XTRIM' },
  // Browse › Search.
  'search.explain': { run: 'press', in: 'view', name: 'Explain the query' },
  'search.profile': { run: 'press', in: 'view', name: 'Profile the query' },
  'search.create': { run: 'press', in: 'view', name: 'Create an index', write: true, sends: 'FT.CREATE' },
  'search.drop': { run: 'press', in: 'view', name: 'Drop the index (the documents stay)', write: true, sends: 'FT.DROPINDEX' },
  // Pub/Sub.
  'sub.add': { run: 'press', in: 'view', name: 'Subscribe' },
  'sub.shard': { run: 'press', in: 'view', name: 'Shard subscribe' },
  publish: { run: 'press', in: 'view', name: 'Publish', write: true, sends: 'PUBLISH' },
  // Connections.
  'conn.add': { run: 'press', in: 'view', name: 'Add a connection' },
  'conn.edit': { run: 'press', in: 'row', name: 'Edit the connection' },
  'conn.remove': { run: 'press', in: 'row', name: 'Remove the connection (it asks first)' },
};

// Where the entries apply.
const ALL = '*';
const KEYS_PANES = { patterns: 'Patterns', keys: 'Keys', value: 'Value' };

/** The pane names the sheet shows. */
export const PANE_LABELS = {
  ...KEYS_PANES, streams: 'Streams', stream: 'The stream', indexes: 'Indexes', index: 'The index', results: 'Results',
  channels: 'Channels', tail: 'The tail', clients: 'Clients', slowlog: 'Slow log', panel: 'The panel', findings: 'Things to look at',
  'top-patterns': 'Memory by key pattern', conns: 'Connections of the group', entry: 'The entry', nodes: 'Nodes', node: 'The node', saved: 'Saved connections', docker: 'Found in Docker',
};

/** The key table. `keys`: the key values (`KeyboardEvent.key`, `Space`, `ctrl+k`, `meta+k`);
 *  `screens`: `*`, or screens (`browse/keys`, `analyze/*` for every panel of an area);
 *  `panes`: only with the focus in one of these panes; `list`: only with the focus in a
 *  list; `act`: the action, or actions tried in order (the first that is on the screen);
 *  `open`: in these panes, the selected row must be the one that is open (the action acts
 *  on what is open, so it says to open the row first rather than act on another); `quiet`:
 *  when its control is not there or is off, the key does nothing and the browser keeps it
 *  (Space scrolls the page when nothing is live). */
export const KEYS = [
  // Moving.
  { keys: ['j', 'ArrowDown'], name: 'Next row', screens: ALL, list: true, act: 'list.down' },
  { keys: ['k', 'ArrowUp'], name: 'Previous row', screens: ALL, list: true, act: 'list.up' },
  { keys: ['PageDown'], name: 'A page down', screens: ALL, list: true, act: 'list.pageDown' },
  { keys: ['PageUp'], name: 'A page up', screens: ALL, list: true, act: 'list.pageUp' },
  { keys: ['Home', 'g'], name: 'First row', screens: ALL, list: true, act: 'list.home' },
  { keys: ['End', 'G'], name: 'Last row', screens: ALL, list: true, act: 'list.end' },
  { keys: ['Enter', 'ArrowRight', 'l'], name: 'Open the row, or go to the pane on the right', screens: ALL, list: true, act: 'list.open' },
  { keys: ['Escape', 'ArrowLeft', 'h'], name: 'Back, or go to the pane on the left', screens: ALL, list: true, act: 'list.back' },
  // Areas.
  { keys: ['1'], name: 'Overview', screens: ALL, act: 'area.overview' },
  { keys: ['2'], name: 'Browse; again: Keys, Streams, Search', screens: ALL, act: 'area.browse' },
  { keys: ['3'], name: 'Analyze; again: its next panel', screens: ALL, act: 'area.analyze' },
  { keys: ['4'], name: 'Server; again: Clients, Slow log, Monitor', screens: ALL, act: 'area.server' },
  { keys: ['5'], name: 'Pub/Sub', screens: ALL, act: 'area.pubsub' },
  { keys: ['0'], name: 'Connections', screens: ALL, act: 'area.connections' },
  // Everywhere.
  { keys: ['/'], name: 'Filter the focused list (on Search: the query); else Go to', screens: ALL, act: 'filter' },
  { keys: ['ctrl+k', 'meta+k'], name: 'Go to', screens: ALL, act: 'goto' },
  { keys: ['?'], name: 'Keys', screens: ALL, act: 'sheet' },
  { keys: [':'], name: 'Command line', screens: ALL, act: 'cmdline' },
  { keys: ['w'], name: 'Writes on or off', screens: ALL, act: 'writes' },
  { keys: ['b'], name: 'Database list', screens: ALL, act: 'db' },
  { keys: ['['], name: 'Previous node', screens: ALL, act: 'node.prev' },
  { keys: [']'], name: 'Next node', screens: ALL, act: 'node.next' },
  { keys: ['Space'], name: 'Pause or resume what is live (the tail, else the live figures)', screens: ALL, act: ['pause', 'live'], quiet: true },
  { keys: ['ctrl+s'], name: 'Save a snapshot', screens: ALL, act: 'snapshot', missing: 'A snapshot is saved on Analyze, once an analysis is done.' },
  // Screens that have them.
  { keys: ['r'], name: 'Read again', screens: ['browse/streams', 'browse/search', 'analyze/slots', 'connections'], act: 'refresh' },
  { keys: ['m'], name: 'Load more', screens: ['browse/keys', 'browse/streams'], act: 'more' },
  { keys: ['s'], name: 'Sort', screens: ['browse/keys', 'analyze/slots'], act: 'sort' },
  { keys: ['a'], name: 'Analyze the keys (it asks first)', screens: ['overview', 'analyze/*', 'browse/streams'], act: 'analyze' },
  // Browse › Keys: lower case on the selected row, upper case on what it belongs to.
  { keys: ['y'], name: 'Copy the value', screens: ['browse/keys'], panes: ['value'], act: 'copy.value', open: ['value'] },
  { keys: ['Y'], name: 'Copy the key name', screens: ['browse/keys'], panes: ['value'], act: 'copy.name' },
  { keys: ['y'], name: 'Copy the pattern', screens: ['browse/keys'], panes: ['patterns'], act: 'copy.pattern', open: ['patterns'] },
  { keys: ['Y'], name: 'Copy the pattern\'s scan command', screens: ['browse/keys'], panes: ['patterns'], act: 'copy.command', open: ['patterns'] },
  { keys: ['e'], name: 'Edit the selected item, or the value', screens: ['browse/keys'], panes: ['value'], act: ['item.edit', 'value.edit'], open: ['value'] },
  { keys: ['+'], name: 'Add a field, element, member or entry', screens: ['browse/keys'], panes: ['value'], act: 'item.add' },
  { keys: ['+'], name: 'Add a key', screens: ['browse/keys'], panes: ['patterns', 'keys'], act: 'key.add' },
  { keys: ['t'], name: 'Set the TTL of the selected field', screens: ['browse/keys'], panes: ['value'], act: 'item.ttl', open: ['value'] },
  { keys: ['T'], name: 'Set the key\'s expiry', screens: ['browse/keys'], panes: ['value'], act: 'key.ttl' },
  { keys: ['t'], name: 'Set the key\'s expiry', screens: ['browse/keys'], panes: ['keys'], act: 'key.ttl', open: ['keys'] },
  { keys: ['T'], name: 'Set a TTL on every key of the pattern', screens: ['browse/keys'], panes: ['keys'], act: 'pattern.bulkTtl' },
  { keys: ['t'], name: 'Set a TTL on every key of the pattern', screens: ['browse/keys'], panes: ['patterns'], act: 'pattern.bulkTtl', open: ['patterns'] },
  { keys: ['d'], name: 'Delete the selected item', screens: ['browse/keys'], panes: ['value'], act: 'item.delete', open: ['value'] },
  { keys: ['D'], name: 'Delete the key', screens: ['browse/keys'], panes: ['value'], act: 'key.delete' },
  { keys: ['d'], name: 'Delete the key', screens: ['browse/keys'], panes: ['keys'], act: 'key.delete', open: ['keys'] },
  { keys: ['D'], name: 'Delete every key of the pattern', screens: ['browse/keys'], panes: ['keys'], act: 'pattern.bulkDelete' },
  { keys: ['d'], name: 'Delete every key of the pattern', screens: ['browse/keys'], panes: ['patterns'], act: 'pattern.bulkDelete', open: ['patterns'] },
  { keys: ['R'], name: 'Rename the key', screens: ['browse/keys'], panes: ['value', 'keys'], act: 'key.rename', open: ['keys'] },
  { keys: ['f'], name: 'Show the value as another format', screens: ['browse/keys'], panes: ['value'], act: 'format' },
  { keys: ['f'], name: 'Show the field as another format', screens: ['browse/search'], act: 'format' },
  // Browse › Streams.
  { keys: ['c'], name: 'Claim pending entries', screens: ['browse/streams'], act: 'stream.claim', open: ['streams'] },
  { keys: ['A'], name: 'Acknowledge the pending entry, or list the pending entries', screens: ['browse/streams'], act: ['stream.ack', 'stream.pending'], open: ['streams'] },
  { keys: ['-'], name: 'Trim the stream', screens: ['browse/streams'], act: 'stream.trim', open: ['streams'] },
  // Browse › Search.
  { keys: ['x'], name: 'Explain the query', screens: ['browse/search'], act: 'search.explain' },
  { keys: ['X'], name: 'Profile the query', screens: ['browse/search'], act: 'search.profile' },
  { keys: ['+'], name: 'Create an index', screens: ['browse/search'], act: 'search.create' },
  { keys: ['d'], name: 'Drop the index', screens: ['browse/search'], act: 'search.drop', open: ['indexes'] },
  // Pub/Sub.
  { keys: ['+'], name: 'Subscribe', screens: ['pubsub'], act: 'sub.add' },
  { keys: ['S'], name: 'Shard subscribe', screens: ['pubsub'], act: 'sub.shard' },
  { keys: ['P'], name: 'Publish', screens: ['pubsub'], act: 'publish' },
  // Connections.
  { keys: ['+'], name: 'Add a connection', screens: ['connections'], act: 'conn.add' },
  { keys: ['e'], name: 'Edit the connection', screens: ['connections'], act: 'conn.edit' },
  { keys: ['d'], name: 'Remove the connection', screens: ['connections'], act: 'conn.remove' },
];

/** An entry's actions, as a list. */
export const actsOf = (entry) => [].concat(entry.act);

/** Every screen id there is (the cluster panels included). */
export function allScreens() {
  return screenList({ cluster: true }).map((s) => s.id);
}

/** Whether an entry's `screens` covers a screen. */
export function onScreen(spec, screen) {
  if (spec === ALL) return true;
  return spec.some((s) => s === screen || (s.endsWith('/*') && screen.startsWith(s.slice(0, -1))));
}

/** Pairs of entries that claim the same key in the same place: the same key, a screen in
 *  common and panes in common (no panes is every pane). A list key and a key that is not
 *  one clash too: in a list the list key would hide the other. The table must have none
 *  (tests/keys.test.js). */
export function conflicts(table = KEYS) {
  const screens = allScreens();
  const out = [];
  for (let i = 0; i < table.length; i += 1) {
    for (let j = i + 1; j < table.length; j += 1) {
      const a = table[i];
      const b = table[j];
      const key = a.keys.find((k) => b.keys.includes(k));
      if (!key) continue;
      const where = screens.find((s) => onScreen(a.screens, s) && onScreen(b.screens, s));
      if (!where) continue;
      const panes = !a.panes || !b.panes || a.panes.some((p) => b.panes.includes(p));
      if (!panes) continue;
      out.push({ key, screen: where, a: a.name, b: b.name });
    }
  }
  return out;
}

// ---------------------------------------------------------------------------------------
// Key names
// ---------------------------------------------------------------------------------------

/** The table's name for a key press, or null for one the table never binds (Alt, or Ctrl
 *  with Meta). Shift shows in `key` itself (`D`, `?`, `+`, `:`). */
export function keyOf(e) {
  if (!e || typeof e.key !== 'string' || e.altKey) return null;
  let k = e.key;
  if (k === ' ' || k === 'Spacebar') k = 'Space';
  if (e.ctrlKey || e.metaKey) {
    if (e.ctrlKey && e.metaKey) return null;
    if (k.length !== 1) return null;
    return `${e.ctrlKey ? 'ctrl' : 'meta'}+${k.toLowerCase()}`;
  }
  return k;
}

const LABELS = {
  ArrowDown: '↓', ArrowUp: '↑', ArrowLeft: '←', ArrowRight: '→', PageDown: 'PgDn', PageUp: 'PgUp', Home: 'Home', End: 'End', Enter: 'enter', Escape: 'esc', Space: 'space',
};

/** How a key is written on the page: `d`, `D`, `ctrl-k`, `cmd-k`, `space`, `↓`. */
export function keyLabel(k) {
  if (LABELS[k]) return LABELS[k];
  if (k.startsWith('ctrl+')) return `ctrl-${k.slice(5)}`;
  if (k.startsWith('meta+')) return `cmd-${k.slice(5)}`;
  return k;
}

/** A key as `aria-keyshortcuts` writes it: `D` is `Shift+D`, `ctrl+k` is `Control+K`. */
export function ariaKey(k) {
  if (k.startsWith('ctrl+')) return `Control+${k.slice(5).toUpperCase()}`;
  if (k.startsWith('meta+')) return `Meta+${k.slice(5).toUpperCase()}`;
  if (k === '+') return 'Plus';
  if (/^[A-Z]$/.test(k)) return `Shift+${k}`;
  return k;
}

// ---------------------------------------------------------------------------------------
// The dispatcher, as a model
// ---------------------------------------------------------------------------------------

/** Which entry a key press runs, or null to leave it to the browser. `c`:
 *  - `screen`: the screen shown (`browse/keys`);
 *  - `pane`: the pane the focus is in (`value`), or null;
 *  - `inList`: the focus is in a pane that has rows;
 *  - `inInput`: the focus is in an input, textarea, select or contenteditable;
 *  - `dialog`: a dialog or menu is open, or the focus is in the command-line drawer (they
 *    handle their own keys);
 *  - `native`: the focus is on a control that Enter presses by itself (a button, a link);
 *  - `onRow`: the focus is on a row of a list.
 *  No key runs in an input or behind a dialog. Tab is never taken. */
export function dispatch(key, c = {}) {
  if (!key || c.dialog || c.inInput) return null;
  if (key === 'Enter' && c.native) return null;
  // Enter and Space on a button, a link or a row do what the browser does with them.
  if (key === 'Space' && (c.native || c.onRow)) return null;
  const screen = c.screen || '';
  for (const entry of KEYS) {
    if (!entry.keys.includes(key)) continue;
    if (!onScreen(entry.screens, screen)) continue;
    if (entry.list && !c.inList) continue;
    if (entry.panes && !entry.panes.includes(c.pane)) continue;
    return entry;
  }
  return null;
}

/** The entry a key would run with no pane focused and no list: for the hint and the sheet. */
export function entryFor(act, screen, pane = null) {
  const fits = KEYS.filter((e) => actsOf(e).includes(act) && onScreen(e.screens, screen));
  return fits.find((e) => e.panes && e.panes.includes(pane)) || fits.find((e) => !e.panes) || null;
}

/** The key hint of a control carrying `data-act`: the first key of the entry that runs it
 *  on this screen and in this pane. Null when no key does. */
export function hintFor(act, screen, pane = null) {
  const e = entryFor(act, screen, pane);
  return e ? { label: keyLabel(e.keys[0]), aria: e.keys.map(ariaKey).join(' ') } : null;
}

/** Moving in a list of `count` rows from `index` (-1 when none is selected). */
export function listMove(index, count, act, page = 10) {
  if (count <= 0) return -1;
  const i = Math.min(index, count - 1);
  const from = i < 0 ? 0 : i;
  switch (act) {
    case 'list.down': return i < 0 ? 0 : Math.min(i + 1, count - 1);
    case 'list.up': return i < 0 ? 0 : Math.max(i - 1, 0);
    case 'list.pageDown': return Math.min(from + page, count - 1);
    case 'list.pageUp': return Math.max(from - page, 0);
    case 'list.home': return 0;
    case 'list.end': return count - 1;
    default: return i;
  }
}

/** The pane beside the current one: `dir` 1 (right) or -1 (left). -1 when there is none. */
export function paneStep(count, index, dir) {
  const next = index + dir;
  return index < 0 || next < 0 || next >= count ? -1 : next;
}

/** What `/` does: filter the focused pane's list when it has a filter box, else the
 *  screen's own box outside the panes (Search's query), else open Go to. */
export function slashTarget({ paneFilter = false, screenFilter = false } = {}) {
  if (paneFilter) return 'pane';
  if (screenFilter) return 'screen';
  return 'goto';
}

/** The row selected in a list once it is drawn: the one remembered, else the open one,
 *  else the first. `rows`: `{ key, open }`. -1 for an empty list. */
export function selectedRow(rows, remembered = null) {
  if (!rows.length) return -1;
  const kept = remembered === null ? -1 : rows.findIndex((r) => r.key === remembered);
  if (kept >= 0) return kept;
  const open = rows.findIndex((r) => r.open);
  return open >= 0 ? open : 0;
}

// ---------------------------------------------------------------------------------------
// Whether a key acts now
// ---------------------------------------------------------------------------------------

/** The actions a held key may repeat: moving in a list. Nothing else fires twice from one
 *  press, so holding `d` never deletes one item after another. */
export const REPEATS = new Set(['list.down', 'list.up', 'list.pageDown', 'list.pageUp', 'list.home', 'list.end']);

/** Whether an action is a write. */
export const isWrite = (act) => !!(act && ACTS[act] && ACTS[act].write);

/** What Enter, → or l does on a row. `r`:
 *  - `open`: the row is the one open in its list;
 *  - `control`: the row is itself a button or a link, `controlAct` its `data-act`;
 *  - `opens`: the row's `[data-open]` controls in order, `{ act, disabled }`;
 *  - `right`: there is a pane to the right.
 *  Returns `{ do: 'right' }` (to that pane), `{ do: 'row' }` (press the row),
 *  `{ do: 'press', index }` (press that `[data-open]` control), or null (nothing). A row is
 *  only opened through itself or a control marked `data-open`, never through a write: a
 *  write is only ever its own key's, or a click. */
export function openStep(key, r = {}) {
  if (r.open) return r.right ? { do: 'right' } : null;
  if (r.control) return isWrite(r.controlAct) ? null : { do: 'row' };
  const index = (r.opens || []).findIndex((o) => !isWrite(o.act) && !o.disabled);
  if (index >= 0) return { do: 'press', index };
  return key !== 'Enter' && r.right ? { do: 'right' } : null;
}

/** Whether a key's entry may act now, and what to say when not. `c`:
 *  - `repeat`: the key is held (`KeyboardEvent.repeat`);
 *  - `pane`: the focused pane's id;
 *  - `row`: the row the entry would act on: the focused row, else the pane's selected row
 *    (`{ open, group }`), or null;
 *  - `control`: the control it would press (`{ disabled, title }`), null when none is shown.
 *  Returns `{ run: true }`, or `{ run: false, say, pass }`: `say` the message to show (or
 *  null), `pass` true to leave the key to the browser. */
export function keyVerdict(entry, c = {}) {
  const acts = actsOf(entry);
  if (c.repeat && !acts.every((a) => REPEATS.has(a))) return { run: false, say: null, pass: false };
  if (ACTS[acts[0]].run === 'app') return { run: true };
  const key = keyLabel(entry.keys[0]);
  if (entry.open && c.pane && entry.open.includes(c.pane) && c.row) {
    if (c.row.group) return { run: false, say: `A group has no keys of its own: open one of its patterns, then press ${key}.`, pass: false };
    if (!c.row.open) return { run: false, say: `Open the selected row first (${keyLabel('Enter')}), then press ${key}.`, pass: false };
  }
  if (!c.control) {
    if (entry.quiet) return { run: false, say: null, pass: true };
    return { run: false, say: entry.missing || `${key}: ${entry.name.toLowerCase()} is not on this screen now.`, pass: false };
  }
  if (c.control.disabled) {
    if (entry.quiet) return { run: false, say: null, pass: true };
    return { run: false, say: c.control.title || `${entry.name}: off now.`, pass: false };
  }
  return { run: true };
}

// ---------------------------------------------------------------------------------------
// The `?` sheet
// ---------------------------------------------------------------------------------------

const rowOf = (e, note = null) => {
  const sends = actsOf(e).map((a) => ACTS[a] && ACTS[a].sends).find(Boolean) || null;
  return { keys: e.keys.map(keyLabel), name: note ? `${e.name} (${note})` : e.name, sends };
};

/** The sheet of keys for a screen, in three groups: this pane, this screen, everywhere.
 *  `filter` keeps the rows whose key is the text (one character: that key exactly, so `D`
 *  is not `d`), or whose name or command holds it. */
export function sheetModel({ screen, pane = null, filter = '' } = {}) {
  const raw = String(filter ?? '').trim();
  const q = raw.toLowerCase();
  const keep = (r) => {
    if (!q) return true;
    if (r.keys.includes(raw)) return true;
    if (raw.length === 1) return false;
    return r.name.toLowerCase().includes(q) || r.keys.some((k) => k.toLowerCase() === q) || Boolean(r.sends && r.sends.toLowerCase().includes(q));
  };
  const here = [];
  const screenRows = [];
  const everywhere = [];
  for (const e of KEYS) {
    if (!onScreen(e.screens, screen)) continue;
    if (e.panes) {
      if (pane && e.panes.includes(pane)) here.push(rowOf(e));
      else screenRows.push(rowOf(e, `in ${e.panes.map((p) => PANE_LABELS[p] || p).join(' or ')}`));
    } else if (e.screens === ALL) everywhere.push(rowOf(e, e.list ? 'in a list' : null));
    else screenRows.push(rowOf(e));
  }
  return [
    { id: 'pane', label: pane ? `This pane: ${PANE_LABELS[pane] || pane}` : 'This pane', rows: here.filter(keep) },
    { id: 'screen', label: 'This screen', rows: screenRows.filter(keep) },
    { id: 'everywhere', label: 'Everywhere', rows: everywhere.filter(keep) },
  ].filter((g) => g.rows.length);
}

/** The actions of a screen for Go to: the entries that press a control (its own and the
 *  top bar's), each name once, the focused pane's entry first where two panes share one. */
export function screenActions(screen, pane = null) {
  const byName = new Map();
  const mine = (e) => !!(e.panes && e.panes.includes(pane));
  for (const e of KEYS) {
    if (e.list || !onScreen(e.screens, screen)) continue;
    const acts = actsOf(e);
    if (!acts.every((a) => ACTS[a].run === 'press')) continue;
    const had = byName.get(e.name);
    if (!had || (mine(e) && !mine(had.entry))) byName.set(e.name, { entry: e, name: e.name, keys: e.keys.map(keyLabel), acts });
  }
  return Array.from(byName.values());
}

// ---------------------------------------------------------------------------------------
// Go to
// ---------------------------------------------------------------------------------------

/** How well a name matches: 0 the whole of it, 1 its start, 2 after a separator, 3
 *  anywhere; null for no match. Case does not count. */
export function matchRank(name, text) {
  const n = String(name ?? '').toLowerCase();
  const q = String(text ?? '').toLowerCase();
  if (!q) return 3;
  const at = n.indexOf(q);
  if (at < 0) return null;
  if (n === q) return 0;
  if (at === 0) return 1;
  return /[\s:._\-/›{}]/.test(n[at - 1]) ? 2 : 3;
}

const ranked = (items, text, nameOf, cap) => items
  .map((it, i) => ({ it, i, r: matchRank(nameOf(it), text) }))
  .filter((x) => x.r !== null)
  .sort((a, b) => a.r - b.r || a.i - b.i)
  .slice(0, cap);

export const GOTO_CAP = { patterns: 6, keys: 6, screens: 12, actions: 12 };

/** Go to's results for what is typed, from what the page holds already: nothing here asks a
 *  server for anything. `held`: `{ patterns: [name], keys: [name], screens: screenList(),
 *  actions: [{ name, keys, off }] }` (`off`: why it is off now, or null). Each item names
 *  its source by index (`ref`), so nothing but the names and the page's own words is in it.
 *  Groups, most specific first: Patterns, Keys (then the Find row), Screens, Actions. Text
 *  starting with `:` is a command for the command line (opened with the rest typed in, not
 *  run); text starting with `/` finds keys. */
export function gotoModel(text, held = {}) {
  const raw = String(text ?? '');
  const t = raw.trim();
  if (t.startsWith(':')) {
    const rest = t.slice(1).trim();
    // The detail is shown and announced: credentials in it are masked as the command line
    // masks its echo. `text` (what goes into the command line) is never drawn.
    return [{ id: 'command', label: 'Command line', items: [{ kind: 'command', label: 'Run in the command line', detail: maskCommand(rest), text: rest }] }];
  }
  const finding = t.startsWith('/');
  const q = finding ? t.slice(1).trim() : t;
  const groups = [];
  const patterns = held.patterns || [];
  const keys = held.keys || [];
  if (q && !finding) {
    const p = ranked(patterns, q, (n) => n, GOTO_CAP.patterns).map(({ it, i }) => ({ kind: 'pattern', label: String(it), ref: i }));
    if (p.length) groups.push({ id: 'patterns', label: 'Patterns', items: p });
  }
  if (q) {
    const k = ranked(keys, q, (n) => n, GOTO_CAP.keys).map(({ it, i }) => ({ kind: 'key', label: String(it), ref: i }));
    k.push({ kind: 'find', label: `Find every key matching ${/[*?[]/.test(q) ? q : `*${q}*`}`, text: q });
    groups.push({ id: 'keys', label: 'Keys', items: k });
  }
  if (finding) return groups;
  const s = ranked(held.screens || [], q, (x) => x.label, GOTO_CAP.screens).map(({ it, i }) => ({ kind: 'screen', label: it.label, keys: it.keys.slice(), ref: i }));
  if (s.length) groups.push({ id: 'screens', label: 'Screens', items: s });
  const a = ranked(held.actions || [], q, (x) => x.name, GOTO_CAP.actions).map(({ it, i }) => ({ kind: 'action', label: it.name, keys: it.keys.slice(), off: it.off || null, ref: i }));
  if (a.length) groups.push({ id: 'actions', label: 'Actions', items: a });
  return groups;
}

/** The results in one list, in order, with their group. */
export function gotoFlat(groups) {
  const out = [];
  groups.forEach((g, gi) => g.items.forEach((it) => out.push({ ...it, group: gi })));
  return out;
}

/** The result Enter picks before any arrow is pressed, as an index in `gotoFlat`: the
 *  command when there is one; else a Screen or Action (not off) whose name, or a word of
 *  it, starts with the text, the closest first; else the first Pattern; else the first
 *  loaded Key; else any other Screen or Action; and only when nothing else matched, Find.
 *  So Enter never starts a read the user did not point at. -1 when there is nothing. */
export function gotoDefault(groups, text = '') {
  const flat = gotoFlat(groups);
  if (!flat.length) return -1;
  const t = String(text ?? '').trim();
  const q = t.startsWith('/') ? t.slice(1).trim() : t;
  const first = (pred) => flat.findIndex(pred);
  const cmd = first((x) => x.kind === 'command');
  if (cmd >= 0) return cmd;
  const named = (x) => (x.kind === 'screen' || x.kind === 'action') && !x.off;
  let best = -1;
  let bestRank = 3;
  flat.forEach((x, i) => {
    if (!named(x)) return;
    const r = matchRank(x.label, q);
    if (r !== null && r < bestRank) {
      best = i;
      bestRank = r;
    }
  });
  if (best >= 0 && bestRank <= 2) return best;
  for (const pred of [(x) => x.kind === 'pattern', (x) => x.kind === 'key', named, (x) => x.kind !== 'find']) {
    const i = first(pred);
    if (i >= 0) return i;
  }
  return first((x) => x.kind === 'find');
}

/** Moving in Go to's results: `down`, `up` (wrapping), `group` (Tab: the first item of the
 *  next group, wrapping) and `groupBack` (Shift-Tab: of the previous group). `at` is the
 *  index in `gotoFlat`. */
export function gotoStep(groups, at, move) {
  const flat = gotoFlat(groups);
  if (!flat.length) return -1;
  const i = at < 0 || at >= flat.length ? -1 : at;
  if (move === 'down') return i < 0 ? 0 : (i + 1) % flat.length;
  if (move === 'up') return i <= 0 ? flat.length - 1 : i - 1;
  if (move === 'group') {
    const g = i < 0 ? -1 : flat[i].group;
    const next = flat.findIndex((x) => x.group > g);
    return next >= 0 ? next : 0;
  }
  if (move === 'groupBack') {
    const g = i < 0 ? 0 : flat[i].group;
    const last = flat[flat.length - 1].group;
    const want = g === 0 ? last : g - 1;
    return flat.findIndex((x) => x.group === want);
  }
  return i < 0 ? 0 : i;
}

// ---------------------------------------------------------------------------------------
// Why buttons are off, in words under their bar
// ---------------------------------------------------------------------------------------

/** The line a read-only connection shows under a bar of writes. */
export function readOnlyLine() {
  const w = KEYS.find((e) => actsOf(e).includes('writes'));
  return `Edits are off: this connection is read-only. Turn writes on (${keyLabel(w.keys[0])}).`;
}

/** The lines under a button bar: why the buttons that are off are off, each reason once.
 *  `offs`: each button's reason (null when it is on). `readOnly`: the reason a read-only
 *  connection gives (writeOff's words); those all become the one read-only line. */
export function reasonLines(offs, readOnly = null) {
  const out = [];
  for (const r of offs) {
    if (!r) continue;
    const line = readOnly && r === readOnly ? readOnlyLine() : r;
    if (!out.includes(line)) out.push(line);
  }
  return out;
}
