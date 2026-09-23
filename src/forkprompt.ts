// The fork-name prompt (#3368, #3318 F5) — DOM wiring, validated by hand. What
// each way out of it MEANS is `forkNameDecision` (`src/forkname.ts`), tested.
//
// A small popover pinned to the top of the pane being forked, not a centred
// dialog: the name is an optional rename, and a modal would put a whole-window
// scrim in front of a gesture the issue asks to be quick. It is `position:
// fixed` on `document.body`, so it floats over the pane rather than joining any
// layout — no box moves, and nothing here can reach a PTY resize (CLAUDE.md
// constraint 1). The source pane's terminal keeps running underneath it.

import { forkNameDecision, type ForkNameDecision, type ForkPromptExit } from "./forkname";

/** The one prompt open at a time. A second fork gesture while one is open
 *  answers the first as a cancel rather than stacking two popovers. */
let open: ((exit: ForkPromptExit) => void) | null = null;

/** Ask for the fork's name over `anchor` (the source pane's element),
 *  pre-filled with `fallback` — `<parent name> (fork)`. Resolves with what the
 *  gesture does next (`forkNameDecision`). */
export function promptForkName(anchor: HTMLElement, fallback: string): Promise<ForkNameDecision> {
  open?.("cancel");
  return new Promise<ForkNameDecision>((resolve) => {
    const box = document.createElement("div");
    box.className = "fork-name-prompt";
    box.setAttribute("role", "dialog");
    box.setAttribute("aria-label", "Name the fork");

    const label = document.createElement("label");
    label.className = "fork-name-label";
    label.textContent = "Fork as";

    const input = document.createElement("input");
    input.className = "fork-name-input";
    input.type = "text";
    input.value = fallback;
    input.spellcheck = false;
    input.setAttribute("aria-label", "Name for the forked pane");
    label.appendChild(input);

    const hint = document.createElement("span");
    hint.className = "fork-name-hint";
    hint.textContent = "Enter forks under this name · Esc forks as the default";

    const cancel = document.createElement("button");
    cancel.className = "fork-name-cancel";
    cancel.type = "button";
    cancel.textContent = "✕";
    cancel.title = "Don't fork";
    cancel.setAttribute("aria-label", "Don't fork");

    box.append(label, hint, cancel);

    let settled = false;
    const finish = (exit: ForkPromptExit): void => {
      if (settled) return;
      settled = true;
      open = null;
      document.removeEventListener("mousedown", outside, true);
      box.remove();
      resolve(forkNameDecision(exit, input.value, fallback));
    };
    open = finish;
    /** A click anywhere else is a cancel — the pointer twin of ✕. Capture
     *  phase, so a click that lands on a pane (whose own handlers may stop
     *  propagation) still dismisses the prompt. */
    const outside = (e: MouseEvent): void => {
      if (!box.contains(e.target as Node)) finish("cancel");
    };

    input.addEventListener("keydown", (e) => {
      // The terminal and the app's shortcuts must not see keys typed here.
      e.stopPropagation();
      if (e.key === "Enter") {
        e.preventDefault();
        finish("enter");
      } else if (e.key === "Escape") {
        e.preventDefault();
        finish("escape");
      }
    });
    cancel.addEventListener("click", () => finish("cancel"));

    // Placed from the pane's box once, at open. The pane cannot move under an
    // open prompt in a way that matters: every way out of it is a gesture.
    const r = anchor.getBoundingClientRect();
    const width = Math.max(220, Math.min(360, r.width - 16));
    box.style.left = `${Math.max(4, Math.min(window.innerWidth - width - 4, r.left + 8))}px`;
    box.style.top = `${Math.max(4, r.top + 34)}px`;
    box.style.width = `${width}px`;
    document.body.appendChild(box);
    document.addEventListener("mousedown", outside, true);
    input.focus();
    input.select();
  });
}
