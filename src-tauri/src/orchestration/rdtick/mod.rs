//! The review-loop driver's registry wiring (#1778 S3).
//!
//! Design note: `docs/design/review-driver.md`. Every decision this file makes
//! is made somewhere else: the state machine is [`reviewdrive::decide`], the
//! gate is [`mergeq::recheck_gate`], the `gh` reads and the notice text are
//! [`rddrive`]. What lives here is what only the registry can do — resolve the
//! policy, the gate and the verdict files, hold the state lock across the
//! read-modify-write, perform the spawns and resumes, emit the audit events,
//! and deliver the notices.
//!
//! # Why its own file rather than more of `mod.rs`
//!
//! Size is the smaller reason. The larger one is that §3.1 item 1's source
//! scan needs a scope, and CLAUDE.md's source-scanning-guard convention
//! forbids deciding one from a binding's NAME — the design note says so about
//! this scan in particular: "any scope keyed on a name — a module, an `rd_*`
//! prefix — is stepped over by a landing verb added in a function that does
//! not carry it". A **file** is not a name. Every landing verb the driver
//! could reach has to be written down somewhere, and this is the somewhere, so
//! `tests/reviewdrive.rs` can default-deny the whole of it and a rename cannot
//! move code out from under the guard.
//!
//! The residual is stated where the scan is implemented, not here, and it is
//! the one the note names: a landing verb the driver reaches through a SHARED
//! helper it does not own. [`rddrive::RdRunner`] closes the `git` half of that
//! structurally — it has no `git` method — and nothing closes the `gh` half but
//! the scan.

use std::sync::Arc;

use serde_json::{json, Value};

use super::{
    brand, manager_block_refusal, mergeq, mqloop, notify, now_ms, orchestrator_block_refusal,
    pr_number, rddrive, render_template, resolve_session_ref, resolve_worker_resume_cwd,
    reviewdrive, tail_snippet, unknown_block_refusal, workflow, AgentEntry, AgentStatus, Delivery,
    GroupId, LockExt, OrchRegistry, Role, TaskPatch,
};

use super::{is_live_cap_refusal, load_active_workflow, Guardrails};

// ---------- the review driver's registry-side types (#1778 S3) ----------

/// The three brief templates (§5.5), embedded like every other built-in.
///
/// **New files, so they are not `pre222` fixtures** — that set pins the four
/// role templates byte-for-byte. These are pinned instead by the goldens and the
/// key-set assertion §5.5 prescribes, over the *rendered* output rather than the
/// source, because the rendered text is what a reviewer receives.
pub const DRIVER_REVIEW_TPL: &str = include_str!("../templates/driver-review.md");
pub const DRIVER_DELTA_TPL: &str = include_str!("../templates/driver-delta.md");
pub const DRIVER_FIX_TPL: &str = include_str!("../templates/driver-fix.md");

/// Every value the driver interpolates into a brief, scrubbed (§5.5).
///
/// **Applied at the render call site, not inside a test harness**, which §5.5
/// makes the difference between a pin and a decoration: "a test that sanitizes
/// inside its own render harness asserts only that the two functions compose,
/// and passes identically while the live call site hands `render_template` a raw
/// job name".
///
/// **Every value, not just the two that are author-controlled.** A failed job
/// name comes from a `.github/workflows` file on the PR branch and a changed
/// path is whatever the pusher chose, so those two are the ones §5.5 names — but
/// a rule applied only to the fields someone remembered to classify is a rule
/// exactly the width of that memory, and the cost of scrubbing a block id
/// orrerix minted itself is nothing.
///
/// `Lines::Collapse` rather than `Keep`: a brief is a prompt typed into a pane,
/// and a value that could open a line of its own is a value that could open a
/// line looking like an instruction. The verdict-summary path keeps newlines
/// because a reviewer's prose is the payload there; here every value is a
/// single token or a list.
fn rd_fact(s: &str) -> String {
    notify::sanitize_pane_text(s, RD_FACT_CAP, notify::Lines::Collapse)
}

/// How long one interpolated fact may be. A brief is a prompt, and a prompt is
/// the delegate's resident context — the same cost argument §6 makes for
/// notices. Long enough for a changed-file list on a real PR, short enough that
/// a pathological one cannot become the whole brief.
const RD_FACT_CAP: usize = 2_000;

/// What one driven delegate did, as the interception arms report it (§7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RdEvent {
    /// `report(done)` from the driven worker.
    WorkerDone,
    /// `report(blocked)` from the driven worker.
    WorkerBlocked,
    /// `report(progress)` from the driven worker's CURRENT pane (#1959).
    ///
    /// **It is not a drive signal and must never become one.** A drive advances
    /// on the head, the checks and the verdict files, and a worker saying it is
    /// still going is none of those — reading it as "the fix is in" would brief a
    /// reviewer over unfinished work. What it IS is evidence the worker thinks
    /// it has finished and has said so in the wrong word, which is what the
    /// dogfood measured: a body-only fix (nothing to push, no new checks) whose
    /// worker read "push, and report when the checks are green" literally and
    /// picked `progress`. The drive consumed it and did nothing for ten minutes,
    /// until the idle watchdog woke the ORCHESTRATOR — the turn the driver exists
    /// to remove. So the tick answers it in the worker's own pane instead, once
    /// per hand-back, and the drive does not move.
    WorkerProgress,
    /// `message_orchestrator` from any driven delegate. **Never intercepted** —
    /// the delegate's own line is delivered unchanged by its own arm; this is
    /// only the routing fact beside it (§7). Carries WHICH delegate, because
    /// that is the fact the hold exists to supply.
    Messaged { by: String },
    /// `review_verdict` from a driven lane. Carries nothing: the verdict FILE is
    /// what the next tick reads, from the same parser the gate reads, so a
    /// signal carrying the word would be a second source for one fact.
    Verdict,
}

/// A driven PR's pending delegate signals, between an event and the tick that
/// acts on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RdSignal {
    pub worker: reviewdrive::WorkerSignal,
    /// The driven worker's current pane called `report(progress)` (#1959).
    ///
    /// Deliberately NOT folded into `worker`: [`reviewdrive::WorkerSignal`] is
    /// what `decide` turns on, and this must not be able to move a drive. It is
    /// read by the tick alone, to answer the worker in its own pane.
    pub worker_progress: bool,
    pub messaged: bool,
    /// WHICH delegate called `message_orchestrator`.
    ///
    /// §2.2 says every hold names "the one fact that decides what the
    /// orchestrator does next", and for `held(messaged)` that fact is which
    /// delegate spoke: its own line is already in the pane, unchanged, and the
    /// hold is the routing fact BESIDE it — a hold that named nobody would leave
    /// the orchestrator correlating two lines by timing.
    pub messaged_by: String,
}

impl Default for RdSignal {
    fn default() -> RdSignal {
        RdSignal {
            worker: reviewdrive::WorkerSignal::Silent,
            worker_progress: false,
            messaged: false,
            messaged_by: String::new(),
        }
    }
}

/// The facts one entry's briefs and notices are rendered from — read once, at
/// the top of the step, so a brief and the notice beside it cannot disagree
/// about what this tick saw.
struct RdBrief {
    pr: u64,
    head: String,
    base: String,
    body_digest: String,
    ci: reviewdrive::CiObservation,
    failing_jobs: Vec<String>,
    /// The gate's required lanes at this head, in `RoutingDecision::required`'s
    /// order — the static `reviewers:` list first, then the fired rules in
    /// declaration order. Lane *k+1* opens only after lane *k* passes, which is
    /// how §4's sequenced-lane rule is expressed with no block name in the code.
    required: Vec<String>,
    lane_notices: Vec<rddrive::LaneNotice>,
    /// The lane `review-wait` is actually acting on this tick — the first whose
    /// `pass` does not stand at this revision, which is the index
    /// `decide_review_wait` itself walks to.
    ///
    /// **Carried rather than re-derived from the verdicts**, because the lane a
    /// hold is ABOUT is not always a lane that has spoken. `lane-stalled` is the
    /// case that proves it: the stalled lane has recorded nothing, so it is
    /// absent from `lane_notices` entirely, and picking "the last lane with a
    /// verdict" names a *different, passing* lane and its pane — in the one
    /// notice whose whole job §2.2 says is to name the pane.
    deciding_lane: Option<String>,
}

impl RdBrief {
    /// The digest as `open_lane` wants it: `None` when the body could not be
    /// read, which `lane_open_for` reads as "cannot tell" rather than as drift.
    fn body_digest_opt(&self) -> Option<&str> {
        (!self.body_digest.is_empty()).then_some(self.body_digest.as_str())
    }

    /// Which of the three hand-back shapes this is, for the audit line.
    fn handback_kind(&self) -> &'static str {
        match self.ci {
            reviewdrive::CiObservation::Conflicting => rddrive::handback_why::CONFLICT,
            reviewdrive::CiObservation::Red => rddrive::handback_why::CI_RED,
            _ => rddrive::handback_why::REVIEW_FINDINGS,
        }
    }

    /// The lane that recorded `fail`, or empty — what a findings hand-back and a
    /// `review-limit` hold both name.
    fn failing_lane(&self) -> String {
        self.lane_notices
            .iter()
            .find(|l| l.verdict == workflow::Verdict::Fail)
            .map(|l| l.block.clone())
            .unwrap_or_default()
    }

    /// The lane that recorded `escalate`, or the failing one, or empty.
    fn speaking_lane(&self) -> Option<&rddrive::LaneNotice> {
        self.lane_notices
            .iter()
            .find(|l| l.verdict != workflow::Verdict::Pass)
            .or_else(|| self.lane_notices.last())
    }

    /// The notice inputs for a hold (§6).
    ///
    /// **The lane a hold is about is the DECIDING lane, not the last one that
    /// spoke.** For `escalate` and `review-limit` the two coincide, because the
    /// deciding lane is the one whose verdict caused the hold. For
    /// `lane-stalled` they do not and cannot: that lane has recorded nothing, so
    /// it is absent from `lane_notices`, and naming the last lane that answered
    /// would put a passing lane's block and pane into a notice about a stalled
    /// one. The summary still comes from whichever lane actually spoke, because
    /// there is no summary to quote from a lane that did not.
    fn held_facts(
        &self,
        entry: &reviewdrive::DriveEntry,
        limits: &reviewdrive::DriveLimits,
        reason: reviewdrive::HeldReason,
        messaged_by: &str,
        refusal: &str,
        provider: &str,
    ) -> rddrive::HeldFacts {
        let speaking = self.speaking_lane();
        let lane = self
            .deciding_lane
            .clone()
            .or_else(|| speaking.map(|l| l.block.clone()))
            .unwrap_or_else(|| self.required.get(entry.lane_index).cloned().unwrap_or_default());
        rddrive::HeldFacts {
            head: entry.head.clone(),
            worker_session: entry.worker_session.clone(),
            lane_agent: entry.lane(&lane).map(|l| l.agent.clone()).unwrap_or_default(),
            lane_summary: speaking.map(|l| l.summary.clone()).unwrap_or_default(),
            messaged_by: messaged_by.to_string(),
            refusal: refusal.to_string(),
            provider: provider.to_string(),
            panes: entry.owned_panes(),
            lane,
            counters: entry.counters.clone(),
            max_review_rounds: limits.max_review_rounds,
            max_ci_attempts: limits.max_ci_attempts,
            failing_jobs: self.failing_jobs.clone(),
            // **Read off the entry AFTER the arc, which is the only place
            // they exist** (#2110). `advance` stamps `held_from` and
            // `held_after_ms` from the clock it is about to reset, so a
            // reader here recomputing them would report the age of the hold
            // rather than the wait that caused it — and this function is
            // called from the `Held` arm below, after `entry.take`.
            held_state: entry.held_from,
            held_state_ms: entry.held_after_ms,
            // **The bound named is the one that FIRED, and the two holds
            // fired on different ones.** `state-stalled` is this state's
            // bound; `drive-stalled` is the drive's total, and printing the
            // state's figure there would be a notice quoting a number that
            // did not decide anything. Every other reason has no time bound
            // behind it, and its notice reads none of this.
            held_bound_ms: match reason {
                reviewdrive::HeldReason::StateStalled => entry
                    .held_from
                    .and_then(|st| {
                        reviewdrive::state_bound_ms(st, limits, self.required.len())
                    })
                    .unwrap_or(0),
                reviewdrive::HeldReason::DriveStalled => {
                    limits.drive_timeout_minutes.saturating_mul(60_000)
                }
                _ => 0,
            },
        }
    }
}

/// What opening one reviewer lane produced (#2109).
///
/// A struct rather than the bare agent id it used to be, because `resumed` is
/// the fact #2109 is about and nothing downstream can re-derive it: a resumed
/// lane and a fresh one hand back the same shape of pane id, and the audit row
/// they produce was identical. `session` travels with it so the row names the
/// conversation rather than only the pane, which is what a reader chasing "did
/// this reviewer keep its context" actually needs.
struct RdLaneOpen {
    agent: String,
    session: String,
    /// Whether this lane was opened by RESUMING the session it already had, as
    /// opposed to on a fresh conversation. False includes the fall-through case
    /// — a resume that was attempted and refused — which is why
    /// `rd-lane-resume-failed` is a separate row rather than a flag here: this
    /// says what happened, that says what was tried.
    resumed: bool,
    /// The round's scope line exactly as the brief rendered it (#2508) — the
    /// one fact about the round that nothing else on the row records, so a
    /// before/after count of whole-diff rounds does not re-derive it. Computed
    /// BEFORE `open_lane` writes the record, the same point the brief itself
    /// sampled, and threaded out for that reason: re-deriving it after the
    /// record was written would read `at_head == head` and answer
    /// `body-only` for every round.
    scope: String,
}


/// **What a restart cost ONE drive, decided where the answer is not ambiguous**
/// (#2811 S10, #3225, #3226).
///
/// Every field is a fact about a PROCESS boundary, and each licenses exactly one
/// recovery the ordinary tick cannot reach on its own. The mark is per
/// `(group, pr)`, in memory, and each field is discharged by the tick that ACTED
/// on it — see the spend sites in `rd_step_entry`, and `rd_forget_restart_mark`
/// for why the whole entry is dropped wherever the drive it describes is.
///
/// It is one struct rather than three maps because the three are one question:
/// what did this drive lose when the panes died. A reader chasing a drive that
/// came back up looks in one place, and a site that clears one field cannot
/// silently clear the other two — which a single `HashSet` keyed on the PR made
/// easy to do by accident.
#[derive(Clone, Debug, Default)]
pub(crate) struct RestartMark {
    /// #2811 S10: `fix-wait`, so the worker is re-briefed (`why: restart`).
    pub(crate) handback: bool,
    /// #3225: `ci-wait` after a push whose worker pane is gone, so the push is
    /// read as the fix delivered rather than waited on.
    pub(crate) push_delivered: bool,
    /// #3226: the lane blocks whose panes died, so each re-brief says
    /// `why: restart` on its `rd-lane-spawned` row. Re-briefing them is the
    /// *reseed's* doing, not this field's — the record no longer names a head
    /// they were briefed at, so `decide_review_wait` opens them by the ordinary
    /// path. What this carries is only why.
    pub(crate) lanes: Vec<String>,
}

impl RestartMark {
    /// Nothing left to say — the key is dropped rather than kept as an empty
    /// record, so `contains` and `is_empty` cannot disagree.
    fn is_empty(&self) -> bool {
        !self.handback && !self.push_delivered && self.lanes.is_empty()
    }
}

/// The panes a drive owned that are **not in the live roster**, dropped from its
/// record (#3225, #3226).
///
/// The sessions survive; only the panes are un-owned. That is the whole of the
/// second half of #3225: `held(fix-stalled)`'s notice enumerates
/// [`reviewdrive::DriveEntry::owned_panes`] as "still OWNED", and after a restart
/// every one of them named a pane that died with the previous process — which
/// sent an orchestrator to look at panes that were not there.
#[derive(Debug, Default)]
struct RdLostPanes {
    /// Worker panes dropped, current first-and-only (the priors are dropped by
    /// `forget_dead_panes` and counted in `superseded`).
    worker: Vec<String>,
    /// `(block, pane)` per lane whose current pane was dropped.
    lanes: Vec<(String, String)>,
    /// Whether any SUPERSEDED pane was dropped as well.
    superseded: bool,
}

impl RdLostPanes {
    /// **Did the RECORD change** — the persistence question, so the superseded
    /// lists count.
    fn any(&self) -> bool {
        !self.worker.is_empty() || !self.lanes.is_empty() || self.superseded
    }

    /// **Was a pane this drive is still USING lost** — the recovery question,
    /// and the superseded lists deliberately do not count (#3228 review 2, W1).
    ///
    /// The two are different questions and conflating them was a live defect. A
    /// drive between a pane replacement and the next tick's own
    /// [`reviewdrive::DriveEntry::forget_dead_panes`] prune has a dead pane on
    /// `prior_worker_agents` as its ORDINARY state — nothing is wrong, nothing
    /// was lost, and the next tick tidies it. Gating the `drive_review` repair
    /// on `any()` therefore took an ordinary duplicate call into the repair arm,
    /// which on a `fix-wait` drive mints `RestartMark { handback: true }` and
    /// re-briefs a LIVE worker mid-fix with `why: restart` — a paid turn and a
    /// duplicated brief on a drive that lost nothing.
    ///
    /// A superseded pane is not one the drive would ever speak to again, so
    /// nothing about it is recoverable: what it is owed is the prune it already
    /// gets on the tick.
    fn current_panes_lost(&self) -> bool {
        !self.worker.is_empty() || !self.lanes.is_empty()
    }
}
/// What one entry's step produced, for the caller to emit outside the lock.
#[derive(Debug, Default)]
struct RdOut {
    pr: u64,
    changed: bool,
    backoff: bool,
    clear_signal: bool,
    on_behalf_of: String,
    advanced: Option<(reviewdrive::DriveState, Option<reviewdrive::HeldReason>)>,
    lanes_opened: Vec<(String, String)>,
    handback: Option<String>,
    /// `(agent, text)` for a kick-back this tick owes the WORKER's own pane
    /// (#1959). Carried out of the step like [`RdOut::notices`], and for the
    /// same reason those are: it is the PRODUCT of a step that is over, not a
    /// part of one, so there is nothing for the state lock to protect while it
    /// is sent.
    ///
    /// Not because a delivery under `rd_state_lock` would deadlock — it would
    /// not, and `rd_reuse_pane` performs one there deliberately. See
    /// [`OrchRegistry::rd_drive_group_with`] for the property that makes both
    /// safe, and why "span the spawn" and #467/#468 do not actually conflict.
    kickback: Option<(String, String)>,
    /// `(role, pane)` for every pane this tick RELEASED (#2501) — reported, not
    /// performed here: the kill and the record write both happen under the state
    /// lock in [`OrchRegistry::rd_step_entry`], because un-recording a pane the
    /// registry has not yet marked dead would leave it live and unowned, which
    /// is the §7 leak this record exists to close. What travels out is the fact,
    /// for [`RdDriveReport`].
    releases: Vec<(reviewdrive::DrivenRole, String)>,
    audits: Vec<(&'static str, Value)>,
    notices: Vec<String>,
    /// #2811 S5b: set when THIS entry held on a provider limit, to the
    /// provider id. The tick aggregates these into ONE notice rather than
    /// letting each drive push its own — a provider limit is one cause with
    /// one remedy that stops N drives at once, and N identical lines would be
    /// N times the orchestrator's attention for one action.
    provider_limited: Option<String>,
    /// This drive's OWN hold wording (#2811 S5b) — written to its board task
    /// so the row says why this PR stopped, while the orchestrator's pane
    /// gets the aggregated line instead. `None` unless this entry held on a
    /// provider limit and `announce_hold` admitted the line.
    provider_note: Option<String>,
    /// What refused, when this tick's hold is about a refusal (#1961) — the
    /// spawn error, or the line the resumed pane exited on. Empty otherwise,
    /// which renders as no clause; see [`rddrive::HeldFacts::refusal`].
    refusal: String,
    /// **A CLEAN satisfied gate the driver must submit to the merge queue**
    /// (#3367 item 5) — the head it was clean at, or `None`.
    ///
    /// Acted on by [`OrchRegistry::rd_drive_group_with`] AFTER this tick's
    /// write and outside `rd_state_lock`, never here: `queue_merge_with`
    /// takes `mq_state_lock`, re-reads `review_drives.json` for §8.1's mutual
    /// refusal (so it must see this drive already terminal, which only the
    /// write makes true), and spends `gh` round trips — none of which belongs
    /// under the driver's own state lock.
    clean_enqueue: Option<String>,
    /// **This tick's hold notice carries the drive's `auto_report`, which is
    /// still on the entry** (#3367 round-3 residual). The delivery loop clears
    /// it once every notice in `notices` landed, and leaves it otherwise — so
    /// a lost hold line costs a repeat of the report on the next notice, never
    /// the report itself.
    report_folded: bool,
    /// **The terminal notice this tick owed, as text** (#3388 review round 1)
    /// — kept for the one path on which the owed copy never reaches a pane: a
    /// tick whose `store_state` FAILED. The flush reads owed notices from
    /// disk, and this one never got there, so [`OrchRegistry::rd_drive_group_with`]
    /// delivers it directly instead, marked [`rddrive::UNRECORDED_SUFFIX`].
    owed_text: Option<String>,
}

impl RdOut {
    fn new(pr: u64) -> RdOut {
        RdOut { pr, ..RdOut::default() }
    }
}

/// What one pass of the owed-notice flush did (#1857) — see
/// [`OrchRegistry::rd_flush_notices`].
#[derive(Debug, Default)]
struct RdFlush {
    /// Owed notices that reached the orchestrator's pane this pass.
    notices: Vec<String>,
    /// PRs whose notice is still owed when the pass ends.
    undelivered: Vec<u64>,
    /// Entries §5.2's retention dropped.
    pruned: Vec<u64>,
    /// `(pr, notice)` for the entries dropped at the retention **ceiling** with
    /// the notice never delivered. The text is here so the caller can put it on
    /// the audit log, which is then the only record of it.
    dropped: Vec<(u64, String)>,
}

/// What one wake of the review driver did, for a test that needs to assert the
/// production path ran rather than infer it from side effects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RdDriveReport {
    /// `(pr, state)` for every entry that took an arc this tick.
    pub advanced: Vec<(u64, reviewdrive::DriveState)>,
    /// `(pr, block, agent)` for every lane briefed this tick.
    ///
    /// The agent id is here and not only the block because it is the only handle
    /// a caller has on the pane that was actually opened — `review_drive_status`
    /// deliberately does not publish one (it is a compaction-recovery surface,
    /// and a pane id is not what an orchestrator routes on), so without it
    /// nothing outside this module can ask where a driver-spawned lane landed.
    pub lanes_opened: Vec<(u64, String, String)>,
    /// `(pr, agent)` for every worker resumed with a hand-back this tick — the
    /// same handle `lanes_opened` carries for a lane, and for the same reason:
    /// it is the only way a caller can ask what the worker was actually told.
    pub handbacks: Vec<(u64, String)>,
    /// `(pr, agent)` for every kick-back this tick typed into a WORKER's own
    /// pane (#1959) — the answer to a `report(progress)` in `fix-wait`.
    ///
    /// **Attempted, not delivered**, for [`notices`](RdDriveReport::notices)'
    /// reason and with the same honesty: the entry is marked before the
    /// delivery, so a pane that could not be reached costs the line rather than
    /// re-emitting on every later tick.
    pub kickbacks: Vec<(u64, String)>,
    /// `(pr, role, pane)` for every pane this tick released (#2501).
    ///
    /// **Performed, not attempted** — the opposite of `kickbacks` above, and the
    /// difference is real rather than a wording preference. A release is only
    /// recorded once `release_driven_pane` has answered `Ok`, which means the
    /// registry won the live→dead transition, so a row here says the slot is
    /// free and the drive's record no longer names that pane. A pane the barrier
    /// refused — still working, already gone, never bound to a terminal — is not
    /// here and is not on the audit log either, because nothing happened.
    pub released: Vec<(u64, reviewdrive::DrivenRole, String)>,
    /// The kick-back notices this tick **produced and attempted** — a hold's,
    /// and every terminal exit's.
    ///
    /// **Attempted, not delivered**, and the wording is the correction rather
    /// than a hedge: this field's doc used to say "delivered to the
    /// orchestrator" while being pushed regardless of what
    /// `deliver_to_orchestrator` answered, which was a claim the code did not
    /// make (#1857). Whether a terminal exit's notice actually landed is
    /// [`notice_undelivered`](RdDriveReport::notice_undelivered)'s question, and
    /// it is the one with a consequence — a notice still owed keeps its entry.
    ///
    /// It stays "attempted" rather than being narrowed to `Ok` deliveries
    /// because the two are not distinguishable from a caller's side for a HOLD,
    /// whose notice is not owed on the entry and has no retry: reporting only
    /// the ones that landed would silently drop a hold's notice out of this
    /// field with nowhere else for it to appear.
    pub notices: Vec<String>,
    /// PRs whose terminal notice is **still owed** at the end of this tick — the
    /// delivery was attempted and failed, so the entry is retained and a later
    /// tick tries again (#1857).
    pub notice_undelivered: Vec<u64>,
    /// Terminal entries dropped after their notices went out (§5.2), plus the
    /// ones given up on at the retention ceiling — which are audited
    /// `rd-notice-dropped` beside their `rd-pruned`.
    pub pruned: Vec<u64>,
    /// Whether this group was backed off `RD_BACKOFF_MS` rather than merely
    /// taking its turn in the rotation.
    pub backoff: bool,
    /// Set when the tick refused outright — a `review_drives.json` this build
    /// will not read (§2.4). **Not** the same as "nothing was driven".
    pub refused: Option<&'static str>,
}

/// The round's scope line for one lane brief (#2508) — `scope: whole-diff` |
/// `scope: delta since <prev head>` | `scope: body-only` — the machine-visible
/// trigger the rev-std persona's Rule 6 acts on.
///
/// **Derived from the same facts the brief's own arms read, and nothing else.**
/// `verify` decides first — a verification round reads the body as it stands,
/// which is `body-only` by definition, and the grant can arrive on a lane with
/// no record (the undriven-to-driven transition, #2308 W1), so the lane history
/// cannot be asked before it. Otherwise `entry.lane(block)` is the lane's own
/// history, sampled BEFORE `open_lane` writes the record, exactly as
/// `rd_lane_brief`'s template-arm `match` does — a lane with no record has
/// never measured this PR, so it gets the whole diff whatever the round counter
/// says (round 1 is always that lane state, which is why round 1 never says
/// delta: structurally, there is no previous revision to name). A re-brief then
/// follows the delta arm's own split: an unchanged head is a body-only round,
/// and a moved head names the revision it is a delta from.
///
/// The three values are a closed set, and the line is rendered into the brief
/// AND carried on the `rd-lane-spawned` audit row, so a reader counting
/// whole-diff rounds for a beta's before/after does not re-derive them from
/// round numbers and head moves.
fn rd_lane_scope(
    entry: &reviewdrive::DriveEntry,
    block: &str,
    brief: &RdBrief,
    verify: bool,
) -> String {
    if verify {
        return "scope: body-only".to_string();
    }
    match entry.lane(block).filter(|l| !l.at_head.is_empty()) {
        Some(rec) if rec.at_head == brief.head => "scope: body-only".to_string(),
        Some(rec) => format!("scope: delta since {}", rd_fact(&rec.at_head)),
        None => "scope: whole-diff".to_string(),
    }
}

mod events;
mod handback;
mod lanes;
mod policy;
mod tick;

impl OrchRegistry {
    /// Render one lane's brief (§5.5), **sanitizing every interpolated value at
    /// this call site**.
    ///
    /// §5.5 makes that placement the difference between a pin and a decoration:
    /// "a test that sanitizes inside its own render harness asserts only that
    /// the two functions compose, and passes identically while the live call
    /// site hands `render_template` a raw job name". So [`rd_fact`] wraps every
    /// value here, and the hostile-value test calls this function.
    fn rd_lane_brief(
        &self,
        entry: &reviewdrive::DriveEntry,
        block: &str,
        brief: &RdBrief,
        limits: &reviewdrive::DriveLimits,
        verify: bool,
        scope: &str,
    ) -> String {
        let round = entry.counters.review_rounds.saturating_add(1).to_string();
        let max = limits.max_review_rounds.to_string();
        let head = rd_fact(&brief.head);
        let pr = brief.pr.to_string();
        // **The CI line states what this tick OBSERVED**, and it is rendered
        // rather than asserted because the driver had the fact in hand and the
        // templates were claiming green unconditionally.
        //
        // A lane is normally briefed out of `review-wait`, which `ci-wait`
        // reaches only on green — but arc 8 moves `fix-wait -> review-wait` on a
        // worker's `report(done)` at an unchanged head WITHOUT consulting
        // `facts.ci`, which is the "that failure was unrelated" turn. A brief
        // that told the reviewer the checks were green there was stating as fact
        // something this tick had just read as false. It cannot produce an
        // unsafe landing — `gate-check` re-evaluates `ci-green` through
        // `recheck_gate` — so it costs a misled reviewer and a wasted round,
        // which is exactly what a driven review is for saving.
        // **One paragraph, one line each.** A backslash-n plus the source
        // indent ships both into a reviewer's brief, and a `.contains` of any
        // single fragment passes straight over it because no asserted substring
        // straddles the break. Written without continuations at all so there is
        // nothing to collapse, and the SHAPE is pinned beside the content in
        // `a_lane_brief_is_one_paragraph_per_sentence`.
        let ci = match brief.ci {
            reviewdrive::CiObservation::Green => "This PR's checks are green at that head.",
            reviewdrive::CiObservation::Red => "This PR's checks are RED at that head. Review the change on its merits; the failure is the worker's to answer.",
            // **Unreachable through `decide` since #2311, and kept anyway.**
            // Arc 8 was the one route that briefed a lane on a conflicting PR;
            // mergeability is now read above the per-state logic, so that tick
            // hands the worker back instead — which is the better trade, since
            // reviewing a PR that must be rebased anyway spends a paid round on
            // a revision that will not survive. The arm stays because the match
            // is over a closed enum and a future arc could reach `review-wait`
            // without consulting mergeability again; what stops it coming back
            // to life unnoticed is `a_conflicting_pr_briefs_no_lane_at_all`,
            // which performs the counterfactual rather than describing it.
            reviewdrive::CiObservation::Conflicting => "This PR does not merge cleanly at that head. Review the change on its merits; the conflict is the worker's to answer.",
            // Pending and Unknown share one sentence on purpose: §8 says unknown
            // is never reported as a fact about the PR, and not-green-yet is the
            // only thing true of both.
            reviewdrive::CiObservation::Pending | reviewdrive::CiObservation::Unknown => "This PR's checks are not green at that head (orrerix could not read a settled result).",
        };
        // **#2168 E2's paragraph is decided by `verify` and by nothing else**
        // (#2308 review round 3, W1). It used to live inside the delta arm
        // below, under a further `rec.at_head == brief.head` — so a lane with
        // NO record, or one that had been briefed and not yet answered, was
        // stamped `briefed_verify` by `open_lane` and handed the ordinary
        // first-round brief. That path is not exotic: it is the
        // undriven-to-driven transition, which is a PR reviewed by hand and
        // then handed to `drive_review` after a body edit — the exact
        // situation #2168 is about. `rd_open_lane` renders this BEFORE
        // `open_lane` writes the record, so the record can never be the thing
        // that decides what the brief says about the grant the record is
        // about.
        //
        // Rendered once, here, and interpolated into whichever template the
        // lane's history selects: the grant and the sentence announcing it are
        // then one decision by construction rather than two that agree today.
        // `a_verification_brief_announces_itself_on_every_path_that_grants_it`
        // walks {record, no record} x {verify, not} and asserts the marker is
        // present exactly when the grant is.
        //
        // **What it asserts is a fact about the VERDICTS, not about the drive's
        // bookkeeping.** `decide_review_wait` sets `verify` only when every
        // required lane has a `pass` bound to this head, which is read from the
        // verdict files; a lane record's `at_head` is what this drive last
        // observed and can lag. Gating the sentence on the latter made the
        // announcement depend on something the claim does not rest on.
        let verification = if verify {
            // Stated as fact because it IS that precondition. The reviewer is
            // told what its pass will be taken to MEAN, because the gate
            // accepts the other lanes' passes on the strength of it — a grant
            // said out loud in the brief rather than only in a design note.
            //
            // **What it asks for is deliberately repo-neutral.** orrerix has no
            // idea what checking a body's receipts costs here, or what tool
            // does it; it points the reviewer at the contributor docs, which is
            // where a repo says (constraint 8).
            " What moved: the head has not moved and the PR body has, and every \
             required lane has already passed the code at this head. This is a \
             VERIFICATION-ONLY round: read the body as it stands, not the diff. \
             Check what it asserts against the tree at this head — figures, run \
             ids, SHAs, quoted passages — plus whatever this repo's contributor \
             docs tell a reviewer to run over a PR body. Your pass records that \
             the body as it stands is sound, and the merge gate accepts the other \
             lanes' passes on the strength of it instead of re-briefing them, so \
             a body defect you wave through is one nobody else will look at. A \
             finding about the code is still in scope if you see one; you are not \
             being asked to go looking for one."
        } else {
            ""
        };
        match entry.lane(block).filter(|l| !l.at_head.is_empty()) {
            // A lane that has answered before gets the delta — the line an
            // orchestrator typed by hand nine times on one PR.
            Some(rec) => {
                let prev_head = rd_fact(&rec.at_head);
                let prev =
                    rec.last_verdict.map(|v| v.as_str()).unwrap_or("unrecorded").to_string();
                let digest_state = if rec.briefed_digest.is_empty() || brief.body_digest.is_empty()
                {
                    // "Cannot tell" is not "changed" — the asymmetry
                    // `ReviewVerdict::body_changed` encodes.
                    "of unknown drift (orrerix could not compare the two digests)"
                } else if rec.briefed_digest == brief.body_digest {
                    "unchanged"
                } else {
                    "changed"
                };
                // **The mode is `scope`'s, not re-derived here** (#2508 review,
                // rev-std finding 2): `rd_lane_scope` already classified this
                // round from the same `(verify, rec.at_head, brief.head)` facts,
                // so the arm that picks the WHAT_MOVED text reads that line
                // instead of re-deriving `rec.at_head == brief.head` beside it —
                // two parallel matches over one predicate were one edit away
                // from a scope line contradicting the brief it rides on. In
                // this arm `scope` is `delta since …` or `body-only` by
                // construction (`whole-diff` implies no lane record, and the
                // `None` arm below renders the first-call template, which has
                // no WHAT_MOVED slot); the final `else` is the unchanged-head
                // body-only round, and the scope-mode test pins each mode to
                // its text so a future drift between the two reads goes red.
                let moved = if verify {
                    // The grant outranks the record: see the block above the
                    // `match`. `verification` opens with a space so it reads as
                    // an appended clause in the other arm; this slot wants it
                    // without one.
                    verification.trim_start().to_string()
                } else if scope.starts_with("scope: delta since ") {
                    // **What this brief does NOT claim.** orrerix does not
                    // compute the per-round delta: the driver's seam is
                    // `gh`-only by construction (§3.1 item 1, made structural in
                    // `RdRunner`), so it has no `git diff` to run. It names the
                    // two revisions and points at the command that answers the
                    // question exactly — facts it read plus an instruction,
                    // rather than a delta it invented.
                    format!(
                        "What moved: the head moved from {prev_head} to {head}. orrerix does not \
                         compute the per-round delta; `git diff {prev_head}..{head}` in your \
                         worktree does."
                    )
                } else {
                    // **What this half says changed at #2168 E1.** Until then
                    // the commonest cause of a body-only re-brief was the
                    // worker pasting its CI receipts after the checks settled —
                    // #1875's class, one re-record round on every code PR of
                    // that session — and a reviewer's right move was to skim.
                    // `decide_ci_wait` no longer briefs a lane at a head its
                    // worker pushed until that worker has reported the fix
                    // finished, so what reaches here after a hand-back is an
                    // edit somebody made on purpose, and the right move is to
                    // read it.
                    //
                    // **Scoped to "after a hand-back", which is the whole of
                    // what is provable here.** E1 gates the `ci-wait` arc on
                    // arc 7, so the claim holds for every revision this drive
                    // handed back; it does NOT hold for the drive's first pass
                    // over a head it never handed back, where the receipts race
                    // is unchanged. A flat sentence would be the wider claim,
                    // and this brief lands in a reviewer's pane as fact.
                    "What moved: the head has not moved and the PR body has. Re-read the \
                     body, not the diff — the body is what a squash merge commits, so text \
                     that moved there is text nobody has passed. After a hand-back the \
                     driver waits for the worker's report(done) before opening this lane, \
                     so a body move you see following one is a deliberate edit rather than \
                     CI receipts landing late."
                        .to_string()
                };
                render_template(
                    DRIVER_DELTA_TPL,
                    &[
                        ("PR", &pr),
                        ("HEAD", &head),
                        ("PREV_VERDICT", &rd_fact(&prev)),
                        ("PREV_HEAD", &prev_head),
                        ("PREV_DIGEST_STATE", digest_state),
                        ("WHAT_MOVED", &moved),
                        ("CI", ci),
                        ("ROUND", &round),
                        ("MAX_ROUNDS", &max),
                        ("SCOPE", scope),
                    ],
                )
            }
            None => {
                let prior: Vec<String> = brief
                    .lane_notices
                    .iter()
                    .take_while(|l| l.block != block)
                    .map(|l| {
                        format!(
                            "{} recorded {}",
                            rd_fact(&l.block),
                            l.verdict.as_str().to_uppercase()
                        )
                    })
                    .collect();
                let prior = if prior.is_empty() {
                    String::new()
                } else {
                    // How a final lane learns it is validating a review as well
                    // as the work, with no block name anywhere in the code —
                    // §4's sequenced-lane rule expressed as an ordered list.
                    format!(" Lanes before yours at this revision: {}.", prior.join("; "))
                };
                render_template(
                    DRIVER_REVIEW_TPL,
                    &[
                        ("PR", &pr),
                        ("HEAD", &head),
                        ("BASE", &rd_fact(&brief.base)),
                        ("LANES", &rd_fact(&brief.required.join(", "))),
                        ("LANE", &rd_fact(block)),
                        ("CI", ci),
                        ("ROUND", &round),
                        ("MAX_ROUNDS", &max),
                        ("PRIOR_LANES", &prior),
                        // #2308 W1: the arm that used to grant in silence. A
                        // lane with no record — the undriven-to-driven
                        // transition — is briefed HERE, and before this it was
                        // stamped `briefed_verify` without ever being told.
                        ("VERIFICATION", verification),
                        ("SCOPE", scope),
                    ],
                )
            }
        }
    }

    /// Replace `from` with `to` in the notice PR `pr`'s entry still OWES
    /// (#3367 item 5) — the clean enqueue's answer, learned after the notice was
    /// owed, replacing the clause that promised it. A text that no longer
    /// carries `from` gets `to` appended, so the answer is never lost to a
    /// wording mismatch.
    ///
    /// Its own short critical section, re-reading the file, for the flush's
    /// reason: the tick's lock has been released, so the entry is read as it
    /// now is. Nothing owed (a concurrent flush already delivered it, or the
    /// entry is gone) changes nothing — the `rd-clean` row still carries the
    /// answer — and a failed read or write is the same: the notice goes out
    /// with the clause that promised the submission, which names
    /// `merge_queue_status()` and was true when written and when read.
    fn rd_amend_owed_notice(&self, dir: &std::path::Path, pr: u64, from: &str, to: &str) {
        let _state_guard = self.rd_state_lock.lock_safe();
        let Ok(mut state) = reviewdrive::load_state(dir) else { return };
        let Some(n) = state.entry_mut(pr).and_then(|e| e.owed_notice.as_mut()) else { return };
        if n.text.contains(from) {
            n.text = n.text.replacen(from, to, 1);
        } else {
            n.text.push_str(to);
        }
        let _ = reviewdrive::store_state(dir, &state);
    }

    /// **The drive's FIRST notice carries the report that started it** (#3367
    /// item 2). `Option::take`, so exactly one notice carries it and every
    /// later one reads as it always did. A drive nobody auto-started has
    /// nothing to take, and its notice is returned unchanged.
    fn rd_fold_auto_report(entry: &mut reviewdrive::DriveEntry, notice: String) -> String {
        let report = entry.auto_report.take();
        Self::rd_fold_text(notice, report.as_deref())
    }

    /// [`rd_fold_auto_report`](Self::rd_fold_auto_report)'s text, without the
    /// take — for the one caller that must not consume the report until its
    /// line is known to have landed (a hold, delivered directly).
    fn rd_fold_text(notice: String, report: Option<&str>) -> String {
        match report {
            Some(r) => format!(
                "{notice} This drive was started by a worker's report(done), delivered here \
                 instead of on its own: {r}"
            ),
            None => notice,
        }
    }

    /// Clear the `auto_report` a DELIVERED hold notice carried (#3367 round-3
    /// residual) — see [`RdOut::report_folded`]. Re-reads under the lock, as
    /// [`rd_amend_owed_notice`](Self::rd_amend_owed_notice) does; a failed
    /// read or write leaves the report on the entry, which costs a repeat on
    /// the next notice and never a loss.
    fn rd_clear_auto_report(&self, dir: &std::path::Path, pr: u64) {
        let _state_guard = self.rd_state_lock.lock_safe();
        let Ok(mut state) = reviewdrive::load_state(dir) else { return };
        let Some(e) = state.entry_mut(pr) else { return };
        if e.auto_report.take().is_some() {
            let _ = reviewdrive::store_state(dir, &state);
        }
    }

    /// The sentence #2509's grace round owes the worker it hands back to.
    ///
    /// **Because the `{{ATTEMPT}}` numbers cannot say it.** They are
    /// `review_rounds` of `max_review_rounds`, and a grace round is the one
    /// hand-back where `review_rounds` has already stopped at the bound — so
    /// the brief would otherwise read "attempt 3 of 3" for the second time in
    /// a row, with no account of why there is a second one. A worker that
    /// cannot tell a grace from a bug in the counter has been told something
    /// false by omission.
    ///
    /// It is also where the worker learns the bound is now REALLY spent: the
    /// next blocking fail parks the drive whatever it is about.
    fn grace_clause(grace: bool) -> &'static str {
        if grace {
            " This is a GRACE round PAST the review bound: the last blocking \
             fail came back on a lane that had been re-briefed about the PR \
             body alone, at a head that had not moved. The attempt count below \
             still reads at the bound, and that is not a mistake — a grace \
             round is not a review round. It is granted once per drive and is \
             now spent, so the next blocking fail parks the drive whatever it \
             is about."
        } else {
            ""
        }
    }

    /// **What a busy reviewer lane is told when its PR stops merging** (#3176).
    ///
    /// One paragraph on one source line, for [`Self::rd_lane_brief`]'s reason: a
    /// newline plus the source indent ships both into a delegate's pane, and a
    /// `.contains` of any single fragment steps straight over it. The shape is
    /// pinned beside the content, as the lane briefs' is.
    ///
    /// **It deliberately does NOT reuse `rd_lane_brief`'s CONFLICTING arm.**
    /// That sentence ends *"Review the change on its merits; the conflict is the
    /// worker's to answer"* — an instruction to CARRY ON, which is the opposite
    /// of this one, and gluing a stop onto it would hand a reviewer a paragraph
    /// that contradicts itself. What the two share is the FACT, not the wording.
    ///
    /// It asks for a report, and that is the mechanism rather than politeness: a
    /// reviewer's report is what stamps `idle_since_ms`, which is what
    /// `release_driven_pane`'s barrier requires — so this line is what makes the
    /// pane releasable at all. The release then happens on an ordinary later
    /// tick through [`reviewdrive::ReleaseReason::Conflict`], with no second
    /// mechanism and no second decision.
    ///
    /// And it says the conversation survives, because it does: the next round
    /// resumes this same session against the rebased head, so a reviewer that
    /// drops what it is holding loses nothing it will not be asked for again.
    fn rd_lane_stop_brief(&self, brief: &RdBrief) -> String {
        format!(
            "STOP this review — orrerix is standing it down. PR #{} does not merge cleanly against {} at the head you were briefed on ({}), so that head is about to be rebased away and any verdict recorded against it goes stale the moment the worker pushes. Do not finish the review, do not record a verdict for this head, and do not report findings: call report with outcome done, and stop there. Nothing is lost — orrerix briefs you again against the rebased head, in this same conversation, and no review round is charged for this one.",
            brief.pr,
            rd_fact(&brief.base),
            rd_fact(&brief.head),
        )
    }

    /// Render the worker's hand-back brief (§5.5).
    ///
    /// `{{WHAT}}` is **loomux-authored text chosen from a closed set of three**,
    /// with facts orrerix read interpolated into it — never delegate- or
    /// repo-authored prose (§3.1 item 4). The three are the three ways a PR
    /// comes back: a lane's findings, a red run, and a conflict.
    fn rd_fix_brief(
        &self,
        entry: &reviewdrive::DriveEntry,
        brief: &RdBrief,
        limits: &reviewdrive::DriveLimits,
        grace: bool,
    ) -> String {
        let base = rd_fact(&brief.base);
        // #3367 item 1: a non-blocking round is not a FAIL, and the brief must
        // not say one was recorded. Read off the ENTRY rather than the step, so
        // the brief a restart re-sends (`Rehandback`) says the same thing the
        // first one did. The attempt figures are the SHARED bound's, because
        // that is the budget this round spent.
        if entry.nit_handback {
            let what = format!(
                "Review: request-changes. Every required lane PASSED, with non-blocking \
                 findings open ({}). The findings are on PR #{}. Address all of them, or \
                 answer on the PR why one is not a defect, then push. This is non-blocking \
                 round {} of {} that the driver runs on its own, and it counts toward the \
                 review bound below.",
                rd_fact(&rddrive::residual_text(&brief.lane_notices)),
                brief.pr,
                entry.nit_rounds,
                limits.fix_nonblocking_rounds,
            );
            return render_template(
                DRIVER_FIX_TPL,
                &[
                    ("PR", &brief.pr.to_string()),
                    ("HEAD", &rd_fact(&brief.head)),
                    ("BASE", &base),
                    ("WHAT", &what),
                    ("ATTEMPT", &entry.counters.review_rounds.to_string()),
                    ("MAX_ATTEMPTS", &limits.max_review_rounds.to_string()),
                ],
            );
        }
        let (what, attempt, max) = match brief.ci {
            reviewdrive::CiObservation::Conflicting => (
                format!(
                    "It is CONFLICTING against {base}. Rebase onto origin/{base}, resolve, and \
                     push."
                ),
                entry.counters.rebase_attempts,
                limits.max_rebase_attempts,
            ),
            reviewdrive::CiObservation::Red => (
                format!(
                    "CI is red at that head. Failing checks: {}. Read them with `gh pr checks \
                     {}`, fix, and push.",
                    rd_fact(&brief.failing_jobs.join(", ")),
                    brief.pr
                ),
                entry.counters.ci_attempts,
                limits.max_ci_attempts,
            ),
            _ => (
                format!(
                    "Review requested changes: {} recorded FAIL. The findings are on the PR. \
                     Address all of them, or answer on the PR why one is not a defect, then \
                     push.",
                    // The DECIDING lane, which is the one `decide_review_wait`
                    // actually acted on — not "the first lane with a `fail`".
                    // The two agree today, because the deciding lane is the
                    // first whose pass does not stand and a `fail` is what put
                    // it there. They agree by an argument rather than by
                    // construction, and naming the wrong lane in a hand-back
                    // sends a worker to the wrong review.
                    rd_fact(
                        &brief
                            .deciding_lane
                            .clone()
                            .unwrap_or_else(|| brief.failing_lane())
                    )
                ) + Self::grace_clause(grace),
                entry.counters.review_rounds,
                limits.max_review_rounds,
            ),
        };
        render_template(
            DRIVER_FIX_TPL,
            &[
                ("PR", &brief.pr.to_string()),
                ("HEAD", &rd_fact(&brief.head)),
                ("BASE", &base),
                ("WHAT", &what),
                ("ATTEMPT", &attempt.to_string()),
                ("MAX_ATTEMPTS", &max.to_string()),
            ],
        )
    }

    /// Forget this PR's restart mark (#2811 S10).
    ///
    /// The mark is keyed `(group, pr)` and the ENTRY it was made for is not:
    /// a drive can be cancelled, pruned, or displaced by a fresh
    /// `drive_review` on the same PR, all within one process. A mark left
    /// behind by any of those outlives the drive it described, and the NEXT
    /// drive on that PR spends it the first time it reaches `fix-wait` — one
    /// unearned `why: restart` re-brief, charged to nobody and explained by
    /// nothing, on a drive no restart ever interrupted.
    ///
    /// So it is cleared everywhere [`rd_signals`](Registry::rd_signals) is,
    /// and for the same reason: both are per-process facts ABOUT AN ENTRY,
    /// held in a map keyed by the PR that entry happened to be for. The three
    /// sites are the prune, `cancel_review_drive`, and `drive_review`'s
    /// re-drive; the tick's own discharge is separate and is the `re_briefed`
    /// spend, which answers "was this mark used" rather than "is this mark
    /// still about anything".
    fn rd_forget_restart_mark(&self, group: &GroupId, pr: u64) {
        self.rd_restart_handback.lock_safe().remove(&(group.clone(), pr));
    }

    /// This drive's restart mark, or the empty one — read at facts-build time,
    /// spent by [`rd_spend_restart_mark`](Self::rd_spend_restart_mark).
    fn rd_restart_mark(&self, group: &GroupId, pr: u64) -> RestartMark {
        self.rd_restart_handback
            .lock_safe()
            .get(&(group.clone(), pr))
            .cloned()
            .unwrap_or_default()
    }

    /// Discharge part of a restart mark, dropping the key once nothing is left
    /// to say.
    ///
    /// **Per field, never wholesale.** A drive can carry more than one — a
    /// `review-wait` drive that lost its lane AND a superseded worker pane — and
    /// the tick that acts on one has decided nothing about the others. Clearing
    /// the whole entry from the site that re-briefed a lane is how #2811 S10's
    /// own discharge would have silently un-marked #3225's push.
    fn rd_spend_restart_mark(&self, group: &GroupId, pr: u64, f: impl FnOnce(&mut RestartMark)) {
        let mut marks = self.rd_restart_handback.lock_safe();
        let key = (group.clone(), pr);
        let Some(mark) = marks.get_mut(&key) else { return };
        f(mark);
        if mark.is_empty() {
            marks.remove(&key);
        }
    }

    /// Whether this pane is in the **live roster** — the one liveness rule the
    /// driver's record-repair uses (#3225, #3226).
    ///
    /// An agent this registry has no record of answers `false` here, and that is
    /// the opposite reading from [`rd_pane_exit`](Self::rd_pane_exit)'s
    /// deliberately — which is why this is only ever asked from the two places
    /// where absence is unambiguous. See
    /// [`rd_forget_lost_panes`](Self::rd_forget_lost_panes).
    fn rd_pane_is_live(&self, agent_id: &str) -> bool {
        self.agent(agent_id).is_some_and(|a| a.status != AgentStatus::Dead)
    }

    /// **Drop every pane this drive owns that the live roster does not have, and
    /// keep the conversations** (#3225, #3226). Answers what went.
    ///
    /// # Where this may be asked, and why not on every tick
    ///
    /// A pane missing from the agent map is normally "we could not check" — the
    /// asymmetry [`reviewdrive::DriveEntry::forget_dead_panes`],
    /// [`rd_pane_exit`](Self::rd_pane_exit) and [`reviewdrive::LaneFact::pane_dead`] all
    /// state, and the fail-direction that keeps a transient gap from un-owning
    /// live panes across the whole group.
    ///
    /// There are exactly two places the reading is not ambiguous, and this is
    /// called from both: the **restart reconcile**, where every pane of the
    /// previous process is gone by construction, and a **`drive_review` an
    /// orchestrator issued against a live drive**, which is an operator saying
    /// in as many words that this drive needs looking at. Nothing on the tick
    /// path calls it — and after either of those has run, no later tick names a
    /// dead pane anyway, because the ownership is gone.
    ///
    /// # What each drop leaves behind
    ///
    /// A lane is **reseeded** rather than merely emptied
    /// ([`reviewdrive::DriveEntry::reseed_lane`], #3176's half of the release):
    /// clearing the pane alone would leave `briefed_head` standing, and
    /// `decide_review_wait`'s wait arm reads that as *open at this revision, pane
    /// alive* — the drive then waits out its state bound for a verdict no pane
    /// can produce, which is #3226's incident exactly. The reseed carries the
    /// session, so the re-brief resumes the same reviewer conversation.
    ///
    /// The **worker** keeps `worker_session`, which is what the hand-back and
    /// #3225's push-delivered read both resume from; only `worker_agent` goes.
    ///
    /// **A pane whose session cannot be named is KEPT**, by both
    /// `reseed_lane`'s and `release_pane`'s own refusal: dropping it would cost
    /// the conversation rather than a slot, and the drive is still bounded by
    /// `lane-stalled` / `fix-stalled` as it was before. That is the residual —
    /// rare, since `rd_lane_session` resolves from the record, the live map and
    /// the roster, of which the roster survives a restart.
    fn rd_forget_lost_panes(
        &self,
        group: &GroupId,
        entry: &mut reviewdrive::DriveEntry,
    ) -> RdLostPanes {
        let mut lost = RdLostPanes::default();
        // Lanes first: resolving a lane's session borrows `entry` immutably and
        // reads three sources, so it is done before anything mutates the record.
        let blocks: Vec<String> = entry
            .lanes
            .iter()
            .filter(|l| !l.agent.trim().is_empty())
            .map(|l| l.block.clone())
            .collect();
        for block in blocks {
            let Some(pane) = entry.lane(&block).map(|r| r.agent.clone()) else { continue };
            if self.rd_pane_is_live(&pane) {
                continue;
            }
            let session = self.rd_lane_session(group, entry.lane(&block)).unwrap_or_default();
            if let Some(freed) = entry.reseed_lane(&block, &session) {
                lost.lanes.push((block, freed));
            }
        }
        let worker = entry.worker_agent.clone();
        if !worker.trim().is_empty() && !self.rd_pane_is_live(&worker) {
            let session = entry.worker_session.clone();
            if let Some(freed) = entry.release_pane(&reviewdrive::DrivenRole::Worker, &session) {
                lost.worker.push(freed);
            }
        }
        // The superseded lists, by the same predicate — including the pane the
        // reseed just moved onto one of them.
        if entry.forget_dead_panes(&|id| self.rd_pane_is_live(id)) {
            lost.superseded = true;
        }
        lost
    }

    /// Whether this drive is still USING a pane the live roster does not have —
    /// the cheap question [`drive_review_with`](Self::drive_review_with) asks
    /// before refusing `already-driven` (#3226).
    ///
    /// Reads the same predicate as [`rd_forget_lost_panes`](Self::rd_forget_lost_panes)
    /// over the same population as the locked gate below it
    /// ([`RdLostPanes::current_panes_lost`]), and repairs nothing — so the cheap
    /// check and the authoritative one cannot disagree about what a repair would
    /// find.
    ///
    /// **The CURRENT worker and lane panes, never `owned_panes()`** (#3228
    /// review 2, W1): that one includes the superseded lists, whose dead entries
    /// are the ordinary state of a drive between a pane replacement and the next
    /// tick's prune. See `current_panes_lost` for what reading them here cost.
    fn rd_has_lost_panes(&self, entry: &reviewdrive::DriveEntry) -> bool {
        let current = std::iter::once(entry.worker_agent.clone())
            .chain(entry.lanes.iter().map(|l| l.agent.clone()));
        current.filter(|a| !a.trim().is_empty()).any(|a| !self.rd_pane_is_live(&a))
    }

    /// §2.4's restart reconcile, once per group per registry instance, before
    /// driving — `rd_reconciled` is a field of the registry, so "per process"
    /// holds only while a process builds one of these (#2135 review 2).
    ///
    /// The `recover_persisted_queue` posture: a PR **positively established** as
    /// closed or merged becomes `cancelled` with its notice; anything else
    /// resumes from disk and is re-evaluated against the **live** head on the
    /// tick that follows, never against the head the file remembers. A PR whose
    /// state could not be determined is neither — `None` is "the world does not
    /// match", never "probably fine".
    ///
    /// It also drops any cap-starvation run the previous process left standing
    /// (#2135) — see the comment on the call, and
    /// `DriveEntry::discard_cap_starvation_run` for why that run cannot survive
    /// a process boundary and why nothing is charged for it.
    ///
    /// An unresolvable *session* is deliberately not held here, though §2.4
    /// names it: that is what the first hand-back discovers
    /// (`held(worker-unresumable)`), and holding at reconcile would park every
    /// drive whose worker pane merely has not been re-registered yet at startup
    /// — a race the reconcile cannot distinguish from a genuinely lost session,
    /// where the hand-back can.
    fn rd_reconcile_with(&self, group: &GroupId, runner: &dyn rddrive::RdRunner, now: u64) {
        if self.rd_reconciled.lock_safe().contains(group) {
            return;
        }
        let dir = self.group_dir(group);
        // `(on_behalf, pr, cancelled, forgot_cap_run, panes_dropped)`.
        let mut audits: Vec<(String, u64, bool, bool, usize)> = Vec::new();
        // #2135 N2: set false only when a write this reconcile NEEDED actually
        // failed. It stays true when nothing had to be written, which is the
        // honest reading — there was no durable outcome to miss.
        let mut persisted = true;
        {
            let _state_guard = self.rd_state_lock.lock_safe();
            // **The once-only latch is set below, on a reconcile that actually
            // READ the file — not here, on one that merely attempted it.**
            // Latching first is the obvious spelling and it means a torn
            // `review_drives.json` at startup costs this group its reconcile for
            // the life of the process, including after a human fixes the file:
            // the flag says "done" and nothing ever revisits it. §2.4 already
            // makes an unreadable file a loud, rate-bounded "a human has to look
            // at this", and a human who then looks and fixes it should get the
            // reconcile they were owed.
            let Ok(mut state) = reviewdrive::load_state(&dir) else { return };
            self.rd_reconciled.lock_safe().insert(group.clone());
            let mut changed = false;
            let live: Vec<u64> =
                state.entries.iter().filter(|e| !e.state().is_terminal()).map(|e| e.pr).collect();
            for pr in live {
                // `pr_is_open`, not `observe_pr`: reconcile reads nothing but
                // this, and `observe_pr` would spend a second round trip on
                // checks it never looks at, per live entry, at startup.
                let open = rddrive::pr_is_open(runner, pr);
                let Some(entry) = state.entry_mut(pr) else { continue };
                let on_behalf = entry.on_behalf_of.clone();
                // **A cap-starvation run cannot straddle a process boundary**
                // (#2135). The stamp is written and read by ticks that OBSERVED
                // the cap refuse a lane spawn, and across a shutdown no tick
                // ran: the gap is time the cap refused nothing, and after a
                // restart every pane this group's cap was counting is gone.
                // Left standing, a stamp older than `CAP_HOLD_MS` parks the
                // resumed drive `held(cap-full)` on its FIRST tick — before one
                // spawn is attempted — on a notice telling an orchestrator to
                // free a slot in a group whose slots are all free.
                //
                // Unconditional, and above the cancel arm rather than in the
                // `else`: a cancelled entry's `advance` clears the stamp anyway,
                // so putting this in one branch would make the reconcile's rule
                // depend on a fact that has nothing to do with the cap. Charging
                // nothing is `discard_cap_starvation_run`'s own argument — the
                // gap is mostly downtime, and #2110's accumulators are a CREDIT
                // against the age bounds that #2117 deliberately charges
                // downtime to.
                let forgot_cap_run = entry.discard_cap_starvation_run();
                if forgot_cap_run {
                    changed = true;
                }
                if open == Some(false)
                    && entry.advance(reviewdrive::DriveState::Cancelled, None, None, 0).is_ok()
                {
                    changed = true;
                    // Owed onto the entry, not returned for a fire-and-forget
                    // delivery (#1857). Reconcile runs at startup — the exact
                    // moment an orchestrator pane is most likely to be missing
                    // or still coming up — so this is the producer whose notice
                    // was most likely to be the one that vanished. The caller's
                    // flush delivers it from disk, on this tick or a later one.
                    //
                    // #1871 B3's pane list is threaded into the construction
                    // rather than dropped: this hunk is a replace-vs-augment,
                    // and keeping "both sides" would restore the direct push
                    // #1857 deleted while ALSO owing the notice — the line
                    // twice. The panes are read before `owe_notice`'s mutable
                    // borrow.
                    let panes = self.rd_surviving_panes(entry);
                    // No release clause: reconcile takes no tick, so
                    // `releasable` is never asked and nothing was killed here.
                    let n =
                        rddrive::cancelled_notice(pr, rddrive::CancelCause::PrGone, &panes, "");
                    // #3367 item 2: the reconcile's cancel can be an
                    // auto-started drive's FIRST notice — the report was
                    // persisted for exactly this, a first notice one restart
                    // away — so it folds the report like the tick's does.
                    let n = Self::rd_fold_auto_report(entry, n);
                    entry.owe_notice(&n, now);
                    audits.push((on_behalf, pr, true, forgot_cap_run, 0));
                } else {
                    // **What a restart cost this drive, and what recovers it**
                    // (#2811 S10, #3225, #3226) — the three marks below, each
                    // argued at its own arm.
                    //
                    // Every mark is recorded here and acted on by the tick, which
                    // is the split the rest of this function already keeps: the
                    // reconcile is the only place the fact is KNOWN (every pane
                    // died, so a missing one is not the ambiguous mid-session
                    // reading), and the tick is the only place that can afford to
                    // observe the PR, render a brief and resume a session. Doing
                    // the hand-back here would spend a `gh` round trip per live
                    // drive at startup and duplicate the whole hand-back path,
                    // cap refusal and `worker-unresumable` handling included.
                    //
                    // **The panes, read from the LIVE ROSTER rather than from
                    // this file** (#3225, #3226). Every pane of the previous
                    // process died with it, so a recorded pane the roster does
                    // not have is GONE — not "we could not check", which is the
                    // reading every mid-session site makes and the reason this
                    // one is here rather than on the tick. Their SESSIONS
                    // survive, and are what each recovery below resumes.
                    //
                    // Ownership is dropped first, so nothing downstream can name
                    // a dead pane: `held(fix-stalled)`'s notice enumerates
                    // `owned_panes()` as "still OWNED", and on #3225's incident
                    // every entry in that list had died with the process.
                    let lost = self.rd_forget_lost_panes(group, entry);
                    if lost.any() {
                        changed = true;
                    }
                    let mut mark = RestartMark::default();
                    match entry.state() {
                        // **#2811 S10: a drive parked in `fix-wait` is waiting on
                        // a pane that died with the previous process.** Nothing
                        // will ever arrive for it — the signals map is
                        // per-process and empty, and the pane whose `report`
                        // would fill it is gone — so without this the drive waits
                        // out `fix_timeout_minutes` and exits
                        // `held(fix-stalled)`, a claim about a worker's silence
                        // that is really a claim about a restart.
                        //
                        // Unconditional on the state alone, and NOT on the pane
                        // having been dropped above: the hand-back is what
                        // discovers whether the SESSION survived, and a
                        // `fix-wait` drive whose pane somehow did survive is
                        // re-briefed into it by `rd_reuse_pane` at no cost.
                        reviewdrive::DriveState::FixWait => mark.handback = true,
                        // **#3225: `ci-wait` after a push, whose worker is
                        // gone.** `decide_fix_receipts` waits for that worker's
                        // `report(done)` before briefing a lane, and it cannot
                        // come; the push is durable and is the fix. Gated on the
                        // pane ACTUALLY having been lost, because unlike the
                        // hand-back there is no probe here — this mark makes the
                        // drive advance without the report, so it must rest on
                        // the pane really being gone rather than on the process
                        // having restarted.
                        reviewdrive::DriveState::CiWait
                            if entry.fix_pushed() && !lost.worker.is_empty() =>
                        {
                            mark.push_delivered = true
                        }
                        _ => {}
                    }
                    // **#3226: the lanes.** The reseed above already puts each
                    // one back in `lane_open_for`'s false branch, so
                    // `decide_review_wait` re-opens it on the next tick by the
                    // ordinary path and `rd_open_lane` resumes its recorded
                    // session. What the mark carries is the WHY, for the
                    // `rd-lane-spawned` row — without it a restart recovery is
                    // indistinguishable on the audit log from an ordinary
                    // re-brief.
                    mark.lanes = lost.lanes.iter().map(|(block, _)| block.clone()).collect();
                    // On the row, because the only other visible effect of
                    // dropping a pane is a notice that does NOT name it —
                    // which reads exactly like a drive that never owned one
                    // (`rd-lane-duplicate-refused`s reason, one surface over).
                    let dropped = lost.worker.len() + lost.lanes.len();
                    if !mark.is_empty() {
                        self.rd_restart_handback.lock_safe().insert((group.clone(), pr), mark);
                    }
                    audits.push((on_behalf, pr, false, forgot_cap_run, dropped));
                }
            }
            // #2135 N2: whether this reconcile's decisions reached DISK. The
            // in-memory `state` is dropped at the end of this block and the
            // once-only latch is already set, so a failed write means the clear
            // is lost for the life of the process — the next tick reloads the
            // stale stamp and parks `held(cap-full)` anyway. The audit row below
            // therefore reports the durable outcome rather than the decision:
            // a row asserting a clear the write unmade is the one state in which
            // this log actively misleads whoever is reading it. Same rule as the
            // tick's own `persisted` flag one function down, applied to the
            // reconcile, which had no counterpart.
            //
            // **The latch itself is deliberately left alone** and is the
            // residual: it predates #2135, and moving it is a change to when
            // every reconcile re-runs rather than to what this row says.
            if changed {
                persisted = reviewdrive::store_state(&dir, &state).is_ok();
            }
        }
        for (on_behalf, pr, cancelled, forgot_cap_run, dropped) in audits {
            let action = if cancelled {
                rddrive::audit_action::CANCELLED
            } else {
                rddrive::audit_action::RECOVERED
            };
            // #2135: whether this reconcile dropped a cap-starvation run the
            // previous process left behind. On the row rather than silent for
            // `rd-lane-duplicate-refused`'s reason — the clear's only other
            // visible effect is a drive that did NOT park, which on this log is
            // indistinguishable from there having been no stamp at all.
            self.rd_audit(
                group,
                &on_behalf,
                action,
                json!({ "pr": pr, "at": "reconcile",
                        "cap_run_forgotten": forgot_cap_run && persisted,
                        "panes_dropped": dropped }),
            );
        }
    }

    /// Put a `TaskNote` on the board row whose `pr` matches, so a human sees the
    /// drive where they see the work (§3.2).
    ///
    /// **The driver writes to the board and reads nothing from it.** `Task::pr`
    /// is agent-writable, so a driver that took its worker session or its gate
    /// from a row would be letting the thing being checked answer the check.
    /// Matching a row in order to write a note on it is not that: a wrong match
    /// costs a note on the wrong row, never an authorization.
    fn rd_task_note(&self, group: &GroupId, pr: u64, text: &str) {
        let Some(id) = self
            .tasks(group)
            .into_iter()
            .find(|t| t.pr.as_deref().and_then(pr_number) == Some(pr))
            .map(|t| t.id)
        else {
            return;
        };
        let _ = self.upsert_task(
            group,
            brand::AUDIT_ACTOR,
            Some(&id),
            TaskPatch { note: Some(text.to_string()), ..TaskPatch::default() },
        );
    }

    /// One entry, one tick, **at most one advance** (§2.4).
    ///
    /// Runs with `rd_state_lock` held, and that includes the spawn — §2.4 says
    /// so in as many words ("the load-decide-store spans a spawn, and a
    /// `drive_review` landing inside that window would otherwise read the
    /// pre-spawn file and write it back, erasing the entry"). It is safe to span
    /// one here and not a *notice*: no site that takes `rd_state_lock` is
    /// reachable from a pane delivery, so a spawn's own kickoff cannot cycle
    /// back onto it. The orchestrator notices this produces are still delivered
    /// by the caller, outside the lock, for the #467/#468 reason. The full site
    /// list, and why the two interception helpers do not break it, is on
    /// [`Registry::rd_drive_group_with`].
    fn rd_step_entry(
        &self,
        group: &GroupId,
        runner: &dyn rddrive::RdRunner,
        state: &mut reviewdrive::ReviewDrivesState,
        pr: u64,
        limits: &reviewdrive::DriveLimits,
        now: u64,
        base_green_memo: &mut std::collections::HashMap<String, Option<bool>>,
    ) -> Option<RdOut> {
        let resting =
            state.entry(pr).map(|e| e.state().is_parked() || e.state().is_terminal())?;
        if resting {
            // §2.1's `held` row: "nothing; the tick does not advance it". Bailing
            // BEFORE the reads rather than after — `decide` would answer `Wait`
            // anyway, but a parked drive can sit here for days, and spending
            // `gh` round-trips per parked entry per tick to be told so is the
            // cost §2.4's one-group bound exists to keep down.
            return None;
        }
        let obs = rddrive::observe_pr(runner, pr);
        // **Only the states that READ these facts pay for them.** `decide` reads
        // `required_lanes` in `review-wait` and `gate-check` and `gate` in
        // `gate-check` alone; `ci-wait` and `fix-wait` read neither. Resolving
        // them unconditionally would spend a `pr view --json files` on every
        // routing gate and a pair of `gh` reads on every `base-green` gate, per
        // entry, per tick, for answers nothing consults — on the loop that also
        // delivers every `notify_when` notice in the fleet (§2.4). The queue's
        // own driver gates the same two reads on the same principle
        // (`declares_base_green`: a value nothing consults is not worth a round
        // trip, and an unfetched value is `None`, which refuses).
        let here = state.entry(pr).map(|e| e.state())?;
        let want_lanes = matches!(
            here,
            reviewdrive::DriveState::ReviewWait | reviewdrive::DriveState::GateCheck
        );
        let want_gate = here == reviewdrive::DriveState::GateCheck;
        let (gate, required, lane_notices) = if want_lanes {
            self.rd_gate_facts(group, runner, pr, &obs, want_gate, base_green_memo)
        } else {
            // `NotEvaluated` is the honest value here and not a stand-in for
            // "satisfied": §2.1's `gate-check` row treats it as "the tick reached
            // this state without evaluating the gate", which is `Wait`. Neither
            // of the two states that land here can read it at all.
            (reviewdrive::GateOutcome::NotEvaluated, None, Vec::new())
        };
        // **The lane-side twin of `worker_exit` below** (#2163). A pane exit was
        // observed only for the worker and only in `fix-wait`, on the argument
        // that "`review-wait` has `lane-stalled` for its own panes" — which is
        // true and is an HOUR away, measured from the brief rather than from the
        // death. A reviewer pane killed twelve minutes into its round left the
        // drive silent for forty-eight more with no rd-* row at all.
        //
        // Read here rather than in `rd_gate_facts` because it is a fact about
        // the ENTRY (which pane this lane recorded) and the agent map, and that
        // function is given neither on purpose. `_` on a lane the entry has no
        // record for: there is no pane to be dead.
        //
        // **Filled for EVERY required lane, though `decide_review_wait` consults
        // only `first_stale_lane`'s `k`** (#2169 review 2, premortem 1). That is
        // deliberate rather than an oversight: `LaneFact` is a per-lane reading
        // of the world, and one whose fields were populated only for whichever
        // lane happened to be selected would be a struct whose meaning depends
        // on its index — so a later change that reads another lane's entry
        // would silently read a `false` nobody wrote. The cost is one agent-map
        // lookup per required lane per tick. Lanes open strictly sequentially
        // today, so nothing else can reach a non-`k` lane; this keeps that a
        // property of the DECISION rather than of the facts it is handed.
        let required = required.map(|mut lanes| {
            for l in lanes.iter_mut() {
                l.pane_dead = state
                    .entry(pr)
                    .and_then(|e| e.lane(&l.block))
                    .is_some_and(|rec| self.rd_dead_lane_pane(rec).is_some());
            }
            lanes
        });
        let signal = self.rd_signal(group, pr);
        let messaged_by = signal.messaged_by.clone();
        // **The driver watches the pane it resumed** (#1961), and only where
        // the answer can mean anything: in `fix-wait`, on the CURRENT worker
        // pane, and only while that pane has said nothing.
        //
        // A `done` or a `blocked` already in hand outranks it — a worker that
        // reported and then exited is a worker that finished, and reading its
        // exit as a failure would throw away the arc its own report earned.
        // Nor is this asked in any other state: a worker pane exiting outside a
        // hand-back is not this drive's business.
        //
        // **This used to add "`review-wait` has `lane-stalled` for its own
        // panes", and that was the whole of #2163.** It is true and it is an
        // HOUR away, anchored at the brief rather than at the death, so a
        // reviewer pane killed twelve minutes in cost forty-eight more of
        // silence. `LaneFact::pane_dead` above is the lane-side observation
        // that answers it on the next tick instead.
        let worker_exit = (here == reviewdrive::DriveState::FixWait
            && signal.worker == reviewdrive::WorkerSignal::Silent)
            .then(|| state.entry(pr).map(|e| e.worker_agent.clone()).unwrap_or_default())
            .filter(|a| !a.is_empty())
            .and_then(|a| self.rd_pane_exit(&a));
        // **The restart marks, read ONCE** (#3196 review 2, #3225, #3226) —
        // each is SPENT below by the arm that acted on it, never here.
        let restart = self.rd_restart_mark(group, pr);
        let facts = reviewdrive::DriveFacts {
            now_ms: now,
            pr_open: obs.open,
            head: obs.head.clone(),
            body_digest: obs.body_digest.clone(),
            required_lanes: required.clone(),
            ci: obs.ci,
            // A dead resumed pane IS `worker-unresumable`, learned one tick
            // after the hand-back instead of one fix timeout after it. It rides
            // the existing signal rather than a new arc because §2.1 already
            // routes `Unresumable` from `fix-wait` to exactly this hold; what
            // was missing was anything that could ever produce it after the
            // hand-back itself had succeeded.
            worker: match worker_exit {
                Some(_) => reviewdrive::WorkerSignal::Unresumable,
                None => signal.worker,
            },
            gate,
            messaged: signal.messaged,
            // #2811 S10, and READ here — the mark is SPENT below, by the tick
            // that acts on it (#3196 review 2, rev-final finding 1).
            //
            // Taking it here was wrong, and wrong in the direction that revives
            // the incident this slice exists to remove. Several things decide
            // above `decide_fix_wait` and none of them re-brief anybody: the
            // empty-head guard returns `Wait` whenever `observe_pr` could not
            // read the PR — a runner error, a rate limit, an unparseable
            // response, which is a routine first-tick condition when a restart
            // sends a burst of `gh` calls at once — and the age and state
            // backstops park the drive. The reconcile runs once per registry
            // instance, so a mark spent by a tick that did nothing is never
            // re-issued: the drive keeps its dead pane, is never re-briefed, and
            // waits out `fix_timeout_minutes` into exactly the
            // `held(fix-stalled)` this slice is about.
            //
            // So the mark now survives every such tick and is discharged only by
            // one that RESOLVED the restart question — see the spend below.
            restart_handback: restart.handback,
            // #3225: the sibling one state later — a push whose worker pane
            // died with the process is the fix delivered, not a report to
            // keep waiting for. Set by the same reconcile, on the same
            // roster reading, and spent by the arc it produces.
            restart_push_delivered: restart.push_delivered,
            // #2811 S5b: the union over every pane this drive owns — lanes
            // AND the worker — which is why the fact is drive-level and not
            // on `LaneFact`: a drive in `fix-wait` owns a worker pane and no
            // open lane at all, and that is exactly a drive the hold covers.
            //
            // Read from the attention scan's published map, never by reading
            // pane text here: plan-2504 keeps ONE pane-text classifier, and a
            // second would be a second answer that can disagree with the chip
            // the human is looking at.
            //
            // `prior_*` agents come with `owned_panes()` and are harmless:
            // a pane that is gone is not in the map, so it contributes
            // nothing, and one still alive on a limited provider is a pane
            // this drive really did leave stopped.
            provider_limited: state
                .entry(pr)
                .map(|e| e.owned_panes())
                .unwrap_or_default()
                .into_iter()
                .find_map(|(agent, _)| self.provider_limit_for_agent(&agent)),
        };
        let mut out = RdOut::new(pr);
        out.backoff = obs.runner_failed;
        if let Some(why) = &worker_exit {
            out.refusal = why.clone();
        }
        let brief = RdBrief {
            pr,
            head: obs.head.clone(),
            base: obs.base.clone(),
            body_digest: obs.body_digest.clone().unwrap_or_default(),
            ci: obs.ci,
            failing_jobs: obs.failing_jobs.clone(),
            required: required
                .as_ref()
                .map(|r| r.iter().map(|l| l.block.clone()).collect())
                .unwrap_or_default(),
            lane_notices,
            // The index `decide_review_wait` walks to, computed with the same
            // pure function it uses rather than guessed from the verdicts.
            deciding_lane: required.as_deref().and_then(|r| {
                r.get(reviewdrive::first_stale_lane(
                    r,
                    &obs.head,
                    obs.body_digest.as_deref(),
                ))
                .map(|l| l.block.clone())
            }),
        };
        // **Every pane ANOTHER live drive owns**, read here because this is the
        // last point at which the whole state is borrowable immutably — the
        // next line takes this entry mutably (review round 1, finding 1).
        //
        // `already-driven` is keyed on the PR and not on the session
        // (`rd_refuse(RESUME_...)` below), so two live drives may legally name
        // one worker session: the same worker pushed two PRs. Without this, the
        // terminal release of drive A would take a pane drive B's record still
        // names and is still going to speak to — the one claim the session
        // widening rests on ("nobody is going to speak to it again"), broken by
        // the widening itself. Computed for every tick rather than only for the
        // terminal ones: it is a walk over a handful of entries, and a
        // conditional read here would have to be re-derived below where the
        // borrow is gone.
        let owned_elsewhere: Vec<String> = state
            .entries
            .iter()
            .filter(|e| e.pr != pr && e.state().is_live())
            .flat_map(|e| e.owned_panes().into_iter().map(|(a, _)| a))
            .collect();
        let entry = state.entry_mut(pr)?;
        // What each lane's verdict file said this tick, recorded onto that lane
        // BEFORE the decision — not as an input to it (nothing decides from a
        // recorded verdict; the live file is re-read every tick through the
        // gate's own parser) but because `at_head` is what tells a lane that has
        // ANSWERED from one that has only been ASKED. Without it a re-briefed
        // lane looks like a first-time lane forever and §5.5's delta template is
        // unreachable, which is the defect this line closes.
        for l in &brief.lane_notices {
            if entry.record_verdict_seen(&l.block, l.verdict, &l.at_head) {
                out.changed = true;
            }
        }
        let step = reviewdrive::decide(entry, &facts, limits);
        // **Read BEFORE the arc is taken, applied after it** (#2501). Both
        // halves matter. `releasable`'s worker rule is "a hand-back is
        // outstanding and the report it was waiting for arrived", which stops
        // being true the instant `entry.take` takes the arc out of that wait;
        // its terminal rule reads the entry the step is about to END, which
        // `take` equally destroys (#2811 S1); and the
        // lane rule must not see a lane this very tick re-briefed, which the
        // `OpenLane` arm below is about to do. Computed here, the answer is
        // about the world the decision was made in.
        let releases = reviewdrive::releasable(entry, &facts, &step);
        let on_behalf = entry.on_behalf_of.clone();
        // §2.1's `review-wait` row writes "the current lane index", and the
        // current lane is the DECIDING one — the first whose pass does not stand
        // — whether or not this tick had to open it. Writing it only on a
        // successful spawn leaves it lagging every time the drive waits on a
        // lane that is already open, which is most ticks of most rounds. It is
        // only a display and last-resort-fallback field today, so the lag was
        // not reachable as a defect; a field that is silently wrong is how the
        // next reader is misled.
        if entry.state() == reviewdrive::DriveState::ReviewWait {
            if let Some(k) = brief
                .deciding_lane
                .as_deref()
                .and_then(|b| brief.required.iter().position(|r| r == b))
            {
                if entry.lane_index != k {
                    entry.lane_index = k;
                    out.changed = true;
                }
            }
        }
        // **The releases (#2501)** — §3.1 item 5's narrowing, performed.
        //
        // **BEFORE the step's own arm, and that placement is the whole of
        // rev-final W1.** The obvious spot is below the `match`, after the arc
        // has been taken; it is wrong, and wrong in exactly the case this
        // feature exists for. On the `review-wait -> fix-wait` fail route the
        // arm calls `rd_handback`, and since #2501 released the previous
        // worker's pane there is usually no live pane to reuse — so it SPAWNS,
        // under the live-delegate cap, while the lane pane this tick is about to
        // release is still `status != Dead` and still counted. At the cap the
        // spawn is refused, the arm parks the drive `held(cap-refused)` on a
        // notice asking an orchestrator to free a slot, and only then would the
        // release free one. A parked drive does not self-advance, so that is an
        // orchestrator wake — the exact cost #2501 measured, reintroduced by
        // #2501's own worker rule. Releasing first means `mark_dead` has already
        // dropped the pane out of `live_delegate_count` before `rd_spawn` asks.
        //
        // Nothing else depends on the old order. The candidates were computed
        // pre-arc either way (see `releasable` above); reading each pane id here
        // reads the record BEFORE `rd_open_lane` could replace it, which is
        // strictly safer than after; and the exit notices are still built below
        // the whole `match`, off `entry.owned_panes()`, so they name what is
        // actually left. The one thing that moves is the ordering against
        // `entry.take`: a transition `take` refuses (unreachable through
        // `decide`) would now follow a release rather than skip it, which is
        // still correct — a lane that answered at this head is finished with
        // this round whether or not the drive managed to move.
        //
        // Three things happen per candidate, in this order and for these
        // reasons.
        //
        // **The session is resolved FIRST**, through `rd_lane_session`'s three
        // sources, and an empty answer skips the candidate. The promise this
        // whole mechanism rests on is that the conversation survives, and a lane
        // whose session cannot be named is one whose conversation the kill would
        // end. Failing closed here costs a slot; failing open costs the review.
        //
        // **Then the kill**, through the one capability the driver has for it.
        // `release_driven_pane` is the barrier — idle, alive, bound to a
        // terminal, not a manager — and it answers `Err` for every pane that is
        // none of those, which is skipped rather than recorded.
        //
        // **Then the record**, and only on the kill having succeeded. The two
        // are under ONE hold of `rd_state_lock`, which is what keeps the pane
        // from ever being live and unowned: `mark_dead` has already made
        // `resolve_token` refuse that caller by the time its id leaves the
        // record, so §7 has no window to leak through. Doing the kill out in the
        // caller's side-effect loop, the way a kick-back delivery is done, would
        // open exactly that window — and a delivery is safe there because losing
        // one costs a line, while losing this ordering costs an unowned pane.
        //
        // The audit row is written last, on the fact rather than the intent:
        // §5.4 says a release row means a pane went, and a reader counting freed
        // slots must be able to trust the count.
        // **The session of the worker pane this tick released, for the terminal
        // notice** (#2811 S1). Read out of the loop rather than recomputed below,
        // because by the time the notice is built `release_pane` has taken the
        // pane out of the record and the entry no longer names it — and because
        // only a release that the barrier actually PERFORMED may be reported as
        // one, which is the same honesty `out.releases` keeps.
        // **#3176's other half: a lane the release CANNOT take is told to stop
        // instead.**
        //
        // `release_driven_pane` refuses a pane that is not idle, and §3 is why —
        // the driver does not kill a reviewer mid-turn. But a reviewer mid-turn
        // on a PR that does not merge is precisely the case #3176 is about: it
        // is spending a paid round reading a head the rebase is about to
        // replace. So the two arms are complementary rather than alternatives,
        // and exactly one of them applies to a lane on a tick — the idle test
        // below is the same question `release_driven_pane` asks, asked first so
        // the two cannot both fire.
        //
        // **A queued delivery, never an interrupt** — `Delivery::MidSession`,
        // the same mechanism `rd_reuse_pane` types a re-brief with. It lands on
        // the pane's own queue and is pasted when the CLI next takes input;
        // nothing is cancelled, no signal is sent, and a reviewer mid-thought
        // finishes it and then reads this. That is the only kind of text orrerix
        // puts into a delegate's pane, and this adds no second kind.
        //
        // **Once per revision, not once per tick.** The rule is a standing
        // property of the facts — that is what makes the release arm work at all
        // — so `stopped_head` marks the lane, and the mark is written on the
        // delivery SUCCEEDING rather than on the intent, so a line that did not
        // reach the pane is one the next tick still owes.
        //
        // A lane with no pane on record, and one whose stop line has already
        // landed at this head, are skipped. So is a lane the candidate list does
        // not carry, which is the carve-out doing its work one function over: a
        // reviewer whose verdict is already on record is not mid-review and has
        // nothing to stand down from.
        for cand in &releases {
            if cand.reason != reviewdrive::ReleaseReason::Conflict {
                continue;
            }
            let reviewdrive::DrivenRole::Lane(block) = &cand.role else { continue };
            let agent = entry.lane(block).map(|r| r.agent.clone()).unwrap_or_default();
            if agent.trim().is_empty() {
                continue;
            }
            // **Which panes this arm is for, decided once** (rev-std round 1
            // finding 1).
            //
            // IDLE is the release arm's: telling a pane to stop and then killing
            // it on the same tick would be two events for one decision, and
            // `idle_since_ms` is the same signal `release_driven_pane` refuses
            // on, so the two arms cannot both fire.
            //
            // DEAD, or unknown to the registry, is NEITHER arm's — and the
            // reason is the retry. The declined row below earns its retry from
            // the queue-full case, where the next tick can succeed. For a pane
            // that died mid-review (a human kill, the idle reaper) while the PR
            // is CONFLICTING, `deliver_prompt` answers `Err` on every tick of
            // the whole conflict window, the mark is never written and the
            // release barrier refuses the same pane — so a futile retry would
            // put one row per tick on the very surface §5.4 asks a reader to
            // count from. Bounded and truthful, but noise, and noise on that
            // surface is what `rd-hold-repeated` exists to stop one arm over.
            //
            // Nothing else changes: the lane keeps its dead pane on record and
            // `decide_review_wait`'s `pane_dead` arm re-opens it once the rebase
            // clears the conflict, which is #2163's existing path and not this
            // slice's to alter.
            match self.agent(&agent) {
                Some(a) if a.idle_since_ms.is_some() => continue,
                Some(a) if a.status == AgentStatus::Dead => continue,
                Some(_) => {}
                None => continue,
            }
            if entry.lane_stopped_at(block, &brief.head) {
                continue;
            }
            let text = self.rd_lane_stop_brief(&brief);
            // **A refusal is AUDITED, not swallowed** (#3176, aligned with
            // #3203's `rd_take_over_pane`). `deliver_prompt` can refuse for
            // reasons that say nothing about the drive, but one is reachable
            // with nothing wrong at all — a pane at `QUEUE_MAX_PER_PANE` — and
            // on a silent `continue` that is indistinguishable from "there was
            // no busy lane to tell", which is the exact indistinguishability
            // `rd-reuse-declined` and `rd-takeover-declined` were both added to
            // remove. The mark is not written on this path, so the next tick
            // tries again and these rows say how many ticks it took.
            if let Err(why) = self.deliver_prompt(
                &agent,
                &text,
                brand::AUDIT_ACTOR,
                Delivery::MidSession,
            ) {
                out.audits.push((
                    rddrive::audit_action::LANE_STOP_DECLINED,
                    json!({ "pr": pr, "block": block, "agent": agent,
                            "head": brief.head, "reason": why }),
                ));
                continue;
            }
            if entry.mark_lane_stopped(block, &brief.head) {
                out.changed = true;
            }
            out.audits.push((
                rddrive::audit_action::LANE_STOPPED,
                json!({ "pr": pr, "block": block, "agent": agent,
                        "head": brief.head, "why": "conflict" }),
            ));
        }
        let mut released_worker_session = String::new();
        for cand in &releases {
            // **A WORKER candidate names every pane this drive owns on that
            // session, not only the current one** (#3203). `releasable` decides
            // per ROLE, and the worker role is one conversation that may have
            // more than one pane sitting on it: a hand-back that superseded a
            // pane left the old one alive and owned, and before #3203's other
            // half a second hand-back minted one more. Releasing only
            // `worker_agent` left those behind idle and counted against the
            // cap for the rest of the drive — measured on PR #3198, where the
            // ORIGINAL worker pane sat idle through two hand-backs and two
            // releases.
            //
            // Nothing here decides WHETHER a pane may go: `release_driven_pane`
            // is still the barrier (idle, alive, bound to a terminal, not a
            // manager), applied per pane, so a superseded pane that is somehow
            // busy is skipped exactly as the current one would be. What widens
            // is only the population the barrier is asked about.
            //
            // Ordered oldest-first, so the audit rows read as the history
            // they are: `owned_panes`'s own order for an owned-only population,
            // and `reviewdrive::release_population`'s sort wherever the founding
            // panes widen it.
            //
            // **And at a TERMINAL exit it also names the panes the drive was
            // STARTED ON** (#3250). `owned_panes` is written by a hand-back, so
            // it is EMPTY for a drive that never took one — which is not a
            // corner: on PRs #3243 and #3248 the audit log runs `rd-started` ->
            // `rd-satisfied` with no `rd-handback` row at all, and the worker
            // pane the orchestrator named at `start_review_drive` sat idle
            // through the exit holding its worktree.
            // `ReleaseCandidate::include_founding` carries the decision and the
            // bound — it is the engine's to make, so this reads the flag rather
            // than re-deriving the step.
            //
            // Still nothing here decides WHETHER a pane may go: the barrier is
            // applied per pane below, unchanged, so a founding pane that is busy
            // or not a driven delegate's role is skipped exactly as an owned
            // one is.
            let (agents, session) = match &cand.role {
                reviewdrive::DrivenRole::Worker => {
                    let mut agents: Vec<String> = entry
                        .owned_panes()
                        .into_iter()
                        .filter(|(_, role)| *role == reviewdrive::DrivenRole::Worker)
                        .map(|(agent, _)| agent)
                        .collect();
                    if cand.include_founding {
                        // **The merge, the narrowing and the ORDER are the
                        // engine's** (review rounds 1 and 2), so each is stated
                        // and tested in one place instead of being spelled as
                        // loop conditions here. All this side supplies is the
                        // registry's own fact: how old a pane is.
                        agents = reviewdrive::release_population(
                            agents,
                            &entry.founding_panes,
                            &owned_elsewhere,
                            &|a: &str| self.agent(a).map(|x| x.started_ms),
                        );
                    }
                    (agents, entry.worker_session.clone())
                }
                reviewdrive::DrivenRole::Lane(block) => {
                    let rec = entry.lane(block);
                    let agent = rec.map(|r| r.agent.clone()).unwrap_or_default();
                    let session = self.rd_lane_session(group, rec).unwrap_or_default();
                    (vec![agent], session)
                }
            };
            if session.trim().is_empty() {
                continue;
            }
            for agent in agents {
                if agent.trim().is_empty() {
                    continue;
                }
                if self.release_driven_pane(&agent).is_err() {
                    continue;
                }
                // **The record drop, and why a superseded pane needs none.**
                // `release_pane` clears the CURRENT pane out of the entry, which
                // is what keeps a live pane from ever being unowned (§7). A
                // superseded id is already on a list bounded by LIVENESS, and
                // `release_driven_pane` has just made this one dead, so
                // `forget_dead_panes` drops it — writing it out here as well
                // would be a write whose only effect is to be undone, which is
                // the argument `release_pane`'s own doc makes about the lane it
                // does not push.
                let freed = if entry.worker_agent == agent {
                    match entry.release_pane(&cand.role, &session) {
                        Some(freed) => freed,
                        None => continue,
                    }
                } else if cand.role == reviewdrive::DrivenRole::Worker {
                    agent.clone()
                } else {
                    // **A conflict release also forgets the revision the lane
                    // was briefed at** (#3176) — see
                    // [`reviewdrive::DriveEntry::reseed_lane`] for why a plain
                    // `release_pane` here leaves `review-wait` waiting on a lane
                    // it can see is open and cannot see has no pane. Every other
                    // reason releases a lane that has ANSWERED, where the field
                    // is inert.
                    //
                    // This is the LANE arm: the two above it are the worker's
                    // (#3203's per-pane widening), and a lane candidate names
                    // exactly one pane, so the reseed cannot reach a superseded
                    // id it was not decided for.
                    let freed = match (&cand.role, cand.reason) {
                        (
                            reviewdrive::DrivenRole::Lane(block),
                            reviewdrive::ReleaseReason::Conflict,
                        ) => entry.reseed_lane(block, &session),
                        _ => entry.release_pane(&cand.role, &session),
                    };
                    match freed {
                        Some(freed) => freed,
                        None => continue,
                    }
                };
                if cand.role == reviewdrive::DrivenRole::Worker {
                    released_worker_session = session.clone();
                }
                out.changed = true;
                let (action, mut detail) = match &cand.role {
                    reviewdrive::DrivenRole::Worker => (
                        rddrive::audit_action::WORKER_RELEASED,
                        json!({ "pr": pr, "agent": freed, "session": session,
                                "reason": cand.reason.as_str() }),
                    ),
                    reviewdrive::DrivenRole::Lane(block) => (
                        rddrive::audit_action::LANE_RELEASED,
                        json!({ "pr": pr, "block": block, "agent": freed, "session": session,
                                "reason": cand.reason.as_str() }),
                    ),
                };
                detail["head"] = Value::String(brief.head.clone());
                out.audits.push((action, detail));
                out.releases.push((cand.role.clone(), freed));
            }
        }
        // #2811 S10: set by the `Rehandback` arm, read by the spend below.
        let mut re_briefed = false;
        match &step {
            reviewdrive::DriveStep::Wait => {
                // **#1959: a worker's `report(progress)` in `fix-wait` is
                // answered, in the worker's own pane.**
                //
                // The drive does not move on it and must not — a drive advances
                // on the head, the checks and the verdict files, and treating
                // "still going" as "the fix is in" would brief a reviewer over
                // unfinished work. But swallowing it is #1857's shape one arm
                // over: the measured round was a BODY-ONLY fix, so there was
                // nothing to push and no new checks, the worker read the
                // brief's "report when the checks are green" literally and sent
                // `progress`, and the drive sat for ten minutes until the idle
                // watchdog woke the ORCHESTRATOR — the turn the driver exists to
                // remove. One line back into the worker's pane costs that turn
                // nothing.
                //
                // Under `Wait` alone, so it can never displace an arc: a tick
                // that has something to DO does it, and a worker whose
                // `report(done)` arrived in the same window is advanced rather
                // than lectured.
                if signal.worker_progress && entry.kickback_owed() {
                    let agent = entry.worker_agent.clone();
                    if !agent.is_empty() {
                        // Marked BEFORE the delivery, which happens outside this
                        // lock. A delivery that fails therefore costs the line —
                        // the same asymmetry §5.2 already draws for a hold's
                        // notice, and the right direction here: the drive stays
                        // bounded by `fix-stalled` either way, while a mark
                        // written only on success would re-emit on every tick
                        // for as long as a pane stayed unreachable.
                        entry.record_kickback(now);
                        out.changed = true;
                        out.kickback = Some((agent.clone(), rddrive::fix_kickback_notice(pr)));
                        out.audits.push((
                            rddrive::audit_action::KICKBACK,
                            json!({ "pr": pr, "agent": agent }),
                        ));
                    }
                }
            }
            // #2811 S10: the restart re-hand-back. **No arc, no counter.**
            //
            // The drive stays in `fix-wait`; what was lost across the process
            // boundary is the worker's PANE, not its session and not anything
            // it had been told. So this re-renders the same fix brief and
            // resumes the recorded session, exactly as the arc into `fix-wait`
            // does — and charges nothing for it, because no review round, CI
            // run or rebase happened. A restart is not a round.
            //
            // `grace: false`: #2509's one-shot grace is granted by an arc that
            // spends it, and this takes no arc. Passing `true` here would print
            // a grant the entry's counters do not record.
            reviewdrive::DriveStep::Rehandback => {
                re_briefed = true;
                match self.rd_handback(group, entry, &brief, limits, false) {
                    Ok((agent, pane)) => {
                        // The clock moves only once the worker has actually
                        // been reached, so a hand-back that failed leaves
                        // `held(fix-stalled)`'s bound where it was rather than
                        // silently extending it by a whole `fix_timeout_minutes`
                        // on every restart.
                        entry.restamp_fix_handback(now);
                        self.rd_handback_fails.lock_safe().remove(&(group.clone(), pr));
                        out.changed = true;
                        out.handback = Some(agent.clone());
                        out.audits.push((
                            rddrive::audit_action::HANDBACK,
                            json!({ "pr": pr, "agent": agent, "head": brief.head,
                                    "why": rddrive::handback_why::RESTART,
                                    // #3203: written on THIS arm too. A restart
                                    // took every pane with the old process, so
                                    // this is normally `spawned` — but the arm
                                    // runs whenever a restart MARK is standing,
                                    // and the orchestrator may already have
                                    // reopened that session by hand, in which
                                    // case the take-over arm reaches that pane
                                    // rather than opening a second one beside it.
                                    "pane": pane }),
                        ));
                    }
                    Err(why) => {
                        // A session that will not resume really is
                        // `worker-unresumable`, and it is learned HERE rather
                        // than assumed at reconcile: the mark says the process
                        // restarted, and only the attempt can say whether the
                        // session survived it. Held through the same arc every
                        // other failed hand-back takes, so the notice, the
                        // second-failure wording and `rd-held` are one path.
                        self.rd_handback_failed(group, entry, pr, &why, &mut out, now);
                    }
                }
            }
            reviewdrive::DriveStep::OpenLane { index, verify, body_only } => {
                // `decide` only ever names an index into the list it was handed,
                // so `None` is unreachable — and it is handled by falling
                // THROUGH rather than returning, because an early return here
                // would skip the head persistence below, which is the one write
                // this function exists to get right. An unreachable branch that
                // skips a load-bearing write is how the reachable one gets
                // broken later.
                let block = brief.required.get(*index).cloned().unwrap_or_default();
                // #2163: read BEFORE the open, which replaces the record this
                // asks about — and turned into a row only on the `Ok` arm,
                // because a re-open the cap refused has re-opened nothing.
                let replaced = entry.lane(&block).and_then(|rec| self.rd_dead_lane_pane(rec));
                match self
                    .rd_open_lane(group, entry, &block, &brief, limits, now, *verify, *body_only)
                {
                    Ok(RdLaneOpen { agent, session, resumed, scope }) => {
                        entry.lane_index = *index;
                        // **#3226: why this lane was re-briefed, where the
                        // answer is not derivable from the row.** A lane
                        // whose pane died with the process is reseeded by
                        // the reconcile, so what arrives here is an
                        // ordinary first-brief-of-a-round — same shape,
                        // same resumed session, same pane kind — and on
                        // #3226s incident the only way to tell a restart
                        // recovery from a normal round was to notice that
                        // no head had moved between them.
                        //
                        // Spent HERE rather than at the end of the tick:
                        // this lane has been re-briefed, and the other
                        // lanes of the same drive have not.
                        let mut why_restart = false;
                        self.rd_spend_restart_mark(group, pr, |mark| {
                            if let Some(k) = mark.lanes.iter().position(|b| b == &block) {
                                mark.lanes.remove(k);
                                why_restart = true;
                            }
                        });
                        if let Some((pane, killed_by)) = replaced {
                            out.audits.push((
                                rddrive::audit_action::LANE_REOPENED,
                                json!({ "pr": pr, "block": block, "head": brief.head,
                                        "pane": pane, "killed_by": killed_by,
                                        "agent": agent, "resumed": resumed }),
                            ));
                        }
                        // #2109: a lane opened is the refusal run ending. Not
                        // folded into `advance` — opening a lane is not an arc
                        // (§2.1), so there is no transition here to hang it on.
                        // #2110: it takes the clock, because ending a run is
                        // what charges its cost to the exclusion totals the
                        // two age bounds subtract.
                        entry.clear_cap_starvation(now);
                        out.changed = true;
                        out.lanes_opened.push((block.clone(), agent.clone()));
                        let mut spawned =
                            // #2109: `head`, `session` and `resumed` on the
                            // row. `resumed` is the fact the issue is about and
                            // the one nothing else records — a resumed lane and
                            // a fresh one produce the same shape of row, the
                            // same kind of pane id, and the same brief, so the
                            // only pre-#2109 way to tell nine panes from six was
                            // to count them by hand across three PRs.
                            // #2508: `scope` beside them, the same string the
                            // brief rendered — a reader counting whole-diff
                            // rounds for a beta's before/after reads it here
                            // rather than re-deriving it from round numbers.
                            json!({ "pr": pr, "block": block, "agent": agent,
                                    "head": brief.head, "session": session,
                                    "resumed": resumed, "scope": scope,
                                    "round": entry.counters.review_rounds + 1 });
                        // #3226, and ABSENT on an ordinary round rather than
                        // null: `why` is a claim about a restart, and a key
                        // present on every row would say nothing on the ones
                        // it is really about. Same shape as `rd-handback`s
                        // own `why`.
                        if why_restart {
                            spawned["why"] = Value::String("restart".to_string());
                        }
                        out.audits.push((rddrive::audit_action::LANE_SPAWNED, spawned));
                    }
                    Err(why) => {
                        // §8's live-delegate-cap row: a refused spawn is a
                        // runner-class outcome. Back off and retry on a later
                        // tick — and NEVER kill a pane to make room (§3.1
                        // item 5).
                        let cap = super::is_live_cap_refusal(&why);
                        out.backoff = true;
                        // **The retry is bounded by something that names the
                        // cap** (#2109). It used to be bounded only by
                        // `drive_timeout_minutes`, whose notice says nothing
                        // about slots: the measured drive sat in `review-wait`
                        // with `lanes: []` for three hours, emitting one of
                        // these rows per tick and no §2.2 exit at all. The stamp
                        // is written on the FIRST cap refusal of a run and left
                        // alone by the cap refusals after it, so `decide` reads
                        // a duration rather than the age of the newest tick.
                        // What keeps it a RUN is the three clears: the Ok arm
                        // below, every state arc, and — since review 4 — the
                        // `else` on this very branch, for any refusal that is
                        // not the cap's.
                        //
                        // **Only a CAP refusal stamps it**, because the hold it
                        // leads to names the cap: a persistent non-cap refusal
                        // (an unknown block, a workspace that will not resolve,
                        // this drive's own duplicate refusal) would park as
                        // `held(cap-full)` on a notice telling an orchestrator to
                        // free a slot that is not the problem — the
                        // diagnosis-for-observation swap #1961 fixed one hold
                        // over. Those keep the bound they have,
                        // `drive_timeout_minutes`, which asserts nothing.
                        //
                        // **And only a cap refusal may LET it stand** (#2109
                        // review 4). Guarding the write alone made the stamp a
                        // latch: nothing on a non-cap refusal re-stamped it and
                        // nothing cleared it, so a single early cap refusal
                        // aged into `held(cap-full)` behind a run of refusals
                        // that were nothing of the kind — the exact outcome the
                        // paragraph above says is prevented, reached from the
                        // read edge instead of the write edge. Worse, the stamp
                        // is the ENTRY's while `first_stale_lane` re-picks the
                        // lane every tick, so a two-lane drive could park on a
                        // stamp left by a lane that is no longer the subject,
                        // while the lane that IS the subject sits in a live pane
                        // the reuse arm could have delivered into for free.
                        //
                        // Clearing on every non-cap refusal closes both, and it
                        // closes the second WITHOUT keying the stamp on a block,
                        // which would be worse than the defect: a drive that
                        // cannot open ANY lane is starved whichever lane the
                        // tick happens to select, and a per-block clock would
                        // restart every time the selection moved — letting a
                        // genuinely starved multi-lane drive evade the bound by
                        // alternating. The cost is that a mixed run restarts the
                        // window at each cap refusal after a non-cap one, which
                        // is the fail-safe direction and is what the word
                        // "continuously" on `HeldReason::CapFull` promises.
                        // #2110: the clear takes the clock, because ending a
                        // run is what CHARGES its cost to the exclusion totals
                        // both age bounds subtract. That matters most on this
                        // site rather than least: a mixed run restarts the
                        // window here, and a restart that forgot what the
                        // previous stretch cost would hand the drive back time
                        // it never had.
                        let moved = if cap {
                            entry.note_cap_starvation(now)
                        } else {
                            entry.clear_cap_starvation(now)
                        };
                        if moved {
                            out.changed = true;
                        }
                        out.audits.push((
                            rddrive::audit_action::REFUSED,
                            json!({ "pr": pr, "block": block, "reason": "lane-spawn-refused",
                                    // #2109: how long the cap has been refusing
                                    // this drive, on the row a reader is already
                                    // looking at. `cap: true` says a slot was
                                    // the problem on THIS tick; a reader chasing
                                    // the starved drive wants the run.
                                    "starved_ms": entry.cap_starved_for(now),
                                    // #1960: whether the CAP is what refused,
                                    // on the row itself. A reader chasing a
                                    // drive that opened no lane for twenty
                                    // minutes was reading a free-text `detail`
                                    // to find out, and the answer decides
                                    // whether anything is wrong at all — a
                                    // capped lane retries and clears itself.
                                    "cap": cap,
                                    "detail": why }),
                        ));
                    }
                }
            }
            reviewdrive::DriveStep::Advance { to, held_reason, .. } => {
                // The CI observation that caused this arc is audited BEFORE the
                // arc is taken, so a green and a red are separate actions in the
                // order they were observed (§5.4: a filter looking for the thing
                // that happened must not match the thing that did not).
                match (entry.state(), obs.ci) {
                    (reviewdrive::DriveState::CiWait, reviewdrive::CiObservation::Green) => {
                        out.audits.push((
                            rddrive::audit_action::CI_GREEN,
                            json!({ "pr": pr, "head": brief.head }),
                        ))
                    }
                    (reviewdrive::DriveState::CiWait, reviewdrive::CiObservation::Red) => {
                        out.audits.push((
                            rddrive::audit_action::CI_RED,
                            json!({ "pr": pr, "head": brief.head, "failing": brief.failing_jobs }),
                        ))
                    }
                    // **Every state the engine lets act on a conflict**, not
                    // `ci-wait` alone (#2311): `decide` reads mergeability above the
                    // per-state logic, so `gate-check` and `review-wait` take the
                    // same arc 3 and owe the same row. It accounts for whichever
                    // step the same tick takes: the `rd-handback` `why:conflict`
                    // while `rebase_attempts` lasts, or the `rd-held`
                    // `rebase-limit` park once it is spent. Either way, an
                    // `rd-handback` `why:conflict` with no `rd-conflicting` above
                    // it is a spent `rebase_attempts` a §5.4 reader cannot
                    // explain, and `scripts/orch-scorecard.cjs` classifies it
                    // only in the generic `rd-*` census — no named case counts
                    // it. `fix-wait` is excluded
                    // by the same explicit clause the engine uses (`state !=
                    // FixWait`, not the arc table): the rebase is already
                    // outstanding there, so no arc is taken and there is nothing
                    // to account for.
                    (st, reviewdrive::CiObservation::Conflicting)
                        if st != reviewdrive::DriveState::FixWait =>
                    {
                        out.audits.push((
                            rddrive::audit_action::CONFLICTING,
                            json!({ "pr": pr, "base": brief.base }),
                        ))
                    }
                    _ => {}
                }
                if let Err(bad) = entry.take(&step, now) {
                    // Unreachable through `decide`, which only proposes arcs the
                    // table names — and handled rather than unwrapped, because an
                    // unwind out of the shared poll thread would take every watch
                    // in the fleet down with it.
                    out.audits.push((
                        rddrive::audit_action::REFUSED,
                        json!({ "pr": pr, "reason": "invalid-transition", "detail": bad.to_string() }),
                    ));
                    return Some(out);
                }
                out.changed = true;
                out.clear_signal = true;
                out.advanced = Some((*to, *held_reason));
                match to {
                    reviewdrive::DriveState::FixWait => {
                        // **#2509's grace, read off the STEP and not off the
                        // counters.** `take` above has already set
                        // `body_only_grace`, so a second derivation here would
                        // read `true` on every later hand-back of this drive
                        // too — which is the grant disagreeing with itself, the
                        // thing `briefed_verify` is placed on the step to avoid.
                        // The bump names this arc and nothing else.
                        let grace = matches!(
                            step,
                            reviewdrive::DriveStep::Advance {
                                bump: Some(reviewdrive::Counter::BodyOnlyGrace),
                                ..
                            }
                        );
                        // Written BEFORE the hand-back, so a grace whose
                        // hand-back then fails still shows the round that was
                        // granted — the arc is what spent it, and `take`
                        // accepted the arc two screens up. The `rd-handback`
                        // row that follows on the `Ok` arm is what says the
                        // worker was actually reached.
                        // #3367 item 1: the driver's OWN non-blocking round,
                        // read off the step for the grace's reason above. The
                        // revision is stamped here, from the facts this tick
                        // read, because `decide` compares the next satisfied
                        // gate against it — a worker that changes nothing does
                        // not buy a second identical round.
                        let nit = matches!(
                            step,
                            reviewdrive::DriveStep::Advance {
                                bump: Some(reviewdrive::Counter::NonblockingRound),
                                ..
                            }
                        );
                        if nit {
                            entry.nit_head = brief.head.clone();
                            entry.nit_digest = brief.body_digest.clone();
                            out.audits.push((
                                rddrive::audit_action::AUTO_HANDBACK,
                                json!({ "pr": pr, "head": brief.head,
                                        "round": entry.nit_rounds,
                                        "of": limits.fix_nonblocking_rounds,
                                        "review_rounds": entry.counters.review_rounds,
                                        "residual": rddrive::residual_text(&brief.lane_notices) }),
                            ));
                        }
                        if grace {
                            out.audits.push((
                                rddrive::audit_action::ROUND_GRACE,
                                json!({ "pr": pr, "head": brief.head,
                                        "block": brief.deciding_lane.clone()
                                            .unwrap_or_else(|| brief.failing_lane()),
                                        "round": entry.counters.review_rounds,
                                        "reason": "body-only" }),
                            ));
                        }
                        match self.rd_handback(group, entry, &brief, limits, grace) {
                            Ok((agent, pane)) => {
                                // A hand-back that WORKED ends the second-failure
                                // count (#2555 item 2): the next failure, whenever it
                                // comes, is a first one again.
                                self.rd_handback_fails.lock_safe().remove(&(group.clone(), pr));
                                out.handback = Some(agent.clone());
                                out.audits.push((
                                    rddrive::audit_action::HANDBACK,
                                    json!({ "pr": pr, "agent": agent, "head": brief.head,
                                            "why": brief.handback_kind(),
                                            // #3203: whether this hand-back put a
                                            // NEW pane on the session, and if not,
                                            // which arm kept it from doing so.
                                            "pane": pane }),
                                ));
                            }
                            Err(why) => {
                                self.rd_handback_failed(group, entry, pr, &why, &mut out, now);
                            }
                        }
                    }
                    reviewdrive::DriveState::GateCheck | reviewdrive::DriveState::Satisfied => {
                        for l in &brief.lane_notices {
                            out.audits.push((
                                rddrive::audit_action::VERDICT,
                                json!({ "pr": pr, "block": l.block,
                                        "verdict": l.verdict.as_str(), "head": brief.head }),
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        // **#2811 S10: the restart mark is spent by the tick that ACTED on it**,
        // never merely by the one that read it (#3196 review 2).
        //
        // Two ways to have acted, and both are about the drive rather than about
        // this tick's luck: the worker was re-briefed, or the drive is no longer
        // in `fix-wait` at all — arc 7 or 8, where a worker that pushed or
        // reported before the shutdown has already answered and no re-brief is
        // owed, or a hold, which is a decision surface for the orchestrator
        // rather than a wait this mark can shorten.
        //
        // Everything else LEAVES IT STANDING, which is the fix: a tick that could
        // not read the PR, or that was preempted by a bound above
        // `decide_fix_wait`, has decided nothing about the restart, and the next
        // tick is owed the same re-brief. That cannot loop — the first tick that
        // succeeds sets `re_briefed` and discharges it, which is what
        // `the_restart_mark_is_spent_by_the_tick_that_reads_it` pins.
        //
        // **Per FIELD since #3225/#3226**, because a drive can carry more
        // than one and a tick that acted on the lanes has decided nothing
        // about the worker. Each clause below is the same rule read for its
        // own recovery: acted, or the drive has left the state that recovery
        // was about.
        // Read once, before the closure borrows nothing of the entry.
        let here_after = entry.state();
        self.rd_spend_restart_mark(group, pr, |mark| {
            if re_briefed || here_after != reviewdrive::DriveState::FixWait {
                mark.handback = false;
            }
            // #3225: the arc out of `ci-wait` IS the act — it briefs the
            // lane at the pushed head, which is the whole recovery. A tick
            // that could not read the PR leaves `ci-wait` standing and the
            // mark with it, for `a_restart_tick_that_cannot_read_the_pr…`s
            // reason one field over.
            if here_after != reviewdrive::DriveState::CiWait {
                mark.push_delivered = false;
            }
            // #3226: a lane is spent by the OpenLane arm that re-briefed it
            // (above), so what is left here is the drive having left
            // `review-wait` — where no lane of this round will be opened at
            // all and the why has nothing to ride on.
            if here_after != reviewdrive::DriveState::ReviewWait {
                mark.lanes.clear();
            }
        });

        // **THE HEAD, PERSISTED — the line two reviewers named on S1 as the one
        // that would be forgotten.** `DriveEntry::head` is only ever *compared*
        // against the live head (arc 6 in `review-wait`, arc 7 in `fix-wait`), so
        // a tick that records it once at `drive_review` time and never again
        // makes that comparison permanently true: the drive takes arc 6 to
        // `ci-wait`, goes green, comes back to `review-wait`, takes arc 6 again,
        // forever — a PR that is never reviewed and never gated.
        //
        // **After `decide`, never before.** Arc 6 *is* `entry.head !=
        // facts.head`; writing first would make the two equal before anything
        // compared them, and the arc would be unreachable rather than permanent.
        //
        // **And only when the head actually resolved.** An empty `facts.head` is
        // a FAILED READ, not a head — the same class as `fix_handback_ms == 0`
        // meaning "ancient" rather than "unset". Writing it would leave every
        // later tick comparing a real live head against a stored `""`, taking
        // arc 6 on every wake; and in `review-wait` a stored `""` makes
        // `lane_open_for` refuse every record briefed at a real head, which is
        // `OpenLane{k}` on every tick — a reviewer spawned per tick, each brief
        // re-arming `spawned_ms` so `lane-stalled` can never fire. `decide`
        // refuses to dispatch at all on an empty LIVE head, which bounds the
        // damage this tick; this is what stops the ENTRY being poisoned so the
        // next read, successful or not, still misbehaves.
        if !obs.head.is_empty() && entry.head != obs.head {
            entry.head = obs.head.clone();
            // **A further push while the drive is already waiting on one**
            // (#2168 E1). In `ci-wait` on an arc-7 head there is no arc to fire
            // — `transition` refuses a self-arc — so without this the receipts
            // wait keeps running from the FIRST push and a worker that pushes a
            // follow-up commit late in the window has minutes to run a fresh
            // matrix and report. `note_fix_push` re-stamps only an anchor that
            // already exists, so this cannot start the wait in a state that
            // never entered it, and it is placed inside the head-actually-moved
            // guard above so a failed `gh` read can never renew it.
            entry.note_fix_push(now);
            out.changed = true;
        }
        // **What bounds the superseded-pane lists (#1871 B2, rev-final).** They
        // are pruned by LIVENESS and never by size: a size cap can only evict by
        // age, and the oldest superseded pane is one that is still running, still
        // on this session and still able to `report` — so evicting it un-owns it
        // exactly as the single slot did, which is B2 reproduced by the record
        // that fixes B2. `DriveEntry::forget_dead_panes` argues why a DEAD pane
        // is safe to forget instead.
        //
        // Liveness is the registry's fact, so the predicate is supplied here
        // rather than being reached for next door. `agent()` answers `None` for
        // an id that is gone; both that and `Dead` are states in which
        // `resolve_token` refuses the caller, so neither can reach the MCP seam.
        if entry.forget_dead_panes(&|id| self.rd_pane_is_live(id)) {
            out.changed = true;
        }
        if let Some(d) = obs.body_digest.as_deref() {
            if entry.body_digest != d {
                entry.body_digest = d.to_string();
                out.changed = true;
            }
        }
        // The exits (§2.2). Built here, where the entry's counters and lanes are
        // in hand; delivered by the caller, outside the lock.
        //
        // **A TERMINAL exit's notice is written ONTO the entry, inside this same
        // load-decide-store, and delivered from there** (#1857). It is not
        // handed to the caller as a string, because a string handed to a
        // delivery that answers `Err` is gone — and §5.2's retention then drops
        // the only record that could reproduce it, which is a drive that ends
        // with nothing in the pane and nothing to say why. `owe_notice` makes
        // the obligation durable before anything attempts it, and
        // `prune_terminal` will not drop an entry that still owes one.
        //
        // A `held` exit keeps the direct path deliberately, and the asymmetry is
        // the one §5.2 already draws: a parked entry is NEVER pruned, so the
        // drive survives its own lost notice — `review_drive_status()` lists it
        // and §2.3's resume re-reads it. What a hold can lose is a line; what a
        // terminal exit loses is the whole record. The mechanism is here if a
        // later change wants the stronger guarantee for a hold too.
        if let Some((to, reason)) = out.advanced {
            match (to, reason) {
                (reviewdrive::DriveState::Satisfied, _) => {
                    let n = rddrive::satisfied_notice(
                        pr,
                        &entry.head,
                        &entry.body_digest,
                        &brief.lane_notices,
                        &entry.counters,
                        &self.rd_surviving_panes(entry),
                        &released_worker_session,
                    ) + &rddrive::nonblocking_clause(
                        entry.nit_rounds,
                        limits.fix_nonblocking_rounds,
                        &brief.lane_notices,
                    );
                    // #3367 item 5: the CLEAN case — decided off the facts
                    // `decide` saw, never re-read. Where the merge queue is on,
                    // the submission itself waits for this tick's write (see
                    // `RdOut::clean_enqueue`) and appends the queue's answer to
                    // the notice owed here, before the flush delivers it.
                    let route = if !reviewdrive::gate_is_clean(&facts) {
                        rddrive::CleanRoute::NotClean
                    } else if self.merge_queue_enabled(group) {
                        rddrive::CleanRoute::Queue
                    } else {
                        rddrive::CleanRoute::Notice
                    };
                    let n = n + &rddrive::clean_clause(route);
                    let n = Self::rd_fold_auto_report(entry, n);
                    out.audits.push((
                        rddrive::audit_action::SATISFIED,
                        json!({ "pr": pr, "head": entry.head }),
                    ));
                    match route {
                        rddrive::CleanRoute::NotClean => {}
                        rddrive::CleanRoute::Notice => out.audits.push((
                            rddrive::audit_action::CLEAN,
                            json!({ "pr": pr, "head": entry.head, "route": "notice" }),
                        )),
                        // Its `rd-clean` row is written where the queue's
                        // answer is known, so the row records what happened.
                        rddrive::CleanRoute::Queue => out.clean_enqueue = Some(entry.head.clone()),
                    }
                    out.owed_text = Some(n.clone());
                    entry.owe_notice(&n, now);
                }
                (reviewdrive::DriveState::Held, Some(r)) => {
                    let refusal = out.refusal.clone();
                    // #2811 S5b: the provider that DECIDED this hold, off the
                    // facts `decide` saw rather than re-read from the map. A
                    // second read could report a provider that recovered
                    // between the decision and the notice, which is a line
                    // contradicting the hold beside it.
                    let provider = facts.provider_limited.clone().unwrap_or_default();
                    let n = rddrive::held_notice(
                        pr,
                        r,
                        &brief.held_facts(entry, limits, r, &messaged_by, &refusal, &provider),
                    );
                    if r == reviewdrive::HeldReason::ProviderLimit {
                        out.provider_limited = Some(provider.clone());
                    }
                    // The refusal rides the `rd-held` row rather than a
                    // `rd-refused` row of its own, and only when there is one:
                    // it is a detail OF this hold, and a separate row pushed
                    // where the refusal was learned would be a claim about an
                    // arc a later condition (age, a closed PR) could still
                    // outrank — §5.4's "a filter looking for the thing that
                    // happened must not match the thing that did not".
                    let mut detail =
                        json!({ "pr": pr, "reason": r.as_str(), "head": entry.head });
                    if !refusal.is_empty() {
                        detail["refusal"] = Value::String(refusal);
                    }
                    out.audits.push((rddrive::audit_action::HELD, detail));
                    // **The hold is recorded either way; only the LINE is
                    // conditional** (#3040 N1). A hold can repeat only after a
                    // resume — `transition` refuses a `held` -> `held`
                    // self-arc — and where that resume changed nothing the
                    // drive can observe, the second line says exactly what the
                    // first did. `announce_hold` owns the comparison and the
                    // stamp together, so a notice that is not built cannot
                    // silence the next one; `rd-held` above is written
                    // whatever it answers, because the hold HAPPENED and §5.4
                    // is a record of what happened, not of what was said.
                    //
                    // **The LINE is what the key digests** (rev-final round 3),
                    // so a repeat whose refusal changed — a hand-back that
                    // failed differently, a cap refusal quoting a roster that
                    // has moved — is a different key and announces. The tuple
                    // alone could not see any of that.
                    if entry.announce_hold(r, &n) {
                        // #2811 S5b: a provider limit's line does NOT go out
                        // per drive. `announce_hold` still runs, because it
                        // is what stamps the hold and keeps a repeat from
                        // re-announcing; only the destination changes. The
                        // aggregated line is built once in `rd_tick_group`
                        // from `out.provider_limited`, and this drive's own
                        // wording still reaches its board task there.
                        if r != reviewdrive::HeldReason::ProviderLimit {
                            // #3367 round-3 residual (rev-final on #3371): a
                            // HOLD's line is delivered fire-and-forget, so the
                            // report is folded in WITHOUT being taken. It is
                            // cleared by the delivery loop only once this line
                            // actually landed (`RdOut::report_folded`); a line
                            // that did not land leaves it for the next notice
                            // to carry. Taking it here dropped the worker's
                            // words from the pane whenever the delivery failed.
                            out.report_folded = entry.auto_report.is_some();
                            out.notices
                                .push(Self::rd_fold_text(n, entry.auto_report.as_deref()));
                        } else {
                            out.provider_note = Some(n);
                        }
                    } else {
                        out.audits.push((
                            rddrive::audit_action::HOLD_REPEATED,
                            json!({ "pr": pr, "reason": r.as_str(), "head": entry.head, "notice": n }),
                        ));
                    }
                    out.changed = true;
                }
                (reviewdrive::DriveState::Cancelled, _) => {
                    out.audits.push((rddrive::audit_action::CANCELLED, json!({ "pr": pr })));
                    // Replace-vs-augment, resolved as reconcile's is: #1871 B3's
                    // panes thread into the construction, and #1857's owe
                    // replaces the direct push rather than sitting beside it.
                    let panes = self.rd_surviving_panes(entry);
                    let n = rddrive::cancelled_notice(
                        pr,
                        rddrive::CancelCause::PrGone,
                        &panes,
                        &released_worker_session,
                    );
                    let n = Self::rd_fold_auto_report(entry, n);
                    out.owed_text = Some(n.clone());
                    entry.owe_notice(&n, now);
                }
                _ => {}
            }
        }
        out.on_behalf_of = on_behalf;
        Some(out)
    }

    // ---------- §5.1's three MCP tools ----------

    /// `drive_review(pr, worker_session, reset_counters?, rounds_already_spent?)`
    /// — §3.2's second key, and the one call an orchestrator makes to start a
    /// drive (a plan drive's hand-off and #3367's auto-start reach it too).
    ///
    /// **Never automatic by default**, and in particular it does not fire on a
    /// worker's `report(done)`: INVARIANT 8 makes *what starts* the
    /// orchestrator's call, and the PRs where a drive is wrong are ordinary — a
    /// scratch or red-evidence PR, a release bump, a PR the human said they
    /// would read themselves. The one opt-in is `driver.auto_drive_on_done`
    /// (#3367), which reaches this function through
    /// [`rd_auto_start`](Self::rd_auto_start) and is argued there.
    ///
    /// **The session is resolved once, and what is persisted is what came
    /// back** (§3.2). `resolve_session_ref` is a resolution against *this
    /// group's roster at the moment of the call*: a prefix that resolves
    /// uniquely today can become ambiguous tomorrow as the roster grows, and the
    /// entry outlives both the call and the process. So the **resolved** id goes
    /// into `review_drives.json`, never the caller's raw string.
    ///
    /// Refusals are §5.1's closed vocabulary, in two classes: the driver
    /// declining, and orrerix having failed. The order they are checked in is
    /// cheapest-first among equals, with one exception that is not: the PR's
    /// state is read **last** among the checks, because it is the only one that
    /// spends a `gh` round trip.
    #[doc(hidden)] // pub for integration tests
    pub fn drive_review(
        &self,
        group: &GroupId,
        pr: u64,
        worker_session: &str,
        reset_counters: bool,
        rounds_already_spent: u32,
        on_behalf_of: &str,
    ) -> Value {
        let injected = self.rd_runner_override.lock_safe().clone();
        let owned;
        let runner: &dyn rddrive::RdRunner = match injected.as_deref() {
            Some(r) => r,
            None => {
                let Some(repo) = self.group(group).map(|g| g.repo) else {
                    return self.rd_refuse(group, pr, rddrive::refusal::UNAVAILABLE);
                };
                owned = rddrive::runner_for(std::path::Path::new(&repo));
                &owned
            }
        };
        self.drive_review_with(
            group,
            runner,
            pr,
            worker_session,
            reset_counters,
            rounds_already_spent,
            on_behalf_of,
            now_ms(),
        )
    }

    /// [`drive_review`](Self::drive_review) with the `gh` seam injected.
    #[doc(hidden)] // pub for integration tests
    #[allow(clippy::too_many_arguments)]
    pub fn drive_review_with(
        &self,
        group: &GroupId,
        runner: &dyn rddrive::RdRunner,
        pr: u64,
        worker_session: &str,
        reset_counters: bool,
        rounds_already_spent: u32,
        on_behalf_of: &str,
        now: u64,
    ) -> Value {
        self.drive_review_seeded(
            group,
            runner,
            pr,
            worker_session,
            reset_counters,
            rounds_already_spent,
            on_behalf_of,
            now,
            None,
        )
    }

    /// [`drive_review_with`](Self::drive_review_with), plus the worker report
    /// that started the drive when one did (#3367 item 2).
    ///
    /// **The report is written onto the entry in the SAME store that creates
    /// it**, not by a second write after the call returns: between two writes
    /// a tick could take the drive's first step, and a hold on that step would
    /// build the first notice with nothing to fold into it. One write is the
    /// only ordering that cannot lose the words.
    #[allow(clippy::too_many_arguments)]
    fn drive_review_seeded(
        &self,
        group: &GroupId,
        runner: &dyn rddrive::RdRunner,
        pr: u64,
        worker_session: &str,
        reset_counters: bool,
        rounds_already_spent: u32,
        on_behalf_of: &str,
        now: u64,
        auto_report: Option<String>,
    ) -> Value {
        use rddrive::refusal as r;
        if !self.driver_enabled(group) {
            return self.rd_refuse(group, pr, r::DRIVER_DISABLED);
        }
        // The session, resolved once (§3.2). An empty string gets its own name
        // rather than leaking `resolve_session_ref`'s untagged prose.
        if worker_session.trim().is_empty() {
            return self.rd_refuse(group, pr, r::RESUME_SESSION_EMPTY);
        }
        let session = match resolve_session_ref(&self.merged_records(group), worker_session) {
            Ok(s) => s,
            Err(e) if e.starts_with("resume-ambiguous") => {
                return self.rd_refuse(group, pr, r::RESUME_AMBIGUOUS)
            }
            Err(_) => return self.rd_refuse(group, pr, r::RESUME_NOT_FOUND),
        };
        // **The block the hand-back would resume under, resolved AT the call**
        // (#2819 (g), S7) — see [`rd_unhandbackable_block`]. Accepting a
        // session whose block is the orchestrator's or the manager's, or one
        // this roster no longer declares, cost #2819 three `worker-unresumable`
        // holds and three orchestrator turns for a PR that could never be
        // handed back; this is the one refusal instead, quoting the SAME
        // sentence the hold would have carried.
        if let Some(why) = self.rd_unhandbackable_block(group, &session) {
            return self.rd_refuse_detail(group, pr, r::WORKER_UNRESUMABLE, &why);
        }
        // The gate, from the same two files the shim reads. `gate-unreadable` is
        // NOT `gate-not-configured`: a wrong label sends the reader somewhere
        // else, which is #681's own lesson and the queue's posture.
        let spec = match self.merge_queue_gate(group) {
            Ok(s) => s,
            Err(_) => return self.rd_refuse(group, pr, r::GATE_UNREADABLE),
        };
        let gate = match &spec {
            mergeq::GateSpec::Declared(g) => g.clone(),
            mergeq::GateSpec::Malformed => return self.rd_refuse(group, pr, r::GATE_UNREADABLE),
            mergeq::GateSpec::Absent => {
                return self.rd_refuse(group, pr, r::GATE_NOT_CONFIGURED)
            }
        };
        // A gate requiring a reviewer the roster does not declare is answerable
        // here, from two files. Left unanswered it becomes `held(lane-stalled)`
        // an hour later instead of an immediate refusal.
        if let Some(g) = self.group(group) {
            if !workflow::gate_missing_blocks(&gate, &g.guardrails.blocks).is_empty() {
                return self.rd_refuse(group, pr, r::GATE_NAMES_NO_SUCH_BLOCK);
            }
        }
        // §8.1's mutual refusal: the two loops both move a PR's head and both
        // read its verdicts, and neither was designed expecting the other to be
        // doing so concurrently. The intended sequence is serial and has a
        // direction — a drive ends at `satisfied`, the orchestrator dispositions
        // the findings, and *then* it queues.
        if let Ok(q) = mqloop::load_state(&self.group_dir(group)) {
            if q.entry(pr).map(|e| !e.state().is_terminal()).unwrap_or(false) {
                return self.rd_refuse(group, pr, r::IN_MERGE_QUEUE);
            }
        }
        // **`already-driven`, checked once cheaply BEFORE the `gh` call.** A
        // second `drive_review` on a live PR is the ordinary duplicate — an
        // orchestrator retrying, or re-reading its own state after a compact —
        // and spending a `gh` round trip to answer it is a round trip on the
        // loop that also delivers every `notify_when` notice in the fleet. The
        // AUTHORITATIVE check is still the one under the lock below: this read
        // is unsynchronized, so it can only ever be stale in the direction of
        // doing more work, never of starting a second drive.
        //
        // **Unless this drive has lost a pane** (#3226). A `drive_review` on
        // a live drive whose panes all died is not a duplicate at all — it
        // is an orchestrator asking for exactly the recovery the locked arm
        // below performs, and refusing it was why the only manual route out
        // of #3226s incident was `cancel_review_drive` plus a fresh drive,
        // which loses the counters. The predicate is an agent-map read, so
        // this stays a cheap check with no round trip in it.
        if reviewdrive::load_state(&self.group_dir(group))
            .map(|s| {
                s.is_driven(pr)
                    && !s.entry(pr).is_some_and(|e| self.rd_has_lost_panes(e))
            })
            .unwrap_or(false)
        {
            return self.rd_refuse(group, pr, r::ALREADY_DRIVEN);
        }
        // Last, because it is the only check that spends a `gh` round trip — and
        // it spends exactly ONE: `drive_review` reads whether the PR is open and
        // nothing else, so `observe_pr`'s second call on `gh pr checks` would be
        // an answer this path never looks at.
        match rddrive::pr_is_open(runner, pr) {
            Some(true) => {}
            Some(false) => return self.rd_refuse(group, pr, r::PR_NOT_OPEN),
            // The remote did not answer. Unknown is never treated as safe, and
            // it is never treated as a fact about the PR either.
            None => return self.rd_refuse(group, pr, r::PR_UNVERIFIABLE),
        }
        let dir = self.group_dir(group);
        // Notices owed by an entry this call is about to displace — audited
        // after the lock is dropped, for `rd_audit`'s own reason (#1857).
        let mut dropped_notices: Vec<(u64, String)> = Vec::new();
        let (audit_action, detail) = {
            let _state_guard = self.rd_state_lock.lock_safe();
            let mut state = match reviewdrive::load_state(&dir) {
                Ok(s) => s,
                // NOT `not-driven`: that would assert something orrerix cannot
                // know, and `already-driven` is unevaluable here, so an unnamed
                // failure becomes a second drive on one PR.
                Err(_) => return self.rd_refuse(group, pr, r::STATE_UNREADABLE),
            };
            if state.is_driven(pr) {
                // **A live drive whose panes are gone RESUMES rather than
                // refusing** (#3226).
                //
                // The refusal is right for the ordinary duplicate — an
                // orchestrator retrying, or re-reading its own state after a
                // compact — and stays for it: a drive whose panes are all live
                // has nothing here to repair and falls through to
                // `already-driven` below. What it was wrong for is the drive
                // this call is really about, where every pane died with a
                // previous process and the only recovery left was
                // `cancel_review_drive` plus a fresh `drive_review`, which
                // starts the counters over — three lost review rounds on
                // #3226s incident.
                //
                // **The repair is the reconcile s, called rather than
                // re-spelled**, so the two cannot answer the pane question
                // differently: drop what the roster does not have, keep every
                // session, and leave the marks the tick acts on. No arc, no
                // counter, and the state is untouched — this is not arc 11,
                // which is `held`s resume and is the branch below.
                let Some(entry) = state.entry_mut(pr) else {
                    return self.rd_refuse(group, pr, r::STATE_UNREADABLE);
                };
                let lost = self.rd_forget_lost_panes(group, entry);
                // **A superseded pane is not a loss to recover from** (#3228
                // review 2, W1) — see `RdLostPanes::current_panes_lost`. The
                // repair below is not free: on a `fix-wait` drive it marks a
                // re-hand-back on the state alone, and a duplicate call that
                // reached it would re-brief a live worker mid-fix.
                if !lost.current_panes_lost() {
                    return self.rd_refuse(group, pr, r::ALREADY_DRIVEN);
                }
                let mut mark = RestartMark::default();
                match entry.state() {
                    reviewdrive::DriveState::FixWait => mark.handback = true,
                    reviewdrive::DriveState::CiWait
                        if entry.fix_pushed() && !lost.worker.is_empty() =>
                    {
                        mark.push_delivered = true
                    }
                    _ => {}
                }
                mark.lanes = lost.lanes.iter().map(|(block, _)| block.clone()).collect();
                let here = entry.state();
                if reviewdrive::store_state(&dir, &state).is_err() {
                    return self.rd_refuse(group, pr, r::STATE_UNWRITABLE);
                }
                // Marked only once the drop is DURABLE. A mark whose pane the
                // record still names would re-brief a lane the next reconcile
                // would then re-brief again.
                if !mark.is_empty() {
                    self.rd_restart_handback.lock_safe().insert((group.clone(), pr), mark);
                }
                self.rd_audit(
                    group,
                    on_behalf_of,
                    rddrive::audit_action::RECOVERED,
                    json!({ "pr": pr, "at": "drive_review",
                            "panes_dropped": lost.worker.len() + lost.lanes.len() }),
                );
                // Serviced on the very next wake, for the same reason the
                // accepted path below clears it.
                self.rd_service_ms.lock_safe().remove(group);
                return json!({ "driving": true, "state": here.as_str(),
                               "recovered": "panes" });
            }
            let resumed = match state.entry(pr).map(|e| e.state()) {
                // A parked drive RESUMES, carrying its counters — §2.3's
                // default, and the whole reason §2.1 makes `held` parked rather
                // than terminal. Clearing them is an explicit, audited argument.
                Some(reviewdrive::DriveState::Held) => true,
                _ => false,
            };
            if resumed {
                let entry = match state.entry_mut(pr) {
                    Some(e) => e,
                    None => return self.rd_refuse(group, pr, r::STATE_UNREADABLE),
                };
                if reset_counters {
                    entry.counters = reviewdrive::Counters::seeded(rounds_already_spent);
                    // #3367: a fresh budget is a fresh budget for the driver's
                    // own non-blocking rounds too — and the revision the last
                    // one was handed back at is no longer a reason to refuse
                    // the next, since the orchestrator has just paid for it.
                    entry.nit_rounds = 0;
                    entry.nit_head.clear();
                    entry.nit_digest.clear();
                    // **And the hold-notice dedup is re-armed with them**
                    // (#3040 N1). A resetting resume buys the drive a fresh
                    // budget, so the next hold at the same bound is a hold
                    // about a round the orchestrator paid for — and the key
                    // cannot see that, because the counters are back at the
                    // values the previous hold carried. Only THIS resume
                    // clears it: a plain one changes nothing the drive can
                    // observe, and its re-hold is the repeat N1 suppresses.
                    entry.rearm_hold_notice();
                }
                // Read BEFORE `advance`, which clears `held_reason` on the way
                // out of `held`. Used by the lane re-open below.
                let was_lane_stalled =
                    entry.held_reason == Some(reviewdrive::HeldReason::LaneStalled);
                if entry
                    .advance(reviewdrive::DriveState::CiWait, None, None, now)
                    .is_err()
                {
                    return self.rd_refuse(group, pr, r::STATE_UNREADABLE);
                }
                // **The age clocks restart on a resume, or arc 11 is a no-op for
                // exactly the holds it exists to recover.**
                //
                // §2.2 makes `drive-stalled` the drive's AGE — `now -
                // started_ms`, "never an idle clock reset by each state
                // advance" — and `decide` checks it BEFORE any per-state logic.
                // Left alone, a drive parked longer than `drive_timeout_minutes`
                // re-holds `drive-stalled` on its very first tick after the
                // resume, and every hold a human takes their time over is
                // exactly that old. Four shipped surfaces promise the opposite.
                //
                // Resetting HERE does not reintroduce the idle clock that row
                // forbids: the ban is on a stamp written by each state advance,
                // and nothing on the tick path touches this. It moves only on a
                // deliberate, role-gated, audited `drive_review` — the same
                // event §2.3 already lets clear the counters. A drive being
                // restarted is a drive whose age starts again; the counters,
                // which are the budget, still carry unless `reset_counters` says
                // otherwise.
                //
                // Each lane's `spawned_ms` is re-armed for the same reason and
                // by the same argument: `lane-stalled` fires at 60 minutes, so
                // without this a resumed drive re-holds on the FIRST tick for a
                // lane the orchestrator has just looked at and chosen to resume.
                entry.started_ms = now;
                // #2110: and with it the starvation the age bound was going
                // to forgive. The exclusion is a credit against THIS age; a
                // resume starts the age over, so carrying the credit would
                // hand the new run a head start it did not earn. The
                // per-state clock and its own accumulator are reset by the
                // `advance` above, on the arc, which is where every arc
                // resets them.
                entry.starved_total_ms = 0;
                for l in entry.lanes.iter_mut() {
                    l.spawned_ms = now;
                }
                // **`lane-stalled` needs its lane RE-BRIEFED, not merely
                // re-timed** — and the clock re-stamp just above is what makes
                // that visible rather than fixing it. `decide_review_wait`
                // re-opens a lane only when `lane_open_for` is false, and at a
                // stable head it stays true, so the lane the notice named is
                // never spoken to again: before the re-stamp the drive re-held
                // instantly, after it the drive waits the full
                // `lane_timeout_minutes` in silence and re-holds then. Neither
                // is the recovery `held(lane-stalled)`'s own notice instructs,
                // and a hold ON A WAIT that its printed remedy cannot clear is
                // the defect arc 11 exists to not have. Holds parked on a
                // JUDGMENT are deliberately outside that rule — §2.2 names the
                // two and why resuming them re-holds by design.
                //
                // Clearing `briefed_head` puts the outstanding lane back in
                // `lane_open_for`'s false branch, so the next tick takes
                // `OpenLane`. `rd_open_lane` resumes the session recorded for
                // that lane when there is one and spawns a fresh reviewer when
                // there is not; either way the record is re-pointed at the pane
                // that now holds the lane, so §7's interception stays keyed on a
                // live pane rather than on an abandoned one.
                //
                // Scoped to this hold because it is the only one it can change.
                // A lane holding `escalate` or `review-limit` carries a verdict
                // that `decide_review_wait` answers before it ever consults the
                // lane record — SO LONG AS that verdict is still bound to the
                // revision in front of it, which is the half #1871 B1 added and
                // this sentence used to state flat. Once the head has moved the
                // verdict decides nothing, `lane_verdict_is_current` reads it as
                // absent, and the lane is re-briefed by the ordinary path with
                // no clearing needed here. Either way clearing in this arm would
                // be a no-op; and a lane that is legitimately mid-review must not
                // be re-briefed merely because some OTHER hold on the same drive
                // was resumed.
                if was_lane_stalled {
                    for l in entry.lanes.iter_mut() {
                        l.briefed_head.clear();
                    }
                }
                // **A new session means the recorded PANES are stale**, and a
                // stale pane is not merely useless — it is an interception key.
                // `driven_role` matches on `worker_agent` and on every pane it
                // superseded, so leaving them would have this drive consume the
                // traffic of a worker it no longer owns, while the worker it
                // DOES own reports to the orchestrator as if undriven. Cleared
                // on a change, kept when the orchestrator resumes with the same
                // session (the common case), where the panes are still this
                // worker's.
                if entry.worker_session != session {
                    entry.forget_worker_panes();
                }
                entry.worker_session = session.clone();
                // **Re-read on every resume** (#3250). The panes a drive was
                // STARTED ON are the ones its terminal release may end, and a
                // resume is a fresh start on whatever the session is carrying
                // now: the pane recorded at the first start may be long dead,
                // and the orchestrator may have resumed the conversation into a
                // new one before handing it back. `record_founding_panes` is
                // total, so this replaces rather than accumulates.
                entry.record_founding_panes(self.live_panes_on_session(group, &session));
                entry.on_behalf_of = on_behalf_of.to_string();
            } else {
                // A `satisfied` or `cancelled` entry that retention has not yet
                // pruned starts a FRESH drive with fresh counters — the queue's
                // own "comes back as a NEW entry" behaviour.
                //
                // **If that entry still owed a notice, this is the one other way
                // one is given up on** (#1857), and it is deliberately not
                // silent: the retained entry becomes reachable far more often
                // now that retention holds it for an undelivered notice, and a
                // re-drive discarding the previous drive's ending would be the
                // same silence with a different cause. Audited with the text,
                // exactly as the ceiling is, so the record survives the entry.
                // The notice is NOT carried onto the new entry: it describes a
                // drive that is over, and delivering it beside a fresh drive's
                // own traffic would read as this drive ending.
                let superseded: Vec<(u64, String)> = state
                    .entries
                    .iter()
                    .filter(|e| e.pr == pr)
                    .filter_map(|e| e.owed_notice().map(|n| (e.pr, n.text.clone())))
                    .collect();
                for (dropped_pr, text) in superseded {
                    dropped_notices.push((dropped_pr, text));
                }
                // **The lanes' CONVERSATIONS survive the entry that held them**
                // (#2153). Lane memory lives only here, so dropping the entry
                // used to drop it — and the sequence a satisfied gate is
                // designed to produce (satisfied, the orchestrator dispositions
                // the findings, a re-drive at the new head) is the ordinary
                // path, not an edge. Measured on PR #2141: two lanes with live,
                // resolvable sessions that had already read the PR once, both
                // re-opened `resumed=false`, on the round where the warm session
                // is cheapest.
                //
                // Read BEFORE the `retain` that discards them, and through
                // `rd_lane_session` rather than off `LaneRecord::session`: that
                // field is what the spawn RETURNED, which is a session id only
                // on a CLI that pre-assigns one, so seeding from it alone would
                // drop exactly the copilot and opencode lanes #2109 was about.
                // A lane with no session to carry from any of the three sources
                // is seeded not at all — an absent record is already "open this
                // one fresh", and a seeded record with nothing to resume would
                // only make `rd_open_lane` audit a resume failure for a session
                // that was never recorded.
                //
                // A seeded session that no longer resolves needs nothing here:
                // `rd_open_lane`'s existing `rd-lane-resume-failed` arm audits
                // the fall-through and spawns fresh, exactly as it does for a
                // lane reaped inside a live drive.
                //
                // `rd_lane_session` under `rd_state_lock` is the established
                // order, not a new one: `rd_step_entry` runs the whole
                // load-decide-store — that call and the spawn after it included
                // — under this same lock (§2.4).
                let seeded: Vec<reviewdrive::LaneRecord> = state
                    .entries
                    .iter()
                    .filter(|e| e.pr == pr)
                    .flat_map(|e| e.lanes.iter())
                    .filter_map(|l| {
                        self.rd_lane_session(group, Some(l)).map(|s| l.reseeded(&s))
                    })
                    .collect();
                state.entries.retain(|e| e.pr != pr);
                // A fresh drive is a fresh second-failure count (#2555 item 2):
                // the displaced entry's hand-back failures were ITS history, and
                // a new drive on the same PR with the same session starts with
                // one honest chance to succeed before the hold says anything.
                self.rd_handback_fails.lock_safe().remove(&(group.clone(), pr));
                // **The clock is the caller's, and that is what makes the age
                // bound testable at all.** `started_ms` is the anchor §2.2
                // measures `drive-stalled` from, and stamping it from the wall
                // clock here while the tick advances on an injected `now` put
                // the two on different scales: `age_ms` saturated to zero for
                // every synthetic clock, so `drive-stalled` could not fire in a
                // test and never had. That is most of why B2 shipped.
                let mut fresh = reviewdrive::DriveEntry::new(
                    pr,
                    &session,
                    on_behalf_of,
                    reviewdrive::Counters::seeded(rounds_already_spent),
                    now,
                );
                // #2153. Assigned after construction rather than threaded
                // through `DriveEntry::new`, which is arc 1 — a drive is CREATED
                // with no lanes, and a constructor that could be handed some
                // would make "a fresh drive has reviewed nothing" a caller's
                // discipline instead of the type's. The counters stay as
                // `rounds_already_spent` says: a warm conversation is not a
                // spent round.
                fresh.lanes = seeded;
                // The panes this drive is being handed (#3250) — see the same
                // call on the resume arm above.
                fresh.record_founding_panes(self.live_panes_on_session(group, &session));
                // #3367 item 2 — see `drive_review_seeded`. Only the FRESH arm
                // carries it: the auto-start refuses every PR with a live or
                // parked entry, so it never reaches the resume arm above.
                fresh.auto_report = auto_report.clone();
                state.entries.push(fresh);
            }
            if reviewdrive::store_state(&dir, &state).is_err() {
                return self.rd_refuse(group, pr, r::STATE_UNWRITABLE);
            }
            if resumed {
                (
                    rddrive::audit_action::RESUMED,
                    json!({ "pr": pr, "reset_counters": reset_counters,
                            "rounds_already_spent": rounds_already_spent }),
                )
            } else {
                (
                    rddrive::audit_action::STARTED,
                    json!({ "pr": pr, "rounds_already_spent": rounds_already_spent }),
                )
            }
        };
        for (dropped_pr, text) in &dropped_notices {
            self.rd_audit(
                group,
                on_behalf_of,
                rddrive::audit_action::NOTICE_DROPPED,
                json!({ "pr": dropped_pr, "reason": "superseded", "notice": text }),
            );
        }
        // A resume that carried a stale signal would re-hold on the reason it
        // was resumed out of — `messaged` most obviously.
        self.rd_signals.lock_safe().remove(&(group.clone(), pr));
        // #2811 S10, same argument one fact over: this call establishes a drive
        // the orchestrator is starting NOW, so a restart mark left by whatever
        // was on this PR before is not about it. Defence in depth on the same
        // measurement as the prune's clear — unreachable today, unpinned, and
        // kept for the same reason.
        self.rd_forget_restart_mark(group, pr);
        self.rd_audit(group, on_behalf_of, audit_action, detail);
        // Service this group on the very next wake rather than after a backoff
        // window that predates the drive.
        self.rd_service_ms.lock_safe().remove(group);
        json!({ "driving": true, "state": reviewdrive::DriveState::CiWait.as_str() })
    }

    /// **A worker's `report(done, ref: <PR>)` starts a drive on that PR**
    /// (#3367 item 2, `driver.auto_drive_on_done`) — or says why it did not.
    ///
    /// Answers `true` when a drive was started and the report is CONSUMED: the
    /// caller then does not deliver it, and the drive's first notice carries it
    /// instead ([`reviewdrive::DriveEntry::auto_report`]). Answers `false` in
    /// every other case, and the caller delivers the report exactly as it did
    /// before this existed — with one `rd-auto-start-declined` row naming why
    /// wherever the policy was on, so an orchestrator asking why a PR did not
    /// auto-drive reads it rather than inferring it from a row that is missing.
    /// With the policy off this does nothing at all, not even audit.
    ///
    /// **Why a report may start a drive at all is argued in
    /// `docs/design/review-driver.md` §3.2**, which is where the "never
    /// automatic" rule lived. The short form: the second key is still turned by
    /// the orchestrator — it spawned this worker onto this branch, and the
    /// refusals below confine the report to starting the drive the orchestrator
    /// would have started on the worker's own PR. Every input that decides
    /// that is something orrerix recorded or GitHub answered; the `ref` a
    /// delegate typed only NAMES a PR, and a PR on someone else's branch is
    /// `not-author`.
    #[doc(hidden)] // pub for integration tests
    pub fn rd_auto_start(&self, group: &GroupId, agent_id: &str, pr_ref: &str, report: &str) -> bool {
        if !self.rd_auto_drive_on_done(group) {
            return false;
        }
        let injected = self.rd_runner_override.lock_safe().clone();
        let owned;
        let runner: &dyn rddrive::RdRunner = match injected.as_deref() {
            Some(r) => r,
            None => {
                let Some(repo) = self.group(group).map(|g| g.repo) else { return false };
                owned = rddrive::runner_for(std::path::Path::new(&repo));
                &owned
            }
        };
        self.rd_auto_start_with(group, runner, agent_id, pr_ref, report, now_ms())
    }

    /// [`rd_auto_start`](Self::rd_auto_start) with the `gh` seam and the clock
    /// injected, for `drive_review_with`'s reason.
    ///
    /// The checks run cheapest-first, and the one `gh` call is last: a report
    /// naming no PR, from a worker with no session, on a PR already driven or
    /// already reviewed, is refused without a round trip.
    #[doc(hidden)] // pub for integration tests
    pub fn rd_auto_start_with(
        &self,
        group: &GroupId,
        runner: &dyn rddrive::RdRunner,
        agent_id: &str,
        pr_ref: &str,
        report: &str,
        now: u64,
    ) -> bool {
        use rddrive::auto_start_refusal as a;
        if !self.rd_auto_drive_on_done(group) {
            return false;
        }
        let decline = |reason: &str, pr: Option<u64>| -> bool {
            self.rd_audit(
                group,
                agent_id,
                rddrive::audit_action::AUTO_START_DECLINED,
                json!({ "agent": agent_id, "ref": rd_fact(pr_ref), "pr": pr, "reason": reason }),
            );
            false
        };
        let Some(pr) = rddrive::pr_from_ref(pr_ref) else { return decline(a::NOT_A_PR, None) };
        let Some(agent) = self.agent(agent_id) else { return decline(a::NOT_AUTHOR, Some(pr)) };
        let Some(session) = agent.session_id.clone().filter(|s| !s.trim().is_empty()) else {
            return decline(a::NO_SESSION, Some(pr));
        };
        // **Live OR parked**, which is wider than `drive_review`'s own
        // `already-driven`: that tool RESUMES a parked drive, and a resume is
        // the orchestrator's decision about a hold it has read (§2.3) — never
        // something a worker's report may do on its behalf.
        match reviewdrive::load_state(&self.group_dir(group)) {
            Ok(s) if s.entry(pr).is_some_and(|e| !e.state().is_terminal()) => {
                return decline(a::ALREADY_DRIVEN, Some(pr));
            }
            Ok(_) => {}
            Err(_) => return decline(rddrive::refusal::STATE_UNREADABLE, Some(pr)),
        }
        // **INVARIANT 9's guard** (§3.2). A drive started here starts its
        // counters at zero, which is true only of a PR nobody has reviewed. A
        // terminal drive's entry is pruned once its notice lands, so without
        // this a worker reporting done AFTER a satisfied gate would start a
        // fresh drive with a fresh three rounds — "yours count too" defeated
        // by a report. A recorded verdict is the durable evidence that a round
        // was spent, whoever spent it.
        if !self.verdict_map(group, pr).is_empty() {
            return decline(a::HAS_VERDICTS, Some(pr));
        }
        let identity = match rddrive::pr_identity(runner, pr) {
            Ok(i) => i,
            Err(reason) => return decline(reason, Some(pr)),
        };
        if !identity.open {
            return decline(a::PR_NOT_OPEN, Some(pr));
        }
        if rddrive::is_scratch_title(&identity.title) {
            return decline(a::SCRATCH, Some(pr));
        }
        // **Authorship is the branch orrerix recorded at spawn**, never the
        // GitHub author (every agent here pushes as the same account) and
        // never anything the report says.
        let branch = agent.branch.clone().unwrap_or_default();
        if branch.trim().is_empty() || branch.trim() != identity.head_ref {
            return decline(a::NOT_AUTHOR, Some(pr));
        }
        let Some(orch) = self
            .agents
            .lock_safe()
            .values()
            .find(|x| &x.group == group && x.role == Role::Orchestrator && x.status != AgentStatus::Dead)
            .map(|x| x.id.clone())
        else {
            return decline(a::NO_ORCHESTRATOR, Some(pr));
        };
        // The text the drive's first notice will carry: the pane's own line,
        // minus the marker a notice already opens with — nesting a second
        // `[orrerix]` mid-line would read as a forged one.
        //
        // **Capped at `RD_FACT_CAP`, one paragraph** (rev-std premortem on
        // #3371): the legacy `summary` path is scrubbed but uncapped, and this
        // text is persisted on the entry — rewritten whole on every drive write
        // for the drive's life — and copied into the audit row. The pane line a
        // delivered report would have produced is not bounded here; the record
        // this one is persisted into is.
        let text = rd_fact(report.trim_start_matches("[orrerix]").trim());
        let out =
            self.drive_review_seeded(group, runner, pr, &session, false, 0, &orch, now, Some(text.clone()));
        if let Some(reason) = out.get("refused").and_then(Value::as_str) {
            return decline(reason, Some(pr));
        }
        self.rd_audit(
            group,
            &orch,
            rddrive::audit_action::AUTO_STARTED,
            json!({ "pr": pr, "agent": agent_id, "session": session, "branch": branch,
                    "report": text }),
        );
        true
    }

    /// `cancel_review_drive(pr)` — one of `held`'s two outgoing arcs, and the
    /// only way an orchestrator stops a live drive short of `satisfied`.
    ///
    /// Not the only way one REACHES `cancelled`: a drive whose PR reads closed
    /// is cancelled on the tick that observes it, with no tool call at all,
    /// which is why [`CancelCause::PrGone`] exists.
    #[doc(hidden)] // pub for integration tests
    pub fn cancel_review_drive(&self, group: &GroupId, pr: u64, on_behalf_of: &str) -> Value {
        self.cancel_review_drive_with(group, pr, on_behalf_of, now_ms())
    }

    /// `cancel_review_drive` with the clock injected — `drive_review` /
    /// `drive_review_with`'s twin convention, and here for that pair's exact
    /// reason (#1857).
    ///
    /// **A bound measured against a clock a test cannot set is a bound no test
    /// can perform.** This function used to stamp [`OwedNotice::owed_ms`] with
    /// the wall clock while the retention ceiling was measured from it by a
    /// tick running on the caller's `now`, so a test on a synthetic clock
    /// compared a small `now` against a wall-clock anchor, `saturating_sub`
    /// answered zero, and the ceiling could never fire on this path — the one
    /// thing the ceiling promises, a documented counterfactual rather than a
    /// pinned one. #1841's B2 shipped out of the same shape one function over:
    /// "the clock is the caller's, and that is what makes the age bound
    /// testable at all".
    ///
    /// **Since #3040 N1 this path owes no notice at all**, so the ceiling has
    /// nothing to fire on here and the seam earns its keep for the other half
    /// of what it always did: `now` is what the flush below prunes and audits
    /// on. The ceiling itself is pinned on the `PrGone` producer, which still
    /// owes one.
    ///
    /// The only production caller passes `now_ms()`, so nothing about live
    /// behaviour changes; what it buys is that a test can drive this tool and
    /// the tick on one clock — which is what
    /// `a_tool_cancel_audits_and_delivers_nothing` needs to assert the prune,
    /// as `the_ceiling_fires_on_a_tool_cancelled_notice_too` needed it to
    /// assert the ceiling before N1 took this path's notice away.
    #[doc(hidden)] // pub for integration tests
    pub fn cancel_review_drive_with(
        &self,
        group: &GroupId,
        pr: u64,
        on_behalf_of: &str,
        now: u64,
    ) -> Value {
        use rddrive::refusal as r;
        if !self.driver_enabled(group) {
            return self.rd_refuse(group, pr, r::DRIVER_DISABLED);
        }
        let dir = self.group_dir(group);
        // #1871 B3: read out of the entry BEFORE the lock is dropped, and it is
        // the ONLY thing that survives it. A cancel is the one exit whose caller
        // is a tool rather than a notice, so the panes have to reach two places —
        // the notice, and this tool's own return value, which is what an
        // orchestrator acts on without waiting for a prompt to arrive.
        let panes: Vec<(String, reviewdrive::DrivenRole)>;
        // The notice this cancel will AUDIT rather than deliver (#3040 N1),
        // built inside the lock where the panes are and written outside it.
        let demoted: Option<String>;
        {
            let _state_guard = self.rd_state_lock.lock_safe();
            let mut state = match reviewdrive::load_state(&dir) {
                Ok(s) => s,
                // **NOT `not-driven`.** A torn file cannot tell you a PR is not
                // driven; it can only tell you orrerix cannot say. §5.1 gives
                // this its own name for exactly the confusion the queue's own
                // contract uses capitals to prevent.
                Err(_) => return self.rd_refuse(group, pr, r::STATE_UNREADABLE),
            };
            let live = state.entry(pr).map(|e| !e.state().is_terminal()).unwrap_or(false);
            if !live {
                return self.rd_refuse(group, pr, r::NOT_DRIVEN);
            }
            let Some(entry) = state.entry_mut(pr) else {
                return self.rd_refuse(group, pr, r::NOT_DRIVEN);
            };
            if entry.advance(reviewdrive::DriveState::Cancelled, None, None, now).is_err() {
                return self.rd_refuse(group, pr, r::STATE_UNREADABLE);
            }
            // Both sides are wanted here — this hunk's base side is EMPTY, so
            // it is add/add rather than the replace-vs-augment above: #1871 B3
            // needs the panes out before the lock drops, and #1857 needs the
            // notice built and owed before the store.
            panes = entry.owned_panes();
            // **DEMOTED to the audit log, not owed to a pane** (#3040 N1).
            // This is the one cancel the orchestrator ASKED for: it is holding
            // this call's return value, which carries the panes and the
            // cancellation, so a prompt arriving later says nothing it does
            // not already have. That is #533-B's `exit_notice_route` test
            // exactly — an event this process was asked to perform is audited,
            // one nobody asked for still interrupts — and `CancelCause::PrGone`
            // (reconcile, or a tick finding the PR closed) is still announced,
            // still owed on the entry, and still bounded by the retention
            // ceiling.
            //
            // The notice is still RENDERED, and the row carries it, because
            // "read it on demand" is only a real path if the text exists to
            // read (#1857's whole argument, kept). What #1857 bought on this
            // path — an obligation that survives a pane that is down — is not
            // lost so much as no longer needed: nothing can fail to deliver a
            // line that is not delivered, and the audit write is the same
            // durable record its retry existed to protect.
            //
            // **The released-worker session stays empty, and that is #2811 S1's
            // reason rather than this one's**: a tool cancel is not a tick, so
            // no step is decided, nothing is released, and the orchestrator
            // that called it is the party disposing of the panes. The demotion
            // changes where the line goes, never what it says.
            // #3367 item 2: a tool cancel of an auto-started drive that never
            // announced anything is its first notice too. It is demoted to the
            // audit log below, so the report lands there rather than nowhere.
            let notice = Self::rd_fold_auto_report(
                entry,
                rddrive::cancelled_notice(pr, rddrive::CancelCause::Tool, &panes, ""),
            );
            demoted = Some(notice);
            if reviewdrive::store_state(&dir, &state).is_err() {
                return self.rd_refuse(group, pr, r::STATE_UNWRITABLE);
            }
        }
        self.rd_signals.lock_safe().remove(&(group.clone(), pr));
        // #2811 S10: cancelled is terminal, so nothing is owed a re-brief.
        // **This is the reachable one**: the tool takes no tick, so the tick's
        // own spend never runs and the mark would otherwise stand. Pinned by
        // `a_cancelled_drive_does_not_leave_a_restart_mark_for_the_next_one`.
        self.rd_forget_restart_mark(group, pr);
        self.rd_audit(
            group,
            on_behalf_of,
            rddrive::audit_action::CANCELLED,
            json!({ "pr": pr, "panes": panes.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>() }),
        );
        // **The demoted notice, written where it can be read on demand**
        // (#3040 N1) — after the store, because the row is a claim about a
        // cancel that HAPPENED, and outside the lock, because the audit sink
        // is not this lock's subject.
        if let Some(notice) = demoted {
            self.rd_audit(
                group,
                on_behalf_of,
                rddrive::audit_action::NOTICE_DEMOTED,
                json!({ "pr": pr, "reason": "tool-cancel", "notice": notice }),
            );
        }
        // The same flush the tick runs, so this tool has exactly one delivery
        // path rather than a second one that would have to be kept in step. It
        // also prunes: a cancel whose notice lands is an entry that leaves
        // here, which is what §5.2 already promised. Since #3040 N1 this path
        // owes NO notice, so what the flush does here is the prune — and it
        // still runs the same one function the tick does, which is the point.
        //
        // `now`, not `now_ms()`: the flush runs the retention ceiling, and a
        // ceiling measured against a different clock from the anchor above is
        // the untestable bound this seam exists to close (#1857).
        let _ = self.rd_flush_notices(group, &dir, now);
        // **The panes ride in the RESULT, and since #3040 N1 that is the ONLY
        // place this cancel puts them in front of its caller.** #1871 B3
        // argued the result from "a notice whose delivery fails is lost", and
        // #1857 answered that by owing the notice on the entry; N1 then
        // demoted this path's notice to the audit log altogether, on the
        // ground the result makes true — this is the one exit whose caller is
        // holding a return value at the moment the panes stop being anyone's,
        // synchronously, where a notice is a prompt that arrives whenever the
        // pane next drains. `CancelCause::PrGone` has no such caller and is
        // still announced.
        json!({
            "cancelled": true,
            "panes": panes
                .iter()
                .map(|(agent, role)| json!({
                    "agent": agent,
                    "role": match role {
                        reviewdrive::DrivenRole::Worker => "worker".to_string(),
                        reviewdrive::DrivenRole::Lane(b) => b.clone(),
                    },
                }))
                .collect::<Vec<_>>(),
        })
    }

    /// Mark an agent `Dead`, so the liveness prune's DEAD side can be reached
    /// from a test (#1871 B2, rev-final).
    ///
    /// **Not a kill path, and deliberately unable to become one.** `kill_agent`
    /// needs a bound pty and performs a real `PtyManager::kill`; a
    /// driver-spawned pane in this harness has neither, so the state this
    /// predicate turns on is otherwise unreachable from an integration test.
    /// This sets the one field the predicate reads and touches nothing else — no
    /// initiator stamp, no exit notice, no pty — so it cannot stand in for
    /// `kill_agent` in a test that means to exercise killing, and a reader
    /// cannot mistake it for the production route.
    ///
    /// Answers whether an agent by that id existed to mark.
    #[doc(hidden)] // pub for integration tests
    pub fn mark_agent_dead_for_test(&self, agent_id: &str) -> bool {
        let mut agents = self.agents.lock_safe();
        match agents.get_mut(agent_id) {
            Some(a) => {
                a.status = AgentStatus::Dead;
                true
            }
            None => false,
        }
    }

    /// Corrupt this group's drive record, so the FAULT paths that read it can
    /// be exercised from outside the crate.
    ///
    /// **It hands out no path**, which is the whole point. CLAUDE.md constraint
    /// 6 keeps `group_dir_at` the single join and keeps it private, and a
    /// `group_dir_for_test` returning a `PathBuf` would hand every future test
    /// exactly the thing that rule exists to withhold. This takes a validated
    /// `GroupId`, writes a fixed payload, and answers whether it wrote — so a
    /// test can reach "the record exists and cannot be parsed" without ever
    /// reaching the directory. The payload is not a parameter for the same
    /// reason: a caller that can choose the bytes is a caller that can write a
    /// VALID record, which is a state seeder rather than a fault injector.
    #[doc(hidden)] // pub for integration tests
    pub fn corrupt_drive_record_for_test(&self, group: &GroupId) -> bool {
        let path = reviewdrive::state_path(&self.group_dir(group));
        std::fs::write(path, b"{ not json").is_ok()
    }

    /// `review_drive_status()` — the surface a **compacted** orchestrator
    /// recovers its drives from, which is why §5.1 puts it in the re-sync list
    /// beside `list_tasks`, `list_agents` and `get_state`.
    ///
    /// **It does not list terminal entries**, exactly as `merge_queue_status`
    /// does not: they would flow into the orchestrator's resident context, which
    /// is the cost this whole feature exists to remove. Parked entries ARE
    /// listed — a `held` drive is the one thing an orchestrator most needs to
    /// see, and §5.2 never prunes one.
    /// **The clock is the caller's**, for `cancel_review_drive_with`'s reason,
    /// one function over: every figure below is DERIVED from `now`, and a status
    /// view reading the wall clock while the tick decides on an injected one puts
    /// the two on different scales. `state_ms` and `since_ms` then come back in
    /// wall units against anchors stamped in the test's units, so a test can
    /// assert nothing about them except where the wall terms happen to cancel —
    /// which is how #2110's first published figures were written, and they were
    /// wrong in the direction that reads as passing. B2 shipped out of exactly
    /// this shape.
    ///
    /// The only production caller passes `now_ms()`, so nothing about live
    /// behaviour changes.
    #[doc(hidden)] // pub for integration tests
    pub fn review_drive_status(&self, group: &GroupId) -> Value {
        self.review_drive_status_with(group, now_ms())
    }

    #[doc(hidden)] // pub for integration tests
    pub fn review_drive_status_with(&self, group: &GroupId, now: u64) -> Value {
        let enabled = self.driver_enabled(group);
        let dir = self.group_dir(group);
        let state = {
            let _state_guard = self.rd_state_lock.lock_safe();
            match reviewdrive::load_state(&dir) {
                Ok(s) => s,
                // The same distinction the two mutating tools make: "orrerix
                // cannot read the record" is not "there is nothing in it".
                Err(_) => {
                    return json!({ "enabled": enabled,
                                   "refused": rddrive::refusal::STATE_UNREADABLE })
                }
            }
        };
        let drives: Vec<Value> = state
            .entries
            .iter()
            .filter(|e| !e.state().is_terminal())
            .map(|e| {
                json!({
                    "pr": e.pr,
                    "state": e.state().as_str(),
                    "held_reason": e.held_reason.map(|r| r.as_str()),
                    "head": e.head,
                    "lanes": e.lanes.iter().map(|l| json!({
                        "block": l.block,
                        "last_verdict": l.last_verdict.map(|v| v.as_str()),
                    })).collect::<Vec<_>>(),
                    "counters": {
                        "review_rounds": e.counters.review_rounds,
                        "ci_attempts": e.counters.ci_attempts,
                        "rebase_attempts": e.counters.rebase_attempts,
                    },
                    // #2509. Published beside the counters rather than inside
                    // them, because it is not one: `review_rounds` is what this
                    // drive has spent of INVARIANT 9's budget and stops at the
                    // bound, while this says whether the one extra round outside
                    // that budget is still available. An orchestrator reading
                    // `review_rounds: 3` of 3 on a LIVE drive is looking at the
                    // grace, and this is the field that says so.
                    "grace_used": e.counters.body_only_grace,
                    // #3367 item 1: the non-blocking rounds the driver ran on
                    // its own. Beside `review_rounds` rather than inside it for
                    // `grace_used`'s reason — each of these is ALSO counted
                    // there, so this says how many of that figure nobody
                    // dispositioned.
                    "nit_rounds": e.nit_rounds,
                    // Derived, never stored: a stored AGE is stale the instant
                    // it is written and meaningless across a restart, which is
                    // the queue's own split between `enqueued_ms` and
                    // `status_view`'s `since_ms`.
                    //
                    // **`since_ms` stays the WALL age and the exclusion is
                    // published beside it** (#2110), rather than being netted
                    // off inside it. A human asking how long a drive has been
                    // going wants the wall figure, and an age that silently
                    // shrank when a cap cleared would be a worse answer than
                    // the one it replaced; what the backstop measures is
                    // `since_ms - starved_ms`, which is checkable here rather
                    // than inferable. `state_ms` is the clock that actually
                    // bounds a working drive, and `held_state`/`held_state_ms`
                    // are what it was doing when a bound fired — the third
                    // bullet of #2110, so a resume is a decision.
                    "since_ms": e.age_ms(now),
                    "starved_ms": e.starved_ms(now),
                    "state_ms": e.state_elapsed_ms(now),
                    "held_state": e.held_from.map(|s| s.as_str()),
                    "held_state_ms": e.held_from.map(|_| e.held_after_ms),
                })
            })
            .collect();
        json!({ "enabled": enabled, "drives": drives })
    }

    /// One refusal, audited then returned — so `rd-refused` and what the caller
    /// was told cannot come apart.
    fn rd_refuse(&self, group: &GroupId, pr: u64, reason: &'static str) -> Value {
        self.rd_audit(
            group,
            "",
            rddrive::audit_action::REFUSED,
            json!({ "pr": pr, "reason": reason,
                    "orrerix_fault": rddrive::refusal::is_orrerix_fault(reason) }),
        );
        json!({ "refused": reason })
    }

    /// [`rd_refuse`](Self::rd_refuse), with the sentence that says why — the
    /// quoted refusal a hold would have carried (#1961's rule, at the call).
    /// The `reason` stays a closed-vocabulary name an agent branches on; the
    /// detail rides beside it, the same shape the `rd-held` row gives a hold's
    /// refusal.
    fn rd_refuse_detail(&self, group: &GroupId, pr: u64, reason: &'static str, detail: &str) -> Value {
        self.rd_audit(
            group,
            "",
            rddrive::audit_action::REFUSED,
            json!({ "pr": pr, "reason": reason, "detail": detail,
                    "orrerix_fault": rddrive::refusal::is_orrerix_fault(reason) }),
        );
        json!({ "refused": reason, "detail": detail })
    }

}
