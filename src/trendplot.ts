// Layout for the token chart's small trend plots (#3505): one plot per
// derived metric, directly under the main chart, sharing its zoom window.
//
// DOM-free and import-free (the TS5097 rule `chartwindow.ts` states), so the
// Node runner tests it directly. The view turns the result into SVG and holds
// no arithmetic of its own — every coordinate and every label value it paints
// comes from here.
//
// **The empty state is part of the answer.** A trend with no measured bucket
// returns `empty: true` and no segments, and the caller prints WHY in words
// ("no items completed in this window"). The alternative the #3475 build had —
// an unlabelled blank strip — reads exactly like a flat zero, and a flat zero
// is a claim the data never made.

export interface TrendPlotInput {
  /** Bucket start instants, ascending. */
  buckets: readonly number[];
  /** One value per bucket; `null`/non-finite is "not measured" and BREAKS the
   *  line rather than drawing it through zero. */
  values: readonly (number | null | undefined)[];
  /** The main chart's window — the x-axis is this, not the buckets' extent,
   *  so every trend lines up under the chart it annotates. */
  startMs: number;
  endMs: number;
  widthPx: number;
  heightPx: number;
  padLeftPx: number;
  padRightPx: number;
  padTopPx: number;
  padBottomPx: number;
}

export interface TrendPlot {
  /** No measured bucket inside the window: draw nothing, say why. */
  empty: boolean;
  /** Polyline runs; a gap in the data starts a new run. A run of ONE point is
   *  kept (the view draws it as a dot) — a lone measured bucket is data. */
  segments: { x: number; y: number }[][];
  /** The y-axis extent the plot maps. Always includes 0 — these are counts,
   *  rates and durations, and an axis floating above zero exaggerates change. */
  yMin: number;
  yMax: number;
  /** Plot-area edges in px. */
  x0: number;
  x1: number;
  yTop: number;
  yBottom: number;
  /** Measured buckets inside the window. */
  measured: number;
}

/** Lay out one trend plot. Samples outside `[startMs, endMs]` are skipped (and
 *  are not counted in `measured`), so zooming the main chart narrows every
 *  trend with it. */
export function trendPlot(input: TrendPlotInput): TrendPlot {
  const x0 = input.padLeftPx;
  const x1 = Math.max(x0, input.widthPx - input.padRightPx);
  const yTop = input.padTopPx;
  const yBottom = Math.max(yTop, input.heightPx - input.padBottomPx);
  const span = input.endMs - input.startMs;
  const base: TrendPlot = { empty: true, segments: [], yMin: 0, yMax: 1, x0, x1, yTop, yBottom, measured: 0 };
  if (!Number.isFinite(span) || span <= 0) return base;

  const pts: ({ t: number; v: number } | null)[] = [];
  let max = -Infinity;
  let min = Infinity;
  const n = Math.min(input.buckets.length, input.values.length);
  for (let i = 0; i < n; i++) {
    const t = input.buckets[i];
    if (!Number.isFinite(t) || t < input.startMs || t > input.endMs) continue;
    const v = input.values[i];
    if (typeof v !== "number" || !Number.isFinite(v)) { pts.push(null); continue; }
    pts.push({ t, v });
    if (v > max) max = v;
    if (v < min) min = v;
  }
  const measured = pts.filter((p) => p !== null).length;
  if (measured === 0) return base;

  const yMin = Math.min(0, min);
  const yMax = max > yMin ? max : yMin + 1;
  const xFor = (t: number) => x0 + ((t - input.startMs) / span) * (x1 - x0);
  const yFor = (v: number) => yBottom - ((v - yMin) / (yMax - yMin)) * (yBottom - yTop);
  const segments: { x: number; y: number }[][] = [];
  let run: { x: number; y: number }[] = [];
  for (const p of pts) {
    if (p === null) { if (run.length > 0) segments.push(run); run = []; continue; }
    run.push({ x: xFor(p.t), y: yFor(p.v) });
  }
  if (run.length > 0) segments.push(run);
  return { empty: false, segments, yMin, yMax, x0, x1, yTop, yBottom, measured };
}
