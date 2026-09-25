import test from "node:test";
import assert from "node:assert/strict";
import { trendPlot } from "../src/trendplot.ts";

const geom = { widthPx: 220, heightPx: 70, padLeftPx: 20, padRightPx: 0, padTopPx: 10, padBottomPx: 10 };

test("a trend with no measured bucket is EMPTY, never a flat zero line", () => {
  const p = trendPlot({ ...geom, buckets: [0, 100, 200], values: [null, null, null], startMs: 0, endMs: 200 });
  assert.equal(p.empty, true);
  assert.deepEqual(p.segments, []);
  assert.equal(p.measured, 0);
});

test("a measured zero is data: not empty, drawn on the floor", () => {
  const p = trendPlot({ ...geom, buckets: [0, 100, 200], values: [0, 0, 0], startMs: 0, endMs: 200 });
  assert.equal(p.empty, false);
  assert.equal(p.measured, 3);
  for (const pt of p.segments[0]) assert.equal(pt.y, p.yBottom);
});

test("a gap BREAKS the line instead of drawing through zero", () => {
  const p = trendPlot({ ...geom, buckets: [0, 50, 100, 150, 200], values: [4, 8, null, 2, 6], startMs: 0, endMs: 200 });
  assert.equal(p.segments.length, 2);
  assert.deepEqual(p.segments.map((s) => s.length), [2, 2]);
  // Every drawn point is a measured value — none sits at y(0) = yBottom.
  for (const s of p.segments) for (const pt of s) assert.ok(pt.y < p.yBottom);
});

test("x follows the MAIN chart's window, so zooming it narrows the trend", () => {
  const buckets = [0, 100, 200, 300, 400];
  const values = [1, 2, 3, 4, 5];
  const whole = trendPlot({ ...geom, buckets, values, startMs: 0, endMs: 400 });
  const zoomed = trendPlot({ ...geom, buckets, values, startMs: 200, endMs: 400 });
  assert.equal(whole.measured, 5);
  assert.equal(zoomed.measured, 3, "buckets before the zoomed window are not plotted");
  // Bucket 200 sits at the left edge of the zoomed plot and mid-way on the whole one.
  assert.equal(zoomed.segments[0][0].x, zoomed.x0);
  assert.equal(whole.segments[0][2].x, whole.x0 + (whole.x1 - whole.x0) / 2);
});

test("the y-axis always includes zero and tops out at the max", () => {
  const p = trendPlot({ ...geom, buckets: [0, 100], values: [50, 80], startMs: 0, endMs: 100 });
  assert.equal(p.yMin, 0);
  assert.equal(p.yMax, 80);
  assert.equal(p.segments[0][1].y, p.yTop);
});

test("a degenerate window is empty rather than NaN coordinates", () => {
  const p = trendPlot({ ...geom, buckets: [0], values: [3], startMs: 100, endMs: 100 });
  assert.equal(p.empty, true);
});

import { runningRatio } from "../src/trendplot.ts";

test("the running per-item ratio ends on the window's own figure", () => {
  const tokens = [100, 300, 0, 600];
  const items = [0, 2, 0, 1];
  const r = runningRatio(tokens, items);
  assert.deepEqual(r, [null, 200, 200, 1000 / 3]);
  const total = tokens.reduce((a, b) => a + b, 0) / items.reduce((a, b) => a + b, 0);
  assert.equal(r[r.length - 1], total);
});

test("before the first completion there is no per-item figure — null, never 0 or Infinity", () => {
  assert.deepEqual(runningRatio([5, 5], [0, 0]), [null, null]);
});

test("an unknown item count poisons the running ratio from there on", () => {
  assert.deepEqual(runningRatio([10, 10, 10], [1, null, 1]), [10, null, null]);
});
