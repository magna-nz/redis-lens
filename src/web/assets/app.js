// redis-lens dashboard: boot, the hash router, the theme and the chrome around the views.
//
// The views live in views/*.js. Each exports `mount(root, ctx, route)` and returns a
// function that unmounts it. They read data only through `ctx.api` (api.js).

import { api, TEXT, initApi, acceptSignIn, adaptAnalysis } from './api.js';
import {
  h, fill, setIcon, envBadge, createThemeStore, createToasts, confirmDialog,
  parseRoute, pushSeries, errorText, analyzing, plural, menuButton,
  connMenuModel, connButtonModel, accessModel, dbModel, nodeModel, liveModel,
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
  /** Browse reads the database list when it mounts (as it did when the picker was on it):
   *  the top bar's database menu shows that list at once when it is opened next. */
  showDbs(dbs, error) {
    dbCache = { who: dbWho(), dbs, error };
  },
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

/** The database list the menu showed last, for the session and run it was read in. */
let dbCache = null;
let menus = null;

function renderChrome() {
  const s = store.state;

  // The connection button: status dot, name, environment. It opens the connection menu.
  const conn = connButtonModel(s);
  fill($('conn-btn'),
    h('span', { class: conn.dot ? `dot ${conn.dot}` : 'dot', 'aria-hidden': 'true' }),
    conn.name ? h('span', { class: 'mono name clip', text: conn.name }) : null,
    conn.env ? envBadge(conn.env) : null,
    conn.note ? h('span', { class: conn.name ? 'muted' : '', text: conn.note }) : null,
    h('span', { class: 'caret', 'aria-hidden': 'true', text: '▾' }));
  $('conn-btn').setAttribute('aria-label', conn.aria);

  // Access: Read-only or Writes on; a click asks the server to switch (it asks first).
  const access = accessModel(s);
  const accessBtn = $('access-btn');
  accessBtn.hidden = !access.show;
  if (access.show) {
    accessBtn.className = `btn ctx-btn ${access.tone}`;
    fill(accessBtn, h('span', { class: 'access-word', text: access.label }), h('span', { class: 'key-hint', 'aria-hidden': 'true', text: 'w' }));
    accessBtn.setAttribute('aria-label', access.aria);
    accessBtn.setAttribute('aria-pressed', access.on ? 'true' : 'false');
    accessBtn.title = access.title;
  }

  // The database: shown from the state; the list is read when the menu opens.
  const db = dbModel(s);
  $('db-wrap').hidden = !db.show;
  if (db.show) {
    $('db-label').textContent = db.label;
    $('db-btn').setAttribute('aria-label', `Database: ${db.label}. ${db.off ? db.off : 'Pick another database.'}`);
  } else if (menus) menus.db.close(false);

  // The node, on a cluster: the views that read one node read the picked one.
  const node = nodeModel(s);
  $('node-wrap').hidden = !node;
  const select = $('node-select');
  if (node && document.activeElement !== select) {
    fill(select, node.options.map((o) => h('option', { value: o.value, selected: o.selected, text: o.text })));
  }

  // Live / Paused, and how often the figures are read (the TUI's + and -).
  const live = liveModel(s);
  const liveBtn = $('live-toggle');
  liveBtn.disabled = !live.enabled;
  liveBtn.querySelector('.dot').classList.toggle('ok', live.enabled && live.live);
  $('live-label').textContent = live.label;
  liveBtn.setAttribute('aria-label', live.aria);
  const every = $('poll-interval');
  every.disabled = !live.enabled;
  if (every.options.length !== live.options.length) {
    fill(every, live.options.map((o) => h('option', { value: o.value, text: o.text })));
  }
  if (document.activeElement !== every) every.value = String(live.every);

  renderBanner();
}

// ---------- The top bar's menus ----------

/** The connection menu: the saved connections, then the Docker containers, each
 *  connecting on click, then Manage connections. Only names, environments and masked
 *  addresses are drawn: a listing carries no credential, and the URL is never shown. */
function buildConnMenu(close) {
  const menu = $('conn-menu');
  const draw = (saved, docker, dockerNote) => {
    const model = connMenuModel(saved, docker, store.state);
    const item = (it) => h('button', {
      type: 'button', class: 'menu-item', 'aria-current': it.current ? 'true' : null,
      'aria-label': `${it.current && it.status ? `${it.name}, ${it.status}` : `Connect to ${it.name}`}, ${it.env}, ${it.address}`,
      onclick: () => {
        close();
        if (it.current && (it.status === 'connected' || it.status === 'connecting')) return;
        ctx.connect(it.target);
      },
    },
    h('span', { class: it.status === 'connected' ? 'dot ok' : it.status === 'connecting' ? 'dot busy' : it.status === 'failed' ? 'dot bad' : 'dot', 'aria-hidden': 'true' }),
    h('span', { class: 'mono name clip', text: it.name }),
    envBadge(it.env),
    h('span', { class: 'sub mono clip', text: it.address }));
    const kids = [h('p', { class: 'lbl menu-group', text: 'Saved connections' })];
    if (saved === null) kids.push(h('p', { class: 'menu-note', text: 'Reading the saved connections…' }));
    else if (!model.saved.length) kids.push(h('p', { class: 'menu-note', text: 'No saved connections yet.' }));
    else kids.push(...model.saved.map(item));
    kids.push(h('p', { class: 'lbl menu-group', text: 'Found in Docker' }));
    if (dockerNote) kids.push(h('p', { class: 'menu-note', text: dockerNote }));
    else if (docker === null) kids.push(h('p', { class: 'menu-note', text: 'Looking for containers in Docker…' }));
    else if (!model.docker.length) kids.push(h('p', { class: 'menu-note', text: 'No Redis or Valkey container is running.' }));
    else kids.push(...model.docker.map(item));
    kids.push(h('div', { class: 'menu-sep', role: 'presentation' }),
      h('a', { class: 'menu-item plain', href: '#/connections', text: 'Manage connections', onclick: () => close(false) }));
    // Keep the focus across a redraw; until the user moves it, it goes to the first entry.
    const label = (el) => el.getAttribute('aria-label') || el.textContent;
    const had = menu.contains(document.activeElement) ? label(document.activeElement) : null;
    fill(menu, kids);
    const list = Array.from(menu.querySelectorAll('button:not(:disabled), a'));
    const again = had !== null && moved ? list.find((el) => label(el) === had) : null;
    const target = again || (had !== null || !moved ? list[0] : null);
    if (target) target.focus({ preventScroll: true });
  };
  let moved = false;
  menu.onkeydown = () => {
    moved = true;
  };
  menu.onpointerdown = () => {
    moved = true;
  };
  let saved = null;
  let docker = connections.dockerKnown() ? connections.dockerKnown().containers : null;
  let dockerNote = null;
  draw(saved, docker, dockerNote);
  api.listConnections().then(
    (r) => {
      saved = r.profiles;
      if (menus.conn.isOpen()) draw(saved, docker, dockerNote);
    },
    (err) => {
      saved = [];
      if (menus.conn.isOpen()) {
        draw(saved, docker, dockerNote);
        notify(errorText(err), 'error');
      }
    },
  );
  // Discovery pings every container: the menu reuses what Connections found.
  if (docker === null) {
    connections.loadDocker(api).then(
      (found) => {
        docker = found.containers;
        if (menus.conn.isOpen()) draw(saved, docker, dockerNote);
      },
      (err) => {
        dockerNote = `Docker could not be asked: ${errorText(err)}`;
        if (menus.conn.isOpen()) draw(saved, docker, dockerNote);
      },
    );
  }
}

/** The database menu: read (`INFO keyspace`) only now that it is open. */
function buildDbMenu(close) {
  const menu = $('db-menu');
  const model = dbModel(store.state);
  if (!model.show) {
    close(false);
    return;
  }
  if (model.off) {
    fill(menu, h('p', { class: 'menu-note', text: model.off }));
    return;
  }
  const who = dbWho();
  const draw = (d, error, reading) => {
    const kids = [h('p', { class: 'lbl menu-group', text: 'Databases' })];
    if (error) kids.push(h('p', { class: 'menu-note', text: `Database: ${error}` }));
    else if (d && d.off) kids.push(h('p', { class: 'menu-note', text: d.off }));
    else if (d) {
      kids.push(...d.rows.map((r) => h('button', {
        type: 'button', class: 'menu-item', 'aria-current': r.current ? 'true' : null,
        onclick: async (e) => {
          const button = e.currentTarget;
          if (r.current) {
            close();
            return;
          }
          button.disabled = true;
          try {
            const done = await api.switchDb(r.db);
            notify(done.switched ? done.text || `Switched to db ${r.db}.` : `Already on db ${r.db}.`, 'ok');
            dbCache = null;
            close();
          } catch (err) {
            notify(errorText(err), err.cancelled ? 'info' : 'error');
            button.disabled = false;
          }
        },
      },
      h('span', { class: r.current ? 'dot ok' : 'dot', 'aria-hidden': 'true' }),
      h('span', { class: 'mono name', text: `db ${r.db}` }),
      h('span', { class: 'muted small', text: plural(r.keys, 'key', 'keys') }))));
      for (const line of [d.note, d.unread]) if (line) kids.push(h('p', { class: 'menu-note', text: line }));
    }
    if (reading) kids.push(h('p', { class: 'menu-note', role: 'status', text: d ? 'Reading the list again…' : 'Reading the databases…' }));
    // Keep the focus where it was across a redraw; until the user moves it, it goes to the
    // current database.
    const focused = menu.contains(document.activeElement) ? document.activeElement.textContent : null;
    fill(menu, kids);
    const list = Array.from(menu.querySelectorAll('button:not(:disabled)'));
    const again = focused !== null && moved ? list.find((el) => el.textContent === focused) : null;
    const target = again || (focused !== null || !moved ? list.find((el) => el.getAttribute('aria-current') === 'true') || list[0] : null);
    if (target) target.focus({ preventScroll: true });
  };
  let moved = false;
  menu.onkeydown = () => {
    moved = true;
  };
  menu.onpointerdown = () => {
    moved = true;
  };
  const known = dbCache && dbCache.who === who ? dbCache : null;
  draw(known && known.dbs, null, true);
  api.getDbs().then(
    (d) => {
      dbCache = { who, dbs: d, error: null };
      if (menus.db.isOpen() && dbWho() === who) draw(d, null, false);
    },
    (err) => {
      if (menus.db.isOpen() && dbWho() === who) draw(null, errorText(err), false);
    },
  );
}

/** Which session and run a database list belongs to. */
function dbWho() {
  const s = store.state;
  return s.conn ? `${s.conn.name}|${s.conn.docker}|${s.analysis ? s.analysis.run : 0}` : null;
}

/** The access button: writes on or off for the session, through the server's write gate;
 *  turning them on asks first (the confirm handler). */
let switchingWrites = false;
async function toggleWrites() {
  const access = accessModel(store.state);
  if (!access.show || switchingWrites) return;
  // Not disabled while it asks, so the focus stays on the button.
  const button = $('access-btn');
  switchingWrites = true;
  button.setAttribute('aria-busy', 'true');
  try {
    const done = await api.setSessionWrites(access.next);
    notify(done.message || (access.next ? 'Writes are on for this session.' : 'Writes are off for this session.'), 'ok');
    await loadState();
  } catch (err) {
    notify(errorText(err), err.cancelled ? 'info' : 'error');
  } finally {
    switchingWrites = false;
    button.removeAttribute('aria-busy');
  }
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
  hideChrome();
}

/** A locked page, or `?mock` without sample data: the top bar keeps only the brand and the theme. */
function hideChrome() {
  if (menus) {
    menus.conn.close(false);
    menus.db.close(false);
  }
  document.body.classList.remove('drawer-open');
  for (const id of ['ctx', 'live-seg', 'find-form']) $(id).hidden = true;
}

/** `?mock` in a build without the sample servers: say so, and show nothing made up. */
function showNoMock(text) {
  store.locked = true;
  fill(document.querySelector('.shell'),
    h('main', { class: 'locked', id: 'main' },
      h('div', { class: 'card locked-card' },
        h('h1', { text: 'No sample data here' }),
        h('p', { text }))));
  hideChrome();
}

// ---------- The command line panel ----------

// A drawer docked to the bottom of the window on every screen. Opening it moves the focus
// into it without scrolling the page; it never changes the address.
function setCommandLine(open) {
  const section = $('cmdline');
  const toggle = $('cmd-toggle');
  if (!commandLine || !section || !toggle) return;
  const next = cmdline.drawerStep({ open: !section.hidden }, open ? 'open' : 'close', section.contains(document.activeElement));
  section.hidden = !next.open;
  document.body.classList.toggle('drawer-open', next.open);
  toggle.setAttribute('aria-expanded', next.open ? 'true' : 'false');
  toggle.classList.toggle('on', next.open);
  if (next.open) commandLine.refresh();
  if (next.focus === 'input') commandLine.focus();
  else if (next.focus === 'toggle') toggle.focus({ preventScroll: true });
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
  $('brand').title = `Served by the redis-lens running in your terminal at ${window.location.host || '127.0.0.1'}. Only this computer can open it.`;
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

  // The context bar: connection menu, access, database menu, node.
  menus = {
    conn: menuButton($('conn-btn'), $('conn-menu'), buildConnMenu),
    db: menuButton($('db-btn'), $('db-menu'), buildDbMenu),
  };
  $('access-btn').addEventListener('click', toggleWrites);
  $('node-select').addEventListener('change', (e) => ctx.pickNode(Number(e.target.value)));

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
