// Browse › Search: the indexes of Redis Search (built into Redis 8) or valkey-search, as the
// TUI's Search panel shows them. The list from FT._LIST, each index's FT.INFO, and FT.SEARCH,
// FT.EXPLAIN and FT.PROFILE as the TUI sends them (LIMIT 0 50, results a page of 10 at a
// time). FT.CREATE from a form and FT.DROPINDEX without DD (the documents stay), each put
// through the one write gate by redis-lens. On a cluster it reads the node picked. Off, in
// the TUI's words, when the module is not loaded or FT._LIST is refused.
//
// Everything from the server is drawn with textContent, as redis-lens escaped it. An index is
// named in a request by its hex id. A query stays in its box and this page's memory; the
// boxes have autocomplete off; nothing goes to storage, the console or an address.

import {
  h, fill, DASH, fmtBytes, fmtGrouped, fmtPercent, plural, notice, emptyState, readOnlyNote, errorText, table, copyText,
} from '../ui.js';

// ---------------------------------------------------------------------------------------
// Pure helpers (tested in tests/search.test.js)
// ---------------------------------------------------------------------------------------

/** The longest query redis-lens sends, as the TUI's query box takes it. */
export const MAX_QUERY = 1000;

/** The words of each command on its button and in its messages. */
export const COMMANDS = { search: 'FT.SEARCH', explain: 'FT.EXPLAIN', profile: 'FT.PROFILE' };

/** The line under the results: matches, how long, how many kept. */
export function resultsLine(r) {
  if (!r) return '';
  const parts = [r.count || plural(r.total, 'match', 'matches')];
  if (r.took) parts.push(r.took);
  if (r.kept < r.total) parts.push(`the first ${fmtGrouped(r.kept)} kept (LIMIT 0 ${r.max ?? r.kept})`);
  return parts.join(' · ');
}

/** The page's place in the results: `1–10 of 50`. */
export function pageText(r) {
  if (!r || !r.kept) return '';
  const last = Math.min(r.kept, r.from + r.docs.length);
  return `${fmtGrouped(r.from + 1)}–${fmtGrouped(last)} of ${fmtGrouped(r.kept)}`;
}

/** Where the previous and next page start, or null at an end. */
export function pageSteps(r) {
  if (!r) return { prev: null, next: null };
  const size = r.page || 10;
  return {
    prev: r.from > 0 ? Math.max(0, r.from - size) : null,
    next: r.from + size < r.kept ? r.from + size : null,
  };
}

/** Why a command's button is off: refused or missing (`cmdOff`), one running, or, for
 *  FT.EXPLAIN and FT.PROFILE, no query run yet. Null when it can be sent. */
export function commandOff(list, cmd) {
  if (!list || list.off) return 'The Search panel is off.';
  const off = list.cmdOff && list.cmdOff[cmd];
  if (off) return off;
  if (list.busy) return `${list.busy} is still running: wait for its answer.`;
  if (cmd !== 'search' && list.lastQuery === null) return `Run a query first: ${COMMANDS[cmd]} uses the query last run.`;
  return null;
}

/** What the query box starts from, as the TUI's: the query last run, when it can be typed
 *  as it is (`lastQueryCopy`); else empty. */
export function queryStart(list) {
  return list && !list.off && typeof list.lastQueryCopy === 'string' ? list.lastQueryCopy : '';
}

/** An index's schema field in one line: `title TEXT SORTABLE`, `$.a AS a TAG`. */
export function fieldText(f) {
  return [f.path ? `${f.path} AS ${f.name}` : f.name, f.type, ...f.flags].filter(Boolean).join(' ');
}

// ---------------------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------------------

const store = { who: null, list: null, error: null, picked: null, results: null, text: null, field: null };

function follow(state) {
  const c = state.conn;
  const who = c ? [c.name, c.docker, c.pickedNode].join('|') : null;
  if (who !== store.who) {
    store.who = who;
    store.list = null;
    store.error = null;
    store.picked = null;
    store.results = null;
    store.text = null;
    store.field = null;
  }
}

export function mount(root, ctx, { head, body: area }) {
  const state = ctx.state();
  follow(state);
  let alive = true;
  const conn = state.conn;
  const readAgain = h('button', { type: 'button', class: 'btn', text: 'Read again', 'data-act': 'refresh', onclick: () => load(true) });
  head.actions.append(readAgain);
  const box = h('div', { class: 'stack-lg', 'aria-live': 'polite' });
  area.append(box);
  // The query box lives across draws, so what is typed stays while the list is read again.
  const queryBox = h('input', {
    class: 'field mono grow', type: 'search', id: 'search-query', autocomplete: 'off', spellcheck: false, autocapitalize: 'off',
    maxlength: String(MAX_QUERY), placeholder: '* (every document)', 'data-act': 'filter',
  });

  let seq = 0;
  // The box starts from the query last run, once, when the view is shown.
  let started = false;
  async function load(refresh = false) {
    const mine = (seq += 1);
    readAgain.disabled = true;
    try {
      store.list = await ctx.api.getSearch({ refresh });
      store.error = null;
      if (!store.results && store.list.results) store.results = store.list.results;
    } catch (err) {
      store.error = errorText(err);
    }
    if (!alive || mine !== seq) return;
    if (!started && store.list) {
      started = true;
      if (!queryBox.value) queryBox.value = queryStart(store.list);
    }
    readAgain.disabled = false;
    draw();
  }

  const writesOff = () => (store.list && store.list.writes) || null;

  function draw() {
    const l = store.list;
    const where = l && l.module ? `${l.module}${typeof l.node === 'number' && conn && conn.nodes[l.node] ? ` on ${conn.nodes[l.node].address}` : ''}` : null;
    head.subtitle.textContent = l && !l.off ? `Indexes of ${where || 'the Search module'}${l.indexes.length ? ` · ${plural(l.indexes.length, 'index', 'indexes')}` : ''}.` : 'The indexes of Redis Search or valkey-search.';
    if (store.error && !l) {
      fill(box, h('div', { class: 'card' }, notice(store.error)));
      return;
    }
    if (!l) {
      fill(box, h('div', { class: 'card' }, h('p', { class: 'muted', text: 'Reading the indexes…' })));
      return;
    }
    if (l.off) {
      fill(box, h('div', { class: 'card stack' },
        h('h2', { class: 'lbl', text: l.noModule ? 'The Search module is not loaded on this server.' : 'Search is off' }),
        notice(l.off),
        l.detail ? h('p', { class: 'muted', text: l.detail }) : null,
        h('p', { class: 'muted small', text: 'Read again asks the server again.' })));
      return;
    }
    const picked = l.indexes.find((i) => i.name.id === store.picked) || l.indexes[0] || null;
    if (picked) store.picked = picked.name.id;
    const keepFocus = box.contains(document.activeElement) && document.activeElement !== queryBox ? document.activeElement.id : null;
    const hadQueryFocus = document.activeElement === queryBox;
    fill(box,
      store.error ? notice(store.error) : null,
      l.error ? notice(l.error) : null,
      writesOff() ? readOnlyNote(writesOff()) : null,
      h('div', { class: 'browse search' }, listCard(l, picked), picked ? detailCard(l, picked) : null),
      picked ? queryCard(l, picked) : null);
    if (hadQueryFocus) queryBox.focus();
    else if (keepFocus) {
      const again = document.getElementById(keepFocus);
      if (again) again.focus();
    }
  }

  function listCard(l, picked) {
    return h('div', { class: 'card stack browse-col search-list', 'data-pane': 'indexes' },
      h('div', { class: 'card-head' }, h('h2', { class: 'lbl', text: `Indexes · ${l.indexes.length}` })),
      l.firstOf ? h('p', { class: 'muted small', text: l.firstOf }) : null,
      l.notes.map((n) => h('p', { class: 'muted small', text: n })),
      l.indexes.length ? table([
        {
          label: 'Index', cls: 'keycell', cell: (i) => h('button', {
            type: 'button', class: 'linkish mono clip', id: `index-${i.name.id}`, text: i.name.name, 'aria-pressed': picked && i.name.id === picked.name.id ? 'true' : 'false', 'data-row': i.name.id,
            onclick: () => {
              store.picked = i.name.id;
              draw();
            },
          }),
        },
        { label: 'On', cls: 'mono nowrap', cell: (i) => i.on || DASH },
        { label: 'Docs', cls: 'mono right nowrap', cell: (i) => (typeof i.docs === 'number' ? fmtGrouped(i.docs) : DASH) },
        { label: 'Size', cls: 'mono right nowrap', cell: (i) => (typeof i.bytes === 'number' ? fmtBytes(i.bytes) : DASH) },
        { label: 'Indexed', cls: 'mono nowrap', cell: (i) => (i.building ? `▲ ${i.progressText || 'building'}` : i.progressText || DASH) },
      ], l.indexes.map((i) => ({ ...i, rowClass: picked && i.name.id === picked.name.id ? 'on' : '' })), { caption: 'The indexes', cls: 'indexes' })
        : emptyState(l.empty ? l.empty.title : 'The module has no indexes.'),
      !l.indexes.length && l.empty && l.empty.text ? h('p', { class: 'muted small', text: l.empty.text }) : null,
      writesOff() ? null : createBox());
  }

  function detailCard(l, i) {
    const facts = [
      ['On', i.on || DASH],
      ['Prefixes', i.prefixes.length ? i.prefixes.join(' ') : 'every key'],
      ['Documents', typeof i.docs === 'number' ? fmtGrouped(i.docs) : DASH],
      ['Size', typeof i.bytes === 'number' ? fmtBytes(i.bytes) : DASH],
      ['Indexed', i.progressText || (typeof i.progress === 'number' ? fmtPercent(i.progress) : DASH)],
      ['Failures', typeof i.failures === 'number' ? fmtGrouped(i.failures) : DASH],
    ];
    return h('div', { class: 'card stack browse-col search-detail', 'data-pane': 'index' },
      h('div', { class: 'value-head' }, h('h2', { class: 'key-title mono wrap-any', text: i.name.name }),
        writesOff() ? null : h('button', { type: 'button', class: 'btn danger', text: 'Drop index', 'aria-label': `Drop the index ${i.name.name} (FT.DROPINDEX, the documents stay)`, 'data-act': 'search.drop', onclick: (ev) => drop(i, ev.currentTarget) })),
      i.infoText ? notice(i.infoText, 'info') : h('dl', { class: 'facts' }, facts.map(([k, v]) => h('div', null, h('dt', { text: k }), h('dd', { class: 'mono', text: v })))),
      i.building ? notice('Still indexing: queries may miss documents until it is done.', 'info') : null,
      i.leftOut ? h('p', { class: 'muted small', text: i.leftOut }) : null,
      h('h3', { class: 'lbl sub-lbl', text: `Fields · ${i.schema.length}` }),
      i.schema.length ? table([
        { label: 'Field', cls: 'mono', cell: (f) => f.name },
        { label: 'Path', cls: 'mono', cell: (f) => f.path || DASH },
        { label: 'Type', cls: 'mono nowrap', cell: (f) => f.type || DASH },
        { label: 'Flags', cls: 'mono', cell: (f) => (f.flags.length ? f.flags.join(' ') : DASH) },
      ], i.schema, { caption: `The fields of ${i.name.name}` }) : h('p', { class: 'muted', text: i.fieldsText || 'The module returned no fields.' }));
  }

  // ---------- queries ----------

  function queryCard(l, i) {
    const run = (cmd) => async (ev) => {
      if (ev && ev.preventDefault) ev.preventDefault();
      await query(i, cmd);
    };
    const button = (cmd, label, { primary = false, act = null } = {}) => {
      const off = commandOff(l, cmd);
      return h('button', { type: cmd === 'search' ? 'submit' : 'button', class: primary ? 'btn primary' : 'btn', text: label, disabled: !!off, title: off || `Send ${COMMANDS[cmd]}`, 'data-act': act, onclick: cmd === 'search' ? null : run(cmd) });
    };
    const offLines = ['search', 'explain', 'profile'].map((c) => l.cmdOff[c]).filter(Boolean);
    const r = store.results;
    return h('section', { class: 'card stack', 'aria-labelledby': 'search-q-title' },
      h('h2', { class: 'lbl', id: 'search-q-title' }, 'Query ', h('span', { class: 'as-is mono', text: i.name.name })),
      h('form', { class: 'toolbar', autocomplete: 'off', onsubmit: run('search') },
        h('label', { class: 'sr-only', htmlFor: 'search-query', text: 'Query (FT.SEARCH)' }), queryBox,
        button('search', 'Search', { primary: true }), button('explain', 'Explain', { act: 'search.explain' }), button('profile', 'Profile', { act: 'search.profile' })),
      h('p', { class: 'muted small', text: `FT.SEARCH ${i.name.name} <query> LIMIT 0 50. Explain and Profile use the query last run${l.lastQuery !== null ? `: ${l.lastQuery}` : ''}.` }),
      offLines.map((t) => notice(t)),
      l.busy ? h('p', { class: 'muted', role: 'status', text: `${l.busy} is running…` }) : null,
      store.text ? textBox(store.text) : null,
      r ? resultsBox(r) : null,
      store.field ? fieldBox(store.field) : null);
  }

  async function query(i, cmd) {
    if (commandOff(store.list, cmd)) return;
    store.list.busy = COMMANDS[cmd];
    draw();
    // The list may be read again meanwhile: what the answer says goes on the list then.
    let lastQuery;
    let off = null;
    try {
      const reply = await ctx.api.runQuery({ index: i.name.id, cmd, query: queryBox.value });
      if (!alive) return;
      if (reply.off) {
        off = reply.off;
        ctx.notify(reply.off, 'error');
      } else {
        if (cmd === 'search') lastQuery = reply.results ? reply.results.query : queryBox.value || '*';
        if (reply.results) store.results = reply.results;
        store.text = reply.text ? { cmd, lines: reply.text.lines, empty: reply.text.empty } : null;
        store.field = null;
      }
    } catch (err) {
      ctx.notify(errorText(err), 'error');
      // What the server now takes as the last query: read from it.
      if (alive && cmd === 'search') {
        store.list.busy = null;
        load();
        return;
      }
    }
    const l = store.list;
    if (l && !l.off) {
      l.busy = null;
      if (off) l.cmdOff = { ...l.cmdOff, [cmd]: off };
      if (lastQuery !== undefined) l.lastQuery = lastQuery;
    }
    if (alive) draw();
  }

  function textBox(t) {
    return h('section', { class: 'value-box', 'aria-label': `${COMMANDS[t.cmd]} reply` },
      h('div', { class: 'value-label', text: COMMANDS[t.cmd] }),
      h('pre', { class: 'value-pre', text: t.lines.length ? t.lines.join('\n') : t.empty || 'The server returned nothing.' }));
  }

  async function page(r, from) {
    try {
      store.results = await ctx.api.getResults({ run: r.run, from });
      store.field = null;
    } catch (err) {
      ctx.notify(errorText(err), 'error');
      if (err.status === 409) store.results = null;
    }
    if (alive) draw();
  }

  function resultsBox(r) {
    const steps = pageSteps(r);
    return h('section', { class: 'stack-sm', 'aria-label': 'Results', 'data-pane': 'results' },
      h('div', { class: 'toolbar' },
        h('h3', { class: 'lbl' }, `${COMMANDS[r.cmd]} `, h('span', { class: 'as-is mono', text: r.query })),
        h('span', { class: 'muted small', text: resultsLine(r) })),
      r.docs.length ? h('ul', { class: 'list' }, r.docs.map((d) => h('li', { class: 'item-actions' },
        h('div', { class: 'toolbar' },
          h('span', { class: 'muted small mono', text: `${d.index + 1}.` }),
          h('button', { type: 'button', class: 'linkish mono clip', text: d.key.name, title: `Open ${d.key.name} on Browse › Keys`, 'data-row': String(d.index), onclick: () => ctx.openKey(d.key) }),
          h('span', { class: 'muted small', text: plural(d.fieldCount, 'field', 'fields') })),
        d.fields.length ? h('dl', { class: 'facts' }, d.fields.map((f, k) => h('div', null,
          h('dt', { class: 'mono', text: f.name }),
          h('dd', { class: 'mono' }, h('span', { class: 'wrap-any', text: f.value }), ' ',
            h('button', { type: 'button', class: 'btn quiet', text: 'Whole', 'aria-label': `Show ${f.name} of ${d.key.name} whole`, onclick: () => openField(r, d, k) }))))) : null))) : emptyState(r.empty || 'No documents match the query.'),
      r.kept > (r.page || 10) ? h('div', { class: 'toolbar' },
        h('button', { type: 'button', class: 'btn', text: 'Previous', disabled: steps.prev === null, onclick: () => page(r, steps.prev) }),
        h('span', { class: 'muted small', text: pageText(r) }),
        h('button', { type: 'button', class: 'btn', text: 'Next', disabled: steps.next === null, onclick: () => page(r, steps.next) })) : null);
  }

  async function openField(r, d, k, format = null) {
    try {
      store.field = await ctx.api.getResultField({ run: r.run, doc: d.index, field: k, format });
      store.field.at = { r, d, k };
    } catch (err) {
      ctx.notify(errorText(err), 'error');
    }
    if (alive) draw();
  }

  function fieldBox(f) {
    const s = f.shown;
    formatSeq += 1;
    const id = `search-fmt-${formatSeq}`;
    const select = h('select', { class: 'field', id, 'data-act': 'format', onchange: () => openField(f.at.r, f.at.d, f.at.k, select.value) },
      s.formats.map((x) => h('option', { value: x.toLowerCase(), selected: x === s.format, text: x })));
    return h('section', { class: 'value-box', 'aria-label': `${f.name} of ${f.key.name}` },
      h('div', { class: 'value-label toolbar' },
        h('span', { class: 'mono wrap-any grow', text: `${f.key.name} · ${f.name} · ${s.header}` }),
        h('label', { htmlFor: id, class: 'sr-only', text: 'Format' }), select,
        s.copy !== null ? h('button', {
          type: 'button', class: 'btn quiet', text: 'Copy',
          onclick: async () => {
            const problem = await copyText(s.copy);
            ctx.notify(problem || `Copied ${f.name} to the clipboard.`, problem ? 'error' : 'ok');
          },
        }) : null,
        h('button', { type: 'button', class: 'btn quiet', text: 'Close', onclick: () => { store.field = null; draw(); } })),
      s.note ? h('p', { class: 'muted small pad', text: s.note }) : null,
      h('pre', { class: 'value-pre', text: s.lines.join('\n') }));
  }
  let formatSeq = 0;

  // ---------- FT.CREATE and FT.DROPINDEX ----------

  // Built once: what is typed stays while the list is drawn again.
  let created = null;
  const createBox = () => {
    if (!created) created = createForm();
    return created;
  };

  function createForm() {
    const row = (label, el, hint = null) => h('div', { class: 'form-row' }, h('label', { htmlFor: el.id, text: label }), el, hint ? h('p', { class: 'hint small muted', text: hint }) : null);
    const name = h('input', { class: 'field mono', type: 'text', id: 'search-new-name', autocomplete: 'off', spellcheck: false, autocapitalize: 'off', placeholder: 'idx:orders' });
    const on = h('select', { class: 'field', id: 'search-new-on' }, h('option', { value: 'HASH', text: 'HASH' }), h('option', { value: 'JSON', text: 'JSON' }));
    const prefixes = h('input', { class: 'field mono', type: 'text', id: 'search-new-prefixes', autocomplete: 'off', spellcheck: false, autocapitalize: 'off', placeholder: 'order:' });
    const fields = h('input', { class: 'field mono', type: 'text', id: 'search-new-fields', autocomplete: 'off', spellcheck: false, autocapitalize: 'off', placeholder: 'title TEXT, price NUMERIC' });
    const error = h('p', { class: 'form-error', role: 'alert' });
    const button = h('button', { type: 'submit', class: 'btn', text: 'Create' });
    return h('details', { class: 'advanced' },
      h('summary', { text: 'Create an index (FT.CREATE)', 'data-act': 'search.create' }),
      h('form', {
        class: 'form', autocomplete: 'off',
        onsubmit: async (ev) => {
          ev.preventDefault();
          error.textContent = '';
          button.disabled = true;
          const typed = name.value.trim();
          try {
            await ctx.api.createIndex({ name: name.value, on: on.value, prefixes: prefixes.value, fields: fields.value });
            ctx.notify(`Created index ${typed}.`, 'ok');
            for (const box of [name, prefixes, fields]) box.value = '';
            if (alive) await load();
          } catch (err) {
            if (err.cancelled) ctx.notify(errorText(err), 'info');
            else error.textContent = errorText(err);
          } finally {
            if (button.isConnected) button.disabled = false;
          }
        },
      },
      row('Name', name),
      row('On', on),
      row('Prefixes', prefixes, 'Separated by spaces or commas; empty: every key.'),
      row('Fields', fields, 'name TYPE, …; on JSON $.path AS alias TYPE. A type is TEXT, TAG, NUMERIC or GEO.'),
      error, h('div', { class: 'row-actions' }, button)));
  }

  async function drop(i, button) {
    button.disabled = true;
    try {
      await ctx.api.dropIndex({ index: i.name.id });
      ctx.notify(`Dropped index ${i.name.name}. Its documents stay.`, 'ok');
      if (store.results && store.results.index.id === i.name.id) {
        store.results = null;
        store.text = null;
        store.field = null;
      }
      store.picked = null;
      if (alive) await load();
    } catch (err) {
      ctx.notify(errorText(err), err.cancelled ? 'info' : 'error');
    } finally {
      if (button.isConnected) button.disabled = false;
    }
  }

  const offState = ctx.api.on('state', (s) => {
    if (!alive) return;
    const before = store.who;
    follow(s);
    if (store.who !== before) load();
  });

  draw();
  load();

  return () => {
    alive = false;
    offState();
  };
}
