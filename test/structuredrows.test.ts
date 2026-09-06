// Tests for the structured pane's row model (#2891 S4).
//
// The renderer's real decisions live in `src/structuredrows.ts` precisely so
// they can be tested here rather than through a fake DOM. The properties below
// are the ones a renderer breaks silently:
//
//  - a virtualisation window that drops a row that is on screen, or keeps
//    thousands that are not (the whole point of the slice);
//  - a spacer height that disagrees with the rows it stands in for, which shows
//    up as a scrollbar that lies and a view that jumps;
//  - a gutter segment reaching for a colour outside the six state dyes;
//  - a collapsed tool card identified by the wrong argument, so every `Grep` in
//    a session reads as the same word;
//  - a permission card that does not say what it is permitting;
//  - a figure nobody reported rendered as `0` rather than as unknown.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { emptyState, emptyViewState, project } from "../src/structuredview.ts";
import { decodeBatch, decodeProjectionInput, ProjectionDecodeError } from "../src/structuredview.ts";
import type { ProjectionInput, RequestBlock, State, ToolBlock } from "../src/structuredview.ts";
import {
  OVERSCAN_PX,
  ROW_ESTIMATE,
  SEGMENTS,
  actionsFor,
  answerWasRefusal,
  argLine,
  bottomWindow,
  isOpen,
  chipFor,
  computeWindow,
  diffOf,
  droppedNotice,
  evictionNotice,
  fmtBytes,
  fmtMs,
  fmtTok,
  fmtUsd,
  headerFacts,
  isEditTool,
  isShellTool,
  payloadIsProse,
  pinnedAtBottom,
  requestPayload,
  requestTitle,
  rowSignature,
  rowsFor,
  segmentOf,
  settlementLine,
  tickerFields,
  toolMark,
  turnReceipt,
  wantsFreeText,
} from "../src/structuredrows.ts";
import type { RowSpec, Segment } from "../src/structuredrows.ts";
import { MARK_NAMES } from "../src/structuredicons.ts";

const FIXTURE = fileURLToPath(
  new URL("./fixtures/structuredview/session.harness.jsonl", import.meta.url),
);

/** Split on `\n` only and strip an optional trailing `\r` — pi's own framing
 *  rule, which is also what makes this reader correct on a CRLF checkout. */
function readFixture(): ProjectionInput[] {
  return readFileSync(FIXTURE, "utf8")
    .split("\n")
    .map((l) => (l.endsWith("\r") ? l.slice(0, -1) : l))
    .filter((l) => l.length > 0)
    // DECODE, never cast. The `as` this replaces is why three wrong spellings
    // sat in this fixture under a green suite (#2891 S4).
    .map((l) => decodeProjectionInput(JSON.parse(l)));
}

function fixtureState(): State {
  return project(emptyState(), readFixture(), { nowMs: 10_000 });
}

// ── virtualisation ──────────────────────────────────────────────────────────

test("the window covers every pixel of the viewport and nothing far outside it", () => {
  // 2000 rows of 30px — the projection's own MAX_BLOCKS ceiling at a plausible
  // row height, which is the load this slice exists to survive.
  const heights = Array.from({ length: 2000 }, () => 30);
  const w = computeWindow(heights, 15_000, 600, OVERSCAN_PX);

  assert.ok(w.start < w.end, "positive control: the window is not empty");
  // Every row intersecting [scrollTop, scrollTop+viewport] is inside [start,end).
  const firstVisible = Math.floor(15_000 / 30);
  const lastVisible = Math.floor((15_000 + 600 - 1) / 30);
  assert.ok(w.start <= firstVisible, `start ${w.start} is past the first visible row ${firstVisible}`);
  assert.ok(w.end > lastVisible, `end ${w.end} does not reach the last visible row ${lastVisible}`);

  // And it is bounded: viewport + two overscans, in rows, plus slop for the
  // partial rows at each edge. The FAILURE this bounds is "render everything".
  const bound = Math.ceil((600 + 2 * OVERSCAN_PX) / 30) + 2;
  assert.ok(
    w.end - w.start <= bound,
    `window holds ${w.end - w.start} rows, more than the ${bound} the viewport plus overscan can need`,
  );
  assert.ok(w.end - w.start < heights.length / 10, "and it is a small fraction of 2000 rows");
});

test("the spacers plus the windowed rows are exactly the total height", () => {
  // A scrollbar scaled to a total the spacers do not add up to is a view that
  // jumps under the human's hand — the defect virtualisation most often ships.
  const heights = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
  for (const scrollTop of [0, 55, 200, 10_000]) {
    const w = computeWindow(heights, scrollTop, 100, 25);
    let inWindow = 0;
    for (let i = w.start; i < w.end; i += 1) inWindow += heights[i]!;
    assert.equal(
      w.padTop + inWindow + w.padBottom,
      w.total,
      `at scrollTop=${scrollTop} the spacers and the window do not sum to the total`,
    );
    assert.equal(w.total, 550, "and the total is every row, not just the scanned ones");
  }
});

test("a zero-height row is absorbed rather than thrown on", () => {
  // A row measured while its pane was `display:none` really does report 0. The
  // window must still be a window.
  const w = computeWindow([0, 0, 0, 40, 40], 0, 100, 0);
  assert.equal(w.total, 80);
  assert.ok(w.end > w.start, "positive control: a real row is still in the window");
});

test("scrolled past every row the window is empty, not the last row", () => {
  const w = computeWindow([30, 30, 30], 5000, 100, 0);
  assert.equal(w.start, 3);
  assert.equal(w.end, 3);
  assert.equal(w.padTop, 90, "the whole height is above the empty window");
  assert.equal(w.padBottom, 0);
});

test("an empty transcript produces an empty window with no height", () => {
  const w = computeWindow([], 0, 600);
  assert.deepEqual(w, { start: 0, end: 0, padTop: 0, padBottom: 0, total: 0 });
});

test("a pinned window always ends at the last row, with NOTHING below it", () => {
  // WHAT RESTS ON THIS. The renderer follows the live end with `scrollTop =
  // scrollHeight` — the BROWSER's number, off the real laid-out height — while
  // the window it built came from the height array, which is ESTIMATES for
  // every row not yet measured. If a pinned window ever ended short of the last
  // row, that scroll would land inside `padBottom` and the viewport would show
  // a spacer: a blank pane, no error, nothing red, self-correcting only once
  // the burst stopped bringing unmeasured rows.
  //
  // `computeWindow` at a derived scrollTop happens to give `padBottom === 0`
  // today, as a consequence of its loop bound rather than as anything stated.
  // This is the stated version, and this test is what keeps it stated.
  const heights = Array.from({ length: 500 }, (_, i) => 20 + (i % 7) * 15);
  const w = bottomWindow(heights, 600, 600);
  assert.equal(w.end, heights.length, "the window ends at the last row");
  assert.equal(w.padBottom, 0, "and there is nothing below it to land in");
  assert.ok(w.start < w.end, "positive control: the window is not empty");

  let inWindow = 0;
  for (let i = w.start; i < w.end; i += 1) inWindow += heights[i]!;
  assert.equal(w.padTop + inWindow, w.total, "the spacer and the window are the whole height");
  assert.ok(
    inWindow >= 600 + 600,
    `the window holds ${inWindow}px, less than the viewport plus overscan it must fill`,
  );
  assert.ok(w.end - w.start < heights.length / 4, "and it is still a small slice of 500 rows");
});

test("a pinned window on a transcript shorter than the viewport is the whole thing", () => {
  const w = bottomWindow([30, 30, 30], 600, 600);
  assert.deepEqual(w, { start: 0, end: 3, padTop: 0, padBottom: 0, total: 90 });
});

test("a pinned window on an empty transcript is empty, not a negative slice", () => {
  assert.deepEqual(bottomWindow([], 600), { start: 0, end: 0, padTop: 0, padBottom: 0, total: 0 });
});

test("pinnedAtBottom tolerates sub-pixel layout, and lets go when the human scrolls up", () => {
  assert.equal(pinnedAtBottom(900.4, 1500, 600), true, "0.4px short of the end is still pinned");
  assert.equal(pinnedAtBottom(880, 1500, 600), true, "within one row's slack");
  assert.equal(pinnedAtBottom(400, 1500, 600), false, "scrolled up, not following");
});

// ── the warp gutter ─────────────────────────────────────────────────────────

test("every block kind's gutter segment is one of the six state dyes", () => {
  // DESIGN.md §3: the gutter's vocabulary is the six state dyes and NOTHING
  // else. A seventh meaning is an edit to ui-redesign.md, not a new value here.
  const s = fixtureState();
  assert.ok(s.blocks.length > 0, "positive control: there are blocks to classify");
  const kinds = new Set(s.blocks.map((b) => b.kind));
  assert.ok(kinds.size >= 6, `positive control: the fixture exercises ${kinds.size} block kinds`);
  for (const b of s.blocks) {
    assert.ok(
      SEGMENTS.includes(segmentOf(b)),
      `${b.kind} claims segment "${segmentOf(b)}", which is not a state dye`,
    );
  }
});

test("a pending request is the attention dye, and a settled one records how it went", () => {
  const s = fixtureState();
  const reqs = s.blocks.filter((b): b is RequestBlock => b.kind === "request");
  assert.equal(reqs.length, 2, "positive control: the fixture has both a permission and a dialog");
  // Both are settled in the fixture — one denied, one answered.
  assert.deepEqual(reqs.map(segmentOf), ["danger", "ok"]);

  // The pending case, built by dropping the settlement: the one the gutter's
  // attention dye exists for.
  const pending: RequestBlock = { ...reqs[0]!, settled: null };
  assert.equal(segmentOf(pending), "attention");
});

test("a delivery and a turn boundary are marked by form, never by a hue of their own", () => {
  const s = fixtureState();
  const deliveries = s.blocks.filter((b) => b.kind === "delivery");
  const turns = s.blocks.filter((b) => b.kind === "turn");
  assert.ok(deliveries.length > 0 && turns.length > 0, "positive control: both are present");
  for (const b of [...deliveries, ...turns]) {
    assert.equal(
      segmentOf(b),
      "idle",
      `${b.kind} took a dye of its own — §3 marks a seam by form, not by hue`,
    );
  }
});

test("a refusal is read the same way wherever the answer shape came from", () => {
  assert.equal(answerWasRefusal("deny"), true);
  assert.equal(answerWasRefusal("cancelled"), true);
  assert.equal(answerWasRefusal({ confirmed: false }), true);
  assert.equal(answerWasRefusal({ value: "no" }), true);
  assert.equal(answerWasRefusal("allow"), false);
  assert.equal(answerWasRefusal({ confirmed: true }), false);
  assert.equal(answerWasRefusal({ value: "fix/2214-resume-model" }), false);
});

// ── the row list ────────────────────────────────────────────────────────────

test("rowsFor is one row per block, in block order, keyed by block id", () => {
  const s = fixtureState();
  const rows = rowsFor(s, emptyViewState());
  assert.equal(rows.length, s.blocks.length);
  assert.deepEqual(rows.map((r) => r.key), s.blocks.map((b) => b.id));
  assert.deepEqual(rows.map((r) => r.kind), s.blocks.map((b) => b.kind));
});

test("the caret follows the open block, not the last one", () => {
  // The caret reports a state and stops the instant that state ends (§5). A
  // caret parked on the last row of a finished turn reports a state that ended.
  const mid = project(emptyState(), [
    { kind: "turn_started", turn: 1 },
    { kind: "text", turn: 1, delta: "still coming" },
  ]);
  const live = rowsFor(mid, emptyViewState()).filter((r) => r.live);
  assert.deepEqual(live.map((r) => r.kind), ["text"], "the open text block is live");

  const done = project(mid, [
    { kind: "turn_ended", turn: 1, usage: null, cost: null, stop: "end_turn" },
  ]);
  assert.deepEqual(
    rowsFor(done, emptyViewState()).filter((r) => r.live),
    [],
    "nothing is live once the turn ended",
  );
});

test("dimThinking folds a STREAMING thinking row and touches nothing else", () => {
  // The switch has to beat the streaming default, which is the only case where
  // it has any work to do — a settled thinking block is folded already, so a
  // fixture of finished turns cannot tell a working switch from a dead one.
  const s = project(emptyState(), [
    { kind: "turn_started", turn: 1 },
    { kind: "tool_call", turn: 1, id: "t1", name: "Grep", input: { pattern: "x" } },
    { kind: "tool_output", turn: 1, id: "t1", delta: "a match", is_error: false },
    { kind: "thinking", turn: 1, delta: "still weighing it up" },
  ]);
  const view = emptyViewState();
  const before = rowsFor(s, view);
  assert.ok(
    before.some((r) => r.kind === "thinking" && !r.collapsed),
    "positive control: a streaming thinking row starts open",
  );
  assert.ok(
    before.some((r) => r.kind === "tool" && !r.collapsed),
    "positive control: an open row of another kind is present to be left alone",
  );

  view.dimThinking = true;
  const after = rowsFor(s, view);
  for (let i = 0; i < after.length; i += 1) {
    if (after[i]!.kind === "thinking") assert.equal(after[i]!.collapsed, true);
    else assert.equal(after[i]!.collapsed, before[i]!.collapsed, `${after[i]!.kind} folded too`);
  }
});

test("a taller block estimates taller — the estimate is not one constant", () => {
  // One estimate for every row makes the scrollbar jump every time a row is
  // measured, which is the shape that makes virtualisation feel broken.
  const short = project(emptyState(), [{ kind: "text", turn: 1, delta: "hi" }]);
  const long = project(emptyState(), [
    { kind: "text", turn: 1, delta: "x".repeat(74 * 40) },
  ]);
  const a = rowsFor(short, emptyViewState())[0]!;
  const b = rowsFor(long, emptyViewState())[0]!;
  assert.equal(a.estimate, ROW_ESTIMATE.line, "a one-line block is one line");
  assert.ok(b.estimate >= 40 * ROW_ESTIMATE.line, `a 40-line block estimated ${b.estimate}`);
});

test("a failure opens itself, and a hand-fold keeps it shut", () => {
  const s = project(emptyState(), [
    { kind: "tool_call", turn: 1, id: "t1", name: "Bash", input: { command: "npm test" } },
    { kind: "tool_output", turn: 1, id: "t1", delta: "boom", is_error: true },
    { kind: "tool_result", turn: 1, id: "t1", ok: false },
  ]);
  const card = s.blocks.find((b): b is ToolBlock => b.kind === "tool")!;
  const view = emptyViewState();
  assert.equal(isOpen(card, view, s), true, "a failed call opens itself");
  view.collapsed.add(card.id);
  assert.equal(isOpen(card, view, s), false, "a human's fold wins");
});

test("a call streaming output opens itself; a queued one does not", () => {
  const view = emptyViewState();
  const sr = project(emptyState(), [
    { kind: "tool_call", turn: 1, id: "t1", name: "Bash", input: { command: "npm test" } },
    { kind: "tool_output", turn: 1, id: "t1", delta: "running...", is_error: false },
  ]);
  const running = sr.blocks.find((b): b is ToolBlock => b.kind === "tool")!;
  assert.equal(running.status, "running", "positive control: the projection marked it running");
  assert.equal(isOpen(running, view, sr), true);

  const sq = project(emptyState(), [
    { kind: "tool_call", turn: 1, id: "t9", name: "Grep", input: { pattern: "x" } },
  ]);
  const queued = sq.blocks.find((b): b is ToolBlock => b.kind === "tool")!;
  assert.equal(isOpen(queued, view, sq), false, "a queued call with no output stays folded");
});

test("the fold set is a FLIP of the default, not a set of folded blocks", () => {
  // The property that makes a fold survive a default moving under the human's
  // feet — and the one a naive "collapsed means folded" reading gets wrong.
  const view = emptyViewState();
  const mid = project(emptyState(), [
    { kind: "tool_call", turn: 1, id: "t1", name: "Grep", input: { pattern: "x" } },
    { kind: "tool_output", turn: 1, id: "t1", delta: "a match", is_error: false },
  ]);
  const card = () => mid.blocks.find((b): b is ToolBlock => b.kind === "tool")!;
  assert.equal(isOpen(card(), view, mid), true, "open by default while it streams");

  // The human folds it while it is running, then it succeeds — whose default is
  // now "folded". A set read as "opened" would re-open it here.
  view.collapsed.add(card().id);
  const done = project(mid, [{ kind: "tool_result", turn: 1, id: "t1", ok: true }]);
  assert.equal(card().status, "ok", "positive control: the default moved");
  assert.equal(isOpen(card(), view, done), true, "flipping a now-folded default OPENS it");

  // And the other direction: a card the human opened while queued stays open
  // when it fails, rather than the failure's own auto-open cancelling the flip.
  const v2 = emptyViewState();
  const failed = project(emptyState(), [
    { kind: "tool_call", turn: 1, id: "t2", name: "Grep", input: { pattern: "y" } },
    { kind: "tool_result", turn: 1, id: "t2", ok: false },
  ]);
  const bad = failed.blocks.find((b): b is ToolBlock => b.kind === "tool")!;
  assert.equal(isOpen(bad, v2, failed), true, "a failure opens itself");
  v2.collapsed.add(bad.id);
  assert.equal(isOpen(bad, v2, failed), false, "and the human may still fold it away");
});

test("thinking is open while it streams and folds itself once the model moves on", () => {
  // §6: the one block the eye should be able to skip. Folding it WHILE it
  // streams would hide the thing the caret is reporting.
  const view = emptyViewState();
  const open = project(emptyState(), [
    { kind: "turn_started", turn: 1 },
    { kind: "thinking", turn: 1, delta: "weighing it up" },
  ]);
  const think = () => open.blocks.find((b) => b.kind === "thinking")!;
  assert.equal(isOpen(think(), view, open), true, "open while it streams");

  const moved = project(open, [{ kind: "text", turn: 1, delta: "Here is the answer." }]);
  assert.equal(isOpen(think(), view, moved), false, "folded once the model moved on");
});

// ── the row signature ───────────────────────────────────────────────────────

/** The row a signature test is about, minus the block. Defaults are a plain,
 *  open, settled row; every test below varies only what it is pinning. */
function rowShell(over: Partial<RowSpec> = {}): Pick<RowSpec, "collapsed" | "live" | "segment"> {
  return { collapsed: false, live: false, segment: "idle", ...over };
}

test("every field a row draws moves the signature", () => {
  // THE FAILURE THIS PINS. `project()` mutates blocks in place, so a renderer
  // that compares by object identity never repaints. The signature is what
  // stands in for identity — and a field it omits is a row that goes stale on
  // screen with nothing red to say so. So each field is moved on its own.
  const base = project(emptyState(), [
    { kind: "tool_call", turn: 1, id: "t1", name: "Grep", input: { pattern: "x" } },
  ]).blocks.find((b): b is ToolBlock => b.kind === "tool")!;
  const sig = (over: Partial<ToolBlock>, collapsed = true) =>
    rowSignature({ ...base, ...over }, rowShell({ collapsed }));

  const start = sig({});
  for (const [what, over] of [
    ["name", { name: "Glob" }],
    ["status", { status: "running" as const }],
    ["isError", { isError: true }],
    ["output", { output: "hello" }],
    ["outputDroppedBytes", { outputDroppedBytes: 99 }],
    ["durationMs", { durationMs: 12 }],
    ["orphan", { orphan: true }],
  ] as const) {
    assert.notEqual(sig(over), start, `${what} does not move the signature — a stale row`);
  }
  assert.notEqual(sig({}, false), start, "the fold does not move the signature");
  assert.equal(sig({}), start, "positive control: an unchanged block is unchanged");
});

test("a text delta moves its row's signature", () => {
  const s = project(emptyState(), [{ kind: "text", turn: 1, delta: "one" }]);
  const before = rowSignature(s.blocks[0]!, rowShell());
  project(s, [{ kind: "text", turn: 1, delta: " two" }]);
  assert.notEqual(rowSignature(s.blocks[0]!, rowShell()), before, "an append must repaint the row");
});

test("a turn's receipt landing moves its signature", () => {
  const s = project(emptyState(), [{ kind: "turn_started", turn: 1 }]);
  const before = rowSignature(s.blocks[0]!, rowShell());
  project(s, [
    {
      kind: "turn_ended",
      turn: 1,
      usage: {
        call_cumulative: { input: 10, output: 2, cache_read: 0, cache_creation: 0 },
        this_turn_main_loop: null,
        per_model: [],
      },
      cost: { usd: 0.5, basis: "reported" },
      stop: "end_turn",
    },
  ]);
  assert.notEqual(rowSignature(s.blocks[0]!, rowShell()), before, "the receipt must repaint the rule");
});

test("a settlement moves a request row's signature", () => {
  const s = project(emptyState(), [
    { kind: "permission_request", id: "r1", tool: "Bash", input: { command: "rm -rf /" } },
  ]);
  const before = rowSignature(s.blocks[0]!, rowShell());
  project(s, [{ kind: "permission_settled", id: "r1", decision: "deny", by: "human" }]);
  assert.notEqual(rowSignature(s.blocks[0]!, rowShell()), before, "a settled card must repaint");
});


test("the caret stops when the live run ends, even though the block never changed", () => {
  // REVIEW ROUND 1, FINDING 1 — a real defect this pins, and the shape of it is
  // worth keeping: `live` is derived from the PROJECTION's open-block pointers,
  // not from the block, so a `tool_call` that closes an open text run flips
  // `live` while leaving the text block byte-identical. A signature built from
  // the block alone was therefore equal across the transition, the reconciler
  // reused the node, and the caret kept blinking on a finished paragraph — for
  // the life of the pane, on the fixture's own shape.
  const s = project(emptyState(), [
    { kind: "turn_started", turn: 1 },
    { kind: "text", turn: 1, delta: "still coming" },
  ]);
  const row = () => rowsFor(s, emptyViewState()).find((r) => r.kind === "text")!;
  const block = () => s.blocks.find((b) => b.kind === "text")!;

  const before = { ...row() };
  const sigBefore = rowSignature(block(), before);
  assert.equal(before.live, true, "positive control: the run is live to start with");

  project(s, [{ kind: "tool_call", turn: 1, id: "t1", name: "Grep", input: { pattern: "x" } }]);
  const after = { ...row() };
  assert.equal(after.live, false, "positive control: closing the run flipped `live`");

  // The block itself did NOT move — which is exactly why this was invisible.
  assert.equal(block().text.length, 12, "the text block was not mutated by the tool call");
  assert.notEqual(
    rowSignature(block(), after),
    sigBefore,
    "the signature is equal across a live→settled flip, so the renderer reuses the node " +
      "and the caret never stops",
  );
});

test("every RowSpec field a row DRAWS moves the signature, and the two that are not drawn do not", () => {
  // The generalisation of the finding above. `rowSignature` takes the ROW now,
  // not just the block, so the fields are enumerated here rather than trusted:
  // a field added to `RowSpec` and drawn but left out of the signature is the
  // same silent-stale-row defect one field over.
  const s = project(emptyState(), [{ kind: "text", turn: 1, delta: "hello" }]);
  const b = s.blocks[0]!;
  const base = rowsFor(s, emptyViewState())[0]!;
  const sig = (over: Partial<RowSpec>) => rowSignature(b, { ...base, ...over });

  const start = sig({});
  assert.equal(sig({}), start, "positive control: an unchanged row is unchanged");
  for (const [what, over] of [
    ["live", { live: !base.live }],
    ["collapsed", { collapsed: !base.collapsed }],
    ["segment", { segment: "danger" as const }],
  ] as const) {
    assert.notEqual(sig(over), start, `${what} is drawn but does not move the signature`);
  }
  // And the two that are deliberately excluded, each for a stated reason:
  // `key` is the map key (a different key is a different row, not a repaint),
  // and `estimate` is never drawn — it is superseded by measurement, so folding
  // it in would repaint every row whose text grew by one character.
  assert.equal(sig({ key: "somewhere-else" }), start, "`key` is the map key, not a painted field");
  assert.equal(sig({ estimate: base.estimate + 999 }), start, "`estimate` is never drawn");
});

// ── the tool's mark and family ──────────────────────────────────────────────

test("a tool's family is an app icon role, and an unknown tool earns no hue", () => {
  assert.deepEqual(toolMark("Grep"), { mark: "search", family: "workspace" });
  assert.deepEqual(toolMark("bash"), { mark: "terminal", family: "source" });
  assert.deepEqual(toolMark("mcp__orrerix__report"), { mark: "bolt", family: "fleet" });
  assert.deepEqual(
    toolMark("SomethingNobodyHasSeen"),
    { mark: "box", family: null },
    "an unknown tool draws in plain ink rather than taking a hue it has not earned",
  );
  assert.deepEqual(toolMark(null), { mark: "box", family: null }, "and so does an orphan card");
});

test("every mark the row model can name is one the icon module draws", () => {
  // A mark the renderer asks for and `structuredicons.ts` does not have is a
  // `undefined` spliced into the markup, which renders as nothing at all.
  const names = new Set(MARK_NAMES as readonly string[]);
  const asked = new Set<string>();
  for (const t of ["Read", "Write", "Edit", "MultiEdit", "NotebookEdit", "Glob", "Grep", "LS",
    "Bash", "BashOutput", "WebFetch", "WebSearch", "Task", "Agent", "TodoWrite", "git",
    "mcp__x__y", "Unheard"]) {
    asked.add(toolMark(t).mark);
  }
  asked.add(toolMark(null).mark);
  assert.ok(asked.size >= 9, `positive control: the table asked for ${asked.size} distinct marks`);
  const missing = [...asked].filter((m) => !names.has(m));
  assert.deepEqual(missing, [], `the row model names marks the icon module does not draw`);
});

// ── the identifying argument ────────────────────────────────────────────────

test("each tool is identified by ITS OWN argument, not by whichever key comes first", () => {
  // The failure this exists to stop: every Grep in a session rendering as the
  // same word because `path` came first in one fixed list.
  assert.equal(argLine("Grep", { path: "src", pattern: "pane_kind" }), "pane_kind");
  assert.equal(argLine("Glob", { path: "src", pattern: "**/*.ts" }), "**/*.ts");
  assert.equal(argLine("Read", { path: "src", file_path: "src/pane.ts" }), "src/pane.ts");
  assert.equal(argLine("Bash", { command: "npm test", timeout: 5 }), "npm test");
  assert.equal(argLine("WebFetch", { url: "https://example.test" }), "https://example.test");
});

test("the tool table is keyed by the tool, never by which CLI is running", () => {
  // Claude Code emits `Bash`, pi emits `bash`. That is a harness vocabulary
  // difference, and CLAUDE.md forbids branching a NAME on the source — so the
  // two spellings take the same row rather than two.
  assert.equal(argLine("bash", { command: "ls" }), argLine("Bash", { command: "ls" }));
  assert.equal(isShellTool("bash"), true);
  assert.equal(isShellTool("Bash"), true);
  assert.equal(isShellTool("Grep"), false);
  assert.equal(isShellTool(null), false);
  assert.equal(isEditTool("MultiEdit"), true);
  assert.equal(isEditTool("multiedit"), true);
  assert.equal(isEditTool("Read"), false);
});

test("a tool nobody has a row for still shows something identifying", () => {
  assert.equal(argLine("mcp__x__do_thing", { file_path: "a.ts" }), "a.ts");
  assert.equal(argLine("mcp__x__do_thing", { weird: 42 }), "weird=42");
  assert.equal(argLine("mcp__x__do_thing", {}), "");
  assert.equal(argLine(null, { command: "ls" }), "ls", "an orphan card has no name and still reads");
});

// ── the request card ────────────────────────────────────────────────────────

const permission = (input: unknown): RequestBlock => ({
  id: "b1",
  turn: null,
  kind: "request",
  channel: "permission",
  requestId: "r1",
  tool: "Bash",
  method: null,
  title: null,
  message: null,
  options: [],
  timeoutMs: null,
  input,
  settled: null,
});

test("a permission card says what it is permitting, not just which tool", () => {
  // §3.2's whole gain over the argv path: the prompt sees the ACTUAL call, so a
  // policy can allow `Bash(git status)` and refuse `Bash(git push)`. A card
  // showing only the tool name trains the human to click Allow without reading.
  const b = permission({ command: "git push --force" });
  assert.equal(requestTitle(b), "Allow Bash?");
  assert.match(requestPayload(b), /git push --force/, "the arguments are on the card");
  assert.notEqual(requestPayload(b), "Bash", "the tool name alone is not the payload");
});

test("a permission with no identifying key still shows its whole input", () => {
  const b = permission({ mode: "acceptEdits" });
  assert.match(requestPayload(b), /acceptEdits/);
});

test("a permission's payload takes mono and a dialog's message takes the UI face", () => {
  // §4's edge: a command line is a literal the machine handed us; a dialog's
  // message is prose written for a human, and prose in mono is a costume.
  assert.equal(payloadIsProse(permission({ command: "ls" })), false);
  const dialog: RequestBlock = {
    ...permission(null),
    channel: "ui",
    tool: null,
    method: "select",
    title: "Which branch?",
    message: "Pick the branch to work on.",
    options: ["fix/a", "fix/b"],
  };
  assert.equal(payloadIsProse(dialog), true);
  assert.equal(requestPayload(dialog), "Pick the branch to work on.");
});

test("a select offers its own options; a confirm and a permission get Allow/Deny", () => {
  const dialog: RequestBlock = {
    ...permission(null),
    channel: "ui",
    method: "select",
    options: ["Allow once", "Always", "Block"],
  };
  assert.deepEqual(actionsFor(dialog).map((a) => a.value), ["Allow once", "Always", "Block"]);
  assert.deepEqual(actionsFor(dialog).map((a) => a.primary), [true, false, false]);

  assert.deepEqual(actionsFor(permission({ command: "ls" })).map((a) => a.value), ["allow", "deny"]);
  assert.equal(actionsFor(permission({ command: "ls" }))[1]!.danger, true);
});

test("an input dialog offers a field, not an Allow button", () => {
  // Offering Allow for a question that wants a string sends an answer the
  // extension cannot read.
  const b: RequestBlock = { ...permission(null), channel: "ui", method: "input", tool: null };
  assert.equal(wantsFreeText(b), true);
  assert.deepEqual(actionsFor(b), []);
  assert.equal(wantsFreeText(permission({ command: "ls" })), false, "a permission is a choice");
});

test("a settled card records the answer AND who decided", () => {
  const s = fixtureState();
  const reqs = s.blocks.filter((b): b is RequestBlock => b.kind === "request");
  assert.equal(settlementLine(reqs[0]!), "deny · by policy");
  assert.equal(settlementLine(reqs[1]!), "fix/2214-resume-model · by human");
  assert.equal(settlementLine({ ...reqs[0]!, settled: null }), null, "pending settles nothing");
});

// ── figures ─────────────────────────────────────────────────────────────────

test("a figure nobody reported is an em dash, never a zero", () => {
  // "Unknown is not a value" (§1.3), on the one surface a human reads it off.
  assert.equal(fmtTok(null), "—");
  assert.equal(fmtTok(undefined), "—");
  assert.equal(fmtUsd(null), "—");
  assert.equal(fmtTok(0), "0", "positive control: a reported zero is still a zero");
  assert.equal(fmtUsd(0), "$0.0000");
});

test("the ticker reads the LATEST usage, never a sum", () => {
  // call_cumulative is already cumulative; adding two reports multiplies a
  // pane's spend by roughly its turn count.
  const s = fixtureState();
  const t = tickerFields(s);
  assert.equal(t.input, "25k", "the second turn's cumulative figure, not 12 300 + 25 000");
  assert.equal(t.cost, "$0.8100");
});

test("the header renders an unknown boot fact as an em dash", () => {
  const blank = headerFacts(emptyState());
  assert.deepEqual(blank, { model: "—", session: "—", capabilities: "—" });
  const known = headerFacts(fixtureState());
  assert.equal(known.model, "anthropic/claude-sonnet-4.5");
  assert.equal(known.session, "0f3c9a2e", "the session is abbreviated, not invented");
});

test("token, duration and byte figures round the way a reader compares them", () => {
  assert.equal(fmtTok(999), "999");
  assert.equal(fmtTok(1500), "1.5k");
  assert.equal(fmtTok(25_000), "25k");
  assert.equal(fmtTok(2_400_000), "2.4M");
  assert.equal(fmtMs(940), "940ms");
  assert.equal(fmtMs(1500), "1.5s");
  assert.equal(fmtMs(null), "");
  assert.equal(fmtBytes(512), "512 bytes");
  assert.equal(fmtBytes(2048), "2.0 KiB");
  assert.equal(fmtBytes(3 * 1024 * 1024), "3.0 MiB");
});

test("a turn's receipt carries what the turn reported and nothing it did not", () => {
  const s = fixtureState();
  const turns = s.blocks.filter((b) => b.kind === "turn");
  assert.equal(turns.length, 2, "positive control: two turns");
  const first = turnReceipt(turns[0]!);
  assert.match(first, /turn 1 ended/);
  assert.match(first, /\$0\.4200/, "the cost it reported");
  assert.match(first, /12k in/, "and its cumulative tokens");

  const silent = turnReceipt({ ...turns[0]!, usage: null, cost: null, durationMs: null });
  assert.equal(silent, "turn 1 ended  ·  completed", "a turn that reported nothing invents nothing");
});

test("both ceilings state their elision rather than logging it", () => {
  // An elision the reader cannot see is a transcript that lies (§7).
  assert.match(evictionNotice(1234), /1,234 earlier blocks/);
  assert.match(evictionNotice(1234), /event log/, "and it says where the rest is");
  assert.match(droppedNotice(2048), /2\.0 KiB elided/);
});

test("a running chip spins and a settled one does not", () => {
  const mk = (status: ToolBlock["status"]): ToolBlock => ({
    id: "b",
    turn: 1,
    kind: "tool",
    toolUseId: "t",
    name: "Bash",
    input: {},
    status,
    output: "",
    outputBytes: 0,
    outputDroppedBytes: 0,
    isError: status === "error",
    durationMs: null,
    orphan: false,
  });
  assert.deepEqual(chipFor(mk("pending")), { status: "pending", label: "queued", spinning: false });
  assert.equal(chipFor(mk("running")).spinning, true, "the one continuous cue a call gets");
  assert.equal(chipFor(mk("ok")).spinning, false);
  assert.equal(chipFor(mk("error")).spinning, false, "a finished call spins nothing");
});

// ── the Edit diff ───────────────────────────────────────────────────────────

test("an Edit renders a real diff of its real arguments", () => {
  const d = diffOf({
    file_path: "src/pane.ts",
    old_string: "a\nb\nOLD\nd\ne",
    new_string: "a\nb\nNEW\nd\ne",
  })!;
  assert.equal(d.file, "src/pane.ts");
  assert.deepEqual(
    d.lines.map((l) => `${l.sign}${l.text}`),
    [" a", " b", "-OLD", "+NEW", " d", " e"],
    "common prefix and suffix as context, the changed middle as -/+",
  );
});

test("a tool with no old/new strings draws no diff at all", () => {
  assert.equal(diffOf({ file_path: "x", pattern: "y" }), null);
  assert.equal(diffOf(null), null);
  assert.ok(diffOf({ old_string: "", new_string: "one line" }), "positive control: a Write does");
});

// ── exhaustiveness ──────────────────────────────────────────────────────────

test("every segment name the row model can produce is one the stylesheet paints", () => {
  // The population control this needs: the assertion above walks the FIXTURE's
  // blocks, and a kind the fixture happens not to carry would ride through it.
  // So enumerate the kinds instead, and check the fixture covers all but the
  // one it structurally cannot.
  const s = fixtureState();
  const seen = new Set(s.blocks.map((b) => b.kind));
  const all = ["text", "thinking", "tool", "delivery", "request", "turn", "notice", "evicted"];
  const missing = all.filter((k) => !seen.has(k as never));
  assert.deepEqual(
    missing,
    ["evicted"],
    "the fixture no longer covers the block catalogue — the segment scan above is narrower than it reads",
  );
  // `evicted` cannot appear in a 28-event fixture (MAX_BLOCKS is 2000), so it
  // is classified directly rather than left uncovered.
  const evicted: Segment = segmentOf({ id: "e", turn: null, kind: "evicted", blocks: 7 });
  assert.equal(evicted, "idle");
});

// ── the decoder: the cast, replaced ─────────────────────────────────────────

test("the decoder refuses every spelling that shipped wrong, and the fixture carries none", () => {
  // THE POSITIVE CONTROL FIRST. A refusal test that never refuses anything is
  // the vacuity this repo has a rule about, so each of the three real defects
  // (#2891 S4) is fed back in and must be REFUSED — with its own assertion, so
  // one of them silently passing cannot hide behind the other two.
  const refused: Array<[string, unknown]> = [
    // The capitalised `UiAnswer` spellings. `rename_all = "snake_case"` renames
    // the VARIANT, so the wire is `{value}` / `{confirmed}` / `"cancelled"`.
    ["ui_settled Value", { kind: "ui_settled", id: "u1", answer: { Value: "x" }, by: "human" }],
    ["ui_settled Confirmed", { kind: "ui_settled", id: "u1", answer: { Confirmed: true }, by: "human" }],
    ["ui_settled Cancelled", { kind: "ui_settled", id: "u1", answer: "Cancelled", by: "human" }],
    // `CompactTrigger` is `manual | auto`.
    ["compacted threshold", { kind: "compacted", trigger: "threshold", pre_tokens: 1 }],
    // And the neighbours of the same class, which nothing had checked either.
    ["note kind", { kind: "note", turn: null, note: "lifecycle", text: "x" }],
    ["ui_request method", { kind: "ui_request", id: "u", method: "Select", options: [] }],
    ["settled by", { kind: "permission_settled", id: "r", decision: "allow", by: "Policy" }],
    ["delivery via", { kind: "delivery", via: "handoff", from: null, text: "x", ts: null }],
  ];
  for (const [what, raw] of refused) {
    assert.throws(
      () => decodeProjectionInput(raw),
      ProjectionDecodeError,
      `${what} was ACCEPTED — the decoder does not refuse the spelling it exists for`,
    );
  }

  // The accepted forms, so "refuse everything" cannot pass either.
  for (const ok of [
    { kind: "ui_settled", id: "u1", answer: { value: "x" }, by: "human" },
    { kind: "ui_settled", id: "u1", answer: { confirmed: false }, by: "policy" },
    { kind: "ui_settled", id: "u1", answer: "cancelled", by: "pane_exited" },
    { kind: "compacted", trigger: "auto", pre_tokens: null },
    { kind: "note", turn: null, note: "retry", text: "x" },
  ]) {
    assert.doesNotThrow(() => decodeProjectionInput(ok), `a valid event was refused: ${JSON.stringify(ok)}`);
  }

  // And the fixture itself, which is what the three defects were hiding in.
  // `readFixture` already decodes, so this asserts the count rather than
  // re-running it — a fixture that shrank to nothing would pass a bare loop.
  assert.equal(readFixture().length, 28, "every one of the fixture's 28 lines decodes");
});

test("an unknown KIND passes through; an unknown payload spelling does not", () => {
  // §1.2's additive rule and the refusal above are different questions, and
  // conflating them would either drop a whole batch from a newer engine or
  // believe a value nothing may emit.
  const future = { kind: "something_the_engine_added_later", v: 1 };
  assert.doesNotThrow(() => decodeProjectionInput(future));
  // And the projection files it rather than throwing — rule 3, end to end.
  const s = project(emptyState(), [decodeProjectionInput(future)]);
  assert.equal(s.unknownEvents, 1, "positive control: the projection saw it and recorded it");
  assert.equal(s.blocks.length, 1);
  assert.equal(s.blocks[0]!.kind, "notice");
});

test("a batch drops what it refuses and keeps the rest", () => {
  // One malformed event must not cost the batch it rode in on: the transcript
  // is what the human is watching, and 63 good events are not collateral.
  const { events, rejected } = decodeBatch([
    { kind: "turn_started", turn: 1 },
    { kind: "compacted", trigger: "threshold", pre_tokens: 1 },
    { kind: "text", turn: 1, delta: "hello" },
  ]);
  assert.equal(events.length, 2, "the two good events survived");
  assert.equal(rejected.length, 1, "positive control: one really was refused");
  assert.equal(rejected[0]!.kind, "compacted");
  assert.equal(rejected[0]!.field, "trigger");
  assert.match(rejected[0]!.message, /threshold/, "the message names the value, not just the field");
});

// ── the parity record ───────────────────────────────────────────────────────

test("the DOM projection draws exactly what the parity record says it does", () => {
  // The TypeScript half of §5.1's parity control. The Rust half is
  // `the_two_projections_diverge_only_where_the_record_says_they_do` in
  // `crates/loomux-engine/src/harness/transcript.rs`, and both read THIS file
  // and the fixture beside it — which is the whole point: a divergence between
  // the two projections of one log is only visible where something compares
  // them, and nothing did before this slice.
  const record = JSON.parse(
    readFileSync(fileURLToPath(new URL("./fixtures/structuredview/parity.json", import.meta.url)), "utf8"),
  ) as {
    events: number;
    local_only: number;
    kinds: Record<string, { dom: boolean; vt: boolean; why?: string }>;
  };

  const raw = readFileSync(FIXTURE, "utf8")
    .split("\n")
    .map((l) => (l.endsWith("\r") ? l.slice(0, -1) : l))
    .filter((l) => l.length > 0);
  assert.equal(raw.length, record.events, "parity.json is dated to a different fixture");

  const state = emptyState();
  const seen = new Set<string>();
  let locals = 0;
  let drewSomething = false;
  for (const line of raw) {
    const ev = decodeProjectionInput(JSON.parse(line));
    if (ev.kind === "delivery") locals += 1;
    // "Did this projection show the human anything about this event" — a block
    // created or updated, or a header/ticker fact set. The same question the
    // Rust half asks of the VT bytes, which is what makes them comparable.
    const before = snapshot(state);
    project(state, [ev], { nowMs: 10_000 });
    const drew = snapshot(state) !== before;
    if (drew) drewSomething = true;
    if (ev.kind === "delivery") continue;

    seen.add(ev.kind);
    const row = record.kinds[ev.kind];
    assert.ok(row, `parity.json has no row for kind "${ev.kind}"`);
    assert.equal(
      drew,
      row.dom,
      `the DOM projection ${drew ? "DRAWS" : "draws nothing"} for "${ev.kind}", ` +
        `and parity.json says it ${row.dom ? "does" : "does not"}`,
    );
  }

  assert.ok(drewSomething, "positive control: nothing drew at all, so the projection never ran");
  assert.equal(locals, record.local_only, "the local-event exemption grew without being written down");
  assert.equal(
    seen.size,
    16,
    `the fixture covers ${seen.size} of the 17 HarnessEvent kinds; it must cover 16 (all but ` +
      "`observed`, which is PTY-only), or the scan above is narrower than it reads",
  );
  assert.equal(
    Object.keys(record.kinds).length,
    seen.size,
    "parity.json describes kinds the fixture never exercises, so those rows assert nothing",
  );
});

test("every divergence in the parity record carries an argument", () => {
  // A divergence between the two projections is allowed — the VT renderer
  // draws nothing for thinking, for tool output, or for a turn boundary — but an UNARGUED
  // one is exactly the drift §5.1 warns about. Both halves check this; a reason
  // that only one side demanded is a reason the other side could delete.
  const record = JSON.parse(
    readFileSync(fileURLToPath(new URL("./fixtures/structuredview/parity.json", import.meta.url)), "utf8"),
  ) as { kinds: Record<string, { dom: boolean; vt: boolean; why?: string }> };

  const diverging = Object.entries(record.kinds).filter(([, r]) => r.dom !== r.vt);
  assert.deepEqual(
    diverging.map(([k]) => k).sort(),
    ["thinking", "tool_output", "turn_started"],
    "the set of divergences moved — that is a design change, not a test fix",
  );
  for (const [kind, row] of diverging) {
    assert.ok(
      (row.why ?? "").length > 40,
      `"${kind}" diverges and parity.json gives no reason worth the name`,
    );
  }
});

/** Everything about `State` a reader can see, as a comparable string. Used only
 *  to answer "did this event change anything", which is what the parity record
 *  means by `dom`. */
function snapshot(s: State): string {
  return JSON.stringify({
    blocks: s.blocks,
    session: s.session,
    model: s.model,
    capabilities: s.capabilities,
    usage: s.usage,
    cost: s.cost,
    steering: s.steering,
    followUp: s.followUp,
    currentTurn: s.currentTurn,
    exitCode: s.exitCode,
    exited: s.exited,
    evicted: s.evicted,
    droppedBytes: s.droppedBytes,
    unknownEvents: s.unknownEvents,
  });
}
