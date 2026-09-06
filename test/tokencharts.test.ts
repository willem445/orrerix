// Intent tests for the token-charts projection (#2011 slice C).
//
// What is being pinned is the set of answers that can be WRONG on a chart the
// human will use to judge a workflow change: which feature an agent's spend
// lands on, whether a quiet hour draws as a floor or a slope, whether two CLIs
// of one block are two lines, and whether a readout says "n/a" rather than a
// plausible zero. The SVG that draws them is hand-validated (repo convention);
// every number it draws comes from here.

import test from "node:test";
import assert from "node:assert/strict";

import {
  DEFAULT_BEFORE_AFTER_K,
  HUE_ORDER,
  HUE_SLOTS,
  ORCHESTRATOR,
  TOTAL_ROW,
  UNATTRIBUTED,
  UNKNOWN,
  attributeAgents,
  beforeAfter,
  bucketSeries,
  diffRows,
  featureBars,
  marks,
  scorecardColumns,
  seriesKeyOf,
  seriesKeys,
  type AgentRowLike,
  type AuditRowLike,
  type BoardRowLike,
  type SeriesRowLike,
  type SeriesSampleLike,
} from "../src/tokencharts.ts";

// ── fixtures ────────────────────────────────────────────────────────────────

const BUCKET = 5 * 60_000;
/** An arbitrary but FIXED instant — nothing here reads a clock — snapped ONTO
 *  the bucket grid. The grid is aligned to the epoch (see the test below), so
 *  an unaligned `T0` would put every fixture's first sample mid-bucket and the
 *  hand-computed means below would be about a window shifted by the remainder. */
const T0 = Math.floor(1_700_000_000_000 / BUCKET) * BUCKET;

/** A sample row with everything defaulted, so each test states only the fields
 *  it is actually about. */
function sample(over: Partial<SeriesSampleLike> & Pick<SeriesSampleLike, "ts_ms" | "key">): SeriesSampleLike {
  return {
    kind: "sample",
    agent: "w-1",
    block: "worker-std",
    cli: "pi",
    role: "worker",
    in: 0,
    out: 0,
    cache_w: 0,
    cache_r: 0,
    cost_usd: null,
    estimated: false,
    source: "pi-transcript",
    model: "glm",
    ...over,
  };
}

function agent(over: Partial<AgentRowLike> & Pick<AgentRowLike, "id">): AgentRowLike {
  return { block: "worker-std", cli: "pi", role: "worker", session: null, task: "", ...over };
}

function row(over: Partial<BoardRowLike> & Pick<BoardRowLike, "id">): BoardRowLike {
  return { title: over.id, ...over };
}

// ── the ladder: one test per rung ───────────────────────────────────────────

test("rung 1 — a board row whose assignee IS the agent decides, and reports via:assignee", () => {
  const board = [
    row({ id: "t-1", title: "the feature", kind: "feature" }),
    row({ id: "t-2", title: "a task", kind: "task", parent: "t-1", assignee: "w-9" }),
  ];
  const at = attributeAgents([agent({ id: "w-9" })], board);
  const a = at.byAgent.get("w-9")!;
  assert.equal(a.via, "assignee");
  assert.equal(a.matched, "t-2", "the ladder matched the TASK");
  assert.equal(a.bucket, "t-1", "…and the container walk put the bar on its feature");
  assert.equal(a.level, "feature");
});

test("rung 2 — with no assignee anywhere, the agent's SESSION decides", () => {
  const board = [
    row({ id: "t-1", title: "the feature", kind: "feature" }),
    row({ id: "t-2", kind: "task", parent: "t-1", session: "sess-abc" }),
  ];
  const at = attributeAgents([agent({ id: "w-9", session: "sess-abc" })], board);
  const a = at.byAgent.get("w-9")!;
  assert.equal(a.via, "session");
  assert.equal(a.bucket, "t-1");

  // The rung is REACHED only because rung 1 did not decide: put the assignee
  // back on a DIFFERENT feature and rung 1 wins, which is what makes this a
  // ladder rather than two independent lookups.
  const withAssignee = attributeAgents(
    [agent({ id: "w-9", session: "sess-abc" })],
    [row({ id: "t-0", title: "other feature", kind: "feature", assignee: "w-9" }), ...board]
  );
  assert.equal(withAssignee.byAgent.get("w-9")!.via, "assignee");
  assert.equal(withAssignee.byAgent.get("w-9")!.bucket, "t-0");
});

test("rung 3 — the brief's #N reaches a board row's issue, and skips refs no row carries", () => {
  const board = [
    row({ id: "t-1", title: "charts", kind: "feature" }),
    row({ id: "t-2", kind: "task", parent: "t-1", issue: "#2011" }),
  ];
  // The brief cites #9999 FIRST (a reference), then the issue it is working.
  // The first ref a board row actually carries is the one that decides.
  const at = attributeAgents(
    [agent({ id: "w-9", task: "per #9999's plan, do slice C of #2011" })],
    board
  );
  const a = at.byAgent.get("w-9")!;
  assert.equal(a.via, "brief");
  assert.equal(a.matched, "t-2");
  assert.equal(a.bucket, "t-1");
});

test("rung 4 — an agent no rung places lands on (unattributed), which is present even when empty", () => {
  const board = [row({ id: "t-1", title: "charts", kind: "feature" })];
  const at = attributeAgents([agent({ id: "w-9", task: "no refs here" })], board);
  assert.equal(at.byAgent.get("w-9")!.via, "none");
  assert.equal(at.byAgent.get("w-9")!.bucket, UNATTRIBUTED);

  // …and the bar exists with NO agents on it too. A bar that says "this chart
  // is not the whole story" must not be able to vanish by being empty.
  const empty = attributeAgents([], board);
  assert.equal(empty.buckets[0].id, UNATTRIBUTED, "unattributed renders FIRST");
  assert.deepEqual(empty.buckets[0].agents, []);
});

test("an orchestrator is never attributed to a feature, even when the board assigns it one", () => {
  // The board row names the orchestrator as its assignee — rung 1 would fire
  // for any other role. The orchestrator rung sits ABOVE the whole ladder
  // precisely so a long-lived session's lifetime spend is never charged to
  // whichever row it happened to be holding.
  const board = [
    row({ id: "t-1", title: "charts", kind: "feature" }),
    row({ id: "t-2", kind: "task", parent: "t-1", assignee: "orch-1" }),
  ];
  const at = attributeAgents([agent({ id: "orch-1", role: "orchestrator", block: "orchestrator" })], board);
  const a = at.byAgent.get("orch-1")!;
  assert.equal(a.via, "orchestrator");
  assert.equal(a.bucket, ORCHESTRATOR);
  assert.equal(a.matched, null, "no board row is recorded — there is no honest one");
  assert.ok(
    at.buckets.every((b) => b.kind !== "feature" || !b.agents.includes("orch-1")),
    "the orchestrator appears on no feature bar"
  );
});

test("the legend's three numbers are exactly features + orchestrator + unattributed", () => {
  const board = [
    row({ id: "t-1", title: "charts", kind: "feature" }),
    row({ id: "t-2", kind: "task", parent: "t-1", assignee: "w-1" }),
  ];
  const agents = [
    agent({ id: "w-1" }),
    agent({ id: "orch-1", role: "orchestrator", block: "orchestrator" }),
    agent({ id: "w-lost", task: "nothing the board knows" }),
  ];
  const rows: SeriesRowLike[] = [
    sample({ ts_ms: T0, key: "k1", agent: "w-1", in: 100 }),
    sample({ ts_ms: T0 + BUCKET, key: "k1", agent: "w-1", in: 400 }), // +300
    sample({ ts_ms: T0, key: "k2", agent: "orch-1", block: "orchestrator", role: "orchestrator", out: 10 }),
    sample({ ts_ms: T0 + BUCKET, key: "k2", agent: "orch-1", block: "orchestrator", role: "orchestrator", out: 60 }), // +50
    sample({ ts_ms: T0, key: "k3", agent: "w-lost", cache_r: 5 }),
    sample({ ts_ms: T0 + BUCKET, key: "k3", agent: "w-lost", cache_r: 12 }), // +7
  ];

  const fb = featureBars(rows, agents, board);
  assert.equal(fb.totals.features, 300);
  assert.equal(fb.totals.orchestrator, 50);
  assert.equal(fb.totals.unattributed, 7);
  assert.equal(fb.totals.total, 357);
  assert.equal(
    fb.totals.features + fb.totals.orchestrator + fb.totals.unattributed,
    fb.totals.total,
    "the three legend numbers ARE the total — the sum identity the chart is checkable by"
  );
  // …and the identity is measured against the deltas themselves, not against
  // the same three numbers re-added. A bar the loop failed to reach would sum
  // to a smaller total that still satisfied the line above.
  const everyDelta = diffRows(rows).deltas.reduce((a, d) => a + d.total, 0);
  assert.equal(fb.totals.total, everyDelta);
});

test("featureBars totals are SCOPED to the caller's window — the field is not a lifetime", () => {
  // The counterfactual behind the rename (review B1/B2 round 1). The field
  // was called `lifetime`, and the docs told the reader that a mismatch
  // against the group panel's lifetime figure meant the chart was wrong — on
  // any group older than the default 24h window that declared a CORRECT chart
  // broken. The identity holds at every scope; the SCOPE was the false claim,
  // so this pins that windowing really does change the number.
  const board = [
    row({ id: "t-1", title: "charts", kind: "feature" }),
    row({ id: "t-2", kind: "task", parent: "t-1", assignee: "w-1" }),
  ];
  const agents = [agent({ id: "w-1" })];
  const rows: SeriesRowLike[] = [
    sample({ ts_ms: T0, key: "k", agent: "w-1", in: 0 }),
    sample({ ts_ms: T0 + BUCKET, key: "k", agent: "w-1", in: 100 }), // +100, early
    sample({ ts_ms: T0 + 10 * BUCKET, key: "k", agent: "w-1", in: 400 }), // +300, late
  ];

  const unscoped = featureBars(rows, agents, board);
  assert.equal(unscoped.totals.total, 400, "no window: every delta counts");

  // A window covering only the LATE delta must report only it. The two
  // figures must DIVERGE, or this fixture would hold under an implementation
  // that ignored the window entirely — which is the bug being pinned.
  const windowed = featureBars(rows, agents, board, {
    startMs: T0 + 5 * BUCKET,
    endMs: T0 + 20 * BUCKET,
  });
  assert.equal(windowed.totals.total, 300);
  assert.notEqual(
    windowed.totals.total,
    unscoped.totals.total,
    "the two scopes must differ, or this test cannot fail"
  );
  // …and the identity still holds at the narrower scope.
  assert.equal(
    windowed.totals.features + windowed.totals.orchestrator + windowed.totals.unattributed,
    windowed.totals.total
  );
});

test("the container walk lands on an EPIC when no feature is in the chain", () => {
  // Review N1: the epic arm had no witness. `level` is documented "so a
  // reader never has to guess", which is only true if each arm is pinned.
  const board = [
    row({ id: "e-1", title: "the epic", kind: "epic" }),
    row({ id: "t-2", kind: "task", parent: "e-1", assignee: "w-9" }),
  ];
  const a = attributeAgents([agent({ id: "w-9" })], board).byAgent.get("w-9")!;
  assert.equal(a.bucket, "e-1");
  assert.equal(a.level, "epic");

  // The arm is REACHED only because no feature is in the chain: insert one
  // between and the feature wins, which is what makes this an ordered walk
  // rather than "whichever container is found first".
  const withFeature = attributeAgents(
    [agent({ id: "w-9" })],
    [
      row({ id: "e-1", title: "the epic", kind: "epic" }),
      row({ id: "f-1", title: "the feature", kind: "feature", parent: "e-1" }),
      row({ id: "t-2", kind: "task", parent: "f-1", assignee: "w-9" }),
    ]
  ).byAgent.get("w-9")!;
  assert.equal(withFeature.bucket, "f-1");
  assert.equal(withFeature.level, "feature");
});

test("a board with no agile levels at all gets a bar per top-level row, reported as root", () => {
  // The pre-#958 shape, and still legal. Sending every one of these agents to
  // (unattributed) would be FALSE — the ladder did find the row being worked.
  const board = [
    row({ id: "r-1", title: "a top-level row" }),
    row({ id: "t-2", parent: "r-1", assignee: "w-9" }),
  ];
  const a = attributeAgents([agent({ id: "w-9" })], board).byAgent.get("w-9")!;
  assert.equal(a.bucket, "r-1");
  assert.equal(a.level, "root");
  assert.equal(a.via, "assignee", "…and this is NOT the unattributed rung");
  assert.notEqual(a.bucket, UNATTRIBUTED);
});

test("a parent naming a row that is not on the board ends the chain instead of throwing", () => {
  // The dangling-parent case the design note says must not throw — and the
  // one N1 called out as unpinned. A cycle is pinned beside it: both are ways
  // the walk could fail to terminate.
  const dangling = [row({ id: "t-3", title: "orphan", parent: "nope", assignee: "w-9" })];
  const a = attributeAgents([agent({ id: "w-9" })], dangling).byAgent.get("w-9")!;
  assert.equal(a.bucket, "t-3", "the chain ends at the matched row itself");
  assert.equal(a.level, "root");

  // A cycle terminates too, rather than looping forever.
  const cyclic = [
    row({ id: "c-1", title: "one", parent: "c-2", assignee: "w-8" }),
    row({ id: "c-2", title: "two", parent: "c-1" }),
  ];
  const c = attributeAgents([agent({ id: "w-8" })], cyclic).byAgent.get("w-8")!;
  assert.equal(c.level, "root");
  assert.ok(c.bucket === "c-1" || c.bucket === "c-2", `unexpected bucket ${c.bucket}`);
});

// ── differencing and bucketing ──────────────────────────────────────────────

test("two occupants of ONE usage key each get the spend counted while THEY held it", () => {
  // The key is a CLI session; `usage.json` would keep only the last occupant.
  // The series keeps both, and the delta lands on the agent that wrote the
  // LATER row — the forward fix for last-occupant attribution.
  const rows: SeriesRowLike[] = [
    sample({ ts_ms: T0, key: "sess-1", agent: "w-a", in: 100 }),
    sample({ ts_ms: T0 + 60_000, key: "sess-1", agent: "w-a", in: 300 }), // +200 -> w-a
    sample({ ts_ms: T0 + 120_000, key: "sess-1", agent: "w-b", in: 900 }), // +600 -> w-b
  ];
  const { deltas } = diffRows(rows);
  assert.equal(deltas.length, 2);
  assert.deepEqual(
    deltas.map((d) => [d.agent, d.total]),
    [
      ["w-a", 200],
      ["w-b", 600],
    ]
  );
  assert.equal(deltas.every((d) => d.usageKey === "sess-1"), true);
});

test("a key with only ONE row yields no delta and is counted as a baseline", () => {
  const { deltas, baselineOnlyKeys } = diffRows([sample({ ts_ms: T0, key: "k", in: 5000 })]);
  assert.deepEqual(deltas, [], "a session's lifetime-to-here is not one bucket's spend");
  assert.equal(baselineOnlyKeys, 1);
});

test("a cumulative counter that goes DOWN clamps to zero and is labelled a reset", () => {
  const rows: SeriesRowLike[] = [
    sample({ ts_ms: T0, key: "k", in: 500, out: 200 }),
    sample({ ts_ms: T0 + 60_000, key: "k", in: 100, out: 260 }), // in shrank: cursor reset
  ];
  const { deltas, resets } = diffRows(rows);
  assert.equal(resets, 1);
  assert.equal(deltas[0].reset, true);
  assert.equal(deltas[0].in, 0, "never negative, never wrapped");
  assert.equal(deltas[0].out, 60, "the components that did NOT shrink are kept");
  assert.equal(deltas[0].total, 60);
});

test("a cost delta is null when EITHER endpoint carried no figure", () => {
  const known = diffRows([
    sample({ ts_ms: T0, key: "k", cost_usd: 1 }),
    sample({ ts_ms: T0 + 1, key: "k", cost_usd: 3 }),
  ]).deltas[0];
  assert.equal(known.cost_usd, 2);

  const half = diffRows([
    sample({ ts_ms: T0, key: "k", cost_usd: null }),
    sample({ ts_ms: T0 + 1, key: "k", cost_usd: 3 }),
  ]).deltas[0];
  assert.equal(half.cost_usd, null, "no figure and cost nothing are different answers");
});

test("a GAP renders as zero-valued buckets, not as a slope between two samples", () => {
  // One sample at T0, the next an hour later. The straight-line reading would
  // draw twelve buckets of steady spend that never happened.
  const rows: SeriesRowLike[] = [
    sample({ ts_ms: T0, key: "k", in: 0 }),
    sample({ ts_ms: T0 + 60 * 60_000, key: "k", in: 1200 }),
  ];
  const b = bucketSeries(rows, { startMs: T0, endMs: T0 + 60 * 60_000, bucketMs: BUCKET });
  assert.equal(b.buckets.length, 13, "12 five-minute steps, inclusive of both ends");
  const pts = b.keys[0].points;
  assert.equal(pts.length, b.buckets.length, "the grid is DENSE — one point per bucket");
  // Every bucket but the last is a real zero with no data behind it…
  assert.deepEqual(
    pts.slice(0, 12).map((p) => [p.total, p.n]),
    Array.from({ length: 12 }, () => [0, 0])
  );
  // …and the whole delta lands in the bucket the LATER row belongs to.
  assert.deepEqual([pts[12].total, pts[12].n], [1200, 1]);
});

test("opencode and pi rows of ONE block are two keys, and collapse to one under the block toggle", () => {
  const rows: SeriesRowLike[] = [
    sample({ ts_ms: T0, key: "oc", cli: "opencode", in: 0 }),
    sample({ ts_ms: T0 + 60_000, key: "oc", cli: "opencode", in: 100 }),
    sample({ ts_ms: T0, key: "pi", cli: "pi", in: 0 }),
    sample({ ts_ms: T0 + 60_000, key: "pi", cli: "pi", in: 40 }),
  ];
  const split = seriesKeys(rows);
  assert.deepEqual(
    split.map((k) => [k.key, k.total]),
    [
      ["worker-std/opencode", 100],
      ["worker-std/pi", 40],
    ]
  );
  // One block, so one hue — the CLI split is carried by line style, not colour.
  assert.equal(split[0].hueIndex, split[1].hueIndex);

  const collapsed = seriesKeys(rows, { collapseCli: true });
  assert.deepEqual(
    collapsed.map((k) => [k.key, k.cli, k.total]),
    [["worker-std", null, 140]]
  );
  // The toggle is a REGROUPING, never a filter: the same tokens are on screen.
  assert.equal(
    split.reduce((a, k) => a + k.total, 0),
    collapsed.reduce((a, k) => a + k.total, 0)
  );
});

test("the key axis is read off the rows — a block this build never heard of still gets a line", () => {
  const rows: SeriesRowLike[] = [
    sample({ ts_ms: T0, key: "x", block: "rev-hypothetical", cli: "some-new-cli", in: 0 }),
    sample({ ts_ms: T0 + 1, key: "x", block: "rev-hypothetical", cli: "some-new-cli", in: 7 }),
  ];
  assert.deepEqual(seriesKeys(rows).map((k) => k.key), ["rev-hypothetical/some-new-cli"]);
});

test("an empty block or cli keeps its own key and labels as unknown, never folded into a real one", () => {
  // A row written before slice B's fields existed. Filing it under a real
  // block would put spend on a block that never spent it.
  const rows: SeriesRowLike[] = [
    sample({ ts_ms: T0, key: "old", block: "", cli: "", in: 0 }),
    sample({ ts_ms: T0 + 1, key: "old", block: "", cli: "", in: 9 }),
    sample({ ts_ms: T0, key: "new", in: 0 }),
    sample({ ts_ms: T0 + 1, key: "new", in: 3 }),
  ];
  assert.equal(seriesKeyOf("", ""), `${UNKNOWN}/${UNKNOWN}`);
  assert.deepEqual(
    seriesKeys(rows).map((k) => [k.key, k.total]),
    [
      ["unknown/unknown", 9],
      ["worker-std/pi", 3],
    ]
  );
});

test("a hue is assigned per BLOCK, so adding a CLI never reshuffles a colour", () => {
  const oneCli: SeriesRowLike[] = [
    sample({ ts_ms: T0, key: "a", block: "rev-lead", cli: "claude", in: 0 }),
    sample({ ts_ms: T0 + 1, key: "a", block: "rev-lead", cli: "claude", in: 1 }),
    sample({ ts_ms: T0, key: "b", block: "worker-std", cli: "pi", in: 0 }),
    sample({ ts_ms: T0 + 1, key: "b", block: "worker-std", cli: "pi", in: 1 }),
  ];
  const before = new Map(seriesKeys(oneCli).map((k) => [k.block, k.hueIndex]));
  const twoCli = [
    ...oneCli,
    sample({ ts_ms: T0, key: "c", block: "worker-std", cli: "opencode", in: 0 }),
    sample({ ts_ms: T0 + 1, key: "c", block: "worker-std", cli: "opencode", in: 1 }),
  ];
  const after = new Map(seriesKeys(twoCli).map((k) => [k.block, k.hueIndex]));
  assert.deepEqual([...after.entries()].sort(), [...before.entries()].sort());
  assert.ok(before.get("rev-lead") !== before.get("worker-std"), "two blocks are two hues");
  assert.ok(seriesKeys(twoCli).every((k) => k.hueIndex >= 0 && k.hueIndex < HUE_SLOTS));
});

test("a hue follows the BLOCK, not its rank — narrowing the window repaints nothing", () => {
  // The data-viz non-negotiable: a filter that changes the series count must
  // not repaint the survivors. Without a stable `blockOrder` the fallback is
  // the sorted blocks OF THE WINDOW, so dropping the alphabetically-first
  // block shifts every remaining hue by one.
  const mk = (block: string, ts: number, key: string): SeriesRowLike[] => [
    sample({ ts_ms: ts, key, block, in: 0 }),
    sample({ ts_ms: ts + 1, key, block, in: 10 }),
  ];
  const roster = ["alpha-block", "worker-std", "rev-lead"];
  const wide = [...mk("alpha-block", T0, "a"), ...mk("worker-std", T0, "b"), ...mk("rev-lead", T0, "c")];
  // The narrowed window keeps only the two later blocks.
  const narrow = [...mk("worker-std", T0, "b"), ...mk("rev-lead", T0, "c")];

  const hues = (rows: SeriesRowLike[]) =>
    new Map(seriesKeys(rows, { blockOrder: roster }).map((k) => [k.block, k.hueIndex]));
  assert.equal(hues(wide).get("worker-std"), hues(narrow).get("worker-std"));
  assert.equal(hues(wide).get("rev-lead"), hues(narrow).get("rev-lead"));
  // …and the guard against a vacuous pass: WITHOUT the stable order the hues
  // really do move, so the assertions above are about `blockOrder` doing
  // something and not about two identical no-ops.
  const unstable = (rows: SeriesRowLike[]) =>
    new Map(seriesKeys(rows).map((k) => [k.block, k.hueIndex]));
  assert.notEqual(
    unstable(wide).get("worker-std"),
    unstable(narrow).get("worker-std"),
    "the fallback ordering is rank-based — this is the failure blockOrder exists to fix"
  );
});

test("a ninth block takes the neutral ramp, never a recycled hue", () => {
  // A repeated hue is a false claim of identity between two blocks. Nothing is
  // MERGED to avoid it, though — this is a cost chart, and folding two blocks'
  // spend together to save a colour would be the worse trade.
  const roster = Array.from({ length: 10 }, (_, i) => `b${i}`);
  const rows = roster.flatMap((b, i) => [
    sample({ ts_ms: T0, key: `k${i}`, block: b, in: 0 }),
    sample({ ts_ms: T0 + 1, key: `k${i}`, block: b, in: 5 }),
  ]);
  const keys = seriesKeys(rows, { blockOrder: roster });
  assert.equal(keys.length, 10, "every block still gets its own line");
  const assigned = keys.filter((k) => k.hueIndex !== null).map((k) => k.hueIndex);
  assert.equal(assigned.length, HUE_SLOTS);
  assert.equal(new Set(assigned).size, HUE_SLOTS, "no hue is used twice");
  assert.equal(keys.filter((k) => k.hueIndex === null).length, 2, "the 9th and 10th go neutral");
});

test("the categorical hue order is the MEASURED one, not theme.ts's declaration order", () => {
  // The order is the only lever a chart has over a fixed design system, and it
  // is what the adjacent-pair separation check reads. `IDENTITY`'s own order
  // puts azure beside violet, a pair a full-colour reader cannot separate
  // (ΔE 5.7 normal-vision); this order was searched, not chosen. The PR body's
  // agent layer carries the figures and the search.
  assert.deepEqual(HUE_ORDER, ["jade", "violet", "amber", "cyan", "rose", "azure", "lime", "orchid"]);
  const azure = HUE_ORDER.indexOf("azure");
  const violet = HUE_ORDER.indexOf("violet");
  assert.ok(
    Math.abs(azure - violet) > 1,
    "azure and violet are the octet's collision pair — they must not be adjacent"
  );
});

test("the bucket grid is aligned to the EPOCH, not to the window's start instant", () => {
  // Epoch-ms math only, the same property `timelinelayout.ts` leans on: bucket
  // boundaries land on multiples of the step in UTC, so the same data buckets
  // identically in every timezone and across a DST boundary. A grid anchored
  // to `startMs` instead would re-bucket every sample each time the window
  // slid by a second.
  const offGrid = T0 + 137_000; // deliberately not a multiple of BUCKET
  const b = bucketSeries([], { startMs: offGrid, endMs: offGrid + 2 * BUCKET, bucketMs: BUCKET });
  assert.ok(
    b.buckets.every((t) => t % BUCKET === 0),
    `a bucket start is off the grid: ${b.buckets.join(", ")}`
  );
  assert.equal(b.buckets[0], T0, "the grid starts at or before the window, on a boundary");
});

test("a delta outside the window is COUNTED as dropped, never clamped onto an edge", () => {
  const rows: SeriesRowLike[] = [
    sample({ ts_ms: T0 - 10 * BUCKET, key: "k", in: 0 }),
    sample({ ts_ms: T0 - 9 * BUCKET, key: "k", in: 500 }),
    sample({ ts_ms: T0 + BUCKET, key: "k", in: 700 }),
  ];
  const b = bucketSeries(rows, { startMs: T0, endMs: T0 + 2 * BUCKET, bucketMs: BUCKET });
  assert.equal(b.dropped, 1, "the pre-window delta is reported, not drawn at the left edge");
  assert.equal(
    b.keys[0].points.reduce((a, p) => a + p.total, 0),
    200,
    "only the in-window delta is plotted"
  );
});

// ── the marks ───────────────────────────────────────────────────────────────

test("a mark is labelled from the roster the samples actually show either side of it", () => {
  // The mark row carries hashes, never content, so the switch is MEASURED from
  // what the fleet then ran — the #2817 opencode -> pi change.
  const rows: SeriesRowLike[] = [
    sample({ ts_ms: T0, key: "a", block: "worker-std", cli: "opencode", in: 0 }),
    sample({ ts_ms: T0 + BUCKET, key: "a", block: "worker-std", cli: "opencode", in: 100 }),
    { kind: "mark", ts_ms: T0 + 2 * BUCKET, changed: ["workflow"], fp: {}, prev: {}, fp_partial: false },
    sample({ ts_ms: T0 + 3 * BUCKET, key: "b", block: "worker-std", cli: "pi", in: 0 }),
    sample({ ts_ms: T0 + 4 * BUCKET, key: "b", block: "worker-std", cli: "pi", in: 40 }),
  ];
  const [m] = marks(rows);
  assert.deepEqual(m.roster, [{ block: "worker-std", from: "opencode", to: "pi" }]);
  assert.equal(m.label, "worker-std: opencode → pi");
  assert.equal(m.tsMs, T0 + 2 * BUCKET);
  assert.equal(m.fpPartial, false);
});

test("a mark no block's CLI moved across falls back to the component list, and carries fp_partial", () => {
  const rows: SeriesRowLike[] = [
    sample({ ts_ms: T0, key: "a", in: 0 }),
    sample({ ts_ms: T0 + BUCKET, key: "a", in: 10 }),
    { kind: "mark", ts_ms: T0 + 2 * BUCKET, changed: ["skills", "claude_md"], fp: {}, prev: {}, fp_partial: true },
    sample({ ts_ms: T0 + 3 * BUCKET, key: "b", in: 0 }),
    sample({ ts_ms: T0 + 4 * BUCKET, key: "b", in: 10 }),
  ];
  const [m] = marks(rows);
  assert.deepEqual(m.roster, [], "same CLI either side — nothing to claim about the roster");
  assert.equal(m.label, "skills, claude_md changed");
  assert.equal(m.fpPartial, true, "an unchanged component is not proof nothing under it moved");
});

test("a block observed on only ONE side of a mark contributes no roster row", () => {
  // "Not observed" is not "unchanged" — the honest cost of measuring the
  // switch from samples rather than from the fingerprint's contents.
  const rows: SeriesRowLike[] = [
    sample({ ts_ms: T0, key: "a", block: "rev-std", cli: "opencode", in: 0 }),
    sample({ ts_ms: T0 + BUCKET, key: "a", block: "rev-std", cli: "opencode", in: 10 }),
    { kind: "mark", ts_ms: T0 + 2 * BUCKET, changed: ["workflow"], fp: {}, prev: {}, fp_partial: false },
    sample({ ts_ms: T0 + 3 * BUCKET, key: "b", block: "worker-adv", cli: "claude", in: 0 }),
  ];
  assert.deepEqual(marks(rows)[0].roster, []);
});

// ── the before/after readout ────────────────────────────────────────────────

test("beforeAfter is null — never zero — below k buckets on a side, and exact at k", () => {
  const mark = T0 + 4 * BUCKET;
  // Buckets T0 .. T0+7*BUCKET (8 of them). The mark splits 4 | 4.
  const rows: SeriesRowLike[] = [
    sample({ ts_ms: T0, key: "k", in: 0 }),
    sample({ ts_ms: T0 + BUCKET, key: "k", in: 10 }),
    sample({ ts_ms: T0 + 2 * BUCKET, key: "k", in: 30 }),
    sample({ ts_ms: T0 + 3 * BUCKET, key: "k", in: 60 }),
    sample({ ts_ms: T0 + 4 * BUCKET, key: "k", in: 160 }),
    sample({ ts_ms: T0 + 5 * BUCKET, key: "k", in: 360 }),
    sample({ ts_ms: T0 + 6 * BUCKET, key: "k", in: 660 }),
    sample({ ts_ms: T0 + 7 * BUCKET, key: "k", in: 1060 }),
  ];
  const b = bucketSeries(rows, { startMs: T0, endMs: T0 + 7 * BUCKET, bucketMs: BUCKET });

  // k = 4 fits exactly: before is buckets 0..3 (0, 10, 20, 30 -> mean 15),
  // after is buckets 4..7 (100, 200, 300, 400 -> mean 250).
  const at4 = beforeAfter(b, mark, "total", 4);
  const total4 = at4.find((r) => r.key === TOTAL_ROW)!;
  assert.equal(total4.before, 15);
  assert.equal(total4.after, 250);
  assert.equal(total4.delta, 235);
  assert.equal(total4.k, 4);
  assert.ok(Math.abs(total4.pct! - (250 / 15 - 1)) < 1e-9);

  // k = 5 does not fit: FOUR buckets before the mark, not five.
  const at5 = beforeAfter(b, mark, "total", 5);
  const total5 = at5.find((r) => r.key === TOTAL_ROW)!;
  assert.equal(total5.before, null, "n/a, not 0 — a zero would claim the fleet spent nothing");
  assert.equal(total5.after, null);
  assert.equal(total5.delta, null);
  assert.equal(total5.pct, null);

  assert.equal(DEFAULT_BEFORE_AFTER_K, 12, "the default half-width is one hour of five-minute buckets");
});

test("beforeAfter reports per key as well as in total", () => {
  const mark = T0 + 2 * BUCKET;
  const rows: SeriesRowLike[] = [
    sample({ ts_ms: T0, key: "oc", cli: "opencode", in: 0 }),
    sample({ ts_ms: T0 + BUCKET, key: "oc", cli: "opencode", in: 100 }),
    sample({ ts_ms: T0 + 2 * BUCKET, key: "oc", cli: "opencode", in: 100 }),
    sample({ ts_ms: T0 + 3 * BUCKET, key: "oc", cli: "opencode", in: 100 }),
    sample({ ts_ms: T0, key: "pi", cli: "pi", in: 0 }),
    sample({ ts_ms: T0 + BUCKET, key: "pi", cli: "pi", in: 0 }),
    sample({ ts_ms: T0 + 2 * BUCKET, key: "pi", cli: "pi", in: 20 }),
    sample({ ts_ms: T0 + 3 * BUCKET, key: "pi", cli: "pi", in: 60 }),
  ];
  const b = bucketSeries(rows, { startMs: T0, endMs: T0 + 3 * BUCKET, bucketMs: BUCKET });
  const rowsOut = beforeAfter(b, mark, "total", 2);
  const oc = rowsOut.find((r) => r.key === "worker-std/opencode")!;
  const pi = rowsOut.find((r) => r.key === "worker-std/pi")!;
  assert.deepEqual([oc.before, oc.after], [50, 0], "opencode spent before the mark and stopped");
  assert.deepEqual([pi.before, pi.after], [0, 30], "pi picked it up after");
  // The total row is a mean of real bucket TOTALS, not a mean of means.
  const all = rowsOut.find((r) => r.key === TOTAL_ROW)!;
  assert.deepEqual([all.before, all.after], [50, 30]);
});

test("a single unknown cost makes that side of the readout null rather than a smaller bill", () => {
  const mark = T0 + 2 * BUCKET;
  const rows: SeriesRowLike[] = [
    sample({ ts_ms: T0, key: "k", cost_usd: 0 }),
    sample({ ts_ms: T0 + BUCKET, key: "k", cost_usd: 1 }),
    sample({ ts_ms: T0 + 2 * BUCKET, key: "k", cost_usd: null }),
    sample({ ts_ms: T0 + 3 * BUCKET, key: "k", cost_usd: 5 }),
  ];
  const b = bucketSeries(rows, { startMs: T0, endMs: T0 + 3 * BUCKET, bucketMs: BUCKET });
  const all = beforeAfter(b, mark, "cost_usd", 2).find((r) => r.key === TOTAL_ROW)!;
  assert.equal(all.after, null, "one hole poisons the mean it sits in");
  // The control: the TOKEN metric over the same buckets is not null, so the
  // null above is the cost rule firing and not an empty window.
  assert.notEqual(beforeAfter(b, mark, "total", 2).find((r) => r.key === TOTAL_ROW)!.after, null);
});

// ── the bars ────────────────────────────────────────────────────────────────

test("a feature bar is stacked by series key, and its segments sum to its total", () => {
  const board = [
    row({ id: "t-1", title: "token charts", kind: "feature" }),
    row({ id: "t-2", kind: "task", parent: "t-1", assignee: "w-oc" }),
    row({ id: "t-3", kind: "task", parent: "t-1", assignee: "w-pi" }),
  ];
  const agents = [agent({ id: "w-oc", cli: "opencode" }), agent({ id: "w-pi", cli: "pi" })];
  const rows: SeriesRowLike[] = [
    sample({ ts_ms: T0, key: "a", agent: "w-oc", cli: "opencode", in: 0 }),
    sample({ ts_ms: T0 + 1, key: "a", agent: "w-oc", cli: "opencode", in: 300 }),
    sample({ ts_ms: T0, key: "b", agent: "w-pi", cli: "pi", in: 0 }),
    sample({ ts_ms: T0 + 1, key: "b", agent: "w-pi", cli: "pi", in: 120 }),
  ];
  const fb = featureBars(rows, agents, board);
  const bar = fb.bars.find((b) => b.id === "t-1")!;
  assert.equal(bar.total, 420);
  assert.equal(
    bar.segments.reduce((a, s) => a + s.tokens, 0),
    bar.total,
    "the stack IS the bar — a segment the loop missed would leave a short stack"
  );
  assert.deepEqual(
    bar.segments.filter((s) => s.tokens > 0).map((s) => [s.key, s.tokens]),
    [
      ["worker-std/opencode", 300],
      ["worker-std/pi", 120],
    ]
  );
  assert.deepEqual(bar.agents.slice().sort(), ["w-oc", "w-pi"]);
});

test("every bar carries the same segment keys in the same order, so stacks are comparable", () => {
  const board = [
    row({ id: "t-1", title: "one", kind: "feature" }),
    row({ id: "t-2", kind: "task", parent: "t-1", assignee: "w-1" }),
  ];
  const rows: SeriesRowLike[] = [
    sample({ ts_ms: T0, key: "a", agent: "w-1", cli: "opencode", in: 0 }),
    sample({ ts_ms: T0 + 1, key: "a", agent: "w-1", cli: "opencode", in: 10 }),
    sample({ ts_ms: T0, key: "b", agent: "w-x", cli: "pi", in: 0 }),
    sample({ ts_ms: T0 + 1, key: "b", agent: "w-x", cli: "pi", in: 20 }),
  ];
  const fb = featureBars(rows, [agent({ id: "w-1" }), agent({ id: "w-x", task: "" })], board);
  const shapes = fb.bars.map((b) => b.segments.map((s) => s.key).join("|"));
  assert.equal(new Set(shapes).size, 1, `bars disagree about their stack order: ${shapes.join(" vs ")}`);
});

test("cost is labelled reported / estimated / mixed, never printed bare", () => {
  const board = [
    row({ id: "t-1", title: "one", kind: "feature" }),
    row({ id: "t-2", kind: "task", parent: "t-1", assignee: "w-1" }),
  ];
  const est: SeriesRowLike[] = [
    sample({ ts_ms: T0, key: "a", agent: "w-1", in: 0, estimated: true, cost_usd: 0 }),
    sample({ ts_ms: T0 + 1, key: "a", agent: "w-1", in: 10, estimated: true, cost_usd: 2 }),
  ];
  assert.equal(featureBars(est, [agent({ id: "w-1" })], board).bars.find((b) => b.id === "t-1")!.costLabel, "estimated");

  const both: SeriesRowLike[] = [
    ...est,
    sample({ ts_ms: T0 + 2, key: "a", agent: "w-1", in: 30, estimated: false, cost_usd: 5 }),
  ];
  assert.equal(featureBars(both, [agent({ id: "w-1" })], board).bars.find((b) => b.id === "t-1")!.costLabel, "mixed");

  const none = featureBars([], [agent({ id: "w-1" })], board).bars.find((b) => b.id === "t-1")!;
  assert.equal(none.costLabel, "none");
  assert.equal(none.total, 0);
});

test("spend by an agent the ROSTER does not know lands on unattributed and is counted apart", () => {
  // A different fact from "the board does not know it": the roster is
  // group-wide and includes exited agents, so a miss here is a real hole.
  const board = [row({ id: "t-1", title: "one", kind: "feature" })];
  const rows: SeriesRowLike[] = [
    sample({ ts_ms: T0, key: "k", agent: "w-ghost", in: 0 }),
    sample({ ts_ms: T0 + 1, key: "k", agent: "w-ghost", in: 42 }),
  ];
  const fb = featureBars(rows, [], board);
  assert.equal(fb.unknownAgentTokens, 42);
  assert.equal(fb.totals.unattributed, 42);
  assert.equal(fb.bars.find((b) => b.kind === "unattributed")!.total, 42);
});

// ── the scorecard columns ───────────────────────────────────────────────────

test("the coverage floor is the oldest audit row read, and null when nothing was read", () => {
  const board = [
    row({ id: "t-1", title: "one", kind: "feature", pr: "#500" }),
    row({ id: "t-2", kind: "task", parent: "t-1", assignee: "w-1" }),
  ];
  const fb = featureBars([], [agent({ id: "w-1" })], board);

  const empty = scorecardColumns([], fb, board);
  assert.equal(empty.floorMs, null, "we have not looked — NOT 'there is no history'");
  assert.equal(empty.auditRows, 0);

  const audit: AuditRowLike[] = [
    { ts_ms: T0 + 5000, actor: "rev-1", action: "review-verdict", detail: { pr: "#500", verdict: "fail" } },
    { ts_ms: T0 + 1000, actor: "rev-1", action: "review-verdict", detail: { pr: "#500", verdict: "pass" } },
  ];
  const sc = scorecardColumns(audit, fb, board);
  assert.equal(sc.floorMs, T0 + 1000, "the OLDEST row, whatever order they arrived in");
  assert.equal(sc.auditRows, 2);

  const col = sc.columns.find((c) => c.featureId === "t-1")!;
  assert.equal(col.rounds, 2, "both verdicts on #500 roll up to its feature");
  assert.equal(col.failRate, 0.5);

  // A feature whose first spend predates the floor is FLAGGED, not silently
  // presented as a complete count.
  const flagged = scorecardColumns(audit, fb, board, {
    firstSpendMs: new Map([["t-1", T0]]),
  });
  assert.equal(flagged.columns.find((c) => c.featureId === "t-1")!.belowFloor, true);
  assert.equal(
    scorecardColumns(audit, fb, board, { firstSpendMs: new Map([["t-1", T0 + 9999]]) }).columns.find(
      (c) => c.featureId === "t-1"
    )!.belowFloor,
    false,
    "…and a feature entirely inside the window is not flagged — the control"
  );
});

test("a feature with no verdicts reports failRate null, never a perfect 0", () => {
  const board = [
    row({ id: "t-1", title: "one", kind: "feature", pr: "#500" }),
    row({ id: "t-9", title: "two", kind: "feature", pr: "#900" }),
  ];
  const fb = featureBars([], [], board);
  const audit: AuditRowLike[] = [
    { ts_ms: T0, actor: "rev-1", action: "review-verdict", detail: { pr: "#500", verdict: "pass" } },
  ];
  const sc = scorecardColumns(audit, fb, board);
  // t-9 has no bar (no agent landed on it), so the column set is the bars'.
  assert.ok(sc.columns.every((c) => c.rounds > 0 || c.failRate === null));
});

test("driver and orchestrator turns land on the group-wide bar, not spread over features", () => {
  const board = [row({ id: "t-1", title: "one", kind: "feature" })];
  const fb = featureBars([], [agent({ id: "orch-1", role: "orchestrator" })], board);
  const audit: AuditRowLike[] = [
    { ts_ms: T0, actor: "driver", action: "rd-lane-spawned", detail: {} },
    { ts_ms: T0 + 1, actor: "driver", action: "rd-handback", detail: {} },
    { ts_ms: T0 + 2, actor: "orch-1", action: "prompt", detail: {} },
    { ts_ms: T0 + 3, actor: "w-1", action: "prompt", detail: {} },
  ];
  const sc = scorecardColumns(audit, fb, board);
  const orch = sc.columns.find((c) => c.featureId === ORCHESTRATOR)!;
  assert.equal(orch.driverTurns, 2);
  assert.equal(orch.orchTurns, 1, "a prompt to a WORKER pane is not an orchestrator turn");
});
