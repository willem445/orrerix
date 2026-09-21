// The To-Do content pane (#3263 S4) — the S0 mock built for real. The mock is
// `demo/todo-pane` in PR #3271, which is UNMERGED at this slice: every citation
// of it below names a tree that is not on `main` and not in this diff, and is
// written so a reader who cannot find it knows why. The sixth `ContentPaneKind`, hosted in a grid cell like the file
// explorer, the editor, the git view, the workflow builder and the structured
// transcript.
//
// WHAT THIS MODULE IS, AND WHAT IT IS NOT. It owns ELEMENTS. Every decision it
// draws was made somewhere testable:
//
//   `todomodel.ts`  (#3263 S3) — the STORE's model: decode, the smart views,
//                    the Planned buckets, ordering, search, undo's inverse.
//   `todoview.ts`   (this slice) — the PANE's projection: the strip's counts,
//                    the rendered groups, the row budget and its elision, the
//                    tag rail, the per-viewer prefs, the draft discipline and
//                    the selection walk.
//   `todoquickadd.ts` (S3) — the natural-language parser behind the chips.
//   `todo.ts`       (S3) — the only module here that reaches the backend, and
//                    it does so through `transport.ts` (constraint 5).
//
// The split is the mock's own (`render.js` §"two halves"): a renderer that also
// decides what My Day contains can only be tested by mounting it.
//
// NO PTY, SO CONSTRAINT 1 HOLDS BY CONSTRUCTION. There is no terminal behind
// this pane — nothing here measures or resizes a ConPTY, and there is none to
// resize. This is the `structured` arm's argument, unchanged.
//
// NO UN-SUBMITTED VALUE LIVES IN AN ELEMENT. This is the discipline the whole
// module is shaped around, and it is not a style preference: `todo-changed`
// fires on EVERY successful write from EITHER writer, so an agent editing an
// unrelated row through MCP re-renders this pane — and a re-render rebuilds
// every control from its seed. A note held in a `<textarea>.value` is a note
// that write eats mid-sentence. So: the quick-add draft, each expanded row's
// notes and next-step field, the search term and the tag filter are FIELDS on
// this view; every control is seeded from them and writes back on `input`; and
// the caret is restored once, centrally, after each render.
//
// See `doc/design/todo-pane.md` §"The pane" — which is on `main` — and
// `demo/todo-pane/DESIGN.md` (PR #3271, unmerged).

import { CoalescingRefresh } from "./refreshgate";
import { showToast } from "./toast";
import {
  onTodoChanged,
  todoApply,
  todoSnapshot,
  type Actor,
  type Applied,
  type TodoItem,
  type TodoOp,
  type TodoSnapshot,
} from "./todo";
import {
  SMART_VIEWS,
  SMART_VIEW_LABEL,
  UndoStack,
  isArchived,
  isDone,
  moveTarget,
  needsRenumber,
  opLabel,
  visibleItems,
  type SmartView,
} from "./todomodel";
import { pruneFired, reminderSummary, scanReminders } from "./todoreminders";
import {
  beginRead,
  initialScope,
  isLoaded,
  readFailed,
  readLanded,
  scopeChanged,
  type ScopeState,
} from "./todoscope";
import {
  DEFAULT_TODO_PREFS,
  EMPTY_TEXT,
  TODO_PREFS_KEY,
  decodeTodoPrefs,
  encodeTodoPrefs,
  moveSelection,
  planReveal,
  projectPane,
  pruneDrafts,
  renderedRows,
  reseedPristineDrafts,
  rowDraftIsPristine,
  seedRowDraft,
  type PaneProjection,
  type RowDraft,
  type ScopeChoice,
  type TodoPrefs,
} from "./todoview";
import { formatDue, parseQuickAdd } from "./todoquickadd";

export interface TodoPaneOptions {
  /** The pane's ROOT — the workspace whose list the `◆` half of the scope
   *  switch shows. The caller resolves it (git work-tree root, falling back to
   *  the pane's cwd) and this view hands it to the backend RAW: `todo.ts`'s
   *  header and `doc/design/todo-pane.md` §"The caller names a ROOT, never a
   *  key" are the reason — the frontend must never name a workspace KEY, or two
   *  spellings of one project become two lists.
   *
   *  A getter rather than a value, like every other content view's `getRoot`:
   *  the pane can be re-rooted under the view. `""`/null is the legitimate
   *  rootless case (a pane opened off a terminal in the user's home), and it
   *  degrades to Global-only rather than to an error — see `scopeAvailable`. */
  getRoot: () => string | null;
  /** Never used: a content pane is closed by the pane's own ✕, exactly as the
   *  editor/git/workflow views are. Present so the shape matches its siblings. */
  onClose?: () => void;
  /** Always true here (the pane IS the surface). Kept so the constructor reads
   *  like `FileEditView`'s and `GitView`'s. */
  embedded?: boolean;
  /** The clock, INJECTED — the same rule `todomodel.ts` and `todoquickadd.ts`
   *  hold, one layer up. Nothing below calls `Date.now()` directly; a render
   *  takes one reading and every bucket, every relative due label and every
   *  parse in that frame agrees with it. */
  now?: () => number;
  /** `localStorage`, injected so the prefs path is exercisable without a
   *  browser. Absent = the real one, reached through a guard (see `readPrefs`). */
  storage?: Pick<Storage, "getItem" | "setItem">;
}

/** How far a `#tag` chip may be from its row before the rail stops showing it.
 *  The footer shows the first N of the scope's tags; the rest are reachable by
 *  typing `#tag` into search. A rail that wrapped to four lines would take the
 *  list's room to display metadata about the list. */
const TAG_RAIL_MAX = 8;

/**
 * How often a VISIBLE todo pane re-scans for reminders and re-renders.
 *
 * One minute: the finest granularity a human reads off a due time ("4pm"), and
 * cheap — a tick is a filter over an in-memory array this view already holds
 * plus one `render()`, with NO IPC and no store write of any kind
 * (`todoreminders.ts` carries that rule and why it is structural).
 *
 * Declared HERE rather than beside the pure scan because the cadence belongs
 * to the timer, not to the predicate: `test/perfpolicy.test.ts`'s TIMERS
 * manifest resolves a cadence from a literal or a same-file constant, so a
 * cadence imported from elsewhere is one the scan cannot pin (INV-4 —
 * "declare the cadence").
 */
const REMINDER_TICK_MS = 60_000;

const CHECK_SVG =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" ' +
  'stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M5 13l4.5 4.5L19 7"/></svg>';
const STAR_SVG =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" ' +
  'stroke-linejoin="round" aria-hidden="true"><path d="M12 3.6l2.6 5.5 5.9.8-4.3 4.2 1 5.9-5.2-2.8' +
  '-5.2 2.8 1-5.9L3.5 9.9l5.9-.8z"/></svg>';
const SUN_SVG =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" ' +
  'stroke-linecap="round" aria-hidden="true"><circle cx="12" cy="12" r="4"/><path d="M12 2.5v2' +
  'M12 19.5v2M2.5 12h2M19.5 12h2M5.2 5.2l1.4 1.4M17.4 17.4l1.4 1.4M18.8 5.2l-1.4 1.4M6.6 17.4l-1.4 1.4"/></svg>';
const CHEV_SVG =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" ' +
  'stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 6l6 6-6 6"/></svg>';

/** Element builder. Attributes by name, text as `text`, children flattened —
 *  the mock's `el()`, which is also `structuredpane.ts`'s shape. Never
 *  `innerHTML` for CONTENT: only the four icon constants above go in that way,
 *  and each is a literal in this file. */
function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs?: Record<string, string | number | boolean | null | undefined>,
  ...kids: (Node | string | null | false | undefined)[]
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs ?? {})) {
    if (v === false || v === null || v === undefined) continue;
    if (k === "class") node.className = String(v);
    else if (k === "text") node.textContent = String(v);
    else node.setAttribute(k, v === true ? "" : String(v));
  }
  for (const kid of kids) {
    if (kid === null || kid === undefined || kid === false) continue;
    node.append(kid);
  }
  return node;
}

function icon(markup: string, cls: string): HTMLElement {
  const span = el("span", { class: cls, "aria-hidden": "true" });
  span.innerHTML = markup; // one of the four literals above — never user text
  return span;
}

/**
 * The attribution dot's class list, or null when there is no dot.
 *
 * **Absence is the human** (DESIGN.md §3): spending an identity pigment on the
 * majority case would make the marks mean "everyone" rather than "which agent".
 *
 * **The hue comes from the app's ONE role table**, not from a hash of the agent
 * id. DESIGN.md §3 asks for a hue "assigned once, in one place, so the same
 * agent is the same colour in the pane header, the session list and here", and
 * this repo already has that table — `theme.ts`'s identity channel, mapped per
 * ROLE, with `test/theme.test.ts`'s "one role table" guard holding every
 * surface to it. A locally-hashed hue would be a SECOND answer to a question
 * the app has already answered, which is the drift that guard exists to catch;
 * so the dot is classed `role-<role>` and the stylesheet names the token, like
 * the group roster's chip and the workflow canvas's node.
 *
 * An actor whose `role` this build does not know — a newer orrerix writing a
 * role that did not exist yet — keeps the dot and falls back to the base rule's
 * neutral ink. That is a DELIBERATE and visibly different third state: a dot
 * means an agent, no dot means the human, and a colourless dot means an agent
 * this build cannot classify. It is not the uncoloured-badge bug the role
 * table's `complete: true` check exists for, which is a role this build DOES
 * know and forgot to paint.
 */
function attributionClass(actor: Actor): string | null {
  if (actor.kind !== "agent") return null;
  // The role string comes off a store two processes write, so it is untrusted
  // text, not an `OrchRole`. Anything outside the alphabet a class may carry is
  // dropped rather than interpolated into a selector.
  const role = /^[a-z]+$/.test(actor.role) ? actor.role : "";
  return role === "" ? "tdp-dot" : `tdp-dot role-${role}`;
}

/** `worker-3 · 12m`, the faint byline under an agent-touched row's title. */
function agoLabel(thenMs: number, nowMs: number): string {
  const mins = Math.max(0, Math.round((nowMs - thenMs) / 60000));
  if (mins < 60) return `${mins}m`;
  const hours = Math.round(mins / 60);
  return hours < 48 ? `${hours}h` : `${Math.round(hours / 24)}d`;
}

/** Which element had the caret, so one central restore can put it back. The
 *  render is wholesale, so this is how a half-typed line survives a re-render
 *  that an AGENT caused — see the module header. */
interface CaretMark {
  act: string;
  row: string | null;
  start: number;
  end: number;
}

/** Which writes volunteer an Undo button. The three that take something OFF
 *  the screen: completing it, deleting it, archiving it. Everything else is
 *  still on the stack and still reachable with `u` — it just does not
 *  interrupt to say so. (An un-complete is excluded for the same reason it is
 *  labelled "Reopened": it PUTS something back, so there is nothing to miss.) */
function offersUndoToast(op: TodoOp): boolean {
  if ("delete" in op) return true;
  if ("archive" in op) return op.archive.archived;
  return "complete" in op && op.complete.done;
}

export class TodoPaneView {
  readonly el: HTMLElement;

  private readonly opts: TodoPaneOptions;
  private readonly now: () => number;
  private readonly refresher = new CoalescingRefresh(() => this.refreshNow());

  // ── what the backend said ───────────────────────────────────────────────
  // THE SCOPE AND THE SNAPSHOT TRAVEL TOGETHER, in `todoscope.ts`'s pure state
  // machine, because both defects this pane has had in that area came from
  // moving one without the other (#3293 rounds 2 and 3). Nothing below assigns
  // these fields by hand: it calls `readLanded` / `readFailed` /
  // `scopeChanged` and keeps what comes back.
  private scope: ScopeState<TodoSnapshot> = initialScope<TodoSnapshot>(null);

  // ── what the human has done and not submitted ───────────────────────────
  // Every field below is the view's, never an element's. See the header.
  private prefs: TodoPrefs = { ...DEFAULT_TODO_PREFS };
  private query = "";
  private tagFilter: string | null = null;
  private draft = "";
  private selected: string | null = null;
  private searchOpen = false;
  private showRoot = false;
  private readonly expanded = new Set<string>();
  private readonly drafts = new Map<string, RowDraft>();

  // ── undo, reminders ─────────────────────────────────────────────────────
  private readonly undos = new UndoStack();
  /** Reminder keys this PANE has already shown. Per-viewer and dropped with the
   *  pane: reminders never write to the store (`todoreminders.ts` carries the
   *  argument and the honest cost). */
  private readonly fired = new Set<string>();
  private reminderTimer: ReturnType<typeof setInterval> | null = null;
  /** The Completed view's "show archived" toggle. A reading position, not a
   *  preference — so it is NOT persisted, for `TODO_PREFS_KEY`'s stated reason
   *  about expanded rows. */
  private showArchived = false;

  private unlisten: (() => void) | null = null;
  private visible = false;
  private disposed = false;
  private caret: CaretMark | null = null;

  constructor(opts: TodoPaneOptions) {
    this.opts = opts;
    this.now = opts.now ?? (() => Date.now());
    this.prefs = this.readPrefs();
    this.scope = initialScope<TodoSnapshot>(this.scopeRoot());

    this.el = el("div", { class: "tdp", tabindex: "-1" });
    // ONE delegated listener per event on the root, rather than a handler per
    // control: the DOM is replaced wholesale on every render, so per-control
    // listeners would be re-attached hundreds of times a burst and every one of
    // them would have to be dropped again. Delegation survives the replace.
    this.el.addEventListener("click", this.onClick);
    this.el.addEventListener("input", this.onInput);
    this.el.addEventListener("keydown", this.onKeyDown);
    this.render();
  }

  // ── lifecycle ─────────────────────────────────────────────────────────────

  /** Attach-then-show, the contract every content view here follows. */
  show(): void {
    if (this.disposed) return;
    this.visible = true;
    // The bound INV-3 declares: the pane re-reads through a `CoalescingRefresh`
    // rather than per event. `todo-changed` fires on every successful write
    // from EITHER writer, and agents write in bursts, so a burst costs the
    // refetch already in flight plus exactly one more — and the trailing run
    // reads the final store. This is the consumer `test/perfpolicy.test.ts`'s
    // row was waiting for; the row moves from `argued-none` to `throttled` in
    // the same commit.
    if (this.unlisten === null) {
      void onTodoChanged(() => this.refresher.request())
        .then((un) => {
          // Disposed while the subscribe was in flight: unlisten immediately
          // rather than parking a handler on a dead view.
          if (this.disposed) un();
          else this.unlisten = un;
        })
        .catch((err) => console.error("[orrerix] todo-changed subscribe failed", err));
    }
    // THE REMINDER TICK. One interval per visible pane, and it does two jobs
    // that both have to happen on a clock rather than on an event: it scans for
    // items that have come due (`todoreminders.ts`), and it re-renders so the
    // time-dependent parts of the view — the overdue dye, the relative "12m"
    // byline and, since S5, My Day emptying itself at local midnight — do not
    // sit frozen on a pane nobody has touched since yesterday.
    //
    // NOTHING HERE WRITES. That is the module's rule, and it is what makes a
    // per-viewer timer safe on a store two processes write to.
    if (this.reminderTimer === null) {
      this.reminderTimer = setInterval(() => this.tick(), REMINDER_TICK_MS);
    }
    this.refresher.request();
    this.tick();
  }

  hide(): void {
    this.visible = false;
    // A hidden pane stops ticking: `refreshNow` already drops a wake for one
    // (the `wakegate.ts` rule), and a reminder toast fired by a pane nobody is
    // looking at would arrive with an action that opens... this pane. `show()`
    // re-arms and scans immediately, so nothing due in the meantime is lost —
    // it arrives on the next look, which is when it is useful.
    this.stopTicking();
  }

  private stopTicking(): void {
    if (this.reminderTimer !== null) {
      clearInterval(this.reminderTimer);
      this.reminderTimer = null;
    }
  }

  /**
   * One reminder scan, plus the re-render that keeps the clock-dependent view
   * honest.
   *
   * The scan is `todoreminders.ts`'s pure function; this side owns only the
   * `fired` set and the toast. The set is pruned FIRST so a rescheduled or
   * deleted item cannot hold a key forever, and every key the scan consumed is
   * added — including the ones it suppressed as too old, which is what stops a
   * long-closed laptop dumping a day of notices on wake.
   */
  private tick(): void {
    if (this.disposed || !this.visible) return;
    const items = this.items();
    pruneFired(this.fired, items);
    const scan = scanReminders(items, this.now(), this.fired);
    for (const key of scan.fired) this.fired.add(key);
    // ONE TOAST PER TICK, however many came due. The app has one toast
    // element, so a loop here would have each call overwrite the last and the
    // human would see only the final notice — a silent loss of the thing the
    // feature exists for (#3301 review round 1). `reminderSummary` owns the
    // wording; the action opens the SOONEST, which is `notices[0]` because the
    // scan returns them soonest-first.
    if (scan.notices.length > 0) {
      const first = scan.notices[0];
      showToast(reminderSummary(scan.notices), "info", {
        label: scan.notices.length > 1 ? "Show first" : "Show",
        run: () => this.revealRow(first.id),
      });
    }
    this.render();
  }

  /**
   * Put a row on screen and select it — the toast action's one gesture.
   *
   * It may have to MOVE THE VIEW to do it: a reminder fires on an item that is
   * due, and the pane may well be sitting on Important or on a tag filter that
   * does not contain it. Showing the toast and then doing nothing visible when
   * it is clicked is precisely the silently-dead control this pane's design
   * note argues against, so the filters that could hide the row are cleared and
   * the view falls back to `all`, which holds every open item.
   */
  private revealRow(id: string): void {
    const nowMs = this.now();
    // WHAT "SHOW" CAN ACTUALLY DO IS A DECISION, and it lives in `todoview.ts`
    // so it is testable without a DOM. The answer is sometimes "nothing", and
    // this used to BE nothing — silently (#3301 review round 2).
    const plan = planReveal(this.items(), id, renderedRows(this.project(nowMs)));
    if (plan.kind === "gone") {
      showToast("That to-do is no longer on this list.", "info");
      return;
    }
    if (plan.kind === "left") {
      // The likely case, and the one that was silent: an agent finished or
      // archived the row between the notice and the click. Say which, because
      // the two have different answers — a finished row is in Completed, an
      // archived one needs the toggle there.
      showToast(
        plan.why === "done"
          ? "That one was completed since the reminder — it is in Completed."
          : "That one was archived since the reminder — turn on Show archived in Completed.",
        "info"
      );
      return;
    }
    this.query = "";
    this.tagFilter = null;
    this.searchOpen = false;
    if (plan.view !== null) {
      // NOT PERSISTED (#3301 review round 1, rev-final). The stored view is
      // "what a fresh pane opens on" — a preference the human expressed by
      // clicking the strip. Jumping to All because a reminder fired is
      // navigation the pane did on its own, and writing it to `localStorage`
      // would let a notification silently redefine a setting: every pane
      // opened afterwards, in every window, would start on All because
      // something came due once while this one happened to be on Important.
      this.setView(plan.view, { persist: false });
    }
    this.selected = id;
    this.expanded.add(id);
    this.render();
    this.focus();
    this.el.querySelector<HTMLElement>(`[data-id="${CSS.escape(id)}"]`)?.scrollIntoView({
      block: "nearest",
    });
  }

  dispose(): void {
    this.disposed = true;
    this.visible = false;
    this.stopTicking();
    this.undos.clear();
    this.fired.clear();
    this.unlisten?.();
    this.unlisten = null;
    this.el.removeEventListener("click", this.onClick);
    this.el.removeEventListener("input", this.onInput);
    this.el.removeEventListener("keydown", this.onKeyDown);
    this.drafts.clear();
    this.expanded.clear();
  }

  /** Focus the pane's own key surface, so the single-letter map works the
   *  moment the pane is opened or focused (the `Alt+J` chord's whole point). */
  focus(): void {
    this.el.focus();
  }

  // ── the store ─────────────────────────────────────────────────────────────

  /** The root as the backend takes it: the workspace root when the switch is on
   *  `◆`, and `null` — the GLOBAL list — otherwise. Raw, never a key. */
  private scopeRoot(): string | null {
    if (this.prefs.scope === "global") return null;
    const root = this.opts.getRoot();
    return root && root.trim() !== "" ? root : null;
  }

  /** Can this pane offer a workspace list at all? A todo pane opened off a
   *  terminal in the user's home has no repo root, and the `◆` half is then
   *  disabled rather than hidden: hiding it would make the switch look like a
   *  feature the build does not have, where a disabled half with the reason on
   *  hover says "point this pane at a folder". */
  private scopeAvailable(): boolean {
    const root = this.opts.getRoot();
    return root !== null && root.trim() !== "";
  }

  private async refreshNow(): Promise<void> {
    if (this.disposed) return;
    // A HIDDEN pane drops the wake outright rather than coalescing it — the
    // `wakegate.ts` rule (#1318): a surface nobody is looking at should not pay
    // a snapshot per agent burst. Nothing is lost by dropping it because
    // `show()` refreshes unconditionally, which is the half that makes the drop
    // safe rather than merely cheap.
    if (!this.visible) return;
    // THE STALE-RESPONSE GUARD, and it is the second half `refreshgate.ts`'s own
    // header says is needed: "the gate alone would still let a slow old-mode
    // fetch paint stale data, and the mode check alone would leave the new mode
    // with nothing to render" — `IssuesView` carries both, and this pane was
    // shipped with only the gate (#3293 review round 2, finding 2).
    //
    // The scope is captured BEFORE the await and compared after, because a
    // `TodoSnapshot` carries no scope of its own and so cannot identify itself.
    // Without it: a `todo-changed` refresh is in flight against the workspace
    // root, the human presses `g`, and the WORKSPACE list paints into a pane
    // whose header, switch and `scopeRoot()` all say Global. The trailing
    // coalesced run corrects it a round-trip later — but in that window the rows
    // are live, and completing one sends the op with `scopeRoot() === null`, so
    // it lands on the global store against an id that is not there and comes
    // back as a refusal the human cannot interpret.
    //
    // Dropping the response loses nothing: `setScope` has already asked for a
    // fresh run, and `CoalescingRefresh` guarantees the trailing one.
    const want = beginRead(this.scope);
    try {
      const snap = await todoSnapshot(want);
      const next = readLanded(this.scope, want, snap);
      if (next === this.scope) return; // stale: the scope moved under the read
      this.scope = next;
    } catch (err) {
      // A failed READ never publishes an empty list over a real one: the pane
      // keeps the snapshot it had and says the read failed. `todoSnapshot`
      // itself does not throw on a malformed payload (it decodes defensively),
      // so reaching here means the COMMAND failed — a degraded backend, not a
      // degraded row.
      // The same guard on this arm, for the same reason: a read that failed for
      // the scope we have since LEFT says nothing about the one we are on, and
      // flagging the pane "stale" over it would be a warning about a list
      // nobody is looking at.
      const next = readFailed(this.scope, want);
      if (next === this.scope) return;
      this.scope = next;
      console.error("[orrerix] todo snapshot failed", err);
    }
    if (!this.disposed) this.render();
  }

  /**
   * Send one op and re-read.
   *
   * Refusals are the human's to see, not the console's: a cap, an unknown id, a
   * store a newer build wrote. `todoApply` rejects with the backend's own
   * message precisely so it can go straight into a toast
   * (`doc/design/todo-pane.md` §"Caps refuse; they never truncate").
   *
   * It does NOT re-read on success: the backend emits `todo-changed` for every
   * successful write including this one, and the subscription's
   * `CoalescingRefresh` is already the path back. Re-reading here as well would
   * make every one of the pane's own writes cost two snapshots.
   */
  private async apply(op: TodoOp): Promise<Applied | null> {
    try {
      return await todoApply(op, this.scope.root);
    } catch (err) {
      showToast(String(err instanceof Error ? err.message : err), "error");
      // The write was refused, so the store is byte-identical and NO event will
      // fire. Re-render off what we already have, so a control the human just
      // flipped optimistically snaps back rather than lying.
      this.render();
      return null;
    }
  }

  /**
   * Send one op, record its inverse, and offer the undo.
   *
   * **The `before` snapshot is read HERE, before the await**, because it is a
   * fact only this frame has: `inverseOp` needs the item as the human was
   * looking at it, and by the time the write returns the store may have moved
   * under it. Reading it afterwards is the "best-effort guess" `inverseOp`
   * refuses to make.
   *
   * A write with no honest inverse is applied and NOT offered an undo — the
   * stack says so and this passes the silence on rather than showing a button
   * that would do nothing. A refused write pushes nothing: `apply` already
   * returned null and the store is byte-identical.
   */
  private async applyUndoable(op: TodoOp, before: TodoItem | null): Promise<Applied | null> {
    const applied = await this.apply(op);
    if (applied === null) return null;
    const inverse = this.undos.push(op, before, applied);
    // THE TOAST IS NARROWER THAN THE STACK, deliberately. `u` undoes any of
    // these; only the three that make something DISAPPEAR volunteer a button.
    // A toast after every quick-add is five toasts for five items, and a
    // channel that fires on everything is one a human learns to ignore —
    // which would cost the reminders beside it, not just this.
    if ("op" in inverse && offersUndoToast(op)) {
      showToast(`${opLabel(op)} · undo?`, "info", { label: "Undo", run: () => void this.undo() });
    }
    return applied;
  }

  /**
   * Apply the most recent inverse.
   *
   * The entry is popped BEFORE the write and never re-queued if the write is
   * refused: an undo can genuinely fail — a tombstone past its 30-day purge
   * window, a scope at `ITEMS_MAX` — and re-offering a gesture that is now
   * known not to work is worse than saying so once, which `apply`'s toast
   * already does with the backend's own message.
   *
   * The undo of an undo is NOT pushed: `u` is a history walk, not a toggle,
   * and pushing the inverse of an undo would make the second press put the
   * first one back forever.
   */
  private async undo(): Promise<void> {
    const entry = this.undos.pop();
    if (entry === null) {
      showToast("Nothing to undo in this pane.", "info");
      return;
    }
    await this.apply(entry.op);
  }

  // ── the projection ────────────────────────────────────────────────────────

  private items(): readonly TodoItem[] {
    return this.scope.snapshot?.items ?? [];
  }

  private project(nowMs: number): PaneProjection {
    return projectPane(
      {
        items: this.items(),
        view: this.prefs.view,
        query: this.query,
        tagFilter: this.tagFilter,
        showArchived: this.showArchived,
        loaded: isLoaded(this.scope),
      },
      nowMs
    );
  }

  private itemById(id: string): TodoItem | null {
    return this.items().find((i) => i.id === id) ?? null;
  }

  /** The draft for an expanded row, created (seeded from the ITEM) on demand. */
  private draftFor(item: TodoItem): RowDraft {
    let d = this.drafts.get(item.id);
    if (d === undefined) {
      d = seedRowDraft(item);
      this.drafts.set(item.id, d);
    }
    return d;
  }

  // ── prefs ─────────────────────────────────────────────────────────────────

  /** `localStorage` THROWS in a profile with site data blocked, and in some
   *  private windows — so every touch is guarded and a failure costs the human
   *  their remembered view and nothing else. Never a `catch` that swallows the
   *  list: no item ever passes through here. */
  private readPrefs(): TodoPrefs {
    try {
      const store = this.opts.storage ?? globalThis.localStorage;
      return decodeTodoPrefs(store.getItem(TODO_PREFS_KEY));
    } catch {
      return { ...DEFAULT_TODO_PREFS };
    }
  }

  private writePrefs(): void {
    try {
      const store = this.opts.storage ?? globalThis.localStorage;
      store.setItem(TODO_PREFS_KEY, encodeTodoPrefs(this.prefs));
    } catch {
      /* a viewer convenience; losing it costs nothing the human owns */
    }
  }

  // ── rendering ─────────────────────────────────────────────────────────────

  /**
   * Rebuild the whole pane from state.
   *
   * WHOLESALE, on purpose — the in-list-editor rule (`CLAUDE.md`, and the S0
   * mock's §7). The board's own lesson is that a list which re-renders on every
   * agent write eats an un-submitted value the moment that value lives in an
   * `<input>`. Here nothing does: every control is seeded from a field on this
   * view, so a rebuild is free of that whole class, and the ONE cost — the
   * caret — is paid in one place, below.
   */
  private render(): void {
    if (this.disposed) return;
    const nowMs = this.now();
    this.markCaret();
    const vm = this.project(nowMs);

    // Prune the per-row maps against what the store still has, beside each
    // other: a map keyed by item id that nothing removes from grows for the
    // life of the pane against ids an agent may have deleted — and a stale
    // draft is worse than a leak, since a re-created id would inherit a
    // stranger's half-typed note.
    const liveIds = new Set(this.items().map((i) => i.id));
    pruneDrafts(this.drafts, liveIds);
    // Follow the store on any row the human has NOT typed into (#3293 review
    // round 2). Without this, a draft seeded before an agent edited the row
    // keeps the pre-agent notes, and Save ships them back — silently reverting
    // a write nobody saw. A draft the human HAS typed into is never touched.
    reseedPristineDrafts(this.drafts, this.items());
    for (const id of [...this.expanded]) if (!liveIds.has(id)) this.expanded.delete(id);
    if (this.selected !== null && !liveIds.has(this.selected)) this.selected = null;

    const frag = document.createDocumentFragment();
    frag.append(this.header(vm));
    frag.append(this.strip(vm));
    if (this.prefs.view === "completed") frag.append(this.completedBar(vm));
    if (this.tagFilter !== null) frag.append(this.filterBar());
    frag.append(this.quickAdd(nowMs));
    frag.append(this.list(vm, nowMs));
    frag.append(this.footer(vm));
    this.el.replaceChildren(frag);
    this.restoreCaret();
  }

  /** Record which field had the caret, and where. */
  private markCaret(): void {
    const active = document.activeElement;
    if (!(active instanceof HTMLInputElement) || !this.el.contains(active)) {
      if (!(active instanceof HTMLTextAreaElement) || !this.el.contains(active)) {
        this.caret = null;
        return;
      }
    }
    const field = active as HTMLInputElement | HTMLTextAreaElement;
    const act = field.dataset.act ?? "";
    this.caret = {
      act,
      row: field.closest<HTMLElement>("[data-id]")?.dataset.id ?? null,
      start: field.selectionStart ?? 0,
      end: field.selectionEnd ?? 0,
    };
  }

  /** Put it back, once, centrally. The alternative — restoring per control as
   *  it is built — is the same work spread over N sites, and the one that gets
   *  forgotten is the one an agent's write interrupts. */
  private restoreCaret(): void {
    const c = this.caret;
    if (c === null) return;
    const scope = c.row === null ? this.el : this.el.querySelector(`[data-id="${CSS.escape(c.row)}"]`);
    const field = scope?.querySelector<HTMLInputElement | HTMLTextAreaElement>(
      `[data-act="${c.act}"]`
    );
    if (!field) return;
    field.focus();
    try {
      field.setSelectionRange(c.start, c.end);
    } catch {
      /* a control that does not support a selection range (none today) */
    }
  }

  private header(vm: PaneProjection): HTMLElement {
    const head = el("header", { class: "tdp-head" });

    const root = this.opts.getRoot() ?? "";
    const hasWorkspace = this.scopeAvailable();
    const scope = el("div", { class: "tdp-scope", role: "group", "aria-label": "Scope" });
    scope.append(
      el(
        "button",
        {
          class: "tdp-scope-btn",
          type: "button",
          "data-act": "scope",
          "data-scope": "global",
          "aria-pressed": this.prefs.scope === "global" ? "true" : "false",
        },
        el("span", { class: "tdp-scope-mark", text: "◐" }),
        el("span", { text: "Global" })
      )
    );
    scope.append(
      el(
        "button",
        {
          class: "tdp-scope-btn",
          type: "button",
          "data-act": "scope",
          "data-scope": "workspace",
          "aria-pressed": this.prefs.scope === "workspace" && hasWorkspace ? "true" : "false",
          disabled: !hasWorkspace,
          title: hasWorkspace
            ? root
            : "This pane has no folder — point it at one for a per-project list.",
        },
        el("span", { class: "tdp-scope-mark", text: "◆" }),
        el("span", { text: hasWorkspace ? tail(root) : "Workspace" })
      )
    );
    head.append(scope);

    if (hasWorkspace) {
      head.append(
        el("button", {
          class: "tdp-rootbtn",
          type: "button",
          "data-act": "root",
          "aria-expanded": this.showRoot ? "true" : "false",
          "aria-label": "Show workspace path",
          text: "⋯",
        })
      );
      // The `⋯` reveal exists for the case two checkouts share a folder name —
      // the switch shows the tail, and this says which one it is.
      if (this.showRoot) head.append(el("code", { class: "tdp-rootpath tdp-mono", text: root }));
    }

    const search = el("div", { class: "tdp-search", "data-open": this.searchOpen ? "true" : "false" });
    search.append(
      el("input", {
        class: "tdp-search-in",
        type: "text",
        "data-act": "search",
        placeholder: "Search",
        value: this.query,
        "aria-label": "Search",
      })
    );
    head.append(search);

    head.append(el("span", { class: "tdp-grow" }));
    if (this.scope.snapshot?.read_only) {
      // A store a NEWER build wrote reads but refuses every write. Saying so
      // once, here, beats N identical refusal toasts as the human discovers it
      // control by control.
      head.append(
        el("span", {
          class: "tdp-ro",
          text: "read-only",
          title: "This list was written by a newer build of orrerix. It reads, but every change is refused.",
        })
      );
    }
    if (this.scope.loadFailed) {
      head.append(el("span", { class: "tdp-ro", text: "stale", title: "The last read failed — showing what was already loaded." }));
    }
    // The total is a claim about the list, so it waits for a read exactly as
    // the empty-state sentence and the strip's chips do (#3293 round 6
    // residual 1). A dash is the one honest thing to draw in that frame.
    head.append(
      el("span", {
        class: "tdp-count tdp-num",
        text: vm.countsKnown ? String(vm.total) : "—",
        title: vm.countsKnown ? "Rows matching" : "Reading the list…",
      })
    );
    return head;
  }

  private strip(vm: PaneProjection): HTMLElement {
    const strip = el("nav", { class: "tdp-strip", "aria-label": "Views" });
    SMART_VIEWS.forEach((v, i) => {
      strip.append(
        el(
          "button",
          {
            class: "tdp-chip",
            type: "button",
            "data-act": "view",
            "data-view": v,
            "aria-pressed": this.prefs.view === v ? "true" : "false",
            // The digit that selects it, so the keyboard map is discoverable
            // from the strip rather than only from the docs.
            title: `${SMART_VIEW_LABEL[v]} (${i + 1})`,
          },
          el("span", { text: SMART_VIEW_LABEL[v] }),
          // NOTHING, not a zero, until a snapshot has landed for this scope.
          // "My Day 0" in the Loading frame is a sentence about a list nobody
          // has read, and it is false whenever that list has rows — the same
          // rule the empty state already followed (#3293 round 6 residual 1).
          vm.countsKnown && el("span", { class: "tdp-chip-n tdp-num", text: String(vm.counts[v]) })
        )
      );
    });
    return strip;
  }

  /**
   * The Completed view's own two controls (#3263 S5).
   *
   * **Archive is the point of this view.** Completed is a log, and a log you
   * cannot clear becomes one you stop opening — so the one bulk gesture in this
   * pane lives here and nowhere else. It archives exactly the rows ON SCREEN,
   * which is why the button says how many: the op carries the ids rather than a
   * scope, so what you saw is what moves, and its inverse is those same ids
   * with the flag flipped (`inverseOp`).
   *
   * **And the way back.** `inView` puts an archived item in no view at all, so
   * without the toggle beside it an archive would be a one-way door whose only
   * exit is an undo stack that dies with the pane. The toggle is a reading
   * position rather than a preference, so it is not persisted — the rule
   * `TODO_PREFS_KEY` states for expanded rows.
   */
  private completedBar(vm: PaneProjection): HTMLElement {
    const bar = el("div", { class: "tdp-donebar" });
    const archivable = this.archivableIds(vm);
    bar.append(
      el("button", {
        class: "tdp-donebtn",
        type: "button",
        "data-act": "archive-all",
        disabled: archivable.length === 0,
        title:
          archivable.length === 0
            ? "Nothing here to archive."
            : "Put these finished tasks away. They stay in the store and undo brings them back.",
        text: archivable.length === 0 ? "Archive" : `Archive ${archivable.length}`,
      })
    );
    bar.append(el("span", { class: "tdp-grow" }));
    bar.append(
      el("button", {
        class: "tdp-donebtn",
        type: "button",
        "data-act": "show-archived",
        "aria-pressed": this.showArchived ? "true" : "false",
        "data-on": this.showArchived ? "true" : "false",
        text: this.showArchived ? "Hide archived" : "Show archived",
      })
    );
    return bar;
  }

  /** The rows "Archive N" would move: what is on screen, minus anything already
   *  archived (which is only visible at all when the toggle is on). Sending an
   *  already-archived id would be a no-op the human cannot tell from a
   *  success, and it would pad the undo entry with ids it must not unarchive. */
  private archivableIds(vm: PaneProjection): string[] {
    return renderedRows(vm)
      .filter((i) => !isArchived(i))
      .map((i) => i.id);
  }

  private filterBar(): HTMLElement {
    return el(
      "div",
      { class: "tdp-filterbar" },
      el("button", {
        class: "tdp-filter-clear",
        type: "button",
        "data-act": "cleartag",
        text: `#${this.tagFilter} ✕`,
      })
    );
  }

  private quickAdd(nowMs: number): HTMLElement {
    // The chips are the parser's EVIDENCE, shown before Enter. The failure that
    // makes a natural-language quick-add untrustworthy is the silent one: a
    // word of your title read as a date, and the title you get is not the one
    // you typed. So the parse is on screen while you are still able to fix it.
    const parse = parseQuickAdd(this.draft, nowMs);
    const qa = el("div", { class: "tdp-qa", "data-valid": parse.valid ? "true" : "false" });
    qa.append(
      el("input", {
        class: "tdp-qa-in",
        type: "text",
        "data-act": "draft",
        placeholder: "Add a task — try “ship notes tomorrow at 4pm #release !!”",
        value: this.draft,
        "aria-label": "Quick add",
      })
    );
    const chips = el("div", { class: "tdp-qa-chips" });
    for (const chip of parse.chips) {
      chips.append(
        el("span", { class: "tdp-qa-chip", "data-kind": chip.kind, title: chip.raw, text: chip.label })
      );
    }
    qa.append(chips);
    return qa;
  }

  private list(vm: PaneProjection, nowMs: number): HTMLElement {
    const list = el("div", { class: "tdp-list", role: "list" });
    if (vm.empty) {
      // "NOTHING HERE" AND "NOTHING YET" ARE DIFFERENT FACTS, and only one of
      // them is safe to assert (#3293 review round 4, premortem 1).
      //
      // `this.snapshot === null` means no read has landed for the scope we are
      // on — the state `setScope` deliberately creates so the previous scope's
      // rows cannot paint under the new header. In that frame the view-model is
      // empty because we have not LOOKED, not because the list is. Saying
      // "Nothing for today — add one, or pull from Planned" there is a sentence
      // about a list nobody has read, and it is false whenever that list has
      // rows. It is normally one frame; on a slow backend it is however long
      // the read takes, and the slower it gets the longer the pane lies.
      //
      // So the empty STATE is only claimed once a snapshot exists. This is the
      // same rule the pane already follows on the read path, where a failed
      // read keeps the list it had rather than publishing an emptiness it
      // cannot vouch for.
      const loading = !vm.countsKnown;
      list.append(
        el(
          "div",
          { class: "tdp-empty" },
          el("p", {
            class: "tdp-empty-text",
            text: loading ? "Loading…" : EMPTY_TEXT[vm.emptyReason],
          }),
          // The hint is about the keyboard, not about the list, so it is true
          // either way — but it is noise under a one-frame "Loading…".
          !loading &&
            el("p", {
              class: "tdp-empty-hint",
              text: "Press n to add · / to search · 1-5 to switch view",
            })
        )
      );
      return list;
    }
    for (const group of vm.groups) {
      if (group.label !== null) {
        list.append(
          el(
            "div",
            { class: "tdp-bucket", "data-bucket": group.key },
            el("span", { text: group.label }),
            el("span", { class: "tdp-bucket-n tdp-num", text: String(group.items.length) })
          )
        );
      }
      for (const item of group.items) list.append(this.row(item, nowMs));
    }
    if (vm.elided > 0) {
      // STATE THE ELISION. A pane that silently stops at 200 rows has lied
      // about how much work there is (DESIGN.md §8).
      list.append(
        el(
          "div",
          { class: "tdp-elision" },
          el("span", { class: "tdp-num", text: String(vm.elided) }),
          el("span", { text: " more not shown — narrow with / or a #tag" })
        )
      );
    }
    return list;
  }

  private row(item: TodoItem, nowMs: number): HTMLElement {
    const open = this.expanded.has(item.id);
    const done = isDone(item);
    const row = el("div", {
      class: "tdp-row",
      "data-id": item.id,
      "data-done": done ? "true" : "false",
      "data-selected": this.selected === item.id ? "true" : "false",
      "data-expanded": open ? "true" : "false",
      "data-archived": isArchived(item) ? "true" : "false",
      "data-priority": String(item.priority),
      role: "listitem",
    });

    const head = el("div", { class: "tdp-rowhead" });
    head.append(
      el(
        "button",
        {
          class: "tdp-check",
          type: "button",
          "data-act": "toggle",
          "aria-pressed": done ? "true" : "false",
          "aria-label": `${done ? "Mark incomplete" : "Complete"}: ${item.title}`,
        },
        icon(CHECK_SVG, "tdp-check-mark")
      )
    );

    const main = el("div", { class: "tdp-rowmain" });
    const titleLine = el("div", { class: "tdp-titleline" });
    const dotClass = attributionClass(item.updated_by);
    if (dotClass !== null && item.updated_by.kind === "agent") {
      // IDENTITY CHANNEL, ONE POSITION. It says WHO, never how it is going —
      // a later slice wanting to show that an agent is mid-edit on a row needs
      // a different position, not this one's colour (DESIGN.md §3).
      titleLine.append(
        el("span", {
          class: dotClass,
          title: `${item.updated_by.id} — ${item.updated_by.role || "agent"}`,
        })
      );
    }
    titleLine.append(el("span", { class: "tdp-title", text: item.title }));
    main.append(titleLine);
    if (item.updated_by.kind === "agent") {
      main.append(
        el("div", {
          class: "tdp-byline",
          text: `${item.updated_by.id} · ${agoLabel(item.updated_ms, nowMs)}`,
        })
      );
    }
    const meta = this.metaLine(item, nowMs);
    if (meta !== null) main.append(meta);
    head.append(main);

    head.append(
      el(
        "button",
        {
          class: "tdp-star",
          type: "button",
          "data-act": "important",
          "aria-pressed": item.important ? "true" : "false",
          "aria-label": "Important",
        },
        icon(STAR_SVG, "tdp-star-mark")
      )
    );
    head.append(
      el(
        "button",
        {
          class: "tdp-chev",
          type: "button",
          "data-act": "expand",
          "aria-expanded": open ? "true" : "false",
          "aria-label": "Details",
        },
        icon(CHEV_SVG, "tdp-chev-mark")
      )
    );
    row.append(head);
    if (open) row.append(this.rowBody(item, nowMs));
    return row;
  }

  private metaLine(item: TodoItem, nowMs: number): HTMLElement | null {
    const bits: HTMLElement[] = [];
    if (item.due_ms !== null) {
      bits.push(
        el("span", {
          class: "tdp-due tdp-mono",
          // THE ONE STATE POSITION IN THIS PANE. The attention dye marks an
          // OVERDUE date and nothing else: a due date that is merely soon is
          // ink, because if everything upcoming is amber, nothing is.
          "data-overdue": !isDone(item) && item.due_ms < nowMs ? "true" : "false",
          text: formatDue(item.due_ms, nowMs, true),
        })
      );
    }
    if (item.steps.length > 0) {
      const done = item.steps.filter((s) => s.done).length;
      bits.push(
        el("span", { class: "tdp-steps tdp-num", text: `${done}/${item.steps.length}` })
      );
    }
    for (const tag of item.tags) {
      bits.push(
        el("button", { class: "tdp-metatag", type: "button", "data-act": "tag", "data-tag": tag, text: `#${tag}` })
      );
    }
    if (item.my_day !== null) bits.push(icon(SUN_SVG, "tdp-sun"));
    if (item.notes !== "") bits.push(el("span", { class: "tdp-notemark", text: "note" }));
    if (bits.length === 0) return null;
    const line = el("div", { class: "tdp-meta" });
    bits.forEach((b, i) => {
      if (i > 0) line.append(el("span", { class: "tdp-metasep", text: "·" }));
      line.append(b);
    });
    return line;
  }

  /** The inline expansion — never a side panel. The pane can be a 320 px grid
   *  cell, and a detail panel at that width is a modal with extra steps
   *  (DESIGN.md §5). */
  private rowBody(item: TodoItem, nowMs: number): HTMLElement {
    const draft = this.draftFor(item);
    const body = el("div", { class: "tdp-rowbody" });

    if (item.steps.length > 0) {
      const steps = el("div", { class: "tdp-steplist" });
      item.steps.forEach((s, i) => {
        steps.append(
          el(
            "button",
            {
              class: "tdp-step",
              type: "button",
              "data-act": "step",
              "data-step": String(i),
              "data-done": s.done ? "true" : "false",
            },
            icon(CHECK_SVG, "tdp-step-mark"),
            el("span", { text: s.title })
          )
        );
      });
      body.append(steps);
    }

    // Seeded FROM the draft, written back on `input` — never read at submit.
    body.append(
      el("input", {
        class: "tdp-stepadd",
        type: "text",
        "data-act": "step-add",
        placeholder: "Next step",
        value: draft.step,
      })
    );
    body.append(
      el("textarea", {
        class: "tdp-notes",
        "data-act": "notes",
        rows: "2",
        placeholder: "Notes",
        text: draft.notes,
      })
    );

    // THE DUE CONTROL (#3263 S5), and it is a text field rather than a date
    // picker on purpose: the quick-add bar already understands "fri 4pm",
    // "tomorrow" and "next mon", and `parseQuickAdd` is the one place that
    // grammar lives. A second date vocabulary in the same pane — a calendar
    // widget with its own idea of what "next Monday" means — is exactly the
    // two-answers-to-one-question drift this repo's conventions bounce.
    //
    // Seeded from the DRAFT and written on `input`, like every other field
    // here; the current value is a label beside it, never the field's seed
    // (see `RowDraft.due`).
    const dueRow = el("div", { class: "tdp-duerow" });
    dueRow.append(
      el("input", {
        class: "tdp-duein",
        type: "text",
        "data-act": "due",
        placeholder: item.due_ms !== null ? formatDue(item.due_ms, nowMs, true) : "Due — try “fri 4pm”",
        value: draft.due,
        "aria-label": "Due date",
      })
    );
    if (item.due_ms !== null) {
      dueRow.append(
        el("button", {
          class: "tdp-rowbtn",
          type: "button",
          "data-act": "due-clear",
          text: "Clear due",
        })
      );
    }
    body.append(dueRow);

    const controls = el("div", { class: "tdp-controls" });
    const pairs: [string, string, boolean][] = [
      ["myday", item.my_day !== null ? "In My Day" : "Add to My Day", item.my_day !== null],
      ["delete", "Delete", false],
    ];
    for (const [act, label, on] of pairs) {
      controls.append(
        el("button", {
          class: "tdp-rowbtn",
          type: "button",
          "data-act": act,
          "data-on": on ? "true" : "false",
          text: label,
        })
      );
    }
    // The notes/step draft is committed by an explicit control, NOT by the
    // render: the button route and the Enter route both go through
    // `commitDraft`, and the seed/pristine pair in `todoview.ts` is what stops
    // a re-render seeding it back from a literal (#1348 N1/N4).
    if (!rowDraftIsPristine(draft)) {
      controls.append(
        el("button", { class: "tdp-rowbtn tdp-save", type: "button", "data-act": "save", text: "Save" })
      );
    }
    body.append(controls);
    return body;
  }

  private footer(vm: PaneProjection): HTMLElement {
    const foot = el("footer", { class: "tdp-foot" });
    if (vm.tags.length > 0) {
      const rail = el("div", { class: "tdp-tagrail" });
      for (const t of vm.tags.slice(0, TAG_RAIL_MAX)) {
        rail.append(
          el("button", {
            class: "tdp-tagbtn",
            type: "button",
            "data-act": "tag",
            "data-tag": t,
            "data-on": this.tagFilter === t ? "true" : "false",
            text: `#${t}`,
          })
        );
      }
      foot.append(rail);
    }
    foot.append(el("span", { class: "tdp-grow" }));
    foot.append(
      el("span", {
        class: "tdp-hint",
        text: "n add · j/k move · space done · e details · u undo · g scope",
      })
    );
    return foot;
  }

  // ── gestures ──────────────────────────────────────────────────────────────

  private onClick = (ev: MouseEvent): void => {
    const target = ev.target;
    if (!(target instanceof Element)) return;
    const btn = target.closest<HTMLElement>("[data-act]");
    if (btn === null || !this.el.contains(btn)) return;
    const act = btn.dataset.act ?? "";
    const rowId = btn.closest<HTMLElement>("[data-id]")?.dataset.id ?? null;
    const item = rowId !== null ? this.itemById(rowId) : null;

    switch (act) {
      case "scope": {
        const next = (btn.dataset.scope ?? "global") as ScopeChoice;
        if (next === this.prefs.scope) return;
        this.setScope(next);
        return;
      }
      case "root":
        this.showRoot = !this.showRoot;
        this.render();
        return;
      case "view":
        this.setView((btn.dataset.view ?? "all") as SmartView);
        return;
      case "tag": {
        const tag = btn.dataset.tag ?? "";
        // Clicking the tag that is already filtering CLEARS it, so the same
        // gesture is its own undo.
        this.tagFilter = this.tagFilter === tag ? null : tag;
        this.render();
        return;
      }
      case "cleartag":
        this.tagFilter = null;
        this.render();
        return;
      case "toggle":
        if (item !== null) {
          void this.applyUndoable({ complete: { id: item.id, done: !isDone(item) } }, item);
        }
        return;
      case "important":
        if (item !== null) {
          void this.applyUndoable({ update: { id: item.id, important: !item.important } }, item);
        }
        return;
      case "expand":
        if (item !== null) this.toggleExpand(item.id);
        return;
      case "myday":
        if (item !== null) {
          void this.applyUndoable(
            { update: { id: item.id, my_day: item.my_day === null ? this.now() : null } },
            item
          );
        }
        return;
      case "due-clear":
        if (item !== null) void this.applyUndoable({ update: { id: item.id, due_ms: null } }, item);
        return;
      case "archive-all":
        void this.archiveShown();
        return;
      case "show-archived":
        this.showArchived = !this.showArchived;
        this.render();
        return;
      case "delete":
        if (item !== null) void this.applyUndoable({ delete: { id: item.id } }, item);
        return;
      case "step":
        if (item !== null) this.toggleStep(item, Number(btn.dataset.step ?? "-1"));
        return;
      case "save":
        if (item !== null) void this.commitDraft(item);
        return;
      case "undo":
        void this.undo();
        return;
      default:
        return;
    }
  };

  private onInput = (ev: Event): void => {
    const field = ev.target;
    if (!(field instanceof HTMLInputElement) && !(field instanceof HTMLTextAreaElement)) return;
    const act = field.dataset.act ?? "";
    const rowId = field.closest<HTMLElement>("[data-id]")?.dataset.id ?? null;
    switch (act) {
      case "draft":
        // Written on INPUT, to the view. The chips re-render from it, which is
        // what makes the parse live — and it is the same write that stops an
        // agent's unrelated edit eating the half-typed line.
        this.draft = field.value;
        this.render();
        return;
      case "search":
        this.query = field.value;
        this.render();
        return;
      case "notes":
      case "due":
      case "step-add": {
        const item = rowId !== null ? this.itemById(rowId) : null;
        if (item === null) return;
        const d = this.draftFor(item);
        if (act === "notes") d.notes = field.value;
        else if (act === "due") d.due = field.value;
        else d.step = field.value;
        // Re-render so the Save control appears the moment the draft stops
        // being pristine. The caret restore is what makes that free.
        this.render();
        return;
      }
      default:
        return;
    }
  };

  private onKeyDown = (ev: KeyboardEvent): void => {
    if (ev.altKey || ev.ctrlKey || ev.metaKey) return; // app chords, not ours
    const inField =
      ev.target instanceof HTMLInputElement || ev.target instanceof HTMLTextAreaElement;
    const act = inField ? ((ev.target as HTMLElement).dataset.act ?? "") : "";

    if (inField) {
      // A focused field SWALLOWS the single-letter map — which is the behaviour
      // you want, and is why `n` and `/` are the two ways in (DESIGN.md §6).
      if (ev.key === "Enter" && act === "draft") {
        ev.preventDefault();
        void this.submitQuickAdd();
        return;
      }
      if (ev.key === "Enter" && (act === "step-add" || act === "due")) {
        ev.preventDefault();
        const id = (ev.target as HTMLElement).closest<HTMLElement>("[data-id]")?.dataset.id ?? null;
        const item = id !== null ? this.itemById(id) : null;
        if (item !== null) void this.commitDraft(item);
        return;
      }
      if (ev.key === "Escape") {
        ev.preventDefault();
        (ev.target as HTMLElement).blur();
        this.el.focus();
        this.render();
        return;
      }
      return;
    }

    const rows = renderedRows(this.project(this.now()));
    const sel = this.selected !== null ? this.itemById(this.selected) : null;

    // REORDER IS Shift+↑/↓, NOT THE MOCK'S Alt+↑/↓ — a deliberate departure
    // from `demo/todo-pane/DESIGN.md` §6 (PR #3271, unmerged), and the one place
    // this pane does not
    // build what the mock drew. `Alt+ArrowUp`/`Alt+ArrowDown` are already the
    // app's `focus-up`/`focus-down` (`shortcuts.ts`), matched on `document` in
    // the CAPTURE phase and withheld from every pane — so a handler here would
    // never see them, and "reorder" would silently be "move focus to the pane
    // above". Shift+arrow is free of both the app's chords (every Alt/Ctrl
    // block in `matchShortcut` is guarded) and the CLI question, since it never
    // leaves the webview. Recorded in the design note's §pane keyboard map so
    // the mock's table and the shipped one do not quietly disagree.
    if (ev.shiftKey && (ev.key === "ArrowUp" || ev.key === "ArrowDown")) {
      ev.preventDefault();
      this.moveSelected(ev.key === "ArrowUp" ? -1 : 1);
      return;
    }
    if (ev.shiftKey) return; // no other Shift chord is ours

    const viewIx = ["1", "2", "3", "4", "5"].indexOf(ev.key);
    if (viewIx >= 0) {
      ev.preventDefault();
      this.setView(SMART_VIEWS[viewIx]);
      return;
    }

    switch (ev.key) {
      case "n":
        ev.preventDefault();
        this.focusField('[data-act="draft"]');
        return;
      case "/":
        ev.preventDefault();
        this.searchOpen = true;
        this.render();
        this.focusField('[data-act="search"]');
        return;
      case "j":
      case "ArrowDown":
        ev.preventDefault();
        this.selected = moveSelection(rows, this.selected, 1);
        this.render();
        return;
      case "k":
      case "ArrowUp":
        ev.preventDefault();
        this.selected = moveSelection(rows, this.selected, -1);
        this.render();
        return;
      case " ":
        if (sel === null) return;
        ev.preventDefault();
        void this.applyUndoable({ complete: { id: sel.id, done: !isDone(sel) } }, sel);
        return;
      case "e":
      case "d":
        if (sel === null) return;
        ev.preventDefault();
        this.toggleExpand(sel.id);
        return;
      case "i":
        if (sel === null) return;
        ev.preventDefault();
        void this.applyUndoable({ update: { id: sel.id, important: !sel.important } }, sel);
        return;
      case "t":
        if (sel === null) return;
        ev.preventDefault();
        void this.applyUndoable(
          { update: { id: sel.id, my_day: sel.my_day === null ? this.now() : null } },
          sel
        );
        return;
      case "Delete":
        if (sel === null) return;
        ev.preventDefault();
        void this.applyUndoable({ delete: { id: sel.id } }, sel);
        return;
      case "g":
        ev.preventDefault();
        if (this.scopeAvailable()) {
          this.setScope(this.prefs.scope === "global" ? "workspace" : "global");
        }
        return;
      case "u":
        ev.preventDefault();
        void this.undo();
        return;
      case "Escape":
        ev.preventDefault();
        // One key, three de-escalations in order of how much they hide: close
        // the open row, then drop the tag filter, then drop the selection.
        if (this.selected !== null && this.expanded.has(this.selected)) {
          this.expanded.delete(this.selected);
        } else if (this.tagFilter !== null) {
          this.tagFilter = null;
        } else {
          this.selected = null;
        }
        this.render();
        return;
      default:
        return;
    }
  };

  private moveSelected(delta: -1 | 1): void {
    if (this.selected === null) return;
    const ordered = visibleItems(
      this.items(),
      { view: this.prefs.view, query: this.query, tag: this.tagFilter },
      this.now()
    );
    const target = moveTarget(ordered, this.selected, delta);
    if (target === null) return;
    if (needsRenumber(ordered)) {
      // The backend places a moved item at the MIDPOINT of its new neighbours,
      // so once the gap runs out the move is a silent no-op: the item does not
      // budge and nothing says why. `needsRenumber` is what lets the pane
      // notice — and saying so beats a drag that does nothing.
      // NOT "(#3263 S5)" any more: S5 is this slice, and it is not building
      // the renumber. A message that names the slice fixing it is a promise,
      // and a promise that ships unfulfilled is worse than no promise — the
      // rule the S4 hooks followed when they said "arrives with S5" and the
      // reason all three of those are now real controls.
      showToast("This list has run out of room between two tasks and cannot be reordered.", "info");
      return;
    }
    // A reorder is applied WITHOUT an undo entry, and `inverseOp` is where the
    // argument lives: `order_after` is a destination rather than a value, and
    // the neighbour it names may itself have moved since. Passing `null` for
    // `before` would make the stack refuse it anyway; going through `apply`
    // says so at the call site instead of relying on that.
    void this.apply({ update: { id: this.selected, order_after: target } });
  }

  /**
   * Archive every finished row on screen.
   *
   * Reads the ids from the CURRENT projection rather than from the bar the
   * human clicked: an agent's write can land between the render and the click,
   * and archiving the ids a stale button carried would move rows the human was
   * never shown. Re-deriving costs a projection and closes the whole window.
   */
  private async archiveShown(): Promise<void> {
    const ids = this.archivableIds(this.project(this.now()));
    if (ids.length === 0) {
      showToast("Nothing here to archive.", "info");
      return;
    }
    // `before` is null and that is correct rather than a gap: the archive op's
    // inverse is derived from the OP (the same ids, flag flipped), not from a
    // snapshot of any one item — see `inverseOp`'s archive arm.
    await this.applyUndoable({ archive: { ids, archived: true } }, null);
  }

  private focusField(sel: string): void {
    const field = this.el.querySelector<HTMLElement>(sel);
    field?.focus();
  }

  private toggleExpand(id: string): void {
    if (this.expanded.has(id)) {
      this.expanded.delete(id);
      // Drop the draft with the row. Keeping it would mean a note typed,
      // collapsed and forgotten comes back on the next expand looking saved.
      this.drafts.delete(id);
    } else {
      this.expanded.add(id);
      this.selected = id;
    }
    this.render();
  }

  private toggleStep(item: TodoItem, ix: number): void {
    if (ix < 0 || ix >= item.steps.length) return;
    // An update REPLACES the step list (the engine's own contract), so the
    // whole list goes back with the one flag flipped — never just the step.
    const steps = item.steps.map((s, i) => ({
      id: s.id,
      title: s.title,
      done: i === ix ? !s.done : s.done,
    }));
    void this.applyUndoable({ update: { id: item.id, steps } }, item);
  }

  private async commitDraft(item: TodoItem): Promise<void> {
    const draft = this.draftFor(item);
    if (rowDraftIsPristine(draft)) return;
    const update: {
      id: string;
      notes?: string;
      due_ms?: number | null;
      steps?: { id?: string; title: string; done: boolean }[];
    } = { id: item.id };
    if (draft.notes !== item.notes) update.notes = draft.notes;
    const due = draft.due.trim();
    if (due !== "") {
      // The row's date grammar IS the quick-add's — one vocabulary per pane.
      // A phrase the parser cannot read is REPORTED and the write is abandoned
      // whole: committing the notes while silently dropping the date the human
      // just typed is the worst of the three available outcomes.
      const parsed = parseQuickAdd(due, this.now());
      if (parsed.dueMs === null) {
        showToast(`Could not read “${due}” as a date — try “fri 4pm” or “tomorrow”.`, "info");
        return;
      }
      update.due_ms = parsed.dueMs;
    }
    const step = draft.step.trim();
    if (step !== "") {
      update.steps = [
        ...item.steps.map((s) => ({ id: s.id, title: s.title, done: s.done })),
        { title: step, done: false },
      ];
    }
    const ok = await this.applyUndoable({ update }, item);
    if (ok === null) return;
    // Cleared on SUCCESS only, and on BOTH routes — the Save button and the
    // Enter key reach this one function. Clearing on the Enter route alone is
    // the #1348 defect: it leaves the draft on the route most people use.
    // Re-seeded, not merely cleared: the committed notes ARE the new seed, so
    // the draft is pristine again against what the store now holds. Writing a
    // seed of the OLD value here would leave the row looking edited forever.
    this.drafts.set(item.id, { notes: draft.notes, step: "", due: "", seededNotes: draft.notes });
  }

  private async submitQuickAdd(): Promise<void> {
    const nowMs = this.now();
    const parse = parseQuickAdd(this.draft, nowMs);
    if (!parse.valid) {
      // An empty title is REPORTED, never thrown (the parser's own contract).
      showToast("Type something to add — the chips show what was understood.", "info");
      return;
    }
    const add: Parameters<typeof this.apply>[0] = {
      add: {
        title: parse.title,
        ...(parse.dueMs !== null ? { due_ms: parse.dueMs } : {}),
        ...(parse.tags.length > 0 ? { tags: parse.tags } : {}),
        ...(parse.priority !== 0 ? { priority: parse.priority } : {}),
        ...(parse.important ? { important: true } : {}),
        // `@myday` puts it in My Day AS OF NOW — the store keeps WHEN, so
        // `myDayIsStale` can report a carry-over later without guessing.
        ...(parse.myDay ? { my_day: nowMs } : {}),
      },
    };
    const applied = await this.applyUndoable(add, null);
    if (applied === null) return; // refused — the line stays so it can be fixed
    this.draft = "";
    this.render();
    this.focusField('[data-act="draft"]');
  }

  private setScope(next: ScopeChoice): void {
    this.prefs = { ...this.prefs, scope: next };
    this.writePrefs();
    // The scope is a different LIST, so the selection, the filters and the
    // expansions belong to a screen that is gone. Clearing them beats carrying
    // an id from the other list, which would select nothing and look broken.
    this.selected = null;
    this.tagFilter = null;
    this.expanded.clear();
    this.drafts.clear();
    // EVERY UNDO ENTRY NAMES AN ID IN THE LIST WE ARE LEAVING, and the engine
    // resolves an id with no scope check — so an undo popped after the switch
    // would write to the other store while this header says otherwise. Same
    // hazard `todoscope.ts` exists for, one gesture over.
    this.undos.clear();
    // The reminder keys go too: they are per-list, and keeping them would mean
    // switching away and back suppressed a reminder the human never saw.
    this.fired.clear();
    this.showArchived = false;
    // AND THE SNAPSHOT GOES WITH THEM (#3293 review round 3, premortem 1).
    //
    // The stale-response guard in `refreshNow` closes the race where an
    // IN-FLIGHT read paints the old list. This is the other half, and it needs
    // no race at all: the `render()` below runs synchronously, before the new
    // read has been asked for, so without this it would paint the OLD scope's
    // rows under the NEW scope's header and switch. Those rows are live — and
    // the engine resolves `update`/`complete`/`delete` by id WITHOUT a scope
    // check (`doc/design/todo-pane.md`), so completing one in that window
    // writes to whichever store actually holds it while the header says
    // otherwise. One paint deep, and entirely avoidable.
    //
    // Dropping it is right rather than merely safe: unlike a FAILED read, where
    // the list we hold is still the truth for the scope we are on and
    // publishing an empty one would destroy it, here the list we hold is
    // definitively the WRONG scope's. The cost is one frame of the empty state
    // before the coalesced read lands. `scopeChanged` is that rule, and the
    // reason it is a function in `todoscope.ts` rather than two assignments
    // here is that this is the half a DOM test cannot reach.
    this.scope = scopeChanged(this.scope, this.scopeRoot());
    this.refresher.request();
    this.render();
  }

  /**
   * Move to `next`.
   *
   * `persist` is true for every gesture the HUMAN made — the strip, the digit
   * keys — and false for a move the pane made on its own. See `revealRow`.
   */
  private setView(next: SmartView, opts: { persist?: boolean } = {}): void {
    if (this.prefs.view === next) return;
    this.prefs = { ...this.prefs, view: next };
    if (opts.persist !== false) this.writePrefs();
    // The archived toggle belongs to the Completed view and nothing else shows
    // an archived row, so it is dropped on the way out rather than left armed
    // for the next visit (the "reading position, not a preference" rule).
    if (next !== "completed") this.showArchived = false;
    this.render();
  }
}

/** The last path segment, for the scope switch's workspace label. Local rather
 *  than imported from `panesetup.ts`, which pulls the whole setup-plan module
 *  into a view that needs four characters of it. */
function tail(p: string): string {
  const parts = p.split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] ?? "";
}
