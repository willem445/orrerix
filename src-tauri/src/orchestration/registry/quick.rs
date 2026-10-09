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
    /// The task, in the human's words — a STEPS run's, which the engine relays
    /// to the first pane. A described run takes none here (#3723): its agent
    /// opens idle and is given the task in its own pane, so a task sent with
    /// `mode: "describe"` is refused rather than silently dropped.
    #[serde(default)]
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
    /// How the run is driven (#3679): `"steps"` — orrerix relays between a
    /// planner, a worker and a reviewer itself — or `"describe"`, where one
    /// agent opens idle, is told the task in its pane and opens its own
    /// helpers. Empty is `steps`, so
    /// a caller written before the second mode existed means what it meant.
    /// Any other word is refused rather than read as one of the two.
    #[serde(default)]
    pub mode: String,
    /// The CLI and model of a described run's one agent. Read only in
    /// `describe` mode; its `instructions` are ignored, because that agent's
    /// role template is its contract and the human says the rest in its pane.
    #[serde(default)]
    pub root: QuickStepConfig,
}

/// The live-agent cap a quick group gets when the launcher names none: one
/// pane per kind of delegate. A steps run never needs more, since exactly one
/// of its panes holds the turn at a time; in a described run it is how many
/// helpers the root may have open at once, and the root itself is a fixture
/// and is not counted. A cap this low is what bounds a run that asked for
/// more.
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
        let described = match req.mode.trim() {
            "" | "steps" => false,
            "describe" => true,
            other => {
                return Err(format!(
                    "unknown quick-task mode {other:?} — one of: steps, describe"
                ))
            }
        };
        let task = qd_text(&req.task, quickdrive::QUICK_TASK_CAP);
        // #3723: the two modes take the task in different places. A steps run
        // has no agent to tell — the engine relays the task into the first
        // pane — so it is required here. A described run's agent opens idle
        // and the human tells it the task in its pane; a task sent with that
        // mode is refused, because accepting it would mean either typing it
        // into the pane (the thing the mode exists not to do) or dropping it
        // without a word.
        if described && !task.trim().is_empty() {
            return Err("a described quick task takes no task here — its agent opens idle, and \
                        you give it the task in its pane. Send an empty task, or use the steps \
                        mode to have orrerix relay one."
                .into());
        }
        if !described && task.trim().is_empty() {
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
        // In a described run the three are not steps the human switched on:
        // they are the helpers the agent MAY open, and it may open any of
        // them, so each one's CLI has to be able to host its class.
        let steps = [
            (true, "work", &req.work, Role::Worker),
            (described || req.plan_step, "plan", &req.plan, Role::Planner),
            (described || req.review_step, "review", &req.review, Role::Reviewer),
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

        let root_cli = match req.root.cli.trim() {
            "" => group_cli.clone(),
            cli => cli.to_string(),
        };
        if described {
            if !SUPPORTED_CLIS.contains(&root_cli.as_str()) {
                return Err(format!(
                    "unsupported CLI {root_cli:?} for the task's agent — supported: {}",
                    SUPPORTED_CLIS.join(", ")
                ));
            }
            // Asked like the three above, though today every CLI can host this
            // class: it is not clamped, so no deny tier rules a CLI out. The
            // check is here so that a class that gains a tier later is refused
            // at the form rather than at the spawn.
            cli_can_host(&root_cli, Role::Quick)
                .map_err(|e| format!("the task's agent cannot run on {root_cli}: {e}"))?;
        }
        let mut blocks = workflow::default_roster(&[
            (Role::Worker, req.work.cli.as_str(), req.work.model.as_str()),
            (Role::Reviewer, req.review.cli.as_str(), req.review.model.as_str()),
            (Role::Planner, req.plan.cli.as_str(), req.plan.model.as_str()),
        ]);
        if described {
            // The root's own block. This is the ONE place a quick block is
            // minted: no workflow file can declare the kind and `spawn_agent`
            // cannot name it, so a roster carries one only because a human
            // started a described run from the launcher.
            blocks.extend(workflow::default_roster(&[(
                Role::Quick,
                root_cli.as_str(),
                req.root.model.as_str(),
            )]));
        }
        for b in &mut blocks {
            // A described run takes no per-step instructions: the task is the
            // human's whole input, and each helper runs on its role's own.
            if described {
                break;
            }
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
            // The watchdog's notice is addressed to an ORCHESTRATOR, and no
            // quick group has one — a steps run has no root at all, and a
            // described run's root is not an orchestrator. A stall here is
            // the run's own bounds to report, in either mode.
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
        let rec = if described {
            let mut rec = QuickDriveRecord::new_described(req.base.trim(), &limits, now);
            rec.root_cli = root_cli.clone();
            rec.root_model = req.root.model.trim().to_string();
            rec
        } else {
            QuickDriveRecord::new(
                &task,
                req.plan_step,
                req.review_step,
                req.base.trim(),
                &limits,
                now,
            )
        };

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
        // Known to THIS process before the marker makes the group findable:
        // the start-up scan reads the orchestration root for marked groups and
        // treats one it does not know as an earlier process's — parking a
        // working run, and (#3723) ending an idle one. A run this call has
        // just written is neither.
        {
            let mut mem = self.qd_mem.lock_safe();
            mem.known.insert(group.id.clone());
            // On the tick's candidate list in both modes: a steps run is
            // working, and a described run is owed its root's pane. The step
            // that opens that pane takes an idle run off the list again.
            mem.working.insert(group.id.clone());
            mem.signals.remove(&group.id);
        }
        // THE MARKER, written after the record: a marker with no run would make
        // every report in this group answer "nothing is listening".
        // Best-effort like the `lead` marker it sits beside — a failed write
        // loses the interception, and the run then parks on its own bounds.
        let _ = fs::write(dir.join(crate::orchestration::QUICK_MARKER), b"1");
        self.qd_audit(&group.id, act::STARTED, json!({
            "plan_step": rec.plan_step, "review_step": rec.review_step,
            "described": rec.described,
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
            // #3723: an idle described run is not listed. Every task it was
            // given has finished, or it was never given one, so there is
            // nothing here to resume or stop — and a pane opened and left
            // alone must not sit on this list for ever.
            if !run.state().is_in_progress() {
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
        let last_note = if r.described { r.worker_note.as_str() } else { "" };
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
            "described": r.described,
            // #3723: how many tasks a described run's root has begun, and what
            // it said when the last one finished — what an idle chip's tooltip
            // has to go on, since the task itself is not on the record.
            "task_seq": r.task_seq,
            "last_note": last_note,
            "panes": {
                "planner": pane(quickdrive::QuickSide::Planner),
                "worker": pane(quickdrive::QuickSide::Worker),
                "reviewer": pane(quickdrive::QuickSide::Reviewer),
                "root": pane(quickdrive::QuickSide::Root),
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
        // A steps run's panes each finished a turn or were waiting for one, so
        // a stop leaves nothing in motion. A described run's root is still
        // deciding what to open next, and it is not told anything by the
        // record changing — so it is told here, once, best-effort. Nothing is
        // killed: the panes are the human's to read or close.
        if rec.described {
            let root = rec.pane(quickdrive::QuickSide::Root).agent.clone();
            if self.agent(&root).is_some_and(|a| a.status != AgentStatus::Dead) {
                let _ = self.deliver_prompt(
                    &root,
                    &format!(
                        "{} the human stopped this quick run. Stop here: open no further \
                         helpers and send no further work. Your report is no longer needed.",
                        brand::NOTICE_MARKER
                    ),
                    brand::AUDIT_ACTOR,
                    Delivery::MidSession,
                );
            }
        }
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
        // A described run's roster had a quick block, and it is not in what
        // was just read: `group.json` is read back through the workflow
        // vocabulary, which has no word for that kind — deliberately, since
        // that absence is what stops a file declaring one. So the block is
        // rebuilt here from the run's own record, for this group and for a
        // run that says it is described, and by nothing else.
        let mut guardrails = guardrails;
        if let Ok(Some(run)) = self.qd_load_run(group) {
            if run.described && guardrails.block(Role::Quick.as_str()).is_none() {
                guardrails.blocks.extend(workflow::default_roster(&[(
                    Role::Quick,
                    run.root_cli.as_str(),
                    run.root_model.as_str(),
                )]));
            }
        }
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

    /// A live root pane in `group`, if there is one — the backstop behind "a
    /// quick run has exactly one root" (#3679). Read by the hand-over before
    /// it opens a root, so a second cannot be opened beside a live first.
    pub(in crate::orchestration) fn qd_live_root(&self, group: &GroupId) -> Option<String> {
        self.agents
            .lock_safe()
            .values()
            .find(|a| &a.group == group && a.status != AgentStatus::Dead && a.role.is_root())
            .map(|a| a.id.clone())
    }

    /// Why a described run's root may not open another helper right now, or
    /// `None` when it may (#3712 review).
    ///
    /// The root is unclamped and is never reaped, and the argument for that is
    /// that its run's bounds bound it. A bound that only changed a record
    /// would bind nothing: `spawn_agent` and `fork_session` read no run
    /// state, so a root whose run was held at its time bound — or had been
    /// stopped — could go on opening helpers for as long as it liked. So both
    /// ask here first. A run that is held refuses until the human resumes it;
    /// a run the human STOPPED refuses for good.
    ///
    /// **A finished task refuses nothing** (#3723). A described run's root
    /// reporting `done` returns the run to idle, where this answers `None`:
    /// the pane takes another task, and the helper it opens for that task is
    /// what begins it ([`qd_root_acted`](Self::qd_root_acted)). The two
    /// refusals left are the two a human caused or has been told about — a
    /// hold, which they are asked to resume, and a stop, which they chose.
    ///
    /// `send_prompt` and `get_output` are not gated: a held root may still
    /// read what its helpers have done and tell one to stop, and the human who
    /// resumes the run finds the same panes it left.
    pub(in crate::orchestration) fn qd_root_spawn_refusal(&self, group: &GroupId) -> Option<String> {
        let run = self.qd_load_run(group).ok().flatten()?;
        if !run.described {
            return None;
        }
        match run.state() {
            QuickState::Held => Some(format!(
                "this quick run is held ({}) — open no further helpers until the human \
                 resumes it. They have been told why.",
                run.held_reason.map(|h| h.as_str()).unwrap_or("held")
            )),
            s if s.is_terminal() => Some(
                "this quick run has ended — open nothing further. Its panes are the human's to \
                 read or close."
                    .to_string(),
            ),
            _ => None,
        }
    }

    /// **A described run's root just called `tool`, and it succeeded** — if
    /// that put a helper to work while the run was idle, a task has begun
    /// (#3723). Answers the tool's own answer, with the task's limits added
    /// when this call is what began one.
    ///
    /// # What begins a task, and why it is these three
    ///
    /// `spawn_agent`, `fork_session` and `send_prompt`: the three calls that
    /// hand a helper something to do. The record cannot see the human type,
    /// and no hook that could exists on every CLI that can host a root; what
    /// it can see, on all of them, is the root starting to delegate — which is
    /// also the only work a root does, since it may not edit. A spawn alone
    /// would not be enough: nothing is closed when a task ends, so a second
    /// task is often begun by prompting a helper that is still open, and a run
    /// that only noticed spawns would leave that task with no clock and its
    /// `done` with nothing to end.
    ///
    /// The reads and the housekeeping — `list_agents`, `get_output`,
    /// `group_usage`, `kill_agent`, `rename_agent`, `focus_agent` — begin
    /// nothing. A root may look around, and clear up after the last task,
    /// without a clock starting.
    ///
    /// # Why the clock starts AFTER the call
    ///
    /// The caller is the dispatch funnel, once the tool has answered `Ok`. A
    /// spawn blocks until its pane has opened, which can take most of a
    /// minute; a task is not charged for that, for the reason a state's clock
    /// starts at delivery rather than at the arc. And a call that was REFUSED
    /// — by the cap, by the class rule — begins nothing.
    ///
    /// Only `root-idle` begins a task. A working run is already on one, and a
    /// held or stopped run is not moved by its root acting — `send_prompt` is
    /// deliberately left open to a held root so it can tell a helper to stop.
    ///
    /// **And only the run's own root.** `agent_id` is the caller's, off its
    /// token; a quick root the record does not name as its current one — a
    /// second root in the group, which `qd_owner` already answers as a
    /// stranger — begins nothing. Its helper is opened, since the spawn rule
    /// is the class's and not the run's, but the run's clock is not its to
    /// start.
    pub(in crate::orchestration) fn qd_root_acted(
        &self,
        group: &GroupId,
        agent_id: &str,
        tool: &str,
        answer: String,
    ) -> String {
        if !matches!(tool, "spawn_agent" | "fork_session" | "send_prompt") {
            return answer;
        }
        if !self.is_quick_group(group) {
            return answer;
        }
        let now = now_ms();
        let begun = self.qd_edit_run(group, |r| {
            let own_root = r.pane(quickdrive::QuickSide::Root).standing(agent_id) == Some(true);
            if !r.described || !r.state().is_idle() || !own_root {
                return Ok((false, None));
            }
            match r.begin_task(r.started_ms.min(now)) {
                Ok(seq) => Ok((true, Some((seq, r.clone())))),
                Err(_) => Ok((false, None)),
            }
        });
        let Ok(Some((seq, rec))) = begun else { return answer };
        {
            let mut mem = self.qd_mem.lock_safe();
            mem.working.insert(group.clone());
            // Whatever was said while idle was said about no task.
            mem.signals.remove(group);
        }
        self.qd_audit(group, act::TASK_BEGUN, json!({
            "task": seq, "by": tool,
            "drive_timeout_minutes": rec.drive_timeout_minutes,
            "max_review_rounds": rec.max_review_rounds,
        }));
        self.qd_emit_changed(group);
        // The limits ride on the answer of the call that began the task: an
        // idle root is handed no first message to carry them, and this is the
        // moment they start to apply.
        format!(
            "{answer}\n\nThis is the start of a task in this quick run (task {seq}). Its limits: \
             {} minutes from now, after which the run is held for the human; and at most {} \
             review rounds, after which you stop and report what is still open. When the task \
             is finished, report(outcome=done).",
            rec.drive_timeout_minutes, rec.max_review_rounds,
        )
    }

    /// **A quick root's pane has gone** — and if no task was in progress, the
    /// run goes with it (#3723).
    ///
    /// A root that dies with a task in progress is the tick's to find: the run
    /// parks on `root-gone`, the human is told, and Resume re-opens the
    /// session. An IDLE root has nothing to park. Every task it was given has
    /// finished, or it was never given one, so a record left behind would be a
    /// run that needs stopping for no reason — and would hold its group id for
    /// a pane that is not coming back. So closing an idle root is the whole of
    /// ending it: the record goes to `cancelled`, with no notice, because
    /// nothing was interrupted.
    ///
    /// Only the run's CURRENT root ends it. A superseded pane closing says
    /// nothing about the one that replaced it.
    pub(in crate::orchestration) fn qd_root_exited(&self, group: &GroupId, agent_id: &str) {
        if !self.is_quick_group(group) {
            return;
        }
        let now = now_ms();
        let closed = self.qd_edit_run(group, |r| {
            let current = r.pane(quickdrive::QuickSide::Root).standing(agent_id) == Some(true);
            if !r.described || !r.state().is_idle() || !current {
                return Ok((false, false));
            }
            let ended = r.advance(QuickState::Cancelled, None, now).is_ok();
            Ok((ended, ended))
        });
        if let Ok(true) = closed {
            {
                let mut mem = self.qd_mem.lock_safe();
                mem.working.remove(group);
                mem.signals.remove(group);
            }
            self.qd_audit(group, act::CLOSED, json!({
                "agent": agent_id,
                "why": "its root's pane closed with no task in progress",
            }));
            self.qd_emit_changed(group);
        }
    }

    /// The branch a described run's helpers are cut from when the root names
    /// none — the one the human set on the form (#3723).
    ///
    /// The root used to be told this in its first message and had to pass it
    /// on every spawn. It is handed no first message now, so the default is
    /// applied where the worktree is cut instead: a spawn in a described run's
    /// group that names no `base` takes the run's. `None` for every other
    /// group, for a steps run (which passes its base itself), and for a run
    /// whose human left the field empty — the repository's default branch.
    pub(in crate::orchestration) fn qd_helper_base(&self, group: &GroupId) -> Option<String> {
        if !self.is_quick_group(group) {
            return None;
        }
        let run = self.qd_load_run(group).ok().flatten()?;
        (run.described && !run.base.trim().is_empty()).then(|| run.base.trim().to_string())
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
            if r.described {
                return Err("a described run has no hand-off — its agent decides who works and \
                            who reviews. Tell it in its pane."
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
            // #3723: an idle run has no task to attach a note to and no brief
            // to carry it in. The human is one keystroke from the agent.
            if r.state().is_idle() {
                return Err("no task is in progress in this quick run — tell its agent in its \
                            pane instead"
                    .to_string());
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
