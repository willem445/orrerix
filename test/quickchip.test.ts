// The quick run's status chip and pane-menu model (#3679), plus the poll that
// keeps the chip current.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import {
  QUICK_HELD_REASONS,
  QUICK_STATES,
  quickChipView,
  quickIsOver,
  quickIsWorking,
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
  assert.ok(states.length >= 8 && reasons.length >= 18, "both enums were read (positive control)");
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

test("the tones separate working, waiting on the human, finished and stopped", () => {
  const tone = (state: string) => quickChipView(run({ state }), null)?.tone;
  assert.deepEqual(
    ["plan-wait", "work-wait", "review-wait", "fix-wait", "held", "satisfied", "cancelled"].map(tone),
    ["working", "working", "working", "working", "held", "done", "stopped"]
  );
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
