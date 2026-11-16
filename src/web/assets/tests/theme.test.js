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
    const dangerOnPanel = over(t['danger-soft'], t.panel);
    const dangerOnBg = over(t['danger-soft'], t.bg);
    const pairs = [
      ['text', solid(t.text), 'bg', solid(t.bg)],
      ['text', solid(t.text), 'panel', solid(t.panel)],
      ['text', solid(t.text), 'panel2', solid(t.panel2)],
      ['text', solid(t.text), 'sel', solid(t.sel)],
      ['text', solid(t.text), 'accent-soft on panel (nav, pills)', softOnPanel],
      ['text', solid(t.text), 'accent-soft on bg (pills)', softOnBg],
      ['text', solid(t.text), 'danger-soft on panel (form errors)', dangerOnPanel],
      ['text', solid(t.text), 'danger-soft on bg (banner)', dangerOnBg],
      ['muted', solid(t.muted), 'bg', solid(t.bg)],
      ['muted', solid(t.muted), 'panel', solid(t.panel)],
      ['muted', solid(t.muted), 'panel2', solid(t.panel2)],
      ['muted', solid(t.muted), 'sel', solid(t.sel)],
      ['muted', solid(t.muted), 'accent-soft on panel (the rail\'s current area)', softOnPanel],
      ['accent', solid(t.accent), 'bg (links in boxes)', solid(t.bg)],
      ['accent', solid(t.accent), 'panel (links)', solid(t.panel)],
      ['accent', solid(t.accent), 'sel (a link on a selected row)', solid(t.sel)],
      ['accent-ink', solid(t['accent-ink']), 'accent (primary buttons)', solid(t.accent)],
      ['warn', solid(t.warn), 'bg', solid(t.bg)],
      ['warn', solid(t.warn), 'panel (▲, read-only)', solid(t.panel)],
      ['warn', solid(t.warn), 'panel2 (the access button)', solid(t.panel2)],
      ['warn', solid(t.warn), 'sel (▲ on a selected row)', solid(t.sel)],
      ['danger', solid(t.danger), 'bg (errors in the command output)', solid(t.bg)],
      ['danger', solid(t.danger), 'panel (field errors)', solid(t.panel)],
      ['danger', solid(t.danger), 'panel2 (destructive buttons, writes on)', solid(t.panel2)],
      ['danger', solid(t.danger), 'danger-soft on panel (a destructive button pointed at)', dangerOnPanel],
      ['danger-ink', solid(t['danger-ink']), 'danger (the PROD badge)', solid(t.danger)],
      ['ph', solid(t.ph), 'bg', solid(t.bg)],
      ['ph', solid(t.ph), 'panel ({id} in a pattern)', solid(t.panel)],
      ['ph', solid(t.ph), 'panel2 (a row pointed at)', solid(t.panel2)],
      ['ph', solid(t.ph), 'sel (a selected row)', solid(t.sel)],
    ];
    for (const [fgName, fg, bgName, bg] of pairs) {
      const ratio = contrast(fg, bg);
      assert.ok(ratio >= 4.5, `${fgName} on ${bgName}: ${ratio.toFixed(2)}:1`);
    }
  });

  test(`${name} theme: bars, dots and the focus ring meet 3:1 against their surface`, () => {
    const t = tokens(selector);
    const solid = (v) => rgb(v).slice(0, 3);
    for (const [fgName, bgName] of [
      ['bar', 'panel'], ['bar', 'bg'], ['bar', 'panel2'],
      ['accent', 'panel'], ['accent', 'bg'], ['accent', 'panel2'],
      ['ok', 'panel'], ['ok', 'panel2'],
      ['warn', 'panel'], ['warn', 'panel2'],
      ['danger', 'panel'], ['danger', 'panel2'],
    ]) {
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

test('the colour roles: the values both themes were checked with', () => {
  const want = {
    ':root': { accent: '#6cb6ff', 'accent-ink': '#0b1a2b', warn: '#e8b45c', danger: '#f07662', 'danger-ink': '#1a0d08', ph: '#b79cf5', bar: '#4f8fd6' },
    ':root[data-theme="light"]': { accent: '#1f5fbf', 'accent-ink': '#ffffff', warn: '#8a5a00', danger: '#b3261e', 'danger-ink': '#ffffff', ph: '#6b45c4', bar: '#3a78c9' },
  };
  for (const [selector, values] of Object.entries(want)) {
    const t = tokens(selector);
    for (const [name, value] of Object.entries(values)) assert.equal(t[name], value, `${selector} --${name}`);
    assert.equal(t.blue, undefined, `${selector}: --blue is gone; bars use --bar, focus uses --accent`);
  }
});

/** The declarations of every rule with a selector exactly `selector` (in its list). */
function rulesFor(selector) {
  const out = [];
  const body = css.replace(/\/\*[\s\S]*?\*\//g, '');
  for (const m of body.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    if (m[1].split(',').some((s) => s.trim() === selector)) out.push(m[2]);
  }
  assert.ok(out.length, `no rule for ${selector}`);
  return out.join(';');
}

test('the accent never means a warning or an error', () => {
  const warnings = ['.glyph', '.dot.busy', '.meter.warn > span', '.vbar.warn', '.badge.flagged', '.tbl tr.flag td:first-child', '.bulk'];
  for (const sel of warnings) {
    const decl = rulesFor(sel);
    assert.match(decl, /var\(--warn\)/, `${sel} uses --warn`);
    assert.doesNotMatch(decl, /var\(--accent/, `${sel} does not use the accent`);
  }
  const errors = ['.badge.env-prod', '.field[aria-invalid="true"]', '.field-error', '.form-error', '.banner', '.toast.error', '.toast.error .glyph', '.cmd-out .err', '.btn.danger', '.glyph.bad', '.dot.bad'];
  for (const sel of errors) {
    const decl = rulesFor(sel);
    assert.match(decl, /var\(--danger/, `${sel} uses --danger`);
    assert.doesNotMatch(decl, /var\(--accent/, `${sel} does not use the accent`);
  }
  assert.match(rulesFor(':focus-visible'), /var\(--accent\)/, 'the focus ring is the accent');
  assert.match(rulesFor('.meter > span'), /var\(--bar\)/, 'meters use --bar');
  assert.match(rulesFor('.bars .bar'), /var\(--bar\)/, 'bar charts use --bar');
  assert.match(rulesFor('.vbar'), /var\(--bar\)/, 'the expiry bars use --bar');
  assert.match(rulesFor('.ph'), /var\(--ph\)/, 'placeholders use --ph');
  assert.match(rulesFor('.btn.primary'), /var\(--accent\)/, 'the primary button is the accent');
  assert.match(rulesFor('.cmd-drawer'), /border-top: 2px solid var\(--accent\)/, 'the command line has a focus-coloured top border');
  assert.doesNotMatch(css, /var\(--blue\)/);
});

test('compact density, with 44px targets again on a small screen', () => {
  const head = css.slice(0, css.indexOf('@media (max-width: 780px)'));
  const small = css.slice(css.indexOf('@media (max-width: 780px)'), css.indexOf('@media (max-width: 480px)'));
  const decl = (re) => head.replace(/\/\*[\s\S]*?\*\//g, '').match(new RegExp(`(?:^|\\})\\s*${re}\\s*\\{([^}]*)\\}`))[1];
  assert.match(decl('\\.btn'), /min-height: 30px/);
  assert.match(decl('\\.field'), /min-height: 30px/);
  assert.match(decl('\\.pill'), /min-height: 30px/);
  assert.match(decl('\\.row'), /min-height: 28px/);
  assert.match(decl('\\.tbl td'), /height: 28px/);
  assert.match(decl('\\.card'), /padding: 12px/);
  assert.match(decl('\\.card'), /border-radius: 8px/);
  assert.doesNotMatch(decl('\\.main-inner'), /max-width/, 'the main column uses the window width');
  assert.match(small, /\.btn, \.field, \.pill[^{]*\{ min-height: 44px; \}/);
  assert.match(small, /\.row \{ min-height: 44px; \}/);
  assert.match(small, /\.tbl td \{ height: 44px; \}/);
});

test('motion is switched off for people who ask for less of it', () => {
  assert.match(css, /@media \(prefers-reduced-motion: reduce\)/);
});
