// Resetting the input modes a program left switched on in a pane's terminal (#3894).
// DOM-free, so the sequence and the toast wording are tested under node:test against
// a real `@xterm/headless` parser (test/termmodes.test.ts).
//
// THE FAILURE. A full-screen program turns on mouse reporting (`ESC[?1003h`, usually
// with SGR encoding `ESC[?1006h`), focus reporting (`?1004h`) or bracketed paste
// (`?2004h`) and is expected to turn them off on exit. When it dies without doing so
// — the classic case is a TUI over `ssh` whose connection closes first — xterm.js keeps
// the mode on and keeps SENDING: every mouse movement becomes `ESC[<35;42;17M` on the
// PTY, which the local shell reads as typed text. The pane is unusable until closed.
//
// THE FIX IS LOCAL. The reset is written into the xterm.js instance (`term.write`),
// never to the PTY: these are the TERMINAL's modes, and only the terminal holds them.
// Nothing here is a `term.reset()` (RIS) — that would wipe the screen and the
// scrollback the human is trying to keep — and nothing touches the PTY size (CLAUDE.md
// hard constraint 1). Each sequence below is a DECRST (`CSI ? Pm l`), which xterm.js
// handles as a pure state change: it draws nothing and answers nothing on `onData`.
//
// WHAT XTERM.JS 6.0.0 SUPPORTS (read off `InputHandler.resetModePrivate` in the
// vendored `@xterm/xterm` 6.0.0 this repo pins, not assumed):
//   9 / 1000 / 1002 / 1003  mouse tracking — all four set `activeProtocol`, and any one
//                            of their resets sets it back to NONE.
//   1006 / 1016              SGR / SGR-pixel encoding — reset to the DEFAULT encoding.
//   1005 / 1015              UTF-8 / urxvt encoding — NOT supported (xtermjs #2507):
//                            xterm.js logs and ignores both the set and the reset. They
//                            are kept in the sequence because they are part of the
//                            mouse-reporting family a program may have asked for, and
//                            the reset of an unsupported mode is a no-op, not an error.
//   1004                     focus reporting.
//   2004                     bracketed paste.
//
// WHAT IS DELIBERATELY NOT RESET. The alternate screen (`?1049`), cursor visibility
// (`?25`), application cursor keys (`?1`) and synchronized output (`?2026`) can be left
// on the same way, but none of them TYPES anything into the shell, and leaving the
// alternate screen changes what is on screen, which this action promises not to do.

/** One mode this action turns off. */
export interface ResetMode {
  /** The DEC private mode number (`CSI ? <mode> l`). */
  readonly mode: number;
  /** What it is, for the docs and the PR — never shown alone in the UI. */
  readonly name: string;
  /** Whether xterm.js 6.0.0 implements it. An unsupported mode's reset is a
   *  no-op there; see the module header. */
  readonly supportedByXterm: boolean;
}

/** Every mode the action turns off, in the order the sequence writes them.
 *  Mouse TRACKING first and the encodings after: once tracking is off nothing
 *  is sent, so the order between the two cannot leak a report either way. */
export const RESET_MODES: readonly ResetMode[] = [
  { mode: 9, name: "X10 mouse reporting", supportedByXterm: true },
  { mode: 1000, name: "VT200 mouse reporting", supportedByXterm: true },
  { mode: 1002, name: "button-event mouse reporting", supportedByXterm: true },
  { mode: 1003, name: "any-event mouse reporting", supportedByXterm: true },
  { mode: 1005, name: "UTF-8 mouse encoding", supportedByXterm: false },
  { mode: 1006, name: "SGR mouse encoding", supportedByXterm: true },
  { mode: 1015, name: "urxvt mouse encoding", supportedByXterm: false },
  { mode: 1016, name: "SGR-pixel mouse encoding", supportedByXterm: true },
  { mode: 1004, name: "focus reporting", supportedByXterm: true },
  { mode: 2004, name: "bracketed paste", supportedByXterm: true },
];

/** The bytes to write into the pane's xterm.js instance — `term.write`, never
 *  the PTY. One DECRST per mode rather than one combined `CSI ? a;b;c l`: the
 *  combined form is equally valid, but a terminal that rejects ONE parameter is
 *  allowed to drop the whole sequence, and this must not depend on that. */
export function terminalModeResetSequence(): string {
  return RESET_MODES.map((m) => `\x1b[?${m.mode}l`).join("");
}

/** The slice of xterm.js's public `term.modes` this action reads — the same
 *  field names, so `Terminal.modes` satisfies it structurally. */
export interface TerminalInputModes {
  readonly mouseTrackingMode: "none" | "x10" | "vt200" | "drag" | "any";
  readonly sendFocusMode: boolean;
  readonly bracketedPasteMode: boolean;
}

/** The human names of the modes that were ON before the reset, for the toast —
 *  so a human who reset a pane that had nothing stuck is told so, rather than
 *  told something was fixed. The encoding is not a separate entry: xterm.js
 *  does not expose it, and an encoding without tracking sends nothing. */
export function modesThatWereOn(m: TerminalInputModes): string[] {
  const on: string[] = [];
  if (m.mouseTrackingMode !== "none") on.push("mouse reporting");
  if (m.sendFocusMode) on.push("focus reporting");
  if (m.bracketedPasteMode) on.push("bracketed paste");
  return on;
}

/** The toast after a reset, from the modes read BEFORE it. */
export function resetToastMessage(before: TerminalInputModes): string {
  const on = modesThatWereOn(before);
  if (on.length === 0) return "Terminal modes reset — none were on.";
  const list = on.length === 1 ? on[0] : `${on.slice(0, -1).join(", ")} and ${on[on.length - 1]}`;
  return `Terminal modes reset — turned off ${list}.`;
}
