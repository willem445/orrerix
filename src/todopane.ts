// The To-Do content pane (#3263 S4) — `demo/todo-pane`'s approved mock, built
// for real. The sixth `ContentPaneKind`, hosted in a grid cell like the file
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
// See `doc/design/todo-pane.md` §"The pane" and `demo/todo-pane/DESIGN.md`.

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
  isDone,
  moveTarget,
  needsRenumber,
  visibleItems,
  type SmartView,
} from "./todomodel";
import {
  DEFAULT_TODO_PREFS,
  EMPTY_TEXT,
  TODO_PREFS_KEY,
  decodeTodoPrefs,
  encodeTodoPrefs,
  moveSelection,
  projectPane,
  pruneDrafts,
  renderedRows,
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

export class TodoPaneView {
  readonly el: HTMLElement;

  private readonly opts: TodoPaneOptions;
  private readonly now: () => number;
  private readonly refresher = new CoalescingRefresh(() => this.refreshNow());

  // ── what the backend said ───────────────────────────────────────────────
  private snapshot: TodoSnapshot | null = null;
  private loadFailed = false;

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

  private unlisten: (() => void) | null = null;
  private visible = false;
  private disposed = false;
  private caret: CaretMark | null = null;

  constructor(opts: TodoPaneOptions) {
    this.opts = opts;
    this.now = opts.now ?? (() => Date.now());
    this.prefs = this.readPrefs();

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
    this.refresher.request();
  }

  hide(): void {
    this.visible = false;
  }

  dispose(): void {
    this.disposed = true;
    this.visible = false;
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
    try {
      this.snapshot = await todoSnapshot(this.scopeRoot());
      this.loadFailed = false;
    } catch (err) {
      // A failed READ never publishes an empty list over a real one: the pane
      // keeps the snapshot it had and says the read failed. `todoSnapshot`
      // itself does not throw on a malformed payload (it decodes defensively),
      // so reaching here means the COMMAND failed — a degraded backend, not a
      // degraded row.
      this.loadFailed = true;
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
      return await todoApply(op, this.scopeRoot());
    } catch (err) {
      showToast(String(err instanceof Error ? err.message : err), "error");
      // The write was refused, so the store is byte-identical and NO event will
      // fire. Re-render off what we already have, so a control the human just
      // flipped optimistically snaps back rather than lying.
      this.render();
      return null;
    }
  }

  // ── the projection ────────────────────────────────────────────────────────

  private items(): readonly TodoItem[] {
    return this.snapshot?.items ?? [];
  }

  private project(nowMs: number): PaneProjection {
    return projectPane(
      {
        items: this.items(),
        view: this.prefs.view,
        query: this.query,
        tagFilter: this.tagFilter,
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
    for (const id of [...this.expanded]) if (!liveIds.has(id)) this.expanded.delete(id);
    if (this.selected !== null && !liveIds.has(this.selected)) this.selected = null;

    const frag = document.createDocumentFragment();
    frag.append(this.header(vm));
    frag.append(this.strip(vm));
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
    if (this.snapshot?.read_only) {
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
    if (this.loadFailed) {
      head.append(el("span", { class: "tdp-ro", text: "stale", title: "The last read failed — showing what was already loaded." }));
    }
    head.append(el("span", { class: "tdp-count tdp-num", text: String(vm.total) }));
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
          el("span", { class: "tdp-chip-n tdp-num", text: String(vm.counts[v]) })
        )
      );
    });
    return strip;
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
      // In an empty view the quick-add IS the empty state (DESIGN.md §4), so
      // this points at it rather than drawing a picture.
      list.append(
        el(
          "div",
          { class: "tdp-empty" },
          el("p", { class: "tdp-empty-text", text: EMPTY_TEXT[vm.emptyReason] }),
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

    const controls = el("div", { class: "tdp-controls" });
    const pairs: [string, string, boolean][] = [
      [
        "due",
        item.due_ms !== null ? formatDue(item.due_ms, nowMs, true) : "Add due date",
        item.due_ms !== null,
      ],
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
    if (!rowDraftIsPristine(draft, item)) {
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
      el("span", { class: "tdp-hint", text: "n add · j/k move · space done · e details · g scope" })
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
        if (item !== null) void this.apply({ complete: { id: item.id, done: !isDone(item) } });
        return;
      case "important":
        if (item !== null) void this.apply({ update: { id: item.id, important: !item.important } });
        return;
      case "expand":
        if (item !== null) this.toggleExpand(item.id);
        return;
      case "myday":
        if (item !== null) {
          void this.apply({
            update: { id: item.id, my_day: item.my_day === null ? this.now() : null },
          });
        }
        return;
      case "due":
        // S5 owns the picker; today this is the one control that has to say so
        // rather than doing nothing, which is the shape the design note's undo
        // section argues against.
        showToast("Due dates are set from the quick-add for now — try “fri 4pm” (#3263 S5).", "info");
        return;
      case "delete":
        if (item !== null) void this.apply({ delete: { id: item.id } });
        return;
      case "step":
        if (item !== null) this.toggleStep(item, Number(btn.dataset.step ?? "-1"));
        return;
      case "save":
        if (item !== null) void this.commitDraft(item);
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
      case "step-add": {
        const item = rowId !== null ? this.itemById(rowId) : null;
        if (item === null) return;
        const d = this.draftFor(item);
        if (act === "notes") d.notes = field.value;
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
      if (ev.key === "Enter" && act === "step-add") {
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
    // from `demo/todo-pane/DESIGN.md` §6, and the one place this pane does not
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
        void this.apply({ complete: { id: sel.id, done: !isDone(sel) } });
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
        void this.apply({ update: { id: sel.id, important: !sel.important } });
        return;
      case "t":
        if (sel === null) return;
        ev.preventDefault();
        void this.apply({
          update: { id: sel.id, my_day: sel.my_day === null ? this.now() : null },
        });
        return;
      case "Delete":
        if (sel === null) return;
        ev.preventDefault();
        void this.apply({ delete: { id: sel.id } });
        return;
      case "g":
        ev.preventDefault();
        if (this.scopeAvailable()) {
          this.setScope(this.prefs.scope === "global" ? "workspace" : "global");
        }
        return;
      case "u":
        // S5 owns the undo stack. Saying so beats a key that silently does
        // nothing — the same rule `inverseOp` follows when it refuses.
        ev.preventDefault();
        showToast("Undo arrives with #3263 S5.", "info");
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
      showToast("This list needs re-spacing before it can be reordered (#3263 S5).", "info");
      return;
    }
    void this.apply({ update: { id: this.selected, order_after: target } });
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
    void this.apply({ update: { id: item.id, steps } });
  }

  private async commitDraft(item: TodoItem): Promise<void> {
    const draft = this.draftFor(item);
    if (rowDraftIsPristine(draft, item)) return;
    const update: { id: string; notes?: string; steps?: { id?: string; title: string; done: boolean }[] } =
      { id: item.id };
    if (draft.notes !== item.notes) update.notes = draft.notes;
    const step = draft.step.trim();
    if (step !== "") {
      update.steps = [
        ...item.steps.map((s) => ({ id: s.id, title: s.title, done: s.done })),
        { title: step, done: false },
      ];
    }
    const ok = await this.apply({ update });
    if (ok === null) return;
    // Cleared on SUCCESS only, and on BOTH routes — the Save button and the
    // Enter key reach this one function. Clearing on the Enter route alone is
    // the #1348 defect: it leaves the draft on the route most people use.
    this.drafts.set(item.id, { notes: draft.notes, step: "" });
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
    const applied = await this.apply(add);
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
    this.refresher.request();
    this.render();
  }

  private setView(next: SmartView): void {
    if (this.prefs.view === next) return;
    this.prefs = { ...this.prefs, view: next };
    this.writePrefs();
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
