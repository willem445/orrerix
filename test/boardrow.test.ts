// Unit tests for the task board's per-row priority ladder (#2937) —
// src/boardrow.ts. Run with `npm test`.
//
// What these test is INTENT, not the shape of the two arrays: the ladder exists
// so the human can read a task's NAME in a normal-width docked pane, so every
// assertion here is a property that would go false if that stopped being true —
// the name leading the line, nothing ranked below it preceding it, the "almost
// never used" chrome staying off the compact line in BOTH states, and Approve
// still being reachable once the row is open.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  consumeExpandFocus,
  expandTitle,
  isExpandToggleKey,
  ROW_FIELDS,
  rowFieldTier,
  rowLayout,
  pruneExpandFocus,
  rowShows,
  toggleExpandedRow,
  titleClickToggles,
  type ClickPathNode,
  type RowField,
} from "../src/boardrow.ts";

/** The fields the human's ladder puts behind the expand, quoted from #2937:
 *  "assignee, session, sprint, links, deps, notes, the Approve / Changes /
 *  Proceed buttons, kind". Named here rather than derived from the module, so
 *  the test disagrees with the module when the module changes. */
const NEVER_COMPACT: readonly RowField[] = [
  "assignee",
  "session",
  "sprint",
  "kind",
  "depPicker",
  "notes",
  "approve",
  "changes",
  "proceed",
];

test("the name and the id lead the compact row, ahead of everything else", () => {
  // #3261 puts one thing between them: the 8px LEVEL MARK. It is tier 1 on
  // #2937's own reasoning — this ladder ranks how much of the LINE a field
  // costs against how often it is read — and it is pinned here as part of the
  // prefix rather than tolerated by a looser assertion, so a fifth compact
  // field cannot arrive unargued.
  const { compact } = rowLayout(false);
  assert.deepEqual(compact.slice(0, 3), ["id", "kindMark", "title"]);
  assert.equal(rowFieldTier("kindMark"), 1, "the level mark is tier 1 or it does not belong on the prefix");
});

test("nothing ranked below the name is allowed to precede it on the line", () => {
  // The defect this whole issue is about: every chip on `.task-top` is
  // `flex: none`, so anything appended AHEAD of the title takes its width out
  // of the title's. The ladder is only worth anything if the order obeys it.
  const { compact } = rowLayout(false);
  const tiers = compact.map(rowFieldTier);
  assert.deepEqual(tiers, [...tiers].sort((a, b) => a - b), `${compact.join(",")} is not in ladder order`);
});

test("the compact row carries the name, the id, issue/PR and progress — and nothing else", () => {
  // #2937's acceptance criterion, literally: "a row shows the full name and id
  // (wrapping) plus the issue/PR line and a status chip; nothing else", plus
  // children_done/children, which #2937 ranks as progress alongside status.
  //
  // #3261 amends it in exactly two ways, and both are stated here rather than
  // left to be inferred from the array: the name is no longer shown WHOLE on
  // this line (it is cut at `TITLE_BUDGET`, with the rest on the tooltip and in
  // the expanded row — src/tasktitle.ts), and the level MARK joins the line.
  // Nothing else did.
  assert.deepEqual(
    [...rowLayout(false).compact],
    ["id", "kindMark", "title", "issue", "pr", "status", "children"]
  );
});

test("collapsing a row hides the chrome and keeps the four things that matter", () => {
  // Named fields, not `rowFieldTier(f) < 4` — asking the ladder to agree with
  // itself passes under any ladder at all, the flat one this issue replaces
  // included. These are the human's own words in #2937 turned into two lists.
  for (const f of ["id", "kindMark", "title", "issue", "pr", "status", "children"] as const) {
    assert.equal(rowShows(f, false), true, `${f} is hidden on a collapsed row`);
  }
  for (const f of NEVER_COMPACT) {
    assert.equal(rowShows(f, false), false, `${f} still renders on a collapsed row`);
  }
  for (const f of ["activeBadge", "marker", "cleared", "ready", "nest", "delete"] as const) {
    assert.equal(rowShows(f, false), false, `${f} still renders on a collapsed row`);
  }
});

test("expanding a row reveals everything and takes nothing away", () => {
  for (const f of ROW_FIELDS) assert.equal(rowShows(f, true), true, `${f} is missing when expanded`);
  // Positive control on the sweep above: it is an assertion over a list, and a
  // list that had gone empty would pass it in silence.
  assert.ok(ROW_FIELDS.length >= 20, `only ${ROW_FIELDS.length} fields on the ladder`);
});

test("expanding never rearranges the compact line — the name stays where the eye found it", () => {
  assert.deepEqual([...rowLayout(true).compact], [...rowLayout(false).compact]);
});

test("the chrome the human almost never uses never reaches the compact line, open or shut", () => {
  for (const f of NEVER_COMPACT) {
    assert.ok(!rowLayout(false).compact.includes(f), `${f} is on the collapsed compact line`);
    assert.ok(!rowLayout(true).compact.includes(f), `${f} is on the expanded compact line`);
    assert.ok(rowLayout(true).detail.includes(f), `${f} is not revealed by the expand either`);
  }
});

test("Approve, Changes and Proceed stay reachable once the row is expanded", () => {
  // The acceptance criterion's own carve-out: they move, they are not removed,
  // and their gating on status is the renderer's and is untouched by the ladder.
  const { detail } = rowLayout(true);
  for (const f of ["approve", "changes", "proceed", "start"] as const) {
    assert.ok(detail.includes(f), `${f} is unreachable when the row is expanded`);
    assert.equal(rowShows(f, true), true);
  }
});

test("a collapsed row reveals nothing at all", () => {
  assert.deepEqual([...rowLayout(false).detail], []);
});

test("every field is placed exactly once, on exactly one rung", () => {
  // The ladder is a `Record<RowField, RowTier>`, so `tsc` already refuses a
  // field with no rung. What it cannot see is a field given a rung and then left
  // out of both render orders — it would have a tier and render NOWHERE, and
  // `renderTask` would file it into a slot no loop ever reads. This is the
  // assertion that catches that, which is why `ROW_FIELDS` is the ladder's own
  // census and not `[...compact, ...detail]`: derived the other way it would
  // agree with itself and pass, vacuously, on exactly the defect it is for.
  const seen = new Set<RowField>();
  for (const f of ROW_FIELDS) {
    assert.ok(!seen.has(f), `${f} is listed twice`);
    seen.add(f);
  }
  const compact = new Set(rowLayout(true).compact);
  for (const f of ROW_FIELDS) {
    const inCompact = compact.has(f);
    const inDetail = rowLayout(true).detail.includes(f);
    assert.ok(inCompact !== inDetail, `${f} is in ${inCompact ? "both" : "neither"} render order`);
  }
  // And the other direction: a render order must not name a field the ladder
  // does not carry. Both loops run over a list, so both need the floor.
  const census = new Set(ROW_FIELDS);
  for (const f of [...rowLayout(true).compact, ...rowLayout(true).detail]) {
    assert.ok(census.has(f), `${f} is rendered but is not on the ladder`);
  }
  assert.equal(seen.size, rowLayout(true).compact.length + rowLayout(true).detail.length);
});

test("a tier is one of the four rungs the human named", () => {
  for (const f of ROW_FIELDS) {
    assert.ok([1, 2, 3, 4].includes(rowFieldTier(f)), `${f} is on rung ${rowFieldTier(f)}`);
  }
});

test("toggling expands, toggling again collapses, and neither mutates what it was given", () => {
  const before = new Set<string>(["t-1"]);
  const opened = toggleExpandedRow(before, "t-2");
  assert.deepEqual([...before], ["t-1"], "the input set was mutated");
  assert.deepEqual([...opened].sort(), ["t-1", "t-2"]);
  const shut = toggleExpandedRow(opened, "t-2");
  assert.deepEqual([...shut], ["t-1"]);
  assert.deepEqual([...opened].sort(), ["t-1", "t-2"], "the input set was mutated");
});

test("Enter and both spellings of Space toggle the focused chevron; nothing else does", () => {
  for (const k of ["Enter", " ", "Spacebar"]) assert.equal(isExpandToggleKey(k), true, k);
  for (const k of ["Tab", "Escape", "ArrowDown", "a", "Space", ""]) {
    assert.equal(isExpandToggleKey(k), false, k);
  }
});

test("the row that was toggled takes focus back after the re-render", () => {
  // The defect: toggling re-renders the board, which destroys the button the
  // human just pressed. Without the hook, Enter/Space on the chevron works once
  // and then strands a keyboard user at the top of the board.
  const { focus, pending } = consumeExpandFocus("t-7", "t-7");
  assert.equal(focus, true);
  assert.equal(pending, null, "the hook must be consumed as it fires");
});

test("the focus hook fires once and never on a later render", () => {
  // This board re-renders on every agent write. A hook that survived its own
  // firing would yank focus back to a row the human had already left.
  let pending: string | null = "t-7";
  const first = consumeExpandFocus(pending, "t-7");
  pending = first.pending;
  assert.equal(first.focus, true);
  const second = consumeExpandFocus(pending, "t-7");
  assert.equal(second.focus, false, "the hook fired twice");
  assert.equal(second.pending, null);
});

test("every other row on the same render leaves the hook armed and untouched", () => {
  // Rows are built in board order, so the row that was toggled is usually not
  // the first one asked. If a non-matching row cleared the hook, the toggled row
  // further down the board would never get focus.
  let pending: string | null = "t-7";
  for (const other of ["t-1", "t-2", "t-3"]) {
    const r = consumeExpandFocus(pending, other);
    assert.equal(r.focus, false, `${other} stole the focus`);
    assert.equal(r.pending, "t-7", `${other} cleared the hook`);
    pending = r.pending;
  }
  assert.equal(consumeExpandFocus(pending, "t-7").focus, true, "the toggled row never got it");
});

test("no pending hook means no row grabs focus on an ordinary refresh", () => {
  for (const id of ["t-1", "t-7"]) {
    const r = consumeExpandFocus(null, id);
    assert.equal(r.focus, false, `${id} took focus with nothing pending`);
    assert.equal(r.pending, null);
  }
});

test("a hook naming a row that has been deleted is dropped, and a live one is kept", () => {
  const live = new Set(["t-1", "t-7"]);
  assert.equal(pruneExpandFocus("t-7", live), "t-7");
  assert.equal(pruneExpandFocus("t-9", live), null, "a hook for a deleted row survived the prune");
  assert.equal(pruneExpandFocus(null, live), null);
  assert.equal(pruneExpandFocus("t-1", new Set()), null, "an empty board kept a hook");
});

test("the expand tooltip counts what is actually hidden, and reads as English at one", () => {
  assert.match(expandTitle(false, 1), /1 more field\b/);
  assert.match(expandTitle(false, 7), /7 more fields\b/);
  assert.match(expandTitle(true, 7), /^Hide\b/);
  assert.doesNotMatch(expandTitle(false, 1), /1 more fields/);
});

// ---------------------------------------------------------------------------
// Clicking the title area (#3261)
//
// The property under test is the one the acceptance criterion states: the title
// AREA opens the row, and every control that sits inside a row keeps its own
// click. So each case below is a control this board really builds, reached
// through a path this board really produces — not a synthetic tag list.
// ---------------------------------------------------------------------------

/** A path step, spelled the way the DOM hands one over. */
function node(tagName: string, ...classes: string[]): ClickPathNode {
  return { tagName, classList: classes };
}

/** The title area itself — the element the listener sits on, so it is the last
 *  step of every path. */
const TITLE_AREA = node("DIV", "task-title-area");

test("a click on the title text itself opens the row", () => {
  assert.equal(titleClickToggles([node("SPAN", "task-title"), TITLE_AREA]), true);
  // And a click on the area's own padding, with nothing in between.
  assert.equal(titleClickToggles([TITLE_AREA]), true);
});

test("every control this board puts in a row keeps its own click", () => {
  // Each entry is a real control off the row: the id chip's button, the issue
  // and PR link chips, the status <select>, the inline title editor, the
  // description editor, the expand button itself.
  const controls: [string, ClickPathNode[]][] = [
    ["the expand button", [node("BUTTON", "task-expand")]],
    ["an issue chip", [node("BUTTON", "task-chip", "issue", "link")]],
    ["the status select", [node("SELECT", "task-status", "st-queued")]],
    ["an option inside it", [node("OPTION"), node("SELECT", "task-status")]],
    ["the inline title editor", [node("INPUT", "dlg-input", "task-title-input")]],
    ["the description editor", [node("TEXTAREA", "task-desc-input")]],
    ["a link out of the row", [node("A")]],
  ];
  for (const [what, path] of controls) {
    assert.equal(
      titleClickToggles([...path, TITLE_AREA]),
      false,
      `${what} must keep its own click, not fold the row`
    );
  }
});

test("a control is honoured wherever on the path it sits, not only at the target", () => {
  // A click lands on a <span> INSIDE a button — the glyph inside the chip. The
  // target is inert; the button on the way out is what owns the click.
  assert.equal(
    titleClickToggles([node("SPAN", "chip-glyph"), node("BUTTON", "task-chip"), TITLE_AREA]),
    false,
    "a click inside a button is the button's, however deep the target is"
  );
});

test("a contenteditable in the title area is an editor, not a toggle target", () => {
  const editable: ClickPathNode = { tagName: "DIV", classList: [], isContentEditable: true };
  assert.equal(titleClickToggles([editable, TITLE_AREA]), false);
});

test("an editor's own wrapper is honoured by CLASS, since its tag is inert", () => {
  // The title editor's wrapper is a plain <div>; without the class rule the tag
  // check alone would let a click on it fold the row out from under the input.
  assert.equal(titleClickToggles([node("DIV", "task-title-input"), TITLE_AREA]), false);
  assert.equal(titleClickToggles([node("DIV", "task-desc-edit"), TITLE_AREA]), false);
  // A class that is merely styling is NOT a control — the rule is keyed on the
  // two the renderer stamps for this purpose, so it cannot creep.
  assert.equal(titleClickToggles([node("DIV", "task-title"), TITLE_AREA]), true);
});

test("a click nobody can account for does not fold the row", () => {
  // Default-deny: an empty path means the caller could not say where the click
  // came from, and a row folding on that is the failure the rule prevents.
  assert.equal(titleClickToggles([]), false);
});

test("the tag rule is case-insensitive, as the DOM's own tagName is not always", () => {
  // `tagName` is upper-case for HTML elements and lower-case inside SVG, and an
  // SVG glyph inside a chip is exactly what this board draws.
  assert.equal(titleClickToggles([node("button", "task-expand"), TITLE_AREA]), false);
});
