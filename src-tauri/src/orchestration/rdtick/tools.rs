//! §5.1's MCP tools — `drive_review`, `cancel_review_drive`,
//! `review_drive_status` — plus `rd_auto_start` (#3367), their test seams and
//! the audited refusal.
//!
//! Design note: `docs/design/review-driver.md`. Part of `rdtick/`, the driver's
//! registry wiring, which `tests/reviewdrive/guards.rs` scans whole (see `mod.rs`).

use super::*;

impl OrchRegistry {
    // ---------- §5.1's three MCP tools ----------

    /// `drive_review(pr, worker_session, reset_counters?, rounds_already_spent?)`
    /// — §3.2's second key, and the one call an orchestrator makes to start a
    /// drive (a plan drive's hand-off and #3367's auto-start reach it too).
    ///
    /// **Never automatic by default**, and in particular it does not fire on a
    /// worker's `report(done)`: INVARIANT 8 makes *what starts* the
    /// orchestrator's call, and the PRs where a drive is wrong are ordinary — a
    /// scratch or red-evidence PR, a release bump, a PR the human said they
    /// would read themselves. The one opt-in is `driver.auto_drive_on_done`
    /// (#3367), which reaches this function through
    /// [`rd_auto_start`](Self::rd_auto_start) and is argued there.
    ///
    /// **The session is resolved once, and what is persisted is what came
    /// back** (§3.2). `resolve_session_ref` is a resolution against *this
    /// group's roster at the moment of the call*: a prefix that resolves
    /// uniquely today can become ambiguous tomorrow as the roster grows, and the
    /// entry outlives both the call and the process. So the **resolved** id goes
    /// into `review_drives.json`, never the caller's raw string.
    ///
    /// Refusals are §5.1's closed vocabulary, in two classes: the driver
    /// declining, and orrerix having failed. The order they are checked in is
    /// cheapest-first among equals, with one exception that is not: the PR's
    /// state is read **last** among the checks, because it is the only one that
    /// spends a `gh` round trip.
    #[doc(hidden)] // pub for integration tests
    pub fn drive_review(
        &self,
        group: &GroupId,
        pr: u64,
        worker_session: &str,
        reset_counters: bool,
        rounds_already_spent: u32,
        on_behalf_of: &str,
    ) -> Value {
        let injected = self.rd_runner_override.lock_safe().clone();
        let owned;
        let runner: &dyn rddrive::RdRunner = match injected.as_deref() {
            Some(r) => r,
            None => {
                let Some(repo) = self.group(group).map(|g| g.repo) else {
                    return self.rd_refuse(group, pr, rddrive::refusal::UNAVAILABLE);
                };
                owned = rddrive::runner_for(std::path::Path::new(&repo));
                &owned
            }
        };
        self.drive_review_with(
            group,
            runner,
            pr,
            worker_session,
            reset_counters,
            rounds_already_spent,
            on_behalf_of,
            now_ms(),
        )
    }

    /// [`drive_review`](Self::drive_review) with the `gh` seam injected.
    #[doc(hidden)] // pub for integration tests
    #[allow(clippy::too_many_arguments)]
    pub fn drive_review_with(
        &self,
        group: &GroupId,
        runner: &dyn rddrive::RdRunner,
        pr: u64,
        worker_session: &str,
        reset_counters: bool,
        rounds_already_spent: u32,
        on_behalf_of: &str,
        now: u64,
    ) -> Value {
        self.drive_review_seeded(
            group,
            runner,
            pr,
            worker_session,
            reset_counters,
            rounds_already_spent,
            on_behalf_of,
            now,
            None,
        )
    }

    /// [`drive_review_with`](Self::drive_review_with), plus the worker report
    /// that started the drive when one did (#3367 item 2).
    ///
    /// **The report is written onto the entry in the SAME store that creates
    /// it**, not by a second write after the call returns: between two writes
    /// a tick could take the drive's first step, and a hold on that step would
    /// build the first notice with nothing to fold into it. One write is the
    /// only ordering that cannot lose the words.
    #[allow(clippy::too_many_arguments)]
    fn drive_review_seeded(
        &self,
        group: &GroupId,
        runner: &dyn rddrive::RdRunner,
        pr: u64,
        worker_session: &str,
        reset_counters: bool,
        rounds_already_spent: u32,
        on_behalf_of: &str,
        now: u64,
        auto_report: Option<String>,
    ) -> Value {
        use rddrive::refusal as r;
        if !self.driver_enabled(group) {
            return self.rd_refuse(group, pr, r::DRIVER_DISABLED);
        }
        // The session, resolved once (§3.2). An empty string gets its own name
        // rather than leaking `resolve_session_ref`'s untagged prose.
        if worker_session.trim().is_empty() {
            return self.rd_refuse(group, pr, r::RESUME_SESSION_EMPTY);
        }
        let session = match resolve_session_ref(&self.merged_records(group), worker_session) {
            Ok(s) => s,
            Err(e) if e.starts_with("resume-ambiguous") => {
                return self.rd_refuse(group, pr, r::RESUME_AMBIGUOUS)
            }
            Err(_) => return self.rd_refuse(group, pr, r::RESUME_NOT_FOUND),
        };
        // **The block the hand-back would resume under, resolved AT the call**
        // (#2819 (g), S7) — see [`rd_unhandbackable_block`]. Accepting a
        // session whose block is the orchestrator's or the manager's, or one
        // this roster no longer declares, cost #2819 three `worker-unresumable`
        // holds and three orchestrator turns for a PR that could never be
        // handed back; this is the one refusal instead, quoting the SAME
        // sentence the hold would have carried.
        if let Some(why) = self.rd_unhandbackable_block(group, &session) {
            return self.rd_refuse_detail(group, pr, r::WORKER_UNRESUMABLE, &why);
        }
        // The gate, from the same two files the shim reads. `gate-unreadable` is
        // NOT `gate-not-configured`: a wrong label sends the reader somewhere
        // else, which is #681's own lesson and the queue's posture.
        let spec = match self.merge_queue_gate(group) {
            Ok(s) => s,
            Err(_) => return self.rd_refuse(group, pr, r::GATE_UNREADABLE),
        };
        let gate = match &spec {
            mergeq::GateSpec::Declared(g) => g.clone(),
            mergeq::GateSpec::Malformed => return self.rd_refuse(group, pr, r::GATE_UNREADABLE),
            mergeq::GateSpec::Absent => {
                return self.rd_refuse(group, pr, r::GATE_NOT_CONFIGURED)
            }
        };
        // A gate requiring a reviewer the roster does not declare is answerable
        // here, from two files. Left unanswered it becomes `held(lane-stalled)`
        // an hour later instead of an immediate refusal.
        if let Some(g) = self.group(group) {
            if !workflow::gate_missing_blocks(&gate, &g.guardrails.blocks).is_empty() {
                return self.rd_refuse(group, pr, r::GATE_NAMES_NO_SUCH_BLOCK);
            }
        }
        // §8.1's mutual refusal: the two loops both move a PR's head and both
        // read its verdicts, and neither was designed expecting the other to be
        // doing so concurrently. The intended sequence is serial and has a
        // direction — a drive ends at `satisfied`, the orchestrator dispositions
        // the findings, and *then* it queues.
        if let Ok(q) = mqloop::load_state(&self.group_dir(group)) {
            if q.entry(pr).map(|e| !e.state().is_terminal()).unwrap_or(false) {
                return self.rd_refuse(group, pr, r::IN_MERGE_QUEUE);
            }
        }
        // **`already-driven`, checked once cheaply BEFORE the `gh` call.** A
        // second `drive_review` on a live PR is the ordinary duplicate — an
        // orchestrator retrying, or re-reading its own state after a compact —
        // and spending a `gh` round trip to answer it is a round trip on the
        // loop that also delivers every `notify_when` notice in the fleet. The
        // AUTHORITATIVE check is still the one under the lock below: this read
        // is unsynchronized, so it can only ever be stale in the direction of
        // doing more work, never of starting a second drive.
        //
        // **Unless this drive has lost a pane** (#3226). A `drive_review` on
        // a live drive whose panes all died is not a duplicate at all — it
        // is an orchestrator asking for exactly the recovery the locked arm
        // below performs, and refusing it was why the only manual route out
        // of #3226s incident was `cancel_review_drive` plus a fresh drive,
        // which loses the counters. The predicate is an agent-map read, so
        // this stays a cheap check with no round trip in it.
        if reviewdrive::load_state(&self.group_dir(group))
            .map(|s| {
                s.is_driven(pr)
                    && !s.entry(pr).is_some_and(|e| self.rd_has_lost_panes(e))
            })
            .unwrap_or(false)
        {
            return self.rd_refuse(group, pr, r::ALREADY_DRIVEN);
        }
        // Last, because it is the only check that spends a `gh` round trip — and
        // it spends exactly ONE: `drive_review` reads whether the PR is open and
        // nothing else, so `observe_pr`'s second call on `gh pr checks` would be
        // an answer this path never looks at.
        match rddrive::pr_is_open(runner, pr) {
            Some(true) => {}
            Some(false) => return self.rd_refuse(group, pr, r::PR_NOT_OPEN),
            // The remote did not answer. Unknown is never treated as safe, and
            // it is never treated as a fact about the PR either.
            None => return self.rd_refuse(group, pr, r::PR_UNVERIFIABLE),
        }
        let dir = self.group_dir(group);
        // Notices owed by an entry this call is about to displace — audited
        // after the lock is dropped, for `rd_audit`'s own reason (#1857).
        let mut dropped_notices: Vec<(u64, String)> = Vec::new();
        let (audit_action, detail) = {
            let _state_guard = self.rd_state_lock.lock_safe();
            let mut state = match reviewdrive::load_state(&dir) {
                Ok(s) => s,
                // NOT `not-driven`: that would assert something orrerix cannot
                // know, and `already-driven` is unevaluable here, so an unnamed
                // failure becomes a second drive on one PR.
                Err(_) => return self.rd_refuse(group, pr, r::STATE_UNREADABLE),
            };
            if state.is_driven(pr) {
                // **A live drive whose panes are gone RESUMES rather than
                // refusing** (#3226).
                //
                // The refusal is right for the ordinary duplicate — an
                // orchestrator retrying, or re-reading its own state after a
                // compact — and stays for it: a drive whose panes are all live
                // has nothing here to repair and falls through to
                // `already-driven` below. What it was wrong for is the drive
                // this call is really about, where every pane died with a
                // previous process and the only recovery left was
                // `cancel_review_drive` plus a fresh `drive_review`, which
                // starts the counters over — three lost review rounds on
                // #3226s incident.
                //
                // **The repair is the reconcile s, called rather than
                // re-spelled**, so the two cannot answer the pane question
                // differently: drop what the roster does not have, keep every
                // session, and leave the marks the tick acts on. No arc, no
                // counter, and the state is untouched — this is not arc 11,
                // which is `held`s resume and is the branch below.
                let Some(entry) = state.entry_mut(pr) else {
                    return self.rd_refuse(group, pr, r::STATE_UNREADABLE);
                };
                let lost = self.rd_forget_lost_panes(group, entry);
                // **A superseded pane is not a loss to recover from** (#3228
                // review 2, W1) — see `RdLostPanes::current_panes_lost`. The
                // repair below is not free: on a `fix-wait` drive it marks a
                // re-hand-back on the state alone, and a duplicate call that
                // reached it would re-brief a live worker mid-fix.
                if !lost.current_panes_lost() {
                    return self.rd_refuse(group, pr, r::ALREADY_DRIVEN);
                }
                let mut mark = RestartMark::default();
                match entry.state() {
                    reviewdrive::DriveState::FixWait => mark.handback = true,
                    reviewdrive::DriveState::CiWait
                        if entry.fix_pushed() && !lost.worker.is_empty() =>
                    {
                        mark.push_delivered = true
                    }
                    _ => {}
                }
                mark.lanes = lost.lanes.iter().map(|(block, _)| block.clone()).collect();
                let here = entry.state();
                if reviewdrive::store_state(&dir, &state).is_err() {
                    return self.rd_refuse(group, pr, r::STATE_UNWRITABLE);
                }
                // Marked only once the drop is DURABLE. A mark whose pane the
                // record still names would re-brief a lane the next reconcile
                // would then re-brief again.
                if !mark.is_empty() {
                    self.rd_restart_handback.lock_safe().insert((group.clone(), pr), mark);
                }
                self.rd_audit(
                    group,
                    on_behalf_of,
                    rddrive::audit_action::RECOVERED,
                    json!({ "pr": pr, "at": "drive_review",
                            "panes_dropped": lost.worker.len() + lost.lanes.len() }),
                );
                // Serviced on the very next wake, for the same reason the
                // accepted path below clears it.
                self.rd_service_ms.lock_safe().remove(group);
                return json!({ "driving": true, "state": here.as_str(),
                               "recovered": "panes" });
            }
            let resumed = match state.entry(pr).map(|e| e.state()) {
                // A parked drive RESUMES, carrying its counters — §2.3's
                // default, and the whole reason §2.1 makes `held` parked rather
                // than terminal. Clearing them is an explicit, audited argument.
                Some(reviewdrive::DriveState::Held) => true,
                _ => false,
            };
            if resumed {
                let entry = match state.entry_mut(pr) {
                    Some(e) => e,
                    None => return self.rd_refuse(group, pr, r::STATE_UNREADABLE),
                };
                if reset_counters {
                    entry.counters = reviewdrive::Counters::seeded(rounds_already_spent);
                    // #3367: a fresh budget is a fresh budget for the driver's
                    // own non-blocking rounds too — and the revision the last
                    // one was handed back at is no longer a reason to refuse
                    // the next, since the orchestrator has just paid for it.
                    entry.nit_rounds = 0;
                    entry.nit_head.clear();
                    entry.nit_digest.clear();
                    // **And the hold-notice dedup is re-armed with them**
                    // (#3040 N1). A resetting resume buys the drive a fresh
                    // budget, so the next hold at the same bound is a hold
                    // about a round the orchestrator paid for — and the key
                    // cannot see that, because the counters are back at the
                    // values the previous hold carried. Only THIS resume
                    // clears it: a plain one changes nothing the drive can
                    // observe, and its re-hold is the repeat N1 suppresses.
                    entry.rearm_hold_notice();
                }
                // Read BEFORE `advance`, which clears `held_reason` on the way
                // out of `held`. Used by the lane re-open below.
                let was_lane_stalled =
                    entry.held_reason == Some(reviewdrive::HeldReason::LaneStalled);
                if entry
                    .advance(reviewdrive::DriveState::CiWait, None, None, now)
                    .is_err()
                {
                    return self.rd_refuse(group, pr, r::STATE_UNREADABLE);
                }
                // **The age clocks restart on a resume, or arc 11 is a no-op for
                // exactly the holds it exists to recover.**
                //
                // §2.2 makes `drive-stalled` the drive's AGE — `now -
                // started_ms`, "never an idle clock reset by each state
                // advance" — and `decide` checks it BEFORE any per-state logic.
                // Left alone, a drive parked longer than `drive_timeout_minutes`
                // re-holds `drive-stalled` on its very first tick after the
                // resume, and every hold a human takes their time over is
                // exactly that old. Four shipped surfaces promise the opposite.
                //
                // Resetting HERE does not reintroduce the idle clock that row
                // forbids: the ban is on a stamp written by each state advance,
                // and nothing on the tick path touches this. It moves only on a
                // deliberate, role-gated, audited `drive_review` — the same
                // event §2.3 already lets clear the counters. A drive being
                // restarted is a drive whose age starts again; the counters,
                // which are the budget, still carry unless `reset_counters` says
                // otherwise.
                //
                // Each lane's `spawned_ms` is re-armed for the same reason and
                // by the same argument: `lane-stalled` fires at 60 minutes, so
                // without this a resumed drive re-holds on the FIRST tick for a
                // lane the orchestrator has just looked at and chosen to resume.
                entry.started_ms = now;
                // #2110: and with it the starvation the age bound was going
                // to forgive. The exclusion is a credit against THIS age; a
                // resume starts the age over, so carrying the credit would
                // hand the new run a head start it did not earn. The
                // per-state clock and its own accumulator are reset by the
                // `advance` above, on the arc, which is where every arc
                // resets them.
                entry.starved_total_ms = 0;
                for l in entry.lanes.iter_mut() {
                    l.spawned_ms = now;
                }
                // **`lane-stalled` needs its lane RE-BRIEFED, not merely
                // re-timed** — and the clock re-stamp just above is what makes
                // that visible rather than fixing it. `decide_review_wait`
                // re-opens a lane only when `lane_open_for` is false, and at a
                // stable head it stays true, so the lane the notice named is
                // never spoken to again: before the re-stamp the drive re-held
                // instantly, after it the drive waits the full
                // `lane_timeout_minutes` in silence and re-holds then. Neither
                // is the recovery `held(lane-stalled)`'s own notice instructs,
                // and a hold ON A WAIT that its printed remedy cannot clear is
                // the defect arc 11 exists to not have. Holds parked on a
                // JUDGMENT are deliberately outside that rule — §2.2 names the
                // two and why resuming them re-holds by design.
                //
                // Clearing `briefed_head` puts the outstanding lane back in
                // `lane_open_for`'s false branch, so the next tick takes
                // `OpenLane`. `rd_open_lane` resumes the session recorded for
                // that lane when there is one and spawns a fresh reviewer when
                // there is not; either way the record is re-pointed at the pane
                // that now holds the lane, so §7's interception stays keyed on a
                // live pane rather than on an abandoned one.
                //
                // Scoped to this hold because it is the only one it can change.
                // A lane holding `escalate` or `review-limit` carries a verdict
                // that `decide_review_wait` answers before it ever consults the
                // lane record — SO LONG AS that verdict is still bound to the
                // revision in front of it, which is the half #1871 B1 added and
                // this sentence used to state flat. Once the head has moved the
                // verdict decides nothing, `lane_verdict_is_current` reads it as
                // absent, and the lane is re-briefed by the ordinary path with
                // no clearing needed here. Either way clearing in this arm would
                // be a no-op; and a lane that is legitimately mid-review must not
                // be re-briefed merely because some OTHER hold on the same drive
                // was resumed.
                if was_lane_stalled {
                    for l in entry.lanes.iter_mut() {
                        l.briefed_head.clear();
                    }
                }
                // **A new session means the recorded PANES are stale**, and a
                // stale pane is not merely useless — it is an interception key.
                // `driven_role` matches on `worker_agent` and on every pane it
                // superseded, so leaving them would have this drive consume the
                // traffic of a worker it no longer owns, while the worker it
                // DOES own reports to the orchestrator as if undriven. Cleared
                // on a change, kept when the orchestrator resumes with the same
                // session (the common case), where the panes are still this
                // worker's.
                if entry.worker_session != session {
                    entry.forget_worker_panes();
                }
                entry.worker_session = session.clone();
                // **Re-read on every resume** (#3250). The panes a drive was
                // STARTED ON are the ones its terminal release may end, and a
                // resume is a fresh start on whatever the session is carrying
                // now: the pane recorded at the first start may be long dead,
                // and the orchestrator may have resumed the conversation into a
                // new one before handing it back. `record_founding_panes` is
                // total, so this replaces rather than accumulates.
                entry.record_founding_panes(self.live_panes_on_session(group, &session));
                entry.on_behalf_of = on_behalf_of.to_string();
            } else {
                // A `satisfied` or `cancelled` entry that retention has not yet
                // pruned starts a FRESH drive with fresh counters — the queue's
                // own "comes back as a NEW entry" behaviour.
                //
                // **If that entry still owed a notice, this is the one other way
                // one is given up on** (#1857), and it is deliberately not
                // silent: the retained entry becomes reachable far more often
                // now that retention holds it for an undelivered notice, and a
                // re-drive discarding the previous drive's ending would be the
                // same silence with a different cause. Audited with the text,
                // exactly as the ceiling is, so the record survives the entry.
                // The notice is NOT carried onto the new entry: it describes a
                // drive that is over, and delivering it beside a fresh drive's
                // own traffic would read as this drive ending.
                let superseded: Vec<(u64, String)> = state
                    .entries
                    .iter()
                    .filter(|e| e.pr == pr)
                    .filter_map(|e| e.owed_notice().map(|n| (e.pr, n.text.clone())))
                    .collect();
                for (dropped_pr, text) in superseded {
                    dropped_notices.push((dropped_pr, text));
                }
                // **The lanes' CONVERSATIONS survive the entry that held them**
                // (#2153). Lane memory lives only here, so dropping the entry
                // used to drop it — and the sequence a satisfied gate is
                // designed to produce (satisfied, the orchestrator dispositions
                // the findings, a re-drive at the new head) is the ordinary
                // path, not an edge. Measured on PR #2141: two lanes with live,
                // resolvable sessions that had already read the PR once, both
                // re-opened `resumed=false`, on the round where the warm session
                // is cheapest.
                //
                // Read BEFORE the `retain` that discards them, and through
                // `rd_lane_session` rather than off `LaneRecord::session`: that
                // field is what the spawn RETURNED, which is a session id only
                // on a CLI that pre-assigns one, so seeding from it alone would
                // drop exactly the copilot and opencode lanes #2109 was about.
                // A lane with no session to carry from any of the three sources
                // is seeded not at all — an absent record is already "open this
                // one fresh", and a seeded record with nothing to resume would
                // only make `rd_open_lane` audit a resume failure for a session
                // that was never recorded.
                //
                // A seeded session that no longer resolves needs nothing here:
                // `rd_open_lane`'s existing `rd-lane-resume-failed` arm audits
                // the fall-through and spawns fresh, exactly as it does for a
                // lane reaped inside a live drive.
                //
                // `rd_lane_session` under `rd_state_lock` is the established
                // order, not a new one: `rd_step_entry` runs the whole
                // load-decide-store — that call and the spawn after it included
                // — under this same lock (§2.4).
                let seeded: Vec<reviewdrive::LaneRecord> = state
                    .entries
                    .iter()
                    .filter(|e| e.pr == pr)
                    .flat_map(|e| e.lanes.iter())
                    .filter_map(|l| {
                        self.rd_lane_session(group, Some(l)).map(|s| l.reseeded(&s))
                    })
                    .collect();
                state.entries.retain(|e| e.pr != pr);
                // A fresh drive is a fresh second-failure count (#2555 item 2):
                // the displaced entry's hand-back failures were ITS history, and
                // a new drive on the same PR with the same session starts with
                // one honest chance to succeed before the hold says anything.
                self.rd_handback_fails.lock_safe().remove(&(group.clone(), pr));
                // **The clock is the caller's, and that is what makes the age
                // bound testable at all.** `started_ms` is the anchor §2.2
                // measures `drive-stalled` from, and stamping it from the wall
                // clock here while the tick advances on an injected `now` put
                // the two on different scales: `age_ms` saturated to zero for
                // every synthetic clock, so `drive-stalled` could not fire in a
                // test and never had. That is most of why B2 shipped.
                let mut fresh = reviewdrive::DriveEntry::new(
                    pr,
                    &session,
                    on_behalf_of,
                    reviewdrive::Counters::seeded(rounds_already_spent),
                    now,
                );
                // #2153. Assigned after construction rather than threaded
                // through `DriveEntry::new`, which is arc 1 — a drive is CREATED
                // with no lanes, and a constructor that could be handed some
                // would make "a fresh drive has reviewed nothing" a caller's
                // discipline instead of the type's. The counters stay as
                // `rounds_already_spent` says: a warm conversation is not a
                // spent round.
                fresh.lanes = seeded;
                // The panes this drive is being handed (#3250) — see the same
                // call on the resume arm above.
                fresh.record_founding_panes(self.live_panes_on_session(group, &session));
                // #3367 item 2 — see `drive_review_seeded`. Only the FRESH arm
                // carries it: the auto-start refuses every PR with a live or
                // parked entry, so it never reaches the resume arm above.
                fresh.auto_report = auto_report.clone();
                state.entries.push(fresh);
            }
            if reviewdrive::store_state(&dir, &state).is_err() {
                return self.rd_refuse(group, pr, r::STATE_UNWRITABLE);
            }
            if resumed {
                (
                    rddrive::audit_action::RESUMED,
                    json!({ "pr": pr, "reset_counters": reset_counters,
                            "rounds_already_spent": rounds_already_spent }),
                )
            } else {
                (
                    rddrive::audit_action::STARTED,
                    json!({ "pr": pr, "rounds_already_spent": rounds_already_spent }),
                )
            }
        };
        for (dropped_pr, text) in &dropped_notices {
            self.rd_audit(
                group,
                on_behalf_of,
                rddrive::audit_action::NOTICE_DROPPED,
                json!({ "pr": dropped_pr, "reason": "superseded", "notice": text }),
            );
        }
        // A resume that carried a stale signal would re-hold on the reason it
        // was resumed out of — `messaged` most obviously.
        self.rd_signals.lock_safe().remove(&(group.clone(), pr));
        // #2811 S10, same argument one fact over: this call establishes a drive
        // the orchestrator is starting NOW, so a restart mark left by whatever
        // was on this PR before is not about it. Defence in depth on the same
        // measurement as the prune's clear — unreachable today, unpinned, and
        // kept for the same reason.
        self.rd_forget_restart_mark(group, pr);
        self.rd_audit(group, on_behalf_of, audit_action, detail);
        // Service this group on the very next wake rather than after a backoff
        // window that predates the drive.
        self.rd_service_ms.lock_safe().remove(group);
        json!({ "driving": true, "state": reviewdrive::DriveState::CiWait.as_str() })
    }

    /// **A worker's `report(done, ref: <PR>)` starts a drive on that PR**
    /// (#3367 item 2, `driver.auto_drive_on_done`) — or says why it did not.
    ///
    /// Answers `true` when a drive was started and the report is CONSUMED: the
    /// caller then does not deliver it, and the drive's first notice carries it
    /// instead ([`reviewdrive::DriveEntry::auto_report`]). Answers `false` in
    /// every other case, and the caller delivers the report exactly as it did
    /// before this existed — with one `rd-auto-start-declined` row naming why
    /// wherever the policy was on, so an orchestrator asking why a PR did not
    /// auto-drive reads it rather than inferring it from a row that is missing.
    /// With the policy off this does nothing at all, not even audit.
    ///
    /// **Why a report may start a drive at all is argued in
    /// `docs/design/review-driver.md` §3.2**, which is where the "never
    /// automatic" rule lived. The short form: the second key is still turned by
    /// the orchestrator — it spawned this worker onto this branch, and the
    /// refusals below confine the report to starting the drive the orchestrator
    /// would have started on the worker's own PR. Every input that decides
    /// that is something orrerix recorded or GitHub answered; the `ref` a
    /// delegate typed only NAMES a PR, and a PR on someone else's branch is
    /// `not-author`.
    #[doc(hidden)] // pub for integration tests
    pub fn rd_auto_start(&self, group: &GroupId, agent_id: &str, pr_ref: &str, report: &str) -> bool {
        if !self.rd_auto_drive_on_done(group) {
            return false;
        }
        let injected = self.rd_runner_override.lock_safe().clone();
        let owned;
        let runner: &dyn rddrive::RdRunner = match injected.as_deref() {
            Some(r) => r,
            None => {
                let Some(repo) = self.group(group).map(|g| g.repo) else { return false };
                owned = rddrive::runner_for(std::path::Path::new(&repo));
                &owned
            }
        };
        self.rd_auto_start_with(group, runner, agent_id, pr_ref, report, now_ms())
    }

    /// [`rd_auto_start`](Self::rd_auto_start) with the `gh` seam and the clock
    /// injected, for `drive_review_with`'s reason.
    ///
    /// The checks run cheapest-first, and the one `gh` call is last: a report
    /// naming no PR, from a worker with no session, on a PR already driven or
    /// already reviewed, is refused without a round trip.
    #[doc(hidden)] // pub for integration tests
    pub fn rd_auto_start_with(
        &self,
        group: &GroupId,
        runner: &dyn rddrive::RdRunner,
        agent_id: &str,
        pr_ref: &str,
        report: &str,
        now: u64,
    ) -> bool {
        use rddrive::auto_start_refusal as a;
        if !self.rd_auto_drive_on_done(group) {
            return false;
        }
        let decline = |reason: &str, pr: Option<u64>| -> bool {
            self.rd_audit(
                group,
                agent_id,
                rddrive::audit_action::AUTO_START_DECLINED,
                json!({ "agent": agent_id, "ref": rd_fact(pr_ref), "pr": pr, "reason": reason }),
            );
            false
        };
        let Some(pr) = rddrive::pr_from_ref(pr_ref) else { return decline(a::NOT_A_PR, None) };
        let Some(agent) = self.agent(agent_id) else { return decline(a::NOT_AUTHOR, Some(pr)) };
        let Some(session) = agent.session_id.clone().filter(|s| !s.trim().is_empty()) else {
            return decline(a::NO_SESSION, Some(pr));
        };
        // **Live OR parked**, which is wider than `drive_review`'s own
        // `already-driven`: that tool RESUMES a parked drive, and a resume is
        // the orchestrator's decision about a hold it has read (§2.3) — never
        // something a worker's report may do on its behalf.
        match reviewdrive::load_state(&self.group_dir(group)) {
            Ok(s) if s.entry(pr).is_some_and(|e| !e.state().is_terminal()) => {
                return decline(a::ALREADY_DRIVEN, Some(pr));
            }
            Ok(_) => {}
            Err(_) => return decline(rddrive::refusal::STATE_UNREADABLE, Some(pr)),
        }
        // **INVARIANT 9's guard** (§3.2). A drive started here starts its
        // counters at zero, which is true only of a PR nobody has reviewed. A
        // terminal drive's entry is pruned once its notice lands, so without
        // this a worker reporting done AFTER a satisfied gate would start a
        // fresh drive with a fresh three rounds — "yours count too" defeated
        // by a report. A recorded verdict is the durable evidence that a round
        // was spent, whoever spent it.
        if !self.verdict_map(group, pr).is_empty() {
            return decline(a::HAS_VERDICTS, Some(pr));
        }
        let identity = match rddrive::pr_identity(runner, pr) {
            Ok(i) => i,
            Err(reason) => return decline(reason, Some(pr)),
        };
        if !identity.open {
            return decline(a::PR_NOT_OPEN, Some(pr));
        }
        if rddrive::is_scratch_title(&identity.title) {
            return decline(a::SCRATCH, Some(pr));
        }
        // **Authorship is the branch orrerix recorded at spawn**, never the
        // GitHub author (every agent here pushes as the same account) and
        // never anything the report says.
        let branch = agent.branch.clone().unwrap_or_default();
        if branch.trim().is_empty() || branch.trim() != identity.head_ref {
            return decline(a::NOT_AUTHOR, Some(pr));
        }
        let Some(orch) = self
            .agents
            .lock_safe()
            .values()
            .find(|x| &x.group == group && x.role == Role::Orchestrator && x.status != AgentStatus::Dead)
            .map(|x| x.id.clone())
        else {
            return decline(a::NO_ORCHESTRATOR, Some(pr));
        };
        // The text the drive's first notice will carry: the pane's own line,
        // minus the marker a notice already opens with — nesting a second
        // `[orrerix]` mid-line would read as a forged one.
        //
        // **Capped at `RD_FACT_CAP`, one paragraph** (rev-std premortem on
        // #3371): the legacy `summary` path is scrubbed but uncapped, and this
        // text is persisted on the entry — rewritten whole on every drive write
        // for the drive's life — and copied into the audit row. The pane line a
        // delivered report would have produced is not bounded here; the record
        // this one is persisted into is.
        let text = rd_fact(report.trim_start_matches("[orrerix]").trim());
        let out =
            self.drive_review_seeded(group, runner, pr, &session, false, 0, &orch, now, Some(text.clone()));
        if let Some(reason) = out.get("refused").and_then(Value::as_str) {
            return decline(reason, Some(pr));
        }
        self.rd_audit(
            group,
            &orch,
            rddrive::audit_action::AUTO_STARTED,
            json!({ "pr": pr, "agent": agent_id, "session": session, "branch": branch,
                    "report": text }),
        );
        true
    }

    /// `cancel_review_drive(pr)` — one of `held`'s two outgoing arcs, and the
    /// only way an orchestrator stops a live drive short of `satisfied`.
    ///
    /// Not the only way one REACHES `cancelled`: a drive whose PR reads closed
    /// is cancelled on the tick that observes it, with no tool call at all,
    /// which is why [`CancelCause::PrGone`] exists.
    #[doc(hidden)] // pub for integration tests
    pub fn cancel_review_drive(&self, group: &GroupId, pr: u64, on_behalf_of: &str) -> Value {
        self.cancel_review_drive_with(group, pr, on_behalf_of, now_ms())
    }

    /// `cancel_review_drive` with the clock injected — `drive_review` /
    /// `drive_review_with`'s twin convention, and here for that pair's exact
    /// reason (#1857).
    ///
    /// **A bound measured against a clock a test cannot set is a bound no test
    /// can perform.** This function used to stamp [`OwedNotice::owed_ms`] with
    /// the wall clock while the retention ceiling was measured from it by a
    /// tick running on the caller's `now`, so a test on a synthetic clock
    /// compared a small `now` against a wall-clock anchor, `saturating_sub`
    /// answered zero, and the ceiling could never fire on this path — the one
    /// thing the ceiling promises, a documented counterfactual rather than a
    /// pinned one. #1841's B2 shipped out of the same shape one function over:
    /// "the clock is the caller's, and that is what makes the age bound
    /// testable at all".
    ///
    /// **Since #3040 N1 this path owes no notice at all**, so the ceiling has
    /// nothing to fire on here and the seam earns its keep for the other half
    /// of what it always did: `now` is what the flush below prunes and audits
    /// on. The ceiling itself is pinned on the `PrGone` producer, which still
    /// owes one.
    ///
    /// The only production caller passes `now_ms()`, so nothing about live
    /// behaviour changes; what it buys is that a test can drive this tool and
    /// the tick on one clock — which is what
    /// `a_tool_cancel_audits_and_delivers_nothing` needs to assert the prune,
    /// as `the_ceiling_fires_on_a_tool_cancelled_notice_too` needed it to
    /// assert the ceiling before N1 took this path's notice away.
    #[doc(hidden)] // pub for integration tests
    pub fn cancel_review_drive_with(
        &self,
        group: &GroupId,
        pr: u64,
        on_behalf_of: &str,
        now: u64,
    ) -> Value {
        use rddrive::refusal as r;
        if !self.driver_enabled(group) {
            return self.rd_refuse(group, pr, r::DRIVER_DISABLED);
        }
        let dir = self.group_dir(group);
        // #1871 B3: read out of the entry BEFORE the lock is dropped, and it is
        // the ONLY thing that survives it. A cancel is the one exit whose caller
        // is a tool rather than a notice, so the panes have to reach two places —
        // the notice, and this tool's own return value, which is what an
        // orchestrator acts on without waiting for a prompt to arrive.
        let panes: Vec<(String, reviewdrive::DrivenRole)>;
        // The notice this cancel will AUDIT rather than deliver (#3040 N1),
        // built inside the lock where the panes are and written outside it.
        let demoted: Option<String>;
        {
            let _state_guard = self.rd_state_lock.lock_safe();
            let mut state = match reviewdrive::load_state(&dir) {
                Ok(s) => s,
                // **NOT `not-driven`.** A torn file cannot tell you a PR is not
                // driven; it can only tell you orrerix cannot say. §5.1 gives
                // this its own name for exactly the confusion the queue's own
                // contract uses capitals to prevent.
                Err(_) => return self.rd_refuse(group, pr, r::STATE_UNREADABLE),
            };
            let live = state.entry(pr).map(|e| !e.state().is_terminal()).unwrap_or(false);
            if !live {
                return self.rd_refuse(group, pr, r::NOT_DRIVEN);
            }
            let Some(entry) = state.entry_mut(pr) else {
                return self.rd_refuse(group, pr, r::NOT_DRIVEN);
            };
            if entry.advance(reviewdrive::DriveState::Cancelled, None, None, now).is_err() {
                return self.rd_refuse(group, pr, r::STATE_UNREADABLE);
            }
            // Both sides are wanted here — this hunk's base side is EMPTY, so
            // it is add/add rather than the replace-vs-augment above: #1871 B3
            // needs the panes out before the lock drops, and #1857 needs the
            // notice built and owed before the store.
            panes = entry.owned_panes();
            // **DEMOTED to the audit log, not owed to a pane** (#3040 N1).
            // This is the one cancel the orchestrator ASKED for: it is holding
            // this call's return value, which carries the panes and the
            // cancellation, so a prompt arriving later says nothing it does
            // not already have. That is #533-B's `exit_notice_route` test
            // exactly — an event this process was asked to perform is audited,
            // one nobody asked for still interrupts — and `CancelCause::PrGone`
            // (reconcile, or a tick finding the PR closed) is still announced,
            // still owed on the entry, and still bounded by the retention
            // ceiling.
            //
            // The notice is still RENDERED, and the row carries it, because
            // "read it on demand" is only a real path if the text exists to
            // read (#1857's whole argument, kept). What #1857 bought on this
            // path — an obligation that survives a pane that is down — is not
            // lost so much as no longer needed: nothing can fail to deliver a
            // line that is not delivered, and the audit write is the same
            // durable record its retry existed to protect.
            //
            // **The released-worker session stays empty, and that is #2811 S1's
            // reason rather than this one's**: a tool cancel is not a tick, so
            // no step is decided, nothing is released, and the orchestrator
            // that called it is the party disposing of the panes. The demotion
            // changes where the line goes, never what it says.
            // #3367 item 2: a tool cancel of an auto-started drive that never
            // announced anything is its first notice too. It is demoted to the
            // audit log below, so the report lands there rather than nowhere.
            let notice = Self::rd_fold_auto_report(
                entry,
                rddrive::cancelled_notice(pr, rddrive::CancelCause::Tool, &panes, ""),
            );
            demoted = Some(notice);
            if reviewdrive::store_state(&dir, &state).is_err() {
                return self.rd_refuse(group, pr, r::STATE_UNWRITABLE);
            }
        }
        self.rd_signals.lock_safe().remove(&(group.clone(), pr));
        // #2811 S10: cancelled is terminal, so nothing is owed a re-brief.
        // **This is the reachable one**: the tool takes no tick, so the tick's
        // own spend never runs and the mark would otherwise stand. Pinned by
        // `a_cancelled_drive_does_not_leave_a_restart_mark_for_the_next_one`.
        self.rd_forget_restart_mark(group, pr);
        self.rd_audit(
            group,
            on_behalf_of,
            rddrive::audit_action::CANCELLED,
            json!({ "pr": pr, "panes": panes.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>() }),
        );
        // **The demoted notice, written where it can be read on demand**
        // (#3040 N1) — after the store, because the row is a claim about a
        // cancel that HAPPENED, and outside the lock, because the audit sink
        // is not this lock's subject.
        if let Some(notice) = demoted {
            self.rd_audit(
                group,
                on_behalf_of,
                rddrive::audit_action::NOTICE_DEMOTED,
                json!({ "pr": pr, "reason": "tool-cancel", "notice": notice }),
            );
        }
        // The same flush the tick runs, so this tool has exactly one delivery
        // path rather than a second one that would have to be kept in step. It
        // also prunes: a cancel whose notice lands is an entry that leaves
        // here, which is what §5.2 already promised. Since #3040 N1 this path
        // owes NO notice, so what the flush does here is the prune — and it
        // still runs the same one function the tick does, which is the point.
        //
        // `now`, not `now_ms()`: the flush runs the retention ceiling, and a
        // ceiling measured against a different clock from the anchor above is
        // the untestable bound this seam exists to close (#1857).
        let _ = self.rd_flush_notices(group, &dir, now);
        // **The panes ride in the RESULT, and since #3040 N1 that is the ONLY
        // place this cancel puts them in front of its caller.** #1871 B3
        // argued the result from "a notice whose delivery fails is lost", and
        // #1857 answered that by owing the notice on the entry; N1 then
        // demoted this path's notice to the audit log altogether, on the
        // ground the result makes true — this is the one exit whose caller is
        // holding a return value at the moment the panes stop being anyone's,
        // synchronously, where a notice is a prompt that arrives whenever the
        // pane next drains. `CancelCause::PrGone` has no such caller and is
        // still announced.
        json!({
            "cancelled": true,
            "panes": panes
                .iter()
                .map(|(agent, role)| json!({
                    "agent": agent,
                    "role": match role {
                        reviewdrive::DrivenRole::Worker => "worker".to_string(),
                        reviewdrive::DrivenRole::Lane(b) => b.clone(),
                    },
                }))
                .collect::<Vec<_>>(),
        })
    }

    /// Mark an agent `Dead`, so the liveness prune's DEAD side can be reached
    /// from a test (#1871 B2, rev-final).
    ///
    /// **Not a kill path, and deliberately unable to become one.** `kill_agent`
    /// needs a bound pty and performs a real `PtyManager::kill`; a
    /// driver-spawned pane in this harness has neither, so the state this
    /// predicate turns on is otherwise unreachable from an integration test.
    /// This sets the one field the predicate reads and touches nothing else — no
    /// initiator stamp, no exit notice, no pty — so it cannot stand in for
    /// `kill_agent` in a test that means to exercise killing, and a reader
    /// cannot mistake it for the production route.
    ///
    /// Answers whether an agent by that id existed to mark.
    #[doc(hidden)] // pub for integration tests
    pub fn mark_agent_dead_for_test(&self, agent_id: &str) -> bool {
        let mut agents = self.agents.lock_safe();
        match agents.get_mut(agent_id) {
            Some(a) => {
                a.status = AgentStatus::Dead;
                true
            }
            None => false,
        }
    }

    /// Corrupt this group's drive record, so the FAULT paths that read it can
    /// be exercised from outside the crate.
    ///
    /// **It hands out no path**, which is the whole point. CLAUDE.md constraint
    /// 6 keeps `group_dir_at` the single join and keeps it private, and a
    /// `group_dir_for_test` returning a `PathBuf` would hand every future test
    /// exactly the thing that rule exists to withhold. This takes a validated
    /// `GroupId`, writes a fixed payload, and answers whether it wrote — so a
    /// test can reach "the record exists and cannot be parsed" without ever
    /// reaching the directory. The payload is not a parameter for the same
    /// reason: a caller that can choose the bytes is a caller that can write a
    /// VALID record, which is a state seeder rather than a fault injector.
    #[doc(hidden)] // pub for integration tests
    pub fn corrupt_drive_record_for_test(&self, group: &GroupId) -> bool {
        let path = reviewdrive::state_path(&self.group_dir(group));
        std::fs::write(path, b"{ not json").is_ok()
    }

    /// `review_drive_status()` — the surface a **compacted** orchestrator
    /// recovers its drives from, which is why §5.1 puts it in the re-sync list
    /// beside `list_tasks`, `list_agents` and `get_state`.
    ///
    /// **It does not list terminal entries**, exactly as `merge_queue_status`
    /// does not: they would flow into the orchestrator's resident context, which
    /// is the cost this whole feature exists to remove. Parked entries ARE
    /// listed — a `held` drive is the one thing an orchestrator most needs to
    /// see, and §5.2 never prunes one.
    /// **The clock is the caller's**, for `cancel_review_drive_with`'s reason,
    /// one function over: every figure below is DERIVED from `now`, and a status
    /// view reading the wall clock while the tick decides on an injected one puts
    /// the two on different scales. `state_ms` and `since_ms` then come back in
    /// wall units against anchors stamped in the test's units, so a test can
    /// assert nothing about them except where the wall terms happen to cancel —
    /// which is how #2110's first published figures were written, and they were
    /// wrong in the direction that reads as passing. B2 shipped out of exactly
    /// this shape.
    ///
    /// The only production caller passes `now_ms()`, so nothing about live
    /// behaviour changes.
    #[doc(hidden)] // pub for integration tests
    pub fn review_drive_status(&self, group: &GroupId) -> Value {
        self.review_drive_status_with(group, now_ms())
    }

    #[doc(hidden)] // pub for integration tests
    pub fn review_drive_status_with(&self, group: &GroupId, now: u64) -> Value {
        let enabled = self.driver_enabled(group);
        let dir = self.group_dir(group);
        let state = {
            let _state_guard = self.rd_state_lock.lock_safe();
            match reviewdrive::load_state(&dir) {
                Ok(s) => s,
                // The same distinction the two mutating tools make: "orrerix
                // cannot read the record" is not "there is nothing in it".
                Err(_) => {
                    return json!({ "enabled": enabled,
                                   "refused": rddrive::refusal::STATE_UNREADABLE })
                }
            }
        };
        let drives: Vec<Value> = state
            .entries
            .iter()
            .filter(|e| !e.state().is_terminal())
            .map(|e| {
                json!({
                    "pr": e.pr,
                    "state": e.state().as_str(),
                    "held_reason": e.held_reason.map(|r| r.as_str()),
                    "head": e.head,
                    "lanes": e.lanes.iter().map(|l| json!({
                        "block": l.block,
                        "last_verdict": l.last_verdict.map(|v| v.as_str()),
                    })).collect::<Vec<_>>(),
                    "counters": {
                        "review_rounds": e.counters.review_rounds,
                        "ci_attempts": e.counters.ci_attempts,
                        "rebase_attempts": e.counters.rebase_attempts,
                    },
                    // #2509. Published beside the counters rather than inside
                    // them, because it is not one: `review_rounds` is what this
                    // drive has spent of INVARIANT 9's budget and stops at the
                    // bound, while this says whether the one extra round outside
                    // that budget is still available. An orchestrator reading
                    // `review_rounds: 3` of 3 on a LIVE drive is looking at the
                    // grace, and this is the field that says so.
                    "grace_used": e.counters.body_only_grace,
                    // #3367 item 1: the non-blocking rounds the driver ran on
                    // its own. Beside `review_rounds` rather than inside it for
                    // `grace_used`'s reason — each of these is ALSO counted
                    // there, so this says how many of that figure nobody
                    // dispositioned.
                    "nit_rounds": e.nit_rounds,
                    // Derived, never stored: a stored AGE is stale the instant
                    // it is written and meaningless across a restart, which is
                    // the queue's own split between `enqueued_ms` and
                    // `status_view`'s `since_ms`.
                    //
                    // **`since_ms` stays the WALL age and the exclusion is
                    // published beside it** (#2110), rather than being netted
                    // off inside it. A human asking how long a drive has been
                    // going wants the wall figure, and an age that silently
                    // shrank when a cap cleared would be a worse answer than
                    // the one it replaced; what the backstop measures is
                    // `since_ms - starved_ms`, which is checkable here rather
                    // than inferable. `state_ms` is the clock that actually
                    // bounds a working drive, and `held_state`/`held_state_ms`
                    // are what it was doing when a bound fired — the third
                    // bullet of #2110, so a resume is a decision.
                    "since_ms": e.age_ms(now),
                    "starved_ms": e.starved_ms(now),
                    "state_ms": e.state_elapsed_ms(now),
                    "held_state": e.held_from.map(|s| s.as_str()),
                    "held_state_ms": e.held_from.map(|_| e.held_after_ms),
                })
            })
            .collect();
        json!({ "enabled": enabled, "drives": drives })
    }

    /// One refusal, audited then returned — so `rd-refused` and what the caller
    /// was told cannot come apart.
    fn rd_refuse(&self, group: &GroupId, pr: u64, reason: &'static str) -> Value {
        self.rd_audit(
            group,
            "",
            rddrive::audit_action::REFUSED,
            json!({ "pr": pr, "reason": reason,
                    "orrerix_fault": rddrive::refusal::is_orrerix_fault(reason) }),
        );
        json!({ "refused": reason })
    }

    /// [`rd_refuse`](Self::rd_refuse), with the sentence that says why — the
    /// quoted refusal a hold would have carried (#1961's rule, at the call).
    /// The `reason` stays a closed-vocabulary name an agent branches on; the
    /// detail rides beside it, the same shape the `rd-held` row gives a hold's
    /// refusal.
    fn rd_refuse_detail(&self, group: &GroupId, pr: u64, reason: &'static str, detail: &str) -> Value {
        self.rd_audit(
            group,
            "",
            rddrive::audit_action::REFUSED,
            json!({ "pr": pr, "reason": reason, "detail": detail,
                    "orrerix_fault": rddrive::refusal::is_orrerix_fault(reason) }),
        );
        json!({ "refused": reason, "detail": detail })
    }
}
