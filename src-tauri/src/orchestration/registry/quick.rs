//! The quick task's human side (#3679): starting a run, reading where it
//! stands, and the four things a human can do to one — stop it, resume it,
//! force a hand-off, add a note — as an `impl OrchRegistry` block.
//!
//! `qdtick.rs` is the other half: the step that moves a run, the briefs and
//! the interception. Both files are read by `tests/quickdrive/guards.rs`'s
//! landing-verb scan; neither builds a `git` or a `gh` command line.
//!
//! Design note: `docs/design/quick-orchestration.md`.

use super::*;

use crate::orchestration::qdtick::{qd_fact, qd_text};
use crate::orchestration::quickdrive::{
    self, audit_action as act, QuickDriveFile, QuickDriveRecord, QuickLimits, QuickState,
};

/// One step's settings on the launcher form: which CLI and model run it, and
/// the human's own instructions for it.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
pub struct QuickStepConfig {
    /// Empty = the work step's CLI.
    #[serde(default)]
    pub cli: String,
    /// Empty = that CLI's default model for the step's class.
    #[serde(default)]
    pub model: String,
    /// The human's instructions for this step. Empty = the role's own
    /// instructions and nothing more, which is the sensible default the
    /// feature promises.
    #[serde(default)]
    pub instructions: String,
}

/// What the launcher asks for when it starts a quick run.
///
/// **This is the whole of a run's policy.** A quick group reads no workflow
/// file — there is no roster preview for the human to have agreed to — so
/// every bound a run obeys arrives here, typed by the human on their own
/// launcher, and is persisted in the run's record.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
pub struct QuickStartRequest {
    /// The repository the run works in.
    pub repo: String,
    /// The task, in the human's words.
    pub task: String,
    /// Whether to plan first.
    #[serde(default)]
    pub plan_step: bool,
    /// Whether to review the work.
    #[serde(default)]
    pub review_step: bool,
    /// The ref the worker's branch is cut from. Empty = the repository's
    /// default branch.
    #[serde(default)]
    pub base: String,
    /// At most this many reviews. Absent = the default; clamped `1..=3`.
    #[serde(default)]
    pub max_review_rounds: Option<u32>,
    /// The run's overall time bound in minutes. Absent = 240; clamped
    /// `5..=1440`.
    #[serde(default)]
    pub drive_timeout_minutes: Option<u32>,
    #[serde(default)]
    pub plan: QuickStepConfig,
    #[serde(default)]
    pub work: QuickStepConfig,
    #[serde(default)]
    pub review: QuickStepConfig,
    /// The group's live-agent cap. Absent = [`QUICK_MAX_AGENTS_DEFAULT`].
    #[serde(default)]
    pub max_agents: Option<u32>,
    /// The launcher's guardrail row, as every other launch carries it.
    #[serde(default)]
    pub auto_ops: bool,
    #[serde(default)]
    pub idle_kill_minutes: u32,
    #[serde(default)]
    pub max_spawns_per_hour: u32,
}

/// The live-agent cap a quick group gets when the launcher names none: one
/// pane per step. A run never needs more — exactly one pane holds the turn —
/// and a cap this low is what bounds a run that somehow opened more.
pub const QUICK_MAX_AGENTS_DEFAULT: u32 = 3;

/// Every action `orch_quick_control` accepts. Closed: an unknown word is
/// refused naming these, never defaulted to one of them.
pub const QUICK_ACTIONS: [&str; 5] = ["step", "stop", "resume", "handoff", "note"];

impl OrchRegistry {
    // ---------- start ----------

    /// **Start a quick run**: mint its group, record the run, and return. The
    /// first pane is opened by the step that follows
    /// ([`quick_step`](Self::quick_step), or the next poll wake).
    ///
    /// **Why the first pane is not opened here.** A spawn blocks until the
    /// frontend has opened and bound the pane, and the frontend places a new
    /// pane by the group its tab is bound to. That binding can only be made
    /// once this call has answered with the group id, so the launch is two
    /// calls on purpose: this one, the frontend's own bind, then the step. A
    /// step that never arrives costs nothing — the run is recorded with its
    /// first brief pending, and the poll tick delivers it.
    ///
    /// **What it creates, and what it does not.** A real group through
    /// `create_group_ex`, under the `creation` mutex like every other mint.
    /// No orchestrator pane and no root of any kind: the engine relays. The
    /// roster is the built-in worker, reviewer and planner on the CLIs the
    /// human picked, each carrying that step's instructions as an inline
    /// persona — exactly what a workflow file's `prompt:` is, append mode
    /// only, and never an `allow:`.
    ///
    /// **`advanced_orchestrator: false`, and that is the consent argument in
    /// code** (`lead_prepare`'s, unchanged): a quick group has no roster
    /// preview, so it never opens the repo's workflow file.
    pub fn quick_start(&self, req: QuickStartRequest) -> Result<Value, String> {
        self.quick_start_at(req, now_ms())
    }

    /// [`quick_start`](Self::quick_start) with the clock injected.
    #[doc(hidden)]
    pub fn quick_start_at(&self, req: QuickStartRequest, now: u64) -> Result<Value, String> {
        let task = qd_text(&req.task, quickdrive::QUICK_TASK_CAP);
        if task.trim().is_empty() {
            return Err("a quick task needs a description of what to do".into());
        }
        validate_group_repo(&req.repo)?;

        // The group's default CLI is the work step's: the worker is the one
        // pane every run has.
        let group_cli = match req.work.cli.trim() {
            "" => Guardrails::default().clamped().agent_cli,
            cli => cli.to_string(),
        };
        // Refused HERE, before anything is created, with the sentence that
        // knows why — the same containment check a spawn makes, asked at the
        // moment the human can still change the form.
        let steps = [
            (true, "work", &req.work, Role::Worker),
            (req.plan_step, "plan", &req.plan, Role::Planner),
            (req.review_step, "review", &req.review, Role::Reviewer),
        ];
        for (on, step, cfg, role) in steps {
            if !on {
                continue;
            }
            let cli = match cfg.cli.trim() {
                "" => group_cli.as_str(),
                cli => cli,
            };
            if !SUPPORTED_CLIS.contains(&cli) {
                return Err(format!(
                    "unsupported CLI {cli:?} for the {step} step — supported: {}",
                    SUPPORTED_CLIS.join(", ")
                ));
            }
            cli_can_host(cli, role)
                .map_err(|e| format!("the {step} step cannot run on {cli}: {e}"))?;
        }

        let mut blocks = workflow::default_roster(&[
            (Role::Worker, req.work.cli.as_str(), req.work.model.as_str()),
            (Role::Reviewer, req.review.cli.as_str(), req.review.model.as_str()),
            (Role::Planner, req.plan.cli.as_str(), req.plan.model.as_str()),
        ]);
        for b in &mut blocks {
            let text = match b.kind {
                Role::Worker => &req.work.instructions,
                Role::Reviewer => &req.review.instructions,
                Role::Planner => &req.plan.instructions,
                _ => continue,
            };
            let text = workflow::sanitize_persona(text);
            if !text.trim().is_empty() {
                b.prompt = Some(text);
            }
        }
        let rails = Guardrails {
            max_agents: req.max_agents.unwrap_or(QUICK_MAX_AGENTS_DEFAULT),
            agent_cli: group_cli,
            blocks,
            advanced_orchestrator: false,
            auto_ops: req.auto_ops,
            idle_kill_minutes: req.idle_kill_minutes,
            max_spawns_per_hour: req.max_spawns_per_hour,
            // The watchdog's notice goes to the group's root, and this group
            // has none — a stall here is the run's own bounds to report.
            watchdog_stall_minutes: 0,
            ..Guardrails::default()
        };
        let limits = QuickLimits::new(
            req.max_review_rounds.unwrap_or(quickdrive::QUICK_REVIEW_ROUNDS_DEFAULT),
            req.drive_timeout_minutes.unwrap_or(quickdrive::QUICK_DRIVE_TIMEOUT_DEFAULT_MIN),
        );
        // The base is recorded as typed. Empty stays empty: the worker's
        // worktree is then cut from whatever `git_worktree_add_sync` resolves
        // as the default, exactly as every other spawn's is, and the review
        // brief resolves a NAME for it when it is rendered.
        let rec = QuickDriveRecord::new(
            &task,
            req.plan_step,
            req.review_step,
            req.base.trim(),
            &limits,
            now,
        );

        // Held across the mint AND the record write, for
        // `create_orchestration_group`'s reason: a group id is chosen by
        // liveness, a quick group has no live agent until its first pane
        // opens, and `next_group_id` skips an id only once an unfinished run is
        // on disk for it. The two are one unit or a second launch on this repo
        // could be handed the same group.
        let _creation = self.creation.lock_safe();
        let group = self.create_group_ex(&req.repo, rails, Launch::Fresh)?;
        let dir = self.group_dir(&group.id);
        // A group dir is never removed, so this id may have hosted an earlier
        // run. Its documents would be read as this run's, and its open notice
        // would sit beside this run's own.
        if let Ok(Some(old)) = self.qd_load_run(&group.id) {
            self.qd_withdraw_notice(&group.id, &old);
        }
        let _ = fs::remove_dir_all(self.qd_doc_dir(&group.id));
        {
            let _state_guard = self.qd_state_lock.lock_safe();
            let file = QuickDriveFile { run: Some(rec.clone()), ..QuickDriveFile::default() };
            quickdrive::store_state(&dir, &file)?;
        }
        // THE MARKER, written after the record: a marker with no run would make
        // every report in this group answer "nothing is listening".
        // Best-effort like the `lead` marker it sits beside — a failed write
        // loses the interception, and the run then parks on its own bounds.
        let _ = fs::write(dir.join(crate::orchestration::QUICK_MARKER), b"1");
        {
            let mut mem = self.qd_mem.lock_safe();
            mem.known.insert(group.id.clone());
            mem.working.insert(group.id.clone());
            mem.signals.remove(&group.id);
        }
        self.qd_audit(&group.id, act::STARTED, json!({
            "plan_step": rec.plan_step, "review_step": rec.review_step,
            "max_review_rounds": rec.max_review_rounds,
            "drive_timeout_minutes": rec.drive_timeout_minutes,
            "base": rec.base, "task_chars": rec.task.chars().count(),
        }));
        crate::obs::breadcrumb("quick-start", &format!("group={}", group.id));
        Ok(json!({ "group_id": group.id, "state": rec.state().as_str() }))
    }

    /// **The human's one door onto a run**, behind `orch_quick_control`:
    /// dispatch `action` to the method that owns it.
    ///
    /// Refused for a group that is not a quick group before anything is read.
    /// Holding a valid `GroupId` is not membership — the id says the string is
    /// a safe path segment, not that the group is one of these — and the marker
    /// is what a quick group is.
    pub fn quick_control(
        &self,
        group: &GroupId,
        action: &str,
        text: Option<&str>,
    ) -> Result<Value, String> {
        if !self.is_quick_group(group) {
            return Err("this group is not a quick run".to_string());
        }
        match action {
            "step" => Ok(self.quick_step(group)),
            "stop" => self.quick_cancel(group),
            "resume" => self.quick_resume(group),
            "handoff" => self.quick_handoff(group),
            "note" => self.quick_note(group, text.unwrap_or_default()),
            other => Err(format!(
                "unknown quick-run action {other:?} — one of: {}",
                QUICK_ACTIONS.join(", ")
            )),
        }
    }

    /// Whether `group` holds a quick run that has not ended — working or
    /// parked.
    ///
    /// `next_group_id` asks this so a launch on the same repo is never handed
    /// a group whose run a human can still resume. A run whose panes have all
    /// closed leaves the group with no live agent, which is the only thing id
    /// selection used to read; without this a parked run's group would be
    /// claimed by the next launch and its record overwritten.
    pub(in crate::orchestration) fn qd_holds_group(&self, group: &GroupId) -> bool {
        self.is_quick_group(group)
            && self.qd_load_run(group).ok().flatten().is_some_and(|r| !r.state().is_terminal())
    }

    /// Step this group's run now — what the launcher calls once its tab is
    /// bound to the group, so the first pane opens at once instead of on the
    /// next poll wake. Idempotent: a run with nothing pending does nothing.
    ///
    /// **`busy: true` when another step holds the group** — the poll tick
    /// claimed it between `quick_start` and this call and is mid-spawn. The
    /// status beside it then shows no live pane, exactly as a pane that failed
    /// to open does, and the launcher must be able to tell the two apart: it
    /// stops a run whose first pane failed, and stopping one whose pane is
    /// opening leaves that pane on a cancelled run (#3681 review W4).
    pub fn quick_step(&self, group: &GroupId) -> Value {
        let out = self.qd_drive_group(group, now_ms());
        let mut status = self.quick_status(group);
        if out.busy {
            if let Some(o) = status.as_object_mut() {
                o.insert("busy".to_string(), json!(true));
            }
        }
        status
    }

    // ---------- status ----------

    /// Where this group's run stands. **A pure read** — the status chip polls
    /// it — so it parks nothing and writes nothing: a run an earlier process
    /// left working is parked by the tick's own start-up scan, and by any of
    /// the commands below.
    pub fn quick_status(&self, group: &GroupId) -> Value {
        match self.qd_load_run(group) {
            Err(e) => json!({ "group_id": group, "exists": false, "error": e }),
            Ok(None) => json!({ "group_id": group, "exists": false }),
            Ok(Some(r)) => self.qd_status_json(group, &r),
        }
    }

    /// **Every quick run that has not ended**, newest first — what the
    /// launcher's Quick task form lists so a run can be resumed or stopped
    /// without a pane (#3679).
    ///
    /// Resume and Stop live on a pane's menu, and a run can outlive every one
    /// of its panes: close them, or quit and reopen the app, and the run is
    /// still on disk, parked, with nothing on screen that leads to it. So the
    /// list is read off the records themselves rather than off anything a
    /// human can close or dismiss — which is why it is a list here and not a
    /// button on the run's needs-you item.
    ///
    /// **A pure read**, like [`quick_status`](Self::quick_status): it parks
    /// nothing, so a run an earlier process left working reads as working
    /// until the tick's start-up scan or a control verb reconciles it. Each
    /// row is that run's status with its group's repository beside it.
    pub fn quick_list(&self) -> Value {
        let mut rows: Vec<(u64, Value)> = Vec::new();
        for group in self.qd_quick_groups() {
            let Ok(Some(run)) = self.qd_load_run(&group) else { continue };
            if run.state().is_terminal() {
                continue;
            }
            let mut row = self.qd_status_json(&group, &run);
            let repo = self
                .group(&group)
                .map(|g| g.repo)
                .or_else(|| self.load_group_file(&group).map(|(repo, _)| repo))
                .unwrap_or_default();
            if let Some(o) = row.as_object_mut() {
                o.insert("repo".to_string(), json!(repo));
            }
            rows.push((run.started_ms, row));
        }
        rows.sort_by(|a, b| b.0.cmp(&a.0));
        Value::Array(rows.into_iter().map(|(_, row)| row).collect())
    }

    fn qd_status_json(&self, group: &GroupId, r: &QuickDriveRecord) -> Value {
        let pane = |side: quickdrive::QuickSide| {
            let p = r.pane(side);
            let live = !p.agent.is_empty()
                && self.agent(&p.agent).is_some_and(|a| a.status != AgentStatus::Dead);
            json!({ "agent": p.agent, "live": live })
        };
        let turn = r.state().turn().map(|side| {
            json!({ "side": side.as_str(), "agent": r.pane(side).agent })
        });
        let can_handoff = !r.brief_pending
            && match r.state() {
                QuickState::WorkWait | QuickState::FixWait => r.review_step,
                QuickState::ReviewWait => true,
                _ => false,
            };
        let plan = self.qd_plan_path(group);
        json!({
            "group_id": group,
            "exists": true,
            "state": r.state().as_str(),
            "held_reason": r.held_reason.map(|h| h.as_str()),
            "held_line": r.held_reason.map(|h| h.notice_line()),
            "held_note": r.held_note,
            "round": r.round(),
            "max_review_rounds": r.max_review_rounds,
            "review_rounds": r.review_rounds,
            "reviews_total": r.reviews_total,
            "plan_step": r.plan_step,
            "review_step": r.review_step,
            "task": r.task,
            "base": r.base,
            "branch": r.worker_branch,
            "cwd": r.worker_cwd,
            "pr": r.pr,
            "started_ms": r.started_ms,
            "state_since_ms": r.state_since_ms,
            "drive_timeout_minutes": r.drive_timeout_minutes,
            "brief_pending": r.brief_pending,
            "turn": turn,
            "can_handoff": can_handoff,
            "notes_pending": r.notes.len(),
            "plan_path": plan.is_file().then(|| plan.to_string_lossy().to_string()),
            "panes": {
                "planner": pane(quickdrive::QuickSide::Planner),
                "worker": pane(quickdrive::QuickSide::Worker),
                "reviewer": pane(quickdrive::QuickSide::Reviewer),
            },
        })
    }

    // ---------- stop ----------

    /// **Stop a run.** Works in any state that has not already ended.
    ///
    /// **It kills nothing**, and that is the contract rather than an omission:
    /// the panes stay open for the human to read, keep typing into, or close.
    /// Stop releases OWNERSHIP — from here every pane's `report` is answered
    /// "this run has ended" and moves nothing.
    pub fn quick_cancel(&self, group: &GroupId) -> Result<Value, String> {
        self.quick_cancel_at(group, now_ms())
    }

    #[doc(hidden)]
    pub fn quick_cancel_at(&self, group: &GroupId, now: u64) -> Result<Value, String> {
        self.qd_mem.lock_safe().known.insert(group.clone());
        let (rec, from) = self.qd_edit_run(group, |r| {
            if r.state().is_terminal() {
                return Err("this quick run has already ended".to_string());
            }
            let from = r.state();
            r.advance(QuickState::Cancelled, None, now).map_err(|e| e.to_string())?;
            Ok((true, (r.clone(), from)))
        })?;
        {
            let mut mem = self.qd_mem.lock_safe();
            mem.working.remove(group);
            mem.signals.remove(group);
        }
        // The human is the one who asked, so the run owes them no notice — and
        // the one it had open is no longer true.
        self.qd_withdraw_notice(group, &rec);
        let left: Vec<String> = quickdrive::QuickSide::ALL
            .into_iter()
            .map(|s| rec.pane(s).agent.clone())
            .filter(|a| !a.is_empty())
            .collect();
        self.qd_audit(group, act::CANCELLED, json!({
            "from": from.as_str(), "panes_left_running": left, "killed": Vec::<String>::new(),
        }));
        Ok(self.quick_status(group))
    }

    // ---------- resume ----------

    /// **Resume a parked run**, back into the state its hold came from, and
    /// hand the pane that holds the turn its brief again — re-opening that
    /// pane from its recorded session if it is gone.
    ///
    /// A resume is a fresh grant of the run's time bound, and of its review
    /// rounds when the hold was `review-limit`: see
    /// [`QuickDriveRecord::resume`].
    pub fn quick_resume(&self, group: &GroupId) -> Result<Value, String> {
        self.quick_resume_at(group, now_ms())
    }

    #[doc(hidden)]
    pub fn quick_resume_at(&self, group: &GroupId, now: u64) -> Result<Value, String> {
        // A run this process did not start is parked first, so "resume" means
        // one thing whether or not the tick has got to it yet.
        self.qd_reconcile(group, now);
        self.qd_ensure_group_loaded(group)?;
        let (rec, from, to) = self.qd_edit_run(group, |r| {
            if r.state().is_terminal() {
                return Err("this quick run has already ended".to_string());
            }
            if !r.state().is_parked() {
                return Err("this quick run is not held, so there is nothing to resume".to_string());
            }
            let from = r.held_reason;
            let to = r.resume(now).map_err(|e| e.to_string())?;
            Ok((true, (r.clone(), from, to)))
        })?;
        // The hold's notice is answered by the resume itself.
        self.qd_withdraw_notice(group, &rec);
        {
            let mut mem = self.qd_mem.lock_safe();
            mem.working.insert(group.clone());
            // Whatever was said before the hold was said to a run that parked.
            mem.signals.remove(group);
        }
        self.qd_audit(group, act::RESUMED, json!({
            "from": from.map(|f| f.as_str()), "to": to.as_str(),
        }));
        self.qd_drive_group(group, now);
        Ok(self.quick_status(group))
    }

    /// Put a quick group back into this process's group table from its own
    /// `group.json`, if it is not there.
    ///
    /// After a restart no group is in memory until something launches or
    /// resumes it, and a quick group has no orchestrator session for the
    /// session browser to resume — so its own Resume has to do it.
    ///
    /// **By id, never by `next_group_id`.** Every other reattach goes through
    /// `create_group_ex`, which picks the first free candidate for the repo
    /// and rewrites that group's `group.json` with the guardrails it was
    /// handed; pointed at a quick group, it could resolve a DIFFERENT dormant
    /// group and overwrite its roster. This reads one named file and inserts
    /// what it says, under `creation`, and only for a group that carries the
    /// quick marker — it is not a second general-purpose reattach.
    fn qd_ensure_group_loaded(&self, group: &GroupId) -> Result<(), String> {
        if self.group(group).is_some() {
            return Ok(());
        }
        if !self.is_quick_group(group) {
            return Err("this group is not a quick run".to_string());
        }
        let _creation = self.creation.lock_safe();
        if self.group(group).is_some() {
            return Ok(());
        }
        let (repo, guardrails) = self
            .load_group_file(group)
            .ok_or("this quick run's group record is missing, so it cannot be resumed")?;
        validate_group_repo(&repo)?;
        let info = GroupInfo { id: group.clone(), repo, guardrails: guardrails.clamped() };
        self.groups.lock_safe().insert(group.clone(), info.clone());
        // Deliberately NOT declared as a root (#1042). `create_group_ex`
        // declares a group's checkout from a value its caller just handed it;
        // this one comes off a file on disk, and a second admit site fed from
        // disk is a way to mint a root that nobody at a keyboard named. The
        // reattach needs the group in the table, not a root: a pane that later
        // browses the checkout is refused or admitted by the rule every other
        // pane is, and `tests/rootreg.rs` keeps this file off its list.
        self.audit(group, brand::AUDIT_ACTOR, "group-resume", json!({
            "repo": info.repo, "max_agents": info.guardrails.max_agents, "by": "quick-resume",
        }));
        Ok(())
    }

    // ---------- force a hand-off ----------

    /// **Hand the turn to the other side now**, without waiting for the pane
    /// that holds it to report: the worker's work goes to the reviewer, or the
    /// reviewer's turn goes back to the worker. It spends no review round —
    /// the human is the one moving the run, not a verdict.
    pub fn quick_handoff(&self, group: &GroupId) -> Result<Value, String> {
        self.quick_handoff_at(group, now_ms())
    }

    #[doc(hidden)]
    pub fn quick_handoff_at(&self, group: &GroupId, now: u64) -> Result<Value, String> {
        self.qd_reconcile(group, now);
        let (from, to) = self.qd_edit_run(group, |r| {
            if r.state().is_terminal() {
                return Err("this quick run has already ended".to_string());
            }
            if r.state().is_parked() {
                return Err("this quick run is held — resume it first".to_string());
            }
            if r.brief_pending {
                return Err("the last hand-off is still being delivered — try again in a moment"
                    .to_string());
            }
            let from = r.state();
            let to = r.force_handoff(now).map_err(|_| match from {
                QuickState::PlanWait => {
                    "the planner holds the turn, and there is no other side to hand a plan to"
                        .to_string()
                }
                _ => "this run has no review step, so there is nobody to hand the work to"
                    .to_string(),
            })?;
            Ok((true, (from, to)))
        })?;
        // The pane that held the turn may have reported a moment ago; that
        // report was about a turn the human has just ended.
        self.qd_mem.lock_safe().signals.remove(group);
        self.qd_audit(group, act::FORCED, json!({ "from": from.as_str(), "to": to.as_str() }));
        self.qd_drive_group(group, now);
        Ok(self.quick_status(group))
    }

    // ---------- add a note ----------

    /// **Add a note to a run.** It is typed into the pane holding the turn
    /// right away, and carried again in the next brief — so the side that is
    /// working reads it now and the side that takes over reads it too.
    pub fn quick_note(&self, group: &GroupId, text: &str) -> Result<Value, String> {
        let note: String = qd_fact(text).chars().take(quickdrive::QUICK_NOTE_CAP).collect();
        if note.trim().is_empty() {
            return Err("a note needs some text".into());
        }
        let rec = self.qd_edit_run(group, |r| {
            if r.state().is_terminal() {
                return Err("this quick run has already ended".to_string());
            }
            r.add_note(note.trim());
            Ok((true, r.clone()))
        })?;
        let turn = rec
            .state()
            .turn()
            .map(|s| rec.pane(s).agent.clone())
            .filter(|a| !a.is_empty() && !rec.brief_pending);
        let typed = turn.is_some_and(|agent| {
            let line = format!(
                "{} note from the human on this quick run: {}",
                brand::NOTICE_MARKER,
                note.trim()
            );
            self.deliver_prompt(&agent, &line, brand::AUDIT_ACTOR, Delivery::MidSession).is_ok()
        });
        self.qd_audit(group, act::NOTE, json!({
            "typed": typed, "pending": rec.notes.len(), "chars": note.trim().chars().count(),
        }));
        Ok(json!({ "typed": typed, "pending": rec.notes.len() }))
    }
}
