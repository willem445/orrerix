// The fork-name prompt (#3368, #3318 F5) — DOM wiring, validated by hand. What
// each way out of it MEANS is `forkNameDecision` (`src/forkname.ts`), tested.
//
// A small popover pinned to the top of the pane being forked, not a centred
// dialog: the name is an optional rename, and a modal would put a whole-window
// scrim in front of a gesture the issue asks to be quick. `.fork-name-prompt` is
// `position: fixed` on `document.body` (`src/styles.css`), so it floats over the
// pane rather than joining any layout — no box moves, and nothing here can reach
// a PTY resize (CLAUDE.md constraint 1). `test/forkprompt.test.ts` pins that
// rule and that every class assigned here has one. The source pane's terminal
// keeps running underneath it.

import { forkNameDecision, type ForkNameDecision, type ForkPromptExit } from "./forkname";

/** The one prompt open at a time. A second fork gesture while one is open
 *  answers the first as a cancel rather than stacking two popovers. */
let open: ((exit: ForkPromptExit) => void) | null = null;

/** The popover's elements. Built detached; `promptForkName` wires and places it. */
function buildPrompt(fallback: string): { box: HTMLElement; input: HTMLInputElement; cancel: HTMLButtonElement } {
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
  return { box, input, cancel };
}

/** Pin the popover to the top of `anchor` (the source pane), inside the window.
 *  Placed once, at open: the pane cannot move under an open prompt in a way that
 *  matters, since every way out of it is a gesture. */
function placePrompt(box: HTMLElement, anchor: HTMLElement): void {
  const r = anchor.getBoundingClientRect();
  const width = Math.max(220, Math.min(360, r.width - 16));
  box.style.left = `${Math.max(4, Math.min(window.innerWidth - width - 4, r.left + 8))}px`;
  box.style.top = `${Math.max(4, r.top + 34)}px`;
  box.style.width = `${width}px`;
}

/** Ask for the fork's name over `anchor` (the source pane's element),
 *  pre-filled with `fallback` — `<parent name> (fork)`. Resolves with what the
 *  gesture does next (`forkNameDecision`). */
export function promptForkName(anchor: HTMLElement, fallback: string): Promise<ForkNameDecision> {
  open?.("cancel");
  return new Promise<ForkNameDecision>((resolve) => {
    const { box, input, cancel } = buildPrompt(fallback);
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
    // On the BOX, not the input (review N2): Enter and Escape mean the same
    // thing wherever focus sits inside the prompt — on the ✕ button included,
    // where Enter is left to the button's own click (a cancel).
    box.addEventListener("keydown", (e) => {
      // The terminal and the app's shortcuts must not see keys typed here.
      e.stopPropagation();
      if (e.key === "Escape") {
        e.preventDefault();
        finish("escape");
      } else if (e.key === "Enter" && e.target === input) {
        e.preventDefault();
        finish("enter");
      }
    });
    cancel.addEventListener("click", () => finish("cancel"));

    placePrompt(box, anchor);
    document.body.appendChild(box);
    document.addEventListener("mousedown", outside, true);
    input.focus();
    input.select();
  });
}
