// The quick task's launcher form, as a pure model (#3679) — DOM-free, so the
// rules that decide what a submit sends are testable without a form.
//
// A quick task is one short-lived run: plan (optional) → work → review
// (optional), relayed by the engine with no orchestrator pane. The form is the
// whole of a run's policy — there is no workflow file behind it — so this
// module is where "what did the human ask for" becomes the exact
// `orch_quick_start` payload, and where a value the backend would refuse is
// refused first, by name, while the form is still open.
//
// Design note: docs/design/quick-orchestration.md.

/** One step of a run. `work` is always on; the other two are checkboxes. */
export type QuickStep = "plan" | "work" | "review";

/** Every step, in the order a run takes them. */
export const QUICK_STEPS: readonly QuickStep[] = ["plan", "work", "review"];

/** What the form calls each step. */
export const QUICK_STEP_LABEL: Record<QuickStep, string> = {
  plan: "Plan",
  work: "Work",
  review: "Review",
};

/** The capability class each step's pane runs as. A planner is held read-only
 *  and a reviewer is denied the editing tools, exactly as in a full
 *  orchestration — the step is what picks the class, never the form. */
export const QUICK_STEP_ROLE = {
  plan: "planner",
  work: "worker",
  review: "reviewer",
} as const;

/** Which CLIs can host each step — a MIRROR of the engine's `cli_can_host` over
 *  `CLI_CAPS` (`crates/loomux-engine/src/model.rs`), pinned against that file by
 *  `test/quickmodel.test.ts` so the two cannot drift.
 *
 *  It is data read off a table, not a list someone chose: a planner needs a CLI
 *  that can be held read-only, a reviewer one that can be denied edits, and a
 *  worker needs neither. The backend re-checks at start and again at spawn;
 *  this only stops the form OFFERING a combination that would be refused. */
export const QUICK_STEP_CLIS: Record<QuickStep, readonly string[]> = {
  plan: ["claude", "copilot", "gemini", "opencode"],
  work: ["claude", "copilot", "gemini", "opencode", "pi", "codex"],
  review: ["claude", "copilot", "gemini", "opencode", "pi"],
};

/** The two ways to run a quick task (#3679).
 *
 *  - `describe`: ONE agent opens idle and is given the task in its own pane
 *    (#3723); it decides for itself whether to plan, who works and who
 *    reviews, and the panes it opens are its helpers. The form asks for no
 *    task in this mode.
 *  - `steps`: orrerix relays between a planner, a worker and a reviewer itself.
 *
 *  `QUICK_MODES` lists them in the order the form shows them under **How**,
 *  and its FIRST entry is the mode the form opens on: `describe`, the one that
 *  needs no task, so submitting the form untouched opens one waiting
 *  agent (#3876). */
export type QuickMode = "steps" | "describe";

export const QUICK_MODES: readonly QuickMode[] = ["describe", "steps"];

/** The CLIs that can host the agent a described run is given to. Every CLI:
 *  that pane is not clamped — it is a full working pane, like an
 *  orchestrator's — so no CLI is ruled out by a deny tier it cannot enforce.
 *  A mirror of the engine's `cli_can_host(_, Role::Quick)`, pinned beside the
 *  step lists in `test/quickmodel.test.ts`. */
export const QUICK_ROOT_CLIS: readonly string[] = ["claude", "copilot", "gemini", "opencode", "pi", "codex"];

/** The review-round bound: the review driver's own `1..=3`. */
export const QUICK_ROUNDS = { min: 1, max: 3, default: 3 } as const;

/** The run's overall time bound, in minutes: the review driver's own
 *  `5..=1440`, with a quick run's own default of four hours. */
export const QUICK_MINUTES = { min: 5, max: 1440, default: 240 } as const;

/** One step's row on the form. */
export interface QuickStepValues {
  cli: string;
  /** Empty = the CLI's default model for the step's class. */
  model: string;
  /** The human's instructions for this step. Empty = the role's own. */
  instructions: string;
}

/** The form, as the human left it. Numbers are `null` when their field is
 *  blank or not a number, so "unset" and "typed something we must refuse" stay
 *  two different things. */
export interface QuickFormValues {
  repo: string;
  task: string;
  mode: QuickMode;
  /** The CLI and model of the one agent a described run is given to. Read
   *  only in `describe` mode. */
  root: { cli: string; model: string };
  planStep: boolean;
  reviewStep: boolean;
  base: string;
  rounds: number | null;
  minutes: number | null;
  steps: Record<QuickStep, QuickStepValues>;
  maxAgents: number;
  idleKillMinutes: number;
  maxSpawnsPerHour: number;
  autoOps: boolean;
}

/** The `orch_quick_start` payload — field for field the backend's
 *  `QuickStartRequest`, which is why it is snake_case. */
export interface QuickStartRequest {
  repo: string;
  task: string;
  mode: QuickMode;
  root: { cli: string; model: string; instructions: string };
  plan_step: boolean;
  review_step: boolean;
  base: string;
  max_review_rounds: number;
  drive_timeout_minutes: number;
  plan: { cli: string; model: string; instructions: string };
  work: { cli: string; model: string; instructions: string };
  review: { cli: string; model: string; instructions: string };
  max_agents: number;
  auto_ops: boolean;
  idle_kill_minutes: number;
  max_spawns_per_hour: number;
}

/** Which field to focus when the plan is refused. */
export type QuickFocus = "repo" | "task" | "rounds" | "minutes" | "root" | QuickStep;

export type QuickPlan =
  | { ok: true; request: QuickStartRequest }
  | { ok: false; error: string; focus: QuickFocus };

/** Whether `step` runs under these values. Work always does. */
export function quickStepOn(
  values: Pick<QuickFormValues, "planStep" | "reviewStep"> & { mode?: QuickMode },
  step: QuickStep
): boolean {
  // In a described run the three rows are not steps the human switched on:
  // they are the helpers the agent MAY open, and it may open any of them. So
  // all three are "on" — each one's CLI has to be able to host its role.
  if (values.mode === "describe") return true;
  return step === "work" || (step === "plan" ? values.planStep : values.reviewStep);
}

/** The root CLIs this build's launcher can actually offer. */
export function quickRootCliOptions(known: readonly string[]): string[] {
  return known.filter((cli) => QUICK_ROOT_CLIS.includes(cli));
}

/** The CLIs the form offers for `step`, out of the ones this build knows —
 *  `known` is the launcher's own CLI list, in its menu order, which is the
 *  order the result keeps. */
export function quickStepCliOptions(step: QuickStep, known: readonly string[]): string[] {
  return known.filter((cli) => QUICK_STEP_CLIS[step].includes(cli));
}

/** `cli` if `step` can run on it, else the first CLI that can — what a step's
 *  select falls back to when the human changes the work CLI to one the step
 *  cannot use. */
export function quickStepCli(step: QuickStep, cli: string, known: readonly string[]): string {
  const options = quickStepCliOptions(step, known);
  return options.includes(cli) ? cli : (options[0] ?? "");
}

/** A whole number inside `[min, max]`, or the sentence that refuses it.
 *
 *  Refused, never clamped: the backend clamps, so a `0` typed for "rounds"
 *  would start a run with one round and nobody would know a zero had been
 *  asked for. Saying so here is the launcher's own rule for a numeric field
 *  (`sshDiscardedFieldError`), applied to two more of them. */
function boundedInt(
  value: number | null,
  bounds: { min: number; max: number },
  what: string
): { ok: true; value: number } | { ok: false; error: string } {
  if (value === null || !Number.isInteger(value) || value < bounds.min || value > bounds.max) {
    return { ok: false, error: `${what} must be a whole number from ${bounds.min} to ${bounds.max}.` };
  }
  return { ok: true, value };
}

/** Validate the form and shape the request. Pure — no probe, no IPC. */
export function planQuickStart(values: QuickFormValues): QuickPlan {
  const repo = values.repo.trim();
  if (!repo) {
    return { ok: false, error: "A quick task needs a repository — pick one first.", focus: "repo" };
  }
  const described = values.mode === "describe";
  // #3723: the task is the steps mode's. There the engine relays it into the
  // first pane and has no agent to tell, so it is required. A described run's
  // agent opens idle and is told in its own pane, so nothing is asked for and
  // nothing is sent — whatever the hidden box may still be holding.
  const task = described ? "" : values.task.trim();
  if (!described && !task) {
    return { ok: false, error: "Steps needs a task — type one, or pick Describe it under How to start with no task.", focus: "task" };
  }
  if (described && !QUICK_ROOT_CLIS.includes(values.root.cli)) {
    return {
      ok: false,
      error: values.root.cli
        ? `${values.root.cli} cannot run a described task — pick one of: ${QUICK_ROOT_CLIS.join(", ")}.`
        : "Pick the CLI the task runs on.",
      focus: "root",
    };
  }
  for (const step of QUICK_STEPS) {
    if (!quickStepOn(values, step)) continue;
    const cli = values.steps[step].cli;
    if (!QUICK_STEP_CLIS[step].includes(cli)) {
      const label = QUICK_STEP_LABEL[step].toLowerCase();
      const what = described ? `be the ${label} helper` : `run the ${label} step`;
      return {
        ok: false,
        error: cli
          ? `${cli} cannot ${what} — pick one of: ${QUICK_STEP_CLIS[step].join(", ")}.`
          : described
            ? `Pick a CLI for the ${label} helper.`
            : `Pick a CLI for the ${label} step.`,
        focus: step,
      };
    }
  }
  // A run with no review step never reads the round bound, so a blank field
  // there is not a reason to refuse it.
  let rounds: number = QUICK_ROUNDS.default;
  // A described run quotes the bound to its agent whether or not it reviews,
  // so the box is read in that mode regardless of the review checkbox.
  if (values.reviewStep || described) {
    const r = boundedInt(values.rounds, QUICK_ROUNDS, "Review rounds");
    if (!r.ok) return { ok: false, error: r.error, focus: "rounds" };
    rounds = r.value;
  }
  const minutes = boundedInt(values.minutes, QUICK_MINUTES, "The time bound (minutes)");
  if (!minutes.ok) return { ok: false, error: minutes.error, focus: "minutes" };

  const row = (step: QuickStep) => ({
    cli: values.steps[step].cli,
    model: values.steps[step].model.trim(),
    // A step that is off sends no instructions: the form may still be holding
    // text for it, and text for a pane that will never open is not a request.
    // …and instructions belong to the steps mode alone. In a described run
    // the human gives no text here at all — the task is said in the agent's
    // pane — so the helpers run on their roles' own instructions, and the
    // agent that opens them says what it wants of each.
    instructions: !described && quickStepOn(values, step) ? values.steps[step].instructions.trim() : "",
  });
  return {
    ok: true,
    request: {
      repo,
      task,
      mode: described ? "describe" : "steps",
      root: {
        cli: described ? values.root.cli : "",
        model: described ? values.root.model.trim() : "",
        instructions: "",
      },
      // The two step switches are the steps mode's. A described run records
      // neither: whether to plan or review is its agent's call.
      plan_step: !described && values.planStep,
      review_step: !described && values.reviewStep,
      base: values.base.trim(),
      max_review_rounds: rounds,
      drive_timeout_minutes: minutes.value,
      plan: row("plan"),
      work: row("work"),
      review: row("review"),
      max_agents: values.maxAgents,
      auto_ops: values.autoOps,
      idle_kill_minutes: values.idleKillMinutes,
      max_spawns_per_hour: values.maxSpawnsPerHour,
    },
  };
}
