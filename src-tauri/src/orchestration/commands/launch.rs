//! Launch-time commands: the per-CLI knob tables, `create_orchestration`,
//! `promote_to_orchestrator`, the launcher's workflow list/preview, and
//! pause/resume of a group. Moved from `orchestration/mod.rs` by #3498 P2;
//! see `commands/mod.rs`.

use super::*;

/// The flags the single-pane launcher appends to a `program` command when its
/// "autopilot / allow all" toggle is on (#101). Empty for CLIs with no known
/// unattended surface. Shares `single_pane_autopilot_flags` with the group
/// spawn path so the two can't drift. Stateless (no registry needed).
#[tauri::command]
pub fn agent_autopilot_flags(program: String) -> String {
    single_pane_autopilot_flags(&program)
}

/// The model knobs (#687) loomux can actually set on `cli`, straight off its
/// [`CliCaps`] row: the `effort`/`context` value sets, and — always — the note
/// that says *why* a set is what it is.
///
/// The launcher's per-role selector reads this so a knob loomux cannot deliver
/// renders **disabled with the vendor's reason** rather than silently doing
/// nothing. That is the whole point of shipping the note alongside the values:
/// a hidden control reads as "loomux forgot", while a disabled one that says
/// "copilot reads effortLevel from ~/.copilot/settings.json" states the fact.
///
/// Stateless (no registry), and deliberately not a second copy of the vendor
/// facts — it reports [`CLI_CAPS`] verbatim, so the launcher, the workflow
/// parser and the spawn path can never disagree about what a CLI supports.
#[tauri::command]
pub fn agent_cli_knobs(cli: String) -> Value {
    cli_knobs_json(&cli)
}

/// The pure body of [`agent_cli_knobs`], so the wire shape is assertable
/// without a tauri runtime. An unknown CLI is `known: false` with both sets
/// empty — the launcher renders every knob disabled, which is the honest
/// answer for a CLI loomux has never evaluated.
pub fn cli_knobs_json(cli: &str) -> Value {
    let caps = cli_caps(cli);
    json!({
        "cli": cli,
        "known": caps.is_some(),
        "effort": {
            "values": caps.map(|c| c.effort_levels).unwrap_or(&[]),
            "note": caps.map(|c| c.effort_note).unwrap_or(""),
        },
        "context": {
            "values": caps.map(|c| c.context_variants).unwrap_or(&[]),
            "note": caps.map(|c| c.context_note).unwrap_or(""),
        },
    })
}

/// The launcher's per-role model knobs (#687), as ONE optional payload object
/// rather than eight more positional arguments on [`create_orchestration`]
/// (which already carries eighteen).
///
/// Optional on purpose: **absent is the pre-#687 caller**, and it means "no
/// knob on any role" — today's group, byte for byte. Tauri treats a missing
/// key for an `Option<T>` argument as `None`, so a frontend that does not send
/// this keeps working unchanged.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RoleKnobs {
    pub orchestrator_effort: String,
    pub orchestrator_context: String,
    pub worker_effort: String,
    pub worker_context: String,
    pub reviewer_effort: String,
    pub reviewer_context: String,
    pub planner_effort: String,
    pub planner_context: String,
}

/// Create (or reattach to) an orchestration group and register its
/// orchestrator. Returns the pane spec the frontend opens directly; initial
/// idle workers are spawned in the background once the orchestrator binds.
///
/// Off-thread (#762 — see [`run_blocking`]): the longest single-gesture stall
/// in the orchestration surface. It holds the global `creation` mutex across
/// `group.json` plus the per-agent MCP config writes plus a session-state scan,
/// and while it was synchronous every millisecond of that was paid on the
/// thread that services paint — which is the whole of the launcher's
/// "nothing happens for a moment after I hit Create".
///
/// **Reentrancy.** The one command in #762 whose exclusion was never
/// accidental: `OrchRegistry::creation` (`performance.md` §4 X6) exists
/// *because* two concurrent launches on one repo would otherwise pick the same
/// group id — id selection by liveness and orchestrator registration are one
/// unit by design, argued and named long before this conversion. Moving the
/// body to a pool thread changes which thread waits on that mutex, not what it
/// guarantees; two launches still serialize, and the second still sees the
/// first's group as live. Nothing in the body runs before the mutex is taken
/// except argument marshalling.
#[tauri::command]
#[allow(clippy::too_many_arguments)] // launcher-collected guardrails, one field each
pub async fn create_orchestration(
    app: AppHandle,
    repo: String,
    // #1020 item 5: OPTIONAL, and the launcher stops sending it. An absent
    // value deserializes to `None`, which `starter_workers` resolves to 0 — no
    // idle workers at launch, the orchestrator opens what the work needs.
    // Optional rather than deleted because this argument is still the only way
    // a caller CAN ask, and a wire shape that could no longer express "open
    // two" would be a capability removal dressed up as a default change.
    initial_workers: Option<u32>,
    max_agents: u32,
    agent_cli: String,
    // Per-role CLI overrides (issue #4). Empty inherits `agent_cli`; the
    // launcher sends the picked CLI for each role.
    orchestrator_cli: String,
    worker_cli: String,
    reviewer_cli: String,
    planner_cli: String,
    worker_model: String,
    reviewer_model: String,
    orchestrator_model: String,
    planner_model: String,
    auto_ops: bool,
    idle_kill_minutes: u32,
    max_spawns_per_hour: u32,
    watchdog_stall_minutes: u32,
    // The advanced-orchestrator toggle (#222). Off = this group never opens the
    // repo's `.loomux/workflow.yml` and runs the roster below, exactly as loomux
    // did before workflows existed.
    advanced_orchestrator: bool,
    // WHICH of the repo's workflows this group runs (#1689 slice D1). Omitted —
    // every caller written before named workflows, and the launcher's own form in
    // a repo that declares one file — is `default`, i.e. `.orrerix/workflow.yml`,
    // which is the pre-#1689 launch byte for byte.
    //
    // Separate from `advanced_orchestrator`, which is the CONSENT: the toggle
    // decides whether a repo-authored roster runs at all, this decides only which
    // file it comes from. A name arriving with the toggle off is recorded and
    // inert, exactly as the roster it names is, so turning the toggle on live
    // comes back to the file the human chose rather than to `default`.
    workflow: Option<String>,
    // Per-role thinking level / context window (#687). Omitted = no knob on any
    // role, i.e. today's group byte for byte — see `RoleKnobs`.
    role_knobs: Option<RoleKnobs>,
) -> Result<SpawnRequest, String> {
    let reg = reg_of(&app);
    run_blocking(move || create_orchestration_sync(
        &reg, repo, initial_workers, max_agents, agent_cli, orchestrator_cli, worker_cli,
        reviewer_cli, planner_cli, worker_model, reviewer_model, orchestrator_model,
        planner_model, auto_ops, idle_kill_minutes, max_spawns_per_hour,
        watchdog_stall_minutes, advanced_orchestrator, workflow, role_knobs,
    ))
    .await
}

/// The body of [`create_orchestration`], as a plain function so the command
/// itself is the thin delegation `performance.md` §2 P1 asks for rather than a
/// nineteen-argument closure.
#[allow(clippy::too_many_arguments)] // it is `create_orchestration`'s argument list, verbatim
#[doc(hidden)] // pub for integration tests
pub fn create_orchestration_sync(
    reg: &Arc<OrchRegistry>,
    repo: String,
    initial_workers: Option<u32>,
    max_agents: u32,
    agent_cli: String,
    orchestrator_cli: String,
    worker_cli: String,
    reviewer_cli: String,
    planner_cli: String,
    worker_model: String,
    reviewer_model: String,
    orchestrator_model: String,
    planner_model: String,
    auto_ops: bool,
    idle_kill_minutes: u32,
    max_spawns_per_hour: u32,
    watchdog_stall_minutes: u32,
    advanced_orchestrator: bool,
    workflow: Option<String>,
    role_knobs: Option<RoleKnobs>,
) -> Result<SpawnRequest, String> {
    // #1689: parse at the BOUNDARY, so the value that reaches `Guardrails` is a
    // `WorkflowName` and the compiler — not a scan — is what stops a webview
    // string reaching `workflow_path_named` (CLAUDE.md constraint 6).
    //
    // REFUSED, not defaulted, and that is the launch/load asymmetry stated on
    // `Guardrails::workflow`: `load_group_file` falls back to `default` for an
    // unusable persisted name because a group must stay rejoinable, while a
    // LAUNCH has a human in front of it and no reason to silently run a workflow
    // other than the one they picked. Nothing is created — this is above
    // `create_orchestration_group`'s own checks, so the refusal costs no state.
    let workflow = match workflow.as_deref() {
        None => workflow::WorkflowName::default_name(),
        Some(raw) => workflow::WorkflowName::parse(raw)
            .map_err(|e| format!("that is not a usable workflow name: {e}"))?,
    };
    // The launcher still collects one CLI + model per role — that IS the
    // built-in 4-block roster (#222), just spelled as flat form fields. Convert
    // it here, at the boundary, so the launcher's wire shape is untouched and
    // the backend has blocks from this point on. A repo that declares
    // `.loomux/workflow.yml` overrides this roster in `create_group` — but only
    // when `advanced_orchestrator` is on.
    //
    // The knobs are passed through RAW: `create_orchestration_group` runs
    // `Guardrails::clamped`, which is the one place that knows each block's
    // resolved CLI and therefore whether the knob can be honored at all. The
    // launcher already greys out what a CLI can't do (`agent_cli_knobs`), so
    // this is the belt to that braces, not the only check.
    let k = role_knobs.unwrap_or_default();
    let blocks = workflow::default_roster_ex(&[
        (
            Role::Orchestrator,
            &orchestrator_cli,
            &orchestrator_model,
            workflow::ModelKnobs { effort: &k.orchestrator_effort, context: &k.orchestrator_context },
        ),
        (
            Role::Worker,
            &worker_cli,
            &worker_model,
            workflow::ModelKnobs { effort: &k.worker_effort, context: &k.worker_context },
        ),
        (
            Role::Reviewer,
            &reviewer_cli,
            &reviewer_model,
            workflow::ModelKnobs { effort: &k.reviewer_effort, context: &k.reviewer_context },
        ),
        (
            Role::Planner,
            &planner_cli,
            &planner_model,
            workflow::ModelKnobs { effort: &k.planner_effort, context: &k.planner_context },
        ),
    ]);
    create_orchestration_group(
        reg,
        &repo,
        Guardrails {
            max_agents,
            agent_cli,
            blocks,
            advanced_orchestrator,
            workflow,
            auto_ops,
            idle_kill_minutes,
            max_spawns_per_hour,
            watchdog_stall_minutes,
            // #83: no autonomous budget at launch; the human sets it live via
            // orch_set_autonomy_budget (W2 adds the launcher knob later). 0 = no cap.
            autonomy_budget_tokens: 0,
            // #83: 0 → clamped() applies DEFAULT_IDLE_TICK_MINUTES; live-settable via
            // orch_set_idle_tick_minutes.
            idle_tick_minutes: 0,
            // #83: 0 → clamped() applies DEFAULT_IDLE_ACTIVITY_FLOOR_BYTES; live-settable
            // via orch_set_idle_activity_floor.
            idle_activity_floor_bytes: 0,
            // #496: 0 → clamped() applies DEFAULT_IDLE_TICK_INPUT_DEFER_MAX_MINUTES; no
            // live setter this round (see the field's own doc) — a launch-time default,
            // adjustable by hand-editing group.json.
            idle_tick_input_defer_max_minutes: 0,
            // #287: off at launch (the conservative default); live-settable via
            // orch_set_compact_nudge_minutes/orch_set_compact_nudge_roles.
            compact_nudge_minutes: 0,
            compact_nudge_roles: Vec::new(),
            // Min-context floor: unset at launch — the smart default (50%)
            // applies automatically once compact_nudge_minutes is turned on
            // (live or otherwise), with zero config. live-settable via
            // orch_set_compact_nudge_min_context_percent (which always sets
            // an explicit value, never restores this None).
            compact_nudge_min_context_percent: None,
            // #3497: new groups escalate at 45%; an explicit persisted 0 remains off.
            compact_context_threshold_percent: DEFAULT_COMPACT_CONTEXT_THRESHOLD_PERCENT,
            // Context-window override (PR #329 round 7): no launcher field
            // yet (same precedent as max_spawns_per_hour) — a human who
            // knows their deployment's actual context tier sets this by
            // hand-editing group.json; `None` defers to the model-based
            // guess.
            context_window_tokens_override: None,
            // #382 P1: the launcher has no intake picker yet — the built-in
            // `github-labels` profile stands unless the repo's workflow file
            // overrides it (only takes effect with `advanced_orchestrator` on,
            // gated exactly like `blocks` in `create_group_ex`).
            intake: workflow::IntakeProfile::default(),
            // #429: unset at launch — the smart default (DEFAULT_INTAKE_POLL_
            // MINUTES) applies automatically the moment the group is autonomous,
            // with zero config, same idiom as compact_nudge_min_context_percent
            // above. There is no live setter; an operator who wants the gate off
            // even while autonomous opts out by hand-editing group.json to an
            // explicit `Some(0)`.
            intake_poll_minutes: None,
            // #332: 0 → clamped() applies DEFAULT_IDLE_TICK_FALLBACK_MINUTES.
            idle_tick_fallback_minutes: 0,
            // #864: 0 → clamped() applies DEFAULT_IDLE_TICK_FALLBACK_MAX_MINUTES,
            // so the backoff is on by default for every new group; hand-edit
            // group.json to pin it to the base (no backoff).
            idle_tick_fallback_max_minutes: 0,
        },
        SessionOrigin::Fresh,
        None,
        initial_workers,
    )
}

/// The knobs a **promote** (#407) may set, as ONE optional payload object —
/// same shape and same reasoning as [`RoleKnobs`] above, and absent means
/// "every default", which is what makes the promote gesture one click.
///
/// This is deliberately a SUBSET of `create_orchestration`'s nineteen
/// arguments rather than a second copy of them: a promote is a right-click on a
/// pane, not the launcher, and the full knob surface stays where the human can
/// actually see what they are choosing. Everything omitted resolves exactly as
/// it would for a launcher group — an empty model string is the block's
/// CLI-default model (`workflow::model_of`), a `0` minute/count field is
/// `Guardrails::clamped`'s default — so a promoted group is configured like any
/// other group, never like a special case.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PromoteConfig {
    /// Live-agent cap. 0 → the clamped default.
    pub max_agents: u32,
    /// Idle workers to open once the promoted orchestrator binds. 0 — the
    /// default — because a promotion happens mid-conversation about a specific
    /// piece of work: the orchestrator decides what it needs, the way a resumed
    /// one does.
    pub initial_workers: u32,
    /// Run the repo's `.loomux/workflow.yml` roster (the promote modal's one
    /// checkbox, offered only when the file exists). Subject to the same
    /// consent rule as any other group — see [`Launch::Promote`].
    pub advanced_orchestrator: bool,
    pub auto_ops: bool,
    pub idle_kill_minutes: u32,
    pub max_spawns_per_hour: u32,
    pub watchdog_stall_minutes: u32,
    /// Per-role models. Empty (the default) = each block's CLI default.
    pub orchestrator_model: String,
    pub worker_model: String,
    pub reviewer_model: String,
    pub planner_model: String,
    pub role_knobs: Option<RoleKnobs>,
    /// The `__solo__` identity this pane currently holds (`solo_prepare`/
    /// `solo_adopt` mint one on connect), if any. Retired as part of the
    /// promotion — see the retirement below for why it cannot be left behind.
    pub solo_agent_id: String,
}

/// Promote a standalone agent pane to the orchestrator of a real orchestration
/// group (#407), reusing its CLI **session** so the human's prototype
/// conversation becomes the orchestrator's own context instead of being
/// hand-transferred into a fresh one.
///
/// Returns the same `SpawnRequest` shape every orchestrator launch returns; the
/// frontend kills the pane's CLI and relaunches it in place from this spec (the
/// old process must have exited before `--resume` reads its transcript).
///
/// **Every refusal is tagged `promote-<reason>:`**, mirroring `resume-<tag>:`
/// (see `resumeerror.ts`), so the frontend can say precisely why one click did
/// nothing rather than surfacing a sentence. Refusals happen BEFORE anything is
/// created or retired — a refused promote leaves the running pane untouched,
/// which is the whole safety story of a gesture that otherwise interrupts a
/// live conversation.
///
/// **Trust posture.** Webview-supplied arguments, exactly like every other
/// group-scoped command (CLAUDE.md constraint 6): `repo` is a path the human's
/// own pane is already running in, `session_id` is shape-checked
/// (`sanitize_session`) before it reaches a command line, and nothing here is
/// reachable from MCP — no agent-controllable input arrives at this seam.
#[tauri::command]
pub async fn promote_to_orchestrator(
    app: AppHandle,
    repo: String,
    session_id: String,
    cli: String,
    config: Option<PromoteConfig>,
) -> Result<SpawnRequest, String> {
    let reg = reg_of(&app);
    run_blocking(move || {
        promote_to_orchestrator_sync(&reg, &repo, &session_id, &cli, config.unwrap_or_default())
    })
    .await
}

/// The body of [`promote_to_orchestrator`], as a plain function — the same
/// split `create_orchestration`/`create_orchestration_sync` uses, and what the
/// integration tests drive (there is no `AppHandle` in test mode).
#[doc(hidden)] // pub for integration tests
pub fn promote_to_orchestrator_sync(
    reg: &Arc<OrchRegistry>,
    repo: &str,
    session_id: &str,
    cli: &str,
    config: PromoteConfig,
) -> Result<SpawnRequest, String> {
    let session_id = session_id.trim();
    let cli = cli.trim();
    let repo = repo.trim();

    // ── v1 scope: claude panes only ──────────────────────────────────────
    // Not a capability limit so much as a validation one: the checks below
    // (full-id shape, "does this session exist in the store") are asked of
    // claude's store, and copilot/opencode panes reach loomux without a known
    // session id at all today. The seam itself is CLI-generic — the plan's
    // follow-up widens this arm, not the machinery under it.
    if cli != "claude" {
        return Err(format!(
            "promote-unsupported-cli: promoting a {cli} pane is not supported yet — v1 covers \
             Claude panes, whose session id loomux knows and can resume"
        ));
    }
    // A full id, never a prefix: the frontend holds the pane's real session id,
    // so anything shorter is a caller bug, and `--resume <prefix>` would fail
    // inside the pane after the old process was already killed.
    let Some(session_id) = sanitize_session(session_id).filter(|s| is_full_session_id(s)) else {
        return Err(format!(
            "promote-bad-session: {session_id:?} is not a full session id — promotion resumes an \
             exact conversation and never resolves a prefix"
        ));
    };
    // Same two path rules `create_orchestration_group` enforces, checked here
    // so the failure carries a parseable tag (that one predates the convention
    // and answers to the launcher, which has its own file picker).
    if repo.is_empty() || repo.contains('"') || !Path::new(repo).is_dir() {
        return Err(format!(
            "promote-bad-repo: {repo:?} is not a usable repository directory — a promoted pane's \
             own working directory becomes the group's repo"
        ));
    }
    // ── never promote a pane that is already an orchestration member ─────
    // A delegate's transcript carries a delegate contract (and a group
    // membership loomux records); promoting it would seat two conflicting role
    // contracts in one session, and — for a recorded ORCHESTRATOR — would mint
    // a second orchestrator for a group that already has one. The frontend
    // refuses this live (a pane in a group has no promote item at all); this is
    // the recorded half, which also covers the pane that WAS a delegate in an
    // earlier, now-dormant group.
    if let Some(rec) = reg.session_roles().into_iter().find(|r| r.session_id == session_id) {
        return Err(format!(
            "promote-already-managed: session {session_id} is already recorded as the {} of \
             orchestration group {} — resume it from the session browser instead of promoting it",
            rec.role, rec.group_id
        ));
    }
    // ── the session has to exist, and to have something in it ────────────
    // The same existence pre-check the orchestrator resume path runs (#412
    // review B1), for the same reason and with the same store router: a
    // `--resume` against an id claude's history does not hold fails INSIDE the
    // pane, after the promotion has already killed the conversation it was
    // meant to preserve. It doubles as the guard against promoting an empty
    // session (claude writes no transcript until the first exchange, so
    // "never spoke to it" reads as "not found" here rather than as claude's
    // opaque "No conversation found" a moment later).
    match session_cwd_in_store(cli, &session_id, None, None) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return Err(format!(
                "promote-not-found: session {session_id} is not in the {cli} session history on \
                 this machine — an unused pane has no conversation to carry over yet"
            ));
        }
        Err(e) => {
            return Err(format!(
                "promote-store-unreadable: could not read the {cli} session store: {e}"
            ));
        }
    }

    let k = config.role_knobs.unwrap_or_default();
    // Every block inherits `agent_cli` (empty `cli:`) exactly as a launcher
    // group's does — the promoted pane's CLI becomes the group default, so the
    // delegates it spawns are the same CLI it is.
    let blocks = workflow::default_roster_ex(&[
        (
            Role::Orchestrator,
            "",
            &config.orchestrator_model,
            workflow::ModelKnobs { effort: &k.orchestrator_effort, context: &k.orchestrator_context },
        ),
        (
            Role::Worker,
            "",
            &config.worker_model,
            workflow::ModelKnobs { effort: &k.worker_effort, context: &k.worker_context },
        ),
        (
            Role::Reviewer,
            "",
            &config.reviewer_model,
            workflow::ModelKnobs { effort: &k.reviewer_effort, context: &k.reviewer_context },
        ),
        (
            Role::Planner,
            "",
            &config.planner_model,
            workflow::ModelKnobs { effort: &k.planner_effort, context: &k.planner_context },
        ),
    ]);
    let request = create_orchestration_group(
        reg,
        repo,
        Guardrails {
            max_agents: config.max_agents,
            agent_cli: cli.to_string(),
            blocks,
            advanced_orchestrator: config.advanced_orchestrator,
            auto_ops: config.auto_ops,
            idle_kill_minutes: config.idle_kill_minutes,
            max_spawns_per_hour: config.max_spawns_per_hour,
            watchdog_stall_minutes: config.watchdog_stall_minutes,
            // Everything below is `create_orchestration`'s "not a launch-time
            // knob" set, verbatim: 0/None → `clamped()`'s default, live-settable
            // afterwards, and re-hydrated from `group.json` when this promote
            // reattaches a dormant group (`create_group_ex`).
            ..Guardrails::default()
        },
        SessionOrigin::Promote { session_id: session_id.clone(), cli: cli.to_string() },
        // No `expect_group`: a promote does not know which group id it will
        // land on — that IS the existing-group policy (new dir, dormant
        // reattach, or live sibling), resolved by `create_group_ex`'s candidate
        // scan under the creation lock.
        None,
        // `Some`, not `None`: a promote's count is a real, defaulted field of
        // its own payload (`PromoteConfig`, 0 unless the modal says otherwise),
        // so this caller HAS an answer — it just usually says zero. `None` means
        // "nobody was asked", which after #1020 is the launcher and only the
        // launcher.
        Some(config.initial_workers),
    )?;

    // ── retire the pane's standalone identity ────────────────────────────
    // The pane held a `__solo__` `AgentEntry` (minted by `solo_prepare` at
    // launch or `solo_adopt` on connect) keyed to the pty it is about to
    // relaunch as an orchestrator. Left alive it is a stale channel endpoint
    // and a stale `by_pty` claim on a pane that is now something else — a
    // `channel_send` would be delivered into an orchestrator that never joined
    // that channel. `mark_dead` is the existing solo teardown (the same one a
    // closed solo pane goes through), so this retires it exactly as closing the
    // pane would have.
    //
    // AFTER the group creation, never before: a refusal above must leave the
    // running pane whole, identity included.
    //
    // Scoped to `Role::Solo` deliberately: this argument comes from the
    // webview, and a `solo_agent_id` naming a real orchestration agent must
    // never be able to kill it through this door.
    if !config.solo_agent_id.trim().is_empty() {
        let solo = config.solo_agent_id.trim();
        match reg.agent(solo) {
            Some(a) if a.role == Role::Solo => {
                reg.mark_dead(solo, None);
                reg.audit(&request.group_id, brand::AUDIT_ACTOR, "solo-retired", json!({
                    "agent": solo,
                    "why": "promoted to orchestrator",
                    "orchestrator": request.agent_id,
                }));
            }
            // Not fatal either way: the group is already created and the pane
            // spec is what the caller needs. A missing id is the ordinary
            // never-connected pane; a non-solo id is a caller bug worth a trail.
            Some(a) => reg.audit(&request.group_id, brand::AUDIT_ACTOR, "solo-retire-skipped", json!({
                "agent": solo, "role": a.role, "why": "not a standalone pane identity",
            })),
            None => {}
        }
    }
    Ok(request)
}

/// What turning the **advanced orchestrator** on for `repo` would actually run
/// (#222) — asked by the launcher *before* the human hits Create, so they see the
/// roster they are enabling rather than discovering it in four spawned panes.
///
/// This is deliberately not a second implementation of the schema. It runs the
/// same `load_workflow` + `Guardrails::clamped` that `create_group` runs, on a
/// throwaway `Guardrails`, and reports the resolved blocks — so a preview that
/// disagrees with the launch is a bug in one shared path, not a drift between two.
/// (The workflow *pane* validates the file too, in TypeScript, but that pane is an
/// editor giving live feedback on text; this is the launcher asking the engine.)
///
/// `agent_cli` is the group's default CLI, because a block may inherit from it —
/// the same picker feeds this and the launch.
///
/// Never fails: a broken file is `{ valid: false, errors: [...] }` and the group
/// would fall back to the built-in roster, which is precisely what the launcher
/// needs to say. Nothing here is persisted and no group is created.
///
/// Off-thread (#762 — see [`run_blocking`]): it opens and parses
/// `.loomux/workflow.yml` from disk, on every preview gesture, on the thread
/// that services paint. The file is small; the path it sits on need not be.
///
/// **Reentrancy.** Nothing to argue, and that is a property of the command
/// rather than an absence of thought: it takes no registry state, holds no
/// lock, writes nothing and creates no group — the doc above already says so,
/// because the launcher's whole reason for asking is that asking is free. Two
/// previews of one repo read the same file twice and cannot interact.
#[tauri::command]
pub async fn orch_workflow_preview(repo: String, agent_cli: String, name: Option<String>) -> Value {
    run_blocking(move || orch_workflow_preview_sync(repo, agent_cli, name)).await
}

/// Every workflow this repo declares (#1689) — the launcher's picker and the
/// group header's read the same list from here.
///
/// Read-only and repo-scoped: it opens `.orrerix/workflows/` (or the legacy
/// spelling), parses each `<name>.yml`, and creates nothing. A repo with only
/// `.orrerix/workflow.yml` answers with the single `default` row it has always
/// effectively had, and a repo with neither answers with an empty list — which
/// is not an error, it is how you start before you write a file.
///
/// `findings` carries what the LISTING could not make sense of, as opposed to
/// what one file could not parse: a `default` declared twice, a file whose stem
/// is not a usable name. Advisory — nothing here blocks a launch.
///
/// Off-thread (#762 — see [`run_blocking`]): one `read_dir` plus a YAML parse
/// per file, on a gesture, on the thread that services paint.
///
/// **Reentrancy.** Nothing to argue, and for the same reason
/// [`orch_workflow_preview`] has nothing to argue: it takes no registry state,
/// holds no lock, writes nothing. Two listings of one repo read the same
/// directory twice and cannot interact.
#[tauri::command]
pub async fn orch_workflow_list(repo: String) -> Value {
    run_blocking(move || orch_workflow_list_sync(repo)).await
}

/// The body of [`orch_workflow_list`], as a plain function so the command is a
/// thin delegation and the tests can exercise it without a Tauri runtime.
#[doc(hidden)] // pub for integration tests
pub fn orch_workflow_list_sync(repo: String) -> Value {
    let listing = workflow::list_workflows(&repo);
    json!({
        "workflows": listing.workflows.iter().map(|e| json!({
            "name": e.name,
            "path": e.path,
            // The file's own `name:` — human prose, and empty when the file
            // will not parse.
            "display_name": e.display_name,
            // Spelled the way `orch_workflow_preview` spells it, so the
            // frontend reads one vocabulary for one question.
            "valid": e.errors.is_empty(),
            "errors": e.errors,
        })).collect::<Vec<_>>(),
        "findings": listing.findings,
    })
}

/// The body of [`orch_workflow_preview`], as a plain function: it is the
/// launcher's answer to "what would this launch run", and the workflow tests
/// exercise it directly, without a Tauri runtime.
///
/// **`name`** (#1689) picks which of the repo's workflows to preview.
///
/// `None` — every caller written before named workflows, and the launcher's own
/// default — means `default`, which resolves to `.orrerix/workflow.yml` whenever
/// that file exists: the same file, and the same JSON, byte for byte. A name
/// that is not a usable one is reported as a validation error on an absent
/// workflow rather than echoed back into `path`, so nothing untrusted reaches a
/// display surface through here.
#[doc(hidden)] // pub for integration tests
pub fn orch_workflow_preview_sync(repo: String, agent_cli: String, name: Option<String>) -> Value {
    let wanted = match name.as_deref() {
        None => Ok(workflow::WorkflowName::default_name()),
        Some(raw) => workflow::WorkflowName::parse(raw),
    };
    let rel = match &wanted {
        Ok(n) => workflow::workflow_path_named(&repo, n),
        Err(_) => String::new(),
    };
    let present = match &wanted {
        Ok(n) => workflow::workflow_file_named(&repo, n).is_file(),
        Err(_) => false,
    };
    let loaded = match &wanted {
        Ok(n) => workflow::load_workflow_named(&repo, n),
        Err(e) => Err(vec![format!("that is not a usable workflow name: {e}")]),
    };
    let (name, blocks, gates, errors, capacity) = match loaded {
        Ok(Some(wf)) => {
            let gates: Vec<String> = wf.gates.keys().cloned().collect();
            // #255: same derivation `create_group_ex` records at load time, so the
            // launcher's warning and the audit trail can never disagree about what
            // a launch would compute.
            let capacity = workflow::recommend_capacity(&wf.blocks, wf.gates.get("merge"));
            (wf.name, wf.blocks, gates, Vec::new(), Some(capacity))
        }
        Ok(None) => (String::new(), Vec::new(), Vec::new(), Vec::new(), None),
        Err(errors) => (String::new(), Vec::new(), Vec::new(), errors, None),
    };
    // Resolve exactly as a launch would: `clamped()` is what fills in an
    // inherited CLI's default model, guarantees the orchestrator block, and drops
    // a row a hand-edit could have made unreachable. Without it the preview would
    // show `model: ""` for every block that inherits — i.e. most of them.
    let resolved = if blocks.is_empty() {
        Vec::new()
    } else {
        Guardrails { agent_cli: agent_cli.clone(), blocks, ..Guardrails::default() }
            .clamped()
            .blocks
    };
    let agent_cli = if SUPPORTED_CLIS.contains(&agent_cli.as_str()) { agent_cli } else { "claude".into() };
    // #255 (rev-1 B1): computed from the RESOLVED blocks so it can never disagree
    // with the roster table below — and handed to the frontend pre-computed so
    // `roster.ts` never has to recount reviewer blocks to describe a number that
    // came from the gate (that conflation was rev-1's finding).
    let extra_tiers = capacity.map(|c| workflow::extra_tiers(&resolved, c.reviewers_needed));
    json!({
        "path": rel,
        "present": present,
        "valid": errors.is_empty(),
        "name": name,
        "errors": errors,
        "gates": gates,
        // #255: null when there's no declared workflow to derive from (absent or
        // invalid file) — the launcher has nothing to warn about in that case,
        // since the group would run the built-in roster.
        "min_agents": capacity.map(|c| c.minimum),
        "recommended_agents": capacity.map(|c| c.recommended),
        "reviewers_needed": capacity.map(|c| c.reviewers_needed),
        "extra_tiers": extra_tiers,
        "blocks": resolved.iter().map(|b| json!({
            "id": b.id,
            "name": b.name,
            "kind": b.kind.as_str(),
            "cli": workflow::cli_of(b, &agent_cli),
            "model": workflow::model_of(b, &agent_cli),
            // #250/#324/#891: surfaced so the launcher preview can badge an
            // advisor/process/liaison block. Cosmetic HERE — this row feeds a
            // badge, not a gate — though the hint itself is not capability-inert
            // in general (see `workflow::Block::role_hint`).
            "role_hint": b.role_hint,
            // #687: the block's RESOLVED knobs (these rows have been through
            // `clamped()` above), because the preview's job is to state the whole
            // spawn and a thinking level is part of it. It is also what makes the
            // trust argument for letting a repo file pin `effort:` on the
            // ORCHESTRATOR block (docs/design/workflows.md) true rather than
            // aspirational: that argument rests on the human being shown every
            // block's resolved value here, before the toggle that reads the file.
            "effort": b.effort,
            "context": b.context,
            // What the human is really being asked to consent to: whether this
            // block carries repo-authored instructions for the agent.
            //
            // Asked through the same predicate the SPAWN asks (rev-11's nit): an
            // orchestrator block's persona is denied at `resolve_persona`, so
            // reporting one here would advertise instructions that will never
            // reach an agent. Unreachable from a parsed workflow file, which
            // rejects it outright — but a preview must not be able to claim what a
            // launch would drop, whatever produced the block.
            "persona": if !workflow::persona_allowed(b) {
                "none"
            } else if b.profile.is_some() {
                "profile"
            } else if b.prompt.is_some() {
                "prompt"
            } else {
                "none"
            },
        })).collect::<Vec<_>>(),
    })
}

/// Pause a group: loomux stops delivering prompts/kickoffs so its agents
/// idle out (cost containment). Human action from the pane UI.
///
/// Off-thread (#743 S4c — see [`run_blocking`]): a marker write plus an audit
/// append under the process-global audit lock.
///
/// **Reentrancy.** The durable write is guarded by the `paused` set's own
/// insert: `pause_group` writes the marker and audits only when the insert was
/// NEWLY true, so two concurrent pauses produce one write and one audit line
/// however they interleave. A pause racing a resume resolves to whichever wins
/// the set lock — the same non-determinism two fast human clicks already had,
/// and the group view disables the button for the duration of the call.
#[tauri::command]
pub async fn orch_pause_group(app: AppHandle, group_id: String) -> Result<(), String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        let out = reg.pause_group(&group_id);
        // #1608: republish this group before the command returns, so the
        // group view's own post-action reload — which is immediate, not on
        // the next publish tick — cannot read the pre-write snapshot.
        reg.publish_group_now(&group_id);
        out
    })
    .await
}

/// Resume a paused group: prompt/kickoff delivery flows again.
///
/// Off-thread (#743 S4c): marker removal, an audit-log READ (the pause window's
/// suppressed deliveries), an audit append, and the queue flush that re-delivers
/// everything the pause held.
///
/// **Reentrancy.** Same guard as `orch_pause_group`, in the mirror direction:
/// the audit read, the notice, and `flush_paused_queues` run only when the
/// `paused` set's remove reported the group WAS paused, so a double-resume
/// cannot flush a pane's queue twice. (The drainer's own at-most-one-per-pane
/// registration, #470, is the second line of defence.)
#[tauri::command]
pub async fn orch_resume_group(app: AppHandle, group_id: String) -> Result<(), String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        let out = reg.resume_group(&group_id);
        // #1608: republish this group before the command returns, so the
        // group view's own post-action reload — which is immediate, not on
        // the next publish tick — cannot read the pre-write snapshot.
        reg.publish_group_now(&group_id);
        out
    })
    .await
}

/// Whether a group is currently paused (drives the pause/resume button state).
/// **Off the UI thread** (#1595). Tauri dispatches a SYNC command directly on
/// the webview/GTK main-loop thread, and `is_paused` takes a registry mutex shared
/// with the background threads (idle reaper, watchdog, gh poller, and the pty
/// path's `note_agent_activity`). A bare `lock_safe` is an infallible
/// acquire — there was no timed form of it anywhere until #1609, and a
/// command like this one gets the bounded form only by running under a
/// budget frame, which a sync command on the webview thread does not — so
/// the acquisition is
/// UNBOUNDED, and on the UI thread an unbounded acquisition is a frozen app,
/// not a slow one. That is #1595's freeze, and it is the same class as #1593's
/// `orch_session_roles`: cheap work, fatal thread.
///
/// "Cheap" was never the property that made this safe to be sync. A cheap
/// CRITICAL SECTION is not a cheap ACQUISITION when someone else holds the
/// lock, and this command is on a fixed-cadence poll, so it re-asks that
/// question every tick forever.
#[tauri::command]
pub async fn orch_group_paused(app: AppHandle, group_id: String) -> bool {
    // #904 / rev-440 N4: `false` is indistinguishable from the honest answer
    // for an unknown group — deliberately, since a caller that cannot name a
    // valid group has no business learning whether one exists. Every MUTATING
    // twin of these three returns `Err` instead; only the read-only pair-state
    // queries degrade silently. See `command_group`.
    let Ok(group_id) = command_group(&group_id) else { return false };
    let reg = reg_of(&app);
    run_blocking(move || reg.is_paused(&group_id)).await
}
