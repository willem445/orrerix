// The token chart's group scorecard (#3505): ONE short table of group-wide
// aggregates for the selected window, replacing the per-pane / per-block /
// per-model / per-item / per-PR lists #3475 rendered inline (those now sit
// behind a closed "breakdown" disclosure).
//
// Every figure here is a fold over results the other pure modules already
// computed (`tokenperitem.ts`, `tokenlifecycle.ts`) plus the cost sum, which
// no module computed per item yet. DOM-free and import-free (the TS5097 rule
// `chartwindow.ts` states); the one statistic it needs is INJECTED, the way
// `tokenaverages.ts` takes `stat`, so there is one median in the codebase.
//
// The rules every row keeps:
//  - a figure that could not be measured is `null` — rendered "n/a" — never
//    `0`, which would say the fleet did something it did not;
//  - every row carries its `n`, so "3.2 rounds" over 2 PRs reads differently
//    from the same figure over 40;
//  - `before`/`after` exist exactly when a mark is selected, and each side
//    carries its own `n`.

export interface CardSide {
  value: number | null;
  n: number;
}

export interface CardRow {
  id: string;
  label: string;
  /** How the view formats `value`. */
  unit: "tokens" | "usd" | "perDay" | "hours" | "rounds" | "attempts" | "share";
  value: number | null;
  n: number;
  /** What `n` counts, in words ("completed items", "PRs"). */
  nLabel: string;
  before?: CardSide;
  after?: CardSide;
  /** A caveat the row cannot be read without, or `null`. */
  note: string | null;
}

// ── structural inputs ───────────────────────────────────────────────────────

export interface CardRoleLike { role: string; share: number | null; n: number }
export interface CardPerItemSideLike { perItem: number | null; items: number | null; byRole: readonly CardRoleLike[] }
export interface CardPerItemLike extends CardPerItemSideLike {
  items: number;
  before?: CardPerItemSideLike;
  after?: CardPerItemSideLike;
}
export interface CardDeltaLike { tsMs: number; cost_usd: number | null }
export interface CardPrCiLike { attempts: { green: number; red: number } | null }
export interface CardLifecycleLike {
  doneIds: { readonly size: number };
  donePerDay: { windowDays: number; rate: number | null };
  ttc: readonly { ms: number }[];
  reviewRoundsPerPr: readonly { rounds: number }[];
  ciAttemptsPerPr: readonly CardPrCiLike[];
  partition: { before: { done: number; ttcMs: readonly number[]; rounds: readonly number[] }; after: { done: number; ttcMs: readonly number[]; rounds: readonly number[] } } | null;
}

export interface CardInput {
  startMs: number;
  endMs: number;
  markTsMs?: number;
  perItem: CardPerItemLike;
  deltas: readonly CardDeltaLike[];
  life: CardLifecycleLike;
  /** Calendar days on each side of the mark (`windowCalendarDays`), for the
   *  per-side done rate. Required for a before/after rate; absent → n/a. */
  daysBefore?: number;
  daysAfter?: number;
  /** CI attempts per PR computed over each side's own window — the lifecycle
   *  partition carries no CI split. */
  ciBefore?: readonly CardPrCiLike[];
  ciAfter?: readonly CardPrCiLike[];
  /** The injected median — `statCell(xs).median` — with its own n floor. */
  median: (xs: readonly number[]) => number | null;
}

// ── helpers ─────────────────────────────────────────────────────────────────

function mean(xs: readonly number[]): number | null {
  let sum = 0;
  let n = 0;
  for (const x of xs) if (Number.isFinite(x)) { sum += x; n++; }
  return n === 0 ? null : sum / n;
}

const HOUR_MS = 3_600_000;
const hours = (ms: number | null): number | null => (ms === null ? null : ms / HOUR_MS);

function ciPerPr(rows: readonly CardPrCiLike[]): CardSide {
  const xs: number[] = [];
  for (const r of rows) if (r.attempts !== null) xs.push(r.attempts.green + r.attempts.red);
  return { value: mean(xs), n: xs.length };
}

function rate(done: number, days: number | undefined): number | null {
  return typeof days === "number" && Number.isFinite(days) && days > 0 ? done / days : null;
}

interface CostSide { usd: number; known: number; missing: number }
function costSide(): CostSide { return { usd: 0, known: 0, missing: 0 }; }
function addCost(side: CostSide, c: number | null): void {
  if (c === null || !Number.isFinite(c)) side.missing++;
  else { side.usd += c; side.known++; }
}
/** Cost per item: `null` when there are no items, or when NO interval carried
 *  a figure (a sum over nothing is not $0). */
function perItemCost(side: CostSide, items: number | null): number | null {
  if (items === null || items <= 0 || side.known === 0) return null;
  return side.usd / items;
}

// ── the card ────────────────────────────────────────────────────────────────

/** The group scorecard's rows, in display order: tokens per completed item
 *  first (the figure #3505 names first), then cost, throughput, latency, the
 *  review loop, and the role split. */
export function groupCard(input: CardInput): CardRow[] {
  const mark = input.markTsMs;
  const hasMark = typeof mark === "number" && Number.isFinite(mark);
  const { perItem, life } = input;
  const rows: CardRow[] = [];
  const sides = <T>(before: T, after: T): { before?: T; after?: T } => (hasMark ? { before, after } : {});

  // Tokens per completed item.
  rows.push({
    id: "tokensPerItem", label: "tokens per completed item", unit: "tokens",
    value: perItem.perItem, n: perItem.items, nLabel: "completed items",
    ...sides(
      { value: perItem.before?.perItem ?? null, n: perItem.before?.items ?? 0 },
      { value: perItem.after?.perItem ?? null, n: perItem.after?.items ?? 0 },
    ),
    note: perItem.items === 0 ? "no items completed in this window" : null,
  });

  // Cost per completed item — the window is INCLUSIVE at both ends, the rule
  // `perCompletedItem` applies to the same deltas, so the two rows share one
  // numerator population.
  const all = costSide(), before = costSide(), after = costSide();
  for (const d of input.deltas) {
    if (!(d.tsMs >= input.startMs && d.tsMs <= input.endMs)) continue;
    addCost(all, d.cost_usd);
    if (hasMark) addCost(d.tsMs < (mark as number) ? before : after, d.cost_usd);
  }
  rows.push({
    id: "costPerItem", label: "cost per completed item", unit: "usd",
    value: perItemCost(all, perItem.items), n: perItem.items, nLabel: "completed items",
    ...sides(
      { value: perItemCost(before, perItem.before?.items ?? null), n: perItem.before?.items ?? 0 },
      { value: perItemCost(after, perItem.after?.items ?? null), n: perItem.after?.items ?? 0 },
    ),
    note: all.missing > 0 && all.known > 0
      ? `${all.missing} interval(s) carried no cost figure, so this is a lower bound`
      : all.known === 0 && all.missing > 0 ? "no interval in this window carried a cost figure" : null,
  });

  // Items done per day.
  const part = life.partition;
  rows.push({
    id: "donePerDay", label: "items done per day", unit: "perDay",
    value: life.donePerDay.rate, n: life.doneIds.size, nLabel: "completed items",
    ...sides(
      { value: part ? rate(part.before.done, input.daysBefore) : null, n: part?.before.done ?? 0 },
      { value: part ? rate(part.after.done, input.daysAfter) : null, n: part?.after.done ?? 0 },
    ),
    note: null,
  });

  // Time to completion, median and mean.
  const ttc = life.ttc.map((t) => t.ms);
  rows.push({
    id: "ttcMedian", label: "median time-to-completion", unit: "hours",
    value: hours(input.median(ttc)), n: ttc.length, nLabel: "completed items",
    ...sides(
      { value: part ? hours(input.median(part.before.ttcMs)) : null, n: part?.before.ttcMs.length ?? 0 },
      { value: part ? hours(input.median(part.after.ttcMs)) : null, n: part?.after.ttcMs.length ?? 0 },
    ),
    note: null,
  });
  rows.push({
    id: "ttcMean", label: "mean time-to-completion", unit: "hours",
    value: hours(mean(ttc)), n: ttc.length, nLabel: "completed items",
    ...sides(
      { value: part ? hours(mean(part.before.ttcMs)) : null, n: part?.before.ttcMs.length ?? 0 },
      { value: part ? hours(mean(part.after.ttcMs)) : null, n: part?.after.ttcMs.length ?? 0 },
    ),
    note: null,
  });

  // Review rounds per PR (the mean — a median of small integers mostly reads
  // "1" and hides the tail the figure exists to show).
  const rounds = life.reviewRoundsPerPr.map((r) => r.rounds);
  rows.push({
    id: "roundsPerPr", label: "review rounds per PR", unit: "rounds",
    value: mean(rounds), n: rounds.length, nLabel: "PRs",
    ...sides(
      { value: part ? mean(part.before.rounds) : null, n: part?.before.rounds.length ?? 0 },
      { value: part ? mean(part.after.rounds) : null, n: part?.after.rounds.length ?? 0 },
    ),
    note: null,
  });

  // CI attempts per PR — only PRs the driver ran CI for; a PR with no CI row
  // is "never driven", not "zero attempts".
  const ci = ciPerPr(life.ciAttemptsPerPr);
  rows.push({
    id: "ciPerPr", label: "CI attempts per PR", unit: "attempts",
    value: ci.value, n: ci.n, nLabel: "PRs with CI",
    ...sides(
      input.ciBefore ? ciPerPr(input.ciBefore) : { value: null, n: 0 },
      input.ciAfter ? ciPerPr(input.ciAfter) : { value: null, n: 0 },
    ),
    note: null,
  });

  // Role share of the window's tokens — one row per role the series carried,
  // never a fixed list (a role this build has not heard of keeps its row).
  const roles = [...perItem.byRole].sort((a, b) => (b.share ?? -1) - (a.share ?? -1) || a.role.localeCompare(b.role));
  for (const r of roles) {
    const pick = (side: CardPerItemSideLike | undefined): CardSide => {
      const hit = side?.byRole.find((x) => x.role === r.role);
      return { value: hit?.share ?? null, n: hit?.n ?? 0 };
    };
    rows.push({
      id: `share:${r.role}`, label: `${r.role || "unknown"} share of tokens`, unit: "share",
      value: r.share, n: r.n, nLabel: "intervals",
      ...sides(pick(perItem.before), pick(perItem.after)),
      note: null,
    });
  }
  return rows;
}
