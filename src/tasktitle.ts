// The board row's TITLE BUDGET (#3261) — how much of a task's name competes for
// the compact line, and what the rest of it costs.
//
// A board read in a docked pane has one line per row, and an agent-written
// title runs to 200+ characters routinely. #2937 gave the name the whole line
// and let it WRAP, which stopped chrome from cutting it off but traded the
// board's scannability for it: three rows of prose each is not a queue you can
// read down. The budget is the other half of that decision — the name keeps
// the line it won, and stops being allowed to take three of them.
//
// NOTHING IS LOST, and that is the load-bearing property: `upsert_task` still
// accepts any title, `tasks.json` still stores it whole, and the full string is
// on the row's tooltip and in the expanded view. This module decides DISPLAY
// only, and it is pure so that the rule can be tested without a DOM.

/** How many characters of a title the compact line shows (#3261).
 *
 *  80 is the issue's figure, and it is a reading measurement rather than a
 *  layout one: it is roughly the line length prose is set at, and a name longer
 *  than that has stopped being a name. The board never has to re-derive it —
 *  the ellipsis is drawn by this module, never by CSS, because a CSS
 *  `text-overflow` cut is a function of the pane's WIDTH and the board must be
 *  able to say, in a test and in a tooltip, exactly what it cut. */
export const TITLE_BUDGET = 80;

/** The character the cut is marked with. One code point, so it costs the same
 *  as it looks — `...` would spend three of the budget on the ellipsis. */
const ELLIPSIS = "…";

/** How far back from the budget a cut will hunt for a word boundary.
 *
 *  Cutting mid-word ("the orchestr…") reads as a broken string; cutting at the
 *  last space before the budget reads as a sentence that stops. 16 characters
 *  is the longest single word worth stepping back over — past that the hunt
 *  would start eating whole words to avoid splitting one, which loses more of
 *  the name than the split did. A title with no space in its last 16 characters
 *  (a URL, a path, one long identifier) is cut mid-token, deliberately. */
const WORD_HUNT = 16;

/** What the compact line shows for `title`, and whether anything was cut.
 *
 *  `full` is always the original, untouched — every caller that needs the whole
 *  name (the tooltip, the expanded view) reads it from here rather than holding
 *  the input separately, so the two cannot drift. */
export interface TitleDisplay {
  /** What to paint on the compact line. */
  shown: string;
  /** The whole title, exactly as stored. */
  full: string;
  /** Did `shown` lose anything? False whenever `shown === full`. */
  truncated: boolean;
}

/** Count of CODE POINTS, not UTF-16 units.
 *
 *  `"x".length` is 1 and `"🙂".length` is 2, so a budget measured in `.length`
 *  cuts an emoji-carrying title early — and, worse, a plain `slice` can land
 *  BETWEEN a surrogate pair and paint a replacement glyph. Everything here
 *  works on the code-point array for both reasons. */
function points(s: string): string[] {
  return Array.from(s);
}

/** Apply the budget to one title. */
export function displayTitle(title: string, budget: number = TITLE_BUDGET): TitleDisplay {
  const full = title;
  const cp = points(title);
  if (cp.length <= budget) return { shown: full, full, truncated: false };
  // The ellipsis is inside the budget, not added to it: a row's name never
  // occupies more than `budget` columns, cut or not.
  const room = budget - 1;
  const head = cp.slice(0, room);
  // Hunt back for a space, but only within `WORD_HUNT` — see the const.
  let cut = head.length;
  for (let i = head.length - 1; i >= head.length - WORD_HUNT && i >= 0; i -= 1) {
    if (head[i] === " ") {
      cut = i;
      break;
    }
  }
  // Trailing spaces would render as a gap before the ellipsis.
  const shown = head.slice(0, cut).join("").replace(/\s+$/, "") + ELLIPSIS;
  return { shown, full, truncated: true };
}

/** Is this title over the budget? The inline editor's SOFT warning (#3261 AC1).
 *
 *  Soft on purpose: the board warns, `upsert_task` still accepts it, and
 *  nothing anywhere refuses a long title. A hard refusal would make the board
 *  unable to edit rows agents can still create, which is a worse board than one
 *  with long names in it. */
export function titleOverBudget(title: string, budget: number = TITLE_BUDGET): boolean {
  return points(title).length > budget;
}

/** The compact title's tooltip.
 *
 *  A cut title puts the WHOLE name there — that is where the rest of it lives
 *  while the row is shut. An uncut one says what the click does instead, since
 *  repeating a name the human can already read in full is noise. */
export function titleTooltip(d: TitleDisplay, expandHint: string): string {
  return d.truncated ? `${d.full}\n\n${expandHint}` : expandHint;
}
