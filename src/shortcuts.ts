// App-level keyboard shortcuts, shared between the document handler and
// each terminal's custom key handler (which must decline them so they
// bubble up instead of being eaten by the shell).

export type ShortcutAction =
  | "split-right"
  | "split-down"
  | "autosize-panes"
  | "close-pane"
  | "new-tab"
  | "close-tab"
  | "next-tab"
  | "prev-tab"
  | "move-tab-left"
  | "move-tab-right"
  | "toggle-sessions"
  | "toggle-git"
  | "toggle-issues"
  | "toggle-files"
  | "open-editor"
  | "open-todo"
  | "toggle-tasks"
  | "toggle-decisions"
  | "toggle-audit"
  | "toggle-timeline"
  | "toggle-tokens"
  | "toggle-group"
  | "focus-compose"
  | "voice-ptt"
  | "maximize-pane"
  | "minimize-pane"
  | "rename-pane"
  | "focus-left"
  | "focus-right"
  | "focus-up"
  | "focus-down"
  | "toggle-watch"
  | "next-watched";

export function matchShortcut(e: KeyboardEvent): ShortcutAction | null {
  if (e.ctrlKey && e.shiftKey && !e.altKey) {
    switch (e.code) {
      case "KeyE": return "split-right";
      case "KeyO": return "split-down";
      case "KeyW": return "close-pane";
      case "KeyP": return "toggle-sessions";
      // Ctrl+Shift+A (#936), the repurpose the removed agents mode (#194) left
      // this key parked for: A for Autosize — even out every pane in the tab.
      // It sits with the other layout gestures (E/O split, M maximize) rather
      // than in the Alt+<key> space, which is overlays and focus.
      //
      // CHECKED against the agent CLIs' own references per the
      // agent-cli-reference discipline — which is not the same as verified
      // free, and keeping the two apart is the whole point of writing it out:
      //   - Claude Code's interactive-mode reference DOES document `Ctrl+A`
      //     ("Move cursor to start of current line") and `Ctrl+_` /
      //     `Ctrl+Shift+-` (undo), and does NOT document Ctrl+Shift+A. The
      //     unshifted Ctrl+A a shell or agent actually uses is untouched by
      //     this, and test/shortcuts.test.ts pins that it stays untouched.
      //   - Copilot CLI's reference pages are SILENT on it — its CLI reference
      //     index carries no key table at all — so that one is UNVERIFIED, not
      //     confirmed free. A reference that lists no bindings is not evidence
      //     of no conflict.
      // This chord is withheld from every terminal pane (isAppShortcut), so a
      // CLI that does bind it loses it with no escape hatch. Settling that
      // needs a human with Copilot running, not a doc read: it is a demo
      // checklist item, open at the time of writing.
      case "KeyA": return "autosize-panes";
      case "KeyM": return "maximize-pane";
      // Ctrl+Shift+H (#3319) — jump to the next WATCHED pane, fleet-wide and
      // wrapping. The other half of Alt+H, and the shifted form of the same
      // letter on purpose: the pair is one idea, "mark" and "go to the marks".
      //
      // It is in the Ctrl+Shift block rather than the Alt one because that is
      // where this app's cross-tab NAVIGATION already lives (the bracket keys
      // page between tabs, Ctrl+Shift+A evens out the grid) while Alt+<key> is
      // overlays and per-pane gestures. This one leaves the tab you are on.
      //
      // CHECKED against every CLI this repo spawns, per the
      // agent-cli-reference discipline, with the references fetched:
      //   - Claude Code's keybindings reference carries exactly two
      //     Ctrl+Shift defaults — `chat:undo` (Ctrl+_, Ctrl+Shift+-) and
      //     `selection:copy` (Ctrl+Shift+C) — and no Ctrl+Shift+H.
      //   - Copilot CLI's command reference carries NO Ctrl+Shift row at all;
      //     its Ctrl rows are A/E/H/K/U/W/G/L/V/Space/Enter/Q/R/P. Plain
      //     Ctrl+H is theirs (delete previous character) and stays theirs: the
      //     `e.shiftKey` requirement on this block is what keeps it reaching
      //     the pane, and test/shortcuts.test.ts pins that.
      //   - opencode's keybinds reference has one ctrl+shift default,
      //     `input_delete_line` (ctrl+shift+d); no ctrl+shift+h.
      //   - pi's ctrl+shift defaults are up/down/f/g only; no ctrl+shift+h.
      //   - Codex documents no Ctrl+Shift binding at all — UNVERIFIED rather
      //     than confirmed free, the same standing as its Alt row.
      // Not a WebView2 accelerator either (Ctrl+Shift+F/G are, which is why
      // the git overlay is Alt+G).
      case "KeyH": return "next-watched";
      // Project tabs (#63). T=new, K=close; the bracket keys page between tabs
      // (VSCode-style) and stay clear of Alt+arrows (pane focus) and the browser
      // accelerators WebView2 eats (Ctrl+Tab / Ctrl+PageUp).
      case "KeyT": return "new-tab";
      case "KeyK": return "close-tab";
      case "BracketRight": return "next-tab";
      case "BracketLeft": return "prev-tab";
    }
  }
  // Tab REORDER (#379): same bracket keys as switching, plus Alt — the
  // keyboard alternative to dragging. The issue's suggested Ctrl+Shift+
  // PgUp/PgDn would have been a fresh convention; this instead extends the
  // bracket-key pair the app already uses for tab navigation, so "move" reads
  // as "switch, but Alt for real."
  if (e.ctrlKey && e.shiftKey && e.altKey) {
    switch (e.code) {
      case "BracketRight": return "move-tab-right";
      case "BracketLeft": return "move-tab-left";
    }
  }
  if (e.altKey && !e.ctrlKey && !e.shiftKey) {
    switch (e.code) {
      case "KeyM": return "minimize-pane";
      case "ArrowLeft": return "focus-left";
      case "ArrowRight": return "focus-right";
      case "ArrowUp": return "focus-up";
      case "ArrowDown": return "focus-down";
      // Alt+G, not Ctrl+Shift+G: WebView2 consumes that as its
      // find-previous accelerator before the page ever sees it.
      case "KeyG": return "toggle-git";
      case "KeyI": return "toggle-issues";
      // Alt+F (files). Free in loomux; not a WebView2 accelerator (Ctrl+F is —
      // that's why the in-file find uses a button, not Ctrl+F). (#174)
      case "KeyF": return "toggle-files";
      case "KeyE": return "open-editor";
      // Alt+J (#3263 S4) — the To-Do pane. A pane, never an overlay: a to-do
      // list is a station you keep open, not a look you take (content-panes.md
      // "Why a pane and not a bigger overlay"), so this OPENS one in the active
      // grid or FOCUSES the one already there.
      //
      // CHECKED against every CLI this repo spawns, per the
      // agent-cli-reference discipline, with the references fetched rather
      // than recalled:
      //   - Claude Code's interactive-mode reference documents Alt+B/D/F/M/O/
      //     P/T/V/Y and the arrows, and its keybindings reference spells
      //     `chat:newline` as Ctrl+J; neither lists Alt+J.
      //   - Copilot CLI's command reference documents Alt+V, Alt+Enter and
      //     Alt+arrows; no Alt+J.
      //   - opencode's keybinds reference binds `input_newline` to
      //     `shift+return,ctrl+return,alt+return,ctrl+j` — Ctrl+J, not Alt+J —
      //     and its Alt rows are a/e/f/b/d/return/arrows.
      //   - pi's keybindings reference has no Alt+J default (a/b/d/f/y/v/
      //     enter/arrows/backspace/delete).
      //   - Codex's reference documents no Alt binding at all, so that one is
      //     UNVERIFIED rather than confirmed free — a reference that lists no
      //     Alt row is not evidence of no conflict.
      //   - Readline leaves `\ej` unbound in this repo's bash, the same shape
      //     Alt+W and Alt+Q rely on, and Alt+J is not a WebView2 accelerator.
      // Alt+H was the other candidate and comes out equally free; J takes it
      // because `h` is the conventional HELP letter and is the likelier of the
      // two to be claimed by a CLI adding a help overlay. Both are bound by
      // pi's *vim example config*, which a user opts into by hand — a
      // user-config collision rather than a shipped default, and the same for
      // either letter, so it does not separate them.
      case "KeyJ": return "open-todo";
      // Alt+H (#3319) — watch, or stop watching, the focused pane. H for
      // HIGHLIGHT, which is the human's own verb in the issue ("I want to
      // easily right click and highlight panes"). It is the chord's whole job:
      // a watch is a mark the human sets and only the human clears, so this
      // toggles and never does anything else.
      //
      // CHECKED against every CLI this repo spawns, per the
      // agent-cli-reference discipline, with the references fetched rather
      // than recalled (all re-read for this slice, not carried from #3263):
      //   - Claude Code's interactive-mode reference documents Alt+B/D/F/M/O/
      //     P/T/V/Y and the arrows; no Alt+H. Its keybindings reference lists
      //     no meta+h default either — its only Meta rows are Meta+P/O/T and
      //     Meta+Up/Down. It RESERVES plain `Ctrl+H` (the ASCII backspace
      //     byte) and cannot rebind it; the `!e.ctrlKey` guard on this block
      //     is what keeps loomux out of that.
      //   - Copilot CLI's command reference DOES carry key tables, and its Alt
      //     rows are Alt+V, Alt+Enter, Alt+arrows and Alt+scroll — no Alt+H.
      //     It binds plain Ctrl+H (delete previous character), which this does
      //     not touch.
      //   - opencode's keybinds reference has alt+a/b/d/e/f, the alt+shift
      //     pairs and the ctrl+alt rows; no alt+h.
      //   - pi's keybindings reference has no alt+h DEFAULT. It appears once
      //     in that page, inside the opt-in `### Vim Example` custom config
      //     (`tui.editor.cursorLeft: ["left", "alt+h"]`), which a user pastes
      //     in by hand — a user-config collision, not a shipped default, and
      //     the same standing Alt+J already ships with.
      //   - Codex's reference documents Up/Down, Ctrl+R/O/C, Tab, Enter and
      //     Esc, and no Alt or Ctrl+Shift binding at all. Its list is prose
      //     rather than a declared-complete table, so that one is UNVERIFIED
      //     rather than confirmed free.
      //   - Readline leaves `\eh` unbound in this repo's bash (`\eH` is only
      //     do-lowercase-version), the same shape Alt+W, Alt+Q and Alt+J rely
      //     on, and Alt+H is not a WebView2 accelerator.
      case "KeyH": return "toggle-watch";
      case "KeyT": return "toggle-tasks";
      // Alt+Q (#1091) — the NEEDS-YOU panel, the board's decision sibling.
      // NOT Alt+D, which is readline's kill-word in every bash pane.
      //
      // CHECKED against the agent CLIs' own references per the
      // agent-cli-reference discipline, and this one comes out CONFIRMED FREE
      // rather than merely unverified:
      //   - Claude Code's interactive-mode reference documents Alt+V/M/P/T/O/
      //     Y/B/F (and Alt+Enter) and no Alt+Q; its keybindings reference lists
      //     no Meta+Q default either.
      //   - Copilot CLI's command reference DOES carry key tables, and its Alt
      //     rows are Alt+V, Alt+Enter, Alt+arrows and Alt+scroll — no Alt+Q. It
      //     binds Ctrl+Q (queue a message), which this does not touch; named
      //     here so a future reader grepping "Q" does not reopen the question.
      // Readline in this repo's bash leaves `\eq` unbound (`\eQ` is only
      // do-lowercase-version) — the same shape Alt+W relies on. Neither
      // vendor documents any Alt+SHIFT default at all, so that variant is
      // UNVERIFIED rather than free; the `!e.shiftKey` guard on this whole
      // block is what keeps loomux from taking it.
      case "KeyQ": return "toggle-decisions";
      case "KeyA": return "toggle-audit";
      // Alt+W (#608) — the progress timeline, the audit log's chart sibling.
      // Verified free before landing, per the agent-cli-reference discipline:
      // Claude Code's interactive-mode reference documents Alt+V/M/P/T/O/Y/B/F
      // and no Alt+W; Copilot CLI's command reference documents Alt+Enter,
      // Alt+arrows and Alt+scroll and no Alt+W. Readline in this repo's bash
      // leaves `\ew` unbound (`\eW` is only do-lowercase-version), and Alt+W
      // is not a WebView2 accelerator the way Ctrl+W is.
      case "KeyW": return "toggle-timeline";
      // Alt+K (#2011) — the token charts, the audit log's cost sibling.
      //
      // CHECKED against every CLI this repo spawns, per the
      // agent-cli-reference discipline, and this one is NOT free — it is the
      // first loomux Alt binding to land on a documented collision, so the
      // reasoning is recorded rather than left for a later reader to redo:
      //   - Claude Code's interactive-mode reference documents Alt+B/D/F/M/
      //     O/P/T/V/Y and the arrows; no Alt+K.
      //   - Copilot CLI's command reference documents Alt+Enter and Alt+V
      //     only; no Alt+K.
      //   - opencode's keybinds reference spells its only `alt+k` as
      //     `ctrl+alt+k` (which_key_toggle), which the `!e.ctrlKey` guard on
      //     this block excludes.
      //   - Readline in this repo's bash leaves `\ek` unbound (`\eK` is only
      //     do-lowercase-version), the same shape Alt+W relies on, and Alt+K
      //     is not a WebView2 accelerator.
      //   - pi does NOT bind it by default. Its `tui.editor.cursorUp` default
      //     is `up` alone; the `["up", "alt+k"]` spelling is in that page's
      //     *Vim Example* custom-config block, which a user opts into by hand
      //     (re-read at the reference, #3263 S4 — the earlier note here read
      //     that example as a shipped default and called Alt+K a real
      //     collision, which it is not).
      // So Alt+K is free of documented defaults everywhere, and a pi user who
      // copies the vim example costs themselves a REDUNDANT alias: pi binds the
      // same action to plain `up`, which loomux does not intercept.
      case "KeyK": return "toggle-tokens";
      case "KeyO": return "toggle-group";
      case "KeyP": return "focus-compose";
      // Alt+S (voice / "speak"). NOT Alt+V: that's Claude Code's paste-image
      // binding, and loomux intercepting it stole it inside agent panes. NOT
      // Alt+M either (that's minimize-pane). Alt+S is free in loomux, unused by
      // Claude Code, and not a readline word-motion binding.
      case "KeyS": return "voice-ptt";
    }
  }
  if (e.code === "F2" && !e.ctrlKey && !e.altKey && !e.shiftKey) return "rename-pane";
  return null;
}

export const isAppShortcut = (e: KeyboardEvent): boolean => matchShortcut(e) !== null;
