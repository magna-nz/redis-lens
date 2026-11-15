// Formatting and bar scaling. The byte, count and duration cases are the ones the TUI's
// own tests use (src/format.rs), so the dashboard and the TUI say the same thing.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  DASH, fmtBytes, fmtCount, fmtGrouped, fmtDuration, fmtPercent, fmtRate, fmtTook, fmtClock,
  plural, barHeights, barPercent, pushSeries, padSeries,
} from '../ui.js';

test('bytes, as the TUI formats them', () => {
  assert.equal(fmtBytes(0), '0B');
  assert.equal(fmtBytes(1023), '1023B');
  assert.equal(fmtBytes(1024), '1.0K');
  assert.equal(fmtBytes(3174), '3.1K');
  assert.equal(fmtBytes(10200), '10K');
  assert.equal(fmtBytes(10238), '10K');
  assert.equal(fmtBytes(41984), '41K');
  assert.equal(fmtBytes(21063468), '20M');
  assert.equal(fmtBytes(15891378995), '15G');
});

test('counts, as the TUI formats them', () => {
  const cases = [[9, '9'], [212, '212'], [999, '999'], [1000, '1.0K'], [1462, '1.5K'], [9949, '9.9K'], [9950, '10K'], [9999, '10K'],
    [10000, '10K'], [48000, '48K'], [999499, '999K'], [999500, '1.00M'], [999999, '1.00M'], [1004999, '1.00M'], [1005000, '1.01M'], [1840000, '1.84M'], [7920114, '7.92M']];
  for (const [n, want] of cases) assert.equal(fmtCount(n), want, String(n));
});

test('grouped counts', () => {
  assert.equal(fmtGrouped(0), '0');
  assert.equal(fmtGrouped(999), '999');
  assert.equal(fmtGrouped(85964), '85,964');
  assert.equal(fmtGrouped(1234567), '1,234,567');
});

test('durations, as the TUI formats them', () => {
  assert.equal(fmtDuration(38), '38s');
  assert.equal(fmtDuration(24 * 60 + 10), '24m 10s');
  assert.equal(fmtDuration(11 * 3600 + 4 * 60), '11h 04m');
  assert.equal(fmtDuration(6 * 86400 + 23 * 3600), '6d 23h');
});

test('percentages', () => {
  assert.equal(fmtPercent(0), '0%');
  assert.equal(fmtPercent(0.004), '<1%');
  assert.equal(fmtPercent(0.31), '31%');
  assert.equal(fmtPercent(1), '100%');
});

test('rates for the live tiles', () => {
  assert.equal(fmtRate(0), '0');
  assert.equal(fmtRate(7.5), '7.5');
  assert.equal(fmtRate(212), '212');
  assert.equal(fmtRate(999.6), '1.0K');
  assert.equal(fmtRate(12412), '12.4K');
  assert.equal(fmtRate(1210000), '1.21M');
});

test('how long a command took, as the TUI command line shows it', () => {
  assert.equal(fmtTook(420), '0.42 ms');
  assert.equal(fmtTook(12000), '12 ms');
  assert.equal(fmtTook(1300000), '1.30 s');
});

test('clock times are UTC', () => {
  const at = Date.UTC(2026, 10, 15, 13, 58, 41, 207);
  assert.equal(fmtClock(at), '13:58:41');
  assert.equal(fmtClock(at, true), '13:58:41.207');
});

test('unknown is a dash, never a made-up number', () => {
  for (const f of [fmtBytes, fmtCount, fmtGrouped, fmtDuration, fmtPercent, fmtRate, fmtTook, fmtClock]) {
    assert.equal(f(null), DASH);
    assert.equal(f(undefined), DASH);
    assert.equal(f(Number.NaN), DASH);
    assert.equal(f('12'), DASH);
  }
  assert.equal(fmtBytes(-1), DASH);
});

test('plural picks the noun', () => {
  assert.equal(plural(1, 'subscriber', 'subscribers'), '1 subscriber');
  assert.equal(plural(0, 'subscriber', 'subscribers'), '0 subscribers');
  assert.equal(plural(1200, 'line', 'lines'), '1,200 lines');
});

test('bar heights scale from the smallest to the largest value', () => {
  assert.deepEqual(barHeights([1, 2, 3], 36), [4, 20, 36]);
  assert.deepEqual(barHeights([10, 20], 96), [4, 96]);
});

test('a flat series sits at the minimum, and gaps are empty', () => {
  assert.deepEqual(barHeights([5, 5, 5], 36), [4, 4, 4]);
  assert.deepEqual(barHeights([null, 1, 3], 36), [0, 4, 36]);
  assert.deepEqual(barHeights([], 36), []);
  assert.deepEqual(barHeights([null, null], 36), [0, 0]);
});

test('bar widths are whole percentages, clamped', () => {
  assert.equal(barPercent(63, 100), 63);
  assert.equal(barPercent(0, 100), 0);
  assert.equal(barPercent(0.001, 100), 1, 'a value above zero never draws an empty bar');
  assert.equal(barPercent(200, 100), 100);
  assert.equal(barPercent(5, 0), 0);
  assert.equal(barPercent(null, 100), 0);
});

test('a rolling series keeps the newest points', () => {
  let s = [];
  for (let i = 0; i < 45; i += 1) s = pushSeries(s, i, 40);
  assert.equal(s.length, 40);
  assert.equal(s[0], 5);
  assert.equal(s[39], 44);
  assert.deepEqual(pushSeries([1], undefined), [1, null], 'a missing reading is a gap, not a zero');
});

test('a short series is padded on the left', () => {
  assert.deepEqual(padSeries([1, 2], 4), [null, null, 1, 2]);
  assert.deepEqual(padSeries([1, 2, 3, 4, 5], 4), [2, 3, 4, 5]);
});
