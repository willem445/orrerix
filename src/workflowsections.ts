// The workflow pane's SECTION forms, split out of workflowview.ts (#3498 F3): the merge gate,
// intake, review driver, merge queue and resources editors the inspector shows when one of
// those roster rows is selected. They sit apart from workflowinspector.ts because the two
// together are over the 1,500-line TS ceiling (test/filebudget.test.ts). They are built from
// the inspector's form primitives, and their bounds come from `POLICY_BOUNDS` and the other
// workflowtypes.ts tables.
//
// A satellite of `WorkflowView`, not a model: it holds no state and reads the pane through
// `WorkflowViewApi` (workflowviewapi.ts), never through workflowview.ts. DOM glue,
// hand-validated. Design note: docs/design/workflows.md; layout conventions:
// docs/design/module-layout.md.

import {
  isReviewingBlock,
  isValidResourceName,
  GATE_REQUIRES,
  INTAKE_SOURCES,
  INTAKE_LABEL_KEYS,
  ID_MAX_CHARS,
  RESOURCES_MAX,
  RESOURCE_SLOTS_MIN,
  RESOURCE_SLOTS_MAX,
  RESOURCE_MAX_HOLD_MINUTES_MIN,
  RESOURCE_MAX_HOLD_MINUTES_MAX,
  MERGE_QUEUE_CHECKS_TIMEOUT_MIN,
  MERGE_QUEUE_CHECKS_TIMEOUT_MAX,
  DRIVER_DEFAULTS,
  POLICY_BOUNDS,
  isDriverOn,
  driverSectionHasComments,
  driverEnabledLineComment,
  setDriverEnabled,
  removeDriverBlock,
  type Workflow,
  type WorkflowResource,
  type IntakeLabelKey,
} from "./workflowmodel";
import { promptModal, confirmModal } from "./modal";
import { el } from "./workflowinspector";
import type { WorkflowViewApi, WorkflowSectionsApi } from "./workflowviewapi";

export class WorkflowSections implements WorkflowSectionsApi {
  constructor(private readonly view: WorkflowViewApi) {}

  gateForm(w: Workflow): HTMLElement {
    const box = el("div", "wf-fields");
    box.append(
      el(
        "p",
        "wf-note",
        "ENFORCED, not advised: orrerix refuses `gh pr merge` (via the PATH shim an agent cannot get around) " +
          "until every reviewer this gate names has recorded a verdict of PASS. This is what makes a second " +
          "reviewer more than a suggestion."
      )
    );

    const gate = w.gates.merge;
    const on = document.createElement("input");
    on.type = "checkbox";
    on.checked = !!gate;
    on.addEventListener("change", () =>
      this.view.mutate((next) => {
        next.gates = {
          ...next.gates,
          merge: on.checked
            ? {
                require: "all-pass",
                // Reviewer-kind minus the liaison (#891 S4): filling this with a
                // bare `kind` filter made ticking the gate on author a file the
                // pane's own validator flags `gate-not-a-reviewer` in the same
                // breath — the human never named the liaison, the checkbox did.
                reviewers: next.blocks.filter(isReviewingBlock).map((b) => b.id),
                also: [],
              }
            : undefined,
        };
      })
    );
    const onLine = el("label", "wf-check");
    onLine.append(on, el("span", "wf-check-label", "Gate merges on review verdicts"));
    box.append(onLine);

    if (!gate) return box;

    box.append(
      this.view.inspector.field(
        "Require",
        this.view.inspector.select(GATE_REQUIRES, gate.require, (v) =>
          this.view.mutate((next) => {
            const g = next.gates.merge!;
            g.require = v;
            if (v === "threshold" && g.threshold === undefined) g.threshold = g.reviewers.length || 1;
            if (v === "all-pass") delete g.threshold;
          })
        ),
        "all-pass = every named reviewer. threshold = at least N of them."
      )
    );

    if (gate.require === "threshold") {
      // Through the same bounded control, and the same `POLICY_BOUNDS` row, as every other
      // number in this pane (#1020 review, finding 7). It used to hand-roll its own input
      // whose floor was the string "1" and whose empty state wrote `Number("") || 1` — the
      // pane inventing a threshold nobody typed, which is the same defect as the invented
      // `max_batch` ceiling one finding earlier. Empty now means UNDECLARED, and a
      // threshold gate with no threshold is exactly what `gate-bad-threshold` is for: the
      // human is told what the gate needs instead of being given a number they didn't ask
      // for.
      box.append(
        this.view.inspector.field(
          "Threshold",
          this.view.inspector.boundedNumber(
            gate.threshold,
            POLICY_BOUNDS["gate.threshold"]!,
            (v) =>
              this.view.mutate((next) => {
                const g = next.gates.merge!;
                if (v === undefined) delete g.threshold;
                else g.threshold = v;
              }, false),
            "how many must pass"
          ),
          "How many of the named reviewers must record a PASS. There is no default — a threshold gate says the number."
        )
      );
    }

    const reviewers = el("div", "wf-checks");
    // Same predicate as the fill-in above, so the offer list and what it fills
    // in agree. A liaison already NAMED by a hand-edited file is not hidden by
    // this — it falls through to the `wf-bad` row below, labelled and
    // untickable, which is where the file's own finding can be acted on.
    const reviewerBlocks = w.blocks.filter((b) => isReviewingBlock(b) && b.id);
    for (const b of reviewerBlocks) {
      const line = el("label", "wf-check");
      const cb = document.createElement("input");
      cb.type = "checkbox";
      cb.checked = gate.reviewers.includes(b.id);
      cb.addEventListener("change", () =>
        this.view.mutate((next) => {
          const g = next.gates.merge!;
          g.reviewers = cb.checked
            ? [...g.reviewers, b.id]
            : g.reviewers.filter((r) => r !== b.id);
        })
      );
      line.append(cb, el("span", "wf-check-label", `${b.name || b.id} (${b.id})`));
      reviewers.append(line);
    }
    // A gate reviewer that isn't a reviewer block (or doesn't exist) can't be a checkbox —
    // but it IS in the file, and hiding it would make the finding about it unfixable here.
    for (const id of gate.reviewers.filter((r) => !reviewerBlocks.some((b) => b.id === r))) {
      const line = el("label", "wf-check");
      const cb = document.createElement("input");
      cb.type = "checkbox";
      cb.checked = true;
      cb.addEventListener("change", () =>
        this.view.mutate((next) => {
          const g = next.gates.merge!;
          g.reviewers = g.reviewers.filter((r) => r !== id);
        })
      );
      // A liaison lands here too, and "not a reviewer block" would be wrong
      // about it in a way the author can see is wrong (their file says
      // `kind: reviewer`) — so say the thing that is actually true of it.
      const why =
        !!id && w.blocks.some((b) => b.id === id && b.kind === "reviewer" && !isReviewingBlock(b))
          ? "a liaison, which records no verdict"
          : "not a reviewer block";
      line.append(cb, el("span", "wf-check-label wf-bad", `${id} — ${why}`));
      reviewers.append(line);
    }
    if (!reviewers.children.length) {
      reviewers.append(el("span", "wf-hint", "No reviewer blocks yet — add one, and it can gate the merge."));
    }
    box.append(this.view.inspector.field("Reviewers", reviewers));

    box.append(
      this.view.inspector.field(
        "Also require",
        this.view.inspector.textInput(
          gate.also.join(", "),
          (v) =>
            this.view.mutate((next) => {
              next.gates.merge!.also = v
                .split(",")
                .map((s) => s.trim())
                .filter(Boolean);
            }, false),
          "ci-green"
        ),
        "Comma-separated extra conditions, enforced by the backend (#197). Known: ci-green, body-unchanged, base-green — one this build cannot check refuses the merge rather than being ignored."
      )
    );

    // #1174's small-batch clause. Declared-only, like `threshold` above: empty means
    // UNDECLARED (no limit), never `0` — which the engine refuses outright, so a form
    // that wrote one would produce a file that will not load.
    box.append(
      this.view.inspector.field(
        "Max diff lines",
        this.view.inspector.boundedNumber(
          gate.max_diff_lines,
          POLICY_BOUNDS["gate.max_diff_lines"]!,
          (v) =>
            this.view.mutate((next) => {
              const g = next.gates.merge!;
              if (v === undefined) delete g.max_diff_lines;
              else g.max_diff_lines = v;
            }, false),
          "no limit"
        ),
        "Refuse a merge whose PR changes more than this many lines (additions + deletions). Leave empty for no limit."
      )
    );

    // #1176's path routing, shown but NOT editable here.
    //
    // The pane round-trips these rules — `readGate`/`emitGatesLines` carry them
    // and `validateWorkflow` checks them — which is the part that matters: without
    // it, a rule would be a line the next form edit silently DELETED, and what it
    // deleted would be a required reviewer. What is missing is an affordance to
    // add or change one, and a row that pretended otherwise would be worse than a
    // row that says where to go instead.
    const rules = gate.routing ?? [];
    if (rules.length) {
      const list = el("div", "wf-static");
      for (const [i, rule] of rules.entries()) {
        list.append(
          el(
            "div",
            "wf-static-row",
            `${i + 1}. ${rule.paths.join(", ")} → ${rule.reviewers.join(", ")}`
          )
        );
      }
      box.append(
        this.view.inspector.field(
          "Reviewers routed by path",
          list,
          // `this.rel`, never a literal and not even `WORKFLOW_FILE`: the pane may
          // have opened the LEGACY path on a repo that still carries it, and a hint
          // telling someone to edit a file that is not the one in front of them is
          // the defect this file already fixed once (see the `startPathEl` note).
          `A PR touching any of a rule's paths requires that rule's reviewers too, on top of the list above. Additive — a rule can only ever make this gate stricter. Edit these in ${this.view.rel}; this pane preserves them but does not yet offer a control for them.`
        )
      );
    }
    return box;
  }

  // ---------- the policy sections (#1020) ----------
  //
  // Three optional sections the file could always carry and the pane could never edit:
  // `intake:` (#382 — where autonomous work comes from), `merge_queue:` (#581) and
  // `resources:` (#858). All three are shaped like `gateForm` — an enable-toggle whose
  // state IS the section's presence in the file, then the fields — and all three lean on
  // the model's declared-only emission: a field left blank writes NO line, so opening a
  // form to read it can never turn "inherit loomux's default" into a pin.

  intakeForm(w: Workflow): HTMLElement {
    const box = el("div", "wf-fields");
    box.append(
      el(
        "p",
        "wf-note",
        "Where autonomous work comes from: which source the orchestrator polls, and the label " +
          "vocabulary it matches on. Every field is optional — an undeclared one inherits orrerix's " +
          "built-in profile, so a repo can override one label and keep the other four."
      )
    );

    const intake = w.intake;
    box.append(
      this.view.inspector.sectionToggle("This repo declares its own intake", !!intake, (on) =>
        this.view.mutate((next) => {
          if (on) next.intake = {};
          else delete next.intake;
        })
      )
    );
    if (!intake) return box;

    box.append(
      this.view.inspector.field(
        "Source",
        this.view.inspector.labelledSelect(
          [
            { value: "", label: "inherit orrerix's default" },
            ...INTAKE_SOURCES.map((s) => ({ value: s, label: s })),
          ],
          intake.source ?? "",
          (v) =>
            this.view.mutate((next) => {
              const i = next.intake!;
              if (v) i.source = v;
              else delete i.source;
            })
        ),
        "github-labels polls the repo's issues; board reads the task board; none disables autonomous intake."
      )
    );

    const LABEL_HINTS: Record<IntakeLabelKey, string> = {
      ready: "Groomed — an agent may start this.",
      investigate: "Research only: post findings, write no code.",
      owned: "An orchestrator has taken this issue.",
      prototype: "Build for a demo, not for merge.",
      hold: "The veto (#778): held by the human — do not start this, even under full autonomy.",
    };
    for (const key of INTAKE_LABEL_KEYS) {
      const value = intake.labels?.[key];
      box.append(
        this.view.inspector.field(
          `Label · ${key}`,
          this.view.inspector.textInput(
            value ?? "",
            (v) =>
              this.view.mutate((next) => {
                const i = next.intake!;
                const labels = i.labels ?? {};
                if (v.trim()) labels[key] = v.trim();
                else delete labels[key];
                // An empty `labels:` mapping is a section nobody declared anything in — drop
                // it rather than writing `labels: {}`, which would be a statement of its own.
                if (Object.keys(labels).length) i.labels = labels;
                else delete i.labels;
              }, false),
            "inherit"
          ),
          LABEL_HINTS[key]
        )
      );
    }
    box.append(
      el(
        "p",
        "wf-note",
        `A label is letters, digits, - and _ (no leading -, at most ${ID_MAX_CHARS} characters). ` +
          "orrerix rejects anything else rather than rewriting it, so the label it looks for stays " +
          "the one your repo actually has."
      )
    );
    const findings = this.view.inspector.sectionFindingList("intake");
    if (findings) box.append(findings);
    return box;
  }

  /** The `driver:` block's form (#1778 §5.3; the enable-toggle and counters since #1869).
   *  The pane parses, preserves, re-emits and validates the block, and this view is the
   *  whole chrome it gets: the checkbox below IS the driver's enabled state — it reads
   *  `isDriverOn` (the `enabled:` line, not the block's presence) and writes through
   *  `setDriverEnabled` (on writes `{ enabled: true }` beside whatever the block already
   *  declares; off deletes a block that holds nothing but the switch, and writes
   *  `enabled: false` — losing nothing — when it holds more, the comment signal coming
   *  from `driverSectionHasComments` over the text this form was rendered from) — and
   *  the six counters are bounded number fields
   *  reading `POLICY_BOUNDS` — the manifest's own min/max, which
   *  `test/workflowschema.test.ts` pins against the engine in both directions, so the
   *  form cannot emit an out-of-range value at all — and which bounds the engine
   *  REFUSES (the three counters) versus CLAMPS (`clamp_expires_minutes`, the three
   *  timeouts) is the manifest's `on_out_of_range`, pinned behaviorally by the
   *  refuse-vs-clamp test. The counters still get
   *  hand-built fields rather than a descriptor registry — slice C is what retires the
   *  hand-built forms, and until then this is the same shape `mergeQueueForm` is. What
   *  the block must not be is invisible: a declared policy the designer cannot show is
   *  the failure the #880 manifest exists to prevent. */
  driverForm(w: Workflow): HTMLElement {
    const box = el("div", "wf-fields");
    box.append(
      el(
        "p",
        "wf-note",
        "The review-loop driver: loomux drives a PR through review and CI on the " +
          "orchestrator's authority. An absent driver: block means the feature is OFF. " +
          "Unticking removes the section when it holds nothing but the switch, and writes " +
          "enabled: false — keeping your counters and comments — when it holds more."
      )
    );

    const dv = w.driver;
    box.append(
      // The read rule lives in `isDriverOn` — the enabled LINE, not the block's
      // presence, since the engine's `enabled` is `#[serde(default)] bool` and a
      // present block without the line is off. The write rule takes the comment
      // signal from the text this form was rendered from: OFF may not delete the
      // file's own prose about the section along with it (#1869 review round 3).
      this.view.inspector.sectionToggle("The review driver is on for this repo", isDriverOn(w), (on) =>
        this.view.mutate((next) =>
          setDriverEnabled(next, on, driverSectionHasComments(this.view.text))
        )
      )
    );
    if (!dv) return box;

    // The flip note (#1876 P2): where the splice will actually PRESERVE the
    // enabled line's trailing comment across a value flip, the note says so —
    // quoting the comment, because it can end up beside a switch it no longer
    // describes. The helper carries the splice's own suffix guard: on a bail
    // shape the flip regenerates the section and the comment does not survive,
    // so the helper returns null and the note does not render (#1876 review 1).
    const enabledComment = driverEnabledLineComment(w, this.view.text);
    if (enabledComment) {
      box.append(
        el(
          "p",
          "wf-note",
          `The enabled: line carries its own comment (${enabledComment}). Flipping the ` +
            "switch rewrites the value on that line and leaves the comment exactly as " +
            "written — edit the line if the comment no longer matches the switch."
        )
      );
    }

    // Every fallback reads `DRIVER_DEFAULTS` - the engine's `DriverPolicy::default`
    // mirrored and manifest-pinned - rather than a literal nothing can check; every
    // bound reads `POLICY_BOUNDS` rather than a retyped range, so a manifest change
    // flows into the form through the pinned table instead of past it.
    type DriverCounter =
      | "max_review_rounds"
      | "max_ci_attempts"
      | "max_rebase_attempts"
      | "lane_timeout_minutes"
      | "fix_timeout_minutes"
      | "drive_timeout_minutes"
      | "plan_review_minutes"
      | "planner_timeout_minutes"
      | "fix_nonblocking_rounds";
    const bounded = (label: string, field: DriverCounter, help: string): void => {
      box.append(
        this.view.inspector.field(
          label,
          this.view.inspector.boundedNumber(
            dv[field],
            POLICY_BOUNDS[`driver.${field}`]!,
            (v) =>
              this.view.mutate((next) => {
                const d = next.driver!;
                if (v === undefined) delete d[field];
                else d[field] = v;
              }, false),
            `orrerix's default (${DRIVER_DEFAULTS[field]})`
          ),
          help
        )
      );
    };
    bounded(
      "Review rounds",
      "max_review_rounds",
      "Review-finding rounds one drive may spend. INVARIANT 9 promises the orchestrator " +
        "three; a repo may run tighter, never looser."
    );
    bounded(
      "CI attempts",
      "max_ci_attempts",
      "CI attempts one drive may spend. Same invariant, same direction."
    );
    bounded(
      "Rebase attempts",
      "max_rebase_attempts",
      "Rebase attempts one drive may spend. 0 is legal - a repo may refuse the driver any rebase."
    );
    bounded(
      "Lane timeout (min)",
      "lane_timeout_minutes",
      "Backstop on a reviewer lane producing a verdict, so a stalled lane surfaces as " +
        "held(lane-stalled) instead of pending in silence. Clamped, not refused."
    );
    bounded(
      "Fix timeout (min)",
      "fix_timeout_minutes",
      "Backstop on a resumed worker pushing or reporting. Clamped, not refused."
    );
    bounded(
      "Drive timeout (min)",
      "drive_timeout_minutes",
      "Backstop on the drive's whole age, from the entry's start - no idle clock resets it. " +
        "The default is this range's ceiling."
    );
    // #3367. The driver's own non-blocking rounds, and the switch that lets a
    // worker's report(done) start a drive. Both under the same enable gate.
    bounded(
      "Non-blocking rounds",
      "fix_nonblocking_rounds",
      "How many times the driver may hand a satisfied gate back to the worker on its own, " +
        "when every required lane passed with only non-blocking findings open. 0 - the " +
        "default - wakes you at once, as before. Each round is also a review round, so it " +
        "never takes a drive past the review-round bound above. A lane whose summary does " +
        "not state its blocking count wakes you instead."
    );
    box.append(
      this.view.inspector.sectionToggle(
        "A worker's report(done) on its own PR starts a drive",
        dv.auto_drive_on_done === true,
        (on) =>
          this.view.mutate((next) => {
            const d = next.driver!;
            // OFF deletes the key, for `plan_enabled`'s reason below.
            if (on) d.auto_drive_on_done = true;
            else delete d.auto_drive_on_done;
          })
      )
    );
    box.append(
      el(
        "p",
        "wf-note",
        "The report then reaches you inside the drive's first notice instead of on its own. " +
          "It is refused - and delivered as before - for a [scratch] PR, a ref that is not a " +
          "PR, a worker whose branch is not the PR's head, a PR already driven or parked, and a PR that " +
          "already carries a verdict."
      )
    );
    // The PLAN driver (#3040), under the same block and the same enable gate.
    //
    // Its own switch rather than a widening of the one above, and the form says
    // so in the same words the engine does: turning the review driver on
    // consented to loomux running a review loop you already had an orchestrator
    // for, not to loomux spawning a planner and turning its output into work.
    // Written as a plain field rather than through `setDriverEnabled`, which is
    // the SECTION's rule (delete a bare block, keep a configured one) and would
    // be the wrong gesture for a key inside it.
    box.append(
      el(
        "p",
        "wf-note",
        "The plan driver: loomux spawns a planner on a labelled issue, validates the plan " +
          "block it posts, and turns the plan into board rows and worker spawns. It is a " +
          "second switch, and it is read UNDER the one above — so it is off wherever the " +
          "review driver is."
      )
    );
    box.append(
      this.view.inspector.sectionToggle(
        "The plan driver is on for this repo",
        dv.plan_enabled === true,
        (on) =>
          this.view.mutate((next) => {
            const d = next.driver!;
            // OFF deletes the key rather than writing `plan_enabled: false`:
            // absent and false are the same state to the engine, and the
            // enclosing block is not at stake here — the section toggle above
            // owns that decision, and a line this form invented would be a
            // policy statement the human never made.
            if (on) d.plan_enabled = true;
            else delete d.plan_enabled;
          })
      )
    );
    bounded(
      "Plan review window (min)",
      "plan_review_minutes",
      "How long a posted plan waits before the drive acts on it, so you can veto. 0 - the " +
        "default - means no window: the label already said go. Any non-zero value costs " +
        "exactly one notice in your pane, which is the price of the window. Refused out of " +
        "range, not clamped."
    );
    bounded(
      "Planner timeout (min)",
      "planner_timeout_minutes",
      "Backstop on a driven planner posting its plan, so a planner that stopped surfaces as " +
        "held(planner-stalled) instead of silence. Refused out of range, not clamped: five " +
        "minutes is a misunderstanding of what a planner does, and quietly giving you " +
        "fifteen would leave it in place."
    );
    // The escape hatch the narrowed toggle no longer provides (#1876 P1): removal
    // is its own destructive gesture, behind its own confirmation, because it
    // discards configuration. A driver: block makes the file unloadable on an
    // orrerix build old enough to refuse the key (`RawWorkflow` is
    // `deny_unknown_fields`), and this button is how the file gets back.
    const remove = document.createElement("button");
    remove.className = "wf-btn wf-btn-danger";
    remove.textContent = "Remove the driver block…";
    remove.addEventListener("click", () => void this.confirmRemoveDriver());
    box.append(remove);
    const findings = this.view.inspector.sectionFindingList("driver");
    if (findings) box.append(findings);
    return box;
  }

  /** Remove the whole `driver:` block, behind its own confirmation (#1876 P1).
   *  Deliberately a different gesture from the enable toggle: the toggle
   *  preserves a configured block (#1869 review round 3), so removal is the one
   *  way to discard it — and the escape hatch for opening the file in an
   *  orrerix build old enough to refuse the key (`RawWorkflow` is
   *  `deny_unknown_fields`, verified against v1.3.0-beta1, whose root type
   *  carries the attribute and no `driver:` field). The dialog names what is
   *  discarded — switch, counters, comments — before the user commits, because
   *  nothing in the file survives it. */
  private async confirmRemoveDriver(): Promise<void> {
    const yes = await confirmModal(
      "Remove the driver block?",
      "This deletes the whole `driver:` block — the switch, every counter it declares, " +
        "and any comment inside it. The file then loads on orrerix builds old enough to " +
        "refuse the key, which is the reason to do this: those builds cannot load the " +
        "file while the block stands. The toggle can write a fresh block afterwards, " +
        "but these counters and comments are gone.",
      "Remove the block",
      true
    );
    if (!yes) return;
    this.view.mutate((next) => removeDriverBlock(next));
  }

  mergeQueueForm(w: Workflow): HTMLElement {
    const box = el("div", "wf-fields");
    box.append(
      el(
        "p",
        "wf-note",
        "The bisecting merge queue: approved sub-PRs land as one batch, and a batch whose checks " +
          "fail is bisected rather than dropped. An absent merge_queue: block means the feature is " +
          "OFF — which is why unticking below removes the section instead of writing enabled: false."
      )
    );

    const mq = w.merge_queue;
    box.append(
      this.view.inspector.sectionToggle("This repo declares a merge queue", !!mq, (on) =>
        this.view.mutate((next) => {
          if (on) next.merge_queue = { enabled: true };
          else delete next.merge_queue;
        })
      )
    );
    if (!mq) return box;

    // A THREE-WAY control, because the file has three states and a checkbox has two
    // (#1020 review, finding 4). The old checkbox claimed, in its own comment, never to
    // invent `enabled: false` — and then did, across two clicks: ticking wrote `true`, and
    // unticking found the key defined and wrote `false` onto a file that had never carried
    // it. Every repair that keeps a checkbox loses a state instead: untick-always-deletes
    // silently drops an explicit `enabled: false` a human wrote.
    //
    // So the control shows what the file says. Absent and `false` mean the same thing to
    // the engine (`#[serde(default)]`), which is exactly why the pane must not silently
    // convert between them — it is the human's line, not ours, and this is the one form in
    // the pane whose entire subject is what the file declares.
    box.append(
      this.view.inspector.field(
        "Enabled",
        this.view.inspector.labelledSelect(
          [
            { value: "", label: "not declared — off (orrerix's default)" },
            { value: "true", label: "true — run the queue" },
            { value: "false", label: "false — declared off" },
          ],
          mq.enabled === undefined ? "" : String(mq.enabled),
          (v) =>
            this.view.mutate((next) => {
              const q = next.merge_queue!;
              if (v === "") delete q.enabled;
              else q.enabled = v === "true";
            })
        ),
        "An absent enabled: is off — the same thing the engine reads from enabled: false, kept apart here because the line is yours."
      )
    );

    box.append(
      this.view.inspector.field(
        "Max batch",
        this.view.inspector.boundedNumber(mq.max_batch, POLICY_BOUNDS["merge_queue.max_batch"]!, (v) =>
          this.view.mutate((next) => {
            const q = next.merge_queue!;
            if (v === undefined) delete q.max_batch;
            else q.max_batch = v;
          }, false)
        ),
        "How many approved sub-PRs one batch may carry. Empty inherits orrerix's default; a batch of none could never land anything."
      )
    );

    box.append(
      this.view.inspector.field(
        "Checks timeout (minutes)",
        this.view.inspector.boundedNumber(
          mq.checks_timeout_minutes,
          POLICY_BOUNDS["merge_queue.checks_timeout_minutes"]!,
          (v) =>
            this.view.mutate((next) => {
              const q = next.merge_queue!;
              if (v === undefined) delete q.checks_timeout_minutes;
              else q.checks_timeout_minutes = v;
            }, false)
        ),
        `How long to wait for a batch's checks before calling it unverifiable. orrerix clamps this to ${MERGE_QUEUE_CHECKS_TIMEOUT_MIN}–${MERGE_QUEUE_CHECKS_TIMEOUT_MAX}.`
      )
    );
    const findings = this.view.inspector.sectionFindingList("merge_queue");
    if (findings) box.append(findings);
    return box;
  }

  resourcesForm(w: Workflow): HTMLElement {
    const box = el("div", "wf-fields");
    box.append(
      el(
        "p",
        "wf-note",
        "Named locks agents take turns on — a build directory, a test database, anything two agents " +
          "must not hold at once. orrerix never learns what a name MEANS: it counts slots and bounds " +
          "how long a hold may last, and the agents' own briefs say what to acquire."
      )
    );

    const resources = w.resources;
    box.append(
      this.view.inspector.sectionToggle("This repo declares shared resources", !!resources, (on) =>
        this.view.mutate((next) => {
          if (on) next.resources = {};
          else delete next.resources;
        })
      )
    );
    if (!resources) return box;

    // Sorted, matching the emitter (and the engine's BTreeMap): a resource map has no
    // authored order to preserve, unlike the roster, where the order is meaning.
    const names = Object.keys(resources).sort();
    for (const name of names) {
      const r = resources[name]!;
      // A plain div, not `this.field(...)`: the card holds several inputs and a button, and
      // wrapping that in the `<label>` `field` produces would nest labels around controls
      // that already have their own.
      const card = el("div", "wf-fields");
      const head = el("div", "wf-check");
      head.append(el("span", "wf-label", name));
      const del = document.createElement("button");
      del.className = "wf-btn wf-btn-danger";
      del.textContent = "Remove";
      del.addEventListener("click", () =>
        this.view.mutate((next) => {
          if (next.resources) delete next.resources[name];
        })
      );
      head.append(del);
      card.append(head);
      const num = (
        label: string,
        key: keyof Pick<WorkflowResource, "slots" | "max_hold_minutes">,
        hint: string
      ): void => {
        card.append(
          this.view.inspector.field(
            label,
            this.view.inspector.boundedNumber(r[key], POLICY_BOUNDS[`resource.${key}`]!, (v) =>
              this.view.mutate((next) => {
                const target = next.resources?.[name];
                if (!target) return;
                if (v === undefined) delete target[key];
                else target[key] = v;
              }, false)
            ),
            hint
          )
        );
      };
      num(
        "Slots",
        "slots",
        `How many agents may hold it at once (${RESOURCE_SLOTS_MIN}–${RESOURCE_SLOTS_MAX}). Empty inherits orrerix's default.`
      );
      num(
        "Max hold (minutes)",
        "max_hold_minutes",
        `How long one hold may last before it expires (${RESOURCE_MAX_HOLD_MINUTES_MIN}–${RESOURCE_MAX_HOLD_MINUTES_MAX}). Empty inherits orrerix's default.`
      );
      box.append(card);
    }

    const add = el("button", "wf-add", "+ Add resource") as HTMLButtonElement;
    add.disabled = names.length >= RESOURCES_MAX;
    add.addEventListener("click", () => void this.addResource(names));
    box.append(add);
    if (names.length >= RESOURCES_MAX) {
      box.append(
        el(
          "span",
          "wf-hint",
          `${RESOURCES_MAX} is the maximum — every name is listed in the acquire_lock tool description every agent in the group reads.`
        )
      );
    }
    const findings = this.view.inspector.sectionFindingList("resources");
    if (findings) box.append(findings);
    return box;
  }

  /** Add a resource — ASKING for the name, the same commitment `createBlock` makes about a
   *  block id and for the same reason: the name is what an agent's own `acquire_lock` call
   *  spells, loomux rejects rather than rewrites anything outside its alphabet, and a name
   *  validated as it is typed never becomes a finding to decode afterwards. */
  private async addResource(existing: readonly string[]): Promise<void> {
    const name = await promptModal({
      title: "New resource",
      body:
        "The name is what an agent asks for by (acquire_lock \"build\"). Letters, digits, - and _; " +
        `at most ${ID_MAX_CHARS} characters.`,
      label: "Resource name",
      placeholder: "build",
      affirm: "Add",
      validate: (v) => {
        if (!v.trim()) return "A resource needs a name.";
        if (!isValidResourceName(v)) {
          return `Use letters, digits, - and _ (at most ${ID_MAX_CHARS} characters).`;
        }
        if (existing.includes(v.trim())) return `This workflow already declares "${v.trim()}".`;
        return null;
      },
    });
    if (!name) return;
    this.view.mutate((next) => {
      const resources = next.resources ?? {};
      // `{}` — declared with loomux's defaults, which is what a human means by adding a name
      // and setting nothing. It emits as `build: {}`, the spelling the engine's serde accepts.
      resources[name.trim()] = {};
      next.resources = resources;
    });
  }
}
