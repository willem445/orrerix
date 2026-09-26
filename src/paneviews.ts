// The nine secondary views a pane can show, split out of pane.ts (#3498 F1): git,
// issues, task board, needs-you, audit, timeline, token charts, group and the file
// editor — each one's lazy construction, its registration into the embed
// registry, its toggle, and its header button. Where each view SHOWS (overlay or
// slot) is PaneEmbeds' job (paneembeds.ts); this file is split from it only so
// neither exceeds the 1,500-line TS class ceiling (test/filebudget.test.ts).
//
// A satellite of `Pane`: DOM glue, hand-validated like pane.ts itself. Design
// note: docs/design/embedded-panels.md; layout conventions: docs/design/module-layout.md.

import { invoke } from "./transport.ts";
import { GitView } from "./gitview";
import { IssuesView } from "./issuesview";
import { TasksView } from "./tasksview";
import { DecisionsView } from "./decisionsview";
import { AuditView } from "./auditview";
import { AuditStore } from "./auditstore";
import type { AuditEntry } from "./auditsummary";
import { TimelineView } from "./timelineview";
import { TokenChartsView } from "./tokenchartsview";
import { GroupView } from "./groupview";
import { EMBED_MIN_PANEL_PX } from "./embedsplit";
import { FileEditView } from "./fileedit";
import { icon } from "./icons.ts";
import { EMBED_TOGGLE_TITLE } from "./paneembeds";
import type { Pane } from "./pane";

export const ICON_BTN_PX = 13;
const TASKS_ICON = icon("list-checks", ICON_BTN_PX);
// NEEDS-YOU panel (#1091, Alt+Q): a raised hand — the group asking for the
// human, rather than offering them help.
const DECISIONS_ICON = icon("hand", ICON_BTN_PX);
const GIT_ICON = icon("git-graph", ICON_BTN_PX);
// Issues view (Alt+I): a dot inside a circle — GitHub's open-issue glyph.
const ISSUES_ICON = icon("circle-dot", ICON_BTN_PX);
// Progress timeline (#608): the audit log's chart sibling.
const TIMELINE_ICON = icon("chart-gantt", ICON_BTN_PX);
// Token charts (#2011): spend over time and per feature — the audit log's
// cost sibling, so a chart glyph distinct from the timeline's gantt.
const TOKENS_ICON = icon("chart-column", ICON_BTN_PX);
// Audit viewer: a clock/history glyph for the group's audit-log timeline.
const AUDIT_ICON = icon("clock-fading", ICON_BTN_PX);
const GROUP_ICON = icon("users", ICON_BTN_PX);
// File-editor overlay (#174): a page with a pencil, to read as "edit files"
// distinct from the external-editor </> glyph above.
const FILES_ICON = icon("file-pen", ICON_BTN_PX);

export class PaneViews {
  /** Lazily created git view; null until the first toggle. */
  gitView: GitView | null = null;
  /** Floating container for the git view + divider. It overlays the top of
   *  the terminal instead of shrinking it: resizing the PTY makes ConPTY and
   *  full-screen TUIs repaint from scratch, flooding scrollback with
   *  duplicate frames. */
  gitOverlay: HTMLElement | null = null;
  gitBtn!: HTMLButtonElement;
  /** GitHub issues view (any pane in a git repo), same overlay mechanics. */
  issuesView: IssuesView | null = null;
  issuesOverlay: HTMLElement | null = null;
  issuesBtn!: HTMLButtonElement;
  /** Task board (orchestrator panes only), same overlay mechanics. */
  tasksView: TasksView | null = null;
  tasksOverlay: HTMLElement | null = null;
  tasksBtn: HTMLButtonElement;
  /** NEEDS-YOU panel (orchestrator panes only, #1091), same overlay
   *  mechanics — pending human decisions and demo-gated board rows. */
  decisionsView: DecisionsView | null = null;
  decisionsOverlay: HTMLElement | null = null;
  decisionsBtn: HTMLButtonElement;
  /** This pane's ONE `orch_audit` read (#1317), shared by the audit viewer
   *  and the progress timeline — two views on one file that each used to
   *  fetch it, parse it and hold their own 5000-row copy of it, at the same
   *  1.5 s follow cadence. Created with the first view that needs it and
   *  released with this pane; see `AuditStore` for why it is per-pane rather
   *  than a module-level map keyed by group. */
  private auditStore: AuditStore | null = null;
  /** Audit-log viewer (any orchestration pane), same overlay mechanics. */
  auditView: AuditView | null = null;
  auditOverlay: HTMLElement | null = null;
  auditBtn: HTMLButtonElement;
  /** Progress timeline (#608) — the same group-scoped audit data as the audit
   *  log plus gh issue/PR lifecycle, on a time axis. Same overlay mechanics
   *  as every other embeddable view; nothing here resizes the terminal. */
  timelineView: TimelineView | null = null;
  timelineOverlay: HTMLElement | null = null;
  timelineBtn: HTMLButtonElement;

  /** Token charts (#2011) — the group's persisted usage series, plotted.
   *  Shares the pane's ONE `orch_audit` read with the audit viewer and the
   *  timeline (#1317); its own series read is a separate command. */
  private tokensView: TokenChartsView | null = null;
  private tokensOverlay: HTMLElement | null = null;
  tokensBtn: HTMLButtonElement;
  /** Group lifecycle panel (orchestrator panes only), same mechanics. */
  groupView: GroupView | null = null;
  groupOverlay: HTMLElement | null = null;
  groupBtn: HTMLButtonElement;
  /** File-editor overlay (#174): file tree + code editor + search/replace.
   *  Unlike the others it is UNGATED — present in every pane type, plain
   *  terminals included. Same no-resize overlay mechanics. */
  fileEditView: FileEditView | null = null;
  fileEditOverlay: HTMLElement | null = null;
  fileEditBtn!: HTMLButtonElement;

  constructor(private readonly pane: Pane, header: HTMLElement) {
    this.tasksBtn = document.createElement("button");
    this.tasksBtn.className = "pane-btn";
    this.tasksBtn.innerHTML = TASKS_ICON;
    this.tasksBtn.title = EMBED_TOGGLE_TITLE.tasks;
    this.tasksBtn.hidden = true; // shown for orchestrator panes in start()
    this.tasksBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      this.toggleTasksView();
    });
    header.appendChild(this.tasksBtn);

    this.decisionsBtn = document.createElement("button");
    this.decisionsBtn.className = "pane-btn";
    this.decisionsBtn.innerHTML = DECISIONS_ICON;
    this.decisionsBtn.title = EMBED_TOGGLE_TITLE.decisions;
    this.decisionsBtn.hidden = true; // shown for orchestrator panes in start()
    this.decisionsBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      this.toggleDecisionsView();
    });
    header.appendChild(this.decisionsBtn);

    this.auditBtn = document.createElement("button");
    this.auditBtn.className = "pane-btn";
    this.auditBtn.innerHTML = AUDIT_ICON;
    this.auditBtn.title = EMBED_TOGGLE_TITLE.audit;
    this.auditBtn.hidden = true; // shown for orchestration panes in start()
    this.auditBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      this.toggleAuditView();
    });
    header.appendChild(this.auditBtn);

    this.timelineBtn = document.createElement("button");
    this.timelineBtn.className = "pane-btn";
    this.timelineBtn.innerHTML = TIMELINE_ICON;
    this.timelineBtn.title = EMBED_TOGGLE_TITLE.timeline;
    this.timelineBtn.hidden = true; // shown for orchestration panes in start()
    this.timelineBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      this.toggleTimelineView();
    });
    header.appendChild(this.timelineBtn);

    this.tokensBtn = document.createElement("button");
    this.tokensBtn.className = "pane-btn";
    this.tokensBtn.innerHTML = TOKENS_ICON;
    this.tokensBtn.title = EMBED_TOGGLE_TITLE.tokens;
    this.tokensBtn.hidden = true; // shown for orchestration panes in start()
    this.tokensBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      this.toggleTokensView();
    });
    header.appendChild(this.tokensBtn);

    this.groupBtn = document.createElement("button");
    this.groupBtn.className = "pane-btn";
    this.groupBtn.innerHTML = GROUP_ICON;
    this.groupBtn.title = EMBED_TOGGLE_TITLE.group;
    this.groupBtn.hidden = true; // shown for orchestrator panes in start()
    this.groupBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      this.toggleGroupView();
    });
    header.appendChild(this.groupBtn);
  }

  /** Constructor wiring that runs later in the header's build order than the rest. */
  wireOverlayToggles(header: HTMLElement): void {
    // The three overlay buttons below are `pty-only`: their panels FLOAT over the
    // terminal and are sized from its height, so they mean nothing on a pane that
    // has no terminal. CSS hides them on a files pane (#214) — see the toggles,
    // which refuse the hotkey path for the same reason.
    this.issuesBtn = document.createElement("button");
    this.issuesBtn.className = "pane-btn pty-only";
    this.issuesBtn.innerHTML = ISSUES_ICON;
    this.issuesBtn.title = EMBED_TOGGLE_TITLE.issues;
    this.issuesBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      this.toggleIssuesView();
    });
    header.appendChild(this.issuesBtn);

    this.gitBtn = document.createElement("button");
    this.gitBtn.className = "pane-btn pty-only";
    this.gitBtn.innerHTML = GIT_ICON;
    this.gitBtn.title = EMBED_TOGGLE_TITLE.git;
    this.gitBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      this.toggleGitView();
    });
    header.appendChild(this.gitBtn);

    // File-editor overlay (#174). Unconditional — every pane type gets it,
    // including plain terminals (unlike the orchestration-gated buttons above).
    // Except a files pane, which IS this surface already.
    this.fileEditBtn = document.createElement("button");
    this.fileEditBtn.className = "pane-btn pty-only";
    this.fileEditBtn.innerHTML = FILES_ICON;
    this.fileEditBtn.title = "File editor (Alt+F)";
    this.fileEditBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      this.toggleFileEditView();
    });
    header.appendChild(this.fileEditBtn);
  }

  /** Toggle the git view. It FLOATS over the top of the terminal — the
   *  terminal keeps its full size and PTY dimensions, so toggling never
   *  triggers a resize repaint (which would push duplicate TUI frames into
   *  scrollback). The bottom strip of the terminal stays visible and usable,
   *  with a draggable divider on the overlay's lower edge. */
  toggleGitView(): void {
    // Alt+G on a git pane: the pane already IS the git view. Refusing with a toast
    // would be absurd — just put the cursor in it.
    if (this.pane.contentKind === "git") {
      this.pane.focus();
      return;
    }
    if (this.pane.embeds.refuseOverlay("The git view")) return;
    this.ensureGitView();
    this.pane.embeds.toggleView("git");
  }

  /** Lazily construct the git view and register it into `embedRegistry`
   *  (#361) — the error-recovery `openView` wraps every `show()` in (never
   *  leave the pane half-toggled) generalizes what was originally a
   *  git-specific `try`/`catch` here, since a `refresh()` failure was the
   *  one case any view's `show()` could throw synchronously. */
  ensureGitView(): void {
    if (this.gitView) return;
    this.gitView = new GitView({
      getCwd: () => this.pane.cwdRaw,
      onClose: () => this.toggleGitView(),
      onRepoAction: () => {
        if (this.pane.cwdRaw) void this.pane.refreshDir(this.pane.cwdRaw);
      },
      onEmbedMenu: (anchor) => this.pane.embeds.showEmbedMenu("git", anchor),
    });
    this.gitOverlay = document.createElement("div");
    this.gitOverlay.className = "git-overlay";
    this.gitOverlay.hidden = true;
    this.gitOverlay.append(this.gitView.el, this.pane.embeds.makeOverlayDivider(() => this.gitOverlay!));
    this.pane.el.appendChild(this.gitOverlay);
    this.pane.embeds.embedRegistry.set("git", {
      overlayEl: this.gitOverlay,
      viewEl: this.gitView.el,
      show: () => this.gitView!.show(),
      hide: () => this.gitView!.hide(),
      setPanelActive: (active) => this.gitView!.setPanelActive(active),
      floorPx: () => EMBED_MIN_PANEL_PX,
    });
  }

  /** Toggle the GitHub issues overlay. Available on any pane whose cwd is a git
   *  repo (the view resolves the repo root itself). Same no-resize overlay
   *  mechanics as the git view — it FLOATS over the terminal and never resizes
   *  the PTY; only one overlay is open at a time. */
  toggleIssuesView(): void {
    if (this.pane.embeds.refuseOverlay("The issues view")) return;
    this.ensureIssuesView();
    this.pane.embeds.toggleView("issues");
  }

  /** Lazily construct the issues view and register it into `embedRegistry`
   *  (#361). */
  private ensureIssuesView(): void {
    if (this.issuesView) return;
    this.issuesView = new IssuesView({
      getCwd: () => this.pane.cwdRaw,
      getGroupId: () => this.pane.orchGroup,
      onClose: () => this.toggleIssuesView(),
      onEmbedMenu: (anchor) => this.pane.embeds.showEmbedMenu("issues", anchor),
    });
    this.issuesOverlay = document.createElement("div");
    this.issuesOverlay.className = "git-overlay";
    this.issuesOverlay.hidden = true;
    this.issuesOverlay.append(
      this.issuesView.el,
      this.pane.embeds.makeOverlayDivider(() => this.issuesOverlay!)
    );
    this.pane.el.appendChild(this.issuesOverlay);
    this.pane.embeds.embedRegistry.set("issues", {
      overlayEl: this.issuesOverlay,
      viewEl: this.issuesView.el,
      show: () => this.issuesView!.show(),
      hide: () => this.issuesView!.hide(),
      setPanelActive: (active) => this.issuesView!.setPanelActive(active),
      floorPx: () => EMBED_MIN_PANEL_PX,
    });
  }

  /** Toggle the task board open/closed (`Alt+T`, and the board's own ✕) — in
   *  EITHER mode. Which mode/side it opens in is a separate, persisted
   *  preference (see `embedViewAtSide`); this never changes it. */
  toggleTasksView(): void {
    if (!this.pane.orchGroup || this.tasksBtn.hidden) return;
    this.ensureTasksView();
    this.pane.embeds.toggleView("tasks");
  }

  /** Lazily construct the task board and register it into `embedRegistry`
   *  (#361). */
  ensureTasksView(): void {
    if (this.tasksView) return;
    this.tasksView = new TasksView(this.pane.orchGroup!, {
      onClose: () => this.toggleTasksView(),
      onEmbedMenu: (anchor) => this.pane.embeds.showEmbedMenu("tasks", anchor),
      takeFocus: () => this.pane.embeds.pendingFocus.take("tasks"),
      // The board-to-panel direction of the focus hook (#1091 slice G): a row
      // marked decision-blocked or demo-gated links straight to that item in
      // the NEEDS-YOU panel, routed through this same hook the panel's own
      // onFocusTask below uses the other way.
      onFocusDecision: (id) => this.pane.requestEmbedFocus("decisions", id),
      // The board's own bounded release (#1318): the pane's authoritative
      // on-screen read, so a `hide`/`show` pair that never arrives cannot leave
      // a visible board frozen. Read live, never snapshotted — the same
      // contract `getRepo`/`isDocked` take on the other views.
      isVisible: () => this.pane.embeds.isViewVisible("tasks"),
    });
    this.tasksOverlay = document.createElement("div");
    this.tasksOverlay.className = "git-overlay";
    this.tasksOverlay.hidden = true;
    this.tasksOverlay.append(this.tasksView.el, this.pane.embeds.makeOverlayDivider(() => this.tasksOverlay!));
    this.pane.el.appendChild(this.tasksOverlay);
    this.pane.embeds.embedRegistry.set("tasks", {
      overlayEl: this.tasksOverlay,
      viewEl: this.tasksView.el,
      show: () => this.tasksView!.show(),
      // Stops the board refetching-and-rebuilding while its panel is closed,
      // on every `orch-tasks-changed` (#1318). See `EmbedEntry.hide` for the
      // rule and for why this entry was written without one.
      hide: () => this.tasksView!.hide(),
      setPanelActive: (active) => this.tasksView!.setPanelActive(active),
      floorPx: () => EMBED_MIN_PANEL_PX,
    });
  }

  /** Toggle the NEEDS-YOU panel open/closed (`Alt+Q`, and the panel's own ✕) —
   *  in EITHER mode, exactly like the board's own toggle. */
  toggleDecisionsView(): void {
    if (!this.pane.orchGroup || this.decisionsBtn.hidden) return;
    this.ensureDecisionsView();
    this.pane.embeds.toggleView("decisions");
  }

  /** Lazily construct the NEEDS-YOU panel and register it into
   *  `embedRegistry` (#361, #1091 slice C). */
  ensureDecisionsView(): void {
    if (this.decisionsView) return;
    this.decisionsView = new DecisionsView(this.pane.orchGroup!, {
      onClose: () => this.toggleDecisionsView(),
      onEmbedMenu: (anchor) => this.pane.embeds.showEmbedMenu("decisions", anchor),
      // The panel-to-board direction of the focus hook: a card citing `t-N`
      // asks the PANE for the board, and the pane decides how to serve it.
      // The panel never reaches into another view.
      onFocusTask: (taskId) => this.pane.requestEmbedFocus("tasks", taskId),
      takeFocus: () => this.pane.embeds.pendingFocus.take("decisions"),
      // This panel's own bounded release (#1318) — see the board's, above.
      isVisible: () => this.pane.embeds.isViewVisible("decisions"),
    });
    this.decisionsOverlay = document.createElement("div");
    this.decisionsOverlay.className = "git-overlay";
    this.decisionsOverlay.hidden = true;
    this.decisionsOverlay.append(
      this.decisionsView.el,
      this.pane.embeds.makeOverlayDivider(() => this.decisionsOverlay!)
    );
    this.pane.el.appendChild(this.decisionsOverlay);
    this.pane.embeds.embedRegistry.set("decisions", {
      overlayEl: this.decisionsOverlay,
      viewEl: this.decisionsView.el,
      show: () => this.decisionsView!.show(),
      // Same as the board's (#1318), over three streams rather than two — and
      // one board write can fire two of them, since the demo-gate hook raises
      // or resolves a needs-you item inside `upsert_task`.
      hide: () => this.decisionsView!.hide(),
      setPanelActive: (active) => this.decisionsView!.setPanelActive(active),
      floorPx: () => EMBED_MIN_PANEL_PX,
    });
  }

  /** Toggle the audit-log viewer overlay (any orchestration pane). Same
   *  no-resize overlay mechanics as the git/task views; only one overlay is
   *  open at a time. */
  toggleAuditView(): void {
    if (!this.pane.orchGroup || this.auditBtn.hidden) return;
    this.ensureAuditView();
    this.pane.embeds.toggleView("audit");
  }

  /** This pane's shared `orch_audit` read (#1317), created on first use.
   *
   *  Bound to `orchGroup` the same way the two views that read it are — they
   *  capture it at construction too — so there is one store per pane per
   *  group's worth of views, and nothing keyed, nothing to prune, and no
   *  release rule beyond the pane's own teardown (INV-8a). `orchGroup` is
   *  non-null at every call site: both callers are reached only through a
   *  `toggle*View` that returns early without one. */
  private ensureAuditStore(): AuditStore {
    if (!this.auditStore) {
      const groupId = this.pane.orchGroup!;
      this.auditStore = new AuditStore(() => invoke<AuditEntry[]>("orch_audit", { groupId }));
    }
    return this.auditStore;
  }

  /** Lazily construct the audit view and register it into `embedRegistry`
   *  (#361). */
  ensureAuditView(): void {
    if (this.auditView) return;
    this.auditView = new AuditView(this.pane.orchGroup!, {
      onClose: () => this.toggleAuditView(),
      onEmbedMenu: (anchor) => this.pane.embeds.showEmbedMenu("audit", anchor),
      store: this.ensureAuditStore(),
    });
    this.auditOverlay = document.createElement("div");
    this.auditOverlay.className = "git-overlay";
    this.auditOverlay.hidden = true;
    this.auditOverlay.append(this.auditView.el, this.pane.embeds.makeOverlayDivider(() => this.auditOverlay!));
    this.pane.el.appendChild(this.auditOverlay);
    this.pane.embeds.embedRegistry.set("audit", {
      overlayEl: this.auditOverlay,
      viewEl: this.auditView.el,
      show: () => this.auditView!.show(),
      // Stops a live-follow poll that a close/eviction otherwise left running
      // for the rest of the session (#1318) — the same wiring the timeline's
      // identical follow toggle has always had.
      hide: () => this.auditView!.hide(),
      setPanelActive: (active) => this.auditView!.setPanelActive(active),
      floorPx: () => EMBED_MIN_PANEL_PX,
    });
  }

  /** Toggle the progress-timeline overlay (#608, any orchestration pane).
   *  Same no-resize overlay mechanics as the audit log it sits beside: it
   *  floats over the terminal, and docking it goes through the shared #361
   *  embed path — no ConPTY resize is introduced by this view. */
  toggleTimelineView(): void {
    if (!this.pane.orchGroup || this.timelineBtn.hidden) return;
    this.ensureTimelineView();
    this.pane.embeds.toggleView("timeline");
  }

  /** Lazily construct the progress timeline and register it into
   *  `embedRegistry` (#361). */
  ensureTimelineView(): void {
    if (this.timelineView) return;
    this.timelineView = new TimelineView(this.pane.orchGroup!, {
      onClose: () => this.toggleTimelineView(),
      onEmbedMenu: (anchor) => this.pane.embeds.showEmbedMenu("timeline", anchor),
      store: this.ensureAuditStore(),
      // Read live, not snapshotted at open time — the same contract the group
      // panel's workflow preview uses (#316). An orchestrator pane's cwd IS
      // the group's repo.
      getRepo: () => this.pane.cwdRaw,
    });
    this.timelineOverlay = document.createElement("div");
    this.timelineOverlay.className = "git-overlay";
    this.timelineOverlay.hidden = true;
    this.timelineOverlay.append(
      this.timelineView.el,
      this.pane.embeds.makeOverlayDivider(() => this.timelineOverlay!)
    );
    this.pane.el.appendChild(this.timelineOverlay);
    this.pane.embeds.embedRegistry.set("timeline", {
      overlayEl: this.timelineOverlay,
      viewEl: this.timelineView.el,
      show: () => this.timelineView!.show(),
      // Stops the follow poll on close/eviction — the leak #361 rev-38 found
      // on the group panel. The general rule this is one instance of lives on
      // `EmbedEntry.hide` as of #1318; it sat here, on one view's registration,
      // from #648 until then, which is most of why three later views missed it.
      hide: () => this.timelineView!.hide(),
      setPanelActive: (active) => this.timelineView!.setPanelActive(active),
      floorPx: () => EMBED_MIN_PANEL_PX,
    });
  }

  /** Toggle the token-charts overlay (#2011, any orchestration pane).
   *  Same no-resize overlay mechanics as the timeline it sits beside: it
   *  floats over the terminal, and docking it goes through the shared #361
   *  embed path — no ConPTY resize is introduced by this view (constraint 1). */
  toggleTokensView(): void {
    if (!this.pane.orchGroup || this.tokensBtn.hidden) return;
    this.ensureTokensView();
    this.pane.embeds.toggleView("tokens");
  }

  /** Lazily construct the token charts and register them into
   *  `embedRegistry` (#361). */
  ensureTokensView(): void {
    if (this.tokensView) return;
    this.tokensView = new TokenChartsView(this.pane.orchGroup!, {
      onClose: () => this.toggleTokensView(),
      onEmbedMenu: (anchor) => this.pane.embeds.showEmbedMenu("tokens", anchor),
      store: this.ensureAuditStore(),
      // Read live, never snapshotted — the same contract every other view
      // here takes on a pane-owned answer.
      isBoardVisible: () => this.pane.embeds.isViewVisible("tasks"),
    });
    this.tokensOverlay = document.createElement("div");
    this.tokensOverlay.className = "git-overlay";
    this.tokensOverlay.hidden = true;
    this.tokensOverlay.append(
      this.tokensView.el,
      this.pane.embeds.makeOverlayDivider(() => this.tokensOverlay!)
    );
    this.pane.el.appendChild(this.tokensOverlay);
    this.pane.embeds.embedRegistry.set("tokens", {
      overlayEl: this.tokensOverlay,
      viewEl: this.tokensView.el,
      show: () => this.tokensView!.show(),
      // Stops the 30 s follow poll on close/eviction — the leak #361 rev-38
      // found on the group panel, and the rule `EmbedEntry.hide` carries.
      hide: () => this.tokensView!.hide(),
      setPanelActive: (active) => this.tokensView!.setPanelActive(active),
      floorPx: () => EMBED_MIN_PANEL_PX,
    });
  }

  /** Toggle the group lifecycle panel overlay (orchestrator panes). Same
   *  no-resize overlay mechanics as the other views; only one is open. */
  toggleGroupView(): void {
    if (!this.pane.orchGroup || this.groupBtn.hidden) return;
    this.ensureGroupView();
    this.pane.embeds.toggleView("group");
  }

  /** Lazily construct the group lifecycle view and register it into
   *  `embedRegistry` (#361) — its floor is its own measured chrome
   *  (`groupFloor`), not the generic default, so the footer/suspended
   *  banner never clip in either hosting mode. */
  ensureGroupView(): void {
    if (this.groupView) return;
    this.groupView = new GroupView(this.pane.orchGroup!, {
      onClose: () => this.toggleGroupView(),
      // Mirror the header's fold-group toggle inside the lifecycle panel (#46).
      onToggleMinimize: () => this.pane.events.onToggleGroupMinimize(this.pane),
      // Content grew (e.g. the suspended banner appeared) — re-clamp
      // whichever host is currently active so the footer never slides under
      // overflow:hidden (#83 rev-58; generalized to embed mode by #361).
      onResize: () => this.pane.embeds.reclampViewFloor("group"),
      // The orchestrator pane's cwd IS the group's repo (create_orchestration
      // opens it there) — the workflow toggle's ON-confirm preview reads it
      // live rather than snapshotting at open time (#316).
      getRepo: () => this.pane.cwdRaw,
      // The header picker's *Edit…* (#1689 slice D2): the same call the file
      // browser's "Open in workflow pane" makes, one caller over.
      onEditWorkflow: (opts) => this.pane.events.onOpenWorkflowPane(this.pane, opts),
      onEmbedMenu: (anchor) => this.pane.embeds.showEmbedMenu("group", anchor),
    });
    this.groupOverlay = document.createElement("div");
    this.groupOverlay.className = "git-overlay";
    this.groupOverlay.hidden = true;
    this.groupOverlay.append(
      this.groupView.el,
      this.pane.embeds.makeOverlayDivider(() => this.groupOverlay!, () => this.pane.embeds.groupFloor())
    );
    this.pane.el.appendChild(this.groupOverlay);
    this.pane.embeds.embedRegistry.set("group", {
      overlayEl: this.groupOverlay,
      viewEl: this.groupView.el,
      show: () => this.groupView!.show(),
      // Stops the poll timer `show()` starts (#361 rev-38 NB2) — without
      // this, every close/eviction left it running, and swapping the embed
      // slot turned what was a rare pre-existing leak into an easy one to
      // hit on every reopen.
      hide: () => this.groupView!.hide(),
      setPanelActive: (active) => this.groupView!.setPanelActive(active),
      floorPx: () => this.pane.embeds.groupFloor(),
    });
  }

  /** Toggle the file-editor overlay (#174). Ungated — works in every pane
   *  type, plain terminals included. Same no-resize overlay mechanics as the
   *  other views, and (#361 scope increase) the same embed contract:
   *  dockable to any of the three slots, its toggle disabled while docked,
   *  single-occupant per slot — see docs/design/embedded-panels.md for why
   *  this changed (and what didn't: the #217 content-pane editor is a fully
   *  separate instance/class-option, `host.embedded`, untouched by this). */
  toggleFileEditView(): void {
    // Alt+F on an editor pane: the pane already IS the file editor. Same for a files
    // pane, whose surface is the file MANAGER — a sibling of the editor, and the pane
    // the user is looking at either way. Refusing with a toast would be absurd; just
    // put the cursor in it.
    if (this.pane.contentKind === "editor" || this.pane.contentKind === "files") {
      this.pane.focus();
      return;
    }
    if (this.pane.embeds.refuseOverlay("The file editor")) return;
    this.ensureFileEditView();
    this.pane.embeds.toggleView("editor");
  }

  /** Lazily construct the file editor and register it into `embedRegistry`
   *  (#361 scope increase). `isDocked` lets `FileEditView.requestClose` skip
   *  the #219 discard-confirm dialog while docked, since the underlying
   *  toggle no-ops then anyway — see `FileEditHost.isDocked`'s own doc
   *  comment for why that ordering matters (a "yes" on that dialog would
   *  otherwise discard real edits for a click that doesn't actually close
   *  anything). */
  ensureFileEditView(): void {
    if (this.fileEditView) return;
    this.fileEditView = new FileEditView({
      getCwd: () => this.pane.cwdRaw,
      onClose: () => this.toggleFileEditView(),
      isAgentWorktree: () =>
        this.pane.orchRoleName === "worker" || this.pane.orchRoleName === "reviewer",
      onEmbedMenu: (anchor) => this.pane.embeds.showEmbedMenu("editor", anchor),
      isDocked: () => this.pane.embeds.sideOf("editor") !== null,
    });
    this.fileEditOverlay = document.createElement("div");
    this.fileEditOverlay.className = "git-overlay";
    this.fileEditOverlay.hidden = true;
    this.fileEditOverlay.append(
      this.fileEditView.el,
      this.pane.embeds.makeOverlayDivider(() => this.fileEditOverlay!)
    );
    this.pane.el.appendChild(this.fileEditOverlay);
    this.pane.embeds.embedRegistry.set("editor", {
      overlayEl: this.fileEditOverlay,
      viewEl: this.fileEditView.el,
      show: () => this.fileEditView!.show(),
      hide: () => this.fileEditView!.hide(),
      setPanelActive: (active) => this.fileEditView!.setPanelActive(active),
      floorPx: () => EMBED_MIN_PANEL_PX,
    });
  }
}
