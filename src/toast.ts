// A small, transient, app-level toast — for non-fatal notices (e.g. "no
// editor configured", "failed to launch editor") that don't warrant the
// full-screen fatal banner in main.ts. Auto-dismisses; click to dismiss early.
//
// ONE ACTION, AND IT IS OPTIONAL (#3263 S5). A reminder and an undo are both
// "here is a thing that happened, and here is the one gesture you want next",
// so the toast grew a single trailing button rather than a second surface.
// Deliberately ONE: a toast with a choice in it is a dialog, and a dialog that
// dismisses itself after five seconds is a trap.

let toastEl: HTMLElement | null = null;
let toastBody: HTMLElement | null = null;
let toastBtn: HTMLButtonElement | null = null;
let toastTimer: number | undefined;

/** The one gesture a toast may offer. `label` is the button's text; `run` is
 *  called on click, and the toast dismisses itself either way. */
export interface ToastAction {
  label: string;
  run: () => void;
}

function hide(): void {
  toastEl?.classList.remove("visible");
}

/**
 * Show a transient toast. `kind` tints it: "error" (red) or "info" (neutral).
 *
 * `action`, when given, adds one trailing button. The button is REBUILT on
 * every call rather than reconfigured, and the handler is held in a closure
 * over this call's `action` — so a second toast can never fire the first one's
 * gesture, which is the failure mode a reused element invites when the two
 * arrive a second apart.
 */
export function showToast(
  message: string,
  kind: "error" | "info" = "error",
  action?: ToastAction
): void {
  if (!toastEl) {
    toastEl = document.createElement("div");
    toastEl.className = "app-toast";
    toastBody = document.createElement("span");
    toastBody.className = "app-toast-text";
    toastEl.append(toastBody);
    // Clicking the toast BODY dismisses, as it always has. The action button
    // stops the event so its own click is not also a dismiss-by-background.
    toastEl.addEventListener("click", hide);
    document.body.appendChild(toastEl);
  }
  toastBody!.textContent = message;
  toastBtn?.remove();
  toastBtn = null;
  if (action) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "app-toast-action";
    btn.textContent = action.label;
    btn.addEventListener("click", (ev) => {
      ev.stopPropagation();
      hide();
      action.run();
    });
    toastEl.append(btn);
    toastBtn = btn;
  }
  toastEl.classList.toggle("info", kind === "info");
  toastEl.classList.add("visible");
  clearTimeout(toastTimer);
  toastTimer = window.setTimeout(hide, 5000);
}
