// Where a pane joins the split tree — the one decision `Grid.restore`,
// `Grid.restoreMany` and `Grid.placeLeaf` share (#3912). DOM-free so it can be
// pinned under `node --test`; `grid.ts` is its executor.
//
// THE DEFECT THIS EXISTS FOR. All three call sites used to anchor on
// `grid.active` and treat "no leaf for the active pane" as "the grid is empty",
// taking the empty-grid branch: `this.root = leaf` plus
// `rootEl.replaceChildren(pane.el)`. But `active` is NOT guaranteed to be in
// the tree. An agent-initiated reveal (`orch-focus`, emitted by the
// orchestrator's `focus_agent` MCP tool) deliberately runs only `set-active` +
// `focus` on a DOCKED pane — constraint 1 bars it from restoring one
// (`revealPlan`, `panefocus.ts`) — so a docked worker becomes the active pane
// while the orchestrator sits alone in the tree. The human then clicks a dock
// chip, `restore` finds no leaf for the active pane, and replaces the whole
// tree with the restored pane: the orchestrator's element leaves the DOM while
// its leaf stays in the grid's map, so it is neither visible nor docked and no
// gesture can bring it back. Restoring the active-but-docked pane ITSELF was
// worse in a different shape: its fresh leaf had just been registered, so it
// was found as its own anchor and split against itself, again over a new root.
//
// The rule below separates the two questions the old code conflated: "is the
// tree empty?" is answered by the tree, never by whether `active` resolved.

/** Where a joining pane lands: as the new root (only when nothing else is
 *  visible), or beside a pane that is really in the tree. */
export type RejoinPlan<T> = { kind: "root" } | { kind: "beside"; anchor: T };

/** Decide where `joining` enters the split tree.
 *
 *  `preferred` is the caller's choices of neighbour, best first — an explicit
 *  `relativeTo`, then the active pane — and the first one genuinely in the
 *  tree (and not the joining pane itself) wins. So a docked `relativeTo` falls
 *  back to a visible active pane, where the human is, before anything else.
 *  Only when none qualifies does the first visible pane anchor it (the same
 *  "first visible" `minimize` picks as the next active).
 *
 *  `inTree` is every pane currently in the split tree, in the grid's order. The
 *  joining pane is excluded from it here rather than trusted to be absent,
 *  because a caller that registers the new leaf first (as `restore` did) would
 *  otherwise anchor the pane on itself.
 *
 *  `root` is returned ONLY when no other pane is in the tree, so a non-empty
 *  tree is never replaced. */
export function rejoinAnchor<T>(
  joining: T,
  preferred: readonly (T | null | undefined)[],
  inTree: readonly T[]
): RejoinPlan<T> {
  const others = inTree.filter((p) => p !== joining);
  if (others.length === 0) return { kind: "root" };
  for (const p of preferred) {
    if (p != null && p !== joining && others.includes(p)) return { kind: "beside", anchor: p };
  }
  return { kind: "beside", anchor: others[0] };
}
