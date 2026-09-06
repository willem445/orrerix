// Marks for the structured pane (#2891 S4).
//
// WHY THIS IS NOT `src/icons.ts`. That module is a VENDORED copy of Lucide,
// pinned by commit in three places and held to it by `test/icons.test.ts` — and
// one of those tests refuses a glyph no surface renders, precisely so the copy
// stays small enough to audit against its licence. Adding eight glyphs to it
// means re-vendoring eight upstream files and updating the provenance pin;
// writing them by hand INTO it would be a false provenance claim on a licence
// surface, which is the one thing a vendored tree must never carry.
//
// So these are hand-authored in Lucide's own geometry — the 24x24 viewBox
// `icons.ts` exports, 2px strokes, round caps and joins, `currentColor` — and
// they are honest about being hand-authored. That is the same call
// `demo/structured-pane/icons.js` made, in the mock the human approved (#2945),
// and for the same reason: where a shape resembles a Lucide primitive it does
// so because the grid and the stroke rules are shared, which is the point of
// having them.
//
// A MARK NEVER NAMES A HUE. It names a role, and CSS resolves the role to an
// `--id-*` token (`.spane-tool[data-family]` in `styles.css`) — the app's own
// role→dye rule, so no call site can pick a colour by hand.
//
// The export is `mark()`, not `icon()`: `test/icons.test.ts` scans every other
// `src/*.ts` for `icon("<name>")` to decide which vendored glyphs are live, and
// a second function of that name in this directory would feed it names its
// registry has never heard of.

// The `.ts` is the rule `transport.ts` states: this is a VALUE import, so
// `node --test` resolves it off disk rather than through Vite.
import { ICON_VIEWBOX } from "./icons.ts";

/** The mark set. Keys are this module's own vocabulary, deliberately NOT
 *  Lucide's upstream names — nothing here is a vendored file and a name that
 *  matched one would invite the assumption that it is. */
const BODY = {
  // ── transcript structure
  chevron: `<path d="m6 9 6 6 6-6"/>`,
  inbound: `<path d="M12 5v14"/><path d="m19 12-7 7-7-7"/>`,
  person: `<path d="M19 21v-2a4 4 0 0 0-4-4H9a4 4 0 0 0-4 4v2"/><circle cx="12" cy="7" r="4"/>`,
  fold: `<path d="M21 8H3"/><path d="M21 12H9"/><path d="M21 16H3"/>`,

  // ── thinking. Three descending strokes: the mark reads as a train of thought
  //    at 13px, which a spiral does not — an involute drawn on a 24px grid
  //    collapses into a blob once scaled to a line of 11px text, and this
  //    block's mark is only ever seen at that size. It belongs to nothing else
  //    in the app, which is the point: thinking is the one block that is the
  //    model's process rather than its product.
  think: `<path d="M4 7h10"/><path d="M7 12h13"/><path d="M4 17h9"/>`,

  // ── tool families
  file: `<path d="M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8l6 6v12a2 2 0 0 1-2 2z"/><path d="M14 2v5a1 1 0 0 0 1 1h5"/>`,
  pencil: `<path d="M21.2 6.8a1 1 0 0 0-4-4L3.8 16.2a2 2 0 0 0-.5.8l-1.3 4.4a.5.5 0 0 0 .6.6l4.4-1.3a2 2 0 0 0 .8-.5z"/><path d="m15 5 4 4"/>`,
  search: `<circle cx="11" cy="11" r="7"/><path d="m21 21-4.3-4.3"/>`,
  terminal: `<path d="m4 17 6-5-6-5"/><path d="M12 19h8"/>`,
  branch: `<circle cx="6" cy="6" r="3"/><circle cx="18" cy="18" r="3"/><path d="M6 9v6a3 3 0 0 0 3 3h6"/>`,
  globe: `<circle cx="12" cy="12" r="10"/><path d="M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20"/><path d="M2 12h20"/>`,
  people: `<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/>`,
  box: `<path d="M21 8v8a2 2 0 0 1-1 1.73l-7 4a2 2 0 0 1-2 0l-7-4A2 2 0 0 1 3 16V8a2 2 0 0 1 1-1.73l7-4a2 2 0 0 1 2 0l7 4A2 2 0 0 1 21 8"/><path d="m3.3 7 8.7 5 8.7-5"/><path d="M12 22V12"/>`,
  bolt: `<path d="M13 2 4.1 12.9a1 1 0 0 0 .8 1.6H11l-1 7.5 8.9-10.9a1 1 0 0 0-.8-1.6H12z"/>`,

  // ── events
  shield: `<path d="M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"/>`,
  compact: `<path d="m8 3-4 4 4 4"/><path d="M4 7h13a3 3 0 0 1 0 6h-1"/><path d="m16 21 4-4-4-4"/><path d="M20 17H7a3 3 0 0 1 0-6h1"/>`,
  layers: `<path d="m12 2 9 5-9 5-9-5z"/><path d="m3 12 9 5 9-5"/><path d="m3 17 9 5 9-5"/>`,
  alert: `<path d="M12 9v4"/><path d="M12 17h.01"/><circle cx="12" cy="12" r="10"/>`,
} as const;

export type MarkName = keyof typeof BODY;

export const MARK_NAMES = Object.keys(BODY) as MarkName[];

/**
 * Render one mark as an inline SVG string.
 *
 * `cls` lands on the `<svg>` so CSS resolves the role to a token; nothing here
 * ever names a colour. The mark is decorative in every consumer — a card
 * carries its tool name, a seam carries its sender — so it is `aria-hidden`
 * and a screen reader does not read the label twice.
 */
export function mark(name: MarkName, cls = ""): string {
  return (
    `<svg viewBox="${ICON_VIEWBOX}" class="${cls}" fill="none" stroke="currentColor"` +
    ` stroke-width="2" stroke-linecap="round" stroke-linejoin="round"` +
    ` aria-hidden="true" focusable="false">${BODY[name]}</svg>`
  );
}
