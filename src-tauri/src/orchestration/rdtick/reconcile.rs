//! Recovery: the restart mark (#2811 S10), the lost-pane prune, §2.4's restart
//! reconcile, and the board-row task note.
//!
//! Design note: `docs/design/review-driver.md`. Part of `rdtick/`, the driver's
//! registry wiring, which `tests/reviewdrive/guards.rs` scans whole (see `mod.rs`).

use super::*;

impl OrchRegistry {
    /// Forget this PR's restart mark (#2811 S10).
    ///
    /// The mark is keyed `(group, pr)` and the ENTRY it was made for is not:
    /// a drive can be cancelled, pruned, or displaced by a fresh
    /// `drive_review` on the same PR, all within one process. A mark left
    /// behind by any of those outlives the drive it described, and the NEXT
    /// drive on that PR spends it the first time it reaches `fix-wait` — one
    /// unearned `why: restart` re-brief, charged to nobody and explained by
    /// nothing, on a drive no restart ever interrupted.
    ///
    /// So it is cleared everywhere [`rd_signals`](Registry::rd_signals) is,
    /// and for the same reason: both are per-process facts ABOUT AN ENTRY,
    /// held in a map keyed by the PR that entry happened to be for. The three
    /// sites are the prune, `cancel_review_drive`, and `drive_review`'s
    /// re-drive; the tick's own discharge is separate and is the `re_briefed`
    /// spend, which answers "was this mark used" rather than "is this mark
    /// still about anything".
    pub(super) fn rd_forget_restart_mark(&self, group: &GroupId, pr: u64) {
        self.rd_restart_handback.lock_safe().remove(&(group.clone(), pr));
    }

    /// This drive's restart mark, or the empty one — read at facts-build time,
    /// spent by [`rd_spend_restart_mark`](Self::rd_spend_restart_mark).
    pub(super) fn rd_restart_mark(&self, group: &GroupId, pr: u64) -> RestartMark {
        self.rd_restart_handback
            .lock_safe()
            .get(&(group.clone(), pr))
            .cloned()
            .unwrap_or_default()
    }

    /// Discharge part of a restart mark, dropping the key once nothing is left
    /// to say.
    ///
    /// **Per field, never wholesale.** A drive can carry more than one — a
    /// `review-wait` drive that lost its lane AND a superseded worker pane — and
    /// the tick that acts on one has decided nothing about the others. Clearing
    /// the whole entry from the site that re-briefed a lane is how #2811 S10's
    /// own discharge would have silently un-marked #3225's push.
    pub(super) fn rd_spend_restart_mark(&self, group: &GroupId, pr: u64, f: impl FnOnce(&mut RestartMark)) {
        let mut marks = self.rd_restart_handback.lock_safe();
        let key = (group.clone(), pr);
        let Some(mark) = marks.get_mut(&key) else { return };
        f(mark);
        if mark.is_empty() {
            marks.remove(&key);
        }
    }

    /// Whether this pane is in the **live roster** — the one liveness rule the
    /// driver's record-repair uses (#3225, #3226).
    ///
    /// An agent this registry has no record of answers `false` here, and that is
    /// the opposite reading from [`rd_pane_exit`](Self::rd_pane_exit)'s
    /// deliberately — which is why this is only ever asked from the two places
    /// where absence is unambiguous. See
    /// [`rd_forget_lost_panes`](Self::rd_forget_lost_panes).
    pub(super) fn rd_pane_is_live(&self, agent_id: &str) -> bool {
        self.agent(agent_id).is_some_and(|a| a.status != AgentStatus::Dead)
    }

    /// **Drop every pane this drive owns that the live roster does not have, and
    /// keep the conversations** (#3225, #3226). Answers what went.
    ///
    /// # Where this may be asked, and why not on every tick
    ///
    /// A pane missing from the agent map is normally "we could not check" — the
    /// asymmetry [`reviewdrive::DriveEntry::forget_dead_panes`],
    /// [`rd_pane_exit`](Self::rd_pane_exit) and [`reviewdrive::LaneFact::pane_dead`] all
    /// state, and the fail-direction that keeps a transient gap from un-owning
    /// live panes across the whole group.
    ///
    /// There are exactly two places the reading is not ambiguous, and this is
    /// called from both: the **restart reconcile**, where every pane of the
    /// previous process is gone by construction, and a **`drive_review` an
    /// orchestrator issued against a live drive**, which is an operator saying
    /// in as many words that this drive needs looking at. Nothing on the tick
    /// path calls it — and after either of those has run, no later tick names a
    /// dead pane anyway, because the ownership is gone.
    ///
    /// # What each drop leaves behind
    ///
    /// A lane is **reseeded** rather than merely emptied
    /// ([`reviewdrive::DriveEntry::reseed_lane`], #3176's half of the release):
    /// clearing the pane alone would leave `briefed_head` standing, and
    /// `decide_review_wait`'s wait arm reads that as *open at this revision, pane
    /// alive* — the drive then waits out its state bound for a verdict no pane
    /// can produce, which is #3226's incident exactly. The reseed carries the
    /// session, so the re-brief resumes the same reviewer conversation.
    ///
    /// The **worker** keeps `worker_session`, which is what the hand-back and
    /// #3225's push-delivered read both resume from; only `worker_agent` goes.
    ///
    /// **A pane whose session cannot be named is KEPT**, by both
    /// `reseed_lane`'s and `release_pane`'s own refusal: dropping it would cost
    /// the conversation rather than a slot, and the drive is still bounded by
    /// `lane-stalled` / `fix-stalled` as it was before. That is the residual —
    /// rare, since `rd_lane_session` resolves from the record, the live map and
    /// the roster, of which the roster survives a restart.
    pub(super) fn rd_forget_lost_panes(
        &self,
        group: &GroupId,
        entry: &mut reviewdrive::DriveEntry,
    ) -> RdLostPanes {
        let mut lost = RdLostPanes::default();
        // Lanes first: resolving a lane's session borrows `entry` immutably and
        // reads three sources, so it is done before anything mutates the record.
        let blocks: Vec<String> = entry
            .lanes
            .iter()
            .filter(|l| !l.agent.trim().is_empty())
            .map(|l| l.block.clone())
            .collect();
        for block in blocks {
            let Some(pane) = entry.lane(&block).map(|r| r.agent.clone()) else { continue };
            if self.rd_pane_is_live(&pane) {
                continue;
            }
            let session = self.rd_lane_session(group, entry.lane(&block)).unwrap_or_default();
            if let Some(freed) = entry.reseed_lane(&block, &session) {
                lost.lanes.push((block, freed));
            }
        }
        let worker = entry.worker_agent.clone();
        if !worker.trim().is_empty() && !self.rd_pane_is_live(&worker) {
            let session = entry.worker_session.clone();
            if let Some(freed) = entry.release_pane(&reviewdrive::DrivenRole::Worker, &session) {
                lost.worker.push(freed);
            }
        }
        // The superseded lists, by the same predicate — including the pane the
        // reseed just moved onto one of them.
        if entry.forget_dead_panes(&|id| self.rd_pane_is_live(id)) {
            lost.superseded = true;
        }
        lost
    }

    /// Whether this drive is still USING a pane the live roster does not have —
    /// the cheap question [`drive_review_with`](Self::drive_review_with) asks
    /// before refusing `already-driven` (#3226).
    ///
    /// Reads the same predicate as [`rd_forget_lost_panes`](Self::rd_forget_lost_panes)
    /// over the same population as the locked gate below it
    /// ([`RdLostPanes::current_panes_lost`]), and repairs nothing — so the cheap
    /// check and the authoritative one cannot disagree about what a repair would
    /// find.
    ///
    /// **The CURRENT worker and lane panes, never `owned_panes()`** (#3228
    /// review 2, W1): that one includes the superseded lists, whose dead entries
    /// are the ordinary state of a drive between a pane replacement and the next
    /// tick's prune. See `current_panes_lost` for what reading them here cost.
    pub(super) fn rd_has_lost_panes(&self, entry: &reviewdrive::DriveEntry) -> bool {
        let current = std::iter::once(entry.worker_agent.clone())
            .chain(entry.lanes.iter().map(|l| l.agent.clone()));
        current.filter(|a| !a.trim().is_empty()).any(|a| !self.rd_pane_is_live(&a))
    }

    /// §2.4's restart reconcile, once per group per registry instance, before
    /// driving — `rd_reconciled` is a field of the registry, so "per process"
    /// holds only while a process builds one of these (#2135 review 2).
    ///
    /// The `recover_persisted_queue` posture: a PR **positively established** as
    /// closed or merged becomes `cancelled` with its notice; anything else
    /// resumes from disk and is re-evaluated against the **live** head on the
    /// tick that follows, never against the head the file remembers. A PR whose
    /// state could not be determined is neither — `None` is "the world does not
    /// match", never "probably fine".
    ///
    /// It also drops any cap-starvation run the previous process left standing
    /// (#2135) — see the comment on the call, and
    /// `DriveEntry::discard_cap_starvation_run` for why that run cannot survive
    /// a process boundary and why nothing is charged for it.
    ///
    /// An unresolvable *session* is deliberately not held here, though §2.4
    /// names it: that is what the first hand-back discovers
    /// (`held(worker-unresumable)`), and holding at reconcile would park every
    /// drive whose worker pane merely has not been re-registered yet at startup
    /// — a race the reconcile cannot distinguish from a genuinely lost session,
    /// where the hand-back can.
    pub(super) fn rd_reconcile_with(&self, group: &GroupId, runner: &dyn rddrive::RdRunner, now: u64) {
        if self.rd_reconciled.lock_safe().contains(group) {
            return;
        }
        let dir = self.group_dir(group);
        // `(on_behalf, pr, cancelled, forgot_cap_run, panes_dropped)`.
        let mut audits: Vec<(String, u64, bool, bool, usize)> = Vec::new();
        // #2135 N2: set false only when a write this reconcile NEEDED actually
        // failed. It stays true when nothing had to be written, which is the
        // honest reading — there was no durable outcome to miss.
        let mut persisted = true;
        {
            let _state_guard = self.rd_state_lock.lock_safe();
            // **The once-only latch is set below, on a reconcile that actually
            // READ the file — not here, on one that merely attempted it.**
            // Latching first is the obvious spelling and it means a torn
            // `review_drives.json` at startup costs this group its reconcile for
            // the life of the process, including after a human fixes the file:
            // the flag says "done" and nothing ever revisits it. §2.4 already
            // makes an unreadable file a loud, rate-bounded "a human has to look
            // at this", and a human who then looks and fixes it should get the
            // reconcile they were owed.
            let Ok(mut state) = reviewdrive::load_state(&dir) else { return };
            self.rd_reconciled.lock_safe().insert(group.clone());
            let mut changed = false;
            let live: Vec<u64> =
                state.entries.iter().filter(|e| !e.state().is_terminal()).map(|e| e.pr).collect();
            for pr in live {
                // `pr_is_open`, not `observe_pr`: reconcile reads nothing but
                // this, and `observe_pr` would spend a second round trip on
                // checks it never looks at, per live entry, at startup.
                let open = rddrive::pr_is_open(runner, pr);
                let Some(entry) = state.entry_mut(pr) else { continue };
                let on_behalf = entry.on_behalf_of.clone();
                // **A cap-starvation run cannot straddle a process boundary**
                // (#2135). The stamp is written and read by ticks that OBSERVED
                // the cap refuse a lane spawn, and across a shutdown no tick
                // ran: the gap is time the cap refused nothing, and after a
                // restart every pane this group's cap was counting is gone.
                // Left standing, a stamp older than `CAP_HOLD_MS` parks the
                // resumed drive `held(cap-full)` on its FIRST tick — before one
                // spawn is attempted — on a notice telling an orchestrator to
                // free a slot in a group whose slots are all free.
                //
                // Unconditional, and above the cancel arm rather than in the
                // `else`: a cancelled entry's `advance` clears the stamp anyway,
                // so putting this in one branch would make the reconcile's rule
                // depend on a fact that has nothing to do with the cap. Charging
                // nothing is `discard_cap_starvation_run`'s own argument — the
                // gap is mostly downtime, and #2110's accumulators are a CREDIT
                // against the age bounds that #2117 deliberately charges
                // downtime to.
                let forgot_cap_run = entry.discard_cap_starvation_run();
                if forgot_cap_run {
                    changed = true;
                }
                if open == Some(false)
                    && entry.advance(reviewdrive::DriveState::Cancelled, None, None, 0).is_ok()
                {
                    changed = true;
                    // Owed onto the entry, not returned for a fire-and-forget
                    // delivery (#1857). Reconcile runs at startup — the exact
                    // moment an orchestrator pane is most likely to be missing
                    // or still coming up — so this is the producer whose notice
                    // was most likely to be the one that vanished. The caller's
                    // flush delivers it from disk, on this tick or a later one.
                    //
                    // #1871 B3's pane list is threaded into the construction
                    // rather than dropped: this hunk is a replace-vs-augment,
                    // and keeping "both sides" would restore the direct push
                    // #1857 deleted while ALSO owing the notice — the line
                    // twice. The panes are read before `owe_notice`'s mutable
                    // borrow.
                    let panes = self.rd_surviving_panes(entry);
                    // No release clause: reconcile takes no tick, so
                    // `releasable` is never asked and nothing was killed here.
                    let n =
                        rddrive::cancelled_notice(pr, rddrive::CancelCause::PrGone, &panes, "");
                    // #3367 item 2: the reconcile's cancel can be an
                    // auto-started drive's FIRST notice — the report was
                    // persisted for exactly this, a first notice one restart
                    // away — so it folds the report like the tick's does.
                    let n = Self::rd_fold_auto_report(entry, n);
                    entry.owe_notice(&n, now);
                    audits.push((on_behalf, pr, true, forgot_cap_run, 0));
                } else {
                    // **What a restart cost this drive, and what recovers it**
                    // (#2811 S10, #3225, #3226) — the three marks below, each
                    // argued at its own arm.
                    //
                    // Every mark is recorded here and acted on by the tick, which
                    // is the split the rest of this function already keeps: the
                    // reconcile is the only place the fact is KNOWN (every pane
                    // died, so a missing one is not the ambiguous mid-session
                    // reading), and the tick is the only place that can afford to
                    // observe the PR, render a brief and resume a session. Doing
                    // the hand-back here would spend a `gh` round trip per live
                    // drive at startup and duplicate the whole hand-back path,
                    // cap refusal and `worker-unresumable` handling included.
                    //
                    // **The panes, read from the LIVE ROSTER rather than from
                    // this file** (#3225, #3226). Every pane of the previous
                    // process died with it, so a recorded pane the roster does
                    // not have is GONE — not "we could not check", which is the
                    // reading every mid-session site makes and the reason this
                    // one is here rather than on the tick. Their SESSIONS
                    // survive, and are what each recovery below resumes.
                    //
                    // Ownership is dropped first, so nothing downstream can name
                    // a dead pane: `held(fix-stalled)`'s notice enumerates
                    // `owned_panes()` as "still OWNED", and on #3225's incident
                    // every entry in that list had died with the process.
                    let lost = self.rd_forget_lost_panes(group, entry);
                    if lost.any() {
                        changed = true;
                    }
                    let mut mark = RestartMark::default();
                    match entry.state() {
                        // **#2811 S10: a drive parked in `fix-wait` is waiting on
                        // a pane that died with the previous process.** Nothing
                        // will ever arrive for it — the signals map is
                        // per-process and empty, and the pane whose `report`
                        // would fill it is gone — so without this the drive waits
                        // out `fix_timeout_minutes` and exits
                        // `held(fix-stalled)`, a claim about a worker's silence
                        // that is really a claim about a restart.
                        //
                        // Unconditional on the state alone, and NOT on the pane
                        // having been dropped above: the hand-back is what
                        // discovers whether the SESSION survived, and a
                        // `fix-wait` drive whose pane somehow did survive is
                        // re-briefed into it by `rd_reuse_pane` at no cost.
                        reviewdrive::DriveState::FixWait => mark.handback = true,
                        // **#3225: `ci-wait` after a push, whose worker is
                        // gone.** `decide_fix_receipts` waits for that worker's
                        // `report(done)` before briefing a lane, and it cannot
                        // come; the push is durable and is the fix. Gated on the
                        // pane ACTUALLY having been lost, because unlike the
                        // hand-back there is no probe here — this mark makes the
                        // drive advance without the report, so it must rest on
                        // the pane really being gone rather than on the process
                        // having restarted.
                        reviewdrive::DriveState::CiWait
                            if entry.fix_pushed() && !lost.worker.is_empty() =>
                        {
                            mark.push_delivered = true
                        }
                        _ => {}
                    }
                    // **#3226: the lanes.** The reseed above already puts each
                    // one back in `lane_open_for`'s false branch, so
                    // `decide_review_wait` re-opens it on the next tick by the
                    // ordinary path and `rd_open_lane` resumes its recorded
                    // session. What the mark carries is the WHY, for the
                    // `rd-lane-spawned` row — without it a restart recovery is
                    // indistinguishable on the audit log from an ordinary
                    // re-brief.
                    mark.lanes = lost.lanes.iter().map(|(block, _)| block.clone()).collect();
                    // On the row, because the only other visible effect of
                    // dropping a pane is a notice that does NOT name it —
                    // which reads exactly like a drive that never owned one
                    // (`rd-lane-duplicate-refused`s reason, one surface over).
                    let dropped = lost.worker.len() + lost.lanes.len();
                    if !mark.is_empty() {
                        self.rd_restart_handback.lock_safe().insert((group.clone(), pr), mark);
                    }
                    audits.push((on_behalf, pr, false, forgot_cap_run, dropped));
                }
            }
            // #2135 N2: whether this reconcile's decisions reached DISK. The
            // in-memory `state` is dropped at the end of this block and the
            // once-only latch is already set, so a failed write means the clear
            // is lost for the life of the process — the next tick reloads the
            // stale stamp and parks `held(cap-full)` anyway. The audit row below
            // therefore reports the durable outcome rather than the decision:
            // a row asserting a clear the write unmade is the one state in which
            // this log actively misleads whoever is reading it. Same rule as the
            // tick's own `persisted` flag one function down, applied to the
            // reconcile, which had no counterpart.
            //
            // **The latch itself is deliberately left alone** and is the
            // residual: it predates #2135, and moving it is a change to when
            // every reconcile re-runs rather than to what this row says.
            if changed {
                persisted = reviewdrive::store_state(&dir, &state).is_ok();
            }
        }
        for (on_behalf, pr, cancelled, forgot_cap_run, dropped) in audits {
            let action = if cancelled {
                rddrive::audit_action::CANCELLED
            } else {
                rddrive::audit_action::RECOVERED
            };
            // #2135: whether this reconcile dropped a cap-starvation run the
            // previous process left behind. On the row rather than silent for
            // `rd-lane-duplicate-refused`'s reason — the clear's only other
            // visible effect is a drive that did NOT park, which on this log is
            // indistinguishable from there having been no stamp at all.
            self.rd_audit(
                group,
                &on_behalf,
                action,
                json!({ "pr": pr, "at": "reconcile",
                        "cap_run_forgotten": forgot_cap_run && persisted,
                        "panes_dropped": dropped }),
            );
        }
    }

    /// Put a `TaskNote` on the board row whose `pr` matches, so a human sees the
    /// drive where they see the work (§3.2).
    ///
    /// **The driver writes to the board and reads nothing from it.** `Task::pr`
    /// is agent-writable, so a driver that took its worker session or its gate
    /// from a row would be letting the thing being checked answer the check.
    /// Matching a row in order to write a note on it is not that: a wrong match
    /// costs a note on the wrong row, never an authorization.
    pub(super) fn rd_task_note(&self, group: &GroupId, pr: u64, text: &str) {
        let Some(id) = self
            .tasks(group)
            .into_iter()
            .find(|t| t.pr.as_deref().and_then(pr_number) == Some(pr))
            .map(|t| t.id)
        else {
            return;
        };
        let _ = self.upsert_task(
            group,
            brand::AUDIT_ACTOR,
            Some(&id),
            TaskPatch { note: Some(text.to_string()), ..TaskPatch::default() },
        );
    }
}
