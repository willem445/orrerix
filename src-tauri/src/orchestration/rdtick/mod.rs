//! The review-loop driver's registry wiring (#1778 S3).
//!
//! Design note: `docs/design/review-driver.md`. Every decision this module makes
//! is made somewhere else: the state machine is [`reviewdrive::decide`], the
//! gate is [`mergeq::recheck_gate`], the `gh` reads and the notice text are
//! [`rddrive`]. What lives here is what only the registry can do — resolve the
//! policy, the gate and the verdict files, hold the state lock across the
//! read-modify-write, perform the spawns and resumes, emit the audit events,
//! and deliver the notices.
//!
//! # Why its own module rather than more of `orchestration/mod.rs`
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
//!
//! # The files (#3498 P5)
//!
//! Split by tick phase, one `impl OrchRegistry` block per file; this file keeps
//! the types they share. `tests/reviewdrive.rs` reads the whole directory as one
//! scope, so the argument above is about `rdtick/`, and a file added here is
//! scanned with no edit to the test.
//!
//! - `policy.rs` — the `driver:` policy, the runner override, the defer.
//! - `events.rs` — delegate events, pane ownership, the driver's audit line.
//! - `tick.rs` — the tick, the notice flush and retention, the gate facts.
//! - `step.rs` — `rd_step_entry`, one advance per entry per tick.
//! - `lanes.rs` — opening a reviewer lane.
//! - `handback.rs` — the worker hand-back and the one spawn or resume.
//! - `briefs.rs` — the brief and notice text.
//! - `reconcile.rs` — restart marks, lost panes, the restart reconcile.
//! - `tools.rs` — §5.1's MCP tools and `rd_auto_start`.

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

mod briefs;
mod events;
mod handback;
mod lanes;
mod policy;
mod reconcile;
mod step;
mod tick;
mod tools;
