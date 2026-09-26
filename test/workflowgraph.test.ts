// Unit tests for the workflow model's derived graph and the canvas's graph EDIT
// operations (#222 v2, #1388), the editing helpers, and the config-dir/scaffold paths.
// Split out of test/workflowmodel.test.ts alongside src/workflowmodel.ts (#3498 F2).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  parseWorkflow,
  serializeWorkflow,
  serializeWorkflowPreserving,
  validateWorkflow,
  analyzeWorkflow,
  deriveGraph,
  removeBlockAt,
  nextBlockId,
  starterWorkflow,
  scaffoldWorkflowText,
  connectBlocks,
  disconnectBlocks,
  connectToGate,
  disconnectFromGate,
  gateConnectionError,
  connectionError,
  addBlock,
  newBlock,
  isValidBlockId,
  hasErrors,
  legacyFallbackFor,
  WORKFLOW_FILE,
  LEGACY_WORKFLOW_FILE,
  CONFIG_DIR,
  LEGACY_CONFIG_DIR,
  type Workflow,
} from "../src/workflowmodel.ts";
import { SAMPLE, codes, has } from "./support/workflowfixtures.ts";

// ---------- graph edit operations (v2: the canvas edits the file) ----------

test("drawing an edge, then re-reading the file, gives back the edge you drew", () => {
  // The round-trip the editable canvas rests on: a gesture → the model → the canonical file
  // → the model again, with the same GRAPH. If this doesn't hold, the canvas is lying about
  // the file.
  const w = starterWorkflow();
  const connected = connectBlocks(w, "planner", "reviewer");
  assert.deepEqual(connected.edges.at(-1), { from: "planner", to: "reviewer" });

  const reread = parseWorkflow(serializeWorkflow(connected)).workflow;
  // As a SET, not a sequence — and that is a property, not a concession: the canonical form
  // groups edges by source in roster order, so the file's edge order is a function of the
  // workflow rather than of the order the human happened to draw them in. Two people who draw
  // the same graph in a different order get the same file, and neither sees a diff from the
  // other's clicking sequence.
  const key = (e: { from: string; to: string }): string => `${e.from}->${e.to}`;
  assert.deepEqual(new Set(reread.edges.map(key)), new Set(connected.edges.map(key)));
  assert.equal(reread.edges.length, connected.edges.length, "no edge invented, none lost");
  assert.deepEqual(codes(validateWorkflow(reread)), []);
});

test("an edge that would be nonsense is refused before it is drawn, not after", () => {
  // A canvas that lets you complete the gesture and THEN says the edge was invalid has
  // wasted the gesture and left you to undo it.
  const w = starterWorkflow();
  assert.equal(connectionError(w, "planner", "reviewer"), null, "a legal edge has no error");
  assert.match(connectionError(w, "worker", "worker") ?? "", /itself/);
  assert.match(connectionError(w, "worker", "ghost") ?? "", /doesn't exist/);
  assert.match(connectionError(w, "", "worker") ?? "", /needs an id/);
  assert.match(connectionError(w, "planner", "worker") ?? "", /already exists/, "planner→worker is already drawn");

  // And the operation enforces it too, not only the pre-check — the canvas is the first line
  // of defence, not the only one.
  assert.deepEqual(connectBlocks(w, "worker", "worker").edges, w.edges);
  assert.deepEqual(connectBlocks(w, "worker", "ghost").edges, w.edges);
  assert.deepEqual(connectBlocks(w, "planner", "worker").edges, w.edges, "no duplicate edge");
});

test("erasing an edge takes the edge and nothing else", () => {
  const w = starterWorkflow();
  const cut = disconnectBlocks(w, "worker", "reviewer");
  assert.deepEqual(cut.edges, [{ from: "planner", to: "worker" }]);
  assert.deepEqual(cut.blocks, w.blocks, "the blocks it joined are untouched");
  // The reviewer is now unwired, which the validator says out loud — as a WARNING, because
  // edges are advisory and the workflow still runs.
  const f = validateWorkflow(cut);
  assert.equal(hasErrors(f), false);
  assert.ok(f.some((x) => x.code === "isolated-block" && x.blockId === "reviewer"));
});

// ---------- the merge gate as a drop target (#1388) ----------

/** The starter roster plus a second reviewer and a non-reviewer, which is the shape every
 *  gate question below needs: someone to add, someone to refuse, someone already seated. */
const gateFixture = (): Workflow => {
  let w = addBlock(starterWorkflow(), newBlock("quick-review", "Quick review"));
  w = addBlock(w, newBlock("scout", "Scout", "worker"));
  w = connectBlocks(w, "worker", "quick-review");
  // Wired, so the roster has no isolated block muddying a `codes(...)` assertion below.
  return connectBlocks(w, "planner", "scout");
};

test("#1388: dropping a reviewer on the gate gives it a seat, and the file reads back the same", () => {
  // The gesture the issue is about: a cheap scoped reviewer placed before the lead one, and
  // BOTH counted by the gate. Before this, the only route to that was the form or the YAML —
  // a release on the gate node did nothing at all and said nothing about why.
  //
  // THE FIXTURE SEATS `quick-review` FIRST ON PURPOSE (rev-lead round 1, B1). The roster is
  // planner, worker, reviewer, quick-review, scout — so seating the LATER block first and then
  // wiring the EARLIER one makes append order and roster order two different lists, and the
  // assertions below can tell them apart. With the seats the other way round they are the same
  // two literals, and every assertion holds under an append, under a sorted insert, and under
  // the sort the serializer actually performs — i.e. it witnesses nothing.
  const seeded = gateFixture();
  const w: Workflow = {
    ...seeded,
    gates: { merge: { ...seeded.gates.merge!, reviewers: ["quick-review"] } },
  };
  assert.equal(gateConnectionError(w, "reviewer"), null, "a reviewer block may gate the merge");

  const gated = connectToGate(w, "reviewer");
  assert.deepEqual(gated.gates.merge?.reviewers, ["quick-review", "reviewer"], "the MODEL appends");
  assert.deepEqual(gated.blocks, w.blocks, "wiring a gate seat edits no block");
  assert.deepEqual(gated.edges, w.edges, "and draws no advisory edge");

  // The round-trip the canvas rests on: gesture → model → file → model. The FILE lists the
  // seats in ROSTER order, not in the order they were wired: `emitGatesLines` writes
  // `sortByBlocks(gate.reviewers, order)`, the same canonical rule that groups edges by
  // source. So the model's array order is not a thing a human controls, and this test says
  // which of the two lists it is measuring at each step rather than letting one stand for the
  // other. This is also what makes "no new backend surface" checkable — a list of block ids
  // under `gates.merge.reviewers` is the shape `parse_workflow` already reads.
  const text = serializeWorkflow(gated);
  assert.match(text, /reviewers: \[reviewer, quick-review\]/, "the FILE is in roster order");
  // The control that makes the two assertions above a discrimination rather than a coincidence:
  // if the fixture ever stops distinguishing, this fails before the claims do.
  assert.notDeepEqual(
    gated.gates.merge?.reviewers,
    ["reviewer", "quick-review"],
    "the fixture must keep append order and roster order DIFFERENT, or neither pin witnesses anything"
  );
  // And the canvas does not write through `serializeWorkflow`: a drop goes out through
  // `serializeWorkflowPreserving`, which reuses the original text for what it did not touch.
  // It sorts too — that is the path the human's file actually takes.
  assert.match(
    serializeWorkflowPreserving(gated, serializeWorkflow(w)),
    /reviewers: \[reviewer, quick-review\]/,
    "the path a real drop writes through sorts as well"
  );

  const reread = parseWorkflow(text).workflow;
  assert.deepEqual(reread.gates.merge?.reviewers, ["reviewer", "quick-review"]);
  assert.deepEqual(codes(validateWorkflow(reread)), [], "and the gate it wrote is a valid one");
});

test("#1388/N1: `threshold: N` with no `require:` is a threshold gate, exactly as the engine reads it", () => {
  // `parse_workflow` treats the shorthand as a threshold gate — "`threshold: N` alone implies
  // a threshold gate; spelling `require: threshold` as well is allowed but redundant". The
  // pane used to default the absent key to all-pass, which made three things go quiet at once.
  const text = [
    "version: 1",
    "name: demo",
    "blocks:",
    "  - id: worker",
    "    kind: worker",
    "    cli: claude",
    "  - id: rev-a",
    "    kind: reviewer",
    "    cli: claude",
    "  - id: rev-b",
    "    kind: reviewer",
    "    cli: claude",
    "edges:",
    "  - from: worker",
    "    to: [rev-a, rev-b]",
    "gates:",
    "  merge:",
    "    threshold: 2",
    "    reviewers: [rev-a, rev-b]",
    "",
  ].join("\n");
  const w = parseWorkflow(text).workflow;
  assert.equal(w.gates.merge?.require, "threshold", "the shorthand IS a threshold gate");
  assert.deepEqual(codes(validateWorkflow(w)), [], "and a satisfiable one, so nothing is flagged");

  // (1) The validator's threshold rules now reach it: raise the bar above the seats and it is
  // the same finding a spelled-out gate gets.
  const greedy: Workflow = { ...w, gates: { merge: { ...w.gates.merge!, threshold: 3 } } };
  assert.ok(codes(validateWorkflow(greedy)).includes("gate-bad-threshold"));

  // (2) The clamp reaches it, so the one-click erase cannot brick the file.
  const cut = disconnectFromGate(w, "rev-b");
  assert.deepEqual(cut.gates.merge?.reviewers, ["rev-a"]);
  assert.equal(cut.gates.merge?.threshold, 1, "the number follows the shorthand list down too");
  assert.deepEqual(codes(validateWorkflow(cut)), []);

  // (3) The live one, and the reason this is worth fixing rather than filing: the pane
  // re-serializes the whole gate on any gate edit, and it used to write `require: all-pass`
  // beside the threshold — a PAIR `parse_workflow` refuses outright, so an unrelated edit
  // silently dropped the repo back to the built-in roster.
  const rewritten = serializeWorkflow(w);
  assert.match(rewritten, /require: threshold/);
  assert.doesNotMatch(rewritten, /require: all-pass/);
  assert.deepEqual(codes(validateWorkflow(parseWorkflow(rewritten).workflow)), []);
});

test("#1388/N1: all-pass AND a threshold is the pair the engine refuses — the pane says so now", () => {
  // The other half of the same question. After the shorthand is normalised, a model carrying
  // both got there by spelling both out (or by the gate form's picker being moved to all-pass
  // over a threshold still in the file), and the engine refuses it: "require: all-pass takes
  // no threshold — drop it, or use require: threshold".
  const base = connectToGate(gateFixture(), "quick-review");
  const both: Workflow = {
    ...base,
    gates: { merge: { ...base.gates.merge!, require: "all-pass", threshold: 2 } },
  };
  const f = validateWorkflow(both).filter((x) => x.code === "gate-bad-threshold");
  assert.equal(f.length, 1, "one finding, naming the pair");
  assert.match(f[0]!.message, /require: all-pass and also names threshold: 2/);

  // `all` is the engine's synonym for `all-pass`, and it refuses the pair under that spelling
  // too — a pane that only knew one spelling would bless the other.
  const synonym: Workflow = { ...both, gates: { merge: { ...both.gates.merge!, require: "all" } } };
  assert.ok(codes(validateWorkflow(synonym)).includes("gate-bad-threshold"));

  // Dropping the number clears it, which is what the finding tells the human to do.
  const dropped: Workflow = {
    ...both,
    gates: { merge: { ...both.gates.merge!, threshold: undefined } },
  };
  assert.deepEqual(codes(validateWorkflow(dropped)), []);

  // An UNKNOWN require with a threshold still gets ONE finding, not two stacked on one line:
  // the engine answers that case with "unknown require" alone, and so does the pane.
  const junk: Workflow = { ...both, gates: { merge: { ...both.gates.merge!, require: "banana" } } };
  assert.deepEqual(codes(validateWorkflow(junk)), ["gate-unknown-require"]);
});

test("#1388: the gate refuses what it cannot read a verdict from, in the validator's own words", () => {
  // The refusal is `gateReviewerFinding` — the SAME function the findings strip and the
  // engine's `gate_reviewer_error` use. A canvas that refused a drop in words the validator
  // never uses would be a second definition of the rule, which is what #1176 collapsed.
  const w = gateFixture();
  assert.match(gateConnectionError(w, "scout") ?? "", /only a reviewer records a verdict/);
  assert.match(gateConnectionError(w, "nobody") ?? "", /no block has that id/);
  assert.match(gateConnectionError(w, "") ?? "", /needs an id/);
  assert.match(gateConnectionError(w, "reviewer") ?? "", /already gates the merge/);

  // Word-for-word the finding, not merely the same shape of complaint.
  const finding = validateWorkflow({
    ...w,
    gates: { merge: { ...w.gates.merge!, reviewers: ["scout"] } },
  }).find((f) => f.code === "gate-not-a-reviewer");
  assert.equal(gateConnectionError(w, "scout"), finding?.message);

  // And every refusal is enforced by the operation too, not only by the pre-check: the canvas
  // is the first line of defence, not the only one.
  for (const id of ["scout", "nobody", "", "reviewer"]) {
    assert.deepEqual(connectToGate(w, id).gates.merge?.reviewers, ["reviewer"], `refused: "${id}"`);
  }
});

test("#1388: with no merge gate declared there is nothing to drop onto, and it says so", () => {
  const w: Workflow = { ...gateFixture(), gates: {} };
  assert.match(gateConnectionError(w, "quick-review") ?? "", /no merge gate/);
  assert.deepEqual(connectToGate(w, "quick-review").gates, {}, "a gate is never invented by a drop");
});

test("#1388: erasing a gate edge takes the seat and nothing else", () => {
  const w = connectToGate(gateFixture(), "quick-review");
  const cut = disconnectFromGate(w, "quick-review");
  assert.deepEqual(cut.gates.merge?.reviewers, ["reviewer"]);
  assert.deepEqual(cut.blocks, w.blocks, "the reviewer block itself is untouched");
  assert.deepEqual(cut.edges, w.edges, "and so is every advisory edge");
  assert.equal(cut.gates.merge?.require, "all-pass", "and the rest of the gate's policy");
  assert.deepEqual(disconnectFromGate(w, "scout"), w, "removing a seat nobody holds changes nothing");
});

test("#1388: a threshold follows its reviewer list DOWN, so a one-click erase cannot break the file", () => {
  // `parse_workflow` refuses the WHOLE workflow.yml over "3 passes from 2 reviewers" — not
  // the gate, the file — and the group then silently falls back to the built-in roster. A
  // gesture that cheap must not be able to do that.
  const base = connectToGate(gateFixture(), "quick-review");
  const w: Workflow = {
    ...base,
    gates: { merge: { ...base.gates.merge!, require: "threshold", threshold: 2 } },
  };
  assert.deepEqual(codes(validateWorkflow(w)), [], "2 of 2 is a valid gate to start from");

  const cut = disconnectFromGate(w, "quick-review");
  assert.deepEqual(cut.gates.merge?.reviewers, ["reviewer"]);
  assert.equal(cut.gates.merge?.threshold, 1, "the number follows the list down");
  assert.deepEqual(codes(validateWorkflow(cut)), [], "so the file the engine reads still loads");

  // DOWN only: adding a reviewer leaves the number alone, because "2 of 3 must pass" is a
  // policy, not a proportion.
  const grown = connectToGate(cut, "quick-review");
  assert.equal(grown.gates.merge?.threshold, 1, "adding a seat does not raise the bar");

  // And it clamps to the MINIMUM rather than to zero: emptying the gate is already
  // `gate-no-reviewers`, loudly, and destroying the human's number would buy nothing.
  const empty = disconnectFromGate(cut, "reviewer");
  assert.deepEqual(empty.gates.merge?.reviewers, []);
  assert.equal(empty.gates.merge?.threshold, 1, "a threshold of 0 is not a thing the schema has");
  assert.ok(codes(validateWorkflow(empty)).includes("gate-no-reviewers"));
  // Re-seat a reviewer and the human's own gate is back, valid, with nothing to retype.
  assert.deepEqual(codes(validateWorkflow(connectToGate(empty, "reviewer"))), []);
});

test("#1388: deleting the BLOCK clamps the threshold the same way deleting its gate edge does", () => {
  // Two paths to one empty seat, and there is no reading on which one of them may leave the
  // file unloadable while the other doesn't.
  const base = connectToGate(gateFixture(), "quick-review");
  const w: Workflow = {
    ...base,
    gates: { merge: { ...base.gates.merge!, require: "threshold", threshold: 2 } },
  };
  const deleted = removeBlockAt(w, w.blocks.findIndex((b) => b.id === "quick-review"));
  assert.deepEqual(deleted.gates.merge?.reviewers, ["reviewer"], "the seat goes with the block");
  assert.equal(deleted.gates.merge?.threshold, 1, "and so does the number that counted it");
  assert.deepEqual(codes(validateWorkflow(deleted)), []);

  // A block that is NOT on the gate leaves the gate entirely alone — the clamp is a
  // consequence of the list getting shorter, not something a delete does on its own.
  const other = removeBlockAt(w, w.blocks.findIndex((b) => b.id === "scout"));
  assert.equal(other.gates.merge?.threshold, 2);
  assert.deepEqual(other.gates.merge?.reviewers, ["reviewer", "quick-review"]);
});

test("a block created on the canvas keeps the id the human gave it", () => {
  // §4's first commitment. Dify mints `node_1720794829558`; n8n keys the graph by the display
  // NAME so a rename silently breaks it. A block created here gets a human id, edges name that
  // id, and a rename touches nothing.
  const w = addBlock(starterWorkflow(), newBlock("rev-security", "Security review"));
  const wired = connectBlocks(w, "worker", "rev-security");
  const reread = parseWorkflow(serializeWorkflow(wired)).workflow;
  const made = reread.blocks.find((b) => b.id === "rev-security")!;
  assert.equal(made.name, "Security review");
  assert.equal(made.kind, "reviewer");
  assert.ok(reread.edges.some((e) => e.from === "worker" && e.to === "rev-security"));

  // Renaming it (display only) leaves every reference alone — the property the id buys.
  const renamed: Workflow = {
    ...reread,
    blocks: reread.blocks.map((b) => (b.id === "rev-security" ? { ...b, name: "Sec" } : b)),
  };
  assert.deepEqual(parseWorkflow(serializeWorkflow(renamed)).workflow.edges, reread.edges);
  assert.deepEqual(codes(validateWorkflow(renamed)), []);
});

test("a canvas-authored workflow serializes canonically and stays stable", () => {
  // Build one entirely through the edit ops — the way the canvas does — and it must produce
  // the same shape as a hand-written file: canonical, idempotent, no findings.
  let w = starterWorkflow("0.9.0");
  w = addBlock(w, newBlock("rev-perf", "Perf review"));
  w = connectBlocks(w, "worker", "rev-perf");
  w = disconnectBlocks(w, "planner", "worker");
  w = connectBlocks(w, "planner", "worker");
  const once = serializeWorkflow(w);
  assert.equal(serializeWorkflow(parseWorkflow(once).workflow), once, "GUI-authored files format like any other");
  assert.deepEqual(analyzeWorkflow(once).findings.filter((f) => f.severity === "error"), []);
});

// ---------- the derived graph ----------

test("the graph layers the declared path and flags what doesn't resolve", () => {
  const g = deriveGraph(parseWorkflow(SAMPLE).workflow);
  // Layers hold block INDICES (rev-5 F5) — the roster's rows, not their ids.
  assert.deepEqual(g.layers, [[0], [1], [2, 3]]);
  assert.ok(g.nodes.every((n) => n.known));
  assert.ok(g.edges.every((e) => e.resolved));
  assert.deepEqual(g.gates, [
    { name: "merge", require: "all-pass", threshold: undefined, reviewers: ["rev-security", "rev-tests"] },
  ]);

  const broken = parseWorkflow(SAMPLE).workflow;
  broken.edges.push({ from: "worker", to: "ghost" });
  broken.blocks[0]!.kind = "superuser";
  const bg = deriveGraph(broken);
  assert.equal(bg.nodes.find((n) => n.block.id === "planner")!.known, false);
  assert.equal(bg.edges.find((e) => e.to === "ghost")!.resolved, false);
});

test("broken blocks each get their OWN node in the graph (rev-5 F5)", () => {
  // Keyed by id, two id-less stubs (both "") mapped to ONE position and rendered stacked, so
  // a file with two broken blocks showed one — in the view whose whole job is to show you the
  // file. Same for a duplicate-id pair.
  const stubs: Workflow = {
    version: 1,
    name: "",
    blocks: [
      { id: "", name: "stub A", kind: "worker", cli: "claude", model: "" },
      { id: "", name: "stub B", kind: "reviewer", cli: "claude", model: "" },
      { id: "dupe", name: "first", kind: "reviewer", cli: "claude", model: "" },
      { id: "dupe", name: "second", kind: "reviewer", cli: "claude", model: "" },
    ],
    edges: [],
    gates: {},
  };
  const g = deriveGraph(stubs);
  assert.equal(g.nodes.length, 4);
  assert.deepEqual(
    g.nodes.map((n) => n.index),
    [0, 1, 2, 3],
    "every row is its own node, whatever its id says"
  );
  // …and no two nodes share a slot: the flattened layers hold each index exactly once.
  const placed = g.layers.flat();
  assert.deepEqual([...placed].sort((a, b) => a - b), [0, 1, 2, 3]);
});

test("a file whose blocks have no ids at all makes no claim about entry points (rev-5 F6)", () => {
  // With no ids there is no graph to reason about — every edge is dangling, and
  // `edge-unknown-block` has already said so. "Every block is pointed at by another" was
  // neither true nor useful here.
  const w: Workflow = {
    version: 1,
    name: "",
    blocks: [
      { id: "", name: "a", kind: "worker", cli: "claude", model: "" },
      { id: "", name: "b", kind: "reviewer", cli: "claude", model: "" },
    ],
    edges: [{ from: "a", to: "b" }],
    gates: {},
  };
  const f = validateWorkflow(w);
  assert.ok(!has(f, "no-entry-block"));
  assert.ok(has(f, "block-id-missing"), "the finding that IS true still fires");
  assert.ok(has(f, "edge-unknown-block"));
});

test("a cyclic graph still layers (it must never spin)", () => {
  const w = starterWorkflow();
  w.edges.push({ from: "reviewer", to: "worker" });
  const g = deriveGraph(w);
  assert.equal(g.nodes.length, 3);
  assert.ok(g.layers.length >= 1);
});

// ---------- editing helpers ----------

test("a new block's id is unique and derived from its name", () => {
  const w = starterWorkflow();
  assert.equal(nextBlockId(w, "Security review"), "security-review");
  assert.equal(nextBlockId(w, "Worker"), "worker-2", "an id already in use gets suffixed, never reused");
  assert.equal(nextBlockId(w, "!!!"), "block");
  assert.ok(isValidBlockId(nextBlockId(w, "2nd reviewer")));
});

test("a created workflow records which loomux wrote it — and only a created one (rev-5 F7)", () => {
  // §4's "record the loomux version that authored it" (Langflow's last_tested_version
  // lesson). Written EXACTLY ONCE, at creation.
  const created = starterWorkflow("0.8.0");
  assert.match(serializeWorkflow(created), /^authored_with: 0\.8\.0$/m);
  assert.deepEqual(codes(validateWorkflow(created)), [], "and it is not itself a finding");

  // No version to hand → no key. An `authored_with: unknown` would be worse than an absent one.
  assert.ok(!serializeWorkflow(starterWorkflow()).includes("authored_with"));

  // On an EXISTING file it round-trips verbatim and is never restamped: opening a workflow
  // written by an older build and changing a model must not also rewrite the version line.
  const older = parseWorkflow(`version: 1
authored_with: 0.6.1
blocks:
  - id: w
    name: W
    kind: worker
    cli: claude
`).workflow;
  assert.equal(older.authored_with, "0.6.1");
  assert.equal(older.extra, undefined, "a key loomux itself writes is never an unknown one");
  older.blocks[0]!.model = "opus"; // the ordinary form edit
  assert.match(serializeWorkflow(older), /^authored_with: 0\.6\.1$/m, "preserved, not restamped");
});

test("deleting a block takes every reference to it with it", () => {
  const w = starterWorkflow();
  const after = removeBlockAt(w, 2); // the reviewer
  assert.deepEqual(
    after.blocks.map((b) => b.id),
    ["planner", "worker"]
  );
  assert.deepEqual(after.edges, [{ from: "planner", to: "worker" }]);
  assert.deepEqual(after.gates.merge!.reviewers, [], "the gate must not keep gating on a block that's gone");
  // …and the result is therefore free of dangling references — which is the entire
  // point: a delete that left them behind would turn one click into three errors.
  assert.ok(!has(validateWorkflow(after), "edge-unknown-block"));
  assert.ok(!has(validateWorkflow(after), "gate-unknown-reviewer"));
});

test("deleting a broken block deletes THAT block — not everything shaped like it", () => {
  // The two cases the pane is guaranteed to meet, because they are exactly the ones the
  // validation pass is complaining about when the human reaches for Delete.
  //
  // Two id-LESS stubs: deleting one must not take the other. (An id-keyed delete would
  // remove "every block whose id is empty" — i.e. both.)
  const stubs: Workflow = {
    version: 1,
    name: "",
    blocks: [
      { id: "", name: "first stub", kind: "worker", cli: "claude", model: "" },
      { id: "", name: "second stub", kind: "reviewer", cli: "claude", model: "" },
    ],
    edges: [],
    gates: {},
  };
  const left = removeBlockAt(stubs, 0);
  assert.deepEqual(
    left.blocks.map((b) => b.name),
    ["second stub"]
  );

  // A DUPLICATE id survives its own deletion — the twin still answers to it — so the edges
  // and the gate that name it are still meaningful and must NOT be stripped.
  const dupes = starterWorkflow();
  dupes.blocks.push({ id: "reviewer", name: "Reviewer (copy)", kind: "reviewer", cli: "claude", model: "" });
  const after = removeBlockAt(dupes, 3);
  assert.deepEqual(after.edges, dupes.edges, "the surviving twin still answers to that id");
  assert.deepEqual(after.gates.merge!.reviewers, ["reviewer"]);
  assert.deepEqual(codes(validateWorkflow(after)), [], "and the duplicate is resolved by the delete");
});

// ---------- the repo config dir: `.orrerix/` preferred, `.loomux/` still read (#1153 p4) ----------

test("the default workflow path is the preferred one, and it falls back exactly once", () => {
  assert.equal(WORKFLOW_FILE, `${CONFIG_DIR}/workflow.yml`);
  assert.equal(LEGACY_WORKFLOW_FILE, `${LEGACY_CONFIG_DIR}/workflow.yml`);
  // From the default, the legacy path is what to try next…
  assert.equal(legacyFallbackFor(WORKFLOW_FILE), LEGACY_WORKFLOW_FILE);
  // …and from there, nothing. Without this the pane would retry forever on a repo that
  // has neither file — which is where every repo starts.
  assert.equal(legacyFallbackFor(LEGACY_WORKFLOW_FILE), null);
});

test("a pane opened on an EXPLICIT file never silently opens a different one", () => {
  // A restored tab or the editor's `getFile` names the file the human asked for. Falling
  // back there would mean a pane whose header says one path and whose buffer holds another.
  assert.equal(legacyFallbackFor("teams/api/.orrerix/workflow.yml"), null);
  assert.equal(legacyFallbackFor("docs/example-workflow.yml"), null);
});

test("the legacy config dir is pinned — changing it breaks every existing repo", () => {
  assert.equal(LEGACY_CONFIG_DIR, ".loomux");
  assert.equal(CONFIG_DIR, ".orrerix");
});

test("the scaffold's own header names the file it is about to be written to", () => {
  // rev-lead round 1, B2 — a real user-facing defect, not a rename nit. The pane writes
  // this text to `this.rel`, so a hard-coded `.loomux/workflow.yml` header meant pressing
  // "Create workflow" in a repo with no config dir produced `.orrerix/workflow.yml` whose
  // FIRST LINE named a path that repo does not have: precisely the "reads one file,
  // reports another" failure this phase exists to fix.
  assert.match(scaffoldWorkflowText("1.0.0", WORKFLOW_FILE).split("\n")[0], /^# \.orrerix\/workflow\.yml /);
  // A repo still on the legacy spelling gets a header naming ITS file...
  assert.match(
    scaffoldWorkflowText("1.0.0", LEGACY_WORKFLOW_FILE).split("\n")[0],
    /^# \.loomux\/workflow\.yml /
  );
  // ...and an explicitly-opened path is named too, so the header can never disagree with
  // the pane's own path label.
  assert.match(
    scaffoldWorkflowText("1.0.0", "teams/api/.orrerix/workflow.yml").split("\n")[0],
    /^# teams\/api\/\.orrerix\/workflow\.yml /
  );
  // The default is the preferred spelling — a caller that omits the path never gets the
  // deprecated one.
  assert.match(scaffoldWorkflowText("1.0.0").split("\n")[0], /^# \.orrerix\/workflow\.yml /);
  // And whatever the header says, the body must still parse and validate clean.
  assert.deepEqual(analyzeWorkflow(scaffoldWorkflowText("1.0.0", LEGACY_WORKFLOW_FILE)).findings, []);
});
