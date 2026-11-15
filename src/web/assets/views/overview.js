// Overview: the first screen after a connect. Live figures on top, then what the server
// says about itself. A connect reads no key, and neither does anything here.
//
// The two cards that need an analysis of the keys ("Things to look at" beyond what the
// server reports, and "Memory by key pattern") say that analysis arrives with Browse.
// Nothing is made up: a figure the server did not give shows as "—".

import {
  h, fill, DASH, fmtBytes, fmtCount, fmtDuration, fmtPercent, fmtRate, fmtTook, fmtClock, fmtGrouped, plural, barPercent,
  barChart, meter, setVar, table, notice, emptyState, connectFirst, nodePicker, errorText,
} from '../ui.js';

const NEXT = 'Analysis arrives with Browse in the next update.';

/** Set by server.js (which imports this module): keeps why the slow log's settings are
 *  missing, since the server says so only on the first reading of a connection. */
let rememberSlowlog = (conn, data) => data;
export function setSlowlogMemory(fn) {
  rememberSlowlog = fn;
}

/** What the page read once for this connection, to say what stands out: the slow log and
 *  the client list (as the TUI reads them once on connect). Kept per connection. */
const once = { key: null, slowlog: null, clients: null, reading: null };

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

/** Under the Keys tile: `31% have a TTL`. A missing part is left out. */
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

/** What stands out in what the server reports on its own, no key read: big keys the
 *  server counted, the slowest slow-log entry (its command name and duration only, never
 *  its arguments), and many idle connections from one address (10 or more, each idle for
 *  an hour or more). */
export function serverFindings({ bigKeys = null, slowlog = null, clients = null } = {}) {
  const out = [];
  if (typeof bigKeys === 'number' && bigKeys > 0) {
    out.push({ text: `${plural(bigKeys, 'big key', 'big keys')}, counted by the server (INFO keysizes).`, where: null });
  }
  if (slowlog && !slowlog.off && slowlog.entries.length) {
    const slowest = slowlog.entries.reduce((a, b) => ((b.micros ?? 0) > (a.micros ?? 0) ? b : a));
    if (slowest.name && slowest.micros !== null) {
      const at = slowest.at === null ? '' : ` at ${fmtClock(slowest.at).slice(0, 5)} UTC`;
      out.push({ text: `${slowest.name.toUpperCase()} took ${fmtTook(slowest.micros)}${at}.`, where: 'slowlog' });
    }
  }
  if (clients && !clients.off) {
    const idle = new Map();
    for (const c of clients.connections) {
      if (c.own || c.idleSecs === null || c.idleSecs < 3600) continue;
      const ip = c.addr.replace(/:\d+$/, '');
      idle.set(ip, (idle.get(ip) || 0) + 1);
    }
    const worst = Array.from(idle.entries()).sort((a, b) => b[1] - a[1])[0];
    if (worst && worst[1] >= 10) out.push({ text: `${fmtGrouped(worst[1])} connections from ${worst[0]} idle for over an hour.`, where: 'clients' });
  }
  return out;
}

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

export function mount(root, ctx) {
  const state = ctx.state();
  const gate = connectFirst(state);
  const view = h('section', { class: 'view', 'aria-labelledby': 'overview-title' });
  root.append(view);
  if (gate) {
    view.append(h('h1', { class: 'title', id: 'overview-title', text: 'Overview' }), gate);
    return () => {};
  }

  const conn = state.conn;
  let alive = true;
  let data = null;
  let shardLive = new Map();

  const subtitle = h('p', { class: 'mono subtitle' });
  const head = h('div', { class: 'view-head' },
    h('div', null, h('h1', { class: 'title', id: 'overview-title', text: 'Overview' }), subtitle),
    h('div', { class: 'actions' },
      nodePicker(conn, (index) => ctx.pickNode(index)),
      h('span', { class: 'muted', text: NEXT }),
      h('button', { type: 'button', class: 'btn', text: 'Analyze 10,000 keys', disabled: true }),
      h('button', { type: 'button', class: 'btn primary', text: 'Analyze 100,000 keys', disabled: true })));

  const tile = (label) => {
    const value = h('span', { class: 'tile-value', text: DASH });
    const sub = h('span', { class: 'tile-sub', text: ' ' });
    const el = h('div', { class: 'card tile' }, h('span', { class: 'lbl', text: label }), value, sub);
    return { el, value, sub };
  };
  const memory = tile('Memory');
  const memoryMeter = meter(0, 'thin');
  memoryMeter.hidden = true;
  memory.el.append(memoryMeter);
  const keys = tile('Keys');
  const ops = tile('Commands / sec');
  const opsBars = barChart({ height: 36 });
  ops.el.append(opsBars.el);
  const clients = tile('Clients');
  const clientsBars = barChart({ height: 36 });
  clients.el.append(clientsBars.el);
  const tiles = h('div', { class: 'tiles' }, memory.el, keys.el, ops.el, clients.el);

  const liveNote = h('div');
  const facts = h('dl', { class: 'facts' });
  const factsCard = h('div', { class: 'card stack' }, h('h2', { class: 'lbl', text: 'Server' }), liveNote, facts);
  const shardsBox = h('div');
  const findings = h('div', { class: 'card col' }, h('h2', { class: 'lbl', text: 'Things to look at' }), h('p', { class: 'muted', text: 'Reading what the server reports…' }));
  const patterns = h('div', { class: 'card col' });
  const offBox = h('div');

  view.append(head, tiles, factsCard, shardsBox, h('div', { class: 'cols stretch' }, findings, patterns), offBox);

  function drawSubtitle() {
    const sv = data ? data.server : {};
    subtitle.textContent = [
      [conn.address, conn.tls ? 'TLS' : null].filter(Boolean).join(' · '),
      [sv.kind || conn.server, sv.version || conn.version].filter(Boolean).join(' '),
      sv.topology || conn.topology,
      conn.target ? `reading ${conn.target}` : null,
    ].filter(Boolean).join(' · ') || DASH;
  }

  function drawLive() {
    const s = ctx.state();
    const f = ctx.figures() || (data && data.figures);
    const series = ctx.series();
    const used = f && f.usedMemory !== null ? f.usedMemory : data ? data.memory.used : null;
    const max = f && f.maxMemory ? f.maxMemory : data ? data.memory.max : null;
    memory.value.textContent = fmtBytes(used);
    memory.sub.textContent = data || f ? memorySub(max, (f && f.policy) || (data && data.server.policy) || conn.policy) : ' ';
    memoryMeter.hidden = !(max && used !== null);
    if (max && used !== null) setVar(memoryMeter.firstElementChild, '--w', `${barPercent(used, max)}%`);

    const total = f && f.keys !== null ? f.keys : data ? data.keys.total : null;
    keys.value.textContent = fmtCount(total);
    keys.sub.textContent = data ? keysSub(data.keys, conn.db) : ' ';

    ops.value.textContent = fmtRate(f ? f.opsPerSec ?? f.opsNow : null);
    ops.sub.textContent = spanText(series.ops.length, series.everySecs, s.live);
    opsBars.update(series.ops);
    clients.value.textContent = f && f.clients !== null ? fmtCount(f.clients) : DASH;
    clients.sub.textContent = spanText(series.clients.length, series.everySecs, s.live);
    clientsBars.update(series.clients);

    const off = ctx.figuresOff();
    fill(liveNote, off ? notice(`The live figures are off: ${off}.`) : null);
    drawFacts(f);
  }

  function drawFacts(f) {
    const sv = data ? data.server : {};
    const k = data ? data.keys : {};
    const fact = (label, value) => h('div', null, h('dt', { text: label }), h('dd', { text: value || DASH }));
    const num = (v, text) => (typeof v === 'number' ? text(v) : null);
    const role = f && f.role ? `${f.role}${typeof f.replicas === 'number' ? ` · ${plural(f.replicas, 'replica', 'replicas')}` : ''}${f.linkUp === false ? ' · ✗ link down' : ''}` : null;
    fill(facts,
      fact('Server', [sv.kind || conn.server, sv.version || conn.version].filter(Boolean).join(' ')),
      fact('Topology', sv.topology || conn.topology),
      fact('Uptime', num(f && f.uptimeSecs, fmtDuration)),
      fact('Role', role),
      fact('Persistence', persistenceText(f)),
      fact('Data', num(k.dataBytes, fmtBytes)),
      fact('Overhead', num(k.overheadBytes, fmtBytes)),
      fact('Hit rate', num(f && f.hitRate, fmtPercent)),
      fact('Evicted / sec', num(f && f.evictedPerSec, fmtRate)),
      fact('Expired / sec', num(f && f.expiredPerSec, fmtRate)),
      fact('Blocked clients', num(f && f.blocked, fmtGrouped)),
      fact('Network in', num(f && f.netIn, (v) => `${fmtBytes(Math.round(v))}/s`)),
      fact('Network out', num(f && f.netOut, (v) => `${fmtBytes(Math.round(v))}/s`)),
      fact('CPU user', num(f && f.cpuUser, fmtPercent)),
      fact('CPU system', num(f && f.cpuSys, fmtPercent)),
      fact('Fragmentation', num(f && f.fragmentation, (v) => v.toFixed(2))),
      conn.via ? fact('Sentinel', conn.via) : null);
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
    fill(shardsBox, h('div', { class: 'card' },
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

  function drawFindings() {
    const where = { slowlog: ['#/server/slowlog', 'Server › Slow log'], clients: ['#/server/clients', 'Server › Clients'] };
    const found = serverFindings({ bigKeys: data ? data.keys.bigKeys : null, slowlog: once.slowlog, clients: once.clients });
    const rows = found.map((f) => {
      const [href, label] = where[f.where] || [null, null];
      const inner = [
        h('span', { class: 'glyph', 'aria-hidden': 'true', text: '▲' }),
        h('span', null, h('span', { text: f.text }), label ? h('span', { class: 'sub', text: label }) : null),
        h('span', { class: 'chev', 'aria-hidden': 'true', text: href ? '›' : '' }),
      ];
      return h('li', null, href ? h('a', { class: 'row row-finding', href }, inner) : h('div', { class: 'row row-finding' }, inner));
    });
    const reasons = [
      once.slowlog && once.slowlog.off ? `Slow log is off: ${once.slowlog.off}.` : null,
      once.clients && once.clients.off ? `Idle clients are off: ${once.clients.off}.` : null,
    ].filter(Boolean);
    fill(findings,
      h('h2', { class: 'lbl', text: 'Things to look at' }),
      rows.length ? h('ul', { class: 'list' }, rows) : h('p', { text: 'Nothing stands out in what the server reports.' }),
      reasons.map((r) => h('p', { class: 'muted small', text: r })),
      h('p', { class: 'muted small', text: `From what the server reports: no keys were read. What a sample of the keys finds comes later. ${NEXT}` }));
  }

  function drawPatterns() {
    const last = data && data.lastAnalysis;
    fill(patterns,
      h('h2', { class: 'lbl', text: 'Memory by key pattern' }),
      emptyState(`This needs a sample of the keys. ${NEXT}`),
      last ? h('p', { class: 'muted small', text: `Last analysis: ${analysisWhen(last.at)}, ${fmtGrouped(last.sampledKeys)} keys (a saved snapshot).` }) : null);
  }

  function drawOff() {
    const lines = offLines(data, conn);
    const checks = data ? data.checks : [];
    if (!lines.length && !checks.length) {
      fill(offBox);
      return;
    }
    const glyph = { allowed: '✓', denied: '✗', skipped: '–' };
    const word = { allowed: 'allowed', denied: 'refused', skipped: 'not checked' };
    fill(offBox, h('div', { class: 'card stack' },
      h('h2', { class: 'lbl', text: lines.length ? 'Turned off, and why' : 'Permissions' }),
      lines.length ? h('div', { class: 'notes' }, lines.map((l) => notice(l))) : notice('Every check passed.', 'ok'),
      checks.length ? h('details', { class: 'advanced' },
        h('summary', { text: `Every permission check (${checks.length})` }),
        h('ul', { class: 'list' }, checks.map((c) => h('li', { class: 'row row-check' },
          h('span', { 'aria-hidden': 'true', class: c.status === 'allowed' ? 'glyph good' : c.status === 'denied' ? 'glyph' : 'muted', text: glyph[c.status] }),
          h('span', null,
            h('span', { class: 'mono name', text: c.name }),
            h('span', { class: 'sr-only', text: ` ${word[c.status]}` }),
            c.why ? h('span', { class: 'sub', text: c.why }) : null))))) : null));
  }

  async function load() {
    try {
      const next = await ctx.api.getOverview();
      if (!alive) return;
      data = next;
      drawSubtitle();
      drawLive();
      drawShards();
      drawFindings();
      drawPatterns();
      drawOff();
      readOnce();
    } catch (err) {
      if (!alive) return;
      fill(findings, h('h2', { class: 'lbl', text: 'Things to look at' }), notice(errorText(err)));
    }
  }

  /** Once per connection and node, where the permission check allowed it: the slow log
   *  and the client list, for "Things to look at". Nothing polls them here. */
  async function readOnce() {
    if (ctx.state().liveState !== 'ready') return;
    const key = [conn.name, conn.docker, conn.connectedAt, conn.pickedNode].join('|');
    if (once.key === key) {
      // Already read, or being read by an earlier draw of this screen.
      await once.reading;
      if (alive) drawFindings();
      return;
    }
    once.key = key;
    once.slowlog = null;
    once.clients = null;
    const allowed = (name) => !data.checks.some((c) => c.name === name && c.status !== 'allowed');
    const read = async (name, get, slot) => {
      if (!allowed(name)) {
        const check = data.checks.find((c) => c.name === name);
        once[slot] = { off: (check && check.why) || 'the permission check turned it off', entries: [], connections: [] };
        return;
      }
      try {
        once[slot] = await get();
      } catch {
        once[slot] = null;
      }
    };
    const slow = async () => rememberSlowlog(`${conn.name}|${conn.docker}|${conn.connectedAt}`, await ctx.api.getSlowlog());
    once.reading = Promise.all([read('SLOWLOG', slow, 'slowlog'), read('CLIENT LIST', ctx.api.getClients, 'clients')]);
    await once.reading;
    if (alive) drawFindings();
  }

  const redraw = (f) => {
    if (f && f.shards) {
      shardLive = new Map(f.shards.map((s) => [s.id, s]));
      drawShards();
    }
    drawLive();
  };
  const offs = [ctx.api.on('figures', redraw), ctx.api.on('live', () => drawLive())];
  drawSubtitle();
  drawLive();
  drawPatterns();
  load();

  return () => {
    alive = false;
    offs.forEach((off) => off());
  };
}
