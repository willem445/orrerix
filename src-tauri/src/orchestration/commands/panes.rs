//! Pane lifecycle from the human side: standalone (solo) panes (#271 W3),
//! lead panes (#2519), ending a group, binding/renaming agents, recorded
//! sessions and resume, and forks. Moved from `orchestration/mod.rs` by
//! #3498 P2; see `commands/mod.rs`.

use super::*;

// ---------- standalone panes (#271 W3 addendum, part A): human-only, from
// the launcher's agent-pane spawn path or the pane-menu Connect gesture.

/// Mint a channel-scoped identity for a newly-launching standalone pane
/// BEFORE it boots. See `OrchRegistry::solo_prepare`.
/// Off-thread (#762): it writes the pane's MCP config and appends an audit
/// entry BEFORE the CLI is launched, so every millisecond of it is latency the
/// human reads as a slow pane open.
///
/// **Reentrancy.** Every durable thing this creates is keyed by an id minted
/// under `agent_seq_persist`, which is already a lock rather than an atomic
/// (#524 rev-13 N1, so the high-water counter can never wrap and reissue), and
/// an id is never re-minted. Two concurrent prepares therefore write two
/// distinct config files for two distinct agents and cannot collide on a path,
/// a token, or a roster entry. `ensure_solo_group` is idempotent — it inserts
/// the standalone group only if absent — so racing it produces one group.
#[tauri::command]
pub async fn orch_solo_prepare(
    app: AppHandle,
    cli: String,
    cwd: String,
    name: String,
) -> Result<Value, String> {
    let reg = reg_of(&app);
    run_blocking(move || reg.solo_prepare(&cli, &cwd, &name)).await
}

/// Bind a just-spawned solo pane's pty to the `AgentEntry` `orch_solo_prepare`
/// created. See `OrchRegistry::solo_bind`.
#[tauri::command]
pub fn orch_solo_bind(reg: tauri::State<Arc<OrchRegistry>>, agent_id: String, pty_id: u32) -> Result<(), String> {
    OrchRegistry::mutating_command("orch_solo_bind", || Err(COMMAND_REFUSED.to_string()), || {
        reg.solo_bind(&agent_id, pty_id)
    })
}

/// Start the solo-pane copilot autopilot consent watcher (#364). See
/// `OrchRegistry::confirm_solo_copilot_autopilot`.
#[tauri::command]
pub fn orch_confirm_solo_copilot_autopilot(
    reg: tauri::State<Arc<OrchRegistry>>,
    pty_id: u32,
    cli: String,
) -> Result<(), String> {
    OrchRegistry::mutating_command("orch_confirm_solo_copilot_autopilot", || {
        Err(COMMAND_REFUSED.to_string())
    }, || reg.confirm_solo_copilot_autopilot(pty_id, &cli))
}

/// Adopt an already-running pane (no channel identity yet) as a
/// delivery-only member on its first Connect gesture. See
/// `OrchRegistry::solo_adopt`.
/// Off-thread (#762): the adoption records plus an audit append, on a pane's
/// first Connect gesture.
///
/// **Reentrancy.** This one needed a code change, not just an argument.
/// `solo_adopt` was idempotent by pty through a check-then-insert — read
/// `by_pty`, mint, insert — and the two lock acquisitions with a mint between
/// them were only safe because the webview thread could not be inside the
/// function twice. #762 removes that, so the claim becomes a vacant-entry
/// insert on `by_pty`: exactly one adopt of a pty wins, and the loser rolls its
/// own roster entry back and returns the winner's id. The alternative — a
/// second delivery-only identity for one pane — would have been silent, since
/// both calls return an id that looks fine.
#[tauri::command]
pub async fn orch_solo_adopt(
    app: AppHandle,
    pty_id: u32,
    name: String,
    cwd: String,
    cli: Option<String>,
    session_id: Option<String>,
) -> Result<Value, String> {
    let reg = reg_of(&app);
    run_blocking(move || reg.solo_adopt(pty_id, &name, &cwd, cli.as_deref(), session_id.as_deref())).await
}

/// Record the session id of a solo or lead pane (#3831): the cache-age chip
/// reads that pane's usage through its own transcript, and the id arrives from
/// the CLI after the pane is registered for some CLIs. See
/// `OrchRegistry::human_pane_session`.
///
/// Synchronous, and inside `mutating_command` (constraint 10), like its sibling
/// `orch_solo_bind`. It does one audit append on the webview thread, and for a
/// lead one roster write; both are small, and a lead's roster row is the only
/// disk write here.
#[tauri::command]
pub fn orch_human_pane_session(
    reg: tauri::State<Arc<OrchRegistry>>,
    agent_id: String,
    session_id: String,
) -> Result<(), String> {
    OrchRegistry::mutating_command("orch_human_pane_session", || Err(COMMAND_REFUSED.to_string()), || {
        reg.human_pane_session(&agent_id, &session_id)
    })
}

// ---------- lead panes (#2519): human-only, from the launcher's
// "orrerix subagents" toggle on an agent-pane launch.

/// Mint a lead group and the lead's identity BEFORE its pane boots. See
/// `OrchRegistry::lead_prepare`.
///
/// **Off-thread (#762), like every other group mint**, and deliberately not a
/// synchronous command: this writes `group.json`, the whole roster's
/// instruction files and the pane's MCP config before the CLI is launched, and
/// `create_orchestration`'s own doc calls that "the longest single-gesture stall
/// in the orchestration surface". Every millisecond of it on the webview thread
/// is latency the human reads as a frozen launcher. An `async` command's body
/// runs on the async runtime rather than in the WebView2 COM frame, so it is
/// outside `synccommands.rs`'s population by construction — the same posture
/// `orch_solo_prepare` and `create_orchestration` already take, and the reason
/// this one does not take `mutating_command`.
///
/// **Reentrancy.** `lead_prepare` holds `OrchRegistry::creation` across the
/// group mint AND the one-root check, so two toggles racing on one repo
/// serialize and the second sees the first's group as live — the same guarantee
/// `create_orchestration` documents, for the same reason (id selection by
/// liveness and root registration are one unit).
#[tauri::command]
#[allow(clippy::too_many_arguments)] // the launcher's guardrail fields, one each
pub async fn orch_lead_prepare(
    app: AppHandle,
    cli: String,
    cwd: String,
    name: String,
    max_agents: u32,
    auto_ops: bool,
    idle_kill_minutes: u32,
    max_spawns_per_hour: u32,
    watchdog_stall_minutes: u32,
) -> Result<Value, String> {
    let reg = reg_of(&app);
    run_blocking(move || {
        reg.lead_prepare(
            &cli,
            &cwd,
            &name,
            max_agents,
            auto_ops,
            idle_kill_minutes,
            max_spawns_per_hour,
            watchdog_stall_minutes,
        )
    })
    .await
}

/// Bind a just-spawned lead pane's pty to the `AgentEntry` `orch_lead_prepare`
/// created, and type its kickoff. See `OrchRegistry::lead_bind`.
///
/// **Off-thread (#762), unlike its sibling `orch_solo_bind`, and the difference
/// is the kickoff.** `solo_bind` is three in-memory writes and no I/O, so it
/// runs inline on the webview thread inside a `mutating_command` frame. This
/// one additionally DELIVERS: `deliver_prompt` appends an audit row, persists
/// the pane's queue and may start a drainer thread — disk work the GUI thread
/// must not pay for, and the reason every other command in this module that
/// delivers (`orch_steer`) is `async` too. Being `async` also puts it outside
/// `synccommands.rs`'s population by construction: Tauri spawns an async body
/// onto the runtime rather than running it in the WebView2 COM frame, so an
/// unwind there is a task failure and not a process abort.
#[tauri::command]
pub async fn orch_lead_bind(
    app: AppHandle,
    agent_id: String,
    pty_id: u32,
) -> Result<(), String> {
    let reg = reg_of(&app);
    run_blocking(move || reg.lead_bind(&agent_id, pty_id)).await
}
/// End a whole orchestration: kill all its agents and (optionally) remove
/// their worktrees. Human-initiated, destructive, audited — the frontend
/// confirms before calling this.
///
/// Off-thread (#762 — see [`run_blocking`]): N SEQUENTIAL blocking
/// `git worktree remove` spawns, then recursive directory deletes, then audits.
/// The largest process-spawn count of any command in E1's manifest, and an
/// INV-2 violation for as long as it ran on the webview thread — a fleet
/// teardown froze the whole app for its duration.
///
/// **Reentrancy.** Every step is idempotent or reports its own failure, which
/// is what a teardown needs to be anyway: `mark_dead` is already idempotent
/// against the asynchronous pty-exit path (it returns `None` for an agent that
/// is already dead, so no second exit notice is emitted), the attachment sweep
/// and the agent-file sweep are `remove_dir_all`/scan-and-delete, and a
/// `git worktree remove` of a path a racer already reclaimed fails and is
/// reported in `worktree_errors` rather than lost. So a second end-group —
/// which the human could already produce by confirming twice, since the *first*
/// call left the members in the roster marked dead — behaves as it did before:
/// it finds nothing live to kill and reports what it could not remove. What is
/// new is that the two can now overlap; the outcome is the same set of
/// reclaimed worktrees and an error row for whichever call lost each race.
///
/// **End-versus-CREATE is the interleaving this does not cover** (rev-260).
/// This takes no `creation` lock, and group ids are chosen by liveness and
/// therefore reused, so a relaunch that starts while a teardown is still in its
/// tail can be handed the id the teardown is finishing with — and then the
/// teardown's directory sweep and its `orch-group-ended` emit land on the NEW
/// group's files and panes. Ending a group is a deliberate, confirmed gesture
/// and relaunching into one mid-teardown is not something a careful human does
/// on purpose, but it is reachable now in a way it was not when the two
/// commands could not overlap. Recorded rather than fixed: serializing this
/// against `creation` is a lifecycle change, not a dispatch one, and it is the
/// same lock gap **#799** owns from the resume side.
#[tauri::command]
pub async fn orch_end_group(
    app: AppHandle,
    group_id: String,
    cleanup_worktrees: bool,
) -> Result<Value, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || reg.end_group(&group_id, cleanup_worktrees)).await
}

/// Idle workers a group opens the moment its orchestrator binds, given what the
/// caller asked for and the group's own live-agent cap.
///
/// **`None` is zero, and that is the whole of #1020 item 5.** The launcher used
/// to collect an "initial workers" count (defaulting to 2) and now collects
/// nothing at all, so an absent value is not a missing field to fill in with a
/// sensible number — it is the human declining to pre-decide. Any N chosen here
/// is chosen before the orchestrator has read the issue, so it is a guess, and a
/// wrong guess costs either panes nobody asked for or spend nobody chose. Zero
/// is the rule [`PromoteConfig::initial_workers`] has always carried, for the
/// same stated reason: the orchestrator decides what it needs.
///
/// The clamp is unchanged and lives here rather than at the caller because the
/// two are one answer — "how many does this launch open" — and splitting them
/// would leave the default in a command's argument list and the cap three
/// functions away, with neither reading as the other's neighbour. A cap of 0
/// therefore still yields 0 however many were asked for, which is the existing
/// behaviour written down rather than a new rule.
pub fn starter_workers(requested: Option<u32>, max_agents: u32) -> u32 {
    requested.unwrap_or(0).min(max_agents)
}

/// Create (or reattach to) a group and register its orchestrator, under the
/// creation lock: the group id is picked by liveness, and a group only
/// becomes live once its orchestrator is registered, so id selection and
/// registration must be atomic against concurrent launches.
/// `expect_group` pins restores to their recorded group id.
///
/// `origin` ([`SessionOrigin`]) says what kind of start this is — which group
/// semantics apply, which session flag the pane gets, and which kickoff it is
/// typed. It replaced a `(Launch, Option<String>)` pair (#407) whose two
/// halves were independent, deliberately (#412 rev-17), but whose four real
/// combinations looked like two: `Launch` answers "may this read
/// `.loomux/workflow.yml` and rebuild the roster/merge-gate from it", and the
/// session id answers "does the orchestrator pane `--resume` a specific
/// conversation, or start a fresh one". These used to be the same bool
/// (`resume_session.is_some()` derived `launch`), which was correct for the two
/// cases that existed at the time — but `resume_recorded_session`'s
/// `start_fresh` path (#412) added a THIRD: an existing, previously-launched
/// group getting a fresh *conversation* on its orchestrator, which must still
/// be `Launch::Resume` (never re-derive the roster/gate the human already
/// consented to at the original launch — see `create_group_ex`'s "consent rule,
/// not an optimization" comment) even though there is no session to resume.
/// Conflating them let a `start_fresh` on an orchestrator silently swap the
/// group's roster to whatever the repo's workflow file currently says, and
/// delete its merge-gate spec if the file no longer declared one — the exact
/// provenance violation `create_group_ex`'s resume path exists to prevent, just
/// reached through a caller that thought "no session id" meant "fresh launch".
/// #407's promote is the fourth, and the first one the pair could not spell at
/// all: a session that IS resumed and still needs the full kickoff.
pub fn create_orchestration_group(
    reg: &Arc<OrchRegistry>,
    repo: &str,
    guardrails: Guardrails,
    origin: SessionOrigin,
    expect_group: Option<&str>,
    initial_workers: Option<u32>,
) -> Result<SpawnRequest, String> {
    // The checks every group-minting path shares (`lead_prepare` is the other
    // caller): a quote would escape the quoted shell line a path is
    // interpolated into, and a repo that is not there cannot host a group.
    validate_group_repo(repo)?;
    let _creation = reg.creation.lock_safe();
    // #407 rev-1 B1: the one refusal that needs a RESOLVED roster, hoisted
    // above `create_group_ex` so that it, like the other six, runs before
    // anything is created. It sits inside the creation lock rather than beside
    // the argument checks in `promote_to_orchestrator_sync` precisely because
    // it has to peek the candidate group id, and only under this lock is the id
    // it peeks the id the launch below picks.
    //
    // The same check is re-derived inside `register_orchestrator_pane` against
    // the roster that actually resolved — see the comment there for the one
    // (racing) way the two can disagree.
    if let SessionOrigin::Promote { cli: pane_cli, .. } = &origin {
        if let Some(resolved) = reg.promote_orchestrator_cli(repo, &guardrails) {
            if &resolved != pane_cli {
                return Err(format!(
                    "promote-cli-mismatch: this repo's orchestrator would run {resolved}, but the \
                     pane being promoted is a {pane_cli} session — a conversation cannot be \
                     resumed under another CLI, so nothing was created"
                ));
            }
        }
    }
    let group = reg.create_group_ex(repo, guardrails, origin.launch())?;
    if let Some(want) = expect_group {
        if group.id != want {
            return Err(format!(
                "group id mismatch (recorded {want}, resolved {}) — another orchestration is live on this repo",
                group.id
            ));
        }
    }
    register_orchestrator_pane(reg, &group, &origin, initial_workers)
}

/// Open this group's MANAGER pane, when its roster declares one (#1161 M3).
///
/// **Beside the orchestrator's own launch path, never through the
/// orchestrator.** That is the whole of the first-class claim: the human's
/// interface exists because the repo's `.loomux/workflow.yml` declares it and
/// loomux opened it, not because an orchestrator was asked to and complied.
/// `spawn_agent(kind: "manager")` is refused (`mcp::call_tool`), so this
/// function and the session browser's manager rejoin are the only two openers
/// there are.
///
/// **`block: None`, deliberately.** `spawn_agent_bound` refuses a NAMED manager
/// block outright — that refusal is what closes the agent-reachable spawn and
/// bare-resume routes — so loomux's own openers must resolve the block by
/// class. For a manager the two resolutions are the same block:
/// `workflow::MANAGER_MAX` is 1, so `block_for(Role::Manager)`'s "the first of
/// that kind" is "the only one".
///
/// **The resume path.** When the group's own launch resumes a conversation
/// (`SessionOrigin::resumes_session` — a dormant group being brought back, or a
/// promoted pane), the manager reopens ITS last recorded session too. A human's
/// conversation with their manager surviving an app restart is the same
/// continuity the orchestrator already gets, and it matters more here: this
/// pane's transcript *is* the record of what the human said. A launch that
/// starts fresh, or a group with no recorded manager session, cold-starts.
///
/// **A resumed manager is typed NOTHING.** `spawn_agent_bound` delivers a
/// follow-up on a resume only when the spawn carries a task, and this one never
/// does — so the pane simply reopens with its history. `Delivery::ResumeKickoff`
/// *is* in the permitted set (`Delivery::permitted_into_manager_pane`), and this
/// path declines to use it: that carve-out exists for a pane that has not become
/// a conversation yet, and a resumed manager pane is nothing but one. The fresh
/// arm's kickoff is the pane's first line, which is the case the carve-out is
/// actually for.
///
/// Errors are audited and told to the orchestrator rather than failing the
/// launch: a group whose manager could not open is degraded (the human talks to
/// the orchestrator directly, exactly as they did before this feature), while a
/// launch that refused to happen is a group the human does not have at all.
///
/// **"Could not open" and "one is already open" are different events** (#1161 M3
/// review N2), and only the first is a degraded group. The singleton refusal is
/// genuinely reachable here — `resume_recorded_session` documents a
/// double-restore race where two restores of one session can both pass its
/// liveness pre-check (#799) — and the degrade notice is exactly the text that
/// triggers the orchestrator's "manager not live" fallback. Telling it the human
/// is unreachable while the human is typing into a live manager pane is worse
/// than saying nothing at all, so that case is audited and nothing is delivered.
/// Decided by RE-ASKING the registry ([`OrchRegistry::has_live_manager`]), never
/// by matching the refusal text.
fn open_manager_pane_at_launch(reg: &Arc<OrchRegistry>, group: &GroupInfo, origin: &SessionOrigin) {
    let Some(block) = group.guardrails.block_for(Role::Manager).cloned() else {
        // No manager declared — the overwhelmingly common case, including every
        // default (no-workflow) group. Nothing is opened and nothing is said.
        return;
    };
    // The manager's own last-touched roster row, for its session id and for the
    // name tier a human rename earned (#95r). Read only when the launch itself
    // resumes; a fresh launch cold-starts even if rows from an earlier life of
    // this group id are still on disk.
    let prior = origin.resumes_session().then(|| {
        reg.merged_records(&group.id)
            .into_iter()
            .filter(|r| r.block == block.id && r.session.is_some())
            .max_by_key(|r| r.updated_ms)
    }).flatten();
    let reg2 = reg.clone();
    let group_id = group.id.clone();
    let open = move || {
        let (name, name_source, session) = match &prior {
            Some(r) => (r.name.clone(), Some(r.name_source), r.session.clone()),
            // Empty name → derived from the minted id, like every other spawn.
            None => (String::new(), None, None),
        };
        match reg2.spawn_agent_ex(
            &group_id, Role::Manager, None, &name, "", false, None, None, session, None, name_source,
        ) {
            Ok(a) => {
                reg2.audit(&group_id, brand::AUDIT_ACTOR, "manager-opened", json!({
                    "agent": a.id, "block": a.block, "resumed": a.session_id.is_some() && prior.is_some(),
                }));
            }
            Err(e) if reg2.has_live_manager(&group_id) => {
                // #1161 M3 review N2. A manager IS live, so this open lost a
                // race it did not need to win — the human has their pane. Not
                // an `error` record, because nothing is wrong.
                reg2.audit(&group_id, brand::AUDIT_ACTOR, "manager-already-live", json!({
                    "block": block.id.clone(), "refusal": e,
                }));
            }
            Err(e) => {
                reg2.audit(&group_id, brand::AUDIT_ACTOR, "error", json!({
                    "what": "manager pane open failed", "block": block.id.clone(), "err": e.clone(),
                }));
                // The orchestrator is told because it is the pane that can act
                // on it: its own `{{MANAGER_NOTE}}` fallback prose (M4) is
                // "manager not live — take the human's input in your own pane",
                // and it cannot take that branch on a fact it was never given.
                // #1161 M3 review N3: the delivery OUTCOME is audited, not
                // discarded. This runs on a background thread racing the
                // orchestrator's own bind, so the notice can arrive before that
                // pane has a terminal and be refused (`no-terminal-at-call`) —
                // and the orchestrator's degradation fallback is premised on
                // having been told. A dropped notice must be findable in the
                // trail rather than inferred from its absence.
                if let Err(undelivered) = reg2.deliver_to_orchestrator(
                    &group_id,
                    &format!(
                        "[orrerix] this group's workflow declares a manager block ({}), but its \
                         pane could not be opened: {e}. The human has no manager pane this \
                         session — take their input in your own pane, exactly as your base rules \
                         say.",
                        block.id
                    ),
                    brand::AUDIT_ACTOR,
                ) {
                    reg2.audit(&group_id, brand::AUDIT_ACTOR, "error", json!({
                        "what": "manager-degraded notice was not delivered to the orchestrator",
                        "block": block.id.clone(),
                        "err": undelivered,
                        "consequence": "the orchestrator was never told the manager is absent, so its not-live fallback will not have been triggered",
                    }));
                }
            }
        }
    };
    // `spawn_agent_ex` BLOCKS until the pane binds when a frontend is attached
    // (it emits `orch-spawn-request` and waits on `pending_binds`), and the
    // caller of this function is on the IPC thread that also serves that bind —
    // so running it inline there would deadlock until `BIND_TIMEOUT`. With no
    // frontend (tests) it returns synchronously, and running it inline is what
    // makes a launch fully observable from one call instead of racing a thread.
    if reg.app.lock_safe().is_none() {
        open();
    } else {
        std::thread::spawn(open);
    }
}

/// Register a group's orchestrator and hand back the pane spec the frontend
/// opens. `origin` decides whether the pane reopens a prior conversation (with
/// fresh MCP wiring either way) or starts cold, and which kickoff it is typed —
/// see [`SessionOrigin`]. A background thread waits for the pane bind, types
/// the kickoff/re-sync prompt, and brings up any initial idle workers.
fn register_orchestrator_pane(
    reg: &Arc<OrchRegistry>,
    group: &GroupInfo,
    origin: &SessionOrigin,
    initial_workers: Option<u32>,
) -> Result<SpawnRequest, String> {
    // The orchestrator is a block like any other (#222) — it just isn't spawned
    // through `spawn_agent_ex`, because a group has exactly one and it is minted
    // at launch. A workflow file may still give it a persona and its own
    // CLI/model.
    let block = group
        .guardrails
        .block_for(Role::Orchestrator)
        .cloned()
        .ok_or("this group's workflow declares no orchestrator block")?;
    let model = workflow::model_of(&block, &group.guardrails.agent_cli).to_string();
    // It must be a supported CLI; the launcher only offers supported ones and
    // the workflow parser rejects unknown ones, so an unknown value here is a
    // hand-edited group.json.
    let cli = workflow::cli_of(&block, &group.guardrails.agent_cli);
    if !SUPPORTED_CLIS.contains(&cli) {
        return Err(format!(
            "unsupported orchestrator CLI {cli:?} — supported: {}",
            SUPPORTED_CLIS.join(", ")
        ));
    }
    let cli = cli.to_string();
    // #407, fail-closed: a promoted pane resumes a conversation that belongs to
    // exactly ONE CLI, and a mismatch would compose `<other-cli> --resume
    // <claude uuid>` — a failure INSIDE the pane, after the promotion has
    // already killed the process that held the context.
    //
    // THE BACKSTOP, not the refusal (#407 rev-1 B1). `create_orchestration_group`
    // makes this same call read-only before `create_group_ex` runs, so in every
    // ordinary case a mismatch is refused with nothing created. This one is
    // re-derived against the roster that ACTUALLY resolved.
    //
    // WHY THE TWO CAN DISAGREE, as a class rather than a list (rev-2 N4): the
    // pre-check and the launch READ THE SAME INPUTS TWICE, and `creation` pins
    // none of them — it serializes loomux's own group creations, and that is
    // all. Every input that can change between the two reads is a way to
    // disagree. Known instances, and the list is deliberately not closed:
    //
    //   - **liveness, either direction.** The candidate id is picked by
    //     `group_is_live`, which is not under `creation`: an agent dying
    //     between the scans moves the launch onto an EARLIER candidate, one
    //     starting moves it onto a LATER one — a different group, so a
    //     different roster.
    //   - **`.loomux/workflow.yml`.** Read once here and once in
    //     `create_group_ex`, with no lock on the file. A `git pull` or an agent
    //     editing it in between resolves two different rosters — and this repo
    //     already treats mid-session edits of that file as a live, accepted
    //     reality (see #459 in `create_group_ex`'s resume arm), which makes
    //     this likelier than the liveness race, not rarer.
    //   - **the candidate's own `group.json`.** Likewise read twice, and a live
    //     toggle (`orch_set_advanced_orchestrator`, `set_max_agents`) rewrites
    //     it — so the roster, the advanced flag or the cap a reattach restores
    //     can differ between the two reads.
    //
    // None of these is behavioral: this check covers all of them, which is the
    // point of it existing. It is kept rather than trusting the pre-check alone
    // because the cost of being wrong is a dead pane holding the context the
    // whole gesture exists to preserve — and it stays the one refusal that can
    // leave a created/reattached group behind, the group state being durable
    // enough that the dormant-group Resume card is the way back in.
    if let SessionOrigin::Promote { cli: pane_cli, .. } = origin {
        if &cli != pane_cli {
            return Err(format!(
                "promote-cli-mismatch: this group's orchestrator block runs {cli}, but the pane \
                 being promoted is a {pane_cli} session — a conversation cannot be resumed under \
                 another CLI"
            ));
        }
    }
    let token = new_token();
    let agent_id = format!("orch-{}", reg.mint_agent_seq(&group.id));
    if cli == "copilot" {
        reg.pre_trust_copilot_folder(&group.repo, &group.repo);
    }
    // "Does the CLI get `--resume <id>`" — true for a promote as much as a
    // resume (#407); what a promote does NOT inherit is the resume kickoff,
    // which is `wants_full_kickoff` below, not this.
    let resume = origin.resumes_session();
    let session_id = match origin.session_id() {
        Some(s) => Some(sanitize_session(s).ok_or("invalid resume session id")?),
        // #2126: the capability, not the CLI name — see the same line in
        // `spawn_agent_ex`.
        None => premints_session_id(&cli).then(new_session_uuid),
    };
    // A CLI that mints its own id on boot; snapshot existing
    // sessions now so the orchestrator's newly created one can be tracked
    // (this is what gives such an orchestration its ORCH chip and restore).
    let session_baseline =
        (!resume).then(|| reg.capture_session_baseline(&cli, &group.id)).flatten();
    // The orchestrator block's persona, if the workflow file gave it one. A
    // broken one is audited and dropped, never fatal.
    let persona = reg.resolve_persona_or_audit(group, &block);
    // #416: read back the instructions file `create_group_ex` already wrote
    // (via `write_instruction_files`) rather than re-deriving it — this path
    // has no `vars` in scope, and the file on disk is the one source of
    // truth the kickoff/reinjection paths already trust for this exact
    // reason (see `compact_reinjection_notice`'s doc).
    let instructions_body = fs::read_to_string(reg.group_dir(&group.id).join(block.instructions_file()))
        .unwrap_or_default();
    let contract = block_contract_text(&instructions_body, persona.as_ref());
    let inject = reg.persona_inject(&group.id, &block, &cli, persona.as_ref(), &contract);
    // An orchestrator is `Containment::None` — nothing to deny; the parameter
    // exists for the CLIs whose deny rules live in the config file (#267).
    //
    // **Written AFTER `persona_inject`, matching `spawn_agent_ex` (#722):**
    // opencode's generated document carries the block's agent entry, and
    // `persona_inject` is what writes the file that entry references. This used
    // to sit above the session/persona block; nothing between there and here
    // reads `cfg`, so moving it down is a reordering only — and it makes the
    // two spawn sites agree on the order structurally instead of by accident.
    //
    // Both arguments are DERIVED, never hand-passed, for the same reason the
    // builders below already derive theirs: an orchestrator is
    // `Containment::None` today, but a literal here would be a second place
    // that has to be remembered if `Role::containment` ever changes — and the
    // unattended predicate must stay the one expression
    // `build_agent_command_ex` recomputes internally, or a pane could get a
    // document saying one thing and an argv saying the other.
    let containment = block.kind.containment();
    let cfg = reg.write_mcp_config(
        &group.id,
        &agent_id,
        &token,
        &cli,
        Path::new(&group.repo),
        containment,
        group.guardrails.auto_ops || containment.forces_unattended(),
        // #2515 C1 — the same `block.knobs()` the builders below take, for the
        // same reason `containment` above is derived rather than hand-passed.
        block.knobs(),
        &inject,
    )?;
    // #417, split from `cfg` per rev-4 review N2 — Claude's hook config rides
    // a per-agent `--settings` file; Copilot's own wiring needs no flag.
    let hook_settings = (cli == "claude")
        .then(|| {
            // #925: the settings file is named after the agent.
            let agent_seg = PathSegment::parse(&agent_id).ok()?;
            reg.write_hook_settings_file(&group.id, &agent_seg, containment, Path::new(&group.repo))
        })
        .flatten();
    if cli == "copilot" {
        let _ = reg.ensure_copilot_compact_hook();
    }
    let command = reg.build_agent_command_ex(
        &cli,
        &model,
        // #687: the orchestrator block may pin these too — a value-set pick,
        // so it joins `cli`/`model` on the allowed-to-pin list rather than
        // `prompt:`/`allow:` on the refused one (see `parse_workflow`).
        block.knobs(),
        group.guardrails.auto_ops,
        &cfg.path,
        hook_settings.as_deref(),
        &reg.group_dir(&group.id),
        Path::new(&group.repo),
        session_id.as_deref(),
        resume,
        // Derived from the class, never hand-passed: the orchestrator is
        // `Containment::None`, and if that ever changes it changes in one
        // `match` (`Role::containment`) rather than at a literal here.
        containment,
        &inject,
        // #946 Q4 / #1091 slice H: this spawn site only ever builds the
        // orchestrator's own pane (see `role: Role::Orchestrator` on the
        // entry below) — `Role::Orchestrator` alone already satisfies
        // `claude_denies_interactive_question`, so `role_hint` is `None`
        // rather than threaded from `block` the way `spawn_agent_ex` does.
        Role::Orchestrator,
        None,
        // Never a fork: an orchestrator is refused as a fork source (#3318
        // F2), so this `Ok` is structural — `?` only because the signature
        // is shared with the path that can fork.
        None,
    )?;
    let argv = reg.build_agent_argv_ex(
        &cli,
        &model,
        block.knobs(),
        group.guardrails.auto_ops,
        &cfg.path,
        hook_settings.as_deref(),
        &reg.group_dir(&group.id),
        Path::new(&group.repo),
        session_id.as_deref(),
        resume,
        containment,
        &inject,
        Role::Orchestrator,
        None,
        None,
    )?;
    // Round #417 correction 6: see `command_line_length_guard`'s doc — the
    // orchestrator's own pane must fail loudly pre-spawn too, not just
    // worker/reviewer/planner panes through `spawn_agent_ex`.
    command_line_length_guard(&argv)?;
    let entry = AgentEntry {
        id: agent_id.clone(),
        group: group.id.clone(),
        name: "orchestrator".into(),
        // A stable, meaningful single-orchestrator label — treated as the
        // id-default tier so it never blocks anything (the rename tool targets
        // worker/reviewer panes, not the orchestrator).
        name_source: NameSource::Default,
        block: block.id.clone(),
        role: Role::Orchestrator,
        token: token.clone(),
        status: AgentStatus::Starting,
        pty_id: None,
        pane_id: None,
        pane_kind: None,
        forked_from: None,
        task: String::new(),
        task_id: None, // the group’s own orchestrator is never spawned against a board row
        session_id,
        cwd: group.repo.clone(),
        branch: None, // the orchestrator works on the repo's own checkout, not a branch
        idle_since_ms: None, // the orchestrator is never idle-reaped
        started_ms: now_ms(),
        last_progress_ms: now_ms(), // watchdog ignores the orchestrator; the
        // idle-tick (#83) reuses this as the orchestrator's output-quiet clock.
        // #535: role-agnostic, unlike the two clocks above — an orchestrator
        // gets re-grounded too. `0` = has never called an MCP tool.
        last_mcp_activity_ms: 0,
        last_output_progress_ms: now_ms(), // #496: the output-only half of that clock
        last_output_total: 0,
        watchdog_notified: false,
        watchdog_watch_suppressed: false,
        watchdog_drive_suppressed: false,
        idle_tick_notified: false,
        compact_nudge_notified: false,
        compact_nudge_last_output_total: 0,
        compact_requested: false,
        compact_pending: false,
        compact_seen_busy: false,
        compact_pending_baseline_tokens: None,
        compact_pending_baseline_marker_count: None,
        compact_pending_trusted: false,
        compact_reinject_attempted_ms: None,
        compact_reinject_attempts: 0,
        compact_reinject_busy_deferred: false,
        compact_pending_armed_ms: None,
        compact_last_lost_reason: None,
        compact_last_lost_ms: None,
        compact_last_ack: None,
        compact_last_ack_ms: None,
        last_context_tokens: None,
        last_context_model: None,
        last_context_window: None,
        last_context_window_rounded: false,
        last_context_effort: None,
        last_context_source: None,
        compact_inference_guard_until_ms: 0,
        compact_hook_precompact_seen_ms: None,
        compact_hook_sessionstart_seen_ms: None,
        compact_hook_postcompact_seen_ms: None,
        compact_hook_postcompact_first_seen_ms: None,
        compact_pending_evidence: None,
        compact_hook_native_notice_delivered: false,
        contract_carrier: inject.contract_carrier,
        last_state_write_ms: 0,
        compact_escalation_notified: false,
        cache_idle_nudge_latched: false,
        idle_tick_skip_rearm_ms: 0,
        solo_cli: None,
        last_exit_tail: None,
        killed_by: None,
    };
    reg.agents.lock_safe().insert(agent_id.clone(), entry.clone());
    reg.by_token.lock_safe().insert(token, agent_id.clone());
    reg.persist_agent_record(&entry, "running");
    reg.audit(&group.id, brand::AUDIT_ACTOR, "agent-spawn",
        json!({ "agent": agent_id, "role": "orchestrator", "model": model,
                // `resume` is kept (readers parse it) but is no longer the
                // whole answer: a promote resumes too (#407). `origin` is.
                "session": entry.session_id, "resume": resume,
                "origin": origin.as_str() }));

    let request = SpawnRequest {
        group_id: group.id.clone(),
        agent_id: agent_id.clone(),
        role: Role::Orchestrator,
        name: "orchestrator".into(),
        cwd: group.repo.clone(),
        command,
        // Expire the request when the background bind wait below would (#106).
        deadline_ms: now_ms() + BIND_TIMEOUT.as_millis() as u64,
        argv,
        // The orchestrator is the pane the incident implicated — inject the
        // gh-shim env so its merge gate is enforced too (#83), plus
        // the agent-id variable (#417) and the adapter's own vars (#267).
        env: {
            let mut e = reg.agent_pane_env(&group.id, &agent_id);
            e.extend(cfg.env.clone());
            e
        },
        // The orchestrator's own pane is never minimized (#260) — see
        // `spawn_opens_minimized`, called here too so both `SpawnRequest` sites
        // agree on the rule structurally, not by two independently-written `false`s.
        minimized: spawn_opens_minimized(Role::Orchestrator, reg.spawn_expanded(&group.id)),
    };

    crate::obs::breadcrumb(
        "agent-spawn",
        &format!(
            "group={} agent={agent_id} role=Orchestrator resume={resume} origin={}",
            group.id,
            origin.as_str()
        ),
    );

    // #1161 M3: the manager pane opens HERE — at launch, beside the
    // orchestrator's own pane and off its own declaration, not from anything
    // the orchestrator does. Placed above the test-mode return (rather than in
    // the bind thread below, where the initial workers go) for two reasons: it
    // must not be sequenced behind the orchestrator's bind, since it is not the
    // orchestrator's delegate and does not wait on it; and a group launched
    // with no frontend must still open it, so a test can observe a whole launch
    // from one synchronous call. Neither pane waits on the other — what this
    // guarantees is that the manager is REQUESTED at launch, not that it binds
    // before the orchestrator's kickoff lands.
    open_manager_pane_at_launch(reg, group, origin);
    if reg.app.lock_safe().is_none() {
        // Test mode: no frontend; mark running without a pane. Tolerate a
        // vanished entry rather than unwrapping under the agents lock.
        if let Some(a) = reg.agents.lock_safe().get_mut(&agent_id) {
            a.status = AgentStatus::Running;
        }
        return Ok(request);
    }

    // Background: wait for the orchestrator pane to bind, type its kickoff,
    // then bring up the initial idle workers one by one.
    let (tx, rx) = mpsc::channel::<u32>();
    reg.pending_binds.lock_safe().insert(agent_id.clone(), tx);
    let reg2 = reg.clone();
    let group2 = group.clone();
    // Moved into the bind thread: the kickoff-injection fallback for an
    // orchestrator block whose persona can't ride on a native flag (copilot +
    // an inline `prompt:`). `None` for the built-in roster.
    let kickoff_persona = inject.kickoff.clone();
    // The two kickoff decisions, resolved here (the thread outlives `origin`'s
    // borrow) — and deliberately NOT the same bool: a promoted session resumes
    // AND gets the full contract, because it has never seen one (#407).
    let full_kickoff = origin.wants_full_kickoff();
    let kickoff_origin = match origin {
        SessionOrigin::Promote { .. } => KickoffOrigin::Promoted,
        _ => KickoffOrigin::Normal,
    };
    std::thread::spawn(move || {
        let Ok(pty_id) = rx.recv_timeout(BIND_TIMEOUT) else {
            reg2.pending_binds.lock_safe().remove(&agent_id);
            reg2.mark_dead(&agent_id, None);
            // Cancel the queued request frontend-side (#106) — see spawn_agent_ex.
            reg2.emit_spawn_cancelled(&group2.id, &agent_id);
            return;
        };
        {
            let mut agents = reg2.agents.lock_safe();
            if let Some(a) = agents.get_mut(&agent_id) {
                a.status = AgentStatus::Running;
                a.pty_id = Some(pty_id);
            }
        }
        reg2.by_pty.lock_safe().insert(pty_id, agent_id.clone());
        reg2.audit(&group2.id, brand::AUDIT_ACTOR, "agent-bind", json!({ "agent": agent_id, "pty": pty_id }));
        crate::obs::breadcrumb("agent-bind", &format!("agent={agent_id} pty={pty_id} role=Orchestrator"));
        // #467: the restart's own recovery point. This is the pane the
        // group's queued-to-orchestrator deliveries (worker reports, loomux
        // notices) re-bind to — and it runs BEFORE the restore kickoff
        // below, so a pre-restart delivery still precedes it.
        reg2.readmit_recovered(&group2.id, &agent_id, pty_id);
        let kickoff = if !full_kickoff {
            // #411: an app restart is exactly the surprise discontinuity the
            // directive ledger exists to survive, but this fixed string used
            // to embed nothing from it — a directive noted before the
            // restart had no durable channel back in, even though the SAME
            // ledger file `compact_reinjection_notice` already reads back on
            // a real compaction sat right there on disk.
            // #925: an id that cannot name a file yields an empty path, which
            // `read_to_string` then fails on into the same `unwrap_or_default`
            // degrade an absent ledger already takes.
            let ledger_path = PathSegment::parse(&agent_id)
                .map(|agent_seg| reg2.ledger_path(&group2.id, &agent_seg))
                .unwrap_or_default();
            let ledger = fs::read_to_string(&ledger_path).unwrap_or_default();
            let ledger_embed = directive_ledger_embed(
                &ledger, DIRECTIVE_LEDGER_EMBED_CAP_BYTES, &ledger_path.display().to_string(),
            );
            resume_kickoff_notice(ledger_embed.as_deref())
        } else {
            match reg2.agent(&agent_id) {
                Some(a) => reg2.kickoff_prompt_ex(
                    &a,
                    &group2,
                    "",
                    kickoff_persona.as_deref(),
                    kickoff_origin,
                ),
                None => return, // agent reaped before bind; nothing to kick off
            }
        };
        // #407. `FreshKickoff` rather than `ResumeKickoff` for the one thing
        // that actually separates them: `recovers_lost_kickoff` (#517). Both
        // kickoff kinds hold until the pane has painted and both answer
        // copilot's autopilot dialog, so neither of those picks between them —
        // but a `ResumeKickoff` is not re-delivered when it turns out never to
        // have landed, on the argument that its payload is a re-sync notice
        // re-derivable from durable state. A promoted orchestrator's kickoff is
        // the opposite: it exists nowhere else, nothing will re-send it, and a
        // session that silently never received it is a pane holding an hour of
        // context with no idea it is now an orchestrator.
        let delivery = if full_kickoff { Delivery::FreshKickoff } else { Delivery::ResumeKickoff };
        let _ = reg2.deliver_prompt(&agent_id, &kickoff, brand::AUDIT_ACTOR, delivery);
        // Track the session this orchestrator just minted.
        if let Some(baseline) = session_baseline {
            reg2.clone().spawn_session_watcher(
                agent_id.clone(),
                group2.id.clone(),
                group2.repo.clone(),
                baseline,
            );
        }
        // A starter-worker count assumes the group HAS a worker block. A repo
        // whose `.loomux/workflow.yml` declares only reviewers (a review-only
        // workflow) has none (#222) — and then every spawn below would fail
        // with "declares no worker block", the human would get zero panes, and
        // the only trace would be an audit line they'd have to go looking for.
        // Say it out loud in the orchestrator's pane instead.
        //
        // Reachable only from a caller that ASKS for starters, which since
        // #1020 is the promote modal and not the launcher — the launcher sends
        // no count and `starter_workers` resolves that to 0, so this branch
        // sits under `starters > 0` and simply never fires for a launch.
        let starters = starter_workers(initial_workers, group2.guardrails.max_agents);
        if starters > 0 && group2.guardrails.block_for(Role::Worker).is_none() {
            reg2.audit(&group2.id, brand::AUDIT_ACTOR, "initial-workers-skipped", json!({
                "requested": starters,
                "why": "this repo's workflow declares no worker block",
            }));
            let _ = reg2.deliver_to_orchestrator(
                &group2.id,
                &format!(
                    "[orrerix] this launch asked for {starters} initial worker(s), but this repo's \
                     {} declares no worker block — none were opened. Spawn the blocks it does \
                     declare instead (they are listed above).",
                    active_workflow_path(&group2.repo, &group2.guardrails)
                ),
                brand::AUDIT_ACTOR,
            );
        } else {
            for _ in 0..starters {
                // Empty name → derived from the minted id ("worker 2" for `w-2`),
                // so the pane title agrees with its "W 2" badge instead of the old
                // per-launch counter that drifted from the seq (#95r).
                if let Err(e) = reg2.spawn_agent(&group2.id, Role::Worker, "", "", false, None)
                {
                    reg2.audit(&group2.id, brand::AUDIT_ACTOR, "error",
                        json!({ "what": "initial worker spawn failed", "err": e }));
                    break;
                }
            }
        }
    });

    Ok(request)
}

/// Restore orchestration for a recorded session id (from the session
/// browser). An orchestrator session of a dead group relaunches the whole
/// control plane — group, MCP identity, task board — resuming that
/// conversation, and returns the pane spec for the frontend to open. A
/// worker/reviewer session rejoins its live group; its pane arrives via the
/// normal orch-spawn-request event (the spawn must not block this IPC
/// thread, which also serves the bind), so `None` is returned.
///
/// THE GROUP A SESSION REJOINS IS ITS OWN, ALWAYS (#485). `hint` names the
/// group the CALLER believes this session belongs to; the group it actually
/// rejoins is the one its own record names. When the two disagree this
/// refuses the resume outright (`resume-group-mismatch:`) rather than
/// quietly resolving the disagreement in either direction — see the check
/// below for why silently preferring the record is not good enough.
pub fn resume_recorded_session(
    reg: &Arc<OrchRegistry>,
    session_id: &str,
    // #904: the group half is a `GroupId` — this hint originates in a
    // TRANSCRIPT signature (`sessions::detect_orch_signature`), i.e. text an
    // agent CLI wrote, and it is joined onto the orchestration root below.
    hint: Option<(GroupId, String)>, // (group_id, role) from transcript signatures
    start_fresh: bool,
) -> Result<Option<SpawnRequest>, String> {
    // Kept before the resolution chain below consumes `hint`.
    let hinted_group = hint.as_ref().map(|(g, _)| g.clone());
    let record = hint
        .as_ref()
        // #479: the dormant-group Resume button (and any other caller that
        // already knows which group a session belongs to) names it via
        // `hint` — try that ONE group first rather than unconditionally
        // paying for every OTHER group's group.json/tasks.json/full audit
        // log, which `session_roles()` below pays for regardless of whether
        // its result is used. A miss (wrong/stale hint) falls through to the
        // full scan. This is a genuinely DIFFERENT tie-break from the full
        // scan's, not merely a faster path to the same answer — see
        // `session_role_in_group`'s doc comment for the #485 corner (a
        // session id recorded in more than one group) where the two can
        // resolve to different groups.
        .and_then(|(group_id, _)| reg.session_role_in_group(group_id, session_id))
        .or_else(|| {
            reg.session_roles()
                .into_iter()
                .filter(|r| r.session_id == session_id)
                .last()
        })
        .ok_or(())
        .or_else(|()| {
            // SIGNATURE-ONLY FALLBACK. Sessions from before the roster (and
            // before session-id tracking) have no row anywhere; the session
            // browser identifies them by loomux signatures in their own
            // transcript and names the group via `hint`.
            //
            // #485 review finding 1: this fallback builds its record FROM the
            // hint, so the mismatch check below is vacuous by construction for
            // this class — the caller's claim is the only evidence there is.
            // That is fine for an ORCHESTRATOR (reopening the control plane of
            // a group whose `group.json` is on disk is not a membership
            // operation: the group's identity comes from that file, and no
            // other group's roster is touched), and it is NOT fine for a
            // DELEGATE, where "rejoin into group X" writes membership into X
            // on nothing but the caller's say-so. That was the one route left
            // into the wrong group after the check below: a pre-#485 snapshot
            // whose tab-derived hint names group A while the placeholder is
            // really group B's pre-roster worker. So a record-less delegate is
            // REFUSED — loudly, with its own tag, since "cannot be verified"
            // is a different fact from "contradicted" (the check below) and
            // deserves different copy. It costs the pre-roster delegate rejoin
            // in a legacy tab, which the human can still reach through the
            // session browser or have the orchestrator respawn; the trade is
            // deliberate, and #485's whole point is that a silent wrong-group
            // rejoin is worse than a legible refusal.
            let (group_id, role) = hint.ok_or("this session is not part of a recorded orchestration")?;
            if role != "orchestrator" {
                // THE ESCAPE ROUTE HAS TO EXIST (#485 review round 2). This
                // said "resume it from the session browser", which is
                // circular: the browser classifies a pre-roster session from
                // its transcript SIGNATURE (`SessionsPanel.roleFor`'s
                // `orch_role`/`orch_group` fallback), so clicking it there
                // hints the same group and lands right back on this line. A
                // fresh spawn is not an escape either — it would join the very
                // group that could not be verified. So this names what is
                // actually reachable, and says plainly that a group rejoin is
                // not among it.
                return Err(format!(
                    "resume-group-unknown: session {session_id} has no recorded orchestration \
                     membership on this machine, so the group it belongs to cannot be verified — \
                     refusing to rejoin it into {group_id} on the caller's say-so. Nothing can \
                     rejoin this session INTO a group: the session browser classifies it from its \
                     transcript alone and returns here, and a fresh spawn would join the \
                     unverified group. What does work: the orchestrator can spawn a fresh agent \
                     for the work, and the conversation itself is not lost — it reopens OUTSIDE \
                     orchestration via the CLI's own resume command (shown in the session row's \
                     tooltip), as a plain pane with no group membership."
                ));
            }
            if !reg.group_dir(&group_id).join("group.json").is_file() {
                return Err("this session is not part of a recorded orchestration".to_string());
            }
            let group_live = reg.group_is_live(&group_id);
            Ok(SessionRole {
                session_id: session_id.to_string(),
                agent_name: "orchestrator".into(),
                group_id,
                role,
                group_live,
                // Signature-only fallback (no roster row, no audit line to
                // read): none of #1's metadata is derivable, so it's honestly
                // absent rather than guessed.
                task: String::new(),
                branch: None,
                repo: None,
                pr: None,
                forked_from: None,
            })
        })?;

    // THE JOIN POINT (#485). Every rejoin in loomux funnels through here, so
    // this is the one place that can make "a session joins a group that isn't
    // its own" unreachable instead of merely unlikely — and one caller getting
    // its group id from a TAB rather than from the session's own record (the
    // two-groups-one-tab resume this closes) is exactly how that used to
    // happen.
    //
    // EXACTLY WHAT THIS CHECK COVERS, stated narrowly on purpose (#485 review
    // finding 1): a session that HAS a recorded membership — a roster row or an
    // audit line in some group — can never be rejoined into a different one,
    // because `record` above came from that recording and this compares it
    // against what the caller asked for. A session with NO recording anywhere
    // has nothing to compare against; that class is refused outright by the
    // signature-only fallback above (delegates) rather than covered here, so
    // that "cannot be verified" never resolves to "trust the caller". Between
    // the two, no delegate rejoin proceeds on a group id that only the caller
    // vouches for. What neither closes is stale DATA: rows a pre-#485 rejoin
    // already wrote into the wrong group (see below).
    //
    // Why refuse instead of silently using `record.group_id` — which is what
    // the spawn below would do anyway, and would already land the agent in
    // the right group? Because the caller acts on its own belief afterwards:
    // it binds panes, routing and badges to the group it ASKED for, so a
    // disagreement resolved in silence still ends with the agent's pane
    // filed under the wrong group in the UI. The two ids disagreeing means
    // the caller's model of the world is wrong, and the only safe move is to
    // say so where the human clicked. Tagged like every other resume failure
    // (`resumeerror.ts` parses the `resume-<tag>:` prefix) so the frontend
    // can report it specifically; deliberately NOT a `start fresh`-able kind
    // — minting a fresh session would just create the contamination the
    // refusal prevented.
    //
    // A hintless caller (the session browser resolves the group from the
    // session record itself) has nothing to disagree with and is unaffected.
    //
    // WHAT THIS DOES NOT UNDO: a session that ALREADY has a roster/audit row
    // in a second group — the residue a pre-#485 wrong-group rejoin left on
    // disk — resolves through `session_role_in_group`'s hinted-group fast
    // path to that hinted group, agrees with itself, and passes. This check
    // stops new contamination from being created; it does not clean up old
    // contamination, and nothing here should be read as claiming it does.
    if let Some(hinted) = hinted_group.as_deref() {
        if record.group_id != hinted {
            return Err(format!(
                "resume-group-mismatch: session {session_id} belongs to orchestration group \
                 {}, not {hinted} — refusing to rejoin it into another group",
                record.group_id
            ));
        }
    }

    // #2519 — NOTHING IN A LEAD GROUP CAN BE RESUMED, and the refusal is above
    // both branches below because it is true of both.
    //
    // A lead group has no orchestrator, so the orchestrator branch has nothing to
    // reopen; and its root is a HUMAN pane that orrerix did not launch and cannot
    // relaunch, so rejoining one of its children would put a worker into a group
    // whose `report` resolves no root at all — a delegate typing into nothing, on
    // a branch and worktree whose owner is gone. That is the restore residual
    // `docs/design/lead-pane.md` records, and this is the message the session
    // browser shows in its place.
    //
    // Decided by the group MARKER, not by the roster: `read_blocks` drops a
    // persisted `kind: "lead"` row (see `LEAD_MARKER`), so a roster read back off
    // disk would answer "not a lead group" for every lead group there has ever
    // been. Tagged like every other resume failure (`resumeerror.ts` parses the
    // `resume-<tag>:` prefix) and deliberately NOT `start fresh`-able: a fresh
    // session would join the same rootless group.
    if reg.is_lead_group(&record.group_id) {
        return Err(format!(
            "resume-lead-group: session {session_id} belongs to {}, a group opened by the \
             orrerix-subagents toggle on someone's own agent pane. That pane is the group's \
             root and orrerix never launched it, so there is nothing here to resume into: a \
             rejoined helper would have no lead to report to. Turn the toggle on again in a \
             fresh pane to open a new lead, and brief a new helper — the conversation itself \
             is not lost, and reopens outside orchestration through the CLI's own resume \
             command (shown in the session row's tooltip), as a plain pane with no group \
             membership.",
            record.group_id,
        ));
    }

    if record.role == "orchestrator" {
        // #799: this liveness pre-check runs OUTSIDE the `creation` lock (the
        // only `creation.lock_safe()` is inside `create_group_ex`), so what it
        // reads can be stale by the time the pane actually spawns — a teardown
        // racing a resume can validate here against a group that is gone a
        // moment later, and two restores of one session can both pass it. That
        // is pre-existing, but #762 made it reachable in a way it was not when
        // these commands could not overlap: see `resume_orch_session`'s
        // **Reentrancy.** paragraph for the argument, and #799 for the guard
        // shape (re-check under `creation`, or a liveness token the spawn
        // re-validates). Documented here rather than fixed — the fix is
        // lifecycle locking, not dispatch.
        if record.group_live {
            return Err(format!(
                "group {} already has a live orchestrator — focus its pane instead",
                record.group_id
            ));
        }
        let (repo, guardrails) = reg
            .load_group_file(&record.group_id)
            .ok_or("group.json is missing for this orchestration")?;
        // Existence-only pre-check (#412 review B1): the orchestrator's
        // launch cwd is always `repo`, fixed — never a worktree — so #412's
        // confirmed CWD-MISMATCH failure mode can't arise for it the same
        // way as the worker/reviewer path below, and this never swaps the
        // launch cwd. But the EXISTENCE failure mode (the session cleared
        // from the CLI's history, or the store unreadable, while loomux's
        // own roster still names it) is exactly as reachable for an
        // orchestrator as for a worker — skipping it here left #412's
        // TITULAR bug (a cold-started orchestration pane that fails inside
        // with no steering box) open, and made `start_fresh` unreachable
        // dead code for this branch, since nothing here ever returned a
        // tagged error for `resumeFailureKind` to recognize.
        if !start_fresh {
            let cli = guardrails
                .block_for(Role::Orchestrator)
                .map(|b| workflow::cli_of(b, &guardrails.agent_cli).to_string())
                .unwrap_or_else(|| guardrails.agent_cli.clone());
            // Via the store router, not `sessions::find_session_cwd` directly
            // (#722): its `_` arm searches CLAUDE's projects directory for
            // every CLI it doesn't name, so an opencode orchestrator resume
            // used to fail here — "not found in the opencode session history"
            // after looking somewhere else entirely — and no opencode group
            // could be reopened at all.
            let db = reg.opencode_db_path(&record.group_id);
            let pi = reg.pi_sessions_dir(&record.group_id);
            match session_cwd_in_store(&cli, session_id, Some(&db), Some(&pi)) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err(format!(
                        "resume-not-found: session {session_id} was not found in the {cli} \
                         session history on this machine — it may have been cleared, or the \
                         record is stale. Start a fresh orchestrator for this group instead."
                    ));
                }
                Err(e) => {
                    return Err(format!(
                        "resume-store-unreadable: could not read the {cli} session store: {e}"
                    ));
                }
            }
        }
        // BOTH arms carry Resume GROUP semantics (#412 rev-17 blocker),
        // regardless of `start_fresh`: this is reopening an EXISTING,
        // previously-launched group either way — a fresh CONVERSATION on it
        // (start_fresh) is not a fresh LAUNCH of it. Spelling that used to
        // require remembering to pass `Launch::Resume` alongside a `None`
        // session id, and a caller that read "no session id" as "fresh launch"
        // would re-read `.loomux/workflow.yml` and silently swap the group's
        // roster (and could delete its merge-gate spec) to whatever the repo
        // file currently says — the exact provenance violation the resume path
        // exists to prevent. `SessionOrigin::StartFresh` is that combination,
        // named (#407), so it can no longer be spelled by omission.
        let origin = if start_fresh {
            SessionOrigin::StartFresh
        } else {
            SessionOrigin::Resume(session_id.to_string())
        };
        return create_orchestration_group(
            reg,
            &repo,
            guardrails,
            origin,
            Some(&record.group_id),
            // A restore re-opens a group that already has whatever workers it
            // had; nobody is asking for starters, which is what `None` says.
            // (`Some(0)` would have been the same number and the wrong claim.)
            None,
        )
        .map(Some);
    }

    // Worker / reviewer: only meaningful inside a live group.
    if !record.group_live {
        return Err(
            "this agent's group is not running — restart its orchestrator session (marked ORCH) first"
                .into(),
        );
    }
    // #222: an unrecognized role is REJECTED, not silently coerced to worker.
    // This was the second of the two coercion sites (the other was the MCP
    // `spawn_agent` kind parser); a persisted role loomux cannot name means the
    // roster row is corrupt or from a future build, and rejoining it as a worker
    // would hand it a worktree and write access on nothing but a guess.
    let role = workflow::kind_from_str(&record.role).ok_or_else(|| {
        format!(
            "this session's recorded role {:?} is not a known capability class ({}) — refusing to rejoin it",
            record.role,
            workflow::kind_names()
        )
    })?;
    // Pull the durable roster row for this session: its cwd (where the work
    // happened) and its name tier — so a human-renamed pane rejoins at the
    // `Human` tier and stays un-clobberable, not silently demoted to
    // orchestrator (#95r). Absent (hint-restored, pre-roster) → `None`, and
    // spawn derives the tier from the name as usual.
    // The first roster row naming this session — this rejoin has always read
    // it that way, and since #1961 it says so by calling the function that
    // owns the rule rather than by open-coding a `find` that reads like an
    // accident beside the MCP arm's `max_by_key`.
    let matched = reg.session_identity_record(&record.group_id, session_id);
    let restore_source = matched.as_ref().map(|r| r.name_source);
    let reg2 = reg.clone();
    let sid = session_id.to_string();
    let (group_id, name) = (record.group_id.clone(), record.agent_name.clone());
    // Rejoin as the same BLOCK, not just the same class (#222) — a resumed
    // `rev-security` session must come back with its persona, not as a generic
    // reviewer. Absent (a roster row from before blocks) → `None` → the class's
    // default block, which for the built-in roster is the same thing.
    //
    // A recorded block that is no longer in the roster (the workflow file renamed
    // or dropped it since that session ran) degrades to `None` — the class default
    // — rather than failing the rejoin. Losing the persona is a downgrade; losing
    // the *session* is data loss, and the human has no other way to reach it.
    // `spawn_agent_ex` is deliberately strict about an unknown block id, because
    // for `spawn_agent(block:)` a typo should be an error — so the fallback has to
    // happen here, where "stale" and "wrong" are distinguishable. (`kickoff_prompt`
    // already falls back the same way for the instructions path.)
    // #1161 M3: a MANAGER rejoins by CLASS, never by block id — the one
    // deliberate exception to the rejoin-as-the-same-block rule above, and it
    // costs nothing, because a group has at most one manager block
    // (`workflow::MANAGER_MAX`) so its class default IS its recorded block.
    // `spawn_agent_bound` refuses a NAMED manager block, which is what closes
    // the agent-reachable spawn and bare-resume routes; loomux's two own
    // openers (this one and `open_manager_pane_at_launch`) pass `None` so the
    // refusal can stay unconditional on the shape rather than being softened
    // into a guess about who is calling. This entry point is a Tauri command
    // driven by a human clicking a row in the session browser — the trusted
    // webview, not an agent — and the singleton re-check inside
    // `spawn_agent_bound` is what stops it opening a second live one.
    let block = matched
        .as_ref()
        .filter(|_| role != Role::Manager)
        .map(|r| r.block.clone())
        .filter(|b| !b.trim().is_empty())
        .filter(|b| {
            let known = reg
                .group(&record.group_id)
                .is_some_and(|g| g.guardrails.block(b).is_some());
            if !known {
                reg.audit(&record.group_id, brand::AUDIT_ACTOR, "rejoin-block-missing", json!({
                    "session": session_id, "block": b,
                    "action": "rejoining as the default block for its capability class",
                }));
            }
            known
        });
    // Resolve the launch cwd SYNCHRONOUSLY, before the background spawn below,
    // and authoritatively (#412): a stale/missing roster cwd (a moved or
    // deleted worktree) must fail the resume LOUDLY, back to the human who
    // clicked it, right now — not silently, several audit lines deep in a
    // background thread whose only trace is a message in the orchestrator's
    // own pane (which nobody may be looking at). `start_fresh` skips all of
    // this: a fresh spawn cuts its own new worktree and needs no prior cwd.
    let (resume_session, cwd_override, use_worktree, task) = if start_fresh {
        // #412 review N6: name the OLD branch when known — the worktree is
        // gone, but a `workspace-missing` resume almost always means the
        // BRANCH still holds the prior work (the worktree was merely
        // removed, e.g. after a merge, or cleared independently of the
        // branch itself), and a fresh agent with no idea that branch exists
        // is liable to redo it from scratch.
        let task = match &record.branch {
            Some(b) if !b.trim().is_empty() => format!(
                "{}\n\n(Restarted fresh after its previous session/worktree became unresumable. \
                 Its prior work may still be on branch '{b}' — check it before redoing anything.)",
                record.task
            ),
            _ => record.task.clone(),
        };
        (None, None, true, task)
    } else {
        let group = reg.group(&record.group_id).ok_or("group vanished during resume")?;
        let resolved_block = match block.as_deref() {
            Some(id) => group.guardrails.block(id).cloned(),
            None => group.guardrails.block_for(role).cloned(),
        };
        let cli = resolved_block
            .map(|b| workflow::cli_of(&b, &group.guardrails.agent_cli).to_string())
            .unwrap_or_else(|| group.guardrails.agent_cli.clone());
        // #3443: a dead reviewer's scratch worktree was reclaimed; cut it
        // again at the recorded path before the resume reads it. A no-op for
        // anything else, a worker's worktree included.
        if let Some(r) = matched.as_ref() {
            reg.restore_reviewer_scratch_worktree(&record.group_id, &r.cwd);
        }
        let cwd = resolve_worker_resume_cwd(
            &cli,
            session_id,
            matched.as_ref().map(|r| r.cwd.as_str()),
            &group.repo,
            Some(&reg.opencode_db_path(&group.id)), // #722: this group's store
            Some(&reg.pi_sessions_dir(&group.id)), // #2126: and this one
        )?;
        (Some(sid.clone()), Some(cwd), false, String::new())
    };
    std::thread::spawn(move || {
        if let Err(e) = reg2.spawn_agent_ex(
            &group_id, role, block, &name, &task, use_worktree, None, None, resume_session, cwd_override,
            restore_source,
        ) {
            reg2.audit(&group_id, brand::AUDIT_ACTOR, "error",
                json!({ "what": "session rejoin failed", "session": sid, "err": e.clone() }));
            let _ = reg2.deliver_to_orchestrator(
                &group_id,
                &format!("[orrerix] failed to resume session {sid} into this group: {e}"),
                brand::AUDIT_ACTOR,
            );
        }
    });
    Ok(None)
}

#[tauri::command]
pub fn bind_agent(reg: tauri::State<Arc<OrchRegistry>>, agent_id: String, pty_id: u32) -> Result<(), String> {
    OrchRegistry::mutating_command("bind_agent", || Err(COMMAND_REFUSED.to_string()), || {
        reg.bind(&agent_id, pty_id)
    })
}

/// The human renamed an agent pane in-place (F2 / double-click). Sync the
/// backend so the roster name matches the pane title AND the rename is
/// recorded at the highest precedence tier — an orchestrator `rename_agent`
/// afterwards will not override it (#95r). Best-effort: the pane already shows
/// the new name locally, so a stale/unknown id just fails silently here.
/// Off-thread (#762 — see [`run_blocking`]): `persist_agent_record` rewrites
/// `agents.json` while holding the global `tasks_lock` — the lock the board
/// family and the usage poll contend on. That lock's architecture is #747; this
/// is the dispatch half.
///
/// **Reentrancy.** `rename_agent` has always been reachable from MCP threads
/// (the orchestrator's own `rename_agent` tool) and from the spawn path, so the
/// webview thread was never what made it safe. The decision is taken whole
/// under the `agents` lock — liveness, the #95r precedence check and the name
/// write are one critical section, and the entry is cloned out of it — so a
/// human rename racing an orchestrator rename resolves by RANK, not by arrival:
/// the orchestrator's loses to a name the human set, whichever order they land
/// in. That is the property #95r asked for, and it is stronger than the
/// ordering this conversion gives up.
#[tauri::command]
pub async fn orch_agent_renamed(
    app: AppHandle,
    agent_id: String,
    name: String,
) -> Result<(), String> {
    let reg = reg_of(&app);
    run_blocking(move || reg.rename_agent(&agent_id, &name, NameSource::Human).map(|_| ())).await
}

/// Session ↔ orchestration-role mapping for the session browser badges.
///
/// Off-thread (#1592). This was the last full per-group fan-out still
/// dispatched SYNCHRONOUSLY: Tauri calls a sync `#[tauri::command]` directly on
/// the webview main thread (the same note `git.rs` and `sessions::list_sessions`
/// carry, issues #207/#399), so [`OrchRegistry::session_roles`] — which reads and
/// parses every group's roster AND both audit generations — blocked the UI for
/// as long as that took. On an install with a long orchestration history that is
/// tens of megabytes of JSONL parsed on the thread that has to paint, which is
/// the `AppHangB1` half of #1592. #1568's own doc comment named this as the
/// unconverted debt (#743 F4); this is the conversion.
///
/// **Reentrancy.** Read-only and idempotent, the same rule
/// [`orch_list_recorded`] documents: no registry state is mutated, and two
/// concurrent calls (the sidebar's boot prefetch racing a human's refresh) can
/// disagree only about a group whose files changed between them.
#[tauri::command]
pub async fn orch_session_roles(app: AppHandle) -> Vec<SessionRole> {
    let reg = reg_of(&app);
    run_blocking(move || OrchRegistry::read_command("orch_session_roles", Vec::new, || reg.session_roles()))
        .await
}

/// Every orchestration group loomux has a record of, for the session
/// browser's "Orchestrations" section (see
/// [`OrchRegistry::recorded_orchestrations`], which carries the argument for
/// what this reads and what it refuses to read).
///
/// Off-thread (#762 — see [`run_blocking`]): this walks the group root and
/// reads two small JSON files per group, plus one CLI-store lookup per group
/// that has a recorded orchestrator session — an opencode lookup opens that
/// group's SQLite store. `orch_session_roles` above fans out over every group
/// too, and when this command landed it was still SYNC — putting a second such
/// fan-out on the webview thread is exactly what the #1563 plan forbade, so
/// this one was async from the start. #1592 converted that one as well, so the
/// contrast is gone and the reason is not: two full per-group fan-outs share
/// this surface, and neither may be on the thread that paints.
///
/// **Reentrancy.** Read-only and idempotent: no registry state is mutated, and
/// every file it touches is read whole with the "unreadable degrades, never
/// fails" rule the listing paths already use. Two concurrent calls (the boot
/// prefetch racing a human opening the sidebar) can disagree only about a
/// group whose files changed between them, which is the same freshness
/// question a single call already answers as of when it ran.
#[tauri::command]
pub async fn orch_list_recorded(app: AppHandle) -> Vec<RecordedOrchestration> {
    let reg = reg_of(&app);
    run_blocking(move || {
        OrchRegistry::read_command("orch_list_recorded", Vec::new, || reg.recorded_orchestrations())
    })
    .await
}

/// Restore a recorded orchestration session (see `resume_recorded_session`).
/// Returns the orchestrator pane spec, or null when the pane will arrive
/// via `orch-spawn-request` (worker/reviewer rejoin).
///
/// Off-thread (#762): the session-restore path reads the persisted group state
/// back from disk, the same shape as [`create_orchestration`] and on the same
/// app-restore gesture — several of them at once when a window comes back with
/// several orchestration tabs.
///
/// **Reentrancy.** It reaches `create_orchestration_group`, so it inherits
/// [`create_orchestration`]'s argument for the case it was built for: several
/// restores of DIFFERENT sessions racing on app start each carry their own
/// `expect_group`, and the `creation` mutex (`performance.md` §4 X6) makes id
/// selection and orchestrator registration one unit, so they reattach to their
/// own groups instead of competing for a fresh id.
///
/// **The same-session case is weaker, and rev-260 was right to flag it.** The
/// `already has a live orchestrator` precheck runs OUTSIDE `creation` (the only
/// `creation.lock_safe()` is inside `create_group_ex`), so two restores of ONE
/// recorded session can both pass it. The loser is refused — `expect_group`
/// pins the id and rejects the one liveness picked instead — but only after
/// `create_group_ex` has already run for that second id, leaving a group
/// materialized that nothing asked for, and reporting a cause that describes
/// the id mismatch rather than the double restore. Pre-conversion the second
/// call ran strictly after the first and returned the clean "already has a live
/// orchestrator" error, so this IS newly reachable. It is recorded rather than
/// fixed because the fix is to take `creation` across the precheck (or hand the
/// spawn a liveness token to re-validate), which changes the restore path's
/// locking rather than its dispatch: **#799** owns it, and cites this paragraph
/// back from the precheck itself.
#[tauri::command]
pub async fn resume_orch_session(
    app: AppHandle,
    session_id: String,
    group_hint: Option<String>,
    role_hint: Option<String>,
    start_fresh: bool,
) -> Result<Option<SpawnRequest>, String> {
    let reg = reg_of(&app);
    run_blocking(move || {
        // #904: an unparseable hint is simply no hint — the caller falls
        // through to the full scan, exactly as it does for a stale one.
        let hint = group_hint.and_then(|g| GroupId::parse(&g).ok()).zip(role_hint);
        resume_recorded_session(&reg, &session_id, hint, start_fresh)
    })
    .await
}

/// Fork a delegate's session into a new agent pane (#3318 F2) — the HUMAN's
/// route to [`OrchRegistry::fork_agent`], from a delegate pane's menu. Every
/// refusal is `fork_agent`'s, worded for an orchestrator and a human alike.
///
/// **`async` through [`run_blocking`], not a synchronous command through
/// `mutating_command`, and that is forced rather than chosen.** A fork is a
/// spawn, and a spawn blocks until the FRONTEND opens and binds the new pane
/// (`BIND_TIMEOUT`). A synchronous command runs inline on the webview/GUI
/// thread — the same thread whose event loop has to service
/// `orch-spawn-request` for that bind to happen — so a sync fork would wait on
/// itself for the whole timeout and then fail. Constraint 10's
/// `mutating_command` barrier exists for commands that run on that thread;
/// this one does not, which is `resume_orch_session`'s shape directly above,
/// for the same reason.
#[tauri::command]
pub async fn orch_fork_agent(
    app: AppHandle,
    group_id: String,
    agent_id: String,
    task: Option<String>,
    name: Option<String>,
) -> Result<Value, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        let a = reg.fork_agent(
            &group_id,
            "human",
            &agent_id,
            task.as_deref().unwrap_or(""),
            None,
            None,
            name.as_deref().unwrap_or(""),
        )?;
        Ok(json!({ "agent_id": a.id, "name": a.name, "session_id": a.session_id }))
    })
    .await
}

/// The frontend's ACK for an `orch-fork-solo-request` (#3318 F2, review round
/// 1): whether the Solo pane a lead asked for actually opened. See
/// [`OrchRegistry::record_solo_fork_outcome`]. `async` through [`run_blocking`]
/// for the same reason as its neighbours: the body writes the audit log, and
/// disk I/O stays off the webview thread.
#[tauri::command]
pub async fn orch_fork_solo_result(
    app: AppHandle,
    group_id: String,
    agent_id: String,
    opened: bool,
    detail: String,
) -> Result<(), String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || reg.record_solo_fork_outcome(&group_id, &agent_id, opened, &detail)).await
}
