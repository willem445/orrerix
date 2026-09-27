// The workflow pane's model-KNOB plumbing, split out of workflowview.ts (#3498 F3): what each
// CLI can do with the effort/context knobs as the backend reports it (`agent_cli_knobs`,
// #687), the per-pane model probes (#935), the one detection funnel both detection routes
// share (`applyDetection`, #993/#1020), the `knobLookup` the validation pass asks, and the
// block form's knob row.
//
// A satellite of `WorkflowView`, not a model: it owns the capability state and reads the rest
// of the pane through `WorkflowViewApi` (workflowviewapi.ts), never through workflowview.ts.
// What a knob row SHOWS is decided in workflowknobs.ts (DOM-free, unit-tested); this is the
// state and the DOM half. The detection funnel's wiring is pinned by source scans in
// test/detectrefresh.test.ts, which read this file. Design note: docs/design/model-catalog.md;
// layout conventions: docs/design/module-layout.md.

import { analyzeWorkflow } from "./workflowmodel";
import { agentCliKnobs } from "./pty";
import { knobState, type CliKnobs, type KnobStates } from "./selectorknobs";
import type { CliProbe } from "./modelcatalog";
import { modelCatalog } from "./modelprobe";
import type { KnobFieldSpec } from "./workflowknobs";
import { el } from "./workflowinspector";
import type { WorkflowViewApi, WorkflowCliKnobsApi } from "./workflowviewapi";

export class WorkflowCliKnobs implements WorkflowCliKnobsApi {
  /** What each CLI can do with the model knobs (#687), as the BACKEND reports it
   *  (`agent_cli_knobs`): `undefined` = not asked yet, `null` = asked and the
   *  lookup failed, a record = the answer. The pane never mirrors a capability
   *  of its own — see `knobLookup`. */
  private cliKnobs = new Map<string, CliKnobs | null>();
  /** CLIs already asked about, so a re-analysis per keystroke is not a fetch per
   *  keystroke. Separate from `cliKnobs` because "asked, still in flight" and
   *  "asked, failed" are different states and only one of them is answerable. */
  private knobsAsked = new Set<string>();
  /** One model probe per CLI per PANE — deliberately not per paint, and not once
   *  per app run either.
   *
   *  Not per paint: the block form re-renders on every knob edit, and the catalog
   *  no longer keeps an answer that carried nothing (`worthKeeping`), so probing
   *  from the render path would be a subprocess per paint for exactly the CLIs
   *  that have no answer to give.
   *
   *  Not once per app run: per pane IS the recovery granularity the app-wide memo
   *  would otherwise cost. Install a CLI mid-session, open a workflow pane, and it
   *  is asked again — which is what the pre-#935 per-form memo gave for free.
   *
   *  The PROMISE is what's held, not an "already asked" flag, so a second block
   *  form painted while the first probe is still in flight still gets its
   *  re-set. */
  private modelProbes = new Map<string, Promise<CliProbe>>();
  /** CLIs a detection reply has already been applied to this pane for (#1020).
   *
   *  The push and the pull are two deliveries of ONE sweep answer and both can
   *  land — the lookup's `.then` is already attached when the event arrives, and
   *  neither call site can tell that the other got there first. Deduping in the
   *  funnel they share, rather than at each call site, is what makes "the two
   *  routes must not repaint a form twice" hold for BOTH orderings instead of
   *  the one a guard captured at paint time covers (rev-713 non-blocking 3). */
  private detectionsApplied = new Set<string>();

  /** The capability answer the model's validation pass asks for (#687).
   *
   *  `undefined` in the map = not fetched yet, and the lookup returns `null` for
   *  it — NOT an answer (see `KnobLookup`), so the pass defers instead of
   *  inventing a finding out of its own ignorance. `null` in the map = we asked
   *  and the call failed, which `knobState` renders as disabled-with-a-reason. */
  knobLookup = (cli: string, model: string): KnobStates | null => {
    const caps = this.cliKnobs.get(cli);
    // #993: the detected per-model levels narrow the CLI's general set. The
    // validation pass reads the same lookup the editor's controls do, so a
    // block cannot be flagged for a level the picker was still offering.
    return caps === undefined ? null : knobState(caps, cli, model, modelCatalog.detail(cli, model));
  };

  constructor(private readonly view: WorkflowViewApi) {}

  /** Everything a list-models reply owes this pane (#993, #1020).
   *
   *  **A detection reply owes every surface `agent_cli_knobs` owes.** It is
   *  precisely the answer that makes {@link knobLookup} respond differently, so
   *  repainting only the dropdown leaves the Thinking-level row offering levels
   *  this pane's own validator then rejects — the human picks `xhigh`, the next
   *  mutation re-renders the row disabled, and the findings flag the block. The
   *  treatment below is `ensureCliKnobs`'s, deliberately identical: same pass,
   *  same three renders, same in-place knob repaint when the form must not be
   *  rebuilt.
   *
   *  One method rather than one per route, because both routes owe the same
   *  work: the lookup a block form fires when it paints, and the sweep's push
   *  (`modelCatalog.onReport`) for a form that was already open when the answer
   *  landed. A second copy is the second place a fix has to be remembered —
   *  which is the bug #997 caught here in the first place.
   *
   *  **It never rebuilds the form unconditionally.** `replaceChildren` destroys
   *  the input under the caret, so the pane's own rule holds: the form is
   *  redrawn only when the human is not inside it, and repainted in place when
   *  they are. The menu goes through the inspector's `refreshBlockModels`, which is
   *  `null` when no form is on screen and defers past the mid-type window when
   *  one is.
   *
   *  **Idempotent per CLI**, which is where the two routes are reconciled: the
   *  first delivery to arrive does the work and the second is a no-op, whichever
   *  order they land in. A second application could only ever repeat the first —
   *  the sweep asks each CLI once, so there is no later answer for the same one
   *  to carry. */
  applyDetection(program: string): void {
    if (this.detectionsApplied.has(program)) return;
    this.detectionsApplied.add(program);
    // The findings are the pane's, not any one form's, so they are recomputed
    // and repainted whatever happened to the form meanwhile — a reply that
    // landed after the human moved on still corrects the file's analysis.
    this.view.analysis = analyzeWorkflow(this.view.text, this.knobLookup);
    this.view.renderRoster();
    this.view.renderFindings();
    this.view.canvas.renderGraph();
    // Through the LIVE hooks, never a captured closure: `renderInspector()`
    // nulls both precisely so a late reply cannot paint into a detached row.
    this.view.inspector.refreshBlockModels?.();
    if (this.view.formPane.contains(document.activeElement)) this.view.inspector.repaintBlockKnobs?.();
    else this.view.inspector.renderInspector();
  }

  /** Ask what models `cli` reports, at most once per pane — see {@link modelProbes}. */
  probeModels(cli: string): Promise<CliProbe> {
    let p = this.modelProbes.get(cli);
    if (!p) {
      p = modelCatalog.probe(cli);
      this.modelProbes.set(cli, p);
    }
    return p;
  }

  /** Fetch `agent_cli_knobs` for every CLI the file names, once each (#687).
   *
   *  The pane mirrors no vendor capability of its own — which knobs a CLI has, and
   *  the reason it lacks one, are the backend's `CLI_CAPS` row, asked for. Each
   *  reply re-runs the analysis so the knob findings and the form's controls
   *  appear the moment the answer lands, without blocking the file from opening
   *  on an IPC round-trip. */
  ensureCliKnobs(): void {
    for (const b of this.view.analysis.workflow.blocks) {
      const cli = b.cli.trim();
      if (!cli || this.knobsAsked.has(cli)) continue;
      this.knobsAsked.add(cli);
      void agentCliKnobs(cli).then((caps) => {
        this.cliKnobs.set(cli, caps);
        // Re-run the same pass the pane would have run had the reply been in
        // hand when the file opened.
        this.view.analysis = analyzeWorkflow(this.view.text, this.knobLookup);
        // NOT `render()`: this lands whenever the IPC happens to resolve, which
        // can be mid-keystroke — and `render()` rewrites the YAML textarea from
        // the model, which is how an editor eats a keystroke (the same reason the
        // textarea's own input handler refreshes every surface BUT itself). The
        // form is redrawn only when the human isn't inside it.
        this.view.renderRoster();
        this.view.renderFindings();
        this.view.canvas.renderGraph();
        // …but the knob rows are exactly what this reply is the answer for, so
        // when the inspector can't be redrawn they are repainted in place instead
        // of being left saying "reading this CLI's capabilities…" until the human
        // clicks elsewhere and back (#935).
        if (this.view.formPane.contains(document.activeElement)) this.view.inspector.repaintBlockKnobs?.();
        else this.view.inspector.renderInspector();
      });
    }
  }

  /** One model-knob field (#687): the label, the select, and the hint that
   *  carries the vendor's reason where loomux cannot deliver the knob — plus the
   *  `paint` that redraws all three from a fresh spec.
   *
   *  It is repaintable rather than rebuilt because the answer moves under a form
   *  that must not re-render: `context` is only available where the SELECTED
   *  model has a documented `[1m]` form, so it changes as the human types a model
   *  id, and re-rendering the form on a keystroke would rebuild the input under
   *  their caret. What to show is `workflowknobs.ts`' (`KnobFieldSpec`); this is
   *  the DOM half. */
  knobRow(
    label: string,
    spec: KnobFieldSpec,
    onChange: (v: string) => void
  ): { field: HTMLElement; paint: (next: KnobFieldSpec) => void } {
    const s = document.createElement("select");
    s.className = "wf-input";
    s.addEventListener("change", () => onChange(s.value));
    const hint = el("span", "wf-hint");
    const field = el("label", "wf-field");
    field.append(el("span", "wf-label", label), s, hint);
    const paint = (next: KnobFieldSpec): void => {
      s.replaceChildren(
        ...next.options.map((o) => {
          const opt = document.createElement("option");
          opt.value = o.value;
          opt.textContent = o.label;
          return opt;
        })
      );
      s.value = next.selected;
      s.disabled = next.disabled;
      hint.textContent = next.hint;
    };
    paint(spec);
    return { field, paint };
  }
}
