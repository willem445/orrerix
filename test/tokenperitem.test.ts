// Intent tests for tokens per completed item and the role share (#3475 slice C).
//
// What can be WRONG on the chart this feeds: a ratio divided by zero drawn as a
// plausible 0 or an `Infinity`; a role the build never heard of folded into a
// known one; the orchestrator counted as an item, or dropped from the cost; a
// before/after pair or a per-day series that does not add back up to the
// figure beside it; and a "per day" bucket that is 24 hours on a 23-hour day.
// Every expected number below is computed by hand in a comment beside it.

import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";

import {
  ORCHESTRATOR_ROLE_NOTE,
  perCompletedItem,
  perCompletedItemOverTime,
  type PerItemDeltaLike,
} from "../src/tokenperitem.ts";
import { attributeAgents, diffRows, type AgentRowLike, type BoardRowLike, type SeriesRowLike } from "../src/tokencharts.ts";

/** A delta with distinct per-counter values, so a test can tell `total` from
 *  any one counter. `total` = in + out + cache_w + cache_r, as `diffRows` sums. */
const delta = (
  tsMs: number,
  agent: string,
  role: string,
  c: { in?: number; out?: number; cache_w?: number; cache_r?: number }
): PerItemDeltaLike => {
  const v = { in: c.in ?? 0, out: c.out ?? 0, cache_w: c.cache_w ?? 0, cache_r: c.cache_r ?? 0 };
  return { tsMs, agent, role, ...v, total: v.in + v.out + v.cache_w + v.cache_r };
};

const agent = (id: string, role: string): AgentRowLike => ({
  id,
  block: role,
  cli: "claude",
  role,
  session: null,
  task: "",
});

// ── the shared fixture: two done items, three agents of two roles ───────────
//
//   t-1 done         assignee w-a (worker)    spend 100 + 200 = 300
//   t-2 done         assignee w-b (worker)    spend 300
//   t-3 in-progress  assignee r-1 (reviewer)  spend 400
//                                             whole = 1000
const board: BoardRowLike[] = [
  { id: "t-1", title: "one", status: "done", assignee: "w-a" },
  { id: "t-2", title: "two", status: "done", assignee: "w-b" },
  { id: "t-3", title: "three", status: "in-progress", assignee: "r-1" },
];
const agents = [agent("w-a", "worker"), agent("w-b", "worker"), agent("r-1", "reviewer")];
const attribution = attributeAgents(agents, board);
const deltas: PerItemDeltaLike[] = [
  delta(1_000, "w-a", "worker", { in: 60, out: 40 }), // 100
  delta(2_000, "w-a", "worker", { in: 50, cache_r: 150 }), // 200
  delta(3_000, "w-b", "worker", { out: 100, cache_w: 200 }), // 300
  delta(4_000, "r-1", "reviewer", { in: 100, cache_r: 300 }), // 400
];
const doneIds = new Set(["t-1", "t-2"]);

test("two done items, three agents of two roles: per-item and role shares, by hand", () => {
  const r = perCompletedItem(deltas, attribution, board, { startMs: 0, endMs: 10_000, doneIds });
  assert.equal(r.items, 2);
  assert.equal(r.tokens, 1000); // 300 + 300 + 400
  assert.equal(r.perItem, 500); // 1000 / 2
  assert.equal(r.n, 4);
  assert.equal(r.excluded, 0);
  // worker 600 / 1000, reviewer 400 / 1000; largest first.
  assert.deepEqual(
    r.byRole.map((x) => [x.role, x.tokens, x.share, x.n]),
    [
      ["worker", 600, 0.6, 3],
      ["reviewer", 400, 0.4, 1],
    ]
  );
  // The reviewer's spend is on t-3, which is NOT done: in the numerator
  // (throughput ratio), and the breakdown says it is not on the items.
  assert.deepEqual(r.byClass, { done: 600, orchestrator: 0, inFlight: 400, unattributed: 0 });
  assert.equal(r.dated, false);
  assert.equal(r.before, undefined, "no mark, no halves");
});

test("the metric option sums one counter, not the total", () => {
  // in: 60 + 50 + 0 + 100 = 210 over 2 items = 105.
  const r = perCompletedItem(deltas, attribution, board, {
    startMs: 0,
    endMs: 10_000,
    doneIds,
    metric: "in",
  });
  assert.equal(r.tokens, 210);
  assert.equal(r.perItem, 105);
});

test("zero completed items: perItem is null, never 0 or Infinity, and the spend still counts", () => {
  const r = perCompletedItem(deltas, attribution, board, {
    startMs: 0,
    endMs: 10_000,
    doneIds: new Set(),
    doneAtMs: new Map(),
    markTsMs: 2_500,
  });
  assert.equal(r.items, 0);
  assert.equal(r.tokens, 1000);
  assert.equal(r.perItem, null);
  assert.equal(r.before?.perItem, null);
  assert.equal(r.after?.perItem, null);
  // Nothing is done, so nothing is on a done item.
  assert.equal(r.byClass.done, 0);
});

test("a role this build has never heard of keeps its own row; a blank one reads unknown, apart", () => {
  const r = perCompletedItem(
    [...deltas, delta(5_000, "x-1", "sorcerer", { out: 250 }), delta(6_000, "x-2", "", { in: 50 })],
    attribution,
    board,
    { startMs: 0, endMs: 10_000, doneIds }
  );
  // whole = 1000 + 250 + 50 = 1300
  const row = (role: string) => r.byRole.find((x) => x.role === role);
  assert.equal(row("sorcerer")?.tokens, 250);
  assert.equal(row("sorcerer")?.share, 250 / 1300);
  assert.equal(row("sorcerer")?.orchestrator, false);
  assert.equal(row("unknown")?.tokens, 50);
  assert.equal(r.byRole.length, 4, "worker, reviewer, sorcerer, unknown — none folded");
  // Agents the roster never listed are unattributed, and counted as such.
  assert.equal(r.byClass.unattributed, 300);
  // Shares of the whole sum to one.
  assert.equal(
    r.byRole.reduce((a, x) => a + (x.share ?? 0), 0),
    1
  );
});

test("the orchestrator is in the numerator, is never an item, and its row says so", () => {
  const withOrch = attributeAgents([...agents, agent("o-1", "orchestrator")], board);
  const r = perCompletedItem(
    [...deltas, delta(4_500, "o-1", "orchestrator", { cache_r: 1000 })],
    withOrch,
    board,
    { startMs: 0, endMs: 10_000, doneIds }
  );
  assert.equal(r.items, 2, "the orchestrator's bar did not become an item");
  assert.equal(r.tokens, 2000); // 1000 + 1000
  assert.equal(r.perItem, 1000); // 2000 / 2
  assert.equal(r.byClass.orchestrator, 1000);
  const o = r.byRole.find((x) => x.role === "orchestrator")!;
  assert.equal(o.orchestrator, true);
  assert.equal(o.share, 0.5);
  assert.equal(o.note, ORCHESTRATOR_ROLE_NOTE);
  assert.equal(r.byRole.find((x) => x.role === "worker")!.note, null);

  // The class split reads the DELTA's role, so an orchestrator whose agent has
  // left the roster (no attribution at all) is still the orchestrator's spend,
  // and the class and role splits agree on it — not "unattributed".
  const gone = perCompletedItem(
    [delta(4_500, "o-gone", "orchestrator", { out: 50 })],
    attribution,
    board,
    { startMs: 0, endMs: 10_000, doneIds }
  );
  assert.deepEqual(gone.byClass, { done: 0, orchestrator: 50, inFlight: 0, unattributed: 0 });
});

test("a done container finishes its tasks: spend on a child row of a done feature is on a done item", () => {
  const b: BoardRowLike[] = [
    { id: "f-1", title: "feature", kind: "feature", status: "done" },
    { id: "t-9", title: "task", parent: "f-1", status: "in-progress", assignee: "w-a" },
    // A cycle must terminate, not hang.
    { id: "c-1", title: "c1", parent: "c-2", assignee: "w-b" },
    { id: "c-2", title: "c2", parent: "c-1" },
  ];
  const at = attributeAgents(agents, b);
  const r = perCompletedItem(deltas, at, b, { startMs: 0, endMs: 10_000, doneIds: new Set(["f-1"]) });
  assert.equal(r.byClass.done, 300, "w-a's spend on t-9 under done f-1");
  assert.equal(r.byClass.inFlight, 300, "w-b on the cycle, not done");
  assert.equal(r.byClass.unattributed, 400, "r-1 has no row on this board");

  // The done row is in the MIDDLE of the chain — neither the matched row nor
  // the bar it rolls up to — so only the container walk can find it.
  const mid: BoardRowLike[] = [
    { id: "f-2", title: "feature", kind: "feature", status: "in-progress" },
    { id: "s-2", title: "story", parent: "f-2", status: "done" },
    { id: "t-8", title: "task", parent: "s-2", status: "in-progress", assignee: "w-a" },
  ];
  const at2 = attributeAgents(agents, mid);
  assert.equal(at2.byAgent.get("w-a")?.bucket, "f-2", "fixture: the bar is the feature");
  assert.equal(at2.byAgent.get("w-a")?.matched, "t-8", "fixture: the match is the task");
  const m = perCompletedItem(deltas, at2, mid, { startMs: 0, endMs: 10_000, doneIds: new Set(["s-2"]) });
  assert.equal(m.byClass.done, 300, "w-a's task sits under the done story s-2");
});

test("the window is inclusive at both ends, and a delta outside it is counted, not dropped silently", () => {
  const r = perCompletedItem(deltas, attribution, board, { startMs: 2_000, endMs: 3_000, doneIds });
  assert.equal(r.tokens, 500); // 200 at 2000 + 300 at 3000
  assert.equal(r.n, 2);
  assert.equal(r.excluded, 2);
});

test("before + after partition the whole on the DELTA's instant; items split by their done instant", () => {
  const doneAtMs = new Map([
    ["t-1", 1_500],
    ["t-2", 3_500],
  ]);
  const r = perCompletedItem(deltas, attribution, board, {
    startMs: 0,
    endMs: 10_000,
    doneIds,
    doneAtMs,
    // A delta ON the mark is after it (`tsMs < mark` is before).
    markTsMs: 3_000,
  });
  const b = r.before!;
  const a = r.after!;
  // before: 100 + 200 = 300; after: 300 + 400 = 700.
  assert.equal(b.tokens, 300);
  assert.equal(a.tokens, 700);
  assert.equal(b.tokens + a.tokens, r.tokens, "the halves are a partition of the whole");
  assert.equal(b.n + a.n, r.n);
  for (const k of ["done", "orchestrator", "inFlight", "unattributed"] as const)
    assert.equal(b.byClass[k] + a.byClass[k], r.byClass[k], `byClass.${k} partitions`);
  assert.equal(b.items, 1);
  assert.equal(a.items, 1);
  assert.equal(b.perItem, 300);
  assert.equal(a.perItem, 700);
  assert.equal(r.undatedItems, 0);
});

test("undated items cannot be placed on a side of the mark: half item counts are null, tokens still split", () => {
  const r = perCompletedItem(deltas, attribution, board, {
    startMs: 0,
    endMs: 10_000,
    doneIds,
    markTsMs: 3_000,
  });
  assert.equal(r.dated, false);
  assert.equal(r.perItem, 500, "the whole-window figure needs no dates");
  assert.equal(r.before!.items, null);
  assert.equal(r.before!.perItem, null);
  assert.equal(r.after!.items, null);
  assert.equal(r.before!.tokens + r.after!.tokens, 1000);
  // Partly dated: the undated id is counted, and not placed on either side.
  const p = perCompletedItem(deltas, attribution, board, {
    startMs: 0,
    endMs: 10_000,
    doneIds,
    doneAtMs: new Map([["t-1", 1_500]]),
    markTsMs: 3_000,
  });
  assert.equal(p.undatedItems, 1);
  assert.equal(p.before!.items, 1);
  assert.equal(p.after!.items, 0);
  assert.equal(p.after!.perItem, null);
});

test("the over-time series sums back to the totals, bucket by bucket", () => {
  const doneAtMs = new Map([
    ["t-1", 1_500],
    ["t-2", 3_500],
  ]);
  const opts = { startMs: 0, endMs: 9_999, doneIds, doneAtMs };
  const totals = perCompletedItem(deltas, attribution, board, opts);
  const s = perCompletedItemOverTime(deltas, attribution, board, { ...opts, bucketMs: 2_000 });
  assert.equal(s.calendarDays, false);
  assert.deepEqual(
    s.buckets.map((b) => [b.startMs, b.endMs, b.tokens, b.items, b.perItem]),
    [
      [0, 2_000, 100, 1, 100], // delta at 1000; t-1 done at 1500
      [2_000, 4_000, 500, 1, 500], // 200 at 2000 + 300 at 3000; t-2 done at 3500
      // Half-open: the reviewer's delta at 4000 is here, not in the bucket above.
      [4_000, 6_000, 400, 0, null],
      [6_000, 8_000, 0, 0, null],
      [8_000, 10_000, 0, 0, null],
    ]
  );
  assert.equal(
    s.buckets.reduce((a, b) => a + b.tokens, 0),
    totals.tokens
  );
  assert.equal(s.tokens, totals.tokens);
  assert.equal(
    s.buckets.reduce((a, b) => a + (b.items ?? 0), 0),
    totals.items
  );
  assert.equal(s.items, totals.items);
  for (const k of ["done", "orchestrator", "inFlight", "unattributed"] as const)
    assert.equal(
      s.buckets.reduce((a, b) => a + b.byClass[k], 0),
      totals.byClass[k],
      `byClass.${k} sums`
    );
  assert.equal(s.unplacedItems, 0);
  assert.equal(s.excluded, 0);
});

test("over time, undated items are null per bucket and counted unplaced; a degenerate window is empty", () => {
  const s = perCompletedItemOverTime(deltas, attribution, board, {
    startMs: 0,
    endMs: 9_999,
    doneIds,
    bucketMs: 5_000,
  });
  assert.equal(s.items, null);
  assert.equal(s.unplacedItems, 2);
  assert.ok(s.buckets.every((b) => b.items === null && b.perItem === null));
  assert.equal(s.tokens, 1000, "tokens still bucket without dates");

  const inverted = perCompletedItemOverTime(deltas, attribution, board, {
    startMs: 10_000,
    endMs: 0,
    doneIds,
  });
  assert.equal(inverted.buckets.length, 0);
  assert.equal(inverted.excluded, 4);
});

test("the default bucket is a CALENDAR day: a 23-hour day ends at local midnight, not 24 h later", () => {
  // Forced zone in a child `node` (TZ is read at process start) — CI runs UTC,
  // where every day is 24 hours and this fixture would be blind.
  // America/Chicago: 8 March 2026 is 23 hours long.
  const script = [
    "const { perCompletedItemOverTime } = await import(process.argv[1]);",
    "const d0 = new Date(2026, 2, 8, 0, 0, 0, 0).getTime();",
    "const d1 = new Date(2026, 2, 9, 0, 0, 0, 0).getTime();",
    "const d2 = new Date(2026, 2, 10, 0, 0, 0, 0).getTime();",
    // 00:30 on 9 March: in day 2 by the calendar, in day 1 by d0 + 24h.
    "const t = new Date(2026, 2, 9, 0, 30, 0, 0).getTime();",
    "const del = { tsMs: t, agent: 'a', role: 'worker', in: 7, out: 0, cache_w: 0, cache_r: 0, total: 7 };",
    "const s = perCompletedItemOverTime([del], { byAgent: new Map() }, [],",
    "  { startMs: d0 + 3600000, endMs: d2 - 1, doneIds: new Set(['x']), doneAtMs: new Map([['x', t]]) });",
    "process.stdout.write(JSON.stringify({ len: d1 - d0, d0, d1, d2, t,",
    "  buckets: s.buckets.map((b) => [b.startMs, b.endMs, b.tokens, b.items]) }));",
  ].join("\n");
  const modulePath = new URL("../src/tokenperitem.ts", import.meta.url).href;
  const res = spawnSync(
    process.execPath,
    ["--experimental-strip-types", "--no-warnings", "--input-type=module", "-e", script, modulePath],
    { env: { ...process.env, TZ: "America/Chicago" }, encoding: "utf8" }
  );
  assert.equal(res.status, 0, `child failed: ${res.stderr}`);
  const o = JSON.parse(res.stdout) as {
    len: number;
    d0: number;
    d1: number;
    d2: number;
    t: number;
    buckets: [number, number, number, number][];
  };
  // Positive control: the zone took, so the two readings really diverge here.
  assert.equal(o.len, 23 * 3600_000, "TZ=America/Chicago did not take in the child");
  assert.ok(o.t >= o.d0 + 23 * 3600_000 && o.t < o.d0 + 24 * 3600_000, "the fixture sits in the gap");
  // A window starting mid-day still aligns its first bucket to local midnight.
  assert.deepEqual(o.buckets, [
    [o.d0, o.d1, 0, 0],
    [o.d1, o.d2, 7, 1],
  ]);
});

test("the real pipeline's values pass straight in: diffRows deltas and attributeAgents", () => {
  const sample = (ts_ms: number, key: string, agentId: string, role: string, inTok: number): SeriesRowLike => ({
    kind: "sample",
    ts_ms,
    key,
    agent: agentId,
    block: role,
    cli: "claude",
    role,
    in: inTok,
    out: 0,
    cache_w: 0,
    cache_r: 0,
    cost_usd: null,
    estimated: false,
    source: "claude",
    model: null,
  });
  const { deltas: real } = diffRows([
    sample(0, "k1", "w-a", "worker", 10),
    sample(1_000, "k1", "w-a", "worker", 110), // +100
    sample(0, "k2", "r-1", "reviewer", 0),
    sample(2_000, "k2", "r-1", "reviewer", 300), // +300
  ]);
  const r = perCompletedItem(real, attribution, board, { startMs: 0, endMs: 10_000, doneIds });
  assert.equal(r.tokens, 400);
  assert.equal(r.perItem, 200);
  assert.deepEqual(r.byClass, { done: 100, orchestrator: 0, inFlight: 300, unattributed: 0 });
});
