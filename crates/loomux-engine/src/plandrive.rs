//! The plan driver's pure core (#3040) — the state machine, the persisted
//! shape, and the per-tick decision. `crates/loomux-engine/src/reviewdrive.rs`
//! is the twin this file is deliberately shaped after, and the registry-side
//! wiring is `src-tauri/src/orchestration/pdtick.rs`, `rdtick.rs`'s twin.
//!
//! Design note: `doc/design/plan-driver.md`. The plan document itself — the
//! fenced ```orrerix-plan block a planner posts — is [`crate::plandoc`]'s, and
//! nothing here re-parses it: a [`PdEntry`] stores the *parsed*
//! [`PlanDoc`](crate::plandoc::PlanDoc) that the hook validated at post time.
//!
//! # What this file may and may not do
//!
//! Nothing here touches a file, a child process, a clock or a registry. Every
//! function takes what it needs as a value and answers a value, so the whole
//! machine is exercisable from a unit test in a Tauri-free crate — which is
//! what makes [`decide`]'s arcs pinnable at all. The wiring reads GitHub,
//! spawns panes, writes the record and delivers notices; it makes no state
//! decision.
//!
//! # The two halves, and where the second one's work happens
//!
//! The drive runs in two halves. The first reaches a PLAN: refuse, spawn a
//! planner, and store the block the planner posts — at which point an
//! `agent-investigation` issue is finished by design, because the plan is what
//! it wanted. The second EXECUTES that plan for an `agent-ready` issue: board
//! rows, spawn a worker per ready slice, hand each PR to the review driver, and
//! mark a row `done` on a positively MERGED PR.
//!
//! **Only the first half's decisions are all here.** A board write, a spawn and
//! a `gh pr view` are things only the registry can do, so the executor's arcs
//! ([`PlanDriveState::Boarding`], [`PlanDriveState::Running`]) are decided here
//! off FACTS the wiring reads — `boarded`, `row_removed`, `slices_settled`,
//! `running_idle` — and performed there. Per-SLICE state ([`PdSlice`]) is a
//! record of what happened rather than a second machine: it has no `decide`,
//! because every one of its transitions is caused by something the wiring
//! observed.
//!
//! An older build reading a record this one wrote refuses it loudly through
//! [`parse_state`] rather than acting on a state word it cannot read.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::plandoc::{self, PlanDoc};
use crate::reviewdrive::OwedNotice;

// ── §2(b) the states ────────────────────────────────────────────────────────

/// Where one plan drive stands.
///
/// **One parked state carrying a closed reason**, [`PdHeldReason`], for
/// [`crate::reviewdrive::DriveState`]'s reason: a reader asking "is this drive
/// parked" asks one question, and the reason travels in the notice and the
/// audit line instead of being inferred from which field happens to be set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanDriveState {
    /// A planner pane is open; waiting for it to post a plan block.
    Planning,
    /// A valid plan block has been stored, with the comment it was posted as.
    PlanPosted,
    /// The declared review window (§2(c)): the plan is posted, ONE notice has
    /// been sent, and the drive is waiting out `plan_review_minutes` so the
    /// orchestrator can `cancel_plan_drive` before a single worker is spawned.
    /// Skipped entirely when the window is zero, which is the default — a
    /// window nobody is told about is unused, and the notice is the price.
    PlanReview,
    /// The plan is being turned into board rows: one parent row for the issue
    /// and one child row per slice, written through `upsert_task` as an agent
    /// so `find_dep_cycle` and the WIP caps run exactly as they do for the
    /// orchestrator's own boarding.
    Boarding,
    /// The rows are on the board and the drive is executing them: claim, spawn,
    /// hand the PR to the review driver, mark `done` on a positively MERGED
    /// PR, and let the dependents become ready. **At most one spawn per group
    /// per tick**, after the review driver has spent its own.
    Running,
    /// Terminal: the drive did everything it was going to do — every slice row
    /// settled, or an `agent-investigation` issue whose plan is posted.
    Complete,
    /// Terminal: cancelled by tool, or reconcile positively established the
    /// issue is closed.
    Cancelled,
    /// **Parked**, carrying a [`PdHeldReason`]. The tick does not advance it;
    /// `resume_plan_drive` and `cancel_plan_drive` do.
    Held,
}

impl PlanDriveState {
    /// Every state, so a test can walk the machine without matching on the
    /// enum — which is what lets it fail when a seventh is added rather than
    /// silently keep checking six.
    pub const ALL: [PlanDriveState; 8] = [
        PlanDriveState::Planning,
        PlanDriveState::PlanPosted,
        PlanDriveState::PlanReview,
        PlanDriveState::Boarding,
        PlanDriveState::Running,
        PlanDriveState::Complete,
        PlanDriveState::Cancelled,
        PlanDriveState::Held,
    ];

    /// The wire/audit spelling — the same string serde writes.
    pub fn as_str(self) -> &'static str {
        match self {
            PlanDriveState::Planning => "planning",
            PlanDriveState::PlanPosted => "plan-posted",
            PlanDriveState::PlanReview => "plan-review",
            PlanDriveState::Boarding => "boarding",
            PlanDriveState::Running => "running",
            PlanDriveState::Complete => "complete",
            PlanDriveState::Cancelled => "cancelled",
            PlanDriveState::Held => "held",
        }
    }

    /// Parse a state word. `None` for anything unrecognized — never coerced,
    /// the same "reject, never guess" posture [`PdHeldReason::parse`] takes.
    pub fn parse(s: &str) -> Option<PlanDriveState> {
        PlanDriveState::ALL.into_iter().find(|st| st.as_str() == s.trim())
    }

    /// `complete` / `cancelled`, and only those two. A terminal state has no
    /// outgoing transition at all.
    pub fn is_terminal(self) -> bool {
        matches!(self, PlanDriveState::Complete | PlanDriveState::Cancelled)
    }

    /// Parked. The tick leaves it alone; the two tools move it.
    pub fn is_parked(self) -> bool {
        matches!(self, PlanDriveState::Held)
    }

    /// The working states — the scope of the `already-driven` refusal, and the
    /// scope of the planner interception [`PdEntry::driven_role`] keys. A
    /// `held` entry is parked: its planner's traffic goes to the orchestrator
    /// exactly as it always did, which is what makes a hold a hand-back to a
    /// human rather than a quieter kind of drive.
    pub fn is_live(self) -> bool {
        !self.is_terminal() && !self.is_parked()
    }
}

/// Why a plan drive is parked (§2(e)).
///
/// Closed, for [`PlanDriveState`]'s reason: a hold whose reason this build
/// cannot read is a hold it cannot explain, and the notice is the entire
/// product of a hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PdHeldReason {
    /// The planner posted [`MAX_INVALID_POSTS`] blocks this build refused. The
    /// refusals themselves cost the planner one tool call each and no
    /// orchestrator turn at all; this is the bound on that.
    PlanInvalid,
    /// The planner reported `done`, or its pane went, without ever posting a
    /// plan.
    PlanMissing,
    /// The planner neither posted nor reported, for
    /// `driver.planner_timeout_minutes`.
    PlannerStalled,
    /// The planner reported `blocked`.
    PlannerBlocked,
    /// The issue's `agent-ready`/`agent-investigation` label was withdrawn
    /// while the drive was live (§2(d)). Consent is re-read, not remembered.
    ConsentWithdrawn,
    /// A slice's board row was **struck by hand** while the drive was running
    /// (§2(c)). Its dependents now carry a dangling dep, which is unmet
    /// forever, so the drive parks naming the row rather than waiting out a
    /// timeout it can already see coming.
    RowRemoved,
    /// **Nothing this build produces.** Kept because a `plan_drives.json`
    /// written by the build that shipped #3040 P3a can carry it, and a hold
    /// whose reason this build could not read would be a hold it could not
    /// explain. A drive resumed off such a record re-enters `boarding` and
    /// this build boards it — which is exactly what the reason said was
    /// missing.
    AwaitingP3b,
    /// The whole drive outran `driver.drive_timeout_minutes` (the review
    /// driver's own knob, reused) without reaching a terminal state.
    DriveStalled,
}

impl PdHeldReason {
    /// Every reason, so the notice table and the audit vocabulary can be
    /// checked against the enum rather than against a list someone has to
    /// remember to extend.
    pub const ALL: [PdHeldReason; 8] = [
        PdHeldReason::PlanInvalid,
        PdHeldReason::PlanMissing,
        PdHeldReason::PlannerStalled,
        PdHeldReason::PlannerBlocked,
        PdHeldReason::ConsentWithdrawn,
        PdHeldReason::RowRemoved,
        PdHeldReason::AwaitingP3b,
        PdHeldReason::DriveStalled,
    ];

    /// The wire/audit spelling — the same string serde writes.
    pub fn as_str(self) -> &'static str {
        match self {
            PdHeldReason::PlanInvalid => "plan-invalid",
            PdHeldReason::PlanMissing => "plan-missing",
            PdHeldReason::PlannerStalled => "planner-stalled",
            PdHeldReason::PlannerBlocked => "planner-blocked",
            PdHeldReason::ConsentWithdrawn => "consent-withdrawn",
            PdHeldReason::RowRemoved => "row-removed",
            PdHeldReason::AwaitingP3b => "awaiting-p3b",
            PdHeldReason::DriveStalled => "drive-stalled",
        }
    }

    /// Parse a reason word. `None` for anything unrecognized.
    pub fn parse(s: &str) -> Option<PdHeldReason> {
        PdHeldReason::ALL.into_iter().find(|r| r.as_str() == s.trim())
    }

    /// The one sentence this hold's notice leads with. Held here rather than in
    /// the wiring so that a reason cannot be added without one, and so the
    /// notice text is checkable from a Tauri-free test.
    pub fn notice_line(self) -> &'static str {
        match self {
            PdHeldReason::PlanInvalid => {
                "the planner's plan block was refused three times — the last reasons are on the \
                 drive record, and the plan has to be written by hand or the planner re-briefed"
            }
            PdHeldReason::PlanMissing => {
                "the planner finished without posting a plan block, so there is nothing to board"
            }
            PdHeldReason::PlannerStalled => {
                "the planner has neither posted a plan nor reported inside its timeout"
            }
            PdHeldReason::PlannerBlocked => "the planner reported blocked",
            PdHeldReason::ConsentWithdrawn => {
                "the issue no longer carries agent-ready or agent-investigation, so the drive \
                 stopped — consent is the label, and it is re-read rather than remembered"
            }
            PdHeldReason::RowRemoved => {
                "a slice's board row was struck, so its dependents can never become ready — re-dep \
                 them by hand, or resume the drive once the board says what you meant"
            }
            PdHeldReason::AwaitingP3b => {
                "the plan is posted and validated; boarding it and spawning its slices was not in \
                 the build that parked it — resume the drive and this build boards it"
            }
            PdHeldReason::DriveStalled => {
                "the drive outran driver.drive_timeout_minutes without finishing"
            }
        }
    }
}

/// How long a slice whose worker reported `done` waits for a PR to appear on
/// its branch before parking on [`PdSliceHold::PrMissing`].
///
/// **Its own constant, not [`crate::reviewdrive::CAP_HOLD_MS`] reused**, though
/// the two are the same figure today. That one bounds a delegate cap — a
/// condition orrerix cannot influence and can only wait out; this bounds a
/// worker's own claim to have finished, which is a different thing to be
/// patient about. One name per meaning, so a later change to either does not
/// silently move the other.
///
/// Fifteen minutes is generous for what it measures: a worker reports `done`
/// after pushing, so the PR is normally visible on the very next tick, and the
/// retry that covers the ordinary lag is the same one this bounds.
pub const PR_WAIT_HOLD_MS: u64 = 15 * 60_000;

/// How many refused plan blocks one drive tolerates before parking (§2(e)).
///
/// The planner is still inside its own turn when a post is refused, so a fix
/// costs it one tool call and the orchestrator nothing; three is the point past
/// which the planner is not going to converge on its own.
pub const MAX_INVALID_POSTS: u32 = 3;

/// A transition this machine does not have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PdInvalidTransition {
    pub from: PlanDriveState,
    pub to: PlanDriveState,
}

impl fmt::Display for PdInvalidTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "plan drive: no transition {} -> {}", self.from.as_str(), self.to.as_str())
    }
}

impl std::error::Error for PdInvalidTransition {}

/// The whole arc table (§2(b)), listed rather than inferred.
///
/// **A resume returns to the state the hold came FROM**, which is why `Held`
/// has three outgoing working arcs rather than one: a drive parked on
/// `planner-stalled` and a drive parked on `awaiting-p3b` resume into different
/// work, and a single `Held -> Planning` arc would silently re-open a planner
/// for a plan that is already posted.
pub fn transition(
    from: PlanDriveState,
    to: PlanDriveState,
) -> Result<PlanDriveState, PdInvalidTransition> {
    use PlanDriveState::*;
    let ok = match (from, to) {
        // 1. The hook stored a valid plan (or a restart re-derived one).
        (Planning, PlanPosted) => true,
        // 2. `agent-investigation`: the plan IS the deliverable (§2(d)).
        (PlanPosted, Complete) => true,
        // 3. `agent-ready` with a declared window: ONE notice, then wait.
        (PlanPosted, PlanReview) => true,
        // 4. `agent-ready`: the plan becomes board rows — straight from
        //    `plan-posted` when no window is declared (the default), or out of
        //    the window once it has run.
        (PlanPosted, Boarding) | (PlanReview, Boarding) => true,
        // 5. The rows are written; the drive executes them.
        (Boarding, Running) => true,
        // 6. Every slice row settled.
        (Running, Complete) => true,
        // 7. Every hold, from every live state.
        (Planning | PlanPosted | PlanReview | Boarding | Running, Held) => true,
        // 8. `resume_plan_drive`, back into the state the hold came from.
        (Held, Planning)
        | (Held, PlanPosted)
        | (Held, PlanReview)
        | (Held, Boarding)
        | (Held, Running) => true,
        // 9. `cancel_plan_drive`, or reconcile positively established the issue
        //    is closed. From any non-terminal, `held` included: cancelling is a
        //    parked drive's second way out.
        (Planning | PlanPosted | PlanReview | Boarding | Running | Held, Cancelled) => true,
        _ => false,
    };
    if ok {
        Ok(to)
    } else {
        Err(PdInvalidTransition { from, to })
    }
}

// ── §2(d) consent ───────────────────────────────────────────────────────────

/// The label this issue carries, which is the whole of the human's consent to a
/// drive (§2(d)).
///
/// Read at `drive_plan` **and re-read** before the drive acts again — never
/// remembered — so withdrawing the label stops the drive. There is no
/// `Withdrawn` variant: the absence of a label is expressed by the *absence of
/// a value*, so a caller cannot forget to handle it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Consent {
    /// `agent-ready` — build it.
    Ready,
    /// `agent-investigation` — research only. The drive posts the plan and
    /// completes; it never boards and never spawns a worker.
    Investigation,
}

impl Consent {
    /// The label word, which is also the wire/audit spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Consent::Ready => "agent-ready",
            Consent::Investigation => "agent-investigation",
        }
    }

    /// Which of the two labels a set of issue labels grants, if either.
    ///
    /// **`Ready` wins a tie**, and the tie is real: an issue can carry both.
    /// The wider grant is the one the human pressed go on, and reading it as
    /// `Investigation` would silently refuse to build something that was
    /// labelled to be built.
    pub fn from_labels<S: AsRef<str>>(labels: &[S]) -> Option<Consent> {
        let has = |want: Consent| labels.iter().any(|l| l.as_ref().trim() == want.as_str());
        if has(Consent::Ready) {
            Some(Consent::Ready)
        } else if has(Consent::Investigation) {
            Some(Consent::Investigation)
        } else {
            None
        }
    }
}

// ── §2(c)/(e) the bounds ────────────────────────────────────────────────────

/// The closed range for `driver.plan_review_minutes` (§2(c)). `0` is the
/// default and means "no window": the human already pressed go with the label,
/// and a window nobody is told about is unused.
pub const PLAN_REVIEW_MINUTES_MIN: u32 = 0;
/// Two hours. Past that a drive is not waiting on a review, it is forgotten.
pub const PLAN_REVIEW_MINUTES_MAX: u32 = 120;
/// No window, which is §2(c)'s argued default.
pub const PLAN_REVIEW_MINUTES_DEFAULT: u32 = 0;

/// The closed range for `driver.planner_timeout_minutes` (§2(e)). The floor is
/// a quarter of an hour because a planner reading a large issue legitimately
/// spends that before its first tool call.
pub const PLANNER_TIMEOUT_MINUTES_MIN: u32 = 15;
/// Three hours.
pub const PLANNER_TIMEOUT_MINUTES_MAX: u32 = 180;
/// An hour.
pub const PLANNER_TIMEOUT_MINUTES_DEFAULT: u32 = 60;

/// The bounds one plan drive runs against.
///
/// A plain value type, constructed by [`PdLimits::new`], which **clamps**. The
/// workflow parser already refuses an out-of-range value as it reads the file;
/// this clamps again on the values actually used, and the second is not
/// redundant for [`crate::reviewdrive::DriveLimits`]'s reason: [`decide`] is a
/// `pub fn` over a plain value type that any caller in any crate can reach
/// without passing through that parser, and a boundary that holds only when the
/// expected caller is upstream is not a boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PdLimits {
    /// `driver.planner_timeout_minutes`.
    pub planner_timeout_minutes: u32,
    /// `driver.plan_review_minutes` — the DEFAULT for a `drive_plan` call that
    /// names no window of its own. Resolved once, at `drive_plan`, into
    /// [`PdEntry::review_minutes`], so a tick reads ONE number rather than
    /// reconciling two; `decide` therefore never reads this field, and that is
    /// why a repo key and a per-call argument cannot disagree at run time.
    pub plan_review_minutes: u32,
    /// `driver.drive_timeout_minutes` — the review driver's own knob, reused
    /// rather than duplicated (§3): it bounds the same quantity, a whole
    /// drive's age.
    pub drive_timeout_minutes: u32,
}

impl Default for PdLimits {
    fn default() -> Self {
        PdLimits {
            planner_timeout_minutes: PLANNER_TIMEOUT_MINUTES_DEFAULT,
            plan_review_minutes: PLAN_REVIEW_MINUTES_DEFAULT,
            drive_timeout_minutes: crate::workflow::DRIVER_DRIVE_TIMEOUT_DEFAULT_MIN,
        }
    }
}

impl PdLimits {
    /// Build the limits from raw minute figures, clamping each into its own
    /// closed range.
    pub fn new(
        planner_timeout_minutes: u32,
        plan_review_minutes: u32,
        drive_timeout_minutes: u32,
    ) -> PdLimits {
        PdLimits {
            planner_timeout_minutes: planner_timeout_minutes
                .clamp(PLANNER_TIMEOUT_MINUTES_MIN, PLANNER_TIMEOUT_MINUTES_MAX),
            plan_review_minutes: plan_review_minutes
                .clamp(PLAN_REVIEW_MINUTES_MIN, PLAN_REVIEW_MINUTES_MAX),
            drive_timeout_minutes: drive_timeout_minutes.clamp(
                crate::workflow::DRIVER_DRIVE_TIMEOUT_MIN,
                crate::workflow::DRIVER_DRIVE_TIMEOUT_MAX,
            ),
        }
    }

    /// The planner stall bound, in milliseconds.
    pub fn planner_timeout_ms(&self) -> u64 {
        self.planner_timeout_minutes as u64 * 60_000
    }

    /// The whole-drive bound, in milliseconds.
    pub fn drive_timeout_ms(&self) -> u64 {
        self.drive_timeout_minutes as u64 * 60_000
    }
}

// ── §2(f) the record ────────────────────────────────────────────────────────

/// `plan_drives.json`'s schema version.
pub const PLAN_DRIVES_VERSION: u32 = 1;

/// The file's name inside the group dir. It sits beside `state.json`,
/// `tasks.json`, `merge_queue.json` and `review_drives.json`; the group dir
/// itself is built by `group_dir_at`, the only place a group id becomes a path.
pub const PLAN_DRIVES_FILE: &str = "plan_drives.json";

/// The whole of `plan_drives.json` (§2(f)).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanDrivesState {
    /// Schema version. **Required** — a file with no version is malformed, not
    /// a v1 file, and the reconcile refuses such a file loudly rather than
    /// guessing at it.
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<PdEntry>,
    /// Fields written by a newer build, preserved verbatim across a read/write
    /// cycle.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for PlanDrivesState {
    fn default() -> Self {
        PlanDrivesState { version: PLAN_DRIVES_VERSION, entries: Vec::new(), extra: BTreeMap::new() }
    }
}

impl PlanDrivesState {
    /// Whether this build understands the file's schema.
    ///
    /// The conservative half of forward compatibility: unknown *fields* are
    /// preserved, but a file whose whole schema moved is one this build must
    /// **not act on** — the fields it recognizes may no longer mean what it
    /// thinks.
    pub fn version_supported(&self) -> bool {
        self.version == PLAN_DRIVES_VERSION
    }

    /// The entry for an issue, if this file has one at all.
    pub fn entry(&self, issue: u64) -> Option<&PdEntry> {
        self.entries.iter().find(|e| e.issue == issue)
    }

    /// The entry for an issue, mutably.
    pub fn entry_mut(&mut self, issue: u64) -> Option<&mut PdEntry> {
        self.entries.iter_mut().find(|e| e.issue == issue)
    }

    /// Whether this issue has a **live** drive — the scope of the
    /// `already-driven` refusal. A parked entry is deliberately not live:
    /// resuming one is what `resume_plan_drive` is for, and a flat refusal
    /// would make that path unreachable.
    pub fn is_driven(&self, issue: u64) -> bool {
        self.entry(issue).is_some_and(|e| e.state().is_live())
    }
}

/// Where one SLICE of a running plan stands (§2(b) step 6).
///
/// **Per slice, and that is the whole point of the state living here rather
/// than on the drive.** A worker reporting `blocked` parks ITS slice; the
/// independent slices keep going. A drive-level hold is for the three things
/// that really are drive-wide — consent withdrawn, a struck row, the stall
/// backstop — and nothing else is allowed to stop a plan wholesale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SliceState {
    /// Boarded, waiting for its deps and for the one spawn slot a tick has.
    Queued,
    /// A worker pane is open on it.
    Running,
    /// Its PR was handed to the review driver, which owns the pane from there.
    InReview,
    /// Its PR is positively MERGED and its board row says `done`.
    Done,
    /// Parked, carrying a [`PdSliceHold`]. The drive keeps running.
    Held,
}

impl SliceState {
    /// Every state, so a test walks the machine rather than a remembered list.
    pub const ALL: [SliceState; 5] = [
        SliceState::Queued,
        SliceState::Running,
        SliceState::InReview,
        SliceState::Done,
        SliceState::Held,
    ];

    /// The wire/audit spelling — the string serde writes.
    pub fn as_str(self) -> &'static str {
        match self {
            SliceState::Queued => "queued",
            SliceState::Running => "running",
            SliceState::InReview => "in-review",
            SliceState::Done => "done",
            SliceState::Held => "held",
        }
    }

    /// Parse a state word; `None` for anything unrecognized.
    pub fn parse(s: &str) -> Option<SliceState> {
        SliceState::ALL.into_iter().find(|st| st.as_str() == s.trim())
    }
}

/// Why one slice is parked while its drive keeps running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PdSliceHold {
    /// The live-delegate cap refused this slice's spawn for longer than
    /// [`crate::reviewdrive::CAP_HOLD_MS`]. **A cap refusal is not an error** —
    /// the row stays `queued` and is retried every tick; this is the bound on
    /// retrying forever, and it is the review driver's own `cap-full` with its
    /// own constant, reused rather than re-chosen.
    CapFull,
    /// The slice's worker reported `blocked`. One notice, carrying the
    /// worker's own note, and the other slices keep going.
    WorkerBlocked,
    /// The slice's PR was CLOSED without merging. A row is marked `done` only
    /// on a positively-established MERGED PR, so this is the other positive
    /// answer and it gets its own name rather than being waited out.
    PrClosed,
    /// The slice's worker reported `done` and **no PR ever appeared** on its
    /// branch, for longer than [`PR_WAIT_HOLD_MS`].
    ///
    /// The worker violated its own definition of done, which is exactly the
    /// wrongness this driver is built to survive — a `ref` is a HINT and a
    /// missing one is meant to cost a `gh pr list` and no more. What it must
    /// not cost is silence: without this bound the slice sits in
    /// [`SliceState::Running`] forever, retrying a lookup that will never
    /// succeed, and because a slice in `Running` makes
    /// [`PdFacts::running_idle`] false the whole-drive backstop cannot see it
    /// either. [`WorkerGone`](Self::WorkerGone) does not reach this one: that
    /// arm skips a slice whose worker HAS reported, because a worker that
    /// reports and then exits is a worker that did its job.
    ///
    /// **Held whether the pane is alive or dead**, and that is one rule rather
    /// than two: a worker that said it was finished a quarter of an hour ago
    /// and produced no PR is not about to, and a live pane makes the claim no
    /// truer. The bound is what distinguishes it from a worker that reports
    /// `done` a moment before its PR is visible, which is the common case and
    /// is retried exactly as before.
    PrMissing,
    /// The slice's worker PANE DIED without ever reporting — a CLI crash, a
    /// kill, a machine that went away.
    ///
    /// **Without this arc the drive runs forever in silence**, which is the one
    /// outcome the whole design exists to avoid. Nothing synthesizes a report
    /// for a dead pane: the interception in `mcp.rs` fires only when the worker
    /// itself calls `report`, so a crashed worker leaves its slice in
    /// [`SliceState::Running`] — and a slice in `Running` is exactly what makes
    /// [`PdFacts::running_idle`] false, so even the stall backstop cannot see
    /// it. The planner has had this arm since P3a
    /// ([`PdHeldReason::PlanMissing`] via `planner_live`); this is the
    /// worker's, and it is the same rule.
    WorkerGone,
}

impl PdSliceHold {
    /// Every reason, so the notice table is checked against the enum.
    pub const ALL: [PdSliceHold; 5] = [
        PdSliceHold::CapFull,
        PdSliceHold::WorkerBlocked,
        PdSliceHold::PrClosed,
        PdSliceHold::PrMissing,
        PdSliceHold::WorkerGone,
    ];

    /// The wire/audit spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            PdSliceHold::CapFull => "cap-full",
            PdSliceHold::WorkerBlocked => "worker-blocked",
            PdSliceHold::PrClosed => "pr-closed",
            PdSliceHold::PrMissing => "pr-missing",
            PdSliceHold::WorkerGone => "worker-gone",
        }
    }

    /// Parse a reason word; `None` for anything unrecognized.
    pub fn parse(s: &str) -> Option<PdSliceHold> {
        PdSliceHold::ALL.into_iter().find(|r| r.as_str() == s.trim())
    }

    /// The one sentence this slice hold's notice leads with. Held beside the
    /// reason for [`PdHeldReason::notice_line`]'s reason.
    pub fn notice_line(self) -> &'static str {
        match self {
            PdSliceHold::CapFull => {
                "the live-delegate cap refused this slice's worker for long enough that the \
                 drive stopped retrying — free a delegate and resume the drive"
            }
            PdSliceHold::WorkerBlocked => {
                "its worker reported blocked; the other slices are still going"
            }
            PdSliceHold::PrClosed => {
                "its PR was closed without merging, so the slice is not done and its dependents \
                 will never become ready on their own"
            }
            PdSliceHold::PrMissing => {
                "its worker reported done and no PR ever appeared on its branch, so there is \
                 nothing to hand to the review driver — check what that worker actually pushed"
            }
            PdSliceHold::WorkerGone => {
                "its worker's pane died without ever reporting, so nothing is going to finish this \
                 slice — free its board row and resume the drive, or brief it by hand"
            }
        }
    }
}

/// One slice's run state, keyed in [`PdEntry::slices`] by the slice id.
///
/// **The plan is not copied in here.** Everything a slice IS — its title,
/// branch, block, deps, brief and `hold` flag — is read off the stored
/// [`PlanDoc`] every tick, so there is exactly one copy of it and a plan and
/// its run state cannot drift. What lives here is only what HAPPENED: the row
/// it was boarded as, how far it got, and the pane and PR it produced.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PdSlice {
    /// The board row this slice was boarded as.
    pub task_id: String,
    /// Private, so every write goes through [`PdSlice::advance`].
    state: SliceState,
    /// Set exactly when `state == Held`.
    #[serde(default)]
    pub hold: Option<PdSliceHold>,
    /// The worker pane's agent id — the interception key, and empty until a
    /// spawn succeeds.
    #[serde(default)]
    pub agent: String,
    /// The worker's session, which is what the review driver resumes.
    #[serde(default)]
    pub session: String,
    /// The slice's PR. `0` means "none yet"; a PR number is never zero.
    #[serde(default)]
    pub pr: u64,
    /// When the live-delegate cap first refused this slice's spawn, or `0`.
    /// **Cleared the moment a spawn succeeds**, so a slice that was starved and
    /// then ran does not carry a clock toward a hold it has already escaped.
    #[serde(default)]
    pub cap_starved_since_ms: u64,
    /// When the worker pane was opened.
    #[serde(default)]
    pub spawned_ms: u64,
    /// When this slice started waiting for a PR that has not appeared — stamped
    /// the first tick it is `reported_done` with no PR, cleared the moment one
    /// resolves. `0` means "not waiting", which is why every read is guarded
    /// rather than subtracted from blindly: an unset anchor is not an ancient
    /// one, the rule [`cap_starved_since_ms`](Self::cap_starved_since_ms)
    /// follows.
    #[serde(default)]
    pub pr_wait_since_ms: u64,
    /// The worktree this slice was given, remembered so a slice released from a
    /// hold RESUMES in the workspace it already had.
    ///
    /// Without it a released slice tries to cut its branch a second time and
    ///  refuses the path that is already there — which made
    /// the release fail silently, one layer below the board rollback that made
    /// it fail silently the first time.
    #[serde(default)]
    pub cwd: String,
    /// This slice's worker has reported `done`, and the hand-off to the review
    /// driver has not happened yet.
    ///
    /// **On the RECORD rather than only in the tick's signal map**, and the
    /// difference is a lost hand-off. A `report(done)` arrives as an event; the
    /// tick that consumes it clears it whether or not it could act on it, and
    /// the PR may not be resolvable on that tick at all — the worker named no
    /// `ref` and `gh pr list --head` has not seen the branch yet, or `gh` was
    /// down. Left in the signal map, that `done` is gone and the slice sits
    /// `running` until the whole-drive stall backstop. Written here, the next
    /// tick simply tries again.
    ///
    /// Cleared by the hand-off, which is the thing it is waiting for.
    #[serde(default)]
    pub reported_done: bool,
    /// Fields written by a newer build, preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl PdSlice {
    /// A freshly boarded slice.
    pub fn boarded(task_id: &str) -> PdSlice {
        PdSlice {
            task_id: task_id.to_string(),
            state: SliceState::Queued,
            hold: None,
            agent: String::new(),
            session: String::new(),
            pr: 0,
            cap_starved_since_ms: 0,
            spawned_ms: 0,
            pr_wait_since_ms: 0,
            cwd: String::new(),
            reported_done: false,
            extra: BTreeMap::new(),
        }
    }

    /// This slice's state.
    pub fn state(&self) -> SliceState {
        self.state
    }

    /// Move to `to`, keeping `hold` set exactly when the destination is
    /// `Held` — [`PdEntry::advance`]'s rule, for its reason: "parked with no
    /// reason" and "working with a stale reason" are both unrepresentable
    /// rather than merely unlikely.
    pub fn advance(&mut self, to: SliceState, hold: Option<PdSliceHold>) {
        self.hold = if to == SliceState::Held { hold } else { None };
        self.state = to;
    }

    /// Whether this slice is finished as far as the DRIVE is concerned — the
    /// only state that counts is `done`.
    ///
    /// A `held` slice is deliberately NOT settled: a drive that completed over
    /// a parked slice would report success for work nobody did. What settles a
    /// held slice is a human, on the board — see [`slice_settled_by_board`].
    pub fn is_done(&self) -> bool {
        self.state == SliceState::Done
    }
}

/// Whether a board row is one **this drive's own claim is still holding**, and
/// therefore one the driver may roll back (rev-std round 1, finding 3).
///
/// The driver rolls a row back in exactly two places — a refused spawn, and a
/// released slice hold — and they had two different rules: the release checked
/// the row still carried its slice's agent, the refusal checked nothing at all.
/// A refused spawn would then reset a row a human had claimed in the window
/// between the tick's board snapshot and `pd_spawn_slice`'s own claim, wiping
/// their assignment over a cap refusal that had nothing to do with them.
///
/// One rule, and it is a `pub fn` over plain values rather than a condition
/// spelled twice, because the race it guards is sub-second and not stageable
/// from a test: the *decision* is pinnable here even though the *situation* is
/// not. `claimant` is who the driver expects to find — `brand::AUDIT_ACTOR` for
/// a spawn it just claimed, the slice's own agent for a pane that is being
/// released.
pub fn rollback_is_ours(status: &str, assignee: Option<&str>, claimant: &str) -> bool {
    !claimant.is_empty()
        && status.trim() == "in-progress"
        && assignee.map(str::trim) == Some(claimant)
}

/// The same decision against **every claimant this drive could legally have
/// left on the row** (#3160).
///
/// [`rollback_is_ours`] takes the one name the caller expects, which is right
/// for a refused spawn: the claim being undone is the one the same function
/// just made, so `brand::AUDIT_ACTOR` is the only possible holder. The RELEASE
/// is not that case. `pd_spawn_slice` claims as `AUDIT_ACTOR` and then rewrites
/// the assignee to the pane it opened, and those are two separate board writes:
/// if the second one fails, the record names the agent while the row still
/// names the claimant. A release checking only the agent then finds a stranger,
/// skips the rollback, and leaves the row `in-progress` with nobody on it —
/// which is precisely the row-release defect this rollback exists to close,
/// re-entered through the gap between the two writes.
///
/// So the release asks about both names. The widening is safe for a property of
/// the NAMES rather than of the window: `AUDIT_ACTOR` is orrerix's own brand
/// actor, never an agent id and never a human's, so a row carrying it was
/// claimed by a driver and by nothing else. A human's claim matches neither
/// name and is still left alone, which is the whole of what the guard was
/// added for.
pub fn rollback_is_ours_of(status: &str, assignee: Option<&str>, claimants: &[&str]) -> bool {
    claimants.iter().any(|c| rollback_is_ours(status, assignee, c))
}

/// Board statuses that settle a slice **by the human's hand** (§2(c)).
///
/// `done` is the row the drive itself writes on a merged PR; `cancelled` and
/// `blocked` are a human's terminal verdicts, and a drive must respect them
/// rather than spawning over them — only `queued` is ever ready, so a row in
/// either never spawns, and counting it as settled is what stops the drive
/// waiting forever on a decision that has already been made.
pub const SETTLED_ROW_STATUSES: [&str; 3] = ["done", "cancelled", "blocked"];

/// Whether a board row's status settles its slice for the human.
pub fn slice_settled_by_board(status: &str) -> bool {
    SETTLED_ROW_STATUSES.contains(&status.trim())
}

/// One driven issue (§2(f)).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PdEntry {
    /// The issue this drive plans.
    pub issue: u64,
    /// Private on purpose: [`advance`](PdEntry::advance) is the only way to
    /// change it, and it goes through [`transition`].
    state: PlanDriveState,
    /// Set exactly when `state == Held`.
    #[serde(default)]
    pub held_reason: Option<PdHeldReason>,
    /// What the drive was doing when it parked — the state
    /// [`resume_target`](PdEntry::resume_target) returns to.
    #[serde(default)]
    pub held_from: Option<PlanDriveState>,
    /// The orchestrator this drive acts for; every audit row carries it.
    pub on_behalf_of: String,
    /// The roster block the planner was spawned from.
    pub planner_block: String,
    /// The planner pane's agent id — the interception key (§2(b) step 3).
    #[serde(default)]
    pub planner_agent: String,
    /// The planner's session id, for the record and the status view.
    #[serde(default)]
    pub planner_session: String,
    /// The label read at `drive_plan`. Re-read every tick; this is what the
    /// drive was *started under*, not what it trusts.
    pub consent: Consent,
    /// The base branch slices are to be cut from, if the orchestrator named
    /// one. **P3a records it and spends it on nothing** — P3b's spawns do.
    #[serde(default)]
    pub base: Option<String>,
    /// This drive's own review window in minutes, from the `drive_plan` call.
    /// Same P3a status as [`PdLimits::plan_review_minutes`].
    #[serde(default)]
    pub review_minutes: u32,
    /// When the drive began. **Absolute**, never an elapsed figure: a stored
    /// elapsed time is stale the instant it is written and meaningless across a
    /// restart.
    pub started_ms: u64,
    /// When the entry entered its current state.
    pub state_since_ms: u64,
    /// When the planner pane was opened — the stall bound's anchor. `0` means
    /// "no pane yet", which is why every read of it is guarded rather than
    /// subtracted from blindly: an unset anchor is not an ancient one.
    #[serde(default)]
    pub spawned_ms: u64,
    /// How many plan blocks this drive has refused (§2(e)).
    #[serde(default)]
    pub invalid_count: u32,
    /// The last refusal's reasons, so the hold's notice can say what was wrong
    /// rather than that something was.
    #[serde(default)]
    pub last_invalid: Vec<String>,
    /// The validated plan, stored by the `post_issue_comment` hook at post
    /// time. `None` until then.
    #[serde(default)]
    pub plan: Option<PlanDoc>,
    /// The URL of the comment the plan was posted as.
    #[serde(default)]
    pub comment_url: String,
    /// When the plan was posted.
    #[serde(default)]
    pub posted_ms: u64,
    /// Slice id -> what has HAPPENED to that slice: its board row, how far it
    /// got, the pane and the PR. Empty until [`PlanDriveState::Boarding`] has
    /// run, and empty forever on an `agent-investigation` drive, which never
    /// boards.
    ///
    /// A map rather than a `Vec` keyed by position because the plan is the
    /// only thing that says what slices there ARE: a positional record would
    /// go wrong the first time somebody resumed a drive off a record whose
    /// plan had been re-posted, and a slice id is already a
    /// `PathSegment`-checked value.
    #[serde(default)]
    pub slices: BTreeMap<String, PdSlice>,
    /// Where the in-review PR poll's round robin got to.
    ///
    /// **Persisted rather than restarted at zero every tick**, which is what
    /// makes the poll fair: a drive with more in-review slices than the
    /// per-tick budget would otherwise look at the same first few forever and
    /// never notice a later one merging. Monotonic and taken modulo the list
    /// length at read time, so a plan that shrinks cannot make it point off the
    /// end.
    #[serde(default)]
    pub pr_poll_cursor: u64,
    /// When the drive last made progress — a slice spawned, reached review,
    /// merged or parked.
    ///
    /// **The stall meter for [`PlanDriveState::Running`], and it is a different
    /// meter from the one the other states use** (§2(e)). A whole-drive AGE
    /// bound is right for a drive that has not started work yet; for one that
    /// is executing a plan it would park a perfectly healthy multi-day plan on
    /// `drive-stalled` for the crime of being big. What is actually wrong in
    /// `running` is a drive that has nothing spawnable, nothing in review, and
    /// has not moved — which is what this measures. `0` means "never moved",
    /// and every read falls back to `started_ms` rather than subtracting from
    /// an unset anchor.
    #[serde(default)]
    pub last_progress_ms: u64,
    /// A notice this drive owes the orchestrator and has not delivered.
    #[serde(default)]
    pub owed: Option<OwedNotice>,
    /// Fields written by a newer build, preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl PdEntry {
    /// A fresh drive in `planning`, before a planner has been spawned.
    pub fn new(
        issue: u64,
        on_behalf_of: &str,
        planner_block: &str,
        consent: Consent,
        base: Option<String>,
        review_minutes: u32,
        now_ms: u64,
    ) -> PdEntry {
        PdEntry {
            issue,
            state: PlanDriveState::Planning,
            held_reason: None,
            held_from: None,
            on_behalf_of: on_behalf_of.to_string(),
            planner_block: planner_block.to_string(),
            planner_agent: String::new(),
            planner_session: String::new(),
            consent,
            base,
            review_minutes,
            started_ms: now_ms,
            state_since_ms: now_ms,
            spawned_ms: 0,
            invalid_count: 0,
            last_invalid: Vec::new(),
            plan: None,
            comment_url: String::new(),
            posted_ms: 0,
            slices: BTreeMap::new(),
            pr_poll_cursor: 0,
            last_progress_ms: 0,
            owed: None,
            extra: BTreeMap::new(),
        }
    }

    /// This entry's state. The field is private so that every write goes
    /// through [`advance`](Self::advance) and therefore through [`transition`].
    pub fn state(&self) -> PlanDriveState {
        self.state
    }

    /// Move to `to`, through [`transition`].
    ///
    /// `held_reason` and `held_from` are set exactly when the destination is
    /// `Held` and cleared on every other arc, so "parked with no reason" and
    /// "working with a stale reason" are both unrepresentable rather than
    /// merely unlikely.
    pub fn advance(
        &mut self,
        to: PlanDriveState,
        held: Option<PdHeldReason>,
        now_ms: u64,
    ) -> Result<(), PdInvalidTransition> {
        transition(self.state, to)?;
        if to == PlanDriveState::Held {
            self.held_from = Some(self.state);
            self.held_reason = held;
        } else {
            self.held_from = None;
            self.held_reason = None;
        }
        self.state = to;
        self.state_since_ms = now_ms;
        Ok(())
    }

    /// Take one [`PdStep`].
    pub fn take(&mut self, step: &PdStep, now_ms: u64) -> Result<(), PdInvalidTransition> {
        self.advance(step.to, step.held, now_ms)
    }

    /// Where a resume goes: the state the hold came from, or `planning` for an
    /// entry written before `held_from` was set.
    ///
    /// **Never a terminal or another hold**, whatever the file says: a
    /// `held_from` naming `complete` would resume a finished drive into a state
    /// with no arcs out of it, and a hand-edited record must not be able to
    /// produce one. `is_live` is the check because it excludes both at once.
    pub fn resume_target(&self) -> PlanDriveState {
        match self.held_from {
            Some(s) if s.is_live() => s,
            _ => PlanDriveState::Planning,
        }
    }

    /// Store the plan the hook validated, and advance to `plan-posted`.
    ///
    /// One method rather than four field writes at the call site, because they
    /// have to happen together: a stored plan with the state left at `planning`
    /// is a drive whose next tick re-derives `plan-posted` (harmless), while an
    /// advanced state with no plan is a drive that will board nothing (not
    /// harmless).
    pub fn store_plan(
        &mut self,
        plan: PlanDoc,
        comment_url: &str,
        now_ms: u64,
    ) -> Result<(), PdInvalidTransition> {
        self.plan = Some(plan);
        self.comment_url = comment_url.to_string();
        self.posted_ms = now_ms;
        self.invalid_count = 0;
        self.last_invalid.clear();
        self.advance(PlanDriveState::PlanPosted, None, now_ms)
    }

    /// Record one refused plan block. Answers whether the refusal count has
    /// reached [`MAX_INVALID_POSTS`], which is the caller's cue to hold.
    pub fn note_invalid(&mut self, reasons: &[String]) -> bool {
        self.invalid_count = self.invalid_count.saturating_add(1);
        self.last_invalid = reasons.to_vec();
        self.invalid_count >= MAX_INVALID_POSTS
    }

    /// This drive's whole age.
    pub fn age_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.started_ms)
    }

    /// How long it has been in its current state.
    pub fn state_elapsed_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.state_since_ms)
    }

    /// How long the planner pane has been open, or `None` if there is none.
    pub fn planner_age_ms(&self, now_ms: u64) -> Option<u64> {
        (self.spawned_ms > 0).then(|| now_ms.saturating_sub(self.spawned_ms))
    }

    /// Stamp progress. Called by the wiring on every slice spawn, hand-off,
    /// merge and park — the four things that mean this drive is alive.
    pub fn note_progress(&mut self, now_ms: u64) {
        self.last_progress_ms = now_ms;
    }

    /// How long since this drive last moved, measured from
    /// [`last_progress_ms`](Self::last_progress_ms) and falling back to
    /// `started_ms` for a drive that has not moved yet.
    pub fn idle_ms(&self, now_ms: u64) -> u64 {
        let anchor = if self.last_progress_ms > 0 { self.last_progress_ms } else { self.started_ms };
        now_ms.saturating_sub(anchor)
    }

    /// This drive's own review window in milliseconds (§2(c)). Resolved at
    /// `drive_plan` from the call's argument or the repo key, so there is one
    /// number here rather than two to reconcile every tick.
    pub fn review_window_ms(&self) -> u64 {
        self.review_minutes as u64 * 60_000
    }

    /// Is this agent the worker of one of this drive's slices? Answers the
    /// slice id.
    ///
    /// [`driven_role`](Self::driven_role)'s property, unchanged and for its
    /// reason: keyed on the agent id orrerix minted at spawn, never on text a
    /// caller supplies, and an empty `agent` matches nobody.
    pub fn slice_of_agent(&self, agent_id: &str) -> Option<&str> {
        if agent_id.is_empty() {
            return None;
        }
        self.slices
            .iter()
            .find(|(_, s)| !s.agent.is_empty() && s.agent == agent_id)
            .map(|(id, _)| id.as_str())
    }

    /// Owe the orchestrator a notice.
    pub fn owe_notice(&mut self, text: &str, now_ms: u64) {
        self.owed = Some(OwedNotice { text: text.to_string(), owed_ms: now_ms, failures: 0 });
    }

    /// The owed notice landed.
    pub fn notice_delivered(&mut self) {
        self.owed = None;
    }

    /// The owed notice did not land; keep it and count the attempt.
    pub fn notice_delivery_failed(&mut self) {
        if let Some(n) = self.owed.as_mut() {
            n.failures = n.failures.saturating_add(1);
        }
    }

    /// Is this agent this drive's planner?
    ///
    /// **Keyed on the agent id orrerix minted at spawn**, never on text a
    /// caller supplies — `rd_owner`'s property, for the same reason: a delegate
    /// that could choose whether its report reaches the orchestrator by naming
    /// an issue number is a delegate that can route around the orchestrator.
    ///
    /// An empty `planner_agent` (a drive whose spawn has not happened yet, or
    /// one whose record predates the field) matches nobody, which a bare string
    /// comparison would not guarantee on its own.
    pub fn driven_role(&self, agent_id: &str) -> bool {
        !self.planner_agent.is_empty() && self.planner_agent == agent_id
    }
}

// ── §2(f) the file ──────────────────────────────────────────────────────────

/// Why `plan_drives.json` could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PdStateError {
    /// The file is there and does not parse — torn, hand-edited, or naming a
    /// state this build does not know. The tick refuses, audits
    /// `pd-state-unreadable` and backs off; it never repairs and never deletes.
    Malformed(String),
    /// A schema this build does not understand. Do not operate; do not write.
    Unsupported(u32),
    /// The file is there and could not be read at all.
    Io(String),
}

/// The group's plan-drive file.
pub fn state_path(group_dir: &Path) -> PathBuf {
    group_dir.join(PLAN_DRIVES_FILE)
}

/// Read the group's plan-drive state.
///
/// **An absent file is no drives, not an error** — that is the product default.
/// Every other failure is a [`PdStateError`], because the difference between
/// "nothing is driven" and "orrerix cannot tell what is driven" is exactly what
/// the tools' own `pd-state-unreadable` exists for: answering `not-driven` over
/// a torn file asserts something orrerix cannot know, while a drive may well be
/// live.
pub fn load_state(group_dir: &Path) -> Result<PlanDrivesState, PdStateError> {
    let text = match std::fs::read_to_string(state_path(group_dir)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PlanDrivesState::default())
        }
        Err(e) => return Err(PdStateError::Io(e.to_string())),
    };
    parse_state(&text)
}

/// The parse half of [`load_state`], without the file — so the refusals can be
/// pinned on a string.
pub fn parse_state(text: &str) -> Result<PlanDrivesState, PdStateError> {
    let state: PlanDrivesState =
        serde_json::from_str(text).map_err(|e| PdStateError::Malformed(e.to_string()))?;
    if !state.version_supported() {
        return Err(PdStateError::Unsupported(state.version));
    }
    Ok(state)
}

/// Write the plan-drive state atomically, reusing
/// [`crate::fsatomic::atomic_write`] — the #133-hardened writer (same-directory
/// temp, `sync_all` before the rename, a fallback that keeps the temp on
/// failure).
///
/// Deliberately not a fresh `fs::write`: a disk-full `fs::write` is what
/// truncated `tasks.json` and destroyed a live board in #133, and this file has
/// the same "losing it loses in-flight work" property — the planner's whole
/// output lives in it.
pub fn store_state(group_dir: &Path, state: &PlanDrivesState) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
    crate::fsatomic::atomic_write(&state_path(group_dir), &bytes).map_err(|e| e.to_string())
}

// ── §2(b) the decision ──────────────────────────────────────────────────────

/// What the planner has said to the drive, if anything.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlannerSignal {
    /// Nothing this window.
    #[default]
    None,
    /// `report(done)`.
    Done,
    /// `report(blocked)`.
    Blocked,
}

/// Everything one tick resolved about the world, as values.
///
/// Each fallible reading is an `Option` whose `None` means **unknown**, never a
/// default: `issue_open: None` is "orrerix could not read the issue", and
/// nothing cancels on it. That is [`crate::rddrive::pr_is_open`]'s discipline,
/// and the reason is the same — `Some(false)` cancels a live drive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PdFacts {
    /// The tick's clock. Carried here rather than as a second `decide`
    /// parameter so that the clock a decision is made against and the clock its
    /// facts were read at cannot come apart at a call site.
    pub now_ms: u64,
    /// Is the issue open? `None` = orrerix could not tell.
    pub issue_open: Option<bool>,
    /// Which label the issue carries now. The OUTER `Option` is "orrerix could
    /// not read the labels"; the INNER is "it read them and neither label is
    /// there", which is the withdrawal §2(d) holds on. Two different facts, and
    /// collapsing them would hold a live drive on a `gh` outage.
    pub consent: Option<Option<Consent>>,
    /// Is the planner's pane still alive?
    pub planner_live: bool,
    /// What the planner reported this window.
    pub planner: PlannerSignal,
    /// Every slice in the plan has a board row (§2(b) step 5). The wiring
    /// writes the rows; this is how it says the writing is finished.
    pub boarded: bool,
    /// A slice this drive boarded no longer has a row — struck by hand
    /// (§2(c)). Its dependents now carry a dangling dep, unmet forever.
    pub row_removed: bool,
    /// Every slice is settled: the record says `done`, or the human's own row
    /// status does (`done`/`cancelled`/`blocked`). See
    /// [`slice_settled_by_board`].
    pub slices_settled: bool,
    /// Nothing is spawnable and nothing is in review — the condition §2(e)
    /// bounds with `drive_timeout_minutes`. A drive whose slices are all with
    /// the review driver is waiting on THAT driver's clock, not on this one,
    /// so it is not idle here.
    pub running_idle: bool,
}

/// One tick's decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PdStep {
    /// The state to move to.
    pub to: PlanDriveState,
    /// Set exactly when `to == Held`.
    pub held: Option<PdHeldReason>,
}

impl PdStep {
    /// A move with no hold.
    pub fn to(to: PlanDriveState) -> PdStep {
        PdStep { to, held: None }
    }
    /// A park, with its reason.
    pub fn held(reason: PdHeldReason) -> PdStep {
        PdStep { to: PlanDriveState::Held, held: Some(reason) }
    }
}

/// **The whole of one tick's decision** — the only function that says what a
/// plan drive does next, and it touches nothing.
///
/// `None` means *stay where you are*, which is the common answer: a planner
/// that is still writing is a drive with nothing to do.
///
/// # Why the order of the arms is the design
///
/// 1. **A positively-closed issue cancels, first.** The human closed the issue;
///    everything below is work on something that is over. Unknown does NOT
///    cancel — see [`PdFacts`].
/// 2. **Withdrawn consent holds, second.** §2(d) makes the label the whole of
///    the human's consent, re-read rather than remembered, so it outranks every
///    reason the drive has to carry on. It does not outrank arc 1, because a
///    closed issue's labels are not a withdrawal — they are irrelevant.
/// 3. **A stored plan outranks the planner's stall clock.** A plan that arrived
///    is a plan; parking a drive on `planner-stalled` a tick after the block
///    landed would be measuring the wrong thing.
/// 4. **`blocked` outranks `done`.** The two can only both be present if the
///    planner said one and then the other, and `blocked` is the one that needs
///    a human — [`crate::reviewdrive`]'s own rule.
/// 5. **A struck row outranks everything the executor could otherwise do.** A
///    dependent whose dep names no row is unmet forever, so carrying on would
///    be spending ticks on work that cannot become ready.
/// 6. **The stall backstop is last**, so a drive that has just moved is never
///    parked for being old on the tick it did something — and in `running` it
///    measures IDLENESS rather than age, because a big plan legitimately runs
///    for days (see [`PdEntry::last_progress_ms`]).
pub fn decide(entry: &PdEntry, facts: &PdFacts, limits: &PdLimits) -> Option<PdStep> {
    use PlanDriveState::*;
    let state = entry.state();
    if !state.is_live() {
        // Terminal and parked entries are not advanced by a tick. A parked one
        // moves through `resume_plan_drive` / `cancel_plan_drive`, which is what
        // makes a hold a hand-back to a human rather than a quieter drive.
        return None;
    }

    // 1.
    if facts.issue_open == Some(false) {
        return Some(PdStep::to(Cancelled));
    }
    // 2. `Some(None)` — the labels were read, and neither is there.
    if facts.consent == Some(None) {
        return Some(PdStep::held(PdHeldReason::ConsentWithdrawn));
    }
    // The consent the drive acts on THIS tick: what was just read, falling back
    // to what the entry was started under when the read itself failed. An
    // unreadable label list is not a withdrawal, and it is not a change of kind
    // either.
    let consent = facts.consent.flatten().unwrap_or(entry.consent);

    let step = match state {
        Planning => {
            // 3.
            if entry.plan.is_some() {
                Some(PdStep::to(PlanPosted))
            } else if entry.invalid_count >= MAX_INVALID_POSTS {
                Some(PdStep::held(PdHeldReason::PlanInvalid))
            // 4.
            } else if facts.planner == PlannerSignal::Blocked {
                Some(PdStep::held(PdHeldReason::PlannerBlocked))
            } else if facts.planner == PlannerSignal::Done {
                // The planner says it is finished and there is no plan. That is
                // not a stall and it is never going to become one — the pane is
                // done — so it gets its own reason rather than waiting out a
                // timeout under a name that would misdescribe it.
                Some(PdStep::held(PdHeldReason::PlanMissing))
            } else if entry
                .planner_age_ms(facts.now_ms)
                .is_some_and(|age| age >= limits.planner_timeout_ms())
            {
                Some(PdStep::held(PdHeldReason::PlannerStalled))
            } else if entry.spawned_ms > 0 && !facts.planner_live {
                // The pane is gone and never posted. Same outcome as a `done`
                // with no plan, and named the same way: what is missing is the
                // plan, and nothing is going to produce it now.
                Some(PdStep::held(PdHeldReason::PlanMissing))
            } else {
                None
            }
        }
        PlanPosted => match consent {
            // §2(d): research only. The plan IS the deliverable; the drive
            // never boards and never spawns a worker.
            Consent::Investigation => Some(PdStep::to(Complete)),
            // §2(c): the declared review window, and the DEFAULT is no window
            // at all. Under `agent-ready` the human has already pressed go, and
            // a window costs the orchestrator turn this whole design exists to
            // remove — so it is paid only when somebody asked for it.
            Consent::Ready if entry.review_window_ms() > 0 => Some(PdStep::to(PlanReview)),
            Consent::Ready => Some(PdStep::to(Boarding)),
        },
        // The window, waited out against the state clock rather than the
        // drive's: it is time in THIS state that was promised.
        PlanReview => (entry.state_elapsed_ms(facts.now_ms) >= entry.review_window_ms())
            .then(|| PdStep::to(Boarding)),
        // The wiring writes the rows; this arc is it saying they are written.
        // A drive that cannot board — a WIP cap, an unwritable board — simply
        // stays here and is bounded by the backstop below, which is the same
        // shape a cap-starved slice gets.
        Boarding => facts.boarded.then(|| PdStep::to(Running)),
        Running => facts.slices_settled.then(|| PdStep::to(Complete)),
        // Unreachable: `is_live` excluded all three above. Spelled out rather
        // than caught by a `_` arm so that a ninth state cannot land here
        // silently.
        Complete | Cancelled | Held => None,
    };
    // 5. Above the arcs' own answer, but below the state match, because a
    // struck row is only meaningful once there are rows: it is read off the
    // board against the slices this drive boarded.
    if facts.row_removed && matches!(state, Boarding | Running) {
        return Some(PdStep::held(PdHeldReason::RowRemoved));
    }
    if step.is_some() {
        return step;
    }
    // 6. **A declared review window is not a stall** (rev-std round 1, finding
    //    4). The backstop outside `running` measures whole-drive AGE, which
    //    includes a window the caller deliberately asked to wait out — so
    //    `drive_plan(review_minutes: 2000)` against the 720-minute default
    //    parked the drive on `drive-stalled` for doing exactly what it was
    //    told, with a notice naming the wrong cause. A drive inside its window
    //    is bounded by the window ITSELF: the `plan-review -> boarding` arc
    //    above fires on the tick it elapses, so this is a suppression with an
    //    end rather than an exemption.
    let in_window = state == PlanReview
        && entry.state_elapsed_ms(facts.now_ms) < entry.review_window_ms();
    let stalled = if state == Running {
        facts.running_idle && entry.idle_ms(facts.now_ms) >= limits.drive_timeout_ms()
    } else if in_window {
        false
    } else {
        entry.age_ms(facts.now_ms) >= limits.drive_timeout_ms()
    };
    if stalled {
        return Some(PdStep::held(PdHeldReason::DriveStalled));
    }
    None
}

// ── §3 the brief ────────────────────────────────────────────────────────────

/// **The brief one slice's worker is kicked off with** (§3), composed here so
/// that its layout is pinnable from a Tauri-free test.
///
/// Three parts, in this order and separated by one blank line each:
///
/// 1. a HEADER orrerix writes — the issue, the slice, the branch it is cut
///    from and to, the files the plan asked it not to touch, and the
///    red-before-green line the planner named;
/// 2. the planner's own `brief:`, **VERBATIM**. Nobody rewrites it, nothing is
///    truncated, and the only thing that touches it is the pane-text
///    sanitizer — which the CALLER applies, and passes in already applied, so
///    that a test of this function cannot accidentally certify a composition
///    the live site does not perform;
/// 3. the definition of done, the one copy every brief in this repo quotes.
///
/// `brief` and `dod` arrive as values for that second reason and for a third:
/// this crate cannot see `src-tauri`'s templates at all.
pub fn slice_brief(
    issue: u64,
    slice: &plandoc::Slice,
    base: Option<&str>,
    brief_sanitized: &str,
    dod: &str,
) -> String {
    let mut header = vec![
        format!(
            "You are slice `{}` of the plan for issue #{issue}: {}.",
            slice.id.as_str(),
            slice.title
        ),
        format!("Branch: `{}`", slice.branch.as_str()),
        format!(
            "Base: `{}`",
            base.map(str::trim)
                .filter(|b| !b.is_empty())
                .unwrap_or("the repository default branch")
        ),
    ];
    if !slice.avoid_files.is_empty() {
        header.push(format!("Do not touch: {}", slice.avoid_files.join(", ")));
    }
    if let Some(r) = slice.red_before_green.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
        header.push(format!("Red before green: {r}"));
    }
    // The header is joined and the two joints are fixed. **The planner's text is
    // neither trimmed nor wrapped nor re-indented** — "verbatim" is the whole
    // contract, and a trim is a rewrite however small.
    format!("{}\n\n{brief_sanitized}\n\n{dod}", header.join("\n"))
}

// ── §2(f) the audit vocabulary ──────────────────────────────────────────────

/// The `pd-*` audit actions, beside `rd-*`. Every one is written with
/// [`crate::rddrive::ON_BEHALF_OF`], so an audit reader filters on that detail
/// key rather than on the actor, which stays `brand::AUDIT_ACTOR`.
pub mod audit_action {
    /// `drive_plan` created an entry.
    pub const STARTED: &str = "pd-started";
    /// A tool call was refused, with its closed-vocabulary reason.
    pub const REFUSED: &str = "pd-refused";
    /// The planner pane was opened.
    pub const PLANNER_SPAWNED: &str = "pd-planner-spawned";
    /// A plan block was refused **in the tool**, and nothing was posted. Its
    /// own action rather than a detail on [`PLAN_POSTED`], for `rd-ci-red`'s
    /// reason: a reader counting refusals must not have to match the rows where
    /// the post went through.
    pub const PLAN_INVALID: &str = "pd-plan-invalid";
    /// A valid plan block was posted and stored, carrying its slice ids.
    pub const PLAN_POSTED: &str = "pd-plan-posted";
    /// The planner's `report` was consumed by the driver rather than delivered
    /// to the orchestrator. "Consumed" is a different word from "dropped".
    pub const PLANNER_CONSUMED: &str = "pd-planner-consumed";
    /// The plan became board rows, carrying the slice-id -> task-id map.
    pub const BOARDED: &str = "pd-boarded";
    /// One slice's worker pane was opened.
    pub const SLICE_SPAWNED: &str = "pd-slice-spawned";
    /// The live-delegate cap refused one slice's spawn. **Not an error row** —
    /// the slice stays queued and is retried; its own action so that a reader
    /// counting starvation does not have to match the rows where a spawn went
    /// through.
    pub const SLICE_CAP_REFUSED: &str = "pd-slice-cap-refused";
    /// One slice parked, with its closed [`PdSliceHold`]. Distinct from
    /// [`HELD`], which is the whole drive: a reader asking "did this plan
    /// stop" must not have to sort a slice's park out of the drive's.
    pub const SLICE_HELD: &str = "pd-slice-held";
    /// A slice's PR number was resolved, from the worker's `ref` or from the
    /// one `gh pr list --head` that stands in for it.
    pub const SLICE_PR: &str = "pd-slice-pr";
    /// A slice's PR was handed to the REVIEW driver, which owns the pane from
    /// there.
    pub const REVIEW_DRIVEN: &str = "pd-review-driven";
    /// A slice's PR is positively MERGED and its row is `done`.
    pub const SLICE_MERGED: &str = "pd-slice-merged";
    /// One slice's WORKER `report` was consumed by the driver rather than
    /// delivered to the orchestrator. Its own action beside
    /// [`PLANNER_CONSUMED`], because "the planner spoke" and "a worker spoke"
    /// are different facts and a reader chasing a drive needs to tell them
    /// apart.
    pub const SLICE_CONSUMED: &str = "pd-slice-consumed";
    /// The drive parked, with its closed reason.
    pub const HELD: &str = "pd-held";
    /// A parked drive was resumed.
    pub const RESUMED: &str = "pd-resumed";
    /// The drive was cancelled, carrying the panes it left running.
    pub const CANCELLED: &str = "pd-cancelled";
    /// The drive reached `complete`.
    pub const COMPLETE: &str = "pd-complete";
    /// A notice to the orchestrator was delivered, or failed to be.
    pub const NOTICE: &str = "pd-notice";
    /// A restart reconcile changed an entry.
    pub const RECOVERED: &str = "pd-recovered";
    /// `plan_drives.json` is there and orrerix cannot read it.
    pub const STATE_UNREADABLE: &str = "pd-state-unreadable";

    /// Every action above, so a test can assert the set rather than iterate a
    /// list someone has to remember to extend.
    pub const ALL: [&str; 21] = [
        STARTED,
        REFUSED,
        PLANNER_SPAWNED,
        PLAN_INVALID,
        PLAN_POSTED,
        PLANNER_CONSUMED,
        BOARDED,
        SLICE_SPAWNED,
        SLICE_CAP_REFUSED,
        SLICE_HELD,
        SLICE_PR,
        REVIEW_DRIVEN,
        SLICE_MERGED,
        SLICE_CONSUMED,
        HELD,
        RESUMED,
        CANCELLED,
        COMPLETE,
        NOTICE,
        RECOVERED,
        STATE_UNREADABLE,
    ];
}

/// The closed refusal vocabulary of the four plan-drive tools.
pub mod refusal {
    /// The repo declares no `driver:` block, declares it off, or has not set
    /// `driver.plan_enabled: true`. One name for all three because they are one
    /// answer to the caller: this repo has not turned the plan driver on.
    pub const DRIVER_DISABLED: &str = "plan-driver-disabled";
    /// The remote answered, and the issue is closed.
    pub const ISSUE_NOT_OPEN: &str = "issue-not-open";
    /// The remote did **not** answer. [`ISSUE_NOT_OPEN`] presumes it did;
    /// unknown is never treated as safe, and a drive must not start on an issue
    /// whose state orrerix could not read.
    pub const ISSUE_UNVERIFIABLE: &str = "issue-unverifiable";
    /// The issue carries neither `agent-ready` nor `agent-investigation`.
    /// Consent is the label, and there is none (§2(d)).
    pub const NOT_LABELLED: &str = "issue-not-labelled";
    /// This issue already has a live drive. A `held` entry is deliberately not
    /// live — `resume_plan_drive` is what a parked drive is for.
    pub const ALREADY_DRIVEN: &str = "already-driven";
    /// The roster declares no `kind: planner` block, or the `planner_block`
    /// named is not one.
    pub const NO_PLANNER_BLOCK: &str = "no-planner-block";
    /// The planner pane could not be opened — a delegate cap, a roster refusal.
    /// The tool's answer carries what refused, verbatim.
    pub const PLANNER_UNSPAWNABLE: &str = "planner-unspawnable";
    /// `cancel_plan_drive` / `resume_plan_drive`: this issue has no entry, or
    /// only a terminal one.
    pub const NOT_DRIVEN: &str = "not-driven";
    /// One slice's worker pane could not be opened for a reason that is NOT
    /// the live-delegate cap — an unknown block, a spawn-rate backstop. The cap
    /// has its own path (`pd-slice-cap-refused`, then
    /// [`PdSliceHold::CapFull`]) because a cap is transient and this is not.
    pub const SLICE_UNSPAWNABLE: &str = "slice-unspawnable";
    /// `resume_plan_drive`: the drive is live, so there is nothing to resume.
    /// Distinct from [`NOT_DRIVEN`], which is a drive that is not there at all;
    /// the two want different things from the orchestrator.
    pub const NOT_HELD: &str = "not-held";

    // ── and these three mean ORRERIX FAILED, not that the driver declined ──

    /// `plan_drives.json` is there and orrerix cannot read it — **NOT**
    /// "nothing is driven".
    pub const STATE_UNREADABLE: &str = "pd-state-unreadable";
    /// The change was computed and could not be saved, so it did not happen.
    pub const STATE_UNWRITABLE: &str = "pd-state-unwritable";
    /// A group orrerix cannot resolve at all.
    pub const UNAVAILABLE: &str = "pd-unavailable";
    /// The slice's pane opened and the board write that moves its row's
    /// assignee from the claimant to that pane did not (#3160). The spawn
    /// STANDS — a worker is running — so this is audited rather than raised:
    /// what it records is that the row and the record now disagree about who
    /// holds the slice, which is the fact a reader needs and the one the
    /// discarded `Result` used to swallow.
    pub const SLICE_ROW_UNASSIGNED: &str = "slice-row-unassigned";

    /// Whether a refusal names an **orrerix fault** rather than a policy
    /// decision — the distinction `queue_merge`'s contract uses capitals to
    /// make.
    pub fn is_orrerix_fault(reason: &str) -> bool {
        matches!(reason, STATE_UNREADABLE | STATE_UNWRITABLE | UNAVAILABLE)
    }

    /// Every name above, so a test can assert the set rather than iterate a
    /// list someone has to remember to extend.
    pub const ALL: [&str; 14] = [
        DRIVER_DISABLED,
        ISSUE_NOT_OPEN,
        ISSUE_UNVERIFIABLE,
        NOT_LABELLED,
        ALREADY_DRIVEN,
        NO_PLANNER_BLOCK,
        PLANNER_UNSPAWNABLE,
        SLICE_UNSPAWNABLE,
        NOT_DRIVEN,
        NOT_HELD,
        STATE_UNREADABLE,
        STATE_UNWRITABLE,
        UNAVAILABLE,
        SLICE_ROW_UNASSIGNED,
    ];
}

// ── the plan block, validated for a drive ───────────────────────────────────

/// Extract, parse and drive-validate a plan block out of a comment body — the
/// whole of what the `post_issue_comment` hook does before anything is posted
/// (§2(b) step 3).
///
/// Answers `Ok(doc)` or the line-numbered reasons the planner is handed back.
/// **Nothing is repaired**: [`plandoc`] refuses rather than rewriting, and this
/// adds only the two checks that need the DRIVE's context rather than the
/// document's — the issue number, and that the launched roster can serve every
/// block the plan names.
///
/// `blocks` is the launched roster's `(id, is_worker)` pairs. It is passed as a
/// value rather than read here because this crate cannot see a registry, and
/// because it is the roster **pinned at launch** that consents, never the
/// `.orrerix/workflow.yml` on disk right now (§2(d)).
pub fn validate_for_drive(
    body: &str,
    issue: u64,
    blocks: &[(String, bool)],
) -> Result<PlanDoc, Vec<String>> {
    let located = match plandoc::extract(body) {
        Ok(Some(l)) => l,
        Ok(None) => {
            return Err(vec![format!(
                "this comment carries no ```{} block, and a plan drive's plan has to be one — \
                 the schema is in your instructions",
                plandoc::FENCE_INFO
            )])
        }
        Err(e) => return Err(vec![e.to_string()]),
    };
    let doc = plandoc::parse(located.value, located.first_line)?;

    let mut errs = Vec::new();
    if doc.issue != issue {
        errs.push(format!(
            "plan block: issue: {} — this drive is planning #{issue}, and a plan for another \
             issue is refused rather than retargeted",
            doc.issue
        ));
    }
    for s in &doc.slices {
        match blocks.iter().find(|(id, _)| *id == s.block) {
            None => errs.push(format!(
                "plan block: slice {}: block {:?} is not in this group's roster — the roster is \
                 the one pinned when the group was launched, not the workflow file as it stands \
                 now",
                s.id, s.block
            )),
            // §2(d): a plan naming a reviewer — or a planner, or a manager — as
            // a slice's block is refused at post time. A slice is work, and
            // work is a worker's; anything else would have the drive spawn a
            // capability class the plan invented for itself.
            Some((_, false)) => errs.push(format!(
                "plan block: slice {}: block {:?} is not a worker — a slice is spawned as a \
                 worker, and this group's roster says that block is not one",
                s.id, s.block
            )),
            Some((_, true)) => {}
        }
    }
    if errs.is_empty() {
        Ok(doc)
    } else {
        Err(errs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **All four crossings of {status} × {assignee}**, plus the empty-claimant
    /// guard — the decision `rollback_is_ours` exists to make, pinned here
    /// because the SITUATION it guards is not stageable from an integration
    /// test (a human claiming a row inside a sub-second window) while the
    /// decision is (rev-std round 1, finding 3).
    ///
    /// The first row is the positive control: a rule that answered `false` to
    /// everything would satisfy every other assertion here.
    #[test]
    fn a_rollback_touches_only_a_row_this_drive_still_holds() {
        // Ours: claimed by us, still in progress.
        assert!(rollback_is_ours("in-progress", Some("orrerix"), "orrerix"));

        // Somebody else's claim — the case the unguarded rollback wiped.
        assert!(!rollback_is_ours("in-progress", Some("a-human"), "orrerix"));
        // Moved on by hand: done, blocked, cancelled, or back to queued.
        for status in ["done", "blocked", "cancelled", "queued", "review"] {
            assert!(
                !rollback_is_ours(status, Some("orrerix"), "orrerix"),
                "a row a human moved to {status:?} is theirs now, not ours"
            );
        }
        // Unassigned — nothing to roll back, and no claim of ours to find.
        assert!(!rollback_is_ours("in-progress", None, "orrerix"));
        assert!(!rollback_is_ours("in-progress", Some(""), "orrerix"));

        // An EMPTY claimant matches nobody, which is `driven_role`'s property
        // and is here for its reason: a slice with no agent recorded yet must
        // not match a row whose assignee is also empty.
        assert!(!rollback_is_ours("in-progress", Some(""), ""));
        assert!(!rollback_is_ours("in-progress", None, ""));

        // Whitespace is not a second spelling.
        assert!(rollback_is_ours(" in-progress ", Some(" orrerix "), "orrerix"));
    }

    /// The RELEASE's rule, which is the one-name rule asked twice (#3160).
    ///
    /// The first assertion is the positive control in the sense this file uses
    /// it everywhere else: a rule answering `false` to everything would satisfy
    /// every refusal below and pin nothing.
    #[test]
    fn a_release_rolls_back_a_row_left_holding_either_of_the_drives_own_names() {
        // The agent's own name — the ordinary case, unchanged by the widening.
        assert!(rollback_is_ours_of("in-progress", Some("w-7"), &["w-7", "orrerix"]));

        // The CLAIMANT's name, which is the row a failed post-spawn assignee
        // write leaves behind: the record names `w-7`, the row still names the
        // claim. Before #3160 this answered `false` and the row was stranded.
        assert!(rollback_is_ours_of("in-progress", Some("orrerix"), &["w-7", "orrerix"]));

        // A human's claim matches NEITHER name. This is the property the
        // widening must not cost, so it is asserted rather than argued.
        assert!(!rollback_is_ours_of("in-progress", Some("a-human"), &["w-7", "orrerix"]));

        // Every non-`in-progress` status is the human's decision under either
        // name, exactly as the one-name rule has it.
        for status in ["done", "blocked", "cancelled", "queued", "review"] {
            assert!(
                !rollback_is_ours_of(status, Some("orrerix"), &["w-7", "orrerix"]),
                "a row a human moved to {status:?} is theirs now under either name"
            );
        }

        // An EMPTY candidate never widens the set. A slice released before its
        // pane was ever recorded carries `agent == ""`, and the empty string
        // must not start matching an unassigned row just because it is passed
        // alongside a real name.
        assert!(!rollback_is_ours_of("in-progress", Some(""), &["", "orrerix"]));
        assert!(!rollback_is_ours_of("in-progress", None, &["", "orrerix"]));

        // No candidates at all is no rollback — `any` over an empty slice is
        // `false`, and that is the answer this caller wants rather than an
        // accident of the iterator.
        assert!(!rollback_is_ours_of("in-progress", Some("orrerix"), &[]));
    }
}
