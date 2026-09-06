//! The plan driver's pure core (#3040 P3a) — the state machine, the persisted
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
//! # The scope line, stated because a reader will look for the missing half
//!
//! P3a ships the drive as far as a plan: refuse, spawn a planner, validate and
//! store the block the planner posts, and then STOP — an `agent-investigation`
//! issue completes at that point by design (it never wanted workers), and an
//! `agent-ready` one parks at [`PdHeldReason::AwaitingP3b`], which is a named
//! hold with a notice rather than a drive that silently does nothing. P3b adds
//! the executor: board rows, slice spawns, and the hand-off to the review
//! driver. The state and reason vocabularies here are therefore exactly what
//! P3a *produces*; P3b extends both, and an older build reading a newer file
//! refuses it loudly through [`parse_state`] rather than acting on a word it
//! cannot read.

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
    /// The plan is being turned into board rows. **P3a parks here** — see the
    /// module doc and [`PdHeldReason::AwaitingP3b`].
    Boarding,
    /// Terminal: the drive did everything it was going to do. In P3a that is an
    /// `agent-investigation` issue whose plan is posted.
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
    pub const ALL: [PlanDriveState; 6] = [
        PlanDriveState::Planning,
        PlanDriveState::PlanPosted,
        PlanDriveState::Boarding,
        PlanDriveState::Complete,
        PlanDriveState::Cancelled,
        PlanDriveState::Held,
    ];

    /// The wire/audit spelling — the same string serde writes.
    pub fn as_str(self) -> &'static str {
        match self {
            PlanDriveState::Planning => "planning",
            PlanDriveState::PlanPosted => "plan-posted",
            PlanDriveState::Boarding => "boarding",
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
    /// The plan is boarded no further because **this build has no executor**
    /// (#3040 P3b). A named, audited, notice-bearing park — never a tick that
    /// quietly does nothing.
    AwaitingP3b,
    /// The whole drive outran `driver.drive_timeout_minutes` (the review
    /// driver's own knob, reused) without reaching a terminal state.
    DriveStalled,
}

impl PdHeldReason {
    /// Every reason, so the notice table and the audit vocabulary can be
    /// checked against the enum rather than against a list someone has to
    /// remember to extend.
    pub const ALL: [PdHeldReason; 7] = [
        PdHeldReason::PlanInvalid,
        PdHeldReason::PlanMissing,
        PdHeldReason::PlannerStalled,
        PdHeldReason::PlannerBlocked,
        PdHeldReason::ConsentWithdrawn,
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
            PdHeldReason::AwaitingP3b => {
                "the plan is posted and validated; boarding it and spawning its slices is not in \
                 this build (#3040 P3b), so the rows and the briefs are yours"
            }
            PdHeldReason::DriveStalled => {
                "the drive outran driver.drive_timeout_minutes without finishing"
            }
        }
    }
}

/// How many refused plan blocks one drive tolerates before parking (§2(e)).
///
/// The planner is still inside its own turn when a post is refused, so a fix
/// costs it one tool call and the orchestrator nothing; three is the point past
/// which the planner is not going to converge on its own.
pub const MAX_INVALID_POSTS: u32 = 4;

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
        // 3. `agent-ready`: the plan becomes board rows.
        (PlanPosted, Boarding) => true,
        // 4. Every hold, from every live state.
        (Planning | PlanPosted | Boarding, Held) => true,
        // 5. `resume_plan_drive`, back into the state the hold came from.
        (Held, Planning) | (Held, PlanPosted) | (Held, Boarding) => true,
        // 6. `cancel_plan_drive`, or reconcile positively established the issue
        //    is closed. From any non-terminal, `held` included: cancelling is a
        //    parked drive's second way out.
        (Planning | PlanPosted | Boarding | Held, Cancelled) => true,
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
    /// `driver.plan_review_minutes`. **P3a stores it and spends it on
    /// nothing** — the review window is P3b's — and it lands here in P3a so
    /// that the repo key, the record and the status view are one contract
    /// rather than three separate landings.
    /// `the_declared_review_window_moves_no_arc_in_this_build` pins that it
    /// changes no decision here, which is a counterfactual a later slice can
    /// invert rather than a promise nothing checks.
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
    /// Slice id -> board task id. **P3b fills this**; P3a persists the field so
    /// that the record's shape does not change under a running fleet.
    #[serde(default)]
    pub slice_tasks: BTreeMap<String, String>,
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
            slice_tasks: BTreeMap::new(),
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
/// 5. **The whole-drive backstop is last**, so a drive that has just moved is
///    never parked for being old on the tick it did something.
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
            Consent::Investigation => Some(PdStep::to(Boarding)),
            Consent::Ready => Some(PdStep::to(Boarding)),
        },
        // P3a has no executor. A named, audited, notice-bearing park — never a
        // tick that quietly does nothing. See the module doc.
        Boarding => Some(PdStep::held(PdHeldReason::AwaitingP3b)),
        // Unreachable: `is_live` excluded all three above. Spelled out rather
        // than caught by a `_` arm so that a seventh state cannot land here
        // silently.
        Complete | Cancelled | Held => None,
    };
    if step.is_some() {
        return step;
    }
    // 5.
    if entry.age_ms(facts.now_ms) >= limits.drive_timeout_ms() {
        return Some(PdStep::held(PdHeldReason::DriveStalled));
    }
    None
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
    pub const ALL: [&str; 13] = [
        STARTED,
        REFUSED,
        PLANNER_SPAWNED,
        PLAN_INVALID,
        PLAN_POSTED,
        PLANNER_CONSUMED,
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

    /// Whether a refusal names an **orrerix fault** rather than a policy
    /// decision — the distinction `queue_merge`'s contract uses capitals to
    /// make.
    pub fn is_orrerix_fault(reason: &str) -> bool {
        matches!(reason, STATE_UNREADABLE | STATE_UNWRITABLE | UNAVAILABLE)
    }

    /// Every name above, so a test can assert the set rather than iterate a
    /// list someone has to remember to extend.
    pub const ALL: [&str; 12] = [
        DRIVER_DISABLED,
        ISSUE_NOT_OPEN,
        ISSUE_UNVERIFIABLE,
        NOT_LABELLED,
        ALREADY_DRIVEN,
        NO_PLANNER_BLOCK,
        PLANNER_UNSPAWNABLE,
        NOT_DRIVEN,
        NOT_HELD,
        STATE_UNREADABLE,
        STATE_UNWRITABLE,
        UNAVAILABLE,
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
