//! The group's autonomy guardrails: notification and spawn-expansion
//! preferences, autonomous mode and its suspensions, auto-merge, auto-release,
//! full autonomy and dangerous mode, the budgets and the idle-tick and
//! compact-nudge knobs with their persistence, the autonomy read-back, the
//! agent cap (`set_max_agents`) and the spawn-rate check, as an
//! `impl OrchRegistry` block (#3498). The design is
//! `docs/design/orchestration.md`.

use super::*;

impl OrchRegistry {
    /// Whether desktop notifications are enabled for a group.
    pub fn notify_enabled(&self, group: &GroupId) -> bool {
        self.notify_groups.lock_safe().contains(group)
    }

    /// Enable/disable desktop notifications for a group, durably (a `notify`
    /// marker file, mirroring the pause marker) so the choice survives restarts.
    ///
    /// Marker write and audit append run with the set guard RELEASED (#743 S7,
    /// `performance.md` INV-5) — `pause_group`'s shape, in the same file, which
    /// this had drifted from: the transition is decided under a temporary
    /// guard, the IO follows. It also un-nests `AUDIT_LOCK` (process-global,
    /// §4 X6) from inside `notify_groups`, so no lock-ordering question
    /// survives here at all.
    ///
    /// **Reentrancy.** Two things decide, and they are one unit. The set's own
    /// `insert`/`remove` decides *whether* this call is a real transition, so
    /// only the caller that actually flipped the state does the IO and repeated
    /// enables produce one marker and one audit line. [`Self::marker_io`] then
    /// decides the *order*: it is held across the whole toggle, so an enable
    /// and a disable racing each other cannot land their file operations in the
    /// opposite order to their set mutations. Without it the loser's write
    /// could survive the winner's, leaving the marker — which is what rebuilds
    /// this set at startup — disagreeing with memory for the rest of the
    /// process AND across the next restart. See `marker_io`'s doc for why that
    /// is not left to the fact that both callers happen to be sync commands on
    /// the webview thread.
    pub fn set_notify(&self, group: &GroupId, on: bool) -> Result<(), String> {
        let _io = self.marker_io.lock_safe();
        let dir = self.group_dir(group);
        let changed = if on {
            self.notify_groups.lock_safe().insert(group.clone())
        } else {
            self.notify_groups.lock_safe().remove(group)
        };
        if !changed {
            return Ok(());
        }
        if on {
            fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let _ = fs::write(dir.join("notify"), b"");
            self.audit(group, "human", "notify-on", json!({}));
        } else {
            let _ = fs::remove_file(dir.join("notify"));
            self.audit(group, "human", "notify-off", json!({}));
        }
        Ok(())
    }

    /// Whether this group has opted OUT of the #260 minimize-on-spawn default
    /// (i.e. wants delegate panes to keep opening expanded, the pre-#260
    /// behavior). Feeds `spawn_opens_minimized` at each delegate spawn.
    pub fn spawn_expanded(&self, group: &GroupId) -> bool {
        self.spawn_expanded_groups.lock_safe().contains(group)
    }

    /// Opt a group in/out of the #260 minimize-on-spawn default, durably (a
    /// `spawn_expanded` marker file, mirroring `notify`) so the choice survives
    /// restarts. `on=true` means "expand every spawn like before #260";
    /// `on=false` (the default) restores the minimize-on-spawn behavior.
    /// Marker write and audit append run with the set guard released, the
    /// transition decided under it, and the whole toggle ordered by
    /// [`Self::marker_io`] — [`Self::set_notify`]'s shape, for the reasons
    /// argued there (#743 S7).
    pub fn set_spawn_expanded(&self, group: &GroupId, on: bool) -> Result<(), String> {
        let _io = self.marker_io.lock_safe();
        let dir = self.group_dir(group);
        let changed = if on {
            self.spawn_expanded_groups.lock_safe().insert(group.clone())
        } else {
            self.spawn_expanded_groups.lock_safe().remove(group)
        };
        if !changed {
            return Ok(());
        }
        if on {
            fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let _ = fs::write(dir.join("spawn_expanded"), b"");
            self.audit(group, "human", "spawn-expanded-on", json!({}));
        } else {
            let _ = fs::remove_file(dir.join("spawn_expanded"));
            self.audit(group, "human", "spawn-expanded-off", json!({}));
        }
        Ok(())
    }

    // ---------- autonomous mode (#83): idle-tick + auto-merge + budget ----------

    /// Whether autonomous idle-ticking is enabled for a group (drives the toggle
    /// button state and gates the idle-tick loop).
    pub fn is_autonomous(&self, group: &GroupId) -> bool {
        self.autonomous_groups.lock_safe().contains(group)
    }

    /// The usage-token count captured when autonomous mode was last enabled — the
    /// anchor the budget meters spend *from*, stored as the `autonomous` marker's
    /// content so it survives restarts. 0 when off or unstamped (legacy/empty
    /// marker → meters against 0, i.e. all history, which is the safe/conservative
    /// direction: it can only suspend *sooner*).
    pub(in crate::orchestration) fn autonomy_anchor(&self, group: &GroupId) -> u64 {
        fs::read_to_string(self.group_dir(group).join("autonomous"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
    }

    /// Whether autonomous mode is OFF *because the budget enforcer suspended it*
    /// (as opposed to a plain user toggle-off or never-on). Backed by a durable
    /// `autonomy_suspended` marker written by `enforce_autonomy_budgets` and
    /// cleared on a genuine re-enable, so it survives restarts. Only meaningful
    /// while off — `autonomy_state` gates it on `!is_autonomous`.
    fn autonomy_suspended(&self, group: &GroupId) -> bool {
        self.group_dir(group).join("autonomy_suspended").is_file()
    }

    /// Enable/disable autonomous idle-ticking for a group, durably (an
    /// `autonomous` marker file, mirroring the pause/notify markers). Enabling
    /// stamps the marker with the group's current usage-token total as the budget
    /// anchor, so the budget meters only spend incurred *after* this point — the
    /// "autonomous-era spend" the human is consenting to (re-enabling after a
    /// budget suspension re-anchors, which is what "toggle to resume" means).
    /// Audited on every real state change (actor `human`). The budget-suspension
    /// path uses `suspend_autonomous` instead (different failure policy).
    pub fn set_autonomous(&self, group: &GroupId, on: bool) -> Result<(), String> {
        self.set_autonomous_as(group, on, "human")
    }

    /// Disable autonomous mode as a **budget suspension** (actor `loomux`). Unlike a
    /// user disable (`set_autonomous_as`, disk-first + fail-loud to protect the
    /// consent boundary), the money-stop here is inverted: continued spend past the
    /// cap is the one direction this feature must never allow, so the in-memory flag
    /// is dropped **unconditionally** — ticking halts even if the durable marker
    /// can't be removed. A marker-removal failure is audited, not fatal; a surviving
    /// `autonomous` marker is then overridden at restart by the co-written
    /// `autonomy_suspended` marker (see the re-seed in `create_group`), so the group
    /// comes back OFF + suspended-visible, never silently ticking past its budget.
    pub(in crate::orchestration) fn suspend_autonomous(&self, group: &GroupId) {
        // Stop the spend first and unconditionally.
        self.autonomous_groups.lock_safe().remove(group);
        self.clear_idle_tick_latch(group);
        // Money-stop the merge gate too (rev-79 F4): auto-merge without autonomous
        // would leave the gate open, so drop it from the in-memory gate set
        // UNCONDITIONALLY — the same #149 pattern (in-memory authoritative even if
        // disk removal fails).
        self.force_disable_auto_merge(group, brand::AUDIT_ACTOR, "autonomous-suspended");
        self.force_disable_auto_release(group, brand::AUDIT_ACTOR, "autonomous-suspended");
        // Full autonomy (#778) is the mode that STARTS work, so a spent budget must
        // drop it for the same reason and by the same unconditional route.
        self.force_disable_full_autonomy(group, brand::AUDIT_ACTOR, "autonomous-suspended");
        // Best-effort durable disable; failure is surfaced in the audit trail.
        match remove_marker(&self.group_dir(group).join("autonomous")) {
            Ok(()) => self.audit(group, brand::AUDIT_ACTOR, "autonomous-off", json!({})),
            Err(e) => self.audit(group, brand::AUDIT_ACTOR, "autonomous-off-failed", json!({ "error": e })),
        }
    }

    /// Drop auto-merge for a group UNCONDITIONALLY (rev-79 F4 / #149 money-stop):
    /// the in-memory gate-decision set is authoritative and cleared even if the
    /// durable marker removal fails, so autonomous-off can never leave the merge
    /// gate open. Best-effort marker removal + audit/notify only when it was on.
    fn force_disable_auto_merge(&self, group: &GroupId, actor: &str, reason: &str) {
        if self.auto_merge_groups.lock_safe().remove(group) {
            let _ = remove_marker(&self.group_dir(group).join("auto_merge"));
            self.audit(group, actor, "auto-merge-off", json!({ "reason": reason }));
            let _ = self.deliver_to_orchestrator(group, &auto_merge_notice(false), brand::AUDIT_ACTOR);
        }
    }

    /// Drop auto-release for a group UNCONDITIONALLY (same money-stop as
    /// `force_disable_auto_merge`): auto-release without autonomous would leave the
    /// release gate open, so the in-memory gate set is authoritative and cleared
    /// even if the marker removal fails.
    fn force_disable_auto_release(&self, group: &GroupId, actor: &str, reason: &str) {
        if self.auto_release_groups.lock_safe().remove(group) {
            let _ = remove_marker(&self.group_dir(group).join("auto_release"));
            self.audit(group, actor, "auto-release-off", json!({ "reason": reason }));
            let _ = self.deliver_to_orchestrator(group, &auto_release_notice(false), brand::AUDIT_ACTOR);
        }
    }

    /// Drop supervised dangerous mode UNCONDITIONALLY (mutual exclusion: enabling
    /// autonomous force-clears it — the two can never both be on). In-memory
    /// authoritative even if the marker's disk removal fails, with an audit entry
    /// and a human-visible notice. `by_autonomous` tailors the notice wording.
    fn force_disable_dangerous_mode(&self, group: &GroupId, actor: &str, reason: &str, by_autonomous: bool) {
        if self.dangerous_groups.lock_safe().remove(group) {
            let _ = remove_marker(&self.group_dir(group).join("dangerous_mode"));
            self.audit(group, actor, "dangerous-mode-off", json!({ "reason": reason }));
            let _ = self.deliver_to_orchestrator(group, &dangerous_mode_notice(false, by_autonomous), brand::AUDIT_ACTOR);
        }
    }

    /// `set_autonomous` with an explicit actor so a loomux-initiated suspension
    /// (budget exhausted) audits honestly rather than as a human toggle.
    fn set_autonomous_as(&self, group: &GroupId, on: bool, actor: &str) -> Result<(), String> {
        let dir = self.group_dir(group);
        if on {
            {
                // #762 rev-260 B1: the reserve and the marker write are ONE unit,
                // ordered by `marker_io`. The set `insert` alone decides *who*
                // proceeds, but it does not decide *when* the file lands — the set
                // lock is released the moment the statement ends, and everything
                // between it and the write used to be protected only by the fact
                // that both callers were sync commands on the webview thread. This
                // conversion removes that, and the resulting interleave is not
                // benign: a disable running inside this window removes a marker
                // that is not there yet (`remove_marker` maps NotFound to Ok),
                // clears the set, and then THIS write recreates the marker — memory
                // OFF, consent marker on disk, autonomous mode resurrected at the
                // next restart's re-seed without the human ever re-consenting.
                //
                // The anchor computation is deliberately INSIDE the window. It is
                // the widest part of it (a full usage aggregation over every pane),
                // so leaving it out would be protecting the narrow half and
                // shipping the wide one.
                let _io = self.marker_io.lock_safe();
                // Reserve the enable atomically (L1): a single `insert` decides who
                // proceeds, so a concurrent/duplicate enable can't double-anchor or
                // race the marker write. Only the first caller (newly inserted) writes
                // the anchor + marker; anyone else sees it already on and no-ops.
                let newly = self.autonomous_groups.lock_safe().insert(group.clone());
                if !newly {
                    return Ok(()); // already on — don't re-anchor
                }
                // Anchor = spend at enable time, so the budget delta starts at 0.
                // Computed without holding the set lock (group_usage takes its own).
                let anchor = self.group_token_total(group);
                if let Err(e) = fs::create_dir_all(&dir)
                    .and_then(|_| fs::write(dir.join("autonomous"), anchor.to_string()))
                {
                    // Roll back the reservation so memory never claims ON without a
                    // durable marker (a lost enable is the safe direction — it fails
                    // OFF and re-asks for consent — but we still surface the failure).
                    self.autonomous_groups.lock_safe().remove(group);
                    return Err(format!("failed to enable autonomous mode: {e}"));
                }
                // A genuine (re-)enable resolves any prior budget suspension: clear the
                // suspended marker so the UI stops flagging it. Best-effort — it's a
                // UI hint, and `autonomy_state` only reports suspended while OFF anyway.
                let _ = remove_marker(&dir.join("autonomy_suspended"));
                self.audit(group, actor, "autonomous-on",
                    json!({ "budget_anchor_tokens": anchor }));
            }
            // Outside the guard: this delivers a notice into a pane, and a full
            // pipe parks the writer (performance.md §2 P6). Nothing that can block
            // on an agent's terminal belongs under a process-global lock.
            //
            // Mutual exclusion (#83): supervised dangerous mode is the *not*-
            // autonomous manual mode, so enabling autonomous force-clears it
            // (audited + human-visible notice).
            self.force_disable_dangerous_mode(group, actor, "autonomous-enabled", true);
        } else {
            {
                // Same unit as the enable arm above, in the mirror direction: the
                // marker removal and the set removal cannot be split by a racing
                // enable.
                let _io = self.marker_io.lock_safe();
                if !self.autonomous_groups.lock_safe().contains(group) {
                    return Ok(()); // already off
                }
                // Remove the durable marker FIRST and fail the call if it doesn't go
                // (L2): a surviving marker would silently re-enable autonomous mode on
                // the next restart's re-seed without renewed consent. Only flip the
                // in-memory flag once disk agrees, so a failed disable leaves state
                // consistently ON (matching the marker the human still sees).
                if let Err(e) = remove_marker(&dir.join("autonomous")) {
                    self.audit(group, actor, "autonomous-off-failed", json!({ "error": e }));
                    return Err(format!(
                        "couldn't disable autonomous mode: the consent marker could not be \
                         removed, so it stays ON — retry or check disk/permissions"
                    ));
                }
                self.autonomous_groups.lock_safe().remove(group);
                // Clear the idle-tick latch so a later re-enable starts clean.
                self.clear_idle_tick_latch(group);
                self.audit(group, actor, "autonomous-off", json!({}));
            }
            // Dependency (#83): auto-merge AND auto-release exist only in autonomous
            // mode, so turning autonomous OFF force-disables both unconditionally
            // (rev-79 F4) — the pair can never be gate-on/autonomous-off. Both
            // deliver, so both stay outside the guard.
            self.force_disable_auto_merge(group, actor, "autonomous-disabled");
            self.force_disable_auto_release(group, actor, "autonomous-disabled");
            // Same dependency for full autonomy (#778), for a stronger reason: without
            // the idle tick there is nothing to self-select on, and an inverted start
            // default outliving its consent is the one direction it must never have.
            self.force_disable_full_autonomy(group, actor, "autonomous-disabled");
        }
        Ok(())
    }

    /// Whether the orchestrator may merge adequately-tested PRs itself for a group
    /// (auto-merge gate off = the default human merge gate). Drives the toggle
    /// state and is mirrored into the orchestrator's kickoff config.
    pub fn is_auto_merge(&self, group: &GroupId) -> bool {
        self.auto_merge_groups.lock_safe().contains(group)
    }

    /// Enable/disable the auto-merge gate for a group, durably (an `auto_merge`
    /// marker file). Default OFF = today's behavior (human merges). The behavior
    /// lives in the orchestrator template, which reads the flag from its kickoff
    /// config; a live toggle both re-seeds that config for a restart and delivers
    /// one audited notice so the running orchestrator learns the new gate.
    pub fn set_auto_merge(&self, group: &GroupId, on: bool) -> Result<(), String> {
        let dir = self.group_dir(group);
        // #762 rev-260 B1: `marker_io` makes the dependency check, the reserve and
        // the marker write one unit — see `set_autonomous_as` for the interleave
        // this closes and why the set lock alone cannot. Released before the
        // notice below, which writes into a pane (§2 P6).
        //
        // Holding the `is_autonomous` check inside the same window also closes the
        // HUMAN-vs-human half of #788: an autonomous-off cannot now land between
        // this check and this reserve, because that path takes the same lock for
        // its own marker region. #788 stays open for the idle-tick half
        // (`suspend_autonomous`), which deliberately does not take this lock.
        let _io = self.marker_io.lock_safe();
        if on {
            // Dependency (#83): auto-merge authority exists ONLY in autonomous mode.
            // Reject enabling it while autonomous is off so the pair can never be
            // auto_merge-on/autonomous-off — the combo the enforced gate keys on.
            if !self.is_autonomous(group) {
                return Err(
                    "auto-merge requires autonomous mode — turn on Autonomous mode first".into(),
                );
            }
            // Atomic reserve (mirrors set_autonomous_as): only the newly-inserting
            // caller writes the marker; a duplicate enable no-ops.
            let newly = self.auto_merge_groups.lock_safe().insert(group.clone());
            if !newly {
                return Ok(()); // no-op: don't re-notify
            }
            if let Err(e) = fs::create_dir_all(&dir)
                .and_then(|_| fs::write(dir.join("auto_merge"), b""))
            {
                self.auto_merge_groups.lock_safe().remove(group);
                return Err(format!("failed to enable auto-merge: {e}"));
            }
            self.audit(group, "human", "auto-merge-on", json!({}));
        } else {
            if !self.auto_merge_groups.lock_safe().contains(group) {
                return Ok(()); // no-op: don't re-notify
            }
            // Disk first, then memory (L2): a surviving `auto_merge` marker would
            // silently re-enable the orchestrator's merge authority on restart
            // without renewed consent, so a failed removal fails the toggle and
            // leaves the gate consistently ON.
            if let Err(e) = remove_marker(&dir.join("auto_merge")) {
                self.audit(group, "human", "auto-merge-off-failed", json!({ "error": e }));
                return Err(format!(
                    "couldn't disable auto-merge: the consent marker could not be \
                     removed, so it stays ON — retry or check disk/permissions"
                ));
            }
            self.auto_merge_groups.lock_safe().remove(group);
            self.audit(group, "human", "auto-merge-off", json!({}));
        }
        drop(_io);
        // Tell the running orchestrator the gate moved (best-effort; a dead/paused
        // orchestrator just misses it and re-reads its kickoff config on resume).
        let _ = self.deliver_to_orchestrator(group, &auto_merge_notice(on), brand::AUDIT_ACTOR);
        Ok(())
    }

    /// Whether the orchestrator may publish releases/tags itself for a group
    /// (auto-release gate off = releases need a per-tag human grant). Independent
    /// of auto-merge. Drives the toggle state + mirrored into the kickoff config.
    pub fn is_auto_release(&self, group: &GroupId) -> bool {
        self.auto_release_groups.lock_safe().contains(group)
    }

    /// Enable/disable the auto-release gate for a group, durably (an `auto_release`
    /// marker file). Default OFF = publishing needs a per-tag human grant. Mirrors
    /// `set_auto_merge` exactly (independent marker): gated behind autonomous mode
    /// (rejects enable unless autonomous is on), disk-first fail-loud disable, one
    /// audited notice to the orchestrator.
    pub fn set_auto_release(&self, group: &GroupId, on: bool) -> Result<(), String> {
        let dir = self.group_dir(group);
        // #762 rev-260 B1: same unit and same reason as `set_auto_merge` — this
        // marker carries release/tag authority, so the interleave that resurrects
        // a withdrawn one at restart is the same defect wearing a different name.
        let _io = self.marker_io.lock_safe();
        if on {
            // Same dependency as auto-merge: auto-release authority exists ONLY in
            // autonomous mode, so the pair can never be auto_release-on/autonomous-off.
            if !self.is_autonomous(group) {
                return Err(
                    "auto-release requires autonomous mode — turn on Autonomous mode first".into(),
                );
            }
            let newly = self.auto_release_groups.lock_safe().insert(group.clone());
            if !newly {
                return Ok(()); // no-op: don't re-notify
            }
            if let Err(e) = fs::create_dir_all(&dir)
                .and_then(|_| fs::write(dir.join("auto_release"), b""))
            {
                self.auto_release_groups.lock_safe().remove(group);
                return Err(format!("failed to enable auto-release: {e}"));
            }
            self.audit(group, "human", "auto-release-on", json!({}));
        } else {
            if !self.auto_release_groups.lock_safe().contains(group) {
                return Ok(()); // no-op: don't re-notify
            }
            // Disk first, then memory: a surviving `auto_release` marker would
            // silently re-enable publishing authority on restart, so a failed
            // removal fails the toggle and leaves the gate consistently ON.
            if let Err(e) = remove_marker(&dir.join("auto_release")) {
                self.audit(group, "human", "auto-release-off-failed", json!({ "error": e }));
                return Err(format!(
                    "couldn't disable auto-release: the consent marker could not be \
                     removed, so it stays ON — retry or check disk/permissions"
                ));
            }
            self.auto_release_groups.lock_safe().remove(group);
            self.audit(group, "human", "auto-release-off", json!({}));
        }
        drop(_io);
        let _ = self.deliver_to_orchestrator(group, &auto_release_notice(on), brand::AUDIT_ACTOR);
        Ok(())
    }

    /// The guardrails this group is RUNNING — its roster, its pinned workflow
    /// name and its resolved intake profile — or `None` when this registry no
    /// longer holds the group (it ended, or the id names nothing).
    ///
    /// Exists for `gh.rs` (#2663), which has to answer "what is THIS group's
    /// writable label vocabulary" and had no way to ask. The whole struct rather
    /// than the one field, because it is the same `&Guardrails` the #1689 pair
    /// (`load_active_workflow` / `active_workflow_path`) takes, so a caller that
    /// later needs the group's FILE reaches for that pair instead of growing a
    /// second accessor beside this one.
    ///
    /// **`None` is not "the built-in"**, deliberately, and that is the difference
    /// from [`hold_label_of`] one line down: a caller that could not find the
    /// group must be able to tell that apart from a group that resolved to the
    /// built-in, because the two have different right answers (`gh.rs` falls back
    /// to the REPO's file for the first and must not for the second).
    pub fn guardrails_of(&self, group: &GroupId) -> Option<Guardrails> {
        self.groups.lock_safe().get(group).map(|g| g.guardrails.clone())
    }

    /// This group's resolved veto spelling (#778) — `guardrails.intake.hold`,
    /// falling back to the built-in for a group this registry no longer holds.
    ///
    /// One accessor, so every agent-facing surface that names the veto (the
    /// contract's `{{HOLD_LABEL}}`, the kickoff clause, the toggle notice) reads
    /// the same field the intake poller checks against. The whole B1 defect was
    /// three surfaces each answering this question for themselves.
    pub(in crate::orchestration) fn hold_label_of(&self, group: &GroupId) -> String {
        self.groups
            .lock_safe()
            .get(group)
            .map(|g| g.guardrails.intake.hold.clone())
            .filter(|h| !h.trim().is_empty())
            .unwrap_or_else(builtin_hold_label)
    }

    /// Whether full autonomy (#778) is on for a group: the orchestrator self-selects
    /// eligible work on its idle tick instead of waiting for the opt-in label funnel.
    pub fn is_full_autonomy(&self, group: &GroupId) -> bool {
        self.full_autonomy_groups.lock_safe().contains(group)
    }

    /// The goal string qualifying full autonomy for a group — the `full_autonomy`
    /// marker's *content*, the same anchor-in-content shape `set_autonomous` uses for
    /// the budget anchor, so consent and the parameter that qualifies it are written
    /// atomically and come back together across a restart. `None` when the mode is
    /// on with no goal (empty marker), which renders as "no goal set" rather than as
    /// a goal that got lost.
    ///
    /// **Gated on `is_full_autonomy`, and the gate is the point, not a formality.**
    /// The marker's presence is NOT equivalent to the mode being on: the force-clear
    /// paths are money-stops that drop the in-memory flag unconditionally and remove
    /// the marker only best-effort (`force_disable_full_autonomy`'s `let _ =`), so a
    /// disk failure genuinely leaves a marker — goal and all — behind while the mode
    /// is off. In that window `full_autonomy_groups` is the authority, and reporting
    /// the orphaned goal would have `orch_autonomy` claim consent that is not in
    /// force. Gating here rather than at the one JSON call site makes that total: no
    /// future caller can reintroduce it. (The orphaned marker itself is cleared by
    /// the restart reconcile in `create_group`.)
    ///
    /// Re-sanitized on read: the marker is a file a human can hand-edit, and a
    /// newline smuggled in there would reach a CLI paste. The sanitizer is
    /// idempotent, so this costs nothing for the normal path.
    pub fn full_autonomy_goal(&self, group: &GroupId) -> Option<String> {
        if !self.is_full_autonomy(group) {
            return None;
        }
        let goal = fs::read_to_string(self.group_dir(group).join("full_autonomy")).ok()?;
        let goal = sanitize_full_autonomy_goal(&goal);
        (!goal.is_empty()).then_some(goal)
    }

    /// Enable/disable full autonomy for a group, durably (a `full_autonomy` marker
    /// whose content is the goal). Default OFF = today's opt-in label funnel.
    ///
    /// Mirrors `set_auto_merge`'s machinery — gated behind autonomous mode, atomic
    /// in-memory reserve, disk-first fail-loud disable, one audited notice — for a
    /// stronger reason than its siblings have: this toggle INVERTS the start
    /// default, so it must never outlive the consent to run unattended at all.
    ///
    /// **One deliberate difference from the siblings.** They no-op on a duplicate
    /// enable; here a duplicate enable carrying a DIFFERENT goal re-aims the mode
    /// instead. The goal is the consent's parameter, not decoration: a human who
    /// retypes it has changed what they are consenting to, and silently discarding
    /// that would leave them believing they had re-aimed a fleet still running the
    /// old goal. Same goal → true no-op (no re-audit, no re-notify).
    pub fn set_full_autonomy(&self, group: &GroupId, on: bool, goal: &str) -> Result<(), String> {
        let dir = self.group_dir(group);
        if on {
            // Dependency: full autonomy is a mode of autonomous idle-ticking, so it
            // cannot be enabled without it — there would be no tick to self-select on,
            // and the inverted start default would sit there as unrenewed consent.
            if !self.is_autonomous(group) {
                return Err(
                    "full autonomy requires autonomous mode — turn on Autonomous mode first"
                        .into(),
                );
            }
            let goal = sanitize_full_autonomy_goal(goal);
            // Atomic reserve for the FIRST enable (mirrors set_autonomous_as /
            // set_auto_merge): a single `insert` decides who proceeds, so two
            // concurrent first-enables can't both write the marker or double-audit.
            //
            // It deliberately does NOT serialize the re-aim path below, and saying so
            // is the honest version of this comment: two concurrent re-aims carrying
            // different goals both observe `!newly`, both pass the compare, and both
            // write — last writer wins. That is acceptable rather than merely
            // tolerated. The caller is a human operating one toggle in one panel, so
            // the race needs two humans or two windows to exist at all; and neither
            // outcome is a consent violation, because the mode is on either way and
            // whichever goal lands is one a human typed, with BOTH re-aims in the
            // audit trail. Holding a lock across the read-compare-write would buy
            // ordering nobody can observe and would put file IO under the set lock.
            let newly = self.full_autonomy_groups.lock_safe().insert(group.clone());
            let previous = if newly { None } else { self.full_autonomy_goal(group) };
            // The no-op is gated on the marker being READABLE, not merely on the
            // goal comparing equal (rev round 1 NB2). `full_autonomy_goal`
            // answers `None` both for "on with no goal" and for "the marker is
            // gone" — a marker a force-clear removed best-effort, or a hand
            // delete — and treating those alike let a re-enable with an empty
            // goal return early without rewriting it, leaving the mode ON in
            // memory with nothing durable behind it and OFF after a restart.
            // Asking the disk directly separates them: no marker means fall
            // through and write one, whatever the goal says.
            let marker_present = dir.join("full_autonomy").is_file();
            if !newly && marker_present && previous.as_deref().unwrap_or("") == goal {
                return Ok(()); // already on with this exact goal — don't re-notify
            }
            if let Err(e) = fs::create_dir_all(&dir)
                .and_then(|_| fs::write(dir.join("full_autonomy"), goal.as_bytes()))
            {
                // Roll back the reservation so memory never claims ON without a durable
                // marker (failing OFF re-asks for consent — the safe direction). A
                // failed RE-goal leaves the previous goal in force, which is likewise
                // the conservative outcome.
                if newly {
                    self.full_autonomy_groups.lock_safe().remove(group);
                }
                return Err(format!("failed to enable full autonomy: {e}"));
            }
            // Re-arm the triage trigger (#778). `intake_seen[group].eligible` means
            // "eligible at the last intake poll", and an empty one is what makes the
            // next poll announce the whole eligible backlog once — precisely the
            // triage pass the enable notice tells the orchestrator to post. The
            // poller clears it while the mode is off, but only for groups it
            // actually reaches, so an off→on flip inside one poll interval would
            // otherwise inherit a set populated under different consent and announce
            // nothing at all. A re-aim clears it too: the goal is what decides
            // whether an eligible issue is worth starting, so a human who changes it
            // is asking for the backlog to be judged again.
            self.intake_seen.lock_safe().entry(group.clone()).or_default().eligible.clear();
            if newly {
                self.audit(group, "human", "full-autonomy-on", json!({ "goal": goal }));
            } else {
                // A re-aim, audited as its own action so the trail distinguishes "the
                // human turned this on" from "the human changed what it is pointed at".
                self.audit(group, "human", "full-autonomy-goal-set",
                    json!({ "goal": goal, "previous": previous }));
            }
        } else {
            if !self.full_autonomy_groups.lock_safe().contains(group) {
                return Ok(()); // no-op: don't re-notify
            }
            // Disk first, then memory: a surviving `full_autonomy` marker would
            // silently re-invert the start default on the next restart's re-seed
            // without renewed consent, so a failed removal fails the toggle and leaves
            // the mode consistently ON (matching the marker the human still sees).
            if let Err(e) = remove_marker(&dir.join("full_autonomy")) {
                self.audit(group, "human", "full-autonomy-off-failed", json!({ "error": e }));
                return Err(
                    "couldn't disable full autonomy: the consent marker could not be removed, \
                     so it stays ON — retry or check disk/permissions"
                        .to_string(),
                );
            }
            self.full_autonomy_groups.lock_safe().remove(group);
            self.audit(group, "human", "full-autonomy-off", json!({}));
        }
        // Tell the running orchestrator the start default moved (best-effort; a
        // dead/paused orchestrator just misses it and re-reads its kickoff config on
        // resume). A re-aim re-delivers the ON notice deliberately: the protocol it
        // carries is scoped to the goal, so a new goal is a new instruction.
        let hold = self.hold_label_of(group);
        let _ = self.deliver_to_orchestrator(group, &full_autonomy_notice(on, goal, &hold), brand::AUDIT_ACTOR);
        Ok(())
    }

    /// Drop full autonomy for a group UNCONDITIONALLY — the same money-stop shape as
    /// [`OrchRegistry::force_disable_auto_merge`]. The in-memory set is authoritative
    /// and cleared even if the marker removal fails, so autonomous-off or a spent
    /// budget can never leave the one mode whose job is to START new work armed.
    fn force_disable_full_autonomy(&self, group: &GroupId, actor: &str, reason: &str) {
        if self.full_autonomy_groups.lock_safe().remove(group) {
            let _ = remove_marker(&self.group_dir(group).join("full_autonomy"));
            self.audit(group, actor, "full-autonomy-off", json!({ "reason": reason }));
            // The OFF notice names no veto, so the spelling is irrelevant here —
            // passed for signature reasons only.
            let _ =
                self.deliver_to_orchestrator(group, &full_autonomy_notice(false, "", ""), brand::AUDIT_ACTOR);
        }
    }

    /// Whether supervised dangerous mode is on for a group (the human is present and
    /// authorized manual merges/releases outside autonomous mode). Mutually
    /// exclusive with autonomous.
    pub fn is_dangerous_mode(&self, group: &GroupId) -> bool {
        self.dangerous_groups.lock_safe().contains(group)
    }

    /// Enable/disable supervised dangerous mode, durably (a `dangerous_mode`
    /// marker). **Mutually exclusive with autonomous**: enabling is REJECTED while
    /// autonomous is on (with a clear error); enabling autonomous force-clears this
    /// (see `set_autonomous_as`). Disk-first fail-loud disable, one audited notice.
    /// Human-only (Tauri command; no MCP surface — an agent can no more enable this
    /// than it can mint a grant; the marker's FS-forgeability is the same documented
    /// bypass class as grant files, closed by a machine account).
    pub fn set_dangerous_mode(&self, group: &GroupId, on: bool) -> Result<(), String> {
        let dir = self.group_dir(group);
        // #762 rev-260 B1/B2: same unit as the two gates above. This one is the
        // clearest case of the conversion CREATING the exposure rather than
        // widening it — `force_disable_dangerous_mode` has exactly one call site
        // (`set_autonomous_as`'s enable arm) and no background path reaches it, so
        // until this commit the webview thread really was the only thing keeping
        // two dangerous-mode toggles from interleaving. It is restored here rather
        // than argued away.
        let _io = self.marker_io.lock_safe();
        if on {
            // Mutual exclusion: dangerous mode is the *supervised, not-autonomous*
            // mode. Reject enabling it while autonomous is on.
            if self.is_autonomous(group) {
                return Err(
                    "dangerous mode can't be enabled while autonomous mode is on — they are \
                     mutually exclusive; turn off Autonomous mode first".into(),
                );
            }
            let newly = self.dangerous_groups.lock_safe().insert(group.clone());
            if !newly {
                return Ok(()); // no-op
            }
            if let Err(e) = fs::create_dir_all(&dir)
                .and_then(|_| fs::write(dir.join("dangerous_mode"), b""))
            {
                self.dangerous_groups.lock_safe().remove(group);
                return Err(format!("failed to enable dangerous mode: {e}"));
            }
            self.audit(group, "human", "dangerous-mode-on", json!({}));
        } else {
            if !self.dangerous_groups.lock_safe().contains(group) {
                return Ok(()); // no-op
            }
            // Disk first, fail loud: a surviving marker would silently re-enable
            // merge/release authority on restart.
            if let Err(e) = remove_marker(&dir.join("dangerous_mode")) {
                self.audit(group, "human", "dangerous-mode-off-failed", json!({ "error": e }));
                return Err(format!(
                    "couldn't disable dangerous mode: the marker could not be removed, so it \
                     stays ON — retry or check disk/permissions"
                ));
            }
            self.dangerous_groups.lock_safe().remove(group);
            self.audit(group, "human", "dangerous-mode-off", json!({ "reason": "human" }));
        }
        drop(_io);
        let _ = self.deliver_to_orchestrator(group, &dangerous_mode_notice(on, false), brand::AUDIT_ACTOR);
        Ok(())
    }

    /// Set a live group's autonomous token budget on the fly (0 = no cap). Written
    /// to the in-memory guardrail (which the idle-tick budget check reads fresh)
    /// and persisted to group.json so a restart keeps it, then audited. Does NOT
    /// move the enable-time anchor — the delta the budget meters is unaffected, so
    /// raising the budget after a suspension lets the human resume without losing
    /// the already-counted spend. Returns the applied value.
    pub fn set_autonomy_budget(&self, group: &GroupId, tokens: u64) -> Result<u64, String> {
        let _io = self.group_file_io.lock_safe();
        let old = self
            .group(group)
            .ok_or("unknown group")?
            .guardrails
            .autonomy_budget_tokens;
        if tokens == old {
            return Ok(tokens);
        }
        // Persist first: a failed disk write must leave the in-memory value (what
        // the budget check reads) unchanged so the two never disagree.
        self.persist_autonomy_budget(group, tokens)?;
        self.groups
            .lock_safe()
            .get_mut(group)
            .ok_or("unknown group")?
            .guardrails
            .autonomy_budget_tokens = tokens;
        self.audit(group, "human", "autonomy-budget-set",
            json!({ "from": old, "to": tokens }));
        Ok(tokens)
    }

    /// Set a live group's idle-tick quiet window in minutes on the fly (#83). 0 is
    /// coerced to the default and any value floored at 1 (the `autonomous` marker is
    /// the on/off switch — this must never silently disable ticking); clamped to
    /// `MAX_IDLE_TICK_MINUTES`. Written to the in-memory guardrail (the idle-tick
    /// loop reads it fresh each pass) and persisted, then audited. Returns the
    /// applied (clamped) value — lets the human drop it to 1–2 min to verify.
    pub fn set_idle_tick_minutes(&self, group: &GroupId, minutes: u32) -> Result<u32, String> {
        let _io = self.group_file_io.lock_safe();
        let applied = if minutes == 0 {
            DEFAULT_IDLE_TICK_MINUTES
        } else {
            minutes.clamp(1, MAX_IDLE_TICK_MINUTES)
        };
        let old = self
            .group(group)
            .ok_or("unknown group")?
            .guardrails
            .idle_tick_minutes;
        if applied == old {
            return Ok(applied);
        }
        // Persist first so a failed write leaves the in-memory value (what the loop
        // reads) unchanged.
        self.persist_idle_tick_minutes(group, applied)?;
        self.groups
            .lock_safe()
            .get_mut(group)
            .ok_or("unknown group")?
            .guardrails
            .idle_tick_minutes = applied;
        self.audit(group, "human", "idle-tick-minutes-set",
            json!({ "from": old, "to": applied }));
        Ok(applied)
    }

    /// Rewrite only `guardrails.idle_tick_minutes` in group.json (additive patch).
    fn persist_idle_tick_minutes(&self, group: &GroupId, minutes: u32) -> Result<(), String> {
        self.persist_guardrail_u64(group, "idle_tick_minutes", minutes as u64)
    }

    /// Set a live group's idle-tick activity floor in bytes on the fly (#83, the
    /// rev-59 runtime remedy). 0 → default; floored at 1 and clamped to the max.
    /// Persisted + audited; the idle-tick loop reads it fresh each pass. Returns the
    /// applied (clamped) value — raise it if a chatty CLI's idle repaints starve the
    /// tick, lower it if real small outputs read as idle.
    pub fn set_idle_activity_floor(&self, group: &GroupId, bytes: u64) -> Result<u64, String> {
        let _io = self.group_file_io.lock_safe();
        let applied = if bytes == 0 {
            DEFAULT_IDLE_ACTIVITY_FLOOR_BYTES
        } else {
            bytes.clamp(1, MAX_IDLE_ACTIVITY_FLOOR_BYTES)
        };
        let old = self
            .group(group)
            .ok_or("unknown group")?
            .guardrails
            .idle_activity_floor_bytes;
        if applied == old {
            return Ok(applied);
        }
        self.persist_guardrail_u64(group, "idle_activity_floor_bytes", applied)?;
        self.groups
            .lock_safe()
            .get_mut(group)
            .ok_or("unknown group")?
            .guardrails
            .idle_activity_floor_bytes = applied;
        self.audit(group, "human", "idle-activity-floor-set",
            json!({ "from": old, "to": applied }));
        Ok(applied)
    }

    /// Rewrite only `guardrails.autonomy_budget_tokens` in group.json, preserving
    /// every other stored field (additive patch, same crash-safe write as
    /// `persist_max_agents`).
    fn persist_autonomy_budget(&self, group: &GroupId, tokens: u64) -> Result<(), String> {
        self.persist_guardrail_u64(group, "autonomy_budget_tokens", tokens)
    }

    /// Additive crash-safe patch of a single numeric `guardrails.<key>` in
    /// group.json. The shared body behind the live-settable numeric
    /// guardrails (budget, activity floor). Thin wrapper over
    /// `persist_guardrail_json`.
    fn persist_guardrail_u64(&self, group: &GroupId, key: &str, value: u64) -> Result<(), String> {
        self.persist_guardrail_json(group, key, json!(value))
    }

    /// Additive crash-safe patch of a single arbitrary-JSON `guardrails.<key>`
    /// in group.json (preserves every other field; temp-file + atomic rename,
    /// the `persist_max_agents` pattern). `persist_guardrail_u64` covers the
    /// numeric guardrails; this is the general form, needed for
    /// `compact_nudge_roles` (a string array, not a number).
    fn persist_guardrail_json(&self, group: &GroupId, key: &str, value: Value) -> Result<(), String> {
        let dir = self.group_dir(group);
        let path = dir.join("group.json");
        let mut v: Value = serde_json::from_str(&fs::read_to_string(&path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        let obj = v.as_object_mut().ok_or("group.json root is not a JSON object")?;
        match obj.get_mut("guardrails").and_then(Value::as_object_mut) {
            Some(guard) => {
                guard.insert(key.into(), value);
            }
            None => {
                obj.insert("guardrails".into(), json!({ key: value }));
            }
        }
        // Crash-safe write, exactly as `persist_max_agents` and
        // `persist_roster` do it — this was the one `group.json`
        // writer still hand-rolling a temp-and-rename against a FIXED sibling
        // name (`group.json.tmp`) and without an fsync. Both mattered once
        // these setters left the webview thread (#762): a shared temp name is
        // what `atomic_write`'s own doc says concurrent writers to this file
        // must not have, and the missing fsync is #133's disk-full hole. The
        // read-modify-write above is serialized by `group_file_io`; this makes
        // the write half match the rest of the family rather than differ from
        // it for no reason anybody could state.
        let body = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
        atomic_write(&path, body.as_bytes()).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Set a live group's compact-nudge quiet window in minutes on the fly
    /// (#287). 0 disables the feature entirely — unlike
    /// `set_idle_tick_minutes`, 0 is a legitimate value here (there is no
    /// separate on/off marker for compact-nudge), so it is only capped at the
    /// max, never floored to a default. Written to the in-memory guardrail
    /// (the compact-nudge loop reads it fresh each pass) and persisted, then
    /// audited. Returns the applied (clamped) value.
    pub fn set_compact_nudge_minutes(&self, group: &GroupId, minutes: u32) -> Result<u32, String> {
        let _io = self.group_file_io.lock_safe();
        let applied = minutes.min(MAX_COMPACT_NUDGE_MINUTES);
        let old = self
            .group(group)
            .ok_or("unknown group")?
            .guardrails
            .compact_nudge_minutes;
        if applied == old {
            return Ok(applied);
        }
        self.persist_guardrail_u64(group, "compact_nudge_minutes", applied as u64)?;
        self.groups
            .lock_safe()
            .get_mut(group)
            .ok_or("unknown group")?
            .guardrails
            .compact_nudge_minutes = applied;
        self.audit(group, "human", "compact-nudge-minutes-set",
            json!({ "from": old, "to": applied }));
        Ok(applied)
    }

    /// Set a live group's compact-nudge eligible roles on the fly (#287).
    /// Canonicalized via `canonicalize_compact_nudge_roles` (drops unrecognized
    /// entries, case-normalizes survivors, dedupes/sorts, falls back to
    /// `["orchestrator"]` if empty) since a live setter bypasses `clamped()`.
    /// Persisted + audited. Returns the applied set.
    pub fn set_compact_nudge_roles(&self, group: &GroupId, roles: Vec<String>) -> Result<Vec<String>, String> {
        let _io = self.group_file_io.lock_safe();
        let applied = canonicalize_compact_nudge_roles(roles);
        let old = self
            .group(group)
            .ok_or("unknown group")?
            .guardrails
            .compact_nudge_roles
            .clone();
        if applied == old {
            return Ok(applied);
        }
        self.persist_guardrail_json(group, "compact_nudge_roles", json!(applied))?;
        self.groups
            .lock_safe()
            .get_mut(group)
            .ok_or("unknown group")?
            .guardrails
            .compact_nudge_roles = applied.clone();
        self.audit(group, "human", "compact-nudge-roles-set",
            json!({ "from": old, "to": &applied }));
        Ok(applied)
    }

    /// Set a live group's compact-nudge context-usage escalation threshold on
    /// the fly (#328). `0` disables escalation (purely opportunistic); only
    /// capped at the max, never floored to a default — same "0 is a real off"
    /// shape as `compact_nudge_minutes`. Written to the in-memory guardrail
    /// and persisted, then audited. Returns the applied (clamped) value.
    pub fn set_compact_context_threshold(&self, group: &GroupId, percent: u32) -> Result<u32, String> {
        let _io = self.group_file_io.lock_safe();
        let applied = percent.min(MAX_COMPACT_CONTEXT_THRESHOLD_PERCENT);
        let old = self
            .group(group)
            .ok_or("unknown group")?
            .guardrails
            .compact_context_threshold_percent;
        if applied == old {
            return Ok(applied);
        }
        self.persist_guardrail_u64(group, "compact_context_threshold_percent", applied as u64)?;
        self.groups
            .lock_safe()
            .get_mut(group)
            .ok_or("unknown group")?
            .guardrails
            .compact_context_threshold_percent = applied;
        self.audit(group, "human", "compact-context-threshold-set",
            json!({ "from": old, "to": applied }));
        Ok(applied)
    }

    /// Set a live group's compact-nudge min-context floor on the fly
    /// (benchtest finding). Always sets an EXPLICIT value (`Some`) — this
    /// setter can never restore the tri-state's `None`/unset (the smart
    /// default); once a human has touched the control, that IS the explicit
    /// choice, including `0` (explicitly disabled — heuristic fires on the
    /// lull alone, no context check). Only capped at 100. Written to the
    /// in-memory guardrail and persisted, then audited. Returns the applied
    /// (clamped) value. Never gates `request_compact` itself — see
    /// `compact_nudge_context_floor_met`'s doc.
    pub fn set_compact_nudge_min_context_percent(&self, group: &GroupId, percent: u32) -> Result<u32, String> {
        let _io = self.group_file_io.lock_safe();
        let applied = percent.min(100);
        let old = self
            .group(group)
            .ok_or("unknown group")?
            .guardrails
            .compact_nudge_min_context_percent;
        if old == Some(applied) {
            return Ok(applied);
        }
        self.persist_guardrail_u64(group, "compact_nudge_min_context_percent", applied as u64)?;
        self.groups
            .lock_safe()
            .get_mut(group)
            .ok_or("unknown group")?
            .guardrails
            .compact_nudge_min_context_percent = Some(applied);
        self.audit(group, "human", "compact-nudge-min-context-percent-set",
            json!({ "from": old, "to": applied }));
        Ok(applied)
    }

    /// The autonomous-mode state the frontend panel reads to render its toggle
    /// rows, the budget meter, and the idle-tick countdown (#83, W2's slice). One
    /// call for the whole panel: on/off, the auto-merge gate, the token budget +
    /// enable-time anchor + spend-since-enable (`null` spend when off), the
    /// idle-tick window, and — while on — how long the orchestrator has been
    /// output-quiet and how many seconds until the next tick is eligible (so the
    /// panel can show "next tick in ~Xm" instead of the tick being invisible).
    pub fn autonomy_state(&self, group: &GroupId) -> Value {
        self.autonomy_state_within(group, Duration::ZERO)
    }

    /// [`Self::autonomy_state`], with the usage memo's freshness window as a
    /// parameter (#743 S4b) — the only figure it affects is
    /// `spend_since_enable_tokens`, which is derived from the group's lifetime
    /// token total. `Duration::ZERO` keeps a live read.
    #[doc(hidden)] // pub for integration tests
    pub fn autonomy_state_within(&self, group: &GroupId, usage_max_age: Duration) -> Value {
        let on = self.is_autonomous(group);
        let rails = self.group(group).map(|g| g.guardrails);
        let budget = rails.as_ref().map(|g| g.autonomy_budget_tokens).unwrap_or(0);
        let idle_tick_minutes = rails
            .as_ref()
            .map(|g| g.idle_tick_minutes)
            .filter(|&m| m > 0)
            .unwrap_or(DEFAULT_IDLE_TICK_MINUTES);
        let anchor = if on { self.autonomy_anchor(group) } else { 0 };
        let spend = on.then(|| {
            self.group_token_total_within(group, usage_max_age)
                .saturating_sub(anchor)
        });
        // `suspended` is meaningful only while OFF: true iff the budget enforcer
        // (not the user) flipped it off, so the UI can distinguish "budget spent —
        // raise it or re-enable" from a plain user toggle-off.
        let suspended = !on && self.autonomy_suspended(group);
        let floor = rails
            .as_ref()
            .map(|g| g.idle_activity_floor_bytes)
            .filter(|&b| b > 0)
            .unwrap_or(DEFAULT_IDLE_ACTIVITY_FLOOR_BYTES);

        // Idle-tick observability (while on). `tick_status` is the honest reason the
        // UI renders; `eligible_in_secs` is a *real* countdown only when one exists
        // — never a lying 0 that hits zero while a non-time gate holds the tick
        // (rev-59). Statuses:
        //   "off"                 — autonomous off / no live orchestrator (null secs)
        //   "starting"            — orchestrator still booting (not Running); the tick
        //                           only considers Running panes, so no timer yet (null)
        //   "paused"              — group paused: delivery suppressed, so NO tick fires
        //                           however long the clock runs; secs = null (not a lie)
        //   "counting_down"       — quiet clock < window; secs = time left in window
        //   "eligible"            — window met, latch clear, under cap; secs = 0
        //                           (fires within one loop pass, ≤ IDLE_TICK_INTERVAL)
        //   "waiting_for_activity"— already ticked this window (latch set); no timer
        //                           gates it — it waits for the orchestrator to emit
        //                           output — so secs = null, NOT 0
        //   "rate_capped"         — the per-hour cap is full; secs = time until the
        //                           oldest tick ages out of the window (a real timer)
        let (quiet_secs, eligible_in_secs, tick_status) = if on {
            self.idle_tick_observability(group, idle_tick_minutes)
        } else {
            (None, None, "off")
        };
        json!({
            "autonomous": on,
            "auto_merge": self.is_auto_merge(group),
            "auto_release": self.is_auto_release(group),
            // Full autonomy (#778) and its goal. The goal is null whenever the mode
            // is off — the marker that holds it only exists while it is on — so the
            // panel never renders a goal that isn't in force.
            "full_autonomy": self.is_full_autonomy(group),
            "full_autonomy_goal": self.full_autonomy_goal(group),
            // The group's resolved veto spelling (#778), so the panel's own
            // full-autonomy help and header chip name the label THIS group's
            // poller honors. Reported unconditionally (not only while the mode
            // is on, unlike the goal): the help text explains what the toggle
            // WOULD do, and it has to be true before the human flips it.
            "hold_label": self.hold_label_of(group),
            "dangerous_mode": self.is_dangerous_mode(group),
            "budget_tokens": budget,
            "budget_anchor_tokens": anchor,
            "spend_since_enable_tokens": spend,
            "suspended": suspended,
            "idle_tick_minutes": idle_tick_minutes,
            "idle_activity_floor_bytes": floor,
            "quiet_secs": quiet_secs,
            "eligible_in_secs": eligible_in_secs,
            "tick_status": tick_status,
        })
    }

    /// Compute the honest idle-tick countdown for `autonomy_state` (#83, rev-59):
    /// `(quiet_secs, eligible_in_secs, tick_status)`, folding the quiet clock, the
    /// one-notice latch, and the per-hour cap so a rendered `eligible_in_secs`
    /// never hits 0 while a non-time gate still holds the tick. See the caller's
    /// status table. `now_ms` is read once here so the arithmetic is self-contained.
    fn idle_tick_observability(
        &self,
        group: &GroupId,
        idle_tick_minutes: u32,
    ) -> (Option<u64>, Option<u64>, &'static str) {
        let now = now_ms();
        // The orchestrator's status + quiet clock + latch (maintained by the loop).
        let orch = self
            .agents
            .lock_safe()
            .values()
            .find(|a| a.group == group && a.role == Role::Orchestrator
                && a.status != AgentStatus::Dead)
            .map(|a| (a.status, a.last_progress_ms, a.idle_tick_notified));
        let Some((status, since, latched)) = orch else {
            return (None, None, "off"); // no live orchestrator → no meter
        };
        // Transient boot: `idle_tick_tick` only ticks a Running orchestrator, so a
        // Starting one has no live countdown yet — report it honestly, not a timer.
        if status != AgentStatus::Running {
            return (None, None, "starting");
        }
        let quiet = now.saturating_sub(since) / 1000;
        let window = idle_tick_minutes as u64 * 60;
        let quiet_remaining = window.saturating_sub(quiet);

        // Paused: `idle_tick_tick` skips paused groups wholesale (delivery is
        // suppressed there), so NO tick fires however long the quiet clock runs.
        // Mirror the latch branch — the quiet clock is still live but there is no
        // countdown — so the panel never shows a ticking timer while paused.
        if self.is_paused(group) {
            return (Some(quiet), None, "paused");
        }
        // Latch: already ticked this window — no timer counts down to the next
        // (it waits for the orchestrator to produce output), so report it as such
        // rather than a false 0.
        if latched {
            return (Some(quiet), None, "waiting_for_activity");
        }
        // Per-hour cap: if the trailing-hour tick count is at the cap, the next
        // tick is gated until the oldest ages out — a real timer, so fold it in.
        let cap_remaining = {
            let times = self.idle_tick_times.lock_safe();
            let recent: Vec<u64> = times
                .get(group)
                .map(|v| v.iter().copied().filter(|&t| now.saturating_sub(t) < SPAWN_RATE_WINDOW_MS).collect())
                .unwrap_or_default();
            if recent.len() as u32 >= MAX_IDLE_TICKS_PER_HOUR {
                let oldest = recent.iter().copied().min().unwrap_or(now);
                Some((oldest + SPAWN_RATE_WINDOW_MS).saturating_sub(now) / 1000)
            } else {
                None
            }
        };
        if let Some(cap_wait) = cap_remaining {
            // Eligible only once BOTH the quiet window and a cap slot are satisfied.
            return (Some(quiet), Some(quiet_remaining.max(cap_wait)), "rate_capped");
        }
        if quiet_remaining > 0 {
            (Some(quiet), Some(quiet_remaining), "counting_down")
        } else {
            (Some(quiet), Some(0), "eligible")
        }
    }

    /// Clear the orchestrator's idle-tick anti-nag latch for a group (e.g. on
    /// disable, so a later re-enable starts fresh).
    fn clear_idle_tick_latch(&self, group: &GroupId) {
        for a in self.agents.lock_safe().values_mut() {
            if a.group == group && a.role == Role::Orchestrator {
                a.idle_tick_notified = false;
            }
        }
    }

    /// #255/#259: emit the same advisory `max-agents-below-minimum` /
    /// `max-agents-below-recommended` audit record regardless of which path
    /// found the cap too low for a workflow's derived [`workflow::CapacityRecommendation`]
    /// — launch/resume (`create_group`, once, at `workflow-loaded`/on resume) or
    /// the live stepper (`set_max_agents`, every time a human lowers the cap).
    /// Same two-tier logic, same audit shape, so the trail reads the same no
    /// matter which path wrote it: below `minimum`, the roster's merge gate
    /// plus a worker can never all be live at once (hard — it actively
    /// thrashes); at-or-above `minimum` but below `recommended`, every review
    /// round completes but named tiers can never run alongside one (soft).
    /// Advisory only — never refuses or clamps `max_agents`.
    pub(in crate::orchestration) fn audit_capacity_shortfall(
        &self,
        group: &GroupId,
        blocks: &[workflow::Block],
        max_agents: u32,
        rec: &workflow::CapacityRecommendation,
    ) {
        if max_agents < rec.minimum {
            self.audit(group, brand::AUDIT_ACTOR, "max-agents-below-minimum", json!({
                "max_agents": max_agents,
                "minimum": rec.minimum,
                "recommended": rec.recommended,
                "note": format!(
                    "max_agents ({}) is below this workflow's minimum ({}) — its merge \
                     gate plus a worker can never all be live at once without evicting a \
                     live agent to make room.",
                    max_agents, rec.minimum,
                ),
            }));
        } else if max_agents < rec.recommended {
            let extras = workflow::extra_tiers(blocks, rec.reviewers_needed);
            self.audit(group, brand::AUDIT_ACTOR, "max-agents-below-recommended", json!({
                "max_agents": max_agents,
                "minimum": rec.minimum,
                "recommended": rec.recommended,
                "extra_tiers": extras,
                "note": format!(
                    "max_agents ({}) covers one review round (minimum {}) but not this \
                     workflow's full roster (recommended {}) — {} can never be live \
                     alongside a review round.",
                    max_agents, rec.minimum, rec.recommended,
                    if extras.is_empty() {
                        "some of its declared tiers".to_string()
                    } else {
                        workflow::join_with_and(&extras)
                    },
                ),
            }));
        }
    }

    /// Adjust a live group's max live-agent cap on the fly. Bounds are the
    /// launcher's `1..=MAX_AGENTS_CEILING`. The new value is written to the
    /// in-memory guardrail (which `spawn_agent` reads fresh on every spawn, so
    /// it takes effect immediately — nothing caches the creation-time number)
    /// and persisted to group.json so a restart keeps it, then the change is
    /// audited (per-click) and the orchestrator notice is *debounced* — a burst
    /// of stepper clicks coalesces into one re-plan prompt (#79). Lowering the cap below
    /// the current live count kills nobody: new spawns are simply refused until
    /// attrition brings the count back under the cap. Returns the new value.
    /// A no-op change (`n` already the current cap) short-circuits without a
    /// second write, audit, or notice. `actor` records who made the change.
    pub fn set_max_agents(&self, group: &GroupId, n: u32, actor: &str) -> Result<u32, String> {
        if !(1..=MAX_AGENTS_CEILING).contains(&n) {
            return Err(format!("max agents must be between 1 and {MAX_AGENTS_CEILING}"));
        }
        let _io = self.group_file_io.lock_safe();
        let info = self.group(group).ok_or("unknown group")?;
        let old = info.guardrails.max_agents;
        if n == old {
            return Ok(n);
        }
        // Persist first: a failed disk write must leave the in-memory cap (the
        // value enforcement reads) unchanged, so the two never disagree.
        self.persist_max_agents(group, n)?;
        self.groups
            .lock_safe()
            .get_mut(group)
            .ok_or("unknown group")?
            .guardrails
            .max_agents = n;
        self.audit(group, actor, "max-agents-set", json!({ "from": old, "to": n }));
        // #259: `set_max_agents` is the live-cap stepper's backend — the same
        // knob a workflow's pinned capacity recommendation (#255) is checked
        // against once at launch/resume. A human lowering the cap mid-session
        // below the roster's structural minimum is exactly the thrash #255
        // exists to warn about; without this, that warning goes silent the
        // moment a session outgrows it live instead of at launch/resume.
        // Gated the same way the launch/resume path gates `capacity` (registry/groups.rs
        // `create_group`): only a declared, custom workflow has a structural
        // minimum to re-check against.
        if info.guardrails.advanced_orchestrator && workflow::roster_is_custom(&info.guardrails.blocks) {
            let rec = workflow::recommend_capacity(&info.guardrails.blocks, self.merge_gate(group).as_ref());
            self.audit_capacity_shortfall(group, &info.guardrails.blocks, n, &rec);
        }
        // The orchestrator's kickoff prompt already rendered the old
        // {{MAX_AGENTS}} into static text; it needs the new ceiling to re-plan.
        // But rapid-clicking the stepper (4→3→2) would otherwise fire a notice
        // per click, each a real prompt that burns orchestrator tokens/time
        // (#79). So debounce: record the change here (carrying the burst's
        // original `from`) and let `flush_due_max_notices` deliver ONE notice —
        // 4→2, not 4→3 then 3→2 — once the clicks stop. Enforcement/persist
        // above and the audit are per-click and immediate; only the notice waits.
        record_max_notice(
            &mut self.pending_max_notice.lock_safe(),
            group,
            old,
            n,
            now_ms(),
            MAX_NOTICE_DEBOUNCE,
        );
        Ok(n)
    }

    /// Rewrite only `guardrails.max_agents` in group.json, preserving every
    /// other stored field (created_ms, the other guardrails, and anything a
    /// later feature adds). Patching the parsed JSON in place — rather than
    /// reserializing a full GroupInfo — keeps this additive and rebase-clean.
    fn persist_max_agents(&self, group: &GroupId, n: u32) -> Result<(), String> {
        let dir = self.group_dir(group);
        let path = dir.join("group.json");
        let mut v: Value = serde_json::from_str(&fs::read_to_string(&path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        // Guard the indexing so a corrupt-but-valid-JSON file (e.g. a `null`
        // root) fails soft instead of panicking on assignment.
        let obj = v.as_object_mut().ok_or("group.json root is not a JSON object")?;
        match obj.get_mut("guardrails").and_then(Value::as_object_mut) {
            Some(guard) => {
                guard.insert("max_agents".into(), json!(n));
            }
            None => {
                obj.insert("guardrails".into(), json!({ "max_agents": n }));
            }
        }
        // Crash-safe write: group.json is identity-critical — a half-written
        // file breaks the rejoin path ("group.json is missing") — so never
        // expose a truncated version (#133).
        let body = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
        atomic_write(&path, body.as_bytes()).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Deliver any debounced cap-change notice whose quiet window has elapsed
    /// (#79). Called on a timer by `start_max_notice_flusher`; `now` is injected
    /// so tests drive the coalescing deterministically without sleeping out the
    /// debounce. A burst that netted to a no-op is dropped inside
    /// `take_due_max_notices` and never reaches the orchestrator.
    #[doc(hidden)] // pub for integration tests
    pub fn flush_due_max_notices(&self, now: u64) {
        let Some(_tick) = self.tick_gate("flush_due_max_notices") else { return () };
        let due = take_due_max_notices(&mut self.pending_max_notice.lock_safe(), now);
        for (group, from, to) in due {
            // Best-effort, like the exit notice: a dead/paused orchestrator
            // just misses it. Delivery is intentionally outside the lock.
            let _ = self.deliver_to_orchestrator(&group, &max_agents_notice(from, to), brand::AUDIT_ACTOR);
        }
    }

    /// Record a spawn against the group's rolling-hour window and report
    /// whether the spawn-rate guardrail is now exceeded. Checks and records
    /// under one lock so concurrent spawns can't both slip past the cap.
    pub(in crate::orchestration) fn check_and_record_spawn(&self, group: &GroupId, limit: u32) -> Result<(), String> {
        let now = now_ms();
        let mut all = self.spawn_times.lock_safe();
        let times = all.entry(group.clone()).or_default();
        times.retain(|&t| now.saturating_sub(t) < SPAWN_RATE_WINDOW_MS);
        if spawn_rate_exceeded(times, now, limit, SPAWN_RATE_WINDOW_MS) {
            return Err(format!(
                "guardrail: spawn-rate limit reached ({limit} spawns/hour). Wait, or reuse an idle agent instead of spawning a new one."
            ));
        }
        times.push(now);
        Ok(())
    }
}
