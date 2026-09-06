// Tests for the structured pane's projection (#2891 S2).
//
// The properties under test are the ones a renderer can break silently: a tool
// card that never joins its result, thinking that leaks into the assistant's
// voice, a ceiling that drops without saying so, a fold that lands on the
// wrong card after a re-projection, and a usage ticker that ADDS two already
// cumulative figures. Each has an assertion that fails when the property goes,
// and every absence-only assertion carries a positive control.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import {
  MAX_BLOCKS,
  MAX_TEXT_BYTES_PER_BLOCK,
  emptyState,
  emptyViewState,
  isCollapsed,
  project,
  pruneViewState,
  textTail,
  toggleCollapsed,
  utf8Bytes,
} from "../src/structuredview.ts";
import type {
  Block,
  ProjectionInput,
  State,
  ThinkingBlock,
  TextBlock,
  ToolBlock,
  Tokens,
  Usage,
} from "../src/structuredview.ts";

// ── helpers ─────────────────────────────────────────────────────────────────

function run(batch: ProjectionInput[], opts?: { nowMs?: number }): State {
  return project(emptyState(), batch, opts);
}

function kinds(state: State): string[] {
  return state.blocks.map((b) => b.kind);
}

function only<K extends Block["kind"]>(state: State, kind: K): Array<Extract<Block, { kind: K }>> {
  return state.blocks.filter((b): b is Extract<Block, { kind: K }> => b.kind === kind);
}

function tokens(input: number, output: number): Tokens {
  return { input, output, cache_read: 0, cache_creation: 0 };
}

function usage(input: number, output: number): Usage {
  return { call_cumulative: tokens(input, output), this_turn_main_loop: null, per_model: [] };
}

// ── the tool lifecycle: one card, not three rows ─────────────────────────────

test("a call, its streamed output and its result join into ONE card", () => {
  const s = run(
    [
      { kind: "turn_started", turn: 1 },
      { kind: "tool_call", turn: 1, id: "t1", name: "Bash", input: { command: "ls" } },
      { kind: "tool_output", turn: 1, id: "t1", delta: "a.txt\n", is_error: false },
      { kind: "tool_output", turn: 1, id: "t1", delta: "b.txt\n", is_error: false },
      { kind: "tool_result", turn: 1, id: "t1", ok: true },
    ],
    { nowMs: 1000 },
  );

  const cards = only(s, "tool");
  assert.equal(cards.length, 1, "one ToolUseId is one card");
  const card = cards[0]!;
  assert.equal(card.name, "Bash");
  assert.equal(card.status, "ok");
  assert.equal(card.isError, false);
  // The DELTA rule (§1.2): output APPENDS. If this module ever starts
  // replacing, the second delta would win alone and this reads "b.txt\n".
  assert.equal(card.output, "a.txt\nb.txt\n");
});

test("status walks pending -> running -> ok, and a failure lands as error", () => {
  const s = emptyState();
  project(s, [{ kind: "tool_call", turn: 1, id: "t1", name: "Bash", input: {} }]);
  assert.equal(only(s, "tool")[0]!.status, "pending", "a call with no output yet is pending");

  project(s, [{ kind: "tool_output", turn: 1, id: "t1", delta: "…", is_error: false }]);
  assert.equal(only(s, "tool")[0]!.status, "running");

  project(s, [{ kind: "tool_result", turn: 1, id: "t1", ok: false }]);
  const card = only(s, "tool")[0]!;
  assert.equal(card.status, "error");
  assert.equal(card.isError, true);
});

test("is_error on the OUTPUT is remembered even when the result says ok", () => {
  // pi carries `isError` on `tool_execution_end`'s output; a card that read the
  // verdict only off `ToolResult` would draw a clean card over an error body.
  const s = run([
    { kind: "tool_call", turn: 1, id: "t1", name: "Bash", input: {} },
    { kind: "tool_output", turn: 1, id: "t1", delta: "permission denied", is_error: true },
    { kind: "tool_result", turn: 1, id: "t1", ok: true },
  ]);
  const card = only(s, "tool")[0]!;
  assert.equal(card.isError, true);
  assert.equal(card.status, "ok", "the verdict is still the result's to give");
});

test("duration comes from the caller's clock, and is null without one", () => {
  const s = emptyState();
  project(s, [{ kind: "tool_call", turn: 1, id: "t1", name: "Bash", input: {} }], { nowMs: 1_000 });
  project(s, [{ kind: "tool_result", turn: 1, id: "t1", ok: true }], { nowMs: 1_450 });
  assert.equal(only(s, "tool")[0]!.durationMs, 450);

  // No clock -> no invented figure. `null` is the only honest answer, and the
  // renderer draws nothing rather than "0 ms".
  const t = run([
    { kind: "tool_call", turn: 1, id: "t2", name: "Bash", input: {} },
    { kind: "tool_result", turn: 1, id: "t2", ok: true },
  ]);
  assert.equal(only(t, "tool")[0]!.durationMs, null);
});

test("replaces:true SUPERSEDES the held output instead of appending it", () => {
  // The non-prefix restatement. Without the flag a consumer cannot tell this
  // from an ordinary delta, and the card shows the output twice — which is
  // exactly why the adapter marks it rather than leaving it to be inferred.
  const s = run([
    { kind: "tool_call", turn: 1, id: "t1", name: "Bash", input: {} },
    { kind: "tool_output", turn: 1, id: "t1", delta: "line one\n", is_error: false, replaces: false },
    { kind: "tool_output", turn: 1, id: "t1", delta: "line two\n", is_error: false, replaces: false },
    { kind: "tool_output", turn: 1, id: "t1", delta: "THE WHOLE VALUE\n", is_error: false, replaces: true },
  ]);
  const card = only(s, "tool")[0]!;
  assert.equal(card.output, "THE WHOLE VALUE\n");
  assert.equal(card.outputBytes, utf8Bytes("THE WHOLE VALUE\n"));
  // The duplication the field exists to prevent, named so the assertion above
  // is read as the thing it is testing.
  assert.doesNotMatch(card.output, /line one/, "the superseded value is gone, not prefixed");
  assert.equal(card.output.includes("THE WHOLE VALUE"), true);
});

test("replaces defaults to APPEND — absent and false behave identically", () => {
  // The additive promise: a producer that never sets the field, and one that
  // sets it false, must land in the same place. Reading `undefined` as truthy
  // anywhere here would silently erase output for every pre-field harness.
  const batch = (replaces: boolean | undefined): ProjectionInput[] => [
    { kind: "tool_call", turn: 1, id: "t1", name: "Bash", input: {} },
    { kind: "tool_output", turn: 1, id: "t1", delta: "aaa", is_error: false, replaces: false },
    ...(replaces === undefined
      ? [{ kind: "tool_output" as const, turn: 1, id: "t1", delta: "bbb", is_error: false }]
      : [{ kind: "tool_output" as const, turn: 1, id: "t1", delta: "bbb", is_error: false, replaces }]),
  ];
  const absent = only(run(batch(undefined)), "tool")[0]!;
  const explicit = only(run(batch(false)), "tool")[0]!;
  assert.equal(absent.output, "aaabbb");
  assert.equal(explicit.output, "aaabbb");
  // Positive control: the flag DOES do something on this same fixture, so the
  // equality above is not two spellings of an inert field.
  const replaced = only(run(batch(true)), "tool")[0]!;
  assert.equal(replaced.output, "bbb");
});

test("a replaces:true on the FIRST output has nothing to supersede", () => {
  const s = run([
    { kind: "tool_call", turn: 1, id: "t1", name: "Bash", input: {} },
    { kind: "tool_output", turn: 1, id: "t1", delta: "whole", is_error: false, replaces: true },
  ]);
  const card = only(s, "tool")[0]!;
  assert.equal(card.output, "whole");
  assert.equal(card.outputDroppedBytes, 0);
});

test("a supersede resets the card's dropped-byte figure but not the pane's", () => {
  // The two counters answer different questions: the card's describes what was
  // trimmed from the value it is SHOWING (and that value is gone), the pane's
  // is a session-lifetime total. Rewinding the second would make a monotonic
  // counter go backwards.
  const chunk = "z".repeat(64 * 1024);
  const s = emptyState();
  project(s, [{ kind: "tool_call", turn: 1, id: "t1", name: "Bash", input: {} }]);
  for (let i = 0; i < 5; i += 1) {
    project(s, [{ kind: "tool_output", turn: 1, id: "t1", delta: chunk, is_error: false, replaces: false }]);
  }
  const before = only(s, "tool")[0]!;
  assert.ok(before.outputDroppedBytes > 0, "positive control: the ceiling really fired first");
  const paneTotal = s.droppedBytes;
  assert.ok(paneTotal > 0);

  project(s, [{ kind: "tool_output", turn: 1, id: "t1", delta: "fresh", is_error: false, replaces: true }]);
  const after = only(s, "tool")[0]!;
  assert.equal(after.output, "fresh");
  assert.equal(after.outputDroppedBytes, 0, "the card is showing an untrimmed value");
  assert.equal(s.droppedBytes, paneTotal, "the pane's session total does not rewind");
});

test("an orphan ToolResult does not throw, and its name is null rather than a sentinel", () => {
  const s = run([{ kind: "tool_result", turn: 1, id: "ghost", ok: true }]);
  const cards = only(s, "tool");
  assert.equal(cards.length, 1);
  assert.equal(cards[0]!.orphan, true);
  assert.equal(cards[0]!.name, null, "unknown is not a value (§1.3 rule 1)");
  assert.notEqual(cards[0]!.name, "unknown");
  assert.equal(cards[0]!.status, "ok");
});

test("an orphan ToolOutput opens the card the later ToolCall then fills in", () => {
  // A batch that starts mid-session sees output before the call it belongs to.
  // Two cards for one id would be the defect; the join key is the id.
  const s = run([
    { kind: "tool_output", turn: 1, id: "t1", delta: "early", is_error: false },
    { kind: "tool_call", turn: 1, id: "t1", name: "Read", input: { file: "x" } },
    { kind: "tool_result", turn: 1, id: "t1", ok: true },
  ]);
  const cards = only(s, "tool");
  assert.equal(cards.length, 1);
  assert.equal(cards[0]!.name, "Read");
  assert.equal(cards[0]!.orphan, false, "the call that arrived late still de-orphans it");
  assert.equal(cards[0]!.output, "early");
});

// ── thinking is not text ────────────────────────────────────────────────────

test("thinking never lands in a text block, in either interleaving", () => {
  const s = run([
    { kind: "turn_started", turn: 1 },
    { kind: "thinking", turn: 1, delta: "the header reads " },
    { kind: "text", turn: 1, delta: "Looking at " },
    { kind: "thinking", turn: 1, delta: "model off the roster" },
    { kind: "text", turn: 1, delta: "the resume path" },
  ]);

  const text = only(s, "text").map((b) => b.text).join("");
  const thinking = only(s, "thinking").map((b) => b.text).join("");
  assert.equal(text, "Looking at the resume path");
  assert.equal(thinking, "the header reads model off the roster");

  // The absence assertion, with its positive control: BOTH kinds really were
  // projected, so "no thinking in the text" is not a statement about an empty
  // projection.
  assert.ok(only(s, "text").length > 0, "positive control: text blocks exist");
  assert.ok(only(s, "thinking").length > 0, "positive control: thinking blocks exist");
  for (const b of only(s, "text")) assert.ok(!b.text.includes("roster"), "no thinking in text");
  for (const b of only(s, "thinking")) assert.ok(!b.text.includes("Looking"), "no text in thinking");
});

test("consecutive deltas of one kind stream into ONE block, not one per delta", () => {
  const s = run([
    { kind: "text", turn: 1, delta: "a" },
    { kind: "text", turn: 1, delta: "b" },
    { kind: "text", turn: 1, delta: "c" },
  ]);
  assert.equal(only(s, "text").length, 1);
  assert.equal(only(s, "text")[0]!.text, "abc");
});

test("an ORPHAN card interrupts the text run too — the reconnect case", () => {
  // Round 1 blocking finding 3. Only the `tool_call` arm closed the run, so a
  // card created by `tool_output` left it open and text arriving AFTER the card
  // appended to the paragraph above it. This is the realistic path, not a
  // corner: a client attaching mid-session replays a rotated per-pane log and
  // rejoins mid-tool, so `tool_output` with no `tool_call` is the normal FIRST
  // event for that card.
  const fromOutput = run([
    { kind: "text", turn: 1, delta: "before " },
    { kind: "tool_output", turn: 1, id: "t1", delta: "OUT", is_error: false },
    { kind: "text", turn: 1, delta: "after" },
  ]);
  assert.deepEqual(kinds(fromOutput), ["text", "tool", "text"]);
  assert.deepEqual(only(fromOutput, "text").map((b) => b.text), ["before ", "after"]);
  assert.notEqual(only(fromOutput, "text")[0]!.text, "before after", "the defect this pins");

  // Same for a card created by an orphan result, which takes the other arm.
  const fromResult = run([
    { kind: "text", turn: 1, delta: "before " },
    { kind: "tool_result", turn: 1, id: "t9", ok: true },
    { kind: "text", turn: 1, delta: "after" },
  ]);
  assert.deepEqual(kinds(fromResult), ["text", "tool", "text"]);

  // And thinking, which is the other run a card has to interrupt.
  const think = run([
    { kind: "thinking", turn: 1, delta: "before " },
    { kind: "tool_output", turn: 1, id: "t1", delta: "OUT", is_error: false },
    { kind: "thinking", turn: 1, delta: "after" },
  ]);
  assert.deepEqual(kinds(think), ["thinking", "tool", "thinking"]);
});

test("a tool card interrupts the text run, so the next delta is a new block", () => {
  // Appending after the card would put the paragraph's second half ABOVE the
  // card that arrived between its halves.
  const s = run([
    { kind: "text", turn: 1, delta: "before" },
    { kind: "tool_call", turn: 1, id: "t1", name: "Read", input: {} },
    { kind: "text", turn: 1, delta: "after" },
  ]);
  assert.deepEqual(kinds(s), ["text", "tool", "text"]);
});

// ── turns and the usage ticker ──────────────────────────────────────────────

test("a turn is one block, closed by its TurnEnded receipt", () => {
  const s = run(
    [
      { kind: "turn_started", turn: 1 },
      { kind: "text", turn: 1, delta: "hi" },
      {
        kind: "turn_ended",
        turn: 1,
        usage: usage(100, 10),
        cost: { usd: 0.42, basis: "harness_estimate" },
        stop: "completed",
      },
    ],
    { nowMs: 5_000 },
  );
  const turns = only(s, "turn");
  assert.equal(turns.length, 1, "one turn is one rule, not a start rule and an end rule");
  assert.equal(turns[0]!.ended, true);
  assert.equal(turns[0]!.stop, "completed");
  assert.equal(turns[0]!.cost?.usd, 0.42);
});

test("usage is the LATEST report, never the sum of the reports", () => {
  // `call_cumulative` is already cumulative. Adding two turns' figures would
  // multiply a pane's spend by roughly its turn count — the exact failure the
  // field's doc comment warns about.
  const s = run([
    { kind: "turn_started", turn: 1 },
    { kind: "turn_ended", turn: 1, usage: usage(12_300, 1_100), cost: { usd: 0.42, basis: "harness_estimate" }, stop: "completed" },
    { kind: "turn_started", turn: 2 },
    { kind: "turn_ended", turn: 2, usage: usage(25_000, 2_400), cost: { usd: 0.81, basis: "harness_estimate" }, stop: "completed" },
  ]);
  assert.equal(s.usage?.call_cumulative.input, 25_000);
  assert.equal(s.usage?.call_cumulative.output, 2_400);
  assert.notEqual(s.usage?.call_cumulative.input, 37_300, "12300 + 25000 is the defect");
  assert.equal(s.cost?.usd, 0.81);
  assert.notEqual(s.cost?.usd, 1.23, "0.42 + 0.81 is the defect");
});

test("a turn that reports no usage leaves the last real figure standing", () => {
  // The bounded "missing stats" case. Zeroing the ticker would report a fact
  // the pane does not have as the number 0.
  const s = run([
    { kind: "turn_started", turn: 1 },
    { kind: "turn_ended", turn: 1, usage: usage(500, 50), cost: null, stop: "completed" },
    { kind: "turn_started", turn: 2 },
    { kind: "turn_ended", turn: 2, usage: null, cost: null, stop: "aborted" },
  ]);
  assert.equal(s.usage?.call_cumulative.input, 500);
  assert.equal(only(s, "turn")[1]!.usage, null, "the turn's own receipt still says it had none");
});

// ── the other blocks ────────────────────────────────────────────────────────

test("a delivery is its own block and carries which of Turn's four it was", () => {
  const s = run([
    { kind: "delivery", via: "kickoff", from: "orchestrator", text: "do the thing", ts: "09:14" },
    { kind: "delivery", via: "human", from: null, text: "actually, stop", ts: null },
  ]);
  const d = only(s, "delivery");
  assert.deepEqual(d.map((b) => b.via), ["kickoff", "human"]);
  assert.equal(d[0]!.from, "orchestrator");
  assert.equal(d[1]!.from, null, "unknown is not a value");
});

test("a permission and a UI request are separate channels on one card shape", () => {
  const s = run([
    { kind: "permission_request", id: "r1", tool: "Bash", input: { command: "git push --force" } },
    { kind: "permission_settled", id: "r1", decision: "deny", by: "policy" },
    { kind: "ui_request", id: "u1", method: "select", title: "Which?", message: null, options: ["a", "b"], timeout_ms: 60_000 },
  ]);
  const r = only(s, "request");
  assert.equal(r.length, 2, "settling joins onto the request's own card");
  assert.equal(r[0]!.channel, "permission");
  assert.deepEqual(r[0]!.settled, { answer: "deny", by: "policy" });
  assert.equal(r[1]!.channel, "ui");
  assert.equal(r[1]!.settled, null, "a pending request is what makes the pane want a human");
  assert.equal(r[1]!.timeoutMs, 60_000);
});

test("QueueChanged is state, not a block — an empty queue is not news", () => {
  const s = run([
    { kind: "queue_changed", steering: [], follow_up: ["review the fix"] },
  ]);
  assert.deepEqual(s.followUp, ["review the fix"]);
  assert.deepEqual(s.steering, []);
  assert.equal(s.blocks.length, 0, "no row is drawn for a queue update");
});

test("a Note keeps its own turn, and a null one is NOT bucketed into the open turn", () => {
  // §1.3: a fact the pane does not have is null. A retry begins before a turn
  // reopens and an extension can throw at boot, so `turn: null` is a real
  // answer — and substituting `state.currentTurn` would invent the attribution
  // in the very field a renderer groups by.
  const s = run([
    { kind: "turn_started", turn: 7 },
    { kind: "note", turn: null, note: "retry", text: "upstream 529 — retry 1/5" },
    { kind: "note", turn: 7, note: "error", text: "retries exhausted" },
  ]);
  const notes = only(s, "notice");
  assert.equal(notes.length, 2);
  assert.equal(notes[0]!.turn, null, "the harness said no turn, so the block says no turn");
  assert.notEqual(notes[0]!.turn, 7, "the open turn is NOT substituted");
  assert.notEqual(notes[0]!.turn, 0, "and it is not bucketed into turn 0 either");
  assert.equal(notes[1]!.turn, 7, "positive control: an attributed note keeps its turn");
});

test("the three NoteKinds stay distinguishable, which level alone cannot do", () => {
  // Each is meant to be drawn differently, and `noteKind` is what a renderer
  // separates them by: `level` collapses a harness `ui` note into the same
  // bucket as orrerix's own compaction row, and they are not the same thing.
  const s = run([
    { kind: "note", turn: 1, note: "retry", text: "retrying" },
    { kind: "note", turn: 1, note: "error", text: "gave up" },
    { kind: "note", turn: 1, note: "ui", text: "indexing…" },
    { kind: "compacted", trigger: "auto", pre_tokens: 100 },
  ]);
  const notes = only(s, "notice");
  assert.deepEqual(notes.map((b) => b.noteKind), ["retry", "error", "ui", null]);
  assert.deepEqual(notes.map((b) => b.level), ["warn", "error", "info", "info"]);
  // The point of carrying both: the last two share a level and must not share
  // an identity.
  assert.equal(notes[2]!.level, notes[3]!.level);
  assert.notEqual(notes[2]!.noteKind, notes[3]!.noteKind);
  assert.equal(notes[3]!.noteKind, null, "orrerix's own row is not a harness note");
});

test("an unknown event kind is recorded as a note, never thrown on", () => {
  const s = emptyState();
  assert.doesNotThrow(() => {
    project(s, [{ kind: "future_variant_from_a_later_slice" } as unknown as ProjectionInput]);
  });
  assert.equal(s.unknownEvents, 1, "positive control: the unknown branch really ran");
  assert.equal(only(s, "notice")[0]!.text, "future_variant_from_a_later_slice");
});

test("Observed evidence is drawn as a note, never promoted to a request", () => {
  // §1.3 rule 2: a scraped fact and a reported one must not share a shape.
  const s = run([{ kind: "observed", observed: "question_suspected", matched: "1) yes  2) no" }]);
  assert.equal(only(s, "notice").length, 1);
  assert.equal(only(s, "request").length, 0, "a grid heuristic is not a UiRequest");
});

// ── the two ceilings, both visible ──────────────────────────────────────────

test("MAX_BLOCKS eviction fires, is counted, and leaves a VISIBLE sentinel", () => {
  const batch: ProjectionInput[] = [];
  for (let i = 0; i < MAX_BLOCKS + 50; i += 1) {
    batch.push({ kind: "note", turn: null, note: "ui", text: `n${i}` });
  }
  const s = run(batch);

  // The positive control the brief names: without it, every assertion below
  // passes just as well on a projection where eviction never ran.
  assert.ok(s.evicted > 0, "positive control: eviction really fired");
  assert.equal(s.evicted, 51, "2050 blocks, ceiling 2000, one slot for the sentinel");
  assert.equal(s.blocks.length, MAX_BLOCKS);
  assert.equal(s.blocks[0]!.kind, "evicted", "the sentinel is a block, at the head");
  assert.equal((s.blocks[0] as Extract<Block, { kind: "evicted" }>).blocks, 51);
  // An elision the reader cannot see is a transcript that lies: the count is a
  // rendered line, not only a counter. The sentinel sits at the HEAD, so a
  // thumbnail of the live end correctly does not carry it — a tail long enough
  // to reach the head does.
  assert.match(textTail(s, 200_000), /51 earlier blocks rolled out/);
  assert.doesNotMatch(textTail(s, 40), /rolled out/, "a thumbnail shows the live end, not the head");
});

test("the sentinel accumulates across batches and is never itself evicted", () => {
  const s = emptyState();
  const push = (n: number) => {
    const batch: ProjectionInput[] = [];
    for (let i = 0; i < n; i += 1) batch.push({ kind: "note", turn: null, note: "ui", text: "x" });
    project(s, batch);
  };
  push(MAX_BLOCKS + 10);
  push(20);
  assert.equal(s.blocks.filter((b) => b.kind === "evicted").length, 1, "one sentinel, not one per batch");
  assert.equal(s.blocks[0]!.kind, "evicted");
  assert.equal((s.blocks[0] as Extract<Block, { kind: "evicted" }>).blocks, s.evicted);
  assert.equal(s.evicted, 31);
  assert.equal(s.blocks.length, MAX_BLOCKS);
});

test("MAX_TEXT_BYTES_PER_BLOCK drops from the head and states the byte count", () => {
  const chunk = "x".repeat(64 * 1024);
  const s = emptyState();
  for (let i = 0; i < 5; i += 1) project(s, [{ kind: "text", turn: 1, delta: chunk }]);
  const b = only(s, "text")[0]! as TextBlock;

  assert.ok(s.droppedBytes > 0, "positive control: the per-block ceiling really fired");
  assert.equal(b.bytes, MAX_TEXT_BYTES_PER_BLOCK);
  assert.equal(b.text.length, MAX_TEXT_BYTES_PER_BLOCK, "ASCII: one byte, one char");
  assert.equal(b.droppedBytes, 64 * 1024, "5 * 64 KiB minus the 256 KiB kept");
  assert.equal(b.droppedBytes, s.droppedBytes);
});

test("the same ceiling applies to a tool card's output, separately per card", () => {
  const chunk = "y".repeat(64 * 1024);
  const s = emptyState();
  project(s, [
    { kind: "tool_call", turn: 1, id: "t1", name: "Bash", input: {} },
    { kind: "tool_call", turn: 1, id: "t2", name: "Bash", input: {} },
  ]);
  for (let i = 0; i < 5; i += 1) {
    project(s, [{ kind: "tool_output", turn: 1, id: "t1", delta: chunk, is_error: false }]);
  }
  project(s, [{ kind: "tool_output", turn: 1, id: "t2", delta: "small", is_error: false }]);

  const [a, b] = only(s, "tool") as [ToolBlock, ToolBlock];
  assert.equal(a.outputBytes, MAX_TEXT_BYTES_PER_BLOCK);
  assert.equal(a.outputDroppedBytes, 64 * 1024);
  assert.equal(b.outputBytes, 5, "the ceiling is per card, not per pane");
  assert.equal(b.outputDroppedBytes, 0);
});

test("a head trim never splits a surrogate pair", () => {
  // A cut between the halves leaves a lone surrogate, which is not a character
  // and renders as a replacement glyph. Emoji are 4 UTF-8 bytes / 2 UTF-16
  // code units, so a naive char-count trim lands mid-pair about half the time.
  const emoji = "\u{1F600}"; // one astral code point
  assert.equal(utf8Bytes(emoji), 4);
  const s = emptyState();
  const chunk = emoji.repeat(16 * 1024); // 64 KiB
  for (let i = 0; i < 5; i += 1) project(s, [{ kind: "text", turn: 1, delta: chunk }]);
  const b = only(s, "text")[0]! as TextBlock;

  assert.ok(s.droppedBytes > 0, "positive control: the trim really ran");
  assert.ok(b.bytes <= MAX_TEXT_BYTES_PER_BLOCK);
  for (let i = 0; i < b.text.length; i += 1) {
    const c = b.text.charCodeAt(i);
    if (c >= 0xdc00 && c <= 0xdfff) {
      const prev = i > 0 ? b.text.charCodeAt(i - 1) : 0;
      assert.ok(prev >= 0xd800 && prev <= 0xdbff, `lone low surrogate at ${i}`);
    }
  }
  assert.equal(b.text, [...b.text].join(""), "every code unit is part of a whole code point");
});

test("the head trim agrees with utf8Bytes on the kept tail", () => {
  // Round 1 blocking finding 4 replaced a full re-measure of the kept tail
  // (`utf8Bytes(kept)` on every trim — O(block) once saturated) with widths
  // accumulated in the loop that was already computing them. Identical
  // arithmetic ONLY if the loop's width rules match `utf8Bytes` exactly, so
  // that equality is pinned rather than asserted in a comment — across ASCII,
  // 2-byte, 3-byte, astral and LONE-surrogate text, which is where the two
  // could disagree.
  const enc = new TextEncoder();
  const corpus = [
    "x".repeat(300 * 1024),
    "é".repeat(200 * 1024),
    "€".repeat(150 * 1024),
    "\u{1F600}".repeat(100 * 1024),
    "a\u{1F600}é€".repeat(60 * 1024),
    "\ud800" + "b".repeat(300 * 1024), // lone high surrogate at the head
    "c".repeat(300 * 1024) + "\udc00", // lone low surrogate at the tail
  ];
  for (const text of corpus) {
    const s = emptyState();
    project(s, [{ kind: "text", turn: 1, delta: text }]);
    const b = only(s, "text")[0]!;
    assert.ok(b.bytes <= MAX_TEXT_BYTES_PER_BLOCK, "the ceiling held");
    // The block's own bookkeeping must equal an independent measurement of the
    // string it is actually holding.
    assert.equal(b.bytes, enc.encode(b.text).length, "bytes match the kept text");
    assert.equal(b.bytes, utf8Bytes(b.text));
    assert.equal(
      b.droppedBytes,
      enc.encode(text).length - enc.encode(b.text).length,
      "dropped is exactly what left",
    );
    assert.equal(b.text, text.slice(text.length - b.text.length), "a pure HEAD trim");
  }
  // Positive control: the ceiling really fired on every one of those.
  for (const text of corpus) assert.ok(enc.encode(text).length > MAX_TEXT_BYTES_PER_BLOCK);
});

test("a saturated block's trim cost scales with the DELTA, not the block", () => {
  // The shape of round 1's finding 4, pinned DETERMINISTICALLY. A wall-clock
  // ratio was the obvious test and it flaked: the first run of it under a
  // loaded box reddened the mutation harness's own control row, which would
  // have made every row in that table unattributable. So this counts the work
  // instead of timing it — `charCodeAt` is the inner operation of both
  // `utf8Bytes` and the trim loop, so its call count IS the cost model, and it
  // is identical on every machine.
  //
  // Pre-fix, `trimHead` ended `utf8Bytes(kept)`, re-walking up to
  // MAX_TEXT_BYTES_PER_BLOCK characters on every trim — and once a block is
  // saturated, every delta trims. Post-fix the loop accumulates the widths it
  // was already computing, so the count is proportional to what ARRIVED.
  const TOTAL = 4 * 1024 * 1024;
  const DELTA = 4 * 1024;
  const real = String.prototype.charCodeAt;
  let calls = 0;
  // eslint-disable-next-line no-extend-native
  String.prototype.charCodeAt = function (i: number): number {
    calls += 1;
    return real.call(this, i);
  };
  let saturatedBytes = 0;
  try {
    const chunk = "x".repeat(DELTA);
    const s = emptyState();
    project(s, [{ kind: "tool_call", turn: 1, id: "t1", name: "Bash", input: {} }]);
    for (let sent = 0; sent < TOTAL; sent += DELTA) {
      project(s, [{ kind: "tool_output", turn: 1, id: "t1", delta: chunk, is_error: false }]);
    }
    saturatedBytes = only(s, "tool")[0]!.outputBytes;
  } finally {
    String.prototype.charCodeAt = real;
  }

  assert.equal(saturatedBytes, MAX_TEXT_BYTES_PER_BLOCK, "positive control: the block really saturated");
  assert.ok(calls > 0, "positive control: the counter really observed the work");

  // Every arriving character is measured once by `utf8Bytes`, and each trim
  // walks only the characters it drops — so the total is a small multiple of
  // what arrived. The re-measuring version does ~`trims x 256Ki` on top:
  // (4 MiB / 4 KiB) x 256Ki = about 268M extra, two orders above this bound.
  const arrived = TOTAL;
  assert.ok(
    calls < arrived * 3,
    `charCodeAt calls ${calls} vs ${arrived} bytes arrived — trim is re-reading the block`,
  );
});

test("utf8Bytes agrees with TextEncoder on the shapes the trim cares about", () => {
  // The instrument the two ceilings measure with. A hand-rolled counter that
  // disagreed with reality would make every byte figure above a fiction.
  const enc = new TextEncoder();
  for (const s of ["", "abc", "é", "€", "\u{1F600}", "a\u{1F600}bé", "\ud800"]) {
    assert.equal(utf8Bytes(s), enc.encode(s).length, JSON.stringify(s));
  }
});

// ── the view state, and why it is not on the blocks ─────────────────────────

test("block ids are stable, so a collapse survives a re-projection", () => {
  const batch: ProjectionInput[] = [
    { kind: "turn_started", turn: 1 },
    { kind: "text", turn: 1, delta: "hello" },
    { kind: "tool_call", turn: 1, id: "t1", name: "Bash", input: {} },
    { kind: "tool_result", turn: 1, id: "t1", ok: true },
  ];

  const first = run(batch);
  const card = only(first, "tool")[0]!;
  const view = emptyViewState();
  toggleCollapsed(view, card.id);
  assert.ok(isCollapsed(view, card));

  // Re-project from scratch over the SAME events — what a client does on
  // reconnect, and what the renderer does when it rebuilds. The human's fold
  // must land on the same card.
  const second = run(batch);
  const again = only(second, "tool")[0]!;
  assert.equal(again.id, card.id, "the same event sequence yields the same ids");
  assert.ok(isCollapsed(view, again), "the fold survived the re-projection");
  assert.deepEqual(kinds(first), kinds(second));
});

test("a fold is keyed by id, not by position, so an eviction cannot move it", () => {
  // The failure this pins: with an index key, evicting 51 blocks shifts every
  // remaining block down and the human's fold silently lands on a stranger.
  const s = emptyState();
  const notes = (n: number): ProjectionInput[] =>
    Array.from({ length: n }, () => ({ kind: "note", turn: null, note: "ui", text: "x" }) as const);

  // 100 blocks of history, then the card the human folds, then enough traffic
  // to evict some of that history but not the card itself.
  project(s, notes(100));
  project(s, [{ kind: "tool_call", turn: 1, id: "keep", name: "Bash", input: {} }]);
  const card = only(s, "tool")[0]!;
  const positionBefore = s.blocks.indexOf(card);
  assert.equal(positionBefore, 100);
  const view = emptyViewState();
  toggleCollapsed(view, card.id);

  project(s, notes(1_990));

  assert.ok(s.evicted > 0, "positive control: an eviction really happened");
  assert.ok(s.evicted < 100, "and it stopped short of the card, which is what makes the next line a test");
  const still = only(s, "tool").find((b) => b.toolUseId === "keep");
  assert.ok(still, "the card survived the eviction");
  assert.notEqual(s.blocks.indexOf(still!), positionBefore, "and it MOVED, which is the point");
  assert.ok(isCollapsed(view, still!), "the fold moved with it");
});

test("ids stay UNIQUE across an eviction, so a new block cannot inherit a live fold", () => {
  // This is what the counter buys over the obvious alternative. Keying an id on
  // the block's position (`blocks.length` at creation) is indistinguishable
  // until an eviction shortens the list — after which the NEXT block created is
  // handed an id a surviving block already holds, and the human's fold on the
  // survivor silently appears on a stranger. Nothing throws; the ids simply
  // collide.
  const s = emptyState();
  const notes = (n: number): ProjectionInput[] =>
    Array.from({ length: n }, () => ({ kind: "note", turn: null, note: "ui", text: "x" }) as const);

  project(s, notes(MAX_BLOCKS + 100));
  assert.ok(s.evicted > 0, "positive control: an eviction really happened");

  const before = s.blocks.map((b) => b.id);
  assert.equal(new Set(before).size, before.length, "no two live blocks share an id");

  // Fold every survivor, then keep projecting. A fresh block must not arrive
  // already folded.
  const view = emptyViewState();
  for (const id of before) toggleCollapsed(view, id);
  project(s, [{ kind: "text", turn: 1, delta: "brand new" }]);
  const fresh = only(s, "text").at(-1)!;
  assert.equal(before.includes(fresh.id), false, "the new block's id is not a recycled one");
  assert.equal(isCollapsed(view, fresh), false, "so it did not inherit a stranger's fold");
  const after = s.blocks.map((b) => b.id);
  assert.equal(new Set(after).size, after.length, "and the list is still collision-free");
});

test("pruneViewState drops folds for blocks that are gone, and keeps live ones", () => {
  const s = emptyState();
  project(s, [{ kind: "note", turn: null, note: "ui", text: "first" }]);
  const doomed = s.blocks[0]!.id;
  const view = emptyViewState();
  toggleCollapsed(view, doomed);

  const filler: ProjectionInput[] = [];
  for (let i = 0; i < 60; i += 1) filler.push({ kind: "note", turn: null, note: "ui", text: "x" });
  for (let i = 0; i < 40; i += 1) project(s, filler);
  const live = s.blocks[s.blocks.length - 1]!.id;
  toggleCollapsed(view, live);
  assert.equal(view.collapsed.size, 2);

  pruneViewState(view, s);
  assert.equal(view.collapsed.has(doomed), false, "the evicted block's fold is gone");
  assert.equal(view.collapsed.has(live), true, "positive control: a live fold is kept");
});

test("the reducer never writes to ViewState", () => {
  // The in-list-editor rule, stated as a test: a re-projection rebuilds the
  // model, and a fold held anywhere the reducer can reach would be lost on the
  // next batch (`orch-tasks-changed` is the same shape one surface over).
  const s = emptyState();
  const view = emptyViewState();
  project(s, [{ kind: "text", turn: 1, delta: "a" }]);
  toggleCollapsed(view, s.blocks[0]!.id);
  view.dimThinking = true;

  project(s, [{ kind: "text", turn: 1, delta: "b" }, { kind: "thinking", turn: 1, delta: "c" }]);
  assert.equal(view.dimThinking, true);
  assert.equal(view.collapsed.size, 1);
  assert.equal(view.collapsed.has(s.blocks[0]!.id), true);
  // And nothing about a fold reached the block itself.
  assert.equal("collapsed" in s.blocks[0]!, false);
});

// ── textTail ────────────────────────────────────────────────────────────────

test("textTail returns the live END, bounded", () => {
  const s = run([
    { kind: "text", turn: 1, delta: "an old paragraph nobody is reading any more" },
    { kind: "tool_call", turn: 1, id: "t1", name: "Grep", input: {} },
    { kind: "tool_result", turn: 1, id: "t1", ok: true },
    { kind: "text", turn: 1, delta: "the live end" },
  ]);
  const tail = textTail(s, 30);
  assert.ok(tail.length <= 30);
  assert.ok(tail.endsWith("the live end"));
  assert.equal(textTail(s, 0), "", "a zero-width thumbnail asks for nothing");
});

test("textTail honours dimThinking, which is why it takes the ViewState", () => {
  const s = run([
    { kind: "thinking", turn: 1, delta: "muttering to myself" },
    { kind: "text", turn: 1, delta: "the answer" },
  ]);
  const view = emptyViewState();
  assert.match(textTail(s, 200, view), /muttering/, "positive control: it IS there by default");
  view.dimThinking = true;
  assert.doesNotMatch(textTail(s, 200, view), /muttering/);
  assert.match(textTail(s, 200, view), /the answer/, "and the answer still is");
});

test("textTail shows a tool card's name and status, never its output", () => {
  // A thumbnail of a 4 000-line grep result says nothing about what the agent
  // is doing, which is the one question a tiled pane has to answer.
  const s = run([
    { kind: "tool_call", turn: 1, id: "t1", name: "Grep", input: {} },
    { kind: "tool_output", turn: 1, id: "t1", delta: "MATCH-IN-OUTPUT\n".repeat(50), is_error: false },
    { kind: "tool_result", turn: 1, id: "t1", ok: true },
  ]);
  const tail = textTail(s, 500);
  assert.match(tail, /Grep · ok/, "positive control: the card is in the tail");
  assert.doesNotMatch(tail, /MATCH-IN-OUTPUT/);
});

// ── the fixture ─────────────────────────────────────────────────────────────

const FIXTURE = fileURLToPath(
  new URL("./fixtures/structuredview/session.harness.jsonl", import.meta.url),
);

/** Split on `\n` only and strip an optional trailing `\r` — pi's own framing
 *  rule (§1), which is also what makes this reader correct on a CRLF checkout
 *  (see the fixtures README). */
function readFixture(): ProjectionInput[] {
  return readFileSync(FIXTURE, "utf8")
    .split("\n")
    .map((l) => (l.endsWith("\r") ? l.slice(0, -1) : l))
    .filter((l) => l.length > 0)
    .map((l) => JSON.parse(l) as ProjectionInput);
}

test("the fixture session projects to the block catalogue, with nothing unknown", () => {
  const events = readFixture();
  assert.equal(events.length, 28, "the fixture is 28 events; a silent truncation would pass otherwise");
  const s = run(events, { nowMs: 10_000 });

  assert.equal(s.unknownEvents, 0, "every kind in the fixture is one this build reads");
  assert.equal(s.session, "0f3c9a2e-7b41-4d80-9c15-2ae6f8d31b77");
  assert.equal(s.model, "anthropic/claude-sonnet-4.5");
  assert.equal(s.exited, true);
  assert.equal(s.exitCode, 0);

  assert.deepEqual(kinds(s), [
    "delivery",
    "turn",
    "thinking",
    "text",
    "tool",
    "text",
    "tool",
    "request",
    // `turn_ended` closes the turn block already open; it opens no second one.
    "notice", // compaction
    "notice", // retry
    "turn", // turn 2
    "request",
    "text",
    "notice", // exit
  ]);
  assert.equal(only(s, "turn").length, 2, "two turns, two rules — not one per boundary event");
  assert.ok(only(s, "turn").every((b) => b.ended), "and both carry their receipt");

  // The joins the catalogue depends on.
  const tools = only(s, "tool");
  assert.equal(tools.length, 2, "two ToolUseIds, two cards");
  assert.deepEqual(tools.map((b) => [b.name, b.status, b.isError]), [
    ["Grep", "ok", false],
    ["Bash", "error", true],
  ]);
  assert.match(tools[0]!.output, /pane_kind/);
  // t2's second output RESTATES the whole value (replaces:true), so the card
  // shows it ONCE — the duplication the flag exists to prevent.
  assert.equal(tools[1]!.output, "npm test\n1 test failed\n");
  assert.equal((tools[1]!.output.match(/1 test failed/g) || []).length, 1, "not shown twice");

  const requests = only(s, "request");
  assert.deepEqual(requests.map((b) => b.channel), ["permission", "ui"]);
  assert.deepEqual(requests[0]!.settled, { answer: "deny", by: "policy" });
  assert.deepEqual(requests[1]!.settled, { answer: { Value: "fix/2214-resume-model" }, by: "human" });

  // Latest, not sum: 12 300 then 25 000.
  assert.equal(s.usage?.call_cumulative.input, 25_000);
  assert.equal(s.cost?.usd, 0.81);
  assert.deepEqual(s.followUp, ["review the fix once CI is green"]);
});

test("the fixture re-projects to an identical model, id for id", () => {
  // The property a reconnecting client leans on, and the one that makes a fold
  // safe to hold outside the blocks.
  const events = readFixture();
  const a = run(events, { nowMs: 10_000 });
  const b = run(events, { nowMs: 10_000 });
  assert.deepEqual(b.blocks, a.blocks);
});

test("thinking in the fixture stays out of the assistant's voice", () => {
  const s = run(readFixture());
  const think = only(s, "thinking");
  const text = only(s, "text");
  assert.ok(think.length > 0 && text.length > 0, "positive control: both kinds are present");
  const joined = text.map((b) => b.text).join(" ");
  for (const t of think) {
    assert.ok(t.text.length > 0);
    assert.ok(!joined.includes(t.text), "no thinking block's text appears in the assistant text");
  }
});
