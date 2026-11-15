// Browse: the key tree (from key names before an analysis, from the analysis after one), a
// pattern's keys, Find a key, and a key's value in each of its renderings.
//
// Everything a key holds is drawn with textContent, as redis-lens escaped it. A key is
// named in a request by the hex id redis-lens gave for it, never by the text shown. Values
// stay in this page's memory while it is open: nothing goes to storage, the console or an
// address. Copying is the browser's clipboard, at a click. Edits arrive later.

import {
  h, fill, refill, DASH, fmtBytes, fmtCount, fmtGrouped, fmtPercent, plural, barPercent, meter,
  notice, emptyState, connectFirst, analyzing, errorText, copyText, expiryText, fmtSignedBytes,
} from '../ui.js';

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
  if (typeof length === 'number') parts.push(m.lengthUnit === 'bytes' || v.kind === 'string' ? plural(length, 'byte', 'bytes') : `${fmtGrouped(length)} ${m.lengthUnit || 'items'}`);
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
    dbs: null, dbsError: null, pending: null,
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

// ---------------------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------------------

export function mount(root, ctx) {
  const state = ctx.state();
  const gate = connectFirst(state, { needsLive: true });
  const view = h('section', { class: 'view', 'aria-labelledby': 'browse-title' });
  root.append(view);
  if (gate) {
    view.append(h('h1', { class: 'title', id: 'browse-title', text: 'Browse' }), gate);
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

  // ---------- the head: title, database, node ----------
  const subtitle = h('p', { class: 'subtitle' });
  const dbBox = h('div', { class: 'picker' });
  const nodeBox = h('div', { class: 'picker' });
  view.append(h('div', { class: 'view-head' },
    h('div', null, h('h1', { class: 'title', id: 'browse-title', text: 'Browse' }), subtitle),
    h('div', { class: 'actions' }, nodeBox, dbBox)));

  // ---------- the three cards ----------
  const patFilter = h('input', {
    class: 'field mono', type: 'search', id: 'browse-pat-filter', placeholder: 'Filter patterns', autocomplete: 'off', spellcheck: false, value: store.filter,
    oninput: () => {
      store.filter = patFilter.value;
      drawRows();
    },
  });
  const sortBox = h('div', { class: 'picker' });
  const patSource = h('p', { class: 'muted small' });
  const patRows = h('ul', { class: 'list tree', 'aria-label': 'Key patterns' });
  const patFoot = h('div', { class: 'card-foot' });
  const patCard = h('div', { class: 'card stack browse-col pats' },
    h('div', { class: 'card-head' }, h('h2', { class: 'lbl', text: 'Key patterns' }), sortBox),
    patSource,
    h('div', null, h('label', { class: 'sr-only', htmlFor: 'browse-pat-filter', text: 'Filter the patterns' }), patFilter),
    patRows, patFoot);

  const keysCard = h('div', { class: 'card stack browse-col keys' });
  const valueCard = h('div', { class: 'card stack browse-col value', 'aria-live': 'polite', 'aria-busy': 'false' });
  view.append(h('div', { class: 'browse' }, patCard, keysCard, valueCard));

  // ---------- the database picker ----------
  async function loadDbs() {
    try {
      store.dbs = await ctx.api.getDbs();
      store.dbsError = null;
    } catch (err) {
      store.dbs = null;
      store.dbsError = errorText(err);
    }
    if (alive) drawDbs();
  }

  function drawDbs() {
    const d = store.dbs;
    if (!d) {
      fill(dbBox, store.dbsError ? h('span', { class: 'muted small', text: `Database: ${store.dbsError}` }) : null);
      return;
    }
    if (d.off) {
      fill(dbBox, h('span', { class: 'muted small', text: d.off }));
      return;
    }
    const select = h('select', {
      class: 'field', id: 'browse-db',
      onchange: async () => {
        const db = Number(select.value);
        select.disabled = true;
        try {
          const r = await ctx.api.switchDb(db);
          ctx.notify(r.switched ? r.text || `Switched to db ${db}.` : `Already on db ${db}.`, 'ok');
        } catch (err) {
          ctx.notify(errorText(err), 'error');
          select.value = String(d.current);
        } finally {
          select.disabled = false;
        }
      },
    }, d.rows.map((r) => h('option', { value: String(r.db), selected: r.current, text: `db ${r.db} · ${plural(r.keys, 'key', 'keys')}` })));
    fill(dbBox,
      h('label', { htmlFor: 'browse-db', text: 'Database' }),
      select,
      h('span', { class: 'sr-only', text: [d.note, d.unread].filter(Boolean).join(' ') }));
    select.title = [d.note, d.unread].filter(Boolean).join(' ');
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
        class: 'field', id: 'browse-sort',
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
      ? h('button', { type: 'button', class: 'btn', text: 'Read 1,000 more names', onclick: (e) => moreNames(e.currentTarget) })
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
          type: 'button', class: 'row row-pat group', 'aria-expanded': closed ? 'false' : 'true',
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
      type: 'button', class: `row row-pat${child ? ' child' : ''}${on ? ' on' : ''}`, 'aria-current': on ? 'true' : null,
      onclick: () => openPattern(r.pattern, r),
    },
    h('span', { class: 'name' },
      h('span', { class: 'mono clip block', text: r.pattern.name }),
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
    const head = h('div', { class: 'card-head' },
      h('h2', { class: 'mono key-title clip', text: title, title }),
      store.finding !== null ? h('button', { type: 'button', class: 'btn quiet', text: 'Clear search', onclick: clearFind }) : null);
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
      class: 'field mono', type: 'search', id: 'browse-key-filter', placeholder: 'Filter the loaded keys', autocomplete: 'off', spellcheck: false, value: store.listFilter,
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
      ? h('button', { type: 'button', class: 'btn', text: `Load ${fmtGrouped(l.pageKeys)} more`, onclick: (e) => moreKeys(e.currentTarget) })
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
      return h('li', null, h('button', {
        type: 'button', class: `row row-key${on ? ' on' : ''}`, 'aria-current': on ? 'true' : null,
        onclick: () => openValue(r),
      },
      h('span', { class: 'mono clip', text: r.name, title: r.name }),
      h('span', { class: 'badge mono', text: r.type || DASH }),
      h('span', { class: 'mono right', text: fmtBytes(r.bytes) }),
      h('span', { class: 'mono right muted', text: r.ttl || DASH }),
      typeof r.node === 'number' && nodes[r.node] ? h('span', { class: 'sr-only', text: `on node ${nodes[r.node].address}` }) : null));
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
      ? h('div', { class: 'command' }, h('code', { class: 'mono wrap-any', text: `$ ${r.command}` }), copyButton('Copy command', r.command, 'the command'))
      : h('p', { class: 'muted small', text: 'No command to list them: the pattern has characters that are not safe to paste in every shell.' });
    details.push(command);
    return h('div', { class: 'summary' },
      h('p', { text: parts.join(' · ') }),
      r.marked && r.reason ? notice(r.reason) : r.reason ? h('p', { class: 'muted small', text: r.reason }) : null,
      h('div', { class: 'row-actions' }, copyButton('Copy pattern', r.pattern.copy, 'the pattern', 'The pattern has control characters, so it is not copied: a pasted copy would name other keys.')),
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
    if (store.list) {
      const box = keysCard.querySelector('ul.list[aria-label="Keys"]');
      if (box) drawKeyRows(box, box.nextElementSibling);
    }
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
      v.pattern ? h('p', { class: 'muted small' }, 'Pattern ', h('span', { class: 'mono', text: v.pattern.name })) : null,
      h('div', { class: 'row-actions' },
        copyButton('Copy name', copyName, 'the key name', 'The name has control characters, so it is not copied: a pasted copy would name another key.'),
        shown ? copyButton('Copy value', shown.copy, 'the value', shown.copyRefused || 'This value is not copied as text: show it as hex to copy it.') : null),
      h('p', { class: 'note muted small', text: s.conn && s.conn.readOnly
        ? 'Read-only connection. Editing, expiry, rename and delete arrive later in the dashboard.'
        : 'Editing, expiry, rename and delete arrive later in the dashboard: here a key is only read.' }),
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
        v.more ? h('button', { type: 'button', class: 'btn', text: 'Load more', onclick: (e) => moreValue(e.currentTarget) }) : null,
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
        type: 'button', class: `row row-item cols-${cols.length}${on ? ' on' : ''}`, 'aria-current': on ? 'true' : null,
        onclick: () => pickItem(item, 0),
      }, cells.map((c, i) => h('span', { class: `mono clip${i ? ' muted' : ''}`, text: c, title: c }))));
    }));
    const fields = v.kind === 'stream' && store.item ? (v.items.find((i) => i.index === store.item.index) || {}).fields || [] : [];
    return h('div', { class: 'items-box' },
      h('div', { class: `cols-head row-item cols-${cols.length}`, 'aria-hidden': 'true' }, cols.map((c) => h('span', { text: c }))),
      list,
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
        type: 'button', class: shown.format === f ? 'pill small on' : 'pill small', 'aria-pressed': shown.format === f ? 'true' : 'false', text: f === 'text' ? 'Text' : f === 'hex' ? 'Hex' : f,
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

  function copyButton(label, value, what, refused = null) {
    return h('button', {
      type: 'button', class: 'btn', text: label,
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
  function onState(next) {
    if (!alive) return;
    const what = follow(store, next);
    if (what) {
      drawKeys();
      drawValue();
      loadTree();
      loadDbs();
    } else if (store.tree) drawPatFoot(next.analysis.report);
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
  const offs = [ctx.api.on('state', onState), ctx.api.on('progress', onProgress)];

  active = { takePending };
  // Draw what is kept at once, then read what may have changed meanwhile.
  drawDbs();
  if (store.tree) drawTree();
  else drawRows();
  drawKeys();
  drawValue();
  loadTree();
  loadDbs();
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

