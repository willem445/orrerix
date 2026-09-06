// The task board's per-row PRIORITY LADDER (#2937): which of a row's fields
// compete for the compact line, and which wait behind the per-row expand.
//
// The board is read in a normal-width pane docked to the left, and the one
// thing a human needs from a row — its NAME — was the field that lost, because
// every chip and button on the line is `flex: none` and only `.task-title`
// could give ground. The fix is not a narrower chip: it is deciding, once and
// in one place, that most of a row is DETAIL.
//
// DOM-free on purpose. The renderer in `tasksview.ts` still owns whether a row
// HAS a field (a `sprint` badge only exists on a row carrying a sprint) — this
// module owns only where a field is allowed to appear once it exists, so the
// ladder can be tested without a DOM and cannot drift into per-field `if`s
// scattered through a 3000-line render method.

/** Every field a task row can put on screen, compact line and detail alike.
 *
 *  Deliberately an exhaustive union rather than a string: `FIELD_TIER` below is
 *  a `Record` over it, so adding a field without placing it on the ladder is a
 *  `tsc` error rather than a field that silently renders nowhere. */
export type RowField =
  // Tier 1 — what the row IS.
  | "id"
  | "title"
  // Tier 2 — where the work lives.
  | "issue"
  | "pr"
  // Tier 3 — how far along it is.
  | "status"
  | "children"
  // Tier 4 — everything else.
  | "activeBadge"
  | "marker"
  | "cleared"
  | "kind"
  | "sprint"
  | "assignee"
  | "session"
  | "rollupDone"
  | "parentMissing"
  | "ready"
  | "start"
  | "approve"
  | "changes"
  | "proceed"
  | "depPicker"
  | "nest"
  | "kindPicker"
  | "sprintPicker"
  | "ground"
  | "notes"
  | "restore"
  | "delete";

/** A rung of the human's ladder (#2937): 1 is the name, 4 is everything the
 *  human "almost never uses". Only tier 4 hides. */
export type RowTier = 1 | 2 | 3 | 4;

/** The ladder itself — the single place a field's rung is decided.
 *
 *  Tiers 1-3 are the acceptance criterion read literally: "a row shows the full
 *  name and id (wrapping) plus the issue/PR line and a status chip; nothing
 *  else", plus `children_done/children` for a container, which the issue names
 *  as progress alongside status.
 *
 *  Two fields are tier 4 that the issue's own "everything else" list does not
 *  enumerate, and they are the judgment calls in this module:
 *
 *  - `activeBadge` ("● ACTIVE — <agent>") and `marker` ("needs a decision" /
 *    "needs a look"). Both are tier 4 because neither is the only carrier of
 *    its signal: the ROW carries active as a left accent, a glow and a pulse
 *    (`.task-row-active`) and awaiting-human as its own left accent
 *    (`.awaiting-human`), and those cost no horizontal room at all. The chip
 *    names WHO and offers the deep link; the row already says THAT, which is
 *    what a human scanning for "what is left" is reading. */
const FIELD_TIER: Record<RowField, RowTier> = {
  id: 1,
  title: 1,
  issue: 2,
  pr: 2,
  status: 3,
  children: 3,
  activeBadge: 4,
  marker: 4,
  cleared: 4,
  kind: 4,
  sprint: 4,
  assignee: 4,
  session: 4,
  rollupDone: 4,
  parentMissing: 4,
  ready: 4,
  start: 4,
  approve: 4,
  changes: 4,
  proceed: 4,
  depPicker: 4,
  nest: 4,
  kindPicker: 4,
  sprintPicker: 4,
  ground: 4,
  notes: 4,
  restore: 4,
  delete: 4,
};

/** The compact line, in ladder order — the order the renderer appends in, so
 *  the name is never pushed right by something ranked below it. */
const COMPACT_ORDER: readonly RowField[] = ["id", "title", "issue", "pr", "status", "children"];

/** The detail block, in the order the renderer appends it. Grouped as the eye
 *  reads it: what the row IS (badges), then what it is WAITING on, then what
 *  can be DONE to it. */
const DETAIL_ORDER: readonly RowField[] = [
  "activeBadge",
  "marker",
  "cleared",
  "kind",
  "sprint",
  "assignee",
  "session",
  "rollupDone",
  "parentMissing",
  "ready",
  "start",
  "approve",
  "changes",
  "proceed",
  "depPicker",
  "nest",
  "kindPicker",
  "sprintPicker",
  "ground",
  "notes",
  "restore",
  "delete",
];

/** Every field, once — read off the LADDER, not off the two render orders.
 *
 *  Which way round this is derived is the whole point. `FIELD_TIER` is a
 *  `Record<RowField, RowTier>`, so `tsc` already makes it total over the union;
 *  taking the census from it means a field left out of BOTH render orders — a
 *  field that would silently render nowhere, which no compiler can see — is
 *  still in this list, and `every field is placed exactly once, on exactly one
 *  rung` catches it. Derived from `[...COMPACT_ORDER, ...DETAIL_ORDER]` instead,
 *  that same field would be missing from the census as well and every test over
 *  it would agree, vacuously, that nothing was wrong. */
export const ROW_FIELDS: readonly RowField[] = Object.keys(FIELD_TIER) as RowField[];

/** Which rung `f` sits on. */
export function rowFieldTier(f: RowField): RowTier {
  return FIELD_TIER[f];
}

/** What the row puts where, for one expanded-state.
 *
 *  `compact` NEVER changes with `expanded` — expanding reveals, it never
 *  rearranges, so the name and id stay exactly where the eye already found
 *  them and a collapse is not a re-read. */
export interface RowLayout {
  /** The always-visible line, in ladder order. */
  compact: readonly RowField[];
  /** Fields the expand reveals. Empty while collapsed. */
  detail: readonly RowField[];
}

export function rowLayout(expanded: boolean): RowLayout {
  return { compact: COMPACT_ORDER, detail: expanded ? DETAIL_ORDER : [] };
}

/** Does `f` render at all in this expanded-state? The renderer's per-field
 *  guard — it still decides whether the row HAS the field. */
export function rowShows(f: RowField, expanded: boolean): boolean {
  return FIELD_TIER[f] < 4 || expanded;
}

/** Toggle one row's expanded state, returning a FRESH set.
 *
 *  A new set rather than a mutation because this is view state the board
 *  re-renders from: returning a value keeps the caller from being able to
 *  half-apply it, and makes the toggle testable without a `TasksView`. */
export function toggleExpandedRow(expanded: ReadonlySet<string>, id: string): Set<string> {
  const next = new Set(expanded);
  if (!next.delete(id)) next.add(id);
  return next;
}

/** The expand control's accessible label and tooltip.
 *
 *  `hidden` is how many tier-4 fields this particular row actually carries —
 *  the renderer counts them, since only it knows which exist. A row with none
 *  still gets the control (every row has ✕ at minimum), so this never has to
 *  render a "0 more". */
export function expandTitle(expanded: boolean, hidden: number): string {
  if (expanded) return "Hide this row's details";
  return hidden === 1
    ? "Show this row's details (1 more field, and the row's buttons)"
    : `Show this row's details (${hidden} more fields, and the row's buttons)`;
}

/** Is this key the one that toggles the focused expand control?
 *
 *  Enter and Space, per the issue's acceptance criterion — and the control is a
 *  native `<button>`, which already synthesizes a `click` for both. So the
 *  renderer must NOT toggle from a key handler as well: that would fire twice
 *  and the row would flap back to where it started. What the key handler does
 *  with a `true` from here is `stopPropagation()` and nothing else, so an
 *  app-level shortcut cannot swallow the keystroke before the button acts on
 *  it. Never `preventDefault()`: a button activates on Space at KEYUP, and
 *  cancelling the keydown cancels that activation — which is precisely the
 *  acceptance criterion this exists to hold up.
 *
 *  `"Spacebar"` is the legacy `KeyboardEvent.key` spelling; WebView2 sends
 *  `" "`, and matching both costs one comparison. */
export function isExpandToggleKey(key: string): boolean {
  return key === "Enter" || key === " " || key === "Spacebar";
}
