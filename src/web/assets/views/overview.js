// Overview: the first screen after a connect. Live figures on top, then what the server
// says about itself. A connect reads no key, and neither does anything here.
//
// The two cards that need an analysis of the keys ("Things to look at" beyond what the
// server reports, and "Memory by key pattern") say that analysis arrives with Browse.
// Nothing is made up: a figure the server did not give shows as "—".

import {
  h, fill, DASH, fmtBytes, fmtCount, fmtDuration, fmtPercent, fmtRate, barPercent,
  barChart, meter, setVar, table, notice, emptyState, connectFirst, nodePicker, errorText,
} from '../ui.js';

const NEXT = 'Analysis arrives with Browse in the next update.';

/** The line under a live tile: how far back the bars go, or that they are paused. */
export function spanText(points, everySecs, live) {
  if (!live) return 'Live figures paused';
  if (!points || !everySecs) return 'Live';
  return `Last ${points * everySecs} seconds`;
}

/** Under the Memory tile: `of 24G · allkeys-lfu`, `no limit · noeviction`. */
export function memorySub(max, policy) {
  const parts = [max ? `of ${fmtBytes(max)}` : 'no limit set'];
  if (policy) parts.push(policy);
  return parts.join(' · ');
}

/** Under the Keys tile: `31% have a TTL · database 0`. A missing part is left out. */
export function keysSub(keys) {
  const parts = [];
  if (typeof keys.ttlShare === 'number') parts.push(`${fmtPercent(keys.ttlShare)} have a TTL`);
  if (typeof keys.db === 'number') parts.push(`database ${keys.db}`);
  return parts.join(' · ') || DASH;
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
  let timer = null;

  const subtitle = h('p', { class: 'mono subtitle', text: conn.address || DASH });
  const head = h('div', { class: 'view-head' },
    h('div', null, h('h1', { class: 'title', id: 'overview-title', text: 'Overview' }), subtitle),
    h('div', { class: 'actions' },
      nodePicker(conn, (id) => ctx.pickNode(id)),
      h('span', { class: 'muted', text: NEXT }),
      h('button', { type: 'button', class: 'btn', text: 'Analyze 10,000 keys', disabled: true }),
      h('button', { type: 'button', class: 'btn primary', text: 'Analyze 100,000 keys', disabled: true })));

  // Tiles
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

  const facts = h('dl', { class: 'facts' });
  const factsCard = h('div', { class: 'card' }, h('h2', { class: 'lbl', text: 'Server' }), facts);
  const shardsBox = h('div');
  const findings = h('div', { class: 'card col' }, h('h2', { class: 'lbl', text: 'Things to look at' }), h('p', { class: 'muted', text: 'Reading what the server reports…' }));
  const patterns = h('div', { class: 'card col' },
    h('h2', { class: 'lbl', text: 'Memory by key pattern' }),
    emptyState(`This needs a sample of the keys. ${NEXT}`));
  const offBox = h('div');

  view.append(head, tiles, factsCard, shardsBox, h('div', { class: 'cols stretch' }, findings, patterns), offBox);

  function drawLive() {
    const s = ctx.state();
    const f = ctx.figures() || (data && data.figures);
    const series = ctx.series();
    const used = f && f.usedMemory !== null ? f.usedMemory : data ? data.memory.used : null;
    const max = f && f.maxMemory ? f.maxMemory : data ? data.memory.max : null;
    memory.value.textContent = fmtBytes(used);
    memory.sub.textContent = data || f ? memorySub(max, data && data.server.policy) : ' ';
    memoryMeter.hidden = !(max && used !== null);
    if (max && used !== null) setVar(memoryMeter.firstElementChild, '--w', `${barPercent(used, max)}%`);

    ops.value.textContent = fmtRate(f ? f.opsPerSec : null);
    ops.sub.textContent = spanText(series.ops.length, series.everySecs, s.live);
    opsBars.update(series.ops);
    clients.value.textContent = f && f.clients !== null ? fmtCount(f.clients) : DASH;
    clients.sub.textContent = spanText(series.clients.length, series.everySecs, s.live);
    clientsBars.update(series.clients);
    drawFacts(f);
  }

  function drawFacts(f) {
    const sv = data ? data.server : {};
    const fact = (label, value) => h('div', null, h('dt', { text: label }), h('dd', { text: value || DASH }));
    const role = sv.role ? `${sv.role}${typeof sv.replicas === 'number' ? ` · ${sv.replicas} ${sv.replicas === 1 ? 'replica' : 'replicas'}` : ''}` : null;
    fill(facts,
      fact('Server', [sv.kind || conn.server, sv.version || conn.version].filter(Boolean).join(' ')),
      fact('Topology', sv.topology || conn.topology),
      fact('Uptime', typeof sv.uptimeSecs === 'number' ? fmtDuration(sv.uptimeSecs) : null),
      fact('Role', role),
      fact('Persistence', sv.persistence),
      fact('Data', data && data.keys.dataBytes !== null ? fmtBytes(data.keys.dataBytes) : null),
      fact('Overhead', data && data.keys.overheadBytes !== null ? fmtBytes(data.keys.overheadBytes) : null),
      fact('Hit rate', f && f.hitRate !== null ? fmtPercent(f.hitRate) : null),
      fact('Evicted / sec', f && f.evictedPerSec !== null ? fmtRate(f.evictedPerSec) : null),
      fact('Expired / sec', f && f.expiredPerSec !== null ? fmtRate(f.expiredPerSec) : null),
      conn.target ? fact('Reading', conn.target) : null);
  }

  function drawShards() {
    if (!data || !data.shards.length) {
      fill(shardsBox);
      return;
    }
    const down = { 'not-connected': '✗ not connected', 'no-answer': '✗ no answer' };
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
            : h('button', { type: 'button', class: 'btn quiet', text: 'Show', 'aria-label': `Show node ${r.address || r.id}`, disabled: r.status === 'not-connected', onclick: () => ctx.pickNode(r.id) })),
        },
      ], data.shards.map((r) => ({ ...r, rowClass: r.picked ? 'on' : '' })), { caption: 'One row per shard', cls: 'shards' })));
  }

  function drawFindings() {
    const where = { slowlog: ['#/server/slowlog', 'Server › Slow log'], clients: ['#/server/clients', 'Server › Clients'] };
    const rows = data.findings.map((f) => {
      const [href, label] = where[f.where] || [null, null];
      const inner = [
        h('span', { class: 'glyph', 'aria-hidden': 'true', text: '▲' }),
        h('span', null, h('span', { text: f.text }), label ? h('span', { class: 'sub', text: label }) : null),
        h('span', { class: 'chev', 'aria-hidden': 'true', text: href ? '›' : '' }),
      ];
      return h('li', null, href ? h('a', { class: 'row row-finding', href }, inner) : h('div', { class: 'row row-finding' }, inner));
    });
    fill(findings,
      h('h2', { class: 'lbl', text: 'Things to look at' }),
      rows.length ? h('ul', { class: 'list' }, rows) : h('p', { text: 'Nothing stands out in what the server reports.' }),
      h('p', { class: 'muted small', text: `From what the server reports: no keys were read. What a sample of the keys finds comes later. ${NEXT}` }));
  }

  function drawOff() {
    const lines = [];
    const seen = new Set();
    for (const c of conn.checksOff) {
      const text = c.why || `${c.name} is off.`;
      if (!seen.has(text)) {
        seen.add(text);
        lines.push(text);
      }
    }
    for (const n of data ? data.notes : []) {
      if (!seen.has(n)) {
        seen.add(n);
        lines.push(n);
      }
    }
    if (conn.providerNotice && !seen.has(conn.providerNotice)) lines.push(conn.providerNotice);
    if (!lines.length) {
      fill(offBox);
      return;
    }
    fill(offBox, h('div', { class: 'card' },
      h('h2', { class: 'lbl', text: 'Turned off, and why' }),
      h('div', { class: 'notes' }, lines.map((l) => notice(l)))));
  }

  async function load() {
    try {
      const next = await ctx.api.getOverview();
      if (!alive) return;
      data = next;
      ctx.seedSeries(next.history);
      const sv = next.server;
      subtitle.textContent = [next.address || conn.address, [sv.kind, sv.version].filter(Boolean).join(' '), sv.topology].filter(Boolean).join(' · ') || DASH;
      keys.value.textContent = fmtCount(next.keys.total);
      keys.sub.textContent = keysSub(next.keys);
      drawLive();
      drawShards();
      drawFindings();
      drawOff();
    } catch (err) {
      if (!alive) return;
      if (!data) {
        fill(findings, h('h2', { class: 'lbl', text: 'Things to look at' }), notice(errorText(err)));
        drawOff();
      }
    }
  }

  const offFigures = ctx.api.on('figures', () => drawLive());
  const offState = ctx.api.on('state', () => drawLive());
  drawLive();
  drawOff();
  load();
  // On a cluster the shard rows come from the server's own slow poll: read them again
  // every 5 s while the figures are live. A standalone server needs no second read.
  if (conn.nodes.length >= 2) {
    timer = setInterval(() => {
      if (ctx.state().live && !document.hidden) load();
    }, 5000);
  }

  return () => {
    alive = false;
    offFigures();
    offState();
    if (timer) clearInterval(timer);
  };
}
