// Analyze: start an analysis (after its estimate, as the TUI asks), follow it, stop it,
// analyze again; then the panels Big keys, Expiry, Changes, Hot keys and, on a cluster,
// Nodes and Slots; and the snapshots: save the analysis, list them, compare two.
//
// An analysis reads keys' metadata (SCAN, TYPE, TTL, sizes, LFU counters) through the
// command budget, never a value. Everything from Redis is drawn as text. Without LFU
// counters, Hot keys ranks the keys the last Server › Monitor run named, as the TUI does.

import {
  h, fill, refill, DASH, fmtBytes, fmtCount, fmtGrouped, fmtPercent, fmtDuration, fmtClock, plural, barPercent, meter, setVar,
  table, pills, notice, emptyState, connectFirst, errorText, confirmDialog, copyText, fmtSignedBytes, fmtSignedCount, sparkline, expiryText, patternName,
} from '../ui.js';

// ---------------------------------------------------------------------------------------
// Pure helpers (tested in tests/browse.test.js)
// ---------------------------------------------------------------------------------------

export const PANELS = [
  { id: 'big_keys', label: 'Big keys' },
  { id: 'expiry', label: 'Expiry' },
  { id: 'changes', label: 'Changes' },
  { id: 'hot_keys', label: 'Hot keys' },
  { id: 'nodes', label: 'Nodes', cluster: true },
  { id: 'slots', label: 'Slots', cluster: true },
  { id: 'snapshots', label: 'Snapshots' },
];

/** The panels shown: Nodes and Slots only on a cluster. */
/** The Monitor ranking's line: `Or rank keys by how often Monitor sees them: Server ›
 *  Monitor.` split around its link. */
export function monitorLinkParts(text) {
  const t = String(text ?? '');
  const at = t.lastIndexOf('Server › Monitor');
  if (at < 0) return { before: t, link: null, after: '' };
  return { before: t.slice(0, at), link: 'Server › Monitor', after: t.slice(at + 'Server › Monitor'.length) };
}

export function panelsFor(cluster) {
  return PANELS.filter((p) => !p.cluster || cluster);
}

export const DEPTHS = { quick: 'Analyze 10,000 keys', deep: 'Analyze 100,000 keys' };

/** What an analysis would cost, in the TUI's terms. */
export function estimateText(e) {
  if (!e) return 'redis-lens samples keys with SCAN and reads their type, TTL and size, never a value.';
  const depth = e.depth === 'deep' ? 'A deep analysis' : 'A quick analysis';
  const parts = [`${depth} samples ${typeof e.keys === 'number' ? `${fmtGrouped(e.keys)} keys` : 'keys'} (at most ${fmtGrouped(e.sampleLimit)})`];
  if (e.shards) parts.push(` across ${plural(e.shards, 'shard', 'shards')}`);
  parts.push(` with about ${plural(e.commands, 'command', 'commands')}`);
  if (typeof e.durationSecs === 'number') parts.push(`, about ${fmtDuration(e.durationSecs)}`);
  if (e.about) parts.push(`, ${e.about}`);
  parts.push('.');
  const lines = [parts.join('')];
  lines.push('It reads each key\'s type, TTL and size through the command budget, never a value.');
  if (!e.sizes) lines.push('Sizes are off: MEMORY USAGE is not allowed, so there are no memory figures.');
  if (!e.freq) lines.push('Hot keys stays empty: the server keeps no LFU counters for this policy.');
  return lines.join(' ');
}

/** Why an analysis failed. The server's sentence already starts "The analysis failed:"
 *  for its own fixed reasons; anything else gets that start here. `last` words it as the
 *  last analysis, for the line that stays after the run. */
export function failedText(error, last = false) {
  const e = String(error || '').trim();
  const lead = last ? 'The last analysis failed' : 'The analysis failed';
  const m = /^the analysis failed:\s*/i.exec(e);
  return m ? `${lead}: ${e.slice(m[0].length)}` : `${lead}: ${e}`;
}

/** The analysis in one line, for the head of Analyze. */
export function statusText(a) {
  if (!a) return 'No analysis yet.';
  if (a.status === 'connecting') return 'Connecting to analyze…';
  if (a.status === 'sampling') {
    if (typeof a.sampled === 'number' && typeof a.target === 'number') return `Analyzing: ${fmtGrouped(a.sampled)} of ${fmtGrouped(a.target)} keys sampled.`;
    return 'Analyzing…';
  }
  if (a.status === 'stopping') return 'Stopping: what it sampled so far is kept.';
  const r = a.report;
  if (!r) return a.error ? failedText(a.error, true) : a.note || 'No analysis yet.';
  const when = typeof r.takenAt === 'number' ? `${fmtClock(r.takenAt * 1000).slice(0, 5)} UTC` : DASH;
  const parts = [`Analyzed at ${when} · ${plural(r.sampledKeys, 'key', 'keys')} sampled`];
  if (typeof r.ageSecs === 'number') parts.push(`${fmtDuration(r.ageSecs)} ago`);
  if (r.stopped) parts.push('stopped early: partial, not saved');
  else if (r.complete) parts.push('every key looked at');
  return parts.join(' · ');
}

/** The share of a progress bar, 0 to 100. */
export function progressPercent(a) {
  if (!a || typeof a.sampled !== 'number' || typeof a.target !== 'number' || a.target <= 0) return 0;
  return Math.max(0, Math.min(100, Math.round((a.sampled / a.target) * 100)));
}

/** A change as a share of what was there before: `+12%`, `new`, `gone`. */
export function changePercent(row) {
  const c = row.change;
  if (!c) return DASH;
  if (c.cause === 'new') return 'new';
  if (c.cause === 'gone') return 'gone';
  const before = row.bytesBefore ?? row.keysBefore;
  const delta = typeof row.bytesBefore === 'number' ? c.deltaBytes : c.deltaKeys;
  if (typeof before !== 'number' || before === 0 || typeof delta !== 'number') return DASH;
  const pct = Math.round((delta / before) * 100);
  return `${pct > 0 ? '+' : pct < 0 ? '−' : ''}${Math.abs(pct)}%`;
}

/** The memory freed by TTL, as bars: within each horizon, then never. */
export function freedBars(freed) {
  if (!freed) return [];
  const rows = freed.within.map((w) => ({ label: `within ${w.label}`, bytes: w.bytes, never: false }));
  rows.push({ label: 'never', bytes: freed.never, never: true });
  const max = Math.max(1, ...rows.map((r) => (typeof r.bytes === 'number' ? r.bytes : 0)));
  return rows.map((r) => ({ ...r, pct: barPercent(r.bytes, max), share: typeof freed.total === 'number' && freed.total > 0 && typeof r.bytes === 'number' ? r.bytes / freed.total : null }));
}

// ---------------------------------------------------------------------------------------
// The memory, kept while other areas are shown
// ---------------------------------------------------------------------------------------

function blank() {
  return { who: null, info: null, panels: {}, snapshots: null, compare: { a: '', b: 'current' }, compared: null, slotsOrder: 'keys', pendingStart: null, lastText: null, picked: {} };
}
const store = blank();
const listeners = new Set();

function follow(state) {
  const c = state.conn;
  const who = c ? `${c.name}|${c.docker}|${state.analysis.db}` : null;
  if (who !== store.who) {
    const pendingStart = store.pendingStart;
    Object.assign(store, blank(), { who, pendingStart });
  }
}

/** Called once at boot: an analysis's events are followed whatever area is shown. */
export function init(api) {
  api.on('progress', (p) => {
    if (p.kind === 'done' || p.kind === 'started' || (p.kind === 'stopped' && p.kept)) store.panels = {};
    if (p.kind === 'compared') delete store.panels.changes;
    if (p.kind === 'stopped' && p.text) store.lastText = p.text;
    if (p.kind === 'failed' && p.error) store.lastText = failedText(p.error);
    for (const fn of Array.from(listeners)) fn(p);
  });
  api.on('state', follow);
}

/** Overview's buttons: Analyze asks with its estimate, then starts. */
export function requestStart(depth) {
  store.pendingStart = depth === 'deep' ? 'deep' : 'quick';
}

// ---------------------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------------------

export function mount(root, ctx, route) {
  const state = ctx.state();
  const gate = connectFirst(state);
  const view = h('section', { class: 'view', 'aria-labelledby': 'analyze-title' });
  root.append(view);
  if (gate) {
    view.append(h('h1', { class: 'title', id: 'analyze-title', text: 'Analyze' }), gate);
    store.pendingStart = null;
    return () => {};
  }
  follow(state);
  let alive = true;

  const isCluster = () => {
    const s = ctx.state();
    return !!(s.conn && (s.conn.topology === 'cluster' || s.conn.nodes.length > 1 || (s.analysis.report && s.analysis.report.cluster)));
  };
  const tabs = () => panelsFor(isCluster());
  const tab = tabs().some((t) => t.id === route.sub) ? route.sub : 'big_keys';

  // ---------- head ----------
  const status = h('p', { class: 'subtitle', role: 'status' });
  const actions = h('div', { class: 'actions' });
  const progress = h('div', { class: 'progress' });
  const lastLine = h('div');
  view.append(
    h('div', { class: 'view-head' }, h('div', null, h('h1', { class: 'title', id: 'analyze-title', text: 'Analyze' }), status), actions),
    progress,
    lastLine,
    pills(tabs().map((t) => ({ id: t.id, label: t.label, href: `#/analyze/${t.id}` })), tab, null, 'Analyze panels'));
  const body = h('div', { class: 'stack-lg' });
  view.append(body);

  function drawHead() {
    const a = ctx.state().analysis;
    status.textContent = statusText(a);
    const running = a.running;
    const start = (depth, primary) => h('button', {
      type: 'button', class: primary ? 'btn primary' : 'btn', text: DEPTHS[depth], disabled: running,
      onclick: () => ask(depth),
    });
    const r = a.report;
    refill(actions, [
      running ? h('button', { type: 'button', class: 'btn primary', text: a.status === 'stopping' ? 'Stopping…' : 'Stop', disabled: a.status === 'stopping', onclick: stop }) : null,
      r && !running && !r.stopped ? h('button', { type: 'button', class: 'btn', text: 'Save as snapshot', onclick: (e) => save(e.currentTarget) }) : null,
      start('quick', !r && !running),
      start('deep', false),
    ]);
    if (running) {
      const bar = meter(progressPercent(a));
      fill(progress, h('div', { class: 'card tight stack' },
        h('div', { class: 'chart-head' }, h('span', { class: 'lbl', text: a.depth === 'deep' ? 'Deep analysis' : 'Quick analysis' }), h('span', { class: 'mono', text: `${progressPercent(a)}%` })),
        bar,
        h('p', { class: 'muted small', text: [statusText(a), r && a.previous ? 'The panels show the previous results until this one is done.' : null].filter(Boolean).join(' ') })));
    } else fill(progress);
    const lines = [];
    // Without a report the line above already says it failed; with one, say so here.
    if (a.error && !running && r) lines.push(notice(`${failedText(a.error, true)} The panels show the earlier results.`));
    if (a.note && !running) lines.push(notice(a.note, 'info'));
    fill(lastLine, lines.length ? h('div', { class: 'notes' }, lines) : null);
  }

  async function ask(depth) {
    let info = store.info;
    try {
      info = await ctx.api.getAnalysis();
      store.info = info;
    } catch (err) {
      ctx.notify(errorText(err), 'error');
      return;
    }
    const e = info.estimates.find((x) => x.depth === depth) || null;
    const answer = await confirmDialog({ kind: 'ask', title: 'Analyze the keys?', text: estimateText(e), yes: 'Analyze', after: 'The panels show the results; the earlier ones stay until it has its own.' });
    if (!answer || !alive) return;
    try {
      await ctx.api.startAnalysis(depth);
    } catch (err) {
      ctx.notify(errorText(err), 'error');
    }
  }

  async function stop() {
    try {
      const r = await ctx.api.stopAnalysis();
      if (r.text) ctx.notify(r.text, 'info');
    } catch (err) {
      ctx.notify(errorText(err), 'error');
    }
  }

  async function save(button) {
    button.disabled = true;
    try {
      const r = await ctx.api.saveSnapshot();
      ctx.notify(r.text || `Saved snapshot ${r.file}.`, 'ok');
      store.snapshots = null;
      if (tab === 'snapshots' || tab === 'changes') drawBody();
    } catch (err) {
      ctx.notify(errorText(err), 'error');
    }
    if (alive) button.disabled = false;
  }

  // ---------- the panels ----------
  const LOADERS = {
    big_keys: () => ctx.api.getBigKeys(),
    expiry: () => ctx.api.getExpiry(),
    changes: () => ctx.api.getChanges(),
    hot_keys: () => ctx.api.getHotKeys(),
    nodes: () => ctx.api.getNodes(),
    slots: () => ctx.api.getSlots({ order: store.slotsOrder }),
  };

  let seq = 0;
  async function drawBody(refresh = false) {
    const a = ctx.state().analysis;
    if (tab === 'snapshots') {
      drawSnapshots();
      return;
    }
    if (!a.report) {
      drawNoReport(a);
      return;
    }
    const mine = (seq += 1);
    if (!store.panels[tab] || refresh) {
      if (!store.panels[tab]) fill(body, h('div', { class: 'card' }, h('p', { class: 'muted', text: 'Reading the panel…' })));
      try {
        const data = tab === 'slots' && refresh ? await ctx.api.getSlots({ order: store.slotsOrder, refresh: true }) : await LOADERS[tab]();
        store.panels[tab] = { data };
      } catch (err) {
        store.panels[tab] = { error: errorText(err) };
      }
      if (!alive || mine !== seq) return;
    }
    const p = store.panels[tab];
    if (p.error) {
      fill(body, h('div', { class: 'card' }, notice(p.error)));
      return;
    }
    const d = p.data;
    const prev = a.previous || (d.report && d.report.previous) ? notice('Previous results: a new analysis is running, and these are the last one\'s.', 'info') : null;
    if (d.off) {
      fill(body, h('div', { class: 'card stack' }, prev, notice(d.off)), tab === 'hot_keys' ? monitorBox : null);
      if (tab === 'hot_keys') loadMonitor();
      return;
    }
    const draw = { big_keys: drawBig, expiry: drawExpiry, changes: drawChanges, hot_keys: drawHot, nodes: drawNodes, slots: drawSlots }[tab];
    const focused = body.contains(document.activeElement) ? (document.activeElement.getAttribute('aria-label') || document.activeElement.textContent.trim()) : null;
    fill(body, prev, draw(d), tab === 'hot_keys' && !d.rows.length ? monitorBox : null);
    if (tab === 'hot_keys' && !d.rows.length) loadMonitor();
    if (focused) {
      const again = Array.from(body.querySelectorAll('button, a, select')).find((el) => (el.getAttribute('aria-label') || el.textContent.trim()) === focused);
      if (again) again.focus();
    }
  }

  async function drawNoReport(a) {
    const what = {
      big_keys: 'Big keys ranks the biggest sampled keys and lists what to look at.',
      expiry: 'Expiry ranks patterns by TTL risk and shows the memory their TTLs free.',
      changes: 'Changes compares an analysis with the last snapshot of this connection.',
      hot_keys: 'Hot keys ranks sampled keys by their LFU counter.',
      nodes: 'Nodes shows each shard and the node analyzed for it.',
      slots: 'Slots shows the hottest slots of each shard.',
    }[tab];
    const est = h('div', { class: 'notes' });
    fill(body, h('div', { class: 'card stack' },
      h('h2', { class: 'lbl', text: a.running ? 'Analyzing' : 'No analysis yet' }),
      h('p', { text: a.running ? 'The panel fills in once the analysis is done.' : `${what} It needs a sample of the keys.` }),
      est,
      h('p', { class: 'muted small' }, 'What the server reports without a sample is on ', h('a', { href: '#/overview', text: 'Overview' }), '.')));
    if (a.running) return;
    try {
      const info = await ctx.api.getAnalysis();
      store.info = info;
      if (alive) fill(est, info.estimates.map((e) => h('p', { class: 'muted', text: estimateText(e) })));
    } catch (err) {
      if (alive) fill(est, notice(errorText(err)));
    }
  }

  const openKey = (k) => ctx.openKey(k);
  const keyButton = (k) => h('button', { type: 'button', class: 'linkish mono clip', text: k.name, title: `Open ${k.name} on Browse`, onclick: () => openKey(k) });
  const patternButton = (p, node = null) => h('button', { type: 'button', class: 'linkish mono clip', text: p.name, title: `List the keys of ${p.name} on Browse`, onclick: () => ctx.openPattern(p, node) });
  const nodeName = (i) => {
    const s = ctx.state();
    const n = s.conn && typeof i === 'number' ? s.conn.nodes[i] : null;
    return n ? n.address : typeof i === 'number' ? `node ${i + 1}` : DASH;
  };

  function drawBig(d) {
    const max = Math.max(1, ...d.rows.map((r) => r.key.bytes || 0));
    const cluster = d.rows.some((r) => r.endpoint || typeof r.key.node === 'number');
    const rows = d.rows.map((r) => ({ ...r, rowClass: r.big ? 'flag' : '' }));
    const length = (r) => (typeof r.length === 'number' ? `${fmtGrouped(r.length)} ${r.lengthUnit || ''}`.trim() : DASH);
    const things = h('div', { class: 'card' },
      h('h2', { class: 'lbl', text: 'Things to look at' }),
      d.recommendations.length
        ? h('ul', { class: 'list' }, d.recommendations.map((r) => h('li', { class: 'row row-finding static' },
          h('span', { class: r.severity === 'warn' ? 'glyph' : 'muted', 'aria-hidden': 'true', text: r.severity === 'warn' ? '▲' : '•' }),
          h('span', null, h('span', { text: r.text }), r.hint ? h('span', { class: 'sub', text: r.hint }) : null),
          h('span'))))
        : h('p', { text: 'Nothing stands out in this sample.' }));
    return h('div', { class: 'stack-lg' },
      things,
      h('div', { class: 'card' },
        h('h2', { class: 'lbl', text: `Biggest sampled keys · ${d.rows.length}` }),
        d.empty ? emptyState(d.empty) : table([
          { label: 'Key', cls: 'keycell', cell: (r) => keyButton(r.key) },
          { label: 'Type', cell: (r) => h('span', { class: 'badge mono', text: r.key.type || DASH }) },
          { label: 'Share', cls: 'barcell', cell: (r) => meter(barPercent(r.key.bytes, max)) },
          { label: 'Size', cls: 'mono right nowrap', cell: (r) => `${r.big ? '▲ ' : ''}${fmtBytes(r.key.bytes)}` },
          { label: 'Length', cls: 'mono right nowrap', cell: length },
          { label: 'Encoding', cls: 'mono nowrap', cell: (r) => r.encoding || DASH },
          { label: 'Expires', cls: 'nowrap muted', cell: (r) => expiryText(r.key.ttl) || DASH },
          { label: 'Pattern', cls: 'keycell', cell: (r) => patternButton(r.pattern) },
          cluster ? { label: 'Node', cls: 'mono nowrap', cell: (r) => r.endpoint || nodeName(r.key.node) } : null,
        ].filter(Boolean), rows, { caption: 'The biggest sampled keys', cls: 'big' }),
        d.factsRead ? null : h('p', { class: 'muted small', text: 'Lengths and encodings are read for the listed keys when the panel opens.' }),
        d.notes.map((t) => notice(t))));
  }

  function drawExpiry(d) {
    const bars = freedBars(d.freed);
    const picked = store.picked.expiry ?? 0;
    const sel = d.rows[picked] || d.rows[0] || null;
    const freedCard = h('div', { class: 'card col' },
      h('h2', { class: 'lbl', text: 'Memory that expiry frees' }),
      d.freed ? h('div', { class: 'vbars', role: 'img', 'aria-label': bars.map((b) => `${b.label}: ${fmtBytes(b.bytes)}`).join(', ') }, bars.map((b) => {
        const bar = h('div', { class: b.never && b.share > 0.25 ? 'vbar warn' : b.never ? 'vbar never' : 'vbar' });
        setVar(bar, '--h', `${Math.max(2, Math.round((b.pct / 100) * 160))}px`);
        return h('div', { class: 'vbar-col', 'aria-hidden': 'true' },
          h('span', { class: 'mono small', text: fmtBytes(b.bytes) }), bar,
          h('span', { class: 'small muted', text: b.label }),
          h('span', { class: 'small muted', text: b.share === null ? '' : fmtPercent(b.share) }));
      })) : notice(d.freedOff || 'No memory figures.'),
      d.freed ? h('p', { class: 'muted small', text: `Computed from the sampled TTLs and sizes, scaled like the estimates. ${fmtPercent(d.freed.neverShare)} of the memory never expires.` }) : null);
    const list = h('div', { class: 'card col wide' },
      h('h2', { class: 'lbl', text: 'Patterns by TTL risk' }),
      d.empty ? emptyState(d.empty) : h('ul', { class: 'list' }, d.rows.map((r, i) => h('li', null, h('button', {
        type: 'button', class: `row row-exp${sel === r ? ' on' : ''}`, 'aria-current': sel === r ? 'true' : null,
        onclick: () => {
          store.picked.expiry = i;
          drawBody();
        },
      },
      h('span', { class: 'glyph', 'aria-hidden': 'true', text: r.warning ? '▲' : '' }),
      h('span', { class: 'name' }, patternName(r.pattern.name, { cls: 'mono clip block' }), h('span', { class: 'sub', text: `${r.status || DASH} · ~${fmtCount(r.estKeys)} keys` })),
      h('span', { class: 'mono right', text: `${fmtPercent(r.noTtlRatio)} no TTL` }),
      h('span', { class: 'mono right', text: fmtBytes(r.estBytes) }))))));
    const detail = sel ? h('div', { class: 'card stack' },
      h('h2', { class: 'key-title wrap-any' }, patternName(sel.pattern.name)),
      h('p', { text: `~${fmtGrouped(sel.estKeys)} keys · ${fmtBytes(sel.estBytes)} · ${sel.status || DASH}` }),
      sel.explain.map((t, i) => (i === 0 && sel.warning ? notice(t) : h('p', { class: 'muted', text: t }))),
      sel.ttlHist && d.ttlLabels.length === sel.ttlHist.length ? histogram(d.ttlLabels, sel.ttlHist) : null,
      sel.command ? h('div', { class: 'command' }, h('code', { class: 'mono wrap-any', text: `$ ${sel.command}` }), copy('Copy command', sel.command, 'the command')) : null,
      h('div', { class: 'row-actions' }, h('button', { type: 'button', class: 'btn', text: 'List its keys', onclick: () => ctx.openPattern(sel.pattern) }))) : null;
    return [h('div', { class: 'cols' }, list, freedCard), detail];
  }

  async function snapshotsList() {
    if (!store.snapshots) {
      try {
        store.snapshots = await ctx.api.listSnapshots();
      } catch (err) {
        store.snapshots = { error: errorText(err) };
      }
    }
    return store.snapshots;
  }

  function diffTable(d) {
    const max = Math.max(1, ...d.rows.map((r) => Math.abs((r.change && (r.change.deltaBytes ?? r.change.deltaKeys)) || 0)));
    const trend = d.rows.some((r) => r.trend);
    return d.rows.length ? table([
      { label: 'Pattern', cls: 'keycell', cell: (r) => patternButton(r.pattern) },
      { label: 'Change', cls: 'mono right nowrap', cell: (r) => `${r.change && r.change.alarming ? '▲ ' : ''}${r.change && typeof r.change.deltaBytes === 'number' ? fmtSignedBytes(r.change.deltaBytes) : fmtSignedCount(r.change && r.change.deltaKeys)}` },
      { label: '%', cls: 'mono right nowrap muted', cell: changePercent },
      { label: 'Now', cls: 'mono right nowrap', cell: (r) => (typeof r.bytesAfter === 'number' ? fmtBytes(r.bytesAfter) : fmtCount(r.keysAfter)) },
      { label: 'Keys', cls: 'mono right nowrap', cell: (r) => `${fmtCount(r.keysBefore)} → ${fmtCount(r.keysAfter)}` },
      { label: 'Why', cls: 'nowrap', cell: (r) => (r.change ? r.change.words || DASH : DASH) },
      { label: '', cls: 'barcell', cell: (r) => meter(barPercent(Math.abs((r.change && (r.change.deltaBytes ?? r.change.deltaKeys)) || 0), max), r.change && r.change.alarming ? 'warn' : '') },
      trend ? { label: 'Trend', cls: 'mono nowrap', cell: (r) => (r.trend ? h('span', { title: r.trend.ratioPerDay === null ? '' : `${fmtPercent(Math.abs(r.trend.ratioPerDay))} a day`, text: sparkline(r.trend.values) }) : '') } : null,
    ].filter(Boolean), d.rows, { caption: 'Patterns that changed', cls: 'changes' }) : emptyState(d.empty || 'No pattern changed.');
  }

  function drawChanges(d) {
    const box = h('div', { class: 'card stack' });
    const picker = h('div', { class: 'toolbar' });
    const result = h('div', { class: 'stack' });
    box.append(h('h2', { class: 'lbl', text: 'Changes' }), picker, result);
    const show = (x) => {
      if (x.status === 'diff') {
        fill(result,
          h('p', { text: `From ${x.before.utc || DASH} to ${x.after.utc || DASH}${typeof x.before.usedMemory === 'number' && typeof x.after.usedMemory === 'number' ? ` · server memory ${fmtBytes(x.before.usedMemory)} → ${fmtBytes(x.after.usedMemory)}` : ''}.` }),
          x.note ? notice(x.note, 'info') : null,
          diffTable(x),
          x.trendPoints ? h('p', { class: 'muted small', text: `Trend over ${plural(x.trendPoints.length, 'snapshot', 'snapshots')} and this analysis, oldest first; · marks a snapshot without the pattern.` }) : null);
      } else fill(result, x.status === 'failed' ? notice(x.text || 'Could not compare.') : emptyState(x.text || 'Looking for an earlier snapshot…'));
    };
    show(store.compared || d);
    snapshotsList().then((list) => {
      if (!alive || list.error) {
        if (list && list.error) fill(picker, notice(list.error));
        return;
      }
      const options = (sel, includeLast) => [
        includeLast ? h('option', { value: '', selected: sel === '', text: 'the last snapshot before it' }) : null,
        h('option', { value: 'current', selected: sel === 'current', text: 'this analysis' }),
        list.snapshots.filter((x) => !x.current).map((x) => h('option', { value: x.file, selected: sel === x.file, text: x.utc || x.file })),
      ];
      const a = h('select', { class: 'field', id: 'cmp-a' }, options(store.compare.a, true));
      const b = h('select', { class: 'field', id: 'cmp-b' }, options(store.compare.b, false));
      const go = h('button', {
        type: 'button', class: 'btn', text: 'Compare',
        onclick: async () => {
          store.compare = { a: a.value, b: b.value };
          if (a.value === '' && b.value === 'current') {
            store.compared = null;
            show(d);
            return;
          }
          go.disabled = true;
          try {
            store.compared = await ctx.api.compareSnapshots(a.value || 'current', b.value);
            show(store.compared);
          } catch (err) {
            ctx.notify(errorText(err), 'error');
          }
          go.disabled = false;
        },
      });
      fill(picker, h('label', { htmlFor: 'cmp-b', text: 'Compare' }), b, h('label', { htmlFor: 'cmp-a', text: 'with' }), a, go,
        list.snapshots.length ? null : h('span', { class: 'muted small', text: 'No snapshot saved yet: Save as snapshot keeps this analysis for the next comparison.' }));
    });
    return box;
  }

  function drawHot(d) {
    const max = Math.max(1, ...d.rows.map((r) => r.key.freq || 0));
    const cluster = d.rows.some((r) => r.endpoint || typeof r.key.node === 'number');
    return h('div', { class: 'card stack' },
      h('h2', { class: 'lbl', text: `Most-read keys, by LFU counter · ${d.rows.length}` }),
      d.empty || !d.rows.length ? emptyState(d.empty || 'No sampled key has an LFU counter above 0.') : table([
        { label: 'Key', cls: 'keycell', cell: (r) => keyButton(r.key) },
        { label: 'Type', cell: (r) => h('span', { class: 'badge mono', text: r.key.type || DASH }) },
        { label: 'Reads', cls: 'mono right nowrap', cell: (r) => (typeof r.key.freq === 'number' ? `${r.key.freq} of 255` : DASH) },
        { label: 'Heat', cls: 'barcell', cell: (r) => meter(barPercent(r.key.freq, max)) },
        { label: 'Size', cls: 'mono right nowrap', cell: (r) => `${r.hotAndBig ? '▲ ' : ''}${fmtBytes(r.key.bytes)}` },
        { label: 'Expires', cls: 'nowrap muted', cell: (r) => expiryText(r.key.ttl) || DASH },
        { label: 'Pattern', cls: 'keycell', cell: (r) => patternButton(r.pattern) },
        cluster ? { label: 'Node', cls: 'mono nowrap', cell: (r) => r.endpoint || nodeName(r.key.node) } : null,
      ].filter(Boolean), d.rows, { caption: 'The most-read sampled keys', cls: 'hot' }),
      // The thresholds come only with rows; with none there is nothing to mark.
      d.empty || !d.rows.length ? null
        : h('p', { class: 'muted small', text: `▲ hot and big: an LFU counter of ${fmtGrouped(d.hotFreq)} or more and ${fmtBytes(d.hotBigBytes)} or more.` }));
  }

  // ---------- Hot keys from the last Monitor run ----------

  const monitorBox = h('section', { class: 'card stack', 'aria-label': 'Hot keys from Monitor', 'aria-live': 'polite' });
  let monitorSeq = 0;
  let monitorTimer = null;
  async function loadMonitor() {
    const mine = (monitorSeq += 1);
    clearTimeout(monitorTimer);
    let m;
    try {
      m = await ctx.api.getHotKeysMonitor();
    } catch (err) {
      if (alive && mine === monitorSeq) fill(monitorBox, notice(errorText(err)));
      return;
    }
    if (!alive || mine !== monitorSeq) return;
    drawMonitor(m);
    // Once the run ends its keys are placed with the Slow log's key specs, and the commands
    // those lack are asked about with COMMAND INFO (COMMAND GETKEYS is never sent).
    if (m.status === 'pending') monitorTimer = setTimeout(() => alive && loadMonitor(), 700);
  }

  function drawMonitor(m) {
    monitorBox.hidden = m.status === 'lfu';
    if (m.status === 'none' || m.status === 'lfu') {
      const parts = monitorLinkParts(m.text);
      fill(monitorBox, h('h2', { class: 'lbl', text: 'Hot keys from Monitor' }),
        h('p', { class: 'muted' }, parts.before, parts.link ? h('a', { href: '#/server/monitor', text: parts.link }) : null, parts.after),
        h('p', { class: 'muted small', text: 'A Monitor run names the keys of the commands it sees; Hot keys ranks them once it stops.' }));
      return;
    }
    if (m.status === 'pending') {
      fill(monitorBox, h('h2', { class: 'lbl', text: 'Hot keys from Monitor' }), h('p', { class: 'muted', role: 'status', text: m.text || 'Ranking the keys of the last Monitor run…' }));
      return;
    }
    const r = m.ranking;
    const max = Math.max(1, ...r.rows.map((x) => x.count));
    fill(monitorBox,
      h('div', { class: 'toolbar' },
        h('h2', { class: 'lbl', text: `Most-named keys, ${r.title}` }),
        h('a', { class: 'btn quiet', href: '#/server/monitor', text: 'Run Monitor again' })),
      r.empty || !r.rows.length ? emptyState(r.empty || 'The last Monitor run named no keys.') : table([
        { label: 'Key', cls: 'keycell', cell: (x) => keyButton(x.key) },
        { label: 'Named', cls: 'mono right nowrap', cell: (x) => h('span', { title: x.named || null, text: `${fmtGrouped(x.count)}×` }) },
        { label: 'Share', cls: 'mono right nowrap', cell: (x) => fmtPercent(x.share) },
        { label: 'Heat', cls: 'barcell', cell: (x) => meter(barPercent(x.count, max)) },
        { label: 'Pattern', cls: 'keycell', cell: (x) => patternButton(x.pattern) },
      ], r.rows, { caption: 'The keys the last Monitor run named most', cls: 'hot' }),
      h('p', { class: 'muted small', text: m.text }),
      r.endpoint ? h('p', { class: 'muted small', text: `Monitor ran on ${r.endpoint}.` }) : null,
      r.unknownText ? h('p', { class: 'muted small', text: r.unknownText }) : null,
      r.keysNote ? h('p', { class: 'muted small', text: r.keysNote }) : null);
  }

  function drawNodes(d) {
    const state = { scanned: '✓ analyzed', partial: '▲ partial', failed: '✗ failed' };
    return h('div', { class: 'card stack' },
      h('h2', { class: 'lbl', text: `Shards · ${d.rows.length}` }),
      d.empty ? emptyState(d.empty) : table([
        { label: 'Slots', cls: 'mono nowrap', cell: (r) => r.slots || DASH },
        { label: 'Node', cls: 'mono nowrap', cell: (r) => r.endpoint || DASH },
        { label: 'Role', cell: (r) => r.role || DASH },
        { label: 'State', cls: 'nowrap', cell: (r) => state[r.status] },
        { label: 'Keys', cls: 'mono right', cell: (r) => fmtCount(r.keys) },
        { label: 'Memory', cls: 'mono right', cell: (r) => fmtBytes(r.memory) },
        { label: 'Sampled', cls: 'mono right', cell: (r) => fmtCount(r.sampledKeys) },
        { label: 'Patterns', cls: 'right', cell: (r) => (r.status === 'failed' ? DASH : h('button', { type: 'button', class: 'btn quiet', text: 'Show', 'aria-label': `Show the patterns of ${r.endpoint}`, onclick: () => ctx.openPattern(null, r.index) })) },
      ], d.rows, { caption: 'One row per shard', cls: 'shards' }),
      d.rows.filter((r) => r.note).map((r) => notice(`${r.endpoint}: ${r.note}`)));
  }

  function drawSlots(d) {
    const fig = (v, f) => (typeof v === 'number' ? f(v) : DASH);
    const us = (v) => `${fmtGrouped(v)} µs`;
    const cols = (extra) => [
      { label: 'Keys', cls: 'mono right', cell: (r) => fig(r.keys, fmtGrouped) },
      { label: 'CPU', cls: 'mono right nowrap', cell: (r) => fig(r.cpuUsec, us) },
      { label: 'Net in', cls: 'mono right', cell: (r) => fig(r.netIn, fmtBytes) },
      { label: 'Net out', cls: 'mono right', cell: (r) => fig(r.netOut, fmtBytes) },
      d.memoryColumn ? { label: 'Memory', cls: 'mono right', cell: (r) => fig(r.memory, fmtBytes) } : null,
      ...extra,
    ].filter(Boolean);
    const order = pills([['keys', 'Keys'], ['cpu', 'CPU'], ['net_in', 'Net in'], ['net_out', 'Net out']].map(([id, label]) => ({ id, label })), d.order, (id) => {
      store.slotsOrder = id;
      delete store.panels.slots;
      drawBody();
    }, 'Order the hottest slots by');
    return h('div', { class: 'stack-lg' },
      h('div', { class: 'card stack' },
        h('div', { class: 'toolbar' }, h('h2', { class: 'lbl', text: 'Hottest slots' }), order,
          h('button', { type: 'button', class: 'btn', text: 'Read again', onclick: () => drawBody(true) })),
        d.note ? notice(d.note, 'info') : null,
        d.empty || !d.hottest.length ? emptyState(d.empty || 'No slot has keys.') : table([
          { label: 'Slot', cls: 'mono right', cell: (r) => fig(r.slot, String) },
          { label: 'Node', cls: 'mono nowrap', cell: (r) => r.endpoint || DASH },
          ...cols([{ label: 'Sampled keys', cls: 'keycell', cell: (r) => (r.sampled.length ? h('span', { class: 'mono clip', text: r.sampled.map((k) => k.key.name).join(', ') }) : DASH) }]),
        ], d.hottest, { caption: 'The hottest slots', cls: 'slots' })),
      h('div', { class: 'card stack' },
        h('h2', { class: 'lbl', text: 'Slot ranges' }),
        table([
          { label: 'Slots', cls: 'mono nowrap', cell: (r) => `${fig(r.start, String)}-${fig(r.end, String)}` },
          { label: 'Node', cls: 'mono nowrap', cell: (r) => r.endpoint || DASH },
          { label: 'State', cell: (r) => (r.error ? `✗ ${r.error}` : r.status || DASH) },
          ...cols([]),
        ], d.ranges, { caption: 'Each shard\'s slot ranges', cls: 'slots' })));
  }

  async function drawSnapshots() {
    fill(body, h('div', { class: 'card' }, h('p', { class: 'muted', text: 'Reading the snapshots…' })));
    const list = await snapshotsList();
    if (!alive || tab !== 'snapshots') return;
    if (list.error) {
      fill(body, h('div', { class: 'card' }, notice(list.error)));
      return;
    }
    const a = ctx.state().analysis;
    const saveButton = h('button', { type: 'button', class: 'btn primary', text: 'Save as snapshot', disabled: !list.canSave || a.running, onclick: (e) => save(e.currentTarget) });
    fill(body, h('div', { class: 'card stack' },
      h('div', { class: 'toolbar' }, h('h2', { class: 'lbl', text: `Snapshots${list.profile ? ` of ${list.profile}` : ''} · ${list.snapshots.length}` }), saveButton),
      list.saveOff ? notice(list.saveOff, 'info') : list.saved ? notice('This analysis is saved.', 'ok') : null,
      h('p', { class: 'muted small', text: 'A snapshot keeps the patterns and example key names of an analysis, never a value or a credential. The next analysis compares against the newest one; Changes compares any two.' }),
      list.snapshots.length ? table([
        { label: 'Taken', cls: 'mono nowrap', cell: (r) => r.utc || DASH },
        { label: 'File', cls: 'mono wrap-any', cell: (r) => r.file },
        { label: '', cls: 'nowrap', cell: (r) => (r.current ? h('span', { class: 'badge', text: 'the analysis shown' }) : '') },
        {
          label: 'Compare', cls: 'right',
          cell: (r) => (r.current || !a.report ? '' : h('button', {
            type: 'button', class: 'btn quiet', text: 'Compare', 'aria-label': `Compare this analysis with the snapshot of ${r.utc || r.file}`,
            onclick: async () => {
              try {
                store.compare = { a: r.file, b: 'current' };
                store.compared = await ctx.api.compareSnapshots(r.file, 'current');
                ctx.navigate('#/analyze/changes');
              } catch (err) {
                ctx.notify(errorText(err), 'error');
              }
            },
          })),
        },
      ], list.snapshots, { caption: 'Saved snapshots, newest first' }) : emptyState('No snapshot of this connection yet.')));
  }

  function histogram(labels, counts) {
    const max = Math.max(1, ...counts.map((c) => (typeof c === 'number' ? c : 0)));
    return h('div', { class: 'hist', role: 'img', 'aria-label': `Expires in: ${labels.map((l, i) => `${l} ${counts[i] ?? 0}`).join(', ')}` },
      labels.map((l, i) => h('div', { class: 'hist-row', 'aria-hidden': 'true' }, h('span', { class: 'mono small', text: l }), meter(barPercent(counts[i], max), 'thin'), h('span', { class: 'mono small right', text: fmtCount(counts[i]) }))));
  }

  function copy(label, value, what) {
    return h('button', {
      type: 'button', class: 'btn', text: label,
      onclick: async () => {
        const problem = await copyText(value);
        ctx.notify(problem || `Copied ${what} to the clipboard.`, problem ? 'error' : 'ok');
      },
    });
  }

  // ---------- following the analysis ----------
  let last = JSON.stringify([state.analysis.status, state.analysis.run, !!state.analysis.report]);
  const onState = (s) => {
    if (!alive) return;
    drawHead();
    const now = JSON.stringify([s.analysis.status, s.analysis.run, !!s.analysis.report]);
    if (now !== last) {
      last = now;
      if (s.analysis.status === 'done' || s.analysis.status === 'none') {
        store.snapshots = null;
        drawBody();
      }
    }
  };
  const onProgress = (p) => {
    if (!alive) return;
    if (p.kind === 'stopped' || p.kind === 'failed') ctx.notify(store.lastText, p.kind === 'failed' ? 'error' : 'info');
    if (p.kind === 'done' || (p.kind === 'stopped' && p.kept)) {
      if (p.kind === 'done') ctx.notify(`Analysis done: ${plural(p.sampledKeys, 'key', 'keys')} sampled${p.stopped ? ', stopped early' : ''}.`, 'ok');
      store.compared = null;
      drawBody();
    }
    if (p.kind === 'compared' && tab === 'changes') drawBody();
  };
  listeners.add(onProgress);
  // A Monitor run that ends (here or in another tab) is ranked for Hot keys.
  const onMonitor = (e) => {
    if (alive && e.kind === 'closed' && tab === 'hot_keys' && monitorBox.isConnected) loadMonitor();
  };
  const offs = [ctx.api.on('state', onState), () => listeners.delete(onProgress), ctx.api.on('monitor', onMonitor), () => clearTimeout(monitorTimer)];

  drawHead();
  drawBody();
  if (store.pendingStart) {
    const depth = store.pendingStart;
    store.pendingStart = null;
    if (!ctx.state().analysis.running) ask(depth);
  }

  return () => {
    alive = false;
    offs.forEach((off) => off());
  };
}

