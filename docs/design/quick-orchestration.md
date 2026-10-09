# Quick orchestration — a plan, work, review run with no orchestrator

Issues #3679 and #3723. User page: `docs/features/quick-orchestration.md`.

A **quick task** is one short-lived run for one task: an optional plan, the
work, an optional review, and a bounded loop between the worker and the
reviewer. It has no orchestrator pane, no task board, no issue queue and no
merge gate. orrerix relays between the steps itself and tells the human when the
run finishes, parks, or needs them.

There are two ways to run one. In the **steps** mode ("way 1" in the issue)
orrerix relays between the steps itself; §1–§12 describe it. In the
**describe** mode ("way 2") one agent opens idle, is given its tasks in its own
pane, and decides for itself whether to plan and review; §15 describes it and
the capability class it needed, and §17 argues its lifecycle — the idle start,
what begins a task, and what a second one means. §16 is how a run is reached
when none of its panes is left, which applies to both.

## 1. Why a state machine in the engine, and not a small orchestrator

The relay between a worker and a reviewer is mechanical: hand the work over,
hand the findings back, count the rounds, stop. An LLM pane doing that costs a
resident context per hop and adds a party that can decide to do something else.
So the relay is a state machine — `crates/loomux-engine/src/quickdrive.rs` — and
the panes it relays between are ordinary worker, reviewer and planner panes with
their ordinary containment.

It is a sibling of the review driver and the plan driver rather than a use of
either. Those two read GitHub: a PR's head and checks, verdict files, an
issue's labels. A quick run's facts are a pane's `report` and whether that pane
is alive, and nothing else — which is what lets it need neither a PR nor a
commit. What it does share is the review driver's *delivery* (§6), its
`DriveLimits`, and the split both drivers use: a pure `decide` over values in
the engine crate, and wiring in `src-tauri` that makes no decision.

## 2. The group

A quick run gets a group of its own, minted by `create_group_ex` like every
other group and holding **no root pane**. Three things about it are deliberate.

- **`advanced_orchestrator: false`.** A repository's workflow file is a consent
  surface: the launcher previews the roster it declares and the human agrees to
  it. A quick run has no preview, so it never opens the file. This is the lead
  pane's argument (`lead-pane.md`), unchanged.
- **The roster is the built-in worker, reviewer and planner**, on the CLIs the
  human picked per step, each carrying that step's instructions as an inline
  persona. That is exactly what a workflow file's `prompt:` is: append mode, and
  no `allow:` from any source. The text is typed by the operator on their own
  launcher, so it is not a new trust root.
- **A `quickrun` marker file** in the group dir says what the group is. The roster
  cannot: three ordinary blocks look like any group whose human pinned the same
  CLIs. `create_group_ex` clears the marker on every claim of the id, beside the
  `lead` marker and for its reason.

`next_group_id` picks a group id by liveness, and a quick group can have no live
pane while its run is still resumable — it is parked, or its first pane has not
opened. So `next_group_id` also skips a group whose quick run has not ended.
Without that, the next launch on the repository was handed the run's group and
overwrote its record. An idle described run (§17) has not ended either, and its
group is held the same way until its root's pane goes.

## 3. The states

Exactly one pane **holds the turn** at a time. The run moves only on that
pane's `report`.

| State | Who holds the turn | Leaves on |
| --- | --- | --- |
| `plan-wait` | planner | its `done` → `work-wait` |
| `work-wait` | worker | its `done` → `review-wait`, or → `satisfied` when the review step is off |
| `review-wait` | reviewer | `approved` → `satisfied`; `request_changes` → `fix-wait`, or → `held` on the last round |
| `fix-wait` | worker | its `done` → `review-wait` |
| `root-wait` | the root of a described run | its `done` → `root-idle` (§15, §17) |
| `root-idle` | nobody | the root's first helper → `root-wait`; Stop, or its pane closing → `cancelled` (§17) |
| `held` | nobody | Resume → the state it came from; Stop → `cancelled` |
| `satisfied` | nobody | terminal |
| `cancelled` | nobody | terminal |

`quickdrive::transition` lists every arc and refuses the rest; a unit test walks
every pair of states against that list.

The states are of four kinds, and each state is exactly one: **working** (the
five `…-wait` states — somebody holds the turn), **parked** (`held`), **idle**
(`root-idle`) and **terminal**. Only a working run is read against a clock or
parked by a restart, and only a working run stays on the tick's list — an idle
one is on it just until its root's pane has opened. Only a working or parked
run is "unfinished".
`root-idle` is the one state that is none of the other three, which is why it
is asked about on its own (`is_idle`) rather than folded into a neighbour.

**A reviewer approves by saying `approved` and by no other word.** Its plain
`done`, or any other outcome, is `request_changes`. The one exit that would hand
unreviewed work to a human as approved must not be a reviewer forgetting an
argument, so the rule is in `QuickSignal::from_report` and again in `decide`.

### Holds

A hold is a hand-back to the human, with one closed reason. Each raises one
notice (§9).

| Reason | Cause |
| --- | --- |
| `review-limit` | `request_changes` on the last review round the run allows |
| `plan-stalled`, `lane-stalled`, `fix-stalled` | the planner, reviewer or fixing worker did not report inside 60 minutes |
| `drive-stalled` | the run reached its overall time bound |
| `planner-blocked`, `worker-blocked`, `reviewer-blocked` | that pane reported `blocked` |
| `planner-gone`, `worker-gone`, `reviewer-gone` | the pane holding the turn closed before it reported |
| `cap-refused` | the next pane was refused by the group's live-agent cap |
| `unresumable` | the next pane could not be opened for any other reason |
| `provider-limit` | the provider of the pane holding the turn reported a usage limit |
| `messaged` | a pane of the run called `message_orchestrator` |
| `restart` | orrerix restarted under a working run |

`root-blocked` and `root-gone` are a described run's (§15): its root reported
`blocked`, or its root's pane closed before it reported.

A pane closing is a hold only when it is the pane **holding the turn**. A worker
whose pane is reaped while the reviewer works is not a problem: the next
hand-back re-opens its session (§6).

### Bounds

| Bound | Value | Read by |
| --- | --- | --- |
| review rounds | 1–3, default 3 | the `review-wait` arc; the round being answered is the last when `review_rounds + 1` reaches it |
| run time | 5–1440 minutes, default 240 | every working state, last; in a described run it is a TASK's, counted from when the task begins (§17) |
| planner / reviewer / fix turn | 60 minutes | that state's own clock |

The first pass (`work-wait`) has no clock of its own. It *is* the task, and the
only honest limit on it is the one the human set for the whole run.

The plan named a `state-stalled` hold beside the three named stalls. It is not
here: the review driver needs both a per-lane and a per-state clock because
several lanes share `review-wait`, while a quick run has one pane per state, so
the two clocks are the same interval and the hold is named for the side that
went quiet.

A state's clock starts when its brief is **delivered**, not when the arc is
taken, so a pane is not charged for the time it took to open.

**A resume is a fresh grant.** It re-stamps the run's clock, and a run resumed
off `review-limit` gets its rounds back and goes to the worker — the reviewer
has already answered, and the findings are on disk. Resuming straight back onto
the limit that parked the run would make the button a no-op.

## 4. The record

`<group-dir>/quick_drive.json`, schema version 1, written through
`fsatomic::atomic_write`. One run per file: a quick group is minted for one run.
A described run's tasks are not runs of their own — the one record is re-armed
for each (§17.4).

Unknown fields are preserved at all three levels (file, run, pane). An unknown
state or hold-reason word is refused, a missing version is refused, and so is a
missing `review_rounds` — a defaulted counter would hand back a whole fresh
budget, which is `reviewdrive::Counters`' own rule.

The run's own documents live in `<group-dir>/quick/`:

| File | Written when | By |
| --- | --- | --- |
| `plan.md` | the planner reports `done` | the planner's `summary` |
| `round-<n>.md` | a reviewer reports `request_changes` | the reviewer's `summary` |
| `messages.md` | a pane calls `message_orchestrator` | appended, one line per message |

`<n>` is `reviews_total`, which never resets, rather than the round counter a
resume clears: round four's file must not overwrite round one's. It is a `u32`,
which is the whole of that file name's path argument, and `tests/pathseg.rs`
carries the row that says so.

Both documents are written by the `report` interception, **before the tool
answers**. They are the one part of a report the next pane cannot get from
anywhere else, so they go to disk while the caller is still in its turn and can
be told where they went.

`messages.md` is the one file a pane can write as often as it likes: any pane in
the group may call `message_orchestrator`, whether or not the run still wants
it, with a body of up to a megabyte. So each message is cut to the 20,000
characters a brief inlines, and the file stops taking messages at one megabyte.
A message that was not kept is audited as not saved. Nothing reads the file back
into a pane, so the bound is on disk and not on anyone's context.

## 5. Interception

`report` asks its owners in a fixed order: the review driver, the plan driver,
then the quick run (`qd_owner`). The three sets are disjoint by construction —
both drivers need the advanced orchestrator a quick group is minted without —
and the order is stated rather than relied on.

In a quick group `qd_owner` always answers, because there is no orchestrator
pane for a report to fall through to:

| Caller | What happens to its `report` |
| --- | --- |
| the pane holding the turn | becomes the run's signal; a step is kicked |
| another pane of the run | recorded; told it is not its turn |
| any pane, run held | recorded; told the run is held |
| any pane, run ended | recorded; told the run has ended |
| a described run's root, run idle | recorded; told no task is in progress |
| an agent the run does not own | recorded; told nothing is listening |

Every one is audited `qd-consumed`. None is typed into another pane as-is.

`message_orchestrator` cannot be delivered either. It is appended to
`messages.md`, and a message from a current pane of a working run parks the run
on `messaged`. That is a change from the review driver, which only *notices* a
message and delivers it unchanged — there, the recipient exists.

Both tools' descriptions say so. A tool that behaves differently for a reason
the caller cannot see from its arguments reads as a regression to whoever hits
it.

A signal names the side that sent it, and a step ignores one whose side no
longer holds the turn. The human's own verbs also drop the pending signal. The
two together are what stop a worker's `done`, sent a moment before a forced
hand-off, being read in `review-wait` as the reviewer's.

## 6. The step, and the hand-over

`qd_drive_group` is the only function that moves a run. It is entered from the
poll tick (the seventh step of `gh_poll_tick`, and the only caller that reads
the clocks), from the interception (a consumed report kicks a step on a thread
of its own, so a hop does not wait up to thirty seconds for the next wake), and
from the human's commands.

**No lock is held across a spawn or a delivery.** `qd_state_lock` spans a
load-modify-store of the record and nothing else, which is what lets it be a
ranked leaf (`lockorder::QUICK_DRIVE`, 860). The facts a decision needs from the
registry — a pane's status sits behind `agents` — are read before the lock is
taken. What a held lock would otherwise provide is provided by `QdClaim`: one
caller steps one group at a time, and a second caller finds the claim taken and
does nothing. `qd_mem` (870) holds the in-memory maps and is never held across
anything.

A state change sets `brief_pending`; `decide` answers nothing while it is set.
The step then renders the brief from the record and delivers it:

1. the side has a session → the review driver's own ladder, called rather than
   copied: `rd_reuse_pane` (a live idle pane on the session), then
   `rd_take_over_pane` (any live pane on it), then `rd_spawn` (a new pane
   resuming it);
2. the side has a live pane whose CLI has not reported a session yet → the brief
   is typed into that pane by its id;
3. the side has never been opened → its first pane.

A failed delivery parks the run with the refusal quoted.

**Where a side's session comes from** is three places, asked in order: the
registry's live entry for the pane, the run's own record, and the group's
roster. The third is not a nicety. The record learns a session at a hand-over,
from the spawn's own answer, and that answer is empty for every CLI that mints
its session id after boot — four of the six. The registry learns the id later
and writes it to the roster. After a restart no agent is in memory, so for a
first-pass worker, a planner or a first reviewer on one of those CLIs the roster
is the only place the session is written down. A pane whose CLI closed before it
ever reported one has nothing to re-open, and the hold says that.

A **planner** is resumed by a spawn with no worktree and no workspace override,
which puts it back in the repository it read. The review driver's `rd_spawn`
resolves a dedicated workspace first, through a function written for the two
roles that must never land in the main clone; a planner is not one of them.

Rendering the brief from the record, rather than storing it, is what makes a
resume and a restart re-deliver "the pending brief" without a second copy to
keep in step.

### The reviewer's workspace

The reviewer is opened **in the worker's own worktree**, with no worktree of its
own (`cwd_override`, `use_worktree: false`). It reads uncommitted work where it
is, so a review needs no commit, no push and no PR. This is safe for two
reasons that both have to hold: the run is strictly alternating, so the worker
is idle while the reviewer reads; and the reviewer's class denies it the
editing tools.

The path is the worker's `AgentEntry.cwd`, made by orrerix at the worker's
spawn. It also cannot be mistaken for reviewer scratch space: the scratch
reclaim (`reviewer_scratch_verdict`) requires a reviewer record that carries a
branch for the path, and a reviewer opened this way carries none.

### The planner's pane

A planner's contract is one plan, one report, then exit (#203), and
`close_completed_planner` closes its pane on `done` in a quick run as it does
everywhere. `decide` puts a report ahead of a dead pane, so that close is not a
hold. This is the one pane a run does not leave open, and the plan it wrote is
in `plan.md`.

## 7. The briefs

Four templates in `templates/`: `quick-plan.md`, `quick-work.md`,
`quick-review.md`, `quick-fix.md`. They are pinned the three ways the review
driver's are (`tests/quickdrive/briefs.rs`): a byte-for-byte golden each, a
key-set assertion that no `{{…}}` survives a render on any branch, and a hostile
value that must arrive inert.

- Every interpolated value is sanitized at the render site. A single-line fact
  drops every control character; the task, a plan and a round's findings keep
  their line breaks. Square brackets are mapped in both, so no value can open
  an `[orrerix]` line, and `{{` is split so no value can smuggle a placeholder
  for a later key to expand.
- A plan and a round's findings are **inlined** into the brief, capped at
  20,000 characters, and the file is named beside them. A pane's CLI may need
  permission to read a path outside its worktree, and a run that stalls on a
  permission prompt for its own plan is not a quick one.
- Each brief says that where it and the pane's role instructions differ, the
  brief is the one to follow. The role templates describe the full workflow —
  a PR, `review_verdict`, `post_issue_comment` — and a quick run uses none of
  them.

The step's facts travel in the brief. The human's per-step instructions travel
in the persona. That is the `prompt:` / `brief:` distinction `review-driver.md`
§9 draws, kept.

## 8. What the human can do

Three Tauri commands, all `async` through `run_blocking` because a step may open
a pane and a spawn waits on the frontend:

| Command | ACL set | Does |
| --- | --- | --- |
| `orch_quick_start(req)` | `orch-control` | mints the group, records the run, opens no pane |
| `orch_quick_status(group_id)` | `orch-read` | a pure read of the run |
| `orch_quick_control(group_id, action, text?)` | `orch-control` | `step`, `stop`, `resume`, `handoff`, `note` |
| `orch_quick_list()` | `orch-read` | every run that is working or held, newest first (§16) |

**Starting is two calls on purpose.** A spawned pane is placed by the group its
tab is bound to, and the frontend can only bind once it has the group id. So
`orch_quick_start` returns the id, the launcher binds its tab, and then asks for
the first `step`. A step that never arrives costs nothing: the run is recorded
with its first brief pending and the poll tick delivers it.

The `step` answer carries `busy: true` when another step held the group — the
poll tick got there first and is mid-spawn. Its status shows no live pane, which
is also what a pane that failed to open shows. The launcher stops a run whose
first pane failed, so it has to tell the two apart: on `busy` it asks again
instead (`quickLaunchVerdict` in `src/quickchip.ts`).

**The five verbs are one command**, with a closed action vocabulary. They are
one authority exercised five ways — the same caller, group and ACL tier — and
every `#[tauri::command]` is a manifest row, an ACL grant and a handler line.
The plan named `orch_quick_cancel` and `orch_quick_resume` separately; they are
`stop` and `resume` here.

- **Stop** ends the run and kills nothing. It releases ownership: from then on
  a pane's report is answered "this run has ended".
- **Resume** — §3.
- **Hand-off** gives the turn to the other side now, without a report, and
  spends no round.
- **Note** types a line into the pane holding the turn and carries it again in
  the next brief, so the side that takes over reads it too.

A described run has no hand-off — its agent decides who works — and an idle one
takes no note: there is no task to attach it to, and the human is in the pane.
Both are refused with a sentence that says so.

`orch_quick_control` refuses a group that is not a quick group before it reads
anything. Holding a valid `GroupId` is not membership.

## 9. The notice

A park or a finish raises one needs-you item (`Kind::Feedback`, no task, raised
by orrerix) and, in the app, one desktop toast. The item this run raised before
is withdrawn first, so a run that parks, resumes and parks again leaves the
human one thing to read. A resume and a stop withdraw it too.

A described run's finished tasks are the exception, on purpose (§17.4): each
raises a notice of its own, and a later task does not take an earlier one's
back. A hold's notice is still one per hold.

Nothing is killed at the end. The panes are the human's to read, type into or
close, which is why this is its own exit rather than `release_driven_pane`.

## 10. Restart

Every pane dies with the process, so a record saying "the worker holds the
turn" names a pane that no longer exists. **Nothing is re-opened unasked.** On
its first tick a process scans the orchestration root for quick groups and
parks each working run on `restart`, with one notice. A run that was already
parked is left as it was. An idle described run is ENDED instead, with no
notice (§17.5): nothing was in progress, and its pane is not coming back.

Resume then needs the group in memory, and after a restart no group is until
something launches or resumes it. `qd_ensure_group_loaded` reads **that
group's** `group.json` and inserts it. It does not go through `create_group_ex`,
which resolves the first free group for the repository and rewrites its
`group.json` — pointed at one quick group, it could overwrite another group's
roster.

The reattach puts the group in the table and does **not** declare its checkout
as a root (#1042). `create_group_ex` declares a checkout its caller just named;
this value comes off a file on disk, and an admit site fed from disk would be a
root nobody at a keyboard chose. `tests/rootreg.rs` allows no admit in this
file.

## 11. The frontend

- `src/quickmodel.ts` (pure) turns the form into the exact `orch_quick_start`
  payload. It refuses an out-of-range bound by name rather than clamping it.
  Its per-step CLI lists mirror `cli_can_host` over `CLI_CAPS`, and
  `test/quickmodel.test.ts` derives them from `model.rs` rather than restating
  them.
- `src/quickchip.ts` (pure) gives every state a chip label and every state a
  menu. Its state and hold-reason lists are read out of `quickdrive.rs` by its
  test.
- `src/quickruns.ts` polls a run's status **only while it is working**, through
  the poll gate. A parked run moves when the human moves it, which refreshes it
  directly, and a finished one never moves. So a finished task leaves no timer.
  An idle described run is not polled either; it leaves idle on the backend's
  own account, so the backend says so (`orch-quick-changed`, §17.6).
- `src/quickpresets.ts` owns the saved instruction presets:
  `quickpresets.json` in the app's state directory, beside `sshprofiles.json`.
  They are the user's, not a repository's. Every write re-reads the file and
  edits what it just read, and a read that failed declines the write.

The chip and the form are header and launcher chrome. Nothing here resizes a
PTY.

## 12. What a run can never do

`tests/quickdrive/guards.rs` scans the three files that are the quick drive
(`quickdrive.rs`, `qdtick.rs`, `registry/quick.rs`) and fails on any of:

- a `gh` or `git` command line — it builds none, so there is no allowlist;
- a child process, a `gh` runner, or a `git` helper;
- a call that merges, queues a merge, records a verdict, ends a pane, writes
  the board or posts an issue comment.

A second test plants each violation and shows the scan firing, and shows it not
firing on source that only looks like one.

What an *agent* does in its own pane is outside that scan. It is bounded by the
role's containment, by the `gh` shim's human grant for a merge to the default
branch, and by the brief.

## 13. Contract changes

1. `<group-dir>/quick_drive.json` v1, `<group-dir>/quick/*.md`, and the
   `quickrun` marker file.
2. The commands in §8, and `load_quick_presets` / `save_quick_presets`.
3. `report` and `message_orchestrator` for a caller in a quick group (§5).
4. Four brief templates (§7).
5. `quickpresets.json` in the app's state directory.
6. The launcher's pane kind gains `quick`. The *persisted* pane kind does not
   change: a quick run's panes are ordinary orchestration panes.
7. `next_group_id` skips a group whose quick run has not ended (§2).
8. Two ranked locks, `qd_state_lock` (860) and `qd_mem` (870).
9. `Role` gains `Quick`, wire string `"quick"` — in `agents.json`, in
   `list_agents` and `session_roles`, and as a key of `group_summary.roles`
   (§15).
10. Two templates: `templates/quick.md`, the class's role instructions, and
    `templates/quick-root.md`, a described run's first message.
11. `orch_quick_start`'s request gains `mode` (`steps` | `describe`; absent
    means `steps`) and `root` (the CLI and model of a described run's agent).
12. `quick_drive.json` gains three defaulted fields — `described`,
    `root_cli`, `root_model` — so every record already on disk reads as a
    steps run. A quick run's status gains `described` and `panes.root`.
13. `orch_quick_list`, and `busy` on a `step`'s answer (§8).
14. `report` for a caller of the new class is the run's end; the tool it is
    listed with says so. `message_orchestrator` replies that a message was
    NOT saved when the run's messages file is full.
15. The launcher's result gains `quick-resume` (§16).
16. `quick_drive.json` gains the state word `root-idle` and one defaulted
    field, `task_seq`. The schema version does not move (§17.7).
17. `orch_quick_start` with `mode: "describe"` takes no task and refuses one;
    `task` is defaulted on the request. A described run's root is opened with
    nothing typed (§17.1).
18. A described run's `report(done)` ends a TASK: the run returns to
    `root-idle`, and `spawn_agent` is no longer refused after it. The root's
    first successful `spawn_agent`, `fork_session` or `send_prompt` while idle
    begins a task, and that call's answer gains a paragraph stating its limits.
19. A helper spawned in a described run's group with no `base` is cut from the
    run's own (§17.3).
20. A run's status gains `task_seq` and `last_note`. `orch_quick_list` leaves
    an idle run out. `orch_quick_control` refuses `handoff` on a described run
    and `note` on an idle one.
21. A new backend event, `orch-quick-changed` (`{ group_id }`).
22. `templates/quick.md` and `templates/quick-root.md` are rewritten:
    the first for an agent with no task yet, the second as the message a
    RESUMED task's root is typed. Four audit actions join the vocabulary:
    `qd-opened`, `qd-task-begun`, `qd-closed`, `qd-session-watch`.
23. An idle run whose root's CLI exits by itself is held on `unresumable`
    (§17.5). On codex, an idle root's session watch starts at its first tool
    call instead of at its spawn (§17.8).

## 14. Residuals

- **No token cap.** A run is bounded by rounds, clocks and one pane per step.
  The autonomy budget gates an orchestrator's idle tick, and there is no
  orchestrator here.
- **No PR discovery.** A PR is used when the worker names one in its report's
  `ref`; orrerix does not ask GitHub whether the branch has one. That keeps the
  drive free of `gh` altogether, which is what makes §12 a property of what is
  not written.
- **A paused group** queues the run's deliveries and its clocks keep running.
- **Idle-kill** applies after a run ends, as it does to any idle pane, if the
  human set one.
- **The watchdog** is forced off for a quick group: its notice is addressed
  to an orchestrator, and no quick group has one. A described run's root is
  bounded by its task's clock instead, and by nothing while it is idle.
- **A session rejoined by hand** into a quick group is an agent the run does
  not own. Its report is recorded and answered; it moves nothing.
- **The needs-you card** carries the run's notice as text. Resume and Stop are
  on the pane menu and in the launcher's list (§16), not on the card.

## 15. The second mode — one agent, given its tasks in its pane

In describe mode the human writes nothing on the form but where and on what.
One pane opens, idle, and the human tells the agent in it what they want — as
they would any agent pane. The agent decides whether the task wants a plan and
a review, opens the helpers it needs, reads what they report, relays between
them, and ends the task by reporting. Then it waits for the next one. §17
argues that lifecycle; this section is the class and the run around it.

### 15.1 A class of its own

That agent is a new capability class, `Role::Quick`. It could have been a lead
with more tools, or an orchestrator with fewer, and it is neither:

| | Lead | Quick root | Orchestrator |
| --- | --- | --- | --- |
| Opened by | the human, in their own launcher | orrerix, for a run the human started | orrerix |
| First message | none; the human types | none; the human types the task | the orchestrator kickoff |
| May open | workers | workers, reviewers, planners | every roster block |
| Its `report` | has none | **the end of a task** | has none |
| Board, merge queue, verdicts, issue comments, questions to the human | none | none | all |
| Ends | when the human closes it | when the human closes it or stops it; a task ends when it reports, or is held at a bound | never |

A quick root is close to a lead since #3723 — both are a pane a human types
into — and it is still not one. A lead is the human's own pane, so it has
nothing to report and nobody to report to; a quick root has a task with an end,
limits that bind it, and a notice the human is owed when it finishes. An orchestrator holds exactly the tools a quick run must not have,
and removing them by a hint would make capability a function of data, which is
the thing #222 exists to rule out. So the difference is a class.

**It shares two predicates with the other two roots, and nothing else.**
`Role::is_root` is what a delegate's `report` is delivered by, so a quick
root's helpers report to it with no new code on that path. `Role::is_fixture`
is the one exemption rule the cap, the dock, the reaper, the watchdog, the
review driver, the spawnable-block listing and persona ownership all read, so a
quick root is never reaped, never counted against the cap it spends on its own
helpers, never listed to an agent as a block it could spawn and never given a
repo-authored persona — each at the one place that already decided it for the
other fixtures. The listing is cosmetic. What refuses a spawn of the root's own
block is the spawn rule in §15.3, and nothing else: with that rule removed the
answer was a second root, opened.

**It is unclamped** (`Containment::None`), for the orchestrator's reason: it
delegates and decides, in the repository, with no worktree of its own. Every CLI
can therefore host it, which is why a described run is not limited to the CLIs
that can be held read-only. What bounds it is the four things below, none of
which a deny tier can express.

**The run's bounds reach the root, or they would bind nothing.** The root is
never reaped, it is not waiting for orrerix to hand it anything, and a record
changing on disk tells it nothing. So when a described run is held — at its
time bound, on a provider limit, for any reason — two things happen besides the
notice to the human: one line is typed into the root's pane saying the run is
held and why, and `spawn_agent` and `fork_session` refuse it until the run is
resumed. A run the human STOPPED refuses them for good. A task that finished
refuses nothing: the run is idle, and the next helper the root opens begins the
next task (§17.4). `send_prompt` and `get_output` are left alone, so a held
root can still read what its helpers did and tell one to stop — and a held
root's `send_prompt` begins no task and lifts no hold.

### 15.2 What it may call

Twelve tools: `list_agents`, `request_compact`, `note_directive`;
`spawn_agent`, `fork_session`, `send_prompt`, `get_output`, `kill_agent`,
`focus_agent`, `rename_agent`; `group_usage`; and `report`.

The list is positive and is spelled twice — once where `tools/list` is built
and once where `call_tool` dispatches — and
`the_gate_and_the_listing_agree_for_a_quick_root` asserts the two are one set by
calling every tool any class is ever shown. Most arms of `call_tool` have no
role check of their own, so a class that was merely *not refused* would reach
all of them. This is the manager's and the lead's pattern unchanged, and it
lives in `mcp/quickroot.rs` so that `mcp.rs`, which is at its line budget,
carries only the call sites.

Everything that could land, publish or decide for the human is absent: the
board, the merge queue, `review_verdict`, `post_issue_comment`, `ask_human`,
`request_attention`, and the notify, lock, state, channel, mailbox and to-do
tools. So is `message_orchestrator`: the root is the root, and one that called
it would park its own run.

### 15.3 What it may open

A worker, a reviewer or a planner. The rule is applied to the spawn's
**effective** class — a named block's kind wins over `kind:` — at the same point
the lead's rule is, and a spawn that resolves to no class at all is refused too.

There are three refusals, and it matters which is which:

- `kind: "quick"` is refused as an **unknown kind**, by the parse.
  `workflow::kind_from_str` has no `quick` arm, and that absence is the whole
  no-nesting rule: no workflow file can declare the class and no agent can spawn
  it. An arm written to refuse it would be unreachable code taking credit for a
  refusal it does not make.
- `block: "quick"` — the root's own block — is the one spelling that reaches
  the class rule with `Role::Quick`, and the class rule refuses it.
- `cwd` and `task_id` are refused for this caller. A helper's workspace is
  orrerix's to choose. A fresh worker's or reviewer's `cwd` is already refused
  for every caller by the dedicated-workspace guardrail (#338/#359); the root's
  rule adds the two cases that guardrail leaves to an orchestrator — a
  planner's, and a resume's. There is no board to attach a pane to.

The root also cannot be killed by any agent, itself included, and cannot be
forked: a fork inherits its source's block, so a fork of the root would be a
second root, outside the cap.

### 15.4 The run

`orch_quick_start` with `mode: "describe"` records a run in `root-idle`, with
`described` set and no task, and adds the root's block to the built-in roster.
That is the one place a quick block is ever minted. The step that follows opens
the root's pane and types nothing into it (§17.1).

**The root's helpers are not sides of the run.** In the steps mode every pane is
one, and `qd_owner` answers for all of them. In a described run it answers for
the root alone: a helper's `report` and `message_orchestrator` are the root's to
read, so they take the ordinary relay to the group's root — a `done` is typed
into the root's pane, a `progress` is recorded — and the record never names a
worker. The one caller that must not fall through is a quick root the record
does not name, because the relay's target would be itself; it is answered as a
stranger.

The root's `report(done)` ends the task: the run moves back to `root-idle`,
and the report is typed into no pane. Its note is the account of the task and
is what the notice carries. `blocked` parks the run on `root-blocked`; Resume
gives the turn back to the same pane and says why. Stop types one line into the
root, because a root is mid-decision when a run is stopped and learns nothing
from a record changing. Nothing is killed in any of these.

**If the root's pane closes, its helpers are closed with it.** Helpers left
running would be working towards a report with no recipient; their sessions
and worktrees are still there. With a task in progress the run parks on
`root-gone`. With none, the run is simply over (§17.5).

The root runs in the repository itself, with no worktree and **no branch**. That
last part is load-bearing: a delegate may close only a pull request whose head
is its own recorded branch, so a root with none can close nothing. Its Claude
launch denies the interactive question dialog, and a structured pane does not
park on a dialog, for the orchestrator's reason in both cases — its helpers'
reports queue behind it, and nobody is in the pane.

A reviewer needs no pull request here either. Every helper's worktree shares the
repository, so a reviewer reads a worker's commits as soon as they exist. The
steps mode puts the reviewer in the worker's worktree to read uncommitted work;
a root is told to have the worker commit instead.

### 15.5 Restart

A described run with a task in progress parks on `restart` like any other, and
Resume re-opens the root's own session and types the resume message
(`quick-root.md`). An idle one is ended instead (§17.5). One more thing is
different. The roster in `group.json` is read
back through the workflow vocabulary, which has no word for the root's kind —
deliberately — so the root's block is not in what a restart reads. The reattach
rebuilds it from the run's own record (`root_cli`, `root_model`), for that
group and for a run that says it is described, and by nothing else.

### 15.6 Every place that asks what class a pane is

Adding a class is only safe if nothing decides for it by default. `Role`'s
exhaustive matches are compile errors until they have an arm; the risk is
everywhere else — a `matches!`, an `==`, a wildcard arm, a hand-keyed table.
Each such site was read and classified; the PR that added the class lists them.
The decisions fall into four groups:

- **An arm or a predicate, because the default was wrong.** The two predicates
  above; the tool listing and the dispatch gate; the spawn rule; the workspace
  chain (the default was a worker's worktree and branch); `kill_agent`;
  `fork_agent`'s wildcard (the default made the root forkable); the exit branch
  (the default left helpers running and addressed a notice to nobody); the two
  dialog rules; `group_summary.roles`.
- **Left to the default, because the default is the refusal.** Every
  orchestrator-only gate. A quick root is refused the board, state, questions,
  verdicts and mail by gates that name the orchestrator, and by its own surface
  gate before any of them is reached.
- **Left to the default, because the lead already takes it.** Notices addressed
  to an orchestrator — a delegate's exit notice, a queue failure — find nobody
  in a group whose root is not one. A lead's helpers are in the same position;
  the root's instructions say a helper that dies without reporting sends it
  nothing, and `list_agents` shows it.
- **Deliberately absent.** The workflow vocabulary, the built-in block ids, the
  roster editor and the workflow schema have no `quick`.

### 15.7 Residuals

- **Instruction-only on copilot.** Copilot's in-process `agent` tool cannot be
  denied on its launch seam, so a copilot root is told to prefer `spawn_agent`
  and is not prevented from using its own subagents. The lead has the same row.
- **Inherited from the lead pane: #2893 item 3.** A root's tab is the tab its
  group is bound to; drag the pane to another tab and its helpers still open in
  the first.
- **Not inherited: #2893 item 2**, a restored lead losing its launch
  guardrails. A quick run is not restored as a pane — its record carries its
  bounds and Resume reads them.
- **Not inherited: #2833**, a codex lead. A lead is launched by the human's own
  command line, which is where codex has no seam for the MCP config; a quick
  root is spawned by orrerix like any agent, so codex can host one.
- **The round bound is an instruction.** The record counts no rounds for a
  described run, because the reviews happen between the root and its helpers.
  The root is told the bound, in the answer of the call that begins each task;
  the time bound is the one the engine enforces.
- **A root that reports `done` while a helper is still working** leaves that
  helper running. Nothing is killed at the end of a run, in either mode; the
  panes are the human's.
- **The list of unfinished runs reads every quick record** each time the form
  opens, and ended runs are never pruned from disk. That is one small file per
  run ever started.
- **Helpers are listed flat** in the agent rows, not nested under the root.
- **A root's session is not rejoined by hand.** The session browser's rejoin
  refuses a recorded role it has no class for; the run's own Resume is the way.

## 16. Reaching a run with no pane

Resume and Stop are on a pane's menu, and a run can outlive every pane it had:
close them, or quit and reopen the app. The run is then parked, on disk, with
nothing on screen that leads to it.

**The launcher's Quick task form lists the runs that are working or held**, each
with Stop and, when it is held, Resume. An idle described run is not on it: it
has nothing in progress to resume or stop (§17.5). `orch_quick_list` reads them off the run
records under the orchestration root, newest first, with each run's repository.

The alternative was a button on the run's needs-you item, and the list was
chosen over it for two reasons. A needs-you item can be dismissed, and a run
whose item was dismissed would be unreachable again; a record cannot be
dismissed. And Resume has to open a pane somewhere. The form is in a tab, so
"Resume here" binds that tab to the run's group and resumes it there, which is
exactly how a new run is started; a card has no tab to offer.

**Closing a tab** closes its panes. The engine parks the run when it next looks
and finds the pane that held the turn gone. In the window, a run is painted onto
the panes of the tab its group is bound to, so when that tab closes the run
stops being polled at once; the poll had no reader. "Shown" is asked of the
panes as well as of the binding, so a run whose pane was dragged to another tab
is still read while that pane is on screen. The run itself is not ended by
closing a tab, and it is on the list.

A start or a Resume whose pane fails to open undoes the tab binding it made. A
tab bound to a group it shows nothing of would read as that group's tab.

**A first pane that never opens.** A new pane is not known to have failed until
the backend's bind deadline (`BIND_TIMEOUT`, 20 s) has passed; until then the
step opening it holds the group and every answer is `busy`. The launcher waits
past that deadline — the wait is derived from a mirror of the constant, pinned
against `tuning.rs` — and then shows the reason in the form and stops the run.
A run that is still `busy` after the whole wait is not stopped, since a step is
still holding it; the form says it was left as it is and where to find it.

## 17. The idle start, and what a task is

Issue #3723. As first shipped, describe mode took the task on the launcher form
and typed it into the root as its first message. The run then *was* that task:
it began when the form was submitted and ended, for good, when the root
reported. That made the root unlike every other pane a human opens — they could
not talk it through first, the clock ran while they thought, and one task used
the pane up.

So the root now opens idle, and a run is a pane that takes tasks. Five
decisions follow from that, and each had more than one defensible shape.

### 17.1 Nothing is typed, where the launch already carries the instructions

A root's role instructions do not need a first message to arrive. Since #416
`persona_inject` puts the block's contract on the CLI's own system-prompt layer
at launch, for every block:

| CLI | How the instructions reach it | What is typed |
| --- | --- | --- |
| claude | a generated agent file named on `--agent` — the whole contract (if that directory cannot be written, `--append-system-prompt-file`) | nothing |
| opencode | an agent entry whose prompt is a contract file — the whole contract | nothing |
| pi | `--append-system-prompt` naming a contract file — the whole contract | nothing |
| codex | `developer_instructions` in the profile it is launched with — the whole contract | nothing |
| copilot | a generated agent file named on `--agent` — a slim copy: the mechanics core and a pointer to the full file | nothing |
| gemini | nothing; it has no such seam | one message: where its instructions are, and to wait |

The rule is one pure function, `idle_start_types_nothing(role, task, cli,
carrier)`, and `fresh_kickoff` is the one place both spawn arms (PTY and
structured) ask it, so they cannot answer differently. All four have to hold: a
quick root; no task; a CLI with a launch seam; and a contract that really is on
it for this launch. The last is not the same as the third. A contract file that
could not be written leaves the carrier at `KickoffOnly` on any CLI, and that
pane is typed the same one message gemini's is.

**That message is not a task.** It names the agent, points at its instructions
file, and ends "No task is given here. After reading the instructions, say in
one line that you are ready, and wait: the human will tell you what they want
in this pane." A root with no instructions would be worse than a root typed one
line, and a pointer is the smallest line that fixes it.

Copilot's slim copy is enough to wait on. `mechanics_core`'s quick arm — which
is what that copy embeds — opens by saying the agent is the root, was opened
idle, and takes its tasks from the human in the pane; the full file is one read
away when a task arrives.

**`persona_inject` reports gemini's carrier as full.** It has no branch for
gemini, falls through to the generated-file arm, and sets the carrier that arm
sets — for a file gemini's launch never names. That is not fixed here; the rule
does not rest on it, because it asks about the seam separately
(`NO_SYSTEM_LAYER_CLIS`).

### 17.2 A task begins when the root first puts a helper to work

The time limit must not count the pane sitting idle, so something has to say
when work started. The candidates:

- **The human's first message.** It is the truest signal, and no hook that
  reports it exists on every CLI that can host a root. It is also too early:
  the minutes a human and an agent spend settling what is wanted are exactly
  the ones the limit should not charge for.
- **The root's first `spawn_agent`.** Engine-observed, on every CLI. But
  nothing is closed when a task ends, so a second task is often begun by
  prompting a helper that is still open — and a run that only noticed spawns
  would leave that task with no clock, and its `done` with nothing to end.
- **The root's first `spawn_agent`, `fork_session` or `send_prompt` while
  idle** — the three calls that hand a helper something to do. This is the
  rule. It is what "work starts" means for this class: a root may not edit, so
  delegating is the only work it does.

`list_agents`, `get_output`, `group_usage`, `kill_agent`, `rename_agent` and
`focus_agent` begin nothing. A root may look around, and clear up after the
last task, without a clock starting.

**The clock starts after the call succeeds.** The dispatch funnel asks
`qd_root_acted` once a tool has answered `Ok`. A spawn blocks until its pane
has opened, and a task is not charged for that — the reason a state's clock
starts at delivery rather than at the arc (§3). A call that was refused began
nothing. The funnel is the one place that knows a call succeeded, and the hook
is a few lines there so that `mcp.rs`, at its line budget, carries no rule.

`begin_task` stamps the record's clock at that moment and the run joins the
tick's candidate list. Before it, the run is in `root-idle`: `decide` answers
nothing for it and no bound is read. It is on that list only until the step
that opens its root's pane has run, and off it from then on. A pane left
waiting for a year costs no wake.

**Only `root-idle` begins a task.** A working run is already on one. A held run
is not moved by its root acting — `send_prompt` is deliberately open to a held
root so it can tell a helper to stop, and that must neither lift the hold nor
start a clock. Only the human's Resume moves a hold.

**And only the run's own root begins one.** The hook is told who called, off the
caller's token, and a quick root the record does not name as its current one
begins nothing. `qd_owner` already answers such a pane as a stranger (§15.4);
its helper is opened, because the spawn rule is the class's, but the run's
clock is not its to start.

### 17.3 The limits reach the root without a first message

The first message used to carry three numbers. They now arrive where each is
needed:

- **The time bound and the round bound** ride on the answer of the call that
  begins the task — "This is the start of a task in this quick run (task 2).
  Its limits: …". That is the moment they start to apply, and it repeats for
  each task. The resume message restates them.
- **The base branch** is no longer the root's to pass. A spawn in a described
  run's group that names no `base` is cut from the run's own
  (`qd_helper_base`), applied where the worktree is cut. A base the root does
  name still wins.

The alternative was a placeholder in `quick.md`, rendered from the run's
record. But a block's instructions are rendered twice: into a FILE when the
group is created, which is before the record exists, and onto the system
prompt at spawn, which is after. A value read from the record would have been
there in one and empty in the other — and a copilot root reads the file. It is
the class of ordering defect behind #3161, and the template's golden cannot
see a rendered value to catch it. Saying the numbers at the moment they bind
needs no placeholder at all.

### 17.4 A second task is the same pane's next one

The root's `done` ends a task and returns the run to `root-idle`. The pane, the
helpers and the group are as they were, and the next helper the root puts to
work begins the next task.

- **One record, re-armed.** No new record and no new group begins: the pane is
  bound to its group, and the file holds one run. `task_seq` counts the tasks
  begun. `begin_task` restamps the clock and drops the last task's account —
  its note, its pull request, the id of its notice — from the record. It drops
  `task` too: only a record from the build before this one carries a task
  text (§17.7), it describes the task that record was started with, and left
  in place it would be quoted in the notice of every later task.
- **Limits apply per task.** Each gets the whole time bound from its own
  start, and the same round bound.
- **Each finished task raises its own notice**, and a later task does not
  withdraw an earlier one's. The notice says where finished work is, and that
  stays true. They are numbered from the second on — "(task 2 in this pane)" —
  so two do not read as one notice twice.
- **Helpers stay open** and count against the cap. The root is told to reuse
  one with `send_prompt` or end it with `kill_agent` before opening another.
- **A report made while idle ends nothing**, and is answered in those words
  rather than with "it is not this pane's turn", which promises a brief that
  will never come.

`qd_root_spawn_refusal` had refused a root "for good once its run had ended",
and after a `done` that made a second task impossible. It now has exactly two
refusals, the two a human caused or has been told about:

| The run is | `spawn_agent` / `fork_session` |
| --- | --- |
| idle, or on a task | allowed |
| held | refused until the human resumes it — unchanged, and what makes a bound bind (§15.1) |
| stopped | refused for good |

**Stop stays the end of the run, not of the task.** It could have meant "stop
this task and go back to idle". But a root is mid-loop when a human reaches for
Stop, and from idle its very next spawn would begin a new task: Stop would
bind nothing, which is the #3712 finding in another shape. A human who wants to
redirect rather than end has a better tool now — the pane takes their message.

One ordering is worth knowing. A root that reports `done` and opens a helper
before the step has acted on the report is still on the old task when that
spawn lands, so the spawn begins nothing; the next call does.

### 17.5 A pane nobody gave a task to leaves nothing behind

An idle run is one where every task has finished, or none was given. There is
nothing in it to resume and nothing to stop, so it is treated as nothing:

- **It is not polled** once its pane is open — not by the tick (§17.2) and not
  by the window (§17.6).
- **It is not on the list of unfinished runs.** `orch_quick_list` lists working
  and held runs. A run joins the list when a task begins and leaves it when the
  task is done.
- **Closing its root's pane ends it.** The pane-exit path asks `qd_root_exited`,
  which moves an idle run to `cancelled` with no notice. Nothing was
  interrupted. A notice a finished task had raised is left where it is.
- **A restart ends it too**, for the same reason: `qd_reconcile` parks a
  working run and ends an idle one. Its group id is then free.

**A root that goes by itself is a different thing, and parks.** "Closed" above
means orrerix or the human ended the pane, which the exit path is told
(`expected`). A CLI that exits on its own is not the human being done with it,
and for an idle root it is the ordinary shape of a failed launch: the root is
recorded the moment its pane binds, with nothing typed and so nothing waiting
for the CLI to boot, which means a CLI that dies at boot — a wrong model, not
signed in — dies after it is recorded. Ending the run there would make
every such launch a pane that vanished with no reason given. So an unexpected
exit holds the run on `unresumable`, quoting what the pane went out saying;
the human is told, the run is on the list, and Resume opens a fresh root.

That includes a human who quits the CLI from inside it rather than closing the
pane. The two are told apart by who ended the process, not by exit code: a CLI
that prints an error and exits zero is a failed launch too.

One window is left by recording at bind, and it is closed where it opens. A
pane that dies in the two statements between the spawn returning and the run
recording its root is not matched by the exit path — there is no root on the
record yet. So the hand-over looks once more after it has recorded the root,
and parks the run the same way if the pane is already gone.

With a task in progress none of that applies and §10 and §15.4 hold as before:
the run parks on `root-gone` or `restart`, the human is told, and Resume
re-opens the root's session.

So `root-idle` has two holds: a root pane that could not be opened, and one
that opened and then went by itself. Both park like any first pane that
failed, with the reason quoted, and Resume asks for the pane again.

The open itself is `qd_hand_over`, the function every other turn's pane goes
through, with one difference: its delivery step opens the pane and types
nothing (`qd_open_root`). The store, the "run moved while the pane was
opening" case and the park with the refusal quoted are the same lines for an
idle open as for a brief, and the spawn with its one-root backstop is shared
with a resumed task's (`qd_first_root_pane`).

`quick_start` now marks the group as known to this process *before* it writes
the marker that makes the group findable. The start-up scan treats a marked
group it does not know as an earlier process's; for a steps run that was a
needless `restart` hold in a narrow window, and for an idle run it would have
been an ending.

### 17.6 The window

- **The form** hides the task field in describe mode and sends no task,
  whatever the hidden box still holds. `planQuickStart` requires a task in
  steps mode exactly as before. The time-bound field reads "per task".
- **The chip** reads `quick · idle`, untinted. Its tooltip says what the pane
  is for and when the limit starts, and — once a task has finished there —
  what the agent said. It claims no turn, and its menu is empty: closing the
  pane is the whole of ending an idle run.
- **`orch-quick-changed`.** The window polls a run only while it is working,
  and an idle run leaves idle on the backend's account, when its root opens a
  helper. Nothing in the window would notice. So the backend emits this event
  when a task begins, when a step takes an arc, and when an idle root's pane
  closes, and the window re-reads a run it already shows. It is the only thing
  that wakes an idle chip.
- **The first step's answer.** A `busy` step is "still opening" for a working
  run *or an idle one*. Read as a failure, it would have had the launcher stop
  a described run under its own pane.

### 17.7 The record across builds

`quick_drive.json` stays at schema version 1. It gains one state word,
`root-idle`, and one defaulted field, `task_seq`. A version bump was the
alternative and was not taken: `Unsupported` means "do not operate, do not
write", which would have made every record this build writes — a task in
progress included — unreadable to the build before it, to protect it from one
word.

**A record from the build before this one loads here.** That build started a
described run in `root-wait` with the task on the record. It reads as a task in
progress: bounded by its own clock, parked by a restart, resumable. Its task
text is kept for as long as that task lasts — shown on the chip and the list,
and quoted in the resume message, since a root re-opened cold would otherwise
have no task at all. Its `done` returns the run to idle like any other, and the
text goes when the next task begins (§17.4). One that had already ended is
still ended.
`a_described_record_from_before_the_idle_start_still_loads_as_a_task_in_progress`
reads that shape from a literal, not from this build's serializer.

**What the build before this one does with a record from here:**

| The record is | That build |
| --- | --- |
| on a task (`root-wait`), held from one, or stopped | reads it. `task_seq` is preserved as an unknown field. The task is empty, so its chip and list row have no text and its resume message quotes an empty task. Its root's `done` ends the run for good, as that build's did. |
| idle (`root-idle`), or held from idle | refuses the state word: "quick_drive.json does not parse". The run is not listed and its controls error. That build then no longer holds the group id, so a later launch on the same repository can claim it and overwrite the record. |

Nothing in progress is lost in the second row — an idle run has no task — and
`an_idle_record_round_trips_under_a_state_word_older_builds_do_not_know` pins
the word, since the row rests on it.

### 17.8 Whose session an idle root's is

orrerix learns a pane's session, on the CLIs that mint their own, by watching
the CLI's session store for a NEW session in the pane's directory. A typed
kickoff starts a turn within seconds, so the watch and the session it is
looking for arrive together. An idle root is typed nothing, and its CLI may
not write a session until the human's first message.

On one CLI that gap was a hazard rather than a delay. **Codex's store is the
human's own**, shared with the sessions they start in their terminal, and a
quick root's directory is their checkout. A `codex` they started there while
the root waited was new, in that directory and unclaimed — the one candidate
there was. The watch would have bound it to the root: usage read from the
human's session, and a Resume re-opening their conversation as the root.

So on codex an idle root's watch is **not started at the spawn**. It is held,
with the baseline taken before the spawn, and started by the root's first
answered tool call (`qd_root_acted`). By then the root's CLI is running a
turn, so its own session exists, and a stranger's can no longer be the only
candidate: the search finds the root's, or finds two and answers `Contested`,
which is never a guess. A watch held for a root that goes is dropped with it.

`defers_session_watch` is the rule, and it is this narrow on purpose:

| Store | Whose | Idle root's watch |
| --- | --- | --- |
| codex | the human's, shared | deferred to the root's first tool call |
| opencode | the group's own — no session of the human's is ever in it | at the spawn, as before |
| copilot | the human's, but written a few seconds into boot whether or not anything is typed | at the spawn, as before. Deferring would widen the window: its search takes the newest new session, and a later one of the human's would outrank the root's |

Claude and pi are handed their session id at launch, and gemini's sessions are
not tracked, so none of the three has a watch at all.

The alternative was to start the watch at the human's first input in the
pane and take the baseline then. That would settle the contested case as well,
since a baseline taken at that moment already holds the human's session. It
was not chosen, for two reasons. The registry has no event for a pane's input:
the pty layer keeps a last-input time, which something would have to poll for
as long as the pane sat idle — the cost §17.5 rules out. And a baseline taken
a moment too early reopens the hazard with nothing to show for it, whereas a
tool call is observed by the engine and cannot come before the turn it is
part of.

### 17.9 Residuals

- **Gemini is typed one message.** It is the pointer of §17.1 and carries no
  task. A CLI whose contract file cannot be written is typed the same one.
- **A contested codex session stays unrecorded.** If the human did start their
  own codex in the repository while the root waited, the search sees two and
  binds neither (§17.8). The root then has no recorded session, so a task
  interrupted by a restart or a closed pane cannot be resumed; the hold says
  so. That is the safe direction, and the price of it.
- **On opencode, a late first message can leave a session unrecorded.** It may
  write its session only at the first turn, and orrerix watches for ten
  minutes after the pane opens. A root first spoken to later than that has no
  recorded session, with the same consequence. Deferring its watch as codex's
  is deferred would fix that, and was not done here: nothing of the human's
  can be bound in a store that is the group's own, so it is a missing
  convenience and not a hazard.
- **Quitting the CLI from inside an idle pane holds the run** (§17.5), where
  closing the pane ends it. Stop, or closing the pane, clears it.
- **Copilot's autopilot consent is the human's to answer.** orrerix answers
  that dialog while typing a kickoff. With nothing typed, it appears on the
  human's first message instead.
- **A root that never delegates is never on a clock.** The bound starts with
  the first helper. A root that answers in its pane — or does work there,
  against its instructions — is bounded as a lead is: by a human being in the
  pane.
- **A helper's late report can begin a task.** It is typed into an idle root's
  pane, and if the root answers it by prompting a helper, that is a task
  nobody asked for. It is bounded and it ends with a notice like any other.
- **Finished-task notices accumulate**, one per task, until the human clears
  them.
- **Helpers left open show the run's chip.** A status is painted on every pane
  of its group, so a helper still open after a task reads `quick · idle` too.
