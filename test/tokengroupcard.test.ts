import test from "node:test";
import assert from "node:assert/strict";
import { groupCard, type CardInput, type CardRow } from "../src/tokengroupcard.ts";
import { statCell } from "../src/statcell.ts";

const H = 3_600_000;
const median = (xs: readonly number[]) => statCell(xs).median;

function base(over: Partial<CardInput> = {}): CardInput {
  return {
    startMs: 0,
    endMs: 10 * H,
    perItem: {
      perItem: 500, items: 4,
      byRole: [
        { role: "worker", share: 0.6, n: 30 },
        { role: "orchestrator", share: 0.3, n: 20 },
        { role: "reviewer", share: 0.1, n: 5 },
      ],
    },
    deltas: [
      { tsMs: 1 * H, cost_usd: 2 },
      { tsMs: 2 * H, cost_usd: 6 },
      { tsMs: 20 * H, cost_usd: 1000 }, // outside the window
    ],
    life: {
      doneIds: { size: 4 },
      donePerDay: { windowDays: 0.5, rate: 8 },
      ttc: [{ ms: 1 * H }, { ms: 2 * H }, { ms: 3 * H }, { ms: 10 * H }],
      reviewRoundsPerPr: [{ rounds: 1 }, { rounds: 1 }, { rounds: 4 }],
      ciAttemptsPerPr: [{ attempts: { green: 1, red: 2 } }, { attempts: null }, { attempts: { green: 1, red: 0 } }],
      partition: null,
    },
    median,
    ...over,
  };
}
const row = (rows: CardRow[], id: string) => {
  const r = rows.find((x) => x.id === id);
  assert.ok(r, `row ${id} present`);
  return r!;
};

test("one group-wide row per aggregate, tokens per completed item first", () => {
  const rows = groupCard(base());
  assert.deepEqual(rows.slice(0, 7).map((r) => r.id), ["tokensPerItem", "costPerItem", "donePerDay", "ttcMedian", "ttcMean", "roundsPerPr", "ciPerPr"]);
  assert.deepEqual(rows.slice(7).map((r) => r.id), ["share:worker", "share:orchestrator", "share:reviewer"]);
  for (const r of rows) assert.equal(typeof r.n, "number", `${r.id} carries an n`);
});

test("the figures are the aggregates, each with its own n", () => {
  const rows = groupCard(base());
  assert.equal(row(rows, "tokensPerItem").value, 500);
  // Cost: only the two in-window intervals, over four items.
  assert.equal(row(rows, "costPerItem").value, 2);
  assert.equal(row(rows, "donePerDay").value, 8);
  assert.equal(row(rows, "ttcMedian").value, 2.5);
  assert.equal(row(rows, "ttcMean").value, 4);
  assert.equal(row(rows, "roundsPerPr").value, 2);
  assert.equal(row(rows, "roundsPerPr").n, 3);
  // CI: the PR with no CI rows is never-driven, not zero attempts.
  assert.equal(row(rows, "ciPerPr").value, 2);
  assert.equal(row(rows, "ciPerPr").n, 2);
  assert.equal(row(rows, "share:orchestrator").value, 0.3);
});

test("nothing completed reads n/a with a reason — never a zero", () => {
  const rows = groupCard(base({
    perItem: { perItem: null, items: 0, byRole: [] },
    life: { ...base().life, doneIds: { size: 0 }, donePerDay: { windowDays: 0.5, rate: 0 }, ttc: [], reviewRoundsPerPr: [], ciAttemptsPerPr: [] },
  }));
  assert.equal(row(rows, "tokensPerItem").value, null);
  assert.equal(row(rows, "tokensPerItem").note, "no items completed in this window");
  assert.equal(row(rows, "costPerItem").value, null);
  assert.equal(row(rows, "ttcMedian").value, null);
  assert.equal(row(rows, "ttcMean").value, null);
  assert.equal(row(rows, "roundsPerPr").value, null);
  assert.equal(row(rows, "ciPerPr").value, null);
});

test("cost with no figure at all is n/a, and a partial one says it is a lower bound", () => {
  const none = groupCard(base({ deltas: [{ tsMs: H, cost_usd: null }] }));
  assert.equal(row(none, "costPerItem").value, null);
  assert.match(row(none, "costPerItem").note ?? "", /no interval/);
  const partial = groupCard(base({ deltas: [{ tsMs: H, cost_usd: 8 }, { tsMs: 2 * H, cost_usd: null }] }));
  assert.equal(row(partial, "costPerItem").value, 2);
  assert.match(row(partial, "costPerItem").note ?? "", /lower bound/);
});

test("before/after appear only with a mark, each side with its own n", () => {
  for (const r of groupCard(base())) {
    assert.equal(r.before, undefined, r.id);
    assert.equal(r.after, undefined, r.id);
  }
  const rows = groupCard(base({
    markTsMs: 1.5 * H,
    perItem: {
      ...base().perItem,
      before: { perItem: 800, items: 1, byRole: [{ role: "worker", share: 0.9, n: 3 }] },
      after: { perItem: 400, items: 3, byRole: [{ role: "worker", share: 0.5, n: 27 }] },
    },
    life: { ...base().life, partition: { before: { done: 1, ttcMs: [H], rounds: [1] }, after: { done: 3, ttcMs: [2 * H, 3 * H, 10 * H], rounds: [1, 4] } } },
    daysBefore: 0.25, daysAfter: 0.25,
    ciBefore: [{ attempts: { green: 1, red: 0 } }],
    ciAfter: [{ attempts: { green: 1, red: 2 } }, { attempts: null }],
  }));
  assert.deepEqual(row(rows, "tokensPerItem").before, { value: 800, n: 1 });
  assert.deepEqual(row(rows, "tokensPerItem").after, { value: 400, n: 3 });
  // Cost split on the interval's instant: $2 before (1 item), $6 after (3 items).
  assert.deepEqual(row(rows, "costPerItem").before, { value: 2, n: 1 });
  assert.deepEqual(row(rows, "costPerItem").after, { value: 2, n: 3 });
  assert.deepEqual(row(rows, "donePerDay").before, { value: 4, n: 1 });
  assert.deepEqual(row(rows, "donePerDay").after, { value: 12, n: 3 });
  assert.deepEqual(row(rows, "ttcMedian").after, { value: 3, n: 3 });
  assert.deepEqual(row(rows, "ttcMedian").before, { value: null, n: 1 }, "below the median's n floor");
  assert.deepEqual(row(rows, "roundsPerPr").after, { value: 2.5, n: 2 });
  assert.deepEqual(row(rows, "ciPerPr").before, { value: 1, n: 1 });
  assert.deepEqual(row(rows, "ciPerPr").after, { value: 3, n: 1 });
  assert.deepEqual(row(rows, "share:worker").before, { value: 0.9, n: 3 });
  assert.deepEqual(row(rows, "share:orchestrator").before, { value: null, n: 0 });
});

test("a cost interval AT the mark instant lands AFTER it — the same split perCompletedItem makes on tokens", () => {
  const rows = groupCard(base({
    markTsMs: 2 * H,
    perItem: { ...base().perItem, before: { perItem: 1, items: 1, byRole: [] }, after: { perItem: 1, items: 1, byRole: [] } },
    deltas: [{ tsMs: 1 * H, cost_usd: 2 }, { tsMs: 2 * H, cost_usd: 6 }],
  }));
  assert.deepEqual(row(rows, "costPerItem").before, { value: 2, n: 1 });
  assert.deepEqual(row(rows, "costPerItem").after, { value: 6, n: 1 });
});

test("an unfamiliar role keeps its own share row", () => {
  const rows = groupCard(base({ perItem: { ...base().perItem, byRole: [{ role: "auditor", share: 1, n: 2 }] } }));
  assert.equal(row(rows, "share:auditor").value, 1);
});
