// The quick task's launcher model (#3679): what a submit sends, what it
// refuses, and which CLIs each step may run on — the last pinned against the
// engine's own capability table rather than restated.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import {
  QUICK_MINUTES,
  QUICK_MODES,
  QUICK_ROOT_CLIS,
  QUICK_ROUNDS,
  QUICK_STEPS,
  QUICK_STEP_CLIS,
  QUICK_STEP_ROLE,
  planQuickStart,
  quickRootCliOptions,
  quickStepCli,
  quickStepCliOptions,
  quickStepOn,
  type QuickFormValues,
} from "../src/quickmodel.ts";
import { ORCH_CLIS } from "../src/orchclis.ts";
import { planPaneSetup, type PaneSetupInput } from "../src/panesetup.ts";
import { setupPreviewMark } from "../src/setuppreview.ts";

const KNOWN = ORCH_CLIS.map((c) => c.id);

/** A form a human could plausibly have left: a plan-less, reviewed run on
 *  claude, with something typed for the work and review steps. */
function form(over: Partial<QuickFormValues> = {}): QuickFormValues {
  return {
    repo: "C:/src/widgets",
    task: "  add a --json flag to the list command  ",
    mode: "steps",
    root: { cli: "claude", model: " opus " },
    planStep: false,
    reviewStep: true,
    base: " main ",
    rounds: 3,
    minutes: 240,
    steps: {
      plan: { cli: "claude", model: "", instructions: "plan it carefully" },
      work: { cli: "codex", model: " gpt-5 ", instructions: " keep the diff small " },
      review: { cli: "pi", model: "", instructions: "be strict about tests" },
    },
    maxAgents: 3,
    idleKillMinutes: 30,
    maxSpawnsPerHour: 20,
    autoOps: true,
    ...over,
  };
}

test("a submit builds exactly the orch_quick_start payload", () => {
  const plan = planQuickStart(form());
  assert.ok(plan.ok);
  assert.deepEqual(plan.request, {
    repo: "C:/src/widgets",
    task: "add a --json flag to the list command",
    // A steps run names no agent of its own: what the form is holding for the
    // other mode is not sent.
    mode: "steps",
    root: { cli: "", model: "", instructions: "" },
    plan_step: false,
    review_step: true,
    base: "main",
    max_review_rounds: 3,
    drive_timeout_minutes: 240,
    // The plan step is OFF, so the text the form is still holding for it is
    // not sent — text for a pane that will never open is not a request.
    plan: { cli: "claude", model: "", instructions: "" },
    work: { cli: "codex", model: "gpt-5", instructions: "keep the diff small" },
    review: { cli: "pi", model: "", instructions: "be strict about tests" },
    max_agents: 3,
    auto_ops: true,
    idle_kill_minutes: 30,
    max_spawns_per_hour: 20,
  });
});

test("a step's instructions are sent exactly when that step is on", () => {
  const on = planQuickStart(form({ planStep: true }));
  assert.ok(on.ok);
  assert.equal(on.request.plan.instructions, "plan it carefully");
  assert.equal(on.request.plan_step, true);

  const noReview = planQuickStart(form({ reviewStep: false }));
  assert.ok(noReview.ok);
  assert.equal(noReview.request.review.instructions, "", "a review that will not run is told nothing");
  assert.equal(noReview.request.work.instructions, "keep the diff small", "work is always on");
});

test("a described run asks for no task and sends none (#3723)", () => {
  // The form's task box is hidden in this mode, and whatever it still holds —
  // text typed before the mode was switched — is not a task to send: the
  // agent opens idle and is told in its own pane.
  for (const task of ["", "   \n ", "add a --json flag to the list command"]) {
    const plan = planQuickStart(form({ mode: "describe", task }));
    assert.ok(plan.ok, `a described run with task ${JSON.stringify(task)} is accepted`);
    assert.equal(plan.request.task, "", "and no task is sent");
    assert.equal(plan.request.mode, "describe");
  }
  // The control: the steps mode's rule did not move with it.
  const steps = planQuickStart(form({ task: " \n " }));
  assert.deepEqual([steps.ok, !steps.ok && steps.focus], [false, "task"]);
  const kept = planQuickStart(form());
  assert.equal(kept.ok && kept.request.task, "add a --json flag to the list command");
  // Everything else a described run needs is still asked for.
  const noRepo = planQuickStart(form({ mode: "describe", task: "", repo: " " }));
  assert.equal(!noRepo.ok && noRepo.focus, "repo");
  const noMinutes = planQuickStart(form({ mode: "describe", task: "", minutes: null }));
  assert.equal(!noMinutes.ok && noMinutes.focus, "minutes");
});

test("an empty repository or task is refused, naming the field", () => {
  const noRepo = planQuickStart(form({ repo: "   " }));
  assert.deepEqual([noRepo.ok, !noRepo.ok && noRepo.focus], [false, "repo"]);
  const noTask = planQuickStart(form({ task: " \n " }));
  assert.deepEqual([noTask.ok, !noTask.ok && noTask.focus], [false, "task"]);
  assert.match(!noTask.ok ? noTask.error : "", /Steps needs a task/);
  // The sentence this replaced told the human to describe the task, which is
  // now the mode the form opens on — so it must not come back as the refusal.
  assert.doesNotMatch(!noTask.ok ? noTask.error : "", /Describe the task/);
});

test("an untouched form starts: the opening mode needs nothing typed (#3876)", () => {
  // quickform.ts opens the form on QUICK_MODES[0] with every box empty. That
  // is the mode that needs no task, so a blank Start is accepted and sends none.
  const untouched = form({
    mode: QUICK_MODES[0],
    task: "",
    steps: {
      plan: { cli: "claude", model: "", instructions: "" },
      work: { cli: "codex", model: "", instructions: "" },
      review: { cli: "pi", model: "", instructions: "" },
    },
  });
  const plan = planQuickStart(untouched);
  assert.ok(plan.ok, plan.ok ? "" : plan.error);
  assert.equal(plan.ok && plan.request.mode, "describe");
  assert.equal(plan.ok && plan.request.task, "");
});

test("a number out of range is refused by name, never clamped", () => {
  for (const rounds of [0, 4, 1.5, null]) {
    const p = planQuickStart(form({ rounds }));
    assert.equal(p.ok, false, `rounds=${rounds} must be refused`);
    assert.equal(!p.ok && p.focus, "rounds");
    assert.match(!p.ok ? p.error : "", /whole number from 1 to 3/);
  }
  for (const minutes of [4, 1441, 60.5, null]) {
    const p = planQuickStart(form({ minutes }));
    assert.equal(p.ok, false, `minutes=${minutes} must be refused`);
    assert.equal(!p.ok && p.focus, "minutes");
    assert.match(!p.ok ? p.error : "", /5 to 1440/);
  }
  // The controls: both ends of both ranges are accepted, so "refuse" is not
  // "refuse everything".
  for (const [rounds, minutes] of [
    [QUICK_ROUNDS.min, QUICK_MINUTES.min],
    [QUICK_ROUNDS.max, QUICK_MINUTES.max],
  ]) {
    const p = planQuickStart(form({ rounds, minutes }));
    assert.ok(p.ok, `rounds=${rounds} minutes=${minutes}`);
    assert.deepEqual([p.request.max_review_rounds, p.request.drive_timeout_minutes], [rounds, minutes]);
  }
});

test("a run with no review step does not read the round bound", () => {
  // The rounds box is hidden when the review step is off; a stale or blank
  // value in it is not a reason to refuse a run that will never count a round.
  const p = planQuickStart(form({ reviewStep: false, rounds: null }));
  assert.ok(p.ok);
  assert.equal(p.request.max_review_rounds, QUICK_ROUNDS.default);
});

test("a step on a CLI that cannot host it is refused — but only if the step is on", () => {
  const steps = form().steps;
  const codexReview = planQuickStart(form({ steps: { ...steps, review: { ...steps.review, cli: "codex" } } }));
  assert.equal(codexReview.ok, false);
  assert.equal(!codexReview.ok && codexReview.focus, "review");
  assert.match(!codexReview.ok ? codexReview.error : "", /codex cannot run the review step/);

  const piPlan = planQuickStart(
    form({ planStep: true, steps: { ...steps, plan: { ...steps.plan, cli: "pi" } } })
  );
  assert.equal(!piPlan.ok && piPlan.focus, "plan");

  // The same two values with the step OFF are not a refusal.
  assert.ok(
    planQuickStart(form({ reviewStep: false, steps: { ...steps, review: { ...steps.review, cli: "codex" } } })).ok
  );
  assert.ok(planQuickStart(form({ planStep: false, steps: { ...steps, plan: { ...steps.plan, cli: "pi" } } })).ok);
});

test("work is always on, and the other two follow their checkboxes", () => {
  assert.equal(quickStepOn({ planStep: false, reviewStep: false }, "work"), true);
  assert.equal(quickStepOn({ planStep: false, reviewStep: true }, "plan"), false);
  assert.equal(quickStepOn({ planStep: true, reviewStep: false }, "plan"), true);
  assert.equal(quickStepOn({ planStep: true, reviewStep: false }, "review"), false);
});

test("a step's CLI falls back to one it can run on", () => {
  assert.equal(quickStepCli("work", "codex", KNOWN), "codex");
  assert.equal(quickStepCli("review", "codex", KNOWN), quickStepCliOptions("review", KNOWN)[0]);
  assert.equal(quickStepCli("plan", "pi", KNOWN), quickStepCliOptions("plan", KNOWN)[0]);
  assert.equal(quickStepCli("plan", "copilot", KNOWN), "copilot");
  // The options keep the launcher's own order.
  assert.deepEqual(
    quickStepCliOptions("work", KNOWN),
    KNOWN.filter((c) => QUICK_STEP_CLIS.work.includes(c))
  );
});

test("QUICK_STEP_CLIS is exactly what the engine's cli_can_host allows (#3679)", () => {
  // The form's per-step CLI lists and the backend's containment check are two
  // spellings of one fact, and a hand-typed copy fails silently both ways: a
  // CLI offered for a step it cannot host is refused at Create, and one that
  // gained a rung with no entry here stays hidden for no reason anyone would
  // find. So the sets are DERIVED from the Rust rather than restated — and the
  // extraction is itself an instrument, so it carries its own controls.
  const rust = readFileSync(new URL("../crates/loomux-engine/src/model.rs", import.meta.url), "utf8");

  // Each containment rung's rank.
  const rankFn = rust.slice(rust.indexOf("pub fn rank(self) -> u8"));
  const ranks = new Map(
    [...rankFn.slice(0, rankFn.indexOf("\n    }")).matchAll(/Containment::(\w+) => (\d+)/g)].map((m) => [
      m[1],
      Number(m[2]),
    ])
  );
  assert.deepEqual([...ranks.keys()].sort(), ["NoEdits", "None", "ReadOnly"], "the rank ladder was read");

  // What each step's class asks for.
  const containment = rust.slice(rust.indexOf("pub fn containment(self) -> Containment"));
  const wants = new Map<string, string>();
  for (const m of containment.slice(0, containment.indexOf("\n    }")).matchAll(/((?:Role::\w+(?: \| )?)+) => Containment::(\w+)/g)) {
    for (const role of m[1].matchAll(/Role::(\w+)/g)) wants.set(role[1].toLowerCase(), m[2]);
  }
  for (const step of QUICK_STEPS) {
    assert.ok(wants.has(QUICK_STEP_ROLE[step]), `the engine names a containment for ${QUICK_STEP_ROLE[step]}`);
  }

  // What each CLI can be held to.
  const table = rust.slice(rust.indexOf("pub const CLI_CAPS: &[CliCaps] = &["));
  const caps = [...table.slice(0, table.indexOf("\n];")).matchAll(/cli: "([a-z]+)",[\s\S]*?max_containment: Containment::(\w+)/g)].map(
    (m) => [m[1], m[2]] as const
  );
  assert.ok(caps.length >= 6, `the capability table was read (${caps.length} rows)`);
  // The launcher offers a SUBSET of what the engine can spawn (it has no row
  // for a CLI it cannot populate a model list for), so the mirror is checked
  // against the engine's table and the launcher's list only has to sit inside it.
  const engineClis = caps.map(([cli]) => cli);
  for (const cli of KNOWN) {
    assert.ok(engineClis.includes(cli), `the launcher offers ${cli}, which the engine's table does not name`);
  }

  for (const step of QUICK_STEPS) {
    const want = ranks.get(wants.get(QUICK_STEP_ROLE[step])!)!;
    const allowed = caps.filter(([, max]) => want <= ranks.get(max)!).map(([cli]) => cli);
    assert.deepEqual(
      [...QUICK_STEP_CLIS[step]].sort(),
      allowed.sort(),
      `the ${step} step offers exactly the CLIs the engine can hold to its class`
    );
  }
  // The property this exists for, stated on the table itself: the three lists
  // really differ, so a mirror that offered every CLI for every step would not
  // pass by accident.
  assert.ok(QUICK_STEP_CLIS.plan.length < QUICK_STEP_CLIS.review.length);
  assert.ok(QUICK_STEP_CLIS.review.length < QUICK_STEP_CLIS.work.length);

  // The agent a described run is given to (#3679): its class is in the same
  // table, and the list of CLIs that can host it is derived the same way.
  assert.ok(wants.has("quick"), "the engine names a containment for the quick root's class");
  const rootWant = ranks.get(wants.get("quick")!)!;
  const rootAllowed = caps.filter(([, max]) => rootWant <= ranks.get(max)!).map(([cli]) => cli);
  assert.deepEqual([...QUICK_ROOT_CLIS].sort(), rootAllowed.sort(), "the root offers exactly the CLIs the engine can hold to its class");
  assert.deepEqual([...QUICK_ROOT_CLIS].sort(), [...engineClis].sort(), "which today is every CLI: the class is not clamped");
});

// ── the launcher's own planner and preview, for this kind ───────────────────

test("the launcher's planner asks a quick task for a repository and nothing else", () => {
  const input = (repo: string): PaneSetupInput => ({
    kind: "quick",
    agentId: "claude",
    isCustom: false,
    builtinCommand: "claude",
    customCommand: "",
    count: 1,
    repo,
    worktree: "",
    name: "",
    autopilot: false,
    shellKind: "powershell",
    sshProfile: null,
  });
  assert.deepEqual(planPaneSetup(input("  C:/src/widgets ")), {
    ok: true,
    plan: { kind: "quick", repo: "C:/src/widgets" },
  });
  const refused = planPaneSetup(input("  "));
  assert.equal(refused.ok, false);
  assert.equal(!refused.ok && refused.focus, "repo");
  assert.match(!refused.ok ? refused.error : "", /quick task needs a repository/);
});

test("the setup card names no agent for a quick task — it launches up to three", () => {
  const preview = (kind: "quick" | "agent") =>
    setupPreviewMark({ kind, agentId: "claude", customCommand: "", sshCli: "", orchestratorCli: null });
  assert.equal(preview("quick"), null);
  // The control: the same picker state on the Agent kind DOES draw a mark, so
  // the null above is this kind's rule and not a preview that draws nothing.
  assert.notEqual(preview("agent"), null);
});

// ── describe mode (#3679 way 2) ─────────────────────────────────────────────

test("a described run sends the agent it runs on, and no task and no instructions", () => {
  const plan = planQuickStart(form({ mode: "describe", planStep: true, reviewStep: false }));
  assert.ok(plan.ok);
  assert.equal(plan.request.mode, "describe");
  assert.deepEqual(plan.request.root, { cli: "claude", model: "opus", instructions: "" });
  // The two switches are the steps mode's. A described run records neither —
  // whether to plan or review is its agent's call — whatever the form still holds.
  assert.equal(plan.request.plan_step, false);
  assert.equal(plan.request.review_step, false);
  // The three rows are sent as the helpers' CLIs and models, with no text: in
  // this mode the human says nothing on the form — the task is told to the
  // agent in its pane.
  assert.deepEqual(plan.request.plan, { cli: "claude", model: "", instructions: "" });
  assert.deepEqual(plan.request.work, { cli: "codex", model: "gpt-5", instructions: "" });
  assert.deepEqual(plan.request.review, { cli: "pi", model: "", instructions: "" });
  assert.equal(plan.request.task, "", "the form's text is not sent as a task in this mode");
});

test("a described run checks every helper's CLI, because its agent may open any of them", () => {
  // In steps mode an OFF step on a CLI that cannot host it is fine: that pane
  // never opens. In describe mode there is no off — codex cannot be held to a
  // reviewer's class, and the agent may ask for a reviewer.
  const steps = form({ reviewStep: false, steps: { ...form().steps, review: { cli: "codex", model: "", instructions: "" } } });
  assert.ok(planQuickStart(steps).ok, "the control: the same row is accepted while its step is off");
  const described = planQuickStart({ ...steps, mode: "describe" });
  assert.equal(described.ok, false);
  assert.equal(!described.ok && described.focus, "review");
  assert.match(!described.ok ? described.error : "", /codex cannot be the review helper/);

  const noRoot = planQuickStart(form({ mode: "describe", root: { cli: "", model: "" } }));
  assert.equal(!noRoot.ok && noRoot.focus, "root");
  assert.match(!noRoot.ok ? noRoot.error : "", /Pick the CLI the task runs on/);
  const badRoot = planQuickStart(form({ mode: "describe", root: { cli: "emacs", model: "" } }));
  assert.match(!badRoot.ok ? badRoot.error : "", /emacs cannot run a described task/);
  // …and a steps run does not read the root row at all.
  assert.ok(planQuickStart(form({ root: { cli: "emacs", model: "" } })).ok);
});

test("a described run reads the round bound even with the review switch off", () => {
  // Its agent is told the bound either way, so a blank box is a refusal here
  // where in steps mode, with no review step, it is not read.
  assert.ok(planQuickStart(form({ reviewStep: false, rounds: null })).ok, "the control");
  const described = planQuickStart(form({ mode: "describe", reviewStep: false, rounds: null }));
  assert.equal(!described.ok && described.focus, "rounds");
  const ok = planQuickStart(form({ mode: "describe", reviewStep: false, rounds: 2 }));
  assert.equal(ok.ok && ok.request.max_review_rounds, 2);
});

test("the root's CLI picker offers only what this build's launcher has a row for", () => {
  assert.deepEqual(quickRootCliOptions(["claude", "codex", "vim"]), ["claude", "codex"]);
  assert.deepEqual(quickRootCliOptions([]), []);
});
