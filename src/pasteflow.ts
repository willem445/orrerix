// Pure copy/paste keydown gesture decisions for terminal panes (#370).
// DOM-free so node:test can pin the key matching without a browser; pane.ts
// wires the actual keydown handler. (A right-click Copy/Paste context menu
// lived here briefly — removed in the #402 second live-demo round: its paste
// path was unreliable and the human chose not to iterate on a second
// right-click-specific native-event interaction rather than keep debugging
// it. Right-click on a terminal is back to doing nothing, its pre-#370
// state; Ctrl+C/Ctrl+Shift+C — below — are the supported copy gestures.)
//
// THE BUG THIS EXISTS TO FIX. Terminal panes bound paste to Ctrl+Shift+V only
// (Windows Terminal convention — plain Ctrl+V is a shell's rare "quoted
// insert next char" readline binding) and swallowed every clipboard-read
// failure with `.catch(() => {})`. Users hit plain Ctrl+V from muscle memory
// and got nothing, with no way to tell "wrong key" from "clipboard blocked"
// apart. The fix: a genuine read failure is a menu item / keystroke that
// visibly does nothing rather than silently nothing — see clipboard.ts's
// readClipboard, which pane.ts surfaces via showToast — and plain Ctrl+V
// pastes TOO, but only when `pasteOnPlainCtrlV` opts in (default true;
// see settings.ts). It is not a free win: it costs vim's VISUAL BLOCK mode,
// readline's quoted-insert, and any TUI/agent CLI that wants the raw key —
// review of #370 found the first cost (vim) undocumented and the "every
// terminal emulator already binds it" justification for eating it overstated
// (Windows Terminal does by default; gnome-terminal, iTerm2, kitty, and
// alacritty default to Ctrl+Shift+V precisely to leave plain Ctrl+V for the
// program in the pane). A setting, not an unconditional interception, is the
// same call `Alt+V` made from the other direction (#155, shortcuts.ts) —
// loomux stopped intercepting it once it was shown to steal a key an agent
// pane needed; Ctrl+V gets the option instead of the same unconditional grab.

/** The subset of a KeyboardEvent the gesture matchers need — kept minimal so
 *  tests build one as a plain object instead of a real DOM event. */
export interface PasteKeyEvent {
  ctrlKey: boolean;
  shiftKey: boolean;
  altKey: boolean;
  code: string;
}

/** Is this keydown a terminal paste? Ctrl+Shift+V always is (the original,
 *  unconditional binding — kept). Plain Ctrl+V is a paste only when
 *  `plainCtrlVPastes` is true — the #370 review's blocking finding: binding
 *  it unconditionally silently steals Ctrl+V from vim's VISUAL BLOCK mode,
 *  readline's quoted-insert, and any TUI/agent CLI that wants the raw key
 *  (the exact failure mode `Alt+V` was deliberately left alone for, #155 —
 *  shortcuts.ts). `settings.ts`'s `pasteOnPlainCtrlV` (default true) is the
 *  opt-out; pane.ts reads it and passes the current value in here on every
 *  keydown rather than this module reading global state, so it stays pure
 *  and testable without a settings singleton.
 *
 *  `!e.altKey` guards a keyboard-layout collision the plain-Ctrl+V case
 *  introduced: on layouts where AltGr (= Ctrl+Alt) + V types a character,
 *  Ctrl+Alt+V would otherwise be swallowed as a paste instead of reaching
 *  the shell as that character. The original Ctrl+Shift+V binding never had
 *  this problem (AltGr doesn't hold Shift), so gate the whole match on it. */
export function isPasteKey(e: PasteKeyEvent, plainCtrlVPastes: boolean): boolean {
  if (e.altKey || !e.ctrlKey || e.code !== "KeyV") return false;
  return e.shiftKey || plainCtrlVPastes;
}

/** Is this keydown the terminal's EXPLICIT copy gesture? Ctrl+Shift+C —
 *  always intercepted (see `keyDisposition`), copying when there's a
 *  selection and otherwise a harmless no-op. Shift is why it never doubles
 *  as an accidental interrupt: Ctrl+Shift+<letter> sends the identical
 *  control byte a plain Ctrl+<letter> would (Shift doesn't change what a
 *  terminal's Ctrl-modifier maps to), but nobody's muscle memory reaches for
 *  Shift+C to send SIGINT, so eating it unconditionally is safe. */
export function isCopyKey(e: PasteKeyEvent): boolean {
  return e.ctrlKey && e.shiftKey && e.code === "KeyC";
}

/** Is this keydown the terminal's CONDITIONAL copy gesture — plain Ctrl+C?
 *  #402 (third live-demo round): copy only worked via the explicit
 *  Ctrl+Shift+C above, but plain Ctrl+C is the gesture most people reach
 *  for first (mouse-select, then Ctrl+C — the universal convention outside
 *  a terminal), and it did nothing when nothing else was true either. This
 *  matcher alone is NOT enough to decide "copy" — see `keyDisposition`:
 *  plain Ctrl+C is a terminal's actual interrupt key, so it may only
 *  resolve to copy when a selection exists; with no selection it MUST fall
 *  through to the shell as ^C, unconditionally, or SIGINT would be
 *  unreachable from the keyboard whenever anything happened to be
 *  selected. `!e.shiftKey` excludes Ctrl+Shift+C, which `isCopyKey` already
 *  owns (and, unlike this one, is never a fallback interrupt). */
export function isConditionalCopyKey(e: PasteKeyEvent): boolean {
  return e.ctrlKey && !e.shiftKey && !e.altKey && e.code === "KeyC";
}

/** What a terminal keydown resolves to — pinned as ONE enum, not independent
 *  booleans, because of a live-demo finding (#402 review): the DOM layer
 *  originally called `isCopyKey`/`isPasteKey` and, on a match, `return
 *  false` from xterm's `attachCustomKeyEventHandler` WITHOUT calling
 *  `e.preventDefault()`. Per xterm's own contract, returning `false` means
 *  only "don't let xterm itself process this key" — it does NOT suppress the
 *  browser's native handling of the same key. For plain Ctrl+V specifically,
 *  the browser's native paste accelerator then fired on xterm's own focused
 *  textarea, which xterm ALSO listens to natively (`handlePasteEvent`,
 *  bound to the DOM `"paste"` event) — so the clipboard text landed twice:
 *  once from our own `pasteFromClipboard()`, once from xterm's untouched
 *  native path. `"copy"`/`"paste"` are the dispositions that MUST
 *  preventDefault; `"pass"` is the only one that must not — collapsing to
 *  one enum makes forgetting the preventDefault for one branch, but not
 *  another, a one-branch typo instead of independently-fixed call sites. See
 *  pane.ts for the DOM wiring (the preventDefault calls themselves, and the
 *  capture-phase native-`"paste"`-event kill switch that backstops paste
 *  regardless of what triggers the browser's native paste).
 *
 *  `hasSelection` is what makes plain Ctrl+C's copy/interrupt split
 *  possible: it's DOM/xterm runtime state, not something derivable from the
 *  KeyboardEvent alone, so the caller reads it once per keydown and passes it
 *  in. It means a LIVE selection — `selectionIsLive` below, never a bare
 *  `!!term.getSelection()` (#3595) — same discipline as
 *  `plainCtrlVPastes` reading `settings.ts`'s live value. This function is
 *  identical for every pane kind (plain terminal, agent, orchestrator) —
 *  there is no pane-kind branch anywhere in this module or in pane.ts's
 *  keydown wiring, deliberately: see the pane-kind/selection matrix in
 *  pasteflow.test.ts, which pins that a terminal pane and an agent pane
 *  produce the SAME disposition for the same (event, selection) input. */
export type TermKeyDisposition = "copy" | "paste" | "pass";

export function keyDisposition(
  e: PasteKeyEvent,
  plainCtrlVPastes: boolean,
  hasSelection: boolean
): TermKeyDisposition {
  if (isCopyKey(e)) return "copy";
  if (isConditionalCopyKey(e)) return hasSelection ? "copy" : "pass";
  if (isPasteKey(e, plainCtrlVPastes)) return "paste";
  return "pass";
}

/** A selection's extent, in xterm's `getSelectionPosition()` coordinates:
 *  ABSOLUTE buffer rows (scrollback included, 0-based) and columns, with the
 *  end column EXCLUSIVE — xterm's own `selectionText` reads the last row up to,
 *  not including, `end.x`. */
export interface SelectionRange {
  start: { x: number; y: number };
  end: { x: number; y: number };
}

/** The rows on screen, in the same absolute-row coordinates:
 *  `term.buffer.active.viewportY` and `term.rows`. */
export interface ViewportRows {
  top: number;
  rows: number;
}

/** Does the pane's selection count as "a selection" for plain Ctrl+C (#3595)?
 *  Only when it is VISIBLE — some painted part of it intersects the rows on
 *  screen. Ctrl+Shift+C does not ask this: it is the explicit gesture and
 *  copies whatever is selected.
 *
 *  THE BUG THIS EXISTS TO FIX. An xterm selection is anchored to buffer rows,
 *  so new output or a scroll carries it off screen while it stays selected.
 *  In a pane running a dev server, a line selected a minute ago is long gone
 *  from view, and yet `!!term.getSelection()` still called it a selection:
 *  plain Ctrl+C copied text the human could not see instead of interrupting
 *  the process, which is exactly the reported symptom. The rule the human
 *  gets instead is what they can see: a visible highlight means Ctrl+C
 *  copies, and no visible highlight means it interrupts.
 *
 *  Why not also drop the selection when new output arrives: the commonest copy
 *  in a streaming pane is a line of that stream (an error, a URL), selected
 *  while more lines keep arriving. Dropping it on output would turn that copy
 *  into an interrupt of the very process being read. Visibility separates the
 *  two cases; "output happened since" does not.
 *
 *  The other half of Windows Terminal's rule — a selection is gone once any
 *  other key is pressed — xterm already does itself: every key that produces
 *  input fires `onUserInput`, and its SelectionService clears the selection on
 *  that (`SelectionService` constructor, xterm 6.0).
 *
 *  A selection ending at column 0 of a later row paints nothing on that row
 *  (the end column is exclusive), so that row does not make it visible. */
export function selectionIsLive(
  text: string,
  range: SelectionRange | undefined,
  viewport: ViewportRows
): boolean {
  if (!text || !range || viewport.rows <= 0) return false;
  // Order the two ends: xterm hands them over ordered today, but a reversed
  // pair must not read as "nothing painted".
  const [a, b] =
    range.start.y < range.end.y || (range.start.y === range.end.y && range.start.x <= range.end.x)
      ? [range.start, range.end]
      : [range.end, range.start];
  const firstRow = a.y;
  const lastRow = b.x === 0 && b.y > a.y ? b.y - 1 : b.y;
  const top = viewport.top;
  const bottom = viewport.top + viewport.rows - 1;
  return firstRow <= bottom && lastRow >= top;
}
