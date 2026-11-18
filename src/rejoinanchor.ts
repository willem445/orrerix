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
 *  `preferred` is the caller's choice of neighbour — the active pane, or an
 *  explicit `relativeTo` — and is honoured only when it is genuinely in the
 *  tree and is not the joining pane itself. Otherwise the first visible pane
 *  anchors it (the same "first visible" `minimize` picks as the next active).
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
  preferred: T | null | undefined,
  inTree: readonly T[]
): RejoinPlan<T> {
  const others = inTree.filter((p) => p !== joining);
  if (others.length === 0) return { kind: "root" };
  if (preferred != null && preferred !== joining && others.includes(preferred)) {
    return { kind: "beside", anchor: preferred };
  }
  return { kind: "beside", anchor: others[0] };
}
