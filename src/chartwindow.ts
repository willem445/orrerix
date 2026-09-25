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

/** The before/after scope is half-open and symmetric around the mark. */
export function markSpan(markMs: number, k: number, bucketMs: number): [number, number] {
  if (!Number.isFinite(markMs) || !Number.isFinite(k) || !Number.isFinite(bucketMs) || k < 0 || bucketMs < 0)
    return [markMs, markMs];
  return [markMs - k * bucketMs, markMs + k * bucketMs];
}

export interface Point { tsMs: number; value: number }
/** Autoscale only visible samples; empty windows use [0, 1]. */
export function yDomain(points: readonly Point[], win: Window): [number, number] {
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
  const low = Math.max(0, Math.ceil(Math.log10(Math.max(1, domain[0]))));
  const high = Math.floor(Math.log10(domain[1]));
  const ticks = [0];
  for (let e = low; e <= high && ticks.length < 100; e++) ticks.push(10 ** e);
  return ticks;
}

export interface BucketChoice { bucketMs: number; coarsened: boolean; bucketCount: number }
const BUCKET_LADDER = [60_000, 5 * 60_000, 15 * 60_000, 60 * 60_000, 6 * 60 * 60_000, 24 * 60 * 60_000, 7 * 24 * 60 * 60_000, 30 * 24 * 60 * 60_000, 365 * 24 * 60 * 60_000, 100 * 365 * 24 * 60 * 60_000];
/** Cap bucket×key work at 20,000 cells; report when resolution had to coarsen. */
export function chooseBucket(spanMs: number, keyCount: number, cap = 20_000): BucketChoice {
  if (!Number.isFinite(spanMs) || spanMs < 0 || !Number.isFinite(keyCount) || keyCount < 0 || !Number.isFinite(cap) || cap < 1)
    return { bucketMs: BUCKET_LADDER[BUCKET_LADDER.length - 1], coarsened: true, bucketCount: 0 };
  const keys = Math.max(1, Math.ceil(keyCount));
  const allowed = Math.max(1, Math.floor(cap / keys));
  let index = 0;
  while (index < BUCKET_LADDER.length - 1 && Math.ceil(spanMs / BUCKET_LADDER[index]) > allowed) index++;
  // For exceptional extents or a one-cell cap, extend the fixed ladder by
  // powers of two so the returned grid still honours its hard work budget.
  let bucketMs = BUCKET_LADDER[index];
  while (Math.ceil(spanMs / bucketMs) > allowed) bucketMs *= 2;
  return { bucketMs, coarsened: index > 0, bucketCount: Math.ceil(spanMs / bucketMs) };
}
