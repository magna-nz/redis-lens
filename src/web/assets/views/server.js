// Server: two live charts, and the panels Clients · Slow log · Monitor.
//
// Monitor is time-boxed: the server always asks first (the confirm dialog), it counts down,
// and stops by itself; Stop ends it early. Its lines stay in memory for the session and are never saved. Slow log and
// Monitor lines arrive already masked by redis-lens.

import {
  h, fill, refill, DASH, fmtBytes, fmtCount, fmtDuration, fmtRate, fmtTook, fmtClock, fmtGrouped, plural,
  barPercent, barChart, meter, table, pills, notice, emptyState, connectFirst, nodePicker,
  fillLog, errorText,
} from '../ui.js';
import { spanText } from './overview.js';

const TABS = [
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

export function mount(root, ctx, route) {
  const state = ctx.state();
  const gate = connectFirst(state, { needsLive: true });
  const view = h('section', { class: 'view', 'aria-labelledby': 'server-title' });
  root.append(view);
  if (gate) {
    view.append(h('h1', { class: 'title', id: 'server-title', text: 'Server' }), gate);
    return () => {};
  }

  const conn = state.conn;
  const tab = TABS.some((t) => t.id === route.sub) ? route.sub : 'clients';
  let alive = true;
  const timers = [];
  const offs = [];

  // The two charts
  const chart = (label) => {
    const value = h('span', { class: 'chart-value', text: DASH });
    const bars = barChart({ height: 96, tall: true });
    const span = h('span', { class: 'small muted', text: 'Live' });
    const el = h('div', { class: 'card chart' }, h('div', { class: 'chart-head' }, h('h2', { class: 'lbl', text: label }), value), bars.el, span);
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

  const panel = h('div');
  view.append(
    h('div', { class: 'view-head' },
      h('h1', { class: 'title', id: 'server-title', text: 'Server' }),
      h('div', { class: 'actions' }, nodePicker(conn, (index) => ctx.pickNode(index)))),
    h('div', { class: 'cols stretch' }, ops.el, clients.el),
    pills(TABS, tab, null, 'Server panels'),
    panel);
  drawCharts();

  /** Whether a panel's reading differs from the one on screen: an unchanged list is left
   *  alone, so the page does not jump under the reader every few seconds. */
  let drawn = '';
  function changed(data, extra = '') {
    const signature = JSON.stringify(data) + extra;
    if (signature === drawn) return false;
    drawn = signature;
    return true;
  }

  /** Read a panel now and again every 5 s while the figures are live and the tab is open. */
  function poll(read) {
    read();
    timers.push(setInterval(() => {
      if (ctx.state().live && !document.hidden) read();
    }, 5000));
  }

  // ---------- Clients ----------
  function clientsPanel() {
    let open = null;
    let last = null;
    const draw = () => {
      // Ages and idle times move on every reading: keep the keyboard focus where it was.
      if (last && changed(last, JSON.stringify(open))) refill(panel, drawClients(last));
    };
    const pick = (group) => {
      open = group ? { by: group.by, name: group.name } : null;
      draw();
      const target = panel.querySelector(group ? '[data-back]' : '[data-group]');
      if (target) target.focus();
    };

    function drawClients(data) {
      if (data.off) return h('div', { class: 'card' }, notice(`Clients are off: ${data.off}.`));
      const shown = open && data.groups.find((g) => g.by === open.by && g.name === open.name);
      if (open && shown) return drawGroup(data, shown);
      const section = (by, title) => {
        const groups = data.groups.filter((g) => g.by === by);
        if (!groups.length) return null;
        const most = Math.max(...groups.map((g) => g.connections || 0));
        return [
          h('h2', { class: 'lbl', text: title }),
          h('ul', { class: 'list' }, groups.map((g) => {
            const facts = [
              g.oldestSecs !== null ? `oldest ${fmtDuration(g.oldestSecs)}` : null,
              g.idleSecs !== null ? `longest idle ${fmtDuration(g.idleSecs)}` : null,
              g.lastCommand ? `mostly ${g.lastCommand}` : null,
              g.memory !== null ? fmtBytes(g.memory) : null,
            ].filter(Boolean).join(' · ');
            const idle = g.flagged ? `▲ ${g.idleConns ? `${fmtGrouped(g.idleConns)} ` : ''}idle for an hour or more · ` : '';
            return h('li', null, h('button', { type: 'button', class: 'row row-group', 'data-group': '', 'aria-label': `${g.name}: ${plural(g.connections, 'connection', 'connections')}. List them.`, onclick: () => pick(g) },
              h('span', null,
                h('span', { class: 'mono name', text: g.name }),
                h('span', { class: 'sub small' }, idle ? h('span', { class: 'glyph', text: idle }) : null, facts)),
              h('span', { class: 'mono right', text: g.connections !== null ? fmtGrouped(g.connections) : DASH }),
              meter(barPercent(g.connections, most))));
          })),
        ];
      };
      const lib = section('library', 'Clients by library');
      const addr = section('address', 'Clients by address');
      const idle = data.groups.filter((g) => g.by === 'address').reduce((sum, g) => sum + (g.idleConns || 0), 0);
      return h('div', { class: 'card stack' },
        lib || addr ? [lib, addr] : emptyState('No client connections were listed.'),
        h('p', { class: 'muted small', text: `${plural(data.total, 'connection', 'connections')}. Pick a group to list its connections.` }),
        idle ? notice(`${fmtGrouped(idle)} of them ${idle === 1 ? 'has' : 'have'} been idle for an hour or more. Idle clients hold memory and a connection each.`) : null);
    }

    function drawGroup(data, group) {
      const rows = group.members.map((i) => data.connections[i]).filter(Boolean);
      return h('div', { class: 'card stack' },
        h('div', { class: 'toolbar' },
          h('h2', { class: 'lbl', text: `${group.by === 'address' ? 'Address' : 'Library'} ${group.name} · ${plural(rows.length, 'connection', 'connections')}` }),
          h('button', { type: 'button', class: 'btn', text: 'Back to the groups', 'data-back': '', onclick: () => pick(null) })),
        rows.length
          ? table([
            { label: 'Address', cls: 'mono nowrap', cell: (c) => c.addr || DASH },
            { label: 'Name', cls: 'mono', cell: (c) => (c.own ? 'redis-lens' : c.name || DASH) },
            { label: 'Library', cls: 'mono', cell: (c) => c.lib || DASH },
            { label: 'Age', cls: 'mono right nowrap', cell: (c) => fmtDuration(c.ageSecs) },
            { label: 'Idle', cls: 'mono right nowrap', cell: (c) => `${c.idleSecs !== null && c.idleSecs >= 3600 ? '▲ ' : ''}${fmtDuration(c.idleSecs)}` },
            { label: 'Last command', cls: 'mono', cell: (c) => c.cmd || DASH },
            { label: 'DB', cls: 'mono right', cell: (c) => (c.db === null ? DASH : String(c.db)) },
            { label: 'Memory', cls: 'mono right', cell: (c) => fmtBytes(c.memory) },
          ], rows, { caption: `Connections of ${group.name}`, cls: 'conns' })
          : emptyState('No connections in this group any more.'),
        h('p', { class: 'muted small', text: 'The connection redis-lens itself reads with is named redis-lens. User names are never shown.' }));
    }

    fill(panel, h('div', { class: 'card' }, emptyState('Reading the client list…')));
    poll(async () => {
      try {
        last = await ctx.api.getClients();
        if (alive) draw();
      } catch (err) {
        drawn = '';
        if (alive) fill(panel, h('div', { class: 'card' }, notice(errorText(err))));
      }
    });
  }

  // ---------- Slow log ----------
  function slowPanel() {
    fill(panel, h('div', { class: 'card' }, emptyState('Reading the slow log…')));
    poll(async () => {
      try {
        const data = await ctx.api.getSlowlog();
        if (alive && changed(data)) fill(panel, drawSlow(data));
      } catch (err) {
        drawn = '';
        if (alive) fill(panel, h('div', { class: 'card' }, notice(errorText(err))));
      }
    });
  }

  function drawSlow(data) {
    if (data.off) return h('div', { class: 'card' }, notice(`Slow log is off: ${data.off}.`));
    return h('div', { class: 'card stack' },
      h('div', null,
        h('h2', { class: 'lbl', text: `Slowest commands · ${fmtGrouped(data.entries.length)}` }),
        h('p', { class: 'muted small', text: slowlogSettingsText(data.settings, data.settingsOff) })),
      data.entries.length
        ? table([
          { label: 'Time (UTC)', cls: 'mono muted nowrap', cell: (e) => fmtClock(e.at) },
          { label: 'Took', cls: 'mono right nowrap', cell: (e) => fmtTook(e.micros) },
          {
            label: 'Command',
            cell: (e) => {
              const facts = slowEntryFacts(e);
              return [
                h('span', { class: 'mono wrap-any', text: e.command || DASH }),
                facts ? h('span', { class: 'sub small muted wrap-any', text: facts }) : null,
              ];
            },
          },
        ], data.entries, { caption: 'Slow log entries, newest first', cls: 'slow' })
        : emptyState('The slow log is empty.'),
      data.keysOff ? notice(`Key names are missing: ${data.keysOff}.`) : null,
      h('p', { class: 'muted small', text: 'Arguments are shown with credentials masked, on screen only.' }));
  }

  // ---------- Monitor ----------
  function monitorPanel() {
    const button = h('button', { type: 'button', class: 'btn primary' });
    const status = h('p', { class: 'muted small', role: 'status' });
    const log = h('div', { class: 'box log', tabindex: '0', role: 'log', 'aria-label': 'Commands seen by MONITOR' });
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

    fill(panel, h('div', { class: 'card stack' },
      h('div', { class: 'toolbar' },
        button,
        h('span', { class: 'muted grow', text: 'MONITOR shows every command the server runs. It slows the server, so it stops by itself.' })),
      status,
      log,
      h('p', { class: 'muted small', text: `Lines are shown, never saved. The last ${fmtGrouped(MONITOR_SHOWN)} are on screen.` })));
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
    offs.forEach((off) => off());
    timers.forEach((t) => clearInterval(t));
  };
}
