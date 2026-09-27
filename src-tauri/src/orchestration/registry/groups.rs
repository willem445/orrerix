//! Group lifecycle: creating or reattaching to a group (`create_group` /
//! `create_group_ex`) and the orchestrator-CLI promotion it consults
//! (`promote_orchestrator_cli`), the live-group lookups (`group`,
//! `group_is_live`), and the end of a group's life (`group_summary`,
//! `end_group`, `group_state_exists`), as an `impl OrchRegistry` block
//! (#3498). The group model is `docs/design/orchestration.md`; the group id
//! as a path is `docs/design/groupid-and-path-roots.md`.

use super::*;

impl OrchRegistry {
    /// Create (or reattach to) the group for `repo`. State and audit history
    /// persist under the repo-derived group id; guardrails are refreshed from
    /// the new launch.
    ///
    /// A **fresh launch** — the human is at the launcher, and has just been shown
    /// what the advanced orchestrator would run. [`create_group_ex`] is the same
    /// thing for a resumed orchestrator session, which is a different question.
    ///
    /// [`create_group_ex`]: Self::create_group_ex
    pub fn create_group(&self, repo: &str, guardrails: Guardrails) -> Result<GroupInfo, String> {
        self.create_group_ex(repo, guardrails, Launch::Fresh)
    }

    /// The CLI this promote's orchestrator block **would** resolve to (#407
    /// rev-1 B1) — read-only, so a mismatch can be refused before any group
    /// dir, `group.json`, instruction file, merge-gate spec, marker re-seed or
    /// audit line exists. Without it the one refusal that needs a resolved
    /// roster was also the one refusal that could only fire after
    /// `create_group_ex` had already run to completion, which is not what
    /// "a refused promote leaves the running pane untouched" means.
    ///
    /// Deliberately not a second implementation of the resolution. It reuses
    /// the same primitives in the same order the launch itself will: the
    /// candidate-id scan from [`create_group_ex`](Self::create_group_ex),
    /// `load_group_file` for a reattach, the `load_workflow` + `clamped()` pair
    /// the launcher's own preview (`orch_workflow_preview`) reuses for exactly
    /// this reason, and `workflow::cli_of`. Callers hold `creation`, so the
    /// candidate it peeks is the one `create_group_ex` picks a moment later.
    ///
    /// `None` when the roster declares no orchestrator block at all — that is
    /// `register_orchestrator_pane`'s error to raise, not this one's.
    pub(in crate::orchestration) fn promote_orchestrator_cli(&self, repo: &str, caller: &Guardrails) -> Option<String> {
        let id = self.next_group_id(repo)?;
        let mut rails = caller.clone();
        if self.group_dir(&id).join("group.json").is_file() {
            // Reattach: the roster, its inherited CLI and the toggle come off
            // disk — the same three `create_group_ex` restores.
            if let Some((_, persisted)) = self.load_group_file(&id) {
                rails.blocks = persisted.blocks;
                rails.agent_cli = persisted.agent_cli;
                rails.advanced_orchestrator = persisted.advanced_orchestrator;
                // #1689: the workflow NAME travels with the roster, for the same
                // reason `agent_cli` does — a reattach that read the roster off
                // disk and then resolved the orchestrator's CLI from a different
                // file would answer for a group that does not exist.
                rails.workflow = persisted.workflow;
            }
        } else if caller.advanced_orchestrator {
            // A fresh group dir with the advanced box ticked: the repo's file is
            // what the launch will run, so it is what this must check. A broken
            // or absent file falls back to the caller's roster, exactly as the
            // launch does.
            if let Ok(Some(wf)) = load_active_workflow(repo, &rails) {
                rails.blocks = wf.blocks;
            }
        }
        let rails = rails.clamped();
        rails
            .block_for(Role::Orchestrator)
            .map(|b| workflow::cli_of(b, &rails.agent_cli).to_string())
    }

    /// [`create_group`](Self::create_group), told which kind of start this is.
    ///
    /// The distinction only matters for the advanced orchestrator (#222), and it
    /// matters a lot: **a resumed group runs the roster it was launched with.**
    /// See [`Launch`].
    #[doc(hidden)] // pub for integration tests (the resume pin is asserted on this)
    pub fn create_group_ex(
        &self,
        repo: &str,
        guardrails: Guardrails,
        launch: Launch,
    ) -> Result<GroupInfo, String> {
        let mut guardrails = guardrails.clamped();
        // Base id is repo-derived so a relaunch resumes the same state dir —
        // but a repo can host several *concurrent* orchestrations, and those
        // must never share a group (their orchestrators would receive each
        // other's worker reports). Take the first id without live agents.
        let id = self
            .next_group_id(repo)
            .ok_or_else(|| "could not derive a valid group id for this repo".to_string())?;
        let dir = self.group_dir(&id);
        fs::create_dir_all(dir.join("configs")).map_err(|e| e.to_string())?;
        let resumed = dir.join("group.json").is_file();

        // #2519 B1 (rev-final). A group id is claimed HERE and nowhere else, and
        // this is the one line that makes the lead marker mean "the group at this
        // id, AS IT EXISTS NOW, was minted by the toggle".
        //
        // The hazard is the one `end_group` clears the `paused` marker for, and
        // its neighbour states the general form: "a group id is chosen by
        // liveness and can be handed out again". `next_group_id` returns the
        // first candidate with no LIVE agent, and the group DIRECTORY is never
        // removed — so a lead group whose pane has died leaves its id, and its
        // marker, free for an ordinary orchestration to reattach to. Without
        // this line that ordinary group answers `is_lead_group()` for the rest of
        // its life and `resume_recorded_session` refuses every session in it,
        // citing a toggle nobody flipped — and `offersStartFresh` is false for
        // that kind, so the UI offers no way out either.
        //
        // **Unconditional rather than `Launch::Fresh`-only**, and the extra
        // coverage is the argument: a `Launch::Promote` reattaches a dormant
        // group by exactly the same id selection, so a promote onto a dead
        // lead's id inherits the same stale marker. A `Launch::Resume` cannot
        // reach a lead group at all — `resume_recorded_session` refuses one
        // before anything is created — so there is no case where clearing here
        // erases a fact that is still true. `lead_prepare` re-writes it after
        // its own mint succeeds, which is what keeps the marker one group wide.
        //
        // Best-effort like the `paused` remove it mirrors: a marker that could
        // not be deleted leaves the pre-existing refusal standing, which is the
        // direction this fails in either way.
        let _ = fs::remove_file(dir.join(LEAD_MARKER));

        // #407: a PROMOTE reattaching a dormant group brings the promote
        // modal's defaults with it — a right-click is not the launcher, and
        // there is no roster preview in it — so the ROSTER (and the toggle and
        // intake profile that travel with it) has to come back off disk here,
        // before every decision below reads them. A resume never needed this:
        // its caller loads `group.json` itself and hands the persisted
        // guardrails straight back in. Without it, promoting into a dormant
        // advanced group would silently replace the roster its human approved
        // with the built-in four — the same provenance violation the resume pin
        // exists to prevent, arriving from the other direction — and would
        // clear that group's merge gate on the way past (the toggle-off arm
        // below). The live-adjustable knobs are re-hydrated further down, where
        // both launch kinds share the same `resumed` gate.
        //
        // `agent_cli` travels with the roster (rev-1 N1). A built-in roster
        // persists every block's `cli` as `""` — "inherit the group default" —
        // so restoring `blocks` without it would hand a dormant copilot group's
        // blocks a claude default and quietly re-CLI every delegate it spawns
        // from then on, while the promote modal (which has no CLI field at all)
        // showed the human nothing. Restoring it is also the fail-closed
        // reading: a claude pane promoted into a copilot group now resolves an
        // orchestrator CLI that is not its own, and `promote_orchestrator_cli`
        // below refuses instead of composing `copilot --resume <claude uuid>`.
        if launch == Launch::Promote && resumed {
            if let Some((_, persisted)) = self.load_group_file(&id) {
                guardrails.blocks = persisted.blocks;
                guardrails.agent_cli = persisted.agent_cli;
                guardrails.advanced_orchestrator = persisted.advanced_orchestrator;
                guardrails.intake = persisted.intake;
                // #1689: and the name of the file all three came from. The
                // promote modal has no workflow picker, so without this a
                // reattached group would be re-pinned to `default` and the next
                // gate reload would arm a file the human never chose.
                guardrails.workflow = persisted.workflow;
            }
        }

        // The repo's declared roster AND its merge gate (#222), read ONLY when the
        // human turned the advanced orchestrator on **and this is a fresh launch**.
        //
        // With the toggle off — the default — the file is not even opened: this is
        // the whole promise that the default experience is byte-for-byte what it
        // was before #222, and the cheapest way to keep that promise is to not have
        // a code path.
        //
        // On a RESUME the file is not read for the roster either, and that is a
        // consent rule, not an optimization (rev-11 F2). The roster in `group.json`
        // is the one the human was shown in the launcher preview and approved. A
        // `git pull` between launch and resume must not be able to swap a delegate's
        // persona under a session they already consented to — the consent moment is
        // the launch, so the launch is what the roster is pinned to. Drift is
        // *audited* below, not applied; to run a changed workflow, launch a group.
        //
        // #255: set only on a fresh, valid workflow load — the one moment this
        // function actually knows the roster's structural agent requirement.
        // Checked against the resolved `max_agents` once every guardrail
        // override below (including the resume-cap override) has landed.
        let mut capacity: Option<workflow::CapacityRecommendation> = None;
        // #407: the one place `Launch` is inspected, so it is the one place a
        // promote's split consent moment can be decided — it needs `resumed`,
        // which only this function knows (the caller cannot know which
        // candidate id was free). See [`Launch::Promote`].
        let reads_workflow_file = match launch {
            Launch::Fresh => true,
            Launch::Resume => false,
            Launch::Promote => !resumed,
        };
        if guardrails.advanced_orchestrator && reads_workflow_file {
            // Three outcomes, and only the first changes anything:
            //   - a valid `.loomux/workflow.yml` → its blocks ARE the roster;
            //   - no file → the launcher's 4-block roster stands (turning the
            //     toggle on in a repo that declares nothing is a no-op, not an
            //     error — it is how you launch before you write the file);
            //   - a broken file → AUDITED AND SKIPPED. A repo file must never be
            //     able to stop a group from launching, so a validation failure
            //     falls back to the default roster rather than erroring out.
            //     Every problem is recorded, not just the first, so one look at
            //     the audit log fixes the file in one pass. The launcher shows
            //     the human the same findings *before* they hit Create.
            match load_active_workflow(repo, &guardrails) {
                Ok(Some(wf)) => {
                    // #255: derived from the roster + the (optional) merge gate
                    // while we still hold both — recorded here so a run's capacity
                    // assumptions are reconstructable from the audit log later, and
                    // checked against the resolved cap below.
                    let capacity_rec =
                        workflow::recommend_capacity(&wf.blocks, wf.gates.get("merge"));
                    self.audit(&id, brand::AUDIT_ACTOR, "workflow-loaded", json!({
                        "path": active_workflow_path(repo, &guardrails),
                        "workflow": guardrails.workflow.as_str(),
                        "name": wf.name,
                        "blocks": wf.blocks.iter().map(|b| json!({ "id": b.id, "kind": b.kind })).collect::<Vec<_>>(),
                        "gates": wf.gates.keys().collect::<Vec<_>>(),
                        "min_agents": capacity_rec.minimum,
                        "recommended_agents": capacity_rec.recommended,
                        "reviewers_needed": capacity_rec.reviewers_needed,
                        "intake_source": wf.intake.source.as_str(),
                    }));
                    // The declared merge gate (#222/#197) becomes the `merge_gate`
                    // spec file the gh shim enforces — or, when the file declares
                    // none, is cleared, so removing a gate from the workflow really
                    // removes it.
                    self.sync_merge_gate(&id, wf.gates.get("merge"));
                    // `merge` is the only gate loomux enforces. A gate under any other
                    // name parses (the schema is open) but does nothing — say so, rather
                    // than letting a `gates: { deploy: … }` clause look enforced.
                    let unenforced: Vec<&String> =
                        wf.gates.keys().filter(|k| k.as_str() != "merge").collect();
                    if !unenforced.is_empty() {
                        self.audit(&id, brand::AUDIT_ACTOR, "workflow-gate-unenforced", json!({
                            "gates": unenforced,
                            "note": "only gates.merge is enforced by this build — the rest are inert",
                        }));
                    }
                    guardrails.blocks = wf.blocks;
                    // #382 P1: same gate as the roster override above — the
                    // repo's declared intake profile takes effect only on a
                    // FRESH launch with the toggle on, the moment the human
                    // was shown the resolved roster/gate in the launcher
                    // preview. A resume never re-reads it (see the `else if`
                    // arm below); the persisted profile stands.
                    guardrails.intake = wf.intake;
                    // Re-run the roster normalization (model defaults follow each
                    // block's *effective* CLI, which the file may have changed).
                    guardrails = guardrails.clamped();
                    capacity = Some(capacity_rec);
                }
                // No workflow file: no gate. Clears a stale one from a previous
                // launch, so deleting `.loomux/workflow.yml` restores the pre-#222
                // flow exactly.
                Ok(None) => self.sync_merge_gate(&id, None),
                Err(errors) => {
                    self.audit(&id, brand::AUDIT_ACTOR, "workflow-invalid", json!({
                        "path": active_workflow_path(repo, &guardrails),
                        "errors": errors,
                        "action": "skipped — using the built-in roster",
                    }));
                    // A BROKEN workflow file does NOT clear an existing gate. The
                    // roster can safely fall back to the built-in one — every agent
                    // still spawns, which is #225's "a repo file must never block a
                    // launch". A gate is the opposite kind of thing: dropping it
                    // because the file that declares it stopped parsing would quietly
                    // *widen* what the group's agents may do, and a syntax error is
                    // not consent to merge unreviewed code. So the last known gate
                    // stands, loudly.
                    if self.merge_gate_path(&id).is_file() {
                        self.audit(&id, brand::AUDIT_ACTOR, "merge-gate-retained", json!({
                            "reason": "the workflow file is invalid — keeping the last known merge gate rather than failing open",
                        }));
                    }
                }
            }
        } else if guardrails.advanced_orchestrator {
            // A resume. The persisted roster stands — but if the file has moved on
            // since the launch, the human should be able to SEE that the group they
            // are looking at is not what their repo now says. Silence here would
            // make the pin indistinguishable from a stale read.
            //
            // **The merge gate is left untouched HERE, at this resume — but that no
            // longer means it stays pinned for the life of the session.** Before
            // #385 it did: nothing else ever re-read the file post-launch, so "this
            // branch doesn't re-read it either" meant the gate really was frozen to
            // whatever the human saw at launch, and a `git pull` between launch and
            // resume could never loosen it (drop a reviewer, remove the clause
            // entirely) without a further explicit human action (a relaunch, or the
            // live toggle). That guarantee is gone. `run_workflow_gate_reload`'s
            // periodic pass treats a resumed group exactly like any other live
            // advanced-orchestrator group — it will read whatever's CURRENTLY on
            // disk and re-arm the gate within `WORKFLOW_GATE_POLL_INTERVAL`, git
            // pull or hand-edit, loosening or tightening, with no further consent
            // moment required. That is a decided, accepted risk (#459), not an
            // oversight: the human's call was that an agent — or anything else with
            // filesystem access to the repo — editing `.loomux/workflow.yml` is
            // usually legitimate, and defeating the gate this way already requires
            // the kind of repo write access that is a bigger compromise than the
            // gate itself. What's still guaranteed, unconditionally, is fail-closed:
            // a malformed or vanished file always retains the last-known gate rather
            // than opening it — see `OrchRegistry::reload_merge_gate_if_changed`.
            // The drift audit below still tells the human the repo has moved on;
            // it's the "and nothing acts on that until you do" half that no longer
            // holds.
            self.audit_workflow_drift(&id, repo, &guardrails);
            // #255 (rev-1 B2 / rev-2 non-blocking #1): the roster and gate are
            // PINNED on a resume, not re-read — but they still describe a real
            // structural minimum, and a resumed session can outgrow it exactly as
            // a fresh one can (a human lowers the live cap mid-run, or just never
            // raised it to begin with). `guardrails.blocks` is already the
            // persisted, clamped roster; the gate file on disk is whatever this
            // group most recently had armed — the original launch's write, or a
            // later live toggle or background reload from that same prior session
            // (see the comment above: post-#385, that file no longer necessarily
            // matches the ORIGINAL launch) — and this branch deliberately never
            // re-reads `.loomux/workflow.yml` itself to derive it. Both are exactly
            // what the pinned session is running under, so deriving from them here
            // (rather than skipping the check because "the file wasn't re-read") is
            // correct, not a re-read.
            //
            // Gated on `roster_is_custom`: this whole feature is about a DECLARED
            // workflow's structural need, and a group with no workflow file has
            // none to derive — its Fresh launch (the `Ok(None)` arm above) leaves
            // `capacity` at `None` for exactly that reason. Without this check a
            // resumed built-in-roster group (advanced toggle on, no `workflow.yml`)
            // would get capacity audits its own fresh launch never emitted, and the
            // note would say "this workflow's minimum" about a group that has none.
            if workflow::roster_is_custom(&guardrails.blocks) {
                capacity = Some(workflow::recommend_capacity(&guardrails.blocks, self.merge_gate(&id).as_ref()));
            }
        } else {
            // The toggle is off, so the workflow is not running — and neither is its
            // gate. Clearing it here is what makes "the default experience is
            // byte-for-byte pre-#222" true for the *merge path* too, and it is what
            // stops a gate declared under an earlier advanced launch of this same
            // group dir from outliving the toggle that authorized it.
            self.sync_merge_gate(&id, None);
            if Path::new(repo).join(active_workflow_path(repo, &guardrails)).is_file() {
                // The repo declares a workflow and this group is deliberately not
                // running it. Say so in the trail: "my workflow file did nothing" is
                // otherwise a silent, and very confusing, non-event.
                self.audit(&id, brand::AUDIT_ACTOR, "workflow-ignored", json!({
                    "path": active_workflow_path(repo, &guardrails),
                    "reason": "the advanced orchestrator is off for this group",
                    "action": "using the built-in roster",
                }));
            }
        }
        // The live-agent cap is adjustable mid-session (`set_max_agents`) and
        // persisted, so it's a durable human choice — like the pause/notify
        // markers re-seeded below. On resume, prefer the persisted cap over the
        // caller's param: the launcher hardcodes its default (4) and can't
        // pre-fill from group.json, so without this a relaunch would silently
        // revert an on-the-fly adjustment. Other guardrails still refresh from
        // the launch (only the cap is live-adjustable). Read before the write
        // below overwrites the file.
        if resumed {
            if let Some((_, persisted)) = self.load_group_file(&id) {
                guardrails.max_agents = persisted.max_agents.clamp(1, MAX_AGENTS_CEILING);
                // The autonomy budget (#83) is likewise live-adjustable and
                // persisted, so a relaunch must keep the human's set value rather
                // than reverting to the launcher's param.
                guardrails.autonomy_budget_tokens = persisted.autonomy_budget_tokens;
                // Same for the live-adjustable idle-tick window — re-normalize the
                // persisted value (0/absent from older group.json → default) since
                // this overwrite lands after the top-of-fn `clamped()`.
                guardrails.idle_tick_minutes = if persisted.idle_tick_minutes == 0 {
                    DEFAULT_IDLE_TICK_MINUTES
                } else {
                    persisted.idle_tick_minutes.clamp(1, MAX_IDLE_TICK_MINUTES)
                };
                guardrails.idle_activity_floor_bytes = if persisted.idle_activity_floor_bytes == 0 {
                    DEFAULT_IDLE_ACTIVITY_FLOOR_BYTES
                } else {
                    persisted.idle_activity_floor_bytes.clamp(1, MAX_IDLE_ACTIVITY_FLOOR_BYTES)
                };
                // #496: no live setter yet (same precedent as `idle_tick_fallback_minutes`
                // just below), but still a hand-editable, persisted guardrail — honor it
                // on resume rather than letting a Fresh-shaped caller value (which has no
                // way to express "I set this on disk before") silently reset it.
                //
                // Review fix (round 1): the floor must apply to the UNSET (0 → default)
                // case too, not just an explicit persisted value — `clamped()` floors
                // both, because the floor is dynamic (`idle_tick_minutes`, not a static
                // in-range default like `idle_activity_floor_bytes`'s sibling pattern
                // this was copied from). Resolve 0 → default FIRST, then clamp
                // unconditionally, exactly like `clamped()` does, so a group with
                // `idle_tick_minutes` above the 15m default (the entire pre-#500
                // installed base, which has no `idle_tick_input_defer_max_minutes` key
                // at all) gets the SAME bound on resume that a fresh launch gives it —
                // never a tighter one a resumed group's own docs don't promise.
                guardrails.idle_tick_input_defer_max_minutes = (if persisted.idle_tick_input_defer_max_minutes == 0 {
                    DEFAULT_IDLE_TICK_INPUT_DEFER_MAX_MINUTES
                } else {
                    persisted.idle_tick_input_defer_max_minutes
                })
                .clamp(guardrails.idle_tick_minutes, MAX_IDLE_TICK_MINUTES);
                // Compact-nudge (#287) is likewise live-adjustable and persisted;
                // 0 is a real "off" here (not "unset"), so just re-cap it.
                guardrails.compact_nudge_minutes =
                    persisted.compact_nudge_minutes.min(MAX_COMPACT_NUDGE_MINUTES);
                // Re-run the same role normalization `clamped()` applies (this
                // overwrite lands after that top-of-fn call, straight from a
                // raw disk read that never went through it).
                guardrails.compact_nudge_roles = canonicalize_compact_nudge_roles(persisted.compact_nudge_roles);
                guardrails.compact_nudge_min_context_percent =
                    persisted.compact_nudge_min_context_percent.map(|p| p.min(100));
                guardrails.compact_context_threshold_percent = persisted
                    .compact_context_threshold_percent
                    .min(MAX_COMPACT_CONTEXT_THRESHOLD_PERCENT);
                // Idle-tick intake gate (#332/#429, rev-33 finding): these two were
                // missing from this re-hydration entirely, so a launcher relaunch
                // (Launch::Fresh, caller Guardrails from the launcher UI, which has
                // no field for either) silently wiped a hand-edited `Some(0)`
                // opt-out or a custom fallback cadence back to the launcher's
                // default on every relaunch — while a session-browser resume
                // (which never calls this function with fresh caller guardrails at
                // all) kept it. Both are now live-adjustable-and-persisted exactly
                // like `idle_tick_minutes`/`idle_activity_floor_bytes` above: honor
                // the on-disk value, re-clamped the same way `clamped()` would.
                guardrails.intake_poll_minutes = persisted.intake_poll_minutes.map(|explicit| {
                    if explicit == 0 { 0 } else { explicit.clamp(1, MAX_INTAKE_POLL_MINUTES) }
                });
                guardrails.idle_tick_fallback_minutes = if persisted.idle_tick_fallback_minutes == 0 {
                    DEFAULT_IDLE_TICK_FALLBACK_MINUTES
                } else {
                    persisted.idle_tick_fallback_minutes.clamp(MIN_IDLE_TICK_FALLBACK_MINUTES, MAX_IDLE_TICK_FALLBACK_MINUTES)
                };
                // #864's ceiling rides the same re-hydration for the same
                // reason: it is hand-edited into group.json, and a Fresh-shaped
                // relaunch (launcher guardrails, which have no field for it)
                // would otherwise silently reset it. Resolve 0 → default FIRST,
                // then clamp unconditionally against the fallback resolved just
                // above — `clamped()`'s exact order, because this floor is
                // dynamic, not a static in-range default.
                guardrails.idle_tick_fallback_max_minutes = (if persisted.idle_tick_fallback_max_minutes == 0 {
                    DEFAULT_IDLE_TICK_FALLBACK_MAX_MINUTES
                } else {
                    persisted.idle_tick_fallback_max_minutes
                })
                .clamp(guardrails.idle_tick_fallback_minutes, MAX_IDLE_TICK_FALLBACK_MINUTES);
            }
        }
        // #255: advisory only — never override a cap the human set. A launcher
        // warning (surfaced from `orch_workflow_preview`, computed the same way)
        // is meant to catch this *before* Create; these audit records are the
        // durable trail for a launch that went ahead anyway — e.g. resumed with a
        // persisted cap the file has since outgrown. #259: `set_max_agents` runs
        // the identical check on every live lowering of the cap, via
        // `audit_capacity_shortfall` below.
        if let Some(rec) = &capacity {
            self.audit_capacity_shortfall(&id, &guardrails.blocks, guardrails.max_agents, rec);
        }
        let info = GroupInfo { id: id.clone(), repo: repo.to_string(), guardrails };
        // Atomic replace: group.json is identity-critical (a truncated file
        // breaks the rejoin path), so a failed/interrupted write must leave the
        // prior file intact rather than half-written (#133). Matches the
        // crash-safe pattern `persist_max_agents` already uses for this file.
        // Includes the #83 autonomous guardrails.
        let body = serde_json::to_string_pretty(&json!({
            "group_id": info.id,
            "repo": info.repo,
            "created_ms": now_ms(),
            "guardrails": {
                "max_agents": info.guardrails.max_agents,
                "agent_cli": info.guardrails.agent_cli,
                // #222: the roster replaces the eight flat per-role fields. The
                // reader still understands the old shape (`read_blocks`), so a
                // group.json from 0.8.0 keeps loading; nothing writes it again.
                "blocks": blocks_json(&info.guardrails.blocks),
                // #222: whether this group runs the repo's workflow file. Absent
                // from an older group.json → false on read, which is exactly what
                // that group was: a built-in roster.
                "advanced_orchestrator": info.guardrails.advanced_orchestrator,
                // #1689: the workflow file this group runs, pinned in the SAME
                // atomic write as the roster it produced — a name that could
                // disagree with `blocks` after a crash would leave nothing on
                // disk saying which the human consented to.
                "workflow": info.guardrails.workflow.as_str(),
                "auto_ops": info.guardrails.auto_ops,
                "idle_kill_minutes": info.guardrails.idle_kill_minutes,
                "max_spawns_per_hour": info.guardrails.max_spawns_per_hour,
                "watchdog_stall_minutes": info.guardrails.watchdog_stall_minutes,
                "autonomy_budget_tokens": info.guardrails.autonomy_budget_tokens,
                "idle_tick_minutes": info.guardrails.idle_tick_minutes,
                "idle_activity_floor_bytes": info.guardrails.idle_activity_floor_bytes,
                "idle_tick_input_defer_max_minutes": info.guardrails.idle_tick_input_defer_max_minutes,
                "compact_nudge_minutes": info.guardrails.compact_nudge_minutes,
                "compact_nudge_roles": info.guardrails.compact_nudge_roles,
                "compact_nudge_min_context_percent": info.guardrails.compact_nudge_min_context_percent,
                "compact_context_threshold_percent": info.guardrails.compact_context_threshold_percent,
                "context_window_tokens_override": info.guardrails.context_window_tokens_override,
                // #382 P1: the resolved intake profile, so a restart and the
                // #332 host poller both read exactly what this launch resolved.
                "intake": intake_json(&info.guardrails.intake),
                "intake_poll_minutes": info.guardrails.intake_poll_minutes,
                "idle_tick_fallback_minutes": info.guardrails.idle_tick_fallback_minutes,
                "idle_tick_fallback_max_minutes": info.guardrails.idle_tick_fallback_max_minutes,
            },
        }))
        .unwrap();
        atomic_write(&dir.join("group.json"), body.as_bytes()).map_err(|e| e.to_string())?;
        self.write_instruction_files(&info)?;
        // A pause is a durable human safety action: re-seed it from the
        // marker file so a resumed group stays paused across restarts.
        if dir.join("paused").is_file() {
            self.paused.lock_safe().insert(id.clone());
        }
        // Desktop-notification opt-in is likewise a durable per-group choice.
        if dir.join("notify").is_file() {
            self.notify_groups.lock_safe().insert(id.clone());
        }
        // The #260 minimize-on-spawn opt-OUT is likewise a durable per-group
        // choice, re-seeded the same way.
        if dir.join("spawn_expanded").is_file() {
            self.spawn_expanded_groups.lock_safe().insert(id.clone());
        }
        // The #1151 needs-you migration: a board that was already holding
        // demo-gated rows before the item registry existed gets its items
        // synthesized here, exactly once ever, guarded by its own marker like
        // every durable per-group fact above it. Group load rather than the
        // panel's read, deliberately — `orch_needs_you_list` is a `viewer`-tier
        // command and must not be able to write a file. Best-effort: a group
        // must still load if its items file will not.
        self.migrate_demo_items(&id);
        // Autonomous mode (#83) and the auto-merge gate are durable per-group
        // choices too; re-seed them so a resumed group keeps ticking (and its
        // budget anchor, stored in the marker's content) across restarts. Audit
        // each resume so a persisted consent marker silently resuming autonomy/
        // auto-merge is at least *visible* in the trail (belt-and-suspenders for
        // the L2 consent-boundary concern — a marker that shouldn't be here shows
        // up rather than resuming invisibly).
        // A budget suspension takes precedence over the enable marker: if a failed
        // suspension left the `autonomous` marker on disk, the co-written
        // `autonomy_suspended` marker (rev-49) must still force the group back OFF at
        // restart — never silently resume ticking past a spent budget. The group
        // then reads as suspended (audited below + `orch_autonomy.suspended`).
        if dir.join("autonomy_suspended").is_file() {
            self.audit(&id, brand::AUDIT_ACTOR, "autonomous-suspended-resumed",
                json!({ "from": "marker" }));
        } else if dir.join("autonomous").is_file() {
            self.autonomous_groups.lock_safe().insert(id.clone());
            self.audit(&id, brand::AUDIT_ACTOR, "autonomous-resumed",
                json!({ "from": "marker", "budget_anchor_tokens": self.autonomy_anchor(&id) }));
        }
        // Auto-merge re-seed, with the #83 dependency reconciled: auto-merge is
        // valid ONLY alongside autonomous mode. A stale `auto_merge` marker without
        // a live `autonomous` marker (an older group predating the dependency, or a
        // hand-edited state dir) is force-cleared on read and audited, so the
        // enforced gate can never see the auto_merge-on/autonomous-off combo.
        if dir.join("auto_merge").is_file() {
            if self.autonomous_groups.lock_safe().contains(&id) {
                self.auto_merge_groups.lock_safe().insert(id.clone());
                self.audit(&id, brand::AUDIT_ACTOR, "auto-merge-resumed", json!({ "from": "marker" }));
            } else {
                let _ = remove_marker(&dir.join("auto_merge"));
                self.audit(&id, brand::AUDIT_ACTOR, "auto-merge-off",
                    json!({ "reason": "reconcile-autonomous-off" }));
            }
        }
        // Auto-release re-seed with the same dependency reconcile (independent of
        // auto_merge): valid only alongside autonomous; a stale marker is cleared.
        if dir.join("auto_release").is_file() {
            if self.autonomous_groups.lock_safe().contains(&id) {
                self.auto_release_groups.lock_safe().insert(id.clone());
                self.audit(&id, brand::AUDIT_ACTOR, "auto-release-resumed", json!({ "from": "marker" }));
            } else {
                let _ = remove_marker(&dir.join("auto_release"));
                self.audit(&id, brand::AUDIT_ACTOR, "auto-release-off",
                    json!({ "reason": "reconcile-autonomous-off" }));
            }
        }
        // Full autonomy re-seed (#778) with the same dependency reconcile. It carries
        // the sharper consequence of the three: a stale `full_autonomy` marker without
        // a live `autonomous` one would come back with the start default INVERTED —
        // every open issue eligible — on consent nobody renewed. So it is cleared and
        // audited on read, and only re-seeded beside a live autonomous marker. The
        // goal rides in the resume audit because it is the parameter that qualified
        // the consent, and a resume that doesn't name it is not visible enough.
        if dir.join("full_autonomy").is_file() {
            if self.autonomous_groups.lock_safe().contains(&id) {
                self.full_autonomy_groups.lock_safe().insert(id.clone());
                self.audit(&id, brand::AUDIT_ACTOR, "full-autonomy-resumed",
                    json!({ "from": "marker", "goal": self.full_autonomy_goal(&id) }));
            } else {
                let _ = remove_marker(&dir.join("full_autonomy"));
                self.audit(&id, brand::AUDIT_ACTOR, "full-autonomy-off",
                    json!({ "reason": "reconcile-autonomous-off" }));
            }
        }
        // Supervised dangerous mode re-seed (#83): valid only while NOT autonomous
        // (mutually exclusive). If both markers survived a hand-edit, autonomous
        // wins and the stale dangerous marker is cleared + audited.
        if dir.join("dangerous_mode").is_file() {
            if self.autonomous_groups.lock_safe().contains(&id) {
                let _ = remove_marker(&dir.join("dangerous_mode"));
                self.audit(&id, brand::AUDIT_ACTOR, "dangerous-mode-off",
                    json!({ "reason": "reconcile-autonomous-on" }));
            } else {
                self.dangerous_groups.lock_safe().insert(id.clone());
                self.audit(&id, brand::AUDIT_ACTOR, "dangerous-mode-resumed", json!({ "from": "marker" }));
            }
        }
        self.groups.lock_safe().insert(id.clone(), info.clone());
        // #1042 slice B — engine-derived declaration, on BOTH the create and the
        // resume path (this function is both), because a resumed group's panes
        // browse the same checkout a fresh one's do. Best-effort: the group
        // exists either way, and a checkout that has since been deleted or
        // renamed is a root the later command should refuse, not a reason to
        // fail the resume.
        //
        // On the create path `info.repo` originated as a caller argument, which
        // is the laundering shape slice C closes by making the orchestration
        // `repo` boundaries resolve before they reach here — see the note in
        // `crate::rootreg`'s module docs. Inert until then: nothing enforces and
        // no wire exists.
        crate::rootreg::admit_derived(&self.roots, &info.repo);
        self.audit(&id, brand::AUDIT_ACTOR, if resumed { "group-resume" } else { "group-create" },
            json!({ "repo": repo, "max_agents": info.guardrails.max_agents,
                    "blocks": blocks_json(&info.guardrails.blocks) }));
        Ok(info)
    }

    pub fn group(&self, id: &str) -> Option<GroupInfo> {
        self.groups.lock_safe().get(id).cloned()
    }

    /// A group is live while any of its agents is not dead.
    pub(in crate::orchestration) fn group_is_live(&self, id: &str) -> bool {
        self.agents
            .lock_safe()
            .values()
            .any(|a| a.group == id && a.status != AgentStatus::Dead)
    }

    // ---------- lifecycle: group summary & end-orchestration ----------

    /// #993 S3 publishes each agent's detected model, effort and context-window
    /// reading additively in its `context` object; roster picks remain available
    /// when no live reading exists.
    ///
    /// A one-glance summary of a group's live agents for the lifecycle panel:
    /// how many are up, the role breakdown, and uptime (per agent and for the
    /// group as a whole, measured from the earliest-started live agent — the
    /// orchestrator in practice). Also reports the paused flag so the panel can
    /// compose pause and end-orchestration sanely.
    pub fn group_summary(&self, group: &GroupId) -> Value {
        let now = now_ms();
        let live: Vec<AgentEntry> = self
            .agents
            .lock_safe()
            .values()
            .filter(|a| a.group == group && a.status != AgentStatus::Dead)
            .cloned()
            .collect();
        let (mut orch, mut worker, mut reviewer, mut planner, mut manager, mut lead) =
            (0u32, 0u32, 0u32, 0u32, 0u32, 0u32);
        let mut earliest: Option<u64> = None;
        // Production bug fix (PR #329 round 7): same override this group's
        // escalation threshold uses (`agent_context_percents`) — one shared
        // denominator, never two independently-guessed ones.
        let g = self.group(group);
        let context_window_override = g.as_ref().and_then(|g| g.guardrails.context_window_tokens_override);
        let mut list: Vec<Value> = live
            .iter()
            .map(|a| {
                match a.role {
                    Role::Orchestrator => orch += 1,
                    Role::Worker => worker += 1,
                    Role::Reviewer => reviewer += 1,
                    Role::Planner => planner += 1,
                    Role::Manager => manager += 1,
                    // A solo pane never belongs to a real orchestration
                    // group's summary — it lives in `__solo__` — but the
                    // match must stay exhaustive.
                    Role::Solo => {}
                    // #2519. A lead IS in a real group (it is that group's
                    // root), so it gets a tally rather than the solo pane's
                    // no-op — and a `roles` object that silently omitted it
                    // would tell the lifecycle panel a lead group has zero
                    // agents in it while a pane is plainly running.
                    Role::Lead => lead += 1,
                }
                earliest = Some(earliest.map_or(a.started_ms, |e| e.min(a.started_ms)));
                let declared = g.as_ref().and_then(|g| g.guardrails.blocks.iter().find(|b| b.id == a.block));
                // #993 S3: `None` when only the Claude table could have
                // answered for a source it does not describe (opencode) — the
                // panel then shows tokens without a percent.
                let window = crate::modelstate::published_window(
                    effective_context_window_tokens(
                        context_window_override,
                        a.last_context_window,
                        a.last_context_window_rounded,
                        a.last_context_model.as_deref(),
                        a.last_context_tokens,
                    ),
                    a.last_context_source,
                    a.last_context_window,
                );
                json!({
                    "id": a.id, "name": a.name, "role": a.role,
                    // The block this agent IS (#222). Equal to the role for the
                    // built-in roster, so the group panel shows nothing new for a
                    // default group — and shows `rev-security` rather than a
                    // second anonymous "REV" chip for a workflow group, which is
                    // the whole point of declaring the reviewers separately.
                    "block": a.block,
                    "task": a.task, "idle_since_ms": a.idle_since_ms,
                    "uptime_ms": now.saturating_sub(a.started_ms),
                    // Lifecycle-panel surfacing (PR #329 round 6): live
                    // compaction-state-machine phase + last-known context
                    // usage, straight from tracked state — no extra
                    // transcript read per poll (see `last_context_tokens`'s
                    // doc).
                    "compaction": compaction_status(
                        a.compact_pending,
                        a.compact_pending_trusted,
                        a.compact_seen_busy,
                        a.compact_reinject_attempted_ms,
                        a.compact_reinject_attempts,
                        a.compact_last_lost_reason.as_deref(),
                        a.compact_last_lost_ms,
                        now,
                        a.compact_pending_evidence,
                        a.compact_last_ack,
                        a.compact_last_ack_ms,
                    ),
                    "context": {
                        "tokens": a.last_context_tokens,
                        // Production bug fix (PR #329 round 7): model-aware
                        // window (`effective_context_window_tokens`) instead
                        // of a flat 200K assumption — see its doc.
                        "percent": a.last_context_tokens.zip(window).map(|(t, (w, _))| context_percent_used(t, w)),
                        "window_tokens": a.last_context_tokens.and(window).map(|(w, _)| w),
                        "window_source": a.last_context_tokens.and(window).map(|(_, s)| s.as_str()),
                        "model": a.last_context_model,
                        "effort": a.last_context_effort,
                        "source": a.last_context_source.map(|s| s.as_str()),
                        "declared": {
                            "model": declared.map(|b| b.model.as_str()).unwrap_or(""),
                            "effort": declared.map(|b| b.effort.as_str()).unwrap_or(""),
                        },
                    },
                })
            })
            .collect();
        list.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        // `g` is the single group-record read shared by roster declarations
        // and the manager-declared flag below; this is a polling path.
        json!({
            "group": group,
            "live_agents": live.len(),
            // The current adjustable cap and how many delegates count against
            // it, so the UI can show the stepper's value and warn when a lower
            // cap would (harmlessly) block spawns until attrition. Must match
            // `live_delegate_count` (the value enforcement actually reads).
            //
            // Derived through `counts_against_max_agents`, NOT by summing the
            // per-class tallies below (#1161 M3 review B1). The two spellings
            // agree today, and that is exactly why the hand-sum was wrong to
            // keep: a new `Role` forces a new arm in the exhaustive
            // `match` above — the compiler sees to that — while
            // `worker + reviewer + planner` silently omitted it, so this panel
            // would under-report the number `spawn_agent_bound` enforces and
            // the cap-below-live warning would go quiet at exactly the cap
            // where spawns start failing. The predicate defaults a new class to
            // COUNTED, so routing through it makes the drift impossible rather
            // than merely unlikely. Same `live` set and same filter as
            // `live_delegate_count`, so the panel cannot disagree with the
            // guardrail it describes.
            //
            // The per-class breakdown below still reports `manager`: the human
            // is told the pane is live, and told it is not spending a slot.
            "max_agents": g.as_ref().map(|g| g.guardrails.max_agents),
            "compact_context_threshold_percent": g.as_ref().map(|g| g.guardrails.compact_context_threshold_percent),
            "compact_nudge_minutes": g.as_ref().map(|g| g.guardrails.compact_nudge_minutes),
            "compact_nudge_min_context_percent": g.as_ref().map(|g| g.guardrails.compact_nudge_min_context_percent.unwrap_or(50)),
            "live_delegates": live.iter().filter(|a| counts_against_max_agents(a.role)).count(),
            "paused": self.is_paused(group),
            "uptime_ms": earliest.map(|e| now.saturating_sub(e)),
            "roles": { "orchestrator": orch, "worker": worker, "reviewer": reviewer, "planner": planner, "manager": manager, "lead": lead },
            // Whether the roster this group is RUNNING declares a manager block
            // at all (#1433, #1161 M5). Beside `roles.manager`, which counts LIVE
            // ones, because the panel's question is the difference between the
            // two: a group that declares one and has none is a human whose own
            // interface to this group is not there — because the launch open
            // failed, or because the pane died or was closed — and nothing
            // automatic reopens it, deliberately (see `docs/design/manager.md`).
            // A group that declares none is the overwhelmingly common case and
            // has nothing to say.
            //
            // Read off the RESOLVED roster rather than the repo's file: a group
            // resumes on the roster it launched with and never re-reads
            // `.loomux/workflow.yml`, so the file is not what this group is
            // running.
            "manager_declared": g
                .as_ref()
                .map(|g| g.guardrails.block_for(Role::Manager).is_some())
                .unwrap_or(false),
            "agents": list,
        })
    }

    /// End a whole orchestration: kill every one of the group's agents (the
    /// orchestrator included — unlike `kill_agent`, which protects it), and,
    /// when asked, remove the agents' worktrees. Human-initiated and
    /// destructive: it is a Tauri command only (never an MCP tool an agent
    /// could call on itself), audited as actor `human`, and the frontend
    /// confirms before invoking. Composes with a paused group: killing works
    /// regardless of pause, and the pause marker is cleared so the teardown is
    /// total — a later relaunch on the same repo won't inherit a stale pause.
    pub fn end_group(&self, group: &GroupId, cleanup_worktrees: bool) -> Result<Value, String> {
        // Snapshot every member (all statuses): already-dead workers may still
        // have a worktree on disk that cleanup should reclaim.
        let members: Vec<AgentEntry> = self
            .agents
            .lock_safe()
            .values()
            .filter(|a| a.group == group)
            .cloned()
            .collect();
        if members.is_empty() {
            return Err("no such group (no agents ever registered here)".into());
        }
        let app = self.app.lock_safe().clone();

        // Kill the live ones. Kill the pty (best-effort) then mark the entry
        // dead directly — mark_dead is idempotent against the async pty-exit,
        // and going straight through it avoids the orchestrator-notification
        // path in on_pty_exit (there is no orchestrator left to tell).
        let mut killed = Vec::new();
        for a in &members {
            if a.status == AgentStatus::Dead {
                continue;
            }
            if let (Some(app), Some(pty)) = (app.as_ref(), a.pty_id) {
                app.state::<crate::pty::PtyManager>().kill(pty);
            }
            // #3443: not `mark_dead` — that would reclaim reviewer worktrees
            // behind `cleanup_worktrees`' back, and race the removal below.
            self.mark_dead_keeping_workspace(&a.id, None);
            killed.push(a.id.clone());
        }

        // Optionally reclaim the worktrees. Resolve the repo (from memory or
        // group.json) so `git worktree remove` runs from the main checkout.
        let mut worktrees_removed = Vec::new();
        let mut worktree_errors = Vec::new();
        if cleanup_worktrees {
            let repo = self
                .group(group)
                .map(|g| g.repo)
                .or_else(|| self.load_group_file(group).map(|(r, _)| r));
            if let Some(repo) = repo {
                let cwds: Vec<String> = members.iter().map(|a| a.cwd.clone()).collect();
                for path in worktree_cleanup_targets(&repo, &cwds) {
                    match crate::git::git_worktree_remove(&repo, &path) {
                        Ok(()) => worktrees_removed.push(path),
                        Err(e) => worktree_errors.push(json!({ "path": path, "error": e })),
                    }
                }
            }
        }

        // Sweep the steering-strip image attachments (#72): they're only useful
        // while the group's agents are live, so teardown reclaims the scratch
        // dir alongside the worktrees. Best-effort — a leftover screenshot must
        // never block a group from ending. This includes any that were queued
        // but never sent (removed chips / abandoned drafts), so the cheap
        // policy is simply "cleaned up on group end", no per-image bookkeeping.
        let _ = fs::remove_dir_all(self.attachments_dir(group));

        // Lock state is per-group and in-memory (#858). Every member is dead by
        // now, so `mark_dead` has already released every hold and queue entry —
        // what is left is the empty table itself, which `locks_tick` would
        // otherwise iterate for the rest of the process's life. Dropped here
        // rather than left to accumulate (rev-lead, PR #859 finding 8): the
        // group is over, and a table keyed on an id nothing can reach again is
        // not state, it is residue. `paused_locks_since` goes with it so a
        // relaunch on the same repo cannot inherit a stale pause credit, which
        // is the same reason the pause marker is cleared below.
        self.locks.lock_safe().remove(group);
        self.paused_locks_since.lock_safe().remove(group);

        // rev-4 review (N4; widened round #417 correction 6 for Claude's own
        // generated files): reclaim any generated custom-agent files this
        // group ever got — `write_copilot_agent_file` (#416) and
        // `write_claude_agent_file` (round 6) — otherwise they accumulate in
        // `~/.copilot/agents`/`~/.claude/agents` forever and clutter each
        // CLI's own agent list with dead groups' names.
        //
        // #502 widened this from a per-MEMBER name list to a scan of the two
        // agent dirs, because the member list is not the set of files this
        // group has: a roster change retires a block whose file is already
        // on disk, and a member entry can be gone (pruned, or state edited
        // out from under loomux) while its file is not. The old shape left
        // both behind. The scan asks the ownership question directly —
        // "which live group does this file belong to?" (`owning_group`) —
        // and deletes only what answers with THIS group. Best-effort and per
        // CLI: a Claude-only group's Copilot pass, and vice versa, are
        // harmless no-ops against a directory that never held this group's
        // handles.
        //
        // This is the orderly-teardown half only. A group that never gets
        // here at all (a crash, a kill) leaves its files behind, and
        // reclaiming those is #464's `sweep_orphaned_agent_files` at
        // startup — a separate, already-shipped mechanism this deliberately
        // does not duplicate.
        let reclaimed = self.reclaim_group_agent_files(group);
        if !reclaimed.is_empty() {
            self.audit(group, brand::AUDIT_ACTOR, "generated-agent-files-reclaimed", json!({ "files": reclaimed }));
        }

        // Total teardown: drop any pause (in-memory + marker) so a future
        // relaunch on this repo starts clean rather than silently paused.
        if self.paused.lock_safe().remove(group) {
            let _ = fs::remove_file(self.group_dir(group).join("paused"));
        }
        // …and the polled usage memo (#743 S4b), for the same reason: a group
        // id is chosen by liveness and can be handed out again, so a surviving
        // entry would be a dead group's figures answering for a new one. Also
        // what keeps the map bounded by LIVE groups rather than by every group
        // this process ever opened.
        self.invalidate_usage_memo(group);

        self.audit(group, "human", "group-end", json!({
            "killed": killed,
            "cleanup_worktrees": cleanup_worktrees,
            "worktrees_removed": worktrees_removed,
            "worktree_errors": worktree_errors,
        }));

        // Tell the frontend to close the group's (now-dead) panes so the human
        // isn't left ✕-clicking a screen of dead terminals — the very chore
        // this action exists to remove.
        if let Some(app) = app.as_ref() {
            let _ = app.emit("orch-group-ended", json!({ "group_id": group }));
        }

        Ok(json!({
            "group": group,
            "killed": killed,
            "worktrees_removed": worktrees_removed,
            "worktree_errors": worktree_errors,
        }))
    }

    /// #502: does this group still have state on disk? The same registry the
    /// reclaim paths ask (`root/<group>`, see [`group_dir`](Self::group_dir)),
    /// asked at WRITE time — so a write and a reclaim can never disagree
    /// about whether a group exists.
    ///
    /// This is the "only write for groups that actually exist" half of the
    /// re-mint fix, and it is deliberately a guard at the writers rather
    /// than a fix to one caller: a generated agent file is a durable
    /// artifact in a directory loomux doesn't own, so ANY path that reaches
    /// a writer with a dead group id — a stale roster, a restore index, a
    /// future periodic refresh nobody has written yet — must fail to
    /// resurrect it, not just the one path that does so today. `create_group`
    /// makes this directory before any spawn can reach a writer, so a live
    /// group always passes.
    pub(in crate::orchestration) fn group_state_exists(&self, group: &GroupId) -> bool {
        self.group_dir(group).is_dir()
    }
}
