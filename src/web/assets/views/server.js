// Server: the two live figures in a strip, and the panels Clients · Slow log · Monitor.
// Clients and Slow log are a list with the picked row's detail beside it (under it on a
// narrow window); Monitor's log takes the height.
//
// Monitor is time-boxed: the server always asks first (the confirm dialog), it counts down,
// and stops by itself; Stop ends it early. Its lines stay in memory for the session and are never saved. Slow log and
// Monitor lines arrive already masked by redis-lens.

import {
  h, fill, DASH, fmtBytes, fmtCount, fmtDuration, fmtRate, fmtTook, fmtClock, fmtGrouped, plural,
  barPercent, barChart, meter, table, pills, notice, emptyState, connectFirst,
  fillLog, errorText, listPick, replyFits, paneCard, aboutThis, fillWhile,
} from '../ui.js';
import { spanText } from './overview.js';
import { hexOfText } from './browse.js';

export const TABS = [
  { id: 'clients', label: 'Clients', href: '#/server/clients' },
  { id: 'slowlog', label: 'Slow log', href: '#/server/slowlog' },
  { id: 'monitor', label: 'Monitor', href: '#/server/monitor' },
];

const MONITOR_CAP = 2000;
const MONITOR_SHOWN = 300;

/** A Monitor run's memory, kept while other views are shown. */
export function monitorNew() {
  return { run: null, running: false, endsAt: 0, lines: [], kept: 0, stopped: null, maxSecs: 30, maxLines: 10000, conn: null };
}

const mon = monitorNew();

/** A run was asked for (the reply to Start, or a state that names a run this page did
 *  not know): start with an empty list and count down from now, or from when it began. */
export function monitorBegin(m, { run, maxSecs, maxLines, startedAt = null }, now = Date.now()) {
  m.run = run;
  m.running = true;
  m.stopped = null;
  m.lines = [];
  m.kept = 0;
  if (typeof maxSecs === 'number') m.maxSecs = maxSecs;
  if (typeof maxLines === 'number') m.maxLines = maxLines;
  m.endsAt = (startedAt ?? now) + m.maxSecs * 1000;
  return m;
}

/** Take in a `monitor` event. Events of another run than the one on screen are ignored,
 *  except `ready`, which starts showing its run. */
export function monitorApply(m, e, now = Date.now()) {
  if (e.kind === 'ready') {
    if (e.run !== m.run) monitorBegin(m, { run: e.run, maxSecs: e.maxSecs, maxLines: e.maxLines, startedAt: e.startedAt }, now);
    else {
      m.running = true;
      if (typeof e.maxSecs === 'number') m.maxSecs = e.maxSecs;
      if (typeof e.maxLines === 'number') m.maxLines = e.maxLines;
      m.endsAt = (e.startedAt ?? now) + m.maxSecs * 1000;
    }
    return m;
  }
  if (m.run !== null && e.run !== null && e.run !== m.run) return m;
  if (e.kind === 'lines') {
    m.kept += e.lines.length;
    m.lines = m.lines.concat(e.lines);
    if (m.lines.length > MONITOR_CAP) m.lines = m.lines.slice(m.lines.length - MONITOR_CAP);
  } else if (e.kind === 'closed') {
    m.running = false;
    m.stopped = e.stopped || 'MONITOR stopped.';
  }
  return m;
}

/** Seconds left on the countdown, never below zero. */
export function monitorLeft(m, now = Date.now()) {
  return m.running ? Math.max(0, Math.ceil((m.endsAt - now) / 1000)) : 0;
}

/** Follow the server's state: a run this page did not start (another tab, or a reload in
 *  the middle of one) is picked up; a run that is gone is marked stopped. */
export function monitorFollow(m, state, now = Date.now()) {
  const name = state.conn ? `${state.conn.name}|${state.conn.docker}` : null;
  if (name !== m.conn) {
    Object.assign(m, monitorNew(), { conn: name });
  }
  const run = state.monitor;
  if (run && run.run !== m.run) monitorBegin(m, { run: run.run, maxSecs: run.maxSecs, maxLines: run.maxLines, startedAt: run.startedAt }, now);
  else if (run && run.startedAt) m.endsAt = run.startedAt + m.maxSecs * 1000;
  else if (!run && m.running) {
    m.running = false;
    m.stopped = m.stopped || 'MONITOR stopped.';
  }
  return m;
}

/** Called once at boot: keep the Monitor lines whatever view is on screen. */
export function init(api) {
  api.on('monitor', (e) => monitorApply(mon, e));
  api.on('state', (s) => monitorFollow(mon, s));
}

/** What the slow log keeps, in words. */
export function slowlogSettingsText(settings, off) {
  if (!settings) return off ? `The threshold and length are unknown: ${off}.` : 'The threshold and length are unknown.';
  const kept = settings.maxLen === null ? '' : ` The last ${fmtGrouped(settings.maxLen)} are kept.`;
  const t = settings.thresholdMicros;
  if (t === null) return `The threshold is unknown.${kept}`;
  if (t < 0) return `The slow log is switched off on the server (slowlog-log-slower-than is negative).${kept}`;
  if (t === 0) return `Every command is logged (the threshold is 0).${kept}`;
  return `Commands slower than ${fmtTook(t)} are logged.${kept}`;
}

/** The line under a slow-log command: its keys, its client, the note. */
export function slowEntryFacts(e) {
  const keys = e.keys.length ? `${e.keys.length === 1 ? 'key' : 'keys'} ${e.keys.join(', ')}${e.moreKeys ? ` +${e.moreKeys} more` : ''}` : null;
  const client = e.client ? `from ${e.client}${e.clientName ? ` (${e.clientName})` : ''}` : null;
  const cut = e.argsCut ? `… ${fmtGrouped(e.argsCut)} more arguments` : null;
  return [keys, client, cut, e.note].filter(Boolean).join(' · ');
}

// ---------------------------------------------------------------------------------------
// The lists beside their detail (tests/server.test.js)
// ---------------------------------------------------------------------------------------

/** Slow log as a list beside its entry: each entry with its key (its id in the log, which
 *  the server never gives two entries), whether it is flagged (redis-lens has a note on
 *  it), and the one picked (`listPick`: kept across readings by its key). */
export function slowModel(data, picked = null) {
  const entries = data && !data.off && Array.isArray(data.entries) ? data.entries : [];
  const rows = entries.map((e, i) => ({ ...e, key: typeof e.id === 'number' ? `s:${e.id}` : `t:${e.at}#${i}`, flag: !!e.note }));
  const at = listPick(rows, picked);
  return { rows, key: at.key, sel: at.index >= 0 ? rows[at.index] : null, gone: at.gone };
}

/** An entry's detail, label and value, from the entry only (nothing is read for it). */
export function slowDetail(e) {
  const client = e.client ? `${e.client}${e.clientName ? ` (${e.clientName})` : ''}` : DASH;
  return [
    ['When', typeof e.at === 'number' ? `${new Date(e.at).toISOString().slice(0, 10)} ${fmtClock(e.at)} UTC` : DASH],
    ['Took', fmtTook(e.micros)],
    ['Command', e.name || DASH],
    ['Arguments', `${e.command || DASH}${e.argsCut ? ` … ${fmtGrouped(e.argsCut)} more arguments` : ''}`],
    ['Client', client],
  ];
}

/** A slow-log key that can be opened on Browse: one written as its own text (a key shown
 *  with escapes, `\x00`, is not the text it shows, so it is not linked). */
export function slowKeyOpens(k) {
  return typeof k === 'string' && k.length > 0 && !k.includes('\\');
}

const GROUP_SECTIONS = [['library', 'By library'], ['address', 'By address']];

/** Clients as a list beside its detail: the groups in two headed sections (by library, by
 *  address), each row with its key `<by>:<name>`, and the picked group's connections
 *  (`members`, from the same reading: nothing is read for a pick). A picked group that a
 *  reading no longer has stays picked and is said to be gone; another group is never
 *  shown in its place. */
export function clientsModel(data, picked = null) {
  if (!data || data.off) return { sections: [], rows: [], key: null, sel: null, gone: false, name: null, members: [] };
  const groups = Array.isArray(data.groups) ? data.groups : [];
  const sections = GROUP_SECTIONS.map(([by, title]) => {
    const rows = groups.filter((g) => g.by === by).map((g) => ({ ...g, key: `${g.by}:${g.name}` }));
    return { by, title, rows, most: Math.max(0, ...rows.map((g) => g.connections || 0)) };
  }).filter((s) => s.rows.length);
  const rows = sections.flatMap((s) => s.rows);
  const at = listPick(rows, picked);
  const sel = at.index >= 0 ? rows[at.index] : null;
  const members = sel ? sel.members.map((i) => (data.connections || [])[i]).filter(Boolean) : [];
  const name = sel ? sel.name : at.key ? at.key.slice(at.key.indexOf(':') + 1) : null;
  return { sections, rows, key: at.key, sel, gone: at.gone, name, members };
}

/** The line under a group's name: oldest, longest idle, its commonest command, memory. */
export function groupFacts(g) {
  return [
    g.oldestSecs !== null ? `oldest ${fmtDuration(g.oldestSecs)}` : null,
    g.idleSecs !== null ? `longest idle ${fmtDuration(g.idleSecs)}` : null,
    g.lastCommand ? `mostly ${g.lastCommand}` : null,
    g.memory !== null ? fmtBytes(g.memory) : null,
  ].filter(Boolean).join(' · ');
}

export function mount(root, ctx, route) {
  const state = ctx.state();
  const gate = connectFirst(state, { needsLive: true });
  const view = h('section', { class: 'view', 'aria-labelledby': 'server-title' });
  root.append(view);
  if (gate) {
    view.append(h('h1', { class: 'title', id: 'server-title', text: 'Server' }), gate);
    return () => {};
  }

  const tab = TABS.some((t) => t.id === route.sub) ? route.sub : 'clients';
  let alive = true;
  const timers = [];
  const offs = [];
  // The panes fill the window and scroll inside themselves (app.css).
  const unfill = fillWhile();
  view.classList.add('lv-view');

  // ---------- the strip: the two live figures, compact ----------
  const chart = (label) => {
    const value = h('span', { class: 'strip-value mono', text: DASH });
    const bars = barChart({ points: 30, height: 22 });
    const span = h('span', { class: 'small muted', text: 'Live' });
    const el = h('div', { class: 'strip-item' }, h('span', { class: 'lbl', text: label }), value, bars.el, span);
    return { el, value, bars, span };
  };
  const ops = chart('Commands / sec');
  const clients = chart('Connected clients');
  function drawCharts() {
    const f = ctx.figures();
    const series = ctx.series();
    const live = ctx.state().live;
    const off = ctx.figuresOff();
    ops.value.textContent = fmtRate(f ? f.opsPerSec ?? f.opsNow : null);
    ops.bars.update(series.ops);
    ops.span.textContent = off ? `Off: ${off}` : spanText(series.ops.length, series.everySecs, live);
    clients.value.textContent = f && f.clients !== null ? fmtCount(f.clients) : DASH;
    clients.bars.update(series.clients);
    clients.span.textContent = off ? `Off: ${off}` : spanText(series.clients.length, series.everySecs, live);
  }
  offs.push(ctx.api.on('figures', drawCharts), ctx.api.on('live', drawCharts));

  const sideNote = h('span', { class: 'muted small lv-note' });
  const panel = h('div', { class: `lv ${tab}` });
  view.append(
    h('div', { class: 'view-head lv-head' },
      h('h1', { class: 'title', id: 'server-title', text: 'Server' }),
      pills(TABS, tab, null, 'Server panels'),
      sideNote),
    h('div', { class: 'card tight strip' }, ops.el, clients.el),
    panel);
  drawCharts();

  /** Read a panel now and again every 5 s while the figures are live and the tab is open.
   *  Only the newest reading is drawn, and only for the node it was read for. */
  let seq = 0;
  const who = () => {
    const c = ctx.state().conn;
    return c ? `${c.name}|${c.docker}|${c.pickedNode}` : null;
  };
  function poll(read, draw, failed) {
    const once = async () => {
      seq += 1;
      const sent = { seq, who: who() };
      try {
        const data = await read();
        if (alive && replyFits(sent, { seq, who: who() })) draw(data);
      } catch (err) {
        if (alive && replyFits(sent, { seq, who: who() })) failed(err);
      }
    };
    once();
    timers.push(setInterval(() => {
      if (ctx.state().live && !document.hidden) once();
    }, 5000));
  }

  /** A list pane and its detail pane. Moving in the list (or a click) picks the row: the
   *  detail is drawn from the reading in hand, and nothing is read for it. */
  function listAndDetail({ id, label, detailId, detailLabel }) {
    const list = paneCard({ id, label, cls: 'lv-list' });
    const detail = paneCard({ id: detailId, label: detailLabel, cls: 'lv-detail' });
    fill(panel, list, detail);
    return {
      list,
      detail,
      listHead: list.querySelector('.pane-head'),
      listBody: list.querySelector('.pane-body'),
      listFoot: list.querySelector('.pane-foot'),
      head: detail.querySelector('.pane-head'),
      body: detail.querySelector('.pane-body'),
      foot: detail.querySelector('.pane-foot'),
      /** Calls `pick(key)` for the row the focus moves to, or that is clicked. */
      follow(pick) {
        const on = (e) => {
          const row = e.target && e.target.closest ? e.target.closest('[data-row]') : null;
          if (row && list.contains(row)) pick(row.dataset.row);
        };
        list.addEventListener('focusin', on);
        list.addEventListener('click', on);
      },
    };
  }
  /** Mark the picked row open (the one the detail shows), without drawing the list again. */
  const markOpen = (box, key) => {
    for (const r of box.querySelectorAll('[data-row]')) {
      const on = r.dataset.row === key;
      r.classList.toggle('on', on);
      if (r.tagName !== 'TR') r.setAttribute('aria-current', on ? 'true' : 'false');
    }
  };

  // ---------- Clients ----------
  function clientsPanel() {
    const ui = listAndDetail({ id: 'clients', label: 'Client groups', detailId: 'conns', detailLabel: 'Connections of the group' });
    let last = null;
    let picked = null;
    let drawnList = '';
    let drawnDetail = '';
    fill(ui.listHead, h('h2', { class: 'lbl', text: 'Clients' }));
    fill(ui.listBody, h('p', { class: 'muted pad', text: 'Reading the client list…' }));

    function drawList(m) {
      // Ages and idle times move on every reading: an unchanged list is left alone.
      const signature = JSON.stringify(m.sections);
      if (signature === drawnList) return;
      drawnList = signature;
      fill(ui.listHead, h('h2', { class: 'lbl', text: `Clients · ${plural(last.total, 'connection', 'connections')}` }));
      if (!m.sections.length) {
        fill(ui.listBody, emptyState('No client connections were listed.'));
        fill(ui.listFoot);
        return;
      }
      fill(ui.listBody, h('ul', { class: 'list' }, m.sections.map((s) => [
        h('li', { class: 'list-head' }, h('h3', { class: 'lbl', text: s.title })),
        s.rows.map((g) => h('li', null, h('button', {
          type: 'button', class: 'row row-group', 'data-row': g.key,
          'aria-label': `${g.name}: ${plural(g.connections, 'connection', 'connections')}${g.flagged ? ', some idle for an hour or more' : ''}. Show them.`,
        },
        h('span', { class: 'glyph', 'aria-hidden': 'true', text: g.flagged ? '▲' : '' }),
        h('span', { class: 'mono name clip', text: g.name, title: g.name }),
        h('span', { class: 'mono right', text: g.connections !== null ? fmtGrouped(g.connections) : DASH }),
        meter(barPercent(g.connections, s.most))))),
      ])));
      const idle = last.groups.filter((g) => g.by === 'address').reduce((sum, g) => sum + (g.idleConns || 0), 0);
      fill(ui.listFoot, idle ? notice(`${fmtGrouped(idle)} of them ${idle === 1 ? 'has' : 'have'} been idle for an hour or more. Idle clients hold memory and a connection each.`) : null);
    }

    function drawDetail(m) {
      const signature = JSON.stringify([m.key, m.gone, m.sel, m.members]);
      if (signature === drawnDetail) return;
      drawnDetail = signature;
      const g = m.sel;
      if (!g) {
        fill(ui.head, h('h2', { class: 'lbl', text: 'Connections' }));
        fill(ui.body, h('p', { class: 'muted pad', text: m.gone ? `No connections in ${m.name} any more: it is not in this reading.` : 'Pick a group to list its connections.' }));
        fill(ui.foot);
        return;
      }
      const facts = groupFacts(g);
      fill(ui.head, h('h2', { class: 'pane-title' },
        h('span', { class: 'lbl', text: g.by === 'address' ? 'Address' : 'Library' }),
        h('span', { class: 'mono key-title clip', text: g.name, title: g.name }),
        h('span', { class: 'muted small fig', text: plural(m.members.length, 'connection', 'connections') })));
      fill(ui.body,
        facts || g.flagged ? h('p', { class: 'muted small pad-x' }, g.flagged ? h('span', { class: 'glyph', text: `▲ ${g.idleConns ? `${fmtGrouped(g.idleConns)} ` : ''}idle for an hour or more · ` }) : null, facts) : null,
        m.members.length
          ? table([
            { label: 'Address', cls: 'mono nowrap', cell: (c) => c.addr || DASH },
            { label: 'Name', cls: 'mono', cell: (c) => (c.own ? 'redis-lens' : c.name || DASH) },
            { label: 'Library', cls: 'mono', cell: (c) => c.lib || DASH },
            { label: 'Age', cls: 'mono right nowrap', cell: (c) => fmtDuration(c.ageSecs) },
            { label: 'Idle', cls: 'mono right nowrap', cell: (c) => (c.idleSecs !== null && c.idleSecs >= 3600 ? [h('span', { class: 'glyph', 'aria-hidden': 'true', text: '▲ ' }), fmtDuration(c.idleSecs)] : fmtDuration(c.idleSecs)) },
            { label: 'Last command', cls: 'mono', cell: (c) => c.cmd || DASH },
            { label: 'DB', cls: 'mono right', cell: (c) => (c.db === null ? DASH : String(c.db)) },
            { label: 'Memory', cls: 'mono right', cell: (c) => fmtBytes(c.memory) },
          ], m.members, { caption: `Connections of ${g.name}`, cls: 'conns' })
          : emptyState('No connections in this group any more.'));
      fill(ui.foot, aboutThis('The connection redis-lens itself reads with is named redis-lens. User names are never shown.'));
    }

    function draw() {
      if (!last) return;
      if (last.off) {
        drawnList = '';
        drawnDetail = '';
        fill(panel, h('div', { class: 'card' }, notice(`Clients are off: ${last.off}.`)));
        return;
      }
      if (!panel.contains(ui.list)) fill(panel, ui.list, ui.detail);
      const m = clientsModel(last, picked);
      picked = m.key;
      drawList(m);
      markOpen(ui.list, m.gone ? null : m.key);
      drawDetail(m);
    }
    ui.follow((key) => {
      if (key === picked) return;
      picked = key;
      draw();
    });

    poll(() => ctx.api.getClients(), (data) => {
      last = data;
      draw();
    }, (err) => {
      drawnList = '';
      drawnDetail = '';
      last = null;
      fill(panel, h('div', { class: 'card' }, notice(errorText(err))));
    });
  }

  // ---------- Slow log ----------
  function slowPanel() {
    const ui = listAndDetail({ id: 'slowlog', label: 'Slow log', detailId: 'entry', detailLabel: 'The entry' });
    let last = null;
    let picked = null;
    let drawnList = '';
    let drawnDetail = '';
    fill(ui.listHead, h('h2', { class: 'lbl', text: 'Slowest commands' }));
    fill(ui.listBody, h('p', { class: 'muted pad', text: 'Reading the slow log…' }));

    function drawList(m) {
      const signature = JSON.stringify([m.rows, last.keysOff]);
      if (signature === drawnList) return;
      drawnList = signature;
      fill(ui.listHead, h('h2', { class: 'lbl', text: `Slowest commands · ${fmtGrouped(m.rows.length)}` }));
      fill(ui.listBody, m.rows.length
        ? table([
          { label: '', cls: 'flagcell', cell: (e) => (e.flag ? [h('span', { class: 'glyph', 'aria-hidden': 'true', text: '▲' }), h('span', { class: 'sr-only', text: 'flagged' })] : '') },
          { label: 'Time (UTC)', cls: 'mono muted nowrap', cell: (e) => fmtClock(e.at) },
          { label: 'Took', cls: 'mono right nowrap', cell: (e) => fmtTook(e.micros) },
          { label: 'Command', cls: 'mono nowrap', cell: (e) => e.name || DASH },
          { label: 'Keys', cls: 'mono muted clipcell', cell: (e) => h('span', { class: 'clip block', text: e.keys.length ? `${e.keys.join(' ')}${e.moreKeys ? ` +${e.moreKeys}` : ''}` : '' }) },
          { label: 'Client', cls: 'mono muted clipcell', cell: (e) => h('span', { class: 'clip block', text: e.client || DASH }) },
        ], m.rows, { caption: 'Slow log entries, newest first', cls: 'slow', rowKey: (e) => e.key })
        : emptyState('The slow log is empty.'));
      fill(ui.listFoot,
        last.keysOff ? notice(`Key names are missing: ${last.keysOff}.`) : null,
        aboutThis('Arguments are shown with credentials masked, on screen only.'));
    }

    function drawDetail(m) {
      const signature = JSON.stringify([m.key, m.gone, m.sel]);
      if (signature === drawnDetail) return;
      drawnDetail = signature;
      const e = m.sel;
      if (!e) {
        fill(ui.head, h('h2', { class: 'lbl', text: 'Entry' }));
        fill(ui.body, h('p', { class: 'muted pad', text: m.gone ? 'That entry is not in the slow log any more.' : 'Pick an entry to see it whole.' }));
        return;
      }
      fill(ui.head, h('h2', { class: 'lbl', text: 'Entry' }), h('span', { class: 'mono strong', text: fmtTook(e.micros) }));
      fill(ui.body, h('div', { class: 'stack pad' },
        h('dl', { class: 'facts single' }, slowDetail(e).map(([k, v]) => h('div', null, h('dt', { text: k }), h('dd', { class: 'mono wrap-any', text: v }))),
          h('div', null, h('dt', { text: 'Keys' }), h('dd', null, e.keys.length
            ? [e.keys.map((k) => (slowKeyOpens(k)
              ? h('button', { type: 'button', class: 'linkish mono wrap-any', text: k, title: `Open ${k} on Browse`, onclick: () => ctx.openKey({ id: hexOfText(k), name: k }) })
              : h('span', { class: 'mono wrap-any block', text: k }))),
            e.moreKeys ? h('span', { class: 'muted small', text: `+${e.moreKeys} more` }) : null]
            : h('span', { class: 'muted', text: last.keysOff ? `missing: ${last.keysOff}` : 'none' })))),
        e.note ? notice(e.note) : null));
    }

    function draw() {
      if (!last) return;
      if (last.off) {
        drawnList = '';
        drawnDetail = '';
        sideNote.textContent = '';
        fill(panel, h('div', { class: 'card' }, notice(`Slow log is off: ${last.off}.`)));
        return;
      }
      if (!panel.contains(ui.list)) fill(panel, ui.list, ui.detail);
      sideNote.textContent = slowlogSettingsText(last.settings, last.settingsOff);
      const m = slowModel(last, picked);
      picked = m.key;
      drawList(m);
      markOpen(ui.list, m.gone ? null : m.key);
      drawDetail(m);
    }
    ui.follow((key) => {
      if (key === picked) return;
      picked = key;
      draw();
    });

    poll(() => ctx.api.getSlowlog(), (data) => {
      last = data;
      draw();
    }, (err) => {
      drawnList = '';
      drawnDetail = '';
      last = null;
      fill(panel, h('div', { class: 'card' }, notice(errorText(err))));
    });
  }

  // ---------- Monitor ----------
  // The log takes the height. Each line is drawn whole (it wraps), so no line has more than
  // fits: Monitor keeps one pane.
  function monitorPanel() {
    const button = h('button', { type: 'button', class: 'btn primary' });
    const status = h('p', { class: 'muted small', role: 'status' });
    const log = h('div', { class: 'box log fill', tabindex: '0', role: 'log', 'aria-label': 'Commands seen by MONITOR' });
    let busy = false;
    let shown = '';

    function draw(force = false) {
      const left = monitorLeft(mon);
      const limits = ctx.state().monitorLimits;
      button.textContent = mon.running ? `Stop (${left} s left)` : `Watch commands for ${limits.maxSecs} seconds`;
      button.disabled = busy;
      const count = plural(mon.kept, 'line', 'lines');
      if (mon.running) status.textContent = `running · ${count} · ${left} s left · UTC`;
      else if (mon.stopped) status.textContent = `${mon.stopped} ${count} kept, in memory only.`;
      else status.textContent = '';
      const signature = `${mon.run}:${mon.running}:${mon.kept}:${mon.lines.length}`;
      if (force || signature !== shown) {
        shown = signature;
        if (!mon.lines.length) fillLog(log, h('p', { class: 'muted', text: mon.running ? 'Waiting for the first command…' : 'Nothing recorded yet.' }));
        else {
          fillLog(log, mon.lines.slice(-MONITOR_SHOWN).map((l) => h('div', { class: 'line' },
            h('span', { class: 't', text: `${l.at === null ? DASH : fmtClock(l.at, true)}  ${l.db === null ? '' : `db${l.db}  `}${l.client || ''}  ` }),
            h('span', { text: l.cut ? `${l.command} … ${fmtGrouped(l.cut)} more arguments not kept` : l.command }))));
        }
      }
    }

    button.addEventListener('click', async () => {
      const stopping = mon.running;
      busy = true;
      draw();
      try {
        if (stopping) await ctx.api.stopMonitor();
        else {
          const started = await ctx.api.startMonitor();
          // An event may have started showing this run already; then it knows more.
          if (mon.run !== started.run) monitorBegin(mon, started);
        }
      } catch (err) {
        ctx.notify(errorText(err), err.cancelled ? 'info' : 'error');
      }
      busy = false;
      if (alive) draw(true);
    });

    fill(panel, h('section', { class: 'card pane lv-one', 'aria-label': 'Monitor' },
      h('div', { class: 'pane-head' },
        h('div', { class: 'toolbar' }, button, status),
        h('span', { class: 'muted small', text: 'MONITOR slows the server, so it stops by itself.' })),
      h('div', { class: 'pane-body log-wrap' }, log),
      h('div', { class: 'pane-foot' }, aboutThis(
        'MONITOR shows every command the server runs. The server asks before it starts.',
        `Lines are shown, never saved. The last ${fmtGrouped(MONITOR_SHOWN)} are on screen.`))));
    offs.push(ctx.api.on('monitor', () => draw()), ctx.api.on('state', () => draw()));
    timers.push(setInterval(() => {
      if (mon.running) draw();
    }, 500));
    draw(true);
  }

  if (tab === 'clients') clientsPanel();
  else if (tab === 'slowlog') slowPanel();
  else monitorPanel();

  return () => {
    alive = false;
    unfill();
    offs.forEach((off) => off());
    timers.forEach((t) => clearInterval(t));
  };
}
