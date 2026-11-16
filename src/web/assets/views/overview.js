// Overview: the first screen after a connect, laid out to use the window's height
// (docs/ui-redesign.md §10). One line of server facts with the Analyze button; six live
// figures; then "Things to look at" on the left, with the picked finding under the list, and
// on the right where the memory is and what expiry frees. A connect reads no key, and
// neither does anything here.
//
// "Things to look at" lists the server's own findings (`overview.findings`), then those of
// the last analysis (`overview.analysis.findings`), worst first; "Where the memory is" is the
// last analysis's biggest patterns. Analyze asks first, with the quick and deep estimates.
// Nothing is made up: a figure the server did not give shows as "—". Nothing here asks the
// server for anything the page did not read before: the figures come with the `figures`
// event, the rest with GET /api/overview, and "Freed by TTL" with Analyze › Expiry's reading
// when the page holds it.

import {
  h, fill, DASH, fmtBytes, fmtCount, fmtDuration, fmtPercent, fmtRate, fmtClock, fmtGrouped, plural, barPercent,
  barChart, meter, setVar, table, notice, emptyState, connectFirst, errorText, patternName, fillWhile,
} from '../ui.js';
import { askAnalyze, heldPanel, freedBars, progressPercent, statusText } from './analyze.js';

/** The line under a live tile: how far back the bars go, or that they are paused. */
export function spanText(points, everySecs, live) {
  if (!live) return 'Live figures paused';
  if (!points || !everySecs) return 'Live';
  return `Last ${points * everySecs} seconds`;
}

/** Under the Memory tile: `of 24G · allkeys-lfu`, `no limit set · noeviction`. */
export function memorySub(max, policy) {
  const parts = [max ? `of ${fmtBytes(max)}` : 'no limit set'];
  if (policy) parts.push(policy);
  return parts.join(' · ');
}

/** Under the Keys figure: `31% have a TTL`. A missing part is left out. */
export function keysSub(keys, db) {
  const parts = [];
  if (typeof keys.ttlShare === 'number') parts.push(`${fmtPercent(keys.ttlShare)} have a TTL`);
  if (typeof db === 'number') parts.push(`database ${db}`);
  return parts.join(' · ') || DASH;
}

/** RDB and AOF in words: `RDB ok · AOF off`, with `✗` when a save or a write failed. */
export function persistenceText(f) {
  if (!f || (f.rdbOk === null && f.aofOn === null)) return null;
  const parts = [];
  if (f.rdbOk !== null) parts.push(f.rdbOk ? 'RDB ok' : '✗ RDB save failed');
  if (f.aofOn !== null) parts.push(!f.aofOn ? 'AOF off' : f.aofOk === false ? '✗ AOF write failed' : 'AOF ok');
  return parts.join(' · ');
}

/** When the last analysis was saved: `today 09:12 UTC`, or the date on another day. */
export function analysisWhen(at, now = Date.now()) {
  const day = (ms) => new Date(ms).toISOString().slice(0, 10);
  const time = `${fmtClock(at).slice(0, 5)} UTC`;
  return day(at) === day(now) ? `today ${time}` : `${day(at)} ${time}`;
}

/** The line under "Where the memory is": what the analysis was, in words. */
export function analysisLine(a, now = Date.now()) {
  if (!a) return null;
  const parts = [`From the analysis of ${analysisWhen(a.takenAt * 1000, now)}`, `${plural(a.sampledKeys, 'key', 'keys')} sampled`];
  if (a.stopped) parts.push('stopped early');
  else if (a.complete) parts.push('every key looked at');
  if (typeof a.neverExpiresShare === 'number') parts.push(`${fmtPercent(a.neverExpiresShare)} of the memory never expires`);
  return `${parts.join(' · ')}.`;
}

/** What Changes found, for Overview: null when there is nothing to say. */
export const CHANGES_TEXT = {
  diff: 'Changes since the last snapshot are on Analyze › Changes.',
  none: 'No earlier snapshot to compare with: Analyze › Snapshots saves one.',
  pending: 'Comparing with the last snapshot…',
  failed: 'Could not compare with the last snapshot: Analyze › Changes says why.',
};

/** The lines of "Turned off, and why": the figures that could not be read, the gaps, each
 *  refused check, the provider's notice. No line twice. */
export function offLines(data, conn) {
  const lines = [];
  const add = (text) => {
    if (text && !lines.includes(text)) lines.push(text);
  };
  if (data) {
    add(data.figuresFailed);
    data.gaps.forEach(add);
    for (const c of data.checks) if (c.status === 'denied') add(`${c.name}: ${c.why || 'refused for this user'}`);
    if (data.provider) add(data.provider.notice);
  }
  if (conn) {
    add(conn.providerNotice);
    if (conn.refused.length) add(`Refused by the server this session: ${conn.refused.join(', ')}.`);
  }
  return lines;
}

// ---------------------------------------------------------------------------------------
// The models the screen is drawn from (tests/overview.test.js)
// ---------------------------------------------------------------------------------------

const num = (v) => typeof v === 'number' && Number.isFinite(v);

/** The line under the head: the server facts that are text. Version, topology, uptime,
 *  role and replicas, persistence, policy; a part that is unknown is left out. */
export function factsLine(data, f, conn) {
  const sv = data ? data.server : {};
  const c = conn || {};
  const role = f && f.role
    ? `${f.role}${num(f.replicas) ? ` · ${plural(f.replicas, 'replica', 'replicas')}` : ''}${f.linkUp === false ? ' · ✗ link down' : ''}`
    : null;
  return [
    [sv.kind || c.server, sv.version || c.version].filter(Boolean).join(' ') || null,
    sv.topology || c.topology || null,
    f && num(f.uptimeSecs) ? `up ${fmtDuration(f.uptimeSecs)}` : null,
    role,
    persistenceText(f),
    (f && f.policy) || sv.policy || c.policy || null,
  ].filter(Boolean).join(' · ') || DASH;
}

/** How old the analysis is, for the head line beside Analyze. */
export function analysisAge(data, running, now = Date.now()) {
  if (running && running.running) return statusText(running);
  const a = data && data.analysis;
  if (a && num(a.takenAt)) return `${plural(a.sampledKeys, 'key', 'keys')} sampled ${analysisWhen(a.takenAt * 1000, now)}`;
  const last = data && data.lastAnalysis;
  if (last) return `Last analysis ${analysisWhen(last.at, now)} (a saved snapshot)`;
  return 'No analysis yet';
}

/** The six figure tiles, from what the page holds: the newest `figures` event (or the
 *  figures GET /api/overview came with), the reading of GET /api/overview, and the series
 *  of the bars it keeps (Commands / sec and Clients only: the page keeps no other series).
 *  Each tile: `{ id, label, value, sub, meter, bars }` (`meter` 0–100 or null, `bars` a
 *  series or null). A figure the server did not give is "—". */
export function tilesModel({ f = null, data = null, conn = null, series = null, live = true } = {}) {
  const s = series || { ops: [], clients: [], everySecs: null };
  const used = f && num(f.usedMemory) ? f.usedMemory : data && num(data.memory.used) ? data.memory.used : null;
  const max = f && f.maxMemory ? f.maxMemory : data && data.memory.max ? data.memory.max : null;
  const known = !!(f || data);
  const memorySubText = !known ? DASH : max ? `of ${fmtBytes(max)}${used !== null ? ` · ${fmtPercent(used / max)}` : ''}` : 'no limit set';
  const ops = f ? f.opsPerSec ?? f.opsNow : null;
  const blocked = f && num(f.blocked) ? f.blocked : null;
  const ev = f && num(f.evictedPerSec) ? f.evictedPerSec : null;
  const ex = f && num(f.expiredPerSec) ? f.expiredPerSec : null;
  const k = data ? data.keys : {};
  const frag = f && num(f.fragmentation) ? f.fragmentation : null;
  return [
    { id: 'memory', label: 'Memory', value: fmtBytes(used), sub: memorySubText, meter: max && used !== null ? barPercent(used, max) : null, bars: null },
    { id: 'ops', label: 'Commands / sec', value: fmtRate(ops), sub: spanText(s.ops.length, s.everySecs, live), meter: null, bars: s.ops },
    { id: 'clients', label: 'Clients', value: f && num(f.clients) ? fmtCount(f.clients) : DASH, sub: blocked !== null ? `${fmtGrouped(blocked)} blocked` : spanText(s.clients.length, s.everySecs, live), meter: null, bars: s.clients },
    { id: 'hits', label: 'Hit rate', value: f && num(f.hitRate) ? fmtPercent(f.hitRate) : DASH, sub: 'of key lookups', meter: f && num(f.hitRate) ? Math.round(Math.max(0, Math.min(1, f.hitRate)) * 100) : null, bars: null },
    { id: 'evicted', label: 'Evicted / expired', value: ev === null && ex === null ? DASH : `${fmtRate(ev)} / ${fmtRate(ex)}`, sub: 'keys per second', meter: null, bars: null },
    {
      id: 'frag', label: 'Fragmentation', value: frag === null ? DASH : frag.toFixed(2),
      sub: num(k.dataBytes) || num(k.overheadBytes) ? `${fmtBytes(k.dataBytes)} data · ${fmtBytes(k.overheadBytes)} overhead` : 'RSS ÷ used memory', meter: null, bars: null,
    },
  ];
}

/** The figures under "More figures": what the six tiles and the head line leave out. */
export function moreFigures({ f = null, data = null, conn = null } = {}) {
  const k = data ? data.keys : {};
  const n = (v, text) => (num(v) ? text(v) : DASH);
  const total = f && num(f.keys) ? f.keys : data ? k.total : null;
  return [
    ['Keys', fmtCount(total)],
    ['With a TTL', data ? keysSub(k, conn ? conn.db : null) : DASH],
    ['Data', n(k.dataBytes, fmtBytes)],
    ['Overhead', n(k.overheadBytes, fmtBytes)],
    ['Network in', n(f && f.netIn, (v) => `${fmtBytes(Math.round(v))}/s`)],
    ['Network out', n(f && f.netOut, (v) => `${fmtBytes(Math.round(v))}/s`)],
    ['CPU user', n(f && f.cpuUser, fmtPercent)],
    ['CPU system', n(f && f.cpuSys, fmtPercent)],
    ['Blocked clients', n(f && f.blocked, fmtGrouped)],
    ['Evicted / sec', n(f && f.evictedPerSec, fmtRate)],
    ['Expired / sec', n(f && f.expiredPerSec, fmtRate)],
    conn && conn.via ? ['Sentinel', conn.via] : null,
  ].filter(Boolean);
}

const SEVERITY = { warn: 0, note: 1, info: 2 };

/** "Things to look at": the server's findings, then the analysis's ones it did not say
 *  already, worst first (a warning before a note before the rest; in the order given
 *  within one). Each row: `{ key, warn, text, label, href, from }`. */
export function findingsModel(data) {
  const server = data && Array.isArray(data.findings) ? data.findings : [];
  const said = new Set(server.map((f) => f.text));
  const a = data && data.analysis;
  const sampled = a && Array.isArray(a.findings) ? a.findings.filter((f) => !said.has(f.text)) : [];
  const rows = [
    ...server.map((f) => ({ f, from: 'server' })),
    ...sampled.map((f) => ({ f, from: 'analysis' })),
  ].map((x, i) => ({ ...x, i, rank: SEVERITY[x.f.severity] ?? 1 }));
  rows.sort((x, y) => x.rank - y.rank || x.i - y.i);
  const seen = new Map();
  return rows.map(({ f, from }) => {
    // Keys stay put across readings: the text, numbered when two say the same.
    const n = (seen.get(f.text) || 0) + 1;
    seen.set(f.text, n);
    return { key: n > 1 ? `${f.text}#${n}` : f.text, warn: f.severity === 'warn', text: f.text, label: f.label || null, href: f.where ? `#/${f.where}` : null, from };
  });
}

/** The box under the list for the picked finding: its text, where it came from and where
 *  it is looked into. Only what the server sent for it (the API sends no longer text). */
export function findingBox(row, data) {
  if (!row) return null;
  const a = data && data.analysis;
  return {
    title: row.text,
    warn: row.warn,
    source: row.from === 'analysis'
      ? `From the analysis${a ? ` of ${plural(a.sampledKeys, 'key', 'keys')}` : ''}.`
      : 'From what the server reports: no key was read for it.',
    link: row.href ? { href: row.href, text: `Look into it on ${row.label || row.href.slice(2)}` } : null,
  };
}

/** The right column: "Where the memory is" before, during and after an analysis, and "Freed
 *  by TTL" when the page holds it. `analysis`: Overview's analysis part (null until one has
 *  a report, and while one runs); `run`: the state's analysis; `freed`: Analyze › Expiry's
 *  `freed`, when that panel was read for this analysis; `last`: the saved snapshot's date.
 *  There is no "Slowest right now" card: the page holds no slow-log entries outside
 *  Server › Slow log, and Overview reads none. */
export function rightCards({ analysis = null, run = null, freed = null, last = null, now = Date.now() } = {}) {
  let memory;
  if (run && run.running) {
    memory = { state: 'running', pct: progressPercent(run), text: statusText(run), depth: run.depth === 'deep' ? 'Deep analysis' : 'Quick analysis' };
  } else if (!analysis) {
    memory = {
      state: 'prompt',
      text: 'This needs a sample of the keys: Analyze reads one, after showing what it costs.',
      last: last ? `Last analysis: ${analysisWhen(last.at, now)}, ${fmtGrouped(last.sampledKeys)} keys (a saved snapshot).` : null,
    };
  } else if (!analysis.topPatterns.length) {
    memory = { state: 'empty', text: 'The analysis found no pattern.', line: analysisLine(analysis, now) };
  } else {
    const byBytes = !!analysis.sizes && analysis.topPatterns.some((t) => num(t.estBytes));
    const max = Math.max(1, ...analysis.topPatterns.map((t) => (byBytes ? t.estBytes : t.estKeys) || 0));
    memory = {
      state: 'done',
      title: byBytes ? 'Where the memory is' : 'Keys by pattern',
      rows: analysis.topPatterns.map((t) => ({
        pattern: t.pattern,
        pct: barPercent(byBytes ? t.estBytes : t.estKeys, max),
        share: num(t.share) ? fmtPercent(t.share) : DASH,
        size: byBytes ? fmtBytes(t.estBytes) : `~${fmtCount(t.estKeys)}`,
      })),
      note: analysis.note || null,
      line: analysisLine(analysis, now),
      changes: analysis.changes && CHANGES_TEXT[analysis.changes] ? { text: CHANGES_TEXT[analysis.changes], href: analysis.changes === 'none' ? '#/analyze/snapshots' : '#/analyze/changes' } : null,
    };
  }
  let freedCard = null;
  if (analysis && !(run && run.running)) {
    if (freed) {
      freedCard = { total: num(freed.total) ? fmtBytes(freed.total) : null, rows: freedBars(freed).map((b) => ({ label: b.label, pct: b.pct, never: b.never, share: b.share === null ? DASH : fmtPercent(b.share), size: fmtBytes(b.bytes) })) };
    } else if (num(analysis.neverExpiresShare)) {
      freedCard = { total: null, rows: null, line: `${fmtPercent(analysis.neverExpiresShare)} of the memory never expires. Analyze › Expiry shows what the TTLs free.` };
    }
  }
  return { memory, freed: freedCard, slowest: null };
}

// ---------------------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------------------

export function mount(root, ctx) {
  const state = ctx.state();
  const gate = connectFirst(state);
  const view = h('section', { class: 'view', 'aria-labelledby': 'overview-title' });
  root.append(view);
  if (gate) {
    view.append(h('h1', { class: 'title', id: 'overview-title', text: 'Overview' }), gate);
    return () => {};
  }

  // The screen fills the window and its panes scroll inside themselves (app.css).
  const unfill = fillWhile();
  view.classList.add('ov-view');

  const conn = state.conn;
  let alive = true;
  let data = null;
  let shardLive = new Map();

  // ---------- the head line: facts, analysis age, Analyze ----------
  const factsText = h('p', { class: 'mono muted small ov-facts' });
  const ageText = h('span', { class: 'muted small' });
  const analyzeButton = h('button', { type: 'button', class: 'btn', text: 'Analyze', 'data-act': 'analyze', onclick: () => askAnalyze(ctx) });
  const drawButtons = () => {
    const a = ctx.state().analysis;
    analyzeButton.disabled = a.running;
    analyzeButton.title = a.running ? 'An analysis is running: wait for it, or stop it on Analyze.' : 'Analyze the keys: it asks first, quick or deep, with what each costs.';
    ageText.textContent = analysisAge(data, a);
  };
  view.append(h('div', { class: 'ov-head' },
    h('div', { class: 'ov-title' }, h('h1', { class: 'title', id: 'overview-title', text: 'Overview' }), factsText),
    h('div', { class: 'ov-analyze' }, ageText, analyzeButton)));

  // ---------- the six tiles ----------
  const tileEls = new Map();
  const tilesBox = h('div', { class: 'ov-tiles' });
  for (const t of tilesModel({ conn })) {
    const value = h('span', { class: 'tile-value', text: DASH });
    const sub = h('span', { class: 'tile-sub small', text: ' ' });
    const bars = t.bars ? barChart({ points: 24, height: 22 }) : null;
    const m = meter(0, 'thin');
    const el = h('div', { class: 'card tile' },
      h('span', { class: 'lbl', text: t.label }),
      h('div', { class: 'tile-row' }, value, bars ? bars.el : null),
      sub, m);
    tileEls.set(t.id, { value, sub, bars, meter: m });
    tilesBox.append(el);
  }
  const liveNote = h('div');
  const more = h('dl', { class: 'facts compact' });
  const moreBox = h('details', { class: 'advanced more-figures' }, h('summary', { text: 'More figures' }), more);
  const shardsBox = h('div');

  // ---------- the two columns ----------
  const findList = h('ul', { class: 'list', 'aria-label': 'Things to look at' });
  const findCount = h('h2', { class: 'lbl', text: 'Things to look at' });
  const findBox = h('div', { class: 'finding-box', 'aria-live': 'polite' });
  const findPane = h('section', { class: 'card pane ov-findings', 'data-pane': 'findings', 'aria-label': 'Things to look at' },
    h('div', { class: 'pane-head' }, findCount, h('span', { class: 'muted small', text: 'worst first' })),
    h('div', { class: 'pane-scroll' }, findList, h('p', { class: 'muted pad', text: 'Reading what the server reports…' })),
    findBox);
  const memPane = h('section', { class: 'card pane', 'data-pane': 'top-patterns', 'aria-label': 'Where the memory is' });
  const freedPane = h('section', { class: 'card pane', 'aria-label': 'Freed by TTL', hidden: true });
  const right = h('div', { class: 'ov-right' }, memPane, freedPane);
  const offBox = h('div', { class: 'ov-off' });

  view.append(tilesBox, liveNote, moreBox, shardsBox, h('div', { class: 'ov-cols' }, findPane, right), offBox);

  function drawLive() {
    const s = ctx.state();
    const f = ctx.figures() || (data && data.figures);
    factsText.textContent = factsLine(data, f, conn);
    for (const t of tilesModel({ f, data, conn, series: ctx.series(), live: s.live })) {
      const el = tileEls.get(t.id);
      el.value.textContent = t.value;
      el.sub.textContent = t.sub;
      if (el.bars) el.bars.update(t.bars || []);
      el.meter.hidden = t.meter === null;
      if (t.meter !== null) setVar(el.meter.firstElementChild, '--w', `${t.meter}%`);
    }
    const off = ctx.figuresOff();
    fill(liveNote, off ? notice(`The live figures are off: ${off}.`) : null);
    fill(more, moreFigures({ f, data, conn }).map(([k, v]) => h('div', null, h('dt', { text: k }), h('dd', { class: 'mono', text: v }))));
  }

  function drawShards() {
    if (!data || !data.shards.length) {
      fill(shardsBox);
      return;
    }
    const down = { 'not-connected': '✗ not connected', 'no-answer': '✗ no answer' };
    const rows = data.shards.map((r) => {
      const now = shardLive.get(r.id);
      const merged = now ? { ...r, opsPerSec: now.opsPerSec ?? r.opsPerSec, usedMemory: now.usedMemory ?? r.usedMemory, keys: now.keys ?? r.keys, role: now.role ?? r.role, status: r.status === 'not-connected' ? r.status : now.failed ? 'no-answer' : 'ok' } : r;
      return { ...merged, picked: r.id === conn.pickedNode, rowClass: r.id === conn.pickedNode ? 'on' : '' };
    });
    const figure = (row, text) => (row.status === 'ok' ? text : DASH);
    fill(shardsBox, h('div', { class: 'card tight' },
      h('h2', { class: 'lbl', text: `Shards · ${data.shards.length}` }),
      table([
        { label: 'Slots', cls: 'mono nowrap', cell: (r) => r.slots || DASH },
        { label: 'Node', cls: 'mono nowrap', cell: (r) => r.address || DASH },
        { label: 'Role', cell: (r) => (r.status === 'ok' ? r.role || DASH : down[r.status]) },
        { label: 'Ops / sec', cls: 'mono right', cell: (r) => figure(r, fmtRate(r.opsPerSec)) },
        { label: 'Memory', cls: 'mono right', cell: (r) => figure(r, fmtBytes(r.usedMemory)) },
        { label: 'Keys', cls: 'mono right', cell: (r) => figure(r, fmtCount(r.keys)) },
        {
          label: 'Shown',
          cls: 'right',
          cell: (r) => (r.picked
            ? h('span', { class: 'strong', text: '› shown above' })
            : h('button', { type: 'button', class: 'btn quiet', text: 'Show', 'aria-label': `Show node ${r.address || r.id}`, disabled: r.status === 'not-connected', onclick: () => ctx.pickNode(r.index) })),
        },
      ], rows, { caption: 'One row per shard', cls: 'shards' }),
      data.shards.filter((r) => r.note).map((r) => notice(`${r.address}: ${r.note}`))));
  }

  // ---------- Things to look at ----------
  /** The finding picked: the one the list's selection is on (moving in the list picks; a
   *  click picks). Kept by its key across readings; the first when it is gone. */
  let picked = null;
  let rowsNow = [];
  function drawFindings() {
    rowsNow = findingsModel(data);
    if (!rowsNow.some((r) => r.key === picked)) picked = rowsNow.length ? rowsNow[0].key : null;
    findCount.textContent = data ? `Things to look at · ${rowsNow.length}` : 'Things to look at';
    const scroll = findPane.querySelector('.pane-scroll');
    if (!rowsNow.length) {
      fill(findList);
      fill(scroll, findList, h('p', { class: 'muted pad', text: data && data.analysis ? 'Nothing stands out in what the server reports or in the sampled keys.' : 'Nothing stands out in what the server reports. An analysis samples keys and adds what it finds here.' }));
      drawBox();
      return;
    }
    fill(findList, rowsNow.map((r) => h('li', null, h('div', { class: `row row-finding${r.key === picked ? ' picked' : ''}`, 'data-row': r.key, title: r.text },
      h('span', { class: r.warn ? 'glyph' : 'muted', 'aria-hidden': 'true', text: r.warn ? '▲' : '•' }),
      h('span', { class: 'clip' }, r.warn ? h('span', { class: 'sr-only', text: 'Warning: ' }) : null, r.text),
      r.href ? h('a', { class: 'where', href: r.href, 'data-open': '', text: `${r.label || r.href.slice(2)} ›` }) : h('span', { class: 'muted small', text: r.label || '' })))));
    fill(scroll, findList);
    drawBox();
    // The list's selection (app.js keeps it per pane, by the row's key) is the finding
    // picked: once it is marked, the box follows it.
    queueMicrotask(() => {
      const sel = alive ? findList.querySelector('[data-row].sel') : null;
      if (sel) pickRow(sel);
    });
  }
  function drawBox() {
    const box = findingBox(rowsNow.find((r) => r.key === picked) || null, data);
    if (!box) {
      fill(findBox);
      return;
    }
    fill(findBox,
      h('p', { class: 'strong' }, box.warn ? h('span', { class: 'glyph', 'aria-hidden': 'true', text: '▲ ' }) : null, box.title),
      h('p', { class: 'muted small', text: box.source }),
      box.link ? h('a', { class: 'small', href: box.link.href, text: box.link.text }) : null);
  }
  function pickRow(row) {
    if (!row || !findList.contains(row) || row.dataset.row === picked) return;
    picked = row.dataset.row;
    for (const r of findList.querySelectorAll('[data-row]')) r.classList.toggle('picked', r.dataset.row === picked);
    drawBox();
  }
  const pickFinding = (e) => pickRow(e.target && e.target.closest ? e.target.closest('[data-row]') : null);
  findList.addEventListener('focusin', pickFinding);
  findList.addEventListener('click', (e) => {
    pickFinding(e);
    const row = e.target.closest ? e.target.closest('[data-row]') : null;
    if (row && !e.target.closest('a')) row.focus();
  });

  // ---------- the right column ----------
  function drawRight() {
    const s = ctx.state();
    const a = data ? data.analysis : null;
    const held = a ? heldPanel('expiry', s) : null;
    const m = rightCards({ analysis: a, run: s.analysis, freed: held && held.freed ? held.freed : null, last: data ? data.lastAnalysis : null });
    const mem = m.memory;
    const head = (title, side = null) => h('div', { class: 'pane-head' }, h('h2', { class: 'lbl', text: title }), side);
    if (mem.state === 'running') {
      const bar = meter(mem.pct);
      fill(memPane, head('Where the memory is', h('span', { class: 'mono small', text: `${mem.pct}%` })),
        h('div', { class: 'pane-scroll pad stack' }, h('span', { class: 'lbl', text: mem.depth }), bar, h('p', { class: 'muted small', role: 'status', text: mem.text }),
          h('p', { class: 'muted small' }, 'The biggest patterns show here when it is done. ', h('a', { href: '#/analyze', text: 'Follow it on Analyze' }), '.')));
    } else if (mem.state === 'prompt' || mem.state === 'empty') {
      fill(memPane, head('Where the memory is'),
        h('div', { class: 'pane-scroll pad' }, emptyState(mem.text, mem.state === 'prompt'
          ? h('button', { type: 'button', class: 'btn primary', text: 'Analyze', title: 'It asks first, quick or deep, with what each costs.', onclick: () => askAnalyze(ctx) }) : null),
        mem.last ? h('p', { class: 'muted small', text: mem.last }) : null,
        mem.line ? h('p', { class: 'muted small', text: mem.line }) : null));
    } else {
      fill(memPane, head(mem.title, h('span', { class: 'muted small', text: 'open in Browse ›' })),
        h('div', { class: 'pane-scroll' }, h('ul', { class: 'list' }, mem.rows.map((r) => h('li', null, h('button', {
          type: 'button', class: 'row row-mem', title: `List the keys of ${r.pattern.name} on Browse`, 'data-row': r.pattern.id || r.pattern.name, 'aria-label': `${r.pattern.name}: ${r.size}, ${r.share}. List its keys on Browse.`,
          onclick: () => ctx.openPattern(r.pattern),
        },
        patternName(r.pattern.name, { cls: 'mono clip name' }),
        meter(r.pct),
        h('span', { class: 'mono right small', text: r.share }),
        h('span', { class: 'mono right small muted', text: r.size })))))),
        h('div', { class: 'pane-foot' },
          mem.note ? notice(mem.note, 'info') : null,
          h('p', { class: 'muted small', text: mem.line }),
          mem.changes ? h('p', { class: 'small' }, h('a', { href: mem.changes.href, text: mem.changes.text })) : null));
    }
    const fr = m.freed;
    freedPane.hidden = !fr;
    if (fr) {
      fill(freedPane, head('Freed by TTL', fr.total ? h('span', { class: 'muted small', text: `of ${fr.total} sampled` }) : null),
        fr.rows ? h('ul', { class: 'list pad-x' }, fr.rows.map((r) => h('li', { class: 'row row-mem static' },
          h('span', { class: r.never ? 'warn-line' : '', text: r.label }),
          meter(r.pct, r.never ? 'warn' : ''),
          h('span', { class: 'mono right small', text: r.share }),
          h('span', { class: 'mono right small muted', text: r.size }))))
          : h('p', { class: 'muted small pad' }, fr.line, ' ', h('a', { href: '#/analyze/expiry', text: 'Open Expiry' })));
    }
  }

  // ---------- Turned off, and why ----------
  function drawOff() {
    const lines = offLines(data, conn);
    const checks = data ? data.checks : [];
    if (!lines.length && !checks.length) {
      fill(offBox);
      return;
    }
    const glyph = { allowed: '✓', denied: '✗', skipped: '–' };
    const word = { allowed: 'allowed', denied: 'refused', skipped: 'not checked' };
    fill(offBox,
      h('h2', { class: 'sr-only', text: lines.length ? 'Turned off, and why' : 'Permissions' }),
      lines.length ? lines.map((l) => h('p', { class: 'note off-line' }, h('span', { 'aria-hidden': 'true', class: 'glyph', text: '▲' }), h('span', { text: `Off: ${l}` }))) : notice('Every check passed.', 'ok'),
      checks.length ? h('details', { class: 'about' },
        h('summary', { text: `Every permission check (${checks.length})` }),
        h('ul', { class: 'list' }, checks.map((c) => h('li', { class: 'row row-check' },
          h('span', { 'aria-hidden': 'true', class: c.status === 'allowed' ? 'glyph good' : c.status === 'denied' ? 'glyph bad' : 'muted', text: glyph[c.status] }),
          h('span', null,
            h('span', { class: 'mono name', text: c.name }),
            h('span', { class: 'sr-only', text: ` ${word[c.status]}` }),
            c.why ? h('span', { class: 'sub', text: c.why }) : null))))) : null);
  }

  /** What each part was last drawn from: a part is drawn again only when it changed, so
   *  an open list of checks stays open and the list keeps its focus. */
  const drawnFrom = { findings: null, shards: null, off: null };
  const changed = (part, value) => {
    const signature = JSON.stringify(value);
    if (drawnFrom[part] === signature) return false;
    drawnFrom[part] = signature;
    return true;
  };

  /** Draw a reading: the reply to GET /api/overview, or an `overview` event. */
  function take(next) {
    data = next;
    drawButtons();
    drawRight();
    drawLive();
    if (changed('shards', data.shards)) drawShards();
    if (changed('findings', [data.findings, data.analysis && data.analysis.findings])) drawFindings();
    if (changed('off', [data.figuresFailed, data.gaps, data.checks, data.provider])) drawOff();
  }

  /** Only the newest read is drawn: one asked before another (a load after an analysis,
   *  then another) that answers late is dropped. */
  let readSeq = 0;
  async function load() {
    const mine = (readSeq += 1);
    try {
      const next = await ctx.api.getOverview();
      if (alive && mine === readSeq) take(next);
    } catch (err) {
      if (!alive || mine !== readSeq) return;
      drawnFrom.findings = null;
      fill(findList);
      fill(findPane.querySelector('.pane-scroll'), notice(errorText(err)));
    }
  }

  const redraw = (f) => {
    if (f && f.shards) {
      shardLive = new Map(f.shards.map((s) => [s.id, s]));
      drawShards();
    }
    drawLive();
  };
  // redis-lens sends `overview` when what the connect read changes (its findings land
  // after the live connection is open, another node is picked, ...). The event has no live
  // figures and no shard rows: those come with `figures`, so the ones drawn are kept.
  const fromEvent = (next) => {
    if (!alive) return;
    readSeq += 1;
    take({ ...next, figures: data ? data.figures : null, shards: data ? data.shards : [] });
  };
  let wasRunning = ctx.state().analysis.running;
  let lastProgress = '';
  const onState = (s) => {
    if (!alive) return;
    drawButtons();
    const progress = JSON.stringify([s.analysis.status, s.analysis.sampled, s.analysis.target]);
    if (s.analysis.running !== wasRunning) {
      wasRunning = s.analysis.running;
      drawRight();
      if (!wasRunning) load();
    } else if (wasRunning && progress !== lastProgress) drawRight();
    lastProgress = progress;
  };
  const offs = [ctx.api.on('figures', redraw), ctx.api.on('live', () => drawLive()), ctx.api.on('overview', fromEvent), ctx.api.on('state', onState)];
  drawButtons();
  drawLive();
  drawRight();
  load();

  return () => {
    alive = false;
    unfill();
    offs.forEach((off) => off());
  };
}
