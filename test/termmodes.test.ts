// #3894: "Reset terminal modes" — the sequence a pane writes into its own xterm.js
// instance to recover from a program that left mouse reporting (or focus reporting,
// or bracketed paste) switched on. Pinned against a REAL xterm parser
// (`@xterm/headless`, the same 6.0.0 the app ships), not against the bytes alone: the
// claim is that the modes end up OFF, and only the parser can say so.
import { test } from "node:test";
import assert from "node:assert/strict";
import pkg from "@xterm/headless";
import {
  RESET_MODES,
  modesThatWereOn,
  resetToastMessage,
  terminalModeResetSequence,
  type TerminalInputModes,
} from "../src/termmodes.ts";
const { Terminal } = pkg;

type Term = InstanceType<typeof Terminal>;

function write(term: Term, data: string): Promise<void> {
  return new Promise((resolve) => term.write(data, resolve));
}

function fresh(): Term {
  return new Terminal({ cols: 80, rows: 24, scrollback: 1000, allowProposedApi: true });
}

/** xterm.js does not expose the mouse ENCODING publicly; its core does. Read it
 *  behind a guard so a renamed internal fails loudly here rather than turning
 *  the encoding assertions into `undefined === undefined`. */
function mouseEncoding(term: Term): string {
  const enc = (term as unknown as { _core?: { coreMouseService?: { activeEncoding?: string } } })._core
    ?.coreMouseService?.activeEncoding;
  assert.equal(typeof enc, "string", "xterm internals moved: _core.coreMouseService.activeEncoding is gone");
  return enc as string;
}

/** What the issue's failure looks like: a remote TUI switched every one of these on. */
const STUCK = "\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[?2004h";

test("the reset turns off mouse tracking, SGR encoding, focus reporting and bracketed paste in a real xterm", async () => {
  const term = fresh();
  await write(term, STUCK);
  // Positive control: the fixture really is the stuck state, so the "off"
  // assertions below are about the reset and not about a fixture that never
  // switched anything on.
  assert.equal(term.modes.mouseTrackingMode, "any");
  assert.equal(term.modes.sendFocusMode, true);
  assert.equal(term.modes.bracketedPasteMode, true);
  assert.equal(mouseEncoding(term), "SGR");

  await write(term, terminalModeResetSequence());

  assert.equal(term.modes.mouseTrackingMode, "none");
  assert.equal(term.modes.sendFocusMode, false);
  assert.equal(term.modes.bracketedPasteMode, false);
  assert.equal(mouseEncoding(term), "DEFAULT");
});

test("every mouse-tracking protocol a program can switch on is switched off — X10, VT200, drag and any-event", async () => {
  // One case per protocol, because xterm keeps ONE active protocol: a reset
  // that only names 1003 still clears a 1000 in xterm.js 6 (any of the four
  // resets sets NONE), so only a per-protocol case notices a sequence that
  // relies on that implementation detail rather than naming each mode.
  for (const [mode, expected] of [
    [9, "x10"],
    [1000, "vt200"],
    [1002, "drag"],
    [1003, "any"],
  ] as const) {
    const term = fresh();
    await write(term, `\x1b[?${mode}h`);
    assert.equal(term.modes.mouseTrackingMode, expected, `?${mode}h did not switch tracking on`);
    await write(term, terminalModeResetSequence());
    assert.equal(term.modes.mouseTrackingMode, "none", `?${mode} is still on after the reset`);
  }
});

test("the SGR-pixel encoding (1016) is reset too", async () => {
  const term = fresh();
  await write(term, "\x1b[?1000h\x1b[?1016h");
  assert.equal(mouseEncoding(term), "SGR_PIXELS");
  await write(term, terminalModeResetSequence());
  assert.equal(mouseEncoding(term), "DEFAULT");
});

test("the sequence names every mode the issue lists, as a reset (l), never a set (h)", () => {
  // The issue's list: mouse reporting 1000/1002/1003, encodings 1005/1006/1015,
  // focus 1004, bracketed paste 2004. 1005/1015 are not implemented by
  // xterm.js 6 (see src/termmodes.ts), so no parser test can witness them —
  // this string-level pin is the only thing that does.
  const seq = terminalModeResetSequence();
  for (const mode of [1000, 1002, 1003, 1005, 1006, 1015, 1004, 2004]) {
    assert.ok(seq.includes(`\x1b[?${mode}l`), `?${mode}l missing from the reset`);
  }
  assert.doesNotMatch(seq, /h/, "the reset switches a mode ON");
  // And it is nothing but DECRSTs: each listed mode once, no other bytes.
  assert.equal(seq, RESET_MODES.map((m) => `\x1b[?${m.mode}l`).join(""));
  assert.equal(new Set(RESET_MODES.map((m) => m.mode)).size, RESET_MODES.length);
});

test("the reset keeps the screen and the scrollback — it is not a terminal reset", async () => {
  const term = fresh();
  // Enough lines to push some into scrollback, then the stuck modes on top.
  for (let i = 0; i < 60; i++) await write(term, `line ${i}\r\n`);
  await write(term, "PS C:\\> partial input");
  await write(term, STUCK);
  const buf = term.buffer.active;
  const snapshot = () => {
    const lines: string[] = [];
    for (let y = 0; y < buf.length; y++) lines.push(buf.getLine(y)?.translateToString(true) ?? "");
    return { lines, length: buf.length, cursorX: buf.cursorX, cursorY: buf.cursorY, type: buf.type };
  };
  const before = snapshot();
  // Positive control: there IS scrollback to lose.
  assert.ok(before.length > 24, "the fixture never scrolled");
  assert.equal(before.lines[0], "line 0");

  await write(term, terminalModeResetSequence());

  assert.deepEqual(snapshot(), before);
  // A `term.reset()` (RIS) or an erase is what this must never become.
  const seq = terminalModeResetSequence();
  assert.doesNotMatch(seq, /\x1bc/, "RIS would wipe the screen and scrollback");
  assert.doesNotMatch(seq, /\x1b\[[0-3]?J/, "an erase would clear the screen");
});

test("the reset sends nothing back toward the PTY", async () => {
  // It is written into xterm.js locally, and a DECRST has no reply — so not one
  // byte reaches `onData`, the terminal's only path to the PTY. Pinned with a
  // positive control: a Device Attributes query DOES reach it.
  const term = fresh();
  const sent: string[] = [];
  term.onData((d) => sent.push(d));
  await write(term, STUCK);
  await write(term, terminalModeResetSequence());
  assert.deepEqual(sent, []);
  await write(term, "\x1b[c");
  assert.equal(sent.length, 1, "positive control: onData is wired");
});

test("resetting a pane with nothing stuck is harmless and says so", async () => {
  const term = fresh();
  const before = { ...term.modes };
  assert.equal(resetToastMessage(term.modes), "Terminal modes reset — none were on.");
  await write(term, terminalModeResetSequence());
  // Wraparound and the rest of the modes this action does not own are untouched.
  assert.deepEqual({ ...term.modes }, before);
});

test("the toast names what was on, from the modes read BEFORE the reset", () => {
  const off: TerminalInputModes = { mouseTrackingMode: "none", sendFocusMode: false, bracketedPasteMode: false };
  assert.deepEqual(modesThatWereOn(off), []);
  assert.deepEqual(modesThatWereOn({ ...off, mouseTrackingMode: "vt200" }), ["mouse reporting"]);
  assert.equal(
    resetToastMessage({ ...off, mouseTrackingMode: "any" }),
    "Terminal modes reset — turned off mouse reporting."
  );
  assert.equal(
    resetToastMessage({ mouseTrackingMode: "drag", sendFocusMode: true, bracketedPasteMode: true }),
    "Terminal modes reset — turned off mouse reporting, focus reporting and bracketed paste."
  );
  assert.equal(
    resetToastMessage({ ...off, sendFocusMode: true, bracketedPasteMode: true }),
    "Terminal modes reset — turned off focus reporting and bracketed paste."
  );
});
