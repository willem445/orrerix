// Pure chart-window and axis math. Kept import-free for the Node test runner
// and TypeScript's TS5097 boundary (the same rule as tokencharts.ts).

export interface Window { startMs: number; endMs: number }
export interface WindowBounds { first_ts: number; last_ts: number; now: number; bucketMs: number }

function validWindow(win: Window): boolean {
  return Number.isFinite(win.startMs) && Number.isFinite(win.endMs) && win.startMs <= win.endMs;
}

function limits(bounds: WindowBounds): [number, number] {
  return [bounds.first_ts, Math.max(bounds.now, bounds.last_ts)];
}

/** Clamp a valid window into the series extent without ever inverting it.
 * Invalid windows/bounds leave the input window untouched. */
export function clampWindow(win: Window, bounds: WindowBounds): Window {
  const [lo, hi] = limits(bounds);
  const minSpan = 2 * bounds.bucketMs;
  if (!validWindow(win) || !Number.isFinite(lo) || !Number.isFinite(hi) || lo > hi ||
      !Number.isFinite(minSpan) || minSpan <= 0) return win;
  const extent = hi - lo;
  const requested = win.endMs - win.startMs;
  const span = Math.min(extent, Math.max(Math.min(minSpan, extent), requested));
  const center = (win.startMs + win.endMs) / 2;
  const start = Math.min(Math.max(center - span / 2, lo), hi - span);
  return { startMs: start, endMs: start + span };
}

/** Scale around an instant: its fractional position within the window is invariant. */
export function zoomAbout(win: Window, anchorMs: number, factor: number, bounds: WindowBounds): Window {
  if (!validWindow(win) || !Number.isFinite(anchorMs) || !Number.isFinite(factor) || factor <= 0) return win;
  const span = win.endMs - win.startMs;
  if (span <= 0) return win;
  const fraction = (anchorMs - win.startMs) / span;
  const newSpan = span * factor;
  if (!Number.isFinite(newSpan)) return win;
  return clampWindow({ startMs: anchorMs - fraction * newSpan, endMs: anchorMs + (1 - fraction) * newSpan }, bounds);
}

/** Translate without changing span; boundary contact shifts the whole window. */
export function panBy(win: Window, deltaMs: number, bounds: WindowBounds): Window {
  if (!validWindow(win) || !Number.isFinite(deltaMs)) return win;
  return clampWindow({ startMs: win.startMs + deltaMs, endMs: win.endMs + deltaMs }, bounds);
}

/** The before/after scope is half-open and symmetric around the snapped split. */
export function markSpan(markMs: number, k: number, bucketMs: number, gridOriginMs = 0): [number, number] {
  if (!Number.isFinite(markMs) || !Number.isFinite(k) || !Number.isFinite(bucketMs) || bucketMs <= 0 || !Number.isFinite(gridOriginMs))
    return [markMs, markMs];
  // beforeAfter splits at the first bucket start >= mark. Snap to that same
  // grid boundary before taking the k-bucket window on either side.
  const split = gridOriginMs + Math.ceil((markMs - gridOriginMs) / bucketMs) * bucketMs;
  const width = Math.max(1, Math.floor(k));
  return [split - width * bucketMs, split + width * bucketMs];
}

export interface Point { tsMs: number; value: number }
/** Count measured samples; zero is a real observation, missing/non-finite is not. */
export function trendSampleCount(values: readonly (number | null | undefined)[]): number {
  let count = 0;
  for (const value of values) if (typeof value === "number" && Number.isFinite(value)) count++;
  return count;
}

/** Arithmetic mean of measured values, ignoring unavailable samples. */
export function meanFinite(values: readonly (number | null | undefined)[]): number | null {
  let sum = 0;
  let count = 0;
  for (const value of values) {
    if (typeof value !== "number" || !Number.isFinite(value)) continue;
    sum += value;
    count++;
  }
  return count === 0 ? null : sum / count;
}
/** Autoscale only visible samples; empty windows use [0, 1]. */
export function yDomain(points: Iterable<Point>, win: Window): [number, number] {
  if (!validWindow(win)) return [0, 1];
  // A dense series can contain hundreds of thousands of visible points.
  // Scan once with constant auxiliary space; spreading them into Math.min/max
  // both allocates a second array and exceeds the engine's argument limit.
  let min = Infinity;
  let max = -Infinity;
  for (const point of points) {
    if (Number.isFinite(point.tsMs) && point.tsMs >= win.startMs && point.tsMs <= win.endMs && Number.isFinite(point.value)) {
      min = Math.min(min, point.value);
      max = Math.max(max, point.value);
    }
  }
  if (min === Infinity) return [0, 1];
  if (min === max) { const pad = Math.abs(min) * 0.05 || 1; min -= pad; max += pad; }
  return [min, max];
}

/** Log mapping uses log10(max(1, value)): zero/negative values sit at the floor. */
export function logValue(value: number): number {
  return Math.log10(Math.max(1, Number.isFinite(value) ? value : 1));
}

export function linearTicks(domain: readonly [number, number], count = 5): number[] {
  if (!domain.every(Number.isFinite) || domain[1] <= domain[0] || !Number.isFinite(count) || count < 2) return [];
  return Array.from({ length: Math.floor(count) }, (_, i) => domain[0] + (domain[1] - domain[0]) * i / (Math.floor(count) - 1));
}

/** Powers of ten spanning positive values, with an explicit zero-floor tick. */
export function logTicks(domain: readonly [number, number]): number[] {
  if (!domain.every(Number.isFinite) || domain[1] < 1) return [0];
  // Values below ten all map to the floor at log10(1) === 0, so emitting
  // both 0 and 1 would place two labels on the same pixel.
  const low = Math.max(1, Math.ceil(Math.log10(Math.max(1, domain[0]))));
  const high = Math.floor(Math.log10(domain[1]));
  const ticks = [0];
  for (let e = low; e <= high && ticks.length < 100; e++) ticks.push(10 ** e);
  return ticks;
}

/** Bucket widths a trend may use, finest first. */
const BUCKET_LADDER = [60_000, 5 * 60_000, 15 * 60_000, 60 * 60_000, 6 * 60 * 60_000, 24 * 60 * 60_000, 7 * 24 * 60 * 60_000, 30 * 24 * 60 * 60_000, 365 * 24 * 60 * 60_000, 100 * 365 * 24 * 60 * 60_000];


// ── pointer geometry (#3505) ────────────────────────────────────────────────
// The live chart's handlers sit on the plot CONTAINER and resolve every
// pointer position through these, so the answer never depends on which SVG
// child happened to be under the cursor — or on whether that child survived
// the last re-render.

/** WheelEvent.deltaMode values, spelled out: the DOM constants live on
 *  `WheelEvent`, which the Node test runner does not have. */
export const DELTA_PIXEL = 0;
export const DELTA_LINE = 1;
export const DELTA_PAGE = 2;
/** One wheel "line" in px — what Chromium itself scrolls per line. */
const LINE_PX = 16;
/** A single event's contribution is capped, so one fast flick or a PAGE-mode
 *  device cannot zoom the whole history away in one step. */
const MAX_WHEEL_PX = 400;
/** Zoom per pixel of wheel travel: a 100 px notch is ~16%, and a precision
 *  touchpad's stream of small deltas adds up to the same per distance. */
const ZOOM_PER_PX = 0.0015;

/** The zoom factor for one wheel event: > 1 zooms out (wheel down), < 1 in.
 *  Normalises `deltaMode` — a LINE-mode device reports `3` for a notch, which
 *  read as pixels would zoom by 0.3% and look like nothing happened. A
 *  non-finite or zero delta is exactly `1` (no change). */
export function wheelZoomFactor(deltaY: number, deltaMode: number, pagePx = 800): number {
  if (!Number.isFinite(deltaY) || deltaY === 0) return 1;
  const unit = deltaMode === DELTA_LINE ? LINE_PX : deltaMode === DELTA_PAGE ? (Number.isFinite(pagePx) && pagePx > 0 ? pagePx : 800) : 1;
  const px = Math.max(-MAX_WHEEL_PX, Math.min(MAX_WHEEL_PX, deltaY * unit));
  return Math.exp(px * ZOOM_PER_PX);
}

/** Whether a container-relative x lies on the plot area `[x0, x1]`. */
export function insidePlot(x: number, x0: number, x1: number): boolean {
  return Number.isFinite(x) && x1 > x0 && x >= x0 && x <= x1;
}

/** Whether a container-relative point lies on the plot RECTANGLE — the
 *  gutters on all four sides (y-axis labels, top padding, the time-tick
 *  strip) excluded. The wheel and a drag are claimed only here, so over any
 *  label the wheel still scrolls the panel (#3505 review). */
export function insidePlotArea(x: number, y: number, x0: number, x1: number, y0: number, y1: number): boolean {
  return insidePlot(x, x0, x1) && Number.isFinite(y) && y1 > y0 && y >= y0 && y <= y1;
}

/** The bucket index nearest a container-relative x, or `null` when there are
 *  no buckets or the plot has no width. Clamped onto the grid otherwise, so a
 *  pointer resting on the gutter reads the edge bucket rather than nothing. */
export function bucketIndexAt(x: number, x0: number, x1: number, count: number): number | null {
  if (!Number.isFinite(x) || !(x1 > x0) || !Number.isInteger(count) || count <= 0) return null;
  if (count === 1) return 0;
  const frac = Math.min(1, Math.max(0, (x - x0) / (x1 - x0)));
  return Math.round(frac * (count - 1));
}

/** The index of the mark nearest `x` within `tolerancePx`, or `null`. Ties go
 *  to the earlier mark — deterministic, never "whichever drew last". */
export function markNear(markXs: readonly number[], x: number, tolerancePx: number): number | null {
  if (!Number.isFinite(x) || !(tolerancePx >= 0)) return null;
  let best: number | null = null;
  let bestD = Infinity;
  for (let i = 0; i < markXs.length; i++) {
    const d = Math.abs(markXs[i] - x);
    if (Number.isFinite(d) && d <= tolerancePx && d < bestD) { best = i; bestD = d; }
  }
  return best;
}

/** A press becomes a drag only past this many px, so a click on a mark is
 *  still a click even with a slightly unsteady hand. */
export const DRAG_SLOP_PX = 3;
export function isDrag(downX: number, x: number): boolean {
  return Number.isFinite(downX) && Number.isFinite(x) && Math.abs(x - downX) > DRAG_SLOP_PX;
}

/** At most this many buckets on a trend plot. */
export const TREND_MAX_BUCKETS = 48;
/** The trend grid never goes finer than this. */
const TREND_MIN_BUCKET_MS = 5 * 60_000;

/** The bucket width for the TREND plots (#3505) — deliberately NOT
 *  the finest grid a work budget allows, which #3475 used and which is wrong
 *  for a derived metric: on a 24 h window that is one-minute buckets, where a pane has one delta per
 *  bucket and no bucket holds the three samples a median or mean needs, so
 *  every average/median trend came out empty. A trend wants a population per
 *  bucket: the smallest ladder step (≥ 5 min) that covers the span in at most
 *  `TREND_MAX_BUCKETS`. */
export function trendBucket(spanMs: number, maxBuckets = TREND_MAX_BUCKETS): number {
  const cap = Number.isFinite(maxBuckets) && maxBuckets >= 1 ? Math.floor(maxBuckets) : TREND_MAX_BUCKETS;
  if (!Number.isFinite(spanMs) || spanMs <= 0) return TREND_MIN_BUCKET_MS;
  for (const step of BUCKET_LADDER) {
    if (step < TREND_MIN_BUCKET_MS) continue;
    if (Math.ceil(spanMs / step) <= cap) return step;
  }
  let step = BUCKET_LADDER[BUCKET_LADDER.length - 1];
  while (Math.ceil(spanMs / step) > cap) step *= 2;
  return step;
}
