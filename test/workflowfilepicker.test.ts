// The workflow pane's file picker (#2944) — which files it offers, which one is open, what a
// broken one says about itself, and which names it will CREATE.
//
// What these tests defend is not "the code does what it does". It is the three things the
// picker is able to get catastrophically wrong, each of which is a file:
//   * marking the WRONG option current — the human then edits `review-heavy` believing they
//     are editing `default`, and saves it;
//   * DROPPING a file that will not parse — which is the one file the pane exists to repair,
//     and the picker is the only way back to it;
//   * CREATING a name that collides with one already there, by case (#2892) or exactly —
//     which on Windows and macOS is two names for one file.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  canCreateWorkflow,
  resolveWorkflowFilePicker,
  switchPlan,
  layoutWriteAllowed,
  type CreateVerdict,
} from "../src/workflowfilepicker.ts";
import type { WorkflowEntry, WorkflowListing } from "../src/roster.ts";
import { WORKFLOW_FILE, WORKFLOW_NAME_MAX } from "../src/workflowmodel.ts";
import { layoutFileFor } from "../src/workflowlayout.ts";

const entry = (o: Partial<WorkflowEntry> & { name: string }): WorkflowEntry => ({
  path: `.orrerix/workflows/${o.name}.yml`,
  display_name: "",
  valid: true,
  errors: [],
  ...o,
});
const listing = (workflows: WorkflowEntry[], findings: string[] = []): WorkflowListing => ({
  workflows,
  findings,
});
const DEFAULT_ENTRY = entry({ name: "default", path: WORKFLOW_FILE });

const created = (v: CreateVerdict): string => {
  assert.equal(v.ok, true, v.ok ? "" : `refused: ${v.reason}`);
  return v.ok ? v.path : "";
};
const refusal = (v: CreateVerdict): string => {
  assert.equal(v.ok, false, v.ok ? `created ${v.path}, expected a refusal` : "");
  return v.ok ? "" : v.reason;
};

// ---------- which options, and which one is open ----------

test("every workflow the repo declares is an option, in the listing's order", () => {
  const p = resolveWorkflowFilePicker(
    listing([DEFAULT_ENTRY, entry({ name: "review-heavy" }), entry({ name: "solo" })]),
    WORKFLOW_FILE
  );
  assert.deepEqual(
    p.options.map((o) => o.name),
    ["default", "review-heavy", "solo"]
  );
  assert.deepEqual(
    p.options.map((o) => o.path),
    [WORKFLOW_FILE, ".orrerix/workflows/review-heavy.yml", ".orrerix/workflows/solo.yml"]
  );
});

test("the open file is the option marked current — and it is the only one", () => {
  const p = resolveWorkflowFilePicker(
    listing([DEFAULT_ENTRY, entry({ name: "review-heavy" })]),
    ".orrerix/workflows/review-heavy.yml"
  );
  // Both halves matter and they are different claims: the RIGHT one is marked (a picker that
  // marks `default` while the pane edits `review-heavy` invites a save into the wrong file),
  // and exactly ONE is (two ticks is a control that has stopped meaning anything).
  assert.deepEqual(
    p.options.filter((o) => o.current).map((o) => o.name),
    ["review-heavy"]
  );
  assert.equal(p.currentPath, ".orrerix/workflows/review-heavy.yml");
  assert.equal(p.offListing, false);
});

test("a backslash-spelled path still finds its option — a restored record and the listing need not agree on separators", () => {
  const p = resolveWorkflowFilePicker(
    listing([DEFAULT_ENTRY, entry({ name: "solo" })]),
    ".orrerix\\workflows\\solo.yml"
  );
  assert.equal(p.currentPath, ".orrerix/workflows/solo.yml");
  assert.equal(p.offListing, false);
});

test("paths are compared EXACTLY — a Default/default pair is two files, and the picker marks the one it is on", () => {
  // The #2892 collision, as the picker meets it: on Linux both files are genuinely on disk and
  // both are listed, so a case-folding compare would mark whichever came first and the human
  // would edit the other file believing they were editing this one.
  const lower = entry({ name: "default", path: ".orrerix/workflows/default.yml" });
  const upper = entry({ name: "Default", path: ".orrerix/workflows/Default.yml" });
  const p = resolveWorkflowFilePicker(listing([upper, lower]), ".orrerix/workflows/default.yml");
  assert.deepEqual(
    p.options.filter((o) => o.current).map((o) => o.name),
    ["default"]
  );
  // …and the other way round, which a "the exact match wins, then fold" rule would also pass
  // while still being wrong for the test below.
  const q = resolveWorkflowFilePicker(listing([upper, lower]), ".orrerix/workflows/Default.yml");
  assert.deepEqual(
    q.options.filter((o) => o.current).map((o) => o.name),
    ["Default"]
  );
});

test("a case-only difference is NOT the same file — Linux is a shipped platform (rev-std r1 B1)", () => {
  // `release.yml` builds `ubuntu-22.04` and ships AppImage/deb/rpm, and Linux filesystems are
  // case-sensitive. The premise this module was originally written on — "the platforms this
  // ships on are case-insensitive" — was false, and folding on it declared two real files one.
  //
  // The failure that bought this test is in `switchPlan`, one assertion down: on a repo
  // carrying the #2892 pair, clicking the sibling of the open file returned `same-file`, so
  // the menu listed both and could never move between them. Forever, silently, on the exact
  // navigation surface #2944 exists to add.
  const p = resolveWorkflowFilePicker(
    listing([entry({ name: "solo", path: ".orrerix/workflows/solo.yml" })]),
    ".orrerix/Workflows/solo.yml"
  );
  assert.equal(p.currentPath, null);
  assert.equal(p.offListing, true);

  const lower = ".orrerix/workflows/review.yml";
  const upper = ".orrerix/workflows/Review.yml";
  assert.deepEqual(switchPlan({ current: lower, dirty: false }, upper), {
    kind: "open",
    file: upper,
  });
  assert.deepEqual(switchPlan({ current: upper, dirty: true }, lower), {
    kind: "ask",
    file: lower,
  });
});

test("a .yml that is not one of the repo's workflows is off the listing, and nothing is marked current", () => {
  // *Open in workflow pane* takes any `.yml` (#222), so this is an ordinary state. What it
  // must NOT do is mark an unrelated option current: switching away would then be the human's
  // only clue that the pane was never showing that file.
  const p = resolveWorkflowFilePicker(listing([DEFAULT_ENTRY]), "ci/flow.yml");
  assert.equal(p.currentPath, null);
  assert.equal(p.offListing, true);
  assert.deepEqual(p.options.filter((o) => o.current), []);
});

test("a listing that could not be read is NOT an off-listing file — 'we do not know' is not 'your file is not there'", () => {
  const p = resolveWorkflowFilePicker(null, "ci/flow.yml");
  assert.deepEqual(p.options, []);
  assert.equal(p.currentPath, null);
  assert.equal(p.offListing, false);
  assert.deepEqual(p.findings, []);
});

// ---------- what a broken file says about itself ----------

test("a file that will not parse is STILL an option, carrying its first finding", () => {
  // The whole reason it is listed: this pane is how you fix it, and a workflow that vanishes
  // from the picker the moment it gets a syntax error is one you cannot navigate back to.
  const broken = entry({
    name: "broken",
    valid: false,
    errors: ["line 4: mapping values are not allowed here", "no orchestrator block"],
  });
  const p = resolveWorkflowFilePicker(listing([DEFAULT_ENTRY, broken]), WORKFLOW_FILE);
  const opt = p.options.find((o) => o.name === "broken");
  assert.ok(opt, "the broken file must still be listed");
  assert.equal(opt.valid, false);
  assert.equal(opt.finding, "line 4: mapping values are not allowed here");
  assert.match(opt.label, /has errors/);
});

test("a broken file with no stated error still says it is broken", () => {
  const p = resolveWorkflowFilePicker(
    listing([entry({ name: "mystery", valid: false, errors: ["", "   "] })]),
    WORKFLOW_FILE
  );
  assert.equal(p.options[0].finding, "the file could not be read");
});

test("a valid file carries no finding, and its label reads its `name:` only when that adds something", () => {
  const p = resolveWorkflowFilePicker(
    listing([
      entry({ name: "solo", display_name: "Solo — one worker, no review" }),
      entry({ name: "review-heavy", display_name: "review-heavy" }),
    ]),
    WORKFLOW_FILE
  );
  assert.equal(p.options[0].finding, null);
  assert.equal(p.options[0].label, "solo — Solo — one worker, no review");
  // The `name:` that merely repeats the stem is not shown twice.
  assert.equal(p.options[1].label, "review-heavy");
});

test("listing findings ride through, blank ones dropped", () => {
  const p = resolveWorkflowFilePicker(
    listing([DEFAULT_ENTRY], ["", "   ", "'default' is declared twice"]),
    WORKFLOW_FILE
  );
  assert.deepEqual(p.findings, ["'default' is declared twice"]);
});

// ---------- what New workflow… may create ----------

test("an ordinary name creates workflows/<name>.yml", () => {
  assert.equal(
    created(canCreateWorkflow("review-heavy", listing([DEFAULT_ENTRY]))),
    ".orrerix/workflows/review-heavy.yml"
  );
});

test("a repo on the legacy config dir gets its new workflow there — the spelling is read off the listing, not guessed", () => {
  const legacy = listing([entry({ name: "default", path: ".loomux/workflow.yml" })]);
  assert.equal(created(canCreateWorkflow("solo", legacy)), ".loomux/workflows/solo.yml");
});

test("a MIXED listing creates under the preferred dir, not whichever entry sorts first (rev-std r3 N3)", () => {
  // A repo part-way through the `.loomux/` -> `.orrerix/` migration. Reading `workflows[0]`
  // alone put every new workflow in the DEPRECATED directory on the strength of one leftover
  // file — and the listing is sorted by NAME, so which spelling leads is an accident of what
  // the workflows happen to be called.
  const mixed = listing([
    entry({ name: "aged", path: ".loomux/workflows/aged.yml" }),
    entry({ name: "fresh", path: ".orrerix/workflows/fresh.yml" }),
  ]);
  assert.equal(created(canCreateWorkflow("solo", mixed)), ".orrerix/workflows/solo.yml");
  // The legacy-first ORDER is the fixture's whole point: under the old rule this case — and
  // only this case — came back `.loomux/`, so an `.orrerix/`-first fixture would pass either
  // way and pin nothing.
  assert.ok(mixed.workflows[0].path.startsWith(".loomux/"), "the legacy entry must lead");
  // Unanimity is still honoured in both directions, so this is not "always prefer .orrerix".
  const allLegacy = listing([
    entry({ name: "aged", path: ".loomux/workflows/aged.yml" }),
    entry({ name: "older", path: ".loomux/workflows/older.yml" }),
  ]);
  assert.equal(created(canCreateWorkflow("solo", allLegacy)), ".loomux/workflows/solo.yml");
  // An empty listing has no spelling to read, so it gets the preferred one.
  assert.equal(created(canCreateWorkflow("solo", listing([]))), ".orrerix/workflows/solo.yml");
});

test("a name differing only by CASE from one already there is REFUSED, not created (#2892)", () => {
  // The half of #2892 a creation path can close. On Windows and macOS `Review.yml` and
  // `review.yml` are one file, so creating the second either clobbers the first or produces a
  // repo whose workflow identity depends on which machine reads it.
  const l = listing([DEFAULT_ENTRY, entry({ name: "review" })]);
  const reason = refusal(canCreateWorkflow("Review", l));
  assert.match(reason, /capitalisation/);
  assert.match(reason, /review/);
  // And it holds in the other direction, which is the case a rule written as "lowercase the
  // new name and compare" would let through.
  assert.equal(canCreateWorkflow("review", listing([entry({ name: "Review" })])).ok, false);
});

test("a case-only clash is refused even though the exact name is free — the two checks are not the same check", () => {
  const l = listing([entry({ name: "Solo" })]);
  assert.equal(l.workflows.some((e) => e.name === "solo"), false, "the exact name really is free");
  assert.equal(canCreateWorkflow("solo", l).ok, false);
});

test("a name that already exists exactly is refused, and the refusal names the file", () => {
  const l = listing([DEFAULT_ENTRY, entry({ name: "solo" })]);
  assert.match(refusal(canCreateWorkflow("solo", l)), /\.orrerix\/workflows\/solo\.yml/);
});

test("`default` is refused — that name belongs to the repo's own workflow.yml", () => {
  // Refused whether or not the repo has one yet: `workflows/default.yml` beside
  // `.orrerix/workflow.yml` is the two-files-one-name shape from the other end.
  assert.equal(canCreateWorkflow("default", listing([DEFAULT_ENTRY])).ok, false);
  assert.equal(canCreateWorkflow("default", listing([])).ok, false);
});

test("every name the engine's alphabet refuses is refused here, each with the rule it broke", () => {
  const l = listing([DEFAULT_ENTRY]);
  assert.match(refusal(canCreateWorkflow("", l)), /name/i);
  assert.match(refusal(canCreateWorkflow("../escape", l)), /letters, digits/);
  assert.match(refusal(canCreateWorkflow("a/b", l)), /letters, digits/);
  assert.match(refusal(canCreateWorkflow("a b", l)), /letters, digits/);
  assert.match(refusal(canCreateWorkflow("-lead", l)), /can't start with/);
  assert.match(refusal(canCreateWorkflow("con", l)), /reserved device name/);
  assert.match(refusal(canCreateWorkflow("COM1", l)), /reserved device name/);
  assert.match(refusal(canCreateWorkflow("a".repeat(WORKFLOW_NAME_MAX + 1), l)), /at most 64/);
  // …and the boundary the length message must not be off by one on.
  assert.equal(canCreateWorkflow("a".repeat(WORKFLOW_NAME_MAX), l).ok, true);
});

test("a name is never REWRITTEN into a usable one — two spellings that normalise to one name are two files claiming one workflow", () => {
  for (const bad of [" solo", "solo ", "solo.yml", "Solo/../x"]) {
    const v = canCreateWorkflow(bad, listing([DEFAULT_ENTRY]));
    assert.equal(v.ok, false, `${bad} must be refused rather than cleaned up`);
  }
});

test("the alphabet rule is asked FIRST, and about the string as TYPED", () => {
  // Both halves are invisible to a test that only asks "was it refused?", because the later
  // collision check refuses these too — with a different, wrong reason. They are what makes
  // the top-of-function `isWorkflowName` load-bearing rather than belt-and-braces:
  //
  //  * FIRST: `con` against a listing holding `Con` is a device name AND a case-clash. Drop
  //    the alphabet check and the clash branch answers instead — telling the human to pick
  //    another capitalisation for a name no capitalisation can rescue.
  const order = canCreateWorkflow("con", listing([DEFAULT_ENTRY, entry({ name: "Con" })]));
  assert.match(refusal(order), /reserved device name/);
  assert.doesNotMatch(refusal(order), /capitalisation/);
  //  * AS TYPED: ` solo` against a listing holding `Solo`. Trim before asking — the one
  //    "harmless" relaxation — and a name with a leading space is accepted by the alphabet
  //    rule and then refused as a capitalisation clash, which is a sentence about the wrong
  //    problem. `promptModal` trims what it returns, so this function is asked about the
  //    trimmed string in the app; being right about the untrimmed one is what keeps that a
  //    property of the CALLER rather than a rule this module has quietly given up.
  const typed = canCreateWorkflow(" solo", listing([DEFAULT_ENTRY, entry({ name: "Solo" })]));
  assert.match(refusal(typed), /letters, digits/);
  assert.doesNotMatch(refusal(typed), /capitalisation/);
});

test("no listing means no create — a create that cannot rule out a collision is the create this refuses", () => {
  const v = canCreateWorkflow("review-heavy", null);
  assert.equal(v.ok, false);
  assert.match(refusal(v), /could not list/);
});

// ---------- switching files, and the buffer that belongs to the one you are leaving ----------

const A = ".orrerix/workflow.yml";
const B = ".orrerix/workflows/review-heavy.yml";

test("a clean buffer switches straight over", () => {
  assert.deepEqual(switchPlan({ current: A, dirty: false }, B), { kind: "open", file: B });
});

test("an unsaved buffer is ASKED about — never carried across, never dropped", () => {
  // The failure this forbids is one write: `save()` writes `this.rel`, and `this.rel` is
  // whatever the picker last set, so a switch that retargets over a dirty buffer arms the next
  // Ctrl+S to write review-heavy's text into workflow.yml.
  assert.deepEqual(switchPlan({ current: A, dirty: true }, B), { kind: "ask", file: B });
});

test("clicking the option you are already on does nothing — the commonest click on a marked-current list", () => {
  // `same-file` rather than `open`, and the difference is a lost buffer: an open re-reads the
  // file, which discards unsaved edits, for a gesture that asked for nothing.
  assert.deepEqual(switchPlan({ current: A, dirty: true }, A), { kind: "same-file" });
  assert.deepEqual(switchPlan({ current: A, dirty: false }, A), { kind: "same-file" });
  assert.deepEqual(switchPlan({ current: B, dirty: true }, ".orrerix\\workflows\\review-heavy.yml"), {
    kind: "same-file",
  });
});

test("two workflows that COLLIDE on a block id still keep their canvas layouts apart", () => {
  // The fixture is deliberately the colliding one (CLAUDE.md: a non-interference pin is
  // fail-able only when its two operands MEET). The layout sidecar is keyed by BLOCK ID inside
  // the file, so two workflows that both declare `orchestrator`/`worker` — which is most of
  // them — are exactly the pair a shared sidecar would silently merge: opening `review-heavy`
  // would restore `default`'s node positions, and dragging one would move the other. Disjoint
  // ids would pass either way and pin nothing.
  const shared = ["orchestrator", "worker"];
  const wfA = { file: A, blocks: shared };
  const wfB = { file: B, blocks: shared };
  assert.deepEqual(wfA.blocks, wfB.blocks, "the fixture's whole point: the ids really do collide");
  assert.notEqual(
    layoutFileFor(wfA.file),
    layoutFileFor(wfB.file),
    "one sidecar per workflow file — the ids cannot disambiguate what the path must"
  );
  // And the switch between them is a `switchPlan`, so nothing of A's is in memory when B's
  // sidecar is next written.
  assert.equal(switchPlan({ current: wfA.file, dirty: false }, wfB.file).kind, "open");
});

test("a layout write whose file moved under it is DROPPED, not redirected (rev-final r2)", () => {
  // The same colliding fixture, because this is the same non-interference property reached
  // through the other door. `saveLayout` prunes the positions against the roster it can see,
  // then awaits twice (config dir, then write) — and the destination used to be re-derived
  // from `this.rel` at the END. A switch landing inside that window sent A's node positions
  // into B's sidecar: this pane's own "never written to the other file" rule, broken through
  // the LAYOUT rather than the buffer, where the unsaved-buffer guard cannot see it.
  //
  // The ids must collide for the fixture to witness anything, exactly as above: positions are
  // keyed by block id, so with `orchestrator`/`worker` on both sides the misdirected write
  // lands on real keys in the wrong file and silently moves that workflow's boxes. Disjoint
  // ids would leave two harmless orphan entries and the test would pass against the defect.
  const shared = ["orchestrator", "worker"];
  const wfA = { file: A, blocks: shared };
  const wfB = { file: B, blocks: shared };
  assert.deepEqual(wfA.blocks, wfB.blocks, "the fixture's whole point: the ids really do collide");

  // Computed for A, pane still on A: the ordinary case, and the one that must keep working —
  // a guard that refused everything would pass every assertion but the next one.
  assert.equal(layoutWriteAllowed(wfA.file, wfA.file), true);
  // Computed for A, pane switched to B while the write was in flight: dropped.
  assert.equal(layoutWriteAllowed(wfA.file, wfB.file), false);
  assert.equal(layoutWriteAllowed(wfB.file, wfA.file), false);
  // Separator spelling is not a move — the same normalisation every other comparison uses.
  assert.equal(layoutWriteAllowed(B, ".orrerix\\workflows\\review-heavy.yml"), true);
  // And the two sidecars really are distinct, so "dropped" is what stops the collision and
  // not something else further down.
  assert.notEqual(layoutFileFor(wfA.file), layoutFileFor(wfB.file));
});
