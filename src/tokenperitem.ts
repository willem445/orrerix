// Tokens per completed work item, and the role share of that spend (#3475
// slice C). DOM-free; `tokenchartsview.ts` (slice E) paints what this returns.
//
// Self-contained by the same rule as `tokencharts.ts`: **no intra-src imports
// at all** (TS5097 — see `embedsplit.ts`). The shapes read here — a `Delta`, an
// `Attribution`, a board row — are declared STRUCTURALLY as the subset this
// module needs, so the view passes `tokencharts.ts`'s real values straight in
// and TypeScript checks the two descriptions against each other at that call
// site. `isOrchestratorRole` is re-stated (one line) for the same reason.
//
// What divides what (the design note, `docs/design/token-charts.md` § *Tokens
// per completed item — what divides what*, carries the argument):
//
//  - The NUMERATOR is every token the group spent inside the window — the
//    orchestrator's included, in-flight work and unattributed spend included.
//    It is a throughput ratio: spend over a period divided by what that period
//    finished. That is the only reading under which the before/after halves
//    and the per-day buckets partition the whole: an item's own spend is
//    spread over days it was not finished on, so "tokens of the items finished
//    today" is not a quantity any bucket owns. `byClass` then says how much of
//    the numerator sat on the done items themselves, so the reader is never
//    handed the ratio without its composition.
//  - The DENOMINATOR is the caller's `doneIds`. The orchestrator is in the
//    numerator (it IS a cost of the items) and is never an item; its role row
//    says so.
//  - An item count is placed on a side of a mark, or in a bucket, only by its
//    DONE INSTANT (`doneAtMs`), and only when that instant is inside the
//    window — one outside it was not done in this window, and the totals and
//    the series count it unplaced alike (`placeItems`). A bare id set carries
//    no instant, so those counts are `null` — never estimated from a board
//    row's `updated_ms`, which is the last write and not the done moment.

// ── the inputs, structurally ────────────────────────────────────────────────

/** The subset of `tokencharts.ts`'s `Delta` read here. */
export interface PerItemDeltaLike {
  tsMs: number;
  agent: string;
  role: string;
  in: number;
  out: number;
  cache_w: number;
  cache_r: number;
  total: number;
}

/** The subset of `tokencharts.ts`'s `AgentAttribution` read here. */
export interface PerItemAgentLike {
  bucket: string;
  matched: string | null;
  via: string;
}

/** The subset of `tokencharts.ts`'s `Attribution` read here. */
export interface PerItemAttributionLike {
  byAgent: ReadonlyMap<string, PerItemAgentLike>;
}

/** The subset of a board row read here — the container walk only. */
export interface PerItemBoardRowLike {
  id: string;
  parent?: string | null;
}

/** Which counter is summed. `total` (the default) is the four summed; the
 *  others let a caller leave cache reads out, which a 1M-context orchestrator
 *  otherwise dominates (#3475's scope addition). */
export type PerItemMetric = "in" | "out" | "cache_w" | "cache_r" | "total";

export interface PerItemOpts {
  /** Window, INCLUSIVE at both ends — the same rule `featureBars` applies, so
   *  the numerator here equals the bars' total over the same window. */
  startMs: number;
  endMs: number;
  /** The completed items: board row ids. Slice D's lifecycle projection
   *  supplies the ids done INSIDE the window; until it lands the view passes
   *  the board's currently-`done` rows ("board state, not dated"). */
  doneIds: ReadonlySet<string>;
  /** Each item's done instant. Optional, and the only thing that can place an
   *  item before/after a mark or into a bucket — and only an instant inside
   *  the window does; see the module header. */
  doneAtMs?: ReadonlyMap<string, number>;
  /** The tuning mark. When given, `before`/`after` partition the numerator on
   *  the DELTA's instant (`tsMs < markTsMs` is before) and the items on their
   *  done instant. */
  markTsMs?: number;
  metric?: PerItemMetric;
}

// ── the outputs ─────────────────────────────────────────────────────────────

export interface RoleShare {
  /** The role as the series row carried it; an empty one reads `unknown`.
   *  Never looked up in a list — a role this build has never heard of keeps
   *  its own row (the `"claude" ?` rule applied to roles). */
  role: string;
  tokens: number;
  /** `tokens / whole`; `null` when the whole is 0 (no share of nothing). */
  share: number | null;
  /** Deltas on this row. */
  n: number;
  orchestrator: boolean;
  /** Stated on the orchestrator's row: counted, never an item. */
  note: string | null;
}

/** Where the numerator's tokens sat, by the attribution ladder. Sums to
 *  `tokens`. */
export interface ByClass {
  /** Agents whose matched board row (or a container of it) is a done item. */
  done: number;
  orchestrator: number;
  /** Attributed to a board row that is not among the done items. */
  inFlight: number;
  /** No attribution, or the ladder's `none` rung. */
  unattributed: number;
}

export interface PerItemSide {
  /** `null` when the items carry no done instants and so cannot be placed. */
  items: number | null;
  tokens: number;
  /** `tokens / items`; `null` when `items` is 0 or unknown — never `0` and
   *  never `Infinity`. */
  perItem: number | null;
  /** Deltas counted. */
  n: number;
  byRole: RoleShare[];
  byClass: ByClass;
}

export interface PerCompletedItem extends PerItemSide {
  /** `doneIds.size` — the whole population is always known. */
  items: number;
  /** Deltas outside the window: counted, never silently dropped. */
  excluded: number;
  /** Whether `doneAtMs` was supplied. False is the pre-slice-D "board state,
   *  not dated" figure. */
  dated: boolean;
  /** Done ids with no instant in `doneAtMs`, or one OUTSIDE the window — not
   *  done in it, so placed on neither side of a mark; the series counts the
   *  same ids `unplacedItems`. 0 when undated as a whole — then every
   *  half/bucket item count is `null` instead. */
  undatedItems: number;
  before?: PerItemSide;
  after?: PerItemSide;
}

export const ORCHESTRATOR_ROLE_NOTE = "group-wide: counted in the numerator, never an item";

const UNKNOWN = "unknown";
const nonEmpty = (v: string | null | undefined): string => (typeof v === "string" ? v.trim() : "");
const labelOf = (v: string | null | undefined): string => nonEmpty(v) || UNKNOWN;
const isOrchestratorRole = (role: string | null | undefined): boolean =>
  nonEmpty(role).toLowerCase() === "orchestrator";
const num = (v: unknown): number => (typeof v === "number" && Number.isFinite(v) ? v : 0);

// ── accumulation ────────────────────────────────────────────────────────────

type ClassOf = (d: PerItemDeltaLike) => keyof ByClass;

/** Decide each agent's class once, from the attribution and the board. A done
 *  item may be the matched row itself or any container above it (a `done`
 *  feature finishes its tasks), so the container chain is walked, cycle-safe,
 *  exactly as `tokencharts.ts`'s `chainOf` does. */
function classifier(
  attribution: PerItemAttributionLike,
  board: readonly PerItemBoardRowLike[],
  doneIds: ReadonlySet<string>
): ClassOf {
  const byId = new Map<string, PerItemBoardRowLike>();
  for (const r of board) if (!byId.has(r.id)) byId.set(r.id, r);
  const chainHitsDone = (start: string): boolean => {
    const seen = new Set<string>();
    let cur = nonEmpty(start);
    while (cur && !seen.has(cur)) {
      if (doneIds.has(cur)) return true;
      seen.add(cur);
      cur = nonEmpty(byId.get(cur)?.parent);
    }
    return false;
  };
  const memo = new Map<string, keyof ByClass>();
  return (d) => {
    // The delta's own role decides the orchestrator class too, so the class
    // split and the role split agree on what "the orchestrator" spent.
    if (isOrchestratorRole(d.role)) return "orchestrator";
    const hit = memo.get(d.agent);
    if (hit) return hit;
    const at = attribution.byAgent.get(d.agent);
    let c: keyof ByClass;
    if (!at || at.via === "none") c = "unattributed";
    else if (at.via === "orchestrator") c = "orchestrator";
    else if ((at.matched !== null && chainHitsDone(at.matched)) || doneIds.has(at.bucket)) c = "done";
    else c = "inFlight";
    memo.set(d.agent, c);
    return c;
  };
}

class Acc {
  tokens = 0;
  n = 0;
  roles = new Map<string, { tokens: number; n: number }>();
  byClass: ByClass = { done: 0, orchestrator: 0, inFlight: 0, unattributed: 0 };

  add(d: PerItemDeltaLike, v: number, c: keyof ByClass): void {
    this.tokens += v;
    this.n++;
    this.byClass[c] += v;
    const role = labelOf(d.role);
    const r = this.roles.get(role);
    if (r) {
      r.tokens += v;
      r.n++;
    } else this.roles.set(role, { tokens: v, n: 1 });
  }

  side(items: number | null): PerItemSide {
    const whole = this.tokens;
    const byRole: RoleShare[] = [...this.roles.entries()]
      .map(([role, r]) => {
        const orchestrator = isOrchestratorRole(role);
        return {
          role,
          tokens: r.tokens,
          share: whole > 0 ? r.tokens / whole : null,
          n: r.n,
          orchestrator,
          note: orchestrator ? ORCHESTRATOR_ROLE_NOTE : null,
        };
      })
      // Largest first; a tie breaks on the name so the order never depends on
      // which delta happened to arrive first.
      .sort((a, b) => b.tokens - a.tokens || (a.role < b.role ? -1 : a.role > b.role ? 1 : 0));
    return {
      items,
      tokens: whole,
      perItem: items !== null && items > 0 ? whole / items : null,
      n: this.n,
      byRole,
      byClass: { ...this.byClass },
    };
  }
}

const inWindow = (t: number, startMs: number, endMs: number): boolean =>
  Number.isFinite(t) && t >= startMs && t <= endMs;

/**
 * Count the done ids into `slots` by their done instant. An id is PLACED only
 * when it has an instant INSIDE the window; one with no instant, or with an
 * instant before the window opened or after it closed, is `unplaced` — it was
 * not done in this window, so no half and no bucket may claim it. The ONE rule
 * both entry points read, so the totals' halves and the series' buckets cannot
 * disagree on which items they place (#3490 N1). `slotOf` returning `-1` is
 * also unplaced.
 */
function placeItems(
  opts: Pick<PerItemOpts, "startMs" | "endMs" | "doneIds" | "doneAtMs">,
  slots: number,
  slotOf: (at: number) => number
): { counts: number[]; unplaced: number } {
  const counts: number[] = new Array(slots).fill(0);
  let unplaced = 0;
  for (const id of opts.doneIds) {
    const at = opts.doneAtMs?.get(id);
    const i = typeof at === "number" && inWindow(at, opts.startMs, opts.endMs) ? slotOf(at) : -1;
    if (i < 0) unplaced++;
    else counts[i]++;
  }
  return { counts, unplaced };
}

/** Feed every in-window delta's `metric` value and class to `take`; returns
 *  how many were EXCLUDED — outside the window, or refused by `take` (a
 *  truncated grid has no bucket for it). Counted, never silently dropped. */
function eachInWindow(
  deltas: readonly PerItemDeltaLike[],
  opts: Pick<PerItemOpts, "startMs" | "endMs" | "metric">,
  classOf: ClassOf,
  take: (d: PerItemDeltaLike, v: number, c: keyof ByClass) => boolean
): number {
  const metric = opts.metric ?? "total";
  let excluded = 0;
  for (const d of deltas) {
    if (!inWindow(d.tsMs, opts.startMs, opts.endMs) || !take(d, num(d[metric]), classOf(d))) excluded++;
  }
  return excluded;
}

// ── the totals ──────────────────────────────────────────────────────────────

/**
 * Tokens per completed item over one window, the role share of those tokens,
 * and — when a mark is given — the same figures on each side of it.
 *
 * `before.tokens + after.tokens === tokens` always (the partition is on the
 * delta's instant, and every in-window delta lands on exactly one side). With
 * `doneAtMs`, `before.items + after.items + undatedItems === items`, where an
 * instant outside the window counts as undated (`placeItems`).
 */
export function perCompletedItem(
  deltas: readonly PerItemDeltaLike[],
  attribution: PerItemAttributionLike,
  board: readonly PerItemBoardRowLike[],
  opts: PerItemOpts
): PerCompletedItem {
  const classOf = classifier(attribution, board, opts.doneIds);
  const mark = opts.markTsMs;
  const hasMark = typeof mark === "number" && Number.isFinite(mark);

  const all = new Acc();
  const before = new Acc();
  const after = new Acc();
  const excluded = eachInWindow(deltas, opts, classOf, (d, v, c) => {
    all.add(d, v, c);
    if (hasMark) (d.tsMs < mark ? before : after).add(d, v, c);
    return true;
  });

  const dated = opts.doneAtMs !== undefined;
  const placed = placeItems(opts, 2, (at) => (hasMark && at < mark ? 0 : 1));
  const out: PerCompletedItem = {
    ...all.side(opts.doneIds.size),
    items: opts.doneIds.size,
    excluded,
    dated,
    undatedItems: dated ? placed.unplaced : 0,
  };
  if (hasMark) {
    out.before = before.side(dated ? placed.counts[0] : null);
    out.after = after.side(dated ? placed.counts[1] : null);
  }
  return out;
}

// ── over time ───────────────────────────────────────────────────────────────

export interface PerItemBucket {
  startMs: number;
  /** Exclusive. */
  endMs: number;
  /** Items whose done instant falls in this bucket; `null` when undated. */
  items: number | null;
  tokens: number;
  perItem: number | null;
  n: number;
  /** Where this bucket's tokens sat — same split as the totals'. */
  byClass: ByClass;
}

export interface PerItemOverTime {
  /** True when the grid is calendar days (the default) — local midnights,
   *  so a DST day is 23 or 25 hours rather than drifting by one. */
  calendarDays: boolean;
  /** The fixed width, when one was asked for; `null` for calendar days. */
  bucketMs: number | null;
  buckets: PerItemBucket[];
  /** Sum over buckets — equals `perCompletedItem`'s `tokens` for the same
   *  window unless `truncated`. */
  tokens: number;
  /** Items placed in a bucket; `null` when undated. */
  items: number | null;
  /** Done ids not placed: no instant, or an instant outside the window. */
  unplacedItems: number;
  excluded: number;
  /** The grid hit `MAX_BUCKETS` and stops short of `endMs`. */
  truncated: boolean;
}

/** The same guard `bucketSeries` carries against a runaway grid. */
const MAX_BUCKETS = 100_000;

/** Next local midnight after `t`, by calendar arithmetic — never `+ 86400000`
 *  (a DST day is 23 or 25 hours; CLAUDE.md). */
function nextLocalMidnight(t: number): number {
  const d = new Date(t);
  d.setHours(0, 0, 0, 0);
  d.setDate(d.getDate() + 1);
  return d.getTime();
}

function localMidnight(t: number): number {
  const d = new Date(t);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

interface Grid {
  /** The fixed width in use; `null` for calendar days. */
  fixed: number | null;
  starts: number[];
  truncated: boolean;
  /** Exclusive end of bucket `i`. */
  endOf: (i: number) => number;
  /** Index of the bucket holding `t`, or -1. */
  indexOf: (t: number) => number;
}

/** The bucket grid over `[startMs, endMs]`: local midnights unless a finite
 *  `bucketMs` of at least 1 asks for a fixed width, which aligns to its
 *  multiples. A degenerate or inverted window yields NO buckets rather than a
 *  runaway loop — `bucketSeries`'s rule. */
function buildGrid(startMs: number, endMs: number, bucketMs: number | undefined): Grid {
  const fixed =
    typeof bucketMs === "number" && Number.isFinite(bucketMs) && bucketMs >= 1
      ? Math.floor(bucketMs)
      : null;
  const starts: number[] = [];
  let truncated = false;
  const next = (t: number): number => (fixed !== null ? t + fixed : nextLocalMidnight(t));
  if (Number.isFinite(startMs) && Number.isFinite(endMs) && startMs <= endMs) {
    let t = fixed !== null ? Math.floor(startMs / fixed) * fixed : localMidnight(startMs);
    while (t <= endMs) {
      if (starts.length >= MAX_BUCKETS) {
        truncated = true;
        break;
      }
      starts.push(t);
      t = next(t);
    }
  }
  const endOf = (i: number): number => (i + 1 < starts.length ? starts[i + 1] : next(starts[i]));
  const indexOf = (t: number): number => {
    // Binary search for the last start at or before `t`.
    let lo = 0;
    let hi = starts.length - 1;
    let found = -1;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (starts[mid] <= t) {
        found = mid;
        lo = mid + 1;
      } else hi = mid - 1;
    }
    return found >= 0 && t < endOf(found) ? found : -1;
  };
  return { fixed, starts, truncated, endOf, indexOf };
}

function emptyBuckets(grid: Grid): PerItemBucket[] {
  return grid.starts.map((s, i) => ({
    startMs: s,
    endMs: grid.endOf(i),
    items: null,
    tokens: 0,
    perItem: null,
    n: 0,
    byClass: { done: 0, orchestrator: 0, inFlight: 0, unattributed: 0 },
  }));
}

/**
 * The per-item figure as a trend: one bucket per calendar day by default (or
 * per `bucketMs`, aligned to multiples of it as `bucketSeries` aligns its
 * grid), each carrying the tokens spent in it, the items done in it, and their
 * ratio. Buckets are half-open; the window stays inclusive at both ends, so the
 * bucket containing `endMs` is always on the grid. Over the same window, the
 * buckets' tokens and `byClass` sum to `perCompletedItem`'s, and their items
 * to its `before.items + after.items`, with `unplacedItems === undatedItems`
 * (unless the grid is `truncated`) — the property slice E's plot and the
 * table beside it share.
 */
export function perCompletedItemOverTime(
  deltas: readonly PerItemDeltaLike[],
  attribution: PerItemAttributionLike,
  board: readonly PerItemBoardRowLike[],
  opts: Omit<PerItemOpts, "markTsMs"> & { bucketMs?: number }
): PerItemOverTime {
  const classOf = classifier(attribution, board, opts.doneIds);
  const grid = buildGrid(opts.startMs, opts.endMs, opts.bucketMs);
  const buckets = emptyBuckets(grid);
  let tokens = 0;
  const excluded = eachInWindow(deltas, opts, classOf, (d, v, c) => {
    const b = buckets[grid.indexOf(d.tsMs)];
    if (!b) return false;
    b.tokens += v;
    b.n++;
    b.byClass[c] += v;
    tokens += v;
    return true;
  });

  const dated = opts.doneAtMs !== undefined;
  const placed = placeItems(opts, buckets.length, grid.indexOf);
  buckets.forEach((b, i) => {
    b.items = dated ? placed.counts[i] : null;
    b.perItem = b.items !== null && b.items > 0 ? b.tokens / b.items : null;
  });

  return {
    calendarDays: grid.fixed === null,
    bucketMs: grid.fixed,
    buckets,
    tokens,
    items: dated ? placed.counts.reduce((a, c) => a + c, 0) : null,
    unplacedItems: placed.unplaced,
    excluded,
    truncated: grid.truncated,
  };
}
