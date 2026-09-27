//! The group's roster and workflow mode: persisting the roster keys,
//! switching advanced-orchestrator mode, applying a workflow, and reading it
//! back (`workflow_status`, `list_blocks`), as an `impl OrchRegistry` block
//! (#3498). The design is `docs/design/workflows.md`.

use super::*;

impl OrchRegistry {
    /// Rewrite the four `guardrails` keys that make up a group's ROSTER —
    /// `advanced_orchestrator`, `workflow`, `blocks` and `intake` — in
    /// group.json in place, preserving every other stored field: the same
    /// crash-safe, additive patch `persist_max_agents` uses (#316's live
    /// counterpart to the launch-time-only toggle, generalised by #1689 slice B).
    ///
    /// **One write, because they are one fact.** A live toggle changes the flag
    /// and the roster together (a new roster arms a new gate; toggle-off
    /// restores the built-in one), and an apply changes all four: the name of
    /// the file, the blocks it declared, and the intake vocabulary resolved
    /// from the same document. A name persisted apart from the roster it
    /// produced could disagree with it after a crash, and then nothing on disk
    /// would say which of the two the human consented to — the argument
    /// `Guardrails::workflow` states, enforced here by there being no second
    /// writer.
    ///
    /// A caller not changing one of the four passes back its own current value,
    /// which round-trips byte-for-byte, so the field is effectively preserved.
    /// That is what `set_advanced_orchestrator` does with `workflow`: a toggle
    /// changes which roster is in force, never which FILE the group is pinned to,
    /// so the name it passes back is the one it read. `intake` is not in that
    /// category and an earlier version of this doc wrongly said it was (#2659
    /// review round 1, rev-final 1): the toggle adopts the file's intake on the
    /// way ON and restores the built-in profile on the way OFF, so that value
    /// really does move — it is passed in because it changed, not because it
    /// round-trips.
    fn persist_roster(
        &self,
        group: &GroupId,
        on: bool,
        name: &workflow::WorkflowName,
        blocks: &[workflow::Block],
        intake: &workflow::IntakeProfile,
    ) -> Result<(), String> {
        let dir = self.group_dir(group);
        let path = dir.join("group.json");
        let mut v: Value = serde_json::from_str(&fs::read_to_string(&path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        // Guard the indexing so a corrupt-but-valid-JSON file (e.g. a `null`
        // root) fails soft instead of panicking on assignment.
        let obj = v.as_object_mut().ok_or("group.json root is not a JSON object")?;
        let patch = |guard: &mut serde_json::Map<String, Value>| {
            guard.insert("advanced_orchestrator".into(), json!(on));
            guard.insert("workflow".into(), json!(name.as_str()));
            guard.insert("blocks".into(), blocks_json(blocks));
            guard.insert("intake".into(), intake_json(intake));
        };
        match obj.get_mut("guardrails").and_then(Value::as_object_mut) {
            Some(guard) => patch(guard),
            None => {
                let mut guard = serde_json::Map::new();
                patch(&mut guard);
                obj.insert("guardrails".into(), Value::Object(guard));
            }
        }
        // Crash-safe write: group.json is identity-critical — a half-written
        // file breaks the rejoin path ("group.json is missing") — so never
        // expose a truncated version (#133).
        let body = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
        atomic_write(&path, body.as_bytes()).map_err(|e| e.to_string())?;
        Ok(())
    }
    /// Live setter for the advanced-orchestrator toggle (#316), modeled EXACTLY
    /// on `set_max_agents`/`set_autonomous`: persist-first group.json patch,
    /// in-memory swap, audit, and an `[orrerix] workflow mode changed …` notice to
    /// the orchestrator via the existing `deliver_to_orchestrator` — no new
    /// delivery path. Unlike the launch-time-only toggle this field started as
    /// (#222), this is now a LIVE property of the running session.
    ///
    /// Turning it ON arms the merge gate via the identical `load_workflow` +
    /// `sync_merge_gate` sequence a fresh launch runs (`create_group`'s `Fresh`
    /// arm); turning it OFF clears the gate and restores the built-in roster.
    /// The gate is scoped to the CURRENT SESSION, not to any one PR's
    /// provenance — see `docs/design/workflows.md`, "a gate lives and dies with
    /// the toggle that authorized it" — so toggling OFF opens even a PR built
    /// earlier under workflow mode, and toggling back ON re-arms it.
    ///
    /// **ON refuses** rather than arming anything when the repo declares no
    /// workflow file at all (`Ok(None)` — nothing for a *live* toggle to switch
    /// into; unlike a launch, where "no file" is a legitimate no-op) or when the
    /// file is broken (`Err`) — don't arm a gate for a roster that could not be
    /// resolved. **ON with a gate naming reviewers the resolved roster cannot
    /// spawn arms anyway** (never silently widens by dropping the gate — the
    /// same fail-loud rule `merge-gate-retained` exists for) but reports
    /// `satisfiable: false` and audits `merge-gate-unsatisfiable` (#316's second
    /// stance: never silently arm a gate this session cannot satisfy).
    ///
    /// **Live delegates already spawned are NOT re-personaed.** The roster swap
    /// applies only to FUTURE spawns — the consent model says a delegate's
    /// persona may never change under a human who already approved it, and
    /// while a live toggle IS a fresh consent moment (the human sees the roster
    /// and clicks), that consent only covers agents spawned *after* it.
    ///
    /// Returns the resulting [`Self::workflow_status`] — the same shape
    /// `orch_workflow_status` reads, so the toggle's own confirm and a later
    /// status poll can never disagree.
    pub fn set_advanced_orchestrator(&self, group: &GroupId, on: bool, actor: &str) -> Result<Value, String> {
        // `group_file_io` covers the whole decide-persist-publish window (its
        // doc has the argument), so the flag this reads is the flag the write
        // below patches. Released before `workflow_status` on either exit:
        // that call can resolve a default branch through `git`, and no other
        // group's guardrail write should wait behind a subprocess.
        let _io = self.group_file_io.lock_safe();
        let info = self.group(group).ok_or("unknown group")?;
        if on == info.guardrails.advanced_orchestrator {
            drop(_io);
            return Ok(self.workflow_status(group));
        }

        let mut guardrails = info.guardrails.clone();
        let mut gate: Option<workflow::Gate> = None;
        let mut name = String::new();
        // Captured at load time, emitted only once the persist below actually
        // succeeds (rev-24 N2) — auditing "workflow-loaded" before the write
        // that makes it stick would leave a stray audit line claiming a load
        // that never took effect if `persist_roster` then fails.
        let mut loaded_audit: Option<Value> = None;

        if on {
            match load_active_workflow(&info.repo, &info.guardrails) {
                Ok(Some(wf)) => {
                    loaded_audit = Some(json!({
                        "path": active_workflow_path(&info.repo, &info.guardrails),
                        "name": wf.name,
                        "blocks": wf.blocks.iter().map(|b| json!({ "id": b.id, "kind": b.kind })).collect::<Vec<_>>(),
                        "gates": wf.gates.keys().collect::<Vec<_>>(),
                    }));
                    name = wf.name.clone();
                    gate = wf.gates.get("merge").cloned();
                    guardrails.blocks = wf.blocks;
                    // #2659 review round 1 (rev-final 1): the intake profile comes
                    // from the SAME document as the roster and is adopted with it,
                    // exactly as `create_group_ex`'s Fresh arm and `apply_workflow`
                    // do. Taking `blocks` alone left every live-toggled group running
                    // the built-in label vocabulary under a declared roster — which
                    // `roster_drift` reads, correctly, as permanent drift against a
                    // file nobody had edited, and which #778 makes load-bearing: the
                    // human's `hold` veto is a live label, not decoration.
                    guardrails.intake = wf.intake;
                    guardrails = guardrails.clamped();
                }
                Ok(None) => {
                    return Err(format!(
                        "{} declares no {} — there is nothing for a live workflow-mode toggle to arm. \
                         Add the file, or leave the toggle off.",
                        info.repo,
                        active_workflow_path(&info.repo, &info.guardrails)
                    ));
                }
                Err(errors) => {
                    return Err(format!(
                        "{}'s {} is invalid, so the workflow-mode toggle refuses to arm a roster it \
                         could not load: {}",
                        info.repo,
                        active_workflow_path(&info.repo, &info.guardrails),
                        errors.join("; ")
                    ));
                }
            }
        } else {
            // Toggle-OFF roster restore: rebuild the built-in four-block roster
            // from the group's default agent_cli — `clamped()` on an empty
            // roster does exactly this. Per-role CLI overrides chosen at launch
            // are not separately persisted today, so this restores the
            // default-CLI roster, not necessarily the group's original one.
            guardrails.blocks = Vec::new();
            // The intake vocabulary goes back with it. Restoring the roster and
            // KEEPING the file's labels is the ON arm's defect in reverse — a
            // group running the built-in four while the intake poller and the
            // human's veto still answer to a `hold` spelling only the repo file
            // ever named. Off means the file is not obeyed, and that has to
            // include the half of it that is not blocks.
            guardrails.intake = workflow::builtin_intake_profile();
            guardrails = guardrails.clamped();
        }
        guardrails.advanced_orchestrator = on;

        // Persist first: a failed disk write must leave the in-memory
        // roster/flag (what enforcement reads) unchanged, so the two never
        // disagree — same discipline as `set_max_agents`.
        self.persist_roster(group, on, &guardrails.workflow, &guardrails.blocks, &guardrails.intake)?;
        {
            let mut groups = self.groups.lock_safe();
            let g = groups.get_mut(group).ok_or("unknown group")?;
            g.guardrails.blocks = guardrails.blocks.clone();
            // The vocabulary moves with the roster it was resolved beside — the
            // in-memory half of the same fix. `persist_roster` above already wrote
            // it, so omitting it here is the worst shape: group.json and the live
            // guardrails disagreeing about which `hold` label the group answers to.
            g.guardrails.intake = guardrails.intake.clone();
            g.guardrails.advanced_orchestrator = on;
        }
        // Only now that the persist succeeded — see `loaded_audit`'s doc above.
        if let Some(detail) = loaded_audit {
            self.audit(group, brand::AUDIT_ACTOR, "workflow-loaded", detail);
        }

        self.audit(
            group,
            actor,
            if on { "advanced-orchestrator-on" } else { "advanced-orchestrator-off" },
            json!({}),
        );
        // #762 rev-260 B3: the gate sync stays INSIDE `group_file_io`. An earlier
        // revision dropped the guard above this point on a pure INV-5/latency
        // argument and did not notice it had reopened a race this file already
        // refuses in writing: two toggles could publish their flags in one order
        // and commit their gate specs in the other, leaving a merge-gate spec
        // armed for a group whose `advanced_orchestrator` is now off — which
        // `reload_merge_gate_if_changed` will not clear, because it returns early
        // for an off group. `sync_merge_gate` only writes a small file and audits
        // (no pane delivery, no subprocess), so ordering it here costs nothing the
        // guard was protecting against.
        //
        // The precedent is `reload_merge_gate_if_changed`'s own #385 re-check,
        // which refuses the identical hole in these words: "that's the SAFE
        // direction (more enforcement, not less) — but it is still a real bug, and
        // 'it fails safe' is not a reason to ship a known race in security
        // machinery."
        self.sync_merge_gate_locked(group, gate.as_ref(), &guardrails.blocks);
        drop(_io);
        let _ =
            self.deliver_to_orchestrator(group, &workflow_mode_notice(on, &name, gate.as_ref()), brand::AUDIT_ACTOR);

        Ok(self.workflow_status(group))
    }

    /// What applying a named workflow to THIS group would do (#1689 slice B) —
    /// the one resolution behind both [`Self::workflow_switch_preview`] and
    /// [`Self::apply_workflow`].
    ///
    /// Shared rather than computed twice on purpose: a preview that resolved
    /// the file, the roster or the diff even slightly differently from the
    /// apply would be a confirmation about something other than what happens,
    /// which is the whole of what "consent-preserving" means here. It is the
    /// same argument `orch_workflow_preview` makes for being the engine rather
    /// than a second opinion.
    ///
    /// **`Err` is a refusal that no confirmation can clear**: workflow mode is
    /// off, the file is absent, the file will not parse. `Ok` with
    /// [`SwitchPlan::refusal`] set is a refusal the human can act on by editing
    /// the file, so the diff is returned WITH it — the preview shows what the
    /// switch would change and why it cannot be applied, rather than a bare
    /// error naming neither.
    fn plan_workflow_switch(
        &self,
        info: &GroupInfo,
        name: &workflow::WorkflowName,
    ) -> Result<SwitchPlan, String> {
        // Workflow mode OFF refuses BEFORE the file is even resolved, and the
        // refusal names the fix. The toggle is this group's one consent surface
        // for "does this group obey repo-authored workflow files" (`Guardrails::
        // advanced_orchestrator`: off means the file "is not read, not validated
        // and not obeyed"); an apply answers only the narrower question of WHICH
        // file. Arming the toggle from a picker instead would be the app
        // granting itself the consent that toggle exists to ask for — and every
        // reader gated on the flag (`board_policy`, `workflow_status`'s declared
        // load, the drift audit) would start reading a file nobody switched on.
        if !info.guardrails.advanced_orchestrator {
            return Err(format!(
                "workflow mode is off for this group, so there is no active workflow to switch \
                 away from — turn it on first, which arms '{}', then apply '{name}'.",
                info.guardrails.workflow
            ));
        }
        // Resolved through the SAME pair every group-scoped reader uses, against
        // a copy of the guardrails carrying the TARGET name — never a second
        // spelling of "which file does this name mean".
        let target = Guardrails { workflow: name.clone(), ..info.guardrails.clone() };
        let path = active_workflow_path(&info.repo, &target);
        let wf = match load_active_workflow(&info.repo, &target) {
            Ok(Some(wf)) => wf,
            Ok(None) => {
                return Err(format!(
                    "{} declares no {path} — there is no workflow named '{name}' to apply.",
                    info.repo
                ))
            }
            Err(errors) => {
                return Err(format!(
                    "{}'s {path} is invalid, so applying '{name}' would swap in a roster that \
                     could not be loaded: {}",
                    info.repo,
                    errors.join("; ")
                ))
            }
        };

        let display_name = wf.name.clone();
        let gate = wf.gates.get("merge").cloned();
        let guardrails = Guardrails {
            workflow: name.clone(),
            blocks: wf.blocks,
            intake: wf.intake,
            ..info.guardrails.clone()
        }
        .clamped();

        // The OLD side is what the group is ACTUALLY running: the pinned roster
        // and intake, and the gate file that is armed right now — not whatever
        // the old file says today, which is a different question (that one is
        // `drift`).
        let armed = self.merge_gate(&info.id);
        let diff = workflow::roster_diff(
            &info.guardrails.agent_cli,
            workflow::RosterSide {
                blocks: &info.guardrails.blocks,
                gate: armed.as_ref(),
                intake: &info.guardrails.intake,
            },
            workflow::RosterSide {
                blocks: &guardrails.blocks,
                gate: gate.as_ref(),
                intake: &guardrails.intake,
            },
        );

        // The one change an apply cannot make to a LIVE group. The orchestrator's
        // pane is already running a program and a session cannot change program
        // mid-flight — the same reason `promote_orchestrator_cli` refuses it —
        // and re-opening that pane is a relaunch, not a switch.
        let refusal = diff.orchestrator_cli_changed.then(|| {
            format!(
                "'{name}' runs the orchestrator on a different CLI, and a live session cannot \
                 change the program it is running — relaunch the group on '{name}' instead, or \
                 leave the orchestrator block's cli: as this group's."
            )
        });

        // A `model:`/`effort:`/`context:` change on the orchestrator block IS
        // applied — to group.json — but the running pane keeps the model it was
        // opened with, so the preview says when it takes effect rather than
        // letting the human read the diff as a live change.
        let next_resume = diff
            .changed
            .iter()
            .find(|c| guardrails.block(&c.id).map(|b| b.kind == Role::Orchestrator).unwrap_or(false))
            .map(|c| c.fields.iter().filter(|f| f.as_str() != "cli").cloned().collect::<Vec<_>>())
            .unwrap_or_default();

        let digest = workflow::workflow_digest(&info.repo, name);
        Ok(SwitchPlan { guardrails, gate, display_name, path, digest, diff, refusal, next_resume })
    }

    /// What `apply_workflow(name)` would change, without changing anything
    /// (#1689 slice B) — the payload the group header's **Review & apply**
    /// modal is built from.
    ///
    /// Read-only, and it is the SAME resolution the apply runs
    /// ([`Self::plan_workflow_switch`]), so the confirmation a human reads and
    /// the action they authorize cannot describe different things.
    #[doc(hidden)] // pub for integration tests
    pub fn workflow_switch_preview(&self, group: &GroupId, name: &workflow::WorkflowName) -> Result<Value, String> {
        let info = self.group(group).ok_or("unknown group")?;
        let plan = self.plan_workflow_switch(&info, name)?;
        Ok(plan.to_json(&info))
    }

    /// Bring the group dir's instruction files in line with `g`'s roster (#423),
    /// auditing a failure rather than propagating it (#1689 slice B).
    ///
    /// **Not propagated**, because both callers run AFTER the switch is persisted
    /// and live: turning a disk hiccup into an `Err` there would report a failure
    /// that did not occur. What is genuinely lost is the SWEEP — the per-spawn
    /// render writes a new block's file but never removes a departed one — so the
    /// trail has to say so, and the no-op re-apply arm calls this too, which is
    /// what gives a failed sweep a way to be retried at all.
    fn reconcile_instruction_files(&self, g: &GroupInfo) {
        if let Err(e) = self.write_instruction_files(g) {
            self.audit(&g.id, brand::AUDIT_ACTOR, "instruction-files-failed", json!({
                "error": e,
                "note": "the roster is applied and live; a removed block's stale \
                         instructions file may still be on disk — re-applying the same \
                         workflow retries this",
            }));
        }
    }

    /// Apply a named workflow to a LIVE group (#1689 slice B) — the
    /// consent-preserving roster switch, modelled on
    /// [`Self::set_advanced_orchestrator`] step for step: persist first,
    /// in-memory swap, instruction files, audit, gate sync, notice.
    ///
    /// **What it does NOT do is the consent model.** A pane already running
    /// keeps the block it was spawned under — `AgentEntry.block` is recorded at
    /// spawn and nothing re-reads it — so no delegate is re-personaed under a
    /// human who already approved it. What the switch governs is FUTURE spawns:
    /// `spawn_agent_bound` resolves against `guardrails.blocks`, so a b-only id
    /// becomes spawnable immediately and an id the new roster dropped is refused
    /// by that function's existing unknown-block path, naming the active roster.
    /// A bare `resume_session` of a removed block's agent inherits the recorded
    /// id and reaches the same refusal — which is the required behaviour, not a
    /// new mechanism.
    ///
    /// **`write_instruction_files` runs, and `set_advanced_orchestrator` does
    /// not call it.** That is the difference an apply forces: the live toggle
    /// never introduces a block the group dir has not already seen, while a
    /// switch both adds and REMOVES ids — and only this path reconciles the
    /// group dir against the `.instruction-files-manifest`, sweeping a removed
    /// block's stale `<id>.md` (#423). The per-spawn render
    /// (`spawn_agent_bound`, #1187) would eventually write a NEW block's file,
    /// but it never removes anything.
    ///
    /// Returns the resulting [`Self::workflow_status`], like the toggle, so an
    /// apply's own confirm and a later status poll cannot disagree.
    #[doc(hidden)] // pub for integration tests
    pub fn apply_workflow(
        &self,
        group: &GroupId,
        name: &workflow::WorkflowName,
        expect_digest: Option<&str>,
        actor: &str,
    ) -> Result<Value, String> {
        // `group_file_io` covers the whole decide-persist-publish window, for
        // the reason `set_advanced_orchestrator`'s does: the roster this reads
        // is the roster the write below patches. Released before
        // `workflow_status` on either exit — that call can resolve a default
        // branch through `git`, and no other group's guardrail write should wait
        // behind a subprocess.
        let _io = self.group_file_io.lock_safe();
        let info = self.group(group).ok_or("unknown group")?;
        let plan = self.plan_workflow_switch(&info, name)?;
        if let Some(refusal) = plan.refusal {
            return Err(refusal);
        }
        // The file may have been edited between the preview the human read and
        // the confirmation they gave (#2659 review round 1, premortem 1). Without
        // this the apply would silently install whatever the file says NOW, and
        // the audit row would record a diff nobody had approved. `None` is a
        // caller with no confirmation to honour — a test, a script — and is
        // recorded as such in the trail rather than treated as a match.
        if let Some(want) = expect_digest {
            if plan.digest.as_deref() != Some(want) {
                return Err(format!(
                    "{} changed since the preview you confirmed — re-open the diff and \
                     confirm the file as it stands now.",
                    plan.path
                ));
            }
        }
        let from = info.guardrails.workflow.clone();
        // Already-at-target: the same name AND a file nobody has edited since.
        // Same shape as the toggle's own no-op — no audit row, no notice, no
        // rewrite — and deliberately narrow: re-applying a name whose file HAS
        // been edited is the #1566 apply-an-edit path and must go all the way
        // through.
        if from == *name && plan.diff.is_empty() {
            // It still reconciles the group dir before returning (#2659 review
            // round 1, premortem 2). `write_instruction_files` is audited rather
            // than propagated below, so an apply whose write failed leaves the
            // switch live with a stale `<id>.md` on disk — and the obvious human
            // response, applying the same unedited file again, is exactly the
            // call this arm used to answer without touching the disk. Nothing
            // else self-heals it: the per-spawn render never REMOVES a file. The
            // write is idempotent, and this arm is a human-gated action rather
            // than a poll, so paying it here costs nothing worth saving.
            self.reconcile_instruction_files(&info);
            drop(_io);
            return Ok(self.workflow_status(group));
        }

        let guardrails = plan.guardrails;
        // Persist first: a failed disk write must leave the in-memory
        // roster (what enforcement reads) unchanged, so the two never disagree —
        // same discipline as `set_advanced_orchestrator`.
        self.persist_roster(group, true, &guardrails.workflow, &guardrails.blocks, &guardrails.intake)?;
        {
            let mut groups = self.groups.lock_safe();
            let g = groups.get_mut(group).ok_or("unknown group")?;
            g.guardrails.workflow = guardrails.workflow.clone();
            g.guardrails.blocks = guardrails.blocks.clone();
            g.guardrails.intake = guardrails.intake.clone();
        }

        // The group dir reconciled against the new roster (#423) — see this
        // function's doc for why the toggle does not need this and an apply
        // does. A failure here is audited and NOT propagated: the roster is
        // already persisted and live, the next spawn re-renders its own block's
        // file anyway, and turning a disk hiccup into an `Err` after the switch
        // has happened would report a failure that did not occur. What is
        // genuinely lost is the SWEEP, so the trail has to say so.
        let after = GroupInfo { id: info.id.clone(), repo: info.repo.clone(), guardrails: guardrails.clone() };
        self.reconcile_instruction_files(&after);

        self.audit(group, actor, "workflow-switched", json!({
            "from": from.as_str(),
            "to": guardrails.workflow.as_str(),
            "path": plan.path,
            "actor": actor,
            // Both halves, so a later reader can tell "this is what the file said
            // when it landed" from "this is what the human was shown". A null
            // `confirmed_digest` is a caller that passed no confirmation, which is
            // a different statement from the two agreeing.
            "digest": plan.digest,
            "confirmed_digest": expect_digest,
            "diff": diff_json(&plan.diff),
        }));
        // Inside `group_file_io`, for the reason `set_advanced_orchestrator`
        // states at its own call: the gate spec and the roster it belongs to are
        // one decision, and two writers that commit them in opposite orders
        // leave a gate armed for a roster that never declared it.
        self.sync_merge_gate_locked(group, plan.gate.as_ref(), &guardrails.blocks);
        drop(_io);

        let _ = self.deliver_to_orchestrator(
            group,
            &workflow_switched_notice(
                from.as_str(),
                guardrails.workflow.as_str(),
                &guardrails.agent_cli,
                &guardrails.blocks,
                &plan.diff.removed,
                plan.gate.as_ref(),
            ),
            brand::AUDIT_ACTOR,
        );
        Ok(self.workflow_status(group))
    }

    /// The group's current workflow-mode status (#316) — the single derivation
    /// both `orch_workflow_status` and `set_advanced_orchestrator`'s return
    /// value use, so the lifecycle UI and the toggle's own confirm can never
    /// disagree.
    ///
    /// `blocks` is read from the group's PERSISTED/in-memory roster — pinned
    /// at launch/toggle and never silently re-read from the repo's workflow
    /// file, the same consent rule `create_group`'s resume arm documents.
    /// `name` is a live, best-effort, DISPLAY-ONLY read of the repo's current
    /// workflow file (like `orch_workflow_preview`) — cosmetic, never a
    /// source of enforcement.
    ///
    /// **`gate` is NOT pinned the same way, as of #385.** It reads
    /// `self.merge_gate` — the `merge_gate` spec file — which
    /// `run_workflow_gate_reload`'s periodic background pass keeps in sync
    /// with the repo's CURRENT `.loomux/workflow.yml` for any advanced-
    /// orchestrator group (see `reload_merge_gate_if_changed`), independent
    /// of this call and independent of launch/toggle. So this function's own
    /// read is a plain "whatever's armed right now" — it neither re-reads
    /// the file itself nor pins anything — and what's armed right now can
    /// differ from what launch or the last toggle wrote. The one thing that
    /// reload can never do is take an ALREADY-armed gate down to no gate
    /// (#385/B1 — that still requires the explicit toggle-off), so `gate`
    /// going from `Some` to `null` between two calls is still exactly the
    /// signal it always was: the toggle turned off, not a background read.
    ///
    /// `gate.satisfiable`/`missing_blocks` are recomputed fresh against the
    /// CURRENT roster on every call, never cached from whenever the gate was
    /// armed, so a roster that later regains (or loses) a named reviewer shows
    /// up on the very next read without another toggle.
    ///
    /// `default_branch` (#581) is the repo's default branch name, or `null`
    /// when it doesn't resolve — display data for the board's Approve relabel,
    /// which compares it against a task's recorded `pr_base`. Neither is an
    /// enforcement input: the gate is enforced in the gh shim against the base
    /// ref it resolves live.
    #[doc(hidden)] // pub for integration tests
    pub fn workflow_status(&self, group: &GroupId) -> Value {
        self.workflow_status_within(group, DEFAULT_BRANCH_MAX_AGE)
    }

    /// [`Self::workflow_status`], with the default-branch memo's freshness
    /// window as a parameter (#743 S4a). `Duration::ZERO` forces a live
    /// resolution; see [`Self::default_branch_memo`].
    #[doc(hidden)] // pub for integration tests
    pub fn workflow_status_within(&self, group: &GroupId, default_branch_max_age: Duration) -> Value {
        let info = self.group(group);
        let guardrails = info.as_ref().map(|g| g.guardrails.clone()).unwrap_or_default();
        // ONE load for the two things this call reads out of the workflow file
        // — the display `name` and the `board:` policy below (#1175). They used
        // to be two `load_workflow` calls, which is two YAML parses of the same
        // file on a call the publisher makes once a second for every group
        // holding a view lease (the group view's 2 s poll until #1608).
        let loaded = if guardrails.advanced_orchestrator {
            info.as_ref().map(|g| load_active_workflow(&g.repo, &g.guardrails))
        } else {
            // The toggle being off is authoritative "this group declares
            // nothing", exactly as `board_policy`/`merge_queue_policy` read it
            // — not an unread file. `drift` is `null` here for the same reason,
            // and not because nothing drifted: a group running the built-in
            // roster has no declared file to have drifted from.
            None
        };
        // #1689 slice B: the drift badge reads the SAME comparison the resume
        // audit writes (`roster_drift`), off the SAME load the display name and
        // the board policy below use — one parse, on a call the group-view
        // publisher makes once a second per leased group (#1175's argument,
        // extended rather than paid twice).
        let drift = loaded.as_ref().and_then(|l| roster_drift(l, &guardrails));
        let declared = loaded.and_then(|l| l.ok().flatten());
        let name = declared.as_ref().map(|wf| wf.name.clone()).unwrap_or_default();
        let board = declared.map(|wf| wf.board).unwrap_or_default();
        let gate = self.merge_gate(group).map(|g| gate_json(&g, &guardrails.blocks));
        // The repo's default branch NAME (#581), so the board can tell a merge
        // into the gated default branch from a sub-PR into an integration
        // branch. `null` when it doesn't resolve — a real answer the frontend
        // must read as "unknown" and fall back to its conservative wording,
        // never as "not the default branch". Local refs only (see
        // `default_branch_name`): this command is on a UI path, so the answer
        // is also as stale as the clone's last fetch — a rename on the remote
        // reads as the old name here until something fetches (rev-157 NB1/NB2).
        // Both are tolerable for the same reason: the label is the only
        // consumer, and no gate reads this. #743 S4a adds a third, bounded,
        // source of staleness for the same reason — see `default_branch_within`.
        let default_branch = info
            .as_ref()
            .and_then(|g| self.default_branch_within(&g.repo, default_branch_max_age));
        json!({
            "advanced": guardrails.advanced_orchestrator,
            "name": name,
            // #1689: which workflow this group RUNS, and which ones the repo
            // offers. `workflow` is the pinned name — always present, `default`
            // for every group that predates named workflows — where `name` above
            // is the file's own cosmetic `name:` field and is empty whenever the
            // toggle is off. `available` is a names-only directory walk
            // (`list_workflow_names`), never the picker's full listing: this
            // payload does not need to know why a file will not parse, and
            // reading every one of them per publish tick is the unbounded read
            // #2658 recorded.
            "workflow": guardrails.workflow.as_str(),
            // An UNKNOWN group has no repo to list, and `""` is not one: it would
            // resolve `.orrerix/workflows` against the PROCESS working directory
            // and answer with whatever that happens to hold (#2659 review round 1,
            // rev-std 3). Read-only and bounded, but an answer about the wrong
            // machine is worse than no answer.
            "available": info
                .as_ref()
                .map(|g| workflow::list_workflow_names(&g.repo))
                .unwrap_or_default(),
            // `null` = the group is running what its file says (or declares
            // none). Set = the pinned roster and the file have diverged; the
            // pinned roster is what runs, deliberately (#222 rev-11 F2).
            "drift": drift.map(|d| json!({
                "note": d.note,
                "on_disk_blocks": d.on_disk,
            })),
            "default_branch": default_branch,
            "blocks": roster_json(&guardrails.blocks),
            "gate": gate,
            "wip": self.wip_rows(group, &board),
        })
    }

    /// The MCP `list_blocks` read-back (#1689 slice C): the ACTIVE roster the
    /// orchestrator's spawns resolve against, as `{workflow, name, advanced,
    /// blocks}` — the pinned workflow name, the file's own display `name:`,
    /// whether the advanced-orchestrator toggle is in force, and one
    /// id/kind/cli/model/persona row per block.
    ///
    /// Derived from [`Self::workflow_status`] rather than re-read, and that is
    /// the whole point: the notice, the human's status payload and this read-back
    /// all answer from one load of one guardrails state, so a switch cannot
    /// leave the orchestrator's read-back describing a roster anything else in
    /// the app disagrees with. An unknown group reads exactly like its status
    /// does — a built-in roster under the name `default` — rather than erroring,
    /// because the caller's membership is already proven at the dispatch gate
    /// and an unknown group id reaching here is not a leak, just an empty answer.
    #[doc(hidden)] // pub for integration tests
    pub fn list_blocks(&self, group: &GroupId) -> Value {
        let status = self.workflow_status(group);
        json!({
            "workflow": status["workflow"],
            "name": status["name"],
            "advanced": status["advanced"],
            "blocks": status["blocks"],
        })
    }

    /// The repo's default-branch NAME, served from [`Self::default_branch_memo`]
    /// when the stored answer is younger than `max_age` (#743 S4a).
    ///
    /// `None` is memoised too, and deliberately: a repo that resolves to
    /// "unknown" is the *worst* case for the uncached path — the full ladder
    /// runs, every probe fails, and nothing is learnt — so re-running it every
    /// 2 s is precisely what this exists to stop.
    ///
    /// The memo lock is released before the `git` spawns and re-taken after: no
    /// registry lock is ever held across a subprocess here.
    fn default_branch_within(&self, repo: &str, max_age: Duration) -> Option<String> {
        {
            let memo = self.default_branch_memo.lock_safe();
            if let Some((at, name)) = memo.get(repo) {
                if at.elapsed() < max_age {
                    return name.clone();
                }
            }
        }
        let fresh = crate::git::default_branch_name(repo);
        self.default_branch_memo
            .lock_safe()
            .insert(repo.to_string(), (std::time::Instant::now(), fresh.clone()));
        fresh
    }
}
