// The command line: a drawer docked to the bottom of the window on every screen, one
// command at a time, drawn the way redis-cli draws replies.
//
// The redis-lens process decides what is sent: readonly commands go, writes go through
// the write gate (the confirm dialog), and refusals come back in its own fixed wording.
// It also draws each reply, in four views; the page only shows the one picked.
//
// Rule 1: the history and the output hold the masked line only, in memory, for this page
// load. Nothing typed here goes to localStorage or to the address.

import {
  h, fill, pills, CommandHistory, maskCommand, fmtTook, plural, errorText,
} from '../ui.js';

const VIEWS = [
  { id: 'text', label: 'Text' },
  { id: 'table', label: 'Table' },
  { id: 'json', label: 'JSON' },
  { id: 'raw', label: 'Raw' },
];

const MAX_ENTRIES = 100;
/** The TUI's input cap: redis-lens refuses a longer line (413) in these words. */
export const MAX_LINE = 8192;
const TOO_LONG = 'Not sent: a line holds at most 8,192 characters.';

/** What survives closing and opening the panel. */
const session = { entries: [], history: new CommandHistory(100), view: 'text', pipeline: false, running: false };

/** What is known about the connected server's commands, read once a connection: the
 *  names for completion, and each command's help as it is asked for. */
const known = { key: null, names: null, namesOff: null, help: new Map() };

/** What a reply shows in a view: its text in that view, or its plain text with a note
 *  when the reply has no such view. */
export function replyBody(views, view) {
  if (view !== 'text') {
    if (typeof views[view] === 'string') return { text: views[view], note: null };
    const label = { table: 'table', json: 'JSON', raw: 'raw' }[view];
    return { text: views.text ?? '', note: `This reply has no ${label} view; shown as text.` };
  }
  return { text: views.text ?? '', note: null };
}

/** Keep the newest `max` entries. */
export function capEntries(entries, max = MAX_ENTRIES) {
  return entries.length > max ? entries.slice(entries.length - max) : entries;
}

/** The lines `tab` steps through for what is typed: command names while the first word is
 *  typed, a container command's subcommands while the second is. `names` are the server's
 *  (`get`, `config`, `config|get`). A count in front (`3 GET k`) is kept. The case follows
 *  what was typed: lower-case stays lower-case, anything else completes in capitals. */
export function completeLine(line, names) {
  const m = /^(\s*(?:\d+\s+)?)(\S*)(?:(\s+)(\S*))?$/.exec(String(line ?? ''));
  if (!m || !names || !names.length) return [];
  const [, lead, first, gap, second] = m;
  const cased = (typed, name) => (typed !== '' && typed === typed.toLowerCase() ? name.toLowerCase() : name.toUpperCase());
  if (gap === undefined) {
    if (!first) return [];
    const want = first.toLowerCase();
    return names.filter((n) => !n.includes('|') && n.startsWith(want)).sort().map((n) => `${lead}${cased(first, n)}`);
  }
  const prefix = `${first.toLowerCase()}|${second.toLowerCase()}`;
  return names.filter((n) => n.startsWith(prefix)).sort().map((n) => `${lead}${first}${gap}${cased(second || first, n.split('|')[1])}`);
}

/** The command whose help applies to what is typed: the first word once it is typed out
 *  (a space follows it), without a count in front. Null while it is still being typed. */
export function helpTarget(line) {
  const rest = String(line ?? '').replace(/^\s*\d+\s+/, '');
  const m = /^\s*(\S+)\s/.exec(rest);
  return m && !/^\d+$/.test(m[1]) ? m[1].toLowerCase() : null;
}

/** The two helper lines for what is typed: the command (or the subcommand typed after a
 *  container command) with its arguments and complexity, then its summary. */
export function helpLines(help, line) {
  if (!help) return null;
  const words = String(line ?? '').trim().replace(/^\d+\s+/, '').split(/\s+/);
  let h2 = help;
  if (words.length > 1 && help.subcommands.length) {
    const sub = help.subcommands.find((s) => s.name.toLowerCase() === `${help.name.toLowerCase()}|${words[1].toLowerCase()}`);
    if (sub) h2 = sub;
  }
  const title = [h2.name.replace('|', ' ').toUpperCase(), h2.arguments].filter(Boolean).join(' ');
  return { title: [title, h2.complexity].filter(Boolean).join(' · '), summary: h2.summary || '' };
}

/** Mount the panel once. `onClose` hides it. Returns `{ focus(), refresh() }`. */
export function mount(root, ctx, onClose) {
  const access = h('span', { class: 'muted small' });
  const viewBox = h('div');
  const out = h('div', { class: 'box cmd-out', tabindex: '0', role: 'log', 'aria-live': 'polite', 'aria-label': 'Command output' });
  const helper = h('p', { class: 'cmd-help small', id: 'cmd-help' });
  const input = h('input', {
    class: 'mono', type: 'text', id: 'cmd-input', placeholder: 'PING', autocomplete: 'off', spellcheck: false, maxlength: MAX_LINE,
    'aria-describedby': 'cmd-help cmd-keys', autocapitalize: 'off', autocorrect: 'off',
  });
  let cycle = null;
  let helpFor = null;
  // One line at a time, as redis-lens runs them: Run is off while one is running.
  const runButton = h('button', { type: 'button', class: 'btn primary cmd-run', text: 'Run', onclick: () => run() });
  const busy = (on) => {
    runButton.disabled = on;
    input.setAttribute('aria-busy', on ? 'true' : 'false');
  };

  function drawViews() {
    const pipeline = h('button', {
      type: 'button', class: session.pipeline ? 'pill small on' : 'pill small', 'aria-pressed': session.pipeline ? 'true' : 'false', text: 'Pipeline',
      title: 'A repeat (a count in front of a readonly command) goes in pipelines',
      onclick: () => {
        session.pipeline = !session.pipeline;
        drawViews();
        refresh();
      },
    });
    fill(viewBox, h('div', { class: 'toolbar' }, pills(VIEWS.map((v) => ({ ...v })), session.view, (id) => {
      session.view = id;
      drawViews();
      drawOutput();
    }, 'Reply view'), pipeline));
    for (const pill of viewBox.querySelectorAll('.tabs .pill')) pill.classList.add('small');
  }

  function entryNode(e) {
    const lines = [h('div', { class: 'line in', text: `> ${e.input}` })];
    if (e.node) lines.push(h('div', { class: 'line muted', text: `${e.node}>` }));
    if (e.routeNote) lines.push(h('div', { class: 'line muted', text: e.routeNote }));
    if (e.kind === 'running') lines.push(h('div', { class: 'line muted', text: 'running…' }));
    else if (e.kind === 'refused') lines.push(h('div', { class: 'line err', text: `✗ ${e.text}` }));
    else if (e.kind === 'error') lines.push(h('div', { class: 'line err', text: `✗ (error) ${e.text}` }));
    else if (e.kind === 'note') lines.push(h('div', { class: 'line muted', text: e.text }));
    else {
      if (e.text) lines.push(h('div', { class: 'line muted', text: e.text }));
      // The first replies went past the 4 MiB the output keeps: redis-lens says how many.
      if (e.droppedNote) lines.push(h('div', { class: 'line muted', text: e.droppedNote }));
      const many = e.replies.length > 1;
      let total = 0;
      e.replies.forEach((r, i) => {
        const body = replyBody(r.views, session.view);
        if (many) lines.push(h('div', { class: 'line muted', text: `#${i + 1}` }));
        if (body.note) lines.push(h('div', { class: 'line muted', text: body.note }));
        lines.push(h('div', { class: r.error ? 'line err' : 'line', text: body.text }));
        if (typeof r.micros === 'number') {
          total += r.micros;
          if (!many) lines.push(h('div', { class: 'line muted', text: `(${fmtTook(r.micros)})` }));
        }
      });
      if (many) lines.push(h('div', { class: 'line muted', text: `(${plural(e.replies.length, 'reply', 'replies')} · ${fmtTook(total)})` }));
    }
    return h('div', { class: 'entry' }, lines);
  }

  function drawOutput() {
    const hint = ctx.mock
      ? 'Type a command and press enter. Try PING, DBSIZE, HGETALL user:10293:cart, 3 GET rl:203.0.113.7:login or DEL user:10293:cart.'
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
    if (line.length > MAX_LINE) {
      ctx.notify(TOO_LONG, 'error');
      return;
    }
    const entry = { input: shown, kind: 'running', text: '', replies: [], node: null, routeNote: null, droppedNote: null };
    session.entries = capEntries(session.entries.concat([entry]));
    session.running = true;
    busy(true);
    input.value = '';
    cycle = null;
    drawHelp();
    drawOutput();
    try {
      const reply = await ctx.api.runCommand(line, { pipeline: session.pipeline });
      if (reply.input) entry.input = reply.input;
      entry.node = reply.node;
      entry.routeNote = reply.routeNote;
      entry.replies = reply.replies;
      entry.kind = reply.kind;
      entry.text = reply.text || '';
      entry.droppedNote = reply.droppedNote;
    } catch (err) {
      // A refusal of redis-lens's own (a line too long, one still running, a question the
      // dashboard cannot ask) is shown like any other, in its words.
      entry.kind = err.cancelled ? 'note' : 'refused';
      entry.text = errorText(err);
    }
    session.running = false;
    busy(false);
    drawOutput();
  }

  /** `tab`: step through the completions of what is typed. Returns false when there is
   *  nothing to complete, so the key moves the focus as it does everywhere else. */
  function complete() {
    if (input.selectionStart !== input.value.length) return false;
    if (cycle && cycle.options[cycle.index] === input.value && cycle.options.length > 1) {
      cycle.index = (cycle.index + 1) % cycle.options.length;
    } else {
      const options = completeLine(input.value, known.names);
      if (!options.length || (options.length === 1 && options[0] === input.value)) return false;
      cycle = { options, index: 0 };
    }
    input.value = cycle.options[cycle.index];
    drawHelp();
    return true;
  }

  async function drawHelp() {
    let name = helpTarget(input.value);
    // With the server's command list at hand, a word that is not a command is not asked about.
    if (name && known.names && known.names.length && !known.names.includes(name)) name = null;
    if (!name || !ctx.state().connected) {
      helpFor = null;
      helper.textContent = known.namesOff && !input.value ? `Completion is off: ${known.namesOff}.` : '';
      return;
    }
    helpFor = name;
    if (!known.help.has(name)) known.help.set(name, ctx.api.getCommandHelp(name).catch(() => null));
    const help = await known.help.get(name);
    if (helpFor !== name) return;
    const lines = helpLines(help, input.value);
    fill(helper, lines ? [h('span', { class: 'mono strong', text: lines.title }), lines.summary ? h('span', { class: 'muted', text: `  ${lines.summary}` }) : null] : null);
  }

  input.addEventListener('input', () => {
    cycle = null;
    drawHelp();
  });
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      run();
    } else if (e.key === 'Tab' && !e.shiftKey && !e.ctrlKey && !e.altKey && !e.metaKey) {
      if (complete()) e.preventDefault();
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      input.value = session.history.prev(input.value);
      drawHelp();
    } else if (e.key === 'ArrowDown') {
      e.preventDefault();
      input.value = session.history.next(input.value);
      drawHelp();
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
      drawHelp();
    }
  });

  // Escape closes the drawer from anywhere inside it: the output, the view pills, Clear,
  // Close. The input handles its own Escape first (and marks the event handled).
  if (!root.dataset.escape) {
    root.dataset.escape = 'on';
    root.addEventListener('keydown', (e) => {
      if (drawerEscapes(e)) {
        e.preventDefault();
        onClose();
      }
    });
  }

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
            input.focus({ preventScroll: true });
          },
        }),
        h('button', { type: 'button', class: 'btn', text: 'Close', onclick: onClose }))),
    out,
    h('label', { class: 'box cmd-in', htmlFor: 'cmd-input' },
      h('span', { class: 'prompt', 'aria-hidden': 'true', text: '>' }),
      h('span', { class: 'sr-only', text: 'Redis command' }),
      input,
      runButton),
    helper,
    h('p', { class: 'muted small', id: 'cmd-keys', text: 'enter runs it · tab completes a command or subcommand · ↑ ↓ step through this session’s commands · esc closes · ctrl-l clears the output · ctrl-u clears the line' }));

  /** Called when the panel opens and when the connection changes. */
  function refresh() {
    const s = ctx.state();
    if (!s.connected || !s.conn) access.textContent = 'not connected: connect first';
    else access.textContent = `${s.conn.name} · ${s.conn.readOnly ? 'read-only: readonly commands' : 'writes ask as usual'}${session.pipeline ? ' · pipeline' : ''}`;

    // The command names and help belong to one connection: forget them when it changes,
    // and read the names once the panel is open on a live connection.
    const key = s.connected && s.conn ? [s.conn.name, s.conn.docker, s.conn.connectedAt].join('|') : null;
    if (key !== known.key) {
      known.key = key;
      known.names = null;
      known.namesOff = null;
      known.help = new Map();
      cycle = null;
    }
    if (key && known.names === null && known.namesOff === null && s.liveState === 'ready' && !root.hidden) {
      known.names = [];
      ctx.api.getCommandNames().then(
        (r) => {
          if (known.key !== key) return;
          known.names = r.names;
          known.namesOff = r.off;
          drawHelp();
        },
        () => {
          if (known.key === key) known.names = null;
        },
      );
    }
    drawHelp();
  }

  drawViews();
  drawOutput();
  refresh();
  busy(session.running);
  /** Put a line in the input, typed in but not run (Go to's `:`): enter runs it, as ever. */
  function setLine(text) {
    input.value = String(text ?? '').slice(0, MAX_LINE);
    cycle = null;
    drawHelp();
    input.focus({ preventScroll: true });
    input.setSelectionRange(input.value.length, input.value.length);
  }

  return { focus: () => input.focus({ preventScroll: true }), refresh, setLine };
}

/** Whether a key press inside the drawer closes it: Escape without a modifier that no
 *  control inside handled already (the input's own Escape). */
export function drawerEscapes(e) {
  return e.key === 'Escape' && !e.defaultPrevented && !e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey;
}

/** The drawer's state: `{ open }`. `toggle`, `open` and `close` give the next state and
 *  where the focus goes (`input` when it opens, `toggle` when it closes with the focus
 *  inside it). The drawer is the page's, not a route: the address never changes. */
export function drawerStep(state, action, focusInside = false) {
  const open = action === 'toggle' ? !state.open : action === 'open';
  if (open === state.open) return { open, focus: open ? 'input' : null };
  return { open, focus: open ? 'input' : focusInside ? 'toggle' : null };
}
