// The launcher's Quick-task section (#3679) — DOM glue over two pure models:
// `quickmodel.ts` decides what a submit sends, `quickpresets.ts` owns the saved
// instruction presets. Hand-validated, like every other piece of launcher DOM.
//
// It is a section of the welcome form rather than a form of its own: the
// repository picker, the guardrail row and the Create button are the
// launcher's, and this adds the task, the steps, their instructions and the
// two bounds. Launcher chrome only — it never touches a terminal's size.
//
// Design note: docs/design/quick-orchestration.md.

import { ModelPicker } from "./modelpicker";
import { promptModal } from "./modal";
import { ORCH_CLIS, orchCliFor } from "./orchclis.ts";
import { quickRunRows, type QuickRunRow, type QuickStatus } from "./quickchip.ts";
import {
  QUICK_MINUTES,
  QUICK_MODES,
  QUICK_ROUNDS,
  QUICK_STEPS,
  QUICK_STEP_LABEL,
  QUICK_STEP_ROLE,
  quickRootCliOptions,
  quickStepCli,
  quickStepCliOptions,
  type QuickFocus,
  type QuickFormValues,
  type QuickMode,
  type QuickStep,
} from "./quickmodel.ts";
import {
  presetMatching,
  presetNamed,
  type QuickInstructions,
  type QuickPreset,
  type QuickPresetsStore,
} from "./quickpresets.ts";

/** The picker's "no preset" option. A sentinel rather than the empty string so
 *  it can never collide with a preset's name. */
const NO_PRESET = "\u0000none";

const STEP_HINT: Record<QuickStep, string> = {
  plan: "a read-only planner writes a plan the worker then follows",
  work: "always on — the worker does the task in a worktree of its own",
  review: "a reviewer reads the work where it is and asks for changes or approves",
};

/** The same three rows in a described run, where they are not steps the human
 *  switched on but the helpers the agent may open. */
const HELPER_HINT: Record<QuickStep, string> = {
  plan: "what a planner runs on, if the agent opens one",
  work: "what a worker runs on, in a worktree of its own",
  review: "what a reviewer runs on, if the agent opens one",
};

const MODE_LABEL: Record<QuickMode, string> = {
  steps: "Steps — orrerix passes the work between a planner, a worker and a reviewer",
  describe: "Describe it — one agent opens idle; you tell it the task in its pane and it opens its own helpers",
};

/** Radio groups are matched by `name` across the whole document, and a tab can
 *  hold more than one welcome form — so each form's group gets its own. */
let modeGroupSeq = 0;

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls: string, text?: string): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

function field(label: string, control: HTMLElement, hint?: string): HTMLElement {
  const wrap = el("div", "dlg-field");
  const lab = el("div", "dlg-label", label);
  if (hint) lab.appendChild(el("span", "opt", ` — ${hint}`));
  wrap.append(lab, control);
  return wrap;
}

function textarea(rows: number, placeholder: string): HTMLTextAreaElement {
  const t = el("textarea", "dlg-input quick-text");
  t.rows = rows;
  t.placeholder = placeholder;
  t.spellcheck = false;
  // The welcome form submits on Enter from any field. In a multi-line box
  // Enter is a line break, so it stops here; Ctrl+Enter still submits.
  t.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.ctrlKey && !e.metaKey) e.stopPropagation();
  });
  return t;
}

function numberBox(bounds: { min: number; max: number; default: number }): HTMLInputElement {
  const n = el("input", "dlg-input dlg-num");
  n.type = "number";
  n.min = String(bounds.min);
  n.max = String(bounds.max);
  n.value = String(bounds.default);
  return n;
}

/** The number in a numeric box, or `null` when it is blank or not a number —
 *  carried through exactly as typed so `planQuickStart` can refuse it by name. */
function numberOrNull(input: HTMLInputElement): number | null {
  const raw = input.value.trim();
  if (!raw) return null;
  const n = Number(raw);
  return Number.isFinite(n) ? n : null;
}

interface StepRow {
  wrap: HTMLElement;
  cli: HTMLSelectElement;
  model: ModelPicker;
  instructions: HTMLTextAreaElement;
}

/** Replace a field's label and hint — the two text nodes `field` built. */
function relabel(wrap: HTMLElement, label: string, hint: string): void {
  const lab = wrap.querySelector(".dlg-label");
  if (!lab) return;
  lab.replaceChildren(document.createTextNode(label), el("span", "opt", ` — ${hint}`));
}

export class QuickFormSection {
  readonly el: HTMLElement;
  private readonly task: HTMLTextAreaElement;
  private readonly planBox: HTMLInputElement;
  private readonly reviewBox: HTMLInputElement;
  private readonly rows: Record<QuickStep, StepRow>;
  private readonly rounds: HTMLInputElement;
  private readonly roundsField: HTMLElement;
  private readonly minutes: HTMLInputElement;
  private readonly base: HTMLInputElement;
  private readonly perms: HTMLSelectElement;
  private readonly presetSel: HTMLSelectElement;
  private readonly presetDelete: HTMLButtonElement;
  /** The presets as last read — a display snapshot only. Nothing is ever saved
   *  from it: every write goes through the store, which re-reads the file. */
  private presets: QuickPreset[] = [];
  private presetsLoaded = false;
  private mode: QuickMode = "steps";
  private readonly modeBoxes = new Map<QuickMode, HTMLInputElement>();
  private readonly rootCli: HTMLSelectElement;
  private readonly rootModel: ModelPicker;
  private readonly rootField: HTMLElement;
  private readonly taskField: HTMLElement;
  private readonly minutesField: HTMLElement;
  private readonly stepsField: HTMLElement;
  private readonly presetField: HTMLElement;
  private readonly runsField: HTMLElement;
  private readonly runsList: HTMLElement;
  /** The unfinished runs last read. The list's DOM is a view of this — it is
   *  rebuilt from it, never read back from. */
  private runs: QuickRunRow[] = [];
  /** A row whose Stop is in flight, so a second click cannot send a second. */
  private readonly stopping = new Set<string>();

  constructor(
    private readonly opts: {
      /** The launcher's default CLI, which every step starts on where it can. */
      defaultCli: string;
      presets: QuickPresetsStore;
      /** Show a message in the form's own error line. */
      onError: (msg: string) => void;
      /** The runs that have not ended, and the two things to do with one. */
      runs: {
        list: () => Promise<QuickStatus[]>;
        stop: (group: string) => Promise<unknown>;
        /** Resume `group` into this form's tab. The host owns the tab, so the
         *  form only says which run. */
        resume: (group: string) => void;
      };
    }
  ) {
    const known = ORCH_CLIS.map((c) => c.id);
    // NOT a `.dlg-field`: a field wraps one control and its label, and this
    // wraps a whole section of them. Marking the section as a field made every
    // selector of the form "the field whose label says X" match the section
    // and all of its inputs the moment any label inside it contained X.
    this.el = el("div", "quick-section");

    this.task = textarea(4, "What should be done? One task, in your own words — required");

    const modeRow = el("div", "quick-modes");
    const modeName = `quick-mode-${++modeGroupSeq}`;
    for (const m of QUICK_MODES) {
      const box = el("input", "");
      box.type = "radio";
      box.name = modeName;
      box.value = m;
      box.checked = m === this.mode;
      box.addEventListener("change", () => {
        if (!box.checked) return;
        this.mode = m;
        this.applyMode();
      });
      const wrap = el("label", "quick-check");
      wrap.append(box, document.createTextNode(MODE_LABEL[m]));
      modeRow.appendChild(wrap);
      this.modeBoxes.set(m, box);
    }

    this.rootCli = el("select", "dlg-select");
    for (const id of quickRootCliOptions(known)) {
      const o = el("option", "", id);
      o.value = id;
      this.rootCli.appendChild(o);
    }
    this.rootCli.value = quickRootCliOptions(known).includes(opts.defaultCli)
      ? opts.defaultCli
      : (quickRootCliOptions(known)[0] ?? "");
    this.rootModel = new ModelPicker();
    const seedRoot = () => {
      const c = orchCliFor(this.rootCli.value);
      // The agent a task is given to does an orchestrator's kind of work —
      // deciding, delegating, reading what comes back — so it starts on that
      // class's default model.
      this.rootModel.setOptions(c.models, c.defaults.orchestrator, c.id);
    };
    this.rootCli.addEventListener("change", seedRoot);
    seedRoot();
    const rootPair = el("div", "dlg-row quick-step-row");
    rootPair.append(this.rootCli, this.rootModel.root);
    this.rootField = field(
      "Runs on",
      rootPair,
      "the agent that opens; tell it the task in its pane, and it decides whether to plan and to review"
    );

    this.runsList = el("div", "quick-runs");
    this.runsField = field("Unfinished runs", this.runsList, "resume or stop a run from here when none of its panes is left");
    this.runsField.hidden = true;

    const check = (label: string, on: boolean): [HTMLLabelElement, HTMLInputElement] => {
      const box = el("input", "");
      box.type = "checkbox";
      box.checked = on;
      const wrap = el("label", "quick-check");
      wrap.append(box, document.createTextNode(label));
      return [wrap, box];
    };
    const [planWrap, planBox] = check("Plan first", false);
    const [reviewWrap, reviewBox] = check("Review the work", true);
    this.planBox = planBox;
    this.reviewBox = reviewBox;
    const stepsRow = el("div", "dlg-row quick-steps");
    stepsRow.append(planWrap, reviewWrap);

    const row = (step: QuickStep): StepRow => {
      const cli = el("select", "dlg-select");
      for (const id of quickStepCliOptions(step, known)) {
        const o = el("option", "", id);
        o.value = id;
        cli.appendChild(o);
      }
      cli.value = quickStepCli(step, opts.defaultCli, known);
      const model = new ModelPicker();
      const seed = () => {
        const c = orchCliFor(cli.value);
        model.setOptions(c.models, c.defaults[QUICK_STEP_ROLE[step]], c.id);
      };
      cli.addEventListener("change", seed);
      seed();
      const pair = el("div", "dlg-row quick-step-row");
      pair.append(cli, model.root);
      const instructions = textarea(
        2,
        `Optional — your own instructions for the ${QUICK_STEP_LABEL[step].toLowerCase()} step, added to the role's`
      );
      instructions.addEventListener("input", () => this.paintPresetChoice());
      const wrap = field(`${QUICK_STEP_LABEL[step]} step`, pair, STEP_HINT[step]);
      wrap.classList.add("quick-step");
      wrap.appendChild(instructions);
      return { wrap, cli, model, instructions };
    };
    this.rows = { plan: row("plan"), work: row("work"), review: row("review") };

    this.presetSel = el("select", "dlg-select");
    this.presetSel.addEventListener("change", () => this.applyPreset());
    const save = el("button", "dlg-btn", "Save as…");
    save.type = "button";
    save.addEventListener("click", () => void this.savePreset());
    this.presetDelete = el("button", "dlg-btn", "Delete");
    this.presetDelete.type = "button";
    this.presetDelete.addEventListener("click", () => void this.deletePreset());
    const presetRow = el("div", "dlg-row quick-presets");
    presetRow.append(this.presetSel, save, this.presetDelete);

    this.rounds = numberBox(QUICK_ROUNDS);
    this.minutes = numberBox(QUICK_MINUTES);
    this.base = el("input", "dlg-input");
    this.base.placeholder = "default branch";
    this.base.spellcheck = false;
    this.roundsField = field(`Review rounds (${QUICK_ROUNDS.min}–${QUICK_ROUNDS.max})`, this.rounds);
    this.minutesField = field(`Time bound (min, ${QUICK_MINUTES.min}–${QUICK_MINUTES.max})`, this.minutes);
    const bounds = el("div", "dlg-row dlg-grid");
    bounds.append(this.roundsField, this.minutesField, field("Branch from", this.base));

    this.perms = el("select", "dlg-select");
    for (const [value, label] of [
      ["auto", "Auto — pre-approve git/gh + agent tools (recommended)"],
      ["edits", "Accept edits only — you approve git/gh yourself"],
    ]) {
      const o = el("option", "", label);
      o.value = value;
      this.perms.appendChild(o);
    }

    this.planBox.addEventListener("change", () => this.applySteps());
    this.reviewBox.addEventListener("change", () => this.applySteps());

    this.stepsField = field("Steps", stepsRow, "orrerix relays between them and tells you when the run ends");
    this.presetField = field("Instruction preset", presetRow, "your own saved instructions, offered wherever you work");
    this.taskField = field("Task", this.task);
    this.el.append(
      this.runsField,
      field("How", modeRow),
      this.taskField,
      this.rootField,
      this.stepsField,
      this.rows.plan.wrap,
      this.rows.work.wrap,
      this.rows.review.wrap,
      this.presetField,
      bounds,
      field("Permissions", this.perms)
    );
    this.applyMode();
    this.paintPresets();
  }

  /** Lay the form out for the chosen mode.
   *
   *  In a described run the form asks for no task at all (#3723): the agent
   *  opens idle and the human tells it in its pane. There are no steps to
   *  switch on and no instruction boxes either, because whether to plan, and
   *  what to tell each helper, is the agent's call. The three CLI rows stay —
   *  they say what each KIND of helper runs on, if it is opened — and so do
   *  the limits, which there apply to each task the agent is given. */
  private applyMode(): void {
    const described = this.mode === "describe";
    this.rootField.hidden = !described;
    // Hidden, not cleared: switching back to Steps finds the text still there.
    this.taskField.hidden = described;
    const range = `${QUICK_MINUTES.min}–${QUICK_MINUTES.max}`;
    const minutesLabel = this.minutesField.querySelector(".dlg-label");
    if (minutesLabel) {
      minutesLabel.textContent = described ? `Time bound per task (min, ${range})` : `Time bound (min, ${range})`;
    }
    this.stepsField.hidden = described;
    this.presetField.hidden = described;
    for (const step of QUICK_STEPS) {
      const row = this.rows[step];
      row.instructions.hidden = described;
      relabel(
        row.wrap,
        described ? `${QUICK_STEP_LABEL[step]} helper` : `${QUICK_STEP_LABEL[step]} step`,
        described ? HELPER_HINT[step] : STEP_HINT[step]
      );
    }
    this.applySteps();
  }

  /** Show only the rows of the steps that are on — all three in a described
   *  run, where they are the helpers the agent may open. */
  private applySteps(): void {
    const described = this.mode === "describe";
    this.rows.plan.wrap.hidden = !described && !this.planBox.checked;
    this.rows.review.wrap.hidden = !described && !this.reviewBox.checked;
    // A described run quotes the round bound to its agent, so the box stays.
    if (described) {
      this.roundsField.hidden = false;
      return;
    }
    this.roundsField.hidden = !this.reviewBox.checked;
  }

  /** Called when the launcher's kind becomes Quick task: read the presets once
   *  per form. A failed read leaves the picker empty and is retried by the
   *  next save or delete, which read for themselves. */
  activate(): void {
    void this.refreshRuns();
    if (this.presetsLoaded) return;
    this.presetsLoaded = true;
    void this.opts.presets.read().then((presets) => {
      if (!presets) {
        this.presetsLoaded = false;
        return;
      }
      this.presets = presets;
      this.paintPresets();
    });
  }

  /** Re-read the runs that have not ended and repaint the list. A read that
   *  fails leaves what was shown: "could not look" is not "there are none". */
  private async refreshRuns(): Promise<void> {
    let list: QuickStatus[];
    try {
      list = await this.opts.runs.list();
    } catch {
      return;
    }
    this.runs = quickRunRows(list);
    this.paintRuns();
  }

  /** Rebuild the list from `this.runs`. Hidden when there is nothing to show,
   *  so a form with no unfinished run looks exactly as it did before. */
  private paintRuns(): void {
    this.runsField.hidden = this.runs.length === 0;
    this.runsList.replaceChildren(
      ...this.runs.map((run) => {
        const row = el("div", "quick-run");
        const text = el("div", "quick-run-text");
        text.append(
          el("span", "quick-run-task", run.task || "(no description)"),
          el("span", "quick-run-meta", [run.repo, run.label, run.why].filter(Boolean).join(" · "))
        );
        text.title = [run.task, run.why].filter(Boolean).join("\n");
        row.appendChild(text);
        if (run.canResume) {
          const resume = el("button", "dlg-btn", "Resume here");
          resume.type = "button";
          resume.title = "Re-open this run's pane in this tab and carry on";
          resume.addEventListener("click", () => this.opts.runs.resume(run.group));
          row.appendChild(resume);
        }
        const stop = el("button", "dlg-btn", "Stop");
        stop.type = "button";
        stop.title = "End this run. Nothing is closed or deleted; the work stays where it is.";
        stop.disabled = this.stopping.has(run.group);
        stop.addEventListener("click", () => void this.stopRun(run.group));
        row.appendChild(stop);
        return row;
      })
    );
  }

  private async stopRun(group: string): Promise<void> {
    if (this.stopping.has(group)) return;
    this.stopping.add(group);
    this.paintRuns();
    try {
      await this.opts.runs.stop(group);
    } catch (err) {
      this.opts.onError(`That run could not be stopped: ${String(err)}`);
    } finally {
      this.stopping.delete(group);
    }
    await this.refreshRuns();
    // A failed re-read leaves the old rows, so the button is repainted from
    // the cleared flag either way.
    this.paintRuns();
  }

  private instructions(): QuickInstructions {
    return {
      plan: this.rows.plan.instructions.value,
      work: this.rows.work.instructions.value,
      review: this.rows.review.instructions.value,
    };
  }

  /** Rebuild the preset picker from the snapshot, then mark what matches. */
  private paintPresets(): void {
    this.presetSel.replaceChildren();
    const none = el("option", "", this.presets.length ? "— none —" : "— no saved presets —");
    none.value = NO_PRESET;
    this.presetSel.appendChild(none);
    for (const p of this.presets) {
      const o = el("option", "", p.name);
      o.value = p.name;
      this.presetSel.appendChild(o);
    }
    this.paintPresetChoice();
  }

  /** Point the picker at the preset the three boxes currently equal, or at
   *  "none" — so it never claims a preset the human has since edited away from. */
  private paintPresetChoice(): void {
    const match = presetMatching(this.presets, this.instructions());
    this.presetSel.value = match ? match.name : NO_PRESET;
    this.presetDelete.disabled = !match;
  }

  /** The picker changed: fill the three boxes from the chosen preset. Choosing
   *  "none" leaves the boxes alone — it is not a "clear" button. */
  private applyPreset(): void {
    const preset = presetNamed(this.presets, this.presetSel.value);
    if (preset) {
      this.rows.plan.instructions.value = preset.plan;
      this.rows.work.instructions.value = preset.work;
      this.rows.review.instructions.value = preset.review;
    }
    this.paintPresetChoice();
  }

  private async savePreset(): Promise<void> {
    const texts = this.instructions();
    const current = presetMatching(this.presets, texts);
    const name = await promptModal({
      title: "Save instruction preset",
      body: "Saves the three instruction boxes under a name. A preset is yours rather than a project's: it is offered wherever you start a quick task.",
      label: "Preset name",
      initial: current?.name ?? "",
      affirm: "Save",
      validate: (v) => (v.trim() ? null : "A preset needs a name."),
    });
    if (name === null) return;
    const result = await this.opts.presets.save({ name, ...texts });
    if (result.outcome === "saved") {
      this.presets = result.presets;
      this.paintPresets();
    } else {
      this.opts.onError(
        result.outcome === "refused"
          ? result.error
          : result.outcome === "unreadable"
            ? "Your presets could not be read just now, so nothing was saved over them. Try again."
            : `The preset could not be saved: ${result.error}`
      );
    }
  }

  private async deletePreset(): Promise<void> {
    const preset = presetMatching(this.presets, this.instructions());
    if (!preset) return;
    const result = await this.opts.presets.remove(preset.name);
    if (result.outcome === "saved") {
      this.presets = result.presets;
      this.paintPresets();
    } else if (result.outcome !== "refused") {
      this.opts.onError("That preset could not be deleted just now. Try again.");
    }
  }

  /** The form as the human left it, with the launcher's own fields filled in. */
  values(shared: Pick<QuickFormValues, "repo" | "maxAgents" | "idleKillMinutes" | "maxSpawnsPerHour">): QuickFormValues {
    const step = (s: QuickStep) => ({
      cli: this.rows[s].cli.value,
      model: this.rows[s].model.value,
      instructions: this.rows[s].instructions.value,
    });
    return {
      ...shared,
      mode: this.mode,
      root: { cli: this.rootCli.value, model: this.rootModel.value },
      task: this.task.value,
      planStep: this.planBox.checked,
      reviewStep: this.reviewBox.checked,
      base: this.base.value,
      rounds: numberOrNull(this.rounds),
      minutes: numberOrNull(this.minutes),
      steps: { plan: step("plan"), work: step("work"), review: step("review") },
      autoOps: this.perms.value === "auto",
    };
  }

  /** The CLIs the steps that are ON will run — what the launcher probes on
   *  PATH before it starts anything. */
  programs(): string[] {
    // A described run may open any of the three helpers, so all three CLIs
    // have to be there — and so does the one the agent itself runs on.
    if (this.mode === "describe") {
      return [...new Set([this.rootCli.value, ...QUICK_STEPS.map((s) => this.rows[s].cli.value)])];
    }
    const on = QUICK_STEPS.filter(
      (s) => s === "work" || (s === "plan" ? this.planBox.checked : this.reviewBox.checked)
    );
    return [...new Set(on.map((s) => this.rows[s].cli.value))];
  }

  /** Put the caret where `planQuickStart` said the problem is. `repo` is the
   *  launcher's own field, so it is not handled here. */
  focus(target: QuickFocus): void {
    if (target === "task") this.task.focus();
    else if (target === "rounds") this.rounds.focus();
    else if (target === "minutes") this.minutes.focus();
    else if (target === "root") this.rootCli.focus();
    else if (target !== "repo") this.rows[target].cli.focus();
  }
}
