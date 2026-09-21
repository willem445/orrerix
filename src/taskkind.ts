// The board row's LEVEL MARK (#3261) — which pigment says where a row sits on
// the Agile ladder, and what the mark tells a human it means.
//
// DOM-free on purpose, like `boardrow.ts` beside it: the renderer owns whether
// a row HAS a mark, this module owns what the mark IS. That split is what lets
// "epic is ember, and an unlabelled row is not a fifth level" be a test rather
// than a CSS class somebody has to read a stylesheet to check.
//
// The pigments themselves, and why the level could not take the row's LEFT
// ACCENT, are `theme.ts` §KIND_HUES.

import { KINDS, UNLABELLED_KIND } from "./taskboard.ts";

/** Every value this module maps — the four levels plus the flat board's row. */
export type KindKey = (typeof KINDS)[number] | typeof UNLABELLED_KIND;

/** The levels plus `unlabelled`, in ladder order, outermost first.
 *
 *  Derived from `KINDS` rather than spelled again: the ladder's order and its
 *  membership are decided in `taskboard.ts` (against the backend's own
 *  `TASK_KINDS`), and a second literal here would be a list to keep in step. */
export const KIND_KEYS: readonly KindKey[] = [...KINDS, UNLABELLED_KIND];

/** What a row's `kind` field is NOT: a level this board can place.
 *
 *  Only a hand-edited `tasks.json` or a newer binary can produce one — the
 *  backend refuses an unknown kind on write — so the board already paints it as
 *  BROKEN (`.task-chip.kind-broken`, in the state-danger dye) rather than as
 *  a fifth level. Keeping it out of `KindKey` is what stops this module
 *  laundering it into "unlabelled", which would replace a loud "your board file
 *  is wrong" with a quiet "this row has no level". */
export const UNKNOWN_KIND = "unknown";

/** Which key a row's raw `kind` field maps to.
 *
 *  `null`, `undefined` and `""` are all the flat board's row — the backend's
 *  empty-string clear means "no level", not "a level called empty".
 *
 *  IDEMPOTENT: feeding a key back in returns it, so a caller that already holds
 *  a `KindKey` (the filter chips, which iterate `KIND_KEYS`) gets the same
 *  answer as one holding a raw row field. Without that, `UNLABELLED_KIND` — a
 *  sentinel no task ever stores — would come back as `unknown` and the
 *  unlabelled filter chip would paint as a broken row. */
export function kindKey(kind: string | null | undefined): KindKey | typeof UNKNOWN_KIND {
  const k = (kind ?? "").trim();
  if (k === "" || k === UNLABELLED_KIND) return UNLABELLED_KIND;
  return (KINDS as readonly string[]).includes(k) ? (k as KindKey) : UNKNOWN_KIND;
}

/** The CSS custom property that paints this key, or `null` where the board has
 *  no level to paint.
 *
 *  A `var(--kind-*)` name rather than a hex: `test/theme.test.ts` is what pins
 *  those names to `theme.ts`'s values, and a module that spelled a colour would
 *  be a second place the palette lives. `null` for an unknown kind, so a broken
 *  row gets no level mark at all and its own broken chip is the only thing
 *  saying anything — never a mark painted `var(--kind-undefined)`. */
export function kindToken(kind: string | null | undefined): string | null {
  const k = kindKey(kind);
  return k === UNKNOWN_KIND ? null : `--kind-${k}`;
}

/** What the mark's tooltip says.
 *
 *  The mark is a colour, and a colour on its own is a quiz. Every row's mark
 *  carries the word too — which is also what makes the feature usable by
 *  someone who cannot tell two of the hues apart (theme.ts §KIND_HUES, the CVD
 *  paragraph): colour is the scanning channel, never the only one. */
export function kindTitle(kind: string | null | undefined): string {
  const k = kindKey(kind);
  if (k === UNKNOWN_KIND) {
    return `${(kind ?? "").trim()} is not a level this board knows — only a hand-edited tasks.json can hold it`;
  }
  return k === UNLABELLED_KIND
    ? "No level — this row is on the flat board and may sit anywhere"
    : `${k.charAt(0).toUpperCase()}${k.slice(1)} — its level on the epic ⊃ feature ⊃ story ⊃ task ladder`;
}

/** Does this board show level marks at all?
 *
 *  Pay-for-what-you-use, the rule the kind and sprint FILTER CHIPS already
 *  follow (`tasksview.ts`): a board where nobody has ever set a `kind` is a
 *  flat board, and painting an "unlabelled" mark on all 400 of its rows would
 *  add a column of noise saying nothing. One row with a level turns the marks
 *  on for the whole board, including the rows that have none — at that point
 *  "this one is not in the tree" IS information. */
export function boardShowsKindMarks(tasks: readonly { kind?: string | null }[]): boolean {
  return tasks.some((t) => {
    const k = kindKey(t.kind);
    return k !== UNLABELLED_KIND && k !== UNKNOWN_KIND;
  });
}
