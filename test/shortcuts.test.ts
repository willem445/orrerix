// matchShortcut (shortcuts.ts) — focused on the #379 tab-reorder bindings
// added alongside the existing next/prev-tab bracket keys, and the
// modifier-set boundaries that keep them from colliding.
import { test } from "node:test";
import assert from "node:assert/strict";
import { matchShortcut } from "../src/shortcuts.ts";

function evt(overrides: Partial<KeyboardEvent> & { code: string }): KeyboardEvent {
  return {
    ctrlKey: false,
    shiftKey: false,
    altKey: false,
    ...overrides,
  } as KeyboardEvent;
}

test("Ctrl+Shift+Alt+BracketRight moves the active tab right", () => {
  assert.equal(
    matchShortcut(evt({ ctrlKey: true, shiftKey: true, altKey: true, code: "BracketRight" })),
    "move-tab-right"
  );
});

test("Ctrl+Shift+Alt+BracketLeft moves the active tab left", () => {
  assert.equal(
    matchShortcut(evt({ ctrlKey: true, shiftKey: true, altKey: true, code: "BracketLeft" })),
    "move-tab-left"
  );
});

test("Ctrl+Shift+BracketRight (no Alt) is still plain next-tab, not move", () => {
  assert.equal(matchShortcut(evt({ ctrlKey: true, shiftKey: true, code: "BracketRight" })), "next-tab");
});

test("Ctrl+Shift+BracketLeft (no Alt) is still plain prev-tab, not move", () => {
  assert.equal(matchShortcut(evt({ ctrlKey: true, shiftKey: true, code: "BracketLeft" })), "prev-tab");
});

test("Alt+BracketRight alone (no Ctrl+Shift) matches nothing", () => {
  assert.equal(matchShortcut(evt({ altKey: true, code: "BracketRight" })), null);
});

// --- the progress timeline's binding (#608) --------------------------------

test("Alt+W toggles the progress timeline", () => {
  assert.equal(matchShortcut(evt({ altKey: true, code: "KeyW" })), "toggle-timeline");
});

test("Alt+W does not disturb the audit log's own Alt+A", () => {
  // The two views are siblings on the same panes; a copy-paste that pointed
  // both at one action would be invisible until someone pressed Alt+A.
  assert.equal(matchShortcut(evt({ altKey: true, code: "KeyA" })), "toggle-audit");
});

// --- the NEEDS-YOU panel's binding (#1091) ---------------------------------

test("Alt+Q toggles the NEEDS-YOU panel", () => {
  assert.equal(matchShortcut(evt({ altKey: true, code: "KeyQ" })), "toggle-decisions");
});

test("Alt+Q does not disturb the task board's own Alt+T", () => {
  // Same sibling-collision check the timeline/audit pair above makes: the panel
  // and the board are both orchestrator-pane embeds and sit beside each other
  // in the same switch, which is exactly where a copy-paste points two keys at
  // one action and nobody notices until a key stops working.
  assert.equal(matchShortcut(evt({ altKey: true, code: "KeyT" })), "toggle-tasks");
});

test("Alt+SHIFT+Q is not the panel — the guard the source comment's argument rests on", () => {
  // The vendor references document no Alt+Shift default at all, so that chord
  // is UNVERIFIED rather than confirmed free, and the binding deliberately does
  // not take it. `!e.shiftKey` on the Alt block is what makes that true; this
  // is what would redden if the guard were dropped.
  assert.equal(matchShortcut(evt({ altKey: true, shiftKey: true, code: "KeyQ" })), null);
});

test("plain Q and Ctrl+Q are untouched — Ctrl+Q is Copilot CLI's queue-a-message", () => {
  // Named because it is the one adjacent binding the reference sweep DID find
  // on this letter. loomux must leave it to the CLI in the pane.
  assert.equal(matchShortcut(evt({ code: "KeyQ" })), null);
  assert.equal(matchShortcut(evt({ ctrlKey: true, code: "KeyQ" })), null);
});

test("Ctrl+Shift+W is still close-pane, not the timeline", () => {
  // W is now bound under two different modifier sets; the modifier guards are
  // what keep them apart, and close-pane is the destructive one.
  assert.equal(matchShortcut(evt({ ctrlKey: true, shiftKey: true, code: "KeyW" })), "close-pane");
});

// --- autosize (#936) -------------------------------------------------------

test("Ctrl+Shift+A autosizes the panes", () => {
  assert.equal(matchShortcut(evt({ ctrlKey: true, shiftKey: true, code: "KeyA" })), "autosize-panes");
});

test("plain Ctrl+A is NOT taken — it is the shell's and the agent's start-of-line", () => {
  // Claude Code's interactive-mode reference documents `Ctrl+A` as "Move cursor
  // to start of current line", and readline binds it the same way. Autosize
  // rides the SHIFTED chord precisely so that one keeps reaching the pane; a
  // guard that let Ctrl+A through to this action would be a silent regression
  // inside every agent and shell pane.
  assert.equal(matchShortcut(evt({ ctrlKey: true, code: "KeyA" })), null);
  assert.equal(matchShortcut(evt({ code: "KeyA" })), null);
});

test("Alt+A still opens the audit log, and Ctrl+Shift+Alt+A is nobody's", () => {
  // A is now bound under two modifier sets on the same panes — the same
  // collision the timeline/audit pair above guards against.
  assert.equal(matchShortcut(evt({ altKey: true, code: "KeyA" })), "toggle-audit");
  assert.equal(
    matchShortcut(evt({ ctrlKey: true, shiftKey: true, altKey: true, code: "KeyA" })),
    null
  );
});

test("plain W, Ctrl+W and Alt+Shift+W are not the timeline (Ctrl+W is the shell's kill-word)", () => {
  assert.equal(matchShortcut(evt({ code: "KeyW" })), null);
  // Ctrl+W must keep reaching the shell: it is readline's unix-word-rubout,
  // and swallowing it inside an agent pane would be a real regression.
  assert.equal(matchShortcut(evt({ ctrlKey: true, code: "KeyW" })), null);
  assert.equal(matchShortcut(evt({ altKey: true, shiftKey: true, code: "KeyW" })), null);
});

test("Alt+J opens or focuses the To-Do pane (#3263 S4)", () => {
  assert.equal(matchShortcut(evt({ altKey: true, code: "KeyJ" })), "open-todo");
});

test("Alt+J is an ALT chord and nothing else claims J", () => {
  // The modifier boundary, asserted the way the tab-reorder tests assert theirs:
  // the app takes exactly one of the four J chords, so a shell's Ctrl+J
  // (opencode's `input_newline`, Claude Code's `chat:newline`) and a bare `j`
  // (this pane's own move-down key, and vim's everywhere) both stay with the
  // pane that has focus.
  assert.equal(matchShortcut(evt({ code: "KeyJ" })), null, "bare j is not an app chord");
  assert.equal(matchShortcut(evt({ ctrlKey: true, code: "KeyJ" })), null, "Ctrl+J stays with the CLI");
  assert.equal(
    matchShortcut(evt({ altKey: true, shiftKey: true, code: "KeyJ" })),
    null,
    "Alt+Shift is UNVERIFIED across the CLIs, so the block's !shiftKey guard withholds it"
  );
  assert.equal(matchShortcut(evt({ ctrlKey: true, altKey: true, code: "KeyJ" })), null);
});

test("Alt+H watches the focused pane, Ctrl+Shift+H goes to the next watched one (#3319)", () => {
  assert.equal(matchShortcut(evt({ altKey: true, code: "KeyH" })), "toggle-watch");
  assert.equal(matchShortcut(evt({ ctrlKey: true, shiftKey: true, code: "KeyH" })), "next-watched");
});

test("plain Ctrl+H stays with the CLI — it is the backspace byte (#3319)", () => {
  // THE ONE THAT WOULD BE A REAL REGRESSION. Claude Code's keybindings
  // reference lists Ctrl+H under "Reserved shortcuts" — "Sends the ASCII
  // backspace byte" — and cannot rebind it; Copilot CLI's command reference
  // binds it to "Delete the previous character". Taking Ctrl+H at the app
  // level would eat backspace inside every agent and shell pane, silently.
  //
  // What holds it is the `e.shiftKey` requirement on the Ctrl+Shift block, and
  // that guard is invisible at the one line that adds the chord — so it is
  // asserted here rather than trusted, the way the Ctrl+A guard above is.
  assert.equal(matchShortcut(evt({ ctrlKey: true, code: "KeyH" })), null);
  assert.equal(matchShortcut(evt({ code: "KeyH" })), null, "bare h is not an app chord");
  assert.equal(
    matchShortcut(evt({ altKey: true, shiftKey: true, code: "KeyH" })),
    null,
    "Alt+Shift is UNVERIFIED across the CLIs, so the block's !shiftKey guard withholds it",
  );
  assert.equal(
    matchShortcut(evt({ ctrlKey: true, shiftKey: true, altKey: true, code: "KeyH" })),
    null,
    "the tab-reorder block takes only the bracket keys",
  );
});

test("the watch pair is two chords on one letter, and they are different actions", () => {
  // H is now bound under two modifier sets, the same collision the audit/
  // timeline and Ctrl+Shift+A/Alt+A pairs above guard against — and here it is
  // deliberate, because the pair IS one idea. What must hold is that the two
  // chords stay distinct actions: a switch whose first matching case wins would
  // otherwise let one silently shadow the other.
  const toggle = matchShortcut(evt({ altKey: true, code: "KeyH" }));
  const next = matchShortcut(evt({ ctrlKey: true, shiftKey: true, code: "KeyH" }));
  assert.notEqual(toggle, next);
  assert.notEqual(toggle, null);
  assert.notEqual(next, null);
});

test("no two app shortcuts answer to the same chord", () => {
  // The guard that makes adding a chord safe, rather than a grep. Every chord in
  // `matchShortcut`'s four modifier blocks is enumerated here and each must map
  // to a DISTINCT action — a new binding that silently shadows an existing one
  // (the switch's first matching case wins, so the older one just stops firing)
  // reddens here instead of being discovered by a human whose Alt+T stopped
  // working.
  const CODES = [
    "KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH", "KeyI", "KeyJ",
    "KeyK", "KeyL", "KeyM", "KeyN", "KeyO", "KeyP", "KeyQ", "KeyR", "KeyS", "KeyT",
    "KeyU", "KeyV", "KeyW", "KeyX", "KeyY", "KeyZ",
    "BracketLeft", "BracketRight", "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "F2",
  ];
  const MODS = [
    { ctrlKey: true, shiftKey: true },
    { ctrlKey: true, shiftKey: true, altKey: true },
    { altKey: true },
    {},
  ];
  const seen = new Map<string, string>();
  const dupes: string[] = [];
  let bound = 0;
  for (const mods of MODS) {
    for (const code of CODES) {
      const action = matchShortcut(evt({ ...mods, code }));
      if (action === null) continue;
      bound += 1;
      const chord =
        (mods.ctrlKey ? "Ctrl+" : "") +
        (mods.shiftKey ? "Shift+" : "") +
        (mods.altKey ? "Alt+" : "") +
        code;
      const prior = seen.get(action);
      if (prior !== undefined) dupes.push(`${action} answers to both ${prior} and ${chord}`);
      else seen.set(action, chord);
    }
  }
  // The population control (#1209): this sweep's success shape is an empty
  // list, which is what a sweep that matched nothing also produces.
  assert.ok(bound >= 25, `only ${bound} chords matched — the sweep is blind, not the map clean`);
  assert.deepEqual(dupes, [], `two chords fire one action:\n${dupes.join("\n")}`);
});
