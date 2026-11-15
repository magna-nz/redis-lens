// redis-lens dashboard: shared helpers.
//
// Two halves:
//   1. Pure helpers (no DOM, no globals at import time). The tests under tests/ import these
//      in Node, so nothing here may touch `document` or `window` when the module loads.
//   2. DOM helpers and the shared components (dialog, confirm, toasts, bars, tables).
//
// Content-Security-Policy: the page is served with `style-src 'self'` and `script-src 'self'`.
// So nothing here sets a `style` attribute or parses HTML from a string. Text goes in with
// `textContent`; sizes that change go in through the CSSOM (`el.style.setProperty`).

// ---------------------------------------------------------------------------------------
// 1. Pure helpers
// ---------------------------------------------------------------------------------------

/** What an unknown figure shows as, like the TUI. */
export const DASH = '—';

const isNum = (n) => typeof n === 'number' && Number.isFinite(n);

/** Bytes as `512B`, `3.1K`, `41K`, `1.2M`, `15G` (powers of 1024, one decimal below 10).
 *  The same rules as `format::bytes` in the TUI. */
export function fmtBytes(n) {
  if (!isNum(n) || n < 0) return DASH;
  const units = ['K', 'M', 'G', 'T', 'P'];
  if (n < 1024) return `${Math.round(n)}B`;
  let v = n / 1024;
  let unit = 0;
  while (v >= 1024 && unit < units.length - 1) {
    v /= 1024;
    unit += 1;
  }
  if (v < 10) {
    const s = v.toFixed(1);
    return s === '10.0' ? `10${units[unit]}` : `${s}${units[unit]}`;
  }
  return `${Math.round(v)}${units[unit]}`;
}

/** Counts as `9`, `212`, `1.5K`, `48K`, `1.84M` (powers of 1000), as `format::count`. */
export function fmtCount(n) {
  if (!isNum(n) || n < 0) return DASH;
  const v = Math.round(n);
  if (v < 1000) return String(v);
  const tenthsK = Math.floor((v + 50) / 100);
  if (tenthsK < 100) return `${Math.floor(tenthsK / 10)}.${tenthsK % 10}K`;
  const k = Math.floor((v + 500) / 1000);
  if (k < 1000) return `${k}K`;
  const hundredthsM = Math.floor((v + 5000) / 10000);
  return `${Math.floor(hundredthsM / 100)}.${String(hundredthsM % 100).padStart(2, '0')}M`;
}

/** A count with thousands separators: `85,964`. */
export function fmtGrouped(n) {
  if (!isNum(n)) return DASH;
  const s = String(Math.abs(Math.round(n)));
  let out = '';
  for (let i = 0; i < s.length; i += 1) {
    if (i > 0 && (s.length - i) % 3 === 0) out += ',';
    out += s[i];
  }
  return n < 0 ? `-${out}` : out;
}

/** Seconds as `38s`, `24m 10s`, `11h 04m`, `6d 23h`, as `format::duration`. */
export function fmtDuration(secs) {
  if (!isNum(secs) || secs < 0) return DASH;
  const s = Math.floor(secs);
  const two = (v) => String(v).padStart(2, '0');
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${two(s % 60)}s`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ${two(Math.floor((s % 3600) / 60))}m`;
  return `${Math.floor(s / 86400)}d ${two(Math.floor((s % 86400) / 3600))}h`;
}

/** A ratio from 0 to 1 as a whole percentage, `<1%` for small values above zero. */
export function fmtPercent(ratio) {
  if (!isNum(ratio)) return DASH;
  if (ratio > 0 && ratio < 0.01) return '<1%';
  return `${Math.round(ratio * 100)}%`;
}

/** A rate per second for a live tile: `0`, `7.5`, `212`, `12.4K`, `1.21M`. */
export function fmtRate(n) {
  if (!isNum(n) || n < 0) return DASH;
  if (n < 10) return n === Math.round(n) ? String(n) : n.toFixed(1);
  if (n < 999.5) return String(Math.round(n));
  if (n < 999950) {
    const s = (n / 1000).toFixed(1);
    return `${s}K`;
  }
  return `${(n / 1e6).toFixed(2)}M`;
}

/** How long a command took, from microseconds: `0.42 ms`, `12 ms`, `1.30 s`. */
export function fmtTook(micros) {
  if (!isNum(micros) || micros < 0) return DASH;
  const ms = micros / 1000;
  if (ms < 10) return `${ms.toFixed(2)} ms`;
  if (ms < 1000) return `${Math.round(ms)} ms`;
  return `${(ms / 1000).toFixed(2)} s`;
}

/** A time of day in UTC from epoch milliseconds: `13:58:41`, or with `ms` `13:58:41.207`. */
export function fmtClock(epochMs, withMs = false) {
  if (!isNum(epochMs)) return DASH;
  const iso = new Date(epochMs).toISOString();
  return withMs ? iso.slice(11, 23) : iso.slice(11, 19);
}

/** `1 key`, `2 keys`: a count with its noun. */
export function plural(n, one, many) {
  return `${isNum(n) ? fmtGrouped(n) : DASH} ${n === 1 ? one : many}`;
}

/** Bar heights in pixels for a series, scaled from its smallest to its largest value so a
 *  small steady change is visible. Missing points (null) get height 0. A flat series sits
 *  at the minimum height. */
export function barHeights(series, maxH, minH = 4) {
  let lo = Infinity;
  let hi = -Infinity;
  for (const v of series) {
    if (isNum(v)) {
      lo = Math.min(lo, v);
      hi = Math.max(hi, v);
    }
  }
  const span = hi - lo || 1;
  return series.map((v) => (isNum(v) ? Math.round(minH + ((v - lo) / span) * (maxH - minH)) : 0));
}

/** A share of a maximum as a whole percentage from 0 to 100, for a bar's width. A value
 *  above zero never rounds down to an empty bar. */
export function barPercent(value, max) {
  if (!isNum(value) || !isNum(max) || max <= 0 || value <= 0) return 0;
  return Math.min(100, Math.max(1, Math.round((value / max) * 100)));
}

/** Append to a rolling series, keeping the last `cap` points. Returns a new array. */
export function pushSeries(series, value, cap = 40) {
  const out = series.concat([isNum(value) ? value : null]);
  return out.length > cap ? out.slice(out.length - cap) : out;
}

/** A series left-padded with gaps to exactly `n` points (the newest stay on the right). */
export function padSeries(series, n = 40) {
  const tail = series.slice(-n);
  return new Array(n - tail.length).fill(null).concat(tail);
}

/** Whether a tail message matches the filter: case-insensitive, on channel or message. */
export function tailMatches(msg, query) {
  const q = (query || '').trim().toLowerCase();
  if (!q) return true;
  return String(msg.body ?? '').toLowerCase().includes(q) || String(msg.channel ?? '').toLowerCase().includes(q);
}

/** The messages a filter leaves. */
export function tailFilter(messages, query) {
  return messages.filter((m) => tailMatches(m, query));
}

/** The live tail's memory: the last `cap` messages, a pause that freezes what is shown
 *  while messages still arrive, and the rate over the last five seconds. Nothing here is
 *  ever written anywhere: messages are shown, never saved. */
export class TailBuffer {
  constructor(cap = 2000) {
    this.cap = cap;
    this.kept = [];
    this.frozen = null;
    this.newWhilePaused = 0;
    this.skipped = 0;
    this.arrivals = [];
  }

  get paused() {
    return this.frozen !== null;
  }

  /** Add a batch. `now` is when it arrived (ms), for the rate. */
  push(messages, skipped = 0, now = Date.now()) {
    if (messages.length) {
      this.kept = this.kept.concat(messages);
      if (this.kept.length > this.cap) this.kept = this.kept.slice(this.kept.length - this.cap);
      this.arrivals.push({ at: now, n: messages.length });
      if (this.paused) this.newWhilePaused += messages.length;
    }
    if (isNum(skipped) && skipped > 0) this.skipped += skipped;
    this.trimArrivals(now);
  }

  trimArrivals(now) {
    while (this.arrivals.length && now - this.arrivals[0].at > 5000) this.arrivals.shift();
  }

  pause() {
    if (!this.paused) {
      this.frozen = this.kept.slice();
      this.newWhilePaused = 0;
    }
  }

  resume() {
    this.frozen = null;
    this.newWhilePaused = 0;
  }

  /** Empty the list. The tail keeps running. */
  clear() {
    this.kept = [];
    if (this.paused) this.frozen = [];
    this.newWhilePaused = 0;
    this.skipped = 0;
  }

  /** Messages per second over the last 5 s (over at least 1 s). */
  rate(now = Date.now()) {
    this.trimArrivals(now);
    if (!this.arrivals.length) return 0;
    const total = this.arrivals.reduce((sum, a) => sum + a.n, 0);
    const span = Math.max(1000, Math.min(5000, now - this.arrivals[0].at));
    return total / (span / 1000);
  }

  /** What to draw: the newest `limit` messages that match the filter. */
  visible(query = '', limit = 300) {
    const source = this.paused ? this.frozen : this.kept;
    const matching = tailFilter(source, query);
    return {
      rows: matching.slice(-limit),
      matching: matching.length,
      kept: this.kept.length,
      total: source.length,
    };
  }
}

/** The command line's history: this session's commands, in memory only. `prev` and `next`
 *  step through it; the line being typed comes back when you step past the newest. */
export class CommandHistory {
  constructor(cap = 100) {
    this.cap = cap;
    this.items = [];
    this.pos = null;
    this.draft = '';
  }

  add(line) {
    const text = String(line ?? '').trim();
    this.pos = null;
    this.draft = '';
    if (!text) return;
    if (this.items[this.items.length - 1] !== text) this.items.push(text);
    if (this.items.length > this.cap) this.items = this.items.slice(this.items.length - this.cap);
  }

  /** The older command, given what is in the input now. */
  prev(current = '') {
    if (!this.items.length) return current;
    if (this.pos === null) {
      this.draft = current;
      this.pos = this.items.length - 1;
    } else if (this.pos > 0) {
      this.pos -= 1;
    }
    return this.items[this.pos];
  }

  /** The newer command, or the line that was being typed. */
  next(current = '') {
    if (this.pos === null) return current;
    if (this.pos < this.items.length - 1) {
      this.pos += 1;
      return this.items[this.pos];
    }
    this.pos = null;
    return this.draft;
  }
}

/** The confirm dialog as a state machine, so it can be tested without a DOM.
 *  `confirm` is `{ id, kind: 'ask' | 'type', text, expect }`. Kind `ask` is Yes or Cancel;
 *  kind `type` sends only when the text typed is exactly `expect`. */
export function confirmStart(confirm) {
  const expect = typeof confirm.expect === 'string' ? confirm.expect : '';
  const kind = confirm.kind === 'type' || (confirm.kind !== 'ask' && expect) ? 'type' : 'ask';
  return { id: confirm.id ?? null, kind, text: confirm.text ?? '', expect, typed: '', status: 'open', error: null };
}

/** Whether Yes (or enter) would send it. */
export function confirmCanSend(state) {
  return state.status === 'open' && (state.kind === 'ask' || (state.expect !== '' && state.typed === state.expect));
}

export const CONFIRM_MISMATCH = 'That does not match, so nothing was sent.';

/** One step: `{ type: 'input', value }`, `{ type: 'send' }` or `{ type: 'cancel' }`. */
export function confirmStep(state, action) {
  if (state.status !== 'open') return state;
  if (action.type === 'input') return { ...state, typed: String(action.value ?? ''), error: null };
  if (action.type === 'cancel') return { ...state, status: 'cancelled', error: null };
  if (action.type === 'send') {
    if (confirmCanSend(state)) return { ...state, status: 'confirmed', error: null };
    return { ...state, error: CONFIRM_MISMATCH };
  }
  return state;
}

/** The theme, remembered in a `localStorage`-like store. Every access is guarded: private
 *  windows and locked-down browsers throw on it. Dark is the default. */
export function createThemeStore(storage, key = 'redis-lens.theme') {
  const valid = (t) => t === 'dark' || t === 'light';
  return {
    load() {
      try {
        const t = storage ? storage.getItem(key) : null;
        return valid(t) ? t : 'dark';
      } catch {
        return 'dark';
      }
    },
    save(theme) {
      if (!valid(theme)) return false;
      try {
        if (!storage) return false;
        storage.setItem(key, theme);
        return true;
      } catch {
        return false;
      }
    },
    other(theme) {
      return theme === 'dark' ? 'light' : 'dark';
    },
  };
}

/** Split a command into its arguments the way a shell would: spaces separate, single and
 *  double quotes group, a backslash keeps the next character. Returns `{ args }` or
 *  `{ error }` for a quote left open. Used for the token command, stored as arguments. */
export function splitArgs(line) {
  const args = [];
  let cur = '';
  let has = false;
  let quote = null;
  const s = String(line ?? '');
  for (let i = 0; i < s.length; i += 1) {
    const c = s[i];
    if (quote) {
      if (c === quote) quote = null;
      else if (c === '\\' && quote === '"' && i + 1 < s.length) {
        i += 1;
        cur += s[i];
      } else cur += c;
    } else if (c === '"' || c === "'") {
      quote = c;
      has = true;
    } else if (c === '\\' && i + 1 < s.length) {
      i += 1;
      cur += s[i];
      has = true;
    } else if (/\s/.test(c)) {
      if (has || cur) args.push(cur);
      cur = '';
      has = false;
    } else {
      cur += c;
    }
  }
  if (quote) return { error: 'A quote is left open.' };
  if (has || cur) args.push(cur);
  return { args };
}

/** Tags from one text box: split on commas and spaces, trimmed, no blanks, no repeats. */
export function parseTags(text) {
  const seen = new Set();
  const out = [];
  for (const raw of String(text ?? '').split(/[,\s]+/)) {
    const t = raw.trim();
    if (t && !seen.has(t)) {
      seen.add(t);
      out.push(t);
    }
  }
  return out;
}

/** The schemes redis-lens reads (valkey:// and valkeys:// are the same as redis:// and
 *  rediss://), as the server's `parse_connection_string`. */
const SCHEME = /^\s*(?:rediss?|valkeys?):\/\//i;

/** redis-lens's own words for an address it will not read (`src/conn/parse.rs`). */
export const ADDRESS_TEXT = {
  scheme: 'Could not parse the connection string: only redis, rediss, valkey and valkeys URLs are supported.',
  userinfo: 'Could not parse the connection string: the host holds a user name, a password or other URL parts; give the user name and password in a redis:// URL instead.',
};

/** The parts of a supported URL: the scheme as typed, the user info (null without an
 *  `@`), the host and port, and what follows (path, query, fragment). The user info ends at
 *  the last `@` before the path, as the server's URL parser reads it. */
function urlParts(s) {
  const scheme = SCHEME.exec(s);
  if (!scheme) return null;
  const rest = s.slice(scheme[0].length);
  const cut = rest.search(/[/?#]/);
  const authority = cut < 0 ? rest : rest.slice(0, cut);
  const at = authority.lastIndexOf('@');
  return {
    head: scheme[0],
    userinfo: at < 0 ? null : authority.slice(0, at),
    host: authority.slice(at + 1),
    after: cut < 0 ? '' : rest.slice(cut),
  };
}

/** Why redis-lens will refuse an address, before it is sent (null when it may be sent):
 *  another scheme, or an `@` anywhere but in a supported URL's user info (in the host, the
 *  path or query, or the host of a host:port string). The server stays the judge of the
 *  rest; this keeps a password in an address it cannot read from leaving the page. */
export function addressProblem(text) {
  const s = String(text ?? '').trim();
  if (!s) return null;
  const url = urlParts(s);
  if (url) return url.after.includes('@') ? ADDRESS_TEXT.userinfo : null;
  if (s.includes('://')) return ADDRESS_TEXT.scheme;
  // host:port[,option=value...]: an option's value (a password) may hold anything.
  const endpoints = s.split(',').filter((token) => !token.includes('='));
  if (endpoints.some((token) => token.includes('@'))) return ADDRESS_TEXT.userinfo;
  return null;
}

/** Take a password out of a pasted connection string, so it can go into the password box
 *  instead of sitting readable in the address box. Returns `{ url, password }`; `password`
 *  is null when there was none, or when taking it out is not clear-cut (a percent-escape,
 *  a `?password=` parameter, an address redis-lens will not read): then the string is left
 *  alone. It follows the server's parser: for a redis, rediss, valkey or valkeys URL the
 *  user info ends at the last `@` before the path; for the StackExchange.Redis form the
 *  `password=` option goes. */
export function splitUrlSecret(text) {
  const s = String(text ?? '');
  const none = { url: s, password: null };
  if (addressProblem(s)) return none;
  const url = urlParts(s);
  if (url) {
    if (url.userinfo === null) return none;
    const colon = url.userinfo.indexOf(':');
    if (colon < 0) return none;
    const user = url.userinfo.slice(0, colon);
    const password = url.userinfo.slice(colon + 1);
    if (!password || password.includes('%')) return none;
    return { url: `${url.head}${user ? `${user}@` : ''}${url.host}${url.after}`, password };
  }
  if (s.includes('://')) return none;
  const tokens = s.split(',');
  let password = null;
  const kept = tokens.filter((token) => {
    const eq = token.indexOf('=');
    if (eq < 0) return true;
    if (token.slice(0, eq).trim().toLowerCase() !== 'password') return true;
    password = token.slice(eq + 1).trim();
    return false;
  });
  if (!password) return none;
  return { url: kept.join(','), password };
}

/** Whether a connection string still holds a password (one `splitUrlSecret` left in place
 *  for the server to read). The add form empties the address box after sending such a
 *  string, so the password does not stay readable on the page. */
export function urlHasSecret(text) {
  const s = String(text ?? '');
  if (/password\s*=\s*[^,&\s]/i.test(s)) return true;
  const url = urlParts(s);
  if (!url || url.userinfo === null) return false;
  const colon = url.userinfo.indexOf(':');
  return colon >= 0 && colon < url.userinfo.length - 1;
}

/** Obvious mistakes in the add form, by field name. The server stays the judge of
 *  everything else. `form` holds plain strings and booleans; the password is only checked
 *  for being there together with a token command, never read otherwise. */
export function checkAddForm(form) {
  const errors = {};
  const name = String(form.name ?? '').trim();
  const url = String(form.url ?? '').trim();
  if (!url) errors.url = 'Paste a redis://, rediss://, valkey:// or valkeys:// URL, or a host:port string.';
  else if (/\s/.test(url)) errors.url = 'A connection string has no spaces.';
  else if (addressProblem(url)) errors.url = addressProblem(url);
  if (!name) errors.name = 'Give the connection a name.';
  else if (/^docker:/i.test(name)) errors.name = 'Names starting with docker: are kept for containers found in Docker.';
  else if (/[\s/\\]/.test(name)) errors.name = 'A name has no spaces or slashes.';
  const token = String(form.tokenCommand ?? '').trim();
  if (token) {
    if (splitArgs(token).error) errors.tokenCommand = 'A quote is left open in the command.';
    if (form.hasPassword) errors.tokenCommand = 'A token command replaces the password: use one or the other.';
  }
  const ssh = String(form.sshHost ?? '').trim();
  if (ssh) {
    const host = ssh.slice(ssh.lastIndexOf('@') + 1);
    if (!host || /\s/.test(ssh) || host.startsWith('-')) errors.sshHost = 'Write the bastion as [user@]host[:port].';
  } else if (String(form.sshIdentity ?? '').trim() || String(form.sshKnownHosts ?? '').trim()) {
    errors.sshHost = 'An identity or known-hosts file needs a bastion host.';
  }
  for (const [field, value] of [['sshIdentity', form.sshIdentity], ['sshKnownHosts', form.sshKnownHosts]]) {
    if (/[%"]|\$\{/.test(String(value ?? ''))) errors[field] = 'The path may not contain %, " or ${.';
  }
  if (form.entra && (token || form.hasPassword)) errors.entra = 'Entra ID replaces the password and the token command: use one of them.';
  const sentinels = String(form.sentinelAddresses ?? '').trim();
  const master = String(form.sentinelMaster ?? '').trim();
  if (sentinels && !master) errors.sentinelMaster = 'Sentinel needs the master name.';
  if (!sentinels && (master || form.hasSentinelPassword || String(form.sentinelUser ?? '').trim() || form.sentinelTls)) {
    errors.sentinelAddresses = 'List the Sentinel addresses as host:port, separated by commas.';
  }
  if (sentinels && sentinels.split(',').some((a) => !/^\s*\S+:\d{1,5}\s*$/.test(a))) {
    errors.sentinelAddresses = 'Each Sentinel address is host:port.';
  }
  return errors;
}

const MASK = '***';
const CONFIG_SECRETS = new Set([
  'requirepass', 'masterauth', 'masteruser', 'primaryauth', 'primaryuser',
  'tls-key-file-pass', 'tls-client-key-file-pass', 'sentinel-pass', 'sentinel-user',
]);

/** A typed command with its credentials as `***`, for the echo in the output before the
 *  server's own masked line arrives. The server's masking is the authority; this only
 *  keeps the obvious ones (AUTH, HELLO … AUTH, ACL SETUSER passwords, CONFIG SET
 *  requirepass and friends, MIGRATE … AUTH) off the screen in the meantime. */
export function maskCommand(line) {
  const parts = String(line ?? '').split(/(\s+)/);
  const words = [];
  parts.forEach((p, i) => {
    if (p && !/^\s+$/.test(p)) words.push(i);
  });
  if (!words.length) return String(line ?? '');
  const word = (n) => parts[words[n]];
  const lower = (n) => (words[n] === undefined ? '' : word(n).toLowerCase());
  const hide = (n) => {
    if (words[n] !== undefined) parts[words[n]] = MASK;
  };
  let first = 0;
  if (/^\d+$/.test(word(0)) && words.length > 1) first = 1;
  const cmd = lower(first);
  if (cmd === 'auth') {
    for (let n = first + 1; n < words.length; n += 1) hide(n);
  } else if (cmd === 'hello' || cmd === 'migrate') {
    for (let n = first + 1; n < words.length; n += 1) {
      const w = lower(n);
      if (w === 'auth') {
        hide(n + 1);
        if (cmd === 'hello') hide(n + 2);
      } else if (w === 'auth2') {
        hide(n + 1);
        hide(n + 2);
      }
    }
  } else if (cmd === 'acl' && lower(first + 1) === 'setuser') {
    for (let n = first + 3; n < words.length; n += 1) {
      const w = word(n);
      if (/^[><#!]/.test(w)) parts[words[n]] = w[0] + MASK;
    }
  } else if (cmd === 'config' && lower(first + 1) === 'set') {
    for (let n = first + 2; n + 1 < words.length; n += 2) {
      if (CONFIG_SECRETS.has(lower(n))) hide(n + 1);
    }
  } else if (cmd === 'sentinel') {
    for (let n = first + 1; n + 1 < words.length; n += 1) {
      if (['auth-pass', 'auth-user', 'sentinel-pass', 'sentinel-user'].includes(lower(n))) hide(n + 1);
    }
  }
  return parts.join('');
}

/** The route in a location hash: `#/server/slowlog` is `{ name: 'server', sub: 'slowlog' }`.
 *  An unknown or empty hash gives `fallback`. */
export function parseRoute(hash, names, fallback) {
  const parts = String(hash ?? '').replace(/^#\/?/, '').split('/').filter(Boolean);
  const name = names.includes(parts[0]) ? parts[0] : fallback;
  return { name, sub: name === parts[0] ? parts[1] ?? null : null };
}

// ---------------------------------------------------------------------------------------
// 2. DOM helpers and shared components
// ---------------------------------------------------------------------------------------

const PROPS = new Set(['value', 'checked', 'disabled', 'hidden', 'htmlFor', 'type', 'name', 'id', 'href', 'placeholder', 'open', 'selected', 'tabIndex', 'colSpan', 'title', 'autocomplete', 'spellcheck', 'required', 'readOnly']);

/** Build an element. `props`: `class`, `text`, `on<event>` handlers, a few DOM properties,
 *  anything else as an attribute. Children: nodes, strings (as text), arrays, or nothing.
 *  It refuses `style` and `innerHTML` so a slip can't break the Content-Security-Policy. */
export function h(tag, props, ...kids) {
  const el = document.createElement(tag);
  if (props) {
    for (const [key, value] of Object.entries(props)) {
      if (value === null || value === undefined || value === false) continue;
      if (key === 'style' || key === 'innerHTML' || key === 'outerHTML') throw new Error(`h(): ${key} is not allowed`);
      if (key === 'class') el.className = value;
      else if (key === 'text') el.textContent = String(value);
      else if (key.startsWith('on') && typeof value === 'function') el.addEventListener(key.slice(2), value);
      else if (PROPS.has(key)) el[key] = value;
      else el.setAttribute(key, value === true ? '' : String(value));
    }
  }
  add(el, kids);
  return el;
}

/** Append children: nodes, strings and numbers (as text), nested arrays; null is skipped. */
export function add(parent, ...kids) {
  for (const kid of kids.flat(Infinity)) {
    if (kid === null || kid === undefined || kid === false) continue;
    parent.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
  }
  return parent;
}

/** Replace an element's children. */
export function fill(parent, ...kids) {
  parent.replaceChildren();
  return add(parent, ...kids);
}

const SVG_NS = 'http://www.w3.org/2000/svg';

/** Icon paths (24 × 24, stroked), from the approved mockup. */
export const ICONS = {
  overview: 'M4 4h7v7H4zM13 4h7v7h-7zM4 13h7v7H4zM13 13h7v7h-7z',
  server: 'M3 12h4l3-8 4 16 3-8h4',
  pubsub: 'M5 12a7 7 0 0 1 14 0M8.5 12a3.5 3.5 0 0 1 7 0M12 12v8',
  connections: 'M9 3v6M15 3v6M6 9h12v3a6 6 0 0 1-12 0zM12 18v3',
  cmdline: 'M4 6l6 6-6 6M12 18h8',
  sun: 'M12 8a4 4 0 1 0 0 8a4 4 0 0 0 0-8zM12 2v2M12 20v2M4 12H2M22 12h-2M5 5l1.5 1.5M17.5 17.5L19 19M5 19l1.5-1.5M17.5 6.5L19 5',
  moon: 'M20 14.5A8 8 0 0 1 9.5 4a7 7 0 1 0 10.5 10.5z',
  lock: 'M6 11h12v9H6zM8 11V8a4 4 0 0 1 8 0v3',
  close: 'M6 6l12 12M18 6L6 18',
  plus: 'M12 5v14M5 12h14',
};

/** A decorative icon (hidden from screen readers; the control beside it has the words). */
export function icon(name, size = 18) {
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('focusable', 'false');
  svg.setAttribute('width', String(size));
  svg.setAttribute('height', String(size));
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('fill', 'none');
  svg.setAttribute('stroke', 'currentColor');
  svg.setAttribute('stroke-width', '2');
  svg.setAttribute('stroke-linecap', 'round');
  svg.setAttribute('stroke-linejoin', 'round');
  svg.classList.add('icon');
  const path = document.createElementNS(SVG_NS, 'path');
  path.setAttribute('d', ICONS[name] || '');
  svg.append(path);
  return svg;
}

/** Point an existing icon at another path. */
export function setIcon(svg, name) {
  const path = svg.querySelector('path');
  if (path) path.setAttribute('d', ICONS[name] || '');
}

/** Replace an element's children and keep the keyboard focus where it was: on the control
 *  with the same accessible name, or on `fallback` when that control is gone. Used where
 *  a list is drawn again after an action (the button pressed would otherwise vanish and
 *  drop the focus to the top of the page). */
export function refill(parent, kids, fallback = null) {
  const active = document.activeElement;
  const had = active && parent.contains(active);
  const nameOf = (el) => el.getAttribute('aria-label') || el.textContent.trim();
  const name = had ? nameOf(active) : null;
  fill(parent, kids);
  if (!had) return parent;
  const again = Array.from(parent.querySelectorAll('button, a, input, select')).find((el) => nameOf(el) === name && !el.disabled);
  if (again) again.focus();
  else if (fallback) fallback.focus();
  return parent;
}

/** Set a CSS custom property through the CSSOM (allowed by the CSP, unlike a style attribute). */
export function setVar(el, name, value) {
  el.style.setProperty(name, value);
}

/** A row of live bars. `update(series)` redraws it; gaps stay empty. */
export function barChart({ points = 40, height = 36, tall = false } = {}) {
  const el = h('div', { class: tall ? 'bars tall' : 'bars', 'aria-hidden': 'true' });
  const bars = [];
  for (let i = 0; i < points; i += 1) {
    const bar = h('div', { class: 'bar' });
    setVar(bar, '--h', '0px');
    bars.push(bar);
    el.append(bar);
  }
  return {
    el,
    update(series) {
      const heights = barHeights(padSeries(series, points), height);
      heights.forEach((px, i) => setVar(bars[i], '--h', `${px}px`));
    },
  };
}

/** A thin horizontal meter, `pct` from 0 to 100. */
export function meter(pct, cls = '') {
  const inner = h('span');
  setVar(inner, '--w', `${Math.max(0, Math.min(100, pct))}%`);
  return h('span', { class: `meter ${cls}`.trim(), 'aria-hidden': 'true' }, inner);
}

/** A small label in a box: an environment, a type, a key name. */
export function badge(text, cls = '') {
  return h('span', { class: `badge ${cls}`.trim(), text });
}

/** The environment badge. PROD stands out by its border as well as its word. */
export function envBadge(env) {
  const name = String(env || 'OTHER').toUpperCase();
  return badge(name, name === 'PROD' ? 'env-prod' : '');
}

/** A calm empty or error state: one sentence, and an optional action. */
export function emptyState(text, action = null) {
  return h('div', { class: 'empty' }, h('p', { text }), action);
}

/** A line that says a feature is off and why, with a glyph so it is not colour alone. */
export function notice(text, kind = 'warn') {
  return h('p', { class: `note ${kind}` }, h('span', { 'aria-hidden': 'true', class: 'glyph', text: kind === 'ok' ? '✓' : '▲' }), h('span', { text }));
}

/** The read-only line with its lock. */
export function readOnlyNote(text) {
  return h('p', { class: 'note muted' }, icon('lock', 16), h('span', { text }));
}

/** A semantic table with the mockup's row look. `columns`: `{ label, cls, cell(row) }`,
 *  where `cell` returns a string or a node. It sits in a box that scrolls sideways on a
 *  narrow screen. */
export function table(columns, rows, { caption = '', cls = '' } = {}) {
  const head = h('tr', null, columns.map((c) => h('th', { scope: 'col', class: c.cls || '', text: c.label })));
  const body = rows.map((row) => h('tr', { class: row.rowClass || '' }, columns.map((c) => h('td', { class: c.cls || '' }, c.cell(row)))));
  return h('div', { class: 'scroll-x' },
    h('table', { class: `tbl ${cls}`.trim() },
      caption ? h('caption', { class: 'sr-only', text: caption }) : null,
      h('thead', null, head),
      h('tbody', null, body)));
}

/** Redraw a scrolling log box, staying at the bottom when the reader was at the bottom. */
export function fillLog(box, nodes) {
  const pinned = box.scrollHeight - box.scrollTop - box.clientHeight < 24;
  fill(box, nodes);
  if (pinned) box.scrollTop = box.scrollHeight;
}

let dialogSeq = 0;

/** A modal dialog on the native `<dialog>` element: the browser keeps focus inside it,
 *  Escape closes it, and focus goes back to where it was. `build(close)` returns the body
 *  nodes; `close(result)` closes it. Returns a promise of the result (null when dismissed). */
export function openDialog({ title, build, wide = false, onClose = null }) {
  return new Promise((resolve) => {
    dialogSeq += 1;
    const titleId = `dlg-title-${dialogSeq}`;
    let result = null;
    const dlg = h('dialog', { class: wide ? 'dialog wide' : 'dialog', 'aria-labelledby': titleId });
    const close = (value = null) => {
      result = value;
      if (dlg.open) dlg.close();
    };
    const head = h('div', { class: 'dialog-head' },
      h('h2', { id: titleId, class: 'dialog-title', text: title }),
      h('button', { type: 'button', class: 'btn icon-btn quiet', 'aria-label': 'Close', onclick: () => close(null) }, icon('close', 18)));
    const body = h('div', { class: 'dialog-body' });
    add(body, build(close, dlg));
    dlg.append(head, body);
    dlg.addEventListener('close', () => {
      dlg.remove();
      if (onClose) onClose(result);
      resolve(result);
    });
    document.body.append(dlg);
    if (typeof dlg.showModal === 'function') dlg.showModal();
    else dlg.setAttribute('open', '');
    const first = dlg.querySelector('[data-autofocus]');
    if (first) first.focus();
  });
}

/** The confirm dialog. `confirm` is `{ kind, text, expect, title?, yes?, code?, after? }`
 *  (`code` is shown exactly, in a box; `after` is a muted line under it). Resolves to
 *  `{ typed }` when confirmed and null when cancelled. Cancel has the focus for a Yes/No
 *  question; for a typed one the text box does, and Yes stays off until the text matches. */
export function confirmDialog(confirm) {
  let state = confirmStart(confirm);
  return openDialog({
    title: confirm.title || 'Please confirm',
    build(close) {
      const error = h('p', { class: 'field-error', role: 'alert' });
      const yes = h('button', { type: 'submit', class: 'btn primary', text: confirm.yes || (state.kind === 'type' ? 'Confirm' : 'Yes') });
      const cancel = h('button', { type: 'button', class: 'btn', text: 'Cancel', onclick: () => close(null) });
      const sync = () => {
        yes.disabled = !confirmCanSend(state);
        error.textContent = state.error || '';
      };
      const send = () => {
        state = confirmStep(state, { type: 'send' });
        sync();
        if (state.status === 'confirmed') close({ typed: state.typed });
      };
      let typed = null;
      if (state.kind === 'type') {
        const input = h('input', {
          class: 'field mono', type: 'text', id: `confirm-typed-${dialogSeq}`, autocomplete: 'off', spellcheck: false,
          'data-autofocus': true,
          oninput: () => {
            state = confirmStep(state, { type: 'input', value: input.value });
            sync();
          },
          // Enter with the wrong text sends nothing and says so. (The button is off until
          // the text matches, so the browser would not submit the form by itself.)
          onkeydown: (e) => {
            if (e.key !== 'Enter') return;
            e.preventDefault();
            send();
          },
        });
        typed = h('div', { class: 'form-row' },
          h('label', { htmlFor: input.id }, 'Type ', h('span', { class: 'mono strong', text: state.expect }), ' to confirm'),
          input);
      } else {
        cancel.setAttribute('data-autofocus', '');
      }
      const form = h('form', {
        class: 'form',
        method: 'dialog',
        onsubmit: (e) => {
          e.preventDefault();
          send();
        },
      },
      h('p', { class: 'confirm-text', text: state.text }),
      confirm.code ? h('pre', { class: 'box code-block', text: confirm.code }) : null,
      confirm.after ? h('p', { class: 'muted', text: confirm.after }) : null,
      typed,
      error,
      h('div', { class: 'dialog-actions' }, cancel, yes));
      sync();
      return form;
    },
  });
}

/** Toasts: short notices in a live region, so a screen reader says them. */
export function createToasts(region) {
  return function notify(text, kind = 'info') {
    if (!text) return;
    const toast = h('div', { class: `toast ${kind}` },
      h('span', { 'aria-hidden': 'true', class: 'glyph', text: kind === 'error' ? '✗' : kind === 'ok' ? '✓' : '•' }),
      h('span', { class: 'toast-text', text }),
      h('button', { type: 'button', class: 'btn icon-btn quiet', 'aria-label': 'Dismiss', onclick: () => toast.remove() }, icon('close', 16)));
    region.append(toast);
    while (region.children.length > 4) region.firstElementChild.remove();
    setTimeout(() => toast.remove(), kind === 'error' ? 12000 : 6000);
  };
}

/** A row of pills. `items`: `{ id, label, href? }`. With `href` they are links (tabs kept
 *  in the address); without, buttons that call `onPick(id)`. */
export function pills(items, current, onPick = null, label = '') {
  return h('div', { class: 'tabs', role: 'group', 'aria-label': label || null }, items.map((item) => {
    const on = item.id === current;
    if (item.href) {
      return h('a', { class: on ? 'pill on' : 'pill', href: item.href, 'aria-current': on ? 'true' : null, text: item.label });
    }
    return h('button', { type: 'button', class: on ? 'pill on' : 'pill', 'aria-pressed': on ? 'true' : 'false', text: item.label, onclick: () => onPick && onPick(item.id) });
  }));
}

/** The node picker shown on a cluster: the views that read one node read the picked one. */
export function nodePicker(conn, onPick) {
  const nodes = conn && Array.isArray(conn.nodes) ? conn.nodes : [];
  if (nodes.length < 2) return null;
  const id = `node-pick-${(dialogSeq += 1)}`;
  const select = h('select', { class: 'field', id, onchange: () => onPick(Number(select.value)) },
    nodes.map((n, i) => h('option', { value: n.id, selected: n.id === conn.pickedNode, text: `${n.address} · ${i + 1} of ${nodes.length}` })));
  return h('div', { class: 'picker' }, h('label', { htmlFor: id, text: 'Node' }), select);
}

/** What to say for an error thrown by the data layer. */
export function errorText(err) {
  if (err && err.cancelled) return 'Cancelled: nothing was sent.';
  return (err && err.message) || 'Something went wrong.';
}

/** What an area shows instead of its panels: before there is a connection ("Connect
 *  first"), while a connect runs, after one failed, and (with `needsLive`, for the areas
 *  that read the live connection) while it is not ready. Returns null when the area can
 *  draw. */
export function connectFirst(state, { needsLive = false } = {}) {
  const card = (text, action = null) => h('div', { class: 'card' }, emptyState(text, action));
  const open = () => h('a', { class: 'btn primary', href: '#/connections', text: 'Open Connections' });
  if (state.connecting) return card(state.connecting.text || `Connecting to ${state.connecting.name} and checking permissions…`);
  if (state.failed) return card(`The connect${state.conn ? ` to ${state.conn.name}` : ''} failed: ${state.failed}`, open());
  if (!state.connected) return card('Connect first: open Connections and pick a connection.', open());
  if (needsLive && state.liveState === 'waiting') return card('Waiting for the live connection…');
  if (needsLive && state.liveState === 'closed') return card(state.lost || 'The live connection is closed.');
  return null;
}
