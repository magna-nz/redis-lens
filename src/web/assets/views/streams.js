// Browse › Streams: the streams an analysis found, as the TUI's Streams panel shows them.
// Each stream's length, groups and consumers (lag, pending, idle), why it is marked, its
// pending entries, and its entries newest first, a page at a time. The writes are the TUI's
// and no others: XACK an entry of the pending list shown, XAUTOCLAIM a group's entries for a
// consumer, XTRIM by MAXLEN or MINID. redis-lens builds each one and puts it through the one
// write gate; a question it asks comes back as the confirm dialog.
//
// Everything from the server is drawn with textContent, as redis-lens escaped it. A stream,
// group or consumer is named in a request by the hex id redis-lens gave it. What a form holds
// stays in the form, with autocomplete off; entries stay in this page's memory while it is
// open and nothing goes to storage, the console or an address.

import {
  h, fill, DASH, fmtBytes, fmtGrouped, plural, notice, emptyState, readOnlyNote, errorText, table,
} from '../ui.js';
import { streamWriteText } from '../api.js';

// ---------------------------------------------------------------------------------------
// Pure helpers (tested in tests/streams.test.js)
// ---------------------------------------------------------------------------------------

/** How often the list is read again while the panel is shown, as the TUI polls it. */
export const POLL_MS = 5000;

/** The TUI's trim note. */
export const APPROXIMATE = 'Approximate (~): the server may keep a few more.';

/** A count that may be unknown (lag without XINFO's entries-read, a refused size). */
export function countText(n) {
  return typeof n === 'number' ? fmtGrouped(n) : DASH;
}

/** The line about a group: consumers, pending, lag, last delivered. */
export function groupLine(g) {
  const parts = [plural(g.consumers, 'consumer', 'consumers'), `${fmtGrouped(g.pending)} pending`, `lag ${countText(g.lag)}${g.lagging ? ' ▲' : ''}`];
  if (g.lastDeliveredId) parts.push(`last delivered ${g.lastDeliveredId}`);
  return parts.join(' · ');
}

/** The claim form as the TUI fills it. From the group (`from` null): the most recently active
 *  consumer, 60000 ms, 100 entries. From a consumer's row or a pending entry's (`from` is
 *  `{ name, idleMs }`, that consumer and its idle time): the most recently active other
 *  consumer, and that idle time in whole seconds. A name that can't be typed as it is is left
 *  blank. */
export function claimDefaults(g, from = null) {
  const count = String(g && g.claim ? g.claim.count : 100);
  if (!from) {
    const to = g && g.claim && g.claim.to ? g.claim.to.copy ?? '' : '';
    return { to, minIdleMs: String(g && g.claim ? g.claim.minIdleMs : 60000), count };
  }
  let best = null;
  for (const c of (g && g.readers) || []) {
    if (from.name && c.name.id === from.name.id) continue;
    if (best === null || (c.idleMs ?? Infinity) < (best.idleMs ?? Infinity)) best = c;
  }
  const idle = typeof from.idleMs === 'number' ? Math.floor(from.idleMs / 1000) * 1000 : 60000;
  return { to: best ? best.name.copy ?? '' : '', minIdleMs: String(idle), count };
}

/** The line above the claim form, as the TUI's: where the entries come from. */
export function claimFrom(from) {
  return from && from.name ? `From ${from.name.name}: entries idle at least min idle go to the new consumer.` : 'Entries idle at least min idle go to the new consumer.';
}

/** What is read again after a write on stream `keyId`, as the TUI's `reread_after`: the
 *  pending list when one of that stream is shown (it reads the stream's row too), else the
 *  list at once, without asking again for what the server refused. */
export function rereadAfter(pending, keyId) {
  return pending && pending.key === keyId && pending.group ? 'pending' : 'list';
}

/** Whether the 5 s poll reads the list now: live figures on and ready, the tab shown, a list
 *  that is not off, and the entries not open (the TUI stops the XINFO poll while they are). */
export function pollDue({ live, liveState, hidden, list, entriesOpen }) {
  return !!(live && liveState === 'ready' && !hidden && list && !list.off && !entriesOpen);
}

/** The list's subtitle: how many streams, on how many nodes. */
export function listSubtitle(list) {
  if (!list || list.off) return 'Streams the analysis found.';
  if (list.status === 'no_report') return 'Streams are listed after an analysis.';
  if (list.status === 'none') return 'The analysis found no streams.';
  const nodes = new Set(list.rows.map((r) => r.endpoint).filter(Boolean)).size;
  const head = `${plural(list.shown, 'stream', 'streams')}${list.found > list.shown ? ` of ${fmtGrouped(list.found)}` : ''} from the analysis`;
  return nodes > 1 ? `${head}, each read on its node (${nodes} nodes).` : `${head}.`;
}

/** An entry's fields on one line: `kind=paid · order=770100`. */
export function entryText(item) {
  const fields = Array.isArray(item && item.fields) ? item.fields : [];
  return fields.map((f) => `${f.name}=${f.value}`).join(' · ');
}

/** The row of the list to show: the one picked, else the first one. */
export function pickRow(list, id) {
  if (!list || !list.rows.length) return null;
  return list.rows.find((r) => r.key.id === id) || list.rows[0];
}

// ---------------------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------------------

// What the panel keeps while the page is open (as the TUI keeps its selection).
const store = { who: null, list: null, error: null, picked: null, pending: null, entries: null, claim: null };

function follow(state) {
  const c = state.conn;
  const who = c ? [c.name, c.docker, state.analysis && state.analysis.run].join('|') : null;
  if (who !== store.who) {
    store.who = who;
    store.list = null;
    store.error = null;
    store.picked = null;
    store.pending = null;
    store.entries = null;
    store.claim = null;
  }
}

export function mount(root, ctx, { head, body: area }) {
  const state = ctx.state();
  follow(state);
  let alive = true;
  const timers = [];
  const subtitle = head.subtitle;
  const readAgain = h('button', { type: 'button', class: 'btn', text: 'Read again', onclick: () => load(true) });
  head.actions.append(readAgain);
  const box = h('div', { class: 'stack-lg', 'aria-live': 'polite' });
  area.append(box);

  let seq = 0;
  /** Read the list. `quiet` (the poll): a form being filled in, or a pending list open, stay
   *  as they are; only the list is drawn again. `now` (after a write): read at once, still
   *  leaving out what the server refused. */
  async function load(refresh = false, quiet = false, now = false) {
    const mine = (seq += 1);
    if (!quiet) readAgain.disabled = true;
    try {
      store.list = await ctx.api.getStreams({ refresh, now });
      store.error = null;
    } catch (err) {
      // The poll while the live connection opens again (an analysis): the list stays.
      if (quiet && err.notLive && store.list) {
        if (alive && mine === seq) readAgain.disabled = false;
        return;
      }
      store.error = errorText(err);
    }
    if (!alive || mine !== seq) return;
    readAgain.disabled = false;
    if (quiet && busyDetail()) drawList();
    else draw();
  }

  /** Whether the details are in use: a form open or focused, or a read under way. */
  function busyDetail() {
    const d = box.querySelector('.stream-detail');
    if (!d) return false;
    if (d.querySelector('details[open]')) return true;
    if (d.contains(document.activeElement)) return true;
    return !!((store.pending && store.pending.reading) || (store.entries && store.entries.reading));
  }

  function drawList() {
    const l = store.list;
    const old = box.querySelector('.stream-list');
    if (!old || !l || l.off || l.status !== 'ready') {
      draw();
      return;
    }
    const focusedId = old.contains(document.activeElement) ? document.activeElement.id : null;
    subtitle.textContent = listSubtitle(l);
    old.replaceWith(listCard(l, pickRow(l, store.picked)));
    if (focusedId) {
      const again = document.getElementById(focusedId);
      if (again) again.focus();
    }
  }

  const writesOff = () => (store.list && store.list.writes) || null;
  const nodeName = (r) => r.endpoint || (typeof r.node === 'number' ? `node ${r.node + 1}` : DASH);

  function draw() {
    const l = store.list;
    subtitle.textContent = listSubtitle(l);
    if (store.error && !l) {
      fill(box, h('div', { class: 'card' }, notice(store.error)));
      return;
    }
    if (!l) {
      fill(box, h('div', { class: 'card' }, h('p', { class: 'muted', text: 'Reading the streams…' })));
      return;
    }
    if (l.off) {
      fill(box, h('div', { class: 'card stack' }, notice(l.off),
        h('p', { class: 'muted small', text: 'Read again asks the server again, after a change to the user\'s ACL.' })));
      return;
    }
    if (l.status === 'no_report') {
      const a = ctx.state().analysis;
      fill(box, h('div', { class: 'card stack' },
        h('h2', { class: 'lbl', text: a.running || l.analysisRunning ? 'Analyzing' : 'No analysis yet' }),
        emptyState(a.running || l.analysisRunning ? 'The list fills in once the analysis is done.' : l.text,
          a.running || l.analysisRunning ? null : h('button', { type: 'button', class: 'btn primary', text: 'Analyze', onclick: () => ctx.analyze('quick') }))));
      return;
    }
    if (l.status === 'none') {
      fill(box, h('div', { class: 'card stack' }, emptyState(l.text || 'The analysis found no streams.',
        h('button', { type: 'button', class: 'btn', text: 'Deep analysis', onclick: () => ctx.analyze('deep') })),
      l.hint ? h('p', { class: 'muted small', text: l.hint }) : null));
      return;
    }
    const row = pickRow(l, store.picked);
    if (row) store.picked = row.key.id;
    const focusedId = box.contains(document.activeElement) ? document.activeElement.id : null;
    fill(box,
      store.error ? notice(store.error) : null,
      writesOff() ? readOnlyNote(writesOff()) : null,
      h('div', { class: 'browse streams' }, listCard(l, row), row ? detailCard(row) : null));
    if (focusedId) {
      const again = document.getElementById(focusedId);
      if (again) again.focus();
    }
  }

  function listCard(l, picked) {
    const pick = (r) => {
      const hadEntries = !!store.entries;
      store.picked = r.key.id;
      store.pending = null;
      store.entries = null;
      store.claim = null;
      draw();
      if (hadEntries) entriesClosed();
    };
    return h('div', { class: 'card stack browse-col stream-list' },
      h('div', { class: 'card-head' }, h('h2', { class: 'lbl', text: `Streams · ${l.rows.length}` })),
      l.note ? h('p', { class: 'muted small', text: l.note }) : null,
      table([
        {
          label: 'Stream', cls: 'keycell', cell: (r) => h('button', {
            type: 'button', class: 'linkish mono clip', id: `stream-${r.key.id}`, text: r.key.name, 'aria-pressed': picked && r.key.id === picked.key.id ? 'true' : 'false',
            title: r.gone ? r.text : `Show ${r.key.name}`, onclick: () => pick(r),
          }),
        },
        { label: 'Length', cls: 'mono right nowrap', cell: (r) => (r.gone ? DASH : countText(r.length)) },
        { label: 'Groups', cls: 'mono right nowrap', cell: (r) => (r.gone ? DASH : countText(r.groupCount)) },
        { label: 'Lag', cls: 'mono right nowrap', cell: (r) => (r.gone ? DASH : countText(r.lag)) },
        { label: 'Pending', cls: 'mono right nowrap', cell: (r) => (r.gone ? DASH : countText(r.pending)) },
        { label: 'Oldest', cls: 'mono nowrap', cell: (r) => (r.gone ? DASH : r.oldest || DASH) },
        { label: 'Verdict', cls: 'nowrap', cell: (r) => (r.gone ? h('span', { class: 'badge', text: 'gone' }) : h('span', { class: r.verdict === 'ok' ? 'badge' : 'badge flagged', text: r.verdict === 'ok' ? 'ok' : `▲ ${r.verdict}` })) },
        l.cluster ? { label: 'Node', cls: 'mono nowrap', cell: (r) => nodeName(r) } : null,
      ].filter(Boolean), l.rows.map((r) => ({ ...r, rowClass: picked && r.key.id === picked.key.id ? 'on' : '' })), { caption: 'The streams of the analysis', cls: 'streams' }));
  }

  // ---------- the stream's details ----------

  function detailCard(r) {
    if (r.gone) {
      return h('div', { class: 'card stack browse-col stream-detail' },
        h('h2', { class: 'key-title mono wrap-any', text: r.key.name }), notice(r.text || 'The key is gone, or no longer a stream.'));
    }
    const facts = [
      ['Length', `${countText(r.length)} entries`],
      ['Memory', r.bytes === null ? DASH : fmtBytes(r.bytes)],
      ['First ID', r.firstId || DASH],
      ['Last ID', r.lastId || DASH],
      ['Oldest entry', r.oldest ? `${r.oldest} old` : DASH],
      ['Entries added', countText(r.entriesAdded)],
      r.endpoint || typeof r.node === 'number' ? ['Node', nodeName(r)] : null,
    ].filter(Boolean);
    return h('div', { class: 'card stack browse-col stream-detail' },
      h('div', { class: 'value-head' }, h('h2', { class: 'key-title mono wrap-any', text: r.key.name }),
        h('button', { type: 'button', class: 'btn quiet', text: 'Open on Keys', title: `Open ${r.key.name} on Browse › Keys`, onclick: () => ctx.openKey(r.key) })),
      h('dl', { class: 'facts' }, facts.map(([k, v]) => h('div', null, h('dt', { text: k }), h('dd', { class: 'mono', text: v })))),
      r.verdicts.length ? h('div', { class: 'notes' }, r.verdicts.map((v) => notice(v.text))) : h('p', { class: 'note ok' }, h('span', { class: 'glyph', 'aria-hidden': 'true', text: '✓' }), h('span', { text: 'Nothing stands out.' })),
      r.leftOut ? h('p', { class: 'muted small', text: r.leftOut }) : null,
      h('h3', { class: 'lbl sub-lbl', text: `Consumer groups · ${r.groups ? r.groups.length : DASH}` }),
      r.groups && r.groups.length ? r.groups.map((g) => groupBox(r, g)) : h('p', { class: 'muted', text: r.groupsText || 'No consumer groups: nothing reads this stream.' }),
      store.pending && store.pending.key === r.key.id ? pendingBox(r) : null,
      writesOff() ? null : trimForm(r),
      entriesBox(r));
  }

  function groupBox(r, g) {
    const readers = g.readers;
    return h('section', { class: 'item-actions stack-sm', 'aria-label': `Group ${g.name.name}` },
      h('div', { class: 'toolbar' },
        h('h4', { class: 'mono strong wrap-any', text: g.name.name }),
        h('button', { type: 'button', class: 'btn quiet', text: 'Pending entries', 'aria-label': `List the pending entries of ${g.name.name}`, onclick: () => listPending(r, g, null) })),
      h('p', { class: 'muted small', text: groupLine(g) }),
      readers === null ? h('p', { class: 'muted small', text: 'XINFO CONSUMERS is not available for this user.' })
        : readers.length ? table([
          { label: 'Consumer', cls: 'keycell mono', cell: (c) => `${c.stuck ? '▲ ' : ''}${c.name.name}` },
          { label: 'Pending', cls: 'mono right nowrap', cell: (c) => fmtGrouped(c.pending) },
          { label: 'Idle', cls: 'mono nowrap', cell: (c) => c.idle || DASH },
          { label: 'Inactive', cls: 'mono nowrap', cell: (c) => c.inactive || DASH },
          {
            label: '', cls: 'nowrap', cell: (c) => [
              c.pending ? h('button', {
                type: 'button', class: 'btn quiet', text: 'Pending', 'aria-label': `List the pending entries of ${c.name.name}`, onclick: () => listPending(r, g, c),
              }) : null,
              writesOff() || !g.pending ? null : h('button', {
                type: 'button', class: 'btn quiet', text: 'Claim', 'aria-label': `Claim pending entries of ${g.name.name} from ${c.name.name} (XAUTOCLAIM)`, onclick: () => openClaim(r, g, { name: c.name, idleMs: c.idleMs }),
              }),
            ],
          },
        ], readers, { caption: `The consumers of ${g.name.name}` })
          : h('p', { class: 'muted small', text: 'No consumers.' }),
      readers && readers.some((c) => c.stuck) ? h('p', { class: 'muted small', text: '▲ stuck: holds pending entries and idle 30 minutes or more.' }) : null,
      writesOff() || !g.pending ? null : claimForm(r, g));
  }

  // ---------- pending entries and XACK ----------

  async function listPending(r, g, consumer) {
    store.pending = { key: r.key.id, group: g.name, consumer: consumer ? consumer.name : null, reading: true, reply: null, error: null };
    draw();
    try {
      const reply = await ctx.api.getPending({ key: r.key.id, group: g.name.id, consumer: consumer ? consumer.name.id : null });
      if (!alive || !store.pending || store.pending.key !== r.key.id) return;
      if (reply.off) {
        store.list = { ...store.list, off: reply.off };
        store.pending = null;
      } else {
        store.pending.reply = reply;
        if (reply.row) replaceRow(reply.row);
      }
    } catch (err) {
      if (store.pending) store.pending.error = errorText(err);
    }
    if (store.pending) store.pending.reading = false;
    if (alive) draw();
  }

  function replaceRow(row) {
    if (!store.list || !store.list.rows) return;
    const at = store.list.rows.findIndex((x) => x.key.id === row.key.id);
    if (at >= 0) store.list.rows[at] = row;
  }

  function pendingBox(r) {
    const s = store.pending;
    const group = r.groups ? r.groups.find((x) => x.name.id === s.group.id) || null : null;
    const who = s.consumer ? ` of ${s.consumer.name}` : '';
    const head = h('div', { class: 'toolbar' },
      h('h3', { class: 'lbl' }, 'Pending in ', h('span', { class: 'as-is mono', text: `${s.group.name}${who}` }), s.reply && s.reply.pending ? ` · ${s.reply.pending.entries.length}` : ''),
      h('button', { type: 'button', class: 'btn quiet', text: 'Close', onclick: () => { store.pending = null; draw(); } }));
    if (s.reading) return h('section', { class: 'stack-sm' }, head, h('p', { class: 'muted', text: 'Reading the pending entries…' }));
    if (s.error) return h('section', { class: 'stack-sm' }, head, notice(s.error));
    const p = s.reply && s.reply.pending;
    if (!p) return h('section', { class: 'stack-sm' }, head, notice((s.reply && s.reply.text) || 'The key is gone, or no longer a stream.'));
    return h('section', { class: 'stack-sm', 'aria-label': 'Pending entries' }, head,
      p.error ? notice(p.error) : null,
      p.entries.length ? table([
        { label: 'Entry ID', cls: 'mono nowrap', cell: (e) => e.id },
        { label: 'Consumer', cls: 'mono', cell: (e) => e.consumer.name },
        { label: 'Idle', cls: 'mono nowrap', cell: (e) => e.idle || DASH },
        { label: 'Deliveries', cls: 'mono right nowrap', cell: (e) => fmtGrouped(e.deliveries) },
        writesOff() ? null : {
          label: '', cls: 'nowrap', cell: (e) => [
            h('button', {
              type: 'button', class: 'btn quiet', text: 'Ack', 'aria-label': `Acknowledge ${e.id} (XACK)`, onclick: (ev) => ack(r, p, e, ev.currentTarget),
            }),
            group && group.pending ? h('button', {
              type: 'button', class: 'btn quiet', text: 'Claim', 'aria-label': `Claim pending entries of ${group.name.name} from ${e.consumer.name} (XAUTOCLAIM)`, onclick: () => openClaim(r, group, { name: e.consumer, idleMs: e.idleMs }),
            }) : null,
          ],
        },
      ].filter(Boolean), p.entries, { caption: 'Pending entries' }) : h('p', { class: 'muted', text: 'Nothing is pending.' }),
      typeof p.max === 'number' && p.entries.length >= p.max ? h('p', { class: 'muted small', text: `The first ${fmtGrouped(p.max)} pending entries.` }) : null);
  }

  async function write(button, run, what, extra) {
    if (button) button.disabled = true;
    try {
      const w = await run();
      ctx.notify(streamWriteText(what, w, extra), 'ok');
      return true;
    } catch (err) {
      ctx.notify(errorText(err), err.cancelled ? 'info' : 'error');
      return false;
    } finally {
      if (button && button.isConnected) button.disabled = false;
    }
  }

  async function ack(r, p, e, button) {
    const done = await write(button, () => ctx.api.ackEntry({ key: r.key.id, group: p.group.id, id: e.id }), 'ack', { id: e.id });
    if (!alive) return;
    if (done) await afterWrite(r);
  }

  /** After a write, as the TUI's `reread_after`: the pending list shown of that stream read
   *  again (with the stream's row), else the list read at once. Neither asks again for what
   *  the server refused. */
  async function afterWrite(r) {
    const s = store.pending;
    if (rereadAfter(s, r.key.id) === 'pending') {
      await listPending(r, { name: s.group }, s.consumer ? { name: s.consumer } : null);
      return;
    }
    await load(false, false, true);
  }

  // ---------- the forms: XAUTOCLAIM and XTRIM ----------

  let formSeq = 0;
  function input(label, value, opts = {}) {
    formSeq += 1;
    const id = `sf-${formSeq}`;
    const el = h('input', { class: 'field mono', type: 'text', id, autocomplete: 'off', spellcheck: false, autocapitalize: 'off', value, inputmode: opts.numeric ? 'numeric' : null });
    return { el, row: h('div', { class: 'form-row' }, h('label', { htmlFor: id, text: label }), el) };
  }

  /** Opens the group's claim form filled from a consumer's or a pending entry's row, as the
   *  TUI's `c` there. */
  function openClaim(r, g, from) {
    store.claim = { key: r.key.id, group: g.name.id, from };
    draw();
    const box = document.getElementById(`claim-to-${g.name.id}`);
    if (box) box.focus();
  }

  function claimForm(r, g) {
    const asked = store.claim && store.claim.key === r.key.id && store.claim.group === g.name.id ? store.claim : null;
    const from = asked ? asked.from : null;
    const d = claimDefaults(g, from);
    const to = input('To consumer', d.to);
    to.el.id = `claim-to-${g.name.id}`;
    to.row.querySelector('label').htmlFor = to.el.id;
    const idle = input('Min idle ms', d.minIdleMs, { numeric: true });
    const count = input('Count', d.count, { numeric: true });
    const error = h('p', { class: 'form-error', role: 'alert' });
    const button = h('button', { type: 'submit', class: 'btn', text: 'Claim' });
    return h('details', {
      class: 'advanced', open: !!asked,
      ontoggle: (ev) => {
        // Closed: the next one opens from the group again.
        if (!ev.currentTarget.open && store.claim && store.claim.group === g.name.id) store.claim = null;
      },
    },
      h('summary', { text: 'Claim pending entries (XAUTOCLAIM)' }),
      h('form', {
        class: 'form', autocomplete: 'off',
        onsubmit: async (ev) => {
          ev.preventDefault();
          error.textContent = '';
          button.disabled = true;
          try {
            const w = await ctx.api.claimEntries({ key: r.key.id, group: g.name.id, to: to.el.value, minIdleMs: idle.el.value, count: count.el.value });
            ctx.notify(streamWriteText('claim', w, { to: to.el.value.trim() }), 'ok');
            store.claim = null;
            if (alive) await afterWrite(r);
          } catch (err) {
            if (err.cancelled) ctx.notify(errorText(err), 'info');
            else error.textContent = errorText(err);
          } finally {
            if (button.isConnected) button.disabled = false;
          }
        },
      },
      h('p', { class: 'muted small', text: claimFrom(from) }),
      h('p', { class: 'muted small', text: `Moves entries of ${g.name.name} idle at least that long to the consumer, from any consumer of the group. Sends XAUTOCLAIM ${r.key.name} ${g.name.name} <consumer> <min idle> 0-0 COUNT <count>.` }),
      h('div', { class: 'form-grid' }, to.row, idle.row, count.row), error, h('div', { class: 'row-actions' }, button)));
  }

  function trimForm(r) {
    formSeq += 1;
    const byId = `sf-${formSeq}`;
    const by = h('select', { class: 'field', id: byId }, h('option', { value: 'MAXLEN', text: 'MAXLEN: keep the newest entries' }), h('option', { value: 'MINID', text: 'MINID: drop entries older than an ID' }));
    const value = input('Value', '');
    const error = h('p', { class: 'form-error', role: 'alert' });
    const button = h('button', { type: 'submit', class: 'btn danger', text: 'Trim' });
    return h('details', { class: 'advanced' },
      h('summary', { text: 'Trim the stream (XTRIM)' }),
      h('form', {
        class: 'form', autocomplete: 'off',
        onsubmit: async (ev) => {
          ev.preventDefault();
          error.textContent = '';
          button.disabled = true;
          try {
            const w = await ctx.api.trimStream({ key: r.key.id, by: by.value, value: value.el.value });
            ctx.notify(streamWriteText('trim', w), 'ok');
            value.el.value = '';
            if (alive) await afterWrite(r);
          } catch (err) {
            if (err.cancelled) ctx.notify(errorText(err), 'info');
            else error.textContent = errorText(err);
          } finally {
            if (button.isConnected) button.disabled = false;
          }
        },
      },
      h('div', { class: 'form-grid' }, h('div', { class: 'form-row' }, h('label', { htmlFor: byId, text: 'Trim by' }), by), value.row),
      h('p', { class: 'muted small', text: APPROXIMATE }), error, h('div', { class: 'row-actions' }, button)));
  }

  // ---------- the entries, newest first ----------

  async function openEntries(r, more = false) {
    const e = store.entries;
    store.entries = { key: r.key.id, value: more && e ? e.value : null, reading: true, error: null };
    draw();
    try {
      const v = more && e && e.value ? await ctx.api.moreValue(e.value.view) : await ctx.api.openValue({ key: r.key.id, type: 'stream' });
      if (!alive || !store.entries || store.entries.key !== r.key.id) return;
      store.entries.value = v;
      if (v.loading) {
        setTimeout(() => {
          if (alive && store.entries && store.entries.value === v) openEntries(r, true);
        }, 400);
      }
    } catch (err) {
      if (store.entries) store.entries.error = errorText(err);
    }
    if (store.entries) store.entries.reading = false;
    if (alive) draw();
  }

  function entriesBox(r) {
    const e = store.entries && store.entries.key === r.key.id ? store.entries : null;
    const v = e && e.value;
    const head = h('div', { class: 'toolbar' },
      h('h3', { class: 'lbl', text: v ? `Entries, newest first · ${fmtGrouped(v.items.length)} of ${countText(v.length ?? r.length)}` : 'Entries, newest first' }),
      h('button', { type: 'button', class: 'btn quiet', text: v ? 'Read again' : 'Show entries', disabled: e && e.reading, onclick: () => openEntries(r) }),
      e ? h('button', { type: 'button', class: 'btn quiet', text: 'Close', 'aria-label': 'Close the entries', onclick: closeEntries }) : null);
    if (!e) return h('section', { class: 'stack-sm' }, head, h('p', { class: 'muted small', text: 'XREVRANGE, a page at a time. The list is not read again while the entries are open.' }));
    if (e.error) return h('section', { class: 'stack-sm' }, head, notice(e.error));
    if (!v) return h('section', { class: 'stack-sm' }, head, h('p', { class: 'muted', text: 'Reading the entries…' }));
    if (v.gone || v.kind !== 'stream') return h('section', { class: 'stack-sm' }, head, notice(v.error || 'The key is gone, or no longer a stream.'));
    return h('section', { class: 'stack-sm', 'aria-label': 'Entries' }, head,
      v.error ? notice(v.error) : null,
      v.items.length ? h('ul', { class: 'list items' }, v.items.map((it) => h('li', { class: 'row row-item cols-2 entry' },
        h('span', { class: 'mono small nowrap', text: it.name ?? '' }), h('span', { class: 'mono small wrap-any', text: entryText(it) })))) : h('p', { class: 'muted', text: 'The stream has no entries.' }),
      v.notice ? h('p', { class: 'muted small', text: v.notice }) : null,
      h('p', { class: 'muted small' }, 'Each entry on one line; ', h('button', { type: 'button', class: 'linkish', text: 'Open on Keys', title: `Open ${r.key.name} on Browse › Keys`, onclick: () => ctx.openKey(r.key) }), ' shows an entry\'s fields whole, in a format.'),
      v.more && !(typeof (v.length ?? r.length) === 'number' && v.items.length >= (v.length ?? r.length)) ? h('button', { type: 'button', class: 'btn', text: 'Show more', disabled: e.reading, onclick: () => openEntries(r, true) }) : null);
  }

  // ---------- following the session ----------

  // A new run, or an analysis that ends, lists the streams again.
  // The list is read once the live connection is ready again (an analysis opens it again).
  let lastStatus = state.analysis.status;
  let again = false;
  const offState = ctx.api.on('state', (s) => {
    if (!alive) return;
    const before = store.who;
    follow(s);
    const ended = s.analysis.status !== lastStatus && (s.analysis.status === 'done' || s.analysis.status === 'none');
    lastStatus = s.analysis.status;
    if (store.who !== before || ended) again = true;
    if (again && s.liveState === 'ready' && s.connected) {
      again = false;
      load();
    }
  });
  /** The entries closed: back on the list, it reads at once when a poll came due meanwhile,
   *  as the TUI's does. */
  function closeEntries() {
    store.entries = null;
    draw();
    entriesClosed();
  }
  let missed = false;
  function entriesClosed() {
    if (!missed || !alive) return;
    missed = false;
    const now = ctx.state();
    if (pollDue({ live: now.live, liveState: now.liveState, hidden: document.hidden, list: store.list, entriesOpen: !!store.entries })) load(false, true);
  }

  // The TUI polls the panel while it is shown; paused with the live figures, while the tab is
  // hidden, and while the entries are open.
  timers.push(setInterval(() => {
    if (!alive) return;
    const now = ctx.state();
    const at = { live: now.live, liveState: now.liveState, hidden: document.hidden, list: store.list };
    if (pollDue({ ...at, entriesOpen: !!store.entries })) load(false, true);
    else if (store.entries && pollDue({ ...at, entriesOpen: false })) missed = true;
  }, POLL_MS));

  draw();
  load();

  return () => {
    alive = false;
    offState();
    timers.forEach((t) => clearInterval(t));
  };
}
