//! The plan driver's registry wiring (#3040 P3a) — `rdtick.rs`'s twin, in a
//! file of its own for that file's reason.
//!
//! Not for size alone. A FILE is a scope a RENAME cannot step over, which is
//! what CLAUDE.md's source-scanning-guard convention asks for: the review
//! driver's "never builds a landing verb" scan default-denies the whole of
//! `rdtick.rs`, and `tests/plandrive.rs` does the same to this one. The plan
//! driver reaches GitHub through the same `RdRunner` — a `gh`-only view with no
//! `git` method at all — so the structural "never merges, never pushes"
//! guarantee is inherited rather than re-argued.
//!
//! **This is wiring, not logic.** Every state decision is
//! [`plandrive::decide`]'s and every plan-block judgement is
//! [`plandrive::validate_for_drive`]'s. What lives here is what only the
//! registry can do: resolve the policy and the roster, read the issue through
//! `gh`, hold the state lock across the read-modify-write, perform the planner
//! spawn, emit the audit rows, and deliver the notices.
//!
//! Design note: `doc/design/plan-driver.md`.

use std::sync::Arc;

use serde_json::{json, Value};

use super::{
    brand, notify, now_ms, plandoc, plandrive, rddrive, AgentEntry, AgentStatus, GroupId, LockExt,
    OrchRegistry, Role,
};

use super::plandrive::{
    Consent, PdEntry, PdFacts, PdHeldReason, PdLimits, PlanDriveState, PlannerSignal,
};

/// One interpolated value, sanitized — `rd_fact`'s twin, with the same cap and
/// for the same reason: a brief is a prompt, and a prompt is the delegate's
/// resident context, so a pathological value must not be able to become the
/// whole of one.
fn pd_fact(s: &str) -> String {
    notify::sanitize_pane_text(s, PD_FACT_CAP, notify::Lines::Collapse)
}

/// How long one interpolated fact may be.
const PD_FACT_CAP: usize = 2_000;

/// At most this many `gh` round trips per group per wake, after the review
/// driver has spent its own on the same wake (§6's tick budget).
///
/// One per live drive is the shape today — a single `issue view` resolving the
/// issue's state and its labels together — so this is a ceiling on the number
/// of LIVE DRIVES one wake services, not a second budget layered on a first.
/// A group with more live drives than this services the rest on the next wake;
/// nothing is dropped, and the entries are walked in the file's own order so
/// the same drive cannot be starved twice running by an unstable sort.
///
/// **It bounds the STEADY-STATE wake, and one wake per process is not one.**
/// [`pd_reconcile_with`](OrchRegistry::pd_reconcile_with) runs once per group
/// per process, before this loop, and reads one issue per NON-TERMINAL entry
/// without consulting this figure — so the first wake after a restart spends
/// `non_terminal + min(live, PD_MAX_GH_PER_TICK)` round trips rather than four.
///
/// **Non-terminal, not live**, and the difference is deliberate rather than
/// sloppy: a `held` entry is not live and is still reconciled, because the one
/// thing a restart must be able to learn about a parked drive is that its issue
/// was closed while orrerix was down (`Held -> Cancelled`). So a group carrying
/// holds spends more than its live count here. That is bounded by how many
/// issues an orchestrator chose to drive, and it is a startup cost paid once; it
/// is stated rather than fixed because a reconcile that serviced only four
/// entries would leave the rest unreconciled with nothing scheduled to finish
/// the job.
/// `a_tick_services_at_most_four_drives` measures BOTH figures, so this
/// paragraph cannot go quietly false.
pub const PD_MAX_GH_PER_TICK: usize = 4;

/// What a driven planner's traffic told the drive, between the MCP arm that
/// consumed it and the tick that acts on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PdEvent {
    /// `report(done)`.
    PlannerDone,
    /// `report(blocked)`.
    PlannerBlocked,
    /// One slice's worker reported `done`, carrying the slice id and the `ref`
    /// it named.
    WorkerDone { slice: String, pr_ref: String },
    /// One slice's worker reported `blocked`, carrying its note.
    WorkerBlocked { slice: String, note: String },
}

/// One drive's pending signals — what its planner and its slices' workers said
/// between the MCP arm that consumed them and the tick that acts on them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PdSignal {
    pub planner: PlannerSignal,
    /// Slice id -> what that slice's worker said. Keyed by SLICE rather than by
    /// agent, because the agent is how the report was ATTRIBUTED and the slice
    /// is what the drive acts on — and a slice re-spawned after a hold would
    /// otherwise leave its old pane's signal sitting in the map under a key
    /// nothing reads.
    pub workers: std::collections::BTreeMap<String, PdWorkerSignal>,
}

/// What one plan-drive tick did, for a test to read rather than infer from the
/// audit log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PdDriveReport {
    /// The driver is off for this group, so nothing below ran.
    pub disabled: bool,
    /// `plan_drives.json` is there and could not be read.
    pub state_unreadable: bool,
    /// Issues whose entry this tick advanced.
    pub advanced: Vec<u64>,
    /// Issues this tick could not service because [`PD_MAX_GH_PER_TICK`] was
    /// already spent. Its own field rather than silence, so
    /// `a_tick_services_at_most_four_drives` can assert the bound BIT as well
    /// as the count.
    pub deferred: Vec<u64>,
    /// Notices delivered to the orchestrator this tick.
    pub notices: usize,
}

/// The answer the `post_issue_comment` hook acts on (§2(b) step 3).
///
/// Three outcomes, deliberately not two: "this caller is nobody's planner" and
/// "this caller is a driven planner whose block is fine" both let the post
/// through, and only the second stores anything. Collapsing them would make the
/// store's condition a boolean nobody can read at the call site.
pub enum PdPlanCheck {
    /// Not a driven planner's post — the tool behaves exactly as it always did.
    /// This is the answer for every caller in every group with no plan drive,
    /// which is the product default.
    NotDriven,
    /// A driven planner posted a block this build refuses. **Nothing is
    /// posted**; the reasons go back to the planner as the tool's own error, and
    /// it is still inside its own turn, so a fix costs it one tool call and the
    /// orchestrator no turn at all.
    Invalid(Vec<String>),
    /// A driven planner posted a valid block. Post it, then hand the document
    /// and the comment URL to
    /// [`pd_store_posted_plan`](OrchRegistry::pd_store_posted_plan).
    Valid(Box<plandoc::PlanDoc>),
}

/// How long ONE slice's brief may be.
///
/// Ten times [`PD_FACT_CAP`], because it is a different kind of value: a fact is
/// one interpolated field and a brief is the whole of what a worker is told —
/// the planner wrote it to be the delegate's entire instruction, §3 delivers it
/// VERBATIM, and a cap that truncated it would be the one rewrite that design
/// forbids. It is a cap and not an absence of one because a pane delivery is
/// still a pane delivery.
const PD_BRIEF_CAP: usize = 20_000;

/// At most this many `gh pr view` calls per group per wake, round-robin over
/// the drive's in-review slices (§6's tick budget).
///
/// **Its own name, not [`PD_MAX_GH_PER_TICK`] reused**, and the two figures
/// being equal today is a coincidence rather than a relationship. That one is a
/// ceiling on how many live DRIVES one wake services; this is a ceiling on how
/// many of one drive's in-review PRs it looks at. Spending one name on both
/// would mean a future change to either silently moved the other.
pub const PD_MAX_PR_CHECKS_PER_TICK: usize = 4;

/// What one slice's worker said to its drive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PdWorkerSignal {
    /// `report(done, ref)`. `pr_ref` is the `ref` argument verbatim — a HINT
    /// the driver may resolve a PR number out of, never the thing that decided
    /// this report was intercepted.
    Done { pr_ref: String },
    /// `report(blocked)`, with the note the worker sent.
    Blocked { note: String },
}

/// What one tick's `gh` reads told the executor.
#[derive(Clone, Debug, Default)]
struct PdPrReads {
    /// Slice id -> the PR number just resolved for it.
    resolved: std::collections::BTreeMap<String, u64>,
    /// Slice id -> where its PR got to.
    pr_state: std::collections::BTreeMap<String, PrOutcome>,
    /// How far the round-robin cursor moved.
    cursor_advance: u64,
    /// The seam itself failed — back off.
    runner_failed: bool,
}

/// What one PR is doing. **Two positive answers and two non-answers**, because
/// a row is marked `done` only on a positively-established MERGED PR: an
/// unreadable PR must leave its slice exactly where it was, never advance it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrOutcome {
    /// `state: MERGED`, or a `mergedAt` stamp.
    Merged,
    /// `state: CLOSED` with no `mergedAt`.
    ClosedUnmerged,
    /// `state: OPEN` — still with the review driver.
    Open,
    /// orrerix could not tell.
    Unknown,
}

#[derive(serde::Deserialize)]
struct RawPrNumber {
    #[serde(default)]
    number: u64,
}

#[derive(serde::Deserialize)]
struct RawPrState {
    #[serde(default)]
    state: String,
    #[serde(default)]
    merged_at: Option<String>,
}

impl RawPrState {
    /// **`mergedAt` outranks `state`.** A merged PR reports `state: MERGED`
    /// today, but the stamp is the fact that cannot mean anything else, and a
    /// `state` word this build does not recognise must not be able to turn a
    /// merged PR into an unknown one.
    fn outcome(&self) -> PrOutcome {
        if self.merged_at.as_deref().map(str::trim).is_some_and(|s| !s.is_empty()) {
            return PrOutcome::Merged;
        }
        match self.state.trim().to_ascii_uppercase().as_str() {
            "MERGED" => PrOutcome::Merged,
            "CLOSED" => PrOutcome::ClosedUnmerged,
            "OPEN" => PrOutcome::Open,
            _ => PrOutcome::Unknown,
        }
    }
}

/// What [`OrchRegistry::pd_execute`] did, carried out of the state lock so the
/// audits, the notices and the review-driver hand-off happen outside it.
#[derive(Clone, Debug, Default)]
struct PdTickOutcome {
    on_behalf: String,
    advanced: bool,
    action: &'static str,
    reason: &'static str,
    audits: Vec<(&'static str, Value)>,
    /// Notices owed by SLICE-level events. A drive-level hold owes its notice
    /// through `owe_notice` on the entry, which `pd_flush_notices` delivers;
    /// these are separate because a drive owes at most one at a time and a tick
    /// can produce several slice notices.
    notices: Vec<String>,
    /// (slice id, pr, worker session) to hand to the review driver.
    hand_offs: Vec<(String, u64, String)>,
}

impl OrchRegistry {
    // ---------- policy ----------

    /// This group's plan-driver policy: whether the feature is on at all, and
    /// the bounds one drive runs against.
    ///
    /// **Three conditions, and the third is the point.** `advanced_orchestrator`
    /// and `driver.enabled` are the review driver's own two, unchanged — the
    /// plan driver rides that driver's tick, record and backoff discipline, so
    /// it is off wherever that is. `driver.plan_enabled` is the third, and it is
    /// a SECOND key rather than a widening of the first because the consents are
    /// different: a repo that turned the review driver on consented to orrerix
    /// running a review loop it already had an orchestrator for, not to orrerix
    /// spawning a PLANNER and turning its output into work.
    ///
    /// **Off is the answer to every uncertainty** — no group, no advanced
    /// orchestrator, no workflow file, a file that will not parse. Same posture
    /// as `driver_policy`, and for the sharper version of its reason: a review
    /// driver that failed open spawns a reviewer into a repo that never asked
    /// for one; a plan driver that failed open spawns a planner and then spawns
    /// workers off what it writes.
    fn pd_policy(&self, group: &GroupId) -> (bool, PdLimits) {
        let off = (false, PdLimits::default());
        let Some(g) = self.group(group) else { return off };
        if !g.guardrails.advanced_orchestrator {
            return off;
        }
        let Ok(Some(wf)) = super::load_active_workflow(&g.repo, &g.guardrails) else { return off };
        let d = wf.driver;
        (
            d.enabled && d.plan_enabled,
            PdLimits::new(
                d.planner_timeout_minutes,
                d.plan_review_minutes,
                d.drive_timeout_minutes,
            ),
        )
    }

    /// Whether this group runs a plan driver at all.
    ///
    /// **Private, because every caller is in this file** — the five sites below
    /// it. An earlier draft made it `pub(super)` and justified that by
    /// `driver_enabled`'s reason: the parent module gates the tool listing and
    /// an instruction placeholder on it. That reason describes a mechanism this
    /// build does not have. The four plan tools are listed unconditionally and
    /// gated at dispatch (the `queue_merge` pattern), and there is no
    /// `PLAN_DRIVER_NOTE` to gate — `REVIEW_DRIVER_NOTE` has one and this does
    /// not. The placeholder and the playbook prose are P4's.
    ///
    /// So the visibility is narrowed to what is true now rather than to what a
    /// later slice may want: P4 widens it, with the argument it will then
    /// actually have.
    fn plan_driver_enabled(&self, group: &GroupId) -> bool {
        self.pd_policy(group).0
    }

    /// Hold `group` off until `at`.
    fn pd_defer(&self, group: &GroupId, at: u64) {
        self.pd_service_ms.lock_safe().insert(group.clone(), at);
    }

    /// One audit line in the plan driver's vocabulary, carrying `on_behalf_of`.
    ///
    /// The **actor** stays `brand::AUDIT_ACTOR`, so it is this detail key — not
    /// the actor — that distinguishes a driver action from any other host
    /// action. Shared with the review driver's key on purpose: an audit reader
    /// filtering "what did orrerix do for this orchestrator" wants both drivers'
    /// rows, and the `pd-`/`rd-` prefix is what separates them when it wants
    /// one.
    fn pd_audit(&self, group: &GroupId, on_behalf_of: &str, action: &str, mut detail: Value) {
        if let Some(obj) = detail.as_object_mut() {
            obj.insert(rddrive::ON_BEHALF_OF.to_string(), Value::from(on_behalf_of));
        }
        self.audit(group, brand::AUDIT_ACTOR, action, detail);
    }

    /// One refusal, audited then returned — so `pd-refused` and what the caller
    /// was told cannot come apart.
    fn pd_refuse(&self, group: &GroupId, issue: u64, reason: &'static str) -> Value {
        self.pd_audit(
            group,
            "",
            plandrive::audit_action::REFUSED,
            json!({ "issue": issue, "reason": reason,
                    "orrerix_fault": plandrive::refusal::is_orrerix_fault(reason) }),
        );
        json!({ "refused": reason })
    }

    /// Drop a reservation whose planner never opened.
    ///
    /// Best-effort by construction, and that is the honest posture rather than a
    /// shortcut: the caller is already returning `planner-unspawnable`, and a
    /// reservation this fails to remove is a drive with no planner that the
    /// restart reconcile names (`reservation-unspawned`) and the drive-age
    /// backstop eventually parks. Turning a failed rollback into a second,
    /// different refusal would tell the caller less than the one it is already
    /// getting.
    ///
    /// **Only a reservation** — an entry with no planner recorded. Anything else
    /// is a drive that got further than this call did, and removing it would
    /// erase work on a spawn failure.
    fn pd_release_reservation(&self, dir: &std::path::Path, issue: u64) {
        let _state_guard = self.pd_state_lock.lock_safe();
        let Ok(mut state) = plandrive::load_state(dir) else { return };
        let is_reservation = state
            .entry(issue)
            .is_some_and(|e| e.planner_agent.is_empty() && e.spawned_ms == 0);
        if !is_reservation {
            return;
        }
        state.entries.retain(|e| e.issue != issue);
        let _ = plandrive::store_state(dir, &state);
    }

    /// Where this group's `plan_drives.json` lives — a TEST seam, and the only
    /// way out of this crate to that path.
    ///
    /// Exposed rather than letting a test rebuild the path itself, for CLAUDE.md
    /// constraint 6's reason: `group_dir_at` is the one place a group id becomes
    /// a path, and a test that joined the group dir by hand would be a second
    /// join — in a file the join scan does not read.
    #[doc(hidden)]
    pub fn pd_record_path_for_test(&self, group: &GroupId) -> std::path::PathBuf {
        plandrive::state_path(&self.group_dir(group))
    }

    /// Install (or clear) the canned `gh` this driver reads through.
    ///
    /// **There is no `pd_runner_override`** — this is `set_rd_runner_override`,
    /// named here so a reader looking for the plan driver's seam finds the
    /// answer rather than adding a second one. One `gh` seam per registry is
    /// what keeps a test's call log a statement about the whole tick.
    #[doc(hidden)]
    pub fn set_pd_runner_override(&self, runner: Option<Arc<dyn rddrive::RdRunner>>) {
        self.set_rd_runner_override(runner);
    }

    // ---------- §2(b) step 3/4: interception ----------

    /// Which live drive, if any, this agent is the planner of.
    ///
    /// **Keyed on the agent, never on text**, and **only a LIVE drive owns
    /// anyone** — `rd_owner`'s two properties, and the whole of the reason is
    /// there: a delegate that could choose whether its report reaches the
    /// orchestrator by naming an issue number is a delegate that can route
    /// around the orchestrator, and a `held` entry is parked, so its planner's
    /// traffic goes to the orchestrator exactly as it always did.
    ///
    /// Reads `plan_drives.json` on every `report` and every
    /// `post_issue_comment`. An absent file — the product default — costs a
    /// `stat` and answers `None`.
    pub fn pd_owner(&self, group: &GroupId, agent_id: &str) -> Option<u64> {
        let dir = self.group_dir(group);
        let _state_guard = self.pd_state_lock.lock_safe();
        let state = plandrive::load_state(&dir).ok()?;
        state
            .entries
            .iter()
            .filter(|e| e.state().is_live())
            .find(|e| e.driven_role(agent_id))
            .map(|e| e.issue)
    }

    /// Record that a driven planner's event was **consumed** by the driver
    /// rather than delivered to the orchestrator.
    ///
    /// **Nothing is silent.** Every consumed event is audited with its kind, the
    /// agent and the issue, so traffic that stopped arriving as a prompt is
    /// still on the record and still attributable. "Consumed" is a different
    /// word from "dropped".
    pub fn pd_consume(
        &self,
        group: &GroupId,
        issue: u64,
        agent: &str,
        kind: &str,
        event: Option<PdEvent>,
    ) {
        let on_behalf = {
            let dir = self.group_dir(group);
            let _state_guard = self.pd_state_lock.lock_safe();
            plandrive::load_state(&dir)
                .ok()
                .and_then(|s| s.entry(issue).map(|e| e.on_behalf_of.clone()))
                .unwrap_or_default()
        };
        self.pd_audit(
            group,
            &on_behalf,
            plandrive::audit_action::PLANNER_CONSUMED,
            json!({ "issue": issue, "agent": agent, "kind": kind }),
        );
        if let Some(e) = event {
            self.pd_ingest(group, issue, e);
        }
    }

    /// Record that one slice's WORKER `report` was consumed by the driver
    /// rather than delivered to the orchestrator.
    ///
    /// [`pd_consume`](Self::pd_consume)'s twin, and separate for its audit
    /// action's reason: a reader counting what a drive swallowed wants to know
    /// which side spoke.
    pub fn pd_consume_slice(
        &self,
        group: &GroupId,
        issue: u64,
        slice: &str,
        agent: &str,
        kind: &str,
        event: Option<PdEvent>,
    ) {
        let on_behalf = {
            let dir = self.group_dir(group);
            let _state_guard = self.pd_state_lock.lock_safe();
            plandrive::load_state(&dir)
                .ok()
                .and_then(|s| s.entry(issue).map(|e| e.on_behalf_of.clone()))
                .unwrap_or_default()
        };
        self.pd_audit(
            group,
            &on_behalf,
            plandrive::audit_action::SLICE_CONSUMED,
            json!({ "issue": issue, "slice": slice, "agent": agent, "kind": kind }),
        );
        if let Some(e) = event {
            self.pd_ingest(group, issue, e);
        }
    }

    /// Record a driven planner's event for the next tick.
    ///
    /// **In memory, and that is a bounded choice.** A `report` is an *event*;
    /// the durable fact a plan drive turns on — whether a plan was posted — is
    /// the record's own `plan` field, re-read every tick. An orrerix restart
    /// between a planner's `report(done)` and the next tick therefore loses only
    /// the fast route to `held(plan-missing)`: the drive degrades to
    /// `held(planner-stalled)` at the timeout instead, which is bounded and
    /// named. Persisting an event queue would buy that one arc at the cost of a
    /// second write path into the state file, from the MCP thread.
    #[doc(hidden)]
    pub fn pd_ingest(&self, group: &GroupId, issue: u64, event: PdEvent) {
        let mut map = self.pd_signals.lock_safe();
        let sig = map.entry((group.clone(), issue)).or_default();
        match event {
            PdEvent::PlannerDone => sig.planner = PlannerSignal::Done,
            // `blocked` outranks a `done` seen in the same window: the two can
            // only both be present if the planner said one and then the other,
            // and `blocked` is the one that needs a human.
            PdEvent::PlannerBlocked => sig.planner = PlannerSignal::Blocked,
            PdEvent::WorkerDone { slice, pr_ref } => {
                sig.workers.insert(slice, PdWorkerSignal::Done { pr_ref });
            }
            // Same rule as the planner's, per slice: a worker that said `done`
            // and then `blocked` needs a human, and the later word wins.
            PdEvent::WorkerBlocked { slice, note } => {
                sig.workers.insert(slice, PdWorkerSignal::Blocked { note });
            }
        }
    }

    /// This drive's pending planner signal, **without clearing it**.
    ///
    /// Cleared only once an arc has been taken, because a tick can decline to
    /// act for reasons that have nothing to do with the signal — a runner
    /// failure, an exhausted `gh` budget — and a signal consumed by a tick that
    /// then did nothing is a hand-back the drive never learns about.
    fn pd_signal(&self, group: &GroupId, issue: u64) -> PdSignal {
        self.pd_signals.lock_safe().get(&(group.clone(), issue)).cloned().unwrap_or_default()
    }

    fn pd_clear_signal(&self, group: &GroupId, issue: u64) {
        self.pd_signals.lock_safe().remove(&(group.clone(), issue));
    }

    // ---------- §2(b) step 3: the post_issue_comment hook ----------

    /// **Validate a driven planner's plan block BEFORE anything is posted.**
    ///
    /// The whole reason this hook exists rather than a `gh issue view` after the
    /// fact: the plan reaches orrerix in the tool call's own payload, so a
    /// refusal costs one tool call inside the planner's own turn — no round
    /// trip, no orchestrator turn, and nothing published that a human then has
    /// to read and discount. A refusal here is the tool answering `Err`, so the
    /// planner sees the line-numbered reasons in the place it is already
    /// looking.
    ///
    /// **Every refusal is counted and the count is bounded** (§2(e)):
    /// [`plandrive::MAX_INVALID_POSTS`] refusals park the drive on
    /// `held(plan-invalid)` carrying the last reasons, so a planner that will
    /// not converge stops costing tool calls and starts costing a human one
    /// notice.
    ///
    /// **A later comment by the same planner passes through unvalidated.** The
    /// hook fires only while the drive is in `planning`; once a plan is stored
    /// the drive has what it needs, and a planner adding a note to the issue is
    /// doing what any agent with this tool may do.
    ///
    /// A caller that is nobody's planner reaches [`PdPlanCheck::NotDriven`]
    /// after one `stat` of an absent file — which is the product default and
    /// must stay indistinguishable, from the caller's side, from this hook not
    /// existing (§6: a planner not spawned by a drive is never affected).
    pub fn pd_plan_check(
        &self,
        group: &GroupId,
        agent_id: &str,
        issue: u64,
        body: &str,
    ) -> PdPlanCheck {
        self.pd_plan_check_at(group, agent_id, issue, body, now_ms())
    }

    /// [`pd_plan_check`](Self::pd_plan_check) with the clock injected — the
    /// `drive_review` / `drive_review_with` convention, and here for that pair's
    /// reason: this function stamps the hold's `state_since_ms`, and a bound
    /// measured against a clock a test cannot set is a bound no test can
    /// perform.
    #[doc(hidden)]
    pub fn pd_plan_check_at(
        &self,
        group: &GroupId,
        agent_id: &str,
        issue: u64,
        body: &str,
        now: u64,
    ) -> PdPlanCheck {
        // Cheapest first, and each of these is a reason not to parse a document:
        // the group has no plan driver, the file is absent or unreadable, this
        // issue has no live entry, this caller is not its planner, or the drive
        // is past `planning` and already has its plan.
        if !self.plan_driver_enabled(group) {
            return PdPlanCheck::NotDriven;
        }
        let dir = self.group_dir(group);
        let blocks = self.pd_roster_blocks(group);

        let (on_behalf, held_now) = {
            let _state_guard = self.pd_state_lock.lock_safe();
            let Ok(mut state) = plandrive::load_state(&dir) else {
                // A torn record cannot say this caller is a driven planner, and
                // it must not silently make a post behave differently either.
                // The tick's own loud handling owns the file; this path lets the
                // comment through exactly as it would with no drive at all.
                return PdPlanCheck::NotDriven;
            };
            let Some(entry) = state.entry_mut(issue) else { return PdPlanCheck::NotDriven };
            if !entry.state().is_live()
                || !entry.driven_role(agent_id)
                || entry.state() != PlanDriveState::Planning
            {
                return PdPlanCheck::NotDriven;
            }
            let on_behalf = entry.on_behalf_of.clone();

            match plandrive::validate_for_drive(body, issue, &blocks) {
                Ok(doc) => {
                    // NOT stored here: the plan is stored once the post has
                    // actually happened (`pd_store_posted_plan`), because a
                    // record saying a plan was posted at a comment URL that does
                    // not exist is worse than one saying nothing. The tool's own
                    // `gh` call is the next thing that runs.
                    return PdPlanCheck::Valid(Box::new(doc));
                }
                Err(reasons) => {
                    let hold = entry.note_invalid(&reasons);
                    if hold {
                        let text = format!(
                            "[orrerix] plan drive #{issue}: HELD ({}) — {}. The last refusal: {}",
                            PdHeldReason::PlanInvalid.as_str(),
                            PdHeldReason::PlanInvalid.notice_line(),
                            reasons.join("; ")
                        );
                        entry.owe_notice(&text, now);
                        // `advance` rather than a field write, so the arc goes
                        // through `transition` like every other.
                        let _ = entry.advance(PlanDriveState::Held, Some(PdHeldReason::PlanInvalid), now);
                    }
                    let count = entry.invalid_count;
                    // The write is inside the lock and its failure is not
                    // swallowed silently: a refusal orrerix could not record is
                    // one the bound will not count, so the audit row below says
                    // so and the caller is still refused. Refusing on an
                    // unrecorded count is the safe direction — it never posts a
                    // document this build cannot read.
                    let stored = plandrive::store_state(&dir, &state);
                    (on_behalf, Some((reasons, count, hold, stored.is_err())))
                }
            }
        };

        let Some((reasons, count, held, unwritable)) = held_now else {
            return PdPlanCheck::NotDriven;
        };
        self.pd_audit(
            group,
            &on_behalf,
            plandrive::audit_action::PLAN_INVALID,
            json!({ "issue": issue, "agent": agent_id, "invalid_count": count,
                    "reasons": reasons, "held": held, "unwritable": unwritable }),
        );
        if held {
            self.pd_audit(
                group,
                &on_behalf,
                plandrive::audit_action::HELD,
                json!({ "issue": issue, "reason": PdHeldReason::PlanInvalid.as_str() }),
            );
            self.pd_flush_notices(group, now);
        }
        PdPlanCheck::Invalid(reasons)
    }

    /// Store the plan a driven planner just posted, and advance the drive to
    /// `plan-posted` (§2(b) step 3).
    ///
    /// Called only after `gh` has answered with the comment's URL, so the record
    /// never claims a plan lives at a URL that does not exist.
    #[doc(hidden)]
    pub fn pd_store_posted_plan(
        &self,
        group: &GroupId,
        issue: u64,
        plan: plandoc::PlanDoc,
        comment_url: &str,
    ) {
        self.pd_store_posted_plan_at(group, issue, plan, comment_url, now_ms())
    }

    /// [`pd_store_posted_plan`](Self::pd_store_posted_plan), clock injected.
    #[doc(hidden)]
    pub fn pd_store_posted_plan_at(
        &self,
        group: &GroupId,
        issue: u64,
        plan: plandoc::PlanDoc,
        comment_url: &str,
        now: u64,
    ) {
        let dir = self.group_dir(group);
        let slices: Vec<String> = plan.slices.iter().map(|s| s.id.to_string()).collect();
        let (on_behalf, ok) = {
            let _state_guard = self.pd_state_lock.lock_safe();
            let Ok(mut state) = plandrive::load_state(&dir) else { return };
            let Some(entry) = state.entry_mut(issue) else { return };
            if entry.state() != PlanDriveState::Planning {
                return;
            }
            let on_behalf = entry.on_behalf_of.clone();
            if entry.store_plan(plan, comment_url, now).is_err() {
                return;
            }
            (on_behalf, plandrive::store_state(&dir, &state).is_ok())
        };
        self.pd_audit(
            group,
            &on_behalf,
            plandrive::audit_action::PLAN_POSTED,
            json!({ "issue": issue, "slices": slices, "comment_url": comment_url,
                    "persisted": ok }),
        );
    }

    // ---------- the roster ----------

    /// This group's roster as `(block id, is a worker)` pairs — what
    /// [`plandrive::validate_for_drive`] judges a plan's `block:` fields
    /// against.
    ///
    /// **The roster pinned at LAUNCH**, never `.orrerix/workflow.yml` re-read
    /// (§2(d)): consent to a roster is given when a group is launched, and a
    /// plan that named a block the human added to the file this morning would be
    /// consented to by nobody.
    fn pd_roster_blocks(&self, group: &GroupId) -> Vec<(String, bool)> {
        self.group(group)
            .map(|g| {
                g.guardrails
                    .blocks
                    .iter()
                    .map(|b| (b.id.to_string(), b.kind == Role::Worker))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The planner block this drive spawns, or the refusal.
    ///
    /// `named` overrides; with nothing named, the roster's **first**
    /// `kind: planner` block is the default — "the first block of a class is the
    /// default one" is the roster's own rule, stated in `workflows.md`, and
    /// re-deriving it here rather than inventing a second one is the point.
    fn pd_planner_block(
        &self,
        group: &GroupId,
        named: Option<&str>,
    ) -> Result<String, &'static str> {
        let g = self.group(group).ok_or(plandrive::refusal::UNAVAILABLE)?;
        match named.map(str::trim).filter(|s| !s.is_empty()) {
            Some(id) => g
                .guardrails
                .block(id)
                .filter(|b| b.kind == Role::Planner)
                .map(|b| b.id.to_string())
                .ok_or(plandrive::refusal::NO_PLANNER_BLOCK),
            None => g
                .guardrails
                .blocks
                .iter()
                .find(|b| b.kind == Role::Planner)
                .map(|b| b.id.to_string())
                .ok_or(plandrive::refusal::NO_PLANNER_BLOCK),
        }
    }

    /// Open the planner's pane.
    ///
    /// **No worktree** — a planner reads the repo and posts a comment; it never
    /// commits, and cutting a worktree per plan drive would spend a real `git`
    /// operation on a pane that has nothing to put in it. That is the same
    /// choice `spawn_agent` makes for a hand-spawned planner, so a driven
    /// planner and a hand-spawned one land in the same place.
    fn pd_spawn(
        &self,
        group: &GroupId,
        block: &str,
        task: &str,
    ) -> Result<AgentEntry, String> {
        self.spawn_agent_bound(
            group,
            Role::Planner,
            Some(block.to_string()),
            "",
            task,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    }

    /// The kickoff a driven planner receives.
    ///
    /// **Every interpolated value is sanitized at THIS call site**, which is
    /// what makes the sanitization a pin rather than a decoration: a test that
    /// sanitizes inside its own render harness asserts only that two functions
    /// compose, and passes identically while the live site hands raw text to a
    /// pane. The issue number is a `u64` and cannot carry anything; the block id
    /// came out of the roster; so what is left is the drive's own prose, and it
    /// still goes through the same door.
    fn pd_planner_brief(&self, issue: u64, consent: Consent, base: Option<&str>) -> String {
        let scope = match consent {
            Consent::Investigation => {
                "This issue is `agent-investigation`: research only. Your plan is the whole \
                 deliverable — orrerix will not board it and will not spawn a single worker off \
                 it."
            }
            Consent::Ready => {
                "This issue is `agent-ready`: orrerix will turn your plan into board rows and \
                 spawn a worker per slice. Write every `brief:` as the whole of what that worker \
                 will be told — it is delivered VERBATIM, and nobody rewrites it after you."
            }
        };
        let base_line = match base {
            Some(b) if !b.trim().is_empty() => format!(
                "\nEvery slice branches from `{}` unless its own `branch:` says otherwise.",
                pd_fact(b.trim())
            ),
            _ => String::new(),
        };
        format!(
            "Plan issue #{issue}.\n\n{scope}{base_line}\n\n\
             YOUR COMMENT MUST CARRY ONE FENCED ```{fence} BLOCK — that block IS how the plan \
             reaches orrerix, and prose beside it is read by humans only. Post it with the \
             `post_issue_comment` tool, not by shelling out to `gh`.\n\n\
             The block is validated BEFORE anything is posted: an invalid one comes back to you \
             as a tool error with the line numbers, nothing reaches the issue, and you fix it and \
             post again inside this same turn. After {max} refusals the drive stops and a human \
             is told. Nothing is repaired for you — an id, a branch or a dependency orrerix \
             refuses is refused, never rewritten.\n\n\
             The schema is in your instructions. When the comment is posted, `report(done)`.",
            fence = plandoc::FENCE_INFO,
            max = plandrive::MAX_INVALID_POSTS,
        )
    }

    // ---------- §5.1: the tools ----------

    /// **Hand one issue's planning to orrerix** (§2(b)).
    ///
    /// Orchestrator-only at the MCP arm; this half re-checks the policy, reads
    /// the issue once through `gh`, and opens the planner.
    pub fn drive_plan(
        &self,
        group: &GroupId,
        issue: u64,
        planner_block: Option<&str>,
        review_minutes: Option<u32>,
        base: Option<&str>,
        on_behalf_of: &str,
    ) -> Value {
        let injected = self.rd_runner_override.lock_safe().clone();
        let owned;
        let runner: &dyn rddrive::RdRunner = match injected.as_deref() {
            Some(r) => r,
            None => {
                let Some(repo) = self.group(group).map(|g| g.repo) else {
                    return self.pd_refuse(group, issue, plandrive::refusal::UNAVAILABLE);
                };
                owned = rddrive::runner_for(std::path::Path::new(&repo));
                &owned
            }
        };
        self.drive_plan_with(
            group,
            runner,
            issue,
            planner_block,
            review_minutes,
            base,
            on_behalf_of,
            now_ms(),
        )
    }

    /// [`drive_plan`](Self::drive_plan) with the `gh` seam and the clock
    /// injected — the seam `tests/plandrive.rs` uses to exercise the whole
    /// production path without spawning a child (CLAUDE.md constraint 3).
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn drive_plan_with(
        &self,
        group: &GroupId,
        runner: &dyn rddrive::RdRunner,
        issue: u64,
        planner_block: Option<&str>,
        review_minutes: Option<u32>,
        base: Option<&str>,
        on_behalf_of: &str,
        now: u64,
    ) -> Value {
        use plandrive::refusal as r;
        let (enabled, limits) = self.pd_policy(group);
        if !enabled {
            return self.pd_refuse(group, issue, r::DRIVER_DISABLED);
        }
        // The roster question is answered before the `gh` call, because it needs
        // no network and a group with no planner block can never drive a plan —
        // spending a round trip to tell it so would be spending one to learn
        // nothing.
        let block = match self.pd_planner_block(group, planner_block) {
            Ok(b) => b,
            Err(reason) => return self.pd_refuse(group, issue, reason),
        };

        let dir = self.group_dir(group);

        // §2(d): consent is the label, read here and re-read every tick.
        let facts = pd_issue_facts(runner, issue);
        match facts.open {
            Some(false) => return self.pd_refuse(group, issue, r::ISSUE_NOT_OPEN),
            None => return self.pd_refuse(group, issue, r::ISSUE_UNVERIFIABLE),
            Some(true) => {}
        }
        let Some(consent) = facts.consent() else {
            return self.pd_refuse(group, issue, r::NOT_LABELLED);
        };

        // **RESERVE THE ISSUE BEFORE SPAWNING** (rev-std round 1, finding 5).
        //
        // The entry is written first, with no planner on it, and the pane is
        // opened afterwards. The obvious order — spawn, then write — has a race
        // with a concrete input: an orchestrator batching two `drive_plan` calls
        // for one issue in a single turn. Both passed the unlocked
        // `already-driven` read, both opened a planner, and only the loser's
        // ENTRY was rejected under the lock. Its PANE was left running, unowned
        // and unaudited, so `pd_plan_check` answered `NotDriven` for it and it
        // published a second plan comment on the issue beside the winner's.
        // Killing the loser's pane afterwards would be the other repair, and it
        // is worse: it spends a delegate slot and a model's turn to undo work
        // that never needed doing.
        //
        // Reserving closes it at the only point where "is this issue driven"
        // and "does this issue have an entry" can be made the same question —
        // under the lock, before anything irreversible. The loser now refuses
        // `already-driven` having spawned nothing at all.
        //
        // **The lock still spans no spawn and no delivery**, which is the
        // property this file's tick doc rests on: the reservation is a
        // load-modify-store and nothing else, and the spawn happens after it is
        // released.
        let reserved = PdEntry::new(
            issue,
            on_behalf_of,
            &block,
            consent,
            base.map(str::to_string).filter(|b| !b.trim().is_empty()),
            review_minutes.unwrap_or(limits.plan_review_minutes),
            now,
        );
        let reservation = {
            let _state_guard = self.pd_state_lock.lock_safe();
            match plandrive::load_state(&dir) {
                // **NOT `not-driven`.** A torn file cannot tell you an issue is
                // not driven; it can only tell you orrerix cannot say.
                Err(_) => Err(r::STATE_UNREADABLE),
                Ok(mut state) => {
                    if state.is_driven(issue) {
                        Err(r::ALREADY_DRIVEN)
                    } else {
                        // A terminal entry for this issue is replaced: a drive
                        // that finished or was cancelled does not stop the issue
                        // being driven again, and `is_driven` above already said
                        // no live one exists.
                        state.entries.retain(|e| e.issue != issue);
                        state.entries.push(reserved);
                        plandrive::store_state(&dir, &state).map_err(|_| r::STATE_UNWRITABLE)
                    }
                }
            }
        };
        if let Err(reason) = reservation {
            return self.pd_refuse(group, issue, reason);
        }

        // The spawn, outside the lock. Its failure ROLLS THE RESERVATION BACK
        // rather than leaving a drive whose planner never opened — that entry
        // would sit in `planning` until `drive-stalled` hours later, for
        // something the caller can be told about now.
        let brief = self.pd_planner_brief(issue, consent, base);
        let agent = match self.pd_spawn(group, &block, &brief) {
            Ok(a) => a,
            Err(e) => {
                self.pd_release_reservation(&dir, issue);
                self.pd_audit(
                    group,
                    on_behalf_of,
                    plandrive::audit_action::REFUSED,
                    json!({ "issue": issue, "reason": r::PLANNER_UNSPAWNABLE, "detail": e }),
                );
                return json!({ "refused": r::PLANNER_UNSPAWNABLE, "detail": e });
            }
        };

        // The pane, recorded onto the reservation.
        //
        // **A failure here is the one case that still leaves a live pane the
        // record does not name**, and it is bounded and audited rather than
        // fixed. It needs the record to become unreadable or unwritable BETWEEN
        // the two locks of one call. Releasing the reservation on this path
        // would be the wrong repair — it would re-open the very hole this
        // ordering closes, since the pane is already open by now — so instead
        // the entry stays (which keeps a second `drive_plan` refusing
        // `already-driven`) and the audit row below names the pane, so the
        // orchestrator can kill it. Across a restart the reconcile names the
        // reservation itself; within a process the drive-age backstop is what
        // bounds it.
        let stored = {
            let _state_guard = self.pd_state_lock.lock_safe();
            match plandrive::load_state(&dir) {
                Ok(mut state) => match state.entry_mut(issue) {
                    Some(entry) => {
                        entry.planner_agent = agent.id.clone();
                        entry.planner_session = agent.session_id.clone().unwrap_or_default();
                        entry.spawned_ms = now;
                        plandrive::store_state(&dir, &state).map_err(|_| r::STATE_UNWRITABLE)
                    }
                    // Cancelled out from under this call between the two locks.
                    // Nothing to attach the pane to, and nothing to roll back.
                    None => Err(r::NOT_DRIVEN),
                },
                Err(_) => Err(r::STATE_UNREADABLE),
            }
        };
        if let Err(reason) = stored {
            self.pd_audit(
                group,
                on_behalf_of,
                plandrive::audit_action::REFUSED,
                json!({ "issue": issue, "reason": reason,
                        "orrerix_fault": plandrive::refusal::is_orrerix_fault(reason),
                        // The pane this call opened and could not record. Named
                        // rather than left for someone to find, because it is
                        // live and nothing else knows about it.
                        "orphaned_pane": agent.id }),
            );
            return json!({ "refused": reason, "orphaned_pane": agent.id });
        }

        self.pd_audit(
            group,
            on_behalf_of,
            plandrive::audit_action::STARTED,
            json!({ "issue": issue, "consent": consent.as_str(), "block": block,
                    "title": facts.title }),
        );
        self.pd_audit(
            group,
            on_behalf_of,
            plandrive::audit_action::PLANNER_SPAWNED,
            json!({ "issue": issue, "agent": agent.id, "block": block }),
        );
        json!({
            "driving": true,
            "issue": issue,
            "consent": consent.as_str(),
            "planner": agent.id,
            "planner_block": block,
        })
    }

    /// Where this group's plan drives stand (§2(f)). Read-only.
    pub fn plan_drive_status(&self, group: &GroupId) -> Value {
        self.plan_drive_status_with(group, now_ms())
    }

    #[doc(hidden)]
    pub fn plan_drive_status_with(&self, group: &GroupId, now: u64) -> Value {
        let enabled = self.plan_driver_enabled(group);
        let dir = self.group_dir(group);
        let state = {
            let _state_guard = self.pd_state_lock.lock_safe();
            match plandrive::load_state(&dir) {
                Ok(s) => s,
                // The same distinction the mutating tools make: "orrerix cannot
                // read the record" is not "there is nothing in it".
                Err(_) => {
                    return json!({ "enabled": enabled,
                                   "refused": plandrive::refusal::STATE_UNREADABLE })
                }
            }
        };
        let drives: Vec<Value> = state
            .entries
            .iter()
            .filter(|e| !e.state().is_terminal())
            .map(|e| {
                json!({
                    "issue": e.issue,
                    "state": e.state().as_str(),
                    "held_reason": e.held_reason.map(|r| r.as_str()),
                    "held_state": e.held_from.map(|s| s.as_str()),
                    "consent": e.consent.as_str(),
                    "planner": e.planner_agent,
                    "planner_block": e.planner_block,
                    "comment_url": e.comment_url,
                    "invalid_count": e.invalid_count,
                    "last_invalid": e.last_invalid,
                    "review_minutes": e.review_minutes,
                    "slices": e.plan.as_ref().map(|p| p.slices.iter().map(|s| json!({
                        "id": s.id.as_str(),
                        "title": s.title,
                        "branch": s.branch.as_str(),
                        "block": s.block,
                        "deps": s.deps.iter().map(|d| d.as_str()).collect::<Vec<_>>(),
                        "hold": s.hold,
                        // Everything below is the RUN record, absent until the
                        // drive has boarded: `null` rather than an invented id
                        // or a state the drive has not reached.
                        "task_id": e.slices.get(s.id.as_str()).map(|r| &r.task_id),
                        "slice_state": e.slices.get(s.id.as_str()).map(|r| r.state().as_str()),
                        "slice_hold": e.slices.get(s.id.as_str())
                            .and_then(|r| r.hold).map(|h| h.as_str()),
                        "agent": e.slices.get(s.id.as_str())
                            .map(|r| r.agent.as_str()).filter(|a| !a.is_empty()),
                        "pr": e.slices.get(s.id.as_str())
                            .map(|r| r.pr).filter(|p| *p > 0),
                    })).collect::<Vec<_>>()),
                    // Derived, never stored: a stored AGE is stale the instant
                    // it is written and meaningless across a restart.
                    "since_ms": e.age_ms(now),
                    "state_ms": e.state_elapsed_ms(now),
                })
            })
            .collect();
        json!({ "enabled": enabled, "drives": drives })
    }

    /// **Stop driving an issue.** Works in any non-terminal state.
    ///
    /// **It kills nothing** (§2(c)), and that is the contract rather than an
    /// omission: the planner pane, if one is open, keeps running under the
    /// orchestrator, which is where its traffic starts going again the moment
    /// the entry stops being live. Cancel releases OWNERSHIP; ending a pane is
    /// the orchestrator's own `kill_agent`, unchanged.
    pub fn cancel_plan_drive(&self, group: &GroupId, issue: u64, on_behalf_of: &str) -> Value {
        self.cancel_plan_drive_with(group, issue, on_behalf_of, now_ms())
    }

    #[doc(hidden)]
    pub fn cancel_plan_drive_with(
        &self,
        group: &GroupId,
        issue: u64,
        on_behalf_of: &str,
        now: u64,
    ) -> Value {
        use plandrive::refusal as r;
        if !self.plan_driver_enabled(group) {
            return self.pd_refuse(group, issue, r::DRIVER_DISABLED);
        }
        let dir = self.group_dir(group);
        // Read out of the entry BEFORE the lock is dropped, and it is the only
        // thing that survives it: a cancel is the one exit whose caller is a
        // tool, so the pane list has to reach the tool's own return value, which
        // is what an orchestrator acts on without waiting for a prompt.
        let planner: String;
        {
            let _state_guard = self.pd_state_lock.lock_safe();
            let mut state = match plandrive::load_state(&dir) {
                Ok(s) => s,
                Err(_) => return self.pd_refuse(group, issue, r::STATE_UNREADABLE),
            };
            let Some(entry) = state.entry_mut(issue) else {
                return self.pd_refuse(group, issue, r::NOT_DRIVEN);
            };
            if entry.state().is_terminal() {
                return self.pd_refuse(group, issue, r::NOT_DRIVEN);
            }
            planner = entry.planner_agent.clone();
            if entry.advance(PlanDriveState::Cancelled, None, now).is_err() {
                return self.pd_refuse(group, issue, r::NOT_DRIVEN);
            }
            // A cancelled drive owes nobody a notice: the orchestrator is the
            // one who asked.
            entry.notice_delivered();
            if plandrive::store_state(&dir, &state).is_err() {
                return self.pd_refuse(group, issue, r::STATE_UNWRITABLE);
            }
        }
        self.pd_clear_signal(group, issue);
        self.pd_audit(
            group,
            on_behalf_of,
            plandrive::audit_action::CANCELLED,
            json!({ "issue": issue, "planner": planner, "killed": Vec::<String>::new() }),
        );
        json!({ "cancelled": true, "issue": issue, "planner_left_running": planner })
    }

    /// **Resume a parked drive**, back into the state the hold came from.
    pub fn resume_plan_drive(&self, group: &GroupId, issue: u64, on_behalf_of: &str) -> Value {
        self.resume_plan_drive_with(group, issue, on_behalf_of, now_ms())
    }

    #[doc(hidden)]
    pub fn resume_plan_drive_with(
        &self,
        group: &GroupId,
        issue: u64,
        on_behalf_of: &str,
        now: u64,
    ) -> Value {
        use plandrive::refusal as r;
        if !self.plan_driver_enabled(group) {
            return self.pd_refuse(group, issue, r::DRIVER_DISABLED);
        }
        let dir = self.group_dir(group);
        let (from, to) = {
            let _state_guard = self.pd_state_lock.lock_safe();
            let mut state = match plandrive::load_state(&dir) {
                Ok(s) => s,
                Err(_) => return self.pd_refuse(group, issue, r::STATE_UNREADABLE),
            };
            let Some(entry) = state.entry_mut(issue) else {
                return self.pd_refuse(group, issue, r::NOT_DRIVEN);
            };
            if entry.state().is_terminal() {
                return self.pd_refuse(group, issue, r::NOT_DRIVEN);
            }
            // A LIVE drive has nothing to resume, and saying `not-driven` would
            // be false — it is driven, it is simply not parked. The two want
            // different things from the orchestrator.
            if !entry.state().is_parked() {
                return self.pd_refuse(group, issue, r::NOT_HELD);
            }
            let from = entry.held_reason;
            let to = entry.resume_target();
            if entry.advance(to, None, now).is_err() {
                return self.pd_refuse(group, issue, r::NOT_HELD);
            }
            // The refusal counter is the one thing a resume clears: a human who
            // has read the reasons and resumed anyway is spending a fresh three,
            // visibly, rather than resuming straight back onto the bound.
            entry.invalid_count = 0;
            entry.last_invalid.clear();
            if plandrive::store_state(&dir, &state).is_err() {
                return self.pd_refuse(group, issue, r::STATE_UNWRITABLE);
            }
            (from, to)
        };
        self.pd_audit(
            group,
            on_behalf_of,
            plandrive::audit_action::RESUMED,
            json!({ "issue": issue, "from": from.map(|f| f.as_str()), "to": to.as_str() }),
        );
        json!({ "resumed": true, "issue": issue, "state": to.as_str() })
    }

    // ---------- §2.4: the tick ----------

    /// The one group this wake will service, or `None`.
    ///
    /// `next_rd_group`'s three filters, cheapest first, each of them a reason
    /// not to spend a subprocess: not inside a backoff window, has a
    /// `plan_drives.json` at all (a **file check** rather than a parse, so an
    /// unreadable file still reaches the driver's own loud handling), and the
    /// repo declares the plan driver.
    fn next_pd_group(&self, now: u64) -> Option<GroupId> {
        let all: Vec<GroupId> = self.groups.lock_safe().keys().cloned().collect();
        let service = self.pd_service_ms.lock_safe().clone();
        let mut due: Vec<(u64, GroupId)> = all
            .into_iter()
            .filter(|g| service.get(g).map(|t| now >= *t).unwrap_or(true))
            .filter(|g| plandrive::state_path(&self.group_dir(g)).exists())
            .filter(|g| self.plan_driver_enabled(g))
            .map(|g| (service.get(&g).copied().unwrap_or(0), g))
            .collect();
        // Oldest-serviced first; the group id breaks a tie deterministically so
        // two never-serviced groups do not alternate on HashMap iteration order.
        due.sort();
        due.into_iter().next().map(|(_, g)| g)
    }

    /// One plan-driver step, for at most one group — the SIXTH step in
    /// `gh_poll_tick`, after `rd_driver_tick`, clock injected.
    ///
    /// **After the review driver on the same wake, and that ordering is the
    /// bound rather than a preference.** Both drivers spend `gh` round trips on
    /// one shared poll loop; running the plan driver second means it can only
    /// ever take what the review driver left, which is how "the plan driver
    /// holds, never starves the review driver" is enforced structurally instead
    /// of by a budget nobody can check.
    pub fn pd_driver_tick(&self, now: u64) -> Option<GroupId> {
        let group = self.next_pd_group(now)?;
        let injected = self.rd_runner_override.lock_safe().clone();
        match injected {
            Some(r) => {
                self.pd_drive_group_with(&group, r.as_ref(), now);
            }
            None => {
                let repo = self.group(&group).map(|g| g.repo)?;
                let runner = rddrive::runner_for(std::path::Path::new(&repo));
                self.pd_drive_group_with(&group, &runner, now);
            }
        }
        Some(group)
    }

    /// Drive one group's plan drives with the `gh` seam injected.
    ///
    /// **The lock spans the read-decide-store and nothing else.** No spawn and
    /// no delivery happens under `pd_state_lock` in P3a — the planner spawn is
    /// `drive_plan`'s, outside it, and every notice this produces is delivered
    /// below, after the lock is dropped. That is a narrower claim than the
    /// review driver's and it is deliberately not the same one: when P3b adds
    /// slice spawns under this lock it owes `rd_drive_group_with`'s argument
    /// about pane deliveries, and inheriting it now would be inheriting a proof
    /// about code that is not here.
    #[doc(hidden)]
    pub fn pd_drive_group_with(
        &self,
        group: &GroupId,
        runner: &dyn rddrive::RdRunner,
        now: u64,
    ) -> PdDriveReport {
        let mut report = PdDriveReport::default();
        let (enabled, limits) = self.pd_policy(group);
        if !enabled {
            report.disabled = true;
            return report;
        }
        self.pd_reconcile_with(group, runner, now);

        let dir = self.group_dir(group);
        // The issues this tick will look at, read out under the lock and then
        // released: the `gh` calls below are child processes, and holding a
        // registry lock across one would put a network timeout inside every
        // other caller's wait.
        let live: Vec<u64> = {
            let _state_guard = self.pd_state_lock.lock_safe();
            match plandrive::load_state(&dir) {
                Ok(s) => s.entries.iter().filter(|e| e.state().is_live()).map(|e| e.issue).collect(),
                Err(_) => {
                    report.state_unreadable = true;
                    Vec::new()
                }
            }
        };
        if report.state_unreadable {
            self.pd_audit(
                group,
                "",
                plandrive::audit_action::STATE_UNREADABLE,
                json!({ "path": plandrive::state_path(&dir).to_string_lossy() }),
            );
            self.pd_defer(group, now.saturating_add(rddrive::RD_BACKOFF_MS));
            return report;
        }

        let mut runner_failed = false;
        // **At most ONE spawn per group per wake** (§2(b) step 6), shared across
        // every drive this tick services. The review driver has already spent
        // its own budget by the time this runs — `pd_driver_tick` is the SIXTH
        // step of `gh_poll_tick` — so a plan drive can only ever take the slot
        // the review driver left. That ordering is what makes "the plan driver
        // holds, never starves the review driver" a structural fact rather than
        // a budget nobody can check.
        let mut spawn_budget = 1usize;
        for (i, issue) in live.iter().copied().enumerate() {
            if i >= PD_MAX_GH_PER_TICK {
                report.deferred.push(issue);
                continue;
            }
            // Every `gh` read this issue needs, taken before the state lock.
            let obs = pd_issue_facts(runner, issue);
            runner_failed |= obs.runner_failed;
            let signal = self.pd_signal(group, issue);
            let reads = {
                let _state_guard = self.pd_state_lock.lock_safe();
                plandrive::load_state(&dir).ok().and_then(|s| s.entry(issue).cloned())
            }
            .map(|e| self.pd_pr_reads(runner, &e, &signal))
            .unwrap_or_default();
            runner_failed |= reads.runner_failed;

            let Some(outcome) = self.pd_execute(
                group,
                issue,
                &obs,
                &signal,
                &reads,
                &limits,
                &mut spawn_budget,
                now,
            ) else {
                continue;
            };

            // ---- outside the lock from here ----
            for (action, detail) in &outcome.audits {
                self.pd_audit(group, &outcome.on_behalf, action, detail.clone());
            }
            // The hand-off to the REVIEW driver. Outside `pd_state_lock` because
            // `drive_review_with` reads GitHub and takes its own record's lock,
            // and `gh` never runs under this one.
            for (slice, pr, session) in &outcome.hand_offs {
                self.pd_hand_off(group, runner, issue, slice, *pr, session, &outcome.on_behalf, now);
            }
            if outcome.advanced {
                report.advanced.push(issue);
            }
            self.pd_clear_signal(group, issue);
            if !outcome.action.is_empty() {
                self.pd_audit(
                    group,
                    &outcome.on_behalf,
                    outcome.action,
                    json!({ "issue": issue, "reason": outcome.reason }),
                );
            }
            for text in &outcome.notices {
                let ok = self.deliver_to_orchestrator(group, text, brand::AUDIT_ACTOR).is_ok();
                self.pd_audit(
                    group,
                    &outcome.on_behalf,
                    plandrive::audit_action::NOTICE,
                    json!({ "issue": issue, "delivered": ok, "at": now }),
                );
                if ok {
                    report.notices += 1;
                }
            }
        }

        report.notices += self.pd_flush_notices(group, now);
        if runner_failed {
            self.pd_defer(group, now.saturating_add(rddrive::RD_BACKOFF_MS));
        }
        report
    }

    // ---------- §2(b) steps 5-7: the executor ----------

    /// **Board the plan** (§2(b) step 5): one parent row for the issue, one
    /// child row per slice.
    ///
    /// **Through `upsert_task` as an AGENT**, never a direct `write_tasks`, and
    /// that is the whole design of this step rather than an implementation
    /// detail. An agent-origin write is what makes `find_dep_cycle` run on the
    /// dep edges, what makes the WIP caps a REFUSAL rather than a warning, and
    /// what makes the ladder and link validation apply — the driver gets
    /// exactly the board authority the orchestrator's own boarding has, and not
    /// one check less.
    ///
    /// **The parent row is REUSED if the issue already has one.** An
    /// orchestrator that boarded the issue before handing it over must not end
    /// up with two rows for it; matching on the row's own `issue` field is how
    /// that is decided, because that is the field both writers set.
    ///
    /// Answers the slice-id -> task-id map, or the first refusal. A partial
    /// board is possible and is deliberately not rolled back: the rows that
    /// landed are real work the human can see, and `boarded` stays false until
    /// every slice has one, so the next tick finishes the job.
    fn pd_board(
        &self,
        group: &GroupId,
        issue: u64,
        title: &str,
        plan: &plandoc::PlanDoc,
        comment_url: &str,
        existing: &std::collections::BTreeMap<String, plandrive::PdSlice>,
    ) -> Result<std::collections::BTreeMap<String, plandrive::PdSlice>, String> {
        let actor = brand::AUDIT_ACTOR;
        let issue_ref = format!("#{issue}");
        let board = self.tasks(group);
        // The parent, reused when the issue already has a row. `kind` is left
        // alone on a REUSED row: the orchestrator may have boarded the issue as
        // an epic, a feature or a plain task, and re-levelling somebody else's
        // row is not this driver's call.
        let parent = match board.iter().find(|t| t.issue.as_deref() == Some(issue_ref.as_str())) {
            Some(t) => t.id.clone(),
            None => {
                self.upsert_task(
                    group,
                    actor,
                    None,
                    super::TaskPatch {
                        title: Some(if title.trim().is_empty() {
                            format!("#{issue}")
                        } else {
                            title.trim().to_string()
                        }),
                        issue: Some(issue_ref.clone()),
                        ..super::TaskPatch::default()
                    },
                )?
                .id
            }
        };

        // Two passes, because a slice's `deps` name SLICE ids and a row's deps
        // name TASK ids: nothing can be dep-linked until every row exists.
        let mut map = existing.clone();
        for s in &plan.slices {
            if map.contains_key(s.id.as_str()) {
                continue;
            }
            let row = self.upsert_task(
                group,
                actor,
                None,
                super::TaskPatch {
                    title: Some(format!("{} {}", s.id.as_str(), s.title)),
                    issue: Some(issue_ref.clone()),
                    parent: Some(parent.clone()),
                    links: Some(vec![super::TaskLink {
                        link_type: "spec".into(),
                        target: comment_url.to_string(),
                        label: Some(format!("plan #{issue}, slice {}", s.id.as_str())),
                    }]),
                    ..super::TaskPatch::default()
                },
            )?;
            map.insert(s.id.as_str().to_string(), plandrive::PdSlice::boarded(&row.id));
        }
        for s in &plan.slices {
            if s.deps.is_empty() {
                continue;
            }
            let Some(me) = map.get(s.id.as_str()) else { continue };
            let deps: Vec<String> =
                s.deps.iter().filter_map(|d| map.get(d.as_str()).map(|t| t.task_id.clone())).collect();
            if deps.len() != s.deps.len() {
                continue;
            }
            self.upsert_task(
                group,
                actor,
                Some(&me.task_id),
                super::TaskPatch { deps: Some(deps), ..super::TaskPatch::default() },
            )?;
        }
        Ok(map)
    }

    /// The brief one slice's worker is kicked off with (§3).
    ///
    /// **Every interpolated value is sanitized at THIS call site**, and the
    /// planner's own `brief:` is the one that matters: it is LLM output about to
    /// be typed into another agent's pane, which is the same trust boundary the
    /// review driver's §5.5 crosses. `notify::sanitize_pane_text` is the only
    /// thing that touches it — no rewrite, no truncation, no re-wrap — and the
    /// composition itself is [`plandrive::slice_brief`], which is pure and
    /// pinned in the engine.
    fn pd_slice_brief(&self, issue: u64, slice: &plandoc::Slice, base: Option<&str>) -> String {
        plandrive::slice_brief(
            issue,
            slice,
            base.map(pd_fact).as_deref(),
            &notify::sanitize_pane_text(&slice.brief, PD_BRIEF_CAP, notify::Lines::Keep),
            &super::brief::dod_trailer(),
        )
    }

    /// Whether this slice may be spawned THIS tick, given the board.
    ///
    /// Readiness is re-derived from `tasks.json` every tick and never cached
    /// (§2(c)) — that is exactly what makes a human's board edit work while a
    /// drive is running: strike a dep, mark a row done, set one `blocked`, and
    /// the next tick simply reads what the board now says.
    fn pd_slice_spawnable(
        slice: &plandoc::Slice,
        run: &plandrive::PdSlice,
        board: &[super::Task],
    ) -> bool {
        // §2(c)(i): the planner flagged this slice as carrying a design call.
        // The driver never spawns it; the orchestrator briefs it by hand.
        if slice.hold {
            return false;
        }
        if run.state() != plandrive::SliceState::Queued {
            return false;
        }
        board
            .iter()
            .find(|t| t.id == run.task_id)
            .is_some_and(|t| super::task_ready(t, board))
    }

    /// One slice's PR, read from the worker's `ref` and falling back to the one
    /// `gh pr list --head <branch>` §2(b) step 6 allows.
    ///
    /// **The `ref` is a HINT, not an authority**, and the distinction matters
    /// because `ref` is text the delegate supplies. It never decides whether the
    /// report is intercepted — [`pd_slice_owner`](Self::pd_slice_owner) does
    /// that, keyed on the agent id orrerix minted — and the number it yields is
    /// handed straight to the review driver, which re-reads the PR itself. What
    /// a wrong `ref` can do is send this drive's slice to the wrong PR, and the
    /// remedy for that is the same as for a worker that opened the wrong PR:
    /// the human sees it on the board.
    fn pd_pr_from_ref(hint: &str) -> Option<u64> {
        let t = hint.trim().trim_start_matches('#');
        // A URL's last path segment, or a bare number. Nothing else: guessing a
        // number out of prose is how a drive drives the wrong PR.
        let tail = t.rsplit('/').next().unwrap_or(t);
        tail.parse::<u64>().ok().filter(|n| *n > 0)
    }

    /// Everything one tick's `gh` reads told the executor about one drive.
    ///
    /// Read OUTSIDE `pd_state_lock`, like every other `gh` call this driver
    /// makes: these are child processes, and holding a registry lock across one
    /// would put a network timeout inside every other caller's wait.
    fn pd_pr_reads(
        &self,
        runner: &dyn rddrive::RdRunner,
        entry: &PdEntry,
        signal: &PdSignal,
    ) -> PdPrReads {
        let mut reads = PdPrReads::default();
        let Some(plan) = entry.plan.as_ref() else { return reads };

        // (a) The PR a `done`-reporting worker has just produced. At most one
        // per slice per tick, and only for a slice whose PR is not known yet:
        // the `ref` covers the normal case for free.
        for (id, sig) in &signal.workers {
            let PdWorkerSignal::Done { pr_ref } = sig else { continue };
            let Some(run) = entry.slices.get(id) else { continue };
            if run.pr > 0 || run.state() != plandrive::SliceState::Running {
                continue;
            }
            if let Some(pr) = Self::pd_pr_from_ref(pr_ref) {
                reads.resolved.insert(id.clone(), pr);
                continue;
            }
            let Some(slice) = plan.slice(id) else { continue };
            let branch = slice.branch.as_str().to_string();
            let out = runner.gh(&[
                "pr",
                "list",
                "--head",
                &branch,
                "--state",
                "all",
                "--json",
                "number",
                "--limit",
                "1",
            ]);
            match out {
                Err(_) => reads.runner_failed = true,
                Ok(o) if o.ok() => {
                    if let Ok(rows) = serde_json::from_str::<Vec<RawPrNumber>>(o.line()) {
                        if let Some(n) = rows.first().map(|r| r.number).filter(|n| *n > 0) {
                            reads.resolved.insert(id.clone(), n);
                        }
                    }
                }
                Ok(_) => {}
            }
        }

        // (b) Where each in-review slice's PR got to, round-robin and bounded.
        // The cursor is PERSISTED (`pr_poll_cursor`) rather than restarted at
        // zero every tick, which is what makes this fair: a drive with more
        // in-review slices than the budget would otherwise poll the same first
        // four forever and never notice the fifth merging.
        let watching: Vec<(String, u64)> = entry
            .slices
            .iter()
            .filter(|(_, s)| s.state() == plandrive::SliceState::InReview && s.pr > 0)
            .map(|(id, s)| (id.clone(), s.pr))
            .collect();
        if watching.is_empty() {
            return reads;
        }
        let start = entry.pr_poll_cursor as usize % watching.len();
        let take = PD_MAX_PR_CHECKS_PER_TICK.min(watching.len());
        for k in 0..take {
            let (id, pr) = &watching[(start + k) % watching.len()];
            let n = pr.to_string();
            match runner.gh(&["pr", "view", &n, "--json", "state,mergedAt"]) {
                Err(_) => reads.runner_failed = true,
                Ok(o) if o.ok() => {
                    if let Ok(raw) = serde_json::from_str::<RawPrState>(o.line()) {
                        reads.pr_state.insert(id.clone(), raw.outcome());
                    }
                }
                Ok(_) => {}
            }
        }
        reads.cursor_advance = take as u64;
        reads
    }

    /// **One drive's executor**, run under `pd_state_lock` with every `gh` read
    /// already in hand.
    ///
    /// The lock spans the board write and the spawn, which is the review
    /// driver's own §2.4 choice and is made for its reason: the
    /// load-decide-store spans a spawn, and a `drive_plan`, `cancel_plan_drive`
    /// or `pd_store_posted_plan` landing inside that window would otherwise read
    /// the pre-spawn file and write it back, erasing the very entry this tick
    /// just advanced. What is deliberately NOT inside it is anything that talks
    /// to GitHub (read above, into `reads`) and anything that types into a pane
    /// — the notices are owed here and delivered by `pd_flush_notices` after the
    /// lock is dropped, and the hand-off to the review driver is returned for
    /// the caller to perform.
    #[allow(clippy::too_many_arguments)]
    fn pd_execute(
        &self,
        group: &GroupId,
        issue: u64,
        obs: &PdIssueObs,
        signal: &PdSignal,
        reads: &PdPrReads,
        limits: &PdLimits,
        spawn_budget: &mut usize,
        now: u64,
    ) -> Option<PdTickOutcome> {
        let dir = self.group_dir(group);
        let _state_guard = self.pd_state_lock.lock_safe();
        let mut state = plandrive::load_state(&dir).ok()?;
        let entry = state.entry_mut(issue)?;
        if !entry.state().is_live() {
            return None;
        }
        let mut out = PdTickOutcome { on_behalf: entry.on_behalf_of.clone(), ..Default::default() };

        // ---- 1. what the gh reads and the worker signals changed ----
        for (id, pr) in &reads.resolved {
            if let Some(run) = entry.slices.get_mut(id) {
                run.pr = *pr;
                out.audits.push((
                    plandrive::audit_action::SLICE_PR,
                    json!({ "issue": issue, "slice": id, "pr": pr }),
                ));
            }
        }
        entry.pr_poll_cursor = entry.pr_poll_cursor.saturating_add(reads.cursor_advance);

        // A worker's `blocked` parks ITS slice and nothing else (§2(e)).
        for (id, sig) in &signal.workers {
            let PdWorkerSignal::Blocked { note } = sig else { continue };
            let Some(run) = entry.slices.get_mut(id) else { continue };
            if run.state() != plandrive::SliceState::Running {
                continue;
            }
            run.advance(plandrive::SliceState::Held, Some(plandrive::PdSliceHold::WorkerBlocked));
            entry.note_progress(now);
            out.audits.push((
                plandrive::audit_action::SLICE_HELD,
                json!({ "issue": issue, "slice": id,
                        "reason": plandrive::PdSliceHold::WorkerBlocked.as_str() }),
            ));
            out.notices.push(format!(
                "[orrerix] plan drive #{issue}: slice {id} HELD ({}) — {}. It said: {}",
                plandrive::PdSliceHold::WorkerBlocked.as_str(),
                plandrive::PdSliceHold::WorkerBlocked.notice_line(),
                pd_fact(note),
            ));
        }

        // A PR that positively MERGED marks its row done; one positively CLOSED
        // without merging parks the slice. **Only those two are positive** — a
        // PR orrerix could not read leaves the slice exactly where it was.
        let mut mark_done: Vec<(String, String)> = Vec::new();
        for (id, outcome) in &reads.pr_state {
            let Some(run) = entry.slices.get_mut(id) else { continue };
            if run.state() != plandrive::SliceState::InReview {
                continue;
            }
            let pr = run.pr;
            match outcome {
                PrOutcome::Merged => {
                    run.advance(plandrive::SliceState::Done, None);
                    let task_id = run.task_id.clone();
                    mark_done.push((id.clone(), task_id));
                    entry.note_progress(now);
                    out.audits.push((
                        plandrive::audit_action::SLICE_MERGED,
                        json!({ "issue": issue, "slice": id, "pr": pr }),
                    ));
                }
                PrOutcome::ClosedUnmerged => {
                    run.advance(
                        plandrive::SliceState::Held,
                        Some(plandrive::PdSliceHold::PrClosed),
                    );
                    entry.note_progress(now);
                    out.audits.push((
                        plandrive::audit_action::SLICE_HELD,
                        json!({ "issue": issue, "slice": id, "pr": pr,
                                "reason": plandrive::PdSliceHold::PrClosed.as_str() }),
                    ));
                    out.notices.push(format!(
                        "[orrerix] plan drive #{issue}: slice {id} HELD ({}) — {}",
                        plandrive::PdSliceHold::PrClosed.as_str(),
                        plandrive::PdSliceHold::PrClosed.notice_line(),
                    ));
                }
                PrOutcome::Open | PrOutcome::Unknown => {}
            }
        }

        // ---- 2. board the plan, when that is where the drive is ----
        let st = entry.state();
        if st == PlanDriveState::Boarding {
            if let Some(plan) = entry.plan.clone() {
                let comment_url = entry.comment_url.clone();
                let existing = entry.slices.clone();
                match self.pd_board(group, issue, &obs.title, &plan, &comment_url, &existing) {
                    Ok(map) => {
                        let entry = state.entry_mut(issue)?;
                        let fresh = map.len() != entry.slices.len();
                        entry.slices = map;
                        if fresh {
                            entry.note_progress(now);
                            let ids: std::collections::BTreeMap<&str, &str> = entry
                                .slices
                                .iter()
                                .map(|(k, v)| (k.as_str(), v.task_id.as_str()))
                                .collect();
                            out.audits.push((
                                plandrive::audit_action::BOARDED,
                                json!({ "issue": issue, "rows": ids }),
                            ));
                        }
                    }
                    // A refusal — a WIP cap, a cycle, an unwritable board — is
                    // not a hold. The rows that landed stand, `boarded` stays
                    // false, and the next tick finishes the job; the whole-drive
                    // backstop is what bounds "forever".
                    Err(e) => out.audits.push((
                        plandrive::audit_action::REFUSED,
                        json!({ "issue": issue, "reason": "boarding-refused", "detail": pd_fact(&e) }),
                    )),
                }
            }
        }

        // ---- 3. mark the merged rows done, and re-read the board ----
        for (id, task_id) in &mark_done {
            if let Err(e) = self.upsert_task(
                group,
                brand::AUDIT_ACTOR,
                Some(task_id),
                super::TaskPatch {
                    status: Some("done".into()),
                    ..super::TaskPatch::default()
                },
            ) {
                out.audits.push((
                    plandrive::audit_action::REFUSED,
                    json!({ "issue": issue, "slice": id, "reason": "row-not-marked-done",
                            "detail": pd_fact(&e) }),
                ));
            }
        }
        let board = self.tasks(group);
        let entry = state.entry_mut(issue)?;

        // ---- 4. §2(d): consent is re-read before every spawn ----
        // The label was read this tick, by the same `issue view` the drive's own
        // facts came from. `decide` holds the drive on a withdrawal below; this
        // is the SPAWN's own guard, and it is not redundant with that one — the
        // hold is a state change the tick takes after this point, and a spawn
        // that had already happened cannot be un-spawned by it.
        let consent_ok = obs.consent_read != Some(None);

        // ---- 5. at most ONE spawn per group per tick ----
        let mut hand_offs: Vec<(String, u64, String)> = Vec::new();
        if entry.state() == PlanDriveState::Running {
            if let Some(plan) = entry.plan.clone() {
                let base = entry.base.clone();
                for slice in &plan.slices {
                    let id = slice.id.as_str();
                    let Some(run) = entry.slices.get(id) else { continue };
                    // (a) a worker that reported `done` hands its PR to the
                    //     review driver — performed by the caller, outside this
                    //     lock, because `drive_review_with` reads GitHub.
                    if matches!(signal.workers.get(id), Some(PdWorkerSignal::Done { .. }))
                        && run.state() == plandrive::SliceState::Running
                        && run.pr > 0
                    {
                        hand_offs.push((id.to_string(), run.pr, run.session.clone()));
                        continue;
                    }
                    // (b) the spawn.
                    if *spawn_budget == 0 || !consent_ok {
                        continue;
                    }
                    if !Self::pd_slice_spawnable(slice, run, &board) {
                        continue;
                    }
                    let task_id = run.task_id.clone();
                    match self.pd_spawn_slice(group, issue, slice, base.as_deref(), &task_id, now) {
                        Ok(agent) => {
                            *spawn_budget -= 1;
                            let run = entry.slices.get_mut(id)?;
                            run.agent = agent.id.clone();
                            run.session = agent.session_id.clone().unwrap_or_default();
                            run.spawned_ms = now;
                            run.cap_starved_since_ms = 0;
                            run.advance(plandrive::SliceState::Running, None);
                            entry.note_progress(now);
                            out.audits.push((
                                plandrive::audit_action::SLICE_SPAWNED,
                                json!({ "issue": issue, "slice": id, "agent": agent.id,
                                        "block": slice.block, "branch": slice.branch.as_str(),
                                        "task_id": task_id }),
                            ));
                        }
                        Err(e) => {
                            // §2(b) step 6: a CAP refusal is not an error. The
                            // row stays queued and is retried every tick; the
                            // clock this starts is the bound on retrying
                            // forever.
                            let capped = super::is_live_cap_refusal(&e);
                            let run = entry.slices.get_mut(id)?;
                            if capped {
                                if run.cap_starved_since_ms == 0 {
                                    run.cap_starved_since_ms = now;
                                }
                                let starved = now.saturating_sub(run.cap_starved_since_ms);
                                out.audits.push((
                                    plandrive::audit_action::SLICE_CAP_REFUSED,
                                    json!({ "issue": issue, "slice": id, "starved_ms": starved }),
                                ));
                                if starved >= super::reviewdrive::CAP_HOLD_MS {
                                    run.advance(
                                        plandrive::SliceState::Held,
                                        Some(plandrive::PdSliceHold::CapFull),
                                    );
                                    entry.note_progress(now);
                                    out.audits.push((
                                        plandrive::audit_action::SLICE_HELD,
                                        json!({ "issue": issue, "slice": id,
                                                "reason": plandrive::PdSliceHold::CapFull.as_str() }),
                                    ));
                                    out.notices.push(format!(
                                        "[orrerix] plan drive #{issue}: slice {id} HELD ({}) — {}",
                                        plandrive::PdSliceHold::CapFull.as_str(),
                                        plandrive::PdSliceHold::CapFull.notice_line(),
                                    ));
                                }
                            } else {
                                out.audits.push((
                                    plandrive::audit_action::REFUSED,
                                    json!({ "issue": issue, "slice": id,
                                            "reason": plandrive::refusal::SLICE_UNSPAWNABLE,
                                            "detail": pd_fact(&e) }),
                                ));
                            }
                            // A refused spawn must leave the row exactly as the
                            // drive found it, or a WIP cap would strand a row
                            // `in-progress` with nobody on it.
                            let _ = self.upsert_task(
                                group,
                                brand::AUDIT_ACTOR,
                                Some(&task_id),
                                super::TaskPatch {
                                    status: Some("queued".into()),
                                    assignee: Some(String::new()),
                                    ..super::TaskPatch::default()
                                },
                            );
                        }
                    }
                    // ONE spawn attempt per tick, refused or not: a tick that
                    // walked on to the next slice after a cap refusal would try
                    // every slice against a cap that just said no.
                    break;
                }
            }
        }

        // ---- 6. the facts `decide` needs, read off the record and the board ----
        let facts = PdFacts {
            now_ms: now,
            issue_open: obs.open,
            consent: obs.consent_read,
            planner_live: self.pd_planner_live(entry),
            planner: signal.planner,
            boarded: entry
                .plan
                .as_ref()
                .is_some_and(|p| p.slices.iter().all(|s| entry.slices.contains_key(s.id.as_str()))),
            row_removed: !entry.slices.is_empty()
                && entry.slices.values().any(|s| !board.iter().any(|t| t.id == s.task_id)),
            slices_settled: !entry.slices.is_empty()
                && entry.slices.values().all(|s| {
                    s.is_done()
                        || board
                            .iter()
                            .find(|t| t.id == s.task_id)
                            .is_some_and(|t| plandrive::slice_settled_by_board(&t.status))
                }),
            running_idle: !entry.slices.values().any(|s| {
                matches!(
                    s.state(),
                    plandrive::SliceState::Running | plandrive::SliceState::InReview
                )
            }) && !entry.plan.as_ref().is_some_and(|p| {
                p.slices.iter().any(|slice| {
                    entry
                        .slices
                        .get(slice.id.as_str())
                        .is_some_and(|run| Self::pd_slice_spawnable(slice, run, &board))
                })
            }),
        };

        // ---- 7. the one state decision ----
        if let Some(step) = plandrive::decide(entry, &facts, limits) {
            if entry.take(&step, now).is_ok() {
                out.advanced = true;
                out.reason = step.held.map(|h| h.as_str()).unwrap_or("");
                out.action = match step.to {
                    PlanDriveState::Held => plandrive::audit_action::HELD,
                    PlanDriveState::Complete => plandrive::audit_action::COMPLETE,
                    PlanDriveState::Cancelled => plandrive::audit_action::RECOVERED,
                    _ => "",
                };
                match (step.to, step.held) {
                    (PlanDriveState::Held, Some(why)) => entry.owe_notice(
                        &format!(
                            "[orrerix] plan drive #{issue}: HELD ({}) — {}",
                            why.as_str(),
                            why.notice_line()
                        ),
                        now,
                    ),
                    (PlanDriveState::PlanReview, _) => {
                        // §2(c): the ONE notice the declared window buys, and
                        // the only one it ever sends. The default window is
                        // zero, so this arc is unreachable unless somebody
                        // asked for it — which is what makes the notice a price
                        // that is paid rather than one that is imposed.
                        let text = format!(
                            "[orrerix] plan drive #{issue}: PLAN POSTED — {} slices, spawning in \
                             {} min unless cancel_plan_drive({issue}). The plan is at {}.",
                            entry.plan.as_ref().map(|p| p.slices.len()).unwrap_or(0),
                            entry.review_minutes,
                            entry.comment_url,
                        );
                        entry.owe_notice(&text, now);
                    }
                    (PlanDriveState::Complete, _) if entry.consent == Consent::Investigation => {
                        let text = format!(
                            "[orrerix] plan drive #{issue}: PLAN POSTED ({} — no workers). \
                             The plan is on the issue at {}; nothing was boarded and nothing \
                             was spawned.",
                            Consent::Investigation.as_str(),
                            entry.comment_url,
                        );
                        entry.owe_notice(&text, now);
                    }
                    (PlanDriveState::Complete, _) => {
                        let text = format!(
                            "[orrerix] plan drive #{issue}: COMPLETE — every slice row is \
                             settled. Nothing was merged by orrerix; the PRs are the humans'."
                        );
                        entry.owe_notice(&text, now);
                    }
                    (PlanDriveState::Cancelled, _) => entry.owe_notice(
                        &format!(
                            "[orrerix] plan drive #{issue}: CANCELLED — the issue is closed, so \
                             the drive stopped. Nothing was killed."
                        ),
                        now,
                    ),
                    _ => {}
                }
            }
        }
        out.hand_offs = hand_offs;
        let _ = plandrive::store_state(&dir, &state);
        Some(out)
    }

    /// Open one slice's worker pane (§2(b) step 6).
    ///
    /// **The claim comes first, and the order is the WIP gate.** `claim` is a
    /// guarded write — queued, unclaimed, deps met — and an agent-origin one is
    /// refused outright by a WIP cap. Spawning first and claiming second would
    /// open a pane the board then refused to account for; this way a full board
    /// costs nothing but a retry next tick.
    fn pd_spawn_slice(
        &self,
        group: &GroupId,
        issue: u64,
        slice: &plandoc::Slice,
        base: Option<&str>,
        task_id: &str,
        now: u64,
    ) -> Result<AgentEntry, String> {
        let _ = now;
        self.upsert_task(
            group,
            brand::AUDIT_ACTOR,
            Some(task_id),
            super::TaskPatch { claim: true, ..super::TaskPatch::default() },
        )?;
        let brief = self.pd_slice_brief(issue, slice, base);
        let agent = self.spawn_agent_bound(
            group,
            Role::Worker,
            Some(slice.block.clone()),
            &format!("{} {}", slice.id.as_str(), slice.title),
            &brief,
            true,
            Some(slice.branch.as_str().to_string()),
            base.map(str::to_string).filter(|b| !b.trim().is_empty()),
            None,
            None,
            None,
            Some(task_id.to_string()),
        )?;
        // The row now names the pane that holds it. A plain write, deliberately:
        // the guarded transition already happened above, and re-guarding it here
        // would refuse the very row this call just claimed.
        let _ = self.upsert_task(
            group,
            brand::AUDIT_ACTOR,
            Some(task_id),
            super::TaskPatch {
                assignee: Some(agent.id.clone()),
                session: agent.session_id.clone(),
                ..super::TaskPatch::default()
            },
        );
        Ok(agent)
    }

    /// **Hand one slice's PR to the REVIEW driver** (§2(b) step 6), and record
    /// that the pane now belongs to it.
    ///
    /// This is the join between the two drivers, and it is a plain Rust call
    /// rather than a tool hop: `drive_review_with` is the same function the
    /// `drive_review` tool reaches, called with this drive's own
    /// `on_behalf_of` so the `rd-started` row says which orchestrator the work
    /// is being done for.
    ///
    /// **From here the worker is rd-owned and this driver never sees it again.**
    /// `mcp.rs`'s `report` arm asks `rd_owner` FIRST, so the moment the review
    /// drive exists that arm wins — which is why the plan driver's own
    /// interception is asked for only when `rd_owner` answered `None`, and why
    /// the two sets cannot overlap.
    ///
    /// A refused hand-off is NOT a hold. The refusal is audited with its reason
    /// and the slice stays `running`, so the next tick tries again — a review
    /// driver that is off, a gate that is not configured, or a session that
    /// cannot be resolved are all things a human fixes while the worker's pane
    /// is still open and still useful.
    #[allow(clippy::too_many_arguments)]
    fn pd_hand_off(
        &self,
        group: &GroupId,
        runner: &dyn rddrive::RdRunner,
        issue: u64,
        slice: &str,
        pr: u64,
        session: &str,
        on_behalf_of: &str,
        now: u64,
    ) {
        let out = self.drive_review_with(group, runner, pr, session, false, 0, on_behalf_of, now);
        if let Some(reason) = out.get("refused").and_then(Value::as_str) {
            self.pd_audit(
                group,
                on_behalf_of,
                plandrive::audit_action::REFUSED,
                json!({ "issue": issue, "slice": slice, "pr": pr,
                        "reason": "review-drive-refused", "detail": reason }),
            );
            return;
        }
        let dir = self.group_dir(group);
        {
            let _state_guard = self.pd_state_lock.lock_safe();
            if let Ok(mut state) = plandrive::load_state(&dir) {
                if let Some(entry) = state.entry_mut(issue) {
                    if let Some(run) = entry.slices.get_mut(slice) {
                        run.advance(plandrive::SliceState::InReview, None);
                    }
                    entry.note_progress(now);
                    let _ = plandrive::store_state(&dir, &state);
                }
            }
        }
        self.pd_audit(
            group,
            on_behalf_of,
            plandrive::audit_action::REVIEW_DRIVEN,
            json!({ "issue": issue, "slice": slice, "pr": pr, "worker_session": session }),
        );
    }

    /// Which live drive's slice, if any, this agent is the worker of.
    ///
    /// [`pd_owner`](Self::pd_owner)'s two properties, unchanged: keyed on the
    /// agent id orrerix minted at spawn, and only a LIVE drive owns anybody.
    pub fn pd_slice_owner(&self, group: &GroupId, agent_id: &str) -> Option<(u64, String)> {
        let dir = self.group_dir(group);
        let _state_guard = self.pd_state_lock.lock_safe();
        let state = plandrive::load_state(&dir).ok()?;
        state.entries.iter().filter(|e| e.state().is_live()).find_map(|e| {
            e.slice_of_agent(agent_id).map(|id| (e.issue, id.to_string()))
        })
    }

    /// Is this drive's planner pane still alive?
    ///
    /// A drive with no pane recorded yet answers **true** — "no pane" is not "a
    /// dead pane", and `decide` guards the arc on `spawned_ms` anyway. Getting
    /// this backwards would park every drive `plan-missing` on the tick between
    /// the entry being written and the spawn being recorded.
    fn pd_planner_live(&self, entry: &PdEntry) -> bool {
        if entry.planner_agent.is_empty() {
            return true;
        }
        self.agent(&entry.planner_agent).is_some_and(|a| a.status != AgentStatus::Dead)
    }

    /// Deliver every owed notice, **outside the state lock**, and clear the ones
    /// that landed.
    ///
    /// A notice that fails to deliver is KEPT and counted, never dropped: the
    /// orchestrator's pane may be between agents, and a hold nobody was told
    /// about is a drive that stopped in silence — the one outcome this whole
    /// design exists to avoid.
    fn pd_flush_notices(&self, group: &GroupId, now: u64) -> usize {
        let dir = self.group_dir(group);
        let owed: Vec<(u64, String)> = {
            let _state_guard = self.pd_state_lock.lock_safe();
            match plandrive::load_state(&dir) {
                Ok(s) => s
                    .entries
                    .iter()
                    .filter_map(|e| e.owed.as_ref().map(|n| (e.issue, n.text.clone())))
                    .collect(),
                Err(_) => return 0,
            }
        };
        let mut delivered = 0usize;
        let mut outcomes: Vec<(u64, bool)> = Vec::new();
        for (issue, text) in owed {
            let ok = self.deliver_to_orchestrator(group, &text, brand::AUDIT_ACTOR).is_ok();
            if ok {
                delivered += 1;
            }
            outcomes.push((issue, ok));
        }
        if outcomes.is_empty() {
            return 0;
        }
        {
            let _state_guard = self.pd_state_lock.lock_safe();
            if let Ok(mut state) = plandrive::load_state(&dir) {
                for (issue, ok) in &outcomes {
                    if let Some(e) = state.entry_mut(*issue) {
                        if *ok {
                            e.notice_delivered();
                        } else {
                            e.notice_delivery_failed();
                        }
                    }
                }
                let _ = plandrive::store_state(&dir, &state);
            }
        }
        for (issue, ok) in outcomes {
            self.pd_audit(
                group,
                "",
                plandrive::audit_action::NOTICE,
                json!({ "issue": issue, "delivered": ok, "at": now }),
            );
        }
        delivered
    }

    /// The once-per-process restart reconcile (§2(f)).
    ///
    /// **Undeterminable is a hold, never "probably fine".** Three questions get
    /// answered here and nothing else: is the issue still open (a positive
    /// `closed` cancels the drive and owes a notice); is the planner's pane gone
    /// with no plan posted (that is `plan-missing`, which is what the pane going
    /// away means); and nothing about the slices, which are re-derived from the
    /// BOARD and from `gh` by the tick itself — the record remembers task ids
    /// and PR numbers, never readiness.
    ///
    /// **The once-only latch is set on a reconcile that actually READ the file**,
    /// not on one that merely attempted it. Latching first is the obvious
    /// spelling and it means a torn `plan_drives.json` at startup costs this
    /// group its reconcile for the life of the process — including after a human
    /// fixes the file.
    fn pd_reconcile_with(&self, group: &GroupId, runner: &dyn rddrive::RdRunner, now: u64) {
        if self.pd_reconciled.lock_safe().contains(group) {
            return;
        }
        let dir = self.group_dir(group);
        let live: Vec<u64> = {
            let _state_guard = self.pd_state_lock.lock_safe();
            let Ok(state) = plandrive::load_state(&dir) else { return };
            self.pd_reconciled.lock_safe().insert(group.clone());
            state.entries.iter().filter(|e| !e.state().is_terminal()).map(|e| e.issue).collect()
        };
        let mut audits: Vec<(u64, &'static str)> = Vec::new();
        for issue in live {
            // `issue_is_open` reads nothing else: one round trip per live entry
            // at startup, and the answer is only ever acted on when it is a
            // positive `closed`.
            let open = pd_issue_facts(runner, issue).open;
            let _state_guard = self.pd_state_lock.lock_safe();
            let Ok(mut state) = plandrive::load_state(&dir) else { continue };
            let Some(entry) = state.entry_mut(issue) else { continue };
            let mut changed = None;
            if open == Some(false) && entry.advance(PlanDriveState::Cancelled, None, now).is_ok() {
                entry.owe_notice(
                    &format!(
                        "[orrerix] plan drive #{issue}: CANCELLED — the issue was closed while \
                         orrerix was down. Nothing was killed."
                    ),
                    now,
                );
                changed = Some("issue-closed");
            } else if entry.state() == PlanDriveState::Planning
                && entry.planner_agent.is_empty()
                && entry.spawned_ms == 0
                && entry
                    .advance(PlanDriveState::Held, Some(PdHeldReason::PlanMissing), now)
                    .is_ok()
            {
                // A RESERVATION nobody completed: `drive_plan` wrote the entry,
                // and the process ended before the pane it was reserving for was
                // recorded on it. There is no planner and there never will be,
                // so this is `plan-missing` for the same reason a pane that went
                // away is — what is absent is the plan, and nothing is going to
                // produce it. Without this arm the entry sits in `planning`
                // with no stall clock to charge (`spawned_ms` is 0, which
                // `planner_age_ms` reads as "no pane yet") until the whole-drive
                // backstop fires hours later.
                // ONE LINE. This literal shipped with two 26-space runs — a
                // `\` continuation that collapsed in an authoring path, which
                // is the very class the same commit fixed two files over. A
                // line cannot collapse.
                //
                // The WORDING is corrected with it. "Nothing to hear from" was
                // false for half of this state: the arm fires whenever no
                // planner was RECORDED, which covers both a process that ended
                // before the spawn (no pane) and one that ended between the
                // spawn and the attach (a live pane orrerix cannot name). And
                // "resume it" was useless advice: a resume returns to
                // `planning` with no planner, and nothing in this build
                // re-spawns one, so the drive would sit exactly here again.
                entry.owe_notice(
                    &format!(
                        "[orrerix] plan drive #{issue}: HELD ({}) — the drive was reserved and no planner was ever recorded on it, so nothing will report to it. A pane may still have been opened before orrerix stopped; check your agent list. Cancel this drive and call drive_plan again.",
                        PdHeldReason::PlanMissing.as_str()
                    ),
                    now,
                );
                changed = Some("reservation-unspawned");
            } else if entry.state() == PlanDriveState::Planning
                && entry.plan.is_none()
                && entry.spawned_ms > 0
                && !self.pd_planner_live(entry)
                && entry
                    .advance(PlanDriveState::Held, Some(PdHeldReason::PlanMissing), now)
                    .is_ok()
            {
                entry.owe_notice(
                    &format!(
                        "[orrerix] plan drive #{issue}: HELD ({}) — {}",
                        PdHeldReason::PlanMissing.as_str(),
                        PdHeldReason::PlanMissing.notice_line()
                    ),
                    now,
                );
                changed = Some("planner-pane-gone");
            }
            if let Some(why) = changed {
                let _ = plandrive::store_state(&dir, &state);
                audits.push((issue, why));
            }
        }
        for (issue, why) in audits {
            self.pd_audit(
                group,
                "",
                plandrive::audit_action::RECOVERED,
                json!({ "issue": issue, "why": why }),
            );
        }
        self.pd_flush_notices(group, now);
    }
}

// ---------- the one `gh` read ----------

/// One `gh issue view`'s worth of facts.
struct PdIssueObs {
    /// Is the issue open? `None` = orrerix could not tell.
    open: Option<bool>,
    /// The labels, read. `None` = the read itself failed; `Some(None)` = read,
    /// and neither consent label is on the issue.
    consent_read: Option<Option<Consent>>,
    /// The issue title, for the `pd-started` row and for P3b's parent board row.
    title: String,
    /// The seam itself failed — `gh` missing, or a child killed at the command
    /// timeout. Not a `gh` refusal, and not a fact about the issue.
    runner_failed: bool,
}

impl PdIssueObs {
    /// Which consent this observation grants, if the read succeeded and a label
    /// is there. `drive_plan`'s own accessor, so the two `None`s cannot be
    /// confused at that call site.
    fn consent(&self) -> Option<Consent> {
        self.consent_read.flatten()
    }
}

#[derive(serde::Deserialize)]
struct RawIssueLabel {
    #[serde(default)]
    name: String,
}

#[derive(serde::Deserialize)]
struct RawIssueFacts {
    #[serde(default)]
    state: String,
    #[serde(default)]
    labels: Vec<RawIssueLabel>,
    #[serde(default)]
    title: String,
}

/// **The plan driver's ONE `gh` shape**, and the whole of what it reads.
///
/// One round trip answers both questions a tick asks — is the issue open, and
/// does it still carry consent — because they come off one `issue view`. A
/// second call for the labels would double this driver's whole tick budget to
/// learn something the first call already returned.
///
/// The `None`-is-not-a-fact discipline is [`rddrive::pr_is_open`]'s, unchanged:
/// a seam failure, a non-zero `gh`, an unparseable body and a `state` word this
/// build does not know all answer `None`, because `Some(false)` cancels a live
/// drive and unknown is never treated as safe.
fn pd_issue_facts(r: &dyn rddrive::RdRunner, issue: u64) -> PdIssueObs {
    let mut obs =
        PdIssueObs { open: None, consent_read: None, title: String::new(), runner_failed: false };
    let n = issue.to_string();
    let out = match r.gh(&["issue", "view", &n, "--json", "state,labels,title"]) {
        Ok(o) => o,
        Err(_) => {
            obs.runner_failed = true;
            return obs;
        }
    };
    if !out.ok() {
        // `gh` answering, not the seam failing — a rate limit, an auth failure,
        // or an issue that is genuinely gone. It is still an UNKNOWN.
        return obs;
    }
    let Ok(raw) = serde_json::from_str::<RawIssueFacts>(out.line()) else {
        return obs;
    };
    obs.open = match raw.state.trim().to_ascii_uppercase().as_str() {
        "OPEN" => Some(true),
        "CLOSED" => Some(false),
        // A `state` this build does not recognise leaves `open` at `None` rather
        // than guessing: `Some(false)` is a positive answer and cancels a drive.
        _ => None,
    };
    let names: Vec<String> = raw.labels.iter().map(|l| l.name.clone()).collect();
    obs.consent_read = Some(Consent::from_labels(&names));
    obs.title = pd_fact(&raw.title);
    obs
}
