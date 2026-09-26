// A pane's embed-panel slots and floating overlays, split out of pane.ts (#3498 F1):
// the slot host beside the terminal (#361), its dividers and floors, opening,
// closing and toggling a view between overlay and slot, the embed menu, and
// embed restore. The views themselves are built by PaneViews (paneviews.ts).
//
// A satellite of `Pane`: it owns this cluster's DOM and state and reads the rest
// of the pane through `pane`. DOM glue, hand-validated like pane.ts itself; the
// pure geometry lives in embedsplit.ts and overlaysize.ts. Design note:
// docs/design/embedded-panels.md; layout conventions: docs/design/module-layout.md.

import { admitRoot } from "./fileapi";
import { showToast } from "./toast";
import { PendingEmbedFocus } from "./embedfocus";
import { clampOverlayHeight, OVERLAY_MIN_H } from "./overlaysize";
import {
  embedDragGrow,
  fracFromGrow,
  clampEmbedFrac,
  embedSideFloors,
  embedCenterFloor,
  DEFAULT_EMBED_FRAC,
  EMBED_MIN_PANEL_PX,
  EMBED_SIDES,
  type EmbedSide,
} from "./embedsplit";
import { embedToggleAction, toggleDeclaresCwd } from "./embedtoggle";
import { startDragSession } from "./dragsession";
import { showContextMenu, type MenuItem } from "./contextmenu";
import type { ContentPaneKind, EmbedKind, Pane } from "./pane";

/** What to CALL each content kind when a message has to name it ("the git view isn't
 *  available in a workflow pane"). A table rather than a ternary chain, so a fifth kind
 *  is a row and not a nested conditional nobody re-reads. */
const CONTENT_KIND_LABEL: Record<ContentPaneKind, string> = {
  files: "file explorer",
  editor: "file editor",
  git: "git",
  workflow: "workflow",
  structured: "structured agent",
  todo: "to-do list",
};

export const EMBED_KINDS: readonly EmbedKind[] = [
  "tasks",
  "decisions",
  "git",
  "issues",
  "audit",
  "group",
  "editor",
  "timeline",
  "tokens",
];

/** Human label for the toast a no-op'd toggle shows (`embedToggleAction`,
 *  below) — matches each header button's own name for the kind. */
const EMBED_TOGGLE_LABEL: Record<EmbedKind, string> = {
  tasks: "The task board",
  git: "The git view",
  issues: "The issues view",
  audit: "The audit log",
  timeline: "The progress timeline",
  tokens: "The token charts",
  group: "The group lifecycle panel",
  editor: "The file editor",
  decisions: "The needs-you panel",
};

/** Each kind's PANE HEADER toggle button's normal (undocked) title —
 *  restored by `syncEmbedToggleButton` whenever a kind un-docks; the single
 *  source of truth so the constructor's initial assignment and the restore
 *  can't drift apart. */
export const EMBED_TOGGLE_TITLE: Record<EmbedKind, string> = {
  tasks: "Task board (Alt+T)",
  git: "Git view (Alt+G)",
  issues: "GitHub issues (Alt+I)",
  audit: "Audit log (Alt+A)",
  timeline: "Progress timeline (Alt+W)",
  tokens: "Token charts (Alt+K)",
  group: "Group lifecycle (Alt+O)",
  editor: "File editor (Alt+F)",
  decisions: "Needs you — decisions & demos (Alt+Q)",
};

/** One embeddable view's plumbing, registered once that view is lazily
 *  constructed. Lets the generic engine (`openView`/`closeView`/`toggleView`/
 *  `embedViewAtSide`/`reclampViewFloor`) treat all eight views uniformly
 *  without hardcoding any one view's class. */
interface EmbedEntry {
  /** The view's own floating-overlay host (unchanged pre-#361 mechanics). */
  overlayEl: HTMLElement;
  /** The view's own root element — moved between `overlayEl` and whichever
   *  `EmbedSide`'s panel it's currently docked to. */
  viewEl: HTMLElement;
  /** Called every time the view becomes visible, in either mode. */
  show: () => void;
  /** Called every time the view is about to become hidden, in either mode —
   *  extra per-view cleanup beyond hiding its host (e.g. `GitView.hide()`
   *  dismisses an open context menu). Optional: a view with nothing to stop
   *  needs nothing here.
   *
   *  **A view that can be WOKEN from outside is not that view (#1318).** Until
   *  #1318 this doc said only the sentence above it — "most views need nothing
   *  beyond the generic hide" — while the rule that actually mattered lived
   *  three thousand lines below, as a call-site comment on the `timeline`
   *  entry: "stops the follow poll on close/eviction … which every polling view
   *  has to answer for". Neither half reached whoever added the next view. They
   *  read an invitation to skip the hook, and the rule that would have stopped
   *  them was on another view's registration, where nobody adding a NEW view
   *  has any reason to look — so the board and the NEEDS-YOU panel refetched
   *  and rebuilt off screen on every agent write for the life of the session,
   *  and the audit log kept a live-follow poll running behind a closed panel.
   *  The rule belongs here, and it is about the QUESTION rather than the
   *  mechanism: whether the waker is a `setInterval` or a Tauri `listen`
   *  changes nothing. If something outside this view can make it do work, this
   *  hook is where it says what happens when nobody is looking — enforced by
   *  test/embedwake.test.ts; see docs/design/embedded-panels.md and
   *  src/wakegate.ts. */
  hide?: () => void;
  /** Reflect whether this view is currently docked to ANY embed slot —
   *  updates the view's own header toggle button. Side-agnostic on purpose:
   *  the button reads "embedded" vs "floating," not which edge. */
  setPanelActive: (active: boolean) => void;
  /** The live floor (px) for the OVERLAY height clamp, and for the BOTTOM
   *  slot's own height floor specifically (unchanged from the
   *  pre-multi-slot design). Most views share the generic default
   *  (`EMBED_MIN_PANEL_PX`); the group panel measures its own fixed chrome
   *  (`Pane.groupFloor`). NOT used for the left/right slots' WIDTH floor —
   *  see `EMBED_MIN_PANEL_PX`'s own doc comment in embedsplit.ts for why
   *  that one deliberately stays a fixed constant instead. */
  floorPx: () => number;
}

/** One embed slot's live DOM + state — one instance per `EmbedSide`, created
 *  together in `ensureEmbedHost`. `kind`/`frac` are `null`/default when the
 *  slot is empty; `panelEl`/`dividerEl` exist permanently once created,
 *  hidden when empty (the same "create once, toggle `hidden`, never
 *  destroy" idiom every overlay in this file already uses). */
interface EmbedSlotState {
  side: EmbedSide;
  kind: EmbedKind | null;
  frac: number;
  panelEl: HTMLElement;
  dividerEl: HTMLElement;
}

export class PaneEmbeds {
  /** Focus requests parked for this pane's own embeds (#1091 slice C).
   *  Per-PANE, because the surfaces that cite each other are embeds on the
   *  SAME pane — a card naming `t-7` wants THIS pane's board, not another
   *  window's. See embedfocus.ts for why the request is parked rather than
   *  delivered: the target view may not be constructed yet, and even once it
   *  is, its rows arrive from an async refresh. */
  readonly pendingFocus = new PendingEmbedFocus();
  /** Every view registered as embeddable so far (#361), keyed by kind. Built
   *  lazily — one entry per view, added the first time that view's own
   *  `ensureXView()` runs — so a pane that never opens (say) the group panel
   *  never pays for its entry. The generic open/close/toggle engine below
   *  (`openView`/`closeView`/`toggleView`/`embedViewAtSide`/`unembedView`)
   *  treats every kind uniformly through this registry instead of hardcoding
   *  any one view's class — see docs/design/embedded-panels.md. */
  embedRegistry = new Map<EmbedKind, EmbedEntry>();
  /** Up to THREE simultaneous embed slots — left, right, bottom (#361
   *  generalization from a single bottom-only slot) — each independently
   *  holding at most one view. `null` = nothing embedded anywhere; every
   *  view opens as its floating overlay by default (unchanged pre-#361
   *  behavior). Docking a kind that's already embedded elsewhere, or
   *  docking a DIFFERENT kind onto an already-occupied side, SWAPS that
   *  ONE slot's occupant (`embedViewAtSide`) — the other two slots are
   *  untouched either way. Created together, lazily, in `ensureEmbedHost`. */
  embedSlots: Record<EmbedSide, EmbedSlotState> | null = null;
  /** Lazily created wrapper that turns `termEl` into a flex sibling of the
   *  left/right/bottom embed slots instead of the pane's direct flex:1
   *  child. Created once, on the first embed of ANY kind, and left in place
   *  afterward — with every slot's panel/divider `hidden`, `termEl` alone in
   *  the nested structure lays out identically to being `.pane`'s direct
   *  child (see `ensureEmbedHost`'s own doc comment for the exact
   *  structure, a two-level nesting: `embedHostEl` > [`embedRowEl`,
   *  bottom's divider + slot] and `embedRowEl` > [left's divider + slot,
   *  `embedCenterEl`], `embedCenterEl` > [`embedTermWrapEl` (containing
   *  `termEl`), right's divider + slot]). Bottom spans the row's full width
   *  rather than sitting only beside term — the simpler of the two
   *  corner-layout choices (see
   *  docs/design/embedded-panels.md's "Layout" section). Nested, not a flat
   *  5-child row, so every divider's two sides are a real, single DOM
   *  element pair (grid.ts's own nested-split-tree shape) — the left
   *  divider's far side is `embedCenterEl` as ONE element, not "term plus
   *  whatever's on the right," which is what keeps each divider's own
   *  drag math a plain two-element `embedDragGrow` call (see
   *  `dividerPair`/`dividerFloors`). */
  private embedHostEl: HTMLElement | null = null;
  private embedRowEl: HTMLElement | null = null;
  private embedCenterEl: HTMLElement | null = null;
  /** Thin, permanent wrapper around `termEl` inside `embedCenterEl` — exists
   *  ONLY so the right divider's drag math never writes `.style.flex`
   *  directly onto `termEl` itself (#361 user-demo finding: right-docked
   *  expand-left lag). Left's and bottom's far side is always a WRAPPER
   *  (`embedCenterEl`/`embedRowEl`) that resizes `termEl` only as an
   *  indirect, computed consequence of ITS OWN flex-grow changing —
   *  `termEl`'s own inline style is never touched by their drags. Before
   *  this wrapper existed, right's divider was the ONE exception: its
   *  `beforeEl` WAS `termEl` directly (there was nothing else for it to be,
   *  since `embedCenterEl`'s row is exactly `[termEl, right's divider,
   *  right's panel]`), so `termEl` — the same node `resizeObs` OBSERVES —
   *  had its OWN `.style.flex` rewritten on every mousemove tick, uniquely
   *  among the three dividers. `embedTermWrapEl` removes that one
   *  structural asymmetry: right's `beforeEl` is now this wrapper, matching
   *  left/bottom's shape exactly, and `termEl` fills it via its own
   *  pre-existing `flex: 1` rule, unaffected either way. */
  private embedTermWrapEl: HTMLElement | null = null;

  constructor(private readonly pane: Pane) {}

  /** Refuse an overlay on a CONTENT pane (#214/#217), with a reason.
   *
   *  Every pane overlay (git, issues, tasks, audit, group, file editor) floats over
   *  `.pane-term` and takes its height from it — `overlayClamp` measures
   *  `termEl.clientHeight`, and `updateTermShift` reads the live `.xterm-screen` to
   *  keep the cursor visible under the panel. A content pane has no terminal at all,
   *  so those measurements have no meaning and the panel would open into a zero-height
   *  box. They are therefore cleanly OFF there (buttons hidden by `.is-content`,
   *  hotkeys answered with this) rather than half-working.
   *
   *  #214 deferred "the git view over a files root" to a second overlay sizing model.
   *  #217 answers it by the other road, and the answer is why this refusal can stay:
   *  you don't overlay a git view onto a content pane, you OPEN A GIT PANE (the view
   *  as pane content, sized by the pane's own box). The surfaces that needed a
   *  terminal underneath still say so; the ones that never did are now panes. */
  refuseOverlay(what: string): boolean {
    if (!this.pane.isContent) return false;
    const kind = CONTENT_KIND_LABEL[this.pane.contentKind!];
    showToast(`${what} isn't available in a ${kind} pane.`, "info");
    return true;
  }

  /** #1042: declare this pane's live cwd as a root, because a HUMAN just asked
   *  to OPEN a view at it.
   *
   *  This is the one place the distinction the whole design rests on becomes
   *  code. The cwd itself arrived on an agent-controllable channel — the pane's
   *  OSC-7 report, emitted by whatever process is running in it — and that
   *  stream is resolve-only: it never declares anything, which is why an agent
   *  that `cd`s to `~/.ssh` gets a quiet folder chip rather than a rooted file
   *  browser. A human clicking the branch chip, or pressing Alt+G, is not that
   *  stream. It is the trusted local webview acting on a human gesture, and it
   *  declares.
   *
   *  **Called only from `toggleView`, and only on the `open` action.** It used
   *  to be called from each `toggleXView` *before* `toggleView` decided what the
   *  gesture even was, which meant a CLOSE and a docked NO-OP declared too —
   *  so dismissing a panel permanently declared whatever directory the agent in
   *  that pane happened to have `cd`'d to. That is the exact inversion of the
   *  rule this helper exists to implement (#1092 review). `toggleDeclaresCwd`
   *  now owns the decision, `toggleView` is the one call site, and both halves —
   *  which direction, which views — are pinned in `test/embedtoggle.test.ts`.
   *
   *  Normally there is nothing new to declare: a pane's spawn cwd was declared
   *  by the launcher (or by restore), and the descendant rule already covers
   *  everything the shell `cd`s into below it. This matters exactly when the cwd
   *  has left every declared root — which is the case the gesture is for.
   *
   *  Fire-and-forget, and slice C owes it one look. The declaration is *issued*
   *  before the view opens, but two Tauri commands in flight are not ordered
   *  against each other, so once slice C root-scopes the view's own reads there
   *  is a race here in principle. Bounded rather than ignored: every one of
   *  these views re-reads on its own (a shell prompt, an external git change, a
   *  manual ↻), so losing that race costs one empty render, not the feature. If
   *  slice C wants it airtight, `toggleView` becomes `async` and awaits this —
   *  a change this helper is shaped to make one line.
   *
   *  Never for an SSH pane: its cwd names a directory on the REMOTE machine, and
   *  declaring that string here would declare whatever happens to sit at the
   *  same path locally. `onCwdReported` already refuses to act on an SSH pane's
   *  report for the same reason; this is the belt on that suspender. */
  private declareCwd(): void {
    if (this.pane.isSshPane) return;
    const cwd = this.pane.cwdRaw;
    if (cwd) void admitRoot(cwd);
  }

  /** Keep the overlay tall enough that its bottom drag bar stays grabbable and
   *  no control clips, but always leave a terminal strip visible at the bottom.
   *  `floor` overrides the baseline minimum with a panel-specific one (the group
   *  panel measures its fixed chrome so the footer can't collapse — #83 rev-58).
   *  Pure math + tests in overlaysize.ts. */
  overlayClamp(h: number, floor?: number): number {
    return clampOverlayHeight(h, this.pane.termEl.clientHeight, floor ?? OVERLAY_MIN_H);
  }

  /** The group panel's minimum content height — its measured fixed chrome so
   *  every control (footer End/Pause, suspended banner) stays on-screen — never
   *  below the shared baseline, and never so tall it can't fit the pane. */
  groupFloor(): number {
    const measured = this.pane.views.groupView?.minChromeHeight() ?? 0;
    return Math.max(OVERLAY_MIN_H, measured);
  }

  /** Re-apply `kind`'s floor to whichever host it's CURRENTLY shown in (#361
   *  generalizes what was originally `reclampGroupOverlay`, the only view
   *  whose floor can grow after it opens — the suspended banner appearing
   *  inside the group panel, #83 rev-58). Only touches the host when the
   *  floor actually moves it — typically a bump UP — so it never fights the
   *  human's chosen size. In embed mode this nudges the divider's flex-grow
   *  via `embedDragGrow` with a zero delta, which — because a size already
   *  BELOW the new floor makes `sizePanel - minPanelPx` negative — still
   *  produces exactly the corrective nudge; see embedsplit.ts. */
  reclampViewFloor(kind: EmbedKind): void {
    const side = this.sideOf(kind);
    if (side) {
      this.reclampSlotDivider(side);
      return;
    }
    const entry = this.embedRegistry.get(kind);
    if (!entry || entry.overlayEl.hidden) return;
    const cur = entry.overlayEl.offsetHeight;
    const clamped = this.overlayClamp(cur, entry.floorPx());
    if (clamped !== cur) {
      entry.overlayEl.style.height = `${clamped}px`;
      this.pane.updateTermShift();
    }
  }

  /** Which `EmbedSide` (if any) `kind` currently occupies. Only three sides
   *  exist, so a linear scan is simpler and safer than keeping a second,
   *  separately-maintained reverse-lookup map in sync with `embedSlots`. */
  sideOf(kind: EmbedKind): EmbedSide | null {
    if (!this.embedSlots) return null;
    for (const side of EMBED_SIDES) {
      if (this.embedSlots[side].kind === kind) return side;
    }
    return null;
  }

  /** The OTHER element in `side`'s divider pair — i.e. not the slot's own
   *  panel. Left's counterpart is the composite `embedCenterEl`; right's and
   *  bottom's are the plain `termEl` / `embedRowEl` (see `ensureEmbedHost`'s
   *  doc comment for the nested structure this reflects). */
  private counterpartEl(side: EmbedSide): HTMLElement {
    switch (side) {
      case "left":
        return this.embedCenterEl!;
      case "right":
        return this.embedTermWrapEl!;
      case "bottom":
        return this.embedRowEl!;
    }
  }

  /** `side`'s divider pair as `{beforeEl, afterEl}` (the two elements a drag
   *  redistributes flex-grow between) plus which screen axis it drags along.
   *  "Before" is whichever element sits physically before the divider in
   *  reading order — left's own slot for `"left"` (dragging right grows it),
   *  `embedTermWrapEl` for `"right"` (dragging right grows IT, shrinking the
   *  slot — NEVER `termEl` directly; see `embedTermWrapEl`'s own doc
   *  comment), the row for `"bottom"` (dragging down grows it). Matches
   *  `embedDragGrow`'s convention (`before` grows with a positive delta)
   *  exactly, mirroring grid.ts's own split-divider math. */
  private dividerPair(side: EmbedSide): { beforeEl: HTMLElement; afterEl: HTMLElement; horizontal: boolean } {
    const slot = this.embedSlots![side];
    switch (side) {
      case "left":
        return { beforeEl: slot.panelEl, afterEl: this.embedCenterEl!, horizontal: true };
      case "right":
        return { beforeEl: this.embedTermWrapEl!, afterEl: slot.panelEl, horizontal: true };
      case "bottom":
        return { beforeEl: this.embedRowEl!, afterEl: slot.panelEl, horizontal: false };
    }
  }

  /** The `embedDragGrow` floor pair for `side`'s divider, evaluated LIVE
   *  (the group panel's floor can grow after it opens; the left divider's
   *  far-side floor depends on whether right is CURRENTLY occupied). See
   *  embedsplit.ts's `embedSideFloors`/`embedCenterFloor` for the actual
   *  precedence math this only ever plugs live values into. */
  private dividerFloors(side: EmbedSide): { beforeFloorPx: number; afterFloorPx: number } {
    if (side === "left") {
      const right = this.embedSlots!.right;
      const rightFloorPx = right.kind !== null ? EMBED_MIN_PANEL_PX : null;
      return { beforeFloorPx: EMBED_MIN_PANEL_PX, afterFloorPx: embedCenterFloor(rightFloorPx) };
    }
    if (side === "right") return embedSideFloors("right", EMBED_MIN_PANEL_PX);
    // bottom
    const bottomKind = this.embedSlots!.bottom.kind;
    const panelFloorPx = bottomKind ? this.embedRegistry.get(bottomKind)!.floorPx() : EMBED_MIN_PANEL_PX;
    return embedSideFloors("bottom", panelFloorPx);
  }

  /** Set `side`'s divider pair directly from the slot's own PANEL share
   *  (`frac` — always "how much of the pair the panel itself gets," never
   *  "before" or "after," since which one the panel physically is differs
   *  per side). Position-agnostic on purpose: `flex-grow` only encodes a
   *  ratio, not which sibling is which, so this never needs to know
   *  before/after itself — only `dividerPair`'s drag handler does. */
  private applySlotGrow(side: EmbedSide, frac: number): void {
    const slot = this.embedSlots![side];
    const clamped = clampEmbedFrac(frac);
    slot.panelEl.style.flex = `${clamped} 1 0`;
    this.counterpartEl(side).style.flex = `${1 - clamped} 1 0`;
  }

  /** Re-apply `side`'s CURRENT floors to its CURRENT sizes (a zero-delta
   *  "drag") — the correction for a floor that grew (or, for left, whose
   *  composed far-side floor changed because right's occupancy changed)
   *  since the slot was last sized. Only touches it when the clamp actually
   *  moves it, so it never fights the human's chosen size. Zero delta still
   *  produces a real nudge when a side is already below its (possibly new)
   *  floor, because `sizeAfter - minAfterPx` (or the before equivalent) goes
   *  negative in `embedDragGrow`'s own clamp — see embedsplit.ts. */
  private reclampSlotDivider(side: EmbedSide): void {
    if (!this.embedSlots) return;
    const slot = this.embedSlots[side];
    if (slot.kind === null || slot.panelEl.hidden) return;
    const { beforeEl, afterEl, horizontal } = this.dividerPair(side);
    const sizeBefore = horizontal ? beforeEl.offsetWidth : beforeEl.offsetHeight;
    const sizeAfter = horizontal ? afterEl.offsetWidth : afterEl.offsetHeight;
    const growBefore = parseFloat(beforeEl.style.flexGrow || "1");
    const growAfter = parseFloat(afterEl.style.flexGrow || "1");
    const { beforeFloorPx, afterFloorPx } = this.dividerFloors(side);
    const grow = embedDragGrow(sizeBefore, sizeAfter, growBefore, growAfter, 0, beforeFloorPx, afterFloorPx);
    if (grow.growBefore === growBefore && grow.growAfter === growAfter) return;
    beforeEl.style.flex = `${grow.growBefore} 1 0`;
    afterEl.style.flex = `${grow.growAfter} 1 0`;
    const panelGrow = parseFloat(slot.panelEl.style.flexGrow || "1");
    const counterpartGrow = parseFloat(this.counterpartEl(side).style.flexGrow || "1");
    slot.frac = fracFromGrow(counterpartGrow, panelGrow);
    this.pane.updateTermShift();
    // Left's counterpart IS `embedCenterEl`, which nests right's own
    // divider pair (`termEl` | right's panel) — a change to left's split
    // just changed embedCenterEl's own box size, and the term/right split
    // inside it is a plain CSS flex-grow ratio that does NOT know about
    // right's floor on its own. Re-run right's clamp against its new box so
    // it can't end up below its floor just because left grew (#361 rev-58
    // NB2) — a no-op (see the early-return above) when right isn't
    // occupied or wasn't actually pushed under its floor.
    if (side === "left") this.reclampSlotDivider("right");
  }

  /** Horizontal drag handle on an overlay's bottom edge. `floor` (optional) is a
   *  panel-specific minimum height provider passed to the clamp on each drag. */
  makeOverlayDivider(overlay: () => HTMLElement, floor?: () => number): HTMLElement {
    const div = document.createElement("div");
    div.className = "git-divider";
    div.addEventListener("mousedown", (e) => {
      e.preventDefault();
      const startY = e.clientY;
      const startH = overlay().offsetHeight;
      div.classList.add("dragging");
      // Same drag-suspend discipline as the embed-slot dividers (#361
      // user-demo finding) — the overlay's own height-drag hits the exact
      // same "reflow a huge list on every mousemove" cost.
      overlay().classList.add("resizing");
      const move = (ev: MouseEvent) => {
        const h = this.overlayClamp(startH + (ev.clientY - startY), floor?.());
        overlay().style.height = `${h}px`;
        this.pane.updateTermShift();
      };
      const end = () => {
        div.classList.remove("dragging");
        overlay().classList.remove("resizing");
      };
      startDragSession({ onMove: move, onEnd: end });
    });
    return div;
  }

  // ==================== #1091 slice C: the focus-request hook ====================

  /** Open one of THIS pane's embeds and ask it to bring `target` into view.
   *
   *  The generic hook behind every cross-embed citation on an orchestrator
   *  pane: a NEEDS-YOU card naming `t-7` links to that board row, and (once
   *  #1091 slice G lands the board marker) a held board row links back to the
   *  question holding it. Both surfaces are embeds on this same pane, so this
   *  is intra-pane wiring — no backend command exists or is needed for it.
   *
   *  **The request is PARKED, not delivered.** Two things stand between asking
   *  and the row existing: the target view may never have been constructed
   *  (every embed is lazy), and once it is, its rows arrive from an async
   *  refresh. So the target goes into `pendingFocus` and the view drains it on
   *  its own next render, when the rows are actually there — see
   *  `embedfocus.ts` for why that drain is destructive.
   *
   *  **Never a toggle.** A citation is "show me this", so an already-open view
   *  is re-shown rather than closed — routing this through `toggleView` would
   *  make clicking a link on a visible board close it.
   *
   *  Returns `false` when this pane cannot host that kind (a non-orchestrator
   *  pane has no board), so a caller can render an inert label instead of a
   *  link that goes nowhere. */
  requestEmbedFocus(kind: EmbedKind, target: string): boolean {
    if (!this.pane.orchGroup) return false;
    // Fail closed on the gating button, the same test every `toggleXView`
    // makes: a hidden button means this pane does not offer that view at all.
    if (this.embedToggleBtn(kind)?.hidden !== false) return false;
    if (!this.pendingFocus.request(kind, target)) return false;
    this.ensureEmbedView(kind);
    // FAIL CLOSED on a kind this hook cannot actually route. `ensureEmbedView`
    // has a deliberate silent default, and `openView` returns early on a
    // missing registry entry — so without this check a kind whose header button
    // is visible but which has no `ensureEmbedView` case would park a target
    // nothing will ever drain and still answer `true`. `true` is the one answer
    // that makes a caller render a link that goes nowhere, which is precisely
    // what its `false` contract exists to prevent. Cheaper to make impossible
    // here than to remember at each future call site.
    if (!this.embedRegistry.has(kind)) {
      this.pendingFocus.clear(kind);
      return false;
    }
    if (this.isViewVisible(kind)) this.embedRegistry.get(kind)?.show();
    else this.openView(kind);
    return true;
  }

  /** Lazily construct `kind`'s view, whichever it is — the dispatch behind
   *  `requestEmbedFocus`, which unlike a toggle has no per-kind entry point of
   *  its own to hang the `ensureXView()` call on. Kinds with no focusable
   *  content simply have nothing to construct here yet; adding one is adding
   *  its case. */
  private ensureEmbedView(kind: EmbedKind): void {
    switch (kind) {
      case "tasks":
        this.pane.views.ensureTasksView();
        break;
      case "decisions":
        this.pane.views.ensureDecisionsView();
        break;
      default:
        break;
    }
  }

  // ==================== #361: the generic embed engine ====================
  // Shared by every EmbedKind (tasks/git/issues/audit/group) through
  // `embedRegistry` — see docs/design/embedded-panels.md for the full design,
  // including why this is the legitimate side of the no-PTY-resize-for-chrome
  // rule (CLAUDE.md constraint 1) and why the file-editor overlay is
  // deliberately NOT part of this set.

  /** Lazily promote `termEl` from being `.pane`'s own direct flex:1 child to
   *  living inside a NESTED flex structure alongside up to three embed
   *  slots — left, right, bottom (#361 generalization from a single
   *  bottom-only slot). Created once, on the first embed of ANY kind, and
   *  left in place afterward: with every slot's panel/divider `[hidden]`
   *  (`display: none !important`, styles.css), `termEl` alone lays out
   *  identically to being `.pane`'s direct child, so there is nothing to
   *  undo when nothing is embedded.
   *
   *  The structure, two levels of nesting deep:
   *  ```
   *  embedHostEl (column)
   *    embedRowEl (row, the width axis)
   *      left divider + slot        (hidden unless occupied)
   *      embedCenterEl (row)
   *        embedTermWrapEl > termEl (see its own doc comment: this wrapper
   *                                  is what keeps termEl's OWN inline style
   *                                  untouched by the right divider's drag)
   *        right divider + slot     (hidden unless occupied)
   *    bottom divider + slot        (hidden unless occupied)
   *  ```
   *  Bottom spans the row's FULL width (a sibling of `embedRowEl`, not
   *  nested inside it) rather than sitting only beside `termEl` — the
   *  simpler of the two corner-layout choices (see
   *  docs/design/embedded-panels.md's "Layout" section). NESTED, not a flat
   *  5-child row, so every divider's two sides are a real, single DOM
   *  element pair — see `dividerPair`/`dividerFloors` for why that's what
   *  keeps each divider's own drag math a plain two-element
   *  `embedDragGrow` call instead of a "sum of several siblings" problem. */
  private ensureEmbedHost(): void {
    if (this.embedHostEl) return;
    const host = document.createElement("div");
    host.className = "pane-embed-host";
    this.pane.el.insertBefore(host, this.pane.termEl);

    const row = document.createElement("div");
    row.className = "pane-embed-row";
    const center = document.createElement("div");
    center.className = "pane-embed-center";

    const termWrap = document.createElement("div");
    termWrap.className = "pane-embed-term-wrap";
    termWrap.appendChild(this.pane.termEl);

    const left = this.makeEmbedSlot("left");
    const right = this.makeEmbedSlot("right");
    const bottom = this.makeEmbedSlot("bottom");

    center.append(termWrap, right.dividerEl, right.panelEl);
    row.append(left.panelEl, left.dividerEl, center);
    host.append(row, bottom.dividerEl, bottom.panelEl);

    this.embedHostEl = host;
    this.embedRowEl = row;
    this.embedCenterEl = center;
    this.embedTermWrapEl = termWrap;
    this.embedSlots = { left, right, bottom };
  }

  /** Build one embed slot's permanent (created-once, `hidden`-toggled) DOM:
   *  its panel and its divider. */
  private makeEmbedSlot(side: EmbedSide): EmbedSlotState {
    const panelEl = document.createElement("div");
    panelEl.className = `pane-embed-panel side-${side}`;
    panelEl.hidden = true;
    const dividerEl = document.createElement("div");
    dividerEl.className = `pane-embed-divider side-${side}`;
    dividerEl.hidden = true;
    const slot: EmbedSlotState = { side, kind: null, frac: DEFAULT_EMBED_FRAC, panelEl, dividerEl };
    this.wireEmbedDivider(slot);
    return slot;
  }

  /** Draggable divider for one embed slot. Mirrors grid.ts's own
   *  split-divider math exactly (embedsplit.ts) — the terminal's box
   *  genuinely resizes here (a real flex layout, not an
   *  absolutely-positioned overlay), so the SAME frame-debounced
   *  ResizeObserver → applyFit() path a grid split's divider drag already
   *  drives fires on every real size change, for all three sides alike. */
  private wireEmbedDivider(slot: EmbedSlotState): void {
    slot.dividerEl.addEventListener("mousedown", (e) => {
      e.preventDefault();
      const { beforeEl, afterEl, horizontal } = this.dividerPair(slot.side);
      const startPos = horizontal ? e.clientX : e.clientY;
      const sizeBefore = horizontal ? beforeEl.offsetWidth : beforeEl.offsetHeight;
      const sizeAfter = horizontal ? afterEl.offsetWidth : afterEl.offsetHeight;
      const growBefore = parseFloat(beforeEl.style.flexGrow || "1");
      const growAfter = parseFloat(afterEl.style.flexGrow || "1");
      slot.dividerEl.classList.add("dragging");
      // Coalesce this pane's own PTY resize to one call at drag-end instead
      // of one per animation frame for the whole drag (#432 item 1) — same
      // mechanism grid.ts's split divider uses, scoped to just this pane
      // since only its own termEl resizes here.
      this.pane.beginResizeHold();
      // Suspend layout of the docked panel's OWN content for the duration of
      // the drag (#361 user-demo finding: a large unvirtualized list — e.g.
      // thousands of audit entries — makes every mousemove frame reflow the
      // whole list just because the container's cross-axis size changed,
      // even though nothing about the list's OWN content did). `.resizing`
      // (styles.css) applies `content-visibility: hidden` to the known heavy
      // list classes, so the browser skips their layout/paint entirely while
      // dragging and does ONE normal reflow when it's removed on release —
      // the terminal side of the divider is never touched by this class, so
      // its own resize/PTY-fit path is completely unaffected.
      slot.panelEl.classList.add("resizing");
      const move = (ev: MouseEvent) => {
        const pos = horizontal ? ev.clientX : ev.clientY;
        const { beforeFloorPx, afterFloorPx } = this.dividerFloors(slot.side);
        const grow = embedDragGrow(sizeBefore, sizeAfter, growBefore, growAfter, pos - startPos, beforeFloorPx, afterFloorPx);
        beforeEl.style.flex = `${grow.growBefore} 1 0`;
        afterEl.style.flex = `${grow.growAfter} 1 0`;
      };
      const end = () => {
        slot.dividerEl.classList.remove("dragging");
        slot.panelEl.classList.remove("resizing");
        this.pane.endResizeHold();
        // Terminal (one per drag, not per mousemove) — mirrors grid.ts's own
        // split divider: persist the settled fraction so a restore
        // reproduces THIS size, not the one before the drag. `frac` is
        // always the PANEL's own share regardless of which side of the pair
        // it physically is (see `applySlotGrow`'s doc comment) —
        // `fracFromGrow(counterpartGrow, panelGrow)` extracts exactly that.
        const panelGrow = parseFloat(slot.panelEl.style.flexGrow || "1");
        const counterpartGrow = parseFloat(this.counterpartEl(slot.side).style.flexGrow || "1");
        slot.frac = fracFromGrow(counterpartGrow, panelGrow);
        this.pane.events.onRecordChanged(this.pane);
      };
      startDragSession({ onMove: move, onEnd: end });
    });
  }

  /** Whether `kind`'s view is currently on screen, in EITHER mode. */
  isViewVisible(kind: EmbedKind): boolean {
    const entry = this.embedRegistry.get(kind);
    if (!entry) return false;
    const side = this.sideOf(kind);
    return side ? !this.embedSlots![side].panelEl.hidden : !entry.overlayEl.hidden;
  }

  /** Close `kind`'s view, in whichever mode it's currently shown. Does NOT
   *  un-dock it — a docked-but-closed view stays docked (`slot.kind` is
   *  untouched), exactly mirroring how a never-embedded view stays parked
   *  in its own hidden overlay between opens. `unembedView` is the
   *  separate, explicit action that actually clears a slot. */
  private closeView(kind: EmbedKind): void {
    const entry = this.embedRegistry.get(kind);
    if (!entry) return;
    entry.hide?.();
    const side = this.sideOf(kind);
    if (side) {
      const slot = this.embedSlots![side];
      slot.dividerEl.hidden = true;
      slot.panelEl.hidden = true;
      // Return the view to its OWN overlay host (#361 rev-38 blocker): a
      // slot's panel must never retain a closed/evicted occupant's element.
      // `openView`'s embedded branch below also self-enforces this with
      // `replaceChildren` (belt and suspenders — a panel can never hold
      // more than one child regardless of what called it), but parking the
      // element back in its OWN overlay (rather than just detaching it) is
      // what keeps it reachable and correctly `hidden` the next time THIS
      // view opens as an overlay, exactly where a never-embedded view
      // already lives between opens.
      entry.overlayEl.insertBefore(entry.viewEl, entry.overlayEl.firstChild);
    } else {
      entry.overlayEl.hidden = true;
    }
    this.pane.updateTermShift();
    this.pane.focus();
  }

  /** Close every OTHER floating overlay before `kind` opens AS AN OVERLAY:
   *  they genuinely collide, only one floating panel fits over the
   *  terminal. `editor` (the file editor) is just another `EmbedKind` now
   *  (#361 scope increase) and needs no special-casing here anymore — it's
   *  covered by the loop below like every other kind. Never called for an
   *  embed-mode open (see `openView`) — the whole point of embedding is that
   *  it does NOT collide with a floating panel (#361 NB-4), for any of the
   *  (now up to three, simultaneous) docked views alike. A docked view is
   *  therefore left alone by this loop (`this.sideOf(kind) === null` guards
   *  it, mirroring how it's never the one with an open overlay anyway). */
  private closeOtherOverlays(except?: EmbedKind): void {
    for (const kind of EMBED_KINDS) {
      if (kind === except) continue;
      const entry = this.embedRegistry.get(kind);
      if (entry && this.sideOf(kind) === null && !entry.overlayEl.hidden) this.closeView(kind);
    }
  }

  /** Show `kind`'s view in whichever mode it's currently set to. Wraps the
   *  view's own `show()` in the same never-leave-the-pane-half-toggled
   *  recovery `toggleGitView` originally had for itself — generalized here
   *  because any view's `show()` (a refresh that can throw) has the same
   *  failure shape, not just git's. */
  private openView(kind: EmbedKind): void {
    const entry = this.embedRegistry.get(kind);
    if (!entry) return;
    const side = this.sideOf(kind);
    try {
      if (side) {
        const slot = this.embedSlots![side];
        // `replaceChildren`, not `appendChild` (#361 rev-38 blocker): a
        // slot's panel may only ever hold ONE occupant, and this makes that
        // an invariant of the call itself rather than something every
        // caller has to get right by first evicting whoever was there —
        // even if a future code path forgot to, this can't leave two views
        // stacked and both visible.
        slot.panelEl.replaceChildren(entry.viewEl);
        this.applySlotGrow(side, slot.frac);
        slot.dividerEl.hidden = false;
        slot.panelEl.hidden = false;
        // The share just applied may be stale (a restored preference
        // captured under a smaller floor, a floor that grew while closed,
        // or — for left specifically — the OTHER slot's occupancy having
        // changed since) — reclamp against the CURRENT floor immediately,
        // the same correction a content-driven floor growth applies while
        // already open (#361 rev-38 NB3; see `reclampViewFloor`).
        this.reclampViewFloor(kind);
      } else {
        this.closeOtherOverlays(kind);
        entry.overlayEl.insertBefore(entry.viewEl, entry.overlayEl.firstChild);
        const strip = Math.max(140, Math.round(this.pane.el.clientHeight * 0.35));
        entry.overlayEl.style.height = `${this.overlayClamp(this.pane.termEl.clientHeight - strip, entry.floorPx())}px`;
        entry.overlayEl.hidden = false;
      }
      entry.show();
      this.pane.updateTermShift();
    } catch (err) {
      entry.hide?.();
      if (side) {
        const slot = this.embedSlots![side];
        slot.dividerEl.hidden = true;
        slot.panelEl.hidden = true;
        entry.overlayEl.insertBefore(entry.viewEl, entry.overlayEl.firstChild);
      } else {
        entry.overlayEl.hidden = true;
      }
      this.pane.termEl.style.transform = "";
      throw err;
    }
  }

  /** The pane-header toggle button for `kind` — the button `syncEmbedToggleButton`
   *  disables/retitles while docked. `null` for a kind that never gets one
   *  (there is none today; kept total for a future kind that might not). */
  private embedToggleBtn(kind: EmbedKind): HTMLButtonElement | null {
    switch (kind) {
      case "tasks":
        return this.pane.views.tasksBtn;
      case "decisions":
        return this.pane.views.decisionsBtn;
      case "audit":
        return this.pane.views.auditBtn;
      case "timeline":
        return this.pane.views.timelineBtn;
      case "tokens":
        return this.pane.views.tokensBtn;
      case "group":
        return this.pane.views.groupBtn;
      case "git":
        return this.pane.views.gitBtn;
      case "issues":
        return this.pane.views.issuesBtn;
      case "editor":
        return this.pane.views.fileEditBtn;
    }
  }

  /** Reflect `kind`'s CURRENT dock state on its pane-header toggle button
   *  (#361 user-demo finding): disabled + retitled while docked, since the
   *  plain overlay toggle is deliberately unsupported then
   *  (`embedtoggle.ts`). Called everywhere a kind's dock state changes
   *  (`embedViewAtSide`, `unembedView`, `restoreEmbeds`) — the pane-level
   *  counterpart to what each view's own `setPanelActive` already does for
   *  its INTERNAL embed/close buttons. */
  private syncEmbedToggleButton(kind: EmbedKind): void {
    const btn = this.embedToggleBtn(kind);
    if (!btn) return;
    const docked = this.sideOf(kind) !== null;
    btn.disabled = docked;
    btn.title = docked
      ? `${EMBED_TOGGLE_LABEL[kind]} is docked — un-embed it (its side menu) to use this`
      : EMBED_TOGGLE_TITLE[kind];
  }

  /** Toggle `kind`'s view open/closed, in whichever mode it's currently set
   *  to. The shared entry point every embeddable view's public hotkey
   *  method (`toggleTasksView`, `toggleGitView`, …) delegates to after its
   *  own view-specific gating and lazy `ensureXView()` — and, by extension,
   *  every OTHER thing that can ask a view to toggle: a header button, a
   *  keybinding (main.ts), a view's own internal ✕ / Escape handler
   *  (`onClose`, wired straight back to the matching `toggleXView`), and
   *  `pane-meta`'s branch-name click. Routing all of them through this ONE
   *  function is what makes the docked no-op below (#361 user-demo finding)
   *  actually cover every entry point, rather than needing the same guard
   *  copy-pasted into each caller — a single missed copy would silently
   *  reintroduce the bug for just that one entry point. See
   *  `embedtoggle.ts`'s own doc comment for why a docked view's toggle is
   *  disabled outright rather than fixed to correctly close/reopen it. */
  toggleView(kind: EmbedKind): void {
    const action = embedToggleAction(this.sideOf(kind) !== null, this.isViewVisible(kind));
    if (action === "noop") {
      showToast(`${EMBED_TOGGLE_LABEL[kind]} is docked — un-embed it (its side menu) to use this toggle.`, "info");
      return;
    }
    // #1042: the cwd declaration belongs HERE, after the action is known, for
    // the same reason the docked no-op does — this is the one function every
    // entry point routes through, and it is the only place that knows whether
    // the gesture is an open. Declaring from the `toggleXView` wrappers instead
    // (as this first shipped) declared on close and on the docked no-op too,
    // turning a dismissal into a permanent declaration of an agent-chosen
    // directory (#1092 review).
    if (toggleDeclaresCwd(kind, action)) this.declareCwd();
    if (action === "close") this.closeView(kind);
    else this.openView(kind);
  }

  /** Show the side-picker menu (#361) — a view's own header embed button,
   *  clicked. Left/Right/Bottom (the currently-docked one, if any, checked),
   *  plus "Un-embed" when it's docked anywhere. Built and shown here, not in
   *  each view: the views don't need to know `EmbedSide` exists at all, only
   *  that clicking their button asks the pane "where should I go?" — same
   *  division of responsibility the rest of this engine already keeps
   *  (views are dumb UI; the pane owns embed state). Reuses
   *  `contextmenu.ts`'s existing `showContextMenu` rather than a bespoke
   *  dropdown. */
  showEmbedMenu(kind: EmbedKind, anchor: HTMLElement): void {
    const entry = this.embedRegistry.get(kind);
    if (!entry) return;
    const currentSide = this.sideOf(kind);
    const rect = anchor.getBoundingClientRect();
    const SIDE_LABEL: Record<EmbedSide, string> = { left: "Embed left", right: "Embed right", bottom: "Embed bottom" };
    const items: MenuItem<EmbedSide | "unembed">[] = EMBED_SIDES.map((side) => ({
      label: (currentSide === side ? "✓ " : "") + SIDE_LABEL[side],
      action: side,
    }));
    if (currentSide !== null) {
      items.push({ label: "", separator: true }, { label: "Un-embed — back to a floating overlay", action: "unembed" });
    }
    showContextMenu(rect.left, rect.bottom + 4, items, (action) => {
      if (action === "unembed") this.unembedView(kind);
      else this.embedViewAtSide(kind, action);
    });
  }

  /** Dock `kind` to `side` (#361) — the side-picker menu's action. Docking
   *  onto an OCCUPIED side SWAPS that ONE slot's occupant: whoever was there
   *  is CLOSED outright, not demoted back to an overlay (a silent reopen
   *  elsewhere would be a more surprising UX than "the slot now shows what
   *  you asked for, and the previous occupant is closed — the same one
   *  click that opened it reopens it") — the OTHER two slots are always
   *  left untouched. If `kind` is already docked to a DIFFERENT side, it
   *  moves (leaves that side first). Either way the slot's occupant +
   *  fraction are a PERSISTED preference (tabs.json, via
   *  `onRecordChanged`). A discrete, user-initiated layout change (see
   *  docs/design/embedded-panels.md) — never fired from a resize or a
   *  refresh. */
  private embedViewAtSide(kind: EmbedKind, side: EmbedSide): void {
    const entry = this.embedRegistry.get(kind);
    if (!entry) return;
    this.ensureEmbedHost();
    const currentSide = this.sideOf(kind);
    if (currentSide === side) {
      // Already docked here — just make sure it's actually showing (it may
      // be docked-but-closed).
      if (!this.isViewVisible(kind)) this.openView(kind);
      return;
    }
    // Leave whichever OTHER side this kind currently occupies, if any.
    // `closeView` MUST run before the slot is nulled out (#361 rev-58
    // blocking finding): it looks up `sideOf(kind)` itself to find which
    // slot to hide, so nulling first makes it take the OVERLAY branch
    // instead — the origin slot's now-empty panel+divider stay visible.
    // Mirrors the order the target-eviction block below (and `unembedView`)
    // already used correctly.
    if (currentSide !== null) {
      const wasVisible = this.isViewVisible(kind);
      if (wasVisible) this.closeView(kind);
      this.embedSlots![currentSide].kind = null;
    }
    // Evict whoever (if anyone) is currently on the TARGET side.
    const targetSlot = this.embedSlots![side];
    if (targetSlot.kind !== null) {
      const evicted = targetSlot.kind;
      this.embedRegistry.get(evicted)?.setPanelActive(false);
      this.closeView(evicted);
      targetSlot.kind = null;
      this.syncEmbedToggleButton(evicted);
    }
    const wasOverlayOpen = !entry.overlayEl.hidden;
    if (wasOverlayOpen) entry.overlayEl.hidden = true; // it's about to move into the slot
    targetSlot.kind = kind;
    targetSlot.frac = clampEmbedFrac(targetSlot.frac);
    entry.setPanelActive(true);
    this.openView(kind); // docking always shows it
    this.syncEmbedToggleButton(kind);
    // Right's occupancy just changed (moved onto it, off it, or evicted
    // from it) — left's composed far-side floor depends on that (see
    // dividerFloors's "left" case), so reclamp it too.
    if (side === "right" || currentSide === "right") this.reclampSlotDivider("left");
    this.pane.events.onRecordChanged(this.pane);
  }

  /** Un-dock `kind` (#361) — back to the floating overlay, staying open if
   *  it was. A no-op if it isn't currently docked anywhere. */
  private unembedView(kind: EmbedKind): void {
    const entry = this.embedRegistry.get(kind);
    if (!entry) return;
    const side = this.sideOf(kind);
    if (side === null) return;
    const wasVisible = this.isViewVisible(kind);
    if (wasVisible) this.closeView(kind);
    this.embedSlots![side].kind = null;
    entry.setPanelActive(false);
    this.syncEmbedToggleButton(kind);
    if (wasVisible) this.openView(kind);
    if (side === "right") this.reclampSlotDivider("left");
    this.pane.events.onRecordChanged(this.pane);
  }

  /** Reapply persisted embed preferences (#361) — called once, right after
   *  a resumed/restored orch pane is wired up with its group, so every view
   *  that was docked and open when the layout was captured comes back the
   *  same way, on the same side. Entries naming a kind this pane can't show
   *  right now (the gating button is hidden) are silently skipped — restore
   *  doesn't carry this far for git/issues either (see main.ts's
   *  `resumeDormantGroup`); only orchestration-family kinds
   *  (tasks/audit/group) are ever restored this way today — see
   *  `PersistedPane.embeds`'s decode. */
  restoreEmbeds(embeds: readonly { view: EmbedKind; side: EmbedSide; share: number }[]): void {
    if (!this.pane.orchGroup) return;
    for (const e of embeds) {
      switch (e.view) {
        case "tasks":
          if (this.pane.views.tasksBtn.hidden) continue;
          this.pane.views.ensureTasksView();
          break;
        case "decisions":
          if (this.pane.views.decisionsBtn.hidden) continue;
          this.pane.views.ensureDecisionsView();
          break;
        case "audit":
          if (this.pane.views.auditBtn.hidden) continue;
          this.pane.views.ensureAuditView();
          break;
        case "timeline":
          if (this.pane.views.timelineBtn.hidden) continue;
          this.pane.views.ensureTimelineView();
          break;
        case "tokens":
          if (this.pane.views.tokensBtn.hidden) continue;
          this.pane.views.ensureTokensView();
          break;
        case "group":
          if (this.pane.views.groupBtn.hidden) continue;
          this.pane.views.ensureGroupView();
          break;
        case "git":
          // Restoring a group's orchestrator pane means it's PTY-backed by
          // definition (a content pane is never `kind === "orch"`), so git's
          // button is never CSS-hidden here the way tasks/audit/group's own
          // JS-gated buttons can be — no `.hidden` check needed.
          this.pane.views.ensureGitView();
          break;
        case "editor":
          this.pane.views.ensureFileEditView();
          break;
        default:
          continue; // issues isn't captured for restore today — see RESTORABLE_EMBED_KINDS
      }
      this.ensureEmbedHost();
      const entry = this.embedRegistry.get(e.view)!;
      const slot = this.embedSlots![e.side];
      slot.kind = e.view;
      slot.frac = clampEmbedFrac(e.share);
      entry.setPanelActive(true);
      this.openView(e.view);
      this.syncEmbedToggleButton(e.view);
    }
  }

  /** Whichever overlay (git / tasks / audit / group) is currently covering
   *  the terminal. */
  activeOverlay(): HTMLElement | null {
    if (this.pane.views.gitOverlay && !this.pane.views.gitOverlay.hidden) return this.pane.views.gitOverlay;
    if (this.pane.views.issuesOverlay && !this.pane.views.issuesOverlay.hidden) return this.pane.views.issuesOverlay;
    if (this.pane.views.tasksOverlay && !this.pane.views.tasksOverlay.hidden) return this.pane.views.tasksOverlay;
    if (this.pane.views.decisionsOverlay && !this.pane.views.decisionsOverlay.hidden) return this.pane.views.decisionsOverlay;
    if (this.pane.views.auditOverlay && !this.pane.views.auditOverlay.hidden) return this.pane.views.auditOverlay;
    if (this.pane.views.timelineOverlay && !this.pane.views.timelineOverlay.hidden) return this.pane.views.timelineOverlay;
    if (this.pane.views.groupOverlay && !this.pane.views.groupOverlay.hidden) return this.pane.views.groupOverlay;
    if (this.pane.views.fileEditOverlay && !this.pane.views.fileEditOverlay.hidden) return this.pane.views.fileEditOverlay;
    return null;
  }
}
