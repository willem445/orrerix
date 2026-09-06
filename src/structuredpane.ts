// The structured pane's DOM renderer (#2891 S4) — the first-in-class
// replacement for a scraped TUI.
//
// WHAT THIS IS. `harness-adapters.md` §5.1 is "two projections, one log": the
// engine's `transcript::Renderer` turns each `HarnessEvent` into VT bytes for
// the pane's `OutputBuf` ring (which is what keeps `get_output`, termgrid
// replay, thumbnails, `last_exit_tail` and replay-on-attach working), and THIS
// is the other projection — the human's surface, in the pane's own grid cell,
// fed the events themselves over `orch-pane-event`. Both are derived from the
// same log, which is what makes them agree by construction rather than by
// discipline.
//
// WHAT THIS IS NOT: a second reducer. `structuredview.ts` (S2) owns the
// event→block projection and this file never interprets an event. It is a VIEW
// of `project()`'s output, and every decision it makes that is not element
// creation lives in `structuredrows.ts` where a `node --test` can reach it.
//
// ── CONSTRAINT 1 ───────────────────────────────────────────────────────────
//
// A structured pane has NO PTY. Nothing here measures a terminal, calls `fit`,
// or reaches `resize_pty` — there is nothing to resize. It is a content view in
// the pane's content box (`Pane.startContent`), exactly as the git and editor
// views are, and it lays itself out in the box it is given.
//
// ── VIRTUALISATION, and the constraint the mock's storm fixture established ─
//
// A pane holds up to `MAX_BLOCKS` (2000) blocks and must not cost 2000 nodes.
// Only the rows inside the viewport plus one overscan exist in the DOM; two
// spacer heights stand in for the rest, so the scrollbar is scaled to the whole
// transcript while the node count stays O(viewport).
//
// `DESIGN.md` §7 records the defect that reading the code did NOT find: the
// mock followed the live end by reading `scrollHeight` on every appended row,
// which forces a synchronous layout per row and is O(n^2) in a burst — it froze
// the tab on 461 tool calls. The fix, and the constraint S4 inherits, is that
// **a producer may not make the consumer pay per event**. So:
//
//   - `apply()` folds a batch into the projection and sets a dirty flag. It
//     touches no element and reads no geometry.
//   - ONE `requestAnimationFrame` per dirty frame does all the DOM work.
//   - Inside that frame, every WRITE happens before every READ: rows are built
//     and placed, and only then are their heights measured in one pass. A
//     read interleaved with the writes is the same forced-layout-per-row shape
//     wearing a different call.
//
// That rAF is also this stream's INV-3 bound — see `test/perfpolicy.test.ts`'s
// `orch-pane-event` row, which cites this file.
//
// ── THE HUMAN'S STATE LIVES IN THE VIEW ────────────────────────────────────
//
// A fold, the thinking switch and an unsent dialog answer are held in
// `ViewState` and in this object, never on a DOM element: the renderer rebuilds
// its elements from the model whenever a batch arrives, so state kept on one
// would be destroyed by the next event to land. That is CLAUDE.md's in-list
// editor rule, and a transcript re-renders far more often than a board does.

import {
  MAX_TEXT_BYTES_PER_BLOCK,
  emptyState,
  emptyViewState,
  project,
  textTail,
  toggleCollapsed,
  pruneViewState,
} from "./structuredview.ts";
import type {
  Block,
  DeliveryBlock,
  NoticeBlock,
  ProjectionInput,
  RequestBlock,
  State,
  TextBlock,
  ThinkingBlock,
  ToolBlock,
  TurnBlock,
  ViewState,
} from "./structuredview.ts";
import {
  OVERSCAN_PX,
  actionsFor,
  argLine,
  chipFor,
  computeWindow,
  diffOf,
  droppedNotice,
  evictionNotice,
  fmtMs,
  headerFacts,
  isEditTool,
  isShellTool,
  payloadIsProse,
  pinnedAtBottom,
  requestPayload,
  requestTitle,
  rowSignature,
  rowsFor,
  settlementLine,
  tickerFields,
  toolMark,
  turnReceipt,
  wantsFreeText,
} from "./structuredrows.ts";
import type { RowSpec } from "./structuredrows.ts";
import { mark } from "./structuredicons.ts";
import type { MarkName } from "./structuredicons.ts";

/** How a dialog or permission answer leaves this pane.
 *
 *  Injected rather than imported, for two reasons. It keeps this module free of
 *  `orchestration.ts` (which imports `pane.ts`, which would host this — a
 *  cycle), and it makes the trust boundary visible at the one call site: §3.5's
 *  rule is that **every agent may be asked and no agent may ever answer**, and
 *  the caller's identity is a property of the entry point rather than an
 *  argument. A button in the human's own window IS the trusted path; this
 *  callback is where it leaves. */
export type AnswerFn = (req: {
  requestId: string;
  channel: "permission" | "ui";
  answer: string;
}) => Promise<void>;

export interface StructuredPaneOptions {
  groupId: string;
  agentId: string;
  /** Which agent program runs in this pane, for the header chip. Read off the
   *  source — never derived by branching on one CLI's name. `null` when the
   *  pane does not know yet, which renders as no chip rather than as a guess. */
  cli?: string | null;
  answer: AnswerFn;
  /** Injected for tests and for a replay harness; defaults to the real clock. */
  now?: () => number;
}

interface RowNode {
  el: HTMLElement;
  /** The signature the node was last built from. */
  sig: string;
  /** Last measured height, or the estimate until it has been on screen. */
  height: number;
  measured: boolean;
}

export class StructuredPaneView {
  readonly el: HTMLElement;

  private readonly opts: StructuredPaneOptions;
  private readonly now: () => number;

  /** The projection. Rebuilt only by `project()`; never written here. */
  private state: State = emptyState();
  /** The half the human owns, which no batch may clobber. */
  private view: ViewState = emptyViewState();

  private readonly scrollEl: HTMLElement;
  private readonly listEl: HTMLElement;
  private readonly padTopEl: HTMLElement;
  private readonly padBottomEl: HTMLElement;
  private readonly headEl: HTMLElement;
  private readonly tickerEl: HTMLElement;
  private readonly queueEl: HTMLElement;

  /** Live row nodes, keyed by block id. Bounded by the window, not by the
   *  transcript: rows leaving the window are removed and dropped from here. */
  private nodes = new Map<string, RowNode>();
  /** Heights the view has learned, kept for rows NOT currently in the DOM so a
   *  row scrolled away and back does not re-estimate and shift the scrollbar.
   *  Pruned beside the folds, so it cannot grow past the projection. */
  private heights = new Map<string, number>();

  /** Unsent dialog input, held here rather than on the `<input>` — the renderer
   *  rebuilds its elements on every batch, and a draft kept on one would be
   *  destroyed by an event arriving while the human typed. */
  private drafts = new Map<string, string>();

  private frame: number | null = null;
  private pinned = true;
  private disposed = false;
  private visible = false;

  constructor(opts: StructuredPaneOptions) {
    this.opts = opts;
    this.now = opts.now ?? (() => Date.now());

    this.el = document.createElement("div");
    this.el.className = "spane";
    this.el.dataset.state = "idle";
    this.el.innerHTML =
      `<div class="spane-head">` +
        `<span class="spane-dot"></span>` +
        `<span class="spane-cli" data-cli=""></span>` +
        `<span class="spane-meta">` +
          `<span class="spane-k">model</span><span class="spane-v" data-f="model">—</span>` +
          `<span class="spane-sep">·</span>` +
          `<span class="spane-k">session</span>` +
          `<span class="spane-v spane-num" data-f="session">—</span>` +
        `</span>` +
        `<span class="spane-grow"></span>` +
      `</div>` +
      `<div class="spane-scroll" tabindex="0">` +
        `<div class="spane-transcript">` +
          `<div class="spane-pad" data-pad="top"></div>` +
          `<div class="spane-pad" data-pad="bottom"></div>` +
        `</div>` +
      `</div>` +
      `<div class="spane-foot">` +
        `<div class="spane-queue" hidden><span></span></div>` +
        `<div class="spane-ticker">` +
          `<span class="spane-tk"><span class="spane-tk-k">in</span>` +
            `<span class="spane-tk-v spane-num" data-f="tin">—</span></span>` +
          `<span class="spane-tk"><span class="spane-tk-k">out</span>` +
            `<span class="spane-tk-v spane-num" data-f="tout">—</span></span>` +
          `<span class="spane-tk"><span class="spane-tk-k">cache</span>` +
            `<span class="spane-tk-v spane-num" data-f="cache">—</span></span>` +
          `<span class="spane-grow"></span>` +
          `<span class="spane-tk"><span class="spane-tk-k">cost</span>` +
            `<span class="spane-tk-v spane-num spane-money" data-f="cost">—</span></span>` +
        `</div>` +
      `</div>`;

    this.headEl = this.el.querySelector(".spane-head")!;
    this.scrollEl = this.el.querySelector(".spane-scroll")!;
    this.listEl = this.el.querySelector(".spane-transcript")!;
    this.padTopEl = this.el.querySelector('[data-pad="top"]')!;
    this.padBottomEl = this.el.querySelector('[data-pad="bottom"]')!;
    this.tickerEl = this.el.querySelector(".spane-ticker")!;
    this.queueEl = this.el.querySelector(".spane-queue")!;

    if (opts.cli) this.headEl.querySelector<HTMLElement>(".spane-cli")!.dataset.cli = opts.cli;
    const cliEl = this.headEl.querySelector<HTMLElement>(".spane-cli")!;
    cliEl.textContent = opts.cli ?? "";
    cliEl.hidden = !opts.cli;

    // Scrolling changes only whether we follow the live end, and it must not
    // itself schedule work per wheel tick: the rAF below coalesces it.
    this.scrollEl.addEventListener("scroll", this.onScroll, { passive: true });
    this.scrollEl.addEventListener("click", this.onClick);
  }

  /** Attach-then-show, the contract every content view here follows: nothing
   *  measures a zero-width box before it is in the document. */
  show(): void {
    this.visible = true;
    this.schedule();
  }

  hide(): void {
    this.visible = false;
  }

  /**
   * Fold one `orch-pane-event` batch into the projection.
   *
   * Touches no element and reads no geometry — that is the whole point. The
   * work is deferred to one rAF per frame however many batches land in it,
   * which is P5 (the `ft-files` precedent) and this stream's declared INV-3
   * bound.
   */
  apply(events: readonly ProjectionInput[]): void {
    if (this.disposed || events.length === 0) return;
    project(this.state, events, { nowMs: this.now() });
    pruneViewState(this.view, this.state);
    this.schedule();
  }

  /** The pane's text for a thumbnail / a tail read. Delegated to the
   *  projection: a second summariser here would be a second projection of the
   *  same log, which is the exact drift §5.1 exists to close. */
  tail(bytes: number): string {
    return textTail(this.state, bytes, this.view);
  }

  /** Fold every thinking block, or unfold them. The human's switch, held in
   *  `ViewState` where a batch cannot clobber it. */
  setDimThinking(on: boolean): void {
    this.view.dimThinking = on;
    this.schedule();
  }

  dispose(): void {
    this.disposed = true;
    if (this.frame !== null) cancelAnimationFrame(this.frame);
    this.frame = null;
    this.scrollEl.removeEventListener("scroll", this.onScroll);
    this.scrollEl.removeEventListener("click", this.onClick);
    this.nodes.clear();
    this.heights.clear();
    this.drafts.clear();
  }

  // ── the frame ─────────────────────────────────────────────────────────────

  private onScroll = (): void => {
    this.pinned = pinnedAtBottom(
      this.scrollEl.scrollTop,
      this.scrollEl.scrollHeight,
      this.scrollEl.clientHeight,
    );
    this.schedule();
  };

  /** Ask for one frame.
   *
   *  A pane in a hidden tab schedules NOTHING: its `clientHeight` is zero, so
   *  every row it built would measure zero and it would learn a height it then
   *  has to unlearn — and the saving is real render passes, not a timer. The
   *  projection still advances (`apply` is what folds the batch), so `show()`
   *  draws the transcript that arrived while nobody was looking. */
  private schedule(): void {
    if (this.disposed || !this.visible || this.frame !== null) return;
    this.frame = requestAnimationFrame(() => {
      this.frame = null;
      this.render();
    });
  }

  /**
   * One frame: decide the window, reconcile the nodes in it, then measure.
   *
   * EVERY WRITE BEFORE EVERY READ. The measurement pass at the bottom is the
   * only place geometry is read, and it runs after every element has been
   * placed — so one forced layout per frame over ~20 nodes, never one per row.
   * That ordering is the §7 constraint, and inverting it is invisible until a
   * burst arrives.
   */
  private render(): void {
    if (this.disposed) return;
    const rows = rowsFor(this.state, this.view);
    const heights = rows.map((r) => this.heights.get(r.key) ?? r.estimate);

    const viewport = this.scrollEl.clientHeight || 0;
    // Following the live end means "the window is the bottom of the list",
    // which is decided from the model rather than by scrolling first and
    // reading where we landed.
    let total = 0;
    for (const h of heights) total += h;
    const scrollTop = this.pinned ? Math.max(0, total - viewport) : this.scrollEl.scrollTop;
    const win = computeWindow(heights, scrollTop, viewport, OVERSCAN_PX);

    // --- writes -------------------------------------------------------------
    const wanted = new Set<string>();
    let cursor: Node = this.padTopEl;
    for (let i = win.start; i < win.end; i += 1) {
      const row = rows[i]!;
      const block = this.state.blocks[i]!;
      wanted.add(row.key);
      const sig = rowSignature(block, row.collapsed);
      let node = this.nodes.get(row.key);
      if (!node) {
        node = { el: this.buildRow(block, row), sig, height: row.estimate, measured: false };
        this.nodes.set(row.key, node);
      } else if (node.sig !== sig) {
        const fresh = this.buildRow(block, row);
        node.el.replaceWith(fresh);
        node.el = fresh;
        node.sig = sig;
        node.measured = false;
      }
      if (cursor.nextSibling !== node.el) {
        this.listEl.insertBefore(node.el, cursor.nextSibling);
      }
      cursor = node.el;
    }
    for (const [key, node] of this.nodes) {
      if (wanted.has(key)) continue;
      node.el.remove();
      this.nodes.delete(key);
    }
    if (this.padBottomEl.previousSibling !== cursor) {
      this.listEl.insertBefore(this.padBottomEl, cursor.nextSibling);
    }
    this.padTopEl.style.height = `${win.padTop}px`;
    this.padBottomEl.style.height = `${win.padBottom}px`;

    this.paintHeader();
    if (this.pinned && viewport > 0) this.scrollEl.scrollTop = this.scrollEl.scrollHeight;

    // --- reads --------------------------------------------------------------
    // One pass, after every write. A row measured at zero height is a row in a
    // hidden tab; keep its estimate rather than learning a zero that would then
    // collapse the scrollbar for the rest of the session.
    let learned = false;
    for (let i = win.start; i < win.end; i += 1) {
      const key = rows[i]!.key;
      const node = this.nodes.get(key);
      if (!node) continue;
      const h = node.el.offsetHeight;
      if (h <= 0) continue;
      if (!node.measured || Math.abs(h - node.height) > 0.5) learned = true;
      node.height = h;
      node.measured = true;
      this.heights.set(key, h);
    }
    // A height the view did not have changes where every row below it sits, so
    // the window has to be recomputed once. Bounded to ONE extra frame: the
    // second pass measures the same rows and learns nothing, so it does not
    // schedule a third.
    if (learned) this.schedule();

    this.prune(rows);
  }

  /** Keep the learned-height map from outliving the projection. Same idiom as
   *  the fold set's pruning, and for the same reason: a map keyed by block id
   *  grows for the life of the window otherwise. */
  private prune(rows: readonly RowSpec[]): void {
    if (this.heights.size <= rows.length) return;
    const live = new Set(rows.map((r) => r.key));
    for (const key of Array.from(this.heights.keys())) if (!live.has(key)) this.heights.delete(key);
    for (const key of Array.from(this.drafts.keys())) if (!live.has(key)) this.drafts.delete(key);
  }

  private paintHeader(): void {
    const facts = headerFacts(this.state);
    this.setField(this.headEl, "model", facts.model);
    this.setField(this.headEl, "session", facts.session);

    const t = tickerFields(this.state);
    this.bump("tin", t.input);
    this.bump("tout", t.output);
    this.bump("cache", t.cache);
    this.bump("cost", t.cost);

    const queued = this.state.steering.length + this.state.followUp.length;
    this.queueEl.hidden = queued === 0; // an empty queue is not news
    if (queued > 0) {
      const bits = [`${queued} queued`];
      if (this.state.steering.length) bits.push(`${this.state.steering.length} steering`);
      this.queueEl.querySelector("span")!.textContent = bits.join(" · ");
    }

    this.el.dataset.state = this.paneState();
  }

  /** The pane-edge warp: what the agent is doing NOW, in the six-dye
   *  vocabulary. Read off the projection rather than tracked alongside it, so
   *  it cannot drift from what the transcript shows. */
  private paneState(): string {
    if (this.state.blocks.some((b) => b.kind === "request" && b.settled === null)) return "attention";
    if (this.state.exited) return this.state.exitCode === 0 ? "idle" : "danger";
    if (this.state.currentTurn !== null) return "working";
    return "idle";
  }

  private setField(root: HTMLElement, field: string, text: string): void {
    const el = root.querySelector<HTMLElement>(`[data-f="${field}"]`);
    if (el && el.textContent !== text) el.textContent = text;
  }

  /** A figure that just moved lifts once — a one-shot, never a loop (§5's
   *  motion budget). Restarting it needs the class off, a reflow, and the class
   *  back on; that `offsetWidth` read is the documented way to restart a CSS
   *  animation and is the one geometry read outside the measurement pass. */
  private bump(field: string, text: string): void {
    const el = this.tickerEl.querySelector<HTMLElement>(`[data-f="${field}"]`);
    if (!el || el.textContent === text) return;
    el.textContent = text;
    el.classList.remove("bumped");
    void el.offsetWidth;
    el.classList.add("bumped");
  }

  // ── rows ──────────────────────────────────────────────────────────────────

  private buildRow(b: Block, row: RowSpec): HTMLElement {
    const el = document.createElement("div");
    el.className = "spane-row";
    el.dataset.seg = row.segment;
    el.dataset.key = row.key;
    if (row.live) el.dataset.live = "1";
    switch (b.kind) {
      case "text":
        el.appendChild(this.buildText(b, row));
        break;
      case "thinking":
        el.appendChild(this.buildThinking(b, row));
        break;
      case "tool":
        el.appendChild(this.buildTool(b, row));
        break;
      case "request":
        el.appendChild(this.buildRequest(b));
        break;
      case "delivery":
        el.appendChild(this.buildDelivery(b));
        break;
      case "turn":
        el.appendChild(this.buildTurn(b));
        break;
      case "notice":
        el.appendChild(this.buildNotice(b));
        break;
      case "evicted":
        el.appendChild(this.buildNoticeLine("info", "[ring]", evictionNotice(b.blocks)));
        break;
    }
    return el;
  }

  /** The model's answer is the content: bare prose, no container. */
  private buildText(b: TextBlock, row: RowSpec): HTMLElement {
    const el = document.createElement("div");
    el.className = "spane-say";
    if (b.droppedBytes > 0) el.appendChild(this.elided(b.droppedBytes));
    el.appendChild(document.createTextNode(b.text));
    if (row.live) {
      const caret = document.createElement("span");
      caret.className = "spane-caret";
      el.appendChild(caret);
    }
    return el;
  }

  private buildThinking(b: ThinkingBlock, row: RowSpec): HTMLElement {
    const box = document.createElement("div");
    box.className = "spane-think";
    box.dataset.open = row.collapsed ? "0" : "1";
    box.innerHTML =
      `<button class="spane-think-head" type="button" data-toggle="${b.id}" ` +
        `aria-expanded="${row.collapsed ? "false" : "true"}">` +
        `${mark("chevron", "spane-chev")}${mark("think", "")}` +
        `<span class="spane-think-word">thinking</span>` +
      `</button>` +
      `<div class="spane-fold"><div><div class="spane-think-body"></div></div></div>`;
    const body = box.querySelector<HTMLElement>(".spane-think-body")!;
    if (b.droppedBytes > 0) body.appendChild(this.elided(b.droppedBytes));
    body.appendChild(document.createTextNode(b.text));
    return box;
  }

  private buildTool(b: ToolBlock, row: RowSpec): HTMLElement {
    const shell = isShellTool(b.name);
    // `row.collapsed` already carries the default-plus-flip decision
    // (`isOpen`), so the card never re-derives it and cannot disagree with the
    // height the virtualiser estimated for it.
    const open = !row.collapsed;
    const el = document.createElement("div");
    el.className = shell ? "spane-cmd" : "spane-tool";
    el.dataset.status = b.status;
    el.dataset.open = open ? "1" : "0";
    const { mark: markName, family } = toolMark(b.name);
    if (family) el.dataset.family = family;
    const chip = chipFor(b);
    const dur = fmtMs(b.durationMs);

    const head = document.createElement("button");
    head.type = "button";
    head.className = shell ? "spane-cmd-head" : "spane-tool-head";
    head.dataset.toggle = b.id;
    head.setAttribute("aria-expanded", open ? "true" : "false");
    if (shell) {
      head.innerHTML =
        `<span class="spane-sigil">$</span><span class="spane-cmd-line"></span>` +
        `<span class="spane-cmd-meta"><span class="spane-dur spane-num"></span>` +
        `<span class="spane-chip" data-status="${chip.status}">` +
          `${chip.spinning ? `<span class="spane-spin"></span>` : `<span class="spane-dotmark"></span>`}` +
          `${escapeHtml(chip.label)}</span>${mark("chevron", "spane-chev")}</span>`;
      head.querySelector(".spane-cmd-line")!.textContent = argLine(b.name, b.input);
    } else {
      head.innerHTML =
        `${mark(markName as MarkName, "spane-tool-ic")}` +
        `<span class="spane-tool-name"></span>` +
        `<span class="spane-tool-arg"><span></span></span>` +
        `<span class="spane-dur spane-num"></span>` +
        `<span class="spane-chip" data-status="${chip.status}">` +
          `${chip.spinning ? `<span class="spane-spin"></span>` : `<span class="spane-dotmark"></span>`}` +
          `${escapeHtml(chip.label)}</span>${mark("chevron", "spane-chev")}`;
      // `orphan` is rule 1 made visible: a card built by a result whose call was
      // never seen has no name, and it renders as an em dash rather than as the
      // string "unknown".
      head.querySelector(".spane-tool-name")!.textContent = b.name ?? "—";
      head.querySelector(".spane-tool-arg > span")!.textContent = argLine(b.name, b.input);
    }
    head.querySelector(".spane-dur")!.textContent = dur;
    el.appendChild(head);

    const fold = document.createElement("div");
    fold.className = "spane-fold";
    const inner = document.createElement("div");
    const body = document.createElement("div");
    body.className = "spane-tool-body";

    // An Edit's real payload is a diff, not a JSON blob — the difference
    // between "a tool ran" and "here is what changed".
    const diff = isEditTool(b.name) && b.status === "ok" ? diffOf(b.input) : null;
    if (diff) body.appendChild(this.buildDiff(diff));
    else if (!shell) body.appendChild(this.buildKv(b.input));

    if (b.outputDroppedBytes > 0) body.appendChild(this.elided(b.outputDroppedBytes));
    if (b.output) {
      const pre = document.createElement("pre");
      pre.className = "spane-out";
      if (b.isError) pre.dataset.error = "1";
      pre.textContent = b.output;
      body.appendChild(pre);
    }
    inner.appendChild(body);
    fold.appendChild(inner);
    el.appendChild(fold);
    return el;
  }

  private buildKv(input: unknown): HTMLElement {
    const dl = document.createElement("dl");
    dl.className = "spane-kv";
    const entries = input && typeof input === "object" ? Object.entries(input as object) : [];
    for (const [k, v] of entries) {
      const dt = document.createElement("dt");
      dt.textContent = k;
      const dd = document.createElement("dd");
      dd.textContent = typeof v === "string" ? v : JSON.stringify(v);
      dl.append(dt, dd);
    }
    if (!entries.length) {
      const dt = document.createElement("dt");
      dt.textContent = "(no arguments)";
      dl.append(dt, document.createElement("dd"));
    }
    return dl;
  }

  private buildDiff(diff: NonNullable<ReturnType<typeof diffOf>>): HTMLElement {
    const wrap = document.createElement("div");
    wrap.className = "spane-diff";
    const hunk = document.createElement("div");
    hunk.className = "spane-diff-hunk";
    hunk.textContent = `@@ ${diff.file} @@`;
    wrap.appendChild(hunk);
    for (const l of diff.lines) {
      const d = document.createElement("div");
      d.className = "spane-diff-line";
      d.dataset.k = l.k;
      const ln = document.createElement("span");
      ln.className = "spane-ln";
      ln.textContent = l.n === null ? "" : String(l.n);
      const sg = document.createElement("span");
      sg.className = "spane-sg";
      sg.textContent = l.sign;
      const tx = document.createElement("span");
      tx.className = "spane-tx";
      tx.textContent = l.text;
      d.append(ln, sg, tx);
      wrap.appendChild(d);
    }
    return wrap;
  }

  /**
   * The inline request card.
   *
   * INLINE, never a modal: §3.3 refuses a dialog outright, because a modal
   * strands every other agent reporting to this window (#946). The card sits in
   * the transcript where it happened and the rest of the app keeps working.
   */
  private buildRequest(b: RequestBlock): HTMLElement {
    const el = document.createElement("div");
    el.className = "spane-ask";
    el.dataset.open = b.settled ? "settled" : "pending";
    el.dataset.channel = b.channel;

    const head = document.createElement("div");
    head.className = "spane-ask-head";
    head.innerHTML =
      `${mark("shield", "")}<span>needs you</span><span class="spane-grow"></span>` +
      `<span class="spane-ask-method"></span>`;
    head.querySelector(".spane-ask-method")!.textContent = b.method ?? b.channel;
    el.appendChild(head);

    const body = document.createElement("div");
    body.className = "spane-ask-body";
    const title = document.createElement("p");
    title.className = "spane-ask-title";
    title.textContent = requestTitle(b);
    body.appendChild(title);

    const payload = requestPayload(b);
    if (payload) {
      const what = document.createElement("div");
      what.className = "spane-ask-what";
      // §4's edge: a permission's payload is a command line the machine handed
      // us and takes mono; a dialog's message is prose and takes the UI face.
      if (payloadIsProse(b)) what.dataset.prose = "1";
      what.textContent = payload;
      body.appendChild(what);
    }

    const acts = document.createElement("div");
    acts.className = "spane-ask-acts";
    if (b.settled) {
      const chip = document.createElement("span");
      chip.className = "spane-chip";
      chip.dataset.status = el.dataset.seg === "danger" ? "error" : "ok";
      chip.innerHTML = `<span class="spane-dotmark"></span>`;
      chip.append(document.createTextNode(settlementLine(b) ?? ""));
      acts.appendChild(chip);
    } else {
      if (wantsFreeText(b)) {
        const input = document.createElement("input");
        input.type = "text";
        input.className = "spane-ask-in";
        input.placeholder = "your answer";
        input.value = this.drafts.get(b.id) ?? "";
        input.dataset.draft = b.id;
        acts.appendChild(input);
        const send = document.createElement("button");
        send.type = "button";
        send.className = "spane-btn spane-btn-primary";
        send.dataset.answer = b.requestId;
        send.dataset.channel = b.channel;
        send.dataset.free = b.id;
        send.textContent = "Send";
        acts.appendChild(send);
      }
      for (const a of actionsFor(b)) {
        const btn = document.createElement("button");
        btn.type = "button";
        btn.className =
          "spane-btn" + (a.primary ? " spane-btn-primary" : a.danger ? " spane-btn-danger" : "");
        btn.dataset.answer = b.requestId;
        btn.dataset.channel = b.channel;
        btn.dataset.value = a.value;
        btn.textContent = a.label;
        acts.appendChild(btn);
      }
      const hint = document.createElement("span");
      hint.className = "spane-ask-hint";
      // `timeout_ms` is DESCRIPTIVE (§1.2): it reports the harness's own
      // deadline. orrerix never runs a timer of its own against a request — a
      // timed auto-answer is a decision nobody made, recorded as though
      // somebody had.
      hint.textContent =
        b.timeoutMs !== null
          ? `the harness gives up in ${Math.round(b.timeoutMs / 1000)}s`
          : "waits until answered";
      acts.appendChild(hint);
    }
    body.appendChild(acts);
    el.appendChild(body);
    return el;
  }

  /** A delivery is a SEAM: the one thing in the stream the agent did not
   *  produce. Marked by form — a full-width block with an inbound glyph — never
   *  by a hue of its own (§3). */
  private buildDelivery(b: DeliveryBlock): HTMLElement {
    const el = document.createElement("div");
    el.className = "spane-deliv";
    el.dataset.from = b.via === "human" ? "human" : "agent";
    el.innerHTML =
      `<div class="spane-deliv-head">${mark(b.via === "human" ? "person" : "inbound", "")}` +
        `<span class="spane-deliv-from"></span><span class="spane-deliv-via"></span>` +
        `<span class="spane-grow"></span><span class="spane-deliv-ts spane-num"></span></div>` +
      `<div class="spane-deliv-body"></div>`;
    el.querySelector(".spane-deliv-from")!.textContent = b.from ?? "—";
    el.querySelector(".spane-deliv-via")!.textContent = `· ${b.via}`;
    el.querySelector(".spane-deliv-ts")!.textContent = b.ts ?? "";
    el.querySelector(".spane-deliv-body")!.textContent = b.text;
    return el;
  }

  private buildTurn(b: TurnBlock): HTMLElement {
    const el = document.createElement("div");
    el.className = "spane-turnrule";
    const stop = b.stop === null ? null : typeof b.stop === "string" ? b.stop : b.stop.other;
    el.dataset.stop = stop === "error" ? "error" : "ok";
    const span = document.createElement("span");
    span.textContent = turnReceipt(b);
    el.appendChild(span);
    return el;
  }

  private buildNotice(b: NoticeBlock): HTMLElement {
    return this.buildNoticeLine(b.level, `[${b.tag}]`, b.text);
  }

  private buildNoticeLine(level: string, tag: string, text: string): HTMLElement {
    const el = document.createElement("div");
    el.className = "spane-note";
    el.dataset.kind = level;
    const t = document.createElement("span");
    t.className = "spane-tag";
    t.textContent = tag;
    const s = document.createElement("span");
    s.textContent = text;
    el.append(t, s);
    return el;
  }

  /** The visible artifact of a per-block ceiling. Rendered, never merely
   *  logged: an elision the reader cannot see is a transcript that lies. */
  private elided(bytes: number): HTMLElement {
    const el = document.createElement("div");
    el.className = "spane-elided";
    el.textContent = droppedNotice(bytes);
    el.title = `this block keeps its last ${MAX_TEXT_BYTES_PER_BLOCK} bytes; the whole of it is in the event log`;
    return el;
  }

  // ── input ─────────────────────────────────────────────────────────────────

  /** One delegated listener for the whole transcript rather than a handler per
   *  row: rows are created and destroyed constantly by the virtualiser, and a
   *  per-row listener would be work per row on every frame. */
  private onClick = (ev: MouseEvent): void => {
    const target = ev.target as HTMLElement | null;
    if (!target) return;

    const toggle = target.closest<HTMLElement>("[data-toggle]");
    if (toggle) {
      toggleCollapsed(this.view, toggle.dataset.toggle!);
      this.schedule();
      return;
    }

    const btn = target.closest<HTMLElement>("[data-answer]");
    if (!btn) return;
    const channel = btn.dataset.channel === "permission" ? "permission" : "ui";
    let value = btn.dataset.value ?? "";
    if (btn.dataset.free) {
      const input = this.el.querySelector<HTMLInputElement>(
        `[data-draft="${btn.dataset.free}"]`,
      );
      value = input?.value ?? this.drafts.get(btn.dataset.free) ?? "";
      this.drafts.delete(btn.dataset.free);
    }
    // The answer leaves through the injected trusted path. The card is NOT
    // marked settled here: the settlement is a fact the ENGINE reports back as
    // a `UiSettled`/`PermissionSettled` event, and drawing it optimistically
    // would put a decision on screen that the harness may not have taken.
    void this.opts.answer({ requestId: btn.dataset.answer!, channel, answer: value });
  };
}

function escapeHtml(s: string): string {
  return s.replace(
    /[&<>"']/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!,
  );
}
