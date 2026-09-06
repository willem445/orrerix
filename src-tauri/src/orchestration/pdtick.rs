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
/// per process, before this loop, and reads one issue per live entry without
/// consulting this figure — so the first wake after a restart spends
/// `live + min(live, PD_MAX_GH_PER_TICK)` round trips rather than four. That is
/// bounded by the number of live drives, which is bounded by how many issues an
/// orchestrator chose to drive, and it is a startup cost paid once; it is stated
/// here rather than fixed because a reconcile that serviced only four entries
/// would leave the rest unreconciled with nothing scheduled to finish the job.
/// `a_tick_services_at_most_four_drives` measures BOTH figures, so this
/// paragraph cannot go quietly false.
pub const PD_MAX_GH_PER_TICK: usize = 4;

/// What a driven planner's traffic told the drive, between the MCP arm that
/// consumed it and the tick that acts on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PdEvent {
    /// `report(done)`.
    PlannerDone,
    /// `report(blocked)`.
    PlannerBlocked,
}

/// One drive's pending planner signal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PdSignal {
    pub planner: PlannerSignal,
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
    /// `pub(super)` for `driver_enabled`'s reason and no wider: the parent
    /// module gates the tool listing and the instruction placeholder on it, and
    /// those have to read the same policy the tick does — a group whose tools
    /// all refuse `plan-driver-disabled` must not be told in its instructions
    /// that it has a plan driver.
    pub(super) fn plan_driver_enabled(&self, group: &GroupId) -> bool {
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
        }
    }

    /// This drive's pending planner signal, **without clearing it**.
    ///
    /// Cleared only once an arc has been taken, because a tick can decline to
    /// act for reasons that have nothing to do with the signal — a runner
    /// failure, an exhausted `gh` budget — and a signal consumed by a tick that
    /// then did nothing is a hand-back the drive never learns about.
    fn pd_signal(&self, group: &GroupId, issue: u64) -> PdSignal {
        self.pd_signals.lock_safe().get(&(group.clone(), issue)).copied().unwrap_or_default()
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
        {
            let _state_guard = self.pd_state_lock.lock_safe();
            match plandrive::load_state(&dir) {
                // **NOT `not-driven`.** A torn file cannot tell you an issue is
                // not driven; it can only tell you orrerix cannot say.
                Err(_) => return self.pd_refuse(group, issue, r::STATE_UNREADABLE),
                Ok(state) if state.is_driven(issue) => {
                    return self.pd_refuse(group, issue, r::ALREADY_DRIVEN)
                }
                Ok(_) => {}
            }
        }

        // §2(d): consent is the label, read here and re-read every tick.
        let facts = pd_issue_facts(runner, issue);
        match facts.open {
            Some(false) => return self.pd_refuse(group, issue, r::ISSUE_NOT_OPEN),
            None => return self.pd_refuse(group, issue, r::ISSUE_UNVERIFIABLE),
            Some(true) => {}
        }
        let consent = facts.consent().unwrap_or(Consent::Ready);

        // The spawn is the last thing before the write, and its failure is a
        // refusal rather than a half-created drive: an entry whose planner never
        // opened would sit in `planning` until `planner-stalled`, which is a
        // notice an hour later for something the caller could be told now.
        let brief = self.pd_planner_brief(issue, consent, base);
        let agent = match self.pd_spawn(group, &block, &brief) {
            Ok(a) => a,
            Err(e) => {
                self.pd_audit(
                    group,
                    on_behalf_of,
                    plandrive::audit_action::REFUSED,
                    json!({ "issue": issue, "reason": r::PLANNER_UNSPAWNABLE, "detail": e }),
                );
                return json!({ "refused": r::PLANNER_UNSPAWNABLE, "detail": e });
            }
        };

        let mut entry = PdEntry::new(
            issue,
            on_behalf_of,
            &block,
            consent,
            base.map(str::to_string).filter(|b| !b.trim().is_empty()),
            review_minutes.unwrap_or(limits.plan_review_minutes),
            now,
        );
        entry.planner_agent = agent.id.clone();
        entry.planner_session = agent.session_id.clone().unwrap_or_default();
        entry.spawned_ms = now;

        let stored = {
            let _state_guard = self.pd_state_lock.lock_safe();
            match plandrive::load_state(&dir) {
                Ok(mut state) => {
                    // Re-checked under the lock this time: `already-driven` was
                    // read above without one, and a second `drive_plan` landing
                    // between the two would otherwise append a second entry for
                    // one issue.
                    if state.is_driven(issue) {
                        Err(r::ALREADY_DRIVEN)
                    } else {
                        state.entries.retain(|e| e.issue != issue);
                        state.entries.push(entry);
                        plandrive::store_state(&dir, &state)
                            .map_err(|_| r::STATE_UNWRITABLE)
                    }
                }
                Err(_) => Err(r::STATE_UNREADABLE),
            }
        };
        if let Err(reason) = stored {
            return self.pd_refuse(group, issue, reason);
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
                        // P3b fills this; an absent map answers `null` rather
                        // than an invented id.
                        "task_id": e.slice_tasks.get(s.id.as_str()),
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
        for (i, issue) in live.iter().copied().enumerate() {
            if i >= PD_MAX_GH_PER_TICK {
                report.deferred.push(issue);
                continue;
            }
            let obs = pd_issue_facts(runner, issue);
            runner_failed |= obs.runner_failed;
            let signal = self.pd_signal(group, issue);

            let (on_behalf, action, reason) = {
                let _state_guard = self.pd_state_lock.lock_safe();
                let Ok(mut state) = plandrive::load_state(&dir) else { continue };
                let Some(entry) = state.entry_mut(issue) else { continue };
                let facts = PdFacts {
                    now_ms: now,
                    issue_open: obs.open,
                    consent: obs.consent_read,
                    planner_live: self.pd_planner_live(entry),
                    planner: signal.planner,
                };
                let Some(step) = plandrive::decide(entry, &facts, &limits) else { continue };
                let on_behalf = entry.on_behalf_of.clone();
                if entry.take(&step, now).is_err() {
                    continue;
                }
                // The notice a hold or a completion owes, composed here where
                // the reason is, and delivered below outside the lock.
                let action = match step.to {
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
                    (PlanDriveState::Complete, _) => {
                        // Composed BEFORE the call rather than inside its
                        // argument: owe_notice takes &mut self, and the text it
                        // renders reads comment_url off that same entry.
                        let text = format!(
                            "[orrerix] plan drive #{issue}: PLAN POSTED ({} — no workers). \
                             The plan is on the issue at {}; nothing was boarded and nothing \
                             was spawned.",
                            Consent::Investigation.as_str(),
                            entry.comment_url,
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
                let reason = step.held.map(|h| h.as_str()).unwrap_or("");
                let _ = plandrive::store_state(&dir, &state);
                (on_behalf, action, reason)
            };
            report.advanced.push(issue);
            self.pd_clear_signal(group, issue);
            if !action.is_empty() {
                self.pd_audit(group, &on_behalf, action, json!({ "issue": issue, "reason": reason }));
            }
        }

        report.notices = self.pd_flush_notices(group, now);
        if runner_failed {
            self.pd_defer(group, now.saturating_add(rddrive::RD_BACKOFF_MS));
        }
        report
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
    /// away means); and is a `boarding` entry from a build that had an executor
    /// (P3a re-parks it on `awaiting-p3b`, which the tick would do anyway — it
    /// is here so the audit says a reconcile touched it).
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
