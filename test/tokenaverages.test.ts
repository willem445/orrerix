// Averages over the token series (#3475 slice B) — `src/tokenaverages.ts`.
//
// What is pinned is every definition a reader could get WRONG from a table of
// averages: what one sample is (an interval's delta, or an item's total), what
// n counts (deltas, never rows), what the items denominator holds (feature bars
// with spend — never the orchestrator's group-wide spend, which keeps a row of
// its own), how the window and the mark partition the population, and that the
// time-bucketed form adds back up to the totals it was cut from.
//
// The discriminating fixture is the runaway pane: 100 / 100 / 10 000. Its mean
// (3 400) and median (100) disagree by a factor of 34, so an implementation
// that computed either one as the other cannot pass.

import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  averages,
  averagesOverTime,
  AVERAGE_MIN_N,
  DAY_BUCKET,
  ITEMS_ROW,
  UNATTRIBUTED,
  UNKNOWN_MODEL,
  type AverageDeltaLike,
  type AverageRow,
} from "../src/tokenaverages.ts";
import { statCell, MEDIAN_MIN_N } from "../src/statcell.ts";
import {
  attributeAgents,
  diffRows,
  ORCHESTRATOR,
  UNATTRIBUTED as CHARTS_UNATTRIBUTED,
  type AgentRowLike,
  type BoardRowLike,
  type SeriesSampleLike,
} from "../src/tokencharts.ts";

const MIN = 60_000;
const T0 = 1_780_000_000_000;
const noAttribution = { byAgent: new Map(), buckets: [] };

function delta(tsMs: number, agent: string, total: number, extra: Partial<AverageDeltaLike> = {}): AverageDeltaLike {
  return {
    tsMs,
    agent,
    block: "worker",
    model: "m-1",
    in: total,
    out: 0,
    cache_w: 0,
    cache_r: 0,
    total,
    cost_usd: total / 1000,
    ...extra,
  };
}

const row = (rows: readonly AverageRow[], key: string): AverageRow => {
  const r = rows.find((x) => x.key === key);
  assert.ok(r, `no row ${key} in ${rows.map((x) => x.key).join(", ")}`);
  return r;
};

const win = { startMs: T0, endMs: T0 + 60 * MIN, stat: statCell };

test("the runaway pane: its mean is dragged to 3 400, its median stays at 100", () => {
  // One pane, three intervals, one of them a runaway.
  const deltas = [delta(T0 + 5 * MIN, "w-1", 100), delta(T0 + 10 * MIN, "w-1", 100), delta(T0 + 15 * MIN, "w-1", 10_000)];
  const r = row(averages(deltas, noAttribution, { ...win, groupBy: "agent" }).rows, "w-1");
  assert.equal(r.n, 3);
  assert.equal(r.all.sum, 10_200);
  assert.equal(r.all.mean, 3_400);
  assert.equal(r.all.cell.median, 100);
  assert.notEqual(r.all.mean, r.all.cell.median, "the fixture no longer discriminates mean from median");
});

test("the same divergence per block: three panes of one block, one of them runaway", () => {
  const deltas = [delta(T0 + 5 * MIN, "w-1", 100), delta(T0 + 5 * MIN, "w-2", 100), delta(T0 + 5 * MIN, "w-3", 10_000)];
  const r = row(averages(deltas, noAttribution, { ...win, groupBy: "block" }).rows, "worker");
  assert.equal(r.n, 3);
  assert.equal(r.all.mean, 3_400);
  assert.equal(r.all.cell.median, 100);
});

test("n counts DELTAS, never rows: four samples of one key are three intervals; a baseline-only key is no sample", () => {
  const sample = (ts: number, key: string, total: number): SeriesSampleLike => ({
    kind: "sample",
    ts_ms: ts,
    key,
    agent: key,
    block: "worker",
    cli: "claude",
    role: "worker",
    in: total,
    out: 0,
    cache_w: 0,
    cache_r: 0,
    cost_usd: null,
    estimated: false,
    source: "claude",
    model: "m-1",
  });
  const rows = [
    sample(T0 + 0 * MIN, "w-1", 0),
    sample(T0 + 5 * MIN, "w-1", 100),
    sample(T0 + 10 * MIN, "w-1", 300),
    sample(T0 + 15 * MIN, "w-1", 600),
    sample(T0 + 5 * MIN, "w-2", 999), // baseline only — no delta
  ];
  const { deltas } = diffRows(rows);
  const out = averages(deltas, noAttribution, { ...win, groupBy: "agent" });
  assert.equal(row(out.rows, "w-1").n, 3, "four rows are three intervals");
  assert.equal(row(out.rows, "w-1").all.sum, 600);
  assert.equal(out.rows.some((r) => r.key === "w-2"), false, "a baseline row is not a sample");
});

test("a delta outside the window is excluded AND counted, never clamped onto an edge", () => {
  const deltas = [
    delta(T0 - 1, "w-1", 5_000), // one ms before the window
    delta(T0, "w-1", 100), // on the start edge — inside
    delta(T0 + 60 * MIN, "w-1", 100), // on the end edge — inside
    delta(T0 + 60 * MIN + 1, "w-1", 7_000), // one ms after
    delta(T0 + 30 * MIN, "w-1", 100),
  ];
  const out = averages(deltas, noAttribution, { ...win, groupBy: "agent" });
  assert.equal(out.outside, 2);
  assert.equal(out.inWindow, 3);
  const r = row(out.rows, "w-1");
  assert.equal(r.n, 3);
  assert.equal(r.all.sum, 300, "an excluded delta's spend leaked into the row");
});

test("a NaN window admits nothing rather than everything", () => {
  const out = averages([delta(T0, "w-1", 100)], noAttribution, { ...win, startMs: Number.NaN, groupBy: "agent" });
  assert.equal(out.inWindow, 0);
  assert.equal(out.outside, 1);
});

// ── per item ────────────────────────────────────────────────────────────────

const agents: AgentRowLike[] = [
  { id: "orch", block: "orchestrator", cli: "claude", role: "orchestrator", session: null, task: "" },
  { id: "w-1", block: "worker", cli: "claude", role: "worker", session: null, task: "" },
  { id: "w-2", block: "worker", cli: "claude", role: "worker", session: null, task: "" },
  { id: "w-3", block: "worker", cli: "claude", role: "worker", session: null, task: "" },
  { id: "w-9", block: "worker", cli: "claude", role: "worker", session: null, task: "" }, // on no row
];
const board: BoardRowLike[] = [
  { id: "t-1", title: "Feature one", kind: "feature", assignee: "w-1" },
  { id: "t-2", title: "Feature two", kind: "feature", assignee: "w-2" },
  { id: "t-3", title: "Feature three", kind: "feature", assignee: "w-3" },
];
const attribution = attributeAgents(agents, board);

const itemDeltas = [
  delta(T0 + 5 * MIN, "orch", 50_000), // group-wide spend — not an item
  delta(T0 + 5 * MIN, "w-1", 100),
  delta(T0 + 35 * MIN, "w-1", 200), // item one: 300 total, across the mark
  delta(T0 + 10 * MIN, "w-2", 900), // item two: 900, before the mark only
  delta(T0 + 40 * MIN, "w-9", 4_000), // unattributed — not an item
  delta(T0 + 45 * MIN, "ghost", 11), // an agent the roster never named
  // w-3 spends nothing in the window: t-3 is not a sample of this window.
];

test("per item: one sample per feature bar with spend; the orchestrator and (unattributed) stay out of the denominator", () => {
  const out = averages(itemDeltas, attribution, { ...win, groupBy: "item" });
  const items = row(out.rows, ITEMS_ROW);
  assert.equal(items.n, 2, "items = the two feature bars that spent in the window");
  assert.equal(items.all.sum, 1_200, "the items sum must hold item spend only");
  // Below AVERAGE_MIN_N the mean is refused; the sum still speaks.
  assert.equal(items.all.mean, null);

  // Present as rows of their own, never divided across the items.
  assert.equal(row(out.rows, ORCHESTRATOR).all.sum, 50_000);
  const un = row(out.rows, UNATTRIBUTED);
  assert.equal(un.all.sum, 4_011, "an unrostered agent lands on (unattributed), as featureBars files it");
  assert.equal(un.n, 2);
  assert.equal(row(out.rows, "t-1").all.sum, 300);
  assert.equal(row(out.rows, "t-1").label, "Feature one");
  assert.equal(row(out.rows, "t-3").n, 0, "a feature bar with no spend keeps its row at n 0");

  // Nothing lost: every in-window delta is on exactly one bucket row.
  const bucketRows = out.rows.filter((r) => r.key !== ITEMS_ROW);
  assert.equal(bucketRows.reduce((a, r) => a + r.n, 0), out.inWindow);
});

test("per item: the mean is tokens / items once there are enough items", () => {
  const more = [...itemDeltas, delta(T0 + 20 * MIN, "w-3", 3_000)];
  const items = row(averages(more, attribution, { ...win, groupBy: "item" }).rows, ITEMS_ROW);
  assert.equal(items.n, 3);
  assert.equal(items.all.sum, 4_200);
  assert.equal(items.all.mean, 1_400, "mean = 4 200 tokens / 3 items");
  assert.equal(items.all.cell.median, 900);
});

// ── before / after ──────────────────────────────────────────────────────────

test("before + after partitions all: n on every interval row, sum on the items row", () => {
  const mark = T0 + 30 * MIN;
  const out = averages(itemDeltas, attribution, { ...win, groupBy: "item", markTsMs: mark });
  for (const r of out.rows) {
    assert.ok(r.before && r.after, `${r.key} lost its halves`);
    if (r.key === ITEMS_ROW) continue;
    assert.equal(r.before.n + r.after.n, r.all.n, `${r.key}: n does not partition`);
  }
  const items = row(out.rows, ITEMS_ROW);
  assert.equal((items.before?.sum ?? 0) + (items.after?.sum ?? 0), items.all.sum);
  // An item spending on both sides is one item in `all` and one on each side:
  // the items row's n does NOT partition, by definition.
  assert.equal(items.before?.n, 2);
  assert.equal(items.after?.n, 1);
  assert.equal(items.all.n, 2);

  // And per agent, with a delta ON the mark (it belongs to `after`).
  const byAgent = averages(
    [delta(mark - 1, "w-1", 1), delta(mark, "w-1", 2), delta(mark + 1, "w-1", 4)],
    noAttribution,
    { ...win, groupBy: "agent", markTsMs: mark }
  );
  const w1 = row(byAgent.rows, "w-1");
  assert.equal(w1.before?.n, 1);
  assert.equal(w1.after?.n, 2);
  assert.equal(w1.before?.sum, 1);
  assert.equal(w1.after?.sum, 6);
});

test("no mark, no halves", () => {
  const r = row(averages(itemDeltas, attribution, { ...win, groupBy: "agent" }).rows, "w-1");
  assert.equal(r.before, null);
  assert.equal(r.after, null);
});

// ── model, cost, floors ─────────────────────────────────────────────────────

test("a null or blank model groups as `unknown model`, never merged into a real one", () => {
  const deltas = [
    delta(T0 + 1, "w-1", 10, { model: null }),
    delta(T0 + 2, "w-1", 20, { model: "" }),
    delta(T0 + 3, "w-1", 40, { model: "m-1" }),
  ];
  const out = averages(deltas, noAttribution, { ...win, groupBy: "model" });
  assert.deepEqual(
    out.rows.map((r) => [r.key, r.all.sum]),
    [
      ["m-1", 40],
      [UNKNOWN_MODEL, 30],
    ]
  );
});

test("cost is null-poisoned: one unknown interval makes the sum, mean and median unknown — tokens are unaffected", () => {
  const deltas = [
    delta(T0 + 1, "w-1", 100),
    delta(T0 + 2, "w-1", 100, { cost_usd: null }),
    delta(T0 + 3, "w-1", 100),
    delta(T0 + 4, "w-1", 100),
  ];
  const cost = row(averages(deltas, noAttribution, { ...win, groupBy: "agent", metric: "cost_usd" }).rows, "w-1");
  assert.equal(cost.n, 4);
  assert.equal(cost.all.unknown, 1);
  assert.equal(cost.all.sum, null);
  assert.equal(cost.all.mean, null);
  assert.equal(cost.all.cell.median, null);
  assert.equal(cost.all.cell.dropped, 1, "the injected cell still counts what it dropped");

  const tokens = row(averages(deltas, noAttribution, { ...win, groupBy: "agent" }).rows, "w-1");
  assert.equal(tokens.all.mean, 100);
  assert.equal(tokens.all.unknown, 0);

  // Control: with every cost known the same row has a figure.
  const known = deltas.map((d) => ({ ...d, cost_usd: 0.1 }));
  const ok = row(averages(known, noAttribution, { ...win, groupBy: "agent", metric: "cost_usd" }).rows, "w-1");
  assert.equal(ok.all.mean, 0.1);
});

test("the mean is refused below the same floor as the median", () => {
  assert.equal(AVERAGE_MIN_N, MEDIAN_MIN_N);
  const two = row(
    averages([delta(T0 + 1, "w-1", 1), delta(T0 + 2, "w-1", 3)], noAttribution, { ...win, groupBy: "agent" }).rows,
    "w-1"
  );
  assert.equal(two.n, 2);
  assert.equal(two.all.sum, 4);
  assert.equal(two.all.mean, null);
  assert.equal(two.all.cell.median, null);
});

test("the re-declared (unattributed) spelling is tokencharts.ts' own", () => {
  assert.equal(UNATTRIBUTED, CHARTS_UNATTRIBUTED);
});

// ── over time ───────────────────────────────────────────────────────────────

test("over time: every key's bucket sums add back up to its totals row, in every grouping", () => {
  const deltas = [...itemDeltas, delta(T0 + 20 * MIN, "w-3", 3_000, { model: null })];
  for (const groupBy of ["agent", "block", "model", "item"] as const) {
    const opts = { ...win, groupBy, bucketMs: 10 * MIN };
    const totals = averages(deltas, attribution, opts);
    const series = averagesOverTime(deltas, attribution, opts);
    assert.deepEqual(
      series.keys.map((k) => k.key),
      totals.rows.map((r) => r.key),
      `${groupBy}: the series carries a different key list from the totals`
    );
    let checked = 0;
    for (const s of series.keys) {
      assert.equal(s.points.length, series.buckets.length, `${groupBy}/${s.key}: the grid is not dense`);
      const sum = s.points.reduce((a, p) => a + (p.sum ?? 0), 0);
      assert.equal(sum, row(totals.rows, s.key).all.sum, `${groupBy}/${s.key}`);
      if (s.key !== ITEMS_ROW) assert.equal(s.points.reduce((a, p) => a + p.n, 0), row(totals.rows, s.key).n);
      checked++;
    }
    assert.ok(checked > 0, `${groupBy}: no keys compared`);
    assert.equal(series.inWindow, totals.inWindow);
    assert.equal(series.outside, totals.outside);
  }
});

test("over time: a bucket is its own population — mean and median per bucket, n 0 where nothing spent", () => {
  const A = Math.floor(T0 / (10 * MIN)) * (10 * MIN);
  const deltas = [
    delta(A + 1 * MIN, "w-1", 100),
    delta(A + 2 * MIN, "w-1", 100),
    delta(A + 3 * MIN, "w-1", 10_000),
    delta(A + 25 * MIN, "w-1", 7),
  ];
  // A start mid-bucket still aligns the grid to multiples of bucketMs.
  const s = averagesOverTime(deltas, noAttribution, {
    ...win,
    startMs: A + 30_000,
    endMs: A + 29 * MIN,
    groupBy: "agent",
    bucketMs: 10 * MIN,
  });
  assert.equal(s.bucket, 10 * MIN);
  assert.equal(s.buckets[0], A);
  assert.equal(s.buckets.length, 3);
  const [p0, p1, p2] = s.keys[0].points;
  assert.equal(p0.mean, 3_400);
  assert.equal(p0.cell.median, 100);
  assert.equal(p1.n, 0);
  assert.equal(p1.sum, 0);
  assert.equal(p2.n, 1);
});

test("over time: the default bucket is a local calendar day", () => {
  const s = averagesOverTime([delta(T0 + 1, "w-1", 5)], noAttribution, { ...win, groupBy: "agent" });
  assert.equal(s.bucket, DAY_BUCKET);
  const midnight = new Date(T0);
  midnight.setHours(0, 0, 0, 0);
  assert.equal(s.buckets[0], midnight.getTime());
});

test("over time: day buckets follow the calendar across DST, in a zone that HAS it (not the host's)", () => {
  // CI runs UTC, where every day is 24 h and `n * 86_400_000` would pass. So
  // the zone is forced in a child `node` (TZ is read at process start):
  // America/Chicago, where 8 March 2026 is 23 hours long.
  const script = [
    "const { averagesOverTime } = await import(process.argv[1]);",
    "const stat = (v) => ({ n: v.length, dropped: 0, median: null, q1: null, q3: null, iqr: null, min: null, max: null });",
    "const start = new Date(2026, 2, 7, 12, 0, 0, 0).getTime();",
    "const end = new Date(2026, 2, 10, 12, 0, 0, 0).getTime();",
    // 00:30 on 9 March, local: 23.5 h after the 8th's midnight. A 24-hour grid
    // anchored on the 7th's midnight files it under the 8th.
    "const at = new Date(2026, 2, 9, 0, 30, 0, 0).getTime();",
    "const d = { tsMs: at, agent: 'w-1', block: 'worker', model: 'm', in: 1, out: 0, cache_w: 0, cache_r: 0, total: 1, cost_usd: null };",
    "const s = averagesOverTime([d], { byAgent: new Map(), buckets: [] }, { startMs: start, endMs: end, groupBy: 'agent', stat });",
    "const ninth = new Date(2026, 2, 9, 0, 0, 0, 0).getTime();",
    "const eighth = new Date(2026, 2, 8, 0, 0, 0, 0).getTime();",
    "process.stdout.write(JSON.stringify({",
    "  len: ninth - eighth,",
    "  starts: s.buckets, ends: s.ends,",
    "  landed: s.buckets[s.keys[0].points.findIndex((p) => p.n === 1)],",
    "  ninth,",
    "}));",
  ].join("\n");
  const modulePath = new URL("../src/tokenaverages.ts", import.meta.url).href;
  const res = spawnSync(
    process.execPath,
    ["--experimental-strip-types", "--no-warnings", "--input-type=module", "-e", script, modulePath],
    { env: { ...process.env, TZ: "America/Chicago" }, encoding: "utf8" }
  );
  assert.equal(res.status, 0, `child failed: ${res.stderr}`);
  const out = JSON.parse(res.stdout) as { len: number; starts: number[]; ends: number[]; landed: number; ninth: number };
  // Positive control: the forced zone took.
  assert.equal(out.len, 23 * 3_600_000, "TZ=America/Chicago did not take in the child");
  assert.equal(out.starts.length, 4, "7, 8, 9 and 10 March");
  assert.equal(out.ends[1] - out.starts[1], 23 * 3_600_000, "the 8th's bucket is not 23 hours long");
  for (let i = 1; i < out.starts.length; i++) assert.equal(out.starts[i], out.ends[i - 1], "the day grid has a gap");
  assert.equal(out.landed, out.ninth, "00:30 on the 9th was filed under another day");
});

test("over time: day buckets stay on local midnight where DST starts AT midnight (00:00 does not exist)", () => {
  // The Chicago test above moves the clock at 02:00, so midnight always
  // exists there and a grid that set local midnight ONCE, before its loop,
  // still passes. America/Santiago springs forward at 00:00 (7 Sep 2025):
  // that day starts at 01:00, and a grid that only calls setDate keeps the
  // 01:00 start for every later day, which files each 00:00-01:00 delta
  // under the day before. Every bucket after the transition must start at a
  // real local midnight again.
  const script = [
    "const { averagesOverTime } = await import(process.argv[1]);",
    "const stat = (v) => ({ n: v.length, dropped: 0, median: null, q1: null, q3: null, iqr: null, min: null, max: null });",
    "const start = new Date(2025, 8, 5, 12, 0, 0, 0).getTime();",
    "const end = new Date(2025, 8, 10, 12, 0, 0, 0).getTime();",
    "const at = new Date(2025, 8, 9, 0, 30, 0, 0).getTime();",
    "const d = { tsMs: at, agent: 'w-1', block: 'worker', model: 'm', in: 1, out: 0, cache_w: 0, cache_r: 0, total: 1, cost_usd: null };",
    "const s = averagesOverTime([d], { byAgent: new Map(), buckets: [] }, { startMs: start, endMs: end, groupBy: 'agent', stat });",
    "const hm = (t) => { const x = new Date(t); return [x.getDate(), x.getHours(), x.getMinutes()]; };",
    "process.stdout.write(JSON.stringify({",
    "  seventhOffsetChange: new Date(2025, 8, 6, 12).getTimezoneOffset() - new Date(2025, 8, 8, 12).getTimezoneOffset(),",
    "  starts: s.buckets.map(hm),",
    "  landed: hm(s.buckets[s.keys[0].points.findIndex((p) => p.n === 1)]),",
    "}));",
  ].join("\n");
  const modulePath = new URL("../src/tokenaverages.ts", import.meta.url).href;
  const res = spawnSync(
    process.execPath,
    ["--experimental-strip-types", "--no-warnings", "--input-type=module", "-e", script, modulePath],
    { env: { ...process.env, TZ: "America/Santiago" }, encoding: "utf8" }
  );
  assert.equal(res.status, 0, `child failed: ${res.stderr}`);
  const out = JSON.parse(res.stdout) as { seventhOffsetChange: number; starts: number[][]; landed: number[] };
  // Positive control: the zone took, and it does shift an hour over the 7th.
  assert.equal(out.seventhOffsetChange, 60, "TZ=America/Santiago did not take in the child");
  assert.deepEqual(
    out.starts,
    [
      [5, 0, 0],
      [6, 0, 0],
      [7, 1, 0], // the transition day's first real instant
      [8, 0, 0],
      [9, 0, 0],
      [10, 0, 0],
    ],
    "a day bucket after the midnight transition does not start at local midnight"
  );
  assert.deepEqual(out.landed, [9, 0, 0], "00:30 on the 9th was filed under another day");
});

test("over time: a degenerate window is an empty grid, and every delta is counted outside", () => {
  const s = averagesOverTime([delta(T0, "w-1", 1)], noAttribution, {
    ...win,
    startMs: T0 + 1,
    endMs: T0,
    groupBy: "agent",
  });
  assert.equal(s.buckets.length, 0);
  assert.equal(s.outside, 1);
});
