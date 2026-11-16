// redis-lens dashboard: boot, the hash router, the theme and the chrome around the views.
//
// The views live in views/*.js. Each exports `mount(root, ctx, route)` and returns a
// function that unmounts it. They read data only through `ctx.api` (api.js).

import { api, TEXT, initApi, acceptSignIn, adaptAnalysis } from './api.js';
import {
  h, fill, setIcon, envBadge, createThemeStore, createToasts, confirmDialog,
  parseRoute, pushSeries, errorText, analyzing, plural, menuButton,
  connMenuModel, connButtonModel, accessModel, dbModel, nodeModel, liveModel,
  dbListKey, dbListReply, menuFocus, openDialog, patternName, icon, sessionMark, STALE_TEXT,
} from './ui.js';
import {
  ACTS, actsOf, keyOf, keyLabel, dispatch, hintFor, listMove, paneStep, slashTarget, selectedRow,
  screenOf, areaHash, screenList, screenActions, sheetModel, gotoModel, gotoFlat, gotoStep, gotoDefault,
  openStep, keyVerdict,
} from './keys.js';
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
  /** Go to `hash`, a step in the history. `data`: what the new entry carries besides its
   *  address (Browse's key type), in `history.state`, never in the address. */
  navigate(hash, data = null) {
    if (window.location.hash === hash) render();
    else {
      window.location.hash = hash;
      // The hash change is a step already, synchronously: its entry is the one marked.
      if (data) window.history.replaceState({ ...data }, '');
    }
  },
  /** A step in the history to `hash` without drawing anything again (the view shows it
   *  already): Browse's new step after an entry it must not rewrite. */
  pushRoute(hash) {
    window.history.pushState(null, '', hash);
  },
  /** The route shown. */
  currentRoute: () => currentRoute(),
  /** What the current history entry carries besides its address (`navigate`'s `data`). */
  routeData: () => {
    const s = window.history.state;
    return s && typeof s === 'object' ? s : null;
  },
  /** The top bar's access button: turn writes on or off (the server asks first). */
  toggleWrites: () => toggleWrites(),
  /** Put `hash` in the address in place of the current one: no step in the history, and
   *  nothing is drawn again (the view shows it already). The page's own query (`?mock`)
   *  stays. */
  replaceRoute(hash) {
    if (window.location.hash === hash) return;
    window.history.replaceState(window.history.state, '', hash);
  },
  /** The session a history entry's address was shown for (Browse marks its entries), so
   *  Back to an entry of another connection or database is not applied to this one. Null
   *  for an entry never shown (a link, a page just opened). Only this tab's history holds
   *  it; it is never in the address. */
  routeTag: () => {
    const s = window.history.state;
    return s && typeof s.view === 'string' ? s.view : null;
  },
  setRouteTag(tag) {
    const s = window.history.state;
    if (s && s.view === tag) return;
    window.history.replaceState({ ...(s && typeof s === 'object' ? s : {}), view: tag }, '');
  },
  /** Move the focus into a pane, on its selected row (Browse's breadcrumb). */
  focusPane: (pane) => focusPane(pane),
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
   *  the top bar's database menu shows that list at once when it is opened next. Browse
   *  takes `dbListKey()` before it asks and hands it back with the reply, so a late reply
   *  for another session or run is dropped. */
  dbListKey: () => dbListKey(store.state),
  showDbs(sentKey, dbs, error) {
    const kept = dbListReply(sentKey, dbListKey(store.state), dbs, error);
    if (kept) dbCache = kept;
  },
  /** Find keys by name (Go to's Find row): the results show on Browse. Each of these jumps
   *  is an address (browse.js `hrefFor`), so it is a step in the history, and Browse reads
   *  what the address names when it shows it. */
  findKey(text) {
    const q = String(text ?? '').trim();
    if (!q) return;
    toBrowse(browse.hrefFor({ find: q }, store.state));
  },
  /** Open a key's value (`Name`), or a pattern's keys, on Browse. A key's type, when this
   *  area knows it, goes with the step (not in the address), so its value is read as a
   *  click on its row reads it. */
  openKey(name) {
    toBrowse(browse.hrefFor({ k: name.id }, store.state), typeof name.type === 'string' && name.type ? { keyType: { id: name.id, type: name.type } } : null);
  },
  openPattern(name, node = null) {
    const change = name ? { p: name.id } : {};
    if (node !== null) change.node = node;
    toBrowse(browse.hrefFor(change, store.state));
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

/** Another area's jump to Browse › Keys. To the address shown already (Find with the same
 *  text, the open key again), Browse reads again what failed instead: no step. */
function toBrowse(href, data = null) {
  if (window.location.hash === href && browse.reopen()) return;
  ctx.navigate(href, data);
}

/** A link to the screen shown that would only add a second history entry for it (the
 *  rail's Browse link, the Keys tab, on Browse › Keys): it does nothing (browse.js
 *  `linkStays`). Other links go as usual. */
function stayOnLinks(e) {
  if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
  const a = e.target && e.target.closest ? e.target.closest('a[href^="#/"]') : null;
  if (!a) return;
  if (browse.linkStays(currentRoute(), parseRoute(a.getAttribute('href'), NAMES, null))) e.preventDefault();
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
  let route = currentRoute();
  // A route written another way (`#/browse/keys`): written as the screen's own address, in
  // place, so the screen shown is not drawn again for it.
  const canonical = typeof VIEWS[route.name].canonicalHref === 'function' ? VIEWS[route.name].canonicalHref(route) : null;
  if (canonical) {
    window.history.replaceState(window.history.state, '', canonical);
    route = currentRoute();
  }
  for (const link of document.querySelectorAll('[data-nav]')) {
    const on = link.dataset.nav === route.name;
    link.classList.toggle('on', on);
    if (on) link.setAttribute('aria-current', 'page');
    else link.removeAttribute('aria-current');
  }
  document.title = `${TITLES[route.name]} · redis-lens`;
  const key = viewKey();
  if (mounted && mounted.name === route.name && mounted.sub === route.sub && mounted.key === key) {
    // The same screen with another query (Browse's pattern, key, Find): the view shows it,
    // reading only what changed. It is not torn down.
    if (typeof VIEWS[route.name].onRoute === 'function') VIEWS[route.name].onRoute(route);
    return;
  }
  // Another screen: the row the focus was on is not looked for there.
  if (!mounted || mounted.name !== route.name || mounted.sub !== route.sub) lastRow = null;
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
  applyHints();
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
    // Its key (w) is drawn from `data-hint`, as on every control with an action.
    fill(accessBtn, h('span', { class: 'access-word', text: access.label }));
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
      type: 'button', class: 'menu-item', 'aria-current': it.current ? 'true' : null, 'data-key': it.key,
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
      h('a', { class: 'menu-item plain', href: '#/connections', 'data-key': 'manage', text: 'Manage connections', onclick: () => close(false) }));
    redrawMenu(menu, kids, moved);
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
  const who = dbListKey(store.state);
  const draw = (d, error, reading) => {
    const kids = [h('p', { class: 'lbl menu-group', text: 'Databases' })];
    if (error) kids.push(h('p', { class: 'menu-note', text: `Database: ${error}` }));
    if (d && d.off) kids.push(h('p', { class: 'menu-note', text: d.off }));
    else if (d) {
      kids.push(...d.rows.map((r) => h('button', {
        type: 'button', class: 'menu-item', 'aria-current': r.current ? 'true' : null, 'data-key': `db${r.db}`,
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
    redrawMenu(menu, kids, moved);
  };
  let moved = false;
  menu.onkeydown = () => {
    moved = true;
  };
  menu.onpointerdown = () => {
    moved = true;
  };
  const known = dbCache && dbCache.who === who ? dbCache : null;
  draw(known && known.dbs, known && known.error, true);
  const settle = (dbs, error) => {
    const kept = dbListReply(who, dbListKey(store.state), dbs, error);
    if (!kept) return;
    dbCache = kept;
    if (menus.db.isOpen()) draw(kept.dbs, kept.error, false);
  };
  api.getDbs().then((d) => settle(d, null), (err) => settle(null, errorText(err)));
}

/** Draw a menu's items again, keeping the focus where `menuFocus` says: on the item that
 *  had it once the user has moved, else on the current item (the current database), else
 *  on the first. Items carry `data-key`. */
function redrawMenu(menu, kids, moved) {
  const active = document.activeElement;
  const had = active && menu.contains(active) && active.dataset ? active.dataset.key ?? null : null;
  fill(menu, kids);
  const list = Array.from(menu.querySelectorAll('[data-key]'));
  const at = menuFocus(list.map((el) => ({ key: el.dataset.key, current: el.getAttribute('aria-current') === 'true', disabled: el.disabled === true })), { had, moved });
  if (at >= 0) list[at].focus({ preventScroll: true });
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
    notify(done.message || (access.next ? 'Writes are on.' : 'Writes are off.'), 'ok');
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
    h('p', null, h('span', { class: 'glyph bad', 'aria-hidden': 'true', text: '✗' }), h('span', { text })),
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

// ---------- Keys ----------

// The key grammar (docs/ui-redesign.md §5), from the one table in keys.js. A key does
// what clicking its control does and nothing more: a "press" action clicks the control that
// carries `data-act`, so a write goes through the server's write gate and its question as
// a click would. No key runs while the focus is in a text box or a dialog or menu is open.

const visible = (el) => !!el && el.isConnected && el.getClientRects().length > 0;
const paneOf = (el) => (el && el.closest ? el.closest('[data-pane]') : null);
const rowsOf = (pane) => Array.from(pane.querySelectorAll('[data-row]')).filter(visible);
const panesShown = () => Array.from($('view').querySelectorAll('[data-pane]')).filter(visible);
const isOpenRow = (r) => r.getAttribute('aria-current') === 'true' || r.getAttribute('aria-pressed') === 'true' || r.classList.contains('on') || !!r.closest('tr.on');
/** A row that only groups others (a folder of patterns, a group of clients). */
const isGroupRow = (r) => r.hasAttribute('data-group');
/** A pane's selected row (the one Tab reaches), or null. */
const selectedOf = (pane) => (pane ? rowsOf(pane).find((r) => r.classList.contains('sel')) || null : null);
/** A row as keys.js's decisions see it. */
const rowFacts = (r) => (r ? { open: isOpenRow(r), group: isGroupRow(r) } : null);

/** The row the focus is on, or the row of the list item it is in (a row's own tools). */
function focusedRow(el) {
  if (!el || !el.closest) return null;
  const row = el.closest('[data-row]');
  if (row) return row;
  const item = el.closest('li, tr');
  return item ? item.querySelector('[data-row]') : null;
}

/** Whether the focus is where text is typed: then no key is taken. */
function typing(el) {
  return !!el && el !== document.body && (el.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName));
}

/** A dialog or menu that handles its own keys, or the command-line drawer with the focus. */
function openLayer(el) {
  if (document.querySelector('dialog[open]')) return 'dialog';
  if (menus && (menus.conn.isOpen() || menus.db.isOpen())) return 'menu';
  const drawer = $('cmdline');
  if (drawer && el && drawer.contains(el)) return 'drawer';
  return null;
}

const isCluster = () => {
  const s = store.state;
  return !!(s.conn && (s.conn.topology === 'cluster' || (s.conn.nodes || []).length > 1 || (s.analysis && s.analysis.report && s.analysis.report.cluster)));
};

// ----- Lists: one selected row each, the browser's focus and the selection the same -----

/** The row each pane has selected, by its `data-row`, kept while the pane is drawn again. */
const paneSel = new Map();
/** The row the focus was last on, to put it back when its list is drawn again. */
let lastRow = null;

/** A roving tabindex: the selected row is the one Tab reaches; the others take the focus
 *  only from the keys or a click. */
function syncPane(pane) {
  const rows = rowsOf(pane);
  const id = pane.dataset.pane;
  const at = selectedRow(rows.map((r) => ({ key: r.dataset.row, open: isOpenRow(r) })), paneSel.has(id) ? paneSel.get(id) : null);
  rows.forEach((r, i) => {
    const on = i === at;
    // The attribute itself: a table row's tabIndex is -1 without one, and then the keys
    // could not move the focus onto it.
    const want = on ? '0' : '-1';
    if (r.getAttribute('tabindex') !== want) r.setAttribute('tabindex', want);
    r.classList.toggle('sel', on);
  });
}

function focusRow(pane, row) {
  paneSel.set(pane.dataset.pane, row.dataset.row);
  syncPane(pane);
  row.focus();
}

/** Into a pane: on its selected row, or on the pane itself when it has no rows. */
function focusPane(pane) {
  const rows = rowsOf(pane);
  if (rows.length) {
    syncPane(pane);
    focusRow(pane, rows.find((r) => r.tabIndex === 0) || rows[0]);
    return;
  }
  if (!pane.hasAttribute('tabindex')) pane.tabIndex = -1;
  pane.focus();
}

function listMoveTo(act) {
  const pane = paneOf(document.activeElement);
  if (!pane) return;
  const rows = rowsOf(pane);
  if (!rows.length) return;
  const cur = rows.indexOf(focusedRow(document.activeElement));
  const at = cur >= 0 ? cur : rows.findIndex((r) => r.tabIndex === 0);
  const page = Math.max(1, Math.floor((pane.clientHeight || 300) / 28) - 2);
  const next = listMove(at, rows.length, act, page);
  if (next >= 0) focusRow(pane, rows[next]);
}

/** Enter, → or l: open the row, through the row itself or its `[data-open]` control
 *  (never a write); on the row that is open already, go to the pane on the right
 *  (keys.js openStep decides). Moving never opens a row: only this, or a click, does. */
function listOpen(key) {
  const el = document.activeElement;
  const pane = paneOf(el);
  if (!pane) return;
  const row = focusedRow(el) || selectedOf(pane);
  if (!row) return;
  const panes = panesShown();
  const next = paneStep(panes.length, panes.indexOf(pane), 1);
  // A pane hidden to the right (Browse on a narrow window): its breadcrumb part shows it.
  const forward = next < 0 ? Array.from($('view').querySelectorAll('[data-next]')).find(visible) : null;
  const opens = Array.from(row.querySelectorAll('[data-open]')).filter(visible);
  const step = openStep(key, {
    open: isOpenRow(row),
    control: row.matches('button, a[href]'),
    controlAct: row.dataset.act || null,
    opens: opens.map((o) => ({ act: o.dataset.act || null, disabled: !!o.disabled })),
    right: next >= 0 || !!forward,
  });
  if (!step) return;
  if (step.do === 'right') {
    if (next >= 0) focusPane(panes[next]);
    else forward.click();
  } else if (step.do === 'row') row.click();
  else opens[step.index].click();
}

/** Esc, ← or h: the pane on the left, else the screen's way back (Clients' groups). */
function listBack() {
  const pane = paneOf(document.activeElement);
  const panes = panesShown();
  const prev = paneStep(panes.length, panes.indexOf(pane), -1);
  if (prev >= 0) {
    focusPane(panes[prev]);
    return;
  }
  const back = Array.from($('view').querySelectorAll('[data-back]')).find(visible);
  if (back) back.click();
}

// ----- Pressing a control -----

/** A control a key may press: shown, or inside a closed popover (`details.pop`, such as
 *  Browse's "About this pattern") whose own button is shown, as a click would reach it. */
function reachable(el) {
  if (visible(el)) return true;
  const pop = el.closest('details.pop');
  if (!pop || pop.open) return false;
  const summary = pop.querySelector(':scope > summary');
  return visible(summary) && !pop.parentElement.closest('details.pop:not([open])');
}

/** The controls carrying `data-act="<act>"` where the action looks: the selected row
 *  first, then the focused pane, then the screen, then the page, as far as `scope` goes. */
function findAct(act, scope, pane, row) {
  const pick = (root) => Array.from(root.querySelectorAll(`[data-act="${act}"]`)).filter(reachable);
  if (row) {
    const got = pick(row.closest('li, tr') || row);
    if (got.length) return got;
  }
  if (scope === 'row') return [];
  if (pane) {
    const got = pick(pane);
    if (got.length || scope === 'pane') return got;
  }
  const got = pick($('view'));
  if (got.length || scope !== 'page') return got;
  return pick(document);
}

/** Press a control as a click does: a button or a link is clicked, a `<summary>` opens its
 *  form and puts the focus in it, a text box or select gets the focus, a checkbox is ticked
 *  and the focus goes to its form's text box, and a row of pills moves to the next pill. */
function press(found, entry) {
  const pills = found.filter((el) => el.classList.contains('pill'));
  if (found.length > 1 && pills.length === found.length) {
    const at = pills.findIndex((el) => el.getAttribute('aria-pressed') === 'true' || el.getAttribute('aria-current') === 'true');
    const next = pills[(at + 1) % pills.length];
    if (!next.disabled) next.click();
    return;
  }
  const el = found[0];
  if (el.disabled) {
    notify(el.title || `${entry.name}: off now.`, 'info');
    return;
  }
  if (el.tagName === 'SUMMARY') {
    const box = el.parentElement;
    if (box && !box.open) box.open = true;
    const field = box && box.querySelector('input:not([type="hidden"]):not([type="checkbox"]), select, textarea');
    (field || el).focus();
    return;
  }
  if (el.tagName === 'SELECT' || el.tagName === 'TEXTAREA') {
    el.focus();
    return;
  }
  if (el.tagName === 'INPUT') {
    if (el.type === 'checkbox') {
      if (!el.checked) el.click();
      const form = el.closest('form');
      const text = form && form.querySelector('input[type="text"], input[type="search"]');
      (text || el).focus();
      return;
    }
    el.focus();
    if (typeof el.select === 'function') el.select();
    return;
  }
  el.click();
}

/** The control an entry would press, as keys.js's verdict sees it: a row of pills is off
 *  only when every pill is. */
function controlFacts(found) {
  if (!found.length) return null;
  const pills = found.filter((el) => el.classList.contains('pill'));
  if (found.length > 1 && pills.length === found.length) return { disabled: pills.every((el) => el.disabled), title: pills[0].title || null };
  return { disabled: !!found[0].disabled, title: found[0].title || null };
}

/** Where an entry acts and what it would press. `where` names the pane and row (Go to runs
 *  an action of a pane the focus is not in); by default, where the focus is. The row the
 *  verdict checks is the focused row, else the pane's selected row, so a key pressed from a
 *  pane's own button ("Load more") still needs the selected row to be the open one. The
 *  control is looked for in the focused row only, never in a row the focus is not on. */
function entryPlan(entry, where = null) {
  const el = document.activeElement;
  const pane = where ? where.pane : paneOf(el);
  const row = where ? where.row : focusedRow(el);
  if (ACTS[actsOf(entry)[0]].run === 'app') return { pane, found: [], facts: { pane: null, row: null, control: null } };
  const gateRow = row || selectedOf(pane);
  let found = [];
  for (const act of actsOf(entry)) {
    found = findAct(act, ACTS[act].in, pane, row);
    if (found.length) break;
  }
  return { pane, found, facts: { pane: pane ? pane.dataset.pane : null, row: rowFacts(gateRow), control: controlFacts(found) } };
}

/** Run an entry of the key table, if keys.js's verdict lets it. Returns whether the key
 *  was taken (false leaves it to the browser). */
function runEntry(entry, where = null, { repeat = false, key = null } = {}) {
  const plan = entryPlan(entry, where);
  const verdict = keyVerdict(entry, { ...plan.facts, repeat });
  if (!verdict.run) {
    if (verdict.say) notify(verdict.say, 'info');
    return !verdict.pass;
  }
  const acts = actsOf(entry);
  if (ACTS[acts[0]].run === 'app') APP[acts[0]](entry, key);
  else press(plan.found, entry);
  return true;
}

/** Whether an entry's control is there and on, for Go to's Actions: null, or why not. */
function entryState(entry, pane, row) {
  const plan = entryPlan(entry, { pane, row });
  const verdict = keyVerdict(entry, plan.facts);
  if (verdict.run) return null;
  return verdict.say || 'Not on this screen now.';
}

// ----- The page's own actions -----

function goArea(digit) {
  const hash = areaHash(digit, currentRoute(), { cluster: isCluster() });
  if (hash && !browse.linkStays(currentRoute(), parseRoute(hash, NAMES, null))) ctx.navigate(hash);
}

/** `/`: the focused list's filter, else the screen's own box (Search's query), else Go to. */
function slash() {
  const pane = paneOf(document.activeElement);
  const inPane = pane ? Array.from(pane.querySelectorAll('[data-act="filter"]')).find(visible) : null;
  const onScreen = Array.from($('view').querySelectorAll('[data-act="filter"]')).find((el) => visible(el) && !paneOf(el));
  const to = slashTarget({ paneFilter: !!inPane, screenFilter: !!onScreen });
  if (to === 'goto') {
    openGoto();
    return;
  }
  const box = to === 'pane' ? inPane : onScreen;
  box.focus();
  if (typeof box.select === 'function') box.select();
}

/** `[` and `]`: the previous or next node on a cluster, as the node select picks one. */
function stepNode(dir) {
  const select = $('node-select');
  if ($('node-wrap').hidden || select.options.length < 2) {
    notify('There is one node here: no other to pick.', 'info');
    return;
  }
  const n = select.options.length;
  select.selectedIndex = (select.selectedIndex + dir + n) % n;
  ctx.pickNode(Number(select.value));
}

/** The handlers of the table's `app` actions (tests/keys.test.js checks there is one for each). */
const APP = {
  'list.down': () => listMoveTo('list.down'),
  'list.up': () => listMoveTo('list.up'),
  'list.pageDown': () => listMoveTo('list.pageDown'),
  'list.pageUp': () => listMoveTo('list.pageUp'),
  'list.home': () => listMoveTo('list.home'),
  'list.end': () => listMoveTo('list.end'),
  'list.open': (entry, key) => listOpen(key),
  'list.back': () => listBack(),
  'area.overview': () => goArea('1'),
  'area.browse': () => goArea('2'),
  'area.analyze': () => goArea('3'),
  'area.server': () => goArea('4'),
  'area.pubsub': () => goArea('5'),
  'area.connections': () => goArea('0'),
  'filter': () => slash(),
  'goto': () => openGoto(),
  'sheet': () => openSheet(),
  'cmdline': () => setCommandLine(true),
  'node.prev': () => stepNode(-1),
  'node.next': () => stepNode(1),
};

function onKey(e) {
  if (store.locked || e.defaultPrevented || e.isComposing) return;
  const key = keyOf(e);
  if (!key) return;
  const el = document.activeElement;
  const pane = paneOf(el);
  const row = el && el.closest ? el.closest('[data-row]') : null;
  const entry = dispatch(key, {
    screen: screenOf(currentRoute()),
    pane: pane ? pane.dataset.pane : null,
    inList: !!(pane && rowsOf(pane).length),
    inInput: typing(el),
    dialog: openLayer(el),
    native: !!(el && el !== document.body && el.matches && el.matches('button, a[href], summary')),
    onRow: !!row,
  });
  if (!entry) return;
  if (runEntry(entry, null, { repeat: e.repeat, key })) e.preventDefault();
}

// ----- Hints: each control with an action shows its key, from the table -----

/** Whether this is a Mac, where Go to's key is shown as cmd-k. */
const MAC = /Mac|iPhone|iPad/.test((navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || '');

/** Set `data-hint` (drawn by app.css when `keys-on`: the one way a key is shown on a
 *  control) and `aria-keyshortcuts` on every control with `data-act`, for the screen and
 *  pane it is in. Shown once: not on a control whose text is the key already (`?`), and
 *  for a row of pills on the group's label (`.hint-label[data-hint-of]`, Browse's "Show
 *  as"), not on each pill. Attributes only, so a control's text (which the views use to
 *  put the focus back) never changes. */
function applyHints(roots = null) {
  const screen = screenOf(currentRoute());
  const els = roots ? roots.flatMap((r) => [...(r.matches('[data-act]') ? [r] : []), ...r.querySelectorAll('[data-act]')]) : document.querySelectorAll('[data-act]');
  for (const el of els) {
    const pane = paneOf(el);
    const hint = hintFor(el.dataset.act, screen, pane ? pane.dataset.pane : null);
    if (!hint) {
      el.removeAttribute('data-hint');
      el.removeAttribute('aria-keyshortcuts');
      continue;
    }
    if (el.getAttribute('aria-keyshortcuts') !== hint.aria) el.setAttribute('aria-keyshortcuts', hint.aria);
    const label = MAC && el.dataset.act === 'goto' ? 'cmd-k' : hint.label;
    const group = el.classList.contains('pill') && el.parentElement ? el.parentElement.querySelector(`:scope > .hint-label[data-hint-of="${el.dataset.act}"]`) : null;
    if (group) {
      if (group.dataset.hint !== label) group.dataset.hint = label;
      el.removeAttribute('data-hint');
    } else if (el.textContent.trim() === label) el.removeAttribute('data-hint');
    else if (el.dataset.hint !== label) el.dataset.hint = label;
  }
}

/** After the views draw: each list's selection, the hints, and the focus put back on the
 *  row it was on when that row was drawn anew. Only the parts that changed are looked at,
 *  and a change inside a list's own rows that adds or takes no row (a Pub/Sub message
 *  in the tail, a figure) costs nothing. */
function afterDraw(records) {
  const roots = [];
  for (const r of records || []) {
    const t = r.target;
    const touched = [t, ...r.addedNodes, ...r.removedNodes].some((n) => n.nodeType === 1 && (n.matches('[data-row], [data-pane], [data-act]') || n.querySelector('[data-row], [data-pane], [data-act]')));
    if (!touched) continue;
    if (!roots.some((x) => x.contains(t))) {
      for (let i = roots.length - 1; i >= 0; i -= 1) if (t.contains(roots[i])) roots.splice(i, 1);
      roots.push(t);
    }
  }
  if (records && !roots.length) return;
  const view = $('view');
  const panes = new Set();
  for (const root of records ? roots : [view]) {
    const up = paneOf(root);
    if (up) panes.add(up);
    for (const p of root.querySelectorAll('[data-pane]')) panes.add(p);
  }
  for (const pane of panes) if (pane.isConnected) syncPane(pane);
  const el = document.activeElement;
  if (lastRow && !lastRow.el.isConnected && (!el || el === document.body)) {
    const pane = Array.from(view.querySelectorAll('[data-pane]')).find((p) => p.dataset.pane === lastRow.pane);
    const row = pane ? rowsOf(pane).find((r) => r.dataset.row === lastRow.key) : null;
    if (row) row.focus({ preventScroll: true });
  }
  applyHints(records ? roots : null);
}

function watchFocus() {
  document.addEventListener('focusin', (e) => {
    const t = e.target;
    const pane = paneOf(t);
    for (const p of document.querySelectorAll('.pane-on')) if (p !== pane) p.classList.remove('pane-on');
    if (pane) pane.classList.add('pane-on');
    const row = t && t.closest ? t.closest('[data-row]') : null;
    if (pane && row) {
      paneSel.set(pane.dataset.pane, row.dataset.row);
      syncPane(pane);
      lastRow = { pane: pane.dataset.pane, key: row.dataset.row, el: row };
    } else lastRow = null;
  });
  // The focus left for nothing (a click on the page): forget the row and the pane. A row
  // that was drawn anew is not connected any more, so it is kept to be put back.
  document.addEventListener('focusout', (e) => {
    if (e.relatedTarget) return;
    const was = e.target;
    queueMicrotask(() => {
      if (!was.isConnected || (document.activeElement && document.activeElement !== document.body)) return;
      if (lastRow && lastRow.el === was) lastRow = null;
      for (const p of document.querySelectorAll('.pane-on')) p.classList.remove('pane-on');
    });
  });
  new MutationObserver(afterDraw).observe($('view'), { childList: true, subtree: true });
}

// ----- The keys sheet (?) -----

function openSheet() {
  if (document.querySelector('dialog[open]')) return;
  const back = document.activeElement;
  const pane = paneOf(back);
  const screen = screenOf(currentRoute());
  const where = screenList({ cluster: true }).find((s) => s.id === screen);
  openDialog({
    title: `Keys · ${where ? where.label : TITLES[currentRoute().name]}`,
    wide: true,
    onClose: () => {
      if (back && back.isConnected && back !== document.body) back.focus({ preventScroll: true });
    },
    build() {
      const box = h('div', { 'aria-live': 'polite' });
      const filter = h('input', {
        class: 'field', type: 'search', id: 'sheet-filter', placeholder: 'Filter: a key, or what it does', autocomplete: 'off', spellcheck: false, 'data-autofocus': true,
        oninput: () => draw(),
      });
      const draw = () => {
        const groups = sheetModel({ screen, pane: pane ? pane.dataset.pane : null, filter: filter.value });
        fill(box, groups.length ? groups.map((g) => h('section', { class: 'sheet-group', 'aria-labelledby': `sheet-${g.id}` },
          h('h3', { class: 'lbl', id: `sheet-${g.id}`, text: g.label }),
          h('ul', { class: 'sheet-rows' }, g.rows.map((r) => h('li', null,
            h('span', { class: 'sheet-keys' }, r.keys.map((k) => h('kbd', { class: 'key-cap', text: k }))),
            h('span', { text: r.name }),
            r.sends ? h('span', { class: 'sheet-sends', text: `sends ${r.sends}` }) : h('span')))))) : h('p', { class: 'muted', text: 'No key matches the filter.' }));
      };
      draw();
      return [
        h('label', { class: 'sr-only', htmlFor: 'sheet-filter', text: 'Filter the keys' }), filter,
        box,
        h('p', { class: 'muted small sheet-group', text: 'A key does what its button does: a write still goes through the write gate and asks first where it should. Keys wait while you type in a box. Tab and Shift-Tab move as in any page.' }),
      ];
    },
  });
}

// ----- Go to (ctrl-k) -----

/** What Go to searches: the names Browse holds, the screens, this screen's actions (each
 *  with whether it is off now, and why). Read from the page only, when Go to opens. */
function gotoSources(pane, row) {
  const b = browse.held(store.state);
  const screens = screenList({ cluster: isCluster() });
  const screen = screenOf(currentRoute());
  const actions = screenActions(screen, pane ? pane.dataset.pane : null).map((a) => {
    const e = a.entry;
    // An action of another pane acts there, on that pane's selected row.
    const there = e.panes && !(pane && e.panes.includes(pane.dataset.pane))
      ? panesShown().find((p) => e.panes.includes(p.dataset.pane)) || null
      : pane;
    const thereRow = there === pane ? row : there ? rowsOf(there).find((r) => r.tabIndex === 0) || null : null;
    return { ...a, where: { pane: there, row: thereRow }, off: entryState(e, there, thereRow) };
  });
  return {
    patterns: b.patterns, keys: b.keys, screens, actions,
    model: { patterns: b.patterns.map((p) => p.name), keys: b.keys.map((k) => k.name), screens, actions: actions.map((a) => ({ name: a.name, keys: a.keys, off: a.off })) },
  };
}

let gotoShown = false;

function openGoto() {
  if (gotoShown || store.locked || document.querySelector('dialog[open]')) return;
  gotoShown = true;
  const back = document.activeElement;
  const pane = paneOf(back);
  const row = focusedRow(back);
  const src = gotoSources(pane, row);
  let groups = [];
  let flat = [];
  let at = -1;
  const input = h('input', {
    class: 'field mono goto-input', type: 'text', id: 'goto-input', role: 'combobox', 'aria-expanded': 'true', 'aria-controls': 'goto-list',
    'aria-autocomplete': 'list', 'aria-label': 'Go to', autocomplete: 'off', spellcheck: false, maxlength: '512',
    placeholder: 'A pattern, a key, a screen or an action; : for a command',
  });
  const list = h('div', { class: 'goto-list', id: 'goto-list', role: 'listbox', 'aria-label': 'Results' });
  const cap = (k) => h('kbd', { class: 'key-cap', text: k });
  const dlg = h('dialog', { class: 'dialog goto', 'aria-label': 'Go to' },
    h('div', { class: 'goto-head' }, icon('search', 16), input),
    list,
    h('p', { class: 'goto-foot muted small' },
      h('span', null, cap('↑'), ' ', cap('↓'), ' pick'), h('span', null, cap('enter'), ' go'),
      h('span', null, cap('tab'), ' next group'), h('span', null, cap(':'), ' a command'), h('span', null, cap('esc'), ' close')));

  const side = (item) => {
    if (item.off) return [h('span', { text: `off: ${item.off}` })];
    if (item.keys && item.keys.length) return item.keys.slice(0, 4).map(cap);
    if (item.kind === 'find') return [cap('/')];
    if (item.kind === 'command') return [cap('enter')];
    return [];
  };
  const label = (item) => {
    if (item.kind === 'pattern') return patternName(item.label, { cls: 'mono goto-label' });
    if (item.kind === 'key') return h('span', { class: 'mono goto-label', text: item.label });
    if (item.kind === 'command') return h('span', { class: 'goto-label' }, `${item.label}: `, h('span', { class: 'mono', text: item.detail || '(nothing typed yet)' }));
    return h('span', { class: 'goto-label', text: item.label });
  };

  const draw = () => {
    if (at >= flat.length) at = flat.length - 1;
    let n = -1;
    fill(list, flat.length ? groups.map((g, gi) => h('div', { role: 'group', 'aria-labelledby': `goto-g-${gi}` },
      h('p', { class: 'lbl goto-group', id: `goto-g-${gi}`, text: g.label }),
      g.items.map((item) => {
        n += 1;
        const i = n;
        return h('div', {
          role: 'option', id: `goto-o-${i}`, class: item.off ? 'goto-item off' : 'goto-item',
          'aria-selected': i === at ? 'true' : 'false', 'aria-disabled': item.off ? 'true' : null,
          onpointermove: () => {
            if (at !== i) {
              at = i;
              draw();
            }
          },
          onclick: () => choose(i),
        }, label(item), h('span', { class: 'goto-side' }, side(item)));
      }))) : h('p', { class: 'goto-empty', text: input.value.trim() ? 'Nothing here matches. Go to looks only at what the page holds: the patterns and keys Browse has read, the screens and this screen\'s actions.' : 'Type to look for a pattern, a key, a screen or an action.' }));
    if (at >= 0) {
      input.setAttribute('aria-activedescendant', `goto-o-${at}`);
      const opt = document.getElementById(`goto-o-${at}`);
      if (opt) opt.scrollIntoView({ block: 'nearest' });
    } else input.removeAttribute('aria-activedescendant');
  };
  const search = () => {
    groups = gotoModel(input.value, src.model);
    flat = gotoFlat(groups);
    at = gotoDefault(groups, input.value);
    draw();
  };

  const close = () => {
    if (dlg.open) dlg.close();
  };
  /** Go: close, give the focus back, then do what the item says. Only Find asks the server
   *  for anything, and only now that its row is chosen. */
  function choose(i) {
    const item = flat[i];
    if (!item) return;
    if (item.off) {
      notify(item.off, 'info');
      return;
    }
    close();
    if (item.kind === 'pattern') ctx.openPattern(src.patterns[item.ref]);
    else if (item.kind === 'key') ctx.openKey(src.keys[item.ref]);
    else if (item.kind === 'find') findKeys(item.text);
    else if (item.kind === 'screen') ctx.navigate(src.screens[item.ref].hash);
    else if (item.kind === 'command') {
      setCommandLine(true);
      if (commandLine) commandLine.setLine(item.text);
    } else if (item.kind === 'action') {
      const a = src.actions[item.ref];
      runEntry(a.entry, a.where);
    }
  }

  input.addEventListener('input', search);
  input.addEventListener('keydown', (e) => {
    // An input method is composing: its Enter and arrows are its own.
    if (e.isComposing || e.keyCode === 229) return;
    const move = { ArrowDown: 'down', ArrowUp: 'up' }[e.key] || (e.key === 'Tab' ? (e.shiftKey ? 'groupBack' : 'group') : null);
    if (move) {
      e.preventDefault();
      at = gotoStep(groups, at, move);
      draw();
    } else if (e.key === 'Enter') {
      e.preventDefault();
      choose(at);
    }
  });
  dlg.addEventListener('close', () => {
    dlg.remove();
    gotoShown = false;
    const now = document.activeElement;
    if ((!now || now === document.body) && back && back.isConnected && back !== document.body) back.focus({ preventScroll: true });
  });
  document.body.append(dlg);
  dlg.showModal();
  input.focus();
  search();
}

/** Find keys by name (Go to's Find row): the results show on Browse. */
function findKeys(text) {
  const q = String(text ?? '').trim();
  if (!q) return;
  if (!store.state.connected && !analyzing(store.state)) {
    notify('Connect first: open Connections and pick a connection. Then keys can be found.', 'info');
    return;
  }
  ctx.findKey(q);
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

  // The keys (keys.js): Go to from the top bar's box, ctrl-k or `/`; the keys sheet from
  // the `?` button. The hints show once the keys are bound.
  $('find-form').addEventListener('submit', (e) => e.preventDefault());
  $('goto-btn').addEventListener('click', () => openGoto());
  $('keys-btn').addEventListener('click', () => openSheet());
  document.addEventListener('keydown', onKey);
  watchFocus();
  document.body.classList.add('keys-on');
  applyHints();

  // The stores that keep running whatever view is shown (the tail, Monitor) hear about a
  // new state first, so a view drawn for it finds them up to date.
  browse.init(api);
  pubsub.init(api);
  server.init(api);
  analyze.init(api);

  // A question the server asks is marked with the session when it opens, if there is one:
  // it closes, and nothing is sent, when the connection or database changes under it. (A
  // question asked with no connection yet, such as a connect's, has no session to keep.)
  api.setConfirmHandler((confirm) => confirmDialog(confirm, { session: sessionMark() !== null, onStale: () => notify(STALE_TEXT, 'info') }));
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
  document.addEventListener('click', stayOnLinks);
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
