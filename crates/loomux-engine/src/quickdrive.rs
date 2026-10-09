//! The quick drive's pure core (#3679) — the state machine, the persisted
//! record, and the per-tick decision for a **quick task**: one short-lived
//! plan → work → review run with no orchestrator pane, no board and no merge
//! gate. [`crate::plandrive`] and `crate::reviewdrive` are the two drivers this
//! file is deliberately shaped after, and the registry-side wiring is
//! `src-tauri/src/orchestration/qdtick.rs`.
//!
//! Design note: `docs/design/quick-orchestration.md`.
//!
//! # What this file may and may not do
//!
//! Nothing here touches a child process, a clock or a registry, and the only
//! file it knows is its own record ([`load_state`] / [`store_state`]). Every
//! other function takes what it needs as a value and answers a value, so the
//! whole machine is exercisable from a unit test in a Tauri-free crate. The
//! wiring opens panes, types briefs, writes the plan and findings files and
//! raises the notice; it makes no state decision.
//!
//! # The shape of a run
//!
//! Exactly one pane **holds the turn** at a time, and [`QuickState::turn`] says
//! which side that is. The run moves only on that pane's `report` — never on a
//! `ref` string, a PR or a verdict file — which is what lets it need neither
//! GitHub nor a commit:
//!
//! ```text
//! plan-wait ──planner done──▶ work-wait ──worker done──▶ review-wait
//!                                ▲                            │
//!                                └──── fix-wait ◀── request_changes, round k < N
//! review-wait ──approved──▶ satisfied
//! any working state ──bound / blocked / dead pane / message──▶ held{reason}
//! held ──resume──▶ the state it came from     working | held ──cancel──▶ cancelled
//! ```
//!
//! `plan-wait` is skipped when the plan step is off, and a run with the review
//! step off goes `work-wait → satisfied`.
//!
//! # A described run
//!
//! The "describe it" mode (#3679 way 2, reshaped by #3723) has one pane, its
//! root, and two states of its own. The root opens **idle** and is given its
//! task by the human, in the pane; the record knows a task has begun only when
//! the root first puts a helper to work:
//!
//! ```text
//! root-idle ──the root's first spawn / fork / prompt──▶ root-wait
//! root-wait ──root done──▶ root-idle          (one notice per finished task)
//! root-wait ──bound / blocked / dead root──▶ held{reason} ──resume──▶ root-wait
//! root-idle | root-wait | held ──stop──▶ cancelled
//! ```
//!
//! [`QuickState::RootIdle`] is neither working, parked nor terminal: nothing
//! holds the turn, no clock runs against it, and [`decide`] answers nothing for
//! it. The wiring steps it for one thing only — opening its root's pane — and
//! then stops looking. A described run never enters `satisfied` — its root's
//! `done` ends a TASK, and the pane goes back to waiting for the next one.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::reviewdrive::{DriveLimits, MAX_ROUNDS_CEILING};
use crate::workflow::{DRIVER_DRIVE_TIMEOUT_MAX, DRIVER_DRIVE_TIMEOUT_MIN};

// ── the states ──────────────────────────────────────────────────────────────

/// Where one quick run stands.
///
/// **One parked state carrying a closed reason**, [`QuickHeld`], for
/// [`crate::plandrive::PlanDriveState`]'s reason: a reader asking "is this run
/// parked" asks one question, and the reason travels in the notice and the
/// audit line instead of being inferred from which field happens to be set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuickState {
    /// The planner holds the turn; waiting for its `report(done)`, whose body
    /// is the plan.
    PlanWait,
    /// The worker holds the turn for the first pass at the task.
    WorkWait,
    /// The reviewer holds the turn; waiting for `approved` or
    /// `request_changes`.
    ReviewWait,
    /// The worker holds the turn again, with a round's findings to address.
    FixWait,
    /// A described run's one working state (#3679 way 2): its root has a task
    /// in progress, and the root's own `report` ends that task.
    RootWait,
    /// A described run with **no task in progress** (#3723): its root pane is
    /// open — or still to open — and waiting for the human to say what they
    /// want. Not working, not parked and not terminal: nobody holds the turn,
    /// no bound is read against it, and once its pane is open the tick does
    /// not look at it. It is where a described run starts and where each
    /// finished task returns to.
    RootIdle,
    /// **Parked**, carrying a [`QuickHeld`]. The tick does not advance it;
    /// the human's Resume and Stop do.
    Held,
    /// Terminal: the reviewer approved, or the worker finished a run that has
    /// no review step.
    Satisfied,
    /// Terminal: stopped by the human.
    Cancelled,
}

impl QuickState {
    /// Every state, so a test can walk the machine without matching on the
    /// enum — which is what lets it fail when a ninth is added rather than
    /// silently keep checking eight.
    pub const ALL: [QuickState; 9] = [
        QuickState::PlanWait,
        QuickState::WorkWait,
        QuickState::ReviewWait,
        QuickState::FixWait,
        QuickState::RootWait,
        QuickState::RootIdle,
        QuickState::Held,
        QuickState::Satisfied,
        QuickState::Cancelled,
    ];

    /// The wire/audit spelling — the same string serde writes.
    pub fn as_str(self) -> &'static str {
        match self {
            QuickState::PlanWait => "plan-wait",
            QuickState::WorkWait => "work-wait",
            QuickState::ReviewWait => "review-wait",
            QuickState::FixWait => "fix-wait",
            QuickState::RootWait => "root-wait",
            QuickState::RootIdle => "root-idle",
            QuickState::Held => "held",
            QuickState::Satisfied => "satisfied",
            QuickState::Cancelled => "cancelled",
        }
    }

    /// Parse a state word. `None` for anything unrecognized — never coerced.
    pub fn parse(s: &str) -> Option<QuickState> {
        QuickState::ALL.into_iter().find(|st| st.as_str() == s.trim())
    }

    /// `satisfied` / `cancelled`, and only those two. A terminal state has no
    /// outgoing transition at all.
    pub fn is_terminal(self) -> bool {
        matches!(self, QuickState::Satisfied | QuickState::Cancelled)
    }

    /// Parked. The tick leaves it alone; Resume and Stop move it.
    pub fn is_parked(self) -> bool {
        matches!(self, QuickState::Held)
    }

    /// A described run with no task in progress (#3723). Asked on its own
    /// because it is the one state that is none of the other three: it has
    /// not ended, nobody is waiting on the human, and nothing is being done.
    pub fn is_idle(self) -> bool {
        matches!(self, QuickState::RootIdle)
    }

    /// A working state — neither terminal nor parked nor idle. Only a live
    /// run is read against a clock or parked by a restart, and only a live run
    /// stays on the tick's list: an idle one is on it just long enough for
    /// its root's pane to be opened.
    pub fn is_live(self) -> bool {
        !self.is_terminal() && !self.is_parked() && !self.is_idle()
    }

    /// A run with something IN PROGRESS: working, or parked with a human's
    /// answer owed. An idle described run is not one — every task it was given
    /// has finished, or it was never given one — which is why it is absent
    /// from the launcher's list of unfinished runs.
    ///
    /// Narrower than [`QuickDriveFile::has_unfinished_run`], which asks only
    /// whether the run has ENDED and so is true of an idle one: that is the
    /// question "is this group still spoken for", and an idle run's is.
    pub fn is_in_progress(self) -> bool {
        self.is_live() || self.is_parked() || self.is_idle()
    }

    /// Which side holds the turn in this state, or `None` where nobody does
    /// (parked and terminal).
    ///
    /// The one place a state becomes "whose report moves the run": the MCP
    /// interception asks it to decide whether a report carries a signal at
    /// all, and the wiring asks it to decide which pane a brief is for. Two
    /// answers to that question would be two runs.
    pub fn turn(self) -> Option<QuickSide> {
        match self {
            QuickState::PlanWait => Some(QuickSide::Planner),
            QuickState::WorkWait | QuickState::FixWait => Some(QuickSide::Worker),
            QuickState::ReviewWait => Some(QuickSide::Reviewer),
            QuickState::RootWait => Some(QuickSide::Root),
            // An idle root holds no turn: there is no task for its report to
            // end, so its report is answered and moves nothing.
            QuickState::RootIdle
            | QuickState::Held
            | QuickState::Satisfied
            | QuickState::Cancelled => None,
        }
    }
}

/// One of the panes a quick run can open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuickSide {
    Planner,
    Worker,
    Reviewer,
    /// A described run's root (#3679 way 2): the one agent the human gives
    /// tasks to. Its helpers are not sides of the run.
    Root,
}

impl QuickSide {
    /// Every side, for [`QuickState::ALL`]'s reason.
    pub const ALL: [QuickSide; 4] =
        [QuickSide::Planner, QuickSide::Worker, QuickSide::Reviewer, QuickSide::Root];

    /// The wire/audit spelling — the same string serde writes.
    pub fn as_str(self) -> &'static str {
        match self {
            QuickSide::Planner => "planner",
            QuickSide::Worker => "worker",
            QuickSide::Reviewer => "reviewer",
            QuickSide::Root => "root",
        }
    }
}

/// Why a quick run is parked.
///
/// Closed, for [`QuickState`]'s reason: a hold whose reason this build cannot
/// read is a hold it cannot explain, and the notice is the entire product of a
/// hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuickHeld {
    /// The reviewer asked for changes on the last round the run allows.
    ReviewLimit,
    /// The planner neither reported nor exited inside its timeout.
    PlanStalled,
    /// The worker did not report a fix inside the fix timeout.
    FixStalled,
    /// The reviewer did not report inside the review timeout.
    LaneStalled,
    /// The whole run outran its time bound without finishing.
    DriveStalled,
    /// The planner reported `blocked`.
    PlannerBlocked,
    /// The worker reported `blocked`.
    WorkerBlocked,
    /// The reviewer reported `blocked`.
    ReviewerBlocked,
    /// The "describe it" root reported `blocked` (#3679 item 2).
    RootBlocked,
    /// The planner's pane exited before it reported.
    PlannerGone,
    /// The worker's pane exited before it reported.
    WorkerGone,
    /// The reviewer's pane exited before it reported.
    ReviewerGone,
    /// The "describe it" root's pane exited before it reported.
    RootGone,
    /// The next pane could not be opened because the group is at its
    /// live-agent cap.
    CapRefused,
    /// The next pane could not be opened or re-opened for any other reason —
    /// a CLI that cannot host the role, a session that no longer resumes.
    Unresumable,
    /// The provider of the pane holding the turn reported a usage limit.
    ProviderLimit,
    /// A pane of this run called `message_orchestrator`. There is nobody to
    /// answer but the human, so the run parks and shows them the message.
    Messaged,
    /// orrerix restarted under a live run. Every pane died with the process;
    /// nothing is re-opened until the human says so.
    Restart,
}

impl QuickHeld {
    /// Every reason, so the notice table and the frontend's label map can be
    /// checked against the enum rather than against a list someone has to
    /// remember to extend.
    pub const ALL: [QuickHeld; 18] = [
        QuickHeld::ReviewLimit,
        QuickHeld::PlanStalled,
        QuickHeld::FixStalled,
        QuickHeld::LaneStalled,
        QuickHeld::DriveStalled,
        QuickHeld::PlannerBlocked,
        QuickHeld::WorkerBlocked,
        QuickHeld::ReviewerBlocked,
        QuickHeld::RootBlocked,
        QuickHeld::PlannerGone,
        QuickHeld::WorkerGone,
        QuickHeld::ReviewerGone,
        QuickHeld::RootGone,
        QuickHeld::CapRefused,
        QuickHeld::Unresumable,
        QuickHeld::ProviderLimit,
        QuickHeld::Messaged,
        QuickHeld::Restart,
    ];

    /// The wire/audit spelling — the same string serde writes.
    pub fn as_str(self) -> &'static str {
        match self {
            QuickHeld::ReviewLimit => "review-limit",
            QuickHeld::PlanStalled => "plan-stalled",
            QuickHeld::FixStalled => "fix-stalled",
            QuickHeld::LaneStalled => "lane-stalled",
            QuickHeld::DriveStalled => "drive-stalled",
            QuickHeld::PlannerBlocked => "planner-blocked",
            QuickHeld::WorkerBlocked => "worker-blocked",
            QuickHeld::ReviewerBlocked => "reviewer-blocked",
            QuickHeld::RootBlocked => "root-blocked",
            QuickHeld::PlannerGone => "planner-gone",
            QuickHeld::WorkerGone => "worker-gone",
            QuickHeld::ReviewerGone => "reviewer-gone",
            QuickHeld::RootGone => "root-gone",
            QuickHeld::CapRefused => "cap-refused",
            QuickHeld::Unresumable => "unresumable",
            QuickHeld::ProviderLimit => "provider-limit",
            QuickHeld::Messaged => "messaged",
            QuickHeld::Restart => "restart",
        }
    }

    /// Parse a reason word. `None` for anything unrecognized.
    pub fn parse(s: &str) -> Option<QuickHeld> {
        QuickHeld::ALL.into_iter().find(|r| r.as_str() == s.trim())
    }

    /// The hold a `report(blocked)` from `side` produces.
    pub fn blocked(side: QuickSide) -> QuickHeld {
        match side {
            QuickSide::Planner => QuickHeld::PlannerBlocked,
            QuickSide::Worker => QuickHeld::WorkerBlocked,
            QuickSide::Reviewer => QuickHeld::ReviewerBlocked,
            QuickSide::Root => QuickHeld::RootBlocked,
        }
    }

    /// The hold a dead pane on `side` produces.
    pub fn gone(side: QuickSide) -> QuickHeld {
        match side {
            QuickSide::Planner => QuickHeld::PlannerGone,
            QuickSide::Worker => QuickHeld::WorkerGone,
            QuickSide::Reviewer => QuickHeld::ReviewerGone,
            QuickSide::Root => QuickHeld::RootGone,
        }
    }

    /// The one sentence this hold's notice leads with. Held here rather than
    /// in the wiring so that a reason cannot be added without one, and so the
    /// notice text is checkable from a Tauri-free test.
    pub fn notice_line(self) -> &'static str {
        match self {
            QuickHeld::ReviewLimit => {
                "the reviewer asked for changes on the last review round this run allows"
            }
            QuickHeld::PlanStalled => "the planner has not reported inside its time limit",
            QuickHeld::FixStalled => {
                "the worker has not reported its fix inside the fix time limit"
            }
            QuickHeld::LaneStalled => "the reviewer has not reported inside the review time limit",
            QuickHeld::DriveStalled => "the run reached its overall time bound without finishing",
            QuickHeld::PlannerBlocked => "the planner reported blocked",
            QuickHeld::WorkerBlocked => "the worker reported blocked",
            QuickHeld::ReviewerBlocked => "the reviewer reported blocked",
            QuickHeld::RootBlocked => "the agent running the task reported blocked",
            QuickHeld::PlannerGone => "the planner's pane closed before it reported",
            QuickHeld::WorkerGone => "the worker's pane closed before it reported",
            QuickHeld::ReviewerGone => "the reviewer's pane closed before it reported",
            QuickHeld::RootGone => "the pane running the task closed before it reported",
            QuickHeld::CapRefused => {
                "the next pane could not be opened: the group is at its live-agent limit"
            }
            QuickHeld::Unresumable => "the next pane could not be opened",
            QuickHeld::ProviderLimit => {
                "the provider of the pane holding the turn reported a usage limit"
            }
            QuickHeld::Messaged => {
                "one of this run's agents sent a message that needs you — there is no \
                 orchestrator to answer it"
            }
            QuickHeld::Restart => {
                "orrerix restarted while this run was live, so its panes are gone — nothing is \
                 re-opened until you resume it"
            }
        }
    }
}

/// A transition this machine does not have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuickInvalidTransition {
    pub from: QuickState,
    pub to: QuickState,
}

impl fmt::Display for QuickInvalidTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "quick drive: no transition {} -> {}", self.from.as_str(), self.to.as_str())
    }
}

impl std::error::Error for QuickInvalidTransition {}

/// The whole arc table, listed rather than inferred.
///
/// **A resume returns to the state the hold came FROM**, which is why `Held`
/// has one outgoing arc per working state rather than one in total: a run
/// parked while the reviewer held the turn and a run parked while the planner
/// did resume into different work.
pub fn transition(
    from: QuickState,
    to: QuickState,
) -> Result<QuickState, QuickInvalidTransition> {
    use QuickState::*;
    let ok = match (from, to) {
        // 1. The planner reported done; its body is the plan.
        (PlanWait, WorkWait) => true,
        // 2. The worker reported done and a review step is on — also the
        //    human's "hand to reviewer now", which takes this arc and spends
        //    nothing.
        (WorkWait, ReviewWait) | (FixWait, ReviewWait) => true,
        // 3. The worker reported done and there is no review step.
        (WorkWait, Satisfied) => true,
        // 4. The reviewer asked for changes inside the round bound — also the
        //    human's "send back now".
        (ReviewWait, FixWait) => true,
        // 5. The reviewer approved.
        (ReviewWait, Satisfied) => true,
        // 6. A described run's task (#3723). It BEGINS when the root first
        //    puts a helper to work, and its root's `done` returns the run to
        //    idle rather than ending it: the pane stays, and the human may
        //    give it another task. A described run has no arc to `satisfied`.
        (RootIdle, RootWait) | (RootWait, RootIdle) => true,
        // 7. Every hold, from every working state — and from `root-idle`,
        //    whose one hold is a root pane that could not be opened.
        (PlanWait | WorkWait | ReviewWait | FixWait | RootWait | RootIdle, Held) => true,
        // 8. Resume, back into the state the hold came from.
        (Held, PlanWait)
        | (Held, WorkWait)
        | (Held, ReviewWait)
        | (Held, FixWait)
        | (Held, RootWait)
        | (Held, RootIdle) => true,
        // 9. Stop. From any non-terminal state, `held` included: stopping is a
        //    parked run's second way out. From `root-idle` it is also what an
        //    idle root's pane closing does — there is nothing left to resume.
        (PlanWait | WorkWait | ReviewWait | FixWait | RootWait | RootIdle | Held, Cancelled) => {
            true
        }
        _ => false,
    };
    if ok {
        Ok(to)
    } else {
        Err(QuickInvalidTransition { from, to })
    }
}

// ── the bounds ──────────────────────────────────────────────────────────────

/// The default overall time bound of a quick run, in minutes.
///
/// Its own figure rather than `driver.drive_timeout_minutes`' 720: a quick run
/// that is still going after four hours is the thing this feature says it is
/// not. The RANGE is the review driver's own
/// ([`DRIVER_DRIVE_TIMEOUT_MIN`]..=[`DRIVER_DRIVE_TIMEOUT_MAX`]), so a human
/// who wants 720 can ask for it.
pub const QUICK_DRIVE_TIMEOUT_DEFAULT_MIN: u32 = 240;

/// The default number of review rounds — the review driver's own ceiling.
pub const QUICK_REVIEW_ROUNDS_DEFAULT: u32 = MAX_ROUNDS_CEILING;

/// How long a quick run's planner may hold the turn — the plan driver's own
/// default, read rather than re-spelled.
pub const QUICK_PLANNER_TIMEOUT_MIN: u32 = crate::plandrive::PLANNER_TIMEOUT_MINUTES_DEFAULT;

/// The longest task description a run records, in characters. The wiring
/// sanitizes and caps at this figure; it is here so the record's own bound is
/// stated beside the record.
pub const QUICK_TASK_CAP: usize = 8_000;

/// The longest single human note, in characters.
pub const QUICK_NOTE_CAP: usize = 2_000;

/// How many undelivered human notes one run keeps. A note is consumed by the
/// next brief; past this many the OLDEST is dropped, so a human who keeps
/// typing while a pane is busy cannot grow the next brief without bound.
pub const QUICK_MAX_PENDING_NOTES: usize = 8;

/// The run's overall time bound brought inside the review driver's own range.
pub fn clamp_drive_timeout(minutes: u32) -> u32 {
    minutes.clamp(DRIVER_DRIVE_TIMEOUT_MIN, DRIVER_DRIVE_TIMEOUT_MAX)
}

/// The run's review-round bound brought inside `1..=`[`MAX_ROUNDS_CEILING`].
pub fn clamp_review_rounds(rounds: u32) -> u32 {
    rounds.clamp(1, MAX_ROUNDS_CEILING)
}

/// The bounds one quick run runs against.
///
/// **The review driver's own [`DriveLimits`], reused rather than re-declared**,
/// plus the one figure that type has no field for. `max_review_rounds`,
/// `lane_timeout_minutes`, `fix_timeout_minutes` and `drive_timeout_minutes`
/// mean here exactly what they mean there; the CI and rebase counters ride
/// along unread, because a quick run observes neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuickLimits {
    pub drive: DriveLimits,
    /// How long the planner may hold the turn, in minutes.
    pub planner_timeout_minutes: u64,
}

impl Default for QuickLimits {
    fn default() -> Self {
        QuickLimits::new(QUICK_REVIEW_ROUNDS_DEFAULT, QUICK_DRIVE_TIMEOUT_DEFAULT_MIN)
    }
}

impl QuickLimits {
    /// Build the limits from the two figures a human sets on the launcher
    /// form, clamping both. The three pane timeouts are the review and plan
    /// drivers' own defaults.
    pub fn new(max_review_rounds: u32, drive_timeout_minutes: u32) -> QuickLimits {
        let d = DriveLimits::default();
        QuickLimits {
            drive: DriveLimits::new(
                clamp_review_rounds(max_review_rounds),
                d.max_ci_attempts,
                d.max_rebase_attempts,
                d.lane_timeout_minutes,
                d.fix_timeout_minutes,
                clamp_drive_timeout(drive_timeout_minutes) as u64,
            ),
            planner_timeout_minutes: QUICK_PLANNER_TIMEOUT_MIN as u64,
        }
    }

    /// These limits with every bound brought back inside its range.
    ///
    /// [`decide`] calls this unconditionally and reads only the result, for
    /// [`DriveLimits`]' own reason: the fields are `pub` so a caller can read
    /// them, which means a caller can also assign one, and a boundary that
    /// holds only when the expected constructor was used is not a boundary.
    pub fn clamped(self) -> QuickLimits {
        let drive = self.drive.clamped();
        let timeout = drive
            .drive_timeout_minutes
            .clamp(DRIVER_DRIVE_TIMEOUT_MIN as u64, DRIVER_DRIVE_TIMEOUT_MAX as u64);
        QuickLimits {
            drive: DriveLimits::new(
                drive.max_review_rounds,
                drive.max_ci_attempts,
                drive.max_rebase_attempts,
                drive.lane_timeout_minutes,
                drive.fix_timeout_minutes,
                timeout,
            ),
            planner_timeout_minutes: self.planner_timeout_minutes,
        }
    }

    /// The overall bound, in milliseconds.
    pub fn drive_timeout_ms(&self) -> u64 {
        self.drive.drive_timeout_minutes.saturating_mul(60_000)
    }
}

/// How long a run may sit in `state` before it parks, and the reason it parks
/// with — or `None` where only the overall bound applies.
///
/// **One clock per state, because one pane holds each state.** The review
/// driver keeps a per-lane silence clock AND a per-state elapsed bound because
/// several lanes share `review-wait`; here they are the same interval, so
/// there is one figure and the hold is named for the side that went quiet.
/// `work-wait` has no bound of its own on purpose: the first pass IS the task,
/// and the only honest limit on it is the one the human set for the whole run.
pub fn state_bound(state: QuickState, limits: &QuickLimits) -> Option<(u64, QuickHeld)> {
    let ms = |minutes: u64| minutes.saturating_mul(60_000);
    match state {
        QuickState::PlanWait => Some((ms(limits.planner_timeout_minutes), QuickHeld::PlanStalled)),
        QuickState::ReviewWait => {
            Some((ms(limits.drive.lane_timeout_minutes), QuickHeld::LaneStalled))
        }
        QuickState::FixWait => Some((ms(limits.drive.fix_timeout_minutes), QuickHeld::FixStalled)),
        QuickState::WorkWait
        | QuickState::RootWait
        | QuickState::RootIdle
        | QuickState::Held
        | QuickState::Satisfied
        | QuickState::Cancelled => None,
    }
}

// ── the record ──────────────────────────────────────────────────────────────

/// `quick_drive.json`'s schema version.
pub const QUICK_DRIVE_VERSION: u32 = 1;

/// The file's name inside the group dir, beside `review_drives.json` and
/// `plan_drives.json`. The group dir itself is built by `group_dir_at`, the
/// only place a group id becomes a path.
pub const QUICK_DRIVE_FILE: &str = "quick_drive.json";

/// The directory, inside the group dir, that holds a run's own documents: the
/// plan, each round's findings and the messages its agents sent.
pub const QUICK_DIR: &str = "quick";

/// The plan's file name inside [`QUICK_DIR`].
pub const QUICK_PLAN_FILE: &str = "plan.md";

/// The messages file's name inside [`QUICK_DIR`].
pub const QUICK_MESSAGES_FILE: &str = "messages.md";

/// The file one review round's findings are written to, inside [`QUICK_DIR`].
///
/// **`round` is a `u32`, and that type is the whole of the path argument**: a
/// number cannot carry a separator, a `..` or a device name, so this is a file
/// name no caller can steer. `tests/pathseg.rs` carries the row that says so.
pub fn findings_file_name(round: u32) -> String {
    format!("round-{round}.md")
}

/// One pane a run opened, and the session it runs.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QuickPane {
    /// The pane's agent id — the interception key, minted by orrerix at spawn.
    /// Empty until the pane has been opened.
    #[serde(default)]
    pub agent: String,
    /// The session the pane runs, which is what a hand-back resumes.
    #[serde(default)]
    pub session: String,
    /// Earlier panes this side ran in. Still this run's — their traffic is
    /// consumed rather than misdelivered — but only [`agent`](Self::agent)
    /// holds a turn.
    #[serde(default)]
    pub superseded: Vec<String>,
    /// Fields written by a newer build, preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl QuickPane {
    /// Record the pane that now runs this side. The one it replaces, if it is
    /// a different pane, moves to [`superseded`](Self::superseded).
    pub fn record(&mut self, agent: &str, session: &str) {
        if !self.agent.is_empty() && self.agent != agent && !self.superseded.contains(&self.agent)
        {
            self.superseded.push(self.agent.clone());
        }
        self.superseded.retain(|a| a != agent);
        self.agent = agent.to_string();
        if !session.is_empty() {
            self.session = session.to_string();
        }
    }

    /// Whether `agent_id` is this side's current pane (`Some(true)`), an
    /// earlier one (`Some(false)`), or not this side's at all (`None`).
    ///
    /// An empty id matches nobody, which a bare string comparison against an
    /// unopened side's empty `agent` would not guarantee on its own.
    pub fn standing(&self, agent_id: &str) -> Option<bool> {
        if agent_id.is_empty() {
            return None;
        }
        if self.agent == agent_id {
            return Some(true);
        }
        self.superseded.iter().any(|a| a == agent_id).then_some(false)
    }
}

/// One quick run — the whole of what `quick_drive.json` holds for a group.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuickDriveRecord {
    /// The task, as the human typed it on the launcher form and the wiring
    /// sanitized it. **Empty for a described run** (#3723): its task is given
    /// in the root's own pane and never reaches the record. A described
    /// record written before that change carries the text it was started
    /// with; it is kept, for the chip and the list to show, until that task
    /// is over and the next one begins.
    pub task: String,
    /// Private on purpose: [`advance`](QuickDriveRecord::advance) is the only
    /// way to change it, and it goes through [`transition`].
    state: QuickState,
    /// Set exactly when `state == Held`.
    #[serde(default)]
    pub held_reason: Option<QuickHeld>,
    /// What the run was doing when it parked — the state
    /// [`resume_target`](QuickDriveRecord::resume_target) returns to.
    #[serde(default)]
    pub held_from: Option<QuickState>,
    /// Whether this run has a plan step.
    pub plan_step: bool,
    /// Whether this run has a review step.
    pub review_step: bool,
    /// Whether this is a DESCRIBED run (#3679 way 2): one root pane was given
    /// the task and decides for itself whether to plan, who works and who
    /// reviews. The run then has exactly one working state, `root-wait`, and
    /// the panes the root opens are its delegates rather than sides of the run
    /// — their reports go to the root's pane and the record never names them.
    ///
    /// A field of its own rather than read off the state, because a run that
    /// has parked or ended is no longer IN `root-wait`, and what the run was
    /// is exactly what the question "is this pane one of the run's sides?"
    /// still has to answer then. Defaulted, so a record written before way 2
    /// existed reads as a steps run, which is what every such record is.
    #[serde(default)]
    pub described: bool,
    /// The CLI and model a described run's root was started on.
    ///
    /// The roster carries them too, as the root's block — but a block whose
    /// kind no workflow file can name is dropped when `group.json` is read
    /// back, which is what a restart does. Resume has to re-open the root on
    /// the CLI that owns its session, so the run keeps its own copy.
    #[serde(default)]
    pub root_cli: String,
    #[serde(default)]
    pub root_model: String,
    /// The ref the worker's branch was cut from, which the review brief diffs
    /// against. Empty means the repo's default branch.
    #[serde(default)]
    pub base: String,
    /// This run's review-round bound. Persisted, because the run has no
    /// workflow file to re-read it from: the launcher form IS its policy.
    pub max_review_rounds: u32,
    /// This run's overall time bound, in minutes. Persisted for the same
    /// reason.
    pub drive_timeout_minutes: u32,
    /// When the run began. **Absolute**, never an elapsed figure: a stored
    /// elapsed time is stale the instant it is written and meaningless across
    /// a restart. For a described run this is when its CURRENT task began
    /// ([`begin_task`](QuickDriveRecord::begin_task)); before the first one,
    /// when the run was started.
    pub started_ms: u64,
    /// When the record entered its current state — the per-state bound's
    /// anchor.
    pub state_since_ms: u64,
    /// The overall bound's anchor: `started_ms` at first, re-stamped by every
    /// resume. `0` in a record written before the field existed, and every
    /// read falls back to `started_ms` rather than subtracting from an unset
    /// anchor.
    #[serde(default)]
    pub clock_ms: u64,
    /// How many review rounds this run has SPENT — one per `request_changes`
    /// that went back to the worker. **Required, with no default**: a
    /// defaulted zero silently re-grants a whole fresh budget, which is
    /// `reviewdrive::Counters`' own rule.
    pub review_rounds: u32,
    /// How many reviews have been recorded in total. Monotonic — never reset
    /// by a resume — because it names the findings files, and a reset would
    /// overwrite an earlier round's.
    #[serde(default)]
    pub reviews_total: u32,
    /// How many tasks a described run's root has BEGUN (#3723) — one per
    /// `root-idle → root-wait`. It numbers the finish notices, so two finished
    /// tasks in one pane do not read as the same one. Defaulted: a record
    /// written before the field existed is on its first task, which the
    /// notice then simply does not number.
    #[serde(default)]
    pub task_seq: u32,
    #[serde(default)]
    pub planner: QuickPane,
    #[serde(default)]
    pub worker: QuickPane,
    #[serde(default)]
    pub reviewer: QuickPane,
    /// A described run's root pane. Empty in a steps run.
    #[serde(default)]
    pub root: QuickPane,
    /// The worker's worktree, recorded at its spawn — where the reviewer is
    /// opened. orrerix-made, never caller-supplied.
    #[serde(default)]
    pub worker_cwd: String,
    /// The branch the worker's worktree was cut on.
    #[serde(default)]
    pub worker_branch: String,
    /// The PR this run's branch has, if one is known — named by the worker's
    /// own `report(ref:)` or found on the branch at the hand-off.
    #[serde(default)]
    pub pr: Option<u64>,
    /// The worker's last `report(done)` note, quoted in the review brief.
    #[serde(default)]
    pub worker_note: String,
    /// The reviewer's last one-line verdict note, quoted in the fix brief and
    /// the finish notice.
    #[serde(default)]
    pub review_note: String,
    /// What the hold's notice quotes: a `blocked` note, a message, a refusal.
    #[serde(default)]
    pub held_note: String,
    /// Notes the human added that no brief has carried yet.
    #[serde(default)]
    pub notes: Vec<String>,
    /// Whether the pane holding the turn has already been told, this turn,
    /// that a `report(progress)` moves nothing.
    #[serde(default)]
    pub progress_answered: bool,
    /// **The pane holding the turn has not been handed its brief yet.** Set by
    /// every arc into a working state and cleared by the wiring once the brief
    /// has been delivered; [`decide`] answers `None` while it is set, because
    /// a turn nobody has been given is not a turn to judge.
    ///
    /// In `root-idle` it means the one thing that state can still be owed:
    /// **the root's pane has not been opened yet**. Nothing is typed when it
    /// is — an idle root is handed no brief — so there it is cleared by the
    /// pane opening, not by a delivery.
    #[serde(default)]
    pub brief_pending: bool,
    /// The hold the pending brief is being delivered AFTER — set by
    /// [`resume`](Self::resume) and cleared when the brief lands, so the pane
    /// that is handed the same brief a second time is told why.
    #[serde(default)]
    pub resumed_from: Option<QuickHeld>,
    /// The pending brief is the human's forced hand-off rather than the other
    /// side's report. Set by [`force_handoff`](Self::force_handoff), cleared
    /// when the brief lands.
    #[serde(default)]
    pub forced: bool,
    /// The id of the open needs-you item this run raised, so the next thing
    /// that happens to the run can take it back instead of leaving a second
    /// item beside it. Empty when none is open.
    #[serde(default)]
    pub notice_item: String,
    /// Fields written by a newer build, preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl QuickDriveRecord {
    /// A fresh run, in its first working state with that state's brief still
    /// to deliver.
    pub fn new(
        task: &str,
        plan_step: bool,
        review_step: bool,
        base: &str,
        limits: &QuickLimits,
        now_ms: u64,
    ) -> QuickDriveRecord {
        let limits = limits.clamped();
        QuickDriveRecord {
            task: task.to_string(),
            state: if plan_step { QuickState::PlanWait } else { QuickState::WorkWait },
            held_reason: None,
            held_from: None,
            plan_step,
            review_step,
            described: false,
            root_cli: String::new(),
            root_model: String::new(),
            base: base.trim().to_string(),
            max_review_rounds: limits.drive.max_review_rounds,
            drive_timeout_minutes: limits.drive.drive_timeout_minutes as u32,
            started_ms: now_ms,
            state_since_ms: now_ms,
            clock_ms: now_ms,
            review_rounds: 0,
            reviews_total: 0,
            task_seq: 0,
            planner: QuickPane::default(),
            worker: QuickPane::default(),
            reviewer: QuickPane::default(),
            root: QuickPane::default(),
            worker_cwd: String::new(),
            worker_branch: String::new(),
            pr: None,
            worker_note: String::new(),
            review_note: String::new(),
            held_note: String::new(),
            notes: Vec::new(),
            progress_answered: false,
            brief_pending: true,
            resumed_from: None,
            forced: false,
            notice_item: String::new(),
            extra: BTreeMap::new(),
        }
    }

    /// A fresh DESCRIBED run (#3679 way 2, #3723): in `root-idle`, with its
    /// root's pane still to open and **no task**. The human gives the task in
    /// that pane, so the record never holds one.
    ///
    /// It has no plan step and no review step of its own: whether a task is
    /// planned or reviewed is the root's decision, made in its pane. The round
    /// bound is carried anyway, because the root's instructions quote it.
    pub fn new_described(base: &str, limits: &QuickLimits, now_ms: u64) -> QuickDriveRecord {
        let mut rec = QuickDriveRecord::new("", false, false, base, limits, now_ms);
        rec.state = QuickState::RootIdle;
        rec.described = true;
        rec
    }

    /// **Begin a described run's next task** (#3723): `root-idle → root-wait`,
    /// with the run's clock started HERE. Answers the task's number.
    ///
    /// This is the whole of "the time limit does not count idle time": an idle
    /// root is read against no clock at all, and the bound a task runs against
    /// is anchored at the moment the root first put a helper to work — not at
    /// the moment the pane opened, and not at the previous task's start.
    ///
    /// **No brief is owed.** Every other arc into a working state hands the
    /// turn to a pane that has to be told; here the root took the turn itself,
    /// by acting, so `brief_pending` is cleared rather than left for the
    /// wiring to "deliver" something into a pane that is mid-turn.
    ///
    /// The last task's account — its note, its pull request, the id of the
    /// notice it raised — is dropped from the record. The notice ITSELF is not
    /// withdrawn: it says where finished work is, and that stays true.
    ///
    /// So is `task`. Only a record written before #3723 carries one — that
    /// build took the task on the form — and it describes the task that record
    /// was started with. Left in place it would be quoted in the finish or
    /// hold notice of every LATER task, each of which the human gave in the
    /// pane and none of which it describes.
    ///
    /// Refused from every state but `root-idle`. In particular a HELD run does
    /// not begin a task by its root acting; only the human's Resume moves it.
    pub fn begin_task(&mut self, now_ms: u64) -> Result<u32, QuickInvalidTransition> {
        if self.state != QuickState::RootIdle {
            return Err(QuickInvalidTransition { from: self.state, to: QuickState::RootWait });
        }
        self.advance(QuickState::RootWait, None, now_ms)?;
        self.brief_pending = false;
        self.started_ms = now_ms;
        self.clock_ms = now_ms;
        self.task_seq = self.task_seq.saturating_add(1);
        self.task.clear();
        self.worker_note.clear();
        self.pr = None;
        self.notice_item.clear();
        Ok(self.task_seq)
    }

    /// This run's state. The field is private so that every write goes through
    /// [`advance`](Self::advance) and therefore through [`transition`].
    pub fn state(&self) -> QuickState {
        self.state
    }

    /// The bounds this run was started with, rebuilt from the record.
    pub fn limits(&self) -> QuickLimits {
        QuickLimits::new(self.max_review_rounds, self.drive_timeout_minutes)
    }

    /// The pane record of one side.
    pub fn pane(&self, side: QuickSide) -> &QuickPane {
        match side {
            QuickSide::Planner => &self.planner,
            QuickSide::Worker => &self.worker,
            QuickSide::Reviewer => &self.reviewer,
            QuickSide::Root => &self.root,
        }
    }

    /// The pane record of one side, mutably.
    pub fn pane_mut(&mut self, side: QuickSide) -> &mut QuickPane {
        match side {
            QuickSide::Planner => &mut self.planner,
            QuickSide::Worker => &mut self.worker,
            QuickSide::Reviewer => &mut self.reviewer,
            QuickSide::Root => &mut self.root,
        }
    }

    /// Which side `agent_id` is a pane of, and whether it is that side's
    /// CURRENT pane.
    ///
    /// **Keyed on the agent id orrerix minted at spawn**, never on text a
    /// caller supplies — `rd_owner`'s property, for the same reason: a pane
    /// that could choose whether its report moves the run by naming something
    /// is a pane that can steer it.
    pub fn owner_of(&self, agent_id: &str) -> Option<(QuickSide, bool)> {
        QuickSide::ALL
            .into_iter()
            .find_map(|side| self.pane(side).standing(agent_id).map(|current| (side, current)))
    }

    /// Whether `agent_id` is the pane holding the turn right now.
    pub fn holds_turn(&self, agent_id: &str) -> bool {
        match self.state.turn() {
            Some(side) => self.pane(side).standing(agent_id) == Some(true),
            None => false,
        }
    }

    /// The review round the run is on, counted from 1 — what the brief and the
    /// status chip print as "round k of N".
    pub fn round(&self) -> u32 {
        self.review_rounds.saturating_add(1).min(self.max_review_rounds.max(1))
    }

    /// The round number the NEXT recorded review's findings file takes.
    pub fn next_findings_round(&self) -> u32 {
        self.reviews_total.saturating_add(1)
    }

    /// Move to `to`, through [`transition`].
    ///
    /// `held_reason` and `held_from` are set exactly when the destination is
    /// `Held` and cleared on every other arc, so "parked with no reason" and
    /// "working with a stale reason" are both unrepresentable rather than
    /// merely unlikely. Every arc into a working state sets
    /// [`brief_pending`](Self::brief_pending) and re-arms the progress answer,
    /// because both are properties of a TURN and a new turn has just begun.
    pub fn advance(
        &mut self,
        to: QuickState,
        held: Option<QuickHeld>,
        now_ms: u64,
    ) -> Result<(), QuickInvalidTransition> {
        transition(self.state, to)?;
        if to == QuickState::Held {
            self.held_from = Some(self.state);
            self.held_reason = held;
            self.brief_pending = false;
        } else {
            self.held_from = None;
            self.held_reason = None;
            self.held_note.clear();
            self.brief_pending = to.is_live();
        }
        self.progress_answered = false;
        // Both describe the brief of the turn that just ended. `resume` and
        // `force_handoff` set them again AFTER this returns.
        self.resumed_from = None;
        self.forced = false;
        self.state = to;
        self.state_since_ms = now_ms;
        Ok(())
    }

    /// Take one [`QuickStep`] — the arc and its cost in one place, so the two
    /// cannot come apart at a call site.
    ///
    /// A step out of `review-wait` that the reviewer caused counts a review
    /// ([`reviews_total`](Self::reviews_total)); one that sends findings back
    /// inside the bound also spends a round
    /// ([`review_rounds`](Self::review_rounds)).
    pub fn take(&mut self, step: &QuickStep, now_ms: u64) -> Result<(), QuickInvalidTransition> {
        self.advance(step.to, step.held, now_ms)?;
        if step.reviewed {
            self.reviews_total = self.reviews_total.saturating_add(1);
        }
        if step.spends_round {
            self.review_rounds = self.review_rounds.saturating_add(1);
        }
        Ok(())
    }

    /// Where a resume goes.
    ///
    /// The state the hold came from — except a `review-limit` hold, which came
    /// from `review-wait` and resumes into `fix-wait`: the reviewer has
    /// already answered, so handing it the turn again would be asking the same
    /// question twice. What a human resuming that hold wants is one more round,
    /// and the findings are already on disk.
    ///
    /// **Never a terminal or another hold**, whatever the file says: a
    /// `held_from` naming `satisfied` would resume a finished run into a state
    /// with no arcs out of it, and a hand-edited record must not be able to
    /// produce one.
    pub fn resume_target(&self) -> QuickState {
        if self.held_reason == Some(QuickHeld::ReviewLimit) {
            return QuickState::FixWait;
        }
        match self.held_from {
            // `root-idle` is a resume target too: its one hold is a root pane
            // that could not be opened, and resuming it is asking again.
            Some(s) if s.is_live() || s.is_idle() => s,
            _ => QuickState::WorkWait,
        }
    }

    /// Resume a parked run. Answers the state it resumed into.
    ///
    /// **A resume is a fresh grant, visibly**: the overall clock is re-stamped,
    /// and a run resumed off `review-limit` gets its round budget back. A
    /// human who has read the notice and resumed anyway is spending a second
    /// bound on purpose; resuming straight back onto the limit that parked the
    /// run would make the button a no-op.
    pub fn resume(&mut self, now_ms: u64) -> Result<QuickState, QuickInvalidTransition> {
        let to = self.resume_target();
        let was = self.held_reason;
        self.advance(to, None, now_ms)?;
        if was == Some(QuickHeld::ReviewLimit) {
            self.review_rounds = 0;
        }
        // A hold out of `root-idle` is a root pane that never opened, so the
        // resumed run is owed that pane again. `advance` leaves the flag clear
        // for an idle destination — right for a task that just finished, whose
        // root is sitting there — so it is set here, for this arc alone.
        if to.is_idle() {
            self.brief_pending = true;
        }
        self.clock_ms = now_ms;
        self.resumed_from = was;
        Ok(to)
    }

    /// The human's forced hand-off: give the turn to the other side now,
    /// without waiting for a report and **without spending a round**.
    ///
    /// From `work-wait` or `fix-wait` it hands to the reviewer; from
    /// `review-wait` it sends the work back. Refused everywhere else — a
    /// parked run is resumed first, and a planner has no other side to hand
    /// to. Refused from the worker's side when the run has no review step,
    /// because there is nobody to hand to.
    pub fn force_handoff(&mut self, now_ms: u64) -> Result<QuickState, QuickInvalidTransition> {
        let from = self.state;
        let to = match from {
            QuickState::WorkWait | QuickState::FixWait if self.review_step => {
                QuickState::ReviewWait
            }
            QuickState::ReviewWait => QuickState::FixWait,
            _ => return Err(QuickInvalidTransition { from, to: from }),
        };
        self.advance(to, None, now_ms)?;
        self.forced = true;
        Ok(to)
    }

    /// The pending brief landed in `agent`'s pane: record the pane, start the
    /// state's clock, and clear everything that was only true of a brief still
    /// to deliver.
    ///
    /// **The state clock starts HERE, not at the arc.** Opening a pane can take
    /// most of a minute, and a bound that had already been running while there
    /// was nobody to hold the turn would charge the pane for time it did not
    /// have.
    pub fn brief_delivered(&mut self, side: QuickSide, agent: &str, session: &str, now_ms: u64) {
        self.pane_mut(side).record(agent, session);
        self.brief_pending = false;
        self.resumed_from = None;
        self.forced = false;
        self.notes.clear();
        self.state_since_ms = now_ms;
    }

    /// A described run's root pane opened while the run is idle (#3723):
    /// record the pane and stop owing it. Nothing was typed into it, so there
    /// is no brief to have been delivered — this is
    /// [`brief_delivered`](Self::brief_delivered) under the name of what
    /// actually happened.
    pub fn root_opened(&mut self, agent: &str, session: &str, now_ms: u64) {
        self.brief_delivered(QuickSide::Root, agent, session, now_ms);
    }

    /// Queue one human note for the next brief, dropping the oldest past
    /// [`QUICK_MAX_PENDING_NOTES`].
    pub fn add_note(&mut self, note: &str) {
        self.notes.push(note.to_string());
        while self.notes.len() > QUICK_MAX_PENDING_NOTES {
            self.notes.remove(0);
        }
    }

    /// The overall bound's anchor.
    fn clock_anchor(&self) -> u64 {
        if self.clock_ms > 0 {
            self.clock_ms
        } else {
            self.started_ms
        }
    }

    /// How long the run has been running against its overall bound.
    pub fn age_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.clock_anchor())
    }

    /// How long it has been in its current state.
    pub fn state_elapsed_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.state_since_ms)
    }
}

/// The whole of `quick_drive.json`.
///
/// **One run, not a list.** A quick group is minted for one run and its id is
/// never handed to a second one while a pane of the first is alive, so the
/// file has no key to look a run up by.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuickDriveFile {
    /// Schema version. **Required** — a file with no version is malformed, not
    /// a v1 file.
    pub version: u32,
    #[serde(default)]
    pub run: Option<QuickDriveRecord>,
    /// Fields written by a newer build, preserved verbatim across a read/write
    /// cycle.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for QuickDriveFile {
    fn default() -> Self {
        QuickDriveFile { version: QUICK_DRIVE_VERSION, run: None, extra: BTreeMap::new() }
    }
}

impl QuickDriveFile {
    /// Whether this build understands the file's schema.
    pub fn version_supported(&self) -> bool {
        self.version == QUICK_DRIVE_VERSION
    }

    /// Whether the file holds a run that has not ENDED — working, parked, or
    /// a described run idle between tasks. Wider than
    /// [`QuickState::is_in_progress`], which leaves the idle one out.
    pub fn has_unfinished_run(&self) -> bool {
        self.run.as_ref().is_some_and(|r| !r.state().is_terminal())
    }
}

/// Why `quick_drive.json` could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuickStateError {
    /// The file is there and does not parse — torn, hand-edited, or naming a
    /// state this build does not know. Never repaired and never deleted.
    Malformed(String),
    /// A schema this build does not understand. Do not operate; do not write.
    Unsupported(u32),
    /// The file is there and could not be read at all.
    Io(String),
}

impl fmt::Display for QuickStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QuickStateError::Malformed(e) => write!(f, "quick_drive.json does not parse: {e}"),
            QuickStateError::Unsupported(v) => {
                write!(f, "quick_drive.json is schema version {v}, which this build cannot read")
            }
            QuickStateError::Io(e) => write!(f, "quick_drive.json could not be read: {e}"),
        }
    }
}

/// The group's quick-drive file.
pub fn state_path(group_dir: &Path) -> PathBuf {
    group_dir.join(QUICK_DRIVE_FILE)
}

/// Read the group's quick-drive state.
///
/// **An absent file is no run, not an error** — that is every group that is
/// not a quick group. Every other failure is a [`QuickStateError`], because
/// "there is no run" and "orrerix cannot tell whether there is one" want
/// different things from whoever asked.
pub fn load_state(group_dir: &Path) -> Result<QuickDriveFile, QuickStateError> {
    let text = match std::fs::read_to_string(state_path(group_dir)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(QuickDriveFile::default()),
        Err(e) => return Err(QuickStateError::Io(e.to_string())),
    };
    parse_state(&text)
}

/// The parse half of [`load_state`], without the file — so the refusals can be
/// pinned on a string.
pub fn parse_state(text: &str) -> Result<QuickDriveFile, QuickStateError> {
    let state: QuickDriveFile =
        serde_json::from_str(text).map_err(|e| QuickStateError::Malformed(e.to_string()))?;
    if !state.version_supported() {
        return Err(QuickStateError::Unsupported(state.version));
    }
    Ok(state)
}

/// Write the quick-drive state atomically, through
/// [`crate::fsatomic::atomic_write`] — the #133-hardened writer every other
/// drive record uses, and for its reason: a torn record is a run nobody can
/// resume or stop.
pub fn store_state(group_dir: &Path, state: &QuickDriveFile) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
    crate::fsatomic::atomic_write(&state_path(group_dir), &bytes).map_err(|e| e.to_string())
}

// ── the decision ────────────────────────────────────────────────────────────

/// What the pane holding the turn has said, if anything.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QuickSignal {
    /// Nothing this window.
    #[default]
    None,
    /// `report(done)` from a planner, a worker or the root.
    Done,
    /// `report(blocked)`, from any side.
    Blocked,
    /// A reviewer's `report(outcome: approved)`.
    Approved,
    /// A reviewer's `report(outcome: request_changes)` — and a reviewer's
    /// plain `done`, which is never read as approval.
    RequestChanges,
}

impl QuickSignal {
    /// The signal one `report` carries, from the side that sent it and the
    /// report's own status and outcome words.
    ///
    /// **A reviewer approves by saying `approved` and by no other word.** Its
    /// `done` with any other outcome — or with none — is `request_changes`:
    /// the run's one exit that hands unreviewed work to a human as approved
    /// would otherwise be a reviewer forgetting an argument. `progress` is no
    /// signal at all; nothing advances on a pane saying it is still going.
    pub fn from_report(side: QuickSide, status: &str, outcome: Option<&str>) -> QuickSignal {
        match status {
            "blocked" => QuickSignal::Blocked,
            "done" => match side {
                QuickSide::Reviewer if outcome == Some("approved") => QuickSignal::Approved,
                QuickSide::Reviewer => QuickSignal::RequestChanges,
                _ => QuickSignal::Done,
            },
            _ => QuickSignal::None,
        }
    }

    /// Any of the three words a finished pane can say.
    fn is_finished(self) -> bool {
        matches!(self, QuickSignal::Done | QuickSignal::Approved | QuickSignal::RequestChanges)
    }
}

/// Everything one tick resolved about the world, as values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuickFacts {
    /// The tick's clock. Carried here rather than as a second `decide`
    /// parameter so that the clock a decision is made against and the clock
    /// its facts were read at cannot come apart at a call site.
    pub now_ms: u64,
    /// What the pane holding the turn reported this window.
    pub signal: QuickSignal,
    /// Is the pane holding the turn still alive? **`false` only on a positive
    /// reading** — a pane the registry has no record of is "could not check",
    /// which the wiring reports as `true`.
    pub pane_alive: bool,
    /// A pane of this run called `message_orchestrator` this window.
    pub messaged: bool,
    /// The provider of the pane holding the turn has reported a usage limit.
    pub provider_limited: bool,
}

impl QuickFacts {
    /// Nothing happened: no signal, a live pane, no message, no limit.
    pub fn quiet(now_ms: u64) -> QuickFacts {
        QuickFacts {
            now_ms,
            signal: QuickSignal::None,
            pane_alive: true,
            messaged: false,
            provider_limited: false,
        }
    }
}

/// One tick's decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuickStep {
    /// The state to move to.
    pub to: QuickState,
    /// Set exactly when `to == Held`.
    pub held: Option<QuickHeld>,
    /// This step is a reviewer's verdict being recorded.
    pub reviewed: bool,
    /// This step spends one review round.
    pub spends_round: bool,
}

impl QuickStep {
    /// A move with no hold and no cost.
    pub fn to(to: QuickState) -> QuickStep {
        QuickStep { to, held: None, reviewed: false, spends_round: false }
    }
    /// A park, with its reason.
    pub fn held(reason: QuickHeld) -> QuickStep {
        QuickStep { to: QuickState::Held, held: Some(reason), reviewed: false, spends_round: false }
    }
    /// This step, marked as a reviewer's verdict.
    fn reviewed(self) -> QuickStep {
        QuickStep { reviewed: true, ..self }
    }
    /// This step, marked as spending a review round.
    fn spending_round(self) -> QuickStep {
        QuickStep { spends_round: true, ..self }
    }
}

/// **The whole of one tick's decision** — the only function that says what a
/// quick run does next, and it touches nothing.
///
/// `None` means *stay where you are*, which is the common answer: a pane that
/// is still working is a run with nothing to do.
///
/// # Why the order of the arms is the design
///
/// 0. **A brief still to deliver decides nothing.** The state has moved and
///    the pane it moved to has not been told; judging that pane's liveness or
///    its silence now would park a run on a hand-off that has not happened.
/// 1. **`blocked` outranks everything the same pane could also have said** —
///    it is the word that needs a human.
/// 2. **The turn-holder's own report is the arc.** A reviewer's anything-but-
///    `approved` is `request_changes`, here as well as in
///    [`QuickSignal::from_report`], so the property holds for a caller that
///    built its facts another way.
/// 3. **A message parks AFTER a report has been honoured**, so a pane that
///    messaged and then finished hands the turn on first and the run parks in
///    the next state — where a resume loses nothing.
/// 4. **A provider limit, then a dead pane, then the state's own clock.**
/// 5. **The overall bound is last**, so a run that has just moved is never
///    parked for being old on the tick it did something.
pub fn decide(rec: &QuickDriveRecord, facts: &QuickFacts, limits: &QuickLimits) -> Option<QuickStep> {
    use QuickState::*;
    let limits = limits.clamped();
    let state = rec.state();
    // Parked and terminal runs are not advanced by a tick.
    let side = state.turn()?;
    // 0.
    if rec.brief_pending {
        return None;
    }
    // 1.
    if facts.signal == QuickSignal::Blocked {
        return Some(QuickStep::held(QuickHeld::blocked(side)));
    }
    // 2.
    if facts.signal.is_finished() {
        let step = match state {
            PlanWait => QuickStep::to(WorkWait),
            WorkWait if rec.review_step => QuickStep::to(ReviewWait),
            WorkWait => QuickStep::to(Satisfied),
            FixWait => QuickStep::to(ReviewWait),
            // #3723: the root's `done` ends a TASK. The run goes back to idle
            // and its pane waits for the next one.
            RootWait => QuickStep::to(RootIdle),
            ReviewWait if facts.signal == QuickSignal::Approved => {
                QuickStep::to(Satisfied).reviewed()
            }
            // **Check before bump**, `reviewdrive::counter_exhausted`'s
            // ordering: the round being answered is `review_rounds + 1`, and
            // it is the LAST one exactly when that reaches the bound.
            ReviewWait
                if rec.review_rounds.saturating_add(1) >= limits.drive.max_review_rounds =>
            {
                QuickStep::held(QuickHeld::ReviewLimit).reviewed()
            }
            ReviewWait => QuickStep::to(FixWait).reviewed().spending_round(),
            // Unreachable: `turn()` answered `None` for all four above.
            // Spelled out rather than caught by `_` so a tenth state cannot
            // land here silently.
            RootIdle | Held | Satisfied | Cancelled => return None,
        };
        return Some(step);
    }
    // 3.
    if facts.messaged {
        return Some(QuickStep::held(QuickHeld::Messaged));
    }
    // 4.
    if facts.provider_limited {
        return Some(QuickStep::held(QuickHeld::ProviderLimit));
    }
    if !facts.pane_alive {
        return Some(QuickStep::held(QuickHeld::gone(side)));
    }
    if let Some((bound, reason)) = state_bound(state, &limits) {
        if rec.state_elapsed_ms(facts.now_ms) >= bound {
            return Some(QuickStep::held(reason));
        }
    }
    // 5.
    if rec.age_ms(facts.now_ms) >= limits.drive_timeout_ms() {
        return Some(QuickStep::held(QuickHeld::DriveStalled));
    }
    None
}

// ── the audit vocabulary ────────────────────────────────────────────────────

/// Every audit action the quick drive writes. One module so the wiring cannot
/// spell an action two ways, and so a reader filtering the audit log has the
/// whole list in one place.
pub mod audit_action {
    /// A run was started.
    pub const STARTED: &str = "qd-started";
    /// A start was refused before any run existed.
    pub const REFUSED: &str = "qd-refused";
    /// A pane's `report` was consumed by the run instead of being delivered.
    pub const CONSUMED: &str = "qd-consumed";
    /// A pane's `message_orchestrator` was recorded for the human.
    pub const MESSAGE: &str = "qd-message";
    /// The run moved from one working state to another.
    pub const ADVANCED: &str = "qd-advanced";
    /// A brief was delivered to the pane now holding the turn.
    pub const HANDOFF: &str = "qd-handoff";
    /// The run parked.
    pub const HELD: &str = "qd-held";
    /// The run finished.
    pub const SATISFIED: &str = "qd-satisfied";
    /// The human stopped the run.
    pub const CANCELLED: &str = "qd-cancelled";
    /// The human resumed a parked run.
    pub const RESUMED: &str = "qd-resumed";
    /// The human forced a hand-off.
    pub const FORCED: &str = "qd-forced";
    /// The human added a note to the run.
    pub const NOTE: &str = "qd-note";
    /// The plan or a round's findings were written to the run's directory.
    pub const DOCUMENT: &str = "qd-document";
    /// The finish or hold notice was raised.
    pub const NOTICE: &str = "qd-notice";
    /// `quick_drive.json` is there and could not be read.
    pub const STATE_UNREADABLE: &str = "qd-state-unreadable";
    /// A live run was found after a restart and parked.
    pub const RESTART_PARKED: &str = "qd-restart-parked";
    /// A described run's root pane was opened idle — nothing was typed (#3723).
    pub const OPENED: &str = "qd-opened";
    /// A described run's root put a helper to work, which begins a task and
    /// starts its clock (#3723).
    pub const TASK_BEGUN: &str = "qd-task-begun";
    /// An idle described run ended because its root's pane is gone (#3723).
    pub const CLOSED: &str = "qd-closed";
    /// An idle root's session watch was held back, or started (#3723).
    pub const SESSION_WATCH: &str = "qd-session-watch";
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_000_000;
    const MIN: u64 = 60_000;

    fn run(plan: bool, review: bool) -> QuickDriveRecord {
        let mut r = QuickDriveRecord::new(
            "add a --json flag",
            plan,
            review,
            "main",
            &QuickLimits::default(),
            T0,
        );
        // The first brief has been delivered; every test below is about what
        // happens AFTER a pane holds the turn, except the one that says so.
        r.brief_pending = false;
        r
    }

    /// Hand the pending brief over, as the wiring does after every arc.
    fn delivered(mut r: QuickDriveRecord) -> QuickDriveRecord {
        r.brief_pending = false;
        r
    }

    fn said(signal: QuickSignal) -> QuickFacts {
        QuickFacts { signal, ..QuickFacts::quiet(T0 + 1) }
    }

    fn step(r: &QuickDriveRecord, f: &QuickFacts) -> Option<QuickStep> {
        decide(r, f, &r.limits())
    }

    /// Drive `r` one tick with `signal`, taking whatever `decide` answers.
    fn drive(r: &mut QuickDriveRecord, signal: QuickSignal) -> Option<QuickStep> {
        let s = step(r, &said(signal));
        if let Some(s) = &s {
            r.take(s, T0 + 1).expect("decide proposed an arc the table refuses");
            r.brief_pending = false;
        }
        s
    }

    // ── the table ──────────────────────────────────────────────────────────

    /// The arc table, as a list a reader can check against the module doc's
    /// diagram — and the list the walk below compares `transition` to, pair by
    /// pair, so an arc added to one and not the other fails here.
    fn expected_arcs() -> Vec<(QuickState, QuickState)> {
        use QuickState::*;
        let mut arcs = vec![
            (PlanWait, WorkWait),
            (WorkWait, ReviewWait),
            (FixWait, ReviewWait),
            (WorkWait, Satisfied),
            (ReviewWait, FixWait),
            (ReviewWait, Satisfied),
            // A described run's task: begun by its root, ended by its root.
            (RootIdle, RootWait),
            (RootWait, RootIdle),
        ];
        // `root-idle` holds, resumes and stops like a working state — its one
        // hold is a root pane that could not be opened.
        for from in [PlanWait, WorkWait, ReviewWait, FixWait, RootWait, RootIdle] {
            arcs.push((from, Held));
            arcs.push((Held, from));
            arcs.push((from, Cancelled));
        }
        arcs.push((Held, Cancelled));
        arcs
    }

    #[test]
    fn every_pair_of_states_is_either_a_listed_arc_or_refused() {
        let expected = expected_arcs();
        let mut allowed = 0;
        for from in QuickState::ALL {
            for to in QuickState::ALL {
                let listed = expected.contains(&(from, to));
                let got = transition(from, to);
                assert_eq!(
                    got.is_ok(),
                    listed,
                    "{} -> {}: transition says {:?}, the table says {}",
                    from.as_str(),
                    to.as_str(),
                    got,
                    listed
                );
                allowed += usize::from(got.is_ok());
            }
        }
        // The walk saw every arc the list names — a list that silently lost a
        // row would otherwise pass against a table that lost the same one.
        assert_eq!(allowed, expected.len());
        assert_eq!(expected.len(), 27);
        // A described run never reaches `satisfied`: its root's `done` ends a
        // task and returns the run to idle. The arc that used to end the run
        // is gone, and nothing else leads there from either root state.
        for from in [QuickState::RootWait, QuickState::RootIdle] {
            assert!(transition(from, QuickState::Satisfied).is_err(), "{}", from.as_str());
        }
    }

    #[test]
    fn a_terminal_state_has_no_way_out() {
        for from in [QuickState::Satisfied, QuickState::Cancelled] {
            for to in QuickState::ALL {
                assert!(transition(from, to).is_err(), "{} -> {}", from.as_str(), to.as_str());
            }
        }
    }

    #[test]
    fn state_and_reason_words_round_trip_and_unknown_words_are_refused() {
        for s in QuickState::ALL {
            assert_eq!(QuickState::parse(s.as_str()), Some(s));
            // The word serde writes is the word `as_str` answers.
            assert_eq!(serde_json::to_value(s).unwrap(), Value::from(s.as_str()));
        }
        for r in QuickHeld::ALL {
            assert_eq!(QuickHeld::parse(r.as_str()), Some(r));
            assert_eq!(serde_json::to_value(r).unwrap(), Value::from(r.as_str()));
            assert!(!r.notice_line().is_empty());
        }
        for s in QuickSide::ALL {
            assert_eq!(serde_json::to_value(s).unwrap(), Value::from(s.as_str()));
        }
        assert_eq!(QuickState::parse("merging"), None);
        assert_eq!(QuickHeld::parse("because"), None);
    }

    #[test]
    fn exactly_one_side_holds_the_turn_in_each_working_state() {
        use QuickState::*;
        assert_eq!(PlanWait.turn(), Some(QuickSide::Planner));
        assert_eq!(WorkWait.turn(), Some(QuickSide::Worker));
        assert_eq!(FixWait.turn(), Some(QuickSide::Worker));
        assert_eq!(ReviewWait.turn(), Some(QuickSide::Reviewer));
        assert_eq!(RootWait.turn(), Some(QuickSide::Root));
        for s in QuickState::ALL {
            assert_eq!(s.turn().is_some(), s.is_live(), "{}", s.as_str());
        }
        // The four kinds partition the states: each is exactly one of working,
        // parked, idle or terminal, and "unfinished" is the first two.
        for s in QuickState::ALL {
            let kinds = [s.is_live(), s.is_parked(), s.is_idle(), s.is_terminal()];
            assert_eq!(kinds.iter().filter(|k| **k).count(), 1, "{}: {kinds:?}", s.as_str());
        }
        // What the launcher lists, spelled out state by state: the five
        // working ones and the hold — and NOT an idle described run, which has
        // nothing in progress to resume or stop.
        let unfinished: Vec<QuickState> =
            QuickState::ALL.into_iter().filter(|s| s.is_in_progress()).collect();
        assert_eq!(unfinished, vec![PlanWait, WorkWait, ReviewWait, FixWait, RootWait, Held]);
    }

    // ── the happy path ─────────────────────────────────────────────────────

    #[test]
    fn a_full_run_goes_plan_work_review_and_ends_on_approval() {
        let mut r = run(true, true);
        assert_eq!(r.state(), QuickState::PlanWait);
        drive(&mut r, QuickSignal::Done);
        assert_eq!(r.state(), QuickState::WorkWait);
        drive(&mut r, QuickSignal::Done);
        assert_eq!(r.state(), QuickState::ReviewWait);
        drive(&mut r, QuickSignal::Approved);
        assert_eq!(r.state(), QuickState::Satisfied);
        assert_eq!(r.review_rounds, 0, "an approval spends no round");
        assert_eq!(r.reviews_total, 1);
    }

    #[test]
    fn the_plan_step_off_starts_at_the_worker() {
        let r = run(false, true);
        assert_eq!(r.state(), QuickState::WorkWait);
    }

    #[test]
    fn the_review_step_off_ends_on_the_workers_done() {
        let mut r = run(false, false);
        drive(&mut r, QuickSignal::Done);
        assert_eq!(r.state(), QuickState::Satisfied);
        assert_eq!(r.reviews_total, 0);
    }

    // ── the review loop and its bound ──────────────────────────────────────

    #[test]
    fn request_changes_goes_back_to_the_worker_and_spends_a_round() {
        let mut r = run(false, true);
        drive(&mut r, QuickSignal::Done);
        assert_eq!(r.round(), 1);
        let s = drive(&mut r, QuickSignal::RequestChanges).unwrap();
        assert_eq!(r.state(), QuickState::FixWait);
        assert!(s.spends_round && s.reviewed);
        assert_eq!(r.review_rounds, 1);
        assert_eq!(r.round(), 2);
        drive(&mut r, QuickSignal::Done);
        assert_eq!(r.state(), QuickState::ReviewWait);
    }

    #[test]
    fn request_changes_on_the_last_round_parks_on_review_limit() {
        let mut r = run(false, true);
        assert_eq!(r.max_review_rounds, 3);
        drive(&mut r, QuickSignal::Done);
        for round in 1..=2 {
            drive(&mut r, QuickSignal::RequestChanges);
            assert_eq!(r.state(), QuickState::FixWait, "round {round} goes back");
            drive(&mut r, QuickSignal::Done);
        }
        // The third review is the last one the run allows.
        assert_eq!(r.round(), 3);
        drive(&mut r, QuickSignal::RequestChanges);
        assert_eq!(r.state(), QuickState::Held);
        assert_eq!(r.held_reason, Some(QuickHeld::ReviewLimit));
        assert_eq!(r.review_rounds, 2, "the parked round is not spent");
        assert_eq!(r.reviews_total, 3, "and all three reviews were recorded");
    }

    #[test]
    fn a_one_round_run_parks_on_the_first_request_changes() {
        let mut r = QuickDriveRecord::new("t", false, true, "", &QuickLimits::new(1, 240), T0);
        r.brief_pending = false;
        drive(&mut r, QuickSignal::Done);
        drive(&mut r, QuickSignal::RequestChanges);
        assert_eq!(r.held_reason, Some(QuickHeld::ReviewLimit));
    }

    #[test]
    fn a_reviewers_done_is_a_request_for_changes_and_never_an_approval() {
        // At the report boundary …
        assert_eq!(
            QuickSignal::from_report(QuickSide::Reviewer, "done", None),
            QuickSignal::RequestChanges
        );
        assert_eq!(
            QuickSignal::from_report(QuickSide::Reviewer, "done", Some("done")),
            QuickSignal::RequestChanges
        );
        assert_eq!(
            QuickSignal::from_report(QuickSide::Reviewer, "done", Some("approved")),
            QuickSignal::Approved
        );
        // … and in `decide` itself, for a caller that built its facts another
        // way: a bare `Done` in `review-wait` goes back to the worker.
        let mut r = run(false, true);
        drive(&mut r, QuickSignal::Done);
        drive(&mut r, QuickSignal::Done);
        assert_eq!(r.state(), QuickState::FixWait);
        // The other sides' `approved` is just a finished pane.
        assert_eq!(
            QuickSignal::from_report(QuickSide::Worker, "done", Some("approved")),
            QuickSignal::Done
        );
        assert_eq!(QuickSignal::from_report(QuickSide::Worker, "progress", None), QuickSignal::None);
    }

    // ── the holds ──────────────────────────────────────────────────────────

    #[test]
    fn blocked_parks_naming_the_side_that_said_it() {
        let cases = [
            (run(true, true), QuickHeld::PlannerBlocked),
            (run(false, true), QuickHeld::WorkerBlocked),
        ];
        for (mut r, want) in cases {
            drive(&mut r, QuickSignal::Blocked);
            assert_eq!(r.held_reason, Some(want));
        }
        let mut r = run(false, true);
        drive(&mut r, QuickSignal::Done);
        drive(&mut r, QuickSignal::Blocked);
        assert_eq!(r.held_reason, Some(QuickHeld::ReviewerBlocked));
        assert_eq!(r.reviews_total, 0, "a blocked reviewer recorded no review");
    }

    #[test]
    fn a_dead_pane_parks_naming_the_side_that_went() {
        let dead = QuickFacts { pane_alive: false, ..QuickFacts::quiet(T0 + 1) };
        assert_eq!(step(&run(true, true), &dead), Some(QuickStep::held(QuickHeld::PlannerGone)));
        assert_eq!(step(&run(false, true), &dead), Some(QuickStep::held(QuickHeld::WorkerGone)));
        let mut r = run(false, true);
        drive(&mut r, QuickSignal::Done);
        assert_eq!(step(&r, &dead), Some(QuickStep::held(QuickHeld::ReviewerGone)));
        // The control: the same record with a live pane stays put.
        assert_eq!(step(&r, &QuickFacts::quiet(T0 + 1)), None);
    }

    #[test]
    fn a_report_outranks_the_pane_having_closed_after_it() {
        // A planner closes its pane on `done`; the report that arrived first
        // is the fact, and the pane being gone is not a hold.
        let r = run(true, true);
        let f = QuickFacts { signal: QuickSignal::Done, pane_alive: false, ..QuickFacts::quiet(T0) };
        assert_eq!(step(&r, &f), Some(QuickStep::to(QuickState::WorkWait)));
    }

    #[test]
    fn a_message_parks_but_only_after_a_report_has_been_honoured() {
        let r = run(false, true);
        let quiet = QuickFacts { messaged: true, ..QuickFacts::quiet(T0 + 1) };
        assert_eq!(step(&r, &quiet), Some(QuickStep::held(QuickHeld::Messaged)));
        let both = QuickFacts { signal: QuickSignal::Done, ..quiet };
        assert_eq!(step(&r, &both), Some(QuickStep::to(QuickState::ReviewWait)));
        // `blocked` still outranks both.
        let blocked = QuickFacts { signal: QuickSignal::Blocked, ..quiet };
        assert_eq!(step(&r, &blocked), Some(QuickStep::held(QuickHeld::WorkerBlocked)));
    }

    #[test]
    fn a_provider_limit_parks_without_spending_anything() {
        let mut r = run(false, true);
        drive(&mut r, QuickSignal::Done);
        let f = QuickFacts { provider_limited: true, ..QuickFacts::quiet(T0 + 1) };
        let s = step(&r, &f).unwrap();
        assert_eq!(s, QuickStep::held(QuickHeld::ProviderLimit));
        r.take(&s, T0 + 1).unwrap();
        assert_eq!((r.review_rounds, r.reviews_total), (0, 0));
    }

    #[test]
    fn each_state_parks_on_its_own_clock_and_names_it() {
        let at = |r: &QuickDriveRecord, minutes: u64| {
            step(r, &QuickFacts::quiet(r.state_since_ms + minutes * MIN))
        };
        let plan = run(true, true);
        assert_eq!(at(&plan, 59), None);
        assert_eq!(at(&plan, 60), Some(QuickStep::held(QuickHeld::PlanStalled)));

        let mut review = run(false, true);
        drive(&mut review, QuickSignal::Done);
        assert_eq!(at(&review, 59), None);
        assert_eq!(at(&review, 60), Some(QuickStep::held(QuickHeld::LaneStalled)));

        let mut fix = review.clone();
        drive(&mut fix, QuickSignal::RequestChanges);
        assert_eq!(at(&fix, 59), None);
        assert_eq!(at(&fix, 60), Some(QuickStep::held(QuickHeld::FixStalled)));
    }

    #[test]
    fn the_first_pass_is_bounded_by_the_runs_own_time_bound_and_nothing_tighter() {
        let work = run(false, true);
        let at = |minutes: u64| step(&work, &QuickFacts::quiet(T0 + minutes * MIN));
        // Three times the pane timeouts and still working: `work-wait` has no
        // clock of its own.
        assert_eq!(at(180), None);
        assert_eq!(at(239), None);
        assert_eq!(at(240), Some(QuickStep::held(QuickHeld::DriveStalled)));
    }

    #[test]
    fn the_time_bound_is_clamped_to_the_drivers_range_and_defaults_to_240() {
        assert_eq!(QuickLimits::default().drive.drive_timeout_minutes, 240);
        assert_eq!(QuickLimits::new(3, 1).drive.drive_timeout_minutes, 5);
        assert_eq!(QuickLimits::new(3, 9_999).drive.drive_timeout_minutes, 1_440);
        assert_eq!(QuickLimits::new(0, 240).drive.max_review_rounds, 1);
        assert_eq!(QuickLimits::new(9, 240).drive.max_review_rounds, 3);
        // And a bound assigned past its range after construction still cannot
        // reach a decision: `decide` clamps what it reads.
        let work = run(false, true);
        let mut wide = work.limits();
        wide.drive.drive_timeout_minutes = 100_000;
        let late = QuickFacts::quiet(T0 + 1_440 * MIN);
        assert_eq!(decide(&work, &late, &wide), Some(QuickStep::held(QuickHeld::DriveStalled)));
    }

    #[test]
    fn a_brief_still_to_deliver_decides_nothing() {
        let mut r = run(false, true);
        r.brief_pending = true;
        // A dead pane, a stale clock and a `done` — none of it is read until
        // the pane has actually been handed the turn.
        let f = QuickFacts {
            signal: QuickSignal::Done,
            pane_alive: false,
            ..QuickFacts::quiet(T0 + 10_000 * MIN)
        };
        assert_eq!(step(&r, &f), None);
        // The control: the same facts move the run once the brief is out.
        assert!(step(&delivered(r), &f).is_some());
    }

    // ── resume, stop, force ────────────────────────────────────────────────

    #[test]
    fn a_hold_resumes_into_the_state_it_came_from() {
        use QuickState::*;
        for from in [PlanWait, WorkWait, ReviewWait, FixWait, RootWait] {
            let mut r = run(true, true);
            r.state = from;
            r.advance(Held, Some(QuickHeld::Messaged), T0 + 5).unwrap();
            assert_eq!(r.held_from, Some(from));
            assert!(!r.brief_pending);
            assert_eq!(r.resume(T0 + 9), Ok(from));
            assert_eq!(r.state(), from);
            assert_eq!((r.held_reason, r.held_from), (None, None));
            assert!(r.brief_pending, "the resumed turn-holder is owed its brief again");
        }
    }

    #[test]
    fn resuming_a_review_limit_hold_buys_a_fresh_set_of_rounds_at_the_worker() {
        let mut r = run(false, true);
        drive(&mut r, QuickSignal::Done);
        for _ in 0..2 {
            drive(&mut r, QuickSignal::RequestChanges);
            drive(&mut r, QuickSignal::Done);
        }
        drive(&mut r, QuickSignal::RequestChanges);
        assert_eq!(r.held_reason, Some(QuickHeld::ReviewLimit));
        assert_eq!(r.held_from, Some(QuickState::ReviewWait));
        assert_eq!(r.resume(T0 + 9), Ok(QuickState::FixWait));
        assert_eq!(r.review_rounds, 0);
        // The findings files are numbered by `reviews_total`, which a resume
        // must not reset: round 4's file is not round 1's.
        assert_eq!(r.next_findings_round(), 4);
    }

    #[test]
    fn a_resume_restarts_the_overall_clock() {
        let mut r = run(false, true);
        let late = T0 + 240 * MIN;
        let s = step(&r, &QuickFacts::quiet(late)).unwrap();
        assert_eq!(s.held, Some(QuickHeld::DriveStalled));
        r.take(&s, late).unwrap();
        r.resume(late + MIN).unwrap();
        r.brief_pending = false;
        assert_eq!(step(&r, &QuickFacts::quiet(late + 2 * MIN)), None);
        assert_eq!(r.started_ms, T0, "when the run began is not rewritten");
    }

    #[test]
    fn a_hand_edited_held_from_cannot_resume_into_a_finished_state() {
        let mut r = run(false, true);
        r.advance(QuickState::Held, Some(QuickHeld::Messaged), T0).unwrap();
        r.held_from = Some(QuickState::Satisfied);
        assert_eq!(r.resume_target(), QuickState::WorkWait);
        r.held_from = Some(QuickState::Held);
        assert_eq!(r.resume_target(), QuickState::WorkWait);
    }

    #[test]
    fn a_finished_or_parked_run_is_never_advanced_by_a_tick() {
        let loud = QuickFacts {
            signal: QuickSignal::Done,
            pane_alive: false,
            messaged: true,
            provider_limited: true,
            now_ms: T0 + 10_000 * MIN,
        };
        let mut held = run(false, true);
        held.advance(QuickState::Held, Some(QuickHeld::Messaged), T0).unwrap();
        assert_eq!(step(&held, &loud), None);
        let mut done = run(false, false);
        drive(&mut done, QuickSignal::Done);
        assert_eq!(step(&done, &loud), None);
        let mut stopped = run(false, true);
        stopped.advance(QuickState::Cancelled, None, T0).unwrap();
        assert_eq!(step(&stopped, &loud), None);
        // The control: a WORKING record with the same facts does move.
        assert!(step(&run(false, true), &loud).is_some());
    }

    #[test]
    fn a_forced_hand_off_takes_the_arc_and_spends_no_round() {
        let mut r = run(false, true);
        assert_eq!(r.force_handoff(T0 + 1), Ok(QuickState::ReviewWait));
        assert!(r.brief_pending);
        assert_eq!(r.force_handoff(T0 + 2), Ok(QuickState::FixWait));
        assert_eq!((r.review_rounds, r.reviews_total), (0, 0));
        assert_eq!(r.force_handoff(T0 + 3), Ok(QuickState::ReviewWait));
        assert!(r.forced);
    }

    #[test]
    fn a_delivered_brief_records_the_pane_starts_the_clock_and_clears_its_own_markers() {
        let mut r = run(false, true);
        r.advance(QuickState::Held, Some(QuickHeld::WorkerBlocked), T0 + 1).unwrap();
        r.add_note("try the other parser");
        r.resume(T0 + 2).unwrap();
        assert_eq!(r.resumed_from, Some(QuickHeld::WorkerBlocked));
        assert!(r.brief_pending);
        r.brief_delivered(QuickSide::Worker, "w-1", "s-1", T0 + 50);
        assert_eq!(r.worker.agent, "w-1");
        assert_eq!(r.state_since_ms, T0 + 50, "the turn's clock starts when the brief lands");
        assert!(!r.brief_pending && r.resumed_from.is_none() && !r.forced);
        assert!(r.notes.is_empty(), "the brief carried the notes, so they are spent");
        // A later arc does not inherit a marker from the turn before it.
        r.force_handoff(T0 + 60).unwrap();
        r.advance(QuickState::Held, Some(QuickHeld::Messaged), T0 + 61).unwrap();
        assert!(!r.forced);
    }

    #[test]
    fn a_forced_hand_off_is_refused_where_there_is_no_other_side() {
        // A planner has nobody to hand to.
        assert!(run(true, true).force_handoff(T0).is_err());
        // Nor has a worker in a run with no review step.
        assert!(run(false, false).force_handoff(T0).is_err());
        // A parked run is resumed first.
        let mut held = run(false, true);
        held.advance(QuickState::Held, Some(QuickHeld::Messaged), T0).unwrap();
        assert!(held.force_handoff(T0).is_err());
        assert_eq!(held.state(), QuickState::Held);
    }

    // ── ownership ──────────────────────────────────────────────────────────

    #[test]
    fn a_pane_is_owned_by_the_id_minted_for_it_and_an_empty_id_owns_nothing() {
        let mut r = run(false, true);
        // Nothing has been opened: every side's `agent` is empty, and an empty
        // id must not match that.
        assert_eq!(r.owner_of(""), None);
        assert!(!r.holds_turn(""));
        r.worker.record("w-1", "s-1");
        r.reviewer.record("rev-2", "s-2");
        assert_eq!(r.owner_of("w-1"), Some((QuickSide::Worker, true)));
        assert_eq!(r.owner_of("rev-2"), Some((QuickSide::Reviewer, true)));
        assert_eq!(r.owner_of("w-9"), None);
        assert!(r.holds_turn("w-1"));
        assert!(!r.holds_turn("rev-2"), "the reviewer does not hold the turn in work-wait");
    }

    #[test]
    fn a_replaced_pane_is_still_the_runs_but_no_longer_holds_the_turn() {
        let mut r = run(false, true);
        r.worker.record("w-1", "s-1");
        r.worker.record("w-3", "");
        assert_eq!(r.worker.session, "s-1", "an empty session does not erase the recorded one");
        assert_eq!(r.owner_of("w-1"), Some((QuickSide::Worker, false)));
        assert_eq!(r.owner_of("w-3"), Some((QuickSide::Worker, true)));
        assert!(!r.holds_turn("w-1"));
        // Re-recording an earlier pane makes it current again, once.
        r.worker.record("w-1", "s-1");
        assert_eq!(r.owner_of("w-1"), Some((QuickSide::Worker, true)));
        assert_eq!(r.worker.superseded, vec!["w-3".to_string()]);
    }

    #[test]
    fn pending_notes_are_bounded_by_dropping_the_oldest() {
        let mut r = run(false, true);
        for i in 0..(QUICK_MAX_PENDING_NOTES + 3) {
            r.add_note(&format!("note {i}"));
        }
        assert_eq!(r.notes.len(), QUICK_MAX_PENDING_NOTES);
        assert_eq!(r.notes[0], "note 3");
    }

    // ── the file ───────────────────────────────────────────────────────────

    #[test]
    fn the_findings_file_is_named_by_a_number_and_nothing_else() {
        assert_eq!(findings_file_name(1), "round-1.md");
        assert_eq!(findings_file_name(12), "round-12.md");
    }

    #[test]
    fn the_store_round_trips_and_keeps_a_newer_builds_fields() {
        let mut r = run(true, true);
        r.worker.record("w-1", "s-1");
        r.add_note("be careful with the parser");
        let file = QuickDriveFile { run: Some(r), ..QuickDriveFile::default() };
        let mut v = serde_json::to_value(&file).unwrap();
        // A newer build wrote one field at each of the three levels.
        v["future_top"] = Value::from(1);
        v["run"]["future_run"] = Value::from("x");
        v["run"]["worker"]["future_pane"] = Value::from(true);
        let parsed = parse_state(&v.to_string()).unwrap();
        assert_eq!(parsed.run.as_ref().unwrap().worker.agent, "w-1");
        let again = serde_json::to_value(&parsed).unwrap();
        assert_eq!(again["future_top"], Value::from(1));
        assert_eq!(again["run"]["future_run"], Value::from("x"));
        assert_eq!(again["run"]["worker"]["future_pane"], Value::from(true));
        assert_eq!(again, v);
    }

    #[test]
    fn the_store_refuses_a_state_or_reason_word_it_does_not_know() {
        let file = QuickDriveFile { run: Some(run(false, true)), ..QuickDriveFile::default() };
        let good = serde_json::to_value(&file).unwrap();
        // The control: the unedited document parses.
        assert!(parse_state(&good.to_string()).is_ok());

        let mut bad_state = good.clone();
        bad_state["run"]["state"] = Value::from("merging");
        assert!(matches!(
            parse_state(&bad_state.to_string()),
            Err(QuickStateError::Malformed(_))
        ));

        let mut bad_reason = good.clone();
        bad_reason["run"]["held_reason"] = Value::from("because");
        assert!(matches!(
            parse_state(&bad_reason.to_string()),
            Err(QuickStateError::Malformed(_))
        ));
    }

    #[test]
    fn the_store_refuses_a_missing_version_a_newer_schema_and_a_missing_counter() {
        assert!(matches!(parse_state("{}"), Err(QuickStateError::Malformed(_))));
        assert_eq!(
            parse_state(r#"{"version": 2}"#),
            Err(QuickStateError::Unsupported(2))
        );
        // A run with no `review_rounds` is refused rather than read as zero: a
        // defaulted counter would hand back a whole fresh budget.
        let file = QuickDriveFile { run: Some(run(false, true)), ..QuickDriveFile::default() };
        let mut v = serde_json::to_value(&file).unwrap();
        v["run"].as_object_mut().unwrap().remove("review_rounds");
        assert!(matches!(parse_state(&v.to_string()), Err(QuickStateError::Malformed(_))));
        // An absent `run` is simply no run.
        let empty = parse_state(r#"{"version": 1}"#).unwrap();
        assert!(empty.run.is_none() && !empty.has_unfinished_run());
    }

    #[test]
    fn an_absent_file_is_no_run_and_a_written_one_reads_back() {
        let dir = std::env::temp_dir().join(format!(
            "loomux-quickdrive-test-{}-{}",
            std::process::id(),
            T0
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load_state(&dir), Ok(QuickDriveFile::default()));
        let file = QuickDriveFile { run: Some(run(true, true)), ..QuickDriveFile::default() };
        store_state(&dir, &file).unwrap();
        let back = load_state(&dir).unwrap();
        assert_eq!(back, file);
        assert!(back.has_unfinished_run());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── a described run (way 2) ────────────────────────────────────────────

    /// A described run whose root pane has opened — where every test below
    /// starts, except the one about the pane still to open.
    fn idle_root() -> QuickDriveRecord {
        let mut rec = QuickDriveRecord::new_described("main", &QuickLimits::new(3, 5), T0);
        rec.root_opened("quick-1", "s-root", T0);
        rec
    }

    /// **A described run starts IDLE, with no task and its root still to
    /// open** (#3723). Nobody holds the turn, and no plan or review step is
    /// recorded — those are the root's decisions, not the record's.
    #[test]
    fn a_described_run_starts_idle_with_no_task_and_its_root_still_to_open() {
        let rec = QuickDriveRecord::new_described("main", &QuickLimits::default(), T0);
        assert_eq!(rec.state(), QuickState::RootIdle);
        assert!(rec.described);
        assert_eq!(rec.task, "", "the task is given in the pane; the record never holds one");
        assert!(rec.brief_pending, "the root's pane is still to open");
        assert_eq!(rec.state().turn(), None, "nobody holds the turn while idle");
        assert_eq!(rec.task_seq, 0);
        assert!(!rec.plan_step && !rec.review_step);

        let mut opened = rec.clone();
        opened.root_opened("quick-1", "s-root", T0 + 9);
        assert!(!opened.brief_pending, "the pane opened, and nothing else was owed");
        assert_eq!((opened.root.agent.as_str(), opened.root.session.as_str()), ("quick-1", "s-root"));
        assert_eq!(opened.state(), QuickState::RootIdle, "opening the pane begins no task");

        // The control: a steps run is not one, and starts working.
        let steps = QuickDriveRecord::new("add a flag", false, true, "main", &QuickLimits::default(), T0);
        assert!(!steps.described);
        assert_eq!(steps.state(), QuickState::WorkWait);
    }

    /// **An idle root is read against no clock** (#3723): however long it sits
    /// there, and whatever else is true of it, a tick decides nothing. The
    /// bound is five minutes; the control is the same record with a task
    /// begun, which the same clock parks.
    #[test]
    fn an_idle_described_run_is_never_held_however_long_it_waits() {
        let rec = idle_root();
        for minutes in [0, 4, 5, 6, 60, 10_000] {
            let now = T0 + minutes * MIN;
            assert_eq!(step(&rec, &QuickFacts::quiet(now)), None, "{minutes} min idle");
        }
        // Not a dead pane, not a provider limit, not a message, not a report.
        let loud = QuickFacts {
            signal: QuickSignal::Done,
            pane_alive: false,
            messaged: true,
            provider_limited: true,
            now_ms: T0 + 10_000 * MIN,
        };
        assert_eq!(step(&rec, &loud), None);

        // The control: begin a task at T0 and the same six minutes park it.
        let mut working = idle_root();
        working.begin_task(T0).unwrap();
        assert_eq!(
            step(&working, &QuickFacts::quiet(T0 + 6 * MIN)),
            Some(QuickStep::held(QuickHeld::DriveStalled))
        );
    }

    /// **The clock starts when the task begins**, not when the pane opened.
    /// A root that sat idle for ten hours and then began a task has the whole
    /// of its five minutes.
    #[test]
    fn a_tasks_clock_starts_when_the_task_begins_and_not_when_the_pane_opened() {
        let mut rec = idle_root();
        let began = T0 + 600 * MIN;
        assert_eq!(rec.begin_task(began), Ok(1));
        assert_eq!(rec.state(), QuickState::RootWait);
        assert_eq!(rec.state().turn(), Some(QuickSide::Root));
        assert!(!rec.brief_pending, "the root took the turn itself; no brief is owed");
        assert_eq!((rec.started_ms, rec.clock_ms), (began, began));
        assert_eq!(rec.age_ms(began + 4 * MIN), 4 * MIN, "measured from the task's start");

        assert_eq!(step(&rec, &QuickFacts::quiet(began + 4 * MIN)), None, "inside the bound");
        assert_eq!(
            step(&rec, &QuickFacts::quiet(began + 5 * MIN)),
            Some(QuickStep::held(QuickHeld::DriveStalled)),
            "at the bound, counted from the task's start"
        );
    }

    /// **A task begins from idle and from nowhere else.** A held run in
    /// particular does not begin one because its root acted: only the human's
    /// Resume moves a hold.
    #[test]
    fn a_task_begins_only_from_idle() {
        let mut working = idle_root();
        working.begin_task(T0).unwrap();
        assert!(working.begin_task(T0 + 1).is_err(), "a task is already in progress");
        assert_eq!(working.task_seq, 1, "and the refusal counted nothing");

        let mut held = working.clone();
        held.take(&QuickStep::held(QuickHeld::DriveStalled), T0 + 2).unwrap();
        assert!(held.begin_task(T0 + 3).is_err());
        assert_eq!(held.state(), QuickState::Held);

        let mut stopped = idle_root();
        stopped.advance(QuickState::Cancelled, None, T0 + 1).unwrap();
        assert!(stopped.begin_task(T0 + 2).is_err());

        let mut steps = run(false, true);
        assert!(steps.begin_task(T0).is_err(), "a steps run has no tasks to begin");
        assert_eq!(steps.state(), QuickState::WorkWait);
    }

    /// **The root's `done` ends a TASK and the run goes back to idle**, where
    /// the next task begins with a clock of its own. `blocked` parks it on the
    /// root's reason, and a dead root parks it too.
    #[test]
    fn the_roots_done_ends_a_task_and_a_second_task_gets_a_clock_of_its_own() {
        let mut rec = idle_root();
        rec.begin_task(T0).unwrap();
        assert!(step(&rec, &said(QuickSignal::None)).is_none(), "a quiet root moves nothing");

        let done = step(&rec, &said(QuickSignal::Done)).expect("done decides");
        assert_eq!((done.to, done.held), (QuickState::RootIdle, None));
        assert!(!done.spends_round && !done.reviewed, "no review round is the record's to count");
        rec.take(&done, T0 + 4 * MIN).unwrap();
        rec.worker_note = "work is on agent/w".into();
        rec.pr = Some(77);
        rec.notice_item = "ny-1".into();
        assert_eq!(rec.state(), QuickState::RootIdle);
        assert!(!rec.brief_pending, "the root is sitting there; nothing is owed");
        assert!(rec.described);

        // Idle again: the first task's clock no longer counts, however old.
        assert_eq!(step(&rec, &QuickFacts::quiet(T0 + 900 * MIN)), None);

        // The second task: numbered, its own clock, the first one's account gone.
        let second = T0 + 900 * MIN;
        assert_eq!(rec.begin_task(second), Ok(2));
        assert_eq!((rec.worker_note.as_str(), rec.pr, rec.notice_item.as_str()), ("", None, ""));
        assert_eq!(step(&rec, &QuickFacts::quiet(second + 4 * MIN)), None);
        assert_eq!(
            step(&rec, &QuickFacts::quiet(second + 5 * MIN)),
            Some(QuickStep::held(QuickHeld::DriveStalled))
        );

        // The other two things a working root can do.
        let blocked = step(&rec, &said(QuickSignal::Blocked)).expect("blocked decides");
        assert_eq!((blocked.to, blocked.held), (QuickState::Held, Some(QuickHeld::RootBlocked)));
        let dead = QuickFacts { pane_alive: false, ..said(QuickSignal::None) };
        assert_eq!(step(&rec, &dead).expect("a dead root decides").held, Some(QuickHeld::RootGone));

        // A root is not a reviewer: its report is read as done or blocked, and
        // the outcome word cannot make it anything else.
        assert_eq!(QuickSignal::from_report(QuickSide::Root, "done", Some("approved")), QuickSignal::Done);
        assert_eq!(QuickSignal::from_report(QuickSide::Root, "done", Some("request_changes")), QuickSignal::Done);
        assert_eq!(QuickSignal::from_report(QuickSide::Root, "blocked", None), QuickSignal::Blocked);
    }

    /// **A parked described run resumes to `root-wait`**, owed its message,
    /// and the marker survives the hold — which is the reason it is a field.
    #[test]
    fn a_described_run_keeps_its_marker_through_a_hold_and_resumes_to_the_root() {
        let mut rec = idle_root();
        rec.begin_task(T0).unwrap();
        rec.take(&QuickStep::held(QuickHeld::Restart), T0 + 1).expect("a working run can park");
        assert_eq!(rec.state(), QuickState::Held);
        assert!(rec.described, "still a described run while parked");
        assert_eq!(rec.resume_target(), QuickState::RootWait);
        rec.resume(T0 + 2).expect("a held run resumes");
        assert_eq!(rec.state(), QuickState::RootWait);
        assert!(rec.brief_pending, "the root is owed its message again");
        assert_eq!(rec.task_seq, 1, "a resume continues the task; it does not begin one");

        // And the record written before the field existed reads as a steps run.
        let mut v = serde_json::to_value(&rec).unwrap();
        v.as_object_mut().unwrap().remove("described");
        let old: QuickDriveRecord = serde_json::from_value(v).expect("the field is optional on read");
        assert!(!old.described);
    }

    /// **A root pane that could not be opened parks the idle run, and Resume
    /// asks for the pane again.** That hold is the only one `root-idle` has,
    /// and it resumes to idle — owed its pane, not a brief.
    #[test]
    fn a_hold_out_of_idle_resumes_to_idle_still_owed_its_pane() {
        let mut rec = QuickDriveRecord::new_described("main", &QuickLimits::default(), T0);
        rec.advance(QuickState::Held, Some(QuickHeld::Unresumable), T0 + 1).unwrap();
        assert_eq!(rec.held_from, Some(QuickState::RootIdle));
        assert!(!rec.brief_pending);
        assert_eq!(rec.resume(T0 + 2), Ok(QuickState::RootIdle));
        assert!(rec.brief_pending, "the pane is owed again");
        assert_eq!(rec.task_seq, 0, "no task began");

        // The control: a task that FINISHED goes to idle owing nothing.
        let mut done = idle_root();
        done.begin_task(T0).unwrap();
        done.advance(QuickState::RootIdle, None, T0 + 1).unwrap();
        assert!(!done.brief_pending);
    }

    /// **A record written by the build before #3723 still loads, and is a
    /// task in progress.** That build started a described run in `root-wait`
    /// with the task on the record and no `task_seq`. The text is spelled out
    /// here rather than produced by this build's serializer, so the test reads
    /// the shape that is really on disk.
    #[test]
    fn a_described_record_from_before_the_idle_start_still_loads_as_a_task_in_progress() {
        let beta8 = r#"{
          "version": 1,
          "run": {
            "task": "add a --json flag to the list command",
            "state": "root-wait",
            "held_reason": null,
            "held_from": null,
            "plan_step": false,
            "review_step": false,
            "described": true,
            "root_cli": "claude",
            "root_model": "",
            "base": "main",
            "max_review_rounds": 3,
            "drive_timeout_minutes": 240,
            "started_ms": 1000000,
            "state_since_ms": 1000001,
            "clock_ms": 1000000,
            "review_rounds": 0,
            "reviews_total": 0,
            "planner": { "agent": "", "session": "", "superseded": [] },
            "worker": { "agent": "", "session": "", "superseded": [] },
            "reviewer": { "agent": "", "session": "", "superseded": [] },
            "root": { "agent": "quick-1", "session": "s-root", "superseded": [] },
            "worker_cwd": "",
            "worker_branch": "",
            "pr": null,
            "worker_note": "",
            "review_note": "",
            "held_note": "",
            "notes": [],
            "progress_answered": false,
            "brief_pending": false,
            "resumed_from": null,
            "forced": false,
            "notice_item": ""
          }
        }"#;
        let file = parse_state(beta8).expect("a beta8 record parses");
        let mut rec = file.run.clone().expect("and holds its run");
        assert_eq!(rec.state(), QuickState::RootWait, "a task in progress");
        assert!(rec.described);
        assert_eq!(rec.task, "add a --json flag to the list command", "its task text is kept");
        assert_eq!(rec.task_seq, 0, "the field it never wrote defaults");
        assert!(file.has_unfinished_run() && rec.state().is_in_progress());
        assert!(rec.holds_turn("quick-1"));

        // It is bounded by its own clock, parks, resumes and finishes like any
        // task this build began — and finishing it leaves the run idle.
        assert_eq!(
            step(&rec, &QuickFacts::quiet(T0 + 240 * MIN)),
            Some(QuickStep::held(QuickHeld::DriveStalled))
        );
        rec.take(&QuickStep::held(QuickHeld::Restart), T0 + 2).unwrap();
        assert_eq!(rec.resume(T0 + 3), Ok(QuickState::RootWait));
        rec.brief_pending = false;
        let done = step(&rec, &said(QuickSignal::Done)).unwrap();
        rec.take(&done, T0 + 4).unwrap();
        assert_eq!(rec.state(), QuickState::RootIdle);
        assert_eq!(rec.begin_task(T0 + 5), Ok(1), "and the next task is this build's first");
        assert_eq!(rec.task, "", "which the old task's text does not describe, so it goes");

        // A described run that had ENDED under that build is still ended.
        let ended = beta8.replace("\"root-wait\"", "\"satisfied\"");
        let ended = parse_state(&ended).unwrap();
        assert!(ended.run.as_ref().unwrap().state().is_terminal() && !ended.has_unfinished_run());
    }

    /// **An idle record round-trips**, and its state word is the one a build
    /// before #3723 does not know — which is what "an older build cannot read
    /// a new idle record" rests on, so it is pinned rather than assumed.
    #[test]
    fn an_idle_record_round_trips_under_a_state_word_older_builds_do_not_know() {
        let file = QuickDriveFile { run: Some(idle_root()), ..QuickDriveFile::default() };
        let v = serde_json::to_value(&file).unwrap();
        assert_eq!(v["run"]["state"], Value::from("root-idle"));
        assert_eq!(v["run"]["task"], Value::from(""));
        assert_eq!(v["run"]["task_seq"], Value::from(0));
        let back = parse_state(&v.to_string()).unwrap();
        assert_eq!(back, file);
        assert!(back.has_unfinished_run(), "it has not ended: its group is still held");
        assert!(!back.run.as_ref().unwrap().state().is_in_progress(), "but nothing is in progress");
    }
}
