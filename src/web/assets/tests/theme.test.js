// The theme: remembered in a localStorage-like store that may throw, dark by default; and
// the colours in app.css, checked for WCAG AA contrast in both themes.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createThemeStore } from '../ui.js';

function fakeStorage(initial = {}) {
  const data = { ...initial };
  return { data, getItem: (k) => (k in data ? data[k] : null), setItem: (k, v) => { data[k] = String(v); } };
}

test('dark is the default', () => {
  assert.equal(createThemeStore(fakeStorage()).load(), 'dark');
  assert.equal(createThemeStore(null).load(), 'dark');
});

test('the choice is remembered', () => {
  const storage = fakeStorage();
  const themes = createThemeStore(storage);
  assert.equal(themes.save('light'), true);
  assert.equal(storage.data['redis-lens.theme'], 'light');
  assert.equal(createThemeStore(storage).load(), 'light');
  assert.equal(themes.other('light'), 'dark');
  assert.equal(themes.other('dark'), 'light');
});

test('a stored value that is not a theme is ignored, and only themes are stored', () => {
  assert.equal(createThemeStore(fakeStorage({ 'redis-lens.theme': '<script>' })).load(), 'dark');
  const storage = fakeStorage();
  assert.equal(createThemeStore(storage).save('purple'), false);
  assert.deepEqual(storage.data, {});
});

test('a storage that throws (private window) never breaks the page', () => {
  const throwing = { getItem() { throw new Error('denied'); }, setItem() { throw new Error('denied'); } };
  const themes = createThemeStore(throwing);
  assert.equal(themes.load(), 'dark');
  assert.equal(themes.save('light'), false);
  assert.equal(createThemeStore(null).save('light'), false);
});

// ---- Contrast ----

const css = readFileSync(new URL('../app.css', import.meta.url), 'utf8');

function tokens(selector) {
  const start = css.indexOf(`${selector} {`);
  assert.notEqual(start, -1, `${selector} block not found`);
  const block = css.slice(start, css.indexOf('}', start));
  const out = {};
  for (const m of block.matchAll(/--([a-z0-9-]+):\s*([^;]+);/g)) out[m[1]] = m[2].trim();
  return out;
}

function rgb(value) {
  const hex = /^#([0-9a-f]{6})$/i.exec(value);
  if (hex) return [0, 2, 4].map((i) => parseInt(hex[1].slice(i, i + 2), 16)).concat(1);
  const fn = /^rgba?\(([^)]+)\)$/.exec(value);
  assert.ok(fn, `not a colour: ${value}`);
  const parts = fn[1].split(',').map((p) => Number(p.trim()));
  return [parts[0], parts[1], parts[2], parts.length > 3 ? parts[3] : 1];
}

/** `top` drawn over the opaque `under`. */
function over(top, under) {
  const [r, g, b, a] = rgb(top);
  const [ur, ug, ub] = rgb(under);
  return [r * a + ur * (1 - a), g * a + ug * (1 - a), b * a + ub * (1 - a)];
}

function luminance([r, g, b]) {
  const lin = (c) => {
    const v = c / 255;
    return v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
}

function contrast(a, b) {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
}

for (const [name, selector] of [['dark', ':root'], ['light', ':root[data-theme="light"]']]) {
  test(`${name} theme: text meets WCAG AA (4.5:1) on every surface it sits on`, () => {
    const t = tokens(selector);
    const solid = (v) => rgb(v).slice(0, 3);
    const softOnPanel = over(t['accent-soft'], t.panel);
    const softOnBg = over(t['accent-soft'], t.bg);
    const pairs = [
      ['text', solid(t.text), 'bg', solid(t.bg)],
      ['text', solid(t.text), 'panel', solid(t.panel)],
      ['text', solid(t.text), 'panel2', solid(t.panel2)],
      ['text', solid(t.text), 'sel', solid(t.sel)],
      ['text', solid(t.text), 'accent-soft on panel (nav, pills)', softOnPanel],
      ['text', solid(t.text), 'accent-soft on bg (banner, pills)', softOnBg],
      ['muted', solid(t.muted), 'bg', solid(t.bg)],
      ['muted', solid(t.muted), 'panel', solid(t.panel)],
      ['muted', solid(t.muted), 'panel2', solid(t.panel2)],
      ['muted', solid(t.muted), 'sel', solid(t.sel)],
      ['accent', solid(t.accent), 'bg (links, errors in the log)', solid(t.bg)],
      ['accent', solid(t.accent), 'panel (field errors)', solid(t.panel)],
      ['accent-ink', solid(t['accent-ink']), 'accent (primary buttons)', solid(t.accent)],
    ];
    for (const [fgName, fg, bgName, bg] of pairs) {
      const ratio = contrast(fg, bg);
      assert.ok(ratio >= 4.5, `${fgName} on ${bgName}: ${ratio.toFixed(2)}:1`);
    }
  });

  test(`${name} theme: bars, dots and the focus ring meet 3:1 against their surface`, () => {
    const t = tokens(selector);
    const solid = (v) => rgb(v).slice(0, 3);
    for (const [fgName, bgName] of [['blue', 'panel'], ['blue', 'bg'], ['blue', 'panel2'], ['ok', 'panel'], ['ok', 'panel2'], ['accent', 'panel2']]) {
      const ratio = contrast(solid(t[fgName]), solid(t[bgName]));
      assert.ok(ratio >= 3, `${fgName} on ${bgName}: ${ratio.toFixed(2)}:1`);
    }
  });
}

test('both themes define the same tokens', () => {
  const dark = Object.keys(tokens(':root')).filter((k) => k !== 'sans' && k !== 'mono');
  const light = Object.keys(tokens(':root[data-theme="light"]'));
  assert.deepEqual(light.sort(), dark.sort());
});

test('motion is switched off for people who ask for less of it', () => {
  assert.match(css, /@media \(prefers-reduced-motion: reduce\)/);
});
