import test from "node:test";
import assert from "node:assert/strict";
import { chooseBucket, clampWindow, linearTicks, logTicks, logValue, markSpan, panBy, yDomain, zoomAbout } from "../src/chartwindow.ts";
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
test("bucket chooser stays under cell cap and coarsens only when required", () => {
  assert.equal(chooseBucket(600_000, 10, 100).coarsened, false);
  const coarse = chooseBucket(60 * 24 * 60 * 60_000, 20, 100);
  assert.equal(coarse.coarsened, true);
  assert.ok(coarse.bucketCount * 20 <= 100);
});
