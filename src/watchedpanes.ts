// The WATCHED flag: a pane the human has marked to come back to (#3319).
//
// The human's problem, in their own words: "I have some agents that are just
// idle but I keep them open in case I need them in the near future and then
// other panes I'm actively working with and waiting for results. But when the
// results come back after I step away and come back, it's mixed in with all of
// the other idle panes." So this is a BOOKMARK, not a reading.
//
// WHY IT IS NOT A SECOND `AttentionReason`, and the distinction is the whole
// feature. `attention.ts` answers *what does the agent need* — it is derived
// by the backend from pane text, delivery records and report calls, it appears
// without being asked for, and it is cleared by the thing that caused it.
// `watched` answers *which panes did the human name*, is set only by a human
// gesture, and is cleared only by one. Neither clears the other and both can
// sit on one pane at once; a watched pane that goes `blocked` shows both marks
// and that is the case the feature exists for.
//
// This module is DOM-free and holds no state of its own: the flag lives on the
// `Pane` (and rides its restore record, `PersistedPane.watched`), exactly as
// the attention reason does. What lives here is the part worth testing without
// a browser — the words, the mark, and the two selections a listing surface or
// a chord needs.

/** The minimum a caller must know about a pane for the selections below.
 *
 *  A structural subset on purpose, the same way `panemenu.ts`'s
 *  `PaneConnectState` is one: importing the `Pane` class would make this
 *  module unloadable under `node:test`. */
export interface WatchedPane {
  /** `Pane.key` — the per-window identity a view keys its rows on. */
  readonly key: string;
  readonly watched: boolean;
}

/** The right-click item's label, which is the toggle's current *action* rather
 *  than its current state — "Watch this pane" on an unwatched pane.
 *
 *  A check-mark prefix (the embed menu's idiom, `pane.ts`'s `"✓ "`) was the
 *  other candidate and is wrong here: that menu is a radio group of four sides
 *  where the mark says which one is chosen, and this is a single binary item
 *  where a checked "Watch this pane" and an unchecked one read alike at a
 *  glance. The verb changes instead. */
export function watchMenuLabel(watched: boolean): string {
  return watched ? "Stop watching" : "Watch this pane";
}

/** The glyph every surface marks a watched pane with. One value, so the pane
 *  header, the dock chip, the tab strip and the agents row cannot drift into
 *  four different marks for one flag. */
export const WATCHED_MARK = "◉";

/** The two chords, spelled ONCE (#3320 review round 3, premortem 1).
 *
 *  Several user-facing strings name a chord — the watch chip's tooltip below,
 *  the Agents list's footnote (`agentsviewmodel.ts`), the docs. Each used to
 *  carry its own literal, pinned only by a test asserting that literal against
 *  itself, which is a tautology: rebind `toggle-watch` in `shortcuts.ts` and
 *  every one of those sentences goes on naming a dead key with the whole suite
 *  green.
 *
 *  Spelling them here does not by itself tie them to the binding — nothing in
 *  TypeScript can, because `matchShortcut` is a `switch` over `KeyboardEvent`
 *  and not a table. What it buys is ONE place for a test to tie:
 *  `test/watchedpanes.test.ts` feeds these strings back through `matchShortcut`
 *  as synthetic events and asserts each one really fires the action it claims.
 *  A rebind then reddens there instead of lying to the human. */
export const WATCH_CHORD = "Alt+H";
export const NEXT_WATCHED_CHORD = "Ctrl+Shift+H";

/** The chip's tooltip. Says what UNDOES it, because the mark itself already
 *  says what it is and "never cleared automatically" (#3319 AC5) is only
 *  reassuring if the human can see how to clear it. */
export const WATCHED_TITLE = `You are watching this pane — click, or press ${WATCH_CHORD}, to stop`;

/** How many of these are watched.
 *
 *  Takes the FLAG alone, not a `WatchedPane` — counting needs no identity, and
 *  the narrower parameter is what lets the two counters that exist share it:
 *  the Agents filter chip counts `AgentRow`s and the tab strip counts
 *  `TabPaneInfo`s, and neither is the other's type.
 *
 *  ONE of them routes through here today: `agentsviewmodel.ts`'s chip.
 *  `tabcounts.ts` deliberately does not — it increments inside a loop it is
 *  already making over the same array for the agent and channel counts, and a
 *  second pass to save one line would be a pass. Its call site says so.
 *
 *  (This doc used to claim both callers used it while NEITHER did — #3320
 *  review round 1, N1. A comment that describes callers is a claim about code
 *  elsewhere, and nothing checks it but a reader.) */
export function watchedCount(panes: readonly { readonly watched: boolean }[]): number {
  let n = 0;
  for (const p of panes) if (p.watched) n += 1;
  return n;
}

/** The next watched pane after `fromKey`, in the caller's own order, wrapping.
 *
 *  This is the "show me the watched panes" gesture (#3319 AC4) in its cheapest
 *  honest form: the human presses one chord and arrives at a watched pane,
 *  again to arrive at the next, and the cycle is closed so pressing it enough
 *  times visits every one and returns. There is no panel, no new persisted
 *  filter state, and no PTY resize on the path (constraint 1).
 *
 *  `fromKey` is normally the focused pane. Three cases, each deliberate:
 *   - nothing is watched -> `null`, and the caller says so rather than moving
 *     focus somewhere arbitrary;
 *   - `fromKey` is null, or names a pane that is not in `panes` (it was closed
 *     under us, or focus is on a tab this list does not cover) -> the FIRST
 *     watched pane, which is where a human who just came back wants to land;
 *   - exactly one pane is watched and it IS `fromKey` -> that same key. The
 *     caller gets "you are already on the only one" as an answer it can act on
 *     (re-focus is a no-op) rather than as a `null` it would have to
 *     distinguish from "nothing is watched".
 *
 *  The order is the caller's, and the caller passes panes across EVERY tab —
 *  a watch the human set before stepping away is not confined to whichever tab
 *  they happen to be looking at when they come back. */
export function nextWatchedKey(
  panes: readonly WatchedPane[],
  fromKey: string | null,
): string | null {
  const start = fromKey === null ? -1 : panes.findIndex((p) => p.key === fromKey);
  if (start < 0) {
    const first = panes.find((p) => p.watched);
    return first ? first.key : null;
  }
  for (let step = 1; step <= panes.length; step++) {
    const p = panes[(start + step) % panes.length];
    if (p.watched) return p.key;
  }
  return null;
}

/** What the minimized-dock chip shows for a watched pane.
 *
 *  Shaped like `dockChipAttention` so the two read the same way at the call
 *  site in `grid.ts`, and separate from it so a chip can carry both. */
export interface DockChipWatched {
  readonly watched: boolean;
  readonly title: string;
}

export function dockChipWatched(paneName: string, watched: boolean): DockChipWatched {
  return {
    watched,
    title: watched ? `${paneName} — watched` : paneName,
  };
}
