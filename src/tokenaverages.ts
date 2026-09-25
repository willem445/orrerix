// Averages over the token series (#3475 slice B): mean AND median spend per
// agent (pane), per block, per model, and per work item, with n on every
// figure, an optional before/after split at a tuning mark, and a
// time-bucketed form of the same numbers for the trend plots slice E draws.
//
// **What is a sample** is the whole design question, and the answer differs by
// grouping (the design note's §Averages: what is a sample argues it):
//
//  - agent / block / model: ONE sample per INTERVAL — each `Delta`, the spend
//    between two consecutive rows of one usage key. So `n` counts deltas, never
//    rows (a key's baseline row yields none), and a pane that ran away for one
//    interval is one huge sample among modest ones: its mean is dragged up, its
//    median is not. That divergence is the reason both are shown.
//  - item: ONE sample per WORK ITEM — the item's total spend in the window, so
//    the mean is literally `tokens / items`. Only `feature` bars are items: the
//    orchestrator's group-wide spend and `(unattributed)` are not work items,
//    so they are kept OUT of the items denominator and shown as rows of their
//    own, never divided across the items and never silently dropped.
//
// Self-contained by the same rule as `tokencharts.ts`: **no intra-src imports**
// (TS5097). `Delta` and `Attribution` are declared structurally below and the
// view passes the real values in; the five-number cell is INJECTED (`stat`,
// normally `statcell.ts`' `statCell`) rather than imported, for the same
// reason, and `test/tokenaverages.test.ts` injects the real one.

// ── the inputs, structurally ────────────────────────────────────────────────

/** The `Delta` fields this reads (`tokencharts.ts`' `diffRows`). */
export interface AverageDeltaLike {
  tsMs: number;
  agent: string;
  block: string;
  model: string | null;
  in: number;
  out: number;
  cache_w: number;
  cache_r: number;
  total: number;
  /** `null` = no figure, which is not "cost nothing" — see `AverageCell.sum`. */
  cost_usd: number | null;
}

/** The `Attribution` fields this reads (`tokencharts.ts`' `attributeAgents`). */
export interface AverageAttributionLike {
  byAgent: ReadonlyMap<string, { bucket: string; label: string }>;
  buckets: readonly {
    id: string;
    label: string;
    kind: "feature" | "orchestrator" | "unattributed";
  }[];
}

/** The five-number cell's shape (`statcell.ts`' `StatCell`), structurally. */
export interface AverageStatCell {
  n: number;
  dropped: number;
  median: number | null;
  q1: number | null;
  q3: number | null;
  iqr: number | null;
  min: number | null;
  max: number | null;
}

export type AverageMetric = "in" | "out" | "cache_w" | "cache_r" | "total" | "cost_usd";
export type AverageGroupBy = "agent" | "block" | "model" | "item";

// ── constants ───────────────────────────────────────────────────────────────

/** A `null` (or blank) model's key. Its own row, never merged into a real
 *  model — "the sampler recorded no model" is a different fact from any
 *  model's spend. */
export const UNKNOWN_MODEL = "unknown model";

/** `tokencharts.ts`' `UNATTRIBUTED`, re-declared (no imports); the test pins
 *  the two spellings equal. An agent the attribution does not know lands
 *  here, exactly as `featureBars` files it. */
export const UNATTRIBUTED = "(unattributed)";

/** The synthetic row carrying the per-item population in `item` mode. */
export const ITEMS_ROW = "(per work item)";

/** Below this many samples the MEAN is `null` too, not only the median: a
 *  mean of two is as thin a claim as a median of two, and a table that nulled
 *  one and printed the other would invite reading the printed one. Equal to
 *  `statcell.ts`' `MEDIAN_MIN_N`; the test pins that. */
export const AVERAGE_MIN_N = 3;

/** One calendar day — the default time bucket of `averagesOverTime`. Not a
 *  millisecond count: a local day is 23 or 25 hours across a DST boundary, so
 *  day buckets are built with `setDate`, never `n * 86_400_000`. */
export const DAY_BUCKET = "day";

/** Grid ceiling, the same guard `bucketSeries` carries against a nonsense
 *  window — far above any real one. */
const MAX_BUCKETS = 100_000;

// ── outputs ─────────────────────────────────────────────────────────────────

export interface AverageCell {
  /** Samples in the population — deltas, or items in `ITEMS_ROW`. Counts a
   *  sample with an unknown cost too; `cell.n` counts the finite ones. */
  n: number;
  /** Sum of the samples. `null` when any sample is unknown (cost only): a sum
   *  over a hole prints a number smaller than the truth. */
  sum: number | null;
  /** `sum / n`, or `null` when `sum` is, or when `n < AVERAGE_MIN_N`. */
  mean: number | null;
  /** Samples whose value was unknown (`cost_usd: null`). */
  unknown: number;
  /** The injected five-number cell. When `unknown > 0` its figures are nulled
   *  as well (null-poisoned like `sum`), leaving `n`/`dropped` to speak. */
  cell: AverageStatCell;
}

export interface AverageRow {
  key: string;
  label: string;
  /** `all.n`, lifted for sorting and display. */
  n: number;
  all: AverageCell;
  /** Present only when `markTsMs` was given; partitioned on the delta's
   *  `tsMs` (`< mark` is before). */
  before: AverageCell | null;
  after: AverageCell | null;
}

export interface Averages {
  groupBy: AverageGroupBy;
  metric: AverageMetric;
  rows: AverageRow[];
  /** Deltas inside `[startMs, endMs]` — the population the rows are over. */
  inWindow: number;
  /** Deltas outside the window. Counted, never clamped onto an edge. */
  outside: number;
}

export interface AveragesOptions {
  startMs: number;
  endMs: number;
  groupBy: AverageGroupBy;
  markTsMs?: number;
  metric?: AverageMetric;
  stat: (values: readonly (number | null)[]) => AverageStatCell;
}

export interface AverageSeries {
  key: string;
  label: string;
  /** Index-aligned to `AveragesOverTime.buckets`: one cell per bucket, dense —
   *  a bucket with no samples is `n: 0`, never missing. */
  points: AverageCell[];
}

export interface AveragesOverTime {
  groupBy: AverageGroupBy;
  metric: AverageMetric;
  /** The bucket width in ms, or `DAY_BUCKET` for local calendar days. */
  bucket: number | typeof DAY_BUCKET;
  /** Bucket start instants; bucket `i` is `[buckets[i], ends[i])`. */
  buckets: number[];
  ends: number[];
  keys: AverageSeries[];
  inWindow: number;
  outside: number;
}

// ── helpers ─────────────────────────────────────────────────────────────────

function round2(n: number): number {
  return Math.round(n * 100) / 100;
}

const valueOf = (d: AverageDeltaLike, metric: AverageMetric): number | null =>
  metric === "cost_usd" ? d.cost_usd : d[metric];

/** Inclusive at both ends, as `bucketSeries` counts its window. Written as a
 *  positive test so a NaN bound EXCLUDES rather than admits everything. */
const inside = (d: AverageDeltaLike, startMs: number, endMs: number): boolean =>
  d.tsMs >= startMs && d.tsMs <= endMs;

function cellOf(
  values: readonly (number | null)[],
  stat: AveragesOptions["stat"]
): AverageCell {
  let unknown = 0;
  let sum = 0;
  for (const v of values) {
    if (v === null || !Number.isFinite(v)) unknown++;
    else sum += v;
  }
  const n = values.length;
  const poisoned = unknown > 0;
  let cell = stat(values);
  if (poisoned) cell = { ...cell, median: null, q1: null, q3: null, iqr: null, min: null, max: null };
  return {
    n,
    sum: poisoned ? null : sum,
    mean: poisoned || n < AVERAGE_MIN_N ? null : round2(sum / n),
    unknown,
    cell,
  };
}

interface Group {
  key: string;
  label: string;
  values: (number | null)[];
}

/** The stable row list: which keys exist, and their labels. Built from the
 *  WHOLE window so the before/after halves and every time bucket carry the
 *  same keys, in the same order, as the totals. */
function keyList(
  deltas: readonly AverageDeltaLike[],
  attribution: AverageAttributionLike,
  groupBy: AverageGroupBy
): { key: string; label: string }[] {
  if (groupBy === "item") {
    const out: { key: string; label: string }[] = [{ key: ITEMS_ROW, label: ITEMS_ROW }];
    const seen = new Set<string>();
    for (const b of attribution.buckets) {
      if (seen.has(b.id)) continue;
      seen.add(b.id);
      out.push({ key: b.id, label: b.label });
    }
    // A bucket a delta lands on that the attribution's render list lacks —
    // only `(unattributed)` if a caller ever passed a trimmed list. Appended,
    // never dropped.
    for (const d of deltas) {
      const at = attribution.byAgent.get(d.agent);
      const id = at ? at.bucket : UNATTRIBUTED;
      if (seen.has(id)) continue;
      seen.add(id);
      out.push({ key: id, label: at ? at.label : UNATTRIBUTED });
    }
    return out;
  }
  const keys = new Set<string>();
  for (const d of deltas) keys.add(intervalKey(d, groupBy));
  return [...keys].sort((a, b) => a.localeCompare(b)).map((key) => ({ key, label: key }));
}

function intervalKey(d: AverageDeltaLike, groupBy: Exclude<AverageGroupBy, "item">): string {
  if (groupBy === "agent") return d.agent;
  if (groupBy === "block") return d.block;
  return d.model === null || d.model.trim() === "" ? UNKNOWN_MODEL : d.model;
}

/** Group ONE population of deltas (already window-filtered) under `keys`. */
function group(
  deltas: readonly AverageDeltaLike[],
  attribution: AverageAttributionLike,
  groupBy: AverageGroupBy,
  metric: AverageMetric,
  keys: readonly { key: string; label: string }[]
): Group[] {
  const byKey = new Map<string, Group>();
  for (const k of keys) byKey.set(k.key, { key: k.key, label: k.label, values: [] });

  if (groupBy !== "item") {
    for (const d of deltas) byKey.get(intervalKey(d, groupBy))?.values.push(valueOf(d, metric));
    return [...byKey.values()];
  }

  const kindOf = new Map<string, string>();
  for (const b of attribution.buckets) kindOf.set(b.id, b.kind);
  // Per-bar total, null-poisoned: one unknown cost makes that ITEM's total
  // unknown, and so the items row's sum.
  const totals = new Map<string, number | null>();
  for (const d of deltas) {
    const id = attribution.byAgent.get(d.agent)?.bucket ?? UNATTRIBUTED;
    const v = valueOf(d, metric);
    byKey.get(id)?.values.push(v);
    const prev = totals.has(id) ? (totals.get(id) as number | null) : 0;
    totals.set(id, prev === null || v === null ? null : prev + v);
  }
  const items = byKey.get(ITEMS_ROW) as Group;
  // Items in the attribution's render order (the keys' order), so the sample
  // list is deterministic. Only `feature` bars WITH spend here: an item that
  // spent nothing in this window is not a sample of this window.
  for (const k of keys) {
    if (k.key === ITEMS_ROW || kindOf.get(k.key) !== "feature" || !totals.has(k.key)) continue;
    items.values.push(totals.get(k.key) as number | null);
  }
  return [...byKey.values()];
}

// ── the averages ────────────────────────────────────────────────────────────

/**
 * Mean and median spend per key over `[startMs, endMs]`, with n on every cell
 * and a before/after split when `markTsMs` is given.
 *
 * `before.n + after.n === all.n` on every INTERVAL row — the partition is of
 * the deltas. On `ITEMS_ROW` it is not, by definition: an item that spent on
 * both sides of the mark is one item in `all` and one on each side. Its SUM
 * does partition, which is the identity the test pins there.
 */
export function averages(
  deltas: readonly AverageDeltaLike[],
  attribution: AverageAttributionLike,
  opts: AveragesOptions
): Averages {
  const metric = opts.metric ?? "total";
  const within = deltas.filter((d) => inside(d, opts.startMs, opts.endMs));
  const keys = keyList(within, attribution, opts.groupBy);
  const all = group(within, attribution, opts.groupBy, metric, keys);

  const mark = opts.markTsMs;
  const hasMark = typeof mark === "number" && Number.isFinite(mark);
  const before = hasMark
    ? group(within.filter((d) => d.tsMs < (mark as number)), attribution, opts.groupBy, metric, keys)
    : null;
  const after = hasMark
    ? group(within.filter((d) => d.tsMs >= (mark as number)), attribution, opts.groupBy, metric, keys)
    : null;

  const rows = all.map((g, i): AverageRow => {
    const cell = cellOf(g.values, opts.stat);
    return {
      key: g.key,
      label: g.label,
      n: cell.n,
      all: cell,
      before: before ? cellOf(before[i].values, opts.stat) : null,
      after: after ? cellOf(after[i].values, opts.stat) : null,
    };
  });

  return {
    groupBy: opts.groupBy,
    metric,
    rows,
    inWindow: within.length,
    outside: deltas.length - within.length,
  };
}

/** The bucket grid over `[startMs, endMs]`: fixed-width buckets aligned to
 *  multiples of `bucketMs` (as `bucketSeries` aligns its grid), or local
 *  calendar days built with `setDate`. Empty for a degenerate window. */
function grid(
  startMs: number,
  endMs: number,
  bucket: number | typeof DAY_BUCKET
): { starts: number[]; ends: number[] } {
  const starts: number[] = [];
  const ends: number[] = [];
  if (!Number.isFinite(startMs) || !Number.isFinite(endMs) || endMs < startMs) return { starts, ends };
  if (bucket === DAY_BUCKET) {
    const d = new Date(startMs);
    d.setHours(0, 0, 0, 0);
    while (d.getTime() <= endMs && starts.length < MAX_BUCKETS) {
      starts.push(d.getTime());
      d.setDate(d.getDate() + 1);
      ends.push(d.getTime());
    }
    return { starts, ends };
  }
  for (let t = Math.floor(startMs / bucket) * bucket; t <= endMs && starts.length < MAX_BUCKETS; t += bucket) {
    starts.push(t);
    ends.push(t + bucket);
  }
  return { starts, ends };
}

/**
 * The same averages, per time bucket — the trend series slice E plots.
 *
 * Each bucket is its own population, grouped exactly as `averages` groups the
 * whole window and under the SAME key list, so a key's points line up with its
 * totals row: the bucket sums of a key add up to its `averages` sum (the test
 * pins that), and in `item` mode an item is a sample of each bucket it spent
 * in. `markTsMs` is ignored here — a trend line carries its own before/after.
 *
 * The default bucket is one LOCAL CALENDAR DAY (`DAY_BUCKET`): the metric
 * plots are read as "per day", and a five-minute interval holds one delta per
 * key, where a mean and a median are the same number. Pass `bucketMs` for a
 * fixed-width grid instead.
 */
export function averagesOverTime(
  deltas: readonly AverageDeltaLike[],
  attribution: AverageAttributionLike,
  opts: AveragesOptions & { bucketMs?: number }
): AveragesOverTime {
  const metric = opts.metric ?? "total";
  const bucket: number | typeof DAY_BUCKET =
    typeof opts.bucketMs === "number" && Number.isFinite(opts.bucketMs) && opts.bucketMs >= 1
      ? Math.floor(opts.bucketMs)
      : DAY_BUCKET;
  const within = deltas.filter((d) => inside(d, opts.startMs, opts.endMs));
  const keys = keyList(within, attribution, opts.groupBy);
  const { starts, ends } = grid(opts.startMs, opts.endMs, bucket);

  // Deltas per bucket by binary search on the (sorted) starts. A delta past
  // the grid's ceiling has no bucket — counted into `outside`, never clamped.
  const perBucket: AverageDeltaLike[][] = starts.map(() => []);
  let unplaced = 0;
  for (const d of within) {
    let lo = 0;
    let hi = starts.length - 1;
    let at = -1;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (starts[mid] <= d.tsMs) {
        at = mid;
        lo = mid + 1;
      } else hi = mid - 1;
    }
    if (at === -1 || d.tsMs >= ends[at]) unplaced++;
    else perBucket[at].push(d);
  }

  const series: AverageSeries[] = keys.map((k) => ({ key: k.key, label: k.label, points: [] }));
  for (const bucketDeltas of perBucket) {
    const groups = group(bucketDeltas, attribution, opts.groupBy, metric, keys);
    groups.forEach((g, i) => series[i].points.push(cellOf(g.values, opts.stat)));
  }

  return {
    groupBy: opts.groupBy,
    metric,
    bucket,
    buckets: starts,
    ends,
    keys: series,
    inWindow: within.length - unplaced,
    outside: deltas.length - within.length + unplaced,
  };
}
