// Browse: the key tree (from key names before an analysis, from the analysis after one), a
// pattern's keys, Find a key, and a key's value in each of its renderings. Its other panels,
// Streams and Search (phase 4), are views/streams.js and views/search.js, as the TUI's
// Browse has Keys · Streams · Search.
//
// Everything a key holds is drawn with textContent, as redis-lens escaped it. A key is
// named in a request by the hex id redis-lens gave for it, never by the text shown. Values
// stay in this page's memory while it is open: nothing goes to storage, the console or an
// address. Copying is the browser's clipboard, at a click.
//
// Writes (phase 3) are the TUI's: a value's edits per type, add key, the key's TTL and a hash
// field's, rename, delete, and a bulk expire or delete over a pattern's keys. Each is asked
// of redis-lens, which builds the TUI's own write and puts it through the one write gate; a
// question it asks comes back as the confirm dialog. What a form holds stays in the form: it
// is never kept once the form closes, and its boxes have autocomplete off, so the browser
// does not remember them either.

import {
  h, fill, refill, DASH, fmtBytes, fmtCount, fmtGrouped, fmtPercent, plural, barPercent, meter,
  notice, emptyState, connectFirst, analyzing, errorText, copyText, expiryText, fmtSignedBytes,
  openDialog, readOnlyNote, pills, patternName,
} from '../ui.js';
import { reasonLines, readOnlyLine } from '../keys.js';
import * as streams from './streams.js';
import * as search from './search.js';

// ---------------------------------------------------------------------------------------
// Pure helpers (tested in tests/browse.test.js)
// ---------------------------------------------------------------------------------------

export const SORT_LABELS = { memory: 'Memory', keys: 'Keys', avg_size: 'Average size', no_ttl: 'No TTL', name: 'Name' };

/** How many rows of a long list are drawn at once; "Show more" draws the next. */
export const CHUNK = 300;

/** The rows of the tree that a filter leaves: a pattern whose name holds the text, or a
 *  group whose label does (all its patterns), or a group with such patterns (only those). */
export function filterRows(rows, text) {
  const q = String(text ?? '').trim().toLowerCase();
  if (!q) return rows;
  const has = (s) => String(s ?? '').toLowerCase().includes(q);
  const out = [];
  for (const r of rows) {
    if (r.kind === 'group') {
      if (has(r.label)) out.push(r);
      else {
        const children = r.children.filter((c) => has(c.pattern.name));
        if (children.length) out.push({ ...r, children });
      }
    } else if (has(r.pattern.name)) out.push(r);
  }
  return out;
}

/** The keys of a pattern row, in words: `~8.7K keys`, `60 keys` (every name read, or the
 *  whole keyspace looked at), `one key` for a pattern that names one key. */
export function keysText(row, exact) {
  if (row.literal === true) return 'one key';
  const n = row.estKeys;
  if (typeof n !== 'number') return DASH;
  return `${exact ? '' : '~'}${fmtCount(n)} ${n === 1 ? 'key' : 'keys'}`;
}

/** Whether a tree's counts are exact: every name read, or an analysis that saw every key. */
export function treeExact(tree, report) {
  if (!tree) return false;
  if (tree.source === 'names') return !!(tree.names && tree.names.complete);
  if (tree.source === 'analysis') return !!(report && report.complete);
  return false;
}

/** The line under the pattern list: where the rows come from and how far it got. */
export function treeFoot(tree, report) {
  if (!tree) return '';
  if (tree.source === 'names') {
    const n = tree.names;
    if (!n) return '';
    const parts = [];
    if (n.complete) parts.push(`All ${fmtGrouped(n.namesRead)} names read.`);
    else parts.push(`${fmtGrouped(n.namesRead)} names read${typeof n.dbsize === 'number' ? ` of about ${fmtGrouped(n.dbsize)}` : ''}.`);
    if (n.nodesDone) parts.push(`${n.nodesDone[0]} of ${n.nodesDone[1]} nodes done.`);
    if (n.done && !n.complete && (n.missing.length || n.refused.length || n.skipped.length)) parts.push('Some nodes are left out.');
    if (n.reading) parts.push('Reading more…');
    return parts.join(' ');
  }
  if (tree.source === 'partial' && tree.partial) return `Partial results: ${fmtGrouped(tree.partial.sampled)} of ${fmtGrouped(tree.partial.target)} keys sampled so far.`;
  if (tree.source === 'analysis') {
    const keys = report && typeof report.sampledKeys === 'number' ? `From ${fmtGrouped(report.sampledKeys)} sampled keys` : 'From the analysis';
    return `${keys}${tree.node !== null && tree.nodes[tree.node] ? `, on node ${tree.nodes[tree.node].endpoint} only` : ''}.`;
  }
  return '';
}

/** What the rows come from, in a few words, for the card's sub-title. */
export function treeSource(tree) {
  if (!tree) return 'Reading…';
  if (tree.source === 'names') return 'Names only, no analysis yet: counts, no sizes or TTLs.';
  if (tree.source === 'partial') return `Partial: ${fmtGrouped(tree.partial ? tree.partial.sampled : null)} / ${fmtGrouped(tree.partial ? tree.partial.target : null)} sampled.`;
  if (tree.source === 'analysis') return tree.previous ? 'From the previous analysis, while a new one runs.' : 'From the analysis.';
  return tree.why || 'Nothing to show yet.';
}

/** The line under a key list. */
export function listFoot(list) {
  if (!list) return '';
  const n = list.loaded;
  if (list.kind === 'find') {
    if (!list.done) return n ? `${fmtGrouped(n)} loaded so far.` : 'No key matched yet: scan further to look at more names.';
    if (n === 0) return `No key matches ${list.glob ?? ''}.`.replace(' .', '.');
    return n === 1 ? '1 key matches.' : `${fmtGrouped(n)} keys match.`;
  }
  if (!list.done) return `${fmtGrouped(n)} loaded so far.`;
  return n === 1 ? 'Every key is loaded: 1.' : `Every key is loaded: ${fmtGrouped(n)}.`;
}

/** A key list after "Load more": redis-lens may send the whole list, or only the new rows
 *  (they start at `newFrom`). Either way the page keeps every row once, in order. */
export function mergeList(old, next) {
  if (!old || old.list !== next.list || next.rows.length >= next.loaded) return next;
  const from = Math.max(0, Math.min(next.newFrom, old.rows.length));
  return { ...next, rows: old.rows.slice(0, from).concat(next.rows) };
}

/** The notes of a key list: what was left out, and why. */
export function listNotes(list) {
  if (!list) return [];
  const out = [];
  if (list.error) out.push(list.error);
  out.push(...list.refused);
  if (list.capped && !list.done) out.push(`This list holds ${list.maxRows ? fmtGrouped(list.maxRows) : 'the most'} keys or more, so no more are loaded. Find a narrower name to see others.`);
  if (list.otherPattern) out.push(`${plural(list.otherPattern, 'key matched', 'keys matched')} the pattern's glob but belong to another pattern, so they are not listed.`);
  if (list.unreadable) out.push(`${plural(list.unreadable, 'name is', 'names are')} not UTF-8, so not listed.`);
  if (list.cluster) {
    if (list.cluster.missing.length) out.push(`Not read: ${list.cluster.missing.join(', ')} (no connection to it).`);
  }
  return out;
}

/** The facts line above a value: length, size, expiry, then node, encoding and the LFU
 *  counter when they are known. */
export function valueFacts(v, nodeName = null) {
  const m = v.meta || {};
  const parts = [];
  const length = v.length ?? m.length;
  const unit = m.lengthUnit || 'items';
  const one = { fields: 'field', elements: 'element', members: 'member', entries: 'entry', items: 'item' }[unit] || unit;
  if (typeof length === 'number') parts.push(m.lengthUnit === 'bytes' || v.kind === 'string' ? plural(length, 'byte', 'bytes') : plural(length, one, unit));
  if (typeof m.bytes === 'number') parts.push(`${fmtBytes(m.bytes)} in memory`);
  const exp = expiryText(m.ttl);
  if (exp) parts.push(exp);
  if (nodeName) parts.push(`on node ${nodeName}`);
  if (m.encoding) parts.push(`stored as ${m.encoding}`);
  if (typeof m.freq === 'number') parts.push(`LFU counter ${m.freq}`);
  return parts.join(' · ');
}

/** The column headings of a value's items, by its kind. */
export function itemColumns(v) {
  switch (v.kind) {
    case 'hash': return v.fieldTtls ? ['Field', 'Value', 'TTL'] : ['Field', 'Value'];
    case 'list': return ['Index', 'Element'];
    case 'array': return ['Index', 'Element'];
    case 'set': return ['Member'];
    case 'vector': return ['Element'];
    case 'zset': return ['Member', 'Score'];
    case 'stream': return ['Entry', 'Fields'];
    default: return ['Item'];
  }
}

/** One item's cells, as the columns above. */
export function itemCells(v, item) {
  switch (v.kind) {
    case 'hash': return v.fieldTtls ? [item.name ?? '', item.value ?? '', item.ttl ?? DASH] : [item.name ?? '', item.value ?? ''];
    case 'list': return [String(item.index), item.value ?? ''];
    case 'array': return [String(item.at ?? item.index), item.value ?? ''];
    case 'zset': return [item.name ?? '', item.value ?? ''];
    case 'stream': return [item.name ?? '', (item.fields || []).map((f) => `${f.name}=${f.value}`).join('  ')];
    default: return [item.value ?? item.name ?? ''];
  }
}

/** The line under a value: its own notice, or how much is loaded. */
export function valueFoot(v) {
  const parts = [];
  if (v.items.length && typeof v.length === 'number' && v.loaded < v.length) parts.push(`${fmtGrouped(v.loaded)} of ${fmtGrouped(v.length)} loaded.`);
  if (v.notice) parts.push(v.notice);
  return parts.join(' ');
}

/** A whole Browse memory, as it starts. */
export function blank() {
  return {
    who: null, conn: null, tree: null, treeError: null, sort: null, node: null, filter: '', closed: new Set(),
    picked: null, list: null, listError: null, listFilter: '', listShown: CHUNK, finding: null, back: null,
    value: null, valueError: null, valueId: null, item: null, shown: null, format: null, itemsShown: CHUNK,
    dbs: null, dbsError: null, pending: null, bulk: null,
  };
}

/** Follow the session: another connection starts Browse afresh; a new run (an analysis, a
 *  database switch) drops the key list and the value, which redis-lens drops then too, and
 *  keeps the filter, the sort and the closed groups, as the TUI does. */
export function follow(store, state) {
  const c = state.conn;
  const conn = c ? `${c.name}|${c.docker}` : null;
  const who = conn ? `${conn}|${state.analysis ? state.analysis.run : 0}` : null;
  if (conn !== store.conn) {
    const pending = store.pending;
    Object.assign(store, blank(), { conn, who, pending });
    return 'reset';
  }
  if (who !== store.who) {
    Object.assign(store, { who, tree: null, treeError: null, node: null, picked: null, list: null, listError: null, finding: null, back: null, value: null, valueError: null, valueId: null, item: null, shown: null, format: null, dbs: null, dbsError: null });
    return 'run';
  }
  return null;
}

// ---------------------------------------------------------------------------------------
// Writes (phase 3): what each kind of value offers, why an action is off, values as text
// or hex, and the key list after a write. Pure, tested in tests/edits.test.js.
// ---------------------------------------------------------------------------------------

/** The edits each kind of value offers, as the TUI's key detail offers them, with the
 *  commands each sends (a command the server refused for this user turns it off). */
export const EDITS = {
  // `cmds`: what each write sends, so a command the server refused for this user turns it
  // off. An edit that must not overwrite a change made meanwhile sends WATCH first.
  string: { edit: { label: 'Edit value', what: 'value', cmds: ['WATCH', 'SET'] } },
  json: { edit: { label: 'Edit document', what: 'document', cmds: ['JSON.SET'] } },
  hash: {
    item: 'field',
    edit: { label: 'Edit value', what: 'value', cmds: ['HSET'] },
    add: { label: 'Add field', cmds: ['HSETNX'], note: 'A field that exists already is never overwritten: edit it instead.' },
    remove: { label: 'Delete field', cmds: ['HDEL'] },
    fieldTtl: { label: 'Field TTL', cmds: ['HEXPIRE'] },
  },
  list: {
    item: 'element',
    edit: { label: 'Edit element', what: 'element', cmds: ['WATCH', 'LSET'] },
    add: { label: 'Add element', cmds: ['RPUSH'], note: 'Added at the end of the list (RPUSH).' },
    remove: { label: 'Delete element', cmds: ['WATCH', 'LSET', 'LREM'] },
  },
  set: {
    item: 'member',
    edit: { label: 'Edit member', what: 'member', cmds: ['SREM', 'SADD'] },
    add: { label: 'Add member', cmds: ['SADD'], note: 'A member that is there already is left as it is.' },
    remove: { label: 'Delete member', cmds: ['SREM'] },
  },
  zset: {
    item: 'member',
    edit: { label: 'Edit score', what: 'score', cmds: ['ZADD'] },
    add: { label: 'Add member', cmds: ['ZADD'], note: 'A member that is there already keeps its score (ZADD NX).' },
    remove: { label: 'Delete member', cmds: ['ZREM'] },
  },
  stream: {
    item: 'entry',
    noEdit: 'A stream entry can\'t be edited: add an entry, or delete this one.',
    add: { label: 'Add entry', cmds: ['XADD'], note: 'The server picks the entry\'s ID (XADD *).' },
    remove: { label: 'Delete entry', cmds: ['XDEL'] },
  },
};

/** The commands of the key's own actions. */
/** What the server says to a key write while a bulk action counts or runs. */
export const BULK_RUNS = 'A bulk action is running: wait for it to end, or stop it first.';

export const KEY_CMDS = { ttl: ['EXPIRE'], rename: ['RENAMENX'], delete: ['UNLINK'], bulkExpire: ['EXPIRE'], bulkDelete: ['UNLINK'] };

/** The types a new key can have, with how the page names them. */
export const NEW_KEY_TYPES = [['string', 'String'], ['hash', 'Hash'], ['list', 'List'], ['set', 'Set'], ['zset', 'Sorted set'], ['stream', 'Stream']];

/** Why a read-only connection writes nothing, in the dashboard's words. */
export function readOnlyText(conn) {
  if (!conn) return 'Connect first.';
  return conn.env === 'PROD'
    ? 'Read-only: this is a production connection, so nothing is written to it. Turn writes on in Connections.'
    : `Read-only: nothing is written to ${conn.name}. Turn writes on in Connections.`;
}

/** Why a write that sends `cmds` is off now, or null when it may be tried: no
 *  connection, a read-only one, a command the server refused for this user this session,
 *  and, given the value, a key that is gone, still loading or with a write on its way.
 *  The server decides in the end; this only keeps a button from offering what it would
 *  refuse anyway. */
export function writeOff(conn, cmds = [], v = null) {
  if (!conn) return 'Connect first.';
  if (conn.readOnly) return readOnlyText(conn);
  const refused = (conn.refused || []).map((c) => String(c).toUpperCase());
  const hit = cmds.find((c) => refused.includes(c));
  if (hit) return `Off: the server refused ${hit} for this user.`;
  if (v) {
    if (v.gone) return 'The key no longer exists.';
    if (v.loading) return 'Still loading the value: wait for it.';
    if (v.writing) return 'The last write has not answered yet.';
  }
  return null;
}

/** Why a value's own edit (the string, the document, the selected item) is off for its
 *  kind, before any connection rule; null when the kind can be edited. */
export function editOff(v) {
  const e = EDITS[v.kind];
  if (!e) {
    if (v.kind === 'vector') return 'redis-lens does not edit vector sets.';
    if (v.kind === 'array') return 'redis-lens does not edit arrays.';
    return `redis-lens does not edit values of type ${v.type || v.kind}.`;
  }
  if (!e.edit) return e.noEdit;
  if (v.kind === 'json' && !v.value) return 'The document is not read, so it can\'t be edited here.';
  if (v.kind === 'string' && v.cut) return 'Editing is off: only the first 64 KiB of this string is loaded, and an edit needs the whole value.';
  if (v.kind === 'json' && v.cut) return 'Editing is off: only the first 64 KiB of this document is loaded, and an edit needs the whole document.';
  return null;
}

/** Hex digits as typed (spaces allowed), lower case; null when they are not whole bytes. */
export function cleanHex(s) {
  const t = String(s ?? '').replace(/\s+/g, '').toLowerCase();
  return /^(?:[0-9a-f]{2})*$/.test(t) ? t : null;
}

/** Text as the hex of its UTF-8 bytes. */
export function hexOfText(s) {
  return Array.from(new TextEncoder().encode(String(s ?? '')), (b) => b.toString(16).padStart(2, '0')).join('');
}

// Characters a text box can't hold faithfully, or that would hide what is there: control
// characters but tab and newline (a text box turns CR into a newline), DEL and the C1
// controls, the bidirectional controls, the line and paragraph separators and the byte
// order mark.
const NOT_TYPABLE = /[\u0000-\u0008\u000b-\u001f\u007f-\u009f\u061c\u200e\u200f\u2028\u2029\u202a-\u202e\u2066-\u2069\ufeff]/;

/** The text that hex holds, when it is UTF-8 that can be typed in a text box as it is;
 *  null otherwise (then it is edited as hex). */
export function textOfHex(hex) {
  const clean = cleanHex(hex);
  if (clean === null) return null;
  const bytes = new Uint8Array(clean.length / 2);
  for (let i = 0; i < bytes.length; i += 1) bytes[i] = parseInt(clean.slice(i * 2, i * 2 + 2), 16);
  let s;
  try {
    s = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(bytes);
  } catch {
    return null;
  }
  return NOT_TYPABLE.test(s) ? null : s;
}

/** How an edit starts from the value's bytes (hex): as text when it can be typed, else as hex. */
export function editStart(hex) {
  const text = textOfHex(hex);
  return text === null ? { hex: true, text: cleanHex(hex) ?? '' } : { hex: false, text };
}

/** What a value box sends: the text, or `{ hex }`; `{ error }` for hex that is not whole bytes. */
export function valueFrom(hex, typed) {
  if (!hex) return { value: String(typed ?? '') };
  const clean = cleanHex(typed);
  return clean === null ? { error: 'Hex digits come in pairs, 0 to 9 and a to f, two for each byte.' } : { value: { hex: clean } };
}

/** A key list with one row replaced (after an edit, its TTL, size and length read again;
 *  after a rename, under its new name). */
export function replaceRow(list, id, row) {
  if (!list || !row) return list;
  const at = list.rows.findIndex((r) => r.id === id);
  if (at < 0) return list;
  const rows = list.rows.slice();
  rows[at] = row;
  return { ...list, rows };
}

/** A key list without a deleted key. */
export function dropRow(list, id) {
  if (!list) return list;
  const rows = list.rows.filter((r) => r.id !== id);
  if (rows.length === list.rows.length) return list;
  return { ...list, rows, loaded: Math.max(0, list.loaded - 1) };
}

/** A key list with a key added (first), or its row replaced when it is listed already. */
export function addRow(list, row) {
  if (!list || !row) return list;
  if (list.rows.some((r) => r.id === row.id)) return replaceRow(list, row.id, row);
  return { ...list, rows: [row].concat(list.rows), loaded: list.loaded + 1 };
}

/** The item an index names now: the same index, or the last one when it is past the end
 *  (after a delete), as the TUI keeps its selection. */
export function itemAt(v, index) {
  if (!v || !v.items.length) return null;
  return v.items.find((i) => i.index === index) || v.items[Math.min(Math.max(0, index ?? 0), v.items.length - 1)];
}

/** The name of a new key's first guess: the pattern's text before its first placeholder. */
export function newKeyName(pattern) {
  return String(pattern ?? '').split('{')[0];
}

/** What an item is called, to name it on its buttons: `field coupon`, `element #3`. */
export function itemLabel(v, item) {
  if (!item) return '';
  const what = (EDITS[v.kind] && EDITS[v.kind].item) || 'item';
  if (v.kind === 'list') return `element #${item.index}`;
  return `${what} ${item.name ?? item.value ?? ''}`;
}

/** A bulk action's progress: how far the run is, for the meter and its words. */
export function bulkProgress(b) {
  if (!b) return null;
  if (b.stage === 'counting') return { pct: null, text: `Dry run: ${plural(b.count.matched, 'key', 'keys')} of the pattern counted so far. Nothing is written.` };
  if (!b.run) return null;
  const r = b.run;
  const total = b.count.matched || 0;
  const pct = total ? Math.min(100, Math.round((r.seen / total) * 100)) : 0;
  const did = b.action === 'expire' ? `${plural(r.changed, 'key', 'keys')} given a TTL of ${b.ttl || '?'}` : `${plural(r.changed, 'key', 'keys')} deleted`;
  const parts = [did, `${fmtGrouped(r.seen)} of about ${fmtGrouped(total)} looked at`];
  if (r.skipped) parts.push(`${fmtGrouped(r.skipped)} left alone`);
  if (r.failed) parts.push(`${fmtGrouped(r.failed)} failed`);
  return { pct, text: `${parts.join(' · ')}.` };
}

/** What a bulk action's button that starts the run says. */
export function bulkRunLabel(b) {
  const n = b.count.toChange;
  return b.action === 'expire' ? `Set the TTL on ${plural(n, 'key', 'keys')}…` : `Delete ${plural(n, 'key', 'keys')}…`;
}

// ---------------------------------------------------------------------------------------
// The memory, kept while other areas are shown
// ---------------------------------------------------------------------------------------

const store = blank();
let active = null;

/** Find keys by name (from the top bar). Runs now when Browse is shown, else when it is. */
export function requestFind(text) {
  store.pending = { find: text };
  if (active) active.takePending();
}

/** Open a key's value (`Name`), from Analyze or Overview. */
export function requestKey(name) {
  store.pending = { key: name };
  if (active) active.takePending();
}

/** List a pattern's keys (`Name`), narrowed to a node on a cluster when given. */
export function requestPattern(name, node = null) {
  store.pending = { pattern: name, node };
  if (active) active.takePending();
}

/** What Browse holds already for the session and run of `state`: the tree's patterns
 *  (from the analysis, or from the names read so far) and the keys of the list loaded.
 *  Go to searches these; it reads nothing from the server. Empty for another session. */
export function held(state) {
  const c = state && state.conn;
  const who = c ? `${c.name}|${c.docker}|${state.analysis ? state.analysis.run : 0}` : null;
  if (!who || store.who !== who) return { patterns: [], keys: [] };
  const patterns = [];
  for (const r of (store.tree && store.tree.rows) || []) {
    if (r.kind === 'pattern') patterns.push(r.pattern);
    else if (r.kind === 'group') for (const ch of r.children) patterns.push(ch.pattern);
  }
  return { patterns, keys: store.list ? store.list.rows.slice() : [] };
}

// ---------------------------------------------------------------------------------------
// The view: Keys, Streams and Search, as the TUI's Browse has them
// ---------------------------------------------------------------------------------------

/** Browse's panels, as the TUI's Keys · Streams · Search. */
export const PANELS = [
  { id: 'keys', label: 'Keys', href: '#/browse' },
  { id: 'streams', label: 'Streams', href: '#/browse/streams' },
  { id: 'search', label: 'Search', href: '#/browse/search' },
];

/** The panel a route names (`#/browse/streams`); Keys for anything else. */
export function panelOf(sub) {
  return sub === 'streams' || sub === 'search' ? sub : 'keys';
}

const panelTabs = (panel) => pills(PANELS, panel, null, 'Browse panels');

export function mount(root, ctx, route = null) {
  const panel = panelOf(route && route.sub);
  if (panel === 'keys') return mountKeys(root, ctx);
  const state = ctx.state();
  const gate = connectFirst(state, { needsLive: true });
  const view = h('section', { class: 'view', 'aria-labelledby': 'browse-title' });
  root.append(view);
  const subtitle = h('p', { class: 'subtitle' });
  const actions = h('div', { class: 'actions' });
  view.append(
    h('div', { class: 'view-head' }, h('div', null, h('h1', { class: 'title', id: 'browse-title', text: 'Browse' }), subtitle), actions),
    panelTabs(panel));
  if (gate) {
    view.append(gate);
    return () => {};
  }
  const body = h('div', { class: 'stack-lg' });
  view.append(body);
  return (panel === 'streams' ? streams : search).mount(view, ctx, { head: { subtitle, actions }, body });
}

function mountKeys(root, ctx) {
  const state = ctx.state();
  const gate = connectFirst(state, { needsLive: true });
  const view = h('section', { class: 'view', 'aria-labelledby': 'browse-title' });
  root.append(view);
  if (gate) {
    view.append(h('h1', { class: 'title', id: 'browse-title', text: 'Browse' }), panelTabs('keys'), gate);
    return () => {};
  }
  follow(store, state);

  let alive = true;
  const timers = new Set();
  const later = (fn, ms) => {
    const t = setTimeout(() => {
      timers.delete(t);
      if (alive) fn();
    }, ms);
    timers.add(t);
  };

  // ---------- the head: title, the node the patterns are of ----------
  // The database is picked in the top bar (app.js); Browse reads the list on mount, as it
  // did when the picker was here, and hands it to the top bar.
  const subtitle = h('p', { class: 'subtitle' });
  const nodeBox = h('div', { class: 'picker' });
  view.append(h('div', { class: 'view-head' },
    h('div', null, h('h1', { class: 'title', id: 'browse-title', text: 'Browse' }), subtitle),
    h('div', { class: 'actions' }, nodeBox)),
  panelTabs('keys'));

  // ---------- the three cards ----------
  const patFilter = h('input', {
    class: 'field mono', type: 'search', id: 'browse-pat-filter', placeholder: 'Filter patterns', autocomplete: 'off', spellcheck: false, value: store.filter, 'data-act': 'filter',
    oninput: () => {
      store.filter = patFilter.value;
      drawRows();
    },
  });
  const sortBox = h('div', { class: 'picker' });
  const patSource = h('p', { class: 'muted small' });
  const patRows = h('ul', { class: 'list tree', 'aria-label': 'Key patterns' });
  const patFoot = h('div', { class: 'card-foot' });
  const patCard = h('div', { class: 'card stack browse-col pats', 'data-pane': 'patterns' },
    h('div', { class: 'card-head' }, h('h2', { class: 'lbl', text: 'Key patterns' }), sortBox),
    patSource,
    h('div', null, h('label', { class: 'sr-only', htmlFor: 'browse-pat-filter', text: 'Filter the patterns' }), patFilter),
    patRows, patFoot);

  const keysCard = h('div', { class: 'card stack browse-col keys', 'data-pane': 'keys' });
  const valueCard = h('div', { class: 'card stack browse-col value', 'aria-live': 'polite', 'aria-busy': 'false', 'data-pane': 'value' });
  // A bulk action over a pattern's keys: its dry run, question, progress and summary.
  const bulkBox = h('section', { class: 'card stack bulk', 'aria-label': 'Bulk action', hidden: true });
  view.append(bulkBox, h('div', { class: 'browse' }, patCard, keysCard, valueCard));

  // ---------- the database list, for the top bar's database control ----------
  // The key is taken before asking, so a reply that arrives after a switch or a new run is
  // not shown for the new one.
  async function loadDbs() {
    const key = ctx.dbListKey ? ctx.dbListKey() : null;
    try {
      store.dbs = await ctx.api.getDbs();
      store.dbsError = null;
    } catch (err) {
      store.dbs = null;
      store.dbsError = errorText(err);
    }
    if (alive && ctx.showDbs) ctx.showDbs(key, store.dbs, store.dbsError);
  }

  // ---------- the tree ----------
  let treeSeq = 0;
  async function loadTree() {
    const mine = (treeSeq += 1);
    try {
      const tree = await ctx.api.getBrowse({ sort: store.sort, node: store.node });
      if (!alive || mine !== treeSeq) return;
      store.tree = tree;
      store.treeError = null;
      store.sort = tree.sort;
    } catch (err) {
      if (!alive || mine !== treeSeq) return;
      // A sort or node no longer offered (a new run): back to the defaults.
      if ((err.status === 400 && store.sort) || (err.status === 422 && store.node !== null)) {
        store.sort = null;
        store.node = null;
        loadTree();
        return;
      }
      store.treeError = errorText(err);
    }
    drawTree();
    const t = store.tree;
    // Names are read by redis-lens in the background: look again while it reads, and while
    // nothing is there yet (the live connection opening, an analysis starting).
    if (!store.treeError && t && ((t.names && t.names.reading) || t.source === 'none')) later(loadTree, 900);
  }

  async function moreNames(button) {
    button.disabled = true;
    try {
      store.tree = await ctx.api.moreNames({ sort: store.sort });
      store.treeError = null;
    } catch (err) {
      ctx.notify(errorText(err), 'error');
    }
    if (alive) drawTree();
  }

  function drawTree() {
    const t = store.tree;
    const report = ctx.state().analysis.report;
    patSource.textContent = store.treeError ? '' : treeSource(t);
    subtitle.textContent = t ? subtitleText(t) : ' ';
    // The sort: from names only keys and name.
    if (t && t.sorts.length > 1) {
      const select = h('select', {
        class: 'field', id: 'browse-sort', 'data-act': 'sort',
        onchange: () => {
          store.sort = select.value;
          loadTree();
        },
      }, t.sorts.map((s) => h('option', { value: s, selected: s === t.sort, text: SORT_LABELS[s] || s })));
      if (!sortBox.contains(document.activeElement)) fill(sortBox, h('label', { htmlFor: 'browse-sort', text: 'Sort' }), select);
    } else fill(sortBox);
    // On a cluster the analysis's patterns can be narrowed to one node.
    if (t && t.source === 'analysis' && t.nodes.length > 1) {
      const select = h('select', {
        class: 'field', id: 'browse-node',
        onchange: () => {
          store.node = select.value === '' ? null : Number(select.value);
          loadTree();
        },
      }, h('option', { value: '', selected: t.node === null, text: 'All nodes' }),
      t.nodes.map((n) => h('option', { value: String(n.index), selected: n.index === t.node, text: `${n.endpoint}${n.status && n.status !== 'scanned' ? ` (${n.status})` : ''}` })));
      if (!nodeBox.contains(document.activeElement)) fill(nodeBox, h('label', { htmlFor: 'browse-node', text: 'Patterns of' }), select);
    } else fill(nodeBox);
    drawRows();
    drawPatFoot(report);
  }

  function subtitleText(t) {
    const s = ctx.state();
    const db = s.analysis.db;
    const total = t.source === 'names' && t.names && typeof t.names.dbsize === 'number' ? `${fmtGrouped(t.names.dbsize)} keys in db ${db}` : `db ${db}`;
    return [total, analyzing(s) ? 'an analysis is running' : null].filter(Boolean).join(' · ');
  }

  function drawPatFoot(report) {
    const t = store.tree;
    const n = t && t.names;
    const notes = [];
    if (store.treeError) notes.push(notice(store.treeError));
    if (n) {
      if (n.error) notes.push(notice(n.error));
      for (const e of n.refused) notes.push(notice(`${e}: the server refused SCAN for this user, so its key names are not read.`));
      for (const e of n.skipped) notes.push(notice(`${e}: did not answer SCAN twice, so its key names are left out and the other nodes go on.`));
      for (const e of n.missing) if (!n.refused.includes(e) && !n.skipped.includes(e)) notes.push(notice(`${e}: not connected, so its key names are not read.`));
    }
    const more = n && !n.done && !n.reading && t.source === 'names'
      ? h('button', { type: 'button', class: 'btn', text: 'Read 1,000 more names', 'data-act': 'more', onclick: (e) => moreNames(e.currentTarget) })
      : null;
    refill(patFoot, [
      h('p', { class: 'muted small', text: treeFoot(t, report) }),
      t && t.source === 'names' ? h('p', { class: 'muted small', text: 'Browse reads key names with SCAN, 1,000 a call, and nothing else until a key is opened. Sizes, TTLs and types come from an analysis.' }) : null,
      more,
      notes,
    ]);
  }

  function drawRows() {
    const t = store.tree;
    if (!t) {
      refill(patRows, h('li', { class: 'muted pad', text: store.treeError ? '' : 'Reading key names…' }));
      return;
    }
    if (!t.rows.length) {
      refill(patRows, h('li', { class: 'muted pad', text: t.source === 'none' ? t.why || 'Nothing to show yet.' : 'No patterns.' }));
      return;
    }
    const rows = filterRows(t.rows, store.filter);
    if (!rows.length) {
      refill(patRows, h('li', { class: 'muted pad', text: 'No pattern matches the filter.' }));
      return;
    }
    const exact = treeExact(t, ctx.state().analysis.report);
    const out = [];
    for (const r of rows) {
      if (r.kind === 'group') {
        const closed = store.closed.has(r.key) && !store.filter;
        out.push(h('li', null, h('button', {
          type: 'button', class: 'row row-pat group', 'data-group': '', 'aria-expanded': closed ? 'false' : 'true', 'data-row': `g:${r.key}`,
          title: r.reason || '',
          onclick: () => {
            if (store.closed.has(r.key)) store.closed.delete(r.key);
            else store.closed.add(r.key);
            drawRows();
          },
        },
        h('span', { class: 'name' },
          h('span', { class: 'mono clip block', text: `${closed ? '▸' : '▾'} ${r.label}` }),
          h('span', { class: 'sub', text: `${plural(r.children.length, 'pattern', 'patterns')} · ${figureKeys(r, exact)}` })),
        h('span', { class: 'glyph', 'aria-hidden': 'true', text: r.marked ? '▲' : '' }),
        h('span', { class: 'mono right fig', text: figure(r, t) }))));
        if (!closed) for (const c of r.children) out.push(patternRow(c, t, exact, true));
      } else out.push(patternRow(r, t, exact, false));
    }
    refill(patRows, out);
  }

  const figureKeys = (r, exact) => (r.kind === 'group' ? `${exact ? '' : '~'}${fmtCount(r.estKeys)} keys` : keysText(r, exact));
  const figure = (r, t) => (t.sizes && typeof r.estBytes === 'number' ? fmtBytes(r.estBytes) : fmtPercent(r.share));

  function patternRow(r, t, exact, child) {
    const on = store.picked && store.picked.id === r.pattern.id && (!store.list || store.list.kind === 'pattern');
    const change = r.change ? ` · ${r.change.words}${typeof r.change.deltaBytes === 'number' && r.change.deltaBytes !== 0 ? ` ${fmtSignedBytes(r.change.deltaBytes)}` : ''}` : '';
    return h('li', null, h('button', {
      type: 'button', class: `row row-pat${child ? ' child' : ''}${on ? ' on' : ''}`, 'aria-current': on ? 'true' : null, 'data-row': `p:${r.pattern.id}`,
      onclick: () => openPattern(r.pattern, r),
    },
    h('span', { class: 'name' },
      patternName(r.pattern.name, { literal: r.literal === true, cls: 'mono clip block' }),
      h('span', { class: 'sub', text: `${keysText(r, exact)}${r.type ? ` · ${r.type}` : ''}${change}` })),
    h('span', { class: 'glyph', 'aria-hidden': 'true', text: r.marked ? '▲' : '' }),
    r.marked ? h('span', { class: 'sr-only', text: 'marked: worth a look' }) : null,
    h('span', { class: 'mono right fig', text: figure(r, t) })));
  }

  /** The tree's row of a pattern, when it is in the tree shown. */
  function rowOf(id) {
    const t = store.tree;
    if (!t) return null;
    for (const r of t.rows) {
      if (r.kind === 'pattern' && r.pattern.id === id) return r;
      if (r.kind === 'group') for (const c of r.children) if (c.pattern.id === id) return c;
    }
    return null;
  }

  /** Whether a pattern is a literal key name in the tree shown (its `{…}` is not a placeholder). */
  const literalOf = (pattern) => {
    const r = pattern ? rowOf(pattern.id) : null;
    return !!(r && r.literal === true);
  };

  // ---------- a pattern's keys, and Find ----------
  let listSeq = 0;
  async function openPattern(name, row = null) {
    const mine = (listSeq += 1);
    store.picked = name;
    store.finding = null;
    store.list = null;
    store.listError = null;
    store.listFilter = '';
    store.listShown = CHUNK;
    drawRows();
    drawKeys('Reading the keys…');
    try {
      const list = await ctx.api.getKeys({ pattern: name.id, node: store.node });
      if (!alive || mine !== listSeq) return;
      store.list = list;
    } catch (err) {
      if (!alive || mine !== listSeq) return;
      store.listError = errorText(err);
    }
    drawKeys();
    if (row === null) drawRows();
  }

  async function runFind(text) {
    const mine = (listSeq += 1);
    if (!store.finding && store.picked) store.back = store.picked;
    store.finding = text;
    store.list = null;
    store.listError = null;
    store.listFilter = '';
    store.listShown = CHUNK;
    drawRows();
    drawKeys('Finding keys…');
    try {
      const list = await ctx.api.findKeys(text);
      if (!alive || mine !== listSeq) return;
      store.list = list;
    } catch (err) {
      if (!alive || mine !== listSeq) return;
      store.listError = errorText(err);
      ctx.notify(store.listError, 'error');
    }
    drawKeys();
  }

  async function moreKeys(button) {
    const l = store.list;
    if (!l) return;
    button.disabled = true;
    button.textContent = 'Reading…';
    try {
      store.list = mergeList(l, await ctx.api.moreKeys(l.list));
    } catch (err) {
      // A list redis-lens no longer keeps (it keeps the newest 16): read it again.
      if (err.status === 404) {
        if (l.kind === 'find' && store.finding) runFind(store.finding);
        else if (store.picked) openPattern(store.picked);
        return;
      }
      ctx.notify(errorText(err), 'error');
    }
    if (alive) drawKeys();
  }

  function clearFind() {
    store.finding = null;
    store.list = null;
    store.listError = null;
    const back = store.back;
    store.back = null;
    if (back) openPattern(back);
    else {
      store.picked = null;
      drawKeys();
      drawRows();
    }
  }

  function drawKeys(busy = null) {
    const l = store.list;
    const title = store.finding !== null
      ? `Find: ${l && l.find ? l.find.glob : store.finding}`
      : store.picked ? store.picked.name : 'Keys';
    const addOff = keyOff(ctx.state().conn);
    const head = h('div', { class: 'card-head' },
      h('h2', { class: 'mono key-title clip', text: title, title }),
      h('div', { class: 'row-actions' },
        store.finding !== null ? h('button', { type: 'button', class: 'btn quiet', text: 'Clear search', onclick: clearFind }) : null,
        h('button', { type: 'button', class: 'btn', text: 'Add key', disabled: !!addOff, title: addOff || 'Add a key with its first value. An existing key is never overwritten.', 'data-act': 'key.add', onclick: () => addKeyDialog() })),
      addOff ? whyLines([addOff]) : null);
    if (busy) {
      fill(keysCard, head, h('p', { class: 'muted', text: busy }));
      return;
    }
    if (store.listError) {
      fill(keysCard, head, notice(store.listError));
      return;
    }
    if (!l) {
      fill(keysCard, head, emptyState(store.tree && store.tree.rows.length ? 'Pick a pattern to list its keys, or find a key by name in the box at the top.' : 'Find a key by name in the box at the top, or pick a pattern once there are some.'));
      return;
    }
    const row = l.kind === 'pattern' && store.picked ? rowOf(store.picked.id) : null;
    const filter = h('input', {
      class: 'field mono', type: 'search', id: 'browse-key-filter', placeholder: 'Filter the loaded keys', autocomplete: 'off', spellcheck: false, value: store.listFilter, 'data-act': 'filter',
      oninput: () => {
        store.listFilter = filter.value;
        store.listShown = CHUNK;
        drawKeyRows(rowsBox, showBox);
      },
    });
    const rowsBox = h('ul', { class: 'list', 'aria-label': 'Keys' });
    const showBox = h('div');
    drawKeyRows(rowsBox, showBox);
    const more = !l.done && !l.capped
      ? h('button', { type: 'button', class: 'btn', text: `Load ${fmtGrouped(l.pageKeys)} more`, 'data-act': 'more', onclick: (e) => moreKeys(e.currentTarget) })
      : null;
    const focused = keysCard.contains(document.activeElement) ? document.activeElement.id : null;
    fill(keysCard,
      head,
      row ? patternSummary(row) : null,
      l.kind === 'find' && l.find ? h('p', { class: 'muted small', text: /[*?[]/.test(l.find.text) ? 'A pattern, used as typed.' : `Names that contain ${l.find.text}.` }) : null,
      h('div', null, h('label', { class: 'sr-only', htmlFor: 'browse-key-filter', text: 'Filter the loaded keys' }), filter),
      h('div', { class: 'cols-head key-cols', 'aria-hidden': 'true' }, h('span', { text: 'Key' }), h('span', { text: 'Type' }), h('span', { class: 'right', text: 'Size' }), h('span', { class: 'right', text: 'Expires' })),
      rowsBox,
      showBox,
      h('div', { class: 'card-foot' },
        h('p', { class: 'muted small', text: [listFoot(l), l.cluster && l.cluster.nodesDone !== null && l.cluster.walk.length > 1 && !l.done ? `${l.cluster.nodesDone} of ${l.cluster.walk.length} nodes done.` : null].filter(Boolean).join(' ') }),
        more,
        listNotes(l).map((t) => notice(t))));
    if (focused === 'browse-key-filter') {
      filter.focus();
    }
  }

  function drawKeyRows(box, showBox) {
    const l = store.list;
    const q = store.listFilter.trim().toLowerCase();
    const rows = q ? l.rows.filter((r) => r.name.toLowerCase().includes(q)) : l.rows;
    const shown = rows.slice(0, store.listShown);
    const nodes = ctx.state().conn ? ctx.state().conn.nodes : [];
    refill(box, shown.length ? shown.map((r) => {
      const on = store.valueId === r.id;
      const open = on && store.value && store.value.key && store.value.key.id === r.id;
      return h('li', null, h('button', {
        type: 'button', class: `row row-key${on ? ' on' : ''}`, 'aria-current': on ? 'true' : null, 'data-row': r.id,
        onclick: () => openValue(r),
      },
      h('span', { class: 'mono clip', text: r.name, title: r.name }),
      h('span', { class: 'badge mono', text: r.type || DASH }),
      h('span', { class: 'mono right', text: fmtBytes(r.bytes) }),
      h('span', { class: 'mono right muted', text: r.ttl || DASH }),
      typeof r.node === 'number' && nodes[r.node] ? h('span', { class: 'sr-only', text: `on node ${nodes[r.node].address}` }) : null),
      // The open key's own writes, beside its row as well as above its value.
      open ? h('div', { class: 'row-tools', role: 'group', 'aria-label': `Write to ${r.name}` }, keyButtons(store.value, true)) : null);
    }) : h('li', { class: 'muted pad', text: q ? 'No loaded key matches the filter.' : listFoot(l) }));
    fill(showBox, rows.length > shown.length
      ? h('button', {
        type: 'button', class: 'btn quiet', text: `Show ${fmtGrouped(Math.min(CHUNK, rows.length - shown.length))} more of the ${fmtGrouped(rows.length)} loaded`,
        onclick: () => {
          store.listShown += CHUNK;
          drawKeyRows(box, showBox);
        },
      })
      : null);
  }

  function patternSummary(r) {
    const t = store.tree;
    const exact = treeExact(t, ctx.state().analysis.report);
    const parts = [keysText(r, exact)];
    if (t && t.sizes && typeof r.estBytes === 'number') parts.push(`${fmtBytes(r.estBytes)}, ${fmtPercent(r.share)} of memory`);
    else if (typeof r.share === 'number') parts.push(`${fmtPercent(r.share)} of the keys`);
    if (t && t.source === 'names' && typeof r.namesSeen === 'number') parts.push(`${fmtGrouped(r.namesSeen)} of the names read`);
    const details = [];
    if (typeof r.p50 === 'number') details.push(h('p', { class: 'small', text: `Typical key ${fmtBytes(r.p50)}, 99% under ${fmtBytes(r.p99)}, largest ${fmtBytes(r.max)}.` }));
    if (typeof r.noTtlRatio === 'number') details.push(h('p', { class: 'small', text: `${fmtPercent(r.noTtlRatio)} of its keys never expire${r.ttlVerdict ? ` (${r.ttlVerdict})` : ''}.` }));
    if (r.ttlHist && t && t.ttlLabels.length === r.ttlHist.length) details.push(ttlHistogram(t.ttlLabels, r.ttlHist));
    if (r.examples.length) {
      details.push(h('p', { class: 'lbl', text: 'Example keys' }),
        h('ul', { class: 'list' }, r.examples.map((k) => h('li', null, h('button', {
          type: 'button', class: 'row row-example', onclick: () => openValue(k),
        }, h('span', { class: 'mono clip', text: k.name }), h('span', { class: 'mono right', text: fmtBytes(k.bytes) }), h('span', { class: 'mono right muted', text: k.ttl || DASH }))))));
    }
    const command = r.command
      ? h('div', { class: 'command' }, h('code', { class: 'mono wrap-any', text: `$ ${r.command}` }), copyButton('Copy command', r.command, 'the command', null, { act: 'copy.command' }))
      : h('p', { class: 'muted small', text: 'No command to list them: the pattern has characters that are not safe to paste in every shell.' });
    details.push(command);
    return h('div', { class: 'summary' },
      h('p', { text: parts.join(' · ') }),
      r.marked && r.reason ? notice(r.reason) : r.reason ? h('p', { class: 'muted small', text: r.reason }) : null,
      h('div', { class: 'row-actions' }, copyButton('Copy pattern', r.pattern.copy, 'the pattern', 'The pattern has control characters, so it is not copied: a pasted copy would name other keys.', { act: 'copy.pattern' })),
      bulkButtons(r.pattern),
      h('details', { class: 'advanced' }, h('summary', { text: 'About this pattern' }), h('div', { class: 'details-body' }, details)));
  }

  // ---------- a key's value ----------
  let valueSeq = 0;
  async function openValue(key) {
    const mine = (valueSeq += 1);
    store.valueId = key.id;
    store.value = null;
    store.valueError = null;
    store.item = null;
    store.shown = null;
    store.format = null;
    store.itemsShown = CHUNK;
    redrawKeyRows();
    drawValue('Reading the value…', key);
    try {
      const v = await ctx.api.openValue({ key: key.id, type: key.type || null });
      if (!alive || mine !== valueSeq) return;
      store.value = v;
      store.shown = v.value;
      // The first item is shown whole at once, as the TUI selects it.
      if (v.items.length) pickItem(v.items[0], 0, false);
    } catch (err) {
      if (!alive || mine !== valueSeq) return;
      store.valueError = errorText(err);
    }
    drawValue();
    redrawKeyRows();
  }

  /** Draw the key list's rows again (the open key, its tools), as they are. */
  function redrawKeyRows() {
    if (!store.list) return;
    const box = keysCard.querySelector('ul.list[aria-label="Keys"]');
    if (box) drawKeyRows(box, box.nextElementSibling);
  }

  async function moreValue(button) {
    const v = store.value;
    if (!v) return;
    button.disabled = true;
    button.textContent = 'Reading…';
    try {
      const next = await ctx.api.moreValue(v.view);
      if (!alive || store.value !== v) return;
      store.value = next;
      if (next.notice && next.items.length === v.items.length) ctx.notify(next.notice, 'info');
    } catch (err) {
      if (err.status === 404 && store.valueId) {
        openValue({ id: store.valueId, type: v.meta && v.meta.type });
        return;
      }
      ctx.notify(errorText(err), 'error');
    }
    if (alive) drawValue();
  }

  let itemSeq = 0;
  async function pickItem(item, field = 0, draw = true) {
    const v = store.value;
    if (!v) return;
    const mine = (itemSeq += 1);
    store.item = { index: item.index, field: v.kind === 'stream' ? field : null };
    if (draw) drawValue();
    await loadShown(v, mine);
  }

  async function loadShown(v, mine = (itemSeq += 1)) {
    const index = store.item ? store.item.index : 0;
    const field = store.item ? store.item.field : null;
    try {
      const it = await ctx.api.getItem({ view: v.view, index, field, format: store.format });
      if (!alive || mine !== itemSeq || store.value !== v) return;
      store.shown = it.shown;
    } catch (err) {
      if (!alive || mine !== itemSeq || store.value !== v) return;
      if (err.status === 404 && store.valueId) {
        openValue({ id: store.valueId, type: v.meta && v.meta.type });
        return;
      }
      store.shown = null;
      ctx.notify(errorText(err), 'error');
    }
    drawValue();
  }

  function pickFormat(format) {
    const v = store.value;
    if (!v) return;
    store.format = format;
    if (!store.item) store.item = v.items.length ? { index: v.items[0].index, field: v.kind === 'stream' ? 0 : null } : { index: 0, field: null };
    loadShown(v);
  }

  function drawValue(busy = null, key = null) {
    valueCard.setAttribute('aria-busy', busy ? 'true' : 'false');
    const v = store.value;
    const focusedName = valueCard.contains(document.activeElement) ? (document.activeElement.getAttribute('aria-label') || document.activeElement.textContent.trim()) : null;
    if (busy) {
      fill(valueCard, key ? h('h2', { class: 'mono key-title wrap-any', text: key.name }) : null, h('p', { class: 'muted', text: busy }));
      return;
    }
    if (store.valueError) {
      fill(valueCard, notice(store.valueError));
      return;
    }
    if (!v) {
      fill(valueCard, emptyState('Pick a key to see its value.'));
      return;
    }
    const s = ctx.state();
    const nodes = s.conn ? s.conn.nodes : [];
    const node = v.meta && typeof v.meta.node === 'number' && nodes[v.meta.node] ? nodes[v.meta.node].address : null;
    const name = v.key ? v.key.name : v.meta ? v.meta.name : '';
    const copyName = v.key ? v.key.copy : v.meta ? v.meta.copy : null;
    const shown = store.shown;
    const parts = [
      h('div', { class: 'value-head' },
        h('h2', { class: 'mono key-title wrap-any', text: name }),
        h('span', { class: 'badge mono', text: v.type || v.kind })),
      h('p', { class: 'muted small', text: valueFacts(v, node) || DASH }),
      v.pattern ? h('p', { class: 'muted small' }, 'Pattern ', patternName(v.pattern.name, { literal: literalOf(v.pattern) })) : null,
      h('div', { class: 'row-actions' },
        copyButton('Copy name', copyName, 'the key name', 'The name has control characters, so it is not copied: a pasted copy would name another key.', { act: 'copy.name' }),
        shown ? copyButton('Copy value', shown.copy, 'the value', shown.copyRefused || 'This value is not copied as text: show it as hex to copy it.', { act: 'copy.value' }) : null),
      writeBar(v),
    ];
    const notes = [];
    if (v.gone) notes.push(notice(v.notice || 'The key no longer exists.'));
    if (v.error) notes.push(notice(v.error));
    if (v.message) notes.push(notice(v.message, 'info'));
    v.refused.forEach((t) => notes.push(notice(t)));
    if (v.fieldTtlsOff) notes.push(notice(v.fieldTtlsOff));
    parts.push(notes);

    if (v.items.length) parts.push(itemsBox(v));
    if (v.module) parts.push(moduleBox(v.module));
    if (shown) parts.push(shownBox(shown, v));
    else if (v.items.length && store.item) parts.push(h('p', { class: 'muted small', text: 'Reading the item…' }));

    const foot = valueFoot(v);
    if (!v.gone && (foot || v.more)) {
      parts.push(h('div', { class: 'card-foot' },
        h('p', { class: 'muted small', text: v.gone ? '' : foot }),
        v.more ? h('button', { type: 'button', class: 'btn', text: 'Load more', 'data-act': 'more', onclick: (e) => moreValue(e.currentTarget) }) : null,
        v.capped ? h('p', { class: 'muted small', text: `At most ${fmtGrouped(v.maxItems)} items are held.` }) : null));
    }
    fill(valueCard, parts);
    if (focusedName) {
      const again = Array.from(valueCard.querySelectorAll('button, a, input, select')).find((el) => (el.getAttribute('aria-label') || el.textContent.trim()) === focusedName && !el.disabled);
      if (again) again.focus();
    }
  }

  function itemsBox(v) {
    const cols = itemColumns(v);
    const list = h('ul', { class: 'list items', 'aria-label': `Items of ${v.key ? v.key.name : 'the key'}` });
    const shown = v.items.slice(0, store.itemsShown);
    fill(list, shown.map((item) => {
      const on = store.item && store.item.index === item.index;
      const cells = itemCells(v, item);
      return h('li', null, h('button', {
        type: 'button', class: `row row-item cols-${cols.length}${on ? ' on' : ''}`, 'aria-current': on ? 'true' : null, 'data-row': String(item.index),
        onclick: () => pickItem(item, 0),
      }, cells.map((c, i) => h('span', { class: `mono clip${i ? ' muted' : ''}`, text: c, title: c }))));
    }));
    const fields = v.kind === 'stream' && store.item ? (v.items.find((i) => i.index === store.item.index) || {}).fields || [] : [];
    return h('div', { class: 'items-box' },
      h('div', { class: `cols-head row-item cols-${cols.length}`, 'aria-hidden': 'true' }, cols.map((c) => h('span', { text: c }))),
      list,
      itemBar(v),
      v.items.length > shown.length ? h('button', {
        type: 'button', class: 'btn quiet', text: `Show ${fmtGrouped(Math.min(CHUNK, v.items.length - shown.length))} more of the ${fmtGrouped(v.items.length)} loaded`,
        onclick: () => {
          store.itemsShown += CHUNK;
          drawValue();
        },
      }) : null,
      fields.length ? h('div', { class: 'tabs', role: 'group', 'aria-label': 'Fields of the entry' }, fields.map((f, j) => h('button', {
        type: 'button', class: store.item.field === j ? 'pill small on' : 'pill small', 'aria-pressed': store.item.field === j ? 'true' : 'false', text: f.name || '(empty)',
        onclick: () => pickItem(v.items.find((i) => i.index === store.item.index), j),
      }))) : null);
  }

  function shownBox(shown, v) {
    const formats = shown.formats.length > 1
      ? h('div', { class: 'tabs', role: 'group', 'aria-label': 'Show the value as' }, shown.formats.map((f) => h('button', {
        type: 'button', class: shown.format === f ? 'pill small on' : 'pill small', 'aria-pressed': shown.format === f ? 'true' : 'false', text: f === 'text' ? 'Text' : f === 'hex' ? 'Hex' : f, 'data-act': 'format',
        onclick: () => pickFormat(f),
      })))
      : null;
    const notes = [shown.note, shown.compression ? `Decompressed from ${shown.compression}.` : null, shown.truncated ? 'Truncated at 1 MiB.' : null, v.cut ? `Shown: the ${v.cut}.` : null].filter(Boolean);
    return h('div', { class: 'shown' },
      formats,
      h('div', { class: 'box value-box' },
        h('div', { class: 'value-label', text: shown.header }),
        h('pre', { class: 'mono value-pre', tabIndex: 0, 'aria-label': 'The value', text: shown.lines.join('\n') })),
      notes.map((t) => h('p', { class: 'muted small', text: t })));
  }

  function moduleBox(m) {
    const pairsTable = (title, rows) => (rows.length ? h('div', null, h('p', { class: 'lbl', text: title }),
      h('dl', { class: 'facts' }, rows.map((p) => h('div', null, h('dt', { class: 'mono', text: p.name }), h('dd', { class: 'mono', text: p.value }))))) : null);
    return h('div', { class: 'stack-sm' },
      m.label ? h('p', { class: 'strong', text: m.label }) : null,
      m.unavailable ? notice(m.unavailable) : null,
      pairsTable('Info', m.info),
      pairsTable('Top items', m.top),
      pairsTable('Figures', m.stats),
      m.samples.length ? h('div', null, h('p', { class: 'lbl', text: `Samples, newest first (${m.samples.length})` }),
        h('ul', { class: 'list' }, m.samples.slice(0, CHUNK).map((x) => h('li', { class: 'row row-sample' },
          h('span', { class: 'mono', text: typeof x.at === 'number' ? new Date(x.at).toISOString().replace('T', ' ').slice(0, 19) + ' UTC' : DASH }),
          h('span', { class: 'mono right', text: x.value }))))) : null,
      m.missing.map((t) => notice(t)));
  }

  function copyButton(label, value, what, refused = null, { act = null } = {}) {
    return h('button', {
      type: 'button', class: 'btn', text: label, 'data-act': act,
      onclick: async () => {
        if (value === null || value === undefined) {
          ctx.notify(refused || `There is nothing to copy for ${what}.`, 'info');
          return;
        }
        const problem = await copyText(value);
        ctx.notify(problem || `Copied ${what} to the clipboard.`, problem ? 'error' : 'ok');
      },
    });
  }

  function ttlHistogram(labels, counts) {
    const max = Math.max(1, ...counts.map((c) => (typeof c === 'number' ? c : 0)));
    return h('div', { class: 'hist', role: 'img', 'aria-label': `Expires in: ${labels.map((l, i) => `${l} ${counts[i] ?? 0}`).join(', ')}` },
      labels.map((l, i) => {
        const bar = meter(barPercent(counts[i], max), 'thin');
        return h('div', { class: 'hist-row', 'aria-hidden': 'true' }, h('span', { class: 'mono small', text: l }), bar, h('span', { class: 'mono small right', text: fmtCount(counts[i]) }));
      }));
  }

  // ---------- writes: the buttons ----------

  const conn = () => ctx.state().conn;

  /** Why a write to a key is off now: `writeOff`'s reasons, and a bulk action counting or
   *  running (the server takes no key write until it ends). */
  function keyOff(c, cmds, v = null) {
    const b = store.bulk;
    return writeOff(c, cmds, v) || (b && (b.stage === 'counting' || b.stage === 'running') ? BULK_RUNS : null);
  }

  /** A button for one write: off, saying why in its title, when it can't be sent. */
  function writeButton(label, off, run, { primary = false, quiet = false, danger = false, aria = null, act = null } = {}) {
    const cls = primary ? 'btn primary' : quiet ? 'btn quiet' : 'btn';
    return h('button', {
      type: 'button', class: danger ? `${cls} danger` : cls, text: label, disabled: !!off, title: off || null, 'aria-label': aria, 'data-act': act,
      onclick: (e) => run(e.currentTarget),
    });
  }

  /** The key's own writes: its expiry, rename, delete. `beside` names the key in each
   *  button's label, for the copies beside its row in the key list. */
  function keyButtons(v, beside = false) {
    const c = conn();
    const name = v.key ? v.key.name : '';
    const opt = (what) => (beside ? { quiet: true, aria: `${what} ${name}` } : {});
    return [
      writeButton('Set expiry', keyOff(c, KEY_CMDS.ttl, v), () => ttlDialog(null), { ...opt('Set the expiry of'), act: 'key.ttl' }),
      writeButton('Rename', keyOff(c, KEY_CMDS.rename, v), () => renameDialog(), { ...opt('Rename'), act: 'key.rename' }),
      writeButton('Delete key', keyOff(c, KEY_CMDS.delete, v), (b) => deleteKey(b), { ...opt('Delete'), danger: true, act: 'key.delete' }),
    ];
  }

  /** The writes above a value: its own edit (a string, a document), adding to it (the
   *  collections), then the key's. Below them, why the ones that are off are off. */
  function writeBar(v) {
    const c = conn();
    const e = EDITS[v.kind];
    const buttons = [];
    if (e && e.edit && !e.item) buttons.push(writeButton(e.edit.label, editOff(v) || keyOff(c, e.edit.cmds, v), (b) => startEdit(b, null), { act: 'value.edit' }));
    if (e && e.add) buttons.push(writeButton(e.add.label, keyOff(c, e.add.cmds, v), () => startAdd(), { act: 'item.add' }));
    buttons.push(...keyButtons(v));
    return h('div', { class: 'stack-sm write-bar' },
      h('div', { class: 'row-actions', role: 'group', 'aria-label': 'Write to this key' }, buttons),
      whyLines(buttons.map((b) => (b.disabled ? b.title : null))));
  }

  /** Why the buttons of a bar that are off are off, as text under it (not only in their
   *  titles): each reason once, and a read-only connection's as the one line that says
   *  which key turns writes on (keys.js `reasonLines`). */
  function whyLines(offs) {
    const c = conn();
    const lines = reasonLines(offs, c ? readOnlyText(c) : null);
    if (!lines.length) return null;
    return h('div', { class: 'bar-why' }, lines.map((t) => (t === readOnlyLine()
      ? h('div', { class: 'line' }, readOnlyNote(t), h('a', { href: '#/connections', class: 'small', text: 'Open Connections' }))
      : notice(t, 'info'))));
  }

  /** The writes of the selected field, element, member or entry, under the items. */
  function itemBar(v) {
    const e = EDITS[v.kind];
    if (!e || !e.item || !store.item) return null;
    const item = v.items.find((i) => i.index === store.item.index);
    if (!item) return null;
    const c = conn();
    const label = itemLabel(v, item);
    const buttons = [];
    if (e.edit) {
      const aria = v.kind === 'zset' ? `Edit the score of ${label}` : `Edit ${label}`;
      buttons.push(writeButton(e.edit.label, editOff(v) || keyOff(c, e.edit.cmds, v), (b) => startEdit(b, item), { aria, act: 'item.edit' }));
    }
    if (e.fieldTtl) {
      const off = v.fieldTtls ? keyOff(c, e.fieldTtl.cmds, v) : v.fieldTtlsOff || 'Field TTLs need Redis 7.4 or later, or Valkey 9.0 or later.';
      buttons.push(writeButton(e.fieldTtl.label, off, () => ttlDialog(item), { aria: `Set the TTL of ${label}`, act: 'item.ttl' }));
    }
    if (e.remove) buttons.push(writeButton(e.remove.label, keyOff(c, e.remove.cmds, v), (b) => removeItem(b, item), { aria: `Delete ${label}`, danger: true, act: 'item.delete' }));
    return h('div', { class: 'item-actions', role: 'group', 'aria-label': `Write to the selected ${label}` },
      h('p', { class: 'small muted wrap-any' }, 'Selected: ', h('span', { class: 'mono', text: label })),
      h('div', { class: 'row-actions' }, buttons),
      whyLines(buttons.map((b) => (b.disabled ? b.title : null))),
      e.noEdit ? h('p', { class: 'muted small', text: e.noEdit }) : null);
  }

  // ---------- writes: the forms ----------

  let formSeq = 0;

  /** One box of a write form. `f`: `{ name, label (text, or a function of the form's
   *  choices), kind: 'text' | 'value' | 'select', value, hex, options, hint, placeholder,
   *  required (what to say when it is left empty), show (a function of the choices) }`.
   *  A `value` box is a text area with "As hex" beside it. */
  function fieldRow(f, id, onChoice) {
    const err = h('p', { class: 'field-error', id: `${id}-err`, role: 'alert' });
    const hint = f.hint ? h('p', { class: 'hint', id: `${id}-hint`, text: f.hint }) : null;
    const label = h('label', { htmlFor: id });
    let el;
    let hexBox = null;
    let read;
    if (f.kind === 'select') {
      el = h('select', { class: 'field', id, onchange: onChoice }, f.options.map(([value, text]) => h('option', { value, text, selected: value === f.value })));
      read = () => ({ value: el.value });
    } else if (f.kind === 'value') {
      el = h('textarea', { class: 'field mono', id, rows: '4', autocomplete: 'off', spellcheck: false, autocapitalize: 'off', value: f.value ?? '' });
      const box = h('input', {
        type: 'checkbox', id: `${id}-hex`, checked: !!f.hex,
        onchange: () => {
          if (box.checked) {
            el.value = hexOfText(el.value);
            err.textContent = '';
            return;
          }
          const back = textOfHex(el.value);
          if (back === null) {
            box.checked = true;
            err.textContent = cleanHex(el.value) === null
              ? 'Hex digits come in pairs, 0 to 9 and a to f, two for each byte.'
              : 'These bytes are not text that can be typed here as it is, so they stay as hex.';
          } else {
            el.value = back;
            err.textContent = '';
          }
        },
      });
      hexBox = h('div', { class: 'check compact' }, box, h('label', { htmlFor: box.id, text: 'As hex, two digits a byte' }));
      read = () => valueFrom(box.checked, el.value);
    } else {
      el = h('input', { class: 'field mono', type: 'text', id, autocomplete: 'off', spellcheck: false, autocapitalize: 'off', value: f.value ?? '', placeholder: f.placeholder || null });
      read = () => ({ value: el.value });
    }
    el.setAttribute('aria-describedby', [hint ? hint.id : null, err.id].filter(Boolean).join(' '));
    const row = h('div', { class: 'form-row' }, label, el, hexBox, hint, err);
    return {
      f, el, row,
      setLabel(choices) {
        label.textContent = typeof f.label === 'function' ? f.label(choices) : f.label;
      },
      read() {
        const got = read();
        if (!got.error && f.required && got.value === '') return { error: f.required };
        return got;
      },
      fail(text) {
        err.textContent = text || '';
        el.setAttribute('aria-invalid', text ? 'true' : 'false');
      },
    };
  }

  /** A small form in a dialog for one write. `send(values)` makes the request: its reply
   *  closes the form and goes to `done`; a refusal stays in the form, in the server's words.
   *  A question the server asks opens over it. Cancelled, or when the value was read again
   *  meanwhile, the form closes and says so. `more`: other buttons, `{ label, send }`, which
   *  send without reading the boxes. */
  function writeDialog({ title, sub = null, fields, notes = [], yes = 'Save', send, done, more = [], value = true }) {
    formSeq += 1;
    const seq = formSeq;
    openDialog({
      title,
      build(close) {
        const error = h('p', { class: 'form-error', role: 'alert' });
        const rows = [];
        const choices = () => {
          const out = {};
          for (const r of rows) if (r.f.kind === 'select') out[r.f.name] = r.el.value;
          return out;
        };
        const sync = () => {
          const c = choices();
          for (const r of rows) {
            r.row.hidden = r.f.show ? !r.f.show(c) : false;
            r.setLabel(c);
          }
        };
        fields.forEach((f, i) => rows.push(fieldRow(f, `wf-${seq}-${i}`, sync)));
        sync();
        const first = rows.find((r) => r.f.kind !== 'select');
        if (first) first.el.setAttribute('data-autofocus', '');
        const buttons = [];
        // `check`: read and check the boxes first (an extra button such as "Remove the TTL"
        // sends without them).
        const go = async (sender, check = true) => {
          const values = {};
          for (const r of check ? rows : []) {
            if (r.row.hidden) continue;
            const got = r.read();
            r.fail(got.error);
            if (got.error) {
              r.el.focus();
              return;
            }
            values[r.f.name] = got.value;
          }
          buttons.forEach((b) => { b.disabled = true; });
          error.textContent = '';
          try {
            const result = await sender(values);
            close(true);
            if (alive) done(result);
          } catch (err) {
            buttons.forEach((b) => { b.disabled = false; });
            if (err.cancelled || (value && stale(err))) {
              close(null);
              writeFailed(err, value);
              return;
            }
            error.textContent = errorText(err);
            learnRefusal(err.message);
          }
        };
        const submit = h('button', { type: 'submit', class: 'btn primary', text: yes });
        const extra = (m) => h('button', {
          type: 'button', class: 'btn', text: m.label,
          onclick: () => {
            for (const r of rows) r.fail(null);
            go(m.send, false);
          },
        });
        buttons.push(submit, ...more.map(extra));
        return h('form', {
          class: 'form', autocomplete: 'off',
          onsubmit: (e) => {
            e.preventDefault();
            go(send);
          },
        },
        sub ? h('p', { class: 'mono wrap-any muted', text: sub }) : null,
        rows.map((r) => r.row),
        notes.filter(Boolean).map((t) => h('p', { class: 'hint', text: t })),
        error,
        h('div', { class: 'dialog-actions' },
          h('button', { type: 'button', class: 'btn', text: 'Cancel', onclick: () => close(null) }),
          buttons.slice(1),
          submit));
      },
    });
  }

  /** The server refused a command for this user: it is off for the rest of the session,
   *  and `state.refused` lists it, but no `state` event says so; read the state, so the
   *  buttons that send it turn off. */
  function learnRefusal(text) {
    if (/the server refused \S+ for this user/.test(text || '')) ctx.api.getState().catch(() => {});
  }

  const STALE = /^The value was read again since the page showed it/;
  /** The value the request named is not the one on the page any more: read again (404:
   *  closed meanwhile; 409: read again since). */
  const stale = (err) => err.status === 404 || (err.status === 409 && STALE.test(err.message || ''));

  /** Say why a write was not sent; when the value moved on meanwhile, read it again. */
  function writeFailed(err, value = true) {
    ctx.notify(errorText(err), err.cancelled ? 'info' : 'error');
    learnRefusal(err.message);
    if (value && stale(err) && store.valueId) {
      const v = store.value;
      openValue({ id: store.valueId, name: v && v.key ? v.key.name : '', type: v && v.meta ? v.meta.type : null });
    }
  }

  /** A write's reply: say what happened, then show the value and the key's row as read
   *  again (or drop them, once the key is deleted). */
  function applyWrite(r, keyId) {
    if (!r) return;
    ctx.notify(r.message || (r.done ? 'Done.' : 'Nothing changed.'), r.done ? 'ok' : 'info');
    if (store.list) store.list = r.closed ? dropRow(store.list, keyId) : replaceRow(store.list, keyId, r.row);
    if (store.valueId === keyId) {
      if (r.closed) {
        valueSeq += 1;
        Object.assign(store, { value: null, valueId: null, valueError: null, item: null, shown: null, format: null });
      } else if (r.value) takeValue(r.value);
    }
    drawKeys();
    drawValue();
  }

  /** Show a value read again after a write, keeping the item picked (the next one, after a
   *  delete) and the format. */
  function takeValue(v) {
    valueSeq += 1;
    const keep = store.item;
    store.value = v;
    store.valueId = v.key ? v.key.id : store.valueId;
    store.valueError = null;
    store.shown = v.value;
    const item = itemAt(v, keep ? keep.index : 0);
    if (item) pickItem(item, keep && keep.field !== null ? keep.field : 0, false);
    else {
      store.item = null;
      if (store.format) loadShown(v);
    }
  }

  /** Edit the string, the document or the selected item. Its whole value is read first,
   *  as hex (every byte), and shown as text when it can be typed as it is. */
  async function startEdit(button, item) {
    const v = store.value;
    if (!v || !v.key) return;
    const e = EDITS[v.kind];
    const key = v.key;
    const label = item ? itemLabel(v, item) : null;
    let start;
    if (v.kind === 'zset') start = { hex: false, text: item ? item.value ?? '' : '' };
    else {
      button.disabled = true;
      try {
        const it = await ctx.api.getItem({ view: v.view, index: item ? item.index : 0, format: 'hex' });
        if (it.shown.compression) {
          ctx.notify(`This value is stored compressed (${it.shown.compression}) and shown decompressed, so it is not edited here: saving it would store it uncompressed.`, 'info');
          return;
        }
        if (it.shown.truncated || it.shown.copy === null) {
          ctx.notify('This value is too big to edit here whole.', 'info');
          return;
        }
        start = editStart(it.shown.copy);
      } catch (err) {
        writeFailed(err);
        return;
      } finally {
        if (button.isConnected) button.disabled = false;
      }
      if (!alive || store.value !== v) return;
    }
    const notes = [];
    if (v.kind === 'string' || v.kind === 'list') notes.push('Sent only while it is still the one loaded: if it changed meanwhile, nothing is written and it is read again.');
    if (v.kind === 'string') notes.push('The key keeps its TTL.');
    if (v.kind === 'set') notes.push('The member is removed and the new one added.');
    if (start.hex) notes.push('It holds bytes that are not text you can type here, so it is edited as hex.');
    const field = v.kind === 'zset'
      ? { name: 'value', label: 'Score', kind: 'text', value: start.text, hint: 'A number, such as 1.5, or inf or -inf.', required: 'Type the score.' }
      : { name: 'value', label: e.edit.what === 'value' ? 'Value' : e.edit.what === 'document' ? 'Document' : e.edit.what === 'element' ? 'Element' : 'Member', kind: 'value', value: start.text, hex: start.hex };
    writeDialog({
      title: item ? (v.kind === 'zset' ? `Edit the score of ${label}` : `Edit ${label}`) : e.edit.label,
      sub: key.name,
      fields: [field],
      notes,
      send: (values) => ctx.api.editValue({ view: v.view, rev: v.rev, index: item ? item.index : 0, value: values.value }),
      done: (r) => applyWrite(r, key.id),
    });
  }

  /** Add a field, element, member or entry. */
  function startAdd() {
    const v = store.value;
    if (!v || !v.key) return;
    const e = EDITS[v.kind];
    const key = v.key;
    const name = (label) => ({ name: 'field', label, kind: 'text', required: `Type the ${label.toLowerCase()}'s name.` });
    const fields = {
      hash: [name('Field'), { name: 'value', label: 'Value', kind: 'value' }],
      list: [{ name: 'value', label: 'Element', kind: 'value' }],
      set: [{ name: 'value', label: 'Member', kind: 'value' }],
      zset: [{ name: 'value', label: 'Member', kind: 'value' }, { name: 'score', label: 'Score', kind: 'text', hint: 'A number, such as 1.5, or inf or -inf.', required: 'Type the score.' }],
      stream: [name('Field'), { name: 'value', label: 'Value', kind: 'value' }],
    }[v.kind];
    writeDialog({
      title: e.add.label,
      sub: key.name,
      fields,
      notes: [e.add.note],
      yes: 'Add',
      send: (values) => ctx.api.addItem({ view: v.view, field: values.field, value: values.value, score: values.score }),
      done: (r) => applyWrite(r, key.id),
    });
  }

  /** Set or remove the key's TTL, or a hash field's. */
  function ttlDialog(item) {
    const v = store.value;
    if (!v || !v.key) return;
    const key = v.key;
    const ttl = item ? item.ttl : v.meta && v.meta.ttl;
    const now = expiryText(ttl);
    const has = !!ttl && ttl !== 'never';
    const send = (text) => ctx.api.setTtl(item ? { view: v.view, ttl: text, field: item.index, rev: v.rev } : { view: v.view, ttl: text });
    writeDialog({
      title: item ? `Set the TTL of ${itemLabel(v, item)}` : 'Set the expiry',
      sub: key.name,
      fields: [{
        name: 'ttl', label: 'Expires in', kind: 'text', placeholder: '24h', required: 'Type how long until it expires, such as 24h.',
        hint: 'Seconds, or a number with s, m, h, d or w, several in a row: 90s, 10m, 1h30m, 7d.',
      }],
      notes: [
        now ? `Now it ${now}.` : null,
        item ? 'Only this field expires; the key keeps its own TTL.' : 'A TTL of 0 would delete the key: delete the key instead.',
      ],
      yes: 'Set the TTL',
      more: has ? [{ label: 'Remove the TTL', send: () => send('-') }] : [],
      send: (values) => send(values.ttl),
      done: (r) => applyWrite(r, key.id),
    });
  }

  function renameDialog() {
    const v = store.value;
    if (!v || !v.key) return;
    const key = v.key;
    writeDialog({
      title: 'Rename the key',
      sub: key.name,
      fields: [{ name: 'name', label: 'New name', kind: 'text', value: key.copy ?? key.name, required: 'Type the new name.' }],
      notes: [
        'It is sent with RENAMENX: a key that exists already is never overwritten.',
        v.cluster ? 'On a cluster both names must be in the same slot: keep the part in { } the same.' : null,
      ],
      yes: 'Rename',
      send: (values) => ctx.api.renameKey({ view: v.view, name: values.name }),
      done: (r) => applyWrite(r, key.id),
    });
  }

  /** Delete the key, or the selected item: redis-lens asks first, as the TUI does. */
  async function sendDelete(button, request, keyId) {
    button.disabled = true;
    try {
      applyWrite(await request(), keyId);
    } catch (err) {
      writeFailed(err);
    } finally {
      if (button.isConnected) button.disabled = false;
    }
  }

  function deleteKey(button) {
    const v = store.value;
    if (!v || !v.key) return;
    sendDelete(button, () => ctx.api.deleteKey({ view: v.view }), v.key.id);
  }

  function removeItem(button, item) {
    const v = store.value;
    if (!v || !v.key) return;
    sendDelete(button, () => ctx.api.removeItem({ view: v.view, rev: v.rev, index: item.index }), v.key.id);
  }

  /** Add a key with its first value: from the key list of a pattern its name starts with
   *  the pattern's text before its first placeholder, and it is listed there once added. */
  function addKeyDialog() {
    const list = store.list;
    const pattern = store.finding === null && store.picked ? store.picked : null;
    const is = (...types) => (c) => types.includes(c.type);
    writeDialog({
      title: 'Add a key',
      fields: [
        { name: 'name', label: 'Name', kind: 'text', value: pattern ? newKeyName(pattern.copy ?? pattern.name) : '', required: 'Type the key\'s name.' },
        { name: 'type', label: 'Type', kind: 'select', value: 'string', options: NEW_KEY_TYPES },
        { name: 'field', label: 'Field', kind: 'text', show: is('hash', 'stream'), required: 'Type the field\'s name.' },
        { name: 'value', label: (c) => ({ list: 'First element', set: 'First member', zset: 'First member' }[c.type] || 'Value'), kind: 'value' },
        { name: 'score', label: 'Score', kind: 'text', show: is('zset'), hint: 'A number, such as 1.5, or inf or -inf.', required: 'Type the score.' },
      ],
      notes: ['A key that exists already is never overwritten.', list ? 'Once added it is listed with the keys shown, and opened.' : 'Once added it is opened.'],
      yes: 'Add the key',
      value: false,
      send: (values) => ctx.api.addKey({ list: list ? list.list : undefined, ...values }),
      done: (r) => {
        ctx.notify(r.message || (r.done ? 'Added.' : 'Not added.'), r.done ? 'ok' : 'info');
        if (!r.done || !r.value) return;
        if (store.list && list && store.list.list === list.list && r.row) store.list = addRow(store.list, r.row);
        store.item = null;
        store.format = null;
        store.itemsShown = CHUNK;
        takeValue(r.value);
        drawKeys();
        drawValue();
      },
    });
  }

  // ---------- bulk actions over a pattern's keys ----------

  const bulkBusy = () => !!store.bulk && (store.bulk.stage === 'counting' || store.bulk.stage === 'running');

  function bulkButtons(pattern) {
    const c = conn();
    const busy = bulkBusy() ? 'A bulk action is running already: wait for it to end, or stop it first.' : null;
    const expire = writeOff(c, KEY_CMDS.bulkExpire) || busy;
    const remove = writeOff(c, KEY_CMDS.bulkDelete) || busy;
    return h('div', { class: 'stack-sm' },
      h('p', { class: 'lbl', text: 'Every key of the pattern' }),
      h('div', { class: 'row-actions' },
        writeButton('Set a TTL on every key…', expire, () => bulkExpireDialog(pattern), { act: 'pattern.bulkTtl' }),
        writeButton('Delete every key…', remove, (b) => startBulk(b, pattern), { danger: true, act: 'pattern.bulkDelete' })),
      c && c.readOnly ? whyLines([expire, remove]) : h('p', { class: 'muted small', text: [expire || remove, 'A dry run counts the keys first: nothing is written until you confirm.'].filter(Boolean).join(' ') }));
  }

  function bulkExpireDialog(pattern) {
    writeDialog({
      title: 'Set a TTL on every key of the pattern',
      sub: pattern.name,
      fields: [{ name: 'ttl', label: 'TTL', kind: 'text', value: '24h', required: 'Type the TTL, such as 24h.', hint: 'Seconds, or a number with s, m, h or d: 90s, 10m, 24h, 7d.' }],
      notes: [
        'Only keys without a TTL get it; the others keep theirs.',
        'First a dry run counts the keys. Nothing is written until you confirm.',
        'It sends EXPIRE … NX, which needs Redis 7.0 or later, or Valkey.',
      ],
      yes: 'Count the keys',
      value: false,
      send: (values) => ctx.api.startBulk({ pattern: pattern.id, action: 'expire', ttl: values.ttl }),
      done: (r) => takeBulk(r.bulk, true),
    });
  }

  async function startBulk(button, pattern) {
    button.disabled = true;
    try {
      takeBulk((await ctx.api.startBulk({ pattern: pattern.id, action: 'delete' })).bulk, true);
    } catch (err) {
      ctx.notify(errorText(err), 'error');
      learnRefusal(err.message);
    } finally {
      if (button.isConnected) button.disabled = false;
    }
  }

  async function bulkCall(button, call, quiet = false) {
    if (button) button.disabled = true;
    try {
      const r = await call();
      const ends = r.bulk && r.bulk.stage === 'done' && r.bulk.lines.length && store.bulk && store.bulk.id === r.bulk.id && store.bulk.stage !== 'done';
      if (r.text && !quiet && !ends) ctx.notify(r.text, 'info');
      takeBulk(r.bulk);
    } catch (err) {
      ctx.notify(errorText(err), err.cancelled ? 'info' : 'error');
      learnRefusal(err.message);
      refreshBulk();
    }
  }

  async function refreshBulk() {
    try {
      const r = await ctx.api.getBulk();
      if (alive) takeBulk(r.bulk);
    } catch {
      // Not connected, or no live connection yet: nothing to show.
    }
  }

  /** A bulk action as it now is (a reply, or the `bulk` event). */
  function takeBulk(b, focus = false) {
    const was = store.bulk;
    const busy = bulkBusy();
    store.bulk = b;
    if (b && b.stage === 'done' && was && was.id === b.id && was.stage !== 'done' && b.lines.length) {
      ctx.notify(b.lines[0], b.error ? 'error' : 'ok');
      if (b.error) learnRefusal(b.lines[0]);
    }
    drawBulk();
    // The pattern's bulk buttons are off while one runs.
    if (busy !== bulkBusy()) {
      drawKeys();
      drawValue();
    }
    if (focus && b) {
      const head = bulkBox.querySelector('h2');
      if (head) head.focus();
    }
  }

  function drawBulk() {
    const b = store.bulk;
    if (!b) {
      bulkBox.hidden = true;
      fill(bulkBox);
      return;
    }
    bulkBox.hidden = false;
    // Each event draws the card anew: whoever was in it (on Stop, say) stays on the same
    // button, or on its title when that button is gone.
    const was = bulkBox.contains(document.activeElement) ? document.activeElement : null;
    const wasText = was && was.tagName === 'BUTTON' ? was.textContent : null;
    const stage = { counting: 'Dry run', counted: 'Counted', running: 'Running', done: b.error ? 'Stopped' : 'Ended' }[b.stage];
    const progress = bulkProgress(b);
    const buttons = [];
    if (b.stage === 'counting') buttons.push(h('button', { type: 'button', class: 'btn', text: 'Stop the dry run', onclick: (e) => bulkCall(e.currentTarget, () => ctx.api.stopBulk()) }));
    if (b.stage === 'counted') {
      const off = writeOff(conn(), b.action === 'expire' ? KEY_CMDS.bulkExpire : KEY_CMDS.bulkDelete);
      buttons.push(writeButton(bulkRunLabel(b), off, (button) => bulkCall(button, () => ctx.api.runBulk(b.id)), { primary: true }));
      buttons.push(h('button', { type: 'button', class: 'btn', text: 'Cancel', onclick: (e) => bulkCall(e.currentTarget, () => ctx.api.stopBulk()) }));
    }
    if (b.stage === 'running') {
      const stopping = !!(b.run && b.run.stopping);
      buttons.push(h('button', { type: 'button', class: 'btn', text: stopping ? 'Stopping after this batch…' : 'Stop', disabled: stopping, onclick: (e) => bulkCall(e.currentTarget, () => ctx.api.stopBulk()) }));
    }
    if (b.stage === 'done') {
      if (store.list && store.list.kind === 'pattern' && store.picked && store.picked.id === b.pattern.id && b.run && b.run.changed) {
        buttons.push(h('button', { type: 'button', class: 'btn', text: 'Read the keys again', onclick: () => openPattern(store.picked) }));
      }
      buttons.push(h('button', { type: 'button', class: 'btn', text: 'Close', onclick: (e) => bulkCall(e.currentTarget, () => ctx.api.stopBulk(), true) }));
    }
    const cluster = b.nodes.length
      ? h('p', { class: 'muted small', text: `${plural(b.nodes.length, 'primary', 'primaries')}, one at a time: ${b.nodes.map((n, i) => `${n.endpoint} ${fmtGrouped(n.matched)}${b.at === i && b.stage !== 'done' ? ' (now)' : ''}`).join(' · ')}.` })
      : null;
    let bar = null;
    if (progress && progress.pct !== null && b.stage !== 'counted') {
      bar = meter(progress.pct, b.error ? 'warn' : '');
      bar.setAttribute('aria-hidden', 'true');
    }
    refill(bulkBox, [
      h('div', { class: 'card-head' },
        h('h2', { class: 'bulk-title wrap-any', tabIndex: -1 }, `${b.title} `, patternName(b.pattern.name, { literal: literalOf(b.pattern) }), b.action === 'expire' && b.ttl ? ` · TTL ${b.ttl}` : ''),
        h('span', { class: `badge${b.prod ? ' env-prod' : ''}`, text: `${stage} · ${b.profile || ''}`.trim() })),
      bar,
      progress ? h('p', { role: 'status', text: progress.text }) : null,
      b.summary.length ? h('ul', { class: 'bulk-lines' }, b.summary.map((t) => h('li', { text: t }))) : null,
      cluster,
      b.stage === 'done' && b.lines.length ? h('div', { class: 'stack-sm' }, b.lines.map((t, i) => (b.error && i === 0 ? notice(t) : h('p', { class: 'wrap-any', text: t })))) : null,
      b.stage === 'counted' ? h('p', { class: 'muted small', text: b.prod ? 'Running it asks you to type the pattern first.' : 'Running it asks first.' }) : null,
      h('div', { class: 'row-actions' }, buttons),
    ]);
    if (was) {
      const same = wasText && Array.from(bulkBox.querySelectorAll('button')).find((x) => x.textContent === wasText && !x.disabled);
      (same || bulkBox.querySelector('h2')).focus();
    }
  }

  // ---------- what other areas asked for ----------
  function takePending() {
    const p = store.pending;
    store.pending = null;
    if (!p) return;
    if (p.find) runFind(p.find);
    else if (p.key) {
      openValue(p.key);
      if (!store.list) drawKeys();
    } else if (p.pattern || typeof p.node === 'number') {
      if (typeof p.node === 'number' && p.node !== store.node) {
        store.node = p.node;
        loadTree();
      }
      if (p.pattern) openPattern(p.pattern);
    }
  }

  // ---------- following the session ----------
  // The writes the server refused this session turn their buttons off.
  const refusedKey = (s) => (s.conn ? s.conn.refused.join(',') : '');
  let refusedNow = refusedKey(ctx.state());
  function onState(next) {
    if (!alive) return;
    const what = follow(store, next);
    if (what) {
      drawKeys();
      drawValue();
      loadTree();
      loadDbs();
      if (what === 'reset') {
        drawBulk();
        refreshBulk();
      }
    } else if (store.tree) drawPatFoot(next.analysis.report);
    if (refusedKey(next) !== refusedNow) {
      refusedNow = refusedKey(next);
      drawKeys();
      drawValue();
      drawBulk();
    }
  }
  let lastProgress = 0;
  function onProgress(p) {
    if (!alive) return;
    // Partial results replace the tree as they come, at most once a second.
    if (p.kind === 'sampling' && Date.now() - lastProgress > 1000) {
      lastProgress = Date.now();
      loadTree();
    } else if (p.kind === 'done' || p.kind === 'compared' || p.kind === 'failed' || p.kind === 'stopped') loadTree();
  }
  const offs = [ctx.api.on('state', onState), ctx.api.on('progress', onProgress), ctx.api.on('bulk', (r) => {
    if (alive) takeBulk(r.bulk);
  })];

  active = { takePending };
  // Draw what is kept at once, then read what may have changed meanwhile.
  if (store.tree) drawTree();
  else drawRows();
  drawKeys();
  drawValue();
  drawBulk();
  loadTree();
  loadDbs();
  refreshBulk();
  if (store.pending) takePending();
  else if (store.list && store.list.kind === 'pattern' && !store.list.done && store.picked) {
    // Nothing to do: the list is as it was; more pages load on request.
  }

  return () => {
    alive = false;
    if (active && active.takePending === takePending) active = null;
    timers.forEach((t) => clearTimeout(t));
    offs.forEach((off) => off());
  };
}

