import test from "node:test";
import assert from "node:assert/strict";
import { chooseBucket, clampWindow, linearTicks, logTicks, logValue, markSpan, panBy, yDomain, zoomAbout } from "../src/chartwindow.ts";

const bounds = { first_ts: 0, last_ts: 10_000, now: 10_000, bucketMs: 100 };
test("zoom preserves the anchor fraction exactly", () => {
  const win = { startMs: 1_000, endMs: 5_000 }, anchor = 2_000;
  const out = zoomAbout(win, anchor, 0.5, bounds);
  assert.equal((anchor - out.startMs) / (out.endMs - out.startMs), (anchor - win.startMs) / (win.endMs - win.startMs));
});
test("zoom clamps to two buckets and to full extent", () => {
  assert.deepEqual(zoomAbout({ startMs: 1_000, endMs: 5_000 }, 3_000, 0.001, bounds), { startMs: 2_900, endMs: 3_100 });
  assert.deepEqual(zoomAbout({ startMs: 1_000, endMs: 5_000 }, 3_000, 100, bounds), { startMs: 0, endMs: 10_000 });
});
test("pan clamps while preserving span", () => {
  const out = panBy({ startMs: 1_000, endMs: 3_000 }, 50_000, bounds);
  assert.deepEqual(out, { startMs: 8_000, endMs: 10_000 });
  assert.equal(out.endMs - out.startMs, 2_000);
});
test("markSpan at k=12 matches beforeAfter's default bucket scope", () => {
  const span = markSpan(50_000_000, 12, 300_000);
  assert.deepEqual(span, [46_400_000, 53_600_000]);
  // tokencharts.ts: DEFAULT_BEFORE_AFTER_K = 12; beforeAfter partitions its
  // buckets around the mark using this exact 12-bucket half-width.
});
test("invalid windows and degenerate requests remain unchanged", () => {
  const inverted = { startMs: 4, endMs: 3 };
  assert.equal(clampWindow(inverted, bounds), inverted);
  assert.equal(panBy(inverted, 1, bounds), inverted);
  const nan = { startMs: NaN, endMs: 4 };
  assert.equal(zoomAbout(nan, 2, 0.5, bounds), nan);
});
test("y domain excludes a thousand-fold out-of-window spike", () => {
  assert.deepEqual(yDomain([{ tsMs: 2, value: 10 }, { tsMs: 3, value: 20 }, { tsMs: 20, value: 20_000 }], { startMs: 0, endMs: 5 }), [10, 20]);
});
test("log mapping is finite for zero and negatives; ticks cover both scales", () => {
  assert.ok(Number.isFinite(logValue(0)));
  assert.ok(Number.isFinite(logValue(-9)));
  assert.deepEqual(logTicks([0, 100]), [0, 1, 10, 100]);
  assert.equal(linearTicks([0, 10], 3).length, 3);
});
test("bucket chooser stays under cell cap and coarsens only when required", () => {
  assert.equal(chooseBucket(600_000, 10, 100).coarsened, false);
  const coarse = chooseBucket(60 * 24 * 60 * 60_000, 20, 100);
  assert.equal(coarse.coarsened, true);
  assert.ok(coarse.bucketCount * 20 <= 100);
});
