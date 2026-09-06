// Switching a running group's workflow (#1689 slice D2) — the group header's
// picker, its drift chip, and the confirmation a human reads before a LIVE
// group's roster changes.
//
// What these tests defend is not "the code does what it does". Two properties
// are worth the file on their own:
//   1. The picker can never offer, or silently select, a workflow that is not
//      what the group is running — including when the file behind the running
//      one has been deleted, and including when the view is holding a name the
//      repo has stopped declaring.
//   2. The confirmation says every consequence the backend's diff carries, and
//      says nothing the diff does not — a modal that under-reports is a human
//      authorizing a change they were not shown.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  driftChip,
  resolveEditTarget,
  resolveSwitchPicker,
  switchConfirm,
} from "../src/workflowswitch.ts";
import type {
  RosterDiff,
  WorkflowGateStatus,
  WorkflowStatus,
  WorkflowSwitchPreview,
} from "../src/orchestration.ts";

// ---------- fixtures ----------

function status(over: Partial<WorkflowStatus> = {}): WorkflowStatus {
  return {
    advanced: true,
    name: "Review-heavy",
    workflow: "default",
    available: ["default"],
    drift: null,
    default_branch: "main",
    blocks: [],
    gate: null,
    wip: [],
    ...over,
  };
}

const EMPTY_DIFF: RosterDiff = {
  added: [],
  removed: [],
  changed: [],
  gate_changed: false,
  intake_changed: false,
  orchestrator_cli_changed: false,
};

function gate(over: Partial<WorkflowGateStatus> = {}): WorkflowGateStatus {
  return {
    require: "all-pass",
    reviewers: ["rev-lead"],
    also: [],
    satisfiable: true,
    missing_blocks: [],
    ...over,
  };
}

function preview(over: Partial<WorkflowSwitchPreview> = {}): WorkflowSwitchPreview {
  return {
    name: "b",
    from: "default",
    path: ".orrerix/workflows/b.yml",
    digest: "d1",
    display_name: "",
    empty: false,
    refusal: null,
    next_resume: [],
    diff: { ...EMPTY_DIFF },
    blocks: [],
    gate: null,
    ...over,
  };
}

/** The one line of a confirmation that mentions `needle`. Fails loudly when
 *  zero or several do, so an assertion can never quietly pass against the wrong
 *  sentence — or against a body that grew a second one saying something else. */
function theLine(lines: string[], needle: string): string {
  const hits = lines.filter((l) => l.includes(needle));
  assert.equal(hits.length, 1, `expected exactly one line containing ${JSON.stringify(needle)}, got ${hits.length}: ${JSON.stringify(lines)}`);
  return hits[0];
}

// ---------- the picker ----------

test("every available workflow is an option, and the running one says so", () => {
  const p = resolveSwitchPicker(
    status({ workflow: "b", available: ["default", "a", "b"] }),
    null,
    false
  );
  assert.deepEqual(
    p.options.map((o) => o.name),
    ["default", "a", "b"]
  );
  assert.equal(p.active, "b");
  assert.equal(p.selected, "b");
  assert.equal(p.options.filter((o) => o.active).length, 1);
  assert.equal(theLine(p.options.map((o) => o.label), "(running)"), "b (running)");
  assert.equal(p.dirty, false);
  assert.equal(p.show, true);
});

test("a held selection the repo still declares is honoured, and marks the picker dirty", () => {
  const p = resolveSwitchPicker(status({ workflow: "b", available: ["a", "b"] }), "a", false);
  assert.equal(p.selected, "a");
  assert.equal(p.active, "b");
  assert.equal(p.dirty, true);
});

test("a held selection the repo has stopped declaring falls back to what is RUNNING", () => {
  // Not to `default`: this is a live group, and the one name that is certainly
  // right is the one it is already running. Falling back to `default` would
  // arm Review & apply for a switch nobody asked for.
  const p = resolveSwitchPicker(status({ workflow: "b", available: ["b", "c"] }), "a", false);
  assert.equal(p.selected, "b");
  assert.equal(p.dirty, false);
});

test("the RUNNING workflow is an option even when the repo no longer declares it", () => {
  const p = resolveSwitchPicker(status({ workflow: "b", available: ["default"] }), null, false);
  assert.deepEqual(
    p.options.map((o) => o.name),
    ["default", "b"]
  );
  const running = p.options.find((o) => o.name === "b")!;
  assert.equal(running.active, true);
  assert.equal(running.missing, true);
  assert.match(running.label, /file is gone/);
  assert.equal(p.selected, "b");
  // And the one the repo DOES declare is not mislabelled as missing.
  assert.equal(p.options.find((o) => o.name === "default")!.missing, false);
});

test("the control is disabled, with the toggle named, while workflow mode is off", () => {
  const p = resolveSwitchPicker(
    status({ advanced: false, available: ["default", "b"] }),
    null,
    false
  );
  assert.equal(p.enabled, false);
  assert.match(p.disabledReason!, /workflow mode on/);
  // Still SHOWN — the picker is how a human learns switching exists, and the
  // toggle it names sits in the same panel.
  assert.equal(p.show, true);
});

test("an apply already in flight disables the control without blaming the toggle", () => {
  const p = resolveSwitchPicker(status({ available: ["default", "b"] }), null, true);
  assert.equal(p.enabled, false);
  assert.match(p.disabledReason!, /already in flight/);
  assert.doesNotMatch(p.disabledReason!, /workflow mode/);
});

test("an enabled picker has no reason to give", () => {
  const p = resolveSwitchPicker(status({ available: ["default", "b"] }), null, false);
  assert.equal(p.enabled, true);
  assert.equal(p.disabledReason, null);
});

test("one workflow and no drift hides the control; drift alone brings it back", () => {
  const one = status({ available: ["default"] });
  assert.equal(resolveSwitchPicker(one, null, false).show, false);
  // Drift is adopted by re-applying the ACTIVE name, so hiding the control here
  // would hide the fix for what the chip beside it is complaining about.
  const drifted = status({
    available: ["default"],
    drift: { note: "the file has changed since this group was launched", on_disk_blocks: ["w"] },
  });
  assert.equal(resolveSwitchPicker(drifted, null, false).show, true);
});

test("no status yet claims nothing", () => {
  const p = resolveSwitchPicker(null, "b", false);
  assert.deepEqual(p.options, []);
  assert.equal(p.show, false);
  assert.equal(p.enabled, false);
  assert.equal(p.disabledReason, null);
  assert.equal(p.selected, "default");
});

// ---------- the drift chip ----------

test("no drift, no chip — including for a group with workflow mode off", () => {
  assert.equal(driftChip(status()), null);
  assert.equal(driftChip(status({ advanced: false })), null);
  assert.equal(driftChip(null), null);
});

test("the chip carries the BACKEND's wording and names the fix", () => {
  const note = "the file has changed since this group was launched";
  const chip = driftChip(status({ drift: { note, on_disk_blocks: ["orch", "w-1"] } }))!;
  // Word for word with the audit row, so the badge and the trail cannot say
  // different things about one divergence.
  assert.match(chip.text, new RegExp(note.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
  assert.match(chip.title, /orch, w-1/);
  assert.match(chip.title, /Review & apply/);
});

test("a drift with no readable blocks says so rather than listing nothing", () => {
  const chip = driftChip(
    status({ drift: { note: "the file no longer validates", on_disk_blocks: [] } })
  )!;
  assert.match(chip.title, /declares nothing this group could run/);
});

// ---------- the confirmation ----------

test("an empty diff is an explanation, not a confirmation", () => {
  const c = switchConfirm(preview({ empty: true }));
  assert.equal(c.canApply, false);
  assert.equal(c.title, "Nothing to apply");
  assert.match(theLine(c.lines, "Nothing would change"), /already running/);
  // No consequences are claimed for a switch that has none.
  assert.equal(c.lines.filter((l) => /Adds |Removes |Changes /.test(l)).length, 0);
});

test("added, removed and changed blocks each get their own sentence", () => {
  const c = switchConfirm(
    preview({
      diff: {
        ...EMPTY_DIFF,
        added: ["rev-final"],
        removed: ["w-old", "planner"],
        changed: [{ id: "orch", fields: ["model"] }],
      },
    })
  );
  assert.equal(c.canApply, true);
  assert.equal(theLine(c.lines, "Adds"), "Adds 1 block: rev-final.");
  assert.match(theLine(c.lines, "Removes"), /^Removes 2 blocks: w-old, planner —/);
  assert.match(theLine(c.lines, "Removes"), /bare resume of its session is refused/);
  assert.equal(theLine(c.lines, "Changes orch"), "Changes orch: model.");
  assert.match(theLine(c.lines, "keep the block"), /Agents already running/);
});

test("a gate change quotes the gate the switch would arm", () => {
  const c = switchConfirm(
    preview({
      diff: { ...EMPTY_DIFF, gate_changed: true },
      gate: gate({ reviewers: ["rev-a", "rev-b"], require: "threshold 1", also: ["ci-green"] }),
    })
  );
  const line = theLine(c.lines, "Re-arms");
  assert.match(line, /rev-a \+ rev-b/);
  assert.match(line, /at least 1 pass/);
  assert.match(line, /ci-green/);
});

test("a gate change to NO gate says the gate is cleared", () => {
  const c = switchConfirm(preview({ diff: { ...EMPTY_DIFF, gate_changed: true }, gate: null }));
  assert.equal(theLine(c.lines, "gate"), "Clears the armed merge gate.");
});

test("an unsatisfiable gate is reported even when the GATE itself did not change", () => {
  // The roster moved under it: the same gate names reviewers the new blocks
  // cannot spawn. A modal that only warned on `gate_changed` would miss this,
  // and the human would find out from a merge that bounced.
  const c = switchConfirm(
    preview({
      diff: { ...EMPTY_DIFF, removed: ["rev-b"] },
      gate: gate({ reviewers: ["rev-a", "rev-b"], satisfiable: false, missing_blocks: ["rev-b"] }),
    })
  );
  assert.match(theLine(c.lines, "merges will bounce"), /names rev-b/);
});

test("an intake change names the human's own veto label", () => {
  const c = switchConfirm(preview({ diff: { ...EMPTY_DIFF, intake_changed: true } }));
  assert.match(theLine(c.lines, "intake label"), /hold label your own vetoes use/);
});

test("next_resume keys are named, with what they mean for the running pane", () => {
  const c = switchConfirm(
    preview({ diff: { ...EMPTY_DIFF, changed: [{ id: "orch", fields: ["model"] }] }, next_resume: ["model", "effort"] })
  );
  const line = theLine(c.lines, "until it is resumed");
  assert.match(line, /model, effort/);
  assert.match(line, /written to the group now/);
});

test("a refusal explains AND still shows the diff", () => {
  const c = switchConfirm(
    preview({
      refusal: "the orchestrator block's cli would change from claude to codex",
      diff: { ...EMPTY_DIFF, orchestrator_cli_changed: true, changed: [{ id: "orch", fields: ["cli"] }] },
    })
  );
  assert.equal(c.canApply, false);
  assert.match(theLine(c.lines, "Cannot be applied"), /claude to codex/);
  // The point of previewing a refusal at all: the human is told what would
  // change, not just that it cannot.
  assert.equal(theLine(c.lines, "Changes orch"), "Changes orch: cli.");
});

test("a preview loomux could not fingerprint declines the apply and says why", () => {
  // `applyWorkflow` accepts a null digest and records "the caller had no
  // confirmation to honour" — which is the wrong trail to write for an apply a
  // human just read a diff and clicked through.
  const c = switchConfirm(preview({ digest: null, diff: { ...EMPTY_DIFF, added: ["w-2"] } }));
  assert.equal(c.canApply, false);
  assert.match(theLine(c.lines, "fingerprint"), /Re-open the diff/);
  // Positive control: the SAME preview with a digest is applicable and says
  // nothing about fingerprints, so the assertion above is about the digest and
  // not about some other property of the fixture.
  const ok = switchConfirm(preview({ digest: "d9", diff: { ...EMPTY_DIFF, added: ["w-2"] } }));
  assert.equal(ok.canApply, true);
  assert.equal(ok.lines.filter((l) => l.includes("fingerprint")).length, 0);
});

// ---------- what Edit… opens (review round 1, finding 1) ----------

const LISTING = {
  workflows: [
    { name: "default", path: ".orrerix/workflow.yml" },
    { name: "b", path: ".orrerix/workflows/b.yml" },
  ],
};

test("a named workflow opens the path the LISTING carries", () => {
  const t = resolveEditTarget("b", LISTING);
  assert.deepEqual(t, { kind: "open", paneName: "b", file: ".orrerix/workflows/b.yml" });
});

test("`default` opens with NO file, which is the pane's own default path", () => {
  // Both halves matter: the pane falls back to the repo's default workflow
  // path, and that absent `file` is also what lets it CREATE the first
  // workflow in a repo that has none.
  const t = resolveEditTarget("default", LISTING);
  assert.deepEqual(t, { kind: "open", paneName: "workflow" });
  assert.equal("file" in t, false);
});

test("a named workflow the listing has LOST refuses rather than opening the default file", () => {
  // The defect this replaces: sending no `file` for a named workflow made the
  // pane open `.orrerix/workflow.yml` under the title `b`, and the designer
  // creates and saves a missing file — so an edit there rewrote the DEFAULT
  // workflow other groups may be running.
  const t = resolveEditTarget("b", { workflows: [{ name: "default", path: ".orrerix/workflow.yml" }] });
  assert.equal(t.kind, "refuse");
  assert.match((t as { reason: string }).reason, /no longer declares a workflow called "b"/);
  assert.match((t as { reason: string }).reason, /file is gone/);
});

test("no listing at all refuses too, and says it could not LOOK rather than that the file is gone", () => {
  // A failed read is not knowledge that the file is missing, and the human's
  // next move differs: retry, not recreate.
  const t = resolveEditTarget("b", null);
  assert.equal(t.kind, "refuse");
  assert.match((t as { reason: string }).reason, /Couldn't read this repo's workflows/);
  assert.doesNotMatch((t as { reason: string }).reason, /gone/);
});

test("NO named workflow can ever reach the pane's default-file fallback", () => {
  // The property, not the three cases: for every listing shape, an `open` for a
  // non-default name carries a real path. This is what the fix is FOR, and it
  // fails if any future branch reintroduces an undefined `file`.
  const listings = [
    null,
    { workflows: [] },
    { workflows: [{ name: "default", path: ".orrerix/workflow.yml" }] },
    LISTING,
    { workflows: [{ name: "b", path: ".loomux/workflows/b.yml" }] },
  ];
  let opened = 0;
  for (const l of listings) {
    const t = resolveEditTarget("b", l);
    if (t.kind === "open") {
      opened++;
      assert.equal(typeof t.file, "string");
      assert.notEqual(t.file, "");
      assert.equal(t.paneName, "b");
    }
  }
  // Positive control: the sweep really did reach the opening branch, so the
  // assertions above are not vacuously satisfied by five refusals.
  assert.equal(opened, 2);
});

test("the path is taken from the listing, never derived — a legacy `.loomux/` repo opens its own file", () => {
  const t = resolveEditTarget("b", { workflows: [{ name: "b", path: ".loomux/workflows/b.yml" }] });
  assert.deepEqual(t, { kind: "open", paneName: "b", file: ".loomux/workflows/b.yml" });
});

test("the target line names the file, and the display name only when there is one", () => {
  const bare = switchConfirm(preview()).lines[0];
  assert.equal(bare, "Switching from default to b (.orrerix/workflows/b.yml).");
  const named = switchConfirm(preview({ display_name: "  Review heavy  " })).lines[0];
  assert.equal(named, 'Switching from default to b — "Review heavy" (.orrerix/workflows/b.yml).');
});
