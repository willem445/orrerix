// Where a pane joins the split tree (#3912). Run with `npm test`.
import { test } from "node:test";
import assert from "node:assert/strict";
import { rejoinAnchor, type RejoinPlan } from "../src/rejoinanchor.ts";

// The grid's two executions of a plan, reduced to the list of panes left in the
// split tree afterwards. `root` is what `Grid.restore` does with a root plan:
// `this.root = leaf` + `rootEl.replaceChildren(pane.el)` — everything that was
// in the tree leaves the DOM. `beside` inserts next to the anchor and keeps the
// rest. Modelling it is what turns "the plan said root" into the bug the human
// saw: a pane that was in the tree is no longer in it.
function afterJoin(inTree: string[], joining: string, plan: RejoinPlan<string>): string[] {
  if (plan.kind === "root") return [joining];
  const at = inTree.indexOf(plan.anchor);
  assert.ok(at >= 0, `anchor ${plan.anchor} must be a pane in the tree`);
  const kept = inTree.filter((p) => p !== joining);
  kept.splice(kept.indexOf(plan.anchor) + 1, 0, joining);
  return kept;
}

test("#3912: restoring a docked worker while ANOTHER docked worker is active keeps the orchestrator", () => {
  // An orchestrator alone in the tree; `focus_agent` on docked worker w1 ran
  // set-active + focus only (an agent reveal may not restore), so the active
  // pane is a docked one. The human now clicks w2's dock chip.
  const inTree = ["orch"];
  const plan = rejoinAnchor("w2", ["w1"], inTree);
  assert.deepEqual(plan, { kind: "beside", anchor: "orch" });
  assert.deepEqual(afterJoin(inTree, "w2", plan), ["orch", "w2"]);
});

test("#3912: restoring the active-but-docked pane itself keeps the orchestrator and never anchors on itself", () => {
  // Same reveal, but the human clicks w1's own chip. `restore` registered the
  // new leaf before looking up the anchor, so the pane found itself.
  const inTree = ["orch"];
  for (const tree of [inTree, [...inTree, "w1"]]) {
    const plan = rejoinAnchor("w1", ["w1"], tree);
    assert.deepEqual(plan, { kind: "beside", anchor: "orch" });
    assert.deepEqual(afterJoin(tree, "w1", plan), ["orch", "w1"]);
  }
});

test("no active pane at all is not an empty tree", () => {
  const plan = rejoinAnchor("w1", [null], ["orch", "w3"]);
  assert.deepEqual(plan, { kind: "beside", anchor: "orch" });
  assert.deepEqual(rejoinAnchor("w1", [undefined], ["orch"]), { kind: "beside", anchor: "orch" });
});

test("a visible preferred pane is honoured, not overridden by the first visible pane", () => {
  // Positive control for the fallback: a rule that always picks `inTree[0]`
  // would pass every test above.
  assert.deepEqual(rejoinAnchor("w1", ["w3"], ["orch", "w3"]), { kind: "beside", anchor: "w3" });
});

test("a docked relativeTo falls back to the VISIBLE active pane before the first visible pane", () => {
  // `placeLeaf` passes [relativeTo, active]. relativeTo w1 is docked; the
  // human is in w3, so the new pane lands beside w3 — not beside `orch`, the
  // first visible pane, which is what collapsing the two with `??` gave.
  const inTree = ["orch", "w3"];
  const plan = rejoinAnchor("new", ["w1", "w3"], inTree);
  assert.deepEqual(plan, { kind: "beside", anchor: "w3" });
  assert.deepEqual(afterJoin(inTree, "new", plan), ["orch", "w3", "new"]);
  // A visible relativeTo still outranks the active pane.
  assert.deepEqual(rejoinAnchor("new", ["orch", "w3"], inTree), { kind: "beside", anchor: "orch" });
  // A docked active pane after a docked relativeTo: first visible pane.
  assert.deepEqual(rejoinAnchor("new", ["w1", "w2"], inTree), { kind: "beside", anchor: "orch" });
});

test("an empty tree takes the pane as its root, whatever is preferred", () => {
  assert.deepEqual(rejoinAnchor("w1", [null], []), { kind: "root" });
  assert.deepEqual(rejoinAnchor("w1", ["w2"], []), { kind: "root" });
  assert.deepEqual(rejoinAnchor("w1", ["w1"], ["w1"]), { kind: "root" });
});

test("across every small state, a join never removes a pane that was in the tree", () => {
  const all = ["orch", "w1", "w2", "w3"];
  let nonEmpty = 0;
  let cases = 0;
  // Every subset of panes in the tree × every joining pane × every preference
  // list of up to two entries, each none or any pane — docked, visible, or the
  // joining one (`placeLeaf` passes [relativeTo, active]).
  const slots = [null, ...all];
  const lists: (string | null)[][] = [[], ...slots.map((a) => [a]), ...slots.flatMap((a) => slots.map((b) => [a, b]))];
  for (let mask = 0; mask < 1 << all.length; mask++) {
    const inTree = all.filter((_, i) => mask & (1 << i));
    for (const joining of all) {
      for (const preferred of lists) {
        cases++;
        const plan = rejoinAnchor(joining, preferred, inTree);
        const others = inTree.filter((p) => p !== joining);
        const after = afterJoin(inTree, joining, plan);
        for (const p of others) assert.ok(after.includes(p), `${p} dropped: ${JSON.stringify({ inTree, joining, preferred, plan })}`);
        assert.ok(after.includes(joining));
        assert.equal(plan.kind === "root", others.length === 0);
        if (plan.kind === "beside") {
          assert.notEqual(plan.anchor, joining);
          // The first preference that is really in the tree wins; only when
          // none is does the first visible pane anchor it.
          const wanted = preferred.find((p) => p != null && others.includes(p)) ?? others[0];
          assert.equal(plan.anchor, wanted, JSON.stringify({ inTree, joining, preferred }));
          nonEmpty++;
        }
      }
    }
  }
  // Vacuity control: the sweep really ran, and really reached the non-empty
  // trees where the drop can happen.
  assert.equal(cases, 16 * 4 * (1 + 5 + 25));
  assert.ok(nonEmpty > 0);
});
