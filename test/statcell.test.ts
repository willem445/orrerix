// The duplicated five-number cell (#3475 slice B) — `src/statcell.ts` against
// `src/tokenscorecard.ts`'s own copy.
//
// The two are DUPLICATED on purpose (pure modules are import-free, TS5097), so
// what is pinned is that they cannot drift: both run over hand-known fixtures
// whose answers are written out here, and over a deterministic spread of
// generated samples where only their AGREEMENT is asserted. A change to either
// copy alone reddens the agreement loop; a change to both reddens the
// hand-known ones.

import test from "node:test";
import assert from "node:assert/strict";
import { statCell, MEDIAN_MIN_N } from "../src/statcell.ts";
import { statCell as scorecardStatCell, MEDIAN_MIN_N as SCORECARD_MIN_N } from "../src/tokenscorecard.ts";

const hand: { values: (number | null | undefined)[]; want: ReturnType<typeof statCell> }[] = [
  // Even length: median of the middle pair; Tukey hinges split evenly.
  {
    values: [10, 1, 3, 2],
    want: { n: 4, dropped: 0, median: 2.5, q1: 1.5, q3: 6.5, iqr: 5, min: 1, max: 10 },
  },
  // Odd length: the hinges EXCLUDE the middle element.
  {
    values: [1, 2, 3, 4, 100],
    want: { n: 5, dropped: 0, median: 3, q1: 1.5, q3: 52, iqr: 50.5, min: 1, max: 100 },
  },
  // Non-finite inputs dropped AND counted; what is left is below the floor.
  {
    values: [1, null, Number.NaN, undefined, 2, Number.POSITIVE_INFINITY],
    want: { n: 2, dropped: 4, median: null, q1: null, q3: null, iqr: null, min: null, max: null },
  },
  // Exactly at the floor: figures appear.
  {
    values: [3, 1, 2],
    want: { n: 3, dropped: 0, median: 2, q1: 1, q3: 3, iqr: 2, min: 1, max: 3 },
  },
];

test("statcell.ts gives the hand-known cell on every fixture", () => {
  for (const { values, want } of hand) assert.deepEqual(statCell(values), want, JSON.stringify(values));
});

test("tokenscorecard.ts' copy gives the SAME hand-known cells", () => {
  for (const { values, want } of hand) assert.deepEqual(scorecardStatCell(values), want, JSON.stringify(values));
});

test("the two copies agree on a deterministic spread of samples, sizes 0..40", () => {
  // A fixed LCG, so the spread is reproducible and needs no dependency.
  let seed = 3475;
  const next = () => (seed = (seed * 1103515245 + 12345) % 2147483648) / 2147483648;
  let compared = 0;
  for (let size = 0; size <= 40; size++) {
    for (let rep = 0; rep < 5; rep++) {
      const values = Array.from({ length: size }, () => {
        const r = next();
        if (r < 0.05) return null;
        return Math.round(next() * 10_000) / (r < 0.5 ? 1 : 7);
      });
      assert.deepEqual(statCell(values), scorecardStatCell(values), JSON.stringify(values));
      compared++;
    }
  }
  // Positive control: the loop really ran over the population it claims.
  assert.equal(compared, 41 * 5);
});

test("the two floors are one number", () => {
  assert.equal(MEDIAN_MIN_N, SCORECARD_MIN_N);
  assert.equal(MEDIAN_MIN_N, 3);
});
