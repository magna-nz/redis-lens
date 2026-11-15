// Server: two live charts, and the panels Clients · Slow log · Monitor.
//
// Monitor is time-boxed: it always asks first (the confirm flow), counts down, and stops
// by itself; Stop ends it early. Its lines stay in memory for the session and are never
// saved. Slow log and Monitor lines arrive already masked by redis-lens.

import {
  h, fill, DASH, fmtBytes, fmtCount, fmtDuration, fmtRate, fmtTook, fmtClock, fmtGrouped,
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

/** The Monitor run, kept while other views are shown. */
const mon = { running: false, endsAt: 0, lines: [], kept: 0, node: null, stopped: null, conn: null };

/** Take in a Monitor reading (the reply to Start, or a `monitor` event). */
export function monitorApply(m, update, now = Date.now()) {
  if (update.running && !m.running) {
    // A new run starts with an empty list.
    m.lines = [];
    m.kept = 0;
  }
  if (update.lines && update.lines.length) {
    m.lines = m.lines.concat(update.lines);
    if (m.lines.length > MONITOR_CAP) m.lines = m.lines.slice(m.lines.length - MONITOR_CAP);
  }
  m.kept = typeof update.kept === 'number' ? update.kept : m.kept + (update.lines ? update.lines.length : 0);
  if (update.node) m.node = update.node;
  if (update.running) {
    m.running = true;
    m.stopped = null;
    if (typeof update.left === 'number') m.endsAt = now + update.left * 1000;
  } else {
    m.running = false;
    m.stopped = update.stopped || 'MONITOR stopped.';
  }
  return m;
}

/** Seconds left on the countdown, never below zero. */
export function monitorLeft(m, now = Date.now()) {
  return m.running ? Math.max(0, Math.ceil((m.endsAt - now) / 1000)) : 0;
}

function monitorReset(connName) {
  mon.running = false;
  mon.endsAt = 0;
  mon.lines = [];
  mon.kept = 0;
  mon.node = null;
  mon.stopped = null;
  mon.conn = connName;
}

/** Called once at boot: keep the Monitor lines whatever view is on screen. */
export function init(api) {
  api.on('monitor', (update) => monitorApply(mon, update));
  api.on('state', (s) => {
    const name = s.conn ? s.conn.name : null;
    if (name !== mon.conn) monitorReset(name);
  });
}

export function mount(root, ctx, route) {
  const state = ctx.state();
  const gate = connectFirst(state);
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
    ops.value.textContent = fmtRate(f ? f.opsPerSec : null);
    ops.bars.update(series.ops);
    ops.span.textContent = spanText(series.ops.length, series.everySecs, live);
    clients.value.textContent = f && f.clients !== null ? fmtCount(f.clients) : DASH;
    clients.bars.update(series.clients);
    clients.span.textContent = spanText(series.clients.length, series.everySecs, live);
  }
  offs.push(ctx.api.on('figures', drawCharts), ctx.api.on('state', drawCharts));

  const panel = h('div');
  view.append(
    h('div', { class: 'view-head' },
      h('h1', { class: 'title', id: 'server-title', text: 'Server' }),
      h('div', { class: 'actions' }, nodePicker(conn, (id) => ctx.pickNode(id)))),
    h('div', { class: 'cols stretch' }, ops.el, clients.el),
    pills(TABS, tab, null, 'Server panels'),
    panel);
  drawCharts();

  /** Whether a panel's reading differs from the one on screen: an unchanged list is left
   *  alone, so the page does not jump under the reader every few seconds. */
  let drawn = '';
  function changed(data) {
    const signature = JSON.stringify(data);
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
    fill(panel, h('div', { class: 'card' }, emptyState('Reading the client list…')));
    poll(async () => {
      try {
        const data = await ctx.api.getClients();
        if (alive && changed(data)) fill(panel, drawClients(data));
      } catch (err) {
        drawn = '';
        if (alive) fill(panel, h('div', { class: 'card' }, notice(errorText(err))));
      }
    });
  }

  function drawClients(data) {
    if (data.off) return h('div', { class: 'card' }, notice(data.off));
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
          return h('li', { class: 'row row-group' },
            h('span', null,
              h('span', { class: 'mono name', text: g.name }),
              h('span', { class: 'sub small' }, g.flagged ? h('span', { class: 'glyph', text: '▲ idle for an hour or more · ' }) : null, facts)),
            h('span', { class: 'mono right', text: g.connections !== null ? fmtGrouped(g.connections) : DASH }),
            meter(barPercent(g.connections, most)));
        })),
      ];
    };
    const lib = section('library', 'Clients by library');
    const addr = section('address', 'Clients by address');
    return h('div', { class: 'card stack' },
      lib || addr ? [lib, addr] : emptyState('No client connections were listed.'),
      data.notes.length ? h('div', { class: 'notes' }, data.notes.map((n) => notice(n))) : null);
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
    if (data.off) return h('div', { class: 'card' }, notice(data.off));
    return h('div', { class: 'card stack' },
      h('div', null,
        h('h2', { class: 'lbl', text: 'Slowest commands' }),
        h('p', { class: 'muted small', text: data.threshold || 'The threshold and length are unknown.' })),
      data.entries.length
        ? table([
          { label: 'Time (UTC)', cls: 'mono muted nowrap', cell: (e) => fmtClock(e.at) },
          { label: 'Took', cls: 'mono right nowrap', cell: (e) => fmtTook(e.micros) },
          {
            label: 'Command',
            cell: (e) => {
              const facts = [e.client ? `from ${e.client}` : null, e.note].filter(Boolean).join(' · ');
              return [
                h('span', { class: 'mono wrap-any', text: e.command || DASH }),
                facts ? h('span', { class: 'sub small muted', text: facts }) : null,
              ];
            },
          },
        ], data.entries, { caption: 'Slow log entries, newest first', cls: 'slow' })
        : emptyState('The slow log is empty.'),
      data.notes.length ? h('div', { class: 'notes' }, data.notes.map((n) => notice(n))) : null);
  }

  // ---------- Monitor ----------
  function monitorPanel() {
    const seconds = conn.monitorSeconds || 30;
    const button = h('button', { type: 'button', class: 'btn primary' });
    const status = h('p', { class: 'muted small', role: 'status' });
    const log = h('div', { class: 'box log', tabindex: '0', role: 'log', 'aria-label': 'Commands seen by MONITOR' });
    let busy = false;
    let shown = '';

    function draw(force = false) {
      const left = monitorLeft(mon);
      // The server says when it stopped. If that word never comes, stop counting anyway.
      if (mon.running && left === 0 && Date.now() - mon.endsAt > 4000) monitorApply(mon, { running: false, stopped: 'MONITOR stopped by itself.' });
      button.textContent = mon.running ? `Stop (${left} s left)` : `Watch commands for ${seconds} seconds`;
      button.disabled = busy || (!mon.running && !!conn.monitorOff);
      const count = `${fmtGrouped(mon.kept)} ${mon.kept === 1 ? 'line' : 'lines'}`;
      if (mon.running) status.textContent = `running · ${count} · ${left} s left · UTC${mon.node ? ` · node ${mon.node}` : ''}`;
      else if (mon.stopped) status.textContent = `${mon.stopped} ${count} kept, in memory only.`;
      else status.textContent = '';
      const signature = `${mon.kept}:${mon.lines.length}`;
      if (force || signature !== shown) {
        shown = signature;
        if (!mon.lines.length) fillLog(log, h('p', { class: 'muted', text: 'Nothing recorded yet.' }));
        else {
          fillLog(log, mon.lines.slice(-MONITOR_SHOWN).map((l) => h('div', { class: 'line' },
            h('span', { class: 't', text: `${fmtClock(l.at)}  ${l.client || ''}  ` }),
            h('span', { text: l.command }))));
        }
      }
    }

    button.addEventListener('click', async () => {
      busy = true;
      draw();
      try {
        if (mon.running) await ctx.api.stopMonitor();
        else {
          const started = await ctx.api.startMonitor();
          // An event may have started the run already; then it knows more than this reply.
          if (!mon.running) monitorApply(mon, { ...started, running: true, left: started.left ?? seconds });
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
      conn.monitorOff ? notice(conn.monitorOff) : null,
      status,
      log,
      h('p', { class: 'muted small', text: `Lines are shown, never saved. The last ${fmtGrouped(MONITOR_SHOWN)} are on screen.` })));
    offs.push(ctx.api.on('monitor', () => draw()));
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
