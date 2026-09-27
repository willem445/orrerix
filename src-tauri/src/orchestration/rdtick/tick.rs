//! The tick itself (§2.4): which group this wake services, the once-only
//! disabled-driver announcement, `rd_driver_tick` and `rd_drive_group_with`,
//! the notice flush with §5.2's retention, and the gate facts (§4).
//!
//! Design note: `docs/design/review-driver.md`. Part of `rdtick/`, the driver's
//! registry wiring, which `tests/reviewdrive.rs` scans whole (see `mod.rs`).

use super::*;

impl OrchRegistry {
    /// The one group this wake will service, or `None`.
    ///
    /// `next_mq_group`'s three filters, cheapest first, each of them a reason
    /// not to spend a subprocess: not inside a backoff window, has a
    /// `review_drives.json` at all (a **file check** rather than a parse, so an
    /// unreadable file still reaches the driver's own loud handling), and the
    /// repo declares the driver.
    fn next_rd_group(&self, now: u64) -> Option<GroupId> {
        let all: Vec<GroupId> = self.groups.lock_safe().keys().cloned().collect();
        let service = self.rd_service_ms.lock_safe().clone();
        let mut due: Vec<(u64, GroupId)> = all
            .into_iter()
            .filter(|g| service.get(g).map(|t| now >= *t).unwrap_or(true))
            .filter(|g| reviewdrive::state_path(&self.group_dir(g)).exists())
            .filter(|g| self.driver_enabled(g))
            .map(|g| (service.get(&g).copied().unwrap_or(0), g))
            .collect();
        // Oldest-serviced first; the group id breaks a tie deterministically so
        // two never-serviced groups do not alternate on HashMap iteration order.
        due.sort();
        due.into_iter().next().map(|(_, g)| g)
    }

    /// **Say ONCE that a group's driver is off while its drives sit on disk**
    /// (#3330 ask 2).
    ///
    /// `next_rd_group` skips a group whose driver is off, so neither the tick
    /// nor §2.4's restart reconcile ever runs there: a drive that was live when
    /// the driver went off — `driver.enabled` flipped to false, the workflow
    /// file removed, or the advanced orchestrator toggled off — stays listed by
    /// `review_drive_status` and is never ticked again, with nothing in any
    /// pane. This hands the orchestrator one HOLD-shaped line naming the cause
    /// and the PRs, and writes `rd-disabled-with-drives`.
    ///
    /// **Not for a file that will not parse**: the workflow reload pass owns
    /// that case (`warn_workflow_unparseable`) and already names the drives, so
    /// announcing it here too would be the second line for one fact.
    ///
    /// **Once per (cause, PRs)**: the row on the transition, the line retried
    /// until it lands, then latched; a group whose driver is back on, or whose
    /// drives are all finished, clears its latch. No `gh` call — a disabled
    /// group must cost nothing it did not cost before beyond this read.
    fn rd_announce_disabled_drives(&self) {
        let all: Vec<GroupId> = self.groups.lock_safe().keys().cloned().collect();
        for g in all {
            if !reviewdrive::state_path(&self.group_dir(&g)).exists() {
                self.rd_disabled_warned.lock_safe().remove(&g);
                continue;
            }
            let Some(info) = self.group(&g) else { continue };
            let cause = if !info.guardrails.advanced_orchestrator {
                "the advanced orchestrator is off for this group"
            } else {
                match super::load_active_workflow(&info.repo, &info.guardrails) {
                    Ok(Some(wf)) if wf.driver.enabled => {
                        self.rd_disabled_warned.lock_safe().remove(&g);
                        continue;
                    }
                    Ok(Some(_)) => "the workflow file does not set driver.enabled: true",
                    Ok(None) => "the group's workflow file is gone",
                    // The reload pass's to say (see above).
                    Err(_) => continue,
                }
            };
            let prs = match self.rd_live_drive_prs(&g) {
                Some(prs) if !prs.is_empty() => prs,
                // Nothing left to hold still, or a record orrerix cannot read —
                // which the status tool and every driver call already refuse
                // loudly as `rd-state-unreadable`.
                _ => {
                    self.rd_disabled_warned.lock_safe().remove(&g);
                    continue;
                }
            };
            let delivered = {
                let mut warned = self.rd_disabled_warned.lock_safe();
                match warned.get(&g) {
                    Some((c, p, d)) if c == cause && *p == prs => Some(*d),
                    _ => {
                        warned.insert(g.clone(), (cause.to_string(), prs.clone(), false));
                        None
                    }
                }
            };
            if delivered == Some(true) {
                continue;
            }
            if delivered.is_none() {
                self.rd_audit(
                    &g,
                    "",
                    rddrive::audit_action::DISABLED_WITH_DRIVES,
                    json!({ "cause": cause, "prs": prs }),
                );
            }
            let text = format!(
                "HOLD review driver off with drives on disk: {cause}, so these drives will not be \
                 ticked until it is back on: {}. Turn the driver back on to resume them \
                 (cancel_review_drive also needs it on).",
                prs.iter().map(|p| format!("PR #{p}")).collect::<Vec<_>>().join(", ")
            );
            if self.deliver_to_orchestrator(&g, &text, brand::AUDIT_ACTOR).is_ok() {
                if let Some(entry) = self.rd_disabled_warned.lock_safe().get_mut(&g) {
                    entry.2 = true;
                }
            }
        }
    }

    /// One driver step, for at most one group — the fifth step in
    /// `gh_poll_tick` (§2.4), clock injected.
    ///
    /// **At most one group per wake, oldest-serviced first**, which is
    /// `mq_driver_tick`'s bound and structural rather than a counter: this loop
    /// is shared with every `notify_when` watch in the fleet, and a driver that
    /// serviced N groups on one wake would put N groups' worth of `gh`
    /// round-trips inside one tick.
    pub fn rd_driver_tick(&self, now: u64) -> Option<GroupId> {
        // Before the pick, because the pick is exactly what skips these groups.
        self.rd_announce_disabled_drives();
        let group = self.next_rd_group(now)?;
        let injected = self.rd_runner_override.lock_safe().clone();
        match injected {
            Some(r) => {
                self.rd_drive_group_with(&group, r.as_ref(), now);
            }
            None => {
                let repo = self.group(&group).map(|g| g.repo)?;
                let runner = rddrive::runner_for(std::path::Path::new(&repo));
                self.rd_drive_group_with(&group, &runner, now);
            }
        }
        Some(group)
    }

    /// Drive one group with the `gh` seam injected — the seam
    /// `tests/reviewdrive.rs` uses to exercise the whole production path
    /// without spawning a child (CLAUDE.md constraint 3).
    ///
    /// **This is wiring, not logic.** Every state decision is
    /// `reviewdrive::decide`'s and every gate answer is
    /// `mergeq::recheck_gate`'s. What lives here is what only the registry can
    /// do: resolve the policy, the gate and the verdict files, hold the state
    /// lock across the read-modify-write, perform the spawns and resumes, emit
    /// the audit events, and deliver the notices.
    ///
    /// **The lock spans the spawn, and a DELEGATE delivery; never a notice to
    /// the orchestrator.** That is §2.4's prescription with its one ambiguity
    /// resolved rather than ignored. §2.4 wants the load-decide-store to span
    /// the spawn, because a `drive_review` landing inside that window would read
    /// the pre-spawn file and write it back, erasing the entry; #467/#468 want
    /// no registry lock held across a delivery, because a delivery enqueues and
    /// an enqueue re-enters registry locks. Both hold here, on a property of the
    /// LOCK rather than a count of its callers: **no site that takes
    /// `rd_state_lock` is reachable from a pane delivery**, so a spawn's own
    /// kickoff cannot cycle back onto it.
    ///
    /// Since #1960 a hand-back or lane brief may be typed into a live idle pane
    /// instead of spawning one ([`rd_reuse_pane`](Self::rd_reuse_pane)), so the
    /// lock now spans a `deliver_prompt` DIRECTLY rather than only one a spawn
    /// performs. That is the same delivery under the same property — the design
    /// note already anticipated it ("a spawn delivers its own kickoff, so 'span
    /// the spawn' reads as 'span a delivery'") — and it introduces no new lock
    /// ORDER either: `spawn_agent_bound` already takes `agents` under this lock,
    /// which is the only lock `deliver_prompt` needs before its own queue.
    ///
    /// What stays OUTSIDE is unchanged: the orchestrator notices this produces
    /// are delivered below, and so is #1959's kick-back into a worker's pane —
    /// not because either is unsafe under the lock, but because both are
    /// products of a completed step rather than part of one.
    ///
    /// The sites today are the tick (here and at the prune), the restart
    /// reconcile, the three MCP tools of §5.1, and the two interception helpers
    /// `rd_owner` / `rd_consume` — eight acquisitions across seven functions.
    /// The last two are the ones worth naming: they run on a delegate's own tool
    /// call, which is a later turn scheduled by the runtime, never a frame the
    /// delivery itself pushes, and both drop the lock before auditing. A ninth
    /// caller owes this argument again rather than inheriting it.
    #[doc(hidden)] // pub for integration tests
    pub fn rd_drive_group_with(
        &self,
        group: &GroupId,
        runner: &dyn rddrive::RdRunner,
        now: u64,
    ) -> RdDriveReport {
        let mut report = RdDriveReport::default();
        let (enabled, limits) = self.driver_policy(group);
        if !enabled {
            // Byte-for-byte unchanged behaviour with no `driver:` block (§5.3).
            // Checked here as well as in `next_rd_group` because this method is
            // the test seam, and a seam that skipped the product's own opt-in
            // would be testing something the product cannot do.
            return report;
        }
        // Reconcile owes its cancellations onto their entries; the flush below
        // is what delivers them (#1857).
        self.rd_reconcile_with(group, runner, now);
        let dir = self.group_dir(group);
        let mut outs: Vec<RdOut> = Vec::new();
        // Whether this tick's decisions actually reached disk. A signal is a
        // ONE-SHOT, and consuming one for a transition the next restart forgets
        // is the same shape as latching a once-only flag on a read that failed:
        // the arc is rolled back by the failed write, the delegate's event is
        // gone, and the drive never learns of it again. CLAUDE.md's
        // multi-tenant-store rule is the settled form — a failed read declines
        // rather than defaulting, and is not latched, so one transient rejection
        // does not disable the mechanism. Same rule, applied to a failed WRITE
        // consuming an event.
        let mut persisted = true;
        {
            let _state_guard = self.rd_state_lock.lock_safe();
            let mut state = match reviewdrive::load_state(&dir) {
                Ok(s) => s,
                Err(e) => {
                    // §2.4: refuse the tick, audit, back off. Never repaired and
                    // never deleted — a record orrerix will not read is one
                    // whose live drives it cannot account for, and guessing
                    // would resume a drive against state nobody wrote.
                    self.audit(
                        group,
                        brand::AUDIT_ACTOR,
                        rddrive::audit_action::STATE_UNREADABLE,
                        json!({ "detail": format!("{e:?}") }),
                    );
                    self.rd_defer(group, now.saturating_add(rddrive::RD_BACKOFF_MS));
                    report.backoff = true;
                    report.refused = Some(rddrive::audit_action::STATE_UNREADABLE);
                    return report;
                }
            };
            // One tick, one answer per base branch — see `rd_base_green`.
            let mut base_green: std::collections::HashMap<String, Option<bool>> =
                std::collections::HashMap::new();
            let prs: Vec<u64> = state.entries.iter().map(|e| e.pr).collect();
            for pr in prs {
                if let Some(o) =
                    self.rd_step_entry(group, runner, &mut state, pr, &limits, now, &mut base_green)
                {
                    outs.push(o);
                }
            }
            if outs.iter().any(|o| o.changed) {
                if let Err(e) = reviewdrive::store_state(&dir, &state) {
                    persisted = false;
                    // A transition the next restart forgets is not a transition.
                    // Audited loudly and backed off; reconcile fixes the record
                    // on the next start.
                    self.audit(
                        group,
                        brand::AUDIT_ACTOR,
                        rddrive::audit_action::STATE_UNREADABLE,
                        json!({ "reason": "review_drives.json could not be written",
                                "detail": e }),
                    );
                    report.backoff = true;
                }
            }
        }
        for o in &outs {
            for (action, detail) in &o.audits {
                self.rd_audit(group, &o.on_behalf_of, action, detail.clone());
            }
            for (b, a) in &o.lanes_opened {
                report.lanes_opened.push((o.pr, b.clone(), a.clone()));
            }
            if let Some(a) = &o.handback {
                report.handbacks.push((o.pr, a.clone()));
            }
            // #2501: already performed, under the state lock — see the release
            // loop in `rd_step_entry` for why the kill cannot be deferred to
            // here the way a delivery is.
            for (role, agent) in &o.releases {
                report.released.push((o.pr, role.clone(), agent.clone()));
            }
            // #1959: into the WORKER's pane, not the orchestrator's — the whole
            // point is that this costs the orchestrator no turn. Outside the
            // state lock, like the notices below: a delivery is an enqueue and
            // an enqueue re-enters registry locks.
            if let Some((agent, text)) = &o.kickback {
                let _ = self.deliver_prompt(agent, text, brand::AUDIT_ACTOR, Delivery::MidSession);
                report.kickbacks.push((o.pr, agent.clone()));
            }
            if let Some((to, _)) = o.advanced {
                report.advanced.push((o.pr, to));
            }
            // A HOLD's notice, delivered directly. A failure here still loses
            // the line, and that is argued where the split is made — in
            // `rd_step_entry`, at the exits: a parked entry is never pruned, so
            // the drive survives and `review_drive_status()` still carries it. A
            // terminal exit's notice does not come through here at all; it is
            // owed on the entry and delivered by the flush below (#1857).
            let mut all_landed = !o.notices.is_empty();
            for n in &o.notices {
                all_landed &= self.deliver_to_orchestrator(group, n, brand::AUDIT_ACTOR).is_ok();
                self.rd_task_note(group, o.pr, n);
                report.notices.push(n.clone());
            }
            // #3367 round-3 residual: the report a hold line carried is spent
            // only if that line reached the pane — and only if the tick's write
            // did, since an entry the next restart forgets has no report to
            // clear that this tick's decision could vouch for.
            if o.report_folded && all_landed && persisted {
                self.rd_clear_auto_report(&dir, o.pr);
            }
            // #2811 S5b: the per-drive wording lands on the BOARD only. The
            // orchestrator's copy is the aggregated line below.
            if let Some(n) = &o.provider_note {
                self.rd_task_note(group, o.pr, n);
            }
            // Only once the arc that consumed it is durable — see `persisted`.
            if o.clear_signal && persisted {
                self.rd_signals.lock_safe().remove(&(group.clone(), o.pr));
            }
            if o.backoff {
                report.backoff = true;
            }
        }
        // #3367 item 5: the CLEAN case's enqueue. Here, and only here: after
        // the per-out audits (so `rd-clean` follows the `rd-satisfied` it sits
        // beside), after the write (so `queue_merge_with`'s §8.1 check reads
        // this drive as already terminal) and outside `rd_state_lock` (it takes
        // `mq_state_lock` and spends `gh` round trips). A write that failed
        // enqueues nothing — the satisfied arc did not happen as far as the next
        // restart knows, and a queue entry for it would outlive that.
        //
        // The queue decides everything a merge could turn on, exactly as for
        // an orchestrator's own `queue_merge`: the gate is RE-ENFORCED from the
        // verdict files and the live PR, the default branch is refused
        // structurally (merge-queue.md §7), and `git` is not even reachable —
        // `with_git_denied` hands it the driver's gh-only runner. So the one
        // thing the driver adds is the CALL; what may be queued is still the
        // queue's to say, and its answer is appended to the notice verbatim.
        if persisted {
            for o in &outs {
                let Some(head) = &o.clean_enqueue else { continue };
                let answer = rddrive::with_git_denied(runner, |mq| {
                    self.queue_merge_with(group, o.pr, None, mq)
                });
                // REPLACE, not append (#3388 review round 1): the owed clause
                // says what the driver will do, and from here on it has done
                // it, so the delivered line says what the queue answered.
                self.rd_amend_owed_notice(
                    &dir,
                    o.pr,
                    &rddrive::clean_clause(rddrive::CleanRoute::Queue),
                    &rddrive::clean_queue_submitted(&answer),
                );
                self.rd_audit(
                    group,
                    &o.on_behalf_of,
                    rddrive::audit_action::CLEAN,
                    json!({ "pr": o.pr, "head": head, "route": "queue", "queue_merge": answer }),
                );
            }
        } else {
            // #3388 review round 1: the write FAILED, so every terminal notice
            // this tick owed exists only on the in-memory entry the failed
            // write discarded — the flush below reads owed notices from disk
            // and will never see it, and the `rd-state-unreadable` row above is
            // on the audit log rather than in the pane. So it is delivered
            // here, directly, saying it was not recorded; the queue clause says
            // nothing was submitted, because nothing was. Fire-and-forget like a
            // hold's line: the next tick that can write re-decides the drive
            // and owes its own notice durably.
            for o in &outs {
                let Some(text) = &o.owed_text else { continue };
                let mut text = text.clone();
                if o.clean_enqueue.is_some() {
                    text = text.replacen(
                        &rddrive::clean_clause(rddrive::CleanRoute::Queue),
                        &rddrive::clean_queue_unrecorded(),
                        1,
                    );
                }
                text.push_str(rddrive::UNRECORDED_SUFFIX);
                let _ = self.deliver_to_orchestrator(group, &text, brand::AUDIT_ACTOR);
                self.rd_task_note(group, o.pr, &text);
                report.notices.push(text);
            }
        }
        // #2811 S5b — ONE notice per provider, however many drives it stopped.
        //
        // Built here rather than per entry because the fact it states is about
        // the SET: "4 drives held" is not a thing any single drive knows. The
        // per-drive `rd-held` rows are already written above, so the record is
        // complete either way and this is purely what reaches the pane.
        //
        // Sorted and de-duplicated so the line is stable tick to tick: an
        // unstable ordering would make the same hold read as new information
        // every tick, which is #3040 N1 arriving from the other end.
        //
        // **Emitted on a NEW hold, but counted over every drive still held**
        // (#3191 review finding 4). Those are two different questions and an
        // earlier revision answered both with this tick's arrivals, which
        // under-counts an outage that arrives staggered: two drives on one
        // provider holding on different ticks each produced a line reading
        // "1 drive held", while two were held. Each line was true about what
        // had just happened and false about the thing an orchestrator reads it
        // for — how much of the group is stopped.
        //
        // So the trigger stays per-tick (a line only when something newly
        // parked, or a repeat would fire every three seconds for the whole
        // outage) and the CONTENT is the live set, re-derived from the state
        // file: every entry parked on `provider-limit` whose own panes still
        // show that provider. A drive whose panes recovered drops out of the
        // list even though it is still parked, which is the honest reading —
        // it is held awaiting a `drive_review`, not held by the outage.
        {
            let mut by_provider: std::collections::BTreeMap<String, Vec<u64>> =
                std::collections::BTreeMap::new();
            let newly: std::collections::BTreeSet<String> =
                outs.iter().filter_map(|o| o.provider_limited.clone()).collect();
            if !newly.is_empty() {
                if let Ok(live) = reviewdrive::load_state(&self.group_dir(group)) {
                    for e in &live.entries {
                        if e.state() != reviewdrive::DriveState::Held
                            || e.held_reason != Some(reviewdrive::HeldReason::ProviderLimit)
                        {
                            continue;
                        }
                        let still = e
                            .owned_panes()
                            .into_iter()
                            .find_map(|(a, _)| self.provider_limit_for_agent(&a));
                        if let Some(p) = still.filter(|p| newly.contains(p)) {
                            by_provider.entry(p).or_default().push(e.pr);
                        }
                    }
                }
            }
            // Fail-safe: if the state could not be re-read, say what THIS tick
            // saw rather than nothing. An under-count beats silence, and the
            // per-drive `rd-held` rows are already on the record either way.
            if by_provider.is_empty() {
                for o in &outs {
                    if let Some(p) = &o.provider_limited {
                        by_provider.entry(p.clone()).or_default().push(o.pr);
                    }
                }
            }
            for (provider, mut prs) in by_provider {
                prs.sort_unstable();
                prs.dedup();
                let n = rddrive::provider_limit_notice(&provider, &prs);
                let _ = self.deliver_to_orchestrator(group, &n, brand::AUDIT_ACTOR);
                self.rd_audit(
                    group,
                    brand::AUDIT_ACTOR,
                    rddrive::audit_action::PROVIDER_LIMIT,
                    json!({ "provider": provider, "prs": prs, "drives": prs.len() }),
                );
                report.notices.push(n);
            }
        }
        // **§5.2's ordering rule, implemented** (#1857): every owed notice is
        // attempted, and only then does retention run — over a `prune_terminal`
        // that will not drop an entry still owing one.
        //
        // This is a SEPARATE PASS rather than a relaxation of `rd_step_entry`'s
        // `is_parked() || is_terminal()` early return, and the choice is
        // deliberate. That early return is a cost bound §2.4 wants: it declines
        // a resting entry *before* any `gh` read, and a terminal entry admitted
        // into the step path would have to be threaded past `observe_pr`,
        // `rd_gate_facts` and `decide` — none of which has anything to say about
        // a drive that is over — to arrive at a branch that only re-sends a
        // string. A pass that walks the file and delivers what it owes costs no
        // round trip at all, has one condition, and cannot advance anything.
        //
        // It also covers the two producers the step path never sees: reconcile's
        // startup cancellations, and `cancel_review_drive`'s, both of which owe
        // onto the entry for exactly this to pick up.
        //
        // A failed `store_state` above left `persisted == false`, and the flush
        // re-reads from DISK — so an arc that did not reach the file owes
        // nothing here either. That is the same rule the signal clearing uses: a
        // transition the next restart forgets is not a transition, and it must
        // not announce itself as one.
        let flush = self.rd_flush_notices(group, &dir, now);
        report.notices.extend(flush.notices);
        report.notice_undelivered = flush.undelivered;
        report.pruned = flush.pruned;
        if report.backoff {
            self.rd_defer(group, now.saturating_add(rddrive::RD_BACKOFF_MS));
        } else {
            // Not a backoff — just this group's turn in the rotation, so N
            // groups with live drives share the wakes instead of the first one
            // alphabetically taking every tick.
            self.rd_defer(group, now);
        }
        report
    }

    /// **Deliver every owed notice, then run §5.2's retention over what is
    /// left** (#1857) — the pass that makes "a terminal entry is pruned once its
    /// notice has been delivered" a fact about the code rather than a sentence
    /// in a design note.
    ///
    /// # The three phases, and why they are three
    ///
    /// **Read under the lock, attempt with NO lock held, record under the lock
    /// again.** The middle phase is the constraint: #467/#468 make a delivery an
    /// enqueue and an enqueue re-enters registry locks, so holding
    /// `rd_state_lock` across one is the deadlock `rd_drive_group_with`'s own
    /// doc spends a paragraph keeping out. Splitting the file access in two is
    /// the cost of that, and it is safe for the same reason the tick's own
    /// split is: the second phase re-reads, so an entry a concurrent
    /// `drive_review` or `cancel_review_drive` changed in between is read as it
    /// now is, and `entry_mut` simply finds nothing for one that has gone.
    ///
    /// # What a failure at each step does
    ///
    /// A torn file returns empty — the tick above has already audited and backed
    /// off, and refusing to act on a record orrerix cannot read is §2.4's rule,
    /// not a special case. A failed `store_state` returns without reporting
    /// anything pruned, because nothing WAS pruned: the write covers the
    /// delivery marks and the retention together, so the file is exactly as it
    /// was and the next tick re-attempts. The worst case there is a duplicate
    /// line in the pane, which is the direction to fail in — #1857 is about the
    /// other one.
    pub(super) fn rd_flush_notices(&self, group: &GroupId, dir: &std::path::Path, now: u64) -> RdFlush {
        // Phase 1 — what is owed, and whether this is its first attempt.
        let owed: Vec<(u64, String, bool)> = {
            let _state_guard = self.rd_state_lock.lock_safe();
            match reviewdrive::load_state(dir) {
                Ok(s) => s
                    .entries
                    .iter()
                    .filter_map(|e| e.owed_notice().map(|n| (e.pr, n.text.clone(), n.failures == 0)))
                    .collect(),
                Err(_) => return RdFlush::default(),
            }
        };
        if owed.is_empty() {
            // Nothing owed — but retention still has to run, or a delivered
            // entry never leaves the file.
            let r = self.rd_retain(dir, now, &[], &[]);
            return self.rd_audit_retention(group, r);
        }
        // Phase 2 — attempt each, outside the lock.
        let mut out = RdFlush::default();
        let (mut ok, mut failed) = (Vec::new(), Vec::new());
        for (pr, text, first_attempt) in owed {
            if first_attempt {
                // The board row's note is a SECOND record, and it is written on
                // the first attempt rather than on a successful one: the notice
                // whose row most needs to carry it is precisely the one that
                // never reached a pane. Written once, so a retry does not
                // rewrite the row on every tick.
                self.rd_task_note(group, pr, &text);
            }
            // **`Ok` is exactly "the notice is on the pane's queue and a drainer
            // will paste it", and `Err` is exactly "it is not".** Every refusal
            // in `deliver_prompt_as` either answers before the admission
            // (unknown/dead agent, no terminal, a manager pane, a full queue) or
            // WITHDRAWS the admission it made (`withdraw_unprocessable`, for a
            // missing app handle) — so an `Err` never leaves a payload queued,
            // and re-sending on the next tick cannot duplicate a line that is
            // already going to arrive.
            let landed = self.deliver_to_orchestrator(group, &text, brand::AUDIT_ACTOR).is_ok();
            out.notices.push(text);
            if landed {
                ok.push(pr);
            } else {
                failed.push(pr);
            }
        }
        // Phase 3 — record the outcome and prune, in one write.
        let retained = self.rd_audit_retention(group, self.rd_retain(dir, now, &ok, &failed));
        out.undelivered = retained.undelivered;
        out.pruned = retained.pruned;
        out.dropped = retained.dropped;
        out
    }

    /// The audit lines retention owes, emitted **inside the flush** so every
    /// caller of it gets them — the tick, and `cancel_review_drive`, which runs
    /// the same flush rather than a second delivery path of its own (#1857).
    ///
    /// A drop at the ceiling is audited FIRST and separately from the `rd-pruned`
    /// beside it: `rd-notice-dropped` carries the notice text, and once the entry
    /// is gone that line is the only record that could produce it. Auditing both
    /// is not redundant — `rd-pruned` is the retention event and a filter looking
    /// for a lost notice must not have to read every one of them to find the few
    /// that lost anything.
    fn rd_audit_retention(&self, group: &GroupId, flush: RdFlush) -> RdFlush {
        for (pr, text) in &flush.dropped {
            self.rd_audit(
                group,
                "",
                rddrive::audit_action::NOTICE_DROPPED,
                json!({ "pr": pr, "reason": "retention-ceiling", "notice": text }),
            );
        }
        for pr in &flush.pruned {
            self.rd_audit(group, "", rddrive::audit_action::PRUNED, json!({ "pr": pr }));
            self.rd_signals.lock_safe().remove(&(group.clone(), *pr));
            // #2811 S10: the entry is gone, so a mark about it describes
            // nothing. **Defence in depth, and unreachable today** — an entry
            // becomes terminal either by a tick (whose own spend clears the
            // mark on that same tick) or by `cancel_review_drive` (which
            // clears it), and a reconcile-cancel never marks at all. Kept
            // because the spend rule is one edit away from stopping being
            // exhaustive and this is where `rd_signals` is already cleared;
            // NOT pinned by a test, because one would pass either way
            // (measured: run `34156057201`).
            self.rd_forget_restart_mark(group, *pr);
        }
        flush
    }

    /// Phase 3 of [`rd_flush_notices`](OrchRegistry::rd_flush_notices), and the
    /// no-op tick's whole of it: mark the deliveries, then prune.
    ///
    /// One critical section and **one write**, so the two halves cannot come
    /// apart — a file that recorded a delivery and failed to prune, or pruned
    /// and failed to record, is a state neither this function nor the next tick
    /// has a story for.
    fn rd_retain(&self, dir: &std::path::Path, now: u64, ok: &[u64], failed: &[u64]) -> RdFlush {
        let mut out = RdFlush { undelivered: failed.to_vec(), ..RdFlush::default() };
        let _state_guard = self.rd_state_lock.lock_safe();
        let Ok(mut state) = reviewdrive::load_state(dir) else { return out };
        for pr in ok {
            if let Some(e) = state.entry_mut(*pr) {
                e.notice_delivered();
            }
        }
        for pr in failed {
            if let Some(e) = state.entry_mut(*pr) {
                e.notice_delivery_failed();
            }
        }
        let pruned =
            reviewdrive::prune_terminal(&mut state, now, reviewdrive::NOTICE_RETENTION_MS);
        if ok.is_empty() && failed.is_empty() && pruned.is_empty() {
            // Nothing moved. Not writing is not an optimization here: an
            // unconditional rewrite would touch `review_drives.json` on every
            // wake of every group forever, for no change.
            return out;
        }
        if reviewdrive::store_state(dir, &state).is_err() {
            // Nothing landed, so nothing is claimed. Everything that was owed
            // is still owed, including what this pass just delivered — it will
            // be sent again, which is the duplicate-line direction.
            out.undelivered = ok.iter().chain(failed.iter()).copied().collect();
            out.undelivered.sort_unstable();
            return out;
        }
        for p in pruned {
            out.pruned.push(p.pr);
            if let Some(text) = p.undelivered {
                out.dropped.push((p.pr, text));
            }
        }
        out
    }

    /// The gate, read the way §4 requires: **through the readers that already
    /// exist**, never a re-derivation.
    ///
    /// `workflow::route_reviewers` gives the required lane list `review-wait`
    /// walks; `mergeq::recheck_gate` gives `gate-check` its answer, and that is
    /// where `ci-green`, `body-unchanged`, `base-green`, `max_diff_lines` and
    /// routing-unaccountable are each already decided once. Merge-queue §6 is
    /// explicit that a third *implementation* of the gate decision is a defect
    /// rather than an optimization; this makes the driver a third **reader**.
    ///
    /// The one thing decided here is which of `recheck_gate`'s answers is a
    /// `gate-unreadable` hold and which is an ordinary not-satisfied-yet,
    /// because that mapping is the driver's own vocabulary and nothing else
    /// needs it.
    pub(super) fn rd_gate_facts(
        &self,
        group: &GroupId,
        runner: &dyn rddrive::RdRunner,
        pr: u64,
        obs: &rddrive::PrObservation,
        want_gate: bool,
        base_green_memo: &mut std::collections::HashMap<String, Option<bool>>,
    ) -> (reviewdrive::GateOutcome, Option<Vec<reviewdrive::LaneFact>>, Vec<rddrive::LaneNotice>) {
        let spec = match self.merge_queue_gate(group) {
            Ok(s) => s,
            Err(_) => {
                // The file is on disk and orrerix could not read it. §2.2's
                // `gate-unreadable`, and NOT `gate-not-configured` — a wrong
                // label sends the reader somewhere else (#681's posture, which
                // the queue keeps and the driver inherits).
                return (reviewdrive::GateOutcome::Unreadable, None, Vec::new());
            }
        };
        let gate = match &spec {
            mergeq::GateSpec::Declared(g) => g.clone(),
            // Present and unparseable. §2.2 words `gate-unreadable` as an I/O
            // error; a file whose contents will not parse is read no better than
            // one that would not open, the `gh` shim refuses every merge on
            // exactly this state, and announcing satisfied over it is the
            // outcome §3.1 calls "a bypass with better telemetry". The design
            // note's row is widened to say so in this PR rather than the reading
            // being left implicit here.
            mergeq::GateSpec::Malformed => {
                return (reviewdrive::GateOutcome::Unreadable, None, Vec::new())
            }
            // §5.1 makes an absent gate a drive-time DECLINE, so a live entry
            // can only be here if the repo deleted its gate under a running
            // drive. Not satisfied — arc 10 back to `ci-wait`, then the drive's
            // own age bound — which is §8's degradation for a gate that cannot
            // be reached, honestly labelled, rather than a hold reason §2.2's
            // closed enum does not have.
            mergeq::GateSpec::Absent => {
                return (reviewdrive::GateOutcome::Unsatisfied, None, Vec::new())
            }
        };
        // `base-green` is a fact about a BRANCH, so it is fetched at most once
        // per tick and shared by every entry on that base — `mqloop`'s own
        // driver memoizes it per pass for the same reason. Fetched only when
        // this state evaluates the gate AND the gate declares the condition;
        // `None` is the value that refuses, so declining to fetch can only make
        // the gate harder to satisfy.
        let base_green =
            if want_gate && rddrive::declares_base_green(&spec) && !obs.base.is_empty() {
                match base_green_memo.get(&obs.base) {
                    Some(cached) => *cached,
                    None => {
                        let answer = rddrive::base_ci_green(runner, &obs.base);
                        base_green_memo.insert(obs.base.clone(), answer);
                        answer
                    }
                }
            } else {
                None
            };
        let observed = rddrive::gate_observation(runner, pr, obs, &spec, base_green);
        let Some(routed) = workflow::route_reviewers(&gate, observed.changed_files.as_deref())
        else {
            // `route_reviewers` refused: the changed-file list could not be
            // shown complete, so WHICH reviewers are required is unknown.
            // `required_lanes: None` is what `decide` turns into
            // `held(routing-unaccountable)` from every state that reads it.
            return (reviewdrive::GateOutcome::NotEvaluated, None, Vec::new());
        };
        let verdicts = self.verdict_map(group, pr);
        let lanes: Vec<reviewdrive::LaneFact> = routed
            .required
            .iter()
            .map(|b| reviewdrive::LaneFact {
                block: b.clone(),
                verdict: verdicts.get(b).cloned(),
                // Filled by the caller (#2163): whether a lane's recorded pane
                // is dead is a fact about the ENTRY and the agent map, and this
                // function is deliberately given neither — it reads the gate.
                pane_dead: false,
            })
            .collect();
        let notices: Vec<rddrive::LaneNotice> = lanes
            .iter()
            .filter_map(|l| {
                l.verdict.as_ref().map(|v| rddrive::LaneNotice {
                    block: l.block.clone(),
                    verdict: v.verdict,
                    summary: v.summary.clone(),
                    open_findings: v.open_findings,
                    at_head: v.head.clone(),
                })
            })
            .collect();
        if !want_gate {
            // `review-wait` needs the lane list and nothing else. Evaluating the
            // gate here would be a second answer to a question this state does
            // not ask, on a tick that has to re-ask it at `gate-check` anyway.
            return (reviewdrive::GateOutcome::NotEvaluated, Some(lanes), notices);
        }
        let head = (!obs.head.is_empty()).then_some(obs.head.as_str());
        let outcome = match mergeq::recheck_gate(&spec, &verdicts, head, &observed) {
            mergeq::GateRecheck::Ok => reviewdrive::GateOutcome::Satisfied,
            mergeq::GateRecheck::Malformed | mergeq::GateRecheck::NotConfigured => {
                reviewdrive::GateOutcome::Unreadable
            }
            mergeq::GateRecheck::RoutingUnaccountable => reviewdrive::GateOutcome::NotEvaluated,
            // Arc 10, deliberately wider than "stale": a stale pass, an
            // unsatisfied `also:` condition, a red default branch, a PR over the
            // size clause. Each is "the gate is not satisfied for this
            // revision", which is `ci-wait` and then the drive's own age bound.
            _ => reviewdrive::GateOutcome::Unsatisfied,
        };
        (outcome, Some(lanes), notices)
    }
}
