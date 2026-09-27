//! Idleness and attention: pause and resume, the idle-kill reaper, the
//! stalled-agent watchdog, the orchestrator idle tick and its budget
//! enforcement, the low-disk backstop, and attention routing (which pane
//! needs the human: `attention_*`, `ack_attention`), with each concern's
//! `run_*` background loop, as an `impl OrchRegistry` block (#3498). The
//! design is `docs/design/orchestration.md`; the loops' liveness is
//! `docs/design/lock-liveness.md`.

use super::*;

impl OrchRegistry {
    // ---------- cost containment: pause, idle-kill, spawn-rate, usage ----------

    /// Whether a group is currently paused (prompts/kickoffs suppressed).
    pub fn is_paused(&self, group: &GroupId) -> bool {
        self.paused.lock_safe().contains(group)
    }

    /// Pause a group: loomux stops delivering prompts and kickoffs to its
    /// agents, so they finish their current turn and idle out (containing
    /// unattended spend) without being killed. Durable via a marker file.
    ///
    /// #569 option 2: deliveries that arrive during the pause are HELD in the
    /// target pane's durable queue and flushed on `resume_group`, not
    /// destroyed. The cost containment is unchanged while the pause lasts —
    /// nothing pastes, so no agent is woken and no tokens are spent — and the
    /// spend it defers is re-incurred at resume, which is the contract change
    /// the human signed off on: a pause that loses a worker's `report("done")`
    /// costs more than the tokens it saved, because the orchestrator then
    /// waits on a report that is never coming.
    pub fn pause_group(&self, group: &GroupId) -> Result<(), String> {
        let newly = self.paused.lock_safe().insert(group.clone());
        if newly {
            let dir = self.group_dir(group);
            fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let _ = fs::write(dir.join("paused"), b"");
            self.audit(group, "human", "group-pause", json!({}));
        }
        Ok(())
    }

    /// Resume a paused group: prompt/kickoff delivery flows again, and
    /// everything the pause HELD is flushed to the pane it was addressed to,
    /// in arrival order, behind the standard flush header
    /// (`flush_paused_queues`).
    ///
    /// #569 option 2, the human's decision: a paused group's deliveries are
    /// queued, not destroyed. Pause is still a cost-containment hold — nothing
    /// pastes and no agent is woken while it lasts — but the containment is
    /// now a DELAY rather than a loss, so a worker's `report("done")` fired
    /// during a pause reaches the orchestrator on resume instead of
    /// evaporating and leaving it waiting forever.
    ///
    /// `announce_pause_suppression` still runs, and is NOT purely transitional
    /// (review B2): besides what a build predating this change discarded, it
    /// reports what THIS pause refused — a pane already at
    /// `queue::QUEUE_MAX_PER_PANE` rejects further admissions for as long as
    /// the pause lasts, and the sender's `Err` was previously the only trace.
    /// So "a pause cannot destroy a delivery" is not a claim this makes; the
    /// weaker true one is that a pause no longer destroys deliveries *at the
    /// pause branch*, and the cap is where the remaining loss lives. See its
    /// doc.
    pub fn resume_group(&self, group: &GroupId) -> Result<(), String> {
        let was = self.paused.lock_safe().remove(group);
        if was {
            let _ = fs::remove_file(self.group_dir(group).join("paused"));
            // Read the window BEFORE the resume line lands, so the timeline the
            // scan walks is exactly the pause and nothing after it. The pause
            // flag is already cleared above, which is what lets the notice
            // below actually deliver rather than suppressing itself.
            let swallowed = suppressed_during_pause(&self.audit_log(group));
            self.audit(group, "human", "group-resume", json!({}));
            self.announce_pause_suppression(group, &swallowed);
            self.flush_paused_queues(group);
        }
        Ok(())
    }

    /// #569: start draining every pane this group left holding entries.
    ///
    /// **Why resume owns this.** `deliver_prompt`'s pause branch admits into
    /// the pane's queue and deliberately does NOT spawn a drainer — a paused
    /// group must have nothing running that could paste. That leaves the
    /// #470 invariant ("the admission that saw an empty queue owns starting
    /// the processor") temporarily unmet ON PURPOSE, and this is the single
    /// place that discharges it. If this function is ever removed or made
    /// conditional, every payload a pause held is stranded — queued, audited,
    /// persisted, and never delivered.
    ///
    /// **Not filtered on liveness, deliberately.** A pane whose agent died
    /// mid-pause still gets a drainer here; that drainer's own first-pass
    /// liveness checks then drop the queue through the ordinary
    /// `commit_exit(force: true)` → `announce_dropped(AgentDied)` path, which
    /// is audited and notified. Skipping dead panes here would leave those
    /// entries sitting in `queues` and in `queue.json` with nothing to ever
    /// look at them again — a silent retention, which is the same defect
    /// class as the silent discard this issue is about.
    ///
    /// **Also the restart path.** A loomux restarted in the middle of a pause
    /// reads its queues back through `recover_persisted_queue`, and this is
    /// what starts them moving when the human resumes. Those entries were
    /// re-admitted as ordinary prompts (`readmit_recovered`), so the kickoff
    /// treatment below never applies to them — deliberately; see
    /// `queue::QueuedDelivery::delivery_kind`.
    ///
    /// **#620: what each drainer is started WITH.** This used to pass `None`
    /// unconditionally, which flushed a held `FreshKickoff` as a plain prompt
    /// — no boot wait, no copilot autopilot consent, #517's late-kickoff
    /// recovery unarmed. The kind now rides on the entry, so the treatment is
    /// read back off the pane's front entry (`paused_flush_kickoff`) instead
    /// of being assumed absent.
    ///
    /// One shape this does NOT cover, stated by MECHANISM rather than by the
    /// one origin that first showed it (review NB4): **any pause that lands
    /// between a drainer's spawn and its first pass.** `immediate_first_pass`
    /// is `iteration == 1`, and a pass that meets the paused gate spends that
    /// iteration on a `continue` — so from iteration 2 the treatment is gone,
    /// whichever call spawned that drainer. The drainer then holds the pane
    /// for the rest of the pause (it polls, refusing to paste, rather than
    /// exiting), so `ensure_drainer` here correctly no-ops for it and the
    /// treatment this function computed is never applied.
    ///
    /// That covers a `deliver_prompt` kickoff whose drainer the pause caught
    /// at once, and equally a RE-pause landing on a drainer THIS function just
    /// spawned — the entry is still queued and still says `fresh-kickoff`, but
    /// the next resume no-ops for the live drainer just the same. Closing it
    /// needs the drainer loop to keep an unattempted first pass alive across a
    /// pause hold, which changes what `immediate_first_pass` means for the
    /// hold-escalation and still-queued-notice gates too — out of scope here,
    /// and #620 stays open for it.
    ///
    /// Idempotent: `ensure_drainer` no-ops for a pane already draining, so a
    /// double resume costs nothing. Best-effort in exactly one respect — with
    /// no `AppHandle` (a bare test registry) no thread can be spawned at all,
    /// the same limitation every other drainer-spawning site has.
    fn flush_paused_queues(&self, group: &GroupId) {
        let panes: Vec<u32> = {
            let queues = self.queues.read();
            let mut panes: Vec<u32> = queues
                .iter()
                .filter(|(_, q)| q.iter().any(|d| d.group.as_ref() == Some(group)))
                .map(|(pty_id, _)| *pty_id)
                .collect();
            // Ascending, so the audit line below reads the same on every run
            // (a `HashMap`'s iteration order does not).
            panes.sort_unstable();
            panes
        };
        if panes.is_empty() {
            return;
        }
        // #620: what each pane's front entry says it is, decided BEFORE the
        // audit line below so the record can name the panes whose held
        // delivery is a kickoff. See `paused_flush_kickoff`.
        let treatments: Vec<(u32, Option<(u64, KickoffTreatment)>)> =
            panes.iter().map(|pty_id| (*pty_id, self.paused_flush_kickoff(group, *pty_id))).collect();
        let app = self.app.lock_safe().clone();
        let reg = self.arc();
        // `started` rather than a bare "pause-flush" line: with no `AppHandle`
        // nothing is spawned, and an audit entry claiming a flush began when
        // none did is the unbacked-claim defect `.loomux/lessons.md` catalogues
        // — the log is the only record a reader will ever have of this moment.
        self.audit(group, brand::AUDIT_ACTOR, "pause-flush", json!({
            "panes": &panes,
            "started": app.is_some() && reg.is_some(),
            // #620: which of those panes is having a KICKOFF flushed back into
            // it — the boot wait and the copilot consent confirm are actions
            // taken against a live pane, and this line is the only record that
            // they were (or, before #620, were not) armed.
            //
            // Read alongside `started`, not on its own (review NB3): like
            // `panes`, this field is emitted unconditionally and says what the
            // resume DECIDED, which is a different fact from whether a drainer
            // was spawned to act on it. `started: false` means nothing here was
            // applied to anything. The two are deliberately separate rather
            // than one gated field, because "the resume identified a held
            // kickoff and could not start it" is exactly the state a reader
            // chasing an unbooted agent needs to be able to see.
            "kickoff_panes": treatments.iter()
                .filter(|(_, t)| t.is_some())
                .map(|(pty_id, _)| *pty_id)
                .collect::<Vec<u32>>(),
        }));
        if let (Some(app), Some(reg)) = (app, reg) {
            for (pty_id, treatment) in treatments {
                reg.ensure_drainer(
                    app.clone(), group.clone(), pty_id,
                    treatment.map(FreshFirstAttempt::from),
                );
            }
        }
    }

    /// #620: the kickoff treatment `flush_paused_queues` hands the drainer it
    /// starts for `pty_id`, paired with the id of the entry that treatment
    /// belongs to — `None` when the pane's front entry is an ordinary prompt,
    /// which is every routine pause.
    ///
    /// **The FRONT entry, and only it.** Kickoff treatment applies to a
    /// drainer's first pass, which attempts the front of the queue; the id
    /// travels with it so `FreshFirstAttempt`'s guard can DROP the treatment
    /// rather than misapply it if `plan_flush` picks a different entry by the
    /// time that pass runs. Mirrors `redelivery_treatment`'s `was_first` rule:
    /// the decision about which entry owns the treatment lives at the call
    /// site, not in the drainer's id check after the fact.
    ///
    /// Split out of `flush_paused_queues` rather than written inline for the
    /// reason `hold_escalation_step`'s doc gives (#560): that function needs a
    /// real `AppHandle` to do anything observable, so a headless test — which
    /// is every test in this repo — cannot reach a decision made inside it.
    #[doc(hidden)] // pub for integration tests
    pub fn paused_flush_kickoff(&self, group: &GroupId, pty_id: u32) -> Option<(u64, KickoffTreatment)> {
        let front = {
            let queues = self.queues.read();
            queues.get(&pty_id).and_then(|q| q.front()).cloned()
        }?;
        // A queue is keyed by pane and a pane belongs to one group, so this
        // can only fail if `flush_paused_queues`' own pane filter and this
        // read ever disagree. Cheap to state; the safe answer either way is
        // no treatment.
        if front.group.as_ref() != Some(group) {
            return None;
        }
        let kind = front.delivery_kind;
        // The SAME computation `deliver_prompt`'s front door runs, off the
        // same two inputs (the target's role/CLI and the group's posture) —
        // `confirm_autopilot` is the one flag the kind cannot decide alone.
        // Resolved with the `queues` lock released: this file's order is
        // agents, then groups, and never either under `queues`.
        let confirm_autopilot = self.agent(&front.agent_id).is_some_and(|a| {
            let groups = self.groups.lock_safe();
            groups.get(group).is_some_and(|g| {
                should_confirm_copilot_autopilot(
                    g.guardrails.cli_for_block(&a.block, a.role),
                    g.guardrails.auto_ops || a.role == Role::Planner,
                    kind.confirms_autopilot_dialog(),
                )
            })
        });
        paused_flush_treatment(kind, confirm_autopilot).map(|t| (front.id, t))
    }

    /// #569: tell the orchestrator what a just-ended pause DISCARDED — under
    /// a build that predates enqueue-while-paused.
    ///
    /// **Why this exists at all.** Before option 2, a delivery into a paused
    /// group was destroyed and its caller told `Ok` — after #445/#523 the only
    /// remaining non-crash path where a payload the sender was told succeeded
    /// ceased to exist. It was audited and nothing else, so a worker's
    /// `report("done")` fired during a pause simply evaporated and the
    /// orchestrator went on waiting for it.
    ///
    /// **And why it still exists — for two reasons, not one** (review B2).
    /// Option 2 fixed the behavior, not the history: a group paused under an
    /// older loomux and resumed under this one has `prompt-suppressed-paused`
    /// lines in its audit log and those payloads really are gone. AND this
    /// build can still lose one: `enqueue_text`'s `RejectFull` arm drops the
    /// payload when the target pane is at `queue::QUEUE_MAX_PER_PANE`, which a
    /// long pause reaches easily on the orchestrator's pane — the sender gets
    /// an `Err`, but before B2 nothing ever told the orchestrator, which is the
    /// #569 stall arriving through the queue instead of around it.
    ///
    /// `swallowed` is empty for the ordinary pause — one that queued everything
    /// it was given — so this returns at its first line and nothing below runs.
    /// It is NOT empty merely because the group never met an old build.
    ///
    /// **Reads, never mutates.** The suppressed set comes out of the audit log
    /// — no new state, no new `queues` mutation, and therefore no
    /// `persist_queues` obligation (see `docs/design/orchestration.md`'s
    /// persistence table). The one write anywhere on this path is the notice
    /// itself, admitted through the ordinary front door
    /// (`deliver_to_orchestrator` → `deliver_prompt` → `enqueue_text`), which
    /// already owns that obligation and is already a row in that table.
    ///
    /// **Admitted past the cap, and that is the point** (rev-128). The notice
    /// goes in with `queue::EnqueueReason::PauseLossNotice`, the one reason
    /// `admit` lets exceed `QUEUE_MAX_PER_PANE` — by exactly one entry. Without
    /// it, the flagship case defeats itself: when the overflow happened on the
    /// ORCHESTRATOR's own pane (where a fleet's reports converge, so where a
    /// long pause fills first) that pane is still at capacity at resume, so the
    /// notice reporting the destroyed payloads was CERTAIN — not merely
    /// likely — to be destroyed by the same cap. And the badge fallback below
    /// did not cover it either: by then the pane carries
    /// `note_queue_capacity`'s at-capacity badge, so `pause_badge_decision`'s
    /// never-stomp-another-badge rule skips the pause badge, leaving the audit
    /// tally as the only trace of a lost report.
    ///
    /// The exemption stays a bound rather than a hole: one entry deep, one
    /// notice per resume, emitted only when something was actually lost, and
    /// reachable from this call site alone (`deliver_prompt_as` is private). A
    /// second resume that finds the headroom still occupied is refused like
    /// anything else, `delivered: false` is recorded honestly, and the badge
    /// path below is what carries it from there.
    ///
    /// **And the notice must not be the next silent loss.** It is an in-band
    /// delivery, so it fails when the group has no live orchestrator left to
    /// take it — the long unattended pause, i.e. the case with the most to
    /// report. On failure every distinct live target gets the badge channel
    /// instead, which has no role suppression and needs no orchestrator
    /// (#563's argument). A pane already carrying somebody else's badge is
    /// left alone: that badge is telling the human to look at the same pane
    /// and it is not ours to overwrite, matching `note_queue_capacity`.
    fn announce_pause_suppression(&self, group: &GroupId, swallowed: &PauseSuppression) {
        if swallowed.items.is_empty() {
            return;
        }
        let outcome = self.deliver_to_orchestrator_as(
            group,
            &pause_suppression_notice(swallowed),
            brand::AUDIT_ACTOR,
            queue::EnqueueReason::PauseLossNotice,
        );
        self.audit(group, brand::AUDIT_ACTOR, "pause-suppression-notice", json!({
            "count": swallowed.items.len(),
            "window_start_seen": swallowed.window_start_seen,
            "delivered": outcome.is_ok(),
            "error": outcome.as_ref().err(),
        }));
        let delivered = outcome.is_ok();
        let mut seen: HashSet<&str> = HashSet::new();
        for it in &swallowed.items {
            if !seen.insert(it.to.as_str()) {
                continue; // one badge per pane, however many payloads it lost
            }
            let alive = self.agent(&it.to).is_some_and(|a| a.status != AgentStatus::Dead);
            if pause_badge_decision(delivered, alive, self.stranded_note(&it.to)) {
                self.mark_stranded(group, &it.to, Some(StrandedBlocker::PauseSuppressed));
            }
        }
    }

    /// Flip a worker/reviewer between idle (awaiting/finished a task) and
    /// active. `idle` true stamps `idle_since_ms = now`; false clears it.
    /// No-op for the orchestrator, which is never idle-reaped.
    pub(in crate::orchestration) fn set_agent_idle(&self, agent_id: &str, idle: bool) {
        let mut agents = self.agents.lock_safe();
        if let Some(a) = agents.get_mut(agent_id) {
            if a.role == Role::Orchestrator {
                return;
            }
            a.idle_since_ms = idle.then(now_ms);
            if !idle {
                // (Re)assigned work: restart the watchdog's silence clock and
                // clear its anti-nag latch so the fresh stall gets a nudge.
                a.last_progress_ms = now_ms();
                a.watchdog_notified = false;
                // #852 review finding 3: also clear the suppression latch —
                // left set, a later tick's watch-resolved branch would fire on
                // a latch that no longer describes anything (the watch this
                // NEW assignment might hold, if any, hasn't even been checked
                // yet), stamping a clock this fresh-assignment stamp already
                // owns and delaying a genuine stall by up to one extra window.
                a.watchdog_watch_suppressed = false;
                // New work supersedes a prior done/blocked report — drop its
                // attention latch so a stale badge doesn't linger.
                self.attn_reports.lock_safe().remove(agent_id);
            }
        }
    }

    /// Ids of workers/reviewers whose idle time has crossed their group's
    /// `idle_kill_minutes`. Pure selection (no killing) so the reaper policy
    /// is testable at a chosen `now`.
    ///
    /// **A `Role::Manager` pane is exempt (#1161 M3), by CLASS rather than by
    /// hint** — the reaper's premise is false for it in the strongest form the
    /// premise can be false. A manager spawns with no task (the human's first
    /// message is the task), so `idle_since_ms` stamps at BIRTH: unguarded, it
    /// is taken on the first sweep past `idle_kill_minutes`, before the human
    /// has typed anything, and the notice that it happened goes to the
    /// orchestrator's pane rather than to the human sitting in front of the one
    /// that vanished. An idle manager is a manager whose human is away — that
    /// is its normal state, not an abandoned slot.
    ///
    /// Keyed on `a.role`, never on the `standing` hint set below: the hint set
    /// is a property of the group's ROSTER (which block ids are liaisons) and
    /// the class is a property of the AGENT, so a manager is exempt even on a
    /// pane whose block id no longer resolves in the roster (a workflow file
    /// edited mid-session — #459 treats that as a live reality).
    ///
    /// **A liaison block is exempt (#891 S4)**, and it is the only hint that is.
    /// The reaper's premise — audited as "a slot the orchestrator wasn't using
    /// was reclaimed" — is false for the one pane the orchestrator is not the
    /// user of. Every signal that clears the idle clock is machine-side
    /// (`send_prompt`, a fresh task at spawn) and a human typing into a pane
    /// touches none of them, so a liaison in mid-conversation is indistinguishable
    /// from an abandoned one: it stamps its own clock the moment it
    /// `report`s `done`/`blocked` and is then reaped out from under the human,
    /// whose only notice of it goes to the OTHER pane. The cost argument does not
    /// carry it either — an idle pane spends nothing, and the slot it holds is
    /// deliberate (`docs/design/liaison.md`: size the roster +1).
    ///
    /// The hint is read from the group's own roster via the agent's recorded
    /// block, never from anything the agent supplied — the same source
    /// `record_verdict`'s deny layer reads.
    pub fn idle_reap_candidates(&self, now: u64) -> Vec<String> {
        // One pass, under one lock: the threshold and the group's liaison block
        // ids (plural — a roster may declare more than one, and every one of
        // them is a standing pane).
        let policy: HashMap<GroupId, (u32, HashSet<String>)> = self
            .groups
            .lock_safe()
            .iter()
            .map(|(id, g)| {
                let standing: HashSet<String> = g
                    .guardrails
                    .blocks
                    .iter()
                    .filter(|b| b.role_hint.as_deref() == Some("liaison"))
                    .map(|b| b.id.clone())
                    .collect();
                (id.clone(), (g.guardrails.idle_kill_minutes, standing))
            })
            .collect();
        self.agents
            .lock_safe()
            .values()
            .filter(|a| {
                // #2519: `is_fixture`, which is the same set the hand-spelled
                // `Orchestrator | Manager` named plus `Role::Lead`. A lead pane
                // is silent exactly when its human is reading, so reaping one
                // would close a human's own pane under them mid-thought — the
                // manager's argument, on a pane the human is even more
                // literally sitting in. Its CHILDREN are ordinary workers and
                // are reaped unchanged.
                !a.role.is_fixture() && a.status == AgentStatus::Running
            })
            .filter(|a| {
                let Some((t, standing)) = policy.get(&a.group) else { return false };
                !standing.contains(&a.block) && idle_should_kill(a.idle_since_ms, now, *t)
            })
            .map(|a| a.id.clone())
            .collect()
    }

    /// Gate one cadenced backend tick (#1609, plan §3 Phase 2.1 item 3).
    ///
    /// `None` means SKIP this tick: the registry is wedged, and a tick that
    /// runs anyway just adds one more parked thread to the pile #1600 §1.2 is
    /// about. `Some(scope)` means run, with the body inside a
    /// [`budget::MutationScope`] so no enclosing budget can ever unwind a
    /// half-applied mutation out of a tick.
    ///
    /// **It probes `agents` and `groups` rather than "the tick's own entry
    /// lock", and that is a deliberate correction to the plan's wording.** The
    /// entry lock is not a usable concept here: three of these ticks enter
    /// through `agent_output_totals`/`attention_inputs`, whose first
    /// acquisition is `app` — a trivial cell that says nothing about whether
    /// the registry is wedged — and then park on `agents` two frames down.
    /// Bounding whichever lock happens to be lexically first would produce a
    /// gate that passes and a tick that parks anyway. `agents` and `groups` are
    /// the registry's two core maps — every agent-scoped and every
    /// group-scoped read goes through one of them — so a tick that clears both
    /// is one the registry can currently serve. (Stated structurally rather
    /// than as a share of the acquisition sites: that count depends on how you
    /// match a call site, and two honest countings of this file disagree.)
    ///
    /// **What this buys, and what it does not.** It stops a cadenced loop
    /// ADDING a parked thread to a registry that is ALREADY wedged, which is
    /// the accumulation half of the incident chain. It does NOT bound a tick
    /// that wedges midway: that tick waits, by design, because its body
    /// mutates and an abandoned mutation is worse than a slow one. Phase 0's
    /// watchdog is what reports that case, with the holder named.
    ///
    /// The probe is released immediately and is not mutual exclusion — the
    /// question is "can the registry serve anyone right now", not "may I have
    /// this lock". A wedge arriving in the gap is the mid-tick case above.
    pub(in crate::orchestration) fn tick_gate(&self, tick: &'static str) -> Option<budget::MutationScope> {
        match self.agents.lock_within(budget::TICK_LOCK_BUDGET) {
            Ok(probe) => drop(probe),
            Err(busy) => {
                crate::obs::breadcrumb("tick-skipped", &format!("tick={tick} {}", busy.detail()));
                return None;
            }
        }
        match self.groups.lock_within(budget::TICK_LOCK_BUDGET) {
            Ok(probe) => drop(probe),
            Err(busy) => {
                crate::obs::breadcrumb("tick-skipped", &format!("tick={tick} {}", busy.detail()));
                return None;
            }
        }
        Some(budget::MutationScope::enter())
    }

    pub fn reap_idle_agents(&self, now: u64) -> Vec<String> {
        let Some(_tick) = self.tick_gate("reap_idle_agents") else { return Vec::new() };
        let mut killed = Vec::new();
        for id in self.idle_reap_candidates(now) {
            let Some(a) = self.agent(&id) else { continue };
            let mins = self
                .group(&a.group)
                .map(|g| g.guardrails.idle_kill_minutes)
                .unwrap_or(0);
            // Re-check against the agent's *current* idle state: selection and
            // kill happen under separate locks, so a worker prompted in that
            // window (idle clock cleared) must not be killed.
            if !idle_should_kill(a.idle_since_ms, now, mins) {
                continue;
            }
            self.audit(&a.group, brand::AUDIT_ACTOR, "idle-kill",
                json!({ "agent": id, "name": a.name, "idle_minutes": mins }));
            // #533-B: audit-only, like the exit notice this kill also
            // produces. The reaper only ever takes agents that are IDLE —
            // no task in flight — so the whole event is "a slot the
            // orchestrator wasn't using was reclaimed", which `list_agents`
            // answers on demand. Prompting for it spent a turn per reaped
            // agent to say nothing the roster didn't already show. A reaper
            // mode that killed agents WITH work would be a different event
            // and must prompt — see `exit_notice_route`'s doc.
            self.audit_demoted_exit_notice(
                &a.group,
                &a.id,
                Some(ExitInitiator::IdleTimeout),
                &format!(
                    "[orrerix] idle-kill guardrail: agent {} ({}) sat without a task for {mins}+ min and was terminated to contain cost. Respawn a worker when you have work for it.",
                    a.name, a.id
                ),
            );
            let _ = self.kill_agent_as(&id, ExitInitiator::IdleTimeout);
            killed.push(id);
        }
        killed
    }

    // ---------- watchdog: stalled-agent detection ----------

    /// Record that an agent just did something loomux can see (reported,
    /// messaged the orchestrator): reset its watchdog silence clock and clear
    /// the anti-nag latch so a *later* stall still earns a fresh nudge. No-op
    /// for the orchestrator (never watchdogged). Output-driven activity is
    /// handled separately in `watchdog_tick` via the pty counter.
    pub fn note_agent_activity(&self, agent_id: &str) {
        let mut agents = self.agents.lock_safe();
        if let Some(a) = agents.get_mut(agent_id) {
            if a.role == Role::Orchestrator {
                return;
            }
            a.last_progress_ms = now_ms();
            a.watchdog_notified = false;
            // #852 review finding 3: this is a sign of life too, so it must
            // clear the suppression latch alongside the notice latch — a
            // `message_orchestrator` call between a suppressed stall and its
            // watch resolving would otherwise leave `watchdog_watch_suppressed`
            // set on a latch the agent's own activity already made stale,
            // stamping the clock a tick late and delaying a genuine stall by
            // up to one extra window.
            a.watchdog_watch_suppressed = false;
        }
    }

    /// Stamp the shared agent-acknowledgment clock: this agent's own process
    /// just reached loomux and invoked an MCP tool (#535).
    ///
    /// Called from exactly one place — the `tools/call` arm of `mcp::dispatch`,
    /// before the tool runs — so it covers **every tool and every role** with
    /// no per-tool opt-in to forget. Contrast `note_agent_activity` above,
    /// which is the watchdog's silence clock: stamped from a single tool, and
    /// a deliberate no-op for the orchestrator.
    ///
    /// Three deliberate properties, each load-bearing for a consumer:
    /// - **Role-agnostic.** An orchestrator compacts and gets re-grounded like
    ///   anything else, so this must not skip it the way the watchdog does.
    /// - **Stamped before the tool runs, so a FAILED call still counts.** The
    ///   claim is "the agent is alive and executing its contract", and a call
    ///   that returns `permission denied` proves that exactly as well as one
    ///   that succeeds.
    /// - **Monotone.** `max`, never a bare assignment: a clock that a later
    ///   caller could rewind would let one consumer erase another's evidence.
    ///
    /// A `Role::Solo` pane and any unknown id are simply absent from the
    /// roster, so this is a no-op for them rather than a special case.
    pub fn note_agent_ack(&self, agent_id: &str) {
        let mut agents = self.agents.lock_safe();
        if let Some(a) = agents.get_mut(agent_id) {
            a.last_mcp_activity_ms = a.last_mcp_activity_ms.max(now_ms());
        }
    }

    /// Read the shared agent-acknowledgment clock (#535). `None` for an id not
    /// in the roster; `Some(0)` for an agent that has never called a tool —
    /// `agent_acted_since` treats that as "no ack", never as a fresh one.
    ///
    /// Public so a consumer outside `compact_nudge_tick` (e.g. #539's
    /// unconfirmed-delivery detector) can read the signal without reaching
    /// into `AgentEntry` or taking the agents lock itself.
    pub fn last_mcp_activity_ms(&self, agent_id: &str) -> Option<u64> {
        self.agents.lock_safe().get(agent_id).map(|a| a.last_mcp_activity_ms)
    }

    /// Test seam (#535): place the ack clock at an exact time.
    ///
    /// `note_agent_ack` stamps real `now_ms()`, but `compact_nudge_tick` is
    /// driven with a synthetic `now` (`FAR`, far beyond any wall clock) so the
    /// multi-minute retry windows can be crossed without sleeping. Writing the
    /// REAL field the real reader reads — rather than mocking a clock — keeps
    /// the production comparison (`agent_acted_since`) under test; the same
    /// approach, for the same reason, as #518's `set_user_input_ms_for_test`.
    ///
    /// Unlike `note_agent_ack` this is NOT clamped monotone: a test must be
    /// able to place the stamp BEFORE the attempt to pin the negative case.
    #[doc(hidden)]
    pub fn set_last_mcp_activity_ms_for_test(&self, agent_id: &str, ms: u64) {
        if let Some(a) = self.agents.lock_safe().get_mut(agent_id) {
            a.last_mcp_activity_ms = ms;
        }
    }

    /// Snapshot every agent's monotonic pty output counter. Needs the app's
    /// `PtyManager`, so it yields an empty map without an app handle (unit
    /// tests drive `watchdog_tick` with synthetic counters instead).
    pub(in crate::orchestration) fn agent_output_totals(&self) -> HashMap<String, u64> {
        let Some(app) = self.app.lock_safe().clone() else {
            return HashMap::new();
        };
        let ptys = app.state::<crate::pty::PtyManager>();
        self.output_totals_from(&ptys)
    }

    /// [`Self::agent_output_totals`] with the pty manager passed in — same
    /// integration-test seam, and same reason, as
    /// [`Self::compact_signals_from`], which it runs beside on every
    /// compact-nudge wake.
    ///
    /// Snapshot-then-release for the same reason (#743 S7). Each read is only
    /// a counter load, so the CPU under the guard was never the problem here;
    /// the LOCK ORDER was. Reading a pty while holding `agents` makes the pair
    /// nested, and a nested pair is only safe as long as every future caller
    /// remembers the direction — whereas a snapshot cannot be got wrong. It is
    /// three lines and it is the sibling of the read above, on the same wake:
    /// leaving one of the two nested would have made the invariant a
    /// coincidence rather than a rule.
    #[doc(hidden)] // pub for integration tests
    pub fn output_totals_from(&self, ptys: &crate::pty::PtyManager) -> HashMap<String, u64> {
        let panes: Vec<(String, u32)> = self
            .agents
            .lock_safe()
            .values()
            .filter_map(|a| Some((a.id.clone(), a.pty_id?)))
            .collect();
        panes
            .into_iter()
            .filter_map(|(id, pty_id)| Some((id, ptys.output_total(pty_id)?)))
            .collect()
    }

    /// One watchdog pass. For each *working* agent (running worker/reviewer
    /// with a task assigned — idle clock clear), fold in the latest pty output
    /// counter from `outputs`: any growth is activity that resets the silence
    /// clock and the anti-nag latch. An agent silent (no output, no report)
    /// past its group's `watchdog_stall_minutes` earns exactly one audited
    /// `[orrerix]` nudge to the orchestrator suggesting get_output + re-send —
    /// UNLESS the stall is not NEWS, in which case it is SUPPRESSED instead
    /// (audited as `watchdog-suppressed` carrying a `why`, never delivered).
    /// There are three such reasons and they are decided in two places: a live
    /// `notify_when` watch (#852, `live-watch`) is read from `has_watch` under
    /// the lock below, because the same read drives the latch transition; the
    /// other two (`exit-initiated`, `driven-lane`) are
    /// [`watchdog_suppress_reason`]'s, applied on the lock-free second pass
    /// because one of them costs a file read under another lock.
    ///
    /// **Every suppression that can outlive its cause is BOUNDED, and the one
    /// that cannot is stated.** `live-watch` re-arms when the watch resolves
    /// (#852, below). `driven-lane` re-arms on the first tick where no live drive
    /// owns the pane — released on the drive's absence rather than on elapsed
    /// time, and audited as `watchdog-rearmed`; without it a lane stalled under a
    /// drive that was then cancelled, or whose driver died, would be latched
    /// silent for good, where the base announced once (rev-std round 1, N2).
    /// `exit-initiated` deliberately does NOT re-arm: a pane something in this
    /// process asked to end is not coming back to be nudged. The residual that
    /// leaves is real and small — a kill whose pty teardown hangs leaves an
    /// alive-but-dying pane suppressed with nothing bounding the window, because
    /// `record_exit_initiator` is first-writer-wins and is never cleared. Nothing
    /// today produces it (`kill_agent` refuses a pane with no pty, and the
    /// teardown is milliseconds), so it is disclosed rather than guarded; a
    /// reaper that could leave a pane in that state indefinitely would need this
    /// arm bounded too.
    ///
    /// Paused groups are skipped entirely — delivery is suppressed
    /// there anyway, so we must not spend the one-notice budget while paused.
    /// Returns the notified (never suppressed) agent ids. Split from the pty
    /// read (`agent_output_totals`) so the stall / anti-nag / pause / watch
    /// logic is testable with synthetic counters and no threads. `has_watch`
    /// maps an agent id to the live `notify_when` watch ids it currently holds
    /// (#248/#852) — an injected input like `outputs`, not a lock this
    /// function takes itself, so it stays testable with a plain `HashMap` and
    /// no registry-watches state.
    ///
    /// The suppression latch (`watchdog_watch_suppressed`) does double duty:
    /// while the watch stays live it prevents re-auditing every tick (the same
    /// anti-nag shape as `watchdog_notified`, which it is set alongside), and
    /// on the FIRST tick where the agent no longer holds any live watch (it
    /// fired, expired, failed-out, or was cancelled) it is read to detect that
    /// transition — at which point the clock is reset to `now` and both
    /// latches cleared, handing the agent a fresh full stall window rather
    /// than immediately re-flagging on whatever was left of the old,
    /// already-expired window. Only a genuinely fresh silence past that new
    /// window earns a notice — the combination #852 calls a real stall.
    pub fn watchdog_tick(
        &self,
        now: u64,
        outputs: &HashMap<String, u64>,
        has_watch: &HashMap<String, Vec<String>>,
    ) -> Vec<String> {
        let thresholds: HashMap<GroupId, u32> = self
            .groups
            .lock_safe()
            .iter()
            .map(|(id, g)| (id.clone(), g.guardrails.watchdog_stall_minutes))
            .collect();
        let paused = self.paused.lock_safe().clone();

        // First pass under the agents lock: refresh counters and pick who to
        // nudge or suppress. Delivery (which types into a pane and can block)
        // happens after the lock is released.
        // `to_notify` carries the recorded exit initiator so the second, LOCK-FREE
        // pass can finish the suppression decision (#3040 N2). Two of the three
        // reasons a stall is not news are decided there rather than here: `rd_owner`
        // reads a file under its own state lock, and taking that while holding
        // `agents` would nest the pair in a direction nothing else in this file uses
        // (`lock-order.md` §2). The initiator rides along because it is an `agents`
        // field and copying it is free.
        let mut to_notify: Vec<(String, GroupId, String, u32, Option<ExitInitiator>)> = Vec::new();
        let mut to_suppress: Vec<(String, GroupId, String, u32, Vec<String>, &'static str)> =
            Vec::new();
        // Panes currently latched as `driven-lane`, whatever the stall clock says
        // (rev-std round 1, N2). They are collected UNCONDITIONALLY — before the
        // threshold is consulted — because that is the whole defect: once the
        // latch is set, `watchdog_should_notify` answers false forever, so a pane
        // whose drive has ended would never be looked at again. Whether the drive
        // is still there is a `rd_owner` read, which cannot happen under this
        // lock, so the question is carried out and answered below.
        let mut latched_driven: Vec<(String, GroupId)> = Vec::new();
        {
            let mut agents = self.agents.lock_safe();
            for a in agents.values_mut() {
                // Only agents actively working: running, not the orchestrator,
                // not the manager, and currently assigned (idle_since_ms
                // clear). This excludes idle, done/blocked, dead, and reaped
                // agents by construction.
                //
                // #1161 M3: `Role::Manager` is named EXPLICITLY rather than
                // left to the idle-clock clause, even though a manager opened
                // by the launch path arrives task-less and is therefore already
                // skipped by it. The two are different rules that happen to
                // agree today: the idle clause says "nothing is assigned", and
                // whether a manager's clock is clear is a fact about paths that
                // may change, while "loomux never nags the orchestrator about
                // the human's own pane" is the rule. A stall notice about a
                // manager is a false report either way — its silence means the
                // human is reading, and the notice lands in a pane the human is
                // not looking at, naming a pane they are.
                //
                // #2519 folds `Role::Lead` into the same answer through
                // `is_fixture`, and it needs the explicit-naming argument above
                // even more than a manager does: a lead pane IS assigned work
                // (the human's), so the idle clause would not skip it, and its
                // quiet stretches are the human thinking. There is also nobody
                // to notify — a lead group has no orchestrator — so a stall
                // notice about one would be a false report with no recipient.
                if a.role.is_fixture()
                    || a.status != AgentStatus::Running
                    || a.idle_since_ms.is_some()
                {
                    continue;
                }
                // Asked before every gate below, including the paused-group one:
                // a drive that ended while its group was paused must still
                // re-arm, and the alternative is a latch that outlives the pause.
                if a.watchdog_drive_suppressed {
                    latched_driven.push((a.id.clone(), a.group.clone()));
                }
                // Output growth = activity: reset the clock and both latches,
                // and this tick can't also flag or suppress the agent.
                if let Some(&cur) = outputs.get(&a.id) {
                    if cur > a.last_output_total {
                        a.last_output_total = cur;
                        a.last_progress_ms = now;
                        a.watchdog_notified = false;
                        a.watchdog_watch_suppressed = false;
                        continue;
                    }
                }
                // A paused group's agents idle out on purpose; never nudge and
                // never burn their one-notice budget while paused.
                if paused.contains(&a.group) {
                    continue;
                }
                let watch_ids = has_watch.get(&a.id);
                let watching = watch_ids.is_some_and(|ids| !ids.is_empty());

                // #852: the watch that was suppressing this stall has resolved
                // since we last looked — give the agent a fresh full window
                // starting now instead of evaluating (and possibly firing)
                // against the old, already-expired one.
                if a.watchdog_watch_suppressed && !watching {
                    a.watchdog_watch_suppressed = false;
                    a.watchdog_notified = false;
                    a.last_progress_ms = now;
                    continue;
                }

                let threshold = thresholds.get(&a.group).copied().unwrap_or(0);
                if watchdog_should_notify(a.last_progress_ms, now, threshold, a.watchdog_notified) {
                    let minutes = (now.saturating_sub(a.last_progress_ms) / 60_000) as u32;
                    if watching {
                        a.watchdog_notified = true;
                        a.watchdog_watch_suppressed = true;
                        to_suppress.push((
                            a.id.clone(),
                            a.group.clone(),
                            a.name.clone(),
                            minutes,
                            watch_ids.cloned().unwrap_or_default(),
                            WATCHDOG_SUPPRESS_LIVE_WATCH,
                        ));
                    } else {
                        // The anti-nag latch is set on BOTH paths, including the one
                        // the second pass may still divert to suppression: whichever
                        // way this stall ends up, it is spoken about once.
                        a.watchdog_notified = true;
                        to_notify.push((
                            a.id.clone(),
                            a.group.clone(),
                            a.name.clone(),
                            minutes,
                            a.killed_by,
                        ));
                    }
                }
            }
        }

        let mut still_news: Vec<(String, GroupId, String, u32)> = Vec::new();
        // The second pass, lock-free: finish the decision, then audit every
        // suppression through the one row (#3040 N2). `why` is what makes that row
        // readable now that it carries three different facts — #852's "it holds a
        // live watch" is no longer the only way a stall stops being news.
        for (id, group, name, minutes, killed_by) in to_notify {
            match watchdog_suppress_reason(
                killed_by,
                || self.rd_owner(&group, &id).is_some(),
            ) {
                Some(why) => {
                    // Only the DRIVE arm latches: `exit-initiated` needs no
                    // re-arm, because a pane something asked to end is not coming
                    // back to be nudged, and `watchdog_notified` already stops it
                    // being announced twice on the way out.
                    if why == WATCHDOG_SUPPRESS_DRIVEN_LANE {
                        if let Some(a) = self.agents.lock_safe().get_mut(&id) {
                            a.watchdog_drive_suppressed = true;
                        }
                    }
                    to_suppress.push((id, group, name, minutes, Vec::new(), why))
                }
                None => still_news.push((id, group, name, minutes)),
            }
        }

        // The re-arm (rev-std round 1, N2), mirroring #852's watch arm: on the
        // first tick where no live drive owns a pane we suppressed as
        // `driven-lane`, clear both latches and give it a fresh FULL stall window
        // from now — rather than firing immediately on whatever was left of the
        // old, already-expired one.
        //
        // Released on independent evidence (the drive is gone), never on elapsed
        // time, and it is deliberately not hooked to `cancel_review_drive_with`:
        // a drive also ends by completing, by going terminal, or by its own
        // entry being reconciled away, and a hook on the cancel path alone would
        // bound exactly one of those four.
        for (id, group) in latched_driven {
            if self.rd_owner(&group, &id).is_some() {
                continue;
            }
            if let Some(a) = self.agents.lock_safe().get_mut(&id) {
                a.watchdog_drive_suppressed = false;
                a.watchdog_notified = false;
                a.last_progress_ms = now;
            }
            self.audit(&group, brand::AUDIT_ACTOR, "watchdog-rearmed", json!({
                "agent": id, "why": WATCHDOG_SUPPRESS_DRIVEN_LANE,
            }));
        }

        for (id, group, name, minutes, watch_ids, why) in to_suppress {
            self.audit(&group, brand::AUDIT_ACTOR, "watchdog-suppressed", json!({
                "agent": id, "name": name, "silent_minutes": minutes, "watch_ids": watch_ids,
                "why": why,
            }));
        }

        let mut notified = Vec::new();
        for (id, group, name, minutes) in still_news {
            // #852 review finding 2: `has_live_watch` dropped — by
            // construction an agent reaching this branch (not `to_suppress`
            // above) never holds a live watch, so the field was a constant
            // `false` on every line.
            self.audit(&group, brand::AUDIT_ACTOR, "watchdog-stall", json!({ "agent": id, "name": name, "silent_minutes": minutes }));
            let _ = self.deliver_to_orchestrator(&group, &watchdog_stall_notice(&name, &id, minutes), brand::AUDIT_ACTOR);
            notified.push(id);
        }
        notified
    }

    /// One full watchdog cycle: read pty counters, then tick. Called on a
    /// timer by `start_watchdog`.
    pub fn run_watchdog(&self, now: u64) -> Vec<String> {
        let Some(_tick) = self.tick_gate("run_watchdog") else { return Vec::new() };
        let outputs = self.agent_output_totals();
        // Same registry state `notify_tick`/`list_notifications` read (#248) —
        // no second store. Agent id -> its live watch ids: `watchdog_tick`
        // needs the ids too now (#852), to audit which watch suppressed a
        // stall — an agent can hold more than one (`MAX_WATCHES_PER_AGENT`).
        let mut has_watch: HashMap<String, Vec<String>> = HashMap::new();
        for w in self.watches.lock_safe().values() {
            has_watch.entry(w.agent.clone()).or_default().push(w.id.clone());
        }
        // #3304 S1, bound (2): delivery triage's deferral deadline. It rides
        // the watchdog's timer rather than opening a second one because it is
        // the same quantity the watchdog exists for — "has something been
        // silent too long" — asked of a store instead of a pane, and a
        // feature that suppresses notices must not also be the feature that
        // adds a thread. A no-op in microseconds for every group with no
        // `triage:` block: `triage_flush_tick` reads the roster, finds no
        // enabled policy, and returns.
        //
        // Deliberately NOT inside `watchdog_tick`, which is the pure decision
        // half over injected counters; this is a flush that delivers.
        self.triage_flush_tick(now);
        self.watchdog_tick(now, &outputs, &has_watch)
    }

    // ---------- autonomous mode (#83): idle-tick + budget enforcement ----------

    /// Read every live orchestrator pane's output counter and last human-keystroke
    /// time — the raw inputs `idle_tick_tick` needs to judge output-silence.
    /// Split from the decision (as `agent_output_totals` is for the watchdog) so
    /// the tick logic is testable with synthetic maps and no pty. Empty without an
    /// app handle (unit tests drive `idle_tick_tick` directly).
    fn orchestrator_activity(&self) -> (HashMap<String, u64>, HashMap<String, u64>) {
        let mut outs = HashMap::new();
        let mut ins = HashMap::new();
        let Some(app) = self.app.lock_safe().clone() else {
            return (outs, ins);
        };
        let ptys = app.state::<crate::pty::PtyManager>();
        for a in self.agents.lock_safe().values() {
            if a.role != Role::Orchestrator {
                continue;
            }
            let Some(pid) = a.pty_id else { continue };
            if let Some(t) = ptys.output_total(pid) {
                outs.insert(a.id.clone(), t);
            }
            if let Some(u) = ptys.last_user_input_ms(pid) {
                ins.insert(a.id.clone(), u);
            }
        }
        (outs, ins)
    }

    /// One idle-tick pass. For each **autonomous, non-paused** group's running
    /// orchestrator, fold in the latest pty output counter and last human-input
    /// time: output growth (the orchestrator acting) resets the quiet clock and
    /// the one-notice latch; recent human input also defers the clock (never tick
    /// while the human steers — the belt-and-suspenders gate on top of
    /// output-silence). **#496 hardening:** the input fold is clamped — input
    /// alone can defer the clock at most `idle_tick_input_defer_max_minutes` past
    /// the last real output (`AgentEntry.last_output_progress_ms`), so a signal
    /// that keeps refreshing (a phantom stamp, or anything else not yet modeled)
    /// makes the tick late, never silent forever; see
    /// `DEFAULT_IDLE_TICK_INPUT_DEFER_MAX_MINUTES`'s doc for the deadlock this
    /// closes. An orchestrator output-quiet past `IDLE_TICK_MINUTES` and
    /// not already latched clears the quiet-window gate — but for a group with
    /// the intake gate ON (`intake_poll_minutes > 0`, #332), clearing the quiet
    /// window is necessary, not sufficient: `intake::idle_tick_gate` decides
    /// whether there's actually anything to wake for (a host-side label/PR-check
    /// signal, an outstanding CI watch this tick's sweep still owes, a watchdog
    /// stall nobody resolved) or whether the bounded fallback has come due — and
    /// a tick that clears neither is SKIPPED, audited with its reason, and
    /// retried no sooner than the next intake-poll interval (never silently, and
    /// never in a busy-loop). A group with the gate OFF fires exactly as it did
    /// before #332. Paused groups are skipped wholesale (delivery is suppressed
    /// there; don't burn the latch). Returns the notified orchestrator ids. Split
    /// from the pty read (`orchestrator_activity`) so the gate / latch / cap /
    /// pause logic is testable with synthetic counters — the `watchdog_tick`
    /// shape.
    ///
    /// **#864 — the fallback backs off while a group stays delta-free.** The
    /// bounded fallback is what keeps a poller bug from silencing the
    /// orchestrator, but at a fixed cadence it also charged a fully parked
    /// group (every open item human-gated, no live delegates, nothing changing
    /// anywhere the host can see) that cadence forever: ~30 consecutive
    /// findings-free wakes over one parked weekend, each an API turn over the
    /// orchestrator's whole prefix. So each fallback wake that finds NOTHING
    /// (`heartbeat`) doubles this group's effective fallback interval, up to
    /// `idle_tick_fallback_max_minutes` (`intake::fallback_interval_minutes`),
    /// and the streak resets to zero — back to the base cadence — on ANY of:
    /// a fire with a real signal behind it, human input in the pane since the
    /// last fire, or a live delegate in the group. Note what is NOT on that
    /// list: the orchestrator's own output, which in a parked group is mostly
    /// its reply to the previous wake (see the output branch below). The
    /// guarantee is unchanged in kind and only coarser in degree: the
    /// orchestrator is still woken unconditionally, at worst once per ceiling.
    pub fn idle_tick_tick(
        &self,
        now: u64,
        outputs: &HashMap<String, u64>,
        inputs: &HashMap<String, u64>,
    ) -> Vec<String> {
        /// One decided delivery, carried from the scan loop (which holds the
        /// `agents` lock) to the delivery loop (which must not). Named fields
        /// rather than the positional tuple this started as: #864 added two
        /// more observability values and a 7-tuple stops being readable.
        ///
        /// - `gate_enabled` is false only for the `intake_minutes == 0` bypass
        ///   (the pre-#332 legacy fire).
        /// - `heartbeat` is true only when the gate fired SOLELY because the
        ///   bounded fallback came due — no intake signal, no pending CI watch,
        ///   no watchdog stall. The rev-95 benchtest finding (#429) was that a
        ///   delivered wake and a suppressed one were indistinguishable in the
        ///   audit log, which is how "the gate computes but does not gate" went
        ///   unnoticed; this field and `gate_enabled` exist so they aren't.
        /// - `input_defer_bound` (#496) is true only when THIS fire happened
        ///   because `idle_tick_input_defer_max_minutes` capped the input fold,
        ///   not because the pane was genuinely output-quiet.
        /// - `empty_streak` / `fallback_minutes` (#864) are the delta-free
        ///   streak AFTER this fire and the effective fallback interval that
        ///   produced it — the same reasoning applied to the backoff: a cadence
        ///   that decays silently is a cadence nobody can debug.
        struct IdleTickFire {
            id: String,
            group: GroupId,
            gate_enabled: bool,
            heartbeat: bool,
            input_defer_bound: bool,
            empty_streak: u32,
            fallback_minutes: u32,
        }
        let autonomous = self.autonomous_groups.lock_safe().clone();
        if autonomous.is_empty() {
            return Vec::new();
        }
        let paused = self.paused.lock_safe().clone();
        let tick_times = self.idle_tick_times.lock_safe().clone();
        // Per-group idle-tick window + activity floor (guardrails, live-adjustable).
        // Snapshot like `watchdog_tick` does its thresholds.
        let cfg: HashMap<GroupId, (u32, u64, u32, u32, u32, u32)> = self
            .groups
            .lock_safe()
            .iter()
            .map(|(id, g)| {
                (
                    id.clone(),
                    (
                        g.guardrails.idle_tick_minutes,
                        g.guardrails.idle_activity_floor_bytes,
                        // #429: smart-defaulted ON while autonomous unless the
                        // group explicitly opted out (`Some(0)`) or set its own
                        // cadence — inert (0) while supervised, matching the
                        // pre-#429 legacy-bypass path exactly for those groups.
                        intake::effective_intake_poll_minutes(
                            g.guardrails.intake_poll_minutes,
                            autonomous.contains(id),
                        ),
                        g.guardrails.idle_tick_fallback_minutes,
                        // #496: bound on how long human input alone may defer
                        // the tick past `last_output_progress_ms`.
                        g.guardrails.idle_tick_input_defer_max_minutes,
                        // #864: the ceiling the fallback backs off TO while
                        // this group stays delta-free.
                        g.guardrails.idle_tick_fallback_max_minutes,
                    ),
                )
            })
            .collect();
        // #332: snapshots taken BEFORE the mutable `agents` borrow below, each on
        // its own short-lived lock (a separate mutex for `watches`, a throwaway
        // immutable pass over `agents` for the watchdog signal) — never held
        // across the mutable loop, so there's no lock-ordering hazard.
        let notification_pending_groups: HashSet<GroupId> =
            self.watches.lock_safe().values().map(|w| w.group.clone()).collect();
        let watchdog_stall_groups: HashSet<GroupId> = self
            .agents
            .lock_safe()
            .values()
            .filter(|a| a.watchdog_notified)
            .map(|a| a.group.clone())
            .collect();
        // #864: groups with at least one live (running) DELEGATE — any
        // non-orchestrator agent. The fallback backoff is suppressed for them:
        // a group with agents in flight is not parked by any definition, and
        // the heartbeat tick is exactly how an orchestrator notices a delegate
        // that went quiet without reporting. Taken on its own short-lived lock
        // in the same pass-shape as `watchdog_stall_groups` directly above,
        // before the mutable `agents` borrow below.
        let live_delegate_groups: HashSet<GroupId> = self
            .agents
            .lock_safe()
            .values()
            .filter(|a| a.role != Role::Orchestrator && a.status == AgentStatus::Running)
            .map(|a| a.group.clone())
            .collect();
        let intake_pending = self.intake_pending.lock_safe().clone();
        let last_fired = self.idle_tick_last_fired_ms.lock_safe().clone();
        let empty_streak = self.idle_tick_empty_streak.lock_safe().clone();

        let mut to_notify: Vec<IdleTickFire> = Vec::new();
        let mut skipped: Vec<(GroupId, String)> = Vec::new();
        // #864: (group, new streak) for every group whose delta-free streak
        // this scan changed — collected here and applied after the `agents`
        // borrow drops, so no second lock is ever taken while holding it (the
        // same discipline the snapshots above follow in the other direction).
        let mut streak_updates: Vec<(GroupId, u32)> = Vec::new();
        {
            let mut agents = self.agents.lock_safe();
            for a in agents.values_mut() {
                if a.role != Role::Orchestrator
                    || a.status != AgentStatus::Running
                    || !autonomous.contains(&a.group)
                {
                    continue;
                }
                // Meaningful output growth = the orchestrator produced a real burst
                // (it acted): reset the quiet clock and clear the latch, and this
                // tick can't also fire. Sub-floor growth is idle repaint noise — it
                // rebaselines the counter but does NOT reset the clock, so an
                // occasional statusline/spinner frame can't starve the tick (the
                // bug where any stray byte demanded another full quiet window).
                let (threshold, floor, intake_minutes, fallback_minutes, input_defer_max_minutes, fallback_max_minutes) =
                    cfg.get(&a.group).copied().unwrap_or((
                        DEFAULT_IDLE_TICK_MINUTES,
                        DEFAULT_IDLE_ACTIVITY_FLOOR_BYTES,
                        0,
                        DEFAULT_IDLE_TICK_FALLBACK_MINUTES,
                        DEFAULT_IDLE_TICK_INPUT_DEFER_MAX_MINUTES,
                        DEFAULT_IDLE_TICK_FALLBACK_MAX_MINUTES,
                    ));
                if let Some(&cur) = outputs.get(&a.id) {
                    let meaningful = idle_output_is_activity(a.last_output_total, cur, floor);
                    a.last_output_total = cur; // rebaseline every observation
                    if meaningful {
                        a.last_progress_ms = now;
                        // #496: this is the ONLY place the output-only clock moves —
                        // real progress, never input-deferred.
                        a.last_output_progress_ms = now;
                        a.idle_tick_notified = false;
                        a.idle_tick_skip_rearm_ms = 0;
                        // #864 deliberately does NOT reset the delta-free streak
                        // here. Orchestrator output is not evidence that anything
                        // changed — the dominant source of it in a parked group is
                        // the orchestrator answering our own last wake: it sweeps,
                        // finds nothing, and goes quiet again, which is precisely
                        // the loop the backoff exists to slow down. Resetting on it
                        // would make the streak un-growable in production while
                        // still passing any test that never simulated the reply.
                        // The three resets that DO fire (a wake with a real signal
                        // behind it, human input, a live delegate) are all evidence
                        // about the world, not about the orchestrator's own talking.
                        continue;
                    }
                }
                // Belt-and-suspenders: recent human input in the pane is activity
                // — fold it into the quiet clock so a tick never lands while the
                // human is steering (mirrors attention routing's `waiting`
                // heuristic). Not latch-clearing: human typing isn't the
                // orchestrator acting on our notice, it just defers the window.
                //
                // #496 hardening: the fold is clamped so input alone can never push
                // the quiet clock more than `input_defer_max_minutes` past the last
                // REAL output (`last_output_progress_ms`) — two independent
                // heuristics keying off this same "pane has human input" signal
                // (this fold, and delivery's stranded-text/retry suppression) can
                // deadlock if the signal is ever wrong and never clears (a copilot
                // pane's own xterm auto-replies stamping it with no human present,
                // #496). `input_defer_bound` records whether THIS scan's input was
                // actually clamped — i.e. input claims to be more recent than the
                // bound allows — so a tick that fires off the back of it can be
                // audited with its own reason instead of reading as an ordinary
                // quiet-window fire.
                let mut input_defer_bound = false;
                let group_last_fired = last_fired.get(&a.group).copied().unwrap_or(0);
                // #864: this group's delta-free streak, as it stands entering
                // this scan. Every reset below writes BOTH this local (so the
                // effective interval computed further down already reflects
                // it) and `streak_updates` (so it outlives the scan) — a reset
                // that only did the latter would still widen the interval one
                // last time on the very scan that observed the group waking up.
                let mut streak = empty_streak.get(&a.group).copied().unwrap_or(0);
                if let Some(&last_in) = inputs.get(&a.id) {
                    let bound_ms = (input_defer_max_minutes as u64) * 60_000;
                    let cap = a.last_output_progress_ms.saturating_add(bound_ms);
                    let effective = last_in.min(cap);
                    if last_in > cap {
                        input_defer_bound = true;
                    }
                    if effective > a.last_progress_ms {
                        a.last_progress_ms = effective;
                    }
                    // #864: the human touched this pane since the last tick
                    // fired, so the group is not parked — reset the backoff to
                    // the base cadence. Deliberately the RAW `last_in`, not
                    // the `input_defer_max_minutes`-clamped `effective`: the
                    // clamp exists to stop a possibly-phantom input signal
                    // from muting the tick FOREVER (#496), and honoring it
                    // here would work the other way — a real human typing
                    // past the bound would leave the streak growing.
                    // Over-resetting only ever costs a base-cadence wake.
                    if last_in > group_last_fired {
                        streak = 0;
                        streak_updates.push((a.group.clone(), 0));
                    }
                }
                // #864: a group with agents in flight is not parked, whatever
                // the host-side poll sees — and the heartbeat wake is exactly
                // how an orchestrator notices a delegate that went quiet
                // without reporting. Keep those groups on the base cadence.
                //
                // This suppresses the streak in BOTH directions — the reset
                // here, and the increment at the fire site below. Resetting
                // alone left the counter at 1 between a fire and the next
                // scan (the fire's own `streak_updates` entry lands after this
                // one and wins), so a delegate exiting in that window handed
                // the now-parked group a doubling it never earned.
                let backoff_suppressed = live_delegate_groups.contains(&a.group);
                if backoff_suppressed {
                    streak = 0;
                    streak_updates.push((a.group.clone(), 0));
                }
                // A paused group's orchestrator is deliberately quiet; never tick
                // and never burn its one-notice latch while paused.
                if paused.contains(&a.group) {
                    continue;
                }
                // #332: a SKIPPED window latches the same as a fired one (so this
                // loop doesn't busy-spin re-evaluating the gate every scan), but
                // must still come back on its own — nothing else will clear it,
                // since a skip means the orchestrator produced no output to clear
                // it the normal way. Re-arm once the intake poller could actually
                // have refreshed its findings, never sooner.
                if a.idle_tick_notified && a.idle_tick_skip_rearm_ms != 0 && now >= a.idle_tick_skip_rearm_ms {
                    a.idle_tick_notified = false;
                    a.idle_tick_skip_rearm_ms = 0;
                }
                let times = tick_times.get(&a.group).map(Vec::as_slice).unwrap_or(&[]);
                if idle_tick_should_fire(
                    a.last_progress_ms,
                    now,
                    threshold,
                    a.idle_tick_notified,
                    times,
                    MAX_IDLE_TICKS_PER_HOUR,
                ) {
                    if intake_minutes == 0 {
                        // The gate is off for this group: fire exactly as #332
                        // never happened. #864's backoff rides the gate's
                        // fallback, so it is likewise absent here — there is no
                        // fallback to widen when every tick fires anyway.
                        a.idle_tick_notified = true;
                        to_notify.push(IdleTickFire {
                            id: a.id.clone(),
                            group: a.group.clone(),
                            gate_enabled: false,
                            heartbeat: false,
                            input_defer_bound,
                            empty_streak: 0,
                            fallback_minutes: 0,
                        });
                        continue;
                    }
                    // #329 coexistence note (rev-31 finding 2): this whole `if` is scoped to
                    // ONE agent's iteration of the loop above — a skip below only records
                    // `a` into `skipped` and falls through to the next loop iteration, never
                    // an early `return`/`break` out of the scan. A future mechanism sharing
                    // this loop (#329's compact-nudge check reuses the same pure
                    // `idle_tick_should_fire` against its OWN latch field, not
                    // `idle_tick_notified`) is therefore never starved by this gate skipping
                    // for some other agent — see
                    // `intake_gate_skip_for_one_group_never_starves_another_groups_tick_in_the_same_scan`.
                    // Whoever merges #329 second: re-run both PRs' idle-tick suites together
                    // and confirm a gated skip here still lets a compact nudge fire on
                    // schedule for the SAME agent (the untested cross-feature case, since
                    // #329 isn't in this tree).
                    let has_intake_signal = intake_pending.get(&a.group).is_some_and(|p| !p.is_empty());
                    let has_pending_notification = notification_pending_groups.contains(&a.group);
                    let has_watchdog_stall = watchdog_stall_groups.contains(&a.group);
                    // #864: the fallback the gate consults is the group's
                    // EFFECTIVE one — base, widened by however long it has been
                    // delta-free. `streak` is already 0 here for any group this
                    // scan saw waking up (see the resets above), so a group only
                    // ever pays the widened interval while it is genuinely quiet.
                    let effective_fallback_minutes =
                        intake::fallback_interval_minutes(fallback_minutes, streak, fallback_max_minutes);
                    let fallback_due =
                        intake::idle_tick_fallback_due(group_last_fired, now, effective_fallback_minutes);
                    if intake::idle_tick_gate(has_intake_signal, has_pending_notification, has_watchdog_stall, fallback_due) {
                        a.idle_tick_notified = true;
                        // Heartbeat = the ONLY reason this fired is the bounded
                        // fallback — no real signal, no pending watch, no stall.
                        // `idle_tick_notice` still renders the same generic prompt
                        // (there's nothing to embed), but the audit entry below
                        // marks it distinctly so a null-summary fire is never
                        // silently mistaken for a real signal, or vice versa.
                        let heartbeat = !has_intake_signal && !has_pending_notification && !has_watchdog_stall;
                        // #864: a wake that found nothing extends the streak —
                        // this is the ONLY place it grows, so the cadence decays
                        // once per delivered empty wake, not once per gate
                        // evaluation (a skip re-checks every `intake_minutes`
                        // and must not ramp the interval by itself). A wake with
                        // a real signal behind it resets: the group is not parked.
                        // `backoff_suppressed` short-circuits the growth entirely
                        // so a suppressed group's streak is 0 at ALL times, not
                        // merely re-zeroed on the following scan.
                        streak = if heartbeat && !backoff_suppressed { streak.saturating_add(1) } else { 0 };
                        streak_updates.push((a.group.clone(), streak));
                        to_notify.push(IdleTickFire {
                            id: a.id.clone(),
                            group: a.group.clone(),
                            gate_enabled: true,
                            heartbeat,
                            input_defer_bound,
                            empty_streak: streak,
                            fallback_minutes: effective_fallback_minutes,
                        });
                    } else {
                        a.idle_tick_notified = true;
                        a.idle_tick_skip_rearm_ms = now + intake_minutes as u64 * 60_000;
                        let reason = format!(
                            "no intake signal, no pending CI watch, no watchdog stall, fallback not yet due \
                             (effective fallback ~{effective_fallback_minutes}m after {streak} delta-free wake(s); \
                             next re-check in ~{intake_minutes}m)"
                        );
                        skipped.push((a.group.clone(), reason));
                    }
                }
            }
        }

        // #864: applied here, after the `agents` borrow above has dropped —
        // insert rather than replace-the-map, so a group this scan never
        // looked at (no running orchestrator yet, paused, not autonomous)
        // keeps whatever streak it had.
        if !streak_updates.is_empty() {
            let mut streaks = self.idle_tick_empty_streak.lock_safe();
            for (group, value) in streak_updates {
                streaks.insert(group, value);
            }
        }

        for (group, reason) in skipped {
            self.audit(&group, brand::AUDIT_ACTOR, "idle-tick-skipped", json!({ "reason": reason, "suppressed": true }));
        }

        let mut notified = Vec::new();
        for IdleTickFire {
            id,
            group,
            gate_enabled,
            heartbeat,
            input_defer_bound,
            empty_streak: streak_after,
            fallback_minutes: fallback_minutes_effective,
        } in to_notify
        {
            // Record the delivery for the per-hour backstop, pruning to the window.
            // This ring is in-memory only (like `spawn_times`): a restart resets
            // the window, which is the safe direction — the cap is a runaway
            // backstop, and the quiet-window + one-notice latch already bound ticks
            // to ~one per window regardless, so a fresh window after a (rare)
            // restart can't produce a runaway, only at most a few extra ticks.
            {
                let mut tt = self.idle_tick_times.lock_safe();
                let v = tt.entry(group.clone()).or_default();
                v.push(now);
                v.retain(|&t| now.saturating_sub(t) < SPAWN_RATE_WINDOW_MS);
            }
            // #332: this IS the fire the fallback measures from, gated or not —
            // update it regardless so a group that later turns the gate on
            // doesn't inherit a stale (or zero) reference point.
            self.idle_tick_last_fired_ms.lock_safe().insert(group.clone(), now);
            // The intake summary (if any) is consumed here — cleared whether
            // THIS fire was caused by the signal or a later fallback fire swept
            // it up alongside something else; either way the orchestrator is
            // about to see it in the notice below. Rendered from the bounded
            // `PendingIntake` (rev-33 B2) to the plain string every downstream
            // consumer (the audit, `idle_tick_notice`) already expects — but
            // `dropped_any` is read BEFORE that render consumes the struct, so
            // `idle_tick_notice` (rev-33 N7) can route a saturated summary to
            // the sweep-bearing text instead of pointing the orchestrator at
            // an audit trail it has no tool to read.
            let pending = self.intake_pending.lock_safe().remove(&group);
            let summary_incomplete = pending.as_ref().is_some_and(intake::PendingIntake::dropped_any);
            let intake_summary = pending.map(|p| p.render()).filter(|s| !s.is_empty());
            self.audit(&group, brand::AUDIT_ACTOR, "idle-tick", json!({
                "orchestrator": id,
                "intake_summary": intake_summary,
                "gate_enabled": gate_enabled,
                "suppressed": false,
                "heartbeat": heartbeat,
                // #864: how many consecutive delta-free wakes this group has
                // now taken, and the effective fallback interval that produced
                // THIS one (both 0 for a gate-off bypass fire, which has no
                // fallback at all). Without these a decaying cadence is
                // invisible after the fact — the same observability argument
                // `heartbeat`/`gate_enabled` were added for in #429.
                "empty_streak": streak_after,
                "fallback_minutes": fallback_minutes_effective,
                // #496: distinct, audited reason when THIS fire happened only
                // because `idle_tick_input_defer_max_minutes` capped a perpetually-
                // refreshing input signal — an unusual reason a tick fired, and one
                // that must say so rather than reading as an ordinary quiet-window
                // fire (or, worse, going unexplained).
                "reason": if input_defer_bound { Some(IDLE_TICK_INPUT_DEFER_BOUND_REASON) } else { None },
            }));
            let text = idle_tick_notice(intake_summary.as_deref(), summary_incomplete);
            let _ = self.deliver_to_orchestrator(&group, &text, brand::AUDIT_ACTOR);
            notified.push(id);
        }
        notified
    }

    /// Enforce every autonomous group's token budget (#83). For each autonomous,
    /// non-paused group with a budget set, meter spend as the delta from the
    /// enable-time anchor; once it crosses the budget, **suspend** autonomous mode
    /// (flip the marker off — explicit consent required to resume), audit it, and
    /// deliver ONE `[orrerix]` notice. Because suspension removes the group from
    /// the autonomous set, a later pass skips it, so the notice can't repeat.
    /// Returns the suspended group ids. Runs before the idle tick each cycle.
    pub fn enforce_autonomy_budgets(&self, _now: u64) -> Vec<GroupId> {
        let autonomous = self.autonomous_groups.lock_safe().clone();
        if autonomous.is_empty() {
            return Vec::new();
        }
        let paused = self.paused.lock_safe().clone();
        let mut suspended = Vec::new();
        for group in autonomous {
            // Paused groups already don't tick; leave their meter frozen.
            if paused.contains(&group) {
                continue;
            }
            let budget = self
                .group(&group)
                .map(|g| g.guardrails.autonomy_budget_tokens)
                .unwrap_or(0);
            if budget == 0 {
                continue; // no cap
            }
            let anchor = self.autonomy_anchor(&group);
            let spent = self.group_token_total(&group).saturating_sub(anchor);
            if autonomy_budget_exhausted(spent, budget) {
                self.audit(&group, brand::AUDIT_ACTOR, "autonomy-budget-exhausted",
                    json!({ "spent_tokens": spent, "budget_tokens": budget }));
                // Money-stop: drop the group from the autonomous set unconditionally
                // so ticking halts even if the marker can't be removed (rev-49).
                self.suspend_autonomous(&group);
                // Distinguish a budget suspension from a plain user-off with a
                // durable `autonomy_suspended` marker, so the UI can tell the human
                // "suspended — raise the budget or re-enable" instead of
                // reconstructing it from the audit log. Written *after* the disable
                // (which turns autonomous off); cleared on a genuine re-enable. A
                // hint, not a consent gate, so a failed write fails soft.
                let _ = fs::write(
                    self.group_dir(&group).join("autonomy_suspended"),
                    json!({ "spent_tokens": spent, "budget_tokens": budget }).to_string(),
                );
                let _ = self.deliver_to_orchestrator(
                    &group,
                    &autonomy_budget_notice(spent, budget),
                    brand::AUDIT_ACTOR,
                );
                suspended.push(group);
            }
        }
        suspended
    }

    /// One full idle-tick cycle: enforce budgets (which may suspend groups), then
    /// read pty counters and tick the still-autonomous orchestrators. Called on a
    /// timer by `start_idle_tick`; `now` injected so tests drive it deterministically.
    pub fn run_idle_tick(&self, now: u64) -> Vec<String> {
        let Some(_tick) = self.tick_gate("run_idle_tick") else { return Vec::new() };
        self.enforce_autonomy_budgets(now);
        let (outputs, inputs) = self.orchestrator_activity();
        self.idle_tick_tick(now, &outputs, &inputs)
    }

    // ---------- attention routing: surface which pane needs the human ----------

    /// Latch (or clear) a worker's report as an attention signal. `done` and
    /// `blocked` badge the pane and can fire a toast until the human acks or the
    /// agent is reassigned; `progress` (the agent is working again) clears it.
    /// No-op for the orchestrator, which never reports.
    pub fn note_report_attention(&self, agent_id: &str, status: &str) {
        let mut m = self.attn_reports.lock_safe();
        match status {
            "done" => {
                m.insert(agent_id.to_string(), "done");
            }
            "blocked" => {
                m.insert(agent_id.to_string(), "blocked");
            }
            _ => {
                m.remove(agent_id);
            }
        }
    }

    /// The human focused/handled a pane: drop any latched report so its badge
    /// clears, and suppress the live `waiting` badge so focusing a pane whose
    /// menu is still on screen makes the ack *stick* — otherwise the next 3s scan
    /// re-emits `waiting` and re-lights the pane the human is already on (#40
    /// review). The suppression self-clears once the pane's output changes (the
    /// menu was answered / the CLI repainted), so a genuinely new prompt on the
    /// same pane flags again. The `gate` reason is board state, cleared by moving
    /// the task, so it needs no ack.
    pub fn ack_attention(&self, agent_id: &str) {
        self.attn_reports.lock_safe().remove(agent_id);
        self.attn_waiting_ack.lock_safe().insert(agent_id.to_string());
    }

    /// The human turned to a *plain* pane (#40): make its `waiting` ack stick the
    /// same way `ack_attention` does for agents, keyed by the pane's pty id. The
    /// suppression lifts when the pane's output next changes (see
    /// `plain_pane_attention`).
    pub fn ack_attention_pty(&self, pty_id: u32) {
        self.attn_waiting_ack.lock_safe().insert(format!("pty:{pty_id}"));
    }

    // ---------- low-disk backstop (#134) ----------

    /// One low-disk backstop pass given the current free bytes on the workspace
    /// drive. On the tick that first crosses below `LOW_DISK_BYTES`, deliver ONE
    /// audited notice to each live, non-paused group's orchestrator and latch;
    /// the latch clears once free space recovers past `LOW_DISK_CLEAR_BYTES`.
    /// Paused groups are skipped (like the watchdog) — their agents idle out on
    /// purpose and prompt delivery is suppressed there anyway. Returns the
    /// notified group ids. Free-bytes is injected so the latch/hysteresis logic
    /// is testable without a real disk.
    pub fn disk_tick(&self, free: u64) -> Vec<GroupId> {
        let fire = {
            let mut latched = self.low_disk_notified.lock_safe();
            let (new_latched, fire) =
                low_disk_transition(free, LOW_DISK_BYTES, LOW_DISK_CLEAR_BYTES, *latched);
            *latched = new_latched;
            fire
        };
        if !fire {
            return Vec::new();
        }
        // Snapshot groups/paused, then deliver outside any lock (delivery types
        // into a pane and can block).
        let paused = self.paused.lock_safe().clone();
        let groups: Vec<GroupId> = self.groups.lock_safe().keys().cloned().collect();
        let notice = low_disk_notice(free);
        let mut notified = Vec::new();
        for group in groups {
            if paused.contains(&group) {
                continue;
            }
            self.audit(&group, brand::AUDIT_ACTOR, "low-disk", json!({ "free_bytes": free }));
            if self.deliver_to_orchestrator(&group, &notice, brand::AUDIT_ACTOR).is_ok() {
                notified.push(group);
            }
        }
        notified
    }

    /// Sample free space on the workspace drive (the app-data root, where the
    /// board/state live — the surface a disk-full write corrupts) and run one
    /// `disk_tick`. Best-effort: if the disk can't be read, do nothing.
    pub fn run_disk_monitor(&self) {
        let Some(_tick) = self.tick_gate("run_disk_monitor") else { return () };
        if let Some(free) = free_disk_bytes(&self.root) {
            self.disk_tick(free);
        }
    }


    /// Read every agent pane's output counter, last-lines tail, and last human
    /// keystroke time — the raw inputs `attention_tick` needs. Empty without an
    /// app handle (unit tests drive `attention_tick` with synthetic maps).
    fn attention_inputs(&self) -> (HashMap<String, u64>, HashMap<String, String>, HashMap<String, u64>) {
        let Some(app) = self.app.lock_safe().clone() else {
            return (HashMap::new(), HashMap::new(), HashMap::new());
        };
        let ptys = app.state::<crate::pty::PtyManager>();
        self.attention_inputs_from(&ptys)
    }

    /// Gather core of [`OrchRegistry::attention_inputs`]: the three maps
    /// `attention_tick` consumes, read off a `PtyManager` handed in rather than
    /// resolved from the app handle.
    ///
    /// Its three `_from` siblings ([`OrchRegistry::output_totals_from`],
    /// [`OrchRegistry::compact_signals_from`],
    /// [`OrchRegistry::pane_attention_inputs_from`]) exist for this reason and
    /// this one is #1702 P4's: without it a headless test can only drive
    /// `attention_tick` with SYNTHETIC maps, so the gather — the half that
    /// decides which panes are in the population and how much of each ring is
    /// read — is not part of what any liveness row measures. With it, L7a runs
    /// the tick on the maps production would have built, over
    /// `register_fake_for_test` panes.
    ///
    /// The extraction is a pure move: everything below was `attention_inputs`'s
    /// body from the snapshot onward, unreordered, and `attention_inputs` is
    /// now the app-handle resolution alone.
    #[doc(hidden)] // pub for integration tests
    pub fn attention_inputs_from(
        &self,
        ptys: &crate::pty::PtyManager,
    ) -> (HashMap<String, u64>, HashMap<String, String>, HashMap<String, u64>) {
        let mut outs = HashMap::new();
        let mut tails = HashMap::new();
        let mut ins = HashMap::new();
        // Snapshot (agent id, pty id) and DROP the agents lock before touching
        // any pty: the reads below each take the global `ptys` mutex, and
        // holding `agents` across all of them pins two locks for the length of
        // the whole gather instead of one lock per read (#717). An agent that
        // disappears in the gap simply has no pty to read, which is the same
        // outcome the `pty_id`/`output_total` guards already produce and which
        // the scan already treats as "no item this tick".
        let panes: Vec<(String, u32)> = self
            .agents
            .lock_safe()
            .values()
            .filter_map(|a| a.pty_id.map(|pid| (a.id.clone(), pid)))
            .collect();
        for (id, pid) in panes {
            if let Some(t) = ptys.output_total(pid) {
                outs.insert(id.clone(), t);
            }
            // A prompt is at the very end, so only the last few KB are read —
            // never the whole (up to 256 KB) ring, which is a copy made under
            // the same mutex every keystroke takes (#717).
            if let Some(t) = attention_tail(|n| ptys.output_tail_bounded(pid, n)) {
                tails.insert(id.clone(), t);
            }
            if let Some(u) = ptys.last_user_input_ms(pid) {
                ins.insert(id, u);
            }
        }
        (outs, tails, ins)
    }

    /// Pty snapshots for every live pane that is NOT a registered agent, keyed
    /// by pty id, plus the agent-pty set. Feeds `plain_pane_attention` so the
    /// scan reaches plain shells the human opened by hand (#40). Empty without an
    /// app handle (tests drive the gather core `pane_attention_inputs_from`, which
    /// owns the bounded ring read and takes its reader injected).
    #[allow(clippy::type_complexity)]
    fn pane_attention_inputs(
        &self,
    ) -> (HashMap<u32, u64>, HashMap<u32, String>, HashMap<u32, u64>, HashSet<u32>) {
        let mut agent_ptys = HashSet::new();
        let Some(app) = self.app.lock_safe().clone() else {
            return (HashMap::new(), HashMap::new(), HashMap::new(), agent_ptys);
        };
        for a in self.agents.lock_safe().values() {
            if let Some(pid) = a.pty_id {
                agent_ptys.insert(pid);
            }
        }
        let ptys = app.state::<crate::pty::PtyManager>();
        let mut live = Vec::new();
        for pid in ptys.live_ids() {
            // Skip agent ptys *before* touching the pty at all: `attention_tick`
            // already covers them, and `attention_inputs` already read their
            // tail this tick — reading it a second time here would be pure
            // waste (#40 review). The gather core below repeats the skip, which
            // is where a test can observe that the ring is never READ for an
            // agent pty rather than merely that no entry came out (#717).
            if agent_ptys.contains(&pid) {
                continue;
            }
            let Some(total) = ptys.output_total(pid) else { continue };
            let input = ptys.last_user_input_ms(pid).unwrap_or(0);
            live.push((pid, total, input));
        }
        let (outs, tails, ins) = self.pane_attention_inputs_from(
            &live,
            |pid, n| ptys.output_tail_bounded(pid, n),
            &agent_ptys,
        );
        (outs, tails, ins, agent_ptys)
    }

    /// Gather core of `pane_attention_inputs`: build the pty-keyed snapshot maps
    /// `plain_pane_attention` consumes from a list of live pane snapshots
    /// `(pty_id, output_total, last_input_ms)`, reading and ANSI-stripping only
    /// the trailing `ATTENTION_SCAN_BYTES` of each tail (a prompt is at the
    /// end). Agent ptys are skipped without being read at all.
    ///
    /// `read` is the raw-byte reader — `PtyManager::output_tail_bounded` in
    /// production — rather than a pre-read `Vec<u8>` per pane, so the size of
    /// the request the scan makes under the `ptys` mutex is part of what this
    /// function decides, and therefore part of what a test can pin (#717). Pure
    /// w.r.t. the OS otherwise, so run_attention's gather wiring stays testable
    /// with a fake live-ids source (#40 review).
    #[allow(clippy::type_complexity)]
    pub fn pane_attention_inputs_from(
        &self,
        live: &[(u32, u64, u64)],
        mut read: impl FnMut(u32, usize) -> Option<Vec<u8>>,
        agent_ptys: &HashSet<u32>,
    ) -> (HashMap<u32, u64>, HashMap<u32, String>, HashMap<u32, u64>) {
        let mut outs = HashMap::new();
        let mut tails = HashMap::new();
        let mut ins = HashMap::new();
        for (pid, total, input) in live {
            if agent_ptys.contains(pid) {
                continue;
            }
            outs.insert(*pid, *total);
            tails.insert(*pid, attention_tail(|n| read(*pid, n)).unwrap_or_default());
            ins.insert(*pid, *input);
        }
        (outs, tails, ins)
    }

    /// One attention pass: compute the current set of panes that need the human
    /// from live agent state plus the supplied pty snapshots. Reasons, in
    /// priority order, are `held-dialog` (#946 Q4 / #1091 slice H — a live
    /// interactive dialog is holding the ORCHESTRATOR's own delivery pipe;
    /// see `attn_question_held`'s doc for why this outranks even `blocked`),
    /// `blocked` (reported), `provider-limit` (#2811 S5a — the account behind
    /// this pane's model is out of budget and its CLI is parked on the
    /// provider's refusal; raised once per group per provider), `stranded` (a
    /// delivered prompt never submitted),
    /// `waiting` (parked on a prompt: output quiet past `ATTENTION_QUIET_MS`,
    /// a prompt-shaped tail, and no recent human keystroke), `report`
    /// (reported done), `question` (#1091 slice D — this agent has a pending
    /// `ask_human` row nobody has answered yet), and `gate` (this agent's
    /// board task sits at a `pr`/`human-testing`/`blocked`/`prototype` merge
    /// gate). Pure w.r.t. the OS/pty — the pty reads live in
    /// `attention_inputs` — so the whole policy is testable with synthetic
    /// maps and no real CLI.
    pub fn attention_tick(
        &self,
        now: u64,
        outputs: &HashMap<String, u64>,
        tails: &HashMap<String, String>,
        last_inputs: &HashMap<String, u64>,
    ) -> Vec<AttentionItem> {
        // #1702, phase 1 of 3 — the ROSTER SNAPSHOT. `agents` is taken here,
        // cloned from, and RELEASED before anything else in this function runs:
        // `compute_group_usage`'s shape, and for a sharper reason than cost.
        //
        // This lock used to be held across the whole per-agent loop below, which
        // put `delivered_mask_lines` under it — and that call reaches
        // `session_for_pty`, whose second line is `self.agents.lock_safe()`.
        // `TrackedMutex`'s inner primitive is `parking_lot::Mutex`, which is not
        // re-entrant, and this tick runs inside `tick_gate`'s `MutationScope`,
        // where an expired budget does NOT unwind — `lock_safe` falls through to
        // `acquire_blocking` and waits, unbounded. So the tick did not merely
        // hold the registry's busiest lock for a long time: it DEADLOCKED on it,
        // the first time any pty-bound, `by_pty`-mapped agent went quiet past
        // `ATTENTION_QUIET_MS`. Both halves of that nesting entered in one
        // commit (#903), which is why #1702's field trace is a single hold
        // growing 38 s → 336 s across separate retries, never released, and
        // re-forming within seconds of every restart.
        //
        // The rule this function now keeps — and the one a later edit must not
        // break — is that NO phase holds a registry lock across a call that can
        // take another one:
        //
        //   phase 1  snapshot the roster             short `agents` hold
        //   phase 2  masks and prompt-wait per agent NOTHING held
        //   phase 3  decide and apply                one short hold of the
        //                                            attention maps
        //
        // Phase 2 is where every nested acquisition now happens, and it happens
        // with the registry free. `docs/design/lock-liveness.md` §6 is the
        // contract; `liveness.rs`'s `l6a_`/`l6b_` rows are the guard.
        let roster: Vec<AgentEntry> = self.agents.lock_safe().values().cloned().collect();

        // Board-derived gate map: agent id → gate status, across every live
        // group. Read once per group (a small fs read) rather than per agent.
        let groups: HashSet<GroupId> = roster.iter().map(|a| a.group.clone()).collect();
        let mut gate_of: HashMap<String, String> = HashMap::new();
        // #1091 slice D: pending-question count per asker, across every live
        // group — DERIVED from the #946 Q1 `questions.json` registry, exactly
        // like `gate_of` is derived from the board. This map's LIFETIME is the
        // registry's — a settled/withdrawn question just stops showing up here,
        // so no dismiss machinery of its own is needed — but the ITEM this
        // produces below is additionally gated on the asker's pane still being
        // `AgentStatus::Running` (the per-agent loop below skips non-running
        // agents before it ever consults this map), same as every other
        // reason in this scan. So the badge is the live-pane
        // PROJECTION of the registry, not the registry itself: a question
        // pending against a stopped pane raises nothing here (no chip, no
        // toast) even though the registry still durably holds it and slice
        // C's panel will still show it. `asker` is orchestrator-only today
        // (`humanq::Question` doc), so in practice this keys the
        // orchestrator's own pane. A malformed `questions.json` collapses to
        // "no pending questions" here — the same posture `self.tasks` already
        // takes for `gate_of` above — rather than failing the whole scan;
        // `questions()` stays LOUD for its own read-modify-write callers
        // (`ask_human`, `list_questions`), which is where a human actually
        // needs to hear about corruption.
        let mut question_of: HashMap<String, usize> = HashMap::new();
        for g in &groups {
            for q in self.questions(g).unwrap_or_default() {
                // `is_settled()` — not `== Status::Pending` — so a future
                // non-terminal status (something other than the current
                // pending/answered/withdrawn three) is still counted here by
                // the SAME predicate `ask_human`'s own `PENDING_MAX` check
                // uses (`!q.status.is_settled()`, registry/questions.rs `ask_human`), rather
                // than by a second, independently-drifting spelling of "not
                // done yet".
                if !q.status.is_settled() {
                    *question_of.entry(q.asker).or_insert(0) += 1;
                }
            }
        }
        for g in &groups {
            for t in self.tasks(g) {
                // `prototype` is a human gate too (#147): the assigned pane is
                // where the pending demo-verdict work lives, so flag it like the
                // merge gates and `blocked`.
                let is_gate = MERGE_GATE_STATUSES.contains(&t.status.as_str())
                    || t.status == "blocked"
                    || t.status == PROTOTYPE_STATUS;
                if is_gate {
                    if let Some(assignee) = t.assignee.filter(|s| !s.trim().is_empty()) {
                        gate_of.insert(assignee, t.status);
                    }
                }
            }
        }

        // #1702, phase 2 — the per-agent mask, computed with NO lock held. This
        // is the work that used to run under `agents`; it reaches four other
        // registry locks (`delivered_notices`, `by_pty`, `agents`,
        // `delivered_prompts`) and so may only run from a phase holding none.
        //
        // What it costs is BOUNDED, and saying so is the other half of #1702:
        // the issue's premise was that this scales with a session's age, and it
        // does not. `delivered_mask_lines` unions two drop-oldest records that
        // are capped where they are WRITTEN — `DELIVERED_NOTICES_PER_PANE` (24)
        // lines of the pane's notice record and
        // `DELIVERED_PROMPT_LINES_PER_SESSION` (16) of the session's prompt
        // record — so `delivered` is at most 40 lines however many thousands of
        // deliveries a session has taken, and the maps holding them are capped
        // by pane and by session as well (`DELIVERED_NOTICE_PANES`,
        // `DELIVERED_PROMPT_SESSIONS`), which is what bounds the memory. The
        // other operand is capped too: `attention_tail` reads only the trailing
        // `ATTENTION_SCAN_BYTES` (4096) of the ring, so a tail is ~100 rows.
        // One agent's mask is therefore ~100 × 40 short string comparisons —
        // tens of microseconds, with nothing in it proportional to session age.
        // That is why no SECOND cap is added here: capping an already-capped
        // record could only narrow what the mask may claim, which is a change to
        // #903 B2's guarantees, and it would buy no bound that is not already
        // held.
        //
        // Computed for every running agent that has a tail, rather than only for
        // the ones the cheap terms below would admit. The old code let `&&`
        // short-circuit this away, but those terms read the quiet clock, which
        // lives in a map phase 3 owns — keeping the short-circuit would mean
        // reading one of this predicate's inputs under a lock and the rest
        // outside it. A pure function of (tail, record) is the same value
        // whenever it is evaluated, so the tick's outputs are unchanged; only
        // the cost of an agent the terms would have rejected moves, and it moves
        // off the lock.
        //
        // A pane with no pty bound has taken no delivery, so an empty record is
        // the truth rather than a gap — the same `unwrap_or_default` the old
        // in-loop expression had, kept here so an agent with a tail and no pty
        // is still masked against an empty record rather than skipped.
        //
        // #2811 S5a: the provider-limit read shares this phase and this masked
        // text. It is the same subject (the pane's own tail) asked a second
        // question, so computing it here costs one extra pass over ~100 rows
        // and — the part that matters — inherits the MASK for free. Without it
        // the orchestrator's own pane, which types a provider's refusal into
        // `ask_human` while asking the human to top the account up, and any
        // delegate whose kickoff quotes one, would raise the chip for a limit
        // they are merely talking about (#576's self-latch, arriving at a
        // third consumer).
        let signals: HashMap<String, PaneTailSignals> = roster
            .iter()
            .filter(|a| a.status == AgentStatus::Running)
            // #2850 S3b: never scrape a structured pane. Section 5.4 — orrerix
            // must not read back what orrerix itself rendered, which is not
            // merely pointless but FORGEABLE: the ring holds the pane own
            // transcript, so model prose shaped like a question grid would be
            // detected as one. Its attention comes from the event stream
            // instead (a pending `UiRequest`), decided below.
            //
            // Written as an explicit filter rather than left to the `tails`
            // lookup missing: that would be true today and silently wrong the
            // moment a structured pane grows a tail entry.
            .filter(|a| a.pane_kind.is_none())
            .filter_map(|a| tails.get(&a.id).map(|t| (a, t)))
            .map(|(a, t)| {
                // #1702: the session comes off the ROSTER SNAPSHOT, so this
                // phase resolves nothing and takes neither `by_pty` nor
                // `agents`. That is the difference between "the deadlock is
                // unreachable because no lock is held here" and "the tick never
                // asks the question that deadlocked it".
                let delivered = a
                    .pty_id
                    .map(|p| self.delivered_mask_lines(p, a.session_id.as_deref()))
                    .unwrap_or_default();
                let masked = mask_loomux_notices_with_record(t, &delivered);
                let shaped = prompt_wait_detected(&masked);
                let limit = providerlimit::limit_in_tail(&masked);
                (a.id.clone(), PaneTailSignals { shaped, limit })
            })
            .collect();

        // #2811 S5a, still lock-free — WHICH pane wears the chip.
        //
        // A provider limit stops every pane on that provider at once (four in
        // this repo's own Sep-5 incident), and four identical red chips plus
        // four toasts is worse than one: they say the same thing, they need
        // the same single remedy, and dismissing three of them is busywork the
        // human did not earn. So the item is raised ONCE per (group, provider)
        // — the plan's rule — on the lowest-sorting affected agent id, which is
        // deterministic where the roster's own iteration order is not
        // (`agents` is a `HashMap`), and its `detail` carries how many panes
        // are affected so the one chip still tells the truth about the blast
        // radius.
        //
        // Keyed on the provider DETECTED IN THE TEXT rather than on the
        // block's model prefix, which is the one place this deviates from
        // plan-2504 §3 S5a — see `docs/design/attention-provider-limit.md`. pi
        // and opencode surface OpenRouter's refusal verbatim, so a pane whose
        // model reads `opencode/...` is stopped by OpenRouter's limit; keying
        // on the prefix would file it under a third "provider" that has no
        // remedy and would not merge with the OpenRouter panes it must be
        // counted with.
        // The selection itself is in PHASE 3, below, and that placement is the
        // whole of #3178 review B1: choosing a carrier needs to know what would
        // OUTRANK the chip on each candidate, and those are the latched maps
        // phase 3 owns. Picking here — before `attn_reports` and
        // `attn_question_held` are read — meant the group's only chip could
        // land on a pane whose `blocked` latch then swallowed it, leaving ZERO
        // provider-limit chips for a group where three panes were stopped.

        // #1702, phase 3 — decide and apply, under one short hold of the
        // attention maps. Nothing below reads a pty, a board file or a delivery
        // record; every input it needs was gathered above.
        //
        // `roster` is phase 1's SNAPSHOT, and by here it is up to a phase-2
        // duration old (~12 ms on a six-agent fixture). That is deliberate and
        // it has one consequence worth knowing before editing this loop:
        // `mark_dead` sets an agent `Dead`, DROPS `agents`, and only then
        // prunes `attn_quiet`/`attn_waiting_ack`/`attn_reports`/`attn_emitted`
        // — so a kill landing in that window has its prune undone by the
        // `quiet.entry(...).or_insert(...)` below, and one stale chip can be
        // emitted for a pane that is already dead. It reconciles on the next
        // tick, whose first branch removes all four entries for any agent no
        // longer `Running`, and the cadence is 3 s. The alternative — re-taking
        // `agents` here to re-check liveness — is exactly the coupling #1702
        // exists to remove, so this is a chosen trade rather than an oversight.
        // Under the pre-#1702 shape it could not happen: the tick held `agents`
        // across the whole loop, which serialised `mark_dead` against it.
        let reports = self.attn_reports.lock_safe().clone();
        let mut quiet = self.attn_quiet.lock_safe();
        let mut waiting_ack = self.attn_waiting_ack.lock_safe();
        // #496 PR-C: latched stranded-delivery state, raised by the late
        // monitor's actuation (`actuate_stranded`). Taken here, once, in the
        // same lock order as the other attention maps.
        let mut stranded = self.attn_stranded.lock_safe();
        // #946 Q4 / #1091 slice H: the latched-attention belt's own map,
        // taken in the same lock order (after `attn_stranded`) for the same
        // reason every other attention map is taken here rather than re-locked
        // per agent. It used to read "before `agents`"; `agents` is not taken
        // in this phase at all any more (#1702), and describing an order that
        // no longer exists is how the next reader re-creates it.
        let mut question_held = self.attn_question_held.lock_safe();

        // #2811 S5a — WHICH pane wears the single provider-limit chip, decided
        // here because this is the first point that can see what would outrank
        // it (#3178 review B1).
        //
        // A provider limit stops every pane on that provider at once, and four
        // identical red chips need one remedy once — so the item is raised ONCE
        // per (group, provider), with `detail` carrying how many panes were
        // stopped so the one chip still tells the truth about the blast radius.
        //
        // But the reason chain below runs PER PANE, and `held-dialog` and
        // `blocked` both outrank `provider-limit`. Only the carrier holds a
        // `limit_chip` entry; every other affected pane falls through. So a
        // carrier chosen without consulting those latches could be a pane that
        // then renders `blocked`, and the group would show NO provider-limit
        // chip at all while three of its panes sat stopped — the change's own
        // rationale, inverted, and #576's self-latch one layer up.
        //
        // Hence: prefer a candidate that nothing outranks, and fall back to the
        // lowest id only when EVERY affected pane is outranked. Within each
        // class the pick is the lowest-sorting agent id, which is deterministic
        // where the roster's own iteration order (a `HashMap`'s) is not.
        //
        // **The fallback still loses the chip, and that is disclosed rather
        // than hidden**: when every affected pane is already showing `blocked`
        // or `held-dialog`, those are urgent reasons that summon the human to
        // this same group anyway, so what is lost is the provider attribution,
        // not the alarm. Raising a second chip instead would reintroduce
        // exactly the per-pane spam the one-chip rule exists to prevent.
        // `a_provider_limit_is_subsumed_when_every_affected_pane_is_outranked`
        // pins that arm so the disclosure cannot go quietly false.
        //
        // Keyed on the provider DETECTED IN THE TEXT rather than on the block's
        // model prefix, which is the one place this deviates from plan-2504 §3
        // S5a — see `docs/design/attention-provider-limit.md`. pi and opencode
        // surface OpenRouter's refusal verbatim, so a pane whose model reads
        // `opencode/...` is stopped by OpenRouter's limit; keying on the prefix
        // would file it under a third "provider" that has no remedy and would
        // not merge with the OpenRouter panes it must be counted with.
        // Computed as an owned set rather than read through a closure over the
        // `question_held` guard: the per-agent loop below MUTATES that guard,
        // and a closure still holding a shared borrow of it is a borrowck
        // question this does not need to have.
        //
        // The membership rule is exactly the arms ABOVE `provider-limit` in the
        // chain below. A reason added above it must be added here too, or the
        // carrier can be swallowed again — `provider_limit_sits_under_blocked_
        // and_over_stranded_and_waiting` and
        // `the_single_chip_avoids_a_pane_whose_blocked_latch_would_swallow_it`
        // are what fail if the two drift.
        let outranked: HashSet<&str> = roster
            .iter()
            .map(|a| a.id.as_str())
            .filter(|id| question_held.contains(*id) || reports.get(*id).copied() == Some("blocked"))
            .collect();
        let mut limited_by_key: HashMap<(String, &'static str), (Option<String>, String, usize)> =
            HashMap::new();
        for a in &roster {
            if a.status != AgentStatus::Running {
                continue;
            }
            let Some(limit) = signals.get(&a.id).and_then(|s| s.limit) else { continue };
            let entry = limited_by_key
                .entry((a.group.to_string(), limit.provider))
                .or_insert_with(|| (None, a.id.clone(), 0));
            entry.2 += 1;
            // `.0` is the best UNOUTRANKED candidate, `.1` the best of any.
            if !outranked.contains(a.id.as_str()) && entry.0.as_ref().is_none_or(|best| a.id < *best) {
                entry.0 = Some(a.id.clone());
            }
            if a.id < entry.1 {
                entry.1 = a.id.clone();
            }
        }
        // agent id -> (the provider row, how many panes in its group it stopped).
        let limit_chip: HashMap<String, (&'static providerlimit::Provider, usize)> = limited_by_key
            .into_iter()
            .filter_map(|((_, provider_id), (unblocked, any, panes))| {
                providerlimit::provider(provider_id)
                    .map(|p| (unblocked.unwrap_or(any), (p, panes)))
            })
            .collect();

        // #2811 S5b — publish what this scan found, for the review driver.
        //
        // EVERY limited pane, not just the carrier: the chip is deduped to one
        // per (group, provider) because a human needs one alarm, and a DRIVE
        // needs to know whether ITS OWN panes are stopped. Handing the driver
        // the carrier alone would hold whichever drive happened to own the
        // lowest-sorting id and leave every other affected drive to time out —
        // the sixty-minute stall this slice exists to remove, reintroduced by a
        // presentation decision.
        //
        // Written whole, so a pane that recovered stops appearing and there is
        // no clearing path to remember. Taken after the `limit_chip` build and
        // before the per-agent loop: this is a map of its own, so it adds no
        // ordering against the attention maps phase 3 already holds.
        *self.attn_provider_limit.lock_safe() = roster
            .iter()
            .filter(|a| a.status == AgentStatus::Running)
            .filter_map(|a| {
                signals
                    .get(&a.id)
                    .and_then(|s| s.limit)
                    .map(|l| (a.id.clone(), l.provider.to_string()))
            })
            .collect();

        let mut out = Vec::new();
        for a in &roster {
            if a.status != AgentStatus::Running {
                quiet.remove(&a.id);
                waiting_ack.remove(&a.id);
                // A pane with no running agent cannot be wedged on a prompt
                // any more, and its note would otherwise outlive it (the map
                // is latched — nothing else prunes it).
                stranded.remove(&a.id);
                question_held.remove(&a.id);
                continue;
            }
            // Track how long the pane's output has been stable.
            let cur = outputs.get(&a.id).copied().unwrap_or(0);
            let entry = quiet.entry(a.id.clone()).or_insert((cur, now));
            let output_changed = cur != entry.0;
            if output_changed {
                *entry = (cur, now);
                // The pane repainted — the acked menu was answered or replaced,
                // so re-arm: a fresh prompt on this pane flags again.
                waiting_ack.remove(&a.id);
            }
            let quiet_for = now.saturating_sub(entry.1);
            let recently_typed = last_inputs
                .get(&a.id)
                .map(|&t| t != 0 && now.saturating_sub(t) < ATTENTION_RECENT_INPUT_MS)
                .unwrap_or(false);
            let waiting = !recently_typed
                && !waiting_ack.contains(&a.id)
                && quiet_for >= ATTENTION_QUIET_MS
                // #576, rev-126: masked here too. This is the same self-latch
                // the delivery gate has, arriving at a different consumer: a
                // relayed `[orrerix] w-7 reports blocked: … (y/n)` sits in the
                // tail, the pane is quiet BECAUSE it is idle, and the chip
                // says "waiting on a prompt" about a question nobody asked.
                // Worse here than at the gate, in one respect — the gate's
                // latch is at least reported at ten minutes by
                // `QuestionStale`, whereas a wrong chip just trains the human
                // to ignore chips.
                // #576 residual: with the pane's delivery record, so a notice
                // that WRAPPED does not raise a chip either. A pane with no
                // pty bound has taken no delivery, so an empty record is the
                // truth rather than a gap.
                // #1702: computed in phase 2, off every lock. `unwrap_or(false)`
                // is the same default the in-loop expression had — no entry
                // means this agent had no tail this tick.
                && signals.get(&a.id).map(|s| s.shaped).unwrap_or(false);

            let report = reports.get(a.id.as_str()).copied();
            let (reason, detail): (&'static str, String) = if question_held.contains(a.id.as_str()) {
                // #946 Q4 / #1091 slice H: outranks even `blocked`, because
                // a live dialog holding this pane's delivery pipe stalls
                // every OTHER agent's report behind it, not just this one's
                // own status. Disjoint from
                // #1091 slice D's `question` reason (a pending `ask_human`
                // question, engine-registry state, no pane involved) — see
                // `attn_question_held`'s doc.
                ("held-dialog", format!(
                    "{} is holding on a blocking dialog — every report queued behind it is stuck too",
                    a.name
                ))
            } else if report == Some("blocked") {
                ("blocked", format!("{} reported blocked — it needs you", a.name))
            } else if let Some((p, panes)) = limit_chip.get(a.id.as_str()) {
                // #2811 S5a. Ranked directly under `blocked` and above every
                // other wedge, on the same argument `stranded` makes against
                // `waiting` and one step further: a provider-limited pane will
                // not un-wedge itself, and — unlike `stranded` or `dialog`,
                // which one Enter in that pane clears — nothing done IN the
                // terminal clears this one at all. It is also the only reason
                // here whose blast radius is the whole group. Only `blocked`
                // (an agent that explicitly said so) and `held-dialog` (which
                // strands every other pane's reports too) outrank it.
                let noun = if *panes == 1 { "pane" } else { "panes" };
                (
                    "provider-limit",
                    format!(
                        "{} limit reached — {panes} {noun} stopped; {}",
                        p.display, p.remedy
                    ),
                )
            } else if self
                .structured_pane(&a.id)
                .is_some_and(|p| p.awaiting_human())
            {
                // #2850 S3b. A parked dialog BLOCKS the agent — pi waits on
                // stdin indefinitely — so this ranks with `stranded` rather
                // than with the amber `question`: the pane will not un-wedge
                // itself, and every delivery behind it waits too.
                ("dialog", format!("{} is waiting on a dialog — answer it to unblock the pane", a.name))
            } else if let Some(note) = stranded.get(a.id.as_str()) {
                // #496 PR-C: ranked directly under `blocked` and above
                // `waiting` — a stranded prompt is a wedged pane that will
                // not un-wedge itself, whereas `waiting` is the pane asking
                // a question it is happy to keep asking. Only `blocked` (an
                // agent that explicitly said it cannot proceed) outranks it.
                ("stranded", stranded_detail(&a.name, note.blocker))
            } else if waiting {
                ("waiting", format!("{} is waiting on a prompt", a.name))
            } else if report == Some("done") {
                ("report", format!("{} reported done — review & merge", a.name))
            } else if let Some(&count) = question_of.get(a.id.as_str()) {
                // Non-urgent amber, like `gate` — a question is a decision
                // waiting on the human's own pace, not a wedged pane.
                let noun = if count == 1 { "question" } else { "questions" };
                ("question", format!("{count} pending {noun} — needs your answer"))
            } else if let Some(st) = gate_of.get(a.id.as_str()) {
                ("gate", format!("task is {st} — awaiting your call"))
            } else {
                continue;
            };
            out.push(AttentionItem {
                agent_id: a.id.clone(),
                group: a.group.to_string(),
                name: a.name.clone(),
                role: Some(a.role),
                pty_id: a.pty_id,
                reason,
                detail,
            });
        }
        out.sort_by(|x, y| x.agent_id.cmp(&y.agent_id));
        out
    }

    /// Attention scan for *plain* panes (#40): any pane with a live pty that is
    /// **not** a registered agent — the shells the human opens by hand to run a
    /// CLI. It only ever raises `waiting` (parked on an interactive prompt): the
    /// agent-only reasons (`blocked`/`report`/`gate`) require a roster identity a
    /// plain pane doesn't have. Same quiet + no-keystroke + prompt-tail gate and
    /// the same sticky-ack semantics as the agent path, keyed by a synthetic
    /// `pty:<id>` id in the shared `attn_quiet`/`attn_waiting_ack` maps (agent
    /// ids never collide — they're group-scoped uuids). Pure w.r.t. the pty (the
    /// pty reads live in `pane_attention_inputs`), so it's fixture-testable.
    /// `agent_ptys` are the ptys already handled by `attention_tick`, skipped here.
    pub fn plain_pane_attention(
        &self,
        now: u64,
        outputs: &HashMap<u32, u64>,
        tails: &HashMap<u32, String>,
        last_inputs: &HashMap<u32, u64>,
        agent_ptys: &HashSet<u32>,
    ) -> Vec<AttentionItem> {
        // #1702: the same per-pane mask work `attention_tick` moved out of its
        // locks, hoisted out of this one's for the same reason.
        //
        // This path did NOT deadlock — it holds neither `by_pty` nor `agents`,
        // which are the two `delivered_mask_lines` reaches for — so it is a
        // sibling by shape rather than a second instance of the bug. What it did
        // do is hold `attn_quiet` and `attn_waiting_ack` across a call that
        // takes four other registry locks, which is a lock-order edge one edit
        // away from being the same defect, and is the shape #1702 asks every
        // periodic tick to stop having. Computed here, with nothing held; the
        // per-pane cost is bounded exactly as phase 2's is.
        //
        // Keyed by pty and gated on the same `agent_ptys` skip the loop below
        // applies, so no agent pane is masked twice.
        let prompt_shaped: HashMap<u32, bool> = outputs
            .keys()
            .copied()
            .filter(|pty| !agent_ptys.contains(pty))
            .filter_map(|pty| tails.get(&pty).map(|t| (pty, t)))
            .map(|(pty, t)| {
                // #576 residual: the pane record is keyed by pty id, which is
                // exactly what this path has. The SESSION record needs a
                // resolution, and a plain pane has no snapshot to take it from —
                // so this path asks, and #1702's whole point is that it asks
                // here, with no lock held, rather than from inside the map hold
                // below. Semantics unchanged: a pane with no `by_pty` entry
                // resolves to `None` exactly as it did before.
                let session = self.session_for_pty(pty);
                let delivered = self.delivered_mask_lines(pty, session.as_deref());
                let shaped =
                    prompt_wait_detected(&mask_loomux_notices_with_record(t, &delivered));
                (pty, shaped)
            })
            .collect();
        let mut quiet = self.attn_quiet.lock_safe();
        let mut waiting_ack = self.attn_waiting_ack.lock_safe();
        let mut out = Vec::new();
        for (&pty, &cur) in outputs {
            if agent_ptys.contains(&pty) {
                continue;
            }
            let key = format!("pty:{pty}");
            let entry = quiet.entry(key.clone()).or_insert((cur, now));
            if cur != entry.0 {
                *entry = (cur, now);
                waiting_ack.remove(&key); // repainted → re-arm (menu answered)
            }
            let quiet_for = now.saturating_sub(entry.1);
            let recently_typed = last_inputs
                .get(&pty)
                .map(|&t| t != 0 && now.saturating_sub(t) < ATTENTION_RECENT_INPUT_MS)
                .unwrap_or(false);
            let waiting = !recently_typed
                && !waiting_ack.contains(&key)
                && quiet_for >= ATTENTION_QUIET_MS
                // #576, rev-126: masked for the same reason as the agent path
                // above. A plain pane is not delivered to, so it is the less
                // likely of the two to hold a notice — but a human pasting one
                // in to read it is enough, and the two readings must not
                // disagree about what counts as a question.
                // #1702: computed above, off every lock. `unwrap_or(false)` is
                // the same default the in-loop expression had — no entry means
                // this pane had no tail this tick.
                && prompt_shaped.get(&pty).copied().unwrap_or(false);
            if waiting {
                out.push(AttentionItem {
                    agent_id: String::new(),
                    group: String::new(),
                    name: String::new(),
                    role: None,
                    pty_id: Some(pty),
                    reason: "waiting",
                    detail: "This pane is waiting on your input".to_string(),
                });
            }
        }
        // Prune bookkeeping for ptys that have gone away (pane closed), so the
        // shared maps don't grow unbounded with `pty:` keys.
        quiet.retain(|k, _| !k.starts_with("pty:") || k[4..].parse::<u32>().map(|p| outputs.contains_key(&p)).unwrap_or(false));
        waiting_ack.retain(|k| !k.starts_with("pty:") || k[4..].parse::<u32>().map(|p| outputs.contains_key(&p)).unwrap_or(false));
        out.sort_by_key(|i| i.pty_id);
        out
    }

    /// Decide which current attention items warrant a fresh desktop toast:
    /// their group opted in, the reason is an event (not the persistent `gate`
    /// board state, which the board highlight already surfaces), and this
    /// (agent, reason) hasn't been toasted yet. Records only what actually
    /// fires — so enabling notifications surfaces already-pending attention —
    /// and prunes cleared/changed entries so a fresh onset toasts again.
    /// Returns the agent ids to toast; pure w.r.t. the OS, so the policy is
    /// testable without firing a real notification.
    pub fn attention_toast_targets(&self, items: &[AttentionItem]) -> Vec<String> {
        let notify = self.notify_groups.lock_safe().clone();
        let mut toasted = self.attn_emitted.lock_safe();
        let mut fire = Vec::new();
        for i in items {
            let already = toasted.get(&i.agent_id).map(|p| p == i.reason).unwrap_or(false);
            if !already && i.reason != "gate" && notify.contains(i.group.as_str()) {
                fire.push(i.agent_id.clone());
                toasted.insert(i.agent_id.clone(), i.reason.to_string());
            }
        }
        // Drop ledger entries whose attention cleared or whose reason changed,
        // so the same pane can toast again on a genuinely new onset.
        let current: HashMap<&str, &str> =
            items.iter().map(|i| (i.agent_id.as_str(), i.reason)).collect();
        toasted.retain(|id, reason| current.get(id.as_str()) == Some(&reason.as_str()));
        fire
    }

    /// One full attention cycle: read pty snapshots, compute the attention set,
    /// fire toasts for newly-attention panes in opted-in groups, and push the
    /// whole set to the frontend. Called on a timer by `start_attention`.
    /// The full attention set: the roster scan (`attention_tick`, all reasons)
    /// merged with the plain-pane scan (`plain_pane_attention`, `waiting` only).
    /// This is run_attention's core, factored out so the merge wiring — plain
    /// panes surface, an agent's pty is never double-covered — is testable
    /// without a real PtyManager (#40 review).
    #[allow(clippy::too_many_arguments)]
    pub fn attention_scan(
        &self,
        now: u64,
        agent_outputs: &HashMap<String, u64>,
        agent_tails: &HashMap<String, String>,
        agent_inputs: &HashMap<String, u64>,
        pane_outputs: &HashMap<u32, u64>,
        pane_tails: &HashMap<u32, String>,
        pane_inputs: &HashMap<u32, u64>,
        agent_ptys: &HashSet<u32>,
    ) -> Vec<AttentionItem> {
        let mut items = self.attention_tick(now, agent_outputs, agent_tails, agent_inputs);
        items.extend(self.plain_pane_attention(now, pane_outputs, pane_tails, pane_inputs, agent_ptys));
        items
    }

    pub fn run_attention(&self, now: u64) {
        let Some(_tick) = self.tick_gate("run_attention") else { return () };
        let (outputs, tails, last_inputs) = self.attention_inputs();
        // Also scan plain (non-agent) panes for an interactive prompt (#40).
        let (p_out, p_tails, p_ins, agent_ptys) = self.pane_attention_inputs();
        let items = self.attention_scan(
            now, &outputs, &tails, &last_inputs, &p_out, &p_tails, &p_ins, &agent_ptys,
        );
        for id in self.attention_toast_targets(&items) {
            if let Some(i) = items.iter().find(|i| i.agent_id == id) {
                // #904: `AttentionItem.group` is a display slot and is EMPTY
                // for a plain pane. Parse rather than trust: an empty id used
                // to resolve to `root.join("")` — the orchestration root
                // itself — so a toast for a group-less pane would have
                // appended to an `audit.jsonl` in the root. Unreachable today
                // (only real groups are in `notify_groups`) and now
                // unreachable by construction. The toast still fires.
                if let Ok(g) = GroupId::parse(&i.group) {
                    self.audit(&g, brand::AUDIT_ACTOR, "attention-toast",
                        json!({ "agent": i.agent_id, "reason": i.reason }));
                }
                notify_desktop(&format!("orrerix · {}", i.name), &i.detail);
            }
        }
        // #1702: bound BEFORE the branch, for `emit_session_learned`'s reason
        // one screen up — an `if let` scrutinee's temporaries live for the whole
        // body in edition 2021, so branching on `self.app.lock_safe().clone()`
        // directly holds that guard across the `emit` below AND across
        // `queue_depth_push`, which takes `queues`, `hold_episodes` and
        // `queue_depth_emitted`. `app` is the registry's most-taken lock; this
        // tick was holding it across an IPC serialization of the whole attention
        // set every three seconds, and nesting three more registry locks under
        // it. Same rule as the phases in `attention_tick`: nothing holds a
        // registry lock across a call that can take one.
        let app = self.app.lock_safe().clone();
        if let Some(app) = app {
            let _ = app.emit("orch-attention", &items);
            // #814: the delivery-queue badge rides this tick rather than owning a
            // cadence of its own. Two reasons, both structural. The age it shows
            // has to keep growing on screen, and the frontend has no clock — a
            // new frontend timer is what INV-4 exists to make expensive, and a
            // new backend tick would be a second thing to gate and shut down.
            // And the reading is derived from the same wall-clock `now` this scan
            // already took, so the badge and the attention set can never disagree
            // about when they were computed. `queue_depth_push` returns `None`
            // when the webview already has this exact set, so a group with
            // nothing queued costs one map read per tick and no emit.
            if let Some(depths) = self.queue_depth_push(now) {
                let _ = app.emit("orch-queue-depth", &depths);
            }
        }
    }
}
