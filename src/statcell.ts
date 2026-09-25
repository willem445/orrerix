// The five-number cell (#3475 slice B): `statCell`, `StatCell` and
// `MEDIAN_MIN_N`, lifted out of `tokenscorecard.ts` so the token-charts
// metric modules (`tokenaverages.ts`, and slices C/D after it) share ONE
// definition of "median, quartiles, n" with the scorecard table.
//
// **Duplicated, not moved — and the duplication is pinned.** Every pure module
// under `src/` is import-free (TS5097, see `embedsplit.ts`): `tsc` wants the
// extension-less specifier and `node --test` the `.ts` one, so no pure module
// can import another. `tokenscorecard.ts` therefore keeps its own copy, and
// `test/statcell.test.ts` runs both over the same fixtures and asserts they
// agree — so the two cannot drift apart without a red. The metric modules do
// not import this file either: the VIEW imports it and injects `statCell` as
// a callback (`tokenaverages.ts`' `stat` option), which is the one place a
// real import is allowed to happen.
//
// The convention is `scripts/orch-scorecard.cjs`': median with EXCLUSIVE-
// median (Tukey hinge) quartiles, over finite numbers only, non-numeric inputs
// DROPPED and counted, and every figure `null` below `MEDIAN_MIN_N` — n is
// reported at every size so a null reads as "not enough data" rather than as
// a missing measurement.

/** `orch-scorecard.cjs`'s `MEDIAN_MIN_N`: three points is already a thin
 *  claim; two is an average of a pair, and one is an anecdote wearing a
 *  statistic's clothes. */
export const MEDIAN_MIN_N = 3;

/** `statCell`'s five-number cell. Non-finite inputs are DROPPED and counted;
 *  below `MEDIAN_MIN_N` every figure is `null` and only `n`/`dropped` speak. */
export interface StatCell {
  n: number;
  dropped: number;
  median: number | null;
  q1: number | null;
  q3: number | null;
  iqr: number | null;
  min: number | null;
  max: number | null;
}

function round2(n: number): number {
  return Math.round(n * 100) / 100;
}

/** `medianOf` — input already sorted; `null` for an empty sample. */
function medianOf(sorted: readonly number[]): number | null {
  if (sorted.length === 0) return null;
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
}

/** `statCell` — the five-number cell every comparison column is made of.
 *  Quartiles use the exclusive-median (Tukey hinge) convention: the halves
 *  exclude the middle element on an odd-length sample. */
export function statCell(values: readonly (number | null | undefined)[]): StatCell {
  const xs = values
    .filter((v): v is number => typeof v === "number" && Number.isFinite(v))
    .sort((a, b) => a - b);
  const dropped = values.length - xs.length;
  const cell: StatCell = { n: xs.length, dropped, median: null, q1: null, q3: null, iqr: null, min: null, max: null };
  if (xs.length < MEDIAN_MIN_N) return cell;
  const mid = Math.floor(xs.length / 2);
  const lower = xs.slice(0, mid);
  const upper = xs.length % 2 ? xs.slice(mid + 1) : xs.slice(mid);
  cell.median = round2(medianOf(xs) as number);
  cell.q1 = round2(medianOf(lower) as number);
  cell.q3 = round2(medianOf(upper) as number);
  cell.iqr = round2((cell.q3 as number) - (cell.q1 as number));
  cell.min = xs[0];
  cell.max = xs[xs.length - 1];
  return cell;
}
