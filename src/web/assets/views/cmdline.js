// The command line panel: one command at a time, drawn the way redis-cli draws replies.
//
// The redis-lens process decides what is sent: readonly commands go, writes go through
// the write gate (the confirm dialog), and refusals come back in its own fixed wording.
// The page only shows the reply, in the view picked: text, table, JSON or raw.
//
// Rule 1: the history and the output hold the masked line only, in memory, for this page
// load. Nothing typed here goes to localStorage or to the address.

import {
  h, fill, pills, CommandHistory, maskCommand, fmtTook, errorText,
} from '../ui.js';

const VIEWS = [
  { id: 'text', label: 'Text' },
  { id: 'table', label: 'Table' },
  { id: 'json', label: 'JSON' },
  { id: 'raw', label: 'Raw' },
];

const MAX_ENTRIES = 100;

/** What survives closing and opening the panel. */
const session = { entries: [], history: new CommandHistory(100), view: 'text', running: false };

/** What a reply shows in a view: `{ kind: 'text', text }` or `{ kind: 'table', table }`,
 *  with `note` when the reply has no such view and its text is shown instead. */
export function replyBody(views, view) {
  if (view === 'table') {
    if (views.table && views.table.rows) return { kind: 'table', table: views.table, note: null };
    if (typeof views.table === 'string') return { kind: 'text', text: views.table, note: null };
    return { kind: 'text', text: views.text ?? '', note: 'This reply has no table view; shown as text.' };
  }
  if (view === 'json' || view === 'raw') {
    if (typeof views[view] === 'string') return { kind: 'text', text: views[view], note: null };
    return { kind: 'text', text: views.text ?? '', note: `This reply has no ${view === 'json' ? 'JSON' : 'raw'} view; shown as text.` };
  }
  return { kind: 'text', text: views.text ?? '', note: null };
}

/** Keep the newest `max` entries. */
export function capEntries(entries, max = MAX_ENTRIES) {
  return entries.length > max ? entries.slice(entries.length - max) : entries;
}

/** Mount the panel once. `onClose` hides it. Returns `{ focus(), refresh() }`. */
export function mount(root, ctx, onClose) {
  const access = h('span', { class: 'muted small' });
  const viewBox = h('div');
  const out = h('div', { class: 'box cmd-out', tabindex: '0', role: 'log', 'aria-live': 'polite', 'aria-label': 'Command output' });
  const input = h('input', {
    class: 'mono', type: 'text', id: 'cmd-input', placeholder: 'PING', autocomplete: 'off', spellcheck: false,
    'aria-describedby': 'cmd-keys', autocapitalize: 'off', autocorrect: 'off',
  });

  function drawViews() {
    fill(viewBox, pills(VIEWS.map((v) => ({ ...v })), session.view, (id) => {
      session.view = id;
      drawViews();
      drawOutput();
    }, 'Reply view'));
    for (const pill of viewBox.querySelectorAll('.pill')) pill.classList.add('small');
  }

  function entryNode(e) {
    const lines = [h('div', { class: 'line in', text: `> ${e.input}` })];
    if (e.node) lines.push(h('div', { class: 'line muted', text: `${e.node}>` }));
    if (e.kind === 'running') lines.push(h('div', { class: 'line muted', text: 'running…' }));
    else if (e.kind === 'refused') lines.push(h('div', { class: 'line err', text: `✗ ${e.text}` }));
    else if (e.kind === 'error') lines.push(h('div', { class: 'line err', text: `✗ (error) ${e.text}` }));
    else if (e.kind === 'note') lines.push(h('div', { class: 'line muted', text: e.text }));
    else {
      const body = replyBody(e.views, session.view);
      if (body.note) lines.push(h('div', { class: 'line muted', text: body.note }));
      if (body.kind === 'table') {
        lines.push(h('div', { class: 'scroll-x' }, h('table', { class: 'tbl reply' },
          h('thead', null, h('tr', null, body.table.columns.map((c) => h('th', { scope: 'col', text: c })))),
          h('tbody', null, body.table.rows.map((r) => h('tr', null, r.map((cell) => h('td', { text: cell }))))))));
      } else lines.push(h('div', { class: 'line', text: body.text }));
      if (typeof e.micros === 'number') lines.push(h('div', { class: 'line muted', text: `(${fmtTook(e.micros)})` }));
    }
    return h('div', { class: 'entry' }, lines);
  }

  function drawOutput() {
    const hint = ctx.mock
      ? 'Type a command and press enter. Try PING, DBSIZE, INFO memory, HGETALL user:10293:cart or DEL user:10293:cart.'
      : 'Type a command and press enter. Readonly commands are sent; writes go through the write gate and ask first where they should.';
    fill(out, session.entries.length ? session.entries.map(entryNode) : h('div', { class: 'line muted hint', text: hint }));
    out.scrollTop = out.scrollHeight;
  }

  async function run() {
    const line = input.value.trim();
    if (!line) return;
    const state = ctx.state();
    if (session.running) {
      ctx.notify('Wait: a command is still running.', 'info');
      return;
    }
    if (!state.connected) {
      ctx.notify('The command line needs a connection. Open Connections and pick one.', 'info');
      return;
    }
    const shown = maskCommand(line);
    session.history.add(shown);
    const entry = { input: shown, kind: 'running', text: '', views: null, node: null, micros: null };
    session.entries = capEntries(session.entries.concat([entry]));
    session.running = true;
    input.value = '';
    drawOutput();
    try {
      const reply = await ctx.api.runCommand(line);
      if (reply.input) entry.input = reply.input;
      entry.node = reply.node;
      entry.micros = reply.micros;
      entry.views = reply.views;
      entry.kind = reply.kind;
      entry.text = reply.views.text;
    } catch (err) {
      entry.kind = err.cancelled ? 'note' : 'refused';
      entry.text = errorText(err);
    }
    session.running = false;
    drawOutput();
  }

  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      run();
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      input.value = session.history.prev(input.value);
    } else if (e.key === 'ArrowDown') {
      e.preventDefault();
      input.value = session.history.next(input.value);
    } else if (e.key === 'Escape') {
      e.preventDefault();
      onClose();
    } else if (e.ctrlKey && !e.metaKey && !e.altKey && e.key.toLowerCase() === 'l') {
      e.preventDefault();
      session.entries = [];
      drawOutput();
    } else if (e.ctrlKey && !e.metaKey && !e.altKey && e.key.toLowerCase() === 'u') {
      e.preventDefault();
      input.value = '';
    }
  });

  fill(root,
    h('div', { class: 'cmd-head' },
      h('div', null, h('h2', { class: 'lbl', text: 'Command line' }), access),
      h('div', { class: 'toolbar' },
        viewBox,
        h('button', {
          type: 'button', class: 'btn', text: 'Clear',
          onclick: () => {
            session.entries = [];
            drawOutput();
            input.focus();
          },
        }),
        h('button', { type: 'button', class: 'btn', text: 'Close', onclick: onClose }))),
    out,
    h('label', { class: 'box cmd-in', htmlFor: 'cmd-input' },
      h('span', { class: 'prompt', 'aria-hidden': 'true', text: '>' }),
      h('span', { class: 'sr-only', text: 'Redis command' }),
      input,
      h('span', { class: 'badge mono', 'aria-hidden': 'true', text: 'enter' })),
    h('p', { class: 'muted small', id: 'cmd-keys', text: 'enter runs it · ↑ ↓ step through this session’s commands · esc closes · ctrl-l clears the output · ctrl-u clears the line' }));

  function refresh() {
    const s = ctx.state();
    if (!s.connected || !s.conn) access.textContent = 'not connected: connect first';
    else access.textContent = `${s.conn.name} · ${s.conn.readOnly ? 'read-only: readonly commands' : 'writes ask as usual'}`;
  }

  drawViews();
  drawOutput();
  refresh();
  return { focus: () => input.focus(), refresh };
}
