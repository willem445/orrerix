# Quick orchestration — a plan, work, review run with no orchestrator

Issue #3679. User page: `docs/features/quick-orchestration.md`.

A **quick task** is one short-lived run for one task: an optional plan, the
work, an optional review, and a bounded loop between the worker and the
reviewer. It has no orchestrator pane, no task board, no issue queue and no
merge gate. orrerix relays between the steps itself and tells the human when the
run finishes, parks, or needs them.

This note covers the steps mode ("way 1" in the issue). The second mode, where
one agent runs the task from a description, is a later change; §14 says what
this one leaves in place for it.

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
overwrote its record.

## 3. The states

Exactly one pane **holds the turn** at a time. The run moves only on that
pane's `report`.

| State | Who holds the turn | Leaves on |
| --- | --- | --- |
| `plan-wait` | planner | its `done` → `work-wait` |
| `work-wait` | worker | its `done` → `review-wait`, or → `satisfied` when the review step is off |
| `review-wait` | reviewer | `approved` → `satisfied`; `request_changes` → `fix-wait`, or → `held` on the last round |
| `fix-wait` | worker | its `done` → `review-wait` |
| `root-wait` | (the later mode's root) | nothing in this build enters it — §14 |
| `held` | nobody | Resume → the state it came from; Stop → `cancelled` |
| `satisfied` | nobody | terminal |
| `cancelled` | nobody | terminal |

`quickdrive::transition` lists every arc and refuses the rest; a unit test walks
all sixty-four pairs against that list.

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

`root-blocked` and `root-gone` exist for §14's mode and nothing produces them
yet.

A pane closing is a hold only when it is the pane **holding the turn**. A worker
whose pane is reaped while the reviewer works is not a problem: the next
hand-back re-opens its session (§6).

### Bounds

| Bound | Value | Read by |
| --- | --- | --- |
| review rounds | 1–3, default 3 | the `review-wait` arc; the round being answered is the last when `review_rounds + 1` reaches it |
| run time | 5–1440 minutes, default 240 | every working state, last |
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

**Starting is two calls on purpose.** A spawned pane is placed by the group its
tab is bound to, and the frontend can only bind once it has the group id. So
`orch_quick_start` returns the id, the launcher binds its tab, and then asks for
the first `step`. A step that never arrives costs nothing: the run is recorded
with its first brief pending and the poll tick delivers it.

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

`orch_quick_control` refuses a group that is not a quick group before it reads
anything. Holding a valid `GroupId` is not membership.

## 9. The notice

A park or a finish raises one needs-you item (`Kind::Feedback`, no task, raised
by orrerix) and, in the app, one desktop toast. The item this run raised before
is withdrawn first, so a run that parks, resumes and parks again leaves the
human one thing to read. A resume and a stop withdraw it too.

Nothing is killed at the end. The panes are the human's to read, type into or
close, which is why this is its own exit rather than `release_driven_pane`.

## 10. Restart

Every pane dies with the process, so a record saying "the worker holds the
turn" names a pane that no longer exists. **Nothing is re-opened unasked.** On
its first tick a process scans the orchestration root for quick groups and
parks each working run on `restart`, with one notice. A run that was already
parked is left as it was.

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

## 14. Residuals, and what is left for the second mode

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
- **The watchdog** is forced off for a quick group: its notice goes to the
  group's root, and there is none.
- **A session rejoined by hand** into a quick group is an agent the run does
  not own. Its report is recorded and answered; it moves nothing.
- **The needs-you card** carries the run's notice as text. Resume and Stop are
  on the pane menu, not on the card.
- **`remote-engine-protocol.md` §5.4** partitions the command manifest and is
  dated to an earlier count. These commands are not added to it; reconciling
  that table is its own change.

`root-wait`, `root-blocked`, `root-gone` and the `root` pane record are in the
engine so the second mode is additive to a record already written. Nothing in
`src-tauri` enters that state, no `Role` exists for it, and the launcher offers
no control for it.
