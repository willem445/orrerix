// What a pane's header says about the quick run it belongs to (#3679), and
// what its menu offers — as pure functions of the run's status, so every state
// has a label a test can read without a DOM.
//
// A quick run has no task board and no orchestrator pane, so this chip is the
// whole of "what is it doing": the step the run is on, the review round, and
// whether it is waiting on the human. It is header chrome only — it never
// changes a pane's size (CLAUDE.md constraint 1).
//
// Design note: docs/design/quick-orchestration.md.

/** Every state a run's record can be in — the engine's `QuickState::ALL`
 *  (`crates/loomux-engine/src/quickdrive.rs`), in its order. Mirrored here and
 *  pinned against that file by `test/quickchip.test.ts`, so a tenth state is a
 *  red test rather than a pane whose chip reads `undefined`. */
export const QUICK_STATES = [
  "plan-wait",
  "work-wait",
  "review-wait",
  "fix-wait",
  "root-wait",
  "root-idle",
  "held",
  "satisfied",
  "cancelled",
] as const;
export type QuickState = (typeof QUICK_STATES)[number];

/** Every reason a run can be parked for — the engine's `QuickHeld::ALL`, pinned
 *  the same way. */
export const QUICK_HELD_REASONS = [
  "review-limit",
  "plan-stalled",
  "fix-stalled",
  "lane-stalled",
  "drive-stalled",
  "planner-blocked",
  "worker-blocked",
  "reviewer-blocked",
  "root-blocked",
  "planner-gone",
  "worker-gone",
  "reviewer-gone",
  "root-gone",
  "cap-refused",
  "unresumable",
  "provider-limit",
  "messaged",
  "restart",
] as const;
export type QuickHeldReason = (typeof QUICK_HELD_REASONS)[number];

/** `orch_quick_status`'s answer. Everything past `exists` is absent when the
 *  group has no run. */
export interface QuickStatus {
  group_id: string;
  exists: boolean;
  state?: string;
  held_reason?: string | null;
  /** The engine's own sentence for the hold. */
  held_line?: string | null;
  /** Only on the answer to a `step`: another step held the group, so this
   *  status was read while a pane may still be opening. */
  busy?: boolean;
  /** What the pane said, or what was refused. */
  held_note?: string;
  round?: number;
  max_review_rounds?: number;
  plan_step?: boolean;
  review_step?: boolean;
  task?: string;
  brief_pending?: boolean;
  can_handoff?: boolean;
  turn?: { side: string; agent: string } | null;
  panes?: Record<string, { agent: string; live: boolean }>;
  /** Whether this is a described run: one agent, given its tasks in its pane. */
  described?: boolean;
  /** How many tasks a described run's agent has begun (#3723). */
  task_seq?: number;
  /** What a described run's agent said when its last task finished. */
  last_note?: string;
  /** Only on a row of the unfinished-runs list: the run's repository. */
  repo?: string;
}

/** How a chip is tinted: something is being done, the run is waiting on the
 *  human, it finished, it was stopped — or, for a described run with no task
 *  in progress, nothing is happening and nothing is owed (#3723). `idle` has
 *  no rule of its own in the stylesheet on purpose: it is the chip's plain,
 *  untinted look, which is what "nothing to see" should look like. */
export type QuickTone = "working" | "held" | "done" | "stopped" | "idle";

export interface QuickChipView {
  /** The chip's text. */
  label: string;
  /** Its tooltip: the task, and for a hold what to do about it. */
  title: string;
  tone: QuickTone;
  /** This pane holds the turn — the chip is drawn solid on it. */
  onTurn: boolean;
}

const isQuickState = (s: string | undefined): s is QuickState =>
  (QUICK_STATES as readonly string[]).includes(s ?? "");

/** Whether a run in this state is still doing something on its own — the one
 *  question the status poll asks. A parked run moves only when the human
 *  resumes or stops it, and a finished one never moves, so neither is polled:
 *  when the task ends, nothing keeps asking.
 *
 *  An IDLE described run is not working either (#3723): its agent is waiting
 *  for the human, and a pane left waiting must cost no timer. It starts
 *  working when its agent opens a helper, which the backend announces
 *  (`orch-quick-changed`) — that event, not a poll, is what wakes its chip. */
export function quickIsWorking(status: QuickStatus | null): boolean {
  if (!status?.exists || !isQuickState(status.state)) return false;
  return (
    status.state !== "held" &&
    status.state !== "satisfied" &&
    status.state !== "cancelled" &&
    status.state !== "root-idle"
  );
}

/** Whether a described run has no task in progress (#3723): its agent's pane
 *  is open — or opening — and waiting to be told what to do. */
export function quickIsIdle(status: QuickStatus | null): boolean {
  return status?.exists === true && status.state === "root-idle";
}

/** How long the backend waits for a new pane to bind before it gives the
 *  spawn up, in seconds. A mirror of `BIND_TIMEOUT` in
 *  `src-tauri/src/orchestration/tuning.rs`, pinned against that file by
 *  `test/quickchip.test.ts`. */
export const QUICK_BIND_TIMEOUT_S = 20;

/** How often the launcher re-asks while a run's first pane is opening. */
export const QUICK_OPENING_WAIT_MS = 500;

/** How many times it re-asks before it stops waiting.
 *
 *  **It has to outlast the bind deadline, and it did not.** A pane that never
 *  binds is not a failure the backend knows about until `BIND_TIMEOUT` has
 *  passed: until then the step that is opening it holds the group and every
 *  answer is `busy`. A budget shorter than that deadline — it was ten seconds
 *  against twenty — gives up while the answer is still "opening", so the form
 *  said "still opening" and the failure that arrived ten seconds later was
 *  shown nowhere. Ten seconds past the deadline leaves room for the step to
 *  park the run and for one more ask to read why. */
export const QUICK_OPENING_TRIES = Math.ceil(((QUICK_BIND_TIMEOUT_S + 10) * 1000) / QUICK_OPENING_WAIT_MS);

/** One row of the launcher's list of runs that have not ended. */
export interface QuickRunRow {
  group: string;
  /** The task, on one line. */
  task: string;
  /** The repository's folder name. */
  repo: string;
  /** Where the run stands, in the chip's own words. */
  label: string;
  /** Why a held run is held — the engine's sentence — or "". */
  why: string;
  /** Whether Resume applies: only a held run can be resumed. */
  canResume: boolean;
}

/** The last segment of a path, whichever separator it uses. */
function folderName(path: string): string {
  const parts = path.split(/[\\/]+/).filter(Boolean);
  return parts[parts.length - 1] ?? "";
}

/** The rows of the launcher's unfinished-runs list (#3679).
 *
 *  Resume and Stop are on a pane's menu, and a run can outlive every pane it
 *  had — so this list is the way back to one. It is built from what the
 *  backend read off the run records, and it drops anything that is not a run
 *  still worth acting on: a group with no run, a state this build does not
 *  know, a run that has ended — and an idle described run (#3723), which has
 *  nothing in progress to resume or stop. The backend already leaves that one
 *  out; it is dropped here too so the list's rule is in one readable place.
 *
 *  **Resume is offered only where it applies.** A working run is not resumed —
 *  it has a pane holding the turn, or it is about to be parked for not having
 *  one — so its row carries Stop alone. */
export function quickRunRows(list: readonly QuickStatus[]): QuickRunRow[] {
  const rows: QuickRunRow[] = [];
  for (const status of list) {
    const view = quickChipView(status, null);
    if (!view || quickIsOver(status) || quickIsIdle(status)) continue;
    // A described run's task was given in its pane, so there is none to show;
    // the row says so rather than reading as a run somebody forgot to name.
    const typed = (status.task ?? "").split(/\s+/).filter(Boolean).join(" ");
    const task = typed || (status.described ? "A task given in its pane" : "");
    rows.push({
      group: status.group_id,
      task: task.length > 90 ? `${task.slice(0, 89)}…` : task,
      repo: folderName(status.repo ?? ""),
      label: view.label,
      why: status.state === "held" ? (status.held_line ?? "") : "",
      canResume: status.state === "held",
    });
  }
  return rows;
}

/** What the launcher makes of the status its first `step` answered. */
export type QuickLaunchVerdict =
  | { kind: "opened" }
  | { kind: "opening" }
  | { kind: "failed"; why: string };

/** Read the first step's answer: did the run's first pane open?
 *
 *  Three answers, because "no live pane" has two causes the launcher must not
 *  treat alike. A pane that could not be opened leaves a run nobody can see,
 *  and the launcher stops it. A step that found the group `busy` — the poll
 *  tick claimed it first and is mid-spawn — shows the same empty status while
 *  the pane is on its way, and stopping THAT run leaves the pane to arrive on
 *  a cancelled one (#3681 review W4). So `busy` is "opening": wait, never
 *  stop. A run that has already parked or ended is past waiting for, whatever
 *  else the status says.
 *
 *  An idle described run is one that can still be opening (#3723): its first
 *  pane is its agent's, opened with the run in `root-idle`. So the wait is
 *  asked of "working or idle", not of "working" — otherwise a busy step on a
 *  described run would read as a failure and its pane would arrive on a run
 *  the launcher had just stopped. */
export function quickLaunchVerdict(status: QuickStatus): QuickLaunchVerdict {
  if (Object.values(status.panes ?? {}).some((p) => p.live)) return { kind: "opened" };
  if (status.busy === true && (quickIsWorking(status) || quickIsIdle(status))) return { kind: "opening" };
  return {
    kind: "failed",
    why: status.held_note || status.held_line || "its first pane could not be opened",
  };
}

/** Whether the run has ended — approved, done or stopped. */
export function quickIsOver(status: QuickStatus | null): boolean {
  return status?.state === "satisfied" || status?.state === "cancelled";
}

/** The verb a state is shown as. Total over `QUICK_STATES` by construction —
 *  `Record<QuickState, …>` makes a missing state a compile error. */
const STATE_VERB: Record<QuickState, string> = {
  "plan-wait": "planning",
  "work-wait": "working",
  "review-wait": "reviewing",
  "fix-wait": "fixing",
  "root-wait": "running",
  "root-idle": "idle",
  held: "held",
  satisfied: "approved",
  cancelled: "stopped",
};

const STATE_TONE: Record<QuickState, QuickTone> = {
  "plan-wait": "working",
  "work-wait": "working",
  "review-wait": "working",
  "fix-wait": "working",
  "root-wait": "working",
  "root-idle": "idle",
  held: "held",
  satisfied: "done",
  cancelled: "stopped",
};

/** The chip for one pane of a run, or `null` when there is nothing to show —
 *  the group has no run, or the status names a state this build does not know
 *  (shown as nothing rather than as a guess).
 *
 *  `agentId` is the pane's own agent id; the chip is solid on the pane that
 *  holds the turn and outlined on the others. */
export function quickChipView(status: QuickStatus | null, agentId: string | null): QuickChipView | null {
  if (!status?.exists || !isQuickState(status.state)) return null;
  const state = status.state;
  const reviewing = status.review_step === true;
  // The round is shown while the worker or the reviewer holds the turn in a
  // run that HAS a review step; a plan, a finish and a hold do not carry one.
  const rounds =
    reviewing && (state === "work-wait" || state === "review-wait" || state === "fix-wait")
      ? ` ${status.round ?? 1}/${status.max_review_rounds ?? 1}`
      : "";
  let verb = STATE_VERB[state];
  if (state === "satisfied" && !reviewing) verb = "done";
  if (state === "held") verb = `held: ${status.held_reason ?? "unknown"}`;
  const label = `quick${rounds} · ${verb}`;

  const task = (status.task ?? "").split(/\s+/).filter(Boolean).join(" ");
  const head = task ? `Quick task: ${task}` : "Quick task";
  let title = head;
  if (state === "held") {
    const why = status.held_line ? ` — ${status.held_line}` : "";
    const note = status.held_note ? ` (${status.held_note})` : "";
    title = `${head}\nHeld${why}${note}. Right-click this pane to resume or stop the run.`;
  } else if (state === "satisfied") {
    title = `${head}\nFinished. Nothing was merged; the panes are yours to read or close.`;
  } else if (state === "cancelled") {
    title = `${head}\nStopped. The panes are yours to read or close.`;
  } else if (state === "root-idle") {
    // Nothing is in progress. Say what the pane is for, when its clock
    // starts, and — once a task has finished here — what its agent said.
    const last = (status.last_note ?? "").split(/\s+/).filter(Boolean).join(" ");
    const before = (status.task_seq ?? 0) > 0 ? `\nThe last task finished${last ? `: ${last}` : "."}` : "";
    title =
      `${head}\nIdle — tell this agent what you want done, in its pane. ` +
      `The time limit starts when it opens its first helper.${before}` +
      "\nClose the pane when you are done with it; there is nothing to stop.";
  } else if (status.brief_pending) {
    title = `${head}\nHanding over to the next step…`;
  }
  return {
    label,
    title,
    tone: STATE_TONE[state],
    onTurn: agentId !== null && agentId !== "" && status.turn?.agent === agentId,
  };
}

/** One thing a human can do to a run from a pane's menu. `action` is the word
 *  `orch_quick_control` takes. */
export interface QuickMenuEntry {
  action: "resume" | "stop" | "handoff" | "note";
  label: string;
}

/** What a pane's menu offers for the run it belongs to. Empty once the run has
 *  ended — there is nothing left to do to it — and for a group with no run.
 *
 *  A parked run offers Resume first, because that is the answer the hold's
 *  notice asks for; a working one offers the hand-off first, named for the
 *  direction it would go. Stop is always last.
 *
 *  An idle described run offers nothing (#3723). There is no task to stop,
 *  hand off or annotate, and the human is one keystroke from its agent:
 *  closing the pane is the whole of ending it. */
export function quickMenuEntries(status: QuickStatus | null): QuickMenuEntry[] {
  if (!status?.exists || !isQuickState(status.state) || quickIsOver(status) || quickIsIdle(status)) return [];
  const entries: QuickMenuEntry[] = [];
  if (status.state === "held") {
    entries.push({ action: "resume", label: "Resume quick run" });
  } else if (status.can_handoff) {
    entries.push({
      action: "handoff",
      label: status.state === "review-wait" ? "Send back to the worker now" : "Hand to the reviewer now",
    });
  }
  entries.push({ action: "note", label: "Add note to run…" });
  entries.push({ action: "stop", label: "Stop quick run" });
  return entries;
}
