// Pub/Sub: the channels with their subscriber counts, and a live tail.
//
// One tail at a time; it keeps running while another view is shown. Messages are shown,
// never saved: they live in a TailBuffer in memory and nowhere else. Subscribing is not a
// write, so read-only connections subscribe too. Publishing is a write: it is hidden on a
// read-only connection and goes through the confirm flow otherwise.

import {
  h, fill, DASH, TailBuffer, fmtClock, fmtGrouped, fmtRate, plural, emptyState, notice,
  readOnlyNote, connectFirst, nodePicker, fillLog, errorText,
} from '../ui.js';

const SHOWN = 300;

/** The tail, kept while other views are shown. */
const tail = { sub: null, buffer: new TailBuffer(2000), filter: '', stopped: null, conn: null };

/** Whether text is a pattern (`*`, `?` or `[`): sent with PSUBSCRIBE, not SUBSCRIBE. */
export function isPattern(text) {
  return /[*?[]/.test(String(text ?? ''));
}

/** The line above the tail: `12 msg/s · 340 kept (last 2,000)`, or `paused · 12 new`. */
export function tailStatus(buffer, now = Date.now()) {
  const kept = `${fmtGrouped(buffer.kept.length)} kept (last ${fmtGrouped(buffer.cap)})`;
  const skipped = buffer.skipped ? ` · ${fmtGrouped(buffer.skipped)} skipped, too many at once` : '';
  if (buffer.paused) return `paused · ${fmtGrouped(buffer.newWhilePaused)} new · ${kept}${skipped}`;
  return `${fmtRate(Math.round(buffer.rate(now) * 10) / 10)} msg/s · ${kept}${skipped}`;
}

function tailReset(connName) {
  tail.sub = null;
  tail.buffer = new TailBuffer(2000);
  tail.filter = '';
  tail.stopped = null;
  tail.conn = connName;
}

/** Called once at boot: keep the tail's messages whatever view is on screen. */
export function init(api) {
  api.on('tail', (batch) => {
    if (!tail.sub) return;
    tail.buffer.push(batch.messages, batch.skipped);
    if (batch.stopped) tail.stopped = batch.stopped;
  });
  api.on('state', (s) => {
    const name = s.conn ? s.conn.name : null;
    if (name !== tail.conn) tailReset(name);
  });
}

export function mount(root, ctx) {
  const state = ctx.state();
  const gate = connectFirst(state);
  const view = h('section', { class: 'view', 'aria-labelledby': 'pubsub-title' });
  root.append(view);
  if (gate) {
    view.append(h('h1', { class: 'title', id: 'pubsub-title', text: 'Pub/Sub' }), gate);
    return () => {};
  }

  const conn = state.conn;
  const cluster = conn.nodes.length >= 2 || conn.topology === 'cluster';
  let alive = true;
  let frame = 0;
  const timers = [];
  const offs = [];

  const left = h('div', { class: 'card tight col narrow' });
  const right = h('div', { class: 'card stack col wide' });
  view.append(
    h('div', { class: 'view-head' },
      h('h1', { class: 'title', id: 'pubsub-title', text: 'Pub/Sub' }),
      h('div', { class: 'actions' }, nodePicker(conn, (id) => ctx.pickNode(id)))),
    h('div', { class: 'cols' }, left, right));

  // ---------- Channels ----------
  const target = h('input', { class: 'field mono', type: 'text', id: 'sub-target', placeholder: 'orders.* or a channel', autocomplete: 'off', spellcheck: false });
  const shard = h('input', { type: 'checkbox', id: 'sub-shard' });
  const subError = h('p', { class: 'field-error', role: 'alert' });
  const listBox = h('div', null, h('p', { class: 'muted', text: 'Reading the channels…' }));
  const count = h('span', { class: 'muted small' });

  async function subscribe(name, isShard) {
    subError.textContent = '';
    const text = String(name).trim();
    if (!text) {
      subError.textContent = 'Type a channel or a pattern to subscribe to.';
      return;
    }
    if (isShard && isPattern(text)) {
      subError.textContent = "A pattern can't be shard-subscribed: type one channel name.";
      return;
    }
    try {
      const sub = await ctx.api.subscribe({ target: text, shard: isShard });
      tail.sub = sub;
      tail.buffer = new TailBuffer(2000);
      tail.filter = '';
      tail.stopped = null;
      target.value = '';
      if (alive) {
        drawTail();
        drawChannels(lastChannels);
      }
    } catch (err) {
      subError.textContent = errorText(err);
    }
  }

  let lastChannels = null;
  let drawn = '';
  function drawChannels(data) {
    lastChannels = data;
    if (!data) return;
    // The counts are read again every few seconds. Redraw only when something changed, and
    // keep the keyboard focus on the channel it was on.
    const signature = JSON.stringify([data, tail.sub]);
    if (signature === drawn) return;
    drawn = signature;
    const active = document.activeElement;
    const focused = active && listBox.contains(active) ? active.getAttribute('data-channel') : null;
    if (data.off) {
      count.textContent = '';
      fill(listBox, notice(data.off));
      return;
    }
    const parts = [plural(data.total ?? data.channels.length, 'channel', 'channels')];
    if (data.patterns !== null) parts.push(plural(data.patterns, 'pattern', 'patterns'));
    count.textContent = parts.join(' · ');
    const row = (c) => {
      const on = tail.sub && tail.sub.name === c.name && (tail.sub.kind === 'shard') === c.shard;
      const subs = c.subscribers === null ? DASH : plural(c.subscribers, 'subscriber', 'subscribers');
      return h('li', null, h('button', {
        type: 'button', class: on ? 'row row-channel on' : 'row row-channel', 'aria-pressed': on ? 'true' : 'false',
        'data-channel': `${c.shard ? 's' : 'c'}:${c.name}`,
        onclick: () => subscribe(c.name, c.shard),
      },
      h('span', { class: 'mono name clip', text: c.name }),
      h('span', { class: 'small muted right', text: c.node ? `${subs} · ${c.node}` : subs })));
    };
    const classic = data.channels.filter((c) => !c.shard);
    const shards = data.channels.filter((c) => c.shard);
    fill(listBox,
      data.node ? h('p', { class: 'muted small', text: `Subscribers on ${data.node}` }) : null,
      classic.length
        ? h('ul', { class: 'list' }, classic.map(row))
        : h('p', { class: 'muted', text: 'No active channels. A channel exists only while a client subscribes to it.' }),
      shards.length ? [h('h3', { class: 'lbl', text: 'Shard channels' }), h('ul', { class: 'list' }, shards.map(row))] : null,
      data.notes.map((n) => notice(n)),
      h('p', { class: 'muted small', text: 'Counts read no payloads.' }));
    if (focused !== null) {
      for (const button of listBox.querySelectorAll('button[data-channel]')) {
        if (button.getAttribute('data-channel') === focused) button.focus();
      }
    }
  }

  fill(left,
    h('div', { class: 'toolbar' }, h('h2', { class: 'lbl', text: 'Channels' }), count),
    h('form', {
      class: 'subscribe',
      onsubmit: (e) => {
        e.preventDefault();
        subscribe(target.value, cluster && shard.checked);
      },
    },
    h('label', { class: 'sr-only', htmlFor: 'sub-target', text: 'Channel or pattern to subscribe to' }),
    h('div', { class: 'line' }, target, h('button', { type: 'submit', class: 'btn', text: 'Subscribe' })),
    cluster && conn.shardChannels
      ? h('div', { class: 'check' }, shard, h('label', { htmlFor: 'sub-shard' }, 'Shard channel', h('span', { class: 'hint', text: 'SSUBSCRIBE, on the node that owns the channel’s slot.' })))
      : null,
    subError),
    listBox);

  async function loadChannels() {
    try {
      const data = await ctx.api.getChannels();
      if (alive) drawChannels(data);
    } catch (err) {
      if (alive) fill(listBox, notice(errorText(err)));
    }
  }
  loadChannels();
  timers.push(setInterval(() => {
    if (ctx.state().live && !document.hidden) loadChannels();
  }, 5000));

  // ---------- The tail ----------
  let log = null;
  let status = null;
  let pauseButton = null;

  function drawMessages() {
    if (!log || !tail.sub) return;
    const { rows, matching, total } = tail.buffer.visible(tail.filter, SHOWN);
    if (!rows.length) {
      const text = total && tail.filter.trim() ? 'No message matches the filter.' : tail.stopped ? 'The tail is stopped.' : 'Waiting for messages.';
      fillLog(log, h('p', { class: 'muted', text }));
    } else {
      const showChannel = tail.sub.kind === 'pattern';
      fillLog(log, [
        matching > rows.length ? h('p', { class: 'muted small', text: `Showing the newest ${fmtGrouped(rows.length)} of ${fmtGrouped(matching)}.` }) : null,
        rows.map((m) => h('div', { class: 'row-msg row' },
          h('span', { class: 't', text: fmtClock(m.at, true) }),
          h('span', { class: 'wrap-any' }, showChannel ? h('span', { class: 'muted', text: `${m.channel}  ` }) : null, m.body))),
      ]);
    }
    drawStatus();
  }

  function drawStatus() {
    if (!status || !tail.sub) return;
    status.textContent = tail.stopped ? `The tail stopped: ${tail.stopped}` : tailStatus(tail.buffer);
    if (pauseButton) pauseButton.textContent = tail.buffer.paused ? 'Resume' : 'Pause';
  }

  function schedule() {
    if (frame) return;
    frame = requestAnimationFrame(() => {
      frame = 0;
      if (alive && !tail.buffer.paused) drawMessages();
      else if (alive) drawStatus();
    });
  }

  function publishBox() {
    if (conn.readOnly) return readOnlyNote('Publishing is a write, and this connection is read-only. Turn writes on in Connections.');
    const sub = tail.sub;
    const channel = h('input', {
      class: 'field mono chan', type: 'text', id: 'pub-channel', placeholder: 'Channel', autocomplete: 'off', spellcheck: false,
      value: sub && sub.kind !== 'pattern' ? sub.name : '',
    });
    const message = h('input', { class: 'field mono', type: 'text', id: 'pub-message', placeholder: 'Message to publish', autocomplete: 'off', spellcheck: false });
    const send = h('button', { type: 'submit', class: 'btn primary', text: 'Publish' });
    return h('form', {
      class: 'publish',
      onsubmit: async (e) => {
        e.preventDefault();
        if (!channel.value.trim() || !message.value) {
          ctx.notify('Publishing needs a channel and a message.', 'info');
          return;
        }
        send.disabled = true;
        try {
          const done = await ctx.api.publish({ channel: channel.value.trim(), message: message.value, shard: !!sub && sub.kind === 'shard' && sub.name === channel.value.trim() });
          ctx.notify(done.text || (done.receivers === null ? 'Published.' : `Published: ${plural(done.receivers, 'subscriber', 'subscribers')} got it.`), 'ok');
          message.value = '';
        } catch (err) {
          ctx.notify(errorText(err), err.cancelled ? 'info' : 'error');
        }
        send.disabled = false;
      },
    },
    h('label', { class: 'sr-only', htmlFor: 'pub-channel', text: 'Channel to publish to' }), channel,
    h('label', { class: 'sr-only', htmlFor: 'pub-message', text: 'Message to publish' }), message,
    send);
  }

  function drawTail() {
    log = null;
    status = null;
    pauseButton = null;
    const sub = tail.sub;
    if (!sub) {
      fill(right,
        h('p', { class: 'muted', text: 'Pick a channel to watch its messages live, or type a channel or a pattern. Messages are shown here and never saved.' }),
        publishBox());
      return;
    }
    const filter = h('input', {
      class: 'field mono grow', type: 'text', id: 'tail-filter', placeholder: 'Filter messages', autocomplete: 'off', spellcheck: false, value: tail.filter,
      oninput: () => {
        tail.filter = filter.value;
        drawMessages();
      },
    });
    pauseButton = h('button', {
      type: 'button', class: 'btn', text: 'Pause',
      onclick: () => {
        if (tail.buffer.paused) tail.buffer.resume();
        else tail.buffer.pause();
        drawMessages();
      },
    });
    status = h('p', { class: 'muted small' });
    log = h('div', { class: 'box log tail', tabindex: '0', role: 'log', 'aria-label': `Messages on ${sub.name}` });
    const kind = sub.kind === 'pattern' ? 'pattern ' : sub.kind === 'shard' ? 'shard channel ' : '';
    fill(right,
      h('div', { class: 'toolbar' },
        h('h2', { class: 'tail-name' }, kind ? h('span', { class: 'muted', text: kind }) : null, sub.name),
        h('label', { class: 'sr-only', htmlFor: 'tail-filter', text: 'Filter messages' }),
        filter,
        pauseButton,
        h('button', {
          type: 'button', class: 'btn', text: 'Clear',
          onclick: () => {
            tail.buffer.clear();
            drawMessages();
          },
        }),
        h('button', {
          type: 'button', class: 'btn quiet', text: 'Stop',
          onclick: async () => {
            try {
              await ctx.api.unsubscribe();
            } catch (err) {
              ctx.notify(errorText(err), 'error');
            }
            tail.sub = null;
            tail.stopped = null;
            if (alive) {
              drawTail();
              drawChannels(lastChannels);
            }
          },
        })),
      status,
      log,
      h('p', { class: 'muted small', text: 'Messages are shown, never saved.' }),
      publishBox());
    drawMessages();
  }

  offs.push(ctx.api.on('tail', schedule));
  timers.push(setInterval(drawStatus, 1000));
  drawTail();

  return () => {
    alive = false;
    if (frame) cancelAnimationFrame(frame);
    offs.forEach((off) => off());
    timers.forEach((t) => clearInterval(t));
  };
}
