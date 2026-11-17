// The page is served with `script-src 'self'; style-src 'self'` and may ask no other host
// for anything. This reads the files as text and fails on anything that would break that:
// inline scripts or styles, HTML built from strings, eval, an outside address, a file that
// is referenced but missing.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync, existsSync, statSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const read = (rel) => readFileSync(path.join(root, rel), 'utf8');

/** Every shipped file (the tests are not shipped). */
function shipped(dir = '') {
  const out = [];
  for (const name of readdirSync(path.join(root, dir))) {
    const rel = path.join(dir, name);
    if (rel === 'tests') continue;
    if (statSync(path.join(root, rel)).isDirectory()) out.push(...shipped(rel));
    else out.push(rel);
  }
  return out;
}

const files = shipped();
const scripts = files.filter((f) => f.endsWith('.js'));
const pages = files.filter((f) => f.endsWith('.html'));

test('the fixed file list is all there', () => {
  const want = ['index.html', 'locked.html', 'app.css', 'app.js', 'api.js', 'ui.js', 'keys.js', 'mock.js', 'mark.svg',
    'views/connections.js', 'views/overview.js', 'views/server.js', 'views/pubsub.js', 'views/cmdline.js',
    'views/browse.js', 'views/analyze.js', 'views/streams.js', 'views/search.js'];
  assert.deepEqual(files.slice().sort(), want.slice().sort());
});

test('no inline styles, inline scripts or inline handlers in the pages', () => {
  for (const page of pages) {
    const html = read(page);
    assert.equal(/\sstyle\s*=/i.test(html), false, `${page}: style attribute`);
    assert.equal(/<style[\s>]/i.test(html), false, `${page}: <style> element`);
    assert.equal(/\son[a-z]+\s*=/i.test(html), false, `${page}: inline event handler`);
    for (const tag of html.match(/<script\b[^>]*>[\s\S]*?<\/script>/gi) || []) {
      assert.match(tag, /^<script\b[^>]*\bsrc=/i, `${page}: every script has a src`);
      assert.match(tag, /^<script\b[^>]*>\s*<\/script>$/i, `${page}: a script element has no inline code`);
    }
    assert.equal(/javascript:/i.test(html), false, `${page}: javascript: address`);
  }
  assert.equal(/<script/i.test(read('locked.html')), false, 'locked.html needs no script');
});

test('no HTML is built from strings, and nothing is evaluated', () => {
  const banned = [
    [/\.innerHTML\b/, 'innerHTML'],
    [/\.outerHTML\b/, 'outerHTML'],
    [/insertAdjacentHTML/, 'insertAdjacentHTML'],
    [/document\.write/, 'document.write'],
    [/\beval\s*\(/, 'eval'],
    [/new\s+Function\b/, 'new Function'],
    [/setAttribute\(\s*['"]style['"]/, "setAttribute('style')"],
    [/\.cssText\b/, 'cssText'],
    [/createContextualFragment|DOMParser|srcdoc/, 'HTML parsing'],
    [/setTimeout\(\s*['"`]|setInterval\(\s*['"`]/, 'a timer given a string'],
  ];
  for (const file of scripts) {
    // Comments may name what is forbidden; only code counts.
    const code = read(file).split('\n').filter((line) => !/^\s*(\/\/|\*|\/\*)/.test(line)).join('\n');
    for (const [pattern, name] of banned) assert.equal(pattern.test(code), false, `${file}: ${name}`);
  }
});

test('the element builder refuses style and innerHTML', () => {
  assert.match(read('ui.js'), /key === 'style' \|\| key === 'innerHTML' \|\| key === 'outerHTML'\) throw/);
});

test('no outside address anywhere: no fonts, images, scripts or requests to another host', () => {
  const allowed = new Set(['http://www.w3.org/2000/svg']);
  for (const file of files) {
    for (const url of read(file).match(/\b(?:https?|wss?|ftp):\/\/[^\s"'`)<>]+/gi) || []) {
      assert.ok(allowed.has(url), `${file}: ${url}`);
    }
    assert.equal(/@import|url\(\s*['"]?(?!data:)[a-z]+:/i.test(read(file)) && file.endsWith('.css'), false, `${file}: outside stylesheet or image`);
    assert.equal(/(src|href)\s*=\s*["']\/\//i.test(read(file)), false, `${file}: protocol-relative address`);
  }
});

test('every request goes through api.js: the views and the shell never fetch', () => {
  for (const file of scripts.filter((f) => f !== 'api.js')) {
    const code = read(file);
    assert.equal(/\bfetch\s*\(|XMLHttpRequest|new\s+EventSource|new\s+WebSocket|sendBeacon/.test(code), false, `${file} talks to the network itself`);
  }
});

test('nothing but the theme goes to localStorage, only the session token to sessionStorage, nothing to cookies', () => {
  const code = (file) => read(file).split('\n').filter((l) => !/^\s*(\/\/|\*|\/\*)/.test(l)).join('\n');
  for (const file of scripts) {
    assert.equal(/document\.cookie|indexedDB/.test(read(file)), false, `${file}: storage`);
    if (file !== 'api.js') assert.equal(/sessionStorage/.test(code(file)), false, `${file}: sessionStorage`);
  }
  assert.equal((code('api.js').match(/sessionStorage/g) || []).length, 1, 'api.js reaches sessionStorage in one place, for the token');
  assert.equal(/localStorage/.test(code('api.js')), false, 'the token never goes to localStorage');
  for (const file of scripts) {
    const code = read(file);
    if (file !== 'app.js') assert.equal(/localStorage/.test(code.split('\n').filter((l) => !/^\s*(\/\/|\*|\/\*)/.test(l)).join('\n')), false, `${file}: localStorage`);
  }
  assert.equal((read('app.js').match(/localStorage/g) || []).length, 1, 'app.js touches localStorage in one place, for the theme');
});

test('password boxes are type=password with autocomplete off, and nothing logs', () => {
  const view = read('views/connections.js');
  const boxes = view.match(/input\('(?:password|sentinelPassword)'[^\n]*/g) || [];
  assert.equal(boxes.length, 2);
  for (const box of boxes) assert.match(box, /type: 'password'/);
  assert.match(view, /autocomplete: 'off'/);
  for (const file of scripts) {
    const logs = read(file).match(/console\.[a-z]+\(/g) || [];
    if (file === 'api.js') assert.deepEqual(logs, ['console.error('], 'api.js logs one thing: that a handler failed, by event name');
    else assert.deepEqual(logs, [], `${file} logs`);
  }
});

test('Streams and Search: every box they draw has autocomplete off, and nothing they hold goes to storage or an address', () => {
  for (const file of ['views/streams.js', 'views/search.js']) {
    const src = read(file);
    const inputs = src.match(/h\('input', \{[^}]*\}/g) || [];
    assert.ok(inputs.length > 0, `${file} has inputs`);
    for (const i of inputs) assert.match(i, /autocomplete: 'off'/, `${file}: ${i}`);
    for (const form of src.match(/h\('form', \{[^}]*/g) || []) assert.match(form, /autocomplete: 'off'/, `${file}: a form`);
    assert.equal(/sessionStorage|localStorage|document\.cookie|location\.hash\s*=|history\.(push|replace)State/.test(src), false, `${file}: storage or the address`);
    assert.equal(/\.innerHTML|insertAdjacentHTML/.test(src), false, `${file}: HTML from strings`);
  }
  assert.match(read('views/search.js'), /maxlength: String\(MAX_QUERY\)/, 'the query box takes at most 1,000 characters, as the TUI\'s');
});

test('the token is never logged, never in an address the page builds, never in an error text', () => {
  const api = read('api.js');
  for (const call of api.match(/console\.[a-z]+\([^)]*\)/g) || []) assert.equal(/token/i.test(call), false, call);
  // The only places a token is written: the Authorization header and the tab's storage.
  const uses = api.split('\n').filter((l) => /\$\{t\}|\$\{token\}/.test(l));
  assert.deepEqual(uses.map((l) => l.trim()), ['if (t) out.Authorization = `Bearer ${t}`;']);
  assert.equal(/fetch\w*\([^)]*token/i.test(api), false, 'no request address holds the token');
  for (const file of scripts.filter((f) => f !== 'api.js' && f !== 'mock.js')) {
    assert.equal(/Authorization|#token=|TOKEN_KEY|createTokenStore/.test(read(file)), false, `${file} handles the session token`);
  }
});

test('every file a page or a module refers to exists', () => {
  for (const page of pages) {
    for (const m of read(page).matchAll(/(?:src|href)="([^"#]+)"/g)) {
      assert.match(m[1], /^assets\//, `${page}: ${m[1]} is under assets/`);
      assert.ok(existsSync(path.join(root, m[1].replace(/^assets\//, ''))), `${page}: ${m[1]}`);
    }
  }
  for (const file of scripts) {
    for (const m of read(file).matchAll(/from\s+'(\.[^']+)'/g)) {
      assert.ok(existsSync(path.resolve(root, path.dirname(file), m[1])), `${file}: ${m[1]}`);
    }
  }
});

test('tests/index.js runs every test file', () => {
  const here = path.join(root, 'tests');
  const listed = Array.from(readFileSync(path.join(here, 'index.js'), 'utf8').matchAll(/^import '\.\/([^']+)';$/gm), (m) => m[1]).sort();
  const present = readdirSync(here).filter((f) => f.endsWith('.test.js')).sort();
  assert.deepEqual(listed, present);
});

test('the pages say what they must', () => {
  const locked = read('locked.html');
  assert.match(locked, /Open the dashboard from redis-lens: press <span class="kbd">ctrl-o<\/span> in the terminal, or run <span class="kbd">redis-lens --dashboard<\/span>/);
  const index = read('index.html');
  assert.match(index, /<script type="module" src="assets\/app\.js"><\/script>/);
  assert.match(index, /<html lang="en" data-theme="dark">/);
  for (const area of ['overview', 'browse', 'analyze', 'server', 'pubsub', 'connections']) assert.match(index, new RegExp(`href="#/${area}" data-nav="${area}"`));
  assert.equal(/#\/(streams|search)|data-nav="(streams|search)"/i.test(index), false, 'Streams and Search are panels of Browse, not areas of the nav');
  const browse = read('views/browse.js');
  for (const sub of ['streams', 'search']) assert.match(browse, new RegExp(`href: '#/browse/${sub}'`), `Browse links its ${sub} panel`);
  assert.match(index, /<form class="find" id="find-form" role="search"/, 'Find a key sits in the top bar');
});
