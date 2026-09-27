// The workflow pane's docked INSPECTOR, which is its block editor, split out of
// workflowview.ts (#3498 F3): `renderInspector`, the form primitives every form is built from
// (`field`, `textInput`, `select`, `labelledSelect`, `boundedNumber`, `sectionToggle`), and the
// workflow, block and edge forms. The section forms it also shows (gate, intake, driver, merge
// queue, resources) are workflowsections.ts. Which form a selection gets is decided in
// workflowpane.ts's `inspectorTarget` (DOM-free, unit-tested).
//
// A satellite of `WorkflowView`, not a model: it owns the block form's two live repaint hooks
// and reads the rest of the pane through `WorkflowViewApi` (workflowviewapi.ts), never through
// workflowview.ts. It also exports `el`, the family's one element builder: the view and every
// sibling import it from here, because none of them may import workflowview.ts. DOM glue,
// hand-validated. Design note: docs/design/workflows.md; layout conventions:
// docs/design/module-layout.md.

import {
  isValidBlockId,
  roleHintsForKind,
  allowDenialReason,
  BLOCK_KINDS,
  WORKFLOW_CLIS,
  type FieldBounds,
  type Workflow,
  type WorkflowBlock,
  type FindingSection,
} from "./workflowmodel";
import { blockModelOptions } from "./modelcatalog";
import { modelCatalog } from "./modelprobe";
import { ModelPicker } from "./modelpicker";
import { BLOCK_DEFAULT_MODEL_LABEL } from "./modelnames";
import { BlockKnobFields } from "./workflowknobs";
import { inspectorTarget, inspectorHeading } from "./workflowpane";
import type { WorkflowViewApi, WorkflowInspectorApi } from "./workflowviewapi";

export function el(tag: string, cls: string, text?: string): HTMLElement {
  const e = document.createElement(tag);
  e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

export class WorkflowInspector implements WorkflowInspectorApi {
  /** Redraws the block form's two knob rows in place, or `null` when no block
   *  form is on screen. The form deliberately does not re-render on a model edit
   *  (it would rebuild the input under the caret), so the rows that depend on the
   *  model — and on a capability reply that lands whenever the IPC happens to
   *  resolve — need a way to be refreshed without one (#935). */
  repaintBlockKnobs: (() => void) | null = null;
  /** Rebuilds the block form's model dropdown as soon as doing so stops being
   *  destructive, or `null` when no block form is on screen (#1020).
   *
   *  The sibling of {@link repaintBlockKnobs}, and nulled by the same line for
   *  the same reason. A detection reply now arrives on the sweep's schedule
   *  rather than after a click, so "which picker is live, and is the human
   *  inside its custom-id box right now" is knowledge only the form has —
   *  it installs this, and `renderInspector` takes it away when those controls
   *  are detached. */
  refreshBlockModels: (() => void) | null = null;

  constructor(private readonly view: WorkflowViewApi) {}

  /** The docked inspector: a header naming what is selected, and the editor for it.
   *
   *  WHAT IS SHOWN is `inspectorTarget` (workflowpane.ts) — including the two ways a selection
   *  can outlive the thing it points at (a block deleted from under it, an edge erased) and the
   *  one state where nothing may be edited at all. This used to be a chain of inline checks that
   *  reassigned `this.view.selection` and re-entered itself; the reassignment is still needed — the
   *  roster highlights the SELECTION, so a fallback the roster never hears about would leave a
   *  stale row lit next to a different editor — but it happens once, here, from the answer. */
  renderInspector(): void {
    // Whatever the last form left here belongs to controls that are about to be
    // replaced. Cleared FIRST, so a late `agent_cli_knobs` reply can never paint
    // into a detached row (#935) — every path below ends in a `formPane` swap.
    // Same for the model dropdown's deferred rebuild (#1020): a detection reply
    // that lands after the human selected another block must not reach the
    // picker they left behind.
    this.repaintBlockKnobs = null;
    this.refreshBlockModels = null;
    const w = this.view.analysis.workflow;
    const target = inspectorTarget(this.view.selection, w, this.view.syntaxBroken());
    // Adopt the fallback so the roster and the canvas agree with the editor. Never while
    // `blocked`: that state is about the BUFFER, not the selection, and forgetting which block
    // the human was on because they typo'd a colon would be its own small insult.
    if (target.kind !== "blocked") this.view.selection = target;

    const heading = inspectorHeading(target, w);
    this.view.inspTitleEl.textContent = heading.title;
    this.view.inspTitleEl.title = heading.title;
    this.view.inspSubEl.textContent = heading.sub;

    if (target.kind === "blocked") {
      const warn = el(
        "div",
        "wf-blocked",
        "The YAML doesn't parse, so the editor is disabled — editing it here would rewrite the text you're fixing. " +
          "Fix the error below in the raw file and the editor comes back."
      );
      const toYaml = document.createElement("button");
      toYaml.className = "wf-btn";
      toYaml.textContent = "Edit the YAML";
      toYaml.addEventListener("click", () => this.view.setSurface("yaml"));
      warn.append(toYaml);
      this.view.formPane.replaceChildren(warn);
      return;
    }
    if (target.kind === "block") {
      this.view.formPane.replaceChildren(this.blockForm(w, w.blocks[target.index]!, target.index));
      return;
    }
    if (target.kind === "edge") {
      this.view.formPane.replaceChildren(this.edgeForm(target.from, target.to));
      return;
    }
    if (target.kind === "gate-edge") {
      this.view.formPane.replaceChildren(this.gateEdgeForm(target.reviewer));
      return;
    }
    if (target.kind === "intake") {
      this.view.formPane.replaceChildren(this.view.sections.intakeForm(w));
      return;
    }
    if (target.kind === "merge_queue") {
      this.view.formPane.replaceChildren(this.view.sections.mergeQueueForm(w));
      return;
    }
    if (target.kind === "driver") {
      this.view.formPane.replaceChildren(this.view.sections.driverForm(w));
      return;
    }
    if (target.kind === "resources") {
      this.view.formPane.replaceChildren(this.view.sections.resourcesForm(w));
      return;
    }
    this.view.formPane.replaceChildren(target.kind === "gate" ? this.view.sections.gateForm(w) : this.workflowForm(w));
  }

  // ---------- forms ----------

  field(label: string, control: HTMLElement, hint?: string): HTMLElement {
    const f = el("label", "wf-field");
    f.append(el("span", "wf-label", label), control);
    if (hint) f.append(el("span", "wf-hint", hint));
    return f;
  }

  textInput(value: string, onChange: (v: string) => void, placeholder = ""): HTMLInputElement {
    const i = document.createElement("input");
    i.className = "wf-input";
    i.type = "text";
    i.value = value;
    i.placeholder = placeholder;
    // `input`, not `change`: the file is the source of truth, so it should follow what the
    // human typed as they type it. The form is NOT re-rendered on these (that would move
    // the caret) — only the roster, the findings and the graph are.
    i.addEventListener("input", () => onChange(i.value));
    return i;
  }

  select(
    options: readonly string[],
    value: string,
    onChange: (v: string) => void
  ): HTMLSelectElement {
    const s = document.createElement("select");
    s.className = "wf-input";
    for (const o of options) {
      const opt = document.createElement("option");
      opt.value = o;
      opt.textContent = o;
      s.append(opt);
    }
    // A value the enum doesn't contain still SHOWS — as itself, marked. Dropping it would
    // silently rewrite the user's file to something they never chose the moment they
    // touched any other field on the block.
    if (value && !options.includes(value)) {
      const opt = document.createElement("option");
      opt.value = value;
      opt.textContent = `${value} (unknown)`;
      s.append(opt);
    }
    s.value = value;
    s.addEventListener("change", () => onChange(s.value));
    return s;
  }

  /** A select whose options have a LABEL distinct from their value — the shape every
   *  optional field here needs, because the empty value is a real choice ("inherit
   *  loomux's default") that has to read as one rather than as a blank row. The plain
   *  `select` above stays as it is: its values ARE their labels, which is right for a
   *  closed enum like `kind`. */
  labelledSelect(
    options: readonly { value: string; label: string }[],
    value: string,
    onChange: (v: string) => void
  ): HTMLSelectElement {
    const s = document.createElement("select");
    s.className = "wf-input";
    for (const o of options) {
      const opt = document.createElement("option");
      opt.value = o.value;
      opt.textContent = o.label;
      s.append(opt);
    }
    // Same rule as `select`: a value this build doesn't offer still SHOWS, marked, so that
    // touching another field can never silently rewrite it to something nobody chose.
    if (value && !options.some((o) => o.value === value)) {
      const opt = document.createElement("option");
      opt.value = value;
      opt.textContent = `${value} (unknown)`;
      s.append(opt);
    }
    s.value = value;
    s.addEventListener("change", () => onChange(s.value));
    return s;
  }

  /** A bounded whole-number field for the policy sections, or EMPTY for "loomux's default".
   *
   *  The bounds are the engine's own (`RESOURCE_SLOTS_MAX`, `RESOURCES_MAX`, … — mirrored in
   *  workflowtypes.ts), and they are enforced on the way into the MODEL rather than only as
   *  `min`/`max` attributes: a spinner's attributes are advisory, and a typed `9999` would
   *  otherwise be written into a file the engine then refuses to load. The clamp is shown
   *  back on blur, so it is never a value the human can't see. A hand-written out-of-range
   *  value still gets its finding — this stops the FORM from producing one. */
  boundedNumber(
    value: number | undefined,
    bounds: FieldBounds,
    onChange: (v: number | undefined) => void,
    placeholder = "orrerix's default"
  ): HTMLInputElement {
    const i = document.createElement("input");
    i.className = "wf-input";
    i.type = "number";
    i.min = String(bounds.min);
    // NO `max` attribute where the schema declares no ceiling. An absent `max` in
    // `POLICY_BOUNDS` is a statement — the engine accepts anything above the floor — and a
    // form that invented one would rewrite a legal `max_batch: 100` to whatever it made up
    // (#1020 review, finding 2). The floor is real everywhere, so it is always applied.
    if (bounds.max !== undefined) i.max = String(bounds.max);
    i.value = value === undefined ? "" : String(value);
    i.placeholder = placeholder;
    const clamp = (n: number): number => {
      const atLeast = Math.max(bounds.min, Math.round(n));
      return bounds.max === undefined ? atLeast : Math.min(bounds.max, atLeast);
    };
    i.addEventListener("input", () => {
      const raw = i.value.trim();
      if (!raw) {
        onChange(undefined);
        return;
      }
      const n = Number(raw);
      if (!Number.isFinite(n)) return; // a half-typed "-" or "e" — wait for the rest
      onChange(clamp(n));
    });
    // Show the clamp once they stop typing. Doing it on `input` would fight the caret of
    // someone typing "480" one digit at a time (the "4" would become the minimum).
    i.addEventListener("change", () => {
      const raw = i.value.trim();
      if (!raw) return;
      const n = Number(raw);
      if (Number.isFinite(n)) i.value = String(clamp(n));
    });
    return i;
  }

  /** The enable-toggle every optional section is edited through, and the reason all three
   *  forms are shaped like `gateForm`: the checkbox IS the section's presence in the file.
   *
   *  Off writes nothing at all — not `enabled: false`, not a block of defaults — because the
   *  model emits only what is declared, so an untouched (or re-untouched) section leaves the
   *  file exactly as it found it. That is the property a human relies on when they open this
   *  form to look rather than to change something. */
  sectionToggle(label: string, on: boolean, onChange: (on: boolean) => void): HTMLElement {
    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.checked = on;
    cb.addEventListener("change", () => onChange(cb.checked));
    const line = el("label", "wf-check");
    line.append(cb, el("span", "wf-check-label", label));
    return line;
  }

  /** The findings for one policy section, rendered inline under its form — the same
   *  treatment `blockForm` gives a block's own findings, and for the same reason: the
   *  place to say what is wrong with a value is beside the field that sets it. */
  sectionFindingList(section: FindingSection): HTMLElement | null {
    const found = this.view.sectionFindings(section);
    if (!found.length) return null;
    const list = el("ul", "wf-inline-findings");
    for (const f of found) list.append(el("li", `wf-finding wf-${f.severity}`, f.message));
    return list;
  }

  private workflowForm(w: Workflow): HTMLElement {
    const box = el("div", "wf-fields");
    box.append(
      this.field(
        "Name",
        this.textInput(w.name, (v) => {
          this.view.mutate((next) => {
            next.name = v;
          }, false);
        }),
        "Names the workflow in the audit record. Display only."
      )
    );
    const version = document.createElement("input");
    version.className = "wf-input";
    version.value = String(w.version);
    version.disabled = true;
    box.append(this.field("Schema version", version, "Set by orrerix; a newer version needs a newer build."));
    box.append(
      el(
        "p",
        "wf-note",
        "Edges are ADVISORY — they declare the intended path; the orchestrator still decides when to spawn what. " +
          "The merge gate is ENFORCED: orrerix refuses `gh pr merge` until every reviewer it names has recorded a PASS."
      )
    );
    return box;
  }

  private blockForm(w: Workflow, b: WorkflowBlock, index: number): HTMLElement {
    const box = el("div", "wf-fields");

    /** Edit THIS row, by index. Never by id: the rows that most need editing are the ones
     *  whose id is missing or duplicated, and an id lookup would edit the wrong one. */
    const edit = (f: (t: WorkflowBlock) => void, rerenderForm = true): void =>
      this.view.mutate((next) => {
        const t = next.blocks[index];
        if (t) f(t);
      }, rerenderForm);

    // The id is IMMUTABLE — once it is a usable identity. An id that is missing, malformed
    // or duplicated is not one: nothing can legally reference it, so nothing breaks when it
    // changes, and locking the field would leave the human staring at a validation error
    // with no way to fix the thing it is about (in the form, which is where they are). So
    // the field is editable in exactly the case where immutability protects nothing.
    const dupe = w.blocks.filter((x) => x.id === b.id).length > 1;
    const fixable = !b.id || !isValidBlockId(b.id) || dupe;
    const idInput = this.textInput(b.id, (v) => edit((t) => (t.id = v), false));
    idInput.disabled = !fixable;
    box.append(
      this.field(
        "Id",
        idInput,
        fixable
          ? "This id isn't usable yet, so it can still be set. Once it is valid and unique it becomes immutable — edges and the gate reference it."
          : "Immutable. Edges and the gate reference this id — renaming it would break them silently (the n8n bug)."
      )
    );

    box.append(
      this.field(
        "Name",
        this.textInput(b.name, (v) => edit((t) => (t.name = v), false)),
        "Display only — safe to rename at any time."
      )
    );

    box.append(
      this.field(
        "Kind",
        this.select(BLOCK_KINDS, b.kind, (v) => edit((t) => (t.kind = v))),
        "The capability class. A workflow defines personas, never capabilities: a planner is read-only, " +
          "a reviewer can never push, a worker gets a worktree."
      )
    );

    // The role hint (#250/#324): a persona/template/badge MARKER, never a capability — and
    // the offer is DERIVED from the same pairing rule the validator applies
    // (`roleHintsForKind`), so this picker cannot spell a combination the parser rejects, and
    // a hint added to the model shows up here without an edit. A block already declaring one
    // its kind can't carry still shows it, marked, because that is the finding it needs to fix.
    const hints = roleHintsForKind(b.kind);
    box.append(
      this.field(
        "Role hint",
        this.labelledSelect(
          [{ value: "", label: "none" }, ...hints.map((h) => ({ value: h, label: h }))],
          b.role_hint ?? "",
          (v) =>
            edit((t) => {
              if (v) t.role_hint = v;
              else delete t.role_hint;
            })
        ),
        hints.length
          ? "Optional and INERT: it picks a persona/template fragment and a badge. Capability still comes from kind alone."
          : `No role hint applies to a ${b.kind || "block"} — each hint requires the one kind it is meaningless without.`
      )
    );

    box.append(
      this.field("Agent CLI", this.select(WORKFLOW_CLIS, b.cli, (v) => edit((t) => (t.cli = v))))
    );

    // The model field is the SAME control the launcher renders (#935): one
    // dropdown, one catalog — the CLI's own reported models merged over this
    // repo's curated suggestions — with the `custom…` escape that keeps it a
    // wider field than the free-text box it replaces, not a narrower one. A CLI
    // this repo has no curated row for (`gemini`) is still probed like any
    // other — it is the REPLY that carries nothing today — and with nothing on
    // either side the picker opens straight onto that custom input.
    const cli = b.cli.trim();
    const repaint = (): void => {
      const now = this.view.analysis.workflow.blocks[index]?.model ?? picker.value;
      picker.setOptions(blockModelOptions(modelCatalog.models(cli)), now, cli);
    };
    const picker = new ModelPicker({
      selectClass: "wf-input",
      inputClass: "wf-input",
      placeholder: "model id…",
      blankLabel: BLOCK_DEFAULT_MODEL_LABEL,
      // #993. The lookup is live rather than a snapshot: the catalog's answer
      // can arrive after this control was built, and a picker holding a copy
      // taken at construction would show the old one forever.
      detailFor: (id) => modelCatalog.detail(cli, id),
    });
    repaint();
    box.append(
      this.field(
        "Model",
        picker.root,
        "The CLI's own list, merged over orrerix's suggestions — or type any id (a Bedrock " +
          "profile, a gateway deployment, a model newer than this build). Unset leaves it to orrerix."
      )
    );

    // The two model knobs (#687). Their VALUES and their availability come from
    // the backend's capability row for this block's CLI (`agent_cli_knobs`) — the
    // pane states no vendor fact of its own — narrowed by what the selected model
    // can carry. A knob this CLI/model cannot take renders disabled with that
    // reason as the hint, which is also the finding the validation pass raises if
    // the file declares one anyway.
    const knobs = new BlockKnobFields(this.view.knobs.knobLookup, cli, b.model, b);
    const effortRow = this.view.knobs.knobRow("Thinking level", knobs.effort, (v) =>
      edit((t) => {
        if (v) t.effort = v;
        else delete t.effort;
      })
    );
    const contextRow = this.view.knobs.knobRow("Context window", knobs.context, (v) =>
      edit((t) => {
        if (v) t.context = v;
        else delete t.context;
      })
    );
    box.append(effortRow.field, contextRow.field);

    /** Redraw the knob rows from whatever the model and the capability record now
     *  say — the repaint that a form which must not re-render still owes them. */
    const repaintKnobs = (): void => {
      effortRow.paint(knobs.effort);
      contextRow.paint(knobs.context);
    };
    this.repaintBlockKnobs = repaintKnobs;
    // The menu's half of the same contract (#1020). Deferred past the mid-type
    // window rather than dropped — rebuilding under a half-typed id resolves it
    // to the dropdown branch and hides the input beneath the caret (#997 review
    // NB-3) — and installed as a LIVE hook so `renderInspector` can take it
    // away when these controls are detached.
    this.refreshBlockModels = () => picker.runWhenNotEditing(repaint);

    // Fires for a dropdown pick AND for every keystroke in the `custom…` box.
    // The keystroke is the case that was broken: `context` is only offered where
    // the selected model has a documented `[1m]` form, so typing a model over one
    // that has none (or vice versa) has to re-derive the knob — and a `change`
    // listener on the select alone never sees a typed id at all.
    //
    // `rerenderForm: false`, like every other free-text control here: rebuilding
    // the form on a keystroke would rebuild the input the human is typing into
    // and drop the caret at its end. That suppression is exactly why the repaint
    // has to be explicit.
    picker.onChange = () => {
      const model = picker.value;
      edit((t) => (t.model = model), false);
      knobs.setModel(model);
      repaintKnobs();
    };

    // What the CLI on THIS machine reports, once it answers. Only re-set when it
    // reported something — re-setting an identical list would rebuild the menu
    // for no gain — and only while this form is still the one on screen. The
    // fallback is re-read from the MODEL rather than closed over from `b`: by the
    // time this lands the human may have chosen the blank row, and a stale
    // `b.model` would re-select the id they just cleared.
    if (cli) {
      void this.view.knobs.probeModels(cli).then((p) => {
        if (!p.models.length || !this.view.formPane.contains(picker.root)) return;
        // Never under the caret. `setOptions` re-runs `pickerSelection`, and an
        // id the probe turns out to carry resolves to the DROPDOWN branch —
        // which hides the custom input being typed into, sending the rest of the
        // keystrokes nowhere. The pane takes the same care with the capability
        // reply (`ensureCliKnobs`), and for the same reason. The menu is not
        // lost: the next form render paints it from the resolved catalog.
        if (picker.root.contains(document.activeElement)) return;
        const now = this.view.analysis.workflow.blocks[index]?.model ?? "";
        picker.setOptions(blockModelOptions(modelCatalog.models(cli)), now, cli);
      });
      // And the detection LOOKUP (#1020) — fired from this render path, which
      // #993 forbade and this slice makes correct: it cannot spawn an agent CLI,
      // because the backend swept them once at startup and this reads what it
      // left (`src-tauri/src/modelwire.rs`).
      //
      // **Guarded on `report(cli)` being absent, and that guard is what makes it
      // terminate.** `applyDetection` ends in `renderInspector()`, which rebuilds
      // this form and re-runs this line: without the guard, every reply would
      // re-enter the render it was answering. A reply worth having sets
      // `report(cli)`, so the rebuilt form skips this; one that carries nothing
      // returns below before refreshing anything. Both routes out are dead ends,
      // which is the property to check if this ever grows a third.
      if (!modelCatalog.report(cli)) {
        void modelCatalog.detect(cli).then((r) => {
          if (!r.models.length) return;
          this.view.knobs.applyDetection(cli);
        });
      }
    }

    // Persona: inline prompt, a profile file, or neither (the built-in role template).
    // Exactly one, enforced here rather than only reported: the two compile to different
    // native flags (`claude --agents '<json>'` inline vs `copilot --agent <name>`), so a
    // block with both has no single answer.
    const personaKind: "none" | "prompt" | "profile" =
      b.prompt !== undefined ? "prompt" : b.profile !== undefined ? "profile" : "none";
    box.append(
      this.field(
        "Persona",
        this.select(["none", "prompt", "profile"], personaKind, (v) =>
          edit((t) => {
            delete t.prompt;
            delete t.profile;
            if (v === "prompt") t.prompt = b.prompt ?? "";
            if (v === "profile") t.profile = b.profile ?? "";
          })
        ),
        "none = orrerix's built-in role instructions. prompt = inline (compiled to the CLI's native inline agent). " +
          "profile = a .github/agents/*.md file (Copilot's native --agent)."
      )
    );

    if (personaKind === "prompt") {
      const ta = document.createElement("textarea");
      ta.className = "wf-input wf-textarea";
      ta.value = b.prompt ?? "";
      ta.spellcheck = false;
      ta.rows = 8;
      ta.addEventListener("input", () => edit((t) => (t.prompt = ta.value), false));
      box.append(
        this.field(
          "Prompt",
          ta,
          "Appended to the role's mechanics — it cannot drop the report/git/MCP contract."
        )
      );
    }
    if (personaKind === "profile") {
      box.append(
        this.field(
          "Profile path",
          this.textInput(
            b.profile ?? "",
            (v) => edit((t) => (t.profile = v), false),
            ".github/agents/reviewer.md"
          ),
          "Repo-relative. A Copilot block launches with --agent <name> resolved from this file."
        )
      );
    }

    // `allow:` — extra pre-approved tool patterns (#222), a tag list rather than one
    // comma-separated field for a reason that would otherwise corrupt the value: a real
    // pattern CONTAINS commas (`Bash(gh pr view --json title,body)`), so a comma cannot also
    // be the separator. One row per pattern, and the row is the whole editor for it.
    //
    // It is RESTRICT-ONLY, and the form says so out loud: deny beats allow on both CLIs, so a
    // pattern here can never re-grant something loomux's containment took away — it only
    // pre-approves something the block could already have been asked to approve. That is why
    // the two kinds that may not declare it at all (the orchestrator, and the read-only class)
    // are refused rather than merely warned.
    // THE ROWS ARE LOCAL; the FILE is what is left when the empty ones are dropped.
    //
    // That one rule replaces the draft-row special case the first cut had, and closes the
    // hole it left (#1020 review, finding 5): a *committed* row cleared with select-all-
    // delete wrote `allow: [""]` and then raised the "dropped, and pre-approves nothing"
    // warning about it — the pane complaining about its own keystroke, which is exactly
    // what the draft row existed to avoid, reached from the other direction. An empty row
    // is now a row you are in the middle of typing, wherever it came from, and it reaches
    // the file only once it has something in it.
    const denial = allowDenialReason(b.kind);
    const rows: string[] = [...(b.allow ?? [])];
    const allowList = el("div", "wf-checks");

    /** Write the non-empty rows, in order. The key goes entirely when nothing is left: an
     *  `allow: []` is a line that declares nothing, and the model emits only what is
     *  declared. `rerenderForm: false` — this runs on every keystroke. */
    const commitRows = (): void =>
      edit((t) => {
        const kept = rows.filter((p) => p.trim() !== "");
        if (kept.length) t.allow = kept;
        else delete t.allow;
      }, false);

    /** Rebuild the row DOM from `rows`. Only ever called from add/remove — deliberate
     *  clicks, with no caret to protect — so the indices every row closure captures are
     *  rebuilt at exactly the moments they would otherwise go stale. A keystroke mutates
     *  `rows[i]` in place and repaints nothing. */
    const paintRows = (): void => {
      const built = rows.map((value, i) => {
        const line = el("div", "wf-check");
        const input = this.textInput(
          value,
          (v) => {
            rows[i] = v;
            commitRows();
          },
          "Bash(npm test *)"
        );
        const del = document.createElement("button");
        del.className = "wf-btn wf-btn-danger";
        del.textContent = "✕";
        del.title = "Remove this pattern";
        del.addEventListener("click", () => {
          rows.splice(i, 1);
          commitRows();
          paintRows();
        });
        line.append(input, del);
        return line;
      });
      const addPattern = el("button", "wf-add", "+ Add pattern") as HTMLButtonElement;
      addPattern.disabled = !!denial;
      addPattern.addEventListener("click", () => {
        rows.push("");
        paintRows();
        // Focus the row just added — the point of pressing the button is to type in it.
        const inputs = allowList.querySelectorAll<HTMLInputElement>("input.wf-input");
        inputs[inputs.length - 1]?.focus();
      });
      const children: HTMLElement[] = [...built, addPattern];
      if (!rows.length && !denial) {
        children.push(
          el("span", "wf-hint", "None — the block runs with its class's own tool surface.")
        );
      }
      allowList.replaceChildren(...children);
    };
    paintRows();
    box.append(
      this.field(
        "Extra allowed tools",
        allowList,
        denial
          ? `A ${b.kind} block may not declare allow: — ${denial}.`
          : "Pre-approved tool patterns, passed to the CLI's own --allowedTools/--allow-tool. " +
              "RESTRICT-ONLY: deny beats allow on both CLIs, so this can never re-grant what the " +
              "block's kind takes away. orrerix passes only letters, digits and ( ) : * _ - . / , and spaces."
      )
    );

    // Outgoing edges, edited as "what runs after this" — the honest phrasing for an
    // advisory edge, and the only edge editing the form needs: every edge has a source.
    const targets = el("div", "wf-checks");
    if (!b.id) {
      // An edge is a pair of IDS. A block without one cannot be an endpoint, and offering
      // checkboxes that would write `from: ""` would manufacture the dangling references
      // this pane exists to catch.
      targets.append(el("span", "wf-hint", "Give this block an id before wiring edges to it."));
    } else {
      for (const other of w.blocks) {
        if (other.id === b.id || !other.id) continue;
        const line = el("label", "wf-check");
        const cb = document.createElement("input");
        cb.type = "checkbox";
        cb.checked = w.edges.some((e) => e.from === b.id && e.to === other.id);
        cb.addEventListener("change", () =>
          this.view.mutate((next) => {
            next.edges = cb.checked
              ? [...next.edges, { from: b.id, to: other.id }]
              : next.edges.filter((e) => !(e.from === b.id && e.to === other.id));
          })
        );
        line.append(cb, el("span", "wf-check-label", `${other.name || other.id} (${other.id})`));
        targets.append(line);
      }
      if (!targets.children.length) {
        targets.append(el("span", "wf-hint", "Add another block to draw an edge."));
      }
    }
    box.append(
      this.field("Then run", targets, "Advisory: the declared happy path. The orchestrator still schedules.")
    );

    const inline = this.view.blockFindings(b);
    if (inline.length) {
      const list = el("ul", "wf-inline-findings");
      for (const f of inline) list.append(el("li", `wf-finding wf-${f.severity}`, f.message));
      box.append(list);
    }

    const del = document.createElement("button");
    del.className = "wf-btn wf-btn-danger";
    del.textContent = "Delete block";
    del.addEventListener("click", () => void this.view.deleteBlock(b, index));
    box.append(del);
    return box;
  }

  /** The panel for a selected EDGE. Short, because an edge is a short thing: it has no
   *  properties — it is a pair of ids — so all there is to say is what it means and how to
   *  remove it. Saying *what it means* is the part that earns the panel: this is the one place
   *  a human clicks on an advisory edge, and it is where they should learn that it is advisory. */
  private edgeForm(from: string, to: string): HTMLElement {
    const box = el("div", "wf-fields");
    box.append(
      el(
        "p",
        "wf-note",
        "An ADVISORY edge: it declares the intended path. The orchestrator still decides when to " +
          "spawn what — its judgment about what can run in parallel is the thing that makes it good, " +
          "and a static DAG would replace that with something dumber. The half that is actually " +
          "enforced is the merge gate."
      )
    );
    const del = document.createElement("button");
    del.className = "wf-btn wf-btn-danger";
    del.textContent = "Delete edge";
    del.addEventListener("click", () => this.view.canvas.eraseEdge(from, to));
    box.append(del);
    return box;
  }

  /** The panel for one reviewer's SEAT on the merge gate. The mirror of `edgeForm`, and it
   *  earns its own text for the same reason that one does: this is the one place a human
   *  clicks on an amber line, and it is where they should learn that this line — unlike the
   *  solid one — is the half that actually stops a merge. */
  private gateEdgeForm(reviewer: string): HTMLElement {
    const box = el("div", "wf-fields");
    const gate = this.view.analysis.workflow.gates.merge;
    box.append(
      el(
        "p",
        "wf-note",
        `An ENFORCED seat on the merge gate: orrerix's \`gh\` shim refuses \`gh pr merge\` until ` +
          `"${reviewer}" has recorded a PASS on the commit being merged. Unlike an advisory edge, ` +
          `this one is not a hint to the orchestrator — it is a rule about the merge itself.`
      )
    );
    if (gate?.require === "threshold") {
      box.append(
        el(
          "p",
          "wf-hint",
          `This gate needs ${gate.threshold ?? "?"} of its ${gate.reviewers.length} reviewer(s) to ` +
            `pass. Removing a seat lowers the threshold if it would otherwise ask for more passes ` +
            `than the gate names reviewers — which is a file the engine refuses outright.`
        )
      );
    }
    const del = document.createElement("button");
    del.className = "wf-btn wf-btn-danger";
    del.textContent = "Remove from gate";
    del.addEventListener("click", () => this.view.canvas.eraseGateEdge(reviewer));
    box.append(del);
    return box;
  }
}
