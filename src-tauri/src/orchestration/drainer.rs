//! A pane queue's drainer thread (`run_queue_drainer`), its guard and first
//! attempt, and the admit/deliver outcomes it reports.
//! Design note: `docs/design/delivery-triage.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): `tauri`
//! (`AppHandle`/`Emitter`), `PtyManager`, `OrchRegistry`, `crate::pty`,
//! `lock_safe` (`crate::obs`). IO: threads/sleep. Sibling files it calls:
//! `agentmodel.rs`, `holds.rs`, `noticemask.rs`, `questionhold.rs`,
//! `strandedpolicy.rs`, `submit.rs`.

use super::*;

// ─────────────────────────── #445: delivery queue ───────────────────────────
//
// "Hold means queued, never doomed." `deliver_prompt`'s three hold-cap seams
// used to DESTROY the payload once their bounded wait expired (see
// `queue.rs`'s module doc for the full argument). The fix keeps the caps —
// they bound *thread blocking*, a legitimate concern — but changes what
// happens AT the cap: enqueue, not destroy. This section is the impure half
// (queue map, drainer thread); `queue.rs` is the pure policy.

/// What admitting a payload into a pane's queue (`OrchRegistry::enqueue_text`)
/// resolved to (#470): `id` for audit/removal purposes, and `was_first` —
/// whether the queue was empty immediately before this admission, decided
/// atomically with the push. See `enqueue_text`'s doc for why `was_first`
/// is the one fact the whole ordering fix hangs off of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmitOutcome {
    pub id: u64,
    pub was_first: bool,
    /// #517: this admission collapsed into an ALREADY-QUEUED byte-identical
    /// entry (`queue::AdmitDecision::Coalesce`) rather than adding one.
    /// Reported out of the same critical section that decided it, so a
    /// caller that must not act twice on one payload — the lost-kickoff
    /// re-delivery — can tell "queued" from "already queued" without a
    /// second, racy look at the queue.
    pub coalesced: bool,
}

/// The part of a delivery target's identity that outlives a loomux restart
/// (#467) — resolved once at admission by `OrchRegistry::durable_target` and
/// stamped onto the queued entry, because at recovery time the agent it came
/// from may not exist. See `queue::QueuedDelivery`'s `to_orchestrator` /
/// `session_id` fields for why the obvious keys (`pty_id`, `agent_id`) are
/// both re-minted by a restore and therefore cannot serve.
#[derive(Clone, Debug, Default)]
pub(in crate::orchestration) struct DurableTarget {
    pub(in crate::orchestration) is_orchestrator: bool,
    pub(in crate::orchestration) session_id: Option<String>,
}

/// What `deliver_now` reports back to its caller — always `run_queue_drainer`
/// as of #470 (the front door no longer calls `deliver_now` directly; see
/// its doc). `Done` means nothing is left to queue; the two `Aborted*`
/// variants mean the entry stays exactly where it already sits at the front
/// of the queue (#470: every entry is admitted BEFORE its first delivery
/// attempt, unlike pre-#470 where a fresh delivery's abort had to enqueue
/// something that wasn't in the queue yet) — the drainer's own match on the
/// outcome decides whether to retry in place, convert to a
/// `StrandedSubmit` marker, or pop and move on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::orchestration) enum DeliverOutcome {
    /// Ran to a terminal state that is NOT a hold-cap abort: delivered and
    /// resolved by #112's three-state machinery (Confirmed/Pending/Failed),
    /// or the pty closed mid-delivery. Either way nothing is left to queue
    /// — a closed pty has nowhere to deliver to, and a completed attempt
    /// already entered #451's own (untouched) confirmation lifecycle.
    Done,
    /// A pre-paste hold (box-occupied or question) capped out — nothing was
    /// pasted. The exact text this call was given still needs queuing.
    AbortedPrePaste(queue::EnqueueReason),
    /// A pre-Enter gate declined (seam 3): the text WAS pasted, only the Enter
    /// was withheld. `record_aborted_preenter_outcome` has already run (so the
    /// NEXT thing to touch this pane's box sees it as stranded) — the caller's
    /// job is to queue a `StrandedSubmit` marker, never the text again.
    ///
    /// **Carries its reason since #532 (rev-12 NB1).** This used to have one
    /// cause — the question hold — so the drainer hardcoded
    /// `EnqueueReason::Question` in the notice it sends. #532 gave the path a
    /// second cause (the pre-Enter occupancy gate), which made that notice tell
    /// the orchestrator "an interactive question is on screen" for a pane whose
    /// only blocker was the human's own half-typed line — sending it to look
    /// for a dialog that does not exist. Mirrors `AbortedPrePaste`, which has
    /// carried its reason all along, and closes the same mislabel
    /// `write_admission_badges_the_gate_that_actually_blocked` exists to
    /// prevent.
    AbortedPreEnter(queue::EnqueueReason),
    /// #813: a `StrandedSubmit` marker was dropped without pressing anything —
    /// see `stranded_marker_action`. Its own variant rather than `Done`
    /// because `Done`'s whole meaning is that the pane took a write, and this
    /// pane did not: reusing it would make `HoldObservation::Delivered` — "the
    /// pane accepted a write" — a false claim in the audit trail, which is the
    /// unbacked-claim class this module mints separate variants to avoid
    /// (`QuestionStale` vs `Question`, `QueueFull` vs `Exhausted`).
    Retired(StrandedRetireReason),
}

/// The delivery-queue drainer (#445, generalized by #470 into the ONLY way
/// anything ever reaches `deliver_now`): one per pane with a non-empty
/// queue, spawned by `OrchRegistry::ensure_drainer`. Polls deliverability
/// at `QUEUE_DRAIN_POLL` — no cap, no timeout, matching the standing
/// requirement that a hold whose release condition is "a human answers"
/// must not expire — and replays the queue's FRONT entry the instant the
/// pane is deliverable. Exits when the queue is empty, the pty closes, or
/// the agent dies (dropping any remaining entries with an audit line +
/// notice — today that case is silent).
///
/// **Ownership discipline** (what makes ordering race-free, #470). Only
/// THIS thread ever pops from the FRONT of a given pane's queue.
/// `deliver_prompt`'s front door only ever pushes to the BACK, and — as of
/// #470 — EVERY delivery goes through that same push, atomically with the
/// emptiness check that decides whether it's the one to spawn this thread
/// (`enqueue_text`'s `was_first`) or whether it's landing behind an
/// existing queue a drainer is already (or about to be) working through.
/// There is no longer a separate "race a raw mutex, bypass the queue
/// entirely" path for a later arrival to use to cut ahead — see
/// `docs/design/orchestration.md`'s Ordering subsection for the argument
/// this closes (a plain fair mutex does NOT: a reviewer proved a delivery
/// deferred at its OWN paste-point recheck can still lose its arrival
/// position to a later arrival that queued via a bypass the fair lock never
/// touched).
///
/// **Zero added latency for the common case (#470).** This thread's FIRST
/// pass never sleeps and never pre-checks deliverability — it calls
/// `deliver_now` immediately, exactly as the pre-#470 direct-spawn path
/// did, and `deliver_now`'s own internal waits (bounded, with the same caps
/// as ever) do the real work. Every later pass (a retry, or a second entry
/// that piled up behind the first) uses the normal poll cadence, exactly as
/// before #470.
///
/// **Panic safety (rev-35 review, NB2).** Removal from `queue_draining` is
/// an RAII guard (below), not a manual call at each exit — a panic
/// anywhere in a drain attempt (including inside `deliver_now`, which this
/// thread calls directly, unguarded by `catch_unwind`) used to leave the
/// pty latched in `queue_draining` forever, permanently blocking every
/// future `ensure_drainer` call for that pane. The guard's `Drop` runs on
/// unwind exactly like it does on a normal `return`.
///
/// **Generation-checked, not unconditional (#470 B1 review round 2).** A
/// committed exit (`OrchRegistry::commit_exit`) ALREADY deregisters
/// `pty_id` atomically with confirming the queue is empty — this guard
/// still runs afterwards regardless (it has no way to know commit_exit
/// already acted; RAII fires on every `return`, committed or not). If its
/// `Drop` unconditionally removed `pty_id` again, it could erase a
/// SUCCESSOR drainer's live registration: a fresh delivery can arrive,
/// `was_first: true`, spawn Drainer2, and register — all in the window
/// between Drainer1's `commit_exit` and Drainer1's OWN guard dropping —
/// and an unconditional second removal would strip Drainer2's
/// registration out from under it while it's still running (possibly
/// mid-`deliver_now`, a 60–120s hold), letting a THIRD arrival spawn
/// Drainer3 concurrently with Drainer2 — two live drainers walking the
/// same queue, the same entry pasted twice. This is why `generation`
/// exists: `commit_exit` and this `Drop` both remove `pty_id` ONLY if the
/// CURRENTLY stored generation still matches the one THIS drainer was
/// minted at spawn (`ensure_drainer`). Whichever of them acts first wins
/// the real removal; whichever acts second finds either nothing to remove
/// (already gone) or a DIFFERENT generation (a successor already claimed
/// it) and is a no-op either way — structurally, not because every call
/// site remembered to "arm"/"disarm" anything. See
/// `unified_admission_property::drainer_lifecycle` (queue.rs) for the
/// exhaustive proof, including the stale-guard-drop event modeled
/// explicitly rather than assumed away.
///
/// **#497: the generation check is now the only removal that exists.**
/// `queue_draining` is a [`queuestate::DrainerRegistry`], whose sole
/// removal takes the generation — so the paragraph above stopped being a
/// rule this `Drop` had to honour and became a rule it cannot state
/// otherwise. That matters for the case a property test structurally
/// cannot reach: `drainer_lifecycle` models the three call sites that
/// existed when it was written, so a raw removal added at a FOURTH site
/// changes nothing the model explores. See `queuestate.rs`.
struct DrainerGuard {
    queue_draining: Arc<queuestate::DrainerRegistry>,
    pty_id: u32,
    generation: u64,
}

impl Drop for DrainerGuard {
    fn drop(&mut self) {
        // Whether this fired or found a successor's generation is not this
        // guard's business — both are correct outcomes, which is the point
        // of the generation (see the type doc above).
        self.queue_draining.release(self.pty_id, self.generation);
    }
}

/// The kickoff-specific behavior (`Delivery::wait_ready`/
/// `confirms_autopilot_dialog`) a FRESH delivery resolved at
/// `deliver_prompt`'s front door, threaded through to this drainer's very
/// first pass ONLY (#470). Every pre-#470 replay already hardcoded
/// `false, false` here — a queued entry, by the time anything replays it,
/// is never "the first prompt to a just-booted CLI" in the sense that
/// matters; this preserves that exactly for iteration 2+, and restores it
/// for iteration 1 where #470's unification would otherwise have silently
/// dropped it (a freshly spawned drainer used to only ever mean "replaying
/// a hold-cap timeout," never "the very first attempt").
///
/// `id` guards against the (purely theoretical — see `ensure_drainer`'s
/// doc) race where a DIFFERENT, non-kickoff caller's `ensure_drainer` call
/// happens to win the idempotent spawn instead of the kickoff's own: if the
/// front entry on the first pass isn't this id, `wait_ready`/
/// `confirm_autopilot` are NOT applied — falling back to the always-safe
/// `false, false` rather than misapplying kickoff behavior to a different
/// delivery.
pub(in crate::orchestration) struct FreshFirstAttempt {
    pub(in crate::orchestration) id: u64,
    pub(in crate::orchestration) wait_ready: bool,
    pub(in crate::orchestration) confirm_autopilot: bool,
    /// #517: this attempt is a FRESH spawn's kickoff brief specifically
    /// (`Delivery::FreshKickoff`), the one payload with no other route to
    /// the agent if it never lands. Deliberately NOT the same fact as
    /// `wait_ready`, which is also true for a resume re-sync: the boot wait
    /// is about "hold the paste", this is about "the payload is
    /// unrecoverable". See `kickoff_recovery_action`.
    pub(in crate::orchestration) fresh_kickoff: bool,
}

/// The ONE place a decided `KickoffTreatment` is copied onto the attempt it
/// arms (#620 review NB1).
///
/// `KickoffTreatment`'s own doc argues these three flags are "the same type
/// and mean opposite things, so a positional tuple is one transposed edit
/// away from arming the autopilot watcher on a recovery" — and then every
/// producer copied them across field by field, by hand. A transposition in
/// one of those copies is invisible: the treatment functions still return the
/// right answer (so their tests pass), `kickoff_panes` is derived from
/// `is_some()` (so the audit test passes), and only the drainer — private
/// struct, needs a real `AppHandle`, unreachable by every test in this repo —
/// sees the wrong flags. The mapping cannot be made testable, so it is made
/// singular instead: #620 established that this treatment gains producers,
/// and each new one now inherits the hazard rather than re-creating it.
impl From<(u64, KickoffTreatment)> for FreshFirstAttempt {
    fn from((id, t): (u64, KickoffTreatment)) -> Self {
        FreshFirstAttempt {
            id,
            wait_ready: t.wait_ready,
            confirm_autopilot: t.confirm_autopilot,
            fresh_kickoff: t.fresh_kickoff,
        }
    }
}

pub(in crate::orchestration) fn run_queue_drainer(
    reg: Arc<OrchRegistry>,
    app: AppHandle,
    group: GroupId,
    pty_id: u32,
    fresh_first: Option<FreshFirstAttempt>,
    // #470 B1 review round 2: the generation `ensure_drainer` minted for
    // THIS spawn — threaded through so both this thread's own
    // `DrainerGuard` and its `commit_exit` calls remove `queue_draining`'s
    // entry only if it's still THIS generation. See `DrainerGuard`'s doc.
    generation: u64,
) {
    let _draining_guard = DrainerGuard { queue_draining: reg.queue_draining.clone(), pty_id, generation };
    let ptys = app.state::<crate::pty::PtyManager>();
    // #470: suppressed until real contention is observed (a retry, or more
    // than one entry queued) — see the loop body below. Pre-#470, a drainer
    // only ever existed BECAUSE something already needed to queue, so
    // showing it unconditionally on the first successful send was correct;
    // #470 also spawns this thread for the common, uncontended, zero-hold
    // case, which must never claim anything was "queued while blocked."
    let mut header_pending = false;
    // #563: which reason THIS drainer currently has the pane-header
    // delivery-held chip up for, or `None` for no chip. Owned here rather than
    // in `held_escalation` for the reason `HeldEscalation::Chip`'s doc gives:
    // "is the pane held" is a fact about the pane, "have we said so, and as
    // what" is drainer state. Kept per-drainer (not in the registry) because a
    // drainer is per-pty and its exit is exactly when the chip must come down —
    // see the `ChipGuard` below, which drops it on EVERY exit from this
    // function, including the early returns for a closed pty or a dead agent.
    //
    // **The REASON, not just a bool (rev-10 finding 3).** A bool made the
    // reason freeze at whatever the first blocked poll saw, for the whole
    // episode. The two gates genuinely alternate mid-hold — that alternation is
    // the documented behaviour `PREPASTE_RECHECK_ROUNDS` exists for — and the
    // two chips read materially differently ("submit or clear the text in this
    // pane's box" vs "an interactive question is on screen, answer it"). A
    // frozen reason can tell a human to clear an empty box while a dialog is
    // what is actually waiting, which is a false claim on the very badge this
    // PR added to stop false silence.
    //
    // (a `Cell`, purely so the RAII guard below can share it with the loop
    // body — nothing here is concurrent.)
    let chip_reason: std::cell::Cell<Option<HeldReason>> = std::cell::Cell::new(None);
    // #563: a chip left up by a drainer that exited would be a permanent lie
    // on a pane nothing is holding — the mirror image of the bug this fixes,
    // and worse, because a stale "held" chip trains a human to ignore the real
    // one. RAII rather than a clear before each `return`: this function has
    // four exits and a future edit would add a fifth.
    struct ChipGuard<'a> {
        app: &'a AppHandle,
        pty_id: u32,
        shown: &'a std::cell::Cell<Option<HeldReason>>,
    }
    impl Drop for ChipGuard<'_> {
        fn drop(&mut self) {
            if self.shown.get().is_some() {
                let _ = self
                    .app
                    .emit("orch-delivery-held-cleared", delivery_held_cleared_event(self.pty_id));
            }
        }
    }
    let _chip_guard = ChipGuard { app: &app, pty_id, shown: &chip_reason };
    // Raise/lower the chip. `raise` is called on every held poll and emits only
    // when the chip is not already up FOR THIS REASON, so a pane held for hours
    // on one gate produces one event rather than one every `QUEUE_DRAIN_POLL`,
    // while a pane whose blocking gate actually changes re-words its chip. The
    // frontend's `setHeld` overwrites, so a re-emit needs no paired clear.
    let raise_chip = |agent_id: &str, reason: HeldReason| {
        if chip_reason.get() != Some(reason) {
            chip_reason.set(Some(reason));
            let _ = app.emit(
                "orch-delivery-held",
                delivery_held_event(agent_id, &group, pty_id, reason),
            );
        }
    };
    let lower_chip = || {
        if chip_reason.get().is_some() {
            chip_reason.set(None);
            let _ = app.emit("orch-delivery-held-cleared", delivery_held_cleared_event(pty_id));
        }
    };
    let mut iteration = 0u32;
    // #903: consecutive polls whose FRESH composed-screen read showed this pane
    // sitting at an empty input prompt. Drainer-local for the same reason
    // `chip_reason` is — a drainer is per-pty and its exit is exactly when the
    // observation stops being about anything — and a plain counter rather than a
    // timestamp because what the override needs is "still idle now", proven
    // repeatedly, not "was idle at some point".
    let mut question_idle_streak = 0u32;
    // #903 rev-427 NB4: the hold episode whose override has already been
    // recorded, so the grant can repeat while the audit line does not. `None`
    // until one fires; a new episode (the pane delivered, then wedged again) is
    // a new line, which is what a reader wants.
    let mut override_audited_since: Option<u64> = None;
    loop {
        iteration += 1;
        // #903: set by this iteration's own poll (below) when the last-resort
        // override fires. Per-iteration, never carried: an override is a
        // decision about one attempt against one reading of the pane.
        let mut question_overridden = false;
        // #470: the very first pass of a freshly spawned drainer attempts
        // immediately — no poll sleep, no outer deliverability pre-check —
        // exactly matching the pre-#470 direct path's latency for the
        // common uncontended case. `deliver_now`'s own internal waits are
        // the real (and only) gate; this outer pair exist purely to avoid
        // re-entering `deliver_now`'s setup on every 2s poll once genuine
        // contention is already known, which iteration 1 never is.
        let immediate_first_pass = iteration == 1;
        if !immediate_first_pass {
            std::thread::sleep(queue::QUEUE_DRAIN_POLL);
        }

        // Pty closed: nothing left to drain to. #470 B1: `commit_exit`
        // (not the RAII guard alone) is what makes this exit atomic with
        // deregistering from `queue_draining` — see its doc.
        if ptys.output_total(pty_id).is_none() {
            if let Some(entries) = reg.commit_exit(&group, pty_id, generation, true) {
                reg.announce_dropped(&group, entries, queue::DropReason::AgentDied);
            }
            return;
        }
        let agent_id = reg.by_pty.lock_safe().get(&pty_id).cloned();
        let agent = agent_id.as_ref().and_then(|id| reg.agent(id));
        // Agent record gone or dead, but the pty somehow lingers (teardown
        // ordering) — same treatment as a closed pty.
        if agent.as_ref().map(|a| a.status == AgentStatus::Dead).unwrap_or(true) {
            if let Some(entries) = reg.commit_exit(&group, pty_id, generation, true) {
                reg.announce_dropped(&group, entries, queue::DropReason::AgentDied);
            }
            return;
        }
        let Some(a) = agent else { continue };

        // #470 B1: decide, ATOMICALLY with deregistering from
        // `queue_draining`, whether this thread may exit — BEFORE peeking
        // anything else. A plain "peek front, see None, return" (the
        // pre-fix shape) leaves a window between that peek and the
        // `DrainerGuard`'s eventual drop where a fresh `was_first`
        // admission's own `ensure_drainer` call sees this pty still
        // marked draining, no-ops, and stands no chance of ever being
        // picked up — a silently stranded delivery the sender was
        // already told `Ok` for. `commit_exit` closes that window by
        // holding `queues`'s lock across BOTH the emptiness check and the
        // `queue_draining` removal, so whichever of "a push" or "this
        // exit" the OS schedules first is fully visible to the other.
        if let Some(entries) = reg.commit_exit(&group, pty_id, generation, false) {
            debug_assert!(entries.is_empty(), "force:false only commits when the queue was already empty");
            return;
        }
        // #569: the human paused this group — the queue HOLDS. Nothing is
        // planned, nothing is pasted, no notice is spent and no hold clock
        // runs: a pause is not a stuck pane, and escalating one as though it
        // were would badge the human about a state they set themselves.
        //
        // Placed AFTER the liveness checks and `commit_exit`, which is what
        // makes those two still reachable during a pause: an empty paused
        // queue exits its drainer rather than polling for the whole pause,
        // and a pane whose agent died mid-pause has its queue dropped and
        // announced through the ordinary `AgentDied` path instead of being
        // retained forever with nothing left to look at it.
        //
        // Reached at all only because `deliver_prompt`'s pause branch is not
        // the sole way a drainer starts: `readmit_recovered` kicks one when a
        // pane rebinds after a restart, which for a group paused across that
        // restart would otherwise paste straight into the pause.
        //
        // The chip comes down because the pane is no longer held on anything
        // the human can clear from the pane — the group's paused state is
        // what is holding it, and the group UI already says so
        // (`HoldClass::GroupPaused`'s `PausedGroupUi` channel).
        if reg.is_paused(&group) {
            lower_chip();
            // Only pass 1 sleeps here (review N2). Every later pass already
            // slept at the loop top, and sleeping again would poll a paused
            // pane at half cadence — doubling resume latency for a second
            // sleep that reads as if it were doing something. Pass 1 skips the
            // loop-top sleep by design (`immediate_first_pass`), so without
            // this the gate would spin.
            if immediate_first_pass {
                std::thread::sleep(queue::QUEUE_DRAIN_POLL);
            }
            continue;
        }
        // Not empty (confirmed above) — safe to peek normally now; only
        // this single-consumer thread ever pops the front, so nothing can
        // make it empty again out from under us before we act on it.
        //
        // #533-A: the whole queue is snapshotted, not just the front,
        // because what this pass submits is now decided over the entire
        // backlog (`queue::plan_flush`) rather than one entry at a time.
        // The snapshot is a clone taken under the lock and then released:
        // an admission racing this read lands BEHIND everything planned
        // here, so the worst case is that it flushes on the next pass —
        // never that it is skipped or reordered.
        let mut entries = reg.queue_snapshot(pty_id);
        // #533-A: superseded constituents drop out BEFORE anything is
        // combined — never merged into the paste, never silently dropped
        // either (each gets its own `delivery-dropped` audit line).
        let plan = queue::plan_flush(&entries, queue::QUEUE_FLUSH_MAX_BYTES);
        if !plan.superseded.is_empty() {
            reg.drop_superseded(&group, pty_id, &plan.superseded);
            // rev-13 F4: the drop both REMOVES entries and MOVES their
            // coalesce counts onto survivors, so every number rendered
            // below — the per-constituent "+N identical repeats", the
            // single-entry header's own depth and coalesce total — must
            // come from a re-read, not from a snapshot that predates it.
            entries = reg.queue_snapshot(pty_id);
        }
        let depth = entries.len();
        let batch: Vec<queue::QueuedDelivery> = plan
            .batch
            .iter()
            .filter_map(|id| entries.iter().find(|e| e.id == *id).cloned())
            .collect();
        let front = batch.first().cloned();
        let Some(front) = front else {
            // Should be unreachable — `commit_exit` just confirmed
            // non-empty and nothing else pops. Never trust that blindly
            // on a liveness-critical path: loop again rather than assume.
            continue;
        };
        // The plan's `stranded` flag and the front entry's own payload are
        // two statements of one fact; if they ever disagree the batch would
        // be pasted as text or submitted as a marker against its own
        // content. Cheap to assert, and it keeps `plan_flush`'s contract
        // honest against a future edit to either side.
        debug_assert_eq!(
            plan.stranded,
            matches!(front.payload, queue::QueuedPayload::StrandedSubmit),
            "plan_flush's stranded flag must match the planned front entry's payload"
        );
        // #569: WHY this batch waited, which is also the header's wording.
        // Computed from the batch rather than from `reg.is_paused` — by the
        // time a pause-held entry drains the group is unpaused by definition,
        // so the live flag says nothing; the entries' own admission reasons
        // are the record of what happened to them.
        let cause = queue::flush_cause(&batch);
        if !immediate_first_pass || depth > 1 || cause == queue::FlushCause::GroupPaused {
            // Genuine contention observed: either this pass needed a retry
            // at all, or more than the one entry we started with is now
            // backed up. Sticky for the rest of this drain's lifetime.
            //
            // #569 adds the third term. A lone delivery held through a pause
            // reaches a FRESH drainer at resume, whose very first pass is
            // `immediate_first_pass` with `depth == 1` — the uncontended
            // shape — so without this it would land as a bare payload with
            // nothing saying it had been waiting since before the pause. That
            // silence is the receiver-side half of the stall #569 was filed
            // for: a report arriving with no timestamp context reads as
            // current.
            header_pending = true;
        }

        if !immediate_first_pass {
            // Visibility, never destruction (#445): a queue behind an
            // unanswered question/box is the DESIGNED case for this
            // feature, not a leak — see `queue::still_queued_notice`'s doc.
            let already_notified = reg.queue_still_notified.lock_safe().contains(&pty_id);
            // #560: the same defect the escalation clock had. `front.enqueued_ms`
            // is the FRONT entry's stamp, and a `StrandedSubmit` marker pushed
            // by `enqueue_stranded_front` makes the front the YOUNGEST entry —
            // so this notice was deferred by the very event that proves the pane
            // is stuck. `undelivered_since` takes the earlier of the pane's open
            // hold episode and the oldest entry actually queued; see its doc for
            // why BOTH terms are load-bearing.
            let oldest_entry_ms =
                entries.iter().map(|e| e.enqueued_ms).min().unwrap_or(front.enqueued_ms);
            let waiting_since =
                queue::undelivered_since(oldest_entry_ms, reg.hold_episode_since(pty_id));
            if queue::should_fire_still_queued_notice(waiting_since, now_ms(), already_notified) {
                reg.queue_still_notified.lock_safe().insert(pty_id);
                let minutes = queue::QUEUE_STILL_QUEUED_NOTICE_AFTER.as_secs() / 60;
                let target_is_orchestrator = a.role == Role::Orchestrator;
                reg.notify_queue(&group, &front.agent_id, target_is_orchestrator,
                    &queue::still_queued_notice(&front.agent_id, depth, minutes));
            }

            // #532: the same `write_admission` the paste point uses, rather
            // than a second inline spelling of the same two gates. This poll
            // is where a held delivery actually LIVES — `deliver_now` holds
            // for at most its own capped waits, but this loop re-arms them
            // with no cap — so it is also where the aggregate hold has to be
            // bounded.
            let box_pending = ptys.input_pending(pty_id).unwrap_or(false);
            // #576: THE blind spot this record exists for. This gate has no
            // `pasted_text` (the entry it is considering has written nothing
            // yet), so `mask_own_paste` cannot help — and the pane in front of
            // it is full of the PREVIOUS delivery's notices, wraps and all.
            // #820: witnessed, not merely decided. This poll is the ONLY
            // reader of the gate whose hold has no cap, so it is the one whose
            // audit has to say what it keyed on — see
            // `question_active_witnessed`.
            let reading = question_active_witnessed(
                &ptys,
                pty_id,
                None,
                // #1702: the pty->session resolution is spelled out here rather
                // than hidden inside the record read. This poll holds no
                // registry guard, which is exactly the fact the old shape made
                // impossible to check at a glance.
                reg.delivered_mask_lines(pty_id, reg.session_for_pty(pty_id).as_deref()),
            );
            let question_seen = reading.witnessed;
            let admission = write_admission(box_pending, reading.active);
            // #903: the streak the last-resort override keys on. Counted here,
            // on the poll that took the reading, and reset by ANY poll that did
            // not see an idle prompt — so it can only ever describe the pane's
            // present, never a state it was in before something repainted.
            question_idle_streak =
                if reading.idle_prompt { question_idle_streak.saturating_add(1) } else { 0 };
            // #532: the escalation decision is `held_escalation`'s, not an
            // inline `&&` chain here (rev-12 B1) — this loop is where a held
            // delivery actually LIVES, since `deliver_now` holds only for its
            // own capped waits while this re-arms them with no cap, so it is
            // also where the aggregate hold has to be bounded and reported.
            //
            // #560: and the whole step is `hold_escalation_step`'s, not eight
            // lines here, for the reason that method's doc gives: this function
            // needs a real `AppHandle`, so anything inline here is unreachable
            // by every test in this repo — which is how #560's badge churn
            // survived a review round that verified `held_escalation` itself.
            // What is left below is only the pane-header chip, which is
            // genuinely drainer-local state.
            let escalation = reg.hold_escalation_step(
                &group,
                &front.agent_id,
                pty_id,
                admission,
                depth,
                now_ms(),
                QUESTION_HOLD_STALE_AFTER.as_millis() as u64,
                // #590 L2: the pane's human-keystroke stamp, read from the same
                // `ptys` this poll already consulted for `box_pending`, so the
                // classification and the gate reading describe the same instant.
                ptys.last_user_input_ms(pty_id),
                // #820: from the SAME poll as `admission` above, for the same
                // reason `last_user_input_ms` is read there — an audit that
                // described a different instant than the decision it annotates
                // would be worse than no audit at all.
                question_seen.as_ref(),
            );
            match escalation {
                // #563: the pane is held right now — say so right now. This
                // arm is the fix: pre-#563 it did not exist, `held_escalation`
                // returned `None` for every poll inside the bound, and the
                // pane showed nothing at all until the ten-minute escalation.
                HeldEscalation::Chip(reason) => raise_chip(&front.agent_id, reason),
                HeldEscalation::Badge(_) => {
                    // #563: a hold can reach the bound having never been
                    // chipped — a drainer starting on an entry enqueued long
                    // ago evaluates its very first poll already past it. The
                    // badge must never be the ONLY thing up.
                    if let Some(reason) = admission.held_reason() {
                        raise_chip(&front.agent_id, reason);
                    }
                }
                // #563: fires on every writable poll now, and `lower_chip` is
                // guarded on our own `chip_reason`, so a pane that was never
                // chipped costs no event. #560: the BADGE no longer comes down
                // here — a writable poll is a provisional reading, not proof the
                // delivery can land, and dropping the badge on it (then
                // re-raising it on the next held poll) is symptom 1. The badge
                // now comes down where the pane proves it: `Delivered`.
                HeldEscalation::Clear => lower_chip(),
                // #560: the badge is up and the bound has elapsed — nothing to
                // change about it. The chip is still raised, because it may have
                // been lowered by a `Clear` earlier in this same episode and the
                // pane is held right now. `raise_chip` is idempotent per reason,
                // so a steady hold still emits exactly one event.
                HeldEscalation::None => {
                    if let Some(reason) = admission.held_reason() {
                        raise_chip(&front.agent_id, reason);
                    }
                }
            }
            // #903: the last resort. A question hold that has outlived
            // `QUESTION_HOLD_OVERRIDE_AFTER` on a pane whose composed screen
            // keeps showing an empty input prompt is not a question — it is the
            // detector being wrong about a pane nobody is being asked anything
            // by, and the human has already been badged about it for five
            // minutes (`QUESTION_HOLD_STALE_AFTER`). Paste — and, since this
            // grant now CARRIES to the Enter, deliver: the pre-Enter checkpoint
            // re-proves the pane on fresh reads at this same weak-idleness
            // standard (`override_enter_admits`) rather than aborting against a
            // reading the grant exists because loomux has stopped believing. It
            // was the abort that stranded the paste and wedged the queue behind
            // it; the residual that carrying leaves is named in
            // `docs/design/question-gate-authorship.md`.
            //
            // The chip is NOT lowered here: the pane is still held as far as
            // every gate is concerned, and a successful delivery lowers it below
            // on `Done` — the same place every other release does. Lowering it
            // on a provisional decision is #560's symptom 1.
            //
            // The clock is read ONCE and shared with the audit below. Two reads
            // could disagree — `note_hold` on another thread can end the episode
            // between them — and the record would then describe a hold the
            // decision was not made about, the same "different instant" defect
            // `question_seen` exists to avoid one line up.
            let override_now = now_ms();
            let held_since = reg.hold_episode_since(pty_id);
            question_overridden = question_override_admits(
                admission,
                held_since,
                override_now,
                QUESTION_HOLD_OVERRIDE_AFTER.as_millis() as u64,
                question_idle_streak,
            );
            if question_overridden {
                // Reset so a delivery that goes on to abort for some OTHER
                // reason cannot re-grant an override on every 2s poll: the pane
                // has to re-prove itself idle from scratch.
                question_idle_streak = 0;
            }
            // #903 rev-427 NB4: the GRANT may repeat — an override whose
            // delivery aborts for an unrelated reason must be able to try again,
            // or one transient abort disables the last resort for the rest of a
            // wedge, which is the failure mode this whole layer exists to end.
            // The RECORD must not: re-granting every ~6s wrote hundreds of
            // identical lines into an 8 MiB rotating log over a long hold, which
            // is how a genuinely important line becomes noise. One per hold
            // EPISODE — the same clock the bound and the badge are measured
            // against, so the log reads as one event per thing the human
            // experienced.
            if question_overridden && override_audited_since != held_since {
                override_audited_since = held_since;
                reg.audit(&group, brand::AUDIT_ACTOR, "delivery-question-override", json!({
                    "to": &front.agent_id,
                    "held_ms": held_since.map(|s| override_now.saturating_sub(s)),
                    "bound_ms": QUESTION_HOLD_OVERRIDE_AFTER.as_millis() as u64,
                    "depth": depth,
                    // What the detector was holding for, so a human reading this
                    // line can tell which signal keeps misfiring — the whole
                    // point of #820's witness, and the input to the next
                    // narrowing.
                    "matched": witness_audit(question_seen.as_ref()),
                    "reason": "the question hold outlived its bound while the pane's own \
                               screen showed an idle, empty input prompt",
                }));
            }
            if !admission.go() && !question_overridden {
                continue; // keep polling — no cap
            }
        }

        let target_is_orchestrator = a.role == Role::Orchestrator;
        let cli = reg.cli_for_agent(&a);
        let lock = reg
            .delivery
            .lock_safe()
            .entry(pty_id)
            .or_insert_with(|| Arc::new(TrackedMutex::new("delivery_pane", ())))
            .clone();
        let root = reg.root.clone();
        let reg_for_call = reg.arc();
        // #470: only iteration 1's OWN front entry ever gets kickoff
        // treatment — see `FreshFirstAttempt`'s doc for why the id match
        // matters and why every other pass is unconditionally `false, false`
        // exactly as every pre-#470 replay already was.
        // #470: also gates whether an abort below sends `queued_notice` —
        // true only for THIS entry's own very first attempt (matching the
        // pre-#470 behavior where only a FRESH direct delivery's hold-cap
        // abort ever notified; a replay's abort — including every attempt
        // after this one — stays silent, since the sender was already
        // notified once, at the moment this entry first became genuinely
        // queued, and a `BehindQueue` admission was never notified at all).
        let is_fresh_attempt = fresh_first.as_ref().is_some_and(|f| immediate_first_pass && f.id == front.id);
        let (wait_ready, confirm_autopilot, fresh_kickoff) = fresh_first
            .as_ref()
            .filter(|_| is_fresh_attempt)
            .map(|f| (f.wait_ready, f.confirm_autopilot, f.fresh_kickoff))
            .unwrap_or((false, false, false));

        let outcome = match &front.payload {
            queue::QueuedPayload::Text(text) => {
                // #533-A: when this pass's plan holds more than one entry,
                // the ENTIRE flushable backlog goes out as this one paste —
                // header, then every constituent behind its own itemization
                // banner, in queue order. Pre-#533 this sent `front` alone
                // and came back for the next entry on the following pass,
                // which cost the receiving agent one full turn per queued
                // delivery.
                // #903 B1': what the prompt record may take from this paste —
                // the entries' own texts, never the framed string built below.
                let record_contributions = record_contributions_for(&batch);
                let payload = if batch.len() > 1 {
                    let items: Vec<queue::FlushConstituent> = batch
                        .iter()
                        .filter_map(|e| {
                            e.payload.text().map(|t| queue::FlushConstituent {
                                id: e.id,
                                from: &e.from,
                                enqueued_ms: e.enqueued_ms,
                                coalesced: e.coalesced,
                                text: t,
                            })
                        })
                        .collect();
                    let rendered =
                        queue::coalesced_flush_text(&items, plan.remaining, now_ms(), cause);
                    // #632's door for this producer. `pause_suppression_notice`
                    // can assert "masks away entirely"; a flush cannot, because
                    // the constituent payloads are agent text by design and
                    // must stay unmasked. So the invariant asserted here is the
                    // exact split: everything loomux FRAMED is maskable, and
                    // the only survivors are payload rows. Written through the
                    // real `mask_loomux_notices` (via
                    // `unmaskable_framing_rows`) so it cannot drift from the
                    // rule it stands in for, and `debug_assert` for the same
                    // reason `OrchNoticeInbox::park`'s is — CI's test builds
                    // are debug, a live release session must never panic over
                    // a gate that merely holds too long.
                    debug_assert!(
                        {
                            let payloads: Vec<&str> = items.iter().map(|c| c.text).collect();
                            unmaskable_framing_rows(&rendered, &payloads).is_empty()
                        },
                        "a coalesced flush must leave only constituent payload rows unmasked \
                         (#632) — got {:?}",
                        unmaskable_framing_rows(
                            &rendered,
                            &items.iter().map(|c| c.text).collect::<Vec<_>>()
                        )
                    );
                    rendered
                } else if header_pending {
                    // #445: the flush header ("N deliveries queued ... are
                    // now delivering") rides on the front of the FIRST
                    // replayed text this drain sends, rather than as its own
                    // separate delivery — one paste, header then content,
                    // instead of an extra full echo/confirm cycle for a
                    // single notice line.
                    let coalesced = {
                        let queues = reg.queues.read();
                        queues.get(&pty_id).map(|q| q.iter().map(|e| e.coalesced as usize).sum()).unwrap_or(0)
                    };
                    format!("{}\n\n{text}", queue::flush_header_text(depth.max(1), coalesced, cause))
                } else {
                    text.clone()
                };
                // #517: the re-delivery payload is `text`, never `payload` —
                // a flush header, or the itemization banners of a coalesced
                // batch, are about THIS drain, not part of the brief.
                //
                // #533 rev-13 F2: `payload` and `text` now differ by TWO
                // independent routes, not one. The old note argued they were
                // "equal in practice" because `header_pending` is false on a
                // fresh kickoff's own first pass; the `batch.len() > 1`
                // branch above sits ahead of `header_pending` and is a
                // second route that premise does not cover. Passing `text`
                // is still right either way — that is what makes this
                // correct rather than lucky.
                //
                // The consequence #533 raises, stated rather than left
                // implicit: if `batch.len() > 1` ever coincided with a fresh
                // kickoff on iteration 1, #517's recovery would re-admit
                // only the kickoff brief while constituents 2..N had already
                // been popped as `Done` without landing — N-1 lost, where
                // pre-#533 it was at most 1. `ensure_drainer`'s doc argues
                // that coincidence is purely theoretical (a fresh spawn's
                // pty has no other deliveries to race against yet), and that
                // argument is unchanged by this PR; what changed is the
                // price if it were ever wrong, which is why it is written
                // down here instead of being left to the reader.
                let out = deliver_now(
                    app.clone(), root, group.clone(), front.agent_id.clone(), pty_id,
                    payload, front.from.clone(), confirm_autopilot, wait_ready, cli, lock,
                    reg.last_delivery.clone(), target_is_orchestrator, reg_for_call,
                    fresh_kickoff.then(|| text.clone()),
                    question_overridden,
                    record_contributions,
                );
                if matches!(out, DeliverOutcome::Done) {
                    header_pending = false;
                }
                out
            }
            queue::QueuedPayload::StrandedSubmit => {
                let _guard = lock.lock_safe();
                let submit = submit_sequence(&cli);
                match drain_stranded_submit(
                    &ptys, &reg.last_delivery, front.from.clone(), pty_id, submit,
                    // #1702: unchanged in what it acquires — `delivered_mask_
                    // lines` already reached `by_pty` + `agents` from here — but
                    // now visibly so, under the per-pane delivery `lock` this arm
                    // holds. That nesting predates #1702 and is NOT this PR's to
                    // change; it is filed, with the reverse-order question, as
                    // its own row.
                    reg.delivered_mask_lines(pty_id, reg.session_for_pty(pty_id).as_deref()),
                ) {
                    StrandedMarkerAction::Press => DeliverOutcome::Done,
                    StrandedMarkerAction::Retire(why) => DeliverOutcome::Retired(why),
                    // #532 rev-12 NB1, arriving here: the gate that ACTUALLY
                    // declined, never a hardcoded `Question`. This arm used to
                    // report every decline as an interactive question, sending
                    // the orchestrator to look for a dialog that a box-occupied
                    // hold never painted.
                    StrandedMarkerAction::Retry(reason) => DeliverOutcome::AbortedPrePaste(reason),
                }
            }
        };

        // #533-A: every constituent of this pass's batch closes out on the
        // ONE submit that carried them — see `pop_batch_dequeued`. A
        // single-entry batch takes the identical pre-#533 path.
        let closed: Vec<(u64, u64)> = batch.iter().map(|e| (e.id, e.enqueued_ms)).collect();

        // #560: the hold episode's lifecycle, at the ONE place that knows
        // whether this pane accepted a delivery. `Done` is the only evidence
        // that ends an episode (`ends_hold_episode`); either abort OPENS one if
        // the poll above did not, which is what covers a hold that lives
        // entirely inside `deliver_now` — every poll reads writable, every
        // attempt then aborts, and pre-#560 nothing ever started a clock.
        // #813 adds a second ender that is not a delivery — see `Retired`.
        let observation = match outcome {
            DeliverOutcome::Done => HoldObservation::Delivered,
            DeliverOutcome::Retired(_) => HoldObservation::Retired,
            DeliverOutcome::AbortedPrePaste(_) | DeliverOutcome::AbortedPreEnter(_) => {
                HoldObservation::Aborted
            }
        };
        reg.note_hold(&group, &front.agent_id, pty_id, observation, now_ms());

        match outcome {
            DeliverOutcome::Done => {
                reg.pop_batch_dequeued(&group, pty_id, &closed);
                reg.queue_still_notified.lock_safe().remove(&pty_id);
                // #532/#560: the hold episode ended at the `note_hold` call
                // above — a delivered entry means whatever was holding this pane
                // is over, so a LATER hold clocks and badges afresh.
                lower_chip();
            }
            // #813: the marker leaves the queue without having pressed
            // anything, so the entries behind it get their turn — that release
            // IS the fix. Everything else here mirrors `Done` because the queue
            // consequence is the same (this entry is finished with), and only
            // the audit and the badge treat it differently.
            DeliverOutcome::Retired(why) => {
                reg.audit(&group, brand::AUDIT_ACTOR, "stranded-marker-retired", json!({
                    "to": &front.agent_id,
                    "reason": why.as_str(),
                    // The whole point, in one number: how much work this
                    // marker was holding behind it when it was retired.
                    "depth_behind": depth.saturating_sub(1),
                }));
                // The chip the human has been staring at comes down on the
                // evidence that they resolved it themselves — `HumanResolved`
                // and nothing else (`resolves_the_pane`). `TextGone` says our
                // text left the box with nobody at the keyboard, which is
                // `StrandedBlocker::NotHolding`'s own situation and NOT a
                // clear; `NothingStranded` establishes nothing about the pane.
                // #496 PR-C's badge is the channel that must survive a repair
                // loomux could not make. `clear_stranded` is a no-op when no
                // note is up, so this never invents a clear.
                if why.resolves_the_pane() {
                    reg.clear_stranded(&group, &front.agent_id, why.as_str());
                }
                reg.pop_batch_dequeued(&group, pty_id, &closed);
                reg.queue_still_notified.lock_safe().remove(&pty_id);
                lower_chip();
            }
            DeliverOutcome::AbortedPrePaste(reason) => {
                // Nothing pasted (or the stranded flush declined) — leave
                // the entry at the front exactly as it was; retry next tick.
                // #470: the ONE point at which this entry transitions from
                // "attempted immediately" to "now genuinely queued" — the
                // moment #445's notice vocabulary exists to announce, and
                // (unlike every later retry of the SAME entry, which stays
                // silent) the only point it's still correct to announce,
                // since a `BehindQueue` admission was never announced at
                // admission time either.
                if is_fresh_attempt {
                    reg.notify_queue(&group, &front.agent_id, target_is_orchestrator,
                        &queue::queued_notice(&front.agent_id, reason));
                }
            }
            DeliverOutcome::AbortedPreEnter(reason) => {
                // The Text entry WAS pasted; only the Enter was withheld.
                // Replace it at the front with a StrandedSubmit marker so
                // draining resumes there next tick instead of re-pasting.
                //
                // rev-12 NB1: `reason` comes from the gate that actually
                // declined, never a hardcoded `Question`. #532 gave this path
                // a second cause, and the hardcode was telling the
                // orchestrator to go find a dialog that did not exist.
                if is_fresh_attempt {
                    reg.notify_queue(&group, &front.agent_id, target_is_orchestrator,
                        &queue::queued_notice(&front.agent_id, reason));
                }
                // #533-A: the paste that landed in the box was the WHOLE
                // batch, so the whole batch closes out here and ONE marker
                // stands for all of it. Popping only the front (the
                // pre-#533 shape) would leave the other constituents queued
                // and re-paste text already sitting in the box the moment
                // the marker's Enter submits it.
                reg.pop_batch_dequeued(&group, pty_id, &closed);
                // #445 rev-35 NB3: this rejection is rare (it needs the
                // front door to fill the freed slot in the narrow window
                // between the pop above and this push) but was previously
                // silent — the `let _ =` skipped the loud `dropped_notice`
                // every OTHER drop path sends, leaving pasted-but-
                // unsubmitted text with no signal at all (mitigated only by
                // the next delivery's own stranded-text flush eventually
                // submitting it — never guaranteed to be loud about it).
                // #560: `reason` is this arm's own binding — the gate that
                // actually declined the Enter, `BoxOccupied` as readily as
                // `Question` since #532 — and it is what the marker is
                // recorded under. It is the same value the notice above was
                // already sent with; the audit line simply stopped disagreeing
                // with the notice about one event.
                if let Err(_queue_full) =
                    reg.enqueue_stranded_front(&group, &front.agent_id, &front.from, pty_id, reason)
                {
                    // #533-A: the marker that failed to push stood for the
                    // WHOLE batch that was pasted, so the count names every
                    // delivery left pasted-but-unsubmitted — saying "1" here
                    // would understate what a reader has to go look at.
                    reg.notify_queue(&group, &front.agent_id, target_is_orchestrator,
                        &queue::dropped_notice(&front.agent_id, closed.len(), queue::DropReason::QueueFull));
                }
            }
        }
    }
}
