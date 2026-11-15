// redis-lens dashboard: boot, the hash router, the theme and the chrome around the views.
//
// The views live in views/*.js. Each exports `mount(root, ctx, route)` and returns a
// function that unmounts it. They read data only through `ctx.api` (api.js).

import { api, TEXT } from './api.js';
import {
  h, fill, setIcon, envBadge, createThemeStore, createToasts, confirmDialog,
  parseRoute, pushSeries, errorText,
} from './ui.js';
import * as connections from './views/connections.js';
import * as overview from './views/overview.js';
import * as server from './views/server.js';
import * as pubsub from './views/pubsub.js';
import * as cmdline from './views/cmdline.js';

const VIEWS = { overview, server, pubsub, connections };
const NAMES = Object.keys(VIEWS);
const TITLES = { overview: 'Overview', server: 'Server', pubsub: 'Pub/Sub', connections: 'Connections' };

const $ = (id) => document.getElementById(id);

const store = {
  state: { connected: false, connecting: null, conn: null, live: true, lost: null },
  series: { ops: [], clients: [], everySecs: null },
  figures: null,
  linkUp: true,
  locked: false,
  lastConn: null,
  // Paused on this page only: set when the server has no way to pause its polling.
  pausedHere: false,
};

let mounted = null;
let commandLine = null;
const notify = createToasts($('toasts'));

// ---------- Theme ----------

function safeStorage() {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

const themes = createThemeStore(safeStorage());
let theme = themes.load();

function applyTheme() {
  document.documentElement.setAttribute('data-theme', theme);
  const button = $('theme-toggle');
  const dark = theme === 'dark';
  button.setAttribute('aria-label', dark ? 'Switch to the light theme' : 'Switch to the dark theme');
  setIcon(button.querySelector('svg'), dark ? 'sun' : 'moon');
}

// ---------- The context the views get ----------

const ctx = {
  api,
  mock: api.mock,
  notify,
  state: () => store.state,
  series: () => store.series,
  figures: () => store.figures,
  /** Overview hands over the readings the server already has, so the charts don't start empty. */
  seedSeries(history) {
    if (!history) return;
    if (history.everySecs) store.series.everySecs = history.everySecs;
    if (history.ops.length > store.series.ops.length) store.series.ops = history.ops.slice(-40);
    if (history.clients.length > store.series.clients.length) store.series.clients = history.clients.slice(-40);
  },
  navigate(hash) {
    if (window.location.hash === hash) render();
    else window.location.hash = hash;
  },
  /** Pick a cluster node; the views that read one node follow it. */
  async pickNode(id) {
    try {
      await api.pickNode(id);
    } catch (err) {
      notify(errorText(err), 'error');
    }
  },
  openCommandLine: () => setCommandLine(true),
};

// ---------- Router ----------

function currentRoute() {
  const fallback = store.state.connected || store.state.connecting ? 'overview' : 'connections';
  return parseRoute(window.location.hash, NAMES, fallback);
}

/** What makes a mounted view stale: who is connected, how, and to which node. */
function viewKey() {
  const s = store.state;
  const c = s.conn;
  return [s.connected, s.connecting && s.connecting.name, c && c.name, c && c.env, c && c.readOnly, c && c.pickedNode, c && c.checksOff.length].join('|');
}

function render() {
  if (store.locked) return;
  const route = currentRoute();
  for (const link of document.querySelectorAll('[data-nav]')) {
    const on = link.dataset.nav === route.name;
    link.classList.toggle('on', on);
    if (on) link.setAttribute('aria-current', 'page');
    else link.removeAttribute('aria-current');
  }
  document.title = `${TITLES[route.name]} · redis-lens`;
  const key = viewKey();
  if (mounted && mounted.name === route.name && mounted.sub === route.sub && mounted.key === key) return;
  if (mounted) {
    try {
      mounted.unmount();
    } catch {
      // A view that fails to tidy up must not block the next one.
    }
  }
  const root = fill($('view'));
  const unmount = VIEWS[route.name].mount(root, ctx, route);
  mounted = { name: route.name, sub: route.sub, key, unmount: typeof unmount === 'function' ? unmount : () => {} };
}

// ---------- Chrome ----------

function renderChrome() {
  const s = store.state;
  const pill = $('conn-pill');
  if (s.connected && s.conn) {
    const access = s.conn.readOnly ? 'read-only' : 'writes on';
    fill(pill,
      h('span', { class: 'dot ok', 'aria-hidden': 'true' }),
      h('span', { class: 'mono name clip', text: s.conn.name }),
      envBadge(s.conn.env),
      h('span', { class: 'muted access', text: access }));
    pill.setAttribute('aria-label', `Connected to ${s.conn.name}, ${s.conn.env}, ${access}. Open Connections.`);
  } else if (s.connecting) {
    fill(pill,
      h('span', { class: 'dot busy', 'aria-hidden': 'true' }),
      h('span', { class: 'mono name clip', text: s.connecting.name }),
      h('span', { class: 'muted', text: 'connecting…' }));
    pill.setAttribute('aria-label', `Connecting to ${s.connecting.name}. Open Connections.`);
  } else {
    fill(pill, h('span', { class: 'dot', 'aria-hidden': 'true' }), h('span', { text: 'Not connected' }), h('span', { class: 'muted access', text: 'pick a connection' }));
    pill.setAttribute('aria-label', 'Not connected. Open Connections.');
  }

  const live = $('live-toggle');
  live.disabled = !s.connected;
  live.querySelector('.dot').classList.toggle('ok', s.connected && s.live);
  $('live-label').textContent = s.live ? 'Live' : 'Paused';
  live.setAttribute('aria-label', s.live ? 'Live: pause the live figures' : 'Paused: resume the live figures');

  renderBanner();
}

function renderBanner() {
  const s = store.state;
  const box = $('banner');
  if (!store.linkUp) {
    fill(box, banner(TEXT.network, 'Reconnect', async () => {
      api.restart();
      await loadState();
    }));
  } else if (s.lost) {
    const name = (s.conn && s.conn.name) || store.lastConn;
    fill(box, banner(s.lost, name ? 'Reconnect' : null, async () => {
      try {
        await api.connect(name);
      } catch (err) {
        notify(errorText(err), 'error');
      }
    }));
  } else {
    fill(box);
  }
}

function banner(text, action, run) {
  const button = action ? h('button', {
    type: 'button', class: 'btn', text: action,
    onclick: async () => {
      button.disabled = true;
      try {
        await run();
      } finally {
        button.disabled = false;
      }
    },
  }) : null;
  return h('div', { class: 'banner', role: 'alert' },
    h('p', null, h('span', { class: 'glyph', 'aria-hidden': 'true', text: '✗' }), h('span', { text })),
    button);
}

function showLocked() {
  if (store.locked) return;
  store.locked = true;
  api.stop();
  if (mounted) mounted.unmount();
  mounted = null;
  for (const dlg of document.querySelectorAll('dialog[open]')) dlg.close();
  fill(document.querySelector('.shell'),
    h('main', { class: 'locked', id: 'main' },
      h('div', { class: 'card locked-card' },
        h('h1', { text: 'This page is locked' }),
        h('p', null, 'Open the dashboard from redis-lens: press ', h('span', { class: 'kbd', text: 'ctrl-o' }), ' in the terminal, or run ', h('span', { class: 'kbd', text: 'redis-lens --dashboard' }), '.'),
        h('p', { class: 'muted', text: 'The session this page had is gone: redis-lens was started again, or the page was opened without it.' }))));
  $('conn-pill').hidden = true;
  $('live-toggle').hidden = true;
}

// ---------- The command line panel ----------

function setCommandLine(open) {
  const section = $('cmdline');
  const toggle = $('cmd-toggle');
  section.hidden = !open;
  toggle.setAttribute('aria-expanded', open ? 'true' : 'false');
  toggle.classList.toggle('on', open);
  if (open) {
    commandLine.refresh();
    section.scrollIntoView({ block: 'nearest' });
    commandLine.focus();
  } else if (section.contains(document.activeElement)) {
    toggle.focus();
  }
}

// ---------- State ----------

function applyState(next) {
  const prev = store.state;
  const prevNode = prev.conn && prev.conn.pickedNode;
  const prevName = prev.conn && prev.conn.name;
  store.state = store.pausedHere ? { ...next, live: false } : next;
  if (next.conn) store.lastConn = next.conn.name;
  if (prevName !== (next.conn && next.conn.name) || prevNode !== (next.conn && next.conn.pickedNode)) {
    store.series = { ops: [], clients: [], everySecs: null };
    store.figures = null;
  }
  renderChrome();
  render();
  if (commandLine) commandLine.refresh();
}

async function loadState() {
  try {
    await api.getState();
    return true;
  } catch (err) {
    if (err.locked) showLocked();
    else if (err.network) {
      store.linkUp = false;
      renderBanner();
    } else notify(errorText(err), 'error');
    return false;
  }
}

// ---------- Boot ----------

function boot() {
  applyTheme();
  $('served-at').textContent = window.location.host || '127.0.0.1';
  $('theme-toggle').addEventListener('click', () => {
    theme = themes.other(theme);
    themes.save(theme);
    applyTheme();
  });
  $('live-toggle').addEventListener('click', async () => {
    const want = !store.state.live;
    if (store.pausedHere) {
      store.pausedHere = false;
      applyState({ ...store.state, live: true });
      return;
    }
    try {
      await api.setLive(want);
    } catch (err) {
      if (err.locked || err.network) return;
      // The server could not pause its polling: stop drawing the figures here instead.
      store.pausedHere = !want;
      applyState({ ...store.state, live: want });
    }
  });
  $('cmd-toggle').addEventListener('click', () => setCommandLine($('cmdline').hidden));

  api.setConfirmHandler((confirm) => confirmDialog(confirm));
  api.on('state', applyState);
  api.on('figures', (f) => {
    if (!store.state.live || !store.state.connected) return;
    const last = store.figures;
    if (last && f.at > last.at && !store.series.everySecs) store.series.everySecs = Math.max(1, Math.round((f.at - last.at) / 1000));
    store.figures = f;
    store.series.ops = pushSeries(store.series.ops, f.opsPerSec);
    store.series.clients = pushSeries(store.series.clients, f.clients);
  });
  api.on('notice', (n) => notify(n.text, n.kind));
  api.on('locked', showLocked);
  api.on('link', ({ up }) => {
    if (store.linkUp === up) return;
    store.linkUp = up;
    renderBanner();
    if (up) loadState();
  });

  // The stores that keep running whatever view is shown: the tail, Monitor, the command line.
  pubsub.init(api);
  server.init(api);
  commandLine = cmdline.mount($('cmdline'), ctx, () => setCommandLine(false));

  window.addEventListener('hashchange', render);
  renderChrome();
  loadState().then(() => {
    if (store.locked) return;
    render();
    api.start();
  });
}

boot();
