// The quick run's status chip and pane-menu model (#3679), plus the poll that
// keeps the chip current.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import {
  QUICK_BIND_TIMEOUT_S,
  QUICK_HELD_REASONS,
  QUICK_OPENING_TRIES,
  QUICK_OPENING_WAIT_MS,
  quickRunRows,
  QUICK_STATES,
  quickChipView,
  quickIsIdle,
  quickIsOver,
  quickIsWorking,
  quickLaunchVerdict,
  quickMenuEntries,
  type QuickStatus,
} from "../src/quickchip.ts";
import { QUICK_POLL_MS, QuickRuns } from "../src/quickruns.ts";
import { buildPaneMenu, type PaneConnectState } from "../src/panemenu.ts";

const run = (over: Partial<QuickStatus> = {}): QuickStatus => ({
  group_id: "widgets-1a2b3c4d",
  exists: true,
  state: "work-wait",
  held_reason: null,
  round: 1,
  max_review_rounds: 3,
  plan_step: false,
  review_step: true,
  task: "add a --json flag\nto the list command",
  brief_pending: false,
  can_handoff: true,
  turn: { side: "worker", agent: "w-1" },
  panes: { worker: { agent: "w-1", live: true } },
  ...over,
});

test("the state and hold-reason lists are the engine's own, in its order (#3679)", () => {
  // Two mirrors of two closed enums. Read out of the Rust rather than restated,
  // so a ninth state or a nineteenth reason reddens here instead of rendering
  // a chip that says `undefined`.
  const rust = readFileSync(new URL("../crates/loomux-engine/src/quickdrive.rs", import.meta.url), "utf8");
  const words = (ty: string) => [...rust.matchAll(new RegExp(`${ty}::\\w+ => "([a-z-]+)",`, "g"))].map((m) => m[1]);
  const states = words("QuickState");
  const reasons = words("QuickHeld");
  assert.ok(states.length >= 9 && reasons.length >= 18, "both enums were read (positive control)");
  assert.ok(states.includes("root-idle"), "the idle state a described run starts in is one of them (#3723)");
  assert.deepEqual(states, [...QUICK_STATES]);
  assert.deepEqual(reasons, [...QUICK_HELD_REASONS]);
});

test("every state has a chip, and its words say what the run is doing", () => {
  const labels = new Map(QUICK_STATES.map((s) => [s, quickChipView(run({ state: s, held_reason: "messaged" }), "w-1")?.label]));
  assert.deepEqual(Object.fromEntries(labels), {
    "plan-wait": "quick · planning",
    "work-wait": "quick 1/3 · working",
    "review-wait": "quick 1/3 · reviewing",
    "fix-wait": "quick 1/3 · fixing",
    "root-wait": "quick · running",
    "root-idle": "quick · idle",
    held: "quick · held: messaged",
    satisfied: "quick · approved",
    cancelled: "quick · stopped",
  });
  // The population control: one label per state, none of them missing.
  assert.equal(labels.size, QUICK_STATES.length);
  assert.ok([...labels.values()].every((l) => typeof l === "string" && l.length > 0));
});

test("the round is shown only where there is a review to count", () => {
  assert.equal(quickChipView(run({ state: "review-wait", round: 2 }), null)?.label, "quick 2/3 · reviewing");
  assert.equal(
    quickChipView(run({ state: "work-wait", review_step: false }), null)?.label,
    "quick · working",
    "a run with no review step has no rounds"
  );
  assert.equal(
    quickChipView(run({ state: "satisfied", review_step: false }), null)?.label,
    "quick · done",
    "and it was never approved by anyone — it is done"
  );
});

test("every hold reason reaches the chip in its own words", () => {
  for (const reason of QUICK_HELD_REASONS) {
    const view = quickChipView(run({ state: "held", held_reason: reason, held_line: "the reviewer reported blocked" }), "w-1");
    assert.equal(view?.label, `quick · held: ${reason}`);
    assert.equal(view?.tone, "held");
    assert.match(view?.title ?? "", /the reviewer reported blocked/);
    assert.match(view?.title ?? "", /resume or stop/);
  }
});

test("the chip is solid on the pane that holds the turn, and only on it", () => {
  assert.equal(quickChipView(run(), "w-1")?.onTurn, true);
  assert.equal(quickChipView(run(), "rev-2")?.onTurn, false);
  assert.equal(quickChipView(run(), null)?.onTurn, false);
  // Nobody holds a turn in a parked or finished run — and an unopened side's
  // empty agent id must not match a pane with no agent of its own.
  assert.equal(quickChipView(run({ state: "held", turn: null }), "w-1")?.onTurn, false);
  assert.equal(quickChipView(run({ turn: { side: "worker", agent: "" } }), "")?.onTurn, false);
});

test("the tones separate working, idle, waiting on the human, finished and stopped", () => {
  const tone = (state: string) => quickChipView(run({ state }), null)?.tone;
  // Every state, so a tenth cannot be tinted by accident.
  assert.deepEqual(
    QUICK_STATES.map((s) => [s, tone(s)]),
    [
      ["plan-wait", "working"],
      ["work-wait", "working"],
      ["review-wait", "working"],
      ["fix-wait", "working"],
      ["root-wait", "working"],
      ["root-idle", "idle"],
      ["held", "held"],
      ["satisfied", "done"],
      ["cancelled", "stopped"],
    ]
  );
});

test("an idle described run's chip says what the pane is for, and never claims a turn (#3723)", () => {
  const idle = (over: Partial<QuickStatus> = {}) =>
    run({
      state: "root-idle",
      described: true,
      review_step: false,
      task: "",
      turn: null,
      can_handoff: false,
      panes: { root: { agent: "quick-1", live: true } },
      ...over,
    });
  const fresh = quickChipView(idle(), "quick-1");
  assert.equal(fresh?.label, "quick · idle");
  assert.equal(fresh?.onTurn, false, "nobody holds a turn while nothing is in progress");
  assert.match(fresh?.title ?? "", /^Quick task\nIdle — tell this agent what you want done, in its pane\./);
  assert.match(fresh?.title ?? "", /The time limit starts when it opens its first helper\./);
  assert.match(fresh?.title ?? "", /there is nothing to stop/);
  assert.doesNotMatch(fresh?.title ?? "", /last task/, "a pane never given a task has no last one");

  // Once a task has finished in the pane, the tooltip carries what was said.
  const after = quickChipView(idle({ task_seq: 2, last_note: "the flag is on\nagent/one" }), "quick-1");
  assert.match(after?.title ?? "", /The last task finished: the flag is on agent\/one/);
  assert.equal(after?.label, "quick · idle", "the label does not change with history");
  assert.match(quickChipView(idle({ task_seq: 1, last_note: "" }), null)?.title ?? "", /The last task finished\./);

  // The pane still opening is idle too — the hand-over line is a working run's.
  assert.doesNotMatch(quickChipView(idle({ brief_pending: true }), null)?.title ?? "", /Handing over/);
});

test("a group with no run, or a state this build does not know, shows nothing", () => {
  assert.equal(quickChipView(null, "w-1"), null);
  assert.equal(quickChipView({ group_id: "g", exists: false }, "w-1"), null);
  assert.equal(quickChipView(run({ state: "merging" }), "w-1"), null, "never a guess at an unknown state");
  assert.deepEqual(quickMenuEntries(run({ state: "merging" })), []);
});

test("the tooltip carries the task on one line", () => {
  assert.match(quickChipView(run(), "w-1")?.title ?? "", /^Quick task: add a --json flag to the list command$/);
});

test("the pane menu offers what the run's state allows", () => {
  const labels = (s: QuickStatus | null) => quickMenuEntries(s).map((e) => `${e.action}:${e.label}`);
  assert.deepEqual(labels(run({ state: "work-wait", can_handoff: true })), [
    "handoff:Hand to the reviewer now",
    "note:Add note to run…",
    "stop:Stop quick run",
  ]);
  assert.deepEqual(labels(run({ state: "review-wait", can_handoff: true })), [
    "handoff:Send back to the worker now",
    "note:Add note to run…",
    "stop:Stop quick run",
  ]);
  // No hand-off where the backend says there is none to make.
  assert.deepEqual(labels(run({ state: "plan-wait", can_handoff: false })), [
    "note:Add note to run…",
    "stop:Stop quick run",
  ]);
  // A parked run: Resume first, and no hand-off — it is resumed first.
  assert.deepEqual(labels(run({ state: "held", held_reason: "worker-blocked", can_handoff: false })), [
    "resume:Resume quick run",
    "note:Add note to run…",
    "stop:Stop quick run",
  ]);
  // A described run with a task in progress has no hand-off — its agent
  // decides who works — and keeps the note and the stop.
  assert.deepEqual(labels(run({ state: "root-wait", described: true, can_handoff: false })), [
    "note:Add note to run…",
    "stop:Stop quick run",
  ]);
  // An idle one offers nothing (#3723): no task to stop, hand off or annotate,
  // and closing the pane is the whole of ending it.
  assert.deepEqual(labels(run({ state: "root-idle", described: true, can_handoff: false })), []);
  // A finished run and a group with no run offer nothing.
  assert.deepEqual(labels(run({ state: "satisfied" })), []);
  assert.deepEqual(labels(run({ state: "cancelled" })), []);
  assert.deepEqual(labels(null), []);
  assert.deepEqual(labels({ group_id: "g", exists: false }), []);
});

test("only a working run is one worth polling", () => {
  const working = QUICK_STATES.filter((s) => quickIsWorking(run({ state: s })));
  assert.deepEqual(working, ["plan-wait", "work-wait", "review-wait", "fix-wait", "root-wait"]);
  assert.deepEqual(
    QUICK_STATES.filter((s) => quickIsOver(run({ state: s }))),
    ["satisfied", "cancelled"]
  );
  assert.equal(quickIsWorking(null), false);
  assert.equal(quickIsWorking({ group_id: "g", exists: false }), false);
  // Idle is its own kind (#3723): not working, so not polled; not over, so its
  // chip stays; and the only state that answers `quickIsIdle`.
  assert.deepEqual(
    QUICK_STATES.filter((s) => quickIsIdle(run({ state: s }))),
    ["root-idle"]
  );
  assert.equal(quickIsIdle(null), false);
  assert.equal(quickIsIdle({ group_id: "g", exists: false, state: "root-idle" }), false);
  // The three kinds a state can be, plus held, cover every state exactly once.
  for (const s of QUICK_STATES) {
    const st = run({ state: s });
    const kinds = [quickIsWorking(st), quickIsIdle(st), quickIsOver(st), s === "held"];
    assert.equal(kinds.filter(Boolean).length, 1, `${s}: ${kinds}`);
  }
});

// ── the poll ────────────────────────────────────────────────────────────────

/** A `QuickRuns` over a scripted backend, a hand-cranked timer, and a window
 *  whose visibility the test sets. */
function harness(script: Record<string, QuickStatus[]>) {
  const applied: string[] = [];
  const reads: string[] = [];
  let tick: (() => void) | null = null;
  let timers = 0;
  const window = { visible: true, changed: (): void => {} };
  const runs = new QuickRuns({
    status: async (group) => {
      reads.push(group);
      const queue = script[group];
      if (!queue || queue.length === 0) throw new Error("no answer scripted");
      return queue.length > 1 ? queue.shift()! : queue[0];
    },
    apply: (group, status) => applied.push(`${group}:${status.state}`),
    setInterval: (fn) => {
      tick = fn;
      timers += 1;
      return timers;
    },
    clearInterval: () => {
      tick = null;
    },
    gate: {
      visibility: {
        visible: () => window.visible,
        subscribe: (onChange) => {
          window.changed = onChange;
          return () => {
            window.changed = () => {};
          };
        },
      },
      // The gate's own recheck ticker, which only runs while a wanted poll is
      // held back by a hidden window. Inert here: the test fires the
      // visibility change itself.
      recheck: () => () => {},
    },
  });
  const show = (visible: boolean): void => {
    window.visible = visible;
    window.changed();
  };
  return { runs, applied, reads, armed: () => tick !== null, timers: () => timers, show };
}

test("a working run is polled, and the poll stops the moment it ends", async () => {
  const h = harness({ g: [run({ state: "work-wait" }), run({ state: "review-wait" }), run({ state: "satisfied" })] });
  assert.equal(h.runs.polling, false, "nothing is polled before a run is known");

  await h.runs.refresh("g");
  assert.deepEqual(h.applied, ["g:work-wait"]);
  assert.equal(h.runs.polling, true);

  await h.runs.tick();
  assert.deepEqual(h.applied, ["g:work-wait", "g:review-wait"]);
  assert.equal(h.runs.polling, true);

  await h.runs.tick();
  assert.deepEqual(h.applied.at(-1), "g:satisfied");
  assert.equal(h.runs.polling, false, "the task ended, so nothing keeps asking");
  assert.equal(h.armed(), false);

  // …and it stays stopped: a later tick reads nothing at all.
  const before = h.reads.length;
  await h.runs.tick();
  assert.equal(h.reads.length, before);
  assert.equal(h.runs.statusOf("g")?.state, "satisfied", "the finished run is still known, for its chip");
});

test("an idle described run is not polled, and the backend's announcement is what wakes it (#3723)", async () => {
  const idle = run({ state: "root-idle", described: true, turn: null });
  const h = harness({
    widgets: [idle, run({ state: "root-wait", described: true }), run({ state: "root-idle", described: true, turn: null })],
  });
  await h.runs.refresh("widgets");
  assert.equal(h.runs.polling, false, "a pane left waiting costs no timer");
  assert.equal(h.timers(), 0, "none was ever armed");
  h.reads.length = 0;
  await h.runs.tick();
  assert.deepEqual(h.reads, [], "and a stray tick reads nothing");
  assert.deepEqual(h.applied, ["widgets:root-idle"], "its chip was painted once, by the read that found it");

  // The agent opened a helper: the backend says the run moved, and the window
  // re-reads it. From here it is a working run like any other.
  await h.runs.refresh("widgets");
  assert.equal(h.runs.statusOf("widgets")?.state, "root-wait");
  assert.equal(h.runs.polling, true, "a task in progress is polled");

  // The task finished: the poll itself sees idle, and stops.
  await h.runs.tick();
  assert.equal(h.runs.statusOf("widgets")?.state, "root-idle");
  assert.equal(h.runs.polling, false, "idle again, and nothing keeps asking");
});

test("a parked run is not polled; the human's own verb is what refreshes it", async () => {
  const h = harness({ g: [run({ state: "held", held_reason: "worker-blocked", turn: null })] });
  await h.runs.refresh("g");
  assert.equal(h.runs.polling, false, "a hold moves only when the human moves it");
  assert.deepEqual(h.applied, ["g:held"]);

  // Resume answers the new status, which is accepted without a second read.
  const reads = h.reads.length;
  h.runs.accept("g", run({ state: "fix-wait" }));
  assert.equal(h.reads.length, reads);
  assert.equal(h.runs.polling, true);
  assert.deepEqual(h.applied.at(-1), "g:fix-wait");
});

test("one timer serves every run, and a group with no run is not kept", async () => {
  const h = harness({
    a: [run({ group_id: "a", state: "work-wait" })],
    b: [run({ group_id: "b", state: "review-wait" })],
    plain: [{ group_id: "plain", exists: false }],
  });
  await h.runs.refresh("a");
  await h.runs.refresh("b");
  await h.runs.refresh("plain");
  assert.equal(h.timers(), 1, "a second working run does not start a second timer");
  assert.equal(h.runs.statusOf("plain"), null, "an ordinary group is not a quick run");
  assert.deepEqual(h.applied, ["a:work-wait", "b:review-wait"], "and nothing was painted for it");

  h.reads.length = 0;
  await h.runs.tick();
  assert.deepEqual(h.reads.sort(), ["a", "b"], "the tick reads the working runs and only those");

  h.runs.forget("a");
  assert.equal(h.runs.polling, true, "b is still working");
  h.runs.forget("b");
  assert.equal(h.runs.polling, false);
});

test("a hidden window polls nothing, and catches up once when it comes back", async () => {
  const h = harness({ g: [run({ state: "work-wait" }), run({ state: "review-wait" })] });
  await h.runs.refresh("g");
  assert.equal(h.runs.polling, true);

  h.show(false);
  assert.equal(h.runs.polling, false, "the timer is stopped outright, not left firing into a hidden window");
  assert.equal(h.armed(), false);

  const reads = h.reads.length;
  h.show(true);
  assert.equal(h.runs.polling, true);
  // The catch-up read is asynchronous; let it land.
  await new Promise((r) => setImmediate(r));
  assert.equal(h.reads.length, reads + 1, "exactly one catch-up read on the way back");
  assert.deepEqual(h.applied.at(-1), "g:review-wait");
});

test("the poll runs at its declared cadence", () => {
  assert.equal(QUICK_POLL_MS, 4000);
});

test("a read that fails changes nothing that was known", async () => {
  const h = harness({ g: [run({ state: "work-wait" })] });
  await h.runs.refresh("g");
  // The backend stops answering for another group; the known run is untouched.
  assert.equal(await h.runs.refresh("missing"), null);
  assert.equal(h.runs.statusOf("g")?.state, "work-wait", "could not look is not no run");
  assert.equal(h.runs.polling, true);
  assert.equal(h.runs.statusOf(null), null);
});

// ── the pane menu, composed ─────────────────────────────────────────────────

test("a quick run's items reach the pane menu, fired at the run's group", () => {
  const base: PaneConnectState = {
    group: "widgets-1a2b3c4d",
    agentId: "w-1",
    name: "quick: work",
    role: "worker",
    channelId: null,
    canSend: true,
    senderId: null,
    senderName: null,
    agentCli: "claude",
    sessionId: null,
    workdir: null,
    watched: false,
  };
  const withRun = buildPaneMenu(
    { ...base, quick: { group: "widgets-1a2b3c4d", entries: quickMenuEntries(run({ state: "held", held_reason: "messaged" })) } },
    null
  );
  const quickItems = withRun.filter((i) => i.action?.kind === "quick-control");
  assert.deepEqual(
    quickItems.map((i) => [i.label, i.action]),
    [
      ["Resume quick run", { kind: "quick-control", group: "widgets-1a2b3c4d", action: "resume" }],
      ["Add note to run…", { kind: "quick-control", group: "widgets-1a2b3c4d", action: "note" }],
      ["Stop quick run", { kind: "quick-control", group: "widgets-1a2b3c4d", action: "stop" }],
    ]
  );
  // The control: the same pane with no run — and one whose run has ended —
  // carries none of them, and is otherwise the same menu.
  const labels = (items: ReturnType<typeof buildPaneMenu>) => items.filter((i) => !i.separator).map((i) => i.label);
  const without = buildPaneMenu(base, null);
  assert.equal(without.filter((i) => i.action?.kind === "quick-control").length, 0);
  const ended = buildPaneMenu(
    { ...base, quick: { group: "widgets-1a2b3c4d", entries: quickMenuEntries(run({ state: "satisfied" })) } },
    null
  );
  assert.deepEqual(labels(ended), labels(without));
  assert.deepEqual(
    labels(withRun).filter((l) => !quickItems.some((q) => q.label === l)),
    labels(without),
    "the run's items are added to the menu, and nothing else on it moves"
  );
});

test("the launcher reads a busy first step as a pane still opening, never as a failure", () => {
  // The step opened the pane itself.
  assert.deepEqual(quickLaunchVerdict(run()), { kind: "opened" });
  // Another step held the group: no pane yet, and the run is working. This is
  // the answer the launcher used to stop the run on.
  const noPane = { panes: { worker: { agent: "", live: false } }, brief_pending: true };
  assert.deepEqual(quickLaunchVerdict(run({ ...noPane, busy: true })), { kind: "opening" });
  // The same empty status WITHOUT busy is a pane that could not be opened…
  assert.deepEqual(quickLaunchVerdict(run(noPane)), {
    kind: "failed",
    why: "its first pane could not be opened",
  });
  // …and it carries the reason when the run parked on one.
  assert.deepEqual(
    quickLaunchVerdict(
      run({ ...noPane, state: "held", held_reason: "cap-refused", held_line: "the group is at its limit", held_note: "max_agents reached" })
    ),
    { kind: "failed", why: "max_agents reached" }
  );
  assert.equal(
    quickLaunchVerdict(run({ ...noPane, state: "held", held_reason: "cap-refused", held_line: "the group is at its limit" })).kind === "failed" &&
      (quickLaunchVerdict(run({ ...noPane, state: "held", held_reason: "cap-refused", held_line: "the group is at its limit" })) as { why: string }).why,
    "the group is at its limit"
  );
});

test("an idle described run's first step is read like any other run's (#3723)", () => {
  const idle = (over: Partial<QuickStatus> = {}) =>
    run({ state: "root-idle", described: true, turn: null, panes: { root: { agent: "", live: false } }, brief_pending: true, ...over });
  // Its agent's pane is in: opened, though no task has begun.
  assert.deepEqual(quickLaunchVerdict(idle({ panes: { root: { agent: "quick-1", live: true } }, brief_pending: false })), {
    kind: "opened",
  });
  // Another step holds the group and is opening it. Reading this as a failure
  // is what would stop the run under its own pane.
  assert.deepEqual(quickLaunchVerdict(idle({ busy: true })), { kind: "opening" });
  // No pane and nobody opening one: a failure, with the reason once it parked.
  assert.deepEqual(quickLaunchVerdict(idle()), { kind: "failed", why: "its first pane could not be opened" });
  assert.deepEqual(
    quickLaunchVerdict(idle({ state: "held", held_reason: "unresumable", held_note: "the group already has a live root pane" })),
    { kind: "failed", why: "the group already has a live root pane" }
  );
});

test("busy does not outrank what the status itself says", () => {
  const noPane = { panes: {}, brief_pending: true };
  // A live pane is a live pane, busy or not.
  assert.deepEqual(quickLaunchVerdict(run({ busy: true })), { kind: "opened" });
  // A run that parked or ended is past waiting for: busy must not keep the
  // launcher waiting on a run that will open nothing.
  for (const state of ["held", "satisfied", "cancelled"] as const) {
    assert.equal(quickLaunchVerdict(run({ ...noPane, busy: true, state })).kind, "failed", state);
  }
  // And a group with no run at all is a failure, not a wait.
  assert.equal(quickLaunchVerdict({ group_id: "g", exists: false, busy: true }).kind, "failed");
});

// ── the unfinished-runs list, and the wait for a first pane (#3679) ──────────

test("the launcher's list shows the runs that can still be acted on, and Resume only where it applies", () => {
  const rows = quickRunRows([
    run({ group_id: "held", state: "held", held_reason: "worker-gone", held_line: "the worker's pane closed before it reported", repo: "C:\\src\\widgets", panes: {} }),
    run({ group_id: "working", state: "review-wait", repo: "/home/me/src/gadgets/" }),
    run({ group_id: "root", state: "root-wait", described: true, review_step: false, task: "", repo: "/home/me/src/gadgets" }),
    run({ group_id: "idle", state: "root-idle", described: true, review_step: false, task: "", turn: null }),
    run({ group_id: "done", state: "satisfied" }),
    run({ group_id: "stopped", state: "cancelled" }),
    { group_id: "plain", exists: false },
    run({ group_id: "future", state: "a-state-this-build-does-not-know" }),
  ]);
  assert.deepEqual(
    rows.map((r) => r.group),
    ["held", "working", "root"],
    "ended, absent, unknown and IDLE runs are not listed — an idle one has nothing to resume or stop"
  );
  const [held, working, root] = rows;
  assert.equal(held.canResume, true);
  assert.equal(held.why, "the worker's pane closed before it reported");
  assert.equal(held.repo, "widgets", "a Windows path's folder");
  assert.equal(working.canResume, false, "a working run is not resumed — only stopped");
  assert.equal(working.why, "");
  assert.equal(working.repo, "gadgets", "a trailing separator is not a folder name");
  assert.equal(root.label, "quick · running", "a described run reads as running");
  assert.equal(root.task, "A task given in its pane", "its task is not on the record, and the row says where it is");
  assert.equal(working.task, "add a --json flag to the list command", "a steps run's row still shows its task");
  // The task is one line, and a long one is cut rather than wrapped into the row.
  assert.equal(held.task, "add a --json flag to the list command");
  const long = quickRunRows([run({ task: "x".repeat(200) })])[0];
  assert.equal(long.task.length, 90);
  assert.ok(long.task.endsWith("…"));
});

test("the wait for a first pane outlasts the backend's bind deadline", () => {
  // A pane that never binds is not a failure the backend knows about until its
  // own deadline passes; until then every answer is `busy`. A budget shorter
  // than that deadline gives up while the answer is still "opening", which is
  // how a failed launch came to be shown as "still opening" and nothing else.
  const rust = readFileSync(new URL("../src-tauri/src/orchestration/tuning.rs", import.meta.url), "utf8");
  const m = rust.match(/const BIND_TIMEOUT: Duration = Duration::from_secs\((\d+)\);/);
  assert.ok(m, "tuning.rs still declares BIND_TIMEOUT in seconds");
  assert.equal(QUICK_BIND_TIMEOUT_S, Number(m[1]), "the mirror is the backend's own figure");
  const budgetMs = QUICK_OPENING_TRIES * QUICK_OPENING_WAIT_MS;
  assert.ok(budgetMs >= (QUICK_BIND_TIMEOUT_S + 5) * 1000, `${budgetMs} ms leaves room past a ${QUICK_BIND_TIMEOUT_S} s deadline`);
  assert.ok(budgetMs <= 60_000, "and is still a wait a human sits through");
});

test("a run whose tab closed stops being polled, and the others do not", async () => {
  const h = harness({
    a: [run({ group_id: "a", state: "work-wait" })],
    b: [run({ group_id: "b", state: "review-wait" })],
  });
  await h.runs.refresh("a");
  await h.runs.refresh("b");
  assert.equal(h.runs.polling, true);

  // Tab A closed: only B is still bound to a tab.
  h.runs.retain((group) => group === "b");
  assert.equal(h.runs.statusOf("a"), null, "nothing is kept for a run with no tab");
  assert.notEqual(h.runs.statusOf("b"), null);
  assert.equal(h.runs.polling, true, "B is still working");
  h.reads.length = 0;
  await h.runs.tick();
  assert.deepEqual(h.reads, ["b"], "and only B is read");

  h.runs.retain(() => false);
  assert.equal(h.runs.polling, false, "with no run left there is no timer");
  // Keeping everything changes nothing — the common tab change is not a close.
  await h.runs.refresh("a");
  h.runs.retain(() => true);
  assert.notEqual(h.runs.statusOf("a"), null);
});
