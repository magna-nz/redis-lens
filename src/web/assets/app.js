// redis-lens dashboard: boot, the hash router, the theme and the chrome around the views.
//
// The views live in views/*.js. Each exports `mount(root, ctx, route)` and returns a
// function that unmounts it. They read data only through `ctx.api` (api.js).

import { api, TEXT, initApi, acceptSignIn, adaptAnalysis } from './api.js';
import {
  h, fill, setIcon, envBadge, createThemeStore, createToasts, confirmDialog,
  parseRoute, pushSeries, errorText, analyzing,
} from './ui.js';
import * as connections from './views/connections.js';
import * as overview from './views/overview.js';
import * as browse from './views/browse.js';
import * as analyze from './views/analyze.js';
import * as server from './views/server.js';
import * as pubsub from './views/pubsub.js';
import * as cmdline from './views/cmdline.js';

const VIEWS = { overview, browse, analyze, server, pubsub, connections };
const NAMES = Object.keys(VIEWS);
const TITLES = { overview: 'Overview', browse: 'Browse', analyze: 'Analyze', server: 'Server', pubsub: 'Pub/Sub', connections: 'Connections' };

const $ = (id) => document.getElementById(id);

const IDLE = {
  status: 'idle', connected: false, connecting: null, failed: null, conn: null, liveState: 'waiting', lost: null,
  tail: null, monitor: null, features: { entraId: false, version: null },
  polling: { paused: false, intervalSecs: 2, intervals: [1, 2, 3, 5, 10, 15, 30, 60] }, live: true,
  monitorLimits: { maxLines: 10000, maxSecs: 30 }, analysis: adaptAnalysis(null),
};

const store = {
  /** The server's state. `live` and `polling` are the dashboard's, shared by every tab. */
  state: { ...IDLE },
  series: { ops: [], clients: [], everySecs: IDLE.polling.intervalSecs },
  figures: null,
  /** Why the live figures are off (the server refused INFO), in its words. */
  figuresOff: null,
  linkUp: true,
  locked: false,
  /** What Reconnect connects to again: `{ name }` or `{ docker }`. */
  lastTarget: null,
  /** The connect this tab started, until it is connected or has failed. */
  intent: null,
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
  /** Whether the data is the sample servers' (`?mock`). */
  get mock() {
    return api.mock;
  },
  notify,
  state: () => store.state,
  series: () => store.series,
  figures: () => store.figures,
  figuresOff: () => store.figuresOff,
  navigate(hash) {
    if (window.location.hash === hash) render();
    else window.location.hash = hash;
  },
  /** Connect to `{ name }` or `{ docker }`. The outcome arrives as state: this tab then
   *  lands on Overview, or says why the connect failed. */
  async connect(target) {
    store.intent = target.docker || target.name;
    try {
      await api.connect(target);
    } catch (err) {
      store.intent = null;
      notify(errorText(err), err.cancelled ? 'info' : 'error');
    }
  },
  /** Pick a cluster node; the views that read one node follow it. */
  async pickNode(index) {
    try {
      await api.pickNode(index);
    } catch (err) {
      notify(errorText(err), 'error');
    }
  },
  openCommandLine: () => setCommandLine(true),
  /** Find keys by name (the search field in the top bar): the results show on Browse. */
  findKey(text) {
    const q = String(text ?? '').trim();
    if (!q) return;
    browse.requestFind(q);
    ctx.navigate('#/browse');
  },
  /** Open a key's value (`Name`), or a pattern's keys, on Browse. */
  openKey(name) {
    browse.requestKey(name);
    ctx.navigate('#/browse');
  },
  openPattern(name, node = null) {
    browse.requestPattern(name, node);
    ctx.navigate('#/browse');
  },
  /** Start an analysis from another area (Overview's buttons): Analyze asks first, with
   *  its estimate, as the TUI does. */
  analyze(depth) {
    analyze.requestStart(depth);
    ctx.navigate('#/analyze');
  },
};

// ---------- Router ----------

function currentRoute() {
  const fallback = store.state.connected || store.state.connecting ? 'overview' : 'connections';
  return parseRoute(window.location.hash, NAMES, fallback);
}

/** The connection's status and live state as the areas see them: while an analysis runs it
 *  connects again, and the areas stay as they are meanwhile (they follow the analysis). */
function steady(s) {
  if (!analyzing(s)) return { status: s.status, live: s.liveState };
  return { status: 'connected', live: s.liveState === 'closed' ? 'closed' : 'ready' };
}

/** What makes a mounted view stale: who is connected, how, and to which node. */
function viewKey() {
  const s = store.state;
  const c = s.conn;
  const now = steady(s);
  return [now.status, s.failed, now.live, c && c.name, c && c.docker, c && c.env, c && c.readOnly, c && c.pickedNode].join('|');
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
  const db = s.analysis && s.analysis.db ? `db ${s.analysis.db}` : null;
  if ((s.connected || analyzing(s)) && s.conn) {
    const access = analyzing(s) ? 'analyzing…' : s.conn.readOnly ? 'read-only' : 'writes on';
    fill(pill,
      h('span', { class: s.lost || analyzing(s) ? 'dot busy' : 'dot ok', 'aria-hidden': 'true' }),
      h('span', { class: 'mono name clip', text: s.conn.name }),
      envBadge(s.conn.env),
      db ? h('span', { class: 'badge mono', text: db }) : null,
      h('span', { class: 'muted access', text: access }));
    pill.setAttribute('aria-label', `Connected to ${s.conn.name}, ${s.conn.env}${db ? `, ${db}` : ''}, ${access}. Open Connections.`);
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

  // How often the figures are read: the TUI's + and -, as one quiet list.
  const every = $('poll-interval');
  every.disabled = !s.connected;
  if (every.options.length !== s.polling.intervals.length) {
    fill(every, s.polling.intervals.map((n) => h('option', { value: String(n), text: `every ${n} s` })));
  }
  if (document.activeElement !== every) every.value = String(s.polling.intervalSecs);

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
    fill(box, banner(s.lost, store.lastTarget ? 'Reconnect' : null, () => ctx.connect(store.lastTarget)));
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
  $('poll-interval').hidden = true;
  $('find-form').hidden = true;
}

/** `?mock` in a build without the sample servers: say so, and show nothing made up. */
function showNoMock(text) {
  store.locked = true;
  fill(document.querySelector('.shell'),
    h('main', { class: 'locked', id: 'main' },
      h('div', { class: 'card locked-card' },
        h('h1', { text: 'No sample data here' }),
        h('p', { text }))));
  $('conn-pill').hidden = true;
  $('live-toggle').hidden = true;
  $('poll-interval').hidden = true;
  $('find-form').hidden = true;
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

const targetOf = (conn) => (conn.docker ? { docker: conn.name } : { name: conn.name });

function applyState(next) {
  const prev = store.state;
  const was = prev.conn;
  const now = next.conn;
  store.state = next;
  if (now && (next.connected || next.connecting)) store.lastTarget = targetOf(now);
  // The bars are one reading apart: start them again when what they show changes (another
  // connection or node) or when the readings get another spacing.
  const moved = (was && was.name) !== (now && now.name) || (was && was.pickedNode) !== (now && now.pickedNode) || steady(prev).status !== steady(next).status;
  if (moved || prev.polling.intervalSecs !== next.polling.intervalSecs) {
    store.series = { ops: [], clients: [], everySecs: next.polling.intervalSecs };
    if (moved) {
      store.figures = null;
      store.figuresOff = null;
    }
  }
  renderChrome();
  render();
  if (commandLine) commandLine.refresh();
  if (prev.live !== next.live || prev.polling.intervalSecs !== next.polling.intervalSecs) api.emit('live', { live: next.live });

  // The outcome of a connect this tab started.
  if (store.intent && now && now.name === store.intent) {
    if (next.connected) {
      store.intent = null;
      notify(`Connected to ${now.name}. No key was read.`, 'ok');
      if (currentRoute().name === 'connections') ctx.navigate('#/overview');
    } else if (next.failed) {
      store.intent = null;
      notify(`The connect to ${now.name} failed: ${next.failed}`, 'error');
    }
  } else if (store.intent && next.status === 'idle') {
    store.intent = null;
  }
}

function applyFigures(f) {
  if (!store.state.live || !store.state.connected) return;
  if (f.error) {
    store.figuresOff = f.error;
    return;
  }
  store.figuresOff = null;
  store.figures = f;
  store.series.ops = pushSeries(store.series.ops, f.opsPerSec);
  store.series.clients = pushSeries(store.series.clients, f.clients);
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

async function boot() {
  // First of all: the sign-in token leaves the address bar (initApi takes it at once).
  const ready = initApi(window);
  // The sign-in address opened again in this tab (a locked one, say) only changes the hash:
  // take the token and start the page over with it.
  window.addEventListener('hashchange', () => {
    if (acceptSignIn(window)) window.location.reload();
  });
  applyTheme();
  $('served-at').textContent = window.location.host || '127.0.0.1';
  $('theme-toggle').addEventListener('click', () => {
    theme = themes.other(theme);
    themes.save(theme);
    applyTheme();
  });
  // Live / Paused and the interval are the dashboard's, for every tab: the server stops
  // or slows its poll, and says so in the state that follows.
  const setPolling = async (change) => {
    try {
      const polling = await api.setPolling(change);
      applyState({ ...store.state, polling, live: !polling.paused });
    } catch (err) {
      notify(errorText(err), 'error');
      renderChrome();
    }
  };
  $('live-toggle').addEventListener('click', () => setPolling({ paused: store.state.live }));
  $('poll-interval').addEventListener('change', (e) => setPolling({ intervalSecs: Number(e.target.value) }));
  $('cmd-toggle').addEventListener('click', () => setCommandLine($('cmdline').hidden));

  // Find a key, from anywhere: the results show on Browse. `/` comes here from any area
  // when no text box has the focus.
  $('find-form').addEventListener('submit', (e) => {
    e.preventDefault();
    const text = $('find-input').value.trim();
    if (!text) return;
    if (!store.state.connected && !analyzing(store.state)) {
      notify('Connect first: open Connections and pick a connection. Then keys can be found.', 'info');
      return;
    }
    ctx.findKey(text);
  });
  document.addEventListener('keydown', (e) => {
    if (e.key !== '/' || e.ctrlKey || e.metaKey || e.altKey || store.locked) return;
    const t = e.target;
    if (t && (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName) || t.closest('dialog'))) return;
    e.preventDefault();
    $('find-input').focus();
    $('find-input').select();
  });

  // The stores that keep running whatever view is shown (the tail, Monitor) hear about a
  // new state first, so a view drawn for it finds them up to date.
  pubsub.init(api);
  server.init(api);
  analyze.init(api);

  api.setConfirmHandler((confirm) => confirmDialog(confirm));
  api.on('state', applyState);
  api.on('figures', applyFigures);
  api.on('notice', (n) => notify(n.text, n.kind));
  api.on('locked', showLocked);
  api.on('link', ({ up }) => {
    if (store.linkUp === up) return;
    store.linkUp = up;
    renderBanner();
    if (up) loadState();
  });

  const mode = await ready;
  if (mode.error) {
    showNoMock(mode.error);
    return;
  }
  // Made-up data must never pass for a server's: the badge stays while it is shown.
  $('sample-badge').hidden = !mode.mock;

  commandLine = cmdline.mount($('cmdline'), ctx, () => setCommandLine(false));

  window.addEventListener('hashchange', render);
  // The event stream is closed when the page goes away (or into the back-forward cache),
  // and opened again if it comes back from there.
  window.addEventListener('pagehide', () => api.stop());
  window.addEventListener('pageshow', (e) => {
    if (!e.persisted || store.locked) return;
    api.restart();
    loadState();
  });
  renderChrome();
  loadState().then(() => {
    if (store.locked) return;
    render();
    api.start();
  });
}

boot();
