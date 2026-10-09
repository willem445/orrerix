//! Standalone panes, lead panes and session forks: preparing, binding and
//! adopting a standalone pane (#271), a lead pane's identity and children
//! (#2519), and forking a delegate's session into a new pane (#3318), as an
//! `impl OrchRegistry` block (#3498). The designs are
//! `docs/design/lead-pane.md` and `docs/design/session-fork.md`.

use super::*;

impl OrchRegistry {
    // ---------- standalone panes (#271 W3 addendum, part A) ----------
    //
    // A standalone (launcher) pane has no orchestration group and, before
    // this, no MCP identity at all. To make one a first-class channel member
    // it needs exactly what any channel member needs: an `AgentEntry` with a
    // `pty_id` (delivery is hard-keyed to that), and — only if it's meant to
    // `channel_send` — a token + MCP config. Everything below mints that
    // identity directly, bypassing `spawn_agent_ex`/`build_agent_command`
    // entirely (no block, no persona, no kickoff, no guardrail cap): these
    // are human-only Tauri commands (constraint 5), reached from the
    // launcher's pane-spawn path or the pane-menu Connect gesture, never
    // from MCP.

    /// Lazily register the reserved standalone pseudo-group the first time a
    /// solo pane is created. Idempotent. Minimal `GroupInfo` — no workflow,
    /// no merge gate, no per-role roster: solo panes never traverse the
    /// block machinery, so there is nothing else to configure.
    fn ensure_solo_group(&self) {
        if self.groups.lock_safe().contains_key(SOLO_GROUP) {
            return;
        }
        // #904: the reserved constant, validated like any other id rather than
        // exempted for being ours. `None` is unreachable (pinned by the
        // acceptance suite) and simply declines to register the pseudo-group.
        let solo = solo_group_id().clone();
        let _ = fs::create_dir_all(self.group_dir(solo_group_id()).join("configs"));
        let info = GroupInfo {
            id: solo,
            repo: "(standalone)".to_string(),
            guardrails: Guardrails::default().clamped(),
        };
        self.groups.lock_safe().insert(solo_group_id().clone(), info);
    }

    /// Human-only (the launcher's agent-pane spawn path, constraint 5): mint
    /// a channel-scoped identity for a newly-launching standalone pane
    /// BEFORE it boots, so the MCP flags can be appended to its command line
    /// (you cannot inject an MCP server into an already-running CLI). `cli`
    /// is the CLI the human picked. For a CLI whose [`CliCaps::mcp_argv_seam`]
    /// is true — claude, copilot and pi today — this writes the
    /// same per-agent config `write_mcp_config` gives an orchestration-group
    /// agent and mints a real token: the pane is a FULL member, able to
    /// `channel_send`. For any other CLI (no seam: codex/gemini/opencode/
    /// custom — their own MCP mechanisms are repo/user config files, a
    /// separate per-CLI follow-up, not a spawn-time flag) this mints NO
    /// token — the `AgentEntry` still exists (so the pane CAN be a
    /// `deliver_prompt` target once connected), but it is delivery-only from
    /// birth, exactly like an adopted already-running pane (`solo_adopt`).
    /// Returns `{agent_id, mcp_args, delivery_only}` — `mcp_args` is the
    /// exact per-CLI flag string to append to the launched command line
    /// (empty for a delivery-only CLI).
    pub fn solo_prepare(&self, cli: &str, cwd: &str, name: &str) -> Result<Value, String> {
        self.ensure_solo_group();
        let seq = self.mint_agent_seq(solo_group_id());
        let agent_id = format!("solo-{seq}");
        let display = sanitize_agent_name(name);
        let display = if display.is_empty() { agent_id.clone() } else { display };

        // #267: derived from the capability table, NOT from `SUPPORTED_CLIS`.
        // The question a solo launch asks is "can this CLI's MCP config be
        // delivered as a flag string appended to a command line the human
        // owns?" — and that stopped being the same question as "is this CLI
        // group-spawnable" the moment gemini arrived, whose config is a file
        // named by an environment variable loomux can only set on a pane it
        // spawns itself. Asking the old question here would have run gemini
        // into the `unreachable!` below. See `CliCaps::mcp_argv_seam`.
        let has_seam = cli_caps(cli).is_some_and(|c| c.mcp_argv_seam);
        let (token, mcp_args) = if cli == "codex" && has_seam {
            // codex (#2515 C1) is the one seam CLI whose SOLO profile is not
            // the same document as its group profile, so it is written here
            // rather than through `write_mcp_config` below.
            //
            // `&& has_seam` is not belt-and-braces, it is what keeps this arm
            // DATA-driven like the `match` below it. Without it, a `CLI_CAPS`
            // row that set `mcp_argv_seam: false` for codex — the deliberate
            // way to make a CLI delivery-only — would be silently overruled
            // here: the pane would still mint a token and write a profile
            // while the table said it could not. With it, such a row falls to
            // the `else if has_seam` below, misses, and the pane is
            // delivery-only exactly as the table says.
            //
            // The difference is one field and it is forced: a solo launch only
            // ever appends a flag string to a command line the human owns — it
            // sets no pane environment at all (the pi arm below says so) — so
            // a profile naming `env_http_headers` would name a variable
            // nothing sets. The pane would connect with no auth header and
            // still be advertised `delivery_only: false`, which is exactly the
            // disagreement the `other =>` arm below exists to prevent. So a
            // solo profile carries the token itself; see `CodexMcpAuth`.
            //
            // Everything else is a group pane's document with its group-only
            // parts absent: `Role::Solo` is `Containment::None` and has no
            // block, so there is no persona to compile and no effort knob to
            // deliver, and the posture is `attended` because a solo pane HAS a
            // human in it — the same reading `write_mcp_config`'s
            // `unattended: false` gives on every other CLI here.
            let token = new_token();
            let seg = PathSegment::parse(&agent_id)
                .map_err(|e| format!("invalid agent id {agent_id:?}: {e}"))?;
            let (_path, profile) = self.write_codex_profile(
                solo_group_id(),
                &seg,
                CodexMcpAuth::Literal(&token),
                Path::new(cwd),
                /*unattended*/ false,
                /*effort*/ "",
                /*developer_instructions*/ None,
                // Nothing extra (#3456 is scoped to GROUP panes): a solo pane is
                // the human's own session, `on-request`, with the human in it to
                // approve an escalation — and widening the sandbox of a session
                // loomux does not own is not this line's call.
                /*git*/ &CodexGitAccess::default(),
            )?;
            // The NAME, not the path: `-p` takes a profile name and resolves
            // it against `CODEX_HOME` itself. Unquoted, and safe unquoted,
            // because `codex_profile_name` refuses anything outside codex's
            // own `[A-Za-z0-9_-]` alphabet rather than sanitizing it.
            (token, format!("-p {profile}"))
        } else if has_seam {
            let token = new_token();
            // A solo pane is `Role::Solo` — `Containment::None`, nothing to
            // deny, no block, and therefore no persona to compile. No
            // argv-seam CLI reads `containment` or `persona`; `workdir` IS
            // read on pi, which is why the human's own cwd is passed rather
            // than a placeholder — a solo pi pane is exposed to the repo's own
            // MCP files exactly as a group pane is, and the audit row saying
            // so is worth as much here.
            let cfg = self
                .write_mcp_config(
                    solo_group_id(),
                    &agent_id,
                    &token,
                    cli,
                    Path::new(cwd),
                    Containment::None,
                    false,
                    // No block, so no knobs — and no argv-seam CLI reached
                    // from here reads them anyway: codex, the one that would,
                    // is handled above.
                    workflow::ModelKnobs::default(),
                    &PersonaInject::default(),
                )?
                .path;
            let args = match cli {
                // Every join site APPENDS this string (`launcher.ts`,
                // `panerestore.ts`, `sessions.rs`), so on an autopilot solo
                // launch the line ends up with TWO `--allowedTools`
                // occurrences: `single_pane_autopilot_flags`' git/gh pair and
                // this one. That is NOT #610's defect — nothing splices into
                // either value list, so each stays contiguous — and it is
                // harmless because such a pane runs `--permission-mode auto`,
                // which approves git/gh without consulting an allow list at
                // all. Whether repeated occurrences accumulate or last-wins is
                // undocumented and deliberately NOT relied on here; if a solo
                // pane ever needs an allow list to be honoured (a read-only
                // solo tier, say), merge the two into one occurrence first.
                "claude" => format!(
                    "--mcp-config \"{}\" --strict-mcp-config --allowedTools {tools}",
                    cfg.display(), tools = brand::MCP_TOOL_PREFIX
                ),
                "copilot" => format!(
                    "--additional-mcp-config \"@{}\" --allow-tool {server}",
                    cfg.display(), server = MCP_SERVER
                ),
                // pi: the flag alone, and DELIBERATELY without the exclusivity
                // a group pane gets. `PI_MCP_CONFIG_MODE=exclusive` is pane
                // ENVIRONMENT (`cli_extra_env`), and a solo launch only ever
                // appends a flag string to a command line the human owns — it
                // sets no environment at all. So a solo pi pane's adapter
                // merges this config with the human's own MCP sources, which
                // is the right answer for their pane: the loomux server is an
                // ADDITION to what they already had, not a replacement for it.
                // A group pane is the opposite case and gets the env.
                "pi" => format!("--mcp-config \"{}\"", cfg.display()),
                // Not `unreachable!()` any more (constraint 10). `has_seam`
                // comes from `CLI_CAPS`, so this arm is reached by DATA — a
                // row landing with `mcp_argv_seam: true` and no arm here would
                // have aborted the process, and `solo_prepare` is called
                // inline on the webview thread from a synchronous
                // `#[tauri::command]`, where an unwind is an abort rather than
                // a degrade. A row without an arm is a loomux bug, so it is
                // reported as one, at the one cost a solo pane can actually
                // pay: this pane launches delivery-only, exactly as a CLI with
                // no seam does. `every_argv_seam_cli_has_a_solo_mcp_arm` is
                // what keeps the pairing from drifting in the first place.
                other => {
                    self.audit(solo_group_id(), brand::AUDIT_ACTOR, "error", json!({
                        "what": "solo MCP flag missing for an argv-seam CLI",
                        "cli": other,
                        "detail": "CLI_CAPS says this CLI's MCP config is argv-deliverable, but \
                                   solo_prepare has no flag string for it — the pane launches \
                                   delivery-only. This is a loomux bug: add the arm beside the \
                                   row.",
                    }));
                    String::new()
                }
            };
            // An empty flag string from that arm means the config never
            // reaches the pane, so the token it was minted with can never be
            // presented — and `delivery_only` below is computed from the
            // token. Dropping it here is what keeps the pane's ADVERTISED
            // status ("full member, may channel_send") from disagreeing with
            // what it can actually do; the alternative is a pane the UI
            // offers `channel_send` on that fails at every call.
            if args.is_empty() { (String::new(), args) } else { (token, args) }
        } else {
            (String::new(), String::new())
        };
        let delivery_only = token.is_empty();

        let entry = human_pane_entry(
            &agent_id,
            solo_group_id().clone(),
            display,
            "solo",
            Role::Solo,
            &token,
            cwd,
            Some(cli.to_string()),
        );
        self.agents.lock_safe().insert(agent_id.clone(), entry);
        if !token.is_empty() {
            self.by_token.lock_safe().insert(token, agent_id.clone());
        }
        self.audit(
            solo_group_id(),
            "human",
            "solo-prepare",
            json!({ "agent": agent_id, "cli": cli, "delivery_only": delivery_only }),
        );
        Ok(json!({ "agent_id": agent_id, "mcp_args": mcp_args, "delivery_only": delivery_only }))
    }

    /// Human-only: bind the pty a newly-spawned solo pane's launcher just
    /// opened to the `AgentEntry` `solo_prepare` created. Unlike `bind` (the
    /// channel-based rendezvous a `spawn_agent_ex` call blocks on),
    /// `solo_prepare` already returned synchronously with no spawner thread
    /// waiting — this is direct bookkeeping, not a wakeup. Registers
    /// `by_pty[pty_id] = agent_id` so the EXISTING pty-exit path
    /// (`by_pty` → `mark_dead` → `cleanup_agent_channel`) tears the member
    /// down on pane close with no new teardown code.
    pub fn solo_bind(&self, agent_id: &str, pty_id: u32) -> Result<(), String> {
        {
            let mut agents = self.agents.lock_safe();
            let a = agents.get_mut(agent_id).ok_or("unknown agent")?;
            if a.role != Role::Solo {
                return Err("solo_bind is only for standalone panes".into());
            }
            a.pty_id = Some(pty_id);
            a.status = AgentStatus::Running;
        }
        self.by_pty.lock_safe().insert(pty_id, agent_id.to_string());
        Ok(())
    }

    /// Human-only: start a background watch for copilot's "Enable autopilot
    /// mode" consent dialog on a just-spawned SOLO pane (#364). A single-pane
    /// copilot agent now launches with `--autopilot` whenever the launcher's
    /// Autopilot checkbox is on (see `single_pane_autopilot_flags`), so it
    /// WILL show this dialog — but unlike a group agent, a solo pane never
    /// gets a programmatic kickoff to hang the confirm off of (`Role::Solo`
    /// "never receives a kickoff": the human types their own first message,
    /// and THAT submit is what triggers the dialog). This spawns the SAME
    /// fail-soft watcher the group kickoff path uses
    /// ([`confirm_copilot_autopilot_dialog`]), just started at spawn time
    /// instead of after a loomux-sent Enter, and with
    /// [`SOLO_AUTOPILOT_DIALOG_WAIT`]'s much longer, human-paced window.
    ///
    /// `cli` is re-checked here via [`should_confirm_copilot_autopilot`]
    /// (not just trusted from the caller) so a non-copilot pane, or one
    /// launched with the checkbox off, can never start a watcher that writes
    /// unsolicited bytes into a human's terminal. Best-effort: a missing app
    /// handle or an already-gone pane just means nothing watches this pane,
    /// the same failure mode as any other best-effort mint in this file — the
    /// caller (the launcher, right after pty spawn) never blocks on it.
    pub fn confirm_solo_copilot_autopilot(&self, pty_id: u32, cli: &str) -> Result<(), String> {
        if !should_confirm_copilot_autopilot(cli, true, true) {
            return Ok(());
        }
        let app = self.app.lock_safe().clone().ok_or("no app handle")?;
        let root = self.root.clone();
        std::thread::spawn(move || {
            let ptys = app.state::<crate::pty::PtyManager>();
            confirm_copilot_autopilot_dialog(&ptys, pty_id, &root, solo_group_id(), "solo", SOLO_AUTOPILOT_DIALOG_WAIT);
        });
        Ok(())
    }

    /// Human-only, reached from the pane-menu Connect gesture (part A3): on
    /// the first Connect against a pane with no channel identity — launched
    /// before this feature existed, or simply never `solo_prepare`d —
    /// register it as a delivery-only member: an `AgentEntry` with NO token
    /// (it can never `channel_send`), bound to its already-running pty.
    /// loomux already owns the pty, so inbound delivery works today; this
    /// makes that pane a legitimate **receiver** rather than refusing the
    /// connect outright. Idempotent by pty: re-adopting an already-adopted
    /// pty returns its existing agent id instead of minting a second one.
    ///
    /// `cli` and `session_id` are what the launcher already knows about a pane
    /// it spawned (#3831). The cache-age chip reads a pane's usage through its
    /// CLI and its session, and an adopted plain pane has neither unless they
    /// are given here. Both are optional. An unknown CLI, and a session id that
    /// is not one path component (it names a transcript file), are refused
    /// rather than recorded.
    pub fn solo_adopt(
        &self,
        pty_id: u32,
        name: &str,
        cwd: &str,
        cli: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Value, String> {
        if let Some(existing) = self.by_pty.lock_safe().get(&pty_id).cloned() {
            return Ok(json!({ "agent_id": existing }));
        }
        if let Some(c) = cli {
            if cli_caps(c).is_none() {
                return Err(format!("unknown CLI {c:?}: an adopted pane can only name a CLI loomux knows"));
            }
        }
        if let Some(s) = session_id {
            PathSegment::parse(s).map_err(|e| format!("invalid session id {s:?}: {e}"))?;
        }
        self.ensure_solo_group();
        let seq = self.mint_agent_seq(solo_group_id());
        let agent_id = format!("solo-{seq}");
        let display = sanitize_agent_name(name);
        let display = if display.is_empty() { agent_id.clone() } else { display };
        let entry = AgentEntry {
            id: agent_id.clone(),
            group: solo_group_id().clone(),
            name: display,
            name_source: NameSource::Default,
            block: "solo".to_string(),
            role: Role::Solo,
            token: String::new(), // delivery-only, by construction: never a channel_send caller
            status: AgentStatus::Running,
            pty_id: Some(pty_id),
            pane_id: None,
            pane_kind: None,
            forked_from: None,
            task: String::new(),
            task_id: None, // a solo pane has no board binding
            session_id: session_id.map(str::to_string),
            cwd: cwd.to_string(),
            branch: None, // solo panes aren't part of the multi-agent worktree/branch model
            idle_since_ms: None,
            started_ms: now_ms(),
            last_progress_ms: now_ms(),
            last_mcp_activity_ms: 0,
            last_output_progress_ms: now_ms(), // solo panes are never idle-ticked (not Role::Orchestrator); inert
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
            contract_carrier: ContractCarrier::default(), // unused — see solo_prepare's identical field
            last_state_write_ms: 0,
            compact_escalation_notified: false,
            cache_idle_nudge_latched: false,
            idle_tick_skip_rearm_ms: 0,
            solo_cli: cli.map(str::to_string), // as the launcher knows it; `cli_for_agent` reads it first
            last_exit_tail: None,
            killed_by: None,
        };
        self.agents.lock_safe().insert(agent_id.clone(), entry);
        // Claim the pty as ONE decision, and let the loser of a race roll its
        // own entry back. The `by_pty` read at the top of this function makes
        // the ordinary re-adopt cheap; it does not make the mint idempotent,
        // because check-then-insert is two lock acquisitions with a mint
        // between them. What used to close that window was the webview thread
        // — a second Connect gesture could not be *inside* the first call —
        // and #762 takes that away, so the vacant-entry insert becomes the
        // whole decision: exactly one adopt of a pty wins, and the loser
        // returns the winner's id instead of leaving a second delivery-only
        // identity for one pane in the roster.
        let winner = match self.by_pty.lock_safe().entry(pty_id) {
            std::collections::hash_map::Entry::Occupied(e) => e.get().clone(),
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(agent_id.clone());
                agent_id.clone()
            }
        };
        if winner != agent_id {
            self.agents.lock_safe().remove(&agent_id);
            return Ok(json!({ "agent_id": winner }));
        }
        self.audit(solo_group_id(), "human", "solo-adopt", json!({ "agent": agent_id, "pty_id": pty_id }));
        Ok(json!({ "agent_id": agent_id }))
    }


    /// Human-only (#3831): record the session id of a solo or lead pane whose
    /// CLI named it after the pane was registered — claude and pi name theirs
    /// at launch, so a pane bound after its spawn already has one, and codex,
    /// opencode and copilot learn theirs later.
    ///
    /// Only a `Role::Solo` or `Role::Lead` entry takes one: a delegate learns
    /// its session from its own CLI through the watcher, and this door must not
    /// give it a second source. The same id again is a success that changes
    /// nothing: a restored pane re-reports the session its entry already holds. A
    /// different id on record is refused and never overwritten. An id that is not
    /// one path component is refused, because the usage reader builds a transcript
    /// path from it.
    ///
    /// A solo pane is NOT persisted. `__solo__` has no roster: `agents.json` in
    /// a group directory is read as that group's roster, and a solo pane is the
    /// human's own pane, not a member of anything. A lead's row is already
    /// persisted by `lead_prepare`, so its session is written back to that row.
    pub fn human_pane_session(&self, agent_id: &str, session_id: &str) -> Result<(), String> {
        PathSegment::parse(session_id).map_err(|e| format!("invalid session id {session_id:?}: {e}"))?;
        let entry = {
            let mut agents = self.agents.lock_safe();
            let a = agents.get_mut(agent_id).ok_or("unknown agent")?;
            if !matches!(a.role, Role::Solo | Role::Lead) {
                return Err("human_pane_session is only for solo and lead panes".into());
            }
            if let Some(existing) = a.session_id.as_deref() {
                // The same id again is the session the entry already holds — a restored
                // pane re-reporting it. Nothing changes, so nothing is audited or written.
                if existing == session_id {
                    return Ok(());
                }
                return Err(format!("{agent_id} already has session {existing:?}; a different id is never written over it"));
            }
            a.session_id = Some(session_id.to_string());
            a.clone()
        };
        self.audit(&entry.group, "human", "session-learned", json!({ "agent": agent_id, "session": session_id }));
        if entry.role == Role::Lead {
            self.persist_agent_record(&entry, "running");
        }
        Ok(())
    }

    // ---------- lead panes (#2519) ----------
    //
    // A lead is a human-launched pane like a solo one — orrerix never builds
    // its command line, the launcher does, and `lead_prepare` runs BEFORE the
    // CLI boots so its MCP flags can be appended to that line. What it is not
    // is a solo pane: it is the ROOT of a real, freshly-minted orchestration
    // group whose delegates report into it. So the two halves below are
    // deliberately borrowed from different places — the group mint from
    // `create_orchestration_group`, the identity/flag half from
    // `solo_prepare` — rather than either being copied.
    //
    // See `docs/design/lead-pane.md` for the class, the surface and the
    // consent argument.

    /// Human-only (the launcher's "orrerix subagents" toggle, constraint 5):
    /// mint a lead group and the lead's own identity BEFORE its CLI boots, so
    /// the MCP flags can be appended to the command line the launcher is about
    /// to run. Returns `{group_id, agent_id, mcp_args}`.
    ///
    /// **What it creates, and what it deliberately does not.** A real group —
    /// group dir, `group.json`, instruction files, audit log — through the same
    /// `create_group_ex` every launch uses, under the same `creation` mutex, so
    /// two toggles racing on one repo cannot pick the same id. What it does NOT
    /// create is an orchestrator: `register_orchestrator_pane` is not called
    /// and no `Role::Orchestrator` agent is ever inserted. The roster still
    /// gains an orchestrator BLOCK, because `Guardrails::clamped` step 4
    /// prepends one to any roster that declares none — a row, not a pane, and
    /// the one-root check below is what keeps that distinction from mattering
    /// (slice A's design note flagged exactly this as a tripwire for this
    /// function).
    ///
    /// **`advanced_orchestrator: false`, and that is the consent argument in
    /// code.** A repo's workflow file is a consent surface: the launcher
    /// previews the roster it declares and the human agrees to it before the
    /// group runs (#222). A lead group has no preview and no such moment — the
    /// toggle is the whole of the human's gesture — so it runs the built-in
    /// roster, and with the flag off `create_group_ex` never opens the file at
    /// all.
    ///
    /// **The roster carries reviewer and planner blocks it will never open**,
    /// on purpose. The refusal a lead gets for `kind: "reviewer"` is the
    /// caller-class check in `mcp::call_tool`, which is the refusal
    /// `docs/design/lead-pane.md` argues for and
    /// `a_lead_may_spawn_a_worker_and_nothing_else` pins. Drop the blocks and
    /// the same call fails on "no such block" instead — a different refusal,
    /// from a different mechanism, and one that would silently start passing if
    /// the class check were ever removed.
    ///
    /// Refuses a CLI whose MCP config cannot ride on a command line
    /// (`CliCaps::mcp_argv_seam`): unlike a solo pane, which degrades to
    /// delivery-only and is still useful, a lead with no MCP server holds none
    /// of the tools the toggle exists to grant, so a pane that launched anyway
    /// would be a lead in name only.
    #[allow(clippy::too_many_arguments)] // the launcher's guardrail fields, one each
    pub fn lead_prepare(
        &self,
        cli: &str,
        cwd: &str,
        name: &str,
        max_agents: u32,
        auto_ops: bool,
        idle_kill_minutes: u32,
        max_spawns_per_hour: u32,
        watchdog_stall_minutes: u32,
    ) -> Result<Value, String> {
        if !SUPPORTED_CLIS.contains(&cli) {
            return Err(format!(
                "unsupported CLI {cli:?} for a lead pane — supported: {}",
                SUPPORTED_CLIS.join(", ")
            ));
        }
        // Not `SUPPORTED_CLIS`, and the distinction is `solo_prepare`'s (#267):
        // the question is whether THIS pane's MCP config can be delivered as a
        // flag string appended to a command line the human owns. opencode and
        // codex answer no — their MCP config is a file or an environment
        // variable, and the launcher's seam sets no environment — so the toggle
        // is refused for them here as well as being hidden for them in the
        // launcher. Naming the follow-up in the message matters: this is a
        // missing seam, not a policy, and whoever reads it should know which.
        if !cli_caps(cli).is_some_and(|c| c.mcp_argv_seam) {
            return Err(format!(
                "{cli} cannot host a lead pane yet: its MCP config is delivered through the \
                 pane's environment or a config file, not as flags on the command line the \
                 launcher builds, and a lead with no orrerix MCP server holds none of the \
                 tools this toggle grants. See docs/design/lead-pane.md — widening the prepare \
                 seam to carry environment pairs is the follow-up that lifts this."
            ));
        }
        // codex passes the seam gate by the letter of `mcp_argv_seam` (its
        // identity rides `-p <profile>`, one indirection out - #2515 C1) but
        // `lead_mcp_args` has no codex arm and `write_mcp_config`'s codex branch
        // was built for the solo path, so a codex lead would launch as a lead in
        // name only. Refused here, by name, until the lead path grows the
        // profile arm (#2833). A behaviour gate, not a mistyped identity: the
        // string never becomes a name.
        if cli == "codex" {
            return Err(
                "codex cannot host a lead pane yet: its MCP identity rides a profile file the lead path does not write. See docs/design/lead-pane.md - the codex lead arm is the follow-up (#2833) that lifts this."
                    .into(),
            );
        }
        validate_group_repo(cwd)?;

        // Every roster loomux synthesizes on a group's behalf pins no model
        // knob (`default_roster_ex`'s own doc), and this is one: the human
        // picked their model in their own launcher, and their helpers inherit
        // the CLI they launched.
        let blocks = workflow::default_roster(&[
            (Role::Lead, cli, ""),
            (Role::Worker, cli, ""),
            (Role::Reviewer, cli, ""),
            (Role::Planner, cli, ""),
        ]);
        let rails = Guardrails {
            max_agents,
            agent_cli: cli.to_string(),
            blocks,
            advanced_orchestrator: false,
            auto_ops,
            idle_kill_minutes,
            max_spawns_per_hour,
            watchdog_stall_minutes,
            ..Guardrails::default()
        };

        // Held across the mint AND the one-root check below, for
        // `create_orchestration_group`'s reason: id selection by liveness and
        // root registration are one unit, or two toggles racing on one repo
        // both see the other's candidate as free and both register a root in
        // it.
        let _creation = self.creation.lock_safe();
        let group = self.create_group_ex(cwd, rails, Launch::Fresh)?;

        // THE ONE-ROOT INVARIANT, enforced at the mint because there is nowhere
        // else it can be. `deliver_relayed_to_root` is a `find` over a
        // `HashMap`, whose iteration order is not stable, so a group holding two
        // `is_root()` agents delivers a child's report to whichever comes back
        // first — with no error on either side, and possibly a different answer
        // between runs. `next_group_id` already skips groups with live agents,
        // so this is a backstop rather than the primary defence; it is here
        // because the cost of it being wrong is silent misdelivery rather than
        // a failure.
        let existing_root = self
            .agents
            .lock_safe()
            .values()
            .find(|a| a.group == group.id && a.status != AgentStatus::Dead && a.role.is_root())
            .map(|a| (a.id.clone(), a.role));
        if let Some((other_id, other_role)) = existing_root {
            return Err(format!(
                "group {} already has a live {} ({other_id}) — a group has exactly one root, and \
                 a second one would make which pane receives a child's report depend on hash \
                 order. Close that pane first, or open the lead on another repository",
                group.id,
                other_role.as_str(),
            ));
        }

        // N2 (rev-final): the identity half, split at the seam the review named
        // — everything above this line decides WHICH GROUP, everything below it
        // decides WHO THE PANE IS. It is also every remaining step that can
        // FAIL, which is what lets the marker below be written knowing no error
        // return can strand one (B1, second trigger).
        let (agent_id, mcp_args) = self.mint_lead_identity(&group, cli, cwd, name, auto_ops)?;

        // THE MARKER, written LAST. It used to sit above `write_mcp_config`'s `?`
        // and the empty-flags refusal, so either error return left a `lead`
        // marker on a group that never became a lead group — the same
        // false-refusal state B1's main chain produces, reached from the other
        // side. Nothing below this line can fail.
        //
        // A MARKER FILE rather than the roster, which is the part worth reading
        // twice. `group.json` does carry the lead block, but `read_blocks`
        // resolves every persisted `kind` through `workflow::kind_from_str` —
        // which has no `lead` arm, deliberately and structurally (that absence is
        // what stops a repo file declaring one and stops a lead opening a lead) —
        // so an unknown kind is DROPPED on reload. A reattach or a resume
        // therefore sees a roster with no lead block at all, and anything that
        // asked the roster "is this a lead group?" would answer no for every lead
        // group that has been through a restart.
        //
        // Best-effort, like the `paused` marker it mirrors — and the pairing with
        // `create_group_ex`'s remove is what makes that safe. A failed write
        // loses the resume refusal, which falls through to the ordinary "no
        // recorded orchestration" failures; a marker left behind by a group that
        // has ended is cleared by the next claim of its id, rather than by hoping
        // nothing reuses it.
        let _ = fs::write(self.group_dir(&group.id).join(LEAD_MARKER), b"1");

        self.audit(&group.id, "human", "lead-prepare", json!({
            "agent": agent_id, "cli": cli, "max_agents": max_agents,
        }));
        crate::obs::breadcrumb(
            "lead-prepare",
            &format!("group={} agent={agent_id} cli={cli}", group.id),
        );
        Ok(json!({
            "group_id": group.id,
            "agent_id": agent_id,
            "mcp_args": mcp_args,
        }))
    }

    /// The lead pane's own identity: its id, its token, its MCP config, and the
    /// flag string the launcher appends (#2519, split out per rev-final N2).
    ///
    /// The seam is a statement rather than a line-count trim.
    /// [`lead_prepare`](Self::lead_prepare) decides WHICH GROUP — the argument
    /// checks, the mint, the one-root invariant — and this decides WHO THE PANE
    /// IS, given that group. It is also every step of the prepare that can still
    /// fail, which is what lets its caller write the `lead` marker below the call
    /// and know no error return can strand one.
    ///
    /// Runs under the caller's `creation` guard, and takes `&GroupInfo` rather
    /// than a `&GroupId` so it cannot re-resolve a group its caller has already
    /// pinned.
    fn mint_lead_identity(
        &self,
        group: &GroupInfo,
        cli: &str,
        cwd: &str,
        name: &str,
        auto_ops: bool,
    ) -> Result<(String, String), String> {
        let seq = self.mint_agent_seq(&group.id);
        let agent_id = format!("lead-{seq}");
        let display = sanitize_agent_name(name);
        let display = if display.is_empty() { agent_id.clone() } else { display };
        let token = new_token();
        // Derived from the class, never hand-passed — `register_orchestrator_
        // pane`'s rule: `Role::Lead` is `Containment::None` today, and a literal
        // here would be a second place to remember if that ever changes.
        let containment = Role::Lead.containment();
        // `PersonaInject::default()`, like `solo_prepare`: a lead group has no
        // workflow file, so no block in it can carry a persona, and every CLI
        // that reads `persona` out of this config is one whose seam
        // `lead_prepare` has already refused, above the call to this function.
        let cfg = self.write_mcp_config(
            &group.id,
            &agent_id,
            &token,
            cli,
            Path::new(cwd),
            containment,
            auto_ops || containment.forces_unattended(),
            // Empty on purpose: `knobs` is read by `write_mcp_config`'s codex
            // branch alone (#2515 C1), and `lead_prepare`'s CLI gate above has already
            // refused codex for a lead pane - a lead's effort is argv-borne on
            // the launcher's own command line, never config-borne here.
            workflow::ModelKnobs { effort: "", context: "" },
            &PersonaInject::default(),
        )?;
        let mcp_args = lead_mcp_args(cli, &cfg.path);
        if mcp_args.is_empty() {
            // `solo_prepare`'s argument, with the opposite conclusion. There, a
            // pane with no flags still had a use (delivery-only), so it launched
            // degraded; here the flags ARE the feature, so an argv-seam CLI with
            // no arm refuses rather than shipping a pane advertised as a lead
            // that can do nothing. `every_argv_seam_cli_has_a_lead_mcp_arm` is
            // what keeps the pairing from drifting in the first place.
            self.audit(&group.id, brand::AUDIT_ACTOR, "error", json!({
                "what": "lead MCP flag missing for an argv-seam CLI",
                "cli": cli,
                "detail": "CLI_CAPS says this CLI's MCP config is argv-deliverable, but \
                           lead_mcp_args has no flag string for it — add the arm beside the row.",
            }));
            return Err(format!(
                "loomux has no lead command-line flags for {cli} — this is a loomux bug; nothing \
                 was launched"
            ));
        }

        let entry = human_pane_entry(
            &agent_id,
            group.id.clone(),
            display,
            // The lead block `default_roster` just built. Named by id rather
            // than resolved through `block_for` because this function is what
            // put it there: a lookup here could only ever disagree with itself.
            Role::Lead.as_str(),
            Role::Lead,
            &token,
            cwd,
            // `None`, unlike a solo pane, and it is the same statement
            // `solo_cli`'s own doc makes: a non-solo agent's CLI comes from its
            // BLOCK, and this one's block carries `cli` because the roster
            // `lead_prepare` built pinned it. A `Some` here would be a second
            // source for one fact.
            None,
        );
        self.agents.lock_safe().insert(agent_id.clone(), entry.clone());
        self.by_token.lock_safe().insert(token, agent_id.clone());
        // Persisted, unlike a solo pane's: this row is what makes the pane
        // visible to the session browser and the Agents tab, and it is the
        // `"role": "lead"` row `docs/design/lead-pane.md` lists as a
        // public-contract change.
        self.persist_agent_record(&entry, "running");
        Ok((agent_id, mcp_args))
    }

    /// Human-only: bind the pty the launcher just opened for a lead pane, then
    /// type its kickoff. The bookkeeping half mirrors
    /// [`solo_bind`](Self::solo_bind) exactly — `lead_prepare` returned
    /// synchronously with no spawner thread waiting, so this is direct
    /// bookkeeping and not a wakeup, and registering `by_pty[pty_id]` is what
    /// routes the EXISTING pty-exit path (`by_pty` -> `on_pty_exit` ->
    /// `mark_dead`) at this pane with no new teardown code.
    ///
    /// **The kickoff is the half `solo_bind` has no counterpart for.** A solo
    /// pane is an arbitrary human-launched CLI and is typed nothing, ever; a
    /// lead is a capability class, and `Delivery::FreshKickoff` is the
    /// mechanism every class learns its identity by. It is delivered from HERE
    /// rather than from `lead_prepare` because there is no pane to type into
    /// until the bind: `deliver_prompt` is keyed on `pty_id`.
    ///
    /// **A kickoff that cannot be delivered does NOT fail the bind**, and the
    /// outcome is audited rather than discarded — `open_manager_pane_at_launch`
    /// review N3, which is the same shape. By the time this runs the pane is
    /// open, the pty is spawned and the human is looking at it; returning `Err`
    /// would report a launch failure for a launch that plainly happened, and
    /// there is nothing for the caller to retry or undo. What a dropped kickoff
    /// must not be is INVISIBLE — a lead that never learned it is one is a pane
    /// whose behaviour nobody can explain from the outside — so the delivery's
    /// own error is written to the audit log with the agent named.
    ///
    /// A second bind of the same lead is REFUSED rather than tolerated,
    /// because the kickoff is not idempotent — it would type a second one into
    /// a conversation already under way.
    /// `lead_bind_delivers_the_lead_kickoff_once` is the pin.
    pub fn lead_bind(&self, agent_id: &str, pty_id: u32) -> Result<(), String> {
        let group_id = {
            let mut agents = self.agents.lock_safe();
            let a = agents.get_mut(agent_id).ok_or("unknown agent")?;
            if a.role != Role::Lead {
                return Err("lead_bind is only for lead panes".into());
            }
            if a.pty_id.is_some() {
                return Err(format!("lead {agent_id} is already bound to a terminal"));
            }
            a.pty_id = Some(pty_id);
            a.status = AgentStatus::Running;
            a.group.clone()
        };
        self.by_pty.lock_safe().insert(pty_id, agent_id.to_string());
        self.audit(&group_id, brand::AUDIT_ACTOR, "agent-bind", json!({ "agent": agent_id, "pty": pty_id }));
        crate::obs::breadcrumb("agent-bind", &format!("agent={agent_id} pty={pty_id} role=Lead"));

        let (Some(a), Some(g)) = (self.agent(agent_id), self.group(&group_id)) else {
            // The pane is bound and usable; only the kickoff is lost. Audited
            // rather than returned as an error for the reason
            // `open_manager_pane_at_launch` gives: a degraded pane the human can
            // still type into beats a launch that refused to happen.
            self.audit(&group_id, brand::AUDIT_ACTOR, "error", json!({
                "what": "lead kickoff skipped", "agent": agent_id,
                "detail": "the agent or its group vanished between the bind and the kickoff",
            }));
            return Ok(());
        };
        // No branch note (the kickoff's own arm says where a lead works, and it
        // is never given a worktree) and no persona: a lead group has no
        // workflow file, so no block in it can carry one.
        let kickoff = self.kickoff_prompt(&a, &g, "", None);
        if let Err(e) = self.deliver_prompt(agent_id, &kickoff, brand::AUDIT_ACTOR, Delivery::FreshKickoff) {
            self.audit(&group_id, brand::AUDIT_ACTOR, "error", json!({
                "what": "lead kickoff not delivered", "agent": agent_id, "err": e,
                "detail": "the pane is bound and usable; it did not receive its contract. See \
                           OrchRegistry::lead_bind for why this is not a launch failure.",
            }));
        }
        Ok(())
    }

    /// Every live delegate in a dead lead's group, ended (#2519).
    ///
    /// **A crash must cost what a deliberate close costs.** Closing a lead pane
    /// is a frontend gesture that ends the whole group, so a lead whose CLI
    /// simply dies must not leave its helpers running with nothing to report
    /// to: their `report` would resolve no root, their panes would sit under a
    /// tab whose lead is gone, and they would keep spending the human's tokens
    /// unattended.
    ///
    /// Goes straight through `mark_dead` after killing each pty, exactly as
    /// `end_group` does and for the same reason: that skips `on_pty_exit`'s
    /// orchestrator-notification path, and there is no orchestrator in a lead
    /// group to tell.
    ///
    /// Deliberately NOT `end_group` itself: that function audits as actor
    /// `human` and performs a whole orderly teardown (worktree cleanup when
    /// asked, the generated-agent-file reclaim, the pause marker), which is the
    /// right thing for a human's End-group click and an overstatement for a
    /// pane that crashed. What must happen either way is that no helper
    /// outlives its lead; the rest is the frontend's gesture to ask for.
    ///
    /// `ExitInitiator::LeadExit` on each child, because that is what ended them.
    /// It is what routes their exit notices to the audit log rather than at a
    /// prompt (`exit_notice_route`), which matters here more than for the other
    /// three initiators: the pane those notices would be typed into is the one
    /// whose death caused the exit.
    pub(in crate::orchestration) fn end_lead_children(&self, lead: &AgentEntry) {
        let children: Vec<(String, Option<u32>)> = self
            .agents
            .lock_safe()
            .values()
            .filter(|a| a.group == lead.group && a.id != lead.id && a.status != AgentStatus::Dead)
            .map(|a| (a.id.clone(), a.pty_id))
            .collect();
        if children.is_empty() {
            return;
        }
        let app = self.app.lock_safe().clone();
        let mut ended = Vec::new();
        for (id, pty) in children {
            if let (Some(app), Some(pty)) = (app.as_ref(), pty) {
                app.state::<crate::pty::PtyManager>().kill(pty);
            }
            self.record_exit_initiator(&id, ExitInitiator::LeadExit);
            self.mark_dead(&id, None);
            ended.push(id);
        }
        // The action keeps its name — it is a persisted audit word — and the
        // row says which class of root went, since a quick root (#3679) takes
        // its helpers with it by this same function.
        self.audit(&lead.group, brand::AUDIT_ACTOR, "lead-children-ended", json!({
            "lead": lead.id, "ended": ended, "root": lead.role.as_str(),
        }));
    }

    /// Is this group one a lead pane owns? See `lead_prepare`'s marker-file
    /// comment for why this is not a question about the roster.
    ///
    /// Reads the marker on disk rather than the live registry, so it answers
    /// for a group whose panes are all gone — which is the case that needs it,
    /// the resume of a dead lead group's child.
    #[doc(hidden)] // pub for integration tests
    pub fn is_lead_group(&self, group: &GroupId) -> bool {
        self.group_dir(group).join(LEAD_MARKER).is_file()
    }
    /// **Fork a delegate's session into a new agent pane** (#3318 F2) — the
    /// registry half of the `fork_session` MCP tool and the `orch_fork_agent`
    /// command.
    ///
    /// The new agent is an ordinary delegate of the SOURCE's block — same
    /// persona, CLI, model and capability class — whose session starts as the
    /// vendor's own fork of the source's (`CliCaps.fork`). It is
    /// [`Self::spawn_agent_full`] with a [`ForkSpawn`], so the cap, the
    /// spawn-rate backstop, the CLI pin, the MCP identity and the worktree cut
    /// are the ordinary spawn's; what is decided HERE is only what a fork adds:
    ///
    /// - **Refusals, all before anything is minted or cut:** an unknown or
    ///   foreign source (the `unknown agent` wording — no other group's ids
    ///   leak); an orchestrator, manager or lead source (a fork inherits its
    ///   source's block, and none of those three is a delegate — a lead's own
    ///   pane forks into a SOLO pane through [`Self::request_solo_fork`]
    ///   instead); a source a review or plan drive owns (the driver's ownership
    ///   ladder is per agent id, and a fork is a new agent it never briefed —
    ///   refusing is honest, inventing a third owner is not); a CLI whose seam
    ///   is `None` (the row's own note); a structured-driver block; and a
    ///   source with no recorded session (nothing to fork yet).
    /// - **The workspace:** a worker or reviewer source forks into a NEW
    ///   worktree cut from the source's own branch (`base`), so the child
    ///   starts where the parent's last commit is — `worktree: false` is
    ///   refused for those two classes exactly as `spawn_agent` refuses it
    ///   (#338/#359), since two agents sharing one checkout is #359's
    ///   incident. A planner never gets a worktree, as it never does.
    #[allow(clippy::too_many_arguments)]
    pub fn fork_agent(
        &self,
        group_id: &GroupId,
        requested_by: &str,
        source_id: &str,
        task: &str,
        worktree: Option<bool>,
        branch: Option<String>,
        name: &str,
    ) -> Result<AgentEntry, String> {
        let group = self.group(group_id).ok_or("unknown group")?;
        let src = self
            .agent(source_id)
            .filter(|a| &a.group == group_id)
            .ok_or_else(|| format!("unknown agent: {source_id}"))?;
        match src.role {
            Role::Orchestrator | Role::Manager => {
                return Err(format!(
                    "{} is this group's {} — a fork inherits its source's block, and that class is \
                     not a delegate anyone may open a second of. Fork a worker, reviewer or planner.",
                    src.id,
                    src.role.as_str()
                ))
            }
            Role::Lead => {
                return Err(format!(
                    "{} is a lead pane — the human's own. A lead's session forks into a standalone \
                     (Solo) pane, never a second lead: the lead asks for that with fork_session on its \
                     own id, and the human with the pane menu.",
                    src.id
                ))
            }
            // #3679. A fork inherits its source's block, so forking a quick
            // root would open a SECOND root in the run's group — outside the
            // cap and the spawn-rate limit, since a root is a fixture — and
            // `fork_session` is on the root's own surface, so the caller that
            // could ask is the root itself. Refused by class rather than left
            // to the wildcard below, which is the arm a new class lands in
            // when nobody decides.
            Role::Quick => {
                return Err(format!(
                    "{} is a quick run's own agent — a run has exactly one, and a fork of it \
                     would be a second. Fork a worker, reviewer or planner.",
                    src.id
                ))
            }
            _ => {}
        }
        // The drive refusals. `rd_owner` is asked for the ownership (every pane
        // a live drive opened, superseded ones included — the driver's ladder
        // is per agent id), and `rd_driven_panes` only for its FAILURE: an
        // unreadable drive record is not evidence that the pane is undriven,
        // the same fail-closed reading `kill_agent` takes on the same file.
        if let Some((pr, _)) = self.rd_owner(group_id, &src.id) {
            return Err(format!(
                "{} belongs to the review drive on PR #{pr} — the driver briefs and routes its panes \
                 by agent id, and a fork would be a new agent it never briefed. Fork it once the \
                 drive is done or held, or spawn a fresh agent.",
                src.id
            ));
        }
        if self.rd_driven_panes(group_id).is_err() {
            return Err(format!(
                "orrerix could not read this group's review-drive record, so it cannot tell whether a \
                 live drive owns {} — and an unreadable record is not evidence that nothing does. \
                 Nothing was forked.",
                src.id
            ));
        }
        if let Some(issue) = self.pd_owner(group_id, &src.id) {
            return Err(format!(
                "{} is the plan drive's planner for issue #{issue} — the driver routes its reports by \
                 agent id, and a fork would be a planner it never briefed. Fork it once the drive is \
                 done or held.",
                src.id
            ));
        }
        let block = group.guardrails.block(&src.block).cloned().ok_or_else(|| {
            format!(
                "{}'s block {:?} is no longer in this group's workflow, so a fork has no persona, CLI \
                 or class to inherit.",
                src.id, src.block
            )
        })?;
        let cli = workflow::cli_of(&block, &group.guardrails.agent_cli);
        if let Some(refusal) = fork_refusal(cli) {
            return Err(refusal);
        }
        if workflow::structured_harness_for(block.driver.as_deref(), cli).ok().flatten().is_some() {
            return Err(fork_structured_refusal(&block.id));
        }
        let parent_session = src
            .session_id
            .as_deref()
            .and_then(sanitize_session)
            .ok_or_else(|| {
                format!(
                    "{} has no recorded session yet, so there is nothing to fork — a CLI that mints its \
                     own id records it a few seconds after its first prompt. Try again once \
                     list_agents shows its session.",
                    src.id
                )
            })?;
        let dedicated = matches!(src.role, Role::Worker | Role::Reviewer);
        if dedicated && worktree == Some(false) {
            return Err(format!(
                "guardrail: a {r} fork always gets a dedicated worktree (#338/#359) — two agents \
                 sharing one checkout is exactly the conflict that rule exists for. Omit `worktree`.",
                r = src.role.as_str()
            ));
        }
        let use_worktree = dedicated || worktree.unwrap_or(false);
        // Cut from where the PARENT is: its own branch, when it has one that
        // EXISTS. A worktree pane's recorded branch was cut at its spawn; a
        // shared-repo pane's is only the name it was told to create, which it
        // may not have yet (CI caught exactly that: `cannot resolve base
        // "agent/w-5"`). A branch that is not there, or no branch at all (a
        // planner), leaves `base` to its default — the repo's default branch.
        let base = if use_worktree {
            src.branch.clone().filter(|b| crate::git::local_branch_exists(&group.repo, b))
        } else {
            None
        };
        let name = if sanitize_agent_name(name).is_empty() {
            format!("{} (fork)", src.name)
        } else {
            name.to_string()
        };
        self.spawn_agent_full(
            group_id,
            src.role,
            Some(src.block.clone()),
            &name,
            task,
            use_worktree,
            branch,
            base,
            None,
            None,
            None,
            None,
            Some(ForkSpawn {
                parent_agent: src.id.clone(),
                parent_name: src.name.clone(),
                parent_session,
                requested_by: requested_by.to_string(),
            }),
        )
    }

    /// **A lead forks its OWN pane into a Solo pane** (#3318 F2) — never a
    /// second lead.
    ///
    /// The one-root invariant (`docs/design/lead-pane.md`) is why this is not a
    /// `fork_agent`: a lead is its group's root, `kind_from_str` has no `lead`
    /// arm, and a fork inheriting the lead's block would be a second root. A
    /// Solo pane is what the human would get forking the same pane by hand,
    /// and that gesture already exists in the frontend — strip the lead's
    /// identity, re-mint a solo one through `orch_solo_prepare`, open the pane
    /// on the vendor's fork line. So this asks the frontend to run it on the
    /// lead's pane (`orch-fork-solo-request`) rather than building a second
    /// Solo-opening path here, and returns once the request is emitted: the
    /// Solo pane has no roster row in this group for a bind to wait on.
    ///
    /// Refused up front on the same facts the gesture would refuse on later —
    /// a lead CLI with no fork seam, or no recorded session — so the lead gets
    /// the sentence in its own turn rather than a toast in the human's.
    pub fn request_solo_fork(&self, group_id: &GroupId, lead_id: &str, name: &str) -> Result<String, String> {
        let group = self.group(group_id).ok_or("unknown group")?;
        let lead = self
            .agent(lead_id)
            .filter(|a| &a.group == group_id && a.role == Role::Lead)
            .ok_or_else(|| format!("unknown agent: {lead_id}"))?;
        let cli = group
            .guardrails
            .block(&lead.block)
            .map(|b| workflow::cli_of(b, &group.guardrails.agent_cli).to_string())
            .unwrap_or_else(|| group.guardrails.agent_cli.clone());
        if let Some(refusal) = fork_refusal(&cli) {
            return Err(refusal);
        }
        let session = lead.session_id.clone().ok_or_else(|| {
            "your pane has no recorded session yet, so there is nothing to fork. Try again after \
             your next turn."
                .to_string()
        })?;
        // The Solo pane is outside the delegate cap — it is the human's, not a
        // helper — but it is still a pane an agent's tool call opened, and a
        // runaway loop is possible in a human-driven pane too
        // (`docs/design/lead-pane.md`, Guardrails). So it takes the group's
        // spawn-rate backstop, recorded only when admitted, exactly as a
        // delegate spawn does.
        //
        // The slot is spent HERE, at the request, and not at the frontend's
        // ack below — deliberately. What the backstop bounds is an agent's
        // CALLS: a lead looping on `fork_session` is the runaway, whether or
        // not each call ends in an open pane, and a bound spent only on
        // success would let a loop whose opens all fail run unbounded.
        self.check_and_record_spawn(group_id, group.guardrails.max_spawns_per_hour)?;
        // A REQUEST, and worded as one (review round 1, rev-final 1): nothing
        // is open yet. Whether the frontend opened the Solo pane is recorded
        // by `record_solo_fork_outcome` when it acks — `agent-fork` beside
        // this row for an open, `agent-fork-failed` with the reason otherwise.
        //
        // The NAME (#3368) rides the request to the frontend, which opens the
        // pane under it — `fork_session(name)` used to stop here, so a lead's
        // named fork opened as `<lead> (fork)` whatever it asked for. Blank
        // means "the default", which the frontend derives from the lead pane's
        // own name exactly as a right-click fork does. One line, one field:
        // the same collapse the prompt applies, so a newline an agent passed
        // cannot reach a pane title, then `sanitize_agent_name` — the rule every
        // other pane name goes through, whose 40-character cap the frontend
        // mirrors (`sanitizePaneName`). Recorded on the request row too, so what
        // was asked for is on the log beside what became of it.
        let name = sanitize_agent_name(&name.split_whitespace().collect::<Vec<_>>().join(" "));
        self.audit(group_id, &lead.id, "agent-fork-requested", json!({
            "parent_agent": lead.id,
            "parent_session": session,
            "into": "solo",
            "cli": cli,
            "name": name,
        }));
        if let Some(app) = self.app.lock_safe().clone() {
            app.emit(
                "orch-fork-solo-request",
                json!({ "group_id": group_id, "agent_id": lead.id, "pty_id": lead.pty_id, "name": name }),
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(session)
    }

    /// Record what became of a lead's self-fork request (#3318 F2, review round
    /// 1): the frontend's ACK for `orch-fork-solo-request`, through
    /// `orch_fork_solo_result`.
    ///
    /// `request_solo_fork` writes `agent-fork-requested` before anything is
    /// open, so the log would otherwise say a fork was asked for and never
    /// whether it happened. `opened` writes the `agent-fork` row a delegate fork
    /// writes at spawn (`into: "solo"`); a failure writes `agent-fork-failed`
    /// with the frontend's reason — a pane that was not open in this window, a
    /// refusal the menu would have given, a failed open.
    ///
    /// Refused for anything but a live group's LEAD, named by the id the
    /// request carried: the ack is a statement about that lead's request, and
    /// the frontend is trusted but its payload is still checked (the id could
    /// have been reused, or the group ended, before the ack arrived).
    pub fn record_solo_fork_outcome(
        &self,
        group_id: &GroupId,
        lead_id: &str,
        opened: bool,
        detail: &str,
    ) -> Result<(), String> {
        let lead = self
            .agent(lead_id)
            .filter(|a| &a.group == group_id && a.role == Role::Lead)
            .ok_or_else(|| format!("unknown agent: {lead_id}"))?;
        let detail: String = detail.chars().take(500).collect();
        if opened {
            self.audit(group_id, brand::AUDIT_ACTOR, "agent-fork", json!({
                "agent": null,
                "parent_agent": lead.id,
                "parent_session": lead.session_id,
                "into": "solo",
            }));
        } else {
            self.audit(group_id, brand::AUDIT_ACTOR, "agent-fork-failed", json!({
                "parent_agent": lead.id,
                "into": "solo",
                "reason": detail,
            }));
        }
        Ok(())
    }
}
