// The WORKFLOW pane (#222, restructured by #880): the repo's workflow file, made configurable.
//
// ONE buffer, and the buffer is the FILE (the Kestra pattern — an inspector edit rewrites the
// YAML under the hood; the YAML is never a stale export of some hidden canvas state). What
// changed with #880 is not the model, it is the SHAPE OF THE SCREEN:
//
//   ┌────────────┬───────────────────────────────┬──────────────┐
//   │  roster    │  the canvas (primary surface) │  inspector   │
//   │  (blocks,  │  ── or the raw YAML, which is │  (whatever   │
//   │   gate)    │     a toggle over the same    │   is         │
//   │            │     space)                    │   selected)  │
//   └────────────┴───────────────────────────────┴──────────────┘
//
// The canvas is the primary surface and the inspector is DOCKED beside it, because the thing
// #880 was opened about is that clicking a block did not visibly do anything: the property form
// lived behind a "Blocks" tab, the canvas behind a "Graph" tab, and so selecting something and
// showing it were two separate acts — of which the canvas performed only the first. Docking the
// editor removes the second act rather than remembering to perform it. The roster stays as the
// left column (it is also the keyboard/accessibility path to every selection), and the raw YAML
// stays first-class as a toggle over the canvas: it is a different modality over the same file,
// not a lesser one, and the file remains the source of truth.
//
// The canvas EDITS the file (#222 v2 — it was read-only in v1 and the human asked for more). It
// draws the declared happy path (ADVISORY edges: the orchestrator still schedules) and the merge
// gate (ENFORCED: the `gh` shim refuses a merge until the named reviewers' verdicts are PASS),
// and the two are drawn differently BECAUSE they mean different things. Every gesture goes out
// through the same pure model as a form edit, so it can never become a second source of truth.
//
// All the thinking lives in pure modules, which is where the tests are: the model behind the
// `workflowmodel.ts` barrel (`workflowparse.ts` reads the file, `workflowserialize.ts` writes it,
// `workflowvalidate.ts` finds what is wrong with it, `workflowgraph.ts` derives the graph and
// edits it, all over `workflowtypes.ts`), and `workflowpane.ts` (the pane's own decisions —
// which surface, what the inspector shows). This file is DOM: the frame, focus, dialogs, the
// roster, the findings, and the read/write path through the hash-guarded `ft*` file commands.
// Each panel is a satellite it delegates to (#3498 F3): the canvas (`workflowcanvas.ts`), the
// docked inspector (`workflowinspector.ts`, with its section forms in `workflowsections.ts`), the
// model-knob plumbing (`workflowcliknobs.ts`) and the file picker (`workflowfilemenu.ts`). They
// see this class only through `WorkflowViewApi` (`workflowviewapi.ts`).
//
// The one rule the sync has to obey: while the YAML does not PARSE, the inspector is disabled.
// An inspector edit serializes the model back over the buffer, and serializing a model we only
// half-understood would silently destroy the broken text the human is in the middle of
// fixing. So a syntax error disables the editor and says why; every other kind of breakage
// (an unknown kind, a dangling edge) still renders — as a stub, with a finding — because
// a block you cannot see is a block you cannot repair.

import {
  analyzeWorkflow,
  parseWorkflow,
  serializeWorkflow,
  serializeWorkflowPreserving,
  formatWorkflowText,
  isUnreadable,
  scaffoldWorkflowText,
  removeBlockAt,
  newBlock,
  isValidBlockId,
  hasErrors,
  WORKFLOW_FILE,
  legacyFallbackFor,
  DRIVER_DEFAULTS,
  type Workflow,
  type WorkflowBlock,
  type WorkflowAnalysis,
  type Finding,
  type FindingSection,
} from "./workflowmodel";
import { modelCatalog } from "./modelprobe";
import {
  layoutFileFor,
  parseLayout,
  serializeLayout,
  emptyLayout,
  layoutEquals,
  pruneLayout,
  withPosition,
  freeSlot,
  type Point,
  type WorkflowLayout,
} from "./workflowlayout";
import { ftReadFile, ftWriteFile, ftListDir, errorCode, errorMessage, type FileRead } from "./fileapi";
import { fmNewFolder, fmNewFile, fmErrorCode } from "./filemgr";
import {
  paneSurface,
  createAllowed,
  savePlan,
  layoutPruneIds,
  rewriteImpact,
  rewriteImpactMessage,
  surfaceForFinding,
  canvasDeleteAllowed,
  type LayoutWrite,
  type Selection,
  type Surface,
} from "./workflowpane";
import { layoutWriteAllowed } from "./workflowfilepicker";
import { appVersion } from "./pty";
import { closeDecision, discardEdits, type ConflictChoice } from "./dirtystate";
import { showToast } from "./toast";
  import { modal, promptModal } from "./modal";
import type { WorkflowHost, WorkflowViewApi } from "./workflowviewapi";
import { WorkflowCanvas } from "./workflowcanvas";
import { WorkflowCliKnobs } from "./workflowcliknobs";
import { WorkflowFileMenu } from "./workflowfilemenu";
import { WorkflowInspector, el } from "./workflowinspector";
import { WorkflowSections } from "./workflowsections";

export type { WorkflowHost } from "./workflowviewapi";

export class WorkflowView implements WorkflowViewApi {
  readonly el: HTMLElement;

  readonly host: WorkflowHost;
  root: string | null = null;
  rel: string = WORKFLOW_FILE;

  /** The live buffer — the single source of truth for every surface. The form serializes
   *  INTO it; the text editor edits it directly; the graph is derived from it. */
  text = "";
  /** The buffer as last written to (or read from) disk. `dirty` is text !== savedText. */
  savedText = "";
  /** The on-disk hash at read time, echoed back on write so a concurrent change (an agent,
   *  git, another editor) is a CONFLICT rather than a silent overwrite. "" = no file yet. */
  savedHash = "";
  /** False until the file exists on disk (a repo that has never had a workflow). */
  exists = false;
  /** Why the workflow file could not be READ, when it is there but we can't show it. Distinct
   *  from "there isn't one" — see the error surface. Null when the file loaded (or is simply
   *  absent, which is not an error). */
  loadError: string | null = null;
  /** Node positions (`workflow.layout.json`, beside the workflow file). NOT part of the workflow: a drag changes
   *  this and nothing else, and it is never serialized into the semantic file (§4). */
  layout: WorkflowLayout = emptyLayout();
  /** The layout as last written, so a drag that ends where it began writes nothing. */
  savedLayout: WorkflowLayout = emptyLayout();

  analysis: WorkflowAnalysis;
  selection: Selection = { kind: "workflow" };
  /** Which modality owns the middle of the pane. The canvas is primary; the raw YAML is a
   *  toggle over it (#880). The inspector is beside BOTH, so it is not on this axis. */
  private surface: Surface = "canvas";
  disposed = false;
  /** This build's version, for `authored_with:` on a workflow this pane CREATES. Empty
   *  until the async lookup lands (and if it never does — the key is simply not written,
   *  which beats writing `authored_with: unknown`). */
  private appVersion = "";

  // Header
  /** The file button — the picker's whole affordance (#2944). It was a `<span>` naming the
   *  open file; it still names it, and now opens the list of the repo's other workflows plus
   *  *New workflow…*. In the pane's own chrome, and a MENU rather than anything in the layout:
   *  constraint 1 — no PTY resize for a UI feature, and the header is not on that axis. */
  private pathLabel: HTMLButtonElement;
  /** Which load is current. Two clicks in the file menu start two `load()`s, and they may
   *  resolve in either order — the later-RESOLVING one would otherwise win `text`/`savedHash`
   *  while `this.rel` names the file the human clicked LAST, i.e. the buffer of one workflow
   *  under the path of another (rev-std round 1, N3). The hash guard bounded that to a
   *  conflict dialog rather than corruption, which is why it was non-blocking; a generation
   *  counter removes it instead of bounding it. Every await in the load path re-checks. */
  private loadGen = 0;
  private dirtyDot: HTMLElement;
  private saveBtn: HTMLButtonElement;
  private yamlBtn: HTMLButtonElement;
  private statusEl: HTMLElement;

  // Body
  private rosterEl: HTMLElement;
  /** The docked inspector's header (what is selected) and its body (the editor for it). */
  inspTitleEl: HTMLElement;
  inspSubEl: HTMLElement;
  formPane: HTMLElement;
  private yamlPane: HTMLElement;
  private yamlArea: HTMLTextAreaElement;
  graphPane: HTMLElement;
  private findingsEl: HTMLElement;
  private emptyEl: HTMLElement;
  private errorEl: HTMLElement;
  private errorTextEl: HTMLElement;
  private bodyEl: HTMLElement;
  /** The create button, and the two labels that name the file. All three are re-stated in
   *  `render()` rather than fixed at construction: the button because being pressable is a
   *  DECISION (`createAllowed`) and not a side-effect of being on screen, and the labels because
   *  this pane opens on any `.yml` the file browser hands it (#217's `file`), so a pane rooted on
   *  `ci/flow.yml` that says the default workflow path is telling the human about a file they are
   *  not looking at — which, on the error surface, means naming the wrong file as unreadable. */
  private starterBtn: HTMLButtonElement;
  private startPathEl: HTMLElement;
  private errorTitleEl: HTMLElement;
  /** Releases this pane's `modelCatalog.onReport` subscription. Held so
   *  `dispose()` can call it — see the subscription itself. */
  private unsubscribeReports: (() => void) | null = null;

  // The per-panel sub-controllers (#3498 F3). Each owns its panel's state and methods and
  // reads the rest of the pane through `WorkflowViewApi` (workflowviewapi.ts), never through
  // this module; docs/design/module-layout.md has the shape.
  readonly canvas: WorkflowCanvas = new WorkflowCanvas(this);
  readonly knobs: WorkflowCliKnobs = new WorkflowCliKnobs(this);
  private readonly fileMenu: WorkflowFileMenu = new WorkflowFileMenu(this);
  readonly inspector: WorkflowInspector = new WorkflowInspector(this);
  readonly sections: WorkflowSections = new WorkflowSections(this);

  constructor(host: WorkflowHost) {
    this.host = host;
    this.analysis = analyzeWorkflow("");

    this.el = el("div", "wf");
    // Focusable like every other content view, so Alt+arrow nav / dock-restore / window
    // refocus land ON the surface without grabbing one of its inner controls.
    this.el.tabIndex = -1;

    // Take the startup sweep's answers as they land (#1020) — the push half of
    // detection, for a pane that was already open when one arrived. The pull
    // half is in `blockForm`, for a form opened after the sweep finished.
    //
    // Not filtered by program, deliberately: only CLIs with a `PROTOCOLS` row
    // are ever swept (one today), so this fires at most a handful of times in an
    // app run, and deciding whether a report is "relevant" would mean deciding
    // what a block with no `cli:` inherits — a question `analyzeWorkflow` is
    // about to re-answer anyway.
    //
    // `disposed` is the liveness answer `onReport` asks for: a closed pane must
    // stop being repainted, and this is the only teardown signal it has.
    // The unsubscribe is HELD and called from `dispose()`, not discarded: the
    // catalog is app-scoped and this pane is not, so a subscription nobody
    // releases retains the whole view — its analysis, its detached DOM — for the
    // life of the process (rev-713 blocking 2). `disposed` is the same answer
    // given to the catalog's own prune, for a pane that is closed some other
    // way.
    this.unsubscribeReports = modelCatalog.onReport(
      (program) => this.knobs.applyDetection(program),
      () => !this.disposed
    );

    // ---- header ----
    const head = el("div", "wf-head");
    this.pathLabel = document.createElement("button");
    this.pathLabel.className = "wf-path";
    this.pathLabel.addEventListener("click", (e) => {
      const r = this.pathLabel.getBoundingClientRect();
      this.fileMenu.showFileMenu(r.left, r.bottom + 2);
      e.stopPropagation();
    });
    this.dirtyDot = el("span", "wf-dirty", "●");
    this.dirtyDot.title = "Unsaved changes";
    this.dirtyDot.hidden = true;
    this.statusEl = el("span", "wf-status");

    this.saveBtn = document.createElement("button");
    this.saveBtn.className = "wf-btn";
    this.saveBtn.textContent = "Save";
    this.saveBtn.title = "Save (Ctrl+S)";
    this.saveBtn.disabled = true;
    this.saveBtn.addEventListener("click", () => void this.save());

    const formatBtn = document.createElement("button");
    formatBtn.className = "wf-btn";
    formatBtn.textContent = "Format";
    formatBtn.title = "Rewrite the file in canonical form (fixed key order, references in roster order)";
    formatBtn.addEventListener("click", () => void this.format());

    const reloadBtn = document.createElement("button");
    reloadBtn.className = "wf-btn";
    reloadBtn.textContent = "Reload";
    reloadBtn.title = "Re-read the file from disk";
    reloadBtn.addEventListener("click", () => void this.reload());

    // The YAML toggle — the one surface control left now that the tabs are gone (#880). It is a
    // toggle and not a tab because the two are not peers on screen: the canvas is where the pane
    // lives, and the raw text is a modality you switch INTO deliberately and come back from.
    this.yamlBtn = document.createElement("button");
    this.yamlBtn.className = "wf-btn";
    this.yamlBtn.textContent = "YAML";
    this.yamlBtn.title = "Edit the raw file instead of the canvas (the same buffer, the other way round)";
    this.yamlBtn.addEventListener("click", () =>
      this.setSurface(this.surface === "yaml" ? "canvas" : "yaml")
    );

    const spacer = el("span", "wf-spacer");
    head.append(
      this.pathLabel,
      this.dirtyDot,
      this.statusEl,
      spacer,
      this.yamlBtn,
      formatBtn,
      reloadBtn,
      this.saveBtn
    );
    if (!host.embedded) {
      const closeBtn = document.createElement("button");
      closeBtn.className = "wf-btn";
      closeBtn.textContent = "✕";
      closeBtn.addEventListener("click", () => void this.requestClose());
      head.append(closeBtn);
    }

    // ---- the START surface (no workflow file yet) ----
    //
    // Not a big empty box with a sentence in it. A repo with no workflow is the NORMAL
    // starting point — it is where every repo begins — so this is the pane's front door, and
    // a front door should be the shortest path to being inside. One line of what a workflow
    // is, one button that writes a real, commented, valid one, and the roster it will contain
    // so nobody has to press the button to find out what it does.
    this.emptyEl = el("div", "wf-start");
    const startHead = el("div", "wf-start-head");
    this.startPathEl = el("span", "wf-start-path", WORKFLOW_FILE);
    startHead.append(el("span", "wf-start-title", "Start a workflow"), this.startPathEl);
    const startBody = el(
      "div",
      "wf-start-body",
      "Declares the agent blocks a run may use, the path between them, and the gate that must " +
        "pass before a merge. Committed, so everyone who clones the repo gets it. Orrerix reads " +
        "it only when Advanced orchestrator is ticked."
    );
    const starterBtn = document.createElement("button");
    this.starterBtn = starterBtn;
    starterBtn.className = "wf-btn wf-btn-primary";
    starterBtn.textContent = "Create workflow";
    starterBtn.title = "";  // set from `this.rel` in render() — see the startPathEl note there
    starterBtn.addEventListener("click", () => void this.scaffold());

    // What the button is about to write. A preview is cheaper than a paragraph and it is the
    // thing they actually want to know.
    const preview = el("div", "wf-start-preview");
    for (const [kind, label] of [
      ["planner", "Planner"],
      ["worker", "Worker"],
      ["reviewer", "Reviewer"],
    ] as const) {
      const chip = el("span", `wf-chip wf-chip-${kind}`, label);
      preview.append(chip);
    }
    preview.append(el("span", "wf-start-gate", "→ merge gate: the reviewer must PASS"));

    const startRow = el("div", "wf-start-row");
    startRow.append(starterBtn, preview);
    this.emptyEl.append(startHead, startBody, startRow);
    this.emptyEl.hidden = true;

    // ---- the ERROR surface (a workflow file that exists but cannot be read) ----
    //
    // Its own state, and that is the whole point (v2 bug 1). This used to fall through to the
    // empty state: a file that WAS there — saved as UTF-16 by a PowerShell redirect, say —
    // reported "No workflow in this repo yet" and offered to create one over the top of it.
    // The pane must never invite you to overwrite a file it refused to show you.
    this.errorEl = el("div", "wf-start");
    this.errorTextEl = el("div", "wf-start-body");
    this.errorTitleEl = el("div", "wf-start-title", `Can't read ${WORKFLOW_FILE}`);
    const retry = document.createElement("button");
    retry.className = "wf-btn";
    retry.textContent = "Retry";
    retry.addEventListener("click", () => void this.load());
    const errRow = el("div", "wf-start-row");
    errRow.append(retry);
    this.errorEl.append(this.errorTitleEl, this.errorTextEl, errRow);
    this.errorEl.hidden = true;

    // ---- roster (left) ----
    this.rosterEl = el("div", "wf-roster");

    // ---- the primary surface (middle) and the docked inspector (right) ----
    this.formPane = el("div", "wf-form");
    this.yamlPane = el("div", "wf-yaml");
    this.yamlArea = document.createElement("textarea");
    this.yamlArea.className = "wf-yaml-area";
    this.yamlArea.spellcheck = false;
    this.yamlArea.addEventListener("input", () => {
      // The text is the buffer. Re-read the model from it, refresh every OTHER surface,
      // and leave the textarea alone — rewriting it under the caret is how an editor
      // eats a keystroke.
      this.text = this.yamlArea.value;
      this.reanalyze();
      this.renderSelection();
      this.renderFindings();
      this.updateDirty();
    });
    this.yamlPane.append(this.yamlArea);
    this.graphPane = el("div", "wf-graph");

    // The primary surface: the canvas, or the raw YAML in its place. Exactly one is on screen,
    // and `hidden` is what says which (styles.css's `[hidden] { display: none !important }` is
    // load-bearing here — see test/hiddenrule.test.ts for why that is not belt and braces).
    const surfaceEl = el("div", "wf-surface");
    surfaceEl.append(this.graphPane, this.yamlPane);

    // The inspector, docked. Its HEAD is the part that makes a canvas click legible from across
    // the pane: it names what is selected, by id, beside the node you just clicked.
    this.inspTitleEl = el("div", "wf-insp-title");
    this.inspSubEl = el("div", "wf-insp-sub");
    const inspHead = el("div", "wf-insp-head");
    inspHead.append(this.inspTitleEl, this.inspSubEl);
    const inspector = el("div", "wf-inspector");
    inspector.append(inspHead, this.formPane);

    const main = el("div", "wf-main");
    main.append(surfaceEl, inspector);

    this.bodyEl = el("div", "wf-body");
    this.bodyEl.append(this.rosterEl, main);

    this.findingsEl = el("div", "wf-findings");

    // All FIVE surfaces. `errorEl` was built and never appended (rev-15 F1), so the state
    // added to fix the UTF-16 bug rendered as a blank pane — the fix's own headline case was
    // the one thing that didn't work. `render()` only toggles `hidden`; a surface that is not
    // in the document has nothing to un-hide.
    this.el.append(head, this.errorEl, this.emptyEl, this.bodyEl, this.findingsEl);

    // Which primary surface is showing, stated once BEFORE the first load resolves — otherwise
    // both the canvas and the raw YAML sit un-hidden until `render()` first reaches its body
    // surface, and a pane that opens on the error or start surface never gets there at all.
    this.applySurface();

    // Ctrl+S saves from anywhere in the pane — including from inside the textarea, where
    // the browser would otherwise do nothing at all.
    this.el.addEventListener("keydown", (e) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s") {
        e.preventDefault();
        void this.save();
        return;
      }
      // Delete removes what the CANVAS has selected — and only on the canvas, and never from
      // inside a field. Both halves are `canvasDeleteAllowed` (workflowpane.ts), and the second
      // half matters more now than it did under the tabs: the inspector is docked BESIDE the
      // canvas, so "typing in this block's prompt" and "this block is selected on the canvas"
      // are now the normal state rather than mutually exclusive tabs.
      if (e.key === "Delete" || e.key === "Backspace") {
        const inField = !!(e.target as HTMLElement | null)?.closest?.("input, textarea, select");
        if (!canvasDeleteAllowed({ surface: this.surface, inField })) return;
        e.preventDefault();
        this.canvas.deleteSelection();
      }
    });
  }

  // ---------- lifecycle ----------

  /** Load the file and render. Called by the pane once the view is in the document. */
  show(): void {
    this.el.hidden = false;
    void appVersion().then((v) => {
      if (!this.disposed) this.appVersion = v;
    });
    this.root = this.host.getRoot();
    this.retarget(this.host.getFile?.() || WORKFLOW_FILE);
    void this.load();
    // Not awaited with the load: the file the pane was ASKED to show opens regardless of
    // whether the repo's listing can be read, and the picker fills in when it lands.
    void this.fileMenu.refreshListing();
  }

  hide(): void {
    this.el.hidden = true;
  }

  dispose(): void {
    this.disposed = true;
    // Released here rather than left to the catalog's next prune: this is the
    // moment the view becomes garbage, and the prune only runs when something
    // else subscribes — which may be never.
    this.unsubscribeReports?.();
    this.unsubscribeReports = null;
    this.el.remove();
  }

  focus(): void {
    (this.surface === "yaml" ? this.yamlArea : this.el).focus();
  }

  // ---------- the unsaved-work contract (shared with the editor pane, #219) ----------

  /** Unsaved edits right now — asked WITHOUT prompting. The tab-close path needs the fact
   *  before it can decide how to ask. */
  get dirty(): boolean {
    return this.text !== this.savedText;
  }

  /** The file this view holds, for the persisted layout (#217's `file` field). */
  get openPathRel(): string {
    return this.rel;
  }

  /** May the pane close? Clean → yes; dirty → ask, and a confirmed discard ACTUALLY
   *  discards (the same `discardEdits` rule the editor obeys, stated once in
   *  dirtystate.ts so this view cannot quietly re-implement "discard" as "hide"). */
  async canDiscard(): Promise<boolean> {
    if (closeDecision(this.dirty) === "close") return true;
    const discard = await modal<boolean>((resolve) => ({
      title: "Discard unsaved workflow changes?",
      body: `${this.rel} has unsaved edits. Discarding drops them — the workflow goes back to what's on disk.`,
      buttons: [
        { label: "Cancel", value: false },
        { label: "Discard", value: true, kind: "danger" },
      ],
      onKey: (k) => (k === "Escape" ? resolve(false) : undefined),
    }));
    if (discard) {
      this.setText(discardEdits(this.savedText));
      this.render();
    }
    return discard;
  }

  /** What this view is holding, for the app-quit guard's enumeration (#219). */
  bufferReport(): { file: string | null; dirty: boolean } | null {
    return { file: this.rel, dirty: this.dirty };
  }

  private async requestClose(): Promise<void> {
    if (!(await this.canDiscard())) return;
    this.host.onClose();
  }

  // ---------- disk ----------

  /** Read the workflow, and the canvas layout beside it.
   *
   *  THE BUG THIS METHOD USED TO HAVE (v2 bug 1, and it is the one the human hit): it treated
   *  EVERY read failure as "there is no workflow here". Only `not-found` means that. A file
   *  that exists but cannot be decoded — and the ordinary way to produce one on Windows is to
   *  create it from PowerShell, whose `>` and `Out-File` write UTF-16, which is not valid
   *  UTF-8, which the backend correctly reports as `binary` — rendered the "no workflow yet"
   *  empty state behind a toast that had already gone. The pane then offered to CREATE a
   *  starter over the top of a file it had refused to show. So the two are now separate
   *  states, and the error one has no create button in it. */
  async load(): Promise<void> {
    if (!this.root) {
      this.setText("");
      this.render();
      return;
    }
    // A fresh read is a different file (or a different version of one), so a rewrite the human
    // consented to earlier was consent about text that is no longer there.
    this.rewriteConfirmed = false;
    // Claim this load. Anything that started earlier and resolves later is STALE and drops its
    // result on the floor rather than writing it under whatever `this.rel` now says.
    const gen = ++this.loadGen;
    const stale = (): boolean => this.disposed || gen !== this.loadGen;
    try {
      const fr = await ftReadFile(this.root, this.rel);
      if (stale()) return;
      this.exists = true;
      this.loadError = null;
      this.savedHash = fr.hash;
      this.savedText = fr.content;
      this.text = fr.content;
    } catch (err) {
      if (stale()) return;
      const code = errorCode(err);
      // #1153 phase 4: the DEFAULT path missing is the one case that means
      // "maybe this repo still uses the old `.loomux/` spelling". Adopting it is
      // conditional on the legacy read SUCCEEDING — a repo with neither file
      // must stay on the preferred path, or the empty state would offer to
      // create a workflow at the deprecated name. `legacyFallbackFor` returns
      // null for the legacy path itself and for any explicit host-supplied file,
      // so at most one extra read happens and only from the default. A
      // `binary`/permission error is NOT a fallback trigger: that file is there,
      // and quietly opening a different one would hide it.
      const legacy = code === "not-found" ? legacyFallbackFor(this.rel) : null;
      if (legacy) {
        const found = await this.readLegacy(this.root, legacy);
        if (stale()) return;
        if (found) {
          this.retarget(legacy);
          this.exists = true;
          this.loadError = null;
          this.savedHash = found.hash;
          this.savedText = found.content;
          this.text = found.content;
          await this.loadLayout();
          if (stale()) return;
          this.reanalyze();
          this.render();
          return;
        }
      }
      this.exists = false;
      this.savedHash = "";
      this.savedText = "";
      this.text = "";
      // "not-found" is not an error: it is a repo that hasn't written a workflow yet, which is
      // where every repo starts. ANYTHING else means the file is there and we can't read it.
      this.loadError =
        code === "not-found"
          ? null
          : code === "binary"
            ? `The file is there, but it isn't valid UTF-8 text — so orrerix can't read it, and neither can the backend. A workflow written from PowerShell with \`>\` or \`Out-File\` is UTF-16; re-save it as UTF-8 (\`Set-Content -Encoding utf8NoBOM\`) and it will open.`
            : `${errorMessage(err)}`;
    }
    await this.loadLayout();
    if (stale()) return;
    this.reanalyze();
    this.render();
  }

  /** Read a fallback workflow path, or null if there is nothing usable there. A read that
   *  fails for ANY reason returns null: the preferred path's own error is the one the pane
   *  reports, and a second file's permission problem must not replace it. */
  private async readLegacy(root: string, rel: string): Promise<FileRead | null> {
    try {
      return await ftReadFile(root, rel);
    } catch {
      return null;
    }
  }

  /** Point this pane at `rel` — the path its header shows, its saves write, and its layout
   *  sibling is derived from. One setter, so those three cannot drift apart. */
  retarget(rel: string): void {
    this.rel = rel;
    this.pathLabel.textContent = `${rel} ▾`;
    this.pathLabel.title =
      (this.root ? `${this.root} · ${rel}` : rel) + "\nClick to open another of this repo's workflows, or create one.";
  }

  /** The canvas positions. A layout that is missing or corrupt is simply COMPUTED instead —
   *  never a finding, never a dialog, never a reason not to open the workflow. Nothing in that
   *  file is anyone's work; it is a picture we can redraw. */
  private async loadLayout(): Promise<void> {
    if (!this.root) return;
    try {
      const fr = await ftReadFile(this.root, layoutFileFor(this.rel));
      if (this.disposed) return;
      this.layout = parseLayout(fr.content);
    } catch {
      this.layout = emptyLayout();
    }
    this.savedLayout = this.layout;
  }

  private async reload(): Promise<void> {
    if (this.dirty && !(await this.canDiscard())) return;
    await this.load();
  }

  async save(): Promise<void> {
    if (!this.root || !this.dirty) return;
    // No rewrite-impact gate here (#233): every form/canvas edit already went through
    // `commit()`, which reuses the ORIGINAL text for whatever it didn't touch — so by the
    // time a save happens, `this.text` is not a blind canonical rewrite of the whole file.
    // The one operation left that still rewrites wholesale on purpose is Format, and it asks
    // there, not here.
    //
    // Saving a file whose YAML doesn't parse is allowed on purpose: it is text, the human
    // may be mid-edit, and a half-finished workflow on disk is recoverable while a lost
    // one is not. The findings strip is what says it isn't runnable yet.
    try {
      await this.ensureConfigDir();
      // CREATING vs EDITING are different writes, and conflating them destroyed files
      // (rev-15 F2). When we believe there is no file, we cannot write with a null expected
      // hash — `write_file` reads that as "write unconditionally", so a workflow that appeared
      // AFTER the pane opened (an agent wrote one, a `git pull` brought one in, a teammate's
      // branch landed) was overwritten by our scaffold, and the pane said "Saved".
      //
      // So a create CLAIMS THE PATH first, atomically: `fm_new_file` is `create_new(true)`,
      // which refuses — without truncating — if anything is already there. Then we read the
      // (empty) file we just made and write against ITS hash, so even the sliver between the
      // claim and the write is guarded by the same conflict machinery as every other save.
      const plan = savePlan({ exists: this.exists, savedHash: this.savedHash });
      const hash = plan.kind === "guarded-write" ? plan.expectedHash : await this.claimFile();
      if (hash === null) return; // the path was taken; the error surface now says so
      const res = await ftWriteFile(this.root, this.rel, this.text, hash);
      this.savedText = this.text;
      this.savedHash = res.hash;
      this.exists = true;
      this.updateDirty();
      await this.saveLayout("save"); // the roster on disk and in memory are the same roster now
      showToast(`Saved ${this.rel}`, "info");
    } catch (err) {
      if (errorCode(err) === "conflict") await this.resolveConflict();
      else showToast(`Save failed: ${errorMessage(err)}`);
    }
  }

  /** Ask ONCE, before the first **Format** that would rewrite a human-authored file into fully
   *  canonical form — and only when that rewrite actually costs them something (rev-15 F6,
   *  moved here from every save by #233).
   *
   *  Before #233, EVERY form or canvas edit re-serialized the whole workflow from the model,
   *  unconditionally, and the model did not carry comments — so this guarded every `Ctrl+S`.
   *  Now `commit()` (below) reuses the original text for whatever an edit didn't touch, so an
   *  ordinary save no longer performs the all-or-nothing rewrite this dialog is about. The one
   *  place that rewrite still happens ON PURPOSE is the explicit **Format** button — a human
   *  asking to canonicalize the whole file, comments and all, in one step — and that is the
   *  only place left that needs to say so first.
   *
   *  ONCE per file, not once per Format press: a human who has said "yes, canonicalize it" has
   *  said it about that file, and asking again on every press is how you train someone to stop
   *  reading the question. Reset by `load()`, because that is a different file (or a different
   *  version of it) and the answer was about the old one.
   *
   *  CANCEL IS THE DEFAULT — the affirmative button is deliberately not the focused one here,
   *  which is the opposite of every other dialog in this pane. Everything else asks about
   *  something recoverable; this asks about work that is not. */
  private rewriteConfirmed = false;

  private async confirmFormatRewrite(canonical: string): Promise<boolean> {
    if (this.rewriteConfirmed) return true;
    const impact = rewriteImpact(this.text, canonical, (t) => formatWorkflowText(t) === t);
    if (!impact) return true; // a faithful rewrite — silent, as it should be

    const ok = await modal<boolean>((resolve) => ({
      title: "This rewrites the file",
      body: rewriteImpactMessage(impact, this.rel),
      buttons: [
        { label: "Rewrite and format", value: true, kind: "danger" },
        { label: "Cancel", value: false },
      ],
      onKey: (k) => (k === "Escape" ? resolve(false) : undefined),
    }));
    if (ok) this.rewriteConfirmed = true;
    return ok;
  }

  /** Claim `this.rel` for a file that does not exist yet, and return the hash to write
   *  against — or null when something got there first, in which case the pane is now showing
   *  the error surface and the caller must not write.
   *
   *  `fm_new_file` is the atomic half: `create_new(true)` ("create, but only if it isn't
   *  there") is one syscall, so there is no window between the check and the create. The
   *  `ftReadFile` after it is what turns the rest of the save into an ordinary hash-guarded
   *  write — if anything touches the file between our claim and our write, that is a conflict
   *  and the human gets the same three-way choice as always, instead of a silent overwrite. */
  private async claimFile(): Promise<string | null> {
    const root = this.root!;
    const parts = this.rel.split(/[\\/]/);
    const name = parts.pop() ?? WORKFLOW_FILE;
    const dir = parts.join("/");
    try {
      await fmNewFile(root, dir, name);
    } catch (err) {
      if (fmErrorCode(err) !== "exists") throw err;
      // Something wrote a workflow while this pane was sitting on its start surface. Do NOT
      // scaffold over it — it is somebody's work, it is probably the thing they wanted, and
      // this pane has never even shown it to them. Say so, and let Retry read it.
      this.loadError =
        `A workflow appeared at ${this.rel} while this pane was open — written by an agent, a git pull, or another editor. ` +
        `It has NOT been overwritten. Retry to load it (your unsaved text is discarded).`;
      this.render();
      showToast(`${this.rel} already exists — nothing was overwritten.`);
      return null;
    }
    const fresh = await ftReadFile(root, this.rel); // the empty file we just created
    return fresh.hash;
  }

  /** Make sure the workflow file's directory exists before writing into it.
   *
   *  THE OTHER HALF OF v2 BUG 1, and it made the pane's headline feature a lie: `ft_write_file`
   *  writes atomically (temp file + rename) and does NOT create parent directories, so in a
   *  repo with no config dir — i.e. EVERY repo that has never had a workflow, which is exactly
   *  the repo the "create a workflow" button exists for — the write failed with a raw io error
   *  ("The system cannot find the path specified"). The button appeared to work, the toast
   *  said "Save failed", and reopening the pane showed the empty state again, because nothing
   *  had ever been written. Between the two halves, the pane both mis-reported an existing
   *  workflow as absent AND could not create the one it offered to create.
   *
   *  No new backend command: `fm_new_folder` (#214, the file manager's "New folder") already
   *  does exactly this, through the same root+rel path safety. An "it already exists" failure
   *  is the success case here, so every error is swallowed and the WRITE is left to be the
   *  thing that reports a real problem — it is the one that knows whether it worked. */
  private async ensureConfigDir(): Promise<void> {
    if (!this.root) return;
    const parts = this.rel.split(/[\\/]/).filter((p) => p !== "" && p !== ".");
    parts.pop(); // the file name
    if (!parts.length) return; // a workflow file at the repo root needs no directory
    try {
      await ftListDir(this.root, parts.join("/"));
      return; // already there
    } catch {
      // Not there (or not readable) — build it, ONE SEGMENT AT A TIME. `fm_new_folder` takes
      // a parent `rel` and a single validated `name`, and its `validate_name` refuses a `/`
      // outright ("create_dir, NOT create_dir_all" — filemgr.rs) — so the old single call
      // with `.orrerix/workflows` could never have worked. It never had to: until #2944
      // nothing here created a file below the config dir, and `.orrerix` is one segment.
      // *New workflow…* is the first caller two levels down.
      for (let i = 0; i < parts.length; i++) {
        try {
          await fmNewFolder(this.root, parts.slice(0, i).join("/"), parts[i]);
        } catch {
          // Swallowed on purpose: "already exists" lands here (so does a race with something
          // else creating it), and the WRITE immediately after is the honest test of whether
          // we can proceed — it is the one that knows.
        }
      }
    }
  }

  /** Write the canvas positions, if they changed.
   *
   *  Deliberately NOT part of the dirty/unsaved-work contract: a node's x/y is not the human's
   *  WORK, and a dialog asking whether to save the fact that you nudged a box is a dialog that
   *  teaches people to click through dialogs. A drag writes it directly; a real save writes it
   *  too. No hash guard — this file is ours, nobody else writes it, and a lost position costs a
   *  drag.
   *
   *  `prune` is only ever true from `save()`, and that is the whole of rev-15 F5. Pruning drops
   *  the positions of blocks that "no longer exist" — but a DRAG happens against the unsaved
   *  buffer, where a block the human has deleted-but-not-saved does not exist *yet*. Pruning
   *  there wrote the deletion into `workflow.layout.json` on disk before the human had committed
   *  it to `workflow.yml`, so discarding the edit brought the block back with its position gone.
   *  Pruning belongs where its own comment always claimed it was: at a save, just after the
   *  workflow write succeeded — which is the one moment the roster on disk and the roster in
   *  memory are the same roster. */
  async saveLayout(when: LayoutWrite = "drag"): Promise<void> {
    if (!this.root) return;
    // WHAT MAY BE FORGOTTEN is a rule (`workflowpane.layoutPruneIds`), not a flag: on a save the
    // roster on disk and the roster in memory are the same, so pruning against it is safe; on a
    // drag they are not, so the union of the two is what survives.
    // WHICH FILE these positions belong to, captured BEFORE any await (#2944, rev-final round
    // 2). Everything below was pruned against the roster of the file the pane is showing right
    // now, but the destination used to be re-derived from `this.rel` after two awaits — so a
    // switch landing in that window sent one workflow's node positions into the OTHER
    // workflow's sidecar. That is this pane's own "never written to the other file" rule
    // broken through the layout instead of the buffer, where the unsaved-buffer guard cannot
    // see it and the conflict machinery does not apply (the layout is written with a null
    // hash, because nothing else writes it).
    const computedFor = this.rel;
    const saved = this.savedText.trim() ? parseWorkflow(this.savedText).workflow : null;
    const next = pruneLayout(this.layout, layoutPruneIds(saved, this.analysis.workflow, when));
    this.layout = next;
    if (layoutEquals(next, this.savedLayout)) return;
    try {
      await this.ensureConfigDir();
      // DROPPED, not redirected to `computedFor`: positions belong to the roster they were
      // pruned against and the pane has moved on, so re-aiming them would write a stale
      // picture. A layout is never anyone's work — it comes back computed — so losing one
      // costs a drag, while writing it into the wrong file corrupts a workflow the human was
      // not even editing. `savedLayout` is deliberately NOT advanced here: the write did not
      // happen, and claiming it did would suppress the next honest attempt.
      if (!layoutWriteAllowed(computedFor, this.rel)) return;
      await ftWriteFile(this.root, layoutFileFor(computedFor), serializeLayout(next), null);
      if (!layoutWriteAllowed(computedFor, this.rel)) return; // it moved while we wrote
      this.savedLayout = next;
    } catch {
      // A layout we couldn't save is a picture that comes back computed instead. Not worth a
      // toast, and certainly not worth failing the workflow save that may have preceded it.
    }
  }

  /** Write the scaffold — a commented, valid workflow — into the buffer, and save it. The one
   *  moment `authored_with:` is stamped, because this is the one moment the pane AUTHORS a
   *  file rather than editing one. */
  async scaffold(): Promise<void> {
    // THE LAST WORD ON THE CREATE PATH (#222 live bug 3). A create is allowed on the start
    // surface and nowhere else — `createAllowed` is the same decision that draws the button, so
    // reaching here in any other state means the DOM has drifted from the rules, which is exactly
    // what happened: a stylesheet left the button on screen over a loaded workflow, and pressing
    // it scaffolded over that workflow with a hash-guarded write that the backend was right to
    // honour. Refusing here means no future wiring mistake, CSS or otherwise, can turn "Create"
    // into "destroy" — the guard no longer depends on the button being where we think it is.
    if (!createAllowed({ loadError: this.loadError, exists: this.exists, text: this.text })) {
      // Two states refuse a create, and the message has to be true in BOTH (rev-17 F5): a workflow
      // is loaded, OR a file is there that we could not read. "Already open" is a lie in the second
      // one — the file precisely did not open, which is the whole reason we won't scaffold over it.
      // What holds either way is the only thing worth saying: nothing was destroyed.
      showToast("Nothing was created or overwritten — Create is only offered where there's no workflow.");
      this.render();
      return;
    }
    // `this.rel` — the path `save()` is about to write to — not the default, so the
    // header names the file that will actually exist (#1153 phase 4).
    this.setText(scaffoldWorkflowText(this.appVersion, this.rel));
    this.render();
    await this.save();
    // Land them on the canvas, looking at the thing they just made. Since #880 that is simply
    // the pane's normal state — the canvas is the primary surface and the inspector is beside
    // it — so all this has to do is make sure a YAML toggle left over from a previous file
    // isn't sitting in front of it.
    this.setSurface("canvas");
  }

  /** The file changed under us since we read it — an agent, git, or another editor. Same
   *  three-way choice the file editor offers, for the same reason: an agent rewriting the
   *  workflow it is running under is a real scenario here, not a hypothetical one. */
  private async resolveConflict(): Promise<void> {
    const root = this.root;
    if (!root) return; // unreachable: only a save can conflict, and a save needs a root
    const choice = await modal<ConflictChoice>((resolve) => ({
      title: "Workflow changed on disk",
      body: `${this.rel} was modified since you opened it (by an agent, another tool, or git). Overwrite it with your version, reload the on-disk version (losing your edits), or cancel?`,
      buttons: [
        { label: "Cancel", value: "cancel" },
        { label: "Reload", value: "reload" },
        { label: "Overwrite", value: "overwrite", kind: "danger" },
      ],
      onKey: (k) => (k === "Escape" ? resolve("cancel") : undefined),
    }));
    if (choice === "cancel") return;
    if (choice === "reload") {
      await this.load();
      return;
    }
    try {
      const res = await ftWriteFile(root, this.rel, this.text, null);
      this.savedText = this.text;
      this.savedHash = res.hash;
      this.exists = true;
      this.updateDirty();
      showToast("Overwrote on-disk changes");
    } catch (err) {
      showToast(`Save failed: ${errorMessage(err)}`);
    }
  }

  // ---------- the buffer ----------

  setText(text: string): void {
    this.text = text;
    this.yamlArea.value = text;
    this.reanalyze();
  }

  /** Write the model back into the buffer. EVERY form edit goes through here: the YAML is
   *  the source of truth, so a form edit is not "state the file will catch up with later"
   *  — it IS a file edit, immediately.
   *
   *  Comment-preserving, not a blind canonical rewrite (#233): `serializeWorkflowPreserving`
   *  reuses `this.text` — the buffer as it stood a moment ago — for every top-level piece the
   *  edit didn't touch, and only falls back to the canonical form for the piece that changed.
   *  That is what makes dragging one edge in a heavily-commented file a one-section diff
   *  instead of the whole file. */
  private commit(w: Workflow): void {
    this.setText(serializeWorkflowPreserving(w, this.text));
  }

  private reanalyze(): void {
    this.analysis = analyzeWorkflow(this.text, this.knobs.knobLookup);
    this.knobs.ensureCliKnobs();
  }

  /** The explicit "rewrite this whole file in canonical form" action — the one place left
   *  that drops comments on purpose, in one step, and the one place that still asks first
   *  (`confirmFormatRewrite`). Everyday form/canvas edits go through `commit()` instead, which
   *  preserves comments for whatever they didn't touch. */
  private async format(): Promise<void> {
    if (this.syntaxBroken()) {
      showToast("Fix the YAML syntax first — formatting a file we can't read would rewrite it wrong.");
      return;
    }
    const canonical = serializeWorkflow(this.analysis.workflow);
    if (!(await this.confirmFormatRewrite(canonical))) return;
    this.setText(canonical);
    this.render();
  }

  /** True while the text cannot be read at all. The form is disabled here — see the note
   *  at the top of the file: serializing a half-understood model back over the buffer
   *  would destroy the broken text the human is trying to fix.
   *
   *  `isUnreadable` (workflowtypes.ts) is the same predicate `serializeWorkflowPreserving`
   *  gates its own fallback on (#233 B3) — the two must agree, or a file this view still lets
   *  the human edit (e.g. `version: 2`, unsupported but readable) would silently full-rewrite
   *  on its very first edit for a reason never shown here. */
  syntaxBroken(): boolean {
    return isUnreadable(this.analysis.findings);
  }

  private updateDirty(): void {
    this.dirtyDot.hidden = !this.dirty;
    this.saveBtn.disabled = !this.dirty;
  }

  // ---------- render ----------

  /** Three states, and telling them apart is the fix for v2 bug 1:
   *
   *    ERROR — the file is THERE and we cannot read it. Say why; offer Retry; offer NOTHING
   *            that writes, because writing here means overwriting a file we refused to show.
   *    START — there is no file. The normal beginning of every repo, so this is a front door,
   *            not an apology: one line, one button, and the roster it is about to write.
   *    BODY  — a workflow. The roster, the form, the canvas, the YAML, the findings. */
  render(): void {
    // WHICH SURFACE is a rule, and it lives in `workflowpane.paneSurface` — pure, and tested.
    // The last time this view worked it out for itself, it showed "there is no workflow here"
    // for a file that was there and merely unreadable, and then offered to create one over it.
    const state = { loadError: this.loadError, exists: this.exists, text: this.text };
    const surface = paneSurface(state);
    const error = surface === "error";
    const start = surface === "start";
    this.errorEl.hidden = !error;
    this.errorTextEl.textContent = this.loadError ?? "";
    this.emptyEl.hidden = !start;
    this.bodyEl.hidden = error || start;
    this.findingsEl.hidden = error || start;
    // EVERY surface here names the file this pane is actually open on, not the default
    // one — including the Create button's tooltip, which used to be a static literal
    // naming `.loomux/workflow.yml` while the preview beside it read `.orrerix/...`
    // (#1153 phase 4, rev-lead round 1 B2). The empty state must never advertise the
    // deprecated spelling as the thing it is about to create.
    this.errorTitleEl.textContent = `Can't read ${this.rel}`;
    this.startPathEl.textContent = this.rel;
    this.starterBtn.title = `Scaffold a commented ${this.rel} — today's pipeline, ready to edit`;
    // Pressability is the RULE, not a side-effect of being on screen. `hidden` is now honoured
    // (styles.css `[hidden]`), so this is belt and braces — but it is the belt that matters: the
    // live bug was a create button the human could press over a loaded workflow, and the thing
    // that made it pressable was a stylesheet. A `disabled` that follows the same decision as the
    // surface cannot be undone by one.
    this.starterBtn.disabled = !createAllowed(state);
    this.yamlArea.value = this.text;
    this.updateDirty();
    if (error || start) {
      this.statusEl.textContent = "";
      this.statusEl.className = "wf-status";
      return;
    }
    this.renderSelection();
    this.renderFindings();
    this.applySurface();
  }

  /** Switch the primary surface. There is no `renderInspector()` here on purpose: the inspector
   *  is docked beside BOTH surfaces, so switching one does not change what it is showing — which
   *  is the whole reason the tabs went. */
  setSurface(surface: Surface): void {
    this.surface = surface;
    this.applySurface();
    if (surface === "yaml") this.yamlArea.focus();
  }

  private applySurface(): void {
    this.graphPane.hidden = this.surface !== "canvas";
    this.yamlPane.hidden = this.surface !== "yaml";
    this.yamlBtn.classList.toggle("active", this.surface === "yaml");
    this.yamlBtn.setAttribute("aria-pressed", String(this.surface === "yaml"));
  }

  /** The roster: the workflow itself, each block, and the gate — one column, one click to
   *  the form for any of them. A block with an ERROR carries a marker here, so a broken
   *  block is visible without opening it. */
  renderRoster(): void {
    const w = this.analysis.workflow;
    const rows: HTMLElement[] = [];

    const row = (sel: Selection, title: string, sub: string, bad: boolean): HTMLElement => {
      const r = el("button", "wf-row");
      const cur = this.selection;
      const active =
        cur.kind === sel.kind &&
        (sel.kind !== "block" || (cur as { index: number }).index === sel.index);
      r.classList.toggle("active", active);
      const main = el("span", "wf-row-main", title);
      const meta = el("span", "wf-row-sub", sub);
      r.append(main, meta);
      if (bad) r.append(el("span", "wf-row-bad", "!"));
      r.addEventListener("click", () => this.selectItem(sel));
      return r;
    };

    rows.push(el("div", "wf-roster-head", "Workflow"));
    rows.push(row({ kind: "workflow" }, w.name || "(unnamed)", `version ${w.version}`, false));

    rows.push(el("div", "wf-roster-head", "Blocks"));
    w.blocks.forEach((b, i) => {
      const bad = this.blockFindings(b).some((f) => f.severity === "error");
      // #687: a pinned thinking level / context window is part of what this block
      // will actually run, so the row says so. Unpinned adds nothing — the row
      // stays the line it has always been.
      const knobs = `${b.effort ? ` · effort: ${b.effort}` : ""}${b.context ? ` · context: ${b.context}` : ""}`;
      rows.push(
        row(
          { kind: "block", index: i },
          b.name || b.id || "(no id)",
          `${b.kind || "?"} · ${b.cli || "?"}${knobs}`,
          bad
        )
      );
    });

    const add = el("button", "wf-add", "+ Add block");
    add.addEventListener("click", () => void this.createBlock());
    (add as HTMLButtonElement).disabled = this.syntaxBroken();
    rows.push(add);

    rows.push(el("div", "wf-roster-head", "Gate"));
    const gate = w.gates.merge;
    const gateBad = this.analysis.findings.some((f) => f.code.startsWith("gate-"));
    rows.push(
      row(
        { kind: "gate" },
        "Merge",
        gate ? `${gate.require} · ${gate.reviewers.length} reviewer(s)` : "none — any review merges",
        gateBad
      )
    );

    // The three OPTIONAL policy sections (#1020), beside the gate for the same reason the gate
    // is beside the blocks: they are edited the same way, and a second place to click would be
    // a second place to look. Each sub-line answers the one question that matters about an
    // optional section — does this FILE say anything, or is loomux's own default in force? —
    // because a form full of empty fields cannot distinguish those two by itself.
    rows.push(el("div", "wf-roster-head", "Policy"));
    const intake = w.intake;
    const declaredLabels = intake?.labels
      ? Object.keys(intake.labels).filter((k) => k !== "extra").length
      : 0;
    rows.push(
      row(
        { kind: "intake" },
        "Intake",
        intake
          ? `${intake.source || "inherited source"}${declaredLabels ? ` · ${declaredLabels} label(s)` : ""}`
          : "not declared — orrerix's default",
        this.sectionBad("intake")
      )
    );
    const mq = w.merge_queue;
    rows.push(
      row(
        { kind: "merge_queue" },
        "Merge queue",
        mq
          ? `${mq.enabled ? "on" : "off"}${mq.max_batch !== undefined ? ` · batch ${mq.max_batch}` : ""}`
          : "not declared — off",
        this.sectionBad("merge_queue")
      )
    );
    // `driver:` (#1778) has no form yet — like every other field on the
    // pending list in `test/workflowschema.test.ts` — but the block is real
    // policy, and a declared block that is invisible in the designer is worse
    // than either extreme. The row and its read-only summary are the whole
    // chrome it gets for now.
    const dv = w.driver;
    rows.push(
      row(
        { kind: "driver" },
        "Review driver",
        dv
          ? `${dv.enabled ? "on" : "off"} · rounds ${dv.max_review_rounds ?? DRIVER_DEFAULTS.max_review_rounds} · ci ${dv.max_ci_attempts ?? DRIVER_DEFAULTS.max_ci_attempts}`
          : "not declared — off",
        this.sectionBad("driver")
      )
    );
    const resourceCount = Object.keys(w.resources ?? {}).length;
    rows.push(
      row(
        { kind: "resources" },
        "Resources",
        w.resources ? `${resourceCount} resource(s)` : "not declared — no locks",
        this.sectionBad("resources")
      )
    );

    this.rosterEl.replaceChildren(...rows);
  }

  /** Does this policy section carry an ERROR? Routed by the finding's own `section` rather
   *  than by matching its message, so a reworded message can never quietly stop marking the
   *  row it is about. */
  private sectionBad(section: FindingSection): boolean {
    return this.sectionFindings(section).some((f) => f.severity === "error");
  }

  sectionFindings(section: FindingSection): Finding[] {
    return this.analysis.findings.filter((f) => f.section === section);
  }

  /** The findings about ONE block row. A finding names a block by ID, because that is what
   *  a human reads — so an id-LESS stub takes the id-less findings ("a block has no id"),
   *  and where there are two such stubs they each show it. That is not a compromise: the
   *  finding is the same finding, and it is true of both. */
  blockFindings(b: WorkflowBlock): Finding[] {
    return this.analysis.findings.filter((f) => f.blockId === (b.id || ""));
  }

  /** Point the pane at something — from the roster, the canvas, or a finding. ONE path, because
   *  the bug #880 is about was two paths that were supposed to agree and didn't: the gate box
   *  remembered to bring the editor into view and the node handler didn't, so clicking a block
   *  looked like a dead click. Every selecting gesture now goes through here, and the three
   *  surfaces that show a selection are refreshed together or not at all. */
  selectItem(sel: Selection): void {
    this.selection = sel;
    this.renderSelection();
  }

  /** Re-render the three surfaces that DISPLAY the selection, in the one order that keeps them
   *  agreeing with each other.
   *
   *  THE INSPECTOR GOES FIRST, and that is the whole reason this is a method rather than three
   *  calls at four call sites. `renderInspector` is the render that NORMALIZES the selection —
   *  it asks `inspectorTarget` what is actually still there and adopts the answer — while the
   *  roster and the canvas merely *highlight* whatever `this.selection` currently says. Render
   *  them first and a stale selection lights nothing at all: select the last block, let an agent
   *  rewrite `workflow.yml` without it, press Reload, and the roster draws with an index no row
   *  answers to while the inspector then quietly falls back to the workflow's own settings. The
   *  two disagree until something else re-renders the roster.
   *
   *  It was written correctly in `mutate` and open-coded the wrong way round in the other three
   *  places, which is the argument for stating it once: an ordering rule that lives in a comment
   *  next to one of its four call sites is a rule the next three call sites will get wrong. */
  private renderSelection(): void {
    this.inspector.renderInspector();
    this.renderRoster();
    this.canvas.renderGraph();
  }

  /** Apply an edit to the model and write it straight back into the YAML.
   *
   *  `rerenderForm` is false for the free-text controls: re-rendering the inspector on every
   *  keystroke would rebuild the very input the human is typing into and drop the caret at
   *  its end. Structural edits (a kind change, an edge toggle, a persona switch) DO
   *  re-render, because they change which controls exist. */
  mutate(edit: (w: Workflow) => void, rerenderForm = true): void {
    const next: Workflow = structuredClone(this.analysis.workflow);
    edit(next);
    this.commit(next);
    if (rerenderForm) {
      this.renderSelection();
    } else {
      // The one path that may skip the inspector, and it is safe to: `rerenderForm` is false
      // only for the free-text controls (a name, a model, a prompt body), and typing in one can
      // never remove the block or edge that is selected. There is no stale selection for
      // `renderSelection`'s ordering rule to protect against here — only a caret to protect.
      this.renderRoster();
      this.canvas.renderGraph();
    }
    this.renderFindings();
    this.updateDirty();
  }

  /** Create a block — from the roster's "+ Add block" or the canvas's "+ Block", the same one
   *  path.
   *
   *  IT ASKS FOR THE ID, and that is a design commitment rather than a dialog I forgot to
   *  remove (§4): an id is immutable and human-meaningful, edges and gates reference it, and it
   *  is the thing you read in a diff. Dify mints `node_1720794829558`; n8n keys the graph by
   *  the DISPLAY NAME so a rename silently breaks every reference. Asking costs one dialog,
   *  once, and it is validated as they type — a malformed or duplicate id can't be confirmed at
   *  all, so it never becomes a finding they have to go and decode afterwards.
   *
   *  Everything ELSE about the block (kind, cli, model, prompt/profile) is configured in the
   *  property form, which the new block is immediately selected in. That split is deliberate:
   *  the id is the one field that can never be changed later, so it is the one field worth
   *  interrupting for. */
  async createBlock(at?: Point): Promise<void> {
    const w = this.analysis.workflow;
    const id = await promptModal({
      title: "New block",
      body: "The id is the block's identity — edges and the merge gate reference it, and it can never be changed. Make it something you'd want to read in a diff (rev-security, worker, planner).",
      label: "Block id",
      placeholder: "rev-security",
      affirm: "Create",
      validate: (v) => {
        if (!v) return "A block needs an id.";
        if (!isValidBlockId(v)) return "Use lowercase letters, digits, - and _ (e.g. rev-security).";
        if (w.blocks.some((b) => b.id === v)) return `This workflow already has a block called "${v}".`;
        return null;
      },
    });
    if (!id) return;

    const index = w.blocks.length;
    this.mutate((next) => {
      next.blocks = [...next.blocks, newBlock(id, id)];
    });
    // Put it where the human asked for it (a canvas right-click carries the point), or in the
    // first free slot. Either way it is placed BEFORE it is drawn, so it never flashes at the
    // origin on top of something else.
    this.layout = withPosition(this.layout, id, at ?? freeSlot(this.canvas.positions()));
    void this.saveLayout();
    this.selectItem({ kind: "block", index });
  }

  async deleteBlock(b: WorkflowBlock, index: number): Promise<void> {
    const refs = b.id
      ? this.analysis.workflow.edges.filter((e) => e.from === b.id || e.to === b.id).length
      : 0;
    const gated = (b.id && this.analysis.workflow.gates.merge?.reviewers.includes(b.id)) || false;
    const extra =
      refs || gated
        ? ` Its ${[refs ? `${refs} edge(s)` : "", gated ? "seat on the merge gate" : ""]
            .filter(Boolean)
            .join(" and ")} go with it.`
        : "";
    const ok = await modal<boolean>((resolve) => ({
      title: `Delete block "${b.name || b.id}"?`,
      body: `The block is removed from the workflow.${extra}`,
      buttons: [
        { label: "Cancel", value: false },
        { label: "Delete", value: true, kind: "danger" },
      ],
      onKey: (k) => (k === "Escape" ? resolve(false) : undefined),
    }));
    if (!ok) return;
    // removeBlockAt takes the references with it — a delete that left them behind would
    // turn one click into three validation errors.
    this.commit(removeBlockAt(this.analysis.workflow, index));
    this.selection = { kind: "workflow" };
    this.render();
  }

  // ---------- findings ----------

  renderFindings(): void {
    const findings = this.analysis.findings;
    const errors = findings.filter((f) => f.severity === "error").length;
    const warnings = findings.length - errors;
    this.statusEl.textContent = findings.length
      ? `${errors} error${errors === 1 ? "" : "s"}, ${warnings} warning${warnings === 1 ? "" : "s"}`
      : "valid";
    this.statusEl.className = `wf-status ${hasErrors(findings) ? "wf-error" : warnings ? "wf-warning" : "wf-ok"}`;

    if (!findings.length) {
      this.findingsEl.replaceChildren(
        el("div", "wf-finding wf-ok", "No problems found — every block, edge and gate reference resolves.")
      );
      return;
    }
    const rows = findings.map((f) => {
      const r = el("button", `wf-finding wf-${f.severity}`);
      const where = f.line ? `line ${f.line}` : f.blockId || f.section || "";
      if (where) r.append(el("span", "wf-finding-where", where));
      r.append(el("span", "wf-finding-msg", f.message));
      // Click a finding, land on the thing it is about — the whole value of a pre-run
      // validation pass is that it tells you WHERE.
      r.addEventListener("click", () => {
        // WHICH SURFACE a finding needs is a rule (`surfaceForFinding`): a line wants the caret,
        // which lives in the YAML; a block wants its editor, which — docked — is already on
        // screen, so switching surface would drag the human off the canvas for nothing.
        const surface = surfaceForFinding(f);
        if (surface) this.setSurface(surface);
        if (f.line) {
          this.focusLine(f.line);
          return;
        }
        // A policy-section finding names its own section, which IS a selection — so the
        // click lands on the form that can fix it, exactly like a block finding does.
        if (f.section) {
          this.selectItem({ kind: f.section });
          return;
        }
        // A finding names a block by id; the inspector is keyed by ROW. Land on the first row
        // that answers to that id — which for a duplicate pair is the first of the two,
        // and the duplication is reported on both, so the human sees the pair either way.
        const index = this.analysis.workflow.blocks.findIndex((b) => b.id === f.blockId);
        if (index < 0) return;
        this.selectItem({ kind: "block", index });
      });
      return r;
    });
    this.findingsEl.replaceChildren(...rows);
  }

  /** Put the caret on `line` in the YAML view — the follow-through a clickable line number
   *  promises. */
  private focusLine(line: number): void {
    const lines = this.text.split("\n");
    const at = lines.slice(0, line - 1).reduce((n, l) => n + l.length + 1, 0);
    this.yamlArea.focus();
    this.yamlArea.setSelectionRange(at, at + (lines[line - 1]?.length ?? 0));
  }
}
