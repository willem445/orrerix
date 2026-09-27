// Pure paste/copy keydown gesture decisions for terminal panes (#370) —
// pasteflow.ts. Pins the key matching (plain Ctrl+V pastes only when the
// pasteOnPlainCtrlV setting allows it, Ctrl+Shift+V always pastes, plain
// Ctrl+C copies only WITH a selection — else it must stay SIGINT,
// AltGr/Ctrl+Alt+V is never eaten as a paste) and the keyDisposition enum
// that drives pane.ts's preventDefault() calls.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  isPasteKey,
  isCopyKey,
  isConditionalCopyKey,
  keyDisposition,
  selectionIsLive,
  type PasteKeyEvent,
} from "../src/pasteflow.ts";

const key = (overrides: Partial<PasteKeyEvent>): PasteKeyEvent => ({
  ctrlKey: false,
  shiftKey: false,
  altKey: false,
  code: "",
  ...overrides,
});

test("plain Ctrl+V pastes when the setting allows it (#370 — the gesture nearly everyone reaches for first)", () => {
  assert.equal(isPasteKey(key({ ctrlKey: true, code: "KeyV" }), true), true);
});

test("plain Ctrl+V passes through to the pane when the setting is off (vim VISUAL BLOCK / readline quoted-insert)", () => {
  assert.equal(isPasteKey(key({ ctrlKey: true, code: "KeyV" }), false), false);
});

test("Ctrl+Shift+V always pastes, regardless of the setting", () => {
  assert.equal(isPasteKey(key({ ctrlKey: true, shiftKey: true, code: "KeyV" }), true), true);
  assert.equal(isPasteKey(key({ ctrlKey: true, shiftKey: true, code: "KeyV" }), false), true);
});

test("Shift+V alone (no Ctrl) is not a paste", () => {
  assert.equal(isPasteKey(key({ shiftKey: true, code: "KeyV" }), true), false);
});

test("Ctrl+Alt+V (AltGr on many layouts) is never a paste, even with the setting on", () => {
  assert.equal(isPasteKey(key({ ctrlKey: true, altKey: true, code: "KeyV" }), true), false);
});

test("Ctrl+Shift+Alt+V is not a paste either — Alt held always defers to the pane", () => {
  assert.equal(isPasteKey(key({ ctrlKey: true, shiftKey: true, altKey: true, code: "KeyV" }), true), false);
});

test("Ctrl+Shift+C is the explicit copy key", () => {
  assert.equal(isCopyKey(key({ ctrlKey: true, shiftKey: true, code: "KeyC" })), true);
});

test("plain Ctrl+C does not match the EXPLICIT copy key (isConditionalCopyKey owns it instead)", () => {
  assert.equal(isCopyKey(key({ ctrlKey: true, code: "KeyC" })), false);
});

test("plain Ctrl+C matches the CONDITIONAL copy key", () => {
  assert.equal(isConditionalCopyKey(key({ ctrlKey: true, code: "KeyC" })), true);
});

test("Ctrl+Shift+C does not match the conditional matcher — isCopyKey owns it", () => {
  assert.equal(isConditionalCopyKey(key({ ctrlKey: true, shiftKey: true, code: "KeyC" })), false);
});

test("Ctrl+Alt+C does not match the conditional matcher either (mirrors the paste-side AltGr guard)", () => {
  assert.equal(isConditionalCopyKey(key({ ctrlKey: true, altKey: true, code: "KeyC" })), false);
});

// ---------- keyDisposition (#402 review: the DOM layer must preventDefault
// on every disposition except "pass" — see pasteflow.ts's own doc comment
// for the double-paste bug this collapsing-to-one-enum exists to prevent) ----------

test("keyDisposition: Ctrl+Shift+C is 'copy' regardless of selection (explicit gesture, harmless no-op without one)", () => {
  assert.equal(keyDisposition(key({ ctrlKey: true, shiftKey: true, code: "KeyC" }), true, false), "copy");
  assert.equal(keyDisposition(key({ ctrlKey: true, shiftKey: true, code: "KeyC" }), true, true), "copy");
});

test("keyDisposition: plain Ctrl+C with a selection is 'copy' (#402 third round — this is the fix)", () => {
  assert.equal(keyDisposition(key({ ctrlKey: true, code: "KeyC" }), true, true), "copy");
});

test("keyDisposition: plain Ctrl+C with NO selection is 'pass' — CRITICAL, this is what keeps SIGINT reachable", () => {
  assert.equal(keyDisposition(key({ ctrlKey: true, code: "KeyC" }), true, false), "pass");
});

test("keyDisposition: plain Ctrl+V is 'paste' when the setting allows it", () => {
  assert.equal(keyDisposition(key({ ctrlKey: true, code: "KeyV" }), true, false), "paste");
});

test("keyDisposition: plain Ctrl+V is 'pass' when the setting is off", () => {
  assert.equal(keyDisposition(key({ ctrlKey: true, code: "KeyV" }), false, false), "pass");
});

test("keyDisposition: Ctrl+Shift+V is 'paste' regardless of the setting", () => {
  assert.equal(keyDisposition(key({ ctrlKey: true, shiftKey: true, code: "KeyV" }), false, false), "paste");
});

test("keyDisposition: an unrelated key is 'pass'", () => {
  assert.equal(keyDisposition(key({ ctrlKey: true, code: "KeyA" }), true, true), "pass");
});

// ---------- pane-kind/selection matrix (#402 third round) ----------
//
// keyDisposition takes NO pane-kind input at all — there is nothing in its
// signature to distinguish "plain terminal pane" from "agent pane" from
// "orchestrator pane". These tests pin that directly: a plain terminal
// pane's keydown and an agent pane's keydown, built from identical
// (event, setting, selection) inputs, are passed through the exact same
// call and MUST produce the exact same disposition — there is no branch
// left anywhere for the two to diverge on. The bug this exists to catch:
// copy appearing to work in one pane kind and not another despite there
// being no pane-kind-aware code in this module or in pane.ts's wiring.

interface PaneLikeInput {
  label: string;
  e: PasteKeyEvent;
  hasSelection: boolean;
}

const PANE_KINDS: readonly PaneLikeInput[] = [
  { label: "plain terminal pane", e: key({ ctrlKey: true, code: "KeyC" }), hasSelection: true },
  { label: "agent pane", e: key({ ctrlKey: true, code: "KeyC" }), hasSelection: true },
];

test("pane-kind matrix: Ctrl+C with a selection is 'copy' in every pane kind, identically", () => {
  const dispositions = PANE_KINDS.map((p) => keyDisposition(p.e, true, p.hasSelection));
  assert.deepEqual(dispositions, ["copy", "copy"]);
});

test("pane-kind matrix: Ctrl+C with NO selection passes through as interrupt in every pane kind, identically", () => {
  const noSelection = PANE_KINDS.map((p) => ({ ...p, hasSelection: false }));
  const dispositions = noSelection.map((p) => keyDisposition(p.e, true, p.hasSelection));
  assert.deepEqual(dispositions, ["pass", "pass"]);
});

// ---------- #3595: only a VISIBLE selection makes plain Ctrl+C copy ----------
//
// The reported bug: in a pane running a long-lived process, Ctrl+C copied
// instead of interrupting. An xterm selection is anchored to buffer rows, so
// output carries it off screen while it stays selected — and the wiring asked
// only `!!term.getSelection()`. These pin the replacement rule end to end
// through `keyDisposition`, with the viewport a 24-row screen whose top row is
// absolute buffer row 1000 (1000 lines of scrollback above it).

const SCREEN = { top: 1000, rows: 24 }; // rows 1000..1023 are on screen
const CTRL_C = key({ ctrlKey: true, code: "KeyC" });
const CTRL_SHIFT_C = key({ ctrlKey: true, shiftKey: true, code: "KeyC" });
const range = (sy: number, sx: number, ey: number, ex: number) => ({
  start: { x: sx, y: sy },
  end: { x: ex, y: ey },
});
const ctrlC = (text: string, r: ReturnType<typeof range> | undefined) =>
  keyDisposition(CTRL_C, true, selectionIsLive(text, r, SCREEN));

test("#3595: a selection scrolled off the TOP of the screen does not swallow Ctrl+C — it interrupts", () => {
  // Selected ten lines above the viewport, then the dev server's output kept
  // coming: nothing is highlighted on screen, so Ctrl+C is ^C.
  assert.equal(ctrlC("npm run dev", range(990, 0, 990, 11)), "pass");
});

test("#3595: a selection below the screen (the human scrolled up to read) does not swallow Ctrl+C either", () => {
  assert.equal(ctrlC("later line", range(1030, 2, 1031, 5)), "pass");
});

test("#3595: a selection on screen still copies — the fix must not cost the copy gesture", () => {
  assert.equal(ctrlC("Local: http://localhost:5173", range(1010, 2, 1010, 30)), "copy");
});

test("#3595: a selection straddling either screen edge is visible, so it copies", () => {
  assert.equal(ctrlC("spans the top edge", range(995, 0, 1000, 4)), "copy");
  assert.equal(ctrlC("spans the bottom edge", range(1023, 0, 1040, 4)), "copy");
  assert.equal(ctrlC("covers the whole screen", range(900, 0, 1100, 4)), "copy");
});

test("#3595: the screen's first and last rows are both on screen (off-by-one at each edge)", () => {
  assert.equal(ctrlC("top row", range(1000, 0, 1000, 3)), "copy");
  assert.equal(ctrlC("bottom row", range(1023, 0, 1023, 3)), "copy");
  assert.equal(ctrlC("row above", range(999, 0, 999, 3)), "pass");
  assert.equal(ctrlC("row below", range(1024, 0, 1024, 3)), "pass");
});

test("#3595: a selection whose end is column 0 of the top row paints nothing on screen, so it interrupts", () => {
  // xterm's end column is exclusive: rows 990..999 are highlighted, row 1000
  // is not — the triple-click-a-line shape, one line above the screen.
  assert.equal(ctrlC("line above\n", range(990, 0, 1000, 0)), "pass");
  // …while one more column onto the top row is visible.
  assert.equal(ctrlC("line above\nx", range(990, 0, 1000, 1)), "copy");
});

test("#3595: a reversed range reads the same as its ordered twin", () => {
  // Discriminating case: dragged UP from below the screen to above it. Read
  // unordered, its "first" row (1100) is below the screen and its "last" (900)
  // above it, so neither end alone is on screen though the middle covers it.
  assert.equal(ctrlC("whole screen, dragged upward", range(1100, 4, 900, 0)), "copy");
  assert.equal(ctrlC("visible", range(1010, 30, 1010, 2)), "copy");
  assert.equal(ctrlC("off screen", range(991, 4, 990, 0)), "pass");
});

test("#3595: no selection at all is never live, whatever the range says", () => {
  assert.equal(ctrlC("", range(1010, 2, 1010, 30)), "pass");
  assert.equal(ctrlC("stale text, no range", undefined), "pass");
});

test("#3595: Ctrl+Shift+C still copies an off-screen selection — the explicit gesture does not ask", () => {
  assert.equal(
    keyDisposition(CTRL_SHIFT_C, true, selectionIsLive("npm run dev", range(990, 0, 990, 11), SCREEN)),
    "copy"
  );
});
test("#3595: a zero-row viewport shows nothing, so even a selection spanning it is not live", () => {
  // Without the guard, bottom = top - 1 and a range straddling `top` still
  // satisfies `firstRow <= bottom && lastRow >= top`.
  assert.equal(selectionIsLive("x", range(900, 0, 1100, 4), { top: 1000, rows: 0 }), false);
  // Control: the same range on the real 24-row screen IS live, so the `false`
  // above comes from the empty viewport, not from the range.
  assert.equal(selectionIsLive("x", range(900, 0, 1100, 4), SCREEN), true);
});
