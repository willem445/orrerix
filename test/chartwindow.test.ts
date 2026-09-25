import test from "node:test";
import assert from "node:assert/strict";
import { clampWindow, linearTicks, logTicks, logValue, markSpan, meanFinite, trendSampleCount, panBy, yDomain, zoomAbout } from "../src/chartwindow.ts";
import { beforeAfter, bucketSeries, DEFAULT_BEFORE_AFTER_K, DEFAULT_BUCKET_MS, TOTAL_ROW } from "../src/tokencharts.ts";

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
test("markSpan matches beforeAfter's snapped scope for an off-grid mark", () => {
  const bucketMs = DEFAULT_BUCKET_MS;
  const gridStart = Math.floor(1_700_000_000_000 / bucketMs) * bucketMs;
  let cumulative = 0;
  const rows = Array.from({ length: 61 }, (_, i) => {
    cumulative += i;
    return {
      kind: "sample" as const,
      ts_ms: gridStart + i * bucketMs,
      key: "key",
      agent: "agent",
      block: "worker-std",
      cli: "pi",
      role: "worker",
      in: cumulative,
      out: 0,
      cache_w: 0,
      cache_r: 0,
      cost_usd: null,
      estimated: false,
      source: "test",
      model: "test",
    };
  });
  const series = bucketSeries(rows, {
    startMs: gridStart,
    endMs: gridStart + 60 * bucketMs,
    bucketMs,
  });
  const mark = gridStart + 25.5 * bucketMs;
  const k = DEFAULT_BEFORE_AFTER_K;
  const split = series.buckets.findIndex((bucketStart) => bucketStart >= mark);
  const scope = markSpan(mark, k, bucketMs);
  const readout = beforeAfter(series, mark, "total", k).find((row) => row.key === TOTAL_ROW)!;

  assert.notEqual(mark % bucketMs, 0, "fixture must keep the mark off the bucket grid");
  assert.deepEqual(scope, [series.buckets[split - k], series.buckets[split + k]]);
  assert.equal(readout.before, 19.5);
  assert.equal(readout.after, 31.5);
  assert.equal(scope[1] - scope[0], 2 * k * bucketMs);
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
test("y domain handles a large visible series without argument-spread limits", () => {
  const points = Array.from({ length: 200_000 }, (_, tsMs) => ({ tsMs, value: tsMs }));
  assert.deepEqual(yDomain(points, { startMs: 0, endMs: 199_999 }), [0, 199_999]);
});

test("log mapping is finite for zero and negatives; ticks cover both scales", () => {
  assert.ok(Number.isFinite(logValue(0)));
  assert.ok(Number.isFinite(logValue(-9)));
  const ticks = logTicks([0, 100]);
  assert.deepEqual(ticks, [0, 10, 100]);
  assert.equal(new Set(ticks.map(logValue)).size, ticks.length, "log ticks must not overlap at the floor");
  assert.equal(linearTicks([0, 10], 3).length, 3);
});
test("trend sample population counts measured zero buckets but not unavailable ones", () => {
  assert.equal(trendSampleCount([0, 2, null, undefined, Number.NaN]), 2);
});

test("meanFinite averages measured pane means and ignores missing series", () => {
  assert.equal(meanFinite([null, 4, 8, undefined]), 6);
  assert.equal(meanFinite([null, undefined]), null);
});


// ── #3505: pointer geometry for the container-level handlers ────────────────
import { DELTA_LINE, DELTA_PAGE, DELTA_PIXEL, bucketIndexAt, insidePlot, isDrag, markNear, wheelZoomFactor } from "../src/chartwindow.ts";

test("a LINE-mode wheel notch zooms as much as a PIXEL-mode one", () => {
  // Chromium reports a notch as ~100px in pixel mode; a line-mode device
  // reports 3 lines. Read as pixels, 3 would zoom by 0.45% — invisible.
  const px = wheelZoomFactor(100, DELTA_PIXEL);
  const lines = wheelZoomFactor(3, DELTA_LINE);
  assert.ok(lines > 1.05, `line-mode notch must visibly zoom, got ${lines}`);
  assert.ok(Math.abs(Math.log(lines) - Math.log(px)) < 0.1);
});
test("wheel down zooms out, up zooms in, nothing is exactly 1", () => {
  assert.ok(wheelZoomFactor(100, DELTA_PIXEL) > 1);
  assert.ok(wheelZoomFactor(-100, DELTA_PIXEL) < 1);
  assert.equal(wheelZoomFactor(0, DELTA_PIXEL), 1);
  assert.equal(wheelZoomFactor(Number.NaN, DELTA_PIXEL), 1);
});
test("one PAGE-mode event cannot zoom the history away", () => {
  const f = wheelZoomFactor(1, DELTA_PAGE, 10_000);
  assert.ok(f < 2, `a single event is capped, got ${f}`);
});
test("bucketIndexAt maps the plot edges to the first and last bucket and clamps outside", () => {
  assert.equal(bucketIndexAt(50, 50, 150, 11), 0);
  assert.equal(bucketIndexAt(150, 50, 150, 11), 10);
  assert.equal(bucketIndexAt(100, 50, 150, 11), 5);
  assert.equal(bucketIndexAt(0, 50, 150, 11), 0);
  assert.equal(bucketIndexAt(999, 50, 150, 11), 10);
  assert.equal(bucketIndexAt(100, 50, 150, 0), null);
  assert.equal(bucketIndexAt(100, 50, 50, 5), null);
});
test("insidePlot excludes the y-axis gutter", () => {
  assert.equal(insidePlot(40, 56, 800), false);
  assert.equal(insidePlot(56, 56, 800), true);
  assert.equal(insidePlot(801, 56, 800), false);
});
test("markNear picks the nearest mark inside the tolerance, earlier on a tie, else null", () => {
  assert.equal(markNear([100, 200, 300], 205, 6), 1);
  assert.equal(markNear([100, 200, 300], 250, 6), null);
  assert.equal(markNear([100, 110], 105, 6), 0);
  assert.equal(markNear([], 5, 6), null);
});
test("a press is a click until it travels past the slop", () => {
  assert.equal(isDrag(100, 102), false);
  assert.equal(isDrag(100, 104), true);
  assert.equal(isDrag(100, 96), true);
});

import { trendBucket, TREND_MAX_BUCKETS } from "../src/chartwindow.ts";
import { averagesOverTime } from "../src/tokenaverages.ts";
import { statCell } from "../src/statcell.ts";

test("trend buckets hold a population: a 24h window is hourly, not per-minute", () => {
  const H = 3_600_000;
  assert.equal(trendBucket(24 * H), H);
  assert.equal(trendBucket(7 * 24 * H), 6 * H);
  assert.equal(trendBucket(18 * 24 * H), 24 * H);
  assert.equal(trendBucket(H), 5 * 60_000);
  for (const span of [H, 6 * H, 24 * H, 7 * 24 * H, 400 * 24 * H, 1e13]) {
    assert.ok(Math.ceil(span / trendBucket(span)) <= TREND_MAX_BUCKETS, `span ${span}`);
  }
  assert.equal(trendBucket(0), 5 * 60_000);
});

test("the pane-average trend is MEASURED on a 24h window at the trend grid (it was empty on #3475's grid)", () => {
  // One pane sampled every five minutes for a day — the live sampler's cadence.
  const H = 3_600_000;
  const deltas = Array.from({ length: 288 }, (_, i) => ({ tsMs: i * 300_000 + 1, agent: "w-1", block: "worker", cli: "claude", role: "worker", model: null, in: 1, out: 1, cache_w: 0, cache_r: 0, total: 2, cost_usd: null }));
  const attribution = { byAgent: new Map() };
  const measured = (bucketMs: number) => averagesOverTime(deltas, attribution, { startMs: 0, endMs: 24 * H, groupBy: "agent", metric: "total", bucketMs, stat: statCell })
    .keys[0].points.filter((p) => p.mean !== null).length;
  assert.equal(measured(60_000), 0, "#3475 put a 24h window on one-minute buckets (five-minute past ~13 keys): every bucket below the n floor");
  assert.equal(measured(5 * 60_000), 0, "...and five-minute buckets are no better");
  assert.ok(measured(trendBucket(24 * H)) >= 20, "the trend grid measures (nearly) every hour");
});
