#!/usr/bin/env node
// Records docs/dashboard.gif from a running redis-lens dashboard, in headless Chrome.
//
// Usage:
//   REDIS_LENS_SIGNIN='http://127.0.0.1:7379/#token=…' node demo/record-dashboard.mjs [out.gif]
//   node demo/record-dashboard.mjs --signin-file FILE [out.gif]
//
// The sign-in URL is the `Dashboard:` line that `redis-lens --dashboard --no-open` prints.
// With --signin-file, the first sign-in URL found in FILE is used, so the output of
// `redis-lens --dashboard --no-open > FILE` works as it is. The token is kept in memory
// only: it is never printed, written to a file or shown in a frame (the page removes it
// from the address bar, and headless Chrome draws no address bar).
//
// Needs:
//   - the dashboard running against the demo servers (make demo-up), with a saved
//     connection that has every permission and writes on. It is named by --connection
//     (default demo-admin), for example the demo's `admin` ACL user from demo/users.acl:
//       redis-lens conn add demo-admin --url redis://admin@127.0.0.1:6390 --env local --ask-password
//     The script refuses any connection that is not a `local` one on 127.0.0.1.
//   - Google Chrome or Chromium (CHROME=/path/to/chrome to pick one), Node 22 or newer
//     and ffmpeg. No npm packages.
//   - Optional: REDIS_LENS_DEMO_PASSWORD, the password of the demo's `lens` ACL user. With
//     it, four made-up application clients connect to 127.0.0.1:6390 for the recording and
//     send read-only commands, so Clients, Monitor and the charts have something to show.
//
// What it changes, all on the demo server and the dashboard, and put back where it can be:
//   - before recording it disconnects the dashboard, and afterwards connects again to what
//     was connected (or stays disconnected);
//   - it sends `LRANGE queue:emails 0 -1` once, so the slow log has an entry to show;
//   - while recording it subscribes to `orders.*`, publishes a few made-up orders and runs
//     MONITOR for a few seconds, through the dashboard.
//
// Options: --connection NAME, --width CSS_PX (default 1120), --height CSS_PX (default 780),
// --gif-width PX (default 960), --fps N (default 12), --keep-frames DIR.

import { spawn, execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, writeFileSync, rmSync, mkdirSync, existsSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import net from 'node:net';
import path from 'node:path';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---------- arguments ----------

const args = process.argv.slice(2);
const opt = (name, fallback) => {
  const i = args.indexOf(name);
  if (i < 0) return fallback;
  const value = args[i + 1];
  args.splice(i, 2);
  return value;
};
const signinFile = opt('--signin-file', null);
const connection = opt('--connection', 'demo-admin');
const width = Number(opt('--width', '1120'));
const height = Number(opt('--height', '780'));
const gifWidth = Number(opt('--gif-width', '960'));
const fps = Number(opt('--fps', '12'));
const keepFrames = opt('--keep-frames', null);
const out = path.resolve(args[0] || 'docs/dashboard.gif');

const SIGNIN_RE = /http:\/\/(?:127\.0\.0\.1|localhost):\d+\/#token=[0-9a-f]{64}/;
let signIn = process.env.REDIS_LENS_SIGNIN || '';
if (signinFile) signIn = (readFileSync(signinFile, 'utf8').match(SIGNIN_RE) || [''])[0];
if (!SIGNIN_RE.test(signIn)) {
  console.error('No sign-in URL. Set REDIS_LENS_SIGNIN or pass --signin-file FILE (see the top of this file).');
  process.exit(2);
}
signIn = signIn.match(SIGNIN_RE)[0];
const base = signIn.slice(0, signIn.indexOf('/#'));
const token = signIn.slice(signIn.indexOf('#token=') + 7);
/** Every message this script prints goes through here, so the token never does. */
const say = (...parts) => console.log(parts.join(' ').split(token).join('<token>'));

// ---------- the dashboard's API, for setting up and for the publisher ----------

async function api(method, route, body) {
  const headers = { Authorization: `Bearer ${token}` };
  if (method !== 'GET') Object.assign(headers, { Origin: base, 'X-Redis-Lens': '1', 'Content-Type': 'application/json' });
  const res = await fetch(base + route, { method, headers, body: method === 'GET' ? undefined : JSON.stringify(body ?? {}) });
  const text = await res.text();
  let json = null;
  try { json = JSON.parse(text); } catch { /* not JSON */ }
  return { status: res.status, json };
}

/** Run a line on the dashboard's command line, answering yes if it asks. */
async function runLine(line) {
  let r = await api('POST', '/api/cmdline', { line });
  if (r.status === 409 && r.json && r.json.confirm) r = await api('POST', `/api/confirm/${r.json.confirm.id}`, {});
  return r;
}

// ---------- made-up application clients (optional) ----------

function resp(...parts) {
  return `*${parts.length}\r\n${parts.map((p) => `$${Buffer.byteLength(String(p))}\r\n${p}\r\n`).join('')}`;
}

/** One client: signs in as `lens`, names itself, then sends a read now and then. */
function fakeClient(port, password, name, lib, ver, keys, everyMs) {
  const sock = net.connect(port, '127.0.0.1');
  sock.on('error', () => {});
  sock.on('data', () => {});
  sock.write(resp('HELLO', '2', 'AUTH', 'lens', password, 'SETNAME', name));
  sock.write(resp('CLIENT', 'SETINFO', 'LIB-NAME', lib));
  sock.write(resp('CLIENT', 'SETINFO', 'LIB-VER', ver));
  const reads = [(k) => ['TYPE', k], (k) => ['TTL', k], (k) => ['STRLEN', k], (k) => ['PTTL', k]];
  let i = 0;
  const timer = setInterval(() => {
    const k = keys[(i * 7 + name.length) % keys.length];
    sock.write(resp(...reads[i % reads.length](k)));
    i += 1;
  }, everyMs);
  return () => { clearInterval(timer); sock.end(resp('QUIT')); sock.destroy(); };
}

// ---------- Chrome over the DevTools protocol ----------

function chromePath() {
  if (process.env.CHROME) return process.env.CHROME;
  const known = [
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    '/Applications/Chromium.app/Contents/MacOS/Chromium',
    '/usr/bin/google-chrome', '/usr/bin/chromium', '/usr/bin/chromium-browser',
  ];
  const found = known.find((p) => existsSync(p));
  if (!found) throw new Error('Chrome not found: set CHROME to its path.');
  return found;
}

async function launchChrome(profile) {
  const proc = spawn(chromePath(), [
    '--headless=new', '--remote-debugging-port=0', `--user-data-dir=${profile}`, '--no-first-run',
    '--no-default-browser-check', '--hide-scrollbars', '--force-color-profile=srgb', 'about:blank',
  ], { stdio: ['ignore', 'ignore', 'pipe'] });
  let wsUrl = null;
  proc.stderr.on('data', (d) => {
    const m = /DevTools listening on (ws:\/\/\S+)/.exec(String(d));
    if (m) wsUrl = m[1];
  });
  for (let i = 0; i < 100 && !wsUrl; i += 1) await sleep(100);
  if (!wsUrl) throw new Error('Chrome did not start');
  const port = new URL(wsUrl).port;
  const target = (await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()).find((t) => t.type === 'page');
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => { ws.onopen = resolve; ws.onerror = reject; });
  let seq = 0;
  const waiting = new Map();
  const listeners = new Map();
  const problems = [];
  ws.onmessage = (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.id && waiting.has(msg.id)) {
      const { resolve, reject } = waiting.get(msg.id);
      waiting.delete(msg.id);
      if (msg.error) reject(new Error(msg.error.message));
      else resolve(msg.result);
      return;
    }
    if (msg.method === 'Runtime.exceptionThrown') problems.push(`exception: ${msg.params.exceptionDetails.text}`);
    if (msg.method === 'Runtime.consoleAPICalled' && msg.params.type === 'error') problems.push('console.error');
    (listeners.get(msg.method) || []).forEach((fn) => fn(msg.params));
  };
  const send = (method, params = {}) => new Promise((resolve, reject) => {
    seq += 1;
    waiting.set(seq, { resolve, reject });
    ws.send(JSON.stringify({ id: seq, method, params }));
  });
  const on = (method, fn) => listeners.set(method, [...(listeners.get(method) || []), fn]);
  await send('Page.enable');
  await send('Runtime.enable');
  const close = async () => {
    try { await send('Browser.close'); } catch { /* gone */ }
    proc.kill();
  };
  return { send, on, problems, close };
}

// ---------- the pointer drawn into the page ----------

// A pointer and a click ring, drawn by the recording only (the dashboard has none). They are
// manual popovers so they sit in the top layer, above the page's modal dialogs too.
const POINTER = `(() => {
  if (window.__rec) return true;
  const mk = (html, size) => {
    const el = document.createElement('div');
    el.popover = 'manual';
    Object.assign(el.style, { position: 'fixed', inset: 'auto', left: '0px', top: '0px', margin: '0', padding: '0',
      border: '0', background: 'transparent', overflow: 'visible', width: size + 'px', height: size + 'px',
      pointerEvents: 'none', color: 'inherit' });
    el.innerHTML = html;
    document.body.append(el);
    el.showPopover();
    return el;
  };
  const ring = mk('<div></div>', 34);
  const dot = ring.firstChild;
  Object.assign(dot.style, { width: '34px', height: '34px', borderRadius: '50%', boxSizing: 'border-box',
    border: '3px solid rgba(255, 138, 101, 0.95)', opacity: '0', transform: 'scale(0.4)' });
  const arrow = mk('<svg width="24" height="24" viewBox="0 0 24 24"><path d="M4 2.5 L4 19 L8.4 15 L11.4 21.6 L14.2 20.4 L11.3 13.9 L17.2 13.9 Z" fill="#ffffff" stroke="#16181d" stroke-width="1.5" stroke-linejoin="round"/></svg>', 24);
  let x = -40, y = -40;
  const place = () => { arrow.style.transform = 'translate(' + (x - 4) + 'px,' + (y - 2) + 'px)'; };
  const raise = () => { for (const el of [ring, arrow]) { el.hidePopover(); el.showPopover(); } };
  place();
  window.__rec = {
    raise,
    jump(tx, ty) { x = tx; y = ty; place(); },
    move(tx, ty, ms) {
      raise();
      const sx = x, sy = y, t0 = performance.now();
      return new Promise((done) => {
        const step = (now) => {
          const t = Math.min(1, (now - t0) / ms);
          const e = t < 0.5 ? 2 * t * t : 1 - Math.pow(-2 * t + 2, 2) / 2;
          x = sx + (tx - sx) * e; y = sy + (ty - sy) * e; place();
          if (t < 1) requestAnimationFrame(step); else done(true);
        };
        requestAnimationFrame(step);
      });
    },
    click() {
      raise();
      ring.style.transform = 'translate(' + (x - 17) + 'px,' + (y - 17) + 'px)';
      dot.animate([{ opacity: 0.95, transform: 'scale(0.4)' }, { opacity: 0, transform: 'scale(1.25)' }], { duration: 520, easing: 'ease-out' });
    },
  };
  return true;
})()`;

// ---------- the recording ----------

async function main() {
  const work = mkdtempSync(path.join(tmpdir(), 'redis-lens-gif-'));
  const frames = keepFrames ? path.resolve(keepFrames) : path.join(work, 'frames');
  mkdirSync(frames, { recursive: true });

  // What is connected now, to put it back afterwards.
  const before = await api('GET', '/api/state');
  if (before.status !== 200) throw new Error(`the dashboard answered ${before.status}: is the sign-in URL from this run?`);
  const polling = before.json.polling || { paused: false, interval_secs: 2 };
  const was = before.json.status === 'connected' && before.json.connection && !before.json.docker ? before.json.connection.name : null;
  const saved = (await api('GET', '/api/connections')).json.connections;
  const target = saved.find((c) => c.name === connection);
  if (!target) throw new Error(`no saved connection named ${connection}`);

  // Warm up: connect once to check it is the demo, and leave a slow log entry.
  await api('POST', '/api/connect', { name: connection });
  for (let i = 0; i < 40; i += 1) {
    const s = (await api('GET', '/api/state')).json;
    if (s.status === 'connected' || s.status === 'failed') break;
    await sleep(250);
  }
  const state = (await api('GET', '/api/state')).json;
  if (state.status !== 'connected') throw new Error(`could not connect to ${connection}`);
  const c = state.connection;
  if (c.env !== 'local' || !/^127\.0\.0\.1:\d+$/.test(c.endpoint)) throw new Error(`${connection} is not a local connection on 127.0.0.1; refusing to record against it`);
  const port = Number(c.endpoint.split(':')[1]);
  await runLine('LRANGE queue:emails 0 -1');
  const scan = await runLine('SCAN 0 MATCH user:* COUNT 1000');
  const keys = ((scan.json && scan.json.replies && scan.json.replies[0] && scan.json.replies[0].text) || [])
    .map((l) => (l.match(/"(user:[^"]+)"/) || [])[1]).filter(Boolean);
  await api('POST', '/api/disconnect', {});
  await api('POST', '/api/polling', { paused: false, interval_secs: 1 });

  const stops = [];
  if (process.env.REDIS_LENS_DEMO_PASSWORD && keys.length) {
    const pw = process.env.REDIS_LENS_DEMO_PASSWORD;
    stops.push(fakeClient(port, pw, 'checkout-api', 'ioredis', '5.4.1', keys, 45));
    stops.push(fakeClient(port, pw, 'checkout-api', 'ioredis', '5.4.1', keys, 70));
    stops.push(fakeClient(port, pw, 'email-worker', 'redis-py', '5.2.0', keys, 160));
    stops.push(fakeClient(port, pw, 'cart-sync', 'go-redis', '9.7.0', keys, 110));
  }

  const chrome = await launchChrome(path.join(work, 'profile'));
  const { send } = chrome;
  const evaluate = async (expression) => {
    const r = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error(`in the page: ${r.exceptionDetails.text}`);
    return r.result.value;
  };
  const frameList = [];
  let recording = false;
  chrome.on('Page.screencastFrame', (p) => {
    send('Page.screencastFrameAck', { sessionId: p.sessionId }).catch(() => {});
    if (!recording) return;
    const file = path.join(frames, `f${String(frameList.length).padStart(5, '0')}.png`);
    writeFileSync(file, Buffer.from(p.data, 'base64'));
    frameList.push({ file, at: p.metadata.timestamp });
  });

  let mx = width * 0.55;
  let my = height * 0.5;
  /** The centre of the first element matching the selector (and text, when given). */
  const centre = (selector, text = null) => evaluate(`(() => {
    const els = Array.from(document.querySelectorAll(${JSON.stringify(selector)}));
    const el = ${text === null ? 'els.find((e) => e.getClientRects().length)' : `els.find((e) => e.getClientRects().length && e.textContent.trim().startsWith(${JSON.stringify(text)}))`};
    if (!el) return null;
    const r = el.getBoundingClientRect();
    return { x: Math.round(r.left + r.width / 2), y: Math.round(r.top + r.height / 2) };
  })()`);
  async function waitFor(selector, text = null, ms = 15000) {
    for (let t = 0; t < ms; t += 200) {
      const at = await centre(selector, text);
      if (at) return at;
      await sleep(200);
    }
    throw new Error(`never showed: ${selector} ${text ?? ''}`);
  }
  async function pointTo(selector, text = null, ms = 520) {
    const at = await waitFor(selector, text);
    const dist = Math.hypot(at.x - mx, at.y - my);
    await evaluate(`window.__rec.move(${at.x}, ${at.y}, ${Math.round(Math.min(ms, 220 + dist * 0.7))})`);
    mx = at.x;
    my = at.y;
    await send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: mx, y: my });
    return at;
  }
  async function click(selector, text = null, ms) {
    await pointTo(selector, text, ms);
    await sleep(90);
    await evaluate('window.__rec.click()');
    for (const type of ['mousePressed', 'mouseReleased']) await send('Input.dispatchMouseEvent', { type, x: mx, y: my, button: 'left', clickCount: 1 });
    await sleep(100);
  }
  async function typeText(text, perChar = 40) {
    for (const ch of text) {
      await send('Input.insertText', { text: ch });
      await sleep(perChar + Math.round(Math.random() * 20));
    }
  }
  async function enter() {
    const base = { key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13 };
    await send('Input.dispatchKeyEvent', { type: 'keyDown', ...base, text: '\r' });
    await send('Input.dispatchKeyEvent', { type: 'keyUp', ...base });
  }

  let publisher = null;
  try {
    await send('Emulation.setDeviceMetricsOverride', { width, height, deviceScaleFactor: 1, mobile: false });
    await send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: 'dark' }, { name: 'prefers-reduced-motion', value: 'no-preference' }] });
    await send('Page.navigate', { url: signIn });
    await sleep(2500);
    const clean = await evaluate(`(() => { const t = ${JSON.stringify(token)}; return !location.href.includes(t) && !location.href.includes('token=') && !document.body.innerText.includes(t); })()`);
    if (!clean) throw new Error('the page still shows the token; not recording');
    await evaluate("location.hash = '#/connections'");
    await sleep(1500);
    await evaluate(POINTER);
    await evaluate(`window.__rec.jump(${mx}, ${my})`);
    await send('Page.startScreencast', { format: 'png', everyNthFrame: 1 });
    await sleep(300);
    recording = true;
    const t0 = Date.now();
    const mark = (what) => say(`${((Date.now() - t0) / 1000).toFixed(1).padStart(5)} s  ${what}`);

    // Connections, then connect.
    mark('Connections');
    await sleep(1500);
    await click(`.conn-card[aria-label="${connection}"] .btn.primary`);
    await waitFor('.tiles');
    mark('Overview');
    await sleep(2800);

    // Server: Clients, Slow log, Monitor.
    await click('a.navb[data-nav=server]');
    mark('Server › Clients');
    await sleep(2100);
    await click('a, button', 'Slow log');
    mark('Server › Slow log');
    await sleep(2300);
    await click('a, button', 'Monitor');
    await sleep(400);
    await click('button.btn.primary', 'Watch commands for 30 seconds');
    await waitFor('dialog[open] button[type=submit]');
    mark('Monitor asks first');
    await sleep(1100);
    await click('dialog[open] button[type=submit]');
    mark('Monitor running');
    await sleep(2600);
    await click('button.btn.primary', 'Stop');
    await sleep(500);

    // Pub/Sub: subscribe to a pattern, then messages arrive.
    await click('a.navb[data-nav=pubsub]');
    mark('Pub/Sub');
    await sleep(300);
    await click('#sub-target');
    await typeText('orders.*');
    await sleep(250);
    await click('button[type=submit]', 'Subscribe');
    await waitFor('.box.log.tail');
    const orders = [
      ['orders.eu', '{"id":10482,"total":59.90,"items":3}'],
      ['orders.us', '{"id":10483,"total":12.50,"items":1}'],
      ['orders.eu', '{"id":10484,"total":230.00,"items":7}'],
      ['orders.apac', '{"id":10485,"total":41.75,"items":2}'],
      ['orders.us', '{"id":10486,"total":88.10,"items":4}'],
      ['orders.eu', '{"id":10487,"total":19.99,"items":1}'],
      ['orders.us', '{"id":10488,"total":64.00,"items":3}'],
    ];
    let n = 0;
    publisher = setInterval(() => {
      const [channel, message] = orders[n % orders.length];
      n += 1;
      api('POST', '/api/pubsub/publish', { channel, message, shard: false }).catch(() => {});
    }, 450);
    mark('Pub/Sub tail');
    await sleep(2700);
    clearInterval(publisher);
    publisher = null;

    // The command line: a sorted set's top members, as text and then as a table.
    await click('#cmd-toggle');
    await sleep(300);
    if (!(await evaluate("document.activeElement && document.activeElement.id === 'cmd-input'"))) await click('#cmd-input');
    const feed = keys.find((k) => /^user:\d+:feed$/.test(k));
    await typeText(feed ? `ZRANGE ${feed} 0 5 REV WITHSCORES` : 'SCAN 0 MATCH user:* COUNT 10', 28);
    await sleep(200);
    await enter();
    mark('Command line');
    await sleep(1600);
    await click('.cmd .pill', 'Table');
    mark('Table');
    await sleep(2000);
    await click('.cmd button.btn', 'Close');
    await evaluate("window.scrollTo({ top: 0, behavior: 'smooth' }); document.getElementById('main').scrollTo({ top: 0, behavior: 'smooth' }); true");
    await sleep(600);

    // Light theme and back.
    await click('#theme-toggle');
    mark('Light theme');
    await sleep(2000);
    await click('#theme-toggle');
    await sleep(1000);
    mark('end');
    recording = false;
    await send('Page.stopScreencast');
    const end = Date.now() / 1000;

    const leaked = await evaluate(`document.body.innerText.includes(${JSON.stringify(token)})`);
    if (leaked) throw new Error('the token showed on the page; the GIF is not written');
    if (chrome.problems.length) say('page problems:', chrome.problems.join('; '));

    // ffmpeg: each frame lasts until the next one, then one palette for the whole GIF.
    const list = frameList.map((f, i) => {
      const next = i + 1 < frameList.length ? frameList[i + 1].at : end;
      return `file '${f.file}'\nduration ${Math.max(0.01, next - f.at).toFixed(3)}`;
    });
    list.push(`file '${frameList[frameList.length - 1].file}'`);
    writeFileSync(path.join(work, 'frames.txt'), `${list.join('\n')}\n`);
    const filters = `fps=${fps},scale=${gifWidth}:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=256:stats_mode=full[p];[b][p]paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle`;
    execFileSync('ffmpeg', ['-v', 'error', '-y', '-f', 'concat', '-safe', '0', '-i', path.join(work, 'frames.txt'), '-vf', filters, '-loop', '0', out], { stdio: 'inherit' });
    say(`wrote ${out}: ${frameList.length} frames captured, ${(statSync(out).size / 1e6).toFixed(2)} MB`);
  } finally {
    if (publisher) clearInterval(publisher);
    stops.forEach((stop) => stop());
    await chrome.close();
    // Put the dashboard back: the tail and Monitor end with the connection.
    await api('DELETE', '/api/server/monitor').catch(() => {});
    await api('DELETE', '/api/pubsub/subscribe').catch(() => {});
    await api('POST', '/api/polling', { paused: !!polling.paused, interval_secs: polling.interval_secs || 2 }).catch(() => {});
    if (was) await api('POST', '/api/connect', { name: was }).catch(() => {});
    else await api('POST', '/api/disconnect', {}).catch(() => {});
    if (!keepFrames) rmSync(work, { recursive: true, force: true });
  }
}

main().catch((err) => {
  say('failed:', String(err && err.message));
  process.exit(1);
});
