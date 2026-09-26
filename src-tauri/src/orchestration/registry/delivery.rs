//! Prompt delivery: the registry's delivery path (`deliver_prompt` /
//! `deliver_to_orchestrator` and their `_as` forms), the per-delivery body
//! (`deliver_now`) and its late-confirmation monitor, the pane queues'
//! read side (`queue_*`), and the stranded-prompt janitor and re-admission
//! passes, as an `impl OrchRegistry` block (#3498). The queue's design is
//! #445 in `docs/design/orchestration.md`; its locking is
//! `docs/design/pty-input-path.md`.

use super::*;

#[allow(dead_code)]
fn plant_unnamed_lock() {
    let name: &'static str = "plant";
    let _m = TrackedMutex::new(name, ());
}

/// #112 round 2 (restructured round 3 per rev-20 B1 — see `late_monitor_tick`
/// for the precedence this now runs on): the extended, out-of-window monitor
/// for a delivery that finished its normal confirm/retry window still
/// `Pending` or `Failed` via `ConfirmSource::BoxVeto` (Tier 1's veto isn't
/// infallible — the design note's rejection-guard residual). Spawned as its
/// OWN detached thread, deliberately NOT holding the per-pty delivery mutex
/// `deliver_prompt`'s main closure holds for its own (bounded, ~52s)
/// lifetime: this monitor's own lifetime is bounded only by
/// `LATE_MONITOR_MAX_LIFETIME` / the pty staying alive (the live #112
/// episode needed 38+ minutes), and holding the delivery mutex that long
/// would block every SUBSEQUENT delivery to the same pane.
///
/// **Round 3 fix (rev-20 B1): supersession.** If a NEWER delivery to this
/// same pty has recorded its own `DeliveryOutcome` since this monitor
/// started (compared by `submit_sent_ms`, the one field every delivery's
/// outcome carries and no two deliveries share), this monitor is stale —
/// `late_monitor_tick` returns `Superseded` and this thread exits
/// immediately, WITHOUT writing `last_delivery` or notifying anything. Two
/// hazards this closes at once: an old monitor overwriting a newer
/// delivery's recorded outcome (corrupting the NEXT delivery's stranded-
/// flush decision), and an old monitor matching the RE-SEND's own hook
/// record and announcing a false "no re-send needed" correction — exactly
/// backwards, since the re-send is why anything landed at all. Checked
/// first, every tick, before acting on anything it reads (the hook marker
/// read still executes on a superseded tick; it just can never produce a
/// write or a notify).
#[allow(clippy::too_many_arguments)]
fn run_late_confirmation_monitor(
    app: AppHandle,
    root: PathBuf,
    group: GroupId,
    agent: String,
    pty_id: u32,
    hook_marker_path: PathBuf,
    hook_baseline: usize,
    pasted_text: String,
    delivery_from: String,
    submit_sent_ms: u64,
    target_is_orchestrator: bool,
    already_failed: bool,
    last_delivery: Arc<TrackedMutex<HashMap<u32, DeliveryOutcome>>>,
    reg: Option<Arc<OrchRegistry>>,
    // #517: the brief to re-deliver if this turns out to be a fresh spawn's
    // kickoff that never reached the pane. `None` for every other delivery.
    recoverable_kickoff: Option<String>,
    // #517: the pane's output counter as of this delivery's own Enter, so
    // "did the agent start a turn on our brief?" is measurable at
    // `DeclareFailed` time — the guard that makes a re-delivery safe.
    submit_output_baseline: u64,
    // #517 review F5: whether this delivery's boot wait actually watched the
    // CLI go ready, or timed out and pasted blind. Decides only whether
    // growth-since-submit is reported as a turn or as unattributable — never
    // whether the recovery declines. `true` for a delivery with no boot wait
    // at all (nothing was ever in doubt about the pane's readiness).
    ready_observed: bool,
) {
    let ptys = app.state::<crate::pty::PtyManager>();
    let started = std::time::Instant::now();
    let mut quiet_since: Option<std::time::Instant> = None;
    let mut last_total = ptys.output_total(pty_id).unwrap_or(0);
    let mut failed = already_failed;
    // #496 PR-C: self-heal submits fired by THIS monitor, counted against
    // `STRANDED_SELFHEAL_MAX_HEALS`. Per-monitor (i.e. per-delivery) — a
    // fresh delivery to the same pane gets its own budget, which is correct:
    // its Enter is about different text.
    let mut heals_used = 0u32;
    // #517: lost-kickoff re-deliveries fired by THIS monitor, counted
    // against `KICKOFF_REDELIVERY_MAX`. Per-monitor (i.e. per-delivery) for
    // the same reason `heals_used` is: a later delivery to the same pane is
    // about different text and gets its own budget.
    let mut redeliveries_used = 0u32;
    // #496 PR-C: whether our pasted text was still identifiably in the box at
    // the moment this delivery was declared stranded. A raised badge must be
    // able to come DOWN on evidence — a human who rescues the pane by
    // pressing Enter themselves produces no hook record on a CLI that has
    // none, so waiting for `Confirm` would leave the badge up until the
    // monitor's 4h cap. The text going from present to absent is that
    // evidence, and it is Tier 1's own signal used exactly as `deliver_prompt`
    // uses it. Only a genuine present→absent TRANSITION clears, so a
    // `NotHolding` badge (raised precisely because the text was already gone)
    // is never immediately un-raised by the same reading that raised it.
    let mut paste_seen_in_box = false;
    // #559 (rev-13 N2): computed ONCE for this monitor, not per tick and not
    // per arm. `pasted_text` is fixed for the monitor's whole life, so the
    // derived scan size is a constant of this delivery — and `Tier1Scan::for_
    // paste` normalizes the paste to measure it, which for a coalesced flush
    // is a multi-KiB allocation this loop would otherwise repeat every
    // `LATE_MONITOR_POLL` for up to `LATE_MONITOR_MAX_LIFETIME`. Hoisting it
    // also makes the "no box read for a delivery is narrower than the last"
    // invariant (see `deliver_prompt`'s `tier1_scan` and `Tier1Scan`'s own doc)
    // structural here rather than a property of two call sites happening to
    // call the same function: #685's widening raises this scan's floor, so both
    // arms below inherit whatever density either one has already discovered.
    let mut tier1_scan = Tier1Scan::for_paste(&pasted_text);
    loop {
        std::thread::sleep(LATE_MONITOR_POLL);
        // The pty closing (agent exited/killed) means there is nothing left
        // to observe, ever — exit quietly, matching every other "closed pty"
        // degrade in this module (never an error the caller has to handle).
        let Some(cur_total) = ptys.output_total(pty_id) else { return };

        // Quiescence + question-guard tracking (only meaningful once
        // `late_monitor_tick` actually asks for them, but cheap to keep
        // current every tick so a KeepWaiting tick doesn't lose progress
        // toward the threshold).
        if cur_total != last_total {
            last_total = cur_total;
            quiet_since = Some(std::time::Instant::now());
        } else if quiet_since.is_none() {
            quiet_since = Some(std::time::Instant::now());
        }
        let quiet_long_enough = quiet_since.is_some_and(|t| t.elapsed() >= PENDING_IDLE_QUIET);
        let showing_question = quiet_long_enough
            && ptys
                .output_tail_bounded(pty_id, LATE_MONITOR_QUESTION_SCAN_BYTES)
                .map(|b| strip_ansi(&b))
                // #576: the same two masks the hold predicate applies, in the
                // same order. This scan drives the late monitor's idle
                // trigger, so a self-latch here reads as "the human is still
                // expected to answer" and stalls the delivery's completion
                // exactly like the gate's own latch stalls the write.
                .map(|t| {
                    let masked = mask_loomux_notices_with_record(&t, &delivered_lines(&reg, pty_id));
                    prompt_wait_detected(&mask_own_paste(&masked, &pasted_text))
                })
                .unwrap_or(false);
        let hook_match = poll_promptsubmit_hook(&hook_marker_path, hook_baseline, &pasted_text);
        // #454: the ledger observation is taken AFTER the hook read, never
        // before — and the ordering is the fix, not a tidy-up. A hook record
        // this tick can see was necessarily produced by an Enter that had
        // already been pressed; if that Enter belonged to a NEWER delivery,
        // `record_inflight_delivery` published that delivery's ownership of
        // the pane BEFORE pressing it (see that function's doc for the
        // happens-before chain). So reading the ledger second means any hook
        // evidence this tick could act on is checked against a ledger state
        // at least as new as the evidence itself, and a newer delivery's
        // record can never be resolved as ours. Reading it first — as this
        // loop did before #454 — leaves a window the width of one tick's own
        // work, which is precisely the hazard, just smaller.
        //
        // Supersession is monotone (only a newer delivery ever rewrites this
        // pane's `submit_sent_ms`, and this monitor exits the moment it
        // sees one), so a "late" observation can only ever kill this monitor
        // sooner, never resurrect it.
        let ledger = observe_ledger(&last_delivery, pty_id, submit_sent_ms);
        let expired = started.elapsed() >= LATE_MONITOR_MAX_LIFETIME;

        match late_monitor_tick(ledger.superseded, hook_match, failed, expired, quiet_long_enough, showing_question) {
            MonitorAction::Superseded => {
                // #496 PR-C: a newer delivery owns this pane now. If IT is
                // recorded confirmed, the pane is unwedged — including the
                // case where this monitor's OWN self-heal landed, since
                // `drain_stranded_submit` records the successful press as a
                // fresh confirmed outcome. Drop the badge on the way out; a
                // newer UNCONFIRMED outcome leaves it up, because that
                // delivery's own monitor owns the pane's state from here.
                //
                // #454: a newer delivery now publishes itself BEFORE its
                // Enter, so this arm is reached while that delivery is still
                // in flight (`newer_confirmed: false`) far more often than it
                // used to be. The badge that would once have been dropped
                // here, on a later tick, is dropped by `deliver_now` itself
                // the moment that delivery confirms in-window — the one path
                // that spawns no monitor of its own.
                if ledger.newer_confirmed {
                    if let Some(r) = &reg {
                        r.clear_stranded(&group, &agent, "superseded-confirmed");
                    }
                }
                return;
            }
            MonitorAction::Expired => return,
            MonitorAction::KeepWaiting => {
                // #496 PR-C: keep a RAISED badge honest, every tick. Two
                // things can make it stale, and one bounded tail read (only
                // taken when a badge is actually up) answers both.
                if let Some(note) = reg.as_ref().and_then(|r| r.stranded_note(&agent)) {
                    let r = reg.as_ref().expect("note came from `reg`");
                    // #559: paste-derived scan size, and the three-state
                    // reading. Unreadable tail → `Unverifiable`, which is
                    // neither "holds" (so nothing below re-asserts a heal)
                    // nor "not holding" (so the badge is not dropped on a
                    // reading that never happened) — the same fail-
                    // conservative posture the old `.unwrap_or(true)` had,
                    // now extended to the tail-too-short case it missed.
                    let reading = box_reading(
                        tier1_scan
                            .read(|n| ptys.output_tail_bounded(pty_id, n))
                            .as_ref()
                            .map(|r| r.stripped.as_str()),
                        &pasted_text,
                    );

                    // #825 M2: the verdict is `stranded_badge_release`'s, not
                    // this loop's. What used to be decided here is now decided
                    // in one place that OUTLIVES this thread — the janitor asks
                    // the identical question of the identical reading every
                    // ~30s once `LATE_MONITOR_MAX_LIFETIME` has taken this
                    // monitor away. This arm keeps only what is genuinely its
                    // own: `paste_seen_in_box`, the present→absent TRANSITION
                    // no observer without a continuous watch can ever witness,
                    // which is why it is passed in rather than re-derived.
                    //
                    // Two cells therefore reach this monitor that never did
                    // before, both strictly narrowing what the chip claims:
                    // #819's human-resolved clear, and the `Unverifiable` /
                    // `Exhausted` → `NotHolding` honesty re-word. That is the
                    // point of one matrix rather than two — a chip must not
                    // mean one thing under a live monitor and another an hour
                    // after it exits.
                    //
                    // `tier1_trusted`'s inverse, taken here at the same instant
                    // as the reading it will be judged with. A stamp read, not
                    // a tail scan — nothing measurable on top of the box read
                    // this arm already takes.
                    let human_stamped_since = !tier1_trusted(
                        ptys.last_user_input_ms(pty_id).unwrap_or(0),
                        submit_sent_ms,
                    );
                    let verdict = stranded_badge_release(
                        note.blocker,
                        Some(reading),
                        human_stamped_since,
                        paste_seen_in_box,
                    );
                    if r.apply_stranded_verdict(&group, &agent, note.blocker, verdict) {
                        // The badge this flag was tracking is gone; a chip
                        // raised again later starts its own transition.
                        paste_seen_in_box = false;
                    } else if note.blocker.is_none() {
                        // Reachable exactly as before: an in-flight-heal chip
                        // only ever reaches `Keep` above (the class arms are
                        // the three a pane reading answers), so the sole way
                        // this arm was skipped then and now is the transition
                        // clear.
                        // rev-47 NB1: the badge says "loomux is re-sending
                        // it", but an admitted marker is not a fired one —
                        // `flush_stranded_text` re-decides at press time and
                        // can decline indefinitely (e.g. the human starts
                        // typing AFTER the marker was admitted, which the
                        // trigger-time decision could not have seen). The
                        // press correctly never fires; the badge must stop
                        // saying loomux is handling it and name what the
                        // human has to clear. Re-run the SAME pure decision
                        // against live state rather than re-deriving a
                        // second opinion — budget shown as spent, since a
                        // marker is already queued and this is purely about
                        // what the chip SAYS, never about pressing again.
                        let live = stranded_selfheal_action(
                            // #454/#519: the atomic `LedgerView`, not a
                            // re-read of `recorded` — this arm must judge
                            // outstanding-ness from the same snapshot the
                            // supersession check used.
                            ledger.outstanding,
                            // #518: bounded (see `human_input_block`). This is
                            // the tick that RE-ASSERTED the stale badge every
                            // poll for up to `LATE_MONITOR_MAX_LIFETIME`, which
                            // is what made the false hold indefinite rather
                            // than merely wrong.
                            human_input_block_now(&ptys, pty_id, submit_sent_ms).holds(),
                            showing_question,
                            reading,
                            STRANDED_SELFHEAL_MAX_HEALS,
                            STRANDED_SELFHEAL_MAX_HEALS,
                        );
                        if let StrandedAction::Attention(blocker) = live {
                            // Which live blockers are worth re-wording for —
                            // see `stranded_reword`. `redeliveries_used > 0`
                            // is this monitor's own knowledge that a
                            // lost-kickoff re-delivery is queued but not yet
                            // drained (review F2).
                            if let Some(word) = stranded_reword(blocker, redeliveries_used > 0) {
                                // #825 M2: through the same applier, so this
                                // is a re-word and never a raise. The note was
                                // read at the top of the arm and is written
                                // back here, and since M1 an entry that has
                                // vanished in between means a human dismissed
                                // the chip — a `mark_stranded` insert would
                                // hand it straight back on this monitor's very
                                // next 5s tick, which is the live complaint in
                                // its most reproducible form.
                                r.apply_stranded_verdict(
                                    &group,
                                    &agent,
                                    note.blocker,
                                    BadgeRelease::Reword(word),
                                );
                            }
                        }
                    }
                }
                continue;
            }
            MonitorAction::Confirm { merged, correction } => {
                last_delivery.lock_safe().insert(
                    pty_id,
                    // #813: `confirmed` means our text went in, so there is
                    // nothing stranded left to look for.
                    DeliveryOutcome {
                        confirmed: true,
                        submit_sent_ms,
                        from: delivery_from.clone(),
                        stranded_text: None,
                    },
                );
                append_audit(&root, &group, brand::AUDIT_ACTOR, "delivery-confirmed-late", json!({
                    "to": agent, "confirm_source": "hook", "confirm_merged": merged, "was_failed": correction,
                    // #539: the id the coalescing bucket knows this delivery
                    // by, so the retraction below is checkable from the record.
                    "delivery_id": submit_sent_ms,
                }));
                if let Some(r) = &reg {
                    // #539 (rev-13 finding A): withdraw this delivery from the
                    // coalescing bucket BEFORE anything else. Its alarm may
                    // still be waiting out a 15s window that this very tick
                    // has just proven wrong, and a notice naming an id we
                    // already know landed is precisely the false alarm this
                    // change exists to remove. Unconditional, and ordered
                    // ahead of the correction notice: if the alarm is still
                    // buffered there is nothing to correct, because it never
                    // went out. See `retract_unconfirmed_delivery`.
                    let withdrawn = r.retract_unconfirmed_delivery(&group, &agent, submit_sent_ms);
                    // The correction notice is for an alarm the orchestrator
                    // has ALREADY read. A withdrawn one never reached it, so
                    // sending "stand down, the earlier alarm was wrong" would
                    // refer to a message that does not exist — one more turn
                    // spent, about nothing.
                    if correction && !withdrawn {
                        r.notify_delivery_confirmed_late(&group, &agent, target_is_orchestrator);
                    }
                    // #496 PR-C: the prompt landed after all — whatever
                    // badge the failure raised is stale, drop it.
                    r.clear_stranded(&group, &agent, "confirmed-late");
                }
                return; // resolved — nothing left for this monitor to do
            }
            MonitorAction::DeclareFailed => {
                failed = true;
                // #496 PR-C: ACT on the failure instead of only announcing
                // it — the announcement is suppressed entirely for an
                // orchestrator target, which is exactly the case that
                // wedged. Tier 1's own two signals decide whether a
                // re-submit is safe: is our text still identifiably in the
                // box (`box_holds_paste`), and has the box stayed ours
                // since we pressed Enter (`tier1_trusted`)?
                //
                // #522 moved this reading ABOVE the notice it used to sit
                // below: the same fact that decides whether a re-submit is
                // safe also decides whether there is anything to announce,
                // and announcing first meant announcing before looking.
                // #559: paste-derived scan size and the three-state reading,
                // so an oversized coalesced flush is no longer read as an
                // empty box. `holds_paste` stays as the name for the ONE
                // question the rest of this arm asks of it.
                // #583: the raw length is kept beside the stripped text (never
                // re-read) so the `Unverifiable` record below can say HOW short
                // the tail came back, not only that it did.
                let box_read = tier1_scan.read(|n| ptys.output_tail_bounded(pty_id, n));
                let reading =
                    box_reading(box_read.as_ref().map(|r| r.stripped.as_str()), &pasted_text);
                let holds_paste = reading == BoxReading::Holds;
                // #559: the decline is stated wherever it is reached, not only
                // where it changes the outcome — an `Unverifiable` reading on
                // a delivery that ends up notified anyway (a human's line in
                // the box) would otherwise leave no trace that Tier 1 was
                // blind for it.
                if reading == BoxReading::Unverifiable {
                    // #583/#685: the same census `prompt-typed` carries, from
                    // the same read this reading was decided from. Sampled
                    // minutes later than the precondition's, against a fuller
                    // ring — a second, differently-timed sample of the same
                    // question, not a duplicate of the first. `scan_bytes` is
                    // read off the census rather than computed beside it, so
                    // the two cannot disagree about what was asked for once a
                    // read is allowed to widen itself.
                    let census = tier1_scan.census(box_read.as_ref(), &pasted_text);
                    append_audit(&root, &group, brand::AUDIT_ACTOR, "delivery-unconfirmed-box-unverifiable", json!({
                        "to": agent, "confirm_state": "unconfirmed",
                        "reason": "pasted text is larger than the pane tail loomux could read",
                        "paste_bytes": pasted_text.len(),
                        "scan_bytes": census.requested_bytes,
                        "tier1_scan_census": census.to_json(),
                    }));
                }
                // #522: a pane that is simply DONE — our text consumed, no
                // human characters outstanding, and (by `late_monitor_tick`'s
                // own precondition for reaching this arm) the CLI at rest —
                // is not a strand. Record it and stop, rather than telling the
                // orchestrator to `get_output` and re-send a prompt that is
                // not stuck. See `unconfirmed_disposition`.
                //
                // #559: reachable ONLY on an informative `NotHolding`. An
                // `Unverifiable` reading takes the notice/self-heal/escalation
                // path below like any other unresolved strand — which is the
                // whole fix: a delivery whose box we could not read is not a
                // delivery we watched go idle.
                //
                // #585: and only on one that ran a turn. Bound ONCE here and
                // reused by `kickoff_recovery_action` below, never re-read:
                // these are two decisions about the same question ("did the
                // agent act on our text?") taken on one tick, and computing
                // the growth twice is how they would come to disagree — the
                // same discipline the shared `ledger` observation follows.
                let output_since_submit = cur_total.saturating_sub(submit_output_baseline);
                let turn_evidence = output_since_submit >= KICKOFF_TURN_EVIDENCE_BYTES;
                // #539: the agent's OWN post-delivery MCP call, read from the
                // shared clock #535 stamps at the `tools/call` funnel. Read
                // once, here, rather than per tick: this is the only arm that
                // consults it. `None` (no registry, or an id no longer in the
                // roster) is "no evidence", which leaves the pre-#539
                // behaviour untouched — the fail-safe direction for a reading
                // we cannot take.
                let acted = reg
                    .as_ref()
                    .and_then(|r| r.last_mcp_activity_ms(&agent))
                    .is_some_and(|stamp| {
                        agent_acted_since(stamp, submit_sent_ms.saturating_add(UNCONFIRMED_ACK_SETTLE_MS))
                    });
                let disposition = unconfirmed_disposition(
                    reading,
                    ptys.input_pending(pty_id).unwrap_or(true),
                    turn_evidence,
                    acted,
                    recoverable_kickoff.is_some(),
                );
                // #585: the precedence itself, as a VALUE — see
                // `failed_arm_route`. The defect this issue fixed was a
                // `return` ordered above the recovery it preempted, and no
                // test could see it because the ordering existed only as
                // control flow. Reading the route from a pure function is what
                // makes "does the eaten case still reach the recovery?"
                // assertable at all.
                let route = failed_arm_route(disposition);
                if route == FailedArmRoute::QuietStop {
                    // #539 + #585: ONE route, TWO reasons to be silent, and
                    // they must not share a record — "we watched the pane go
                    // quiet with an empty box" and "we watched the agent act"
                    // are different observations, and a shared action string
                    // could not say which one suppressed the alarm. The route
                    // stays the single source of the control-flow decision
                    // (#585's point: precedence as a value); this only picks
                    // how to say it.
                    let (action, why) = match disposition {
                        UnconfirmedDisposition::ActiveAuditOnly => (
                            "delivery-unconfirmed-agent-active",
                            "box no longer holds our paste, the pane ran a turn on it, and the agent                              called a loomux tool after this delivery settled — busy, not stranded                              (liveness, not a read)",
                        ),
                        _ => (
                            "delivery-unconfirmed-idle-pane",
                            "pane idle — box holds neither our paste nor human input",
                        ),
                    };
                    append_audit(&root, &group, brand::AUDIT_ACTOR, action, json!({
                        "to": agent, "confirm_state": "unconfirmed",
                        "confirm_source": if disposition == UnconfirmedDisposition::ActiveAuditOnly { "activity" } else { "idle" },
                        // #539: the id the coalesced notice names deliveries by.
                        "delivery_id": submit_sent_ms,
                        "settle_ms": UNCONFIRMED_ACK_SETTLE_MS,
                        "reason": why,
                        // #585: the evidence the silence rests on, recorded
                        // WITH the silence. Without it this row asserted an
                        // idle pane and offered nothing to check the assertion
                        // against — which is why reconstructing #585 needed a
                        // code read rather than a log read.
                        "output_since_submit": output_since_submit,
                        "turn_evidence_bytes": KICKOFF_TURN_EVIDENCE_BYTES,
                    }));
                    return; // nothing stranded and nothing to observe further
                }
                // #585: same empty quiet box, no turn behind it — the paste
                // was eaten. Its own action so the two are greppable apart,
                // and NO early return: this delivery must reach the notice,
                // the badge, and `kickoff_recovery_action` below.
                let eaten = route == FailedArmRoute::Escalate { eaten: true };
                if eaten {
                    append_audit(&root, &group, brand::AUDIT_ACTOR, "delivery-eaten", json!({
                        "to": agent, "confirm_source": "idle", "confirm_state": "unconfirmed",
                        "reason": "box holds neither our paste nor human input, and the pane ran no turn on it",
                        "output_since_submit": output_since_submit,
                        "turn_evidence_bytes": KICKOFF_TURN_EVIDENCE_BYTES,
                        "ready_observed": ready_observed,
                    }));
                }
                // Own action name, deliberately distinct from the hook-match
                // branch above ("delivery-confirmed-late" is that branch's
                // SUCCESS action) — this one declares a FAILURE, and the
                // audit's primary key is the action string, not the
                // `confirm_state` detail buried inside it. `DeliveryOutcome`
                // is NOT re-written here: the original `prompt-typed` audit
                // already recorded `confirmed: false` for the `Pending`
                // state this delivery started this monitor in, and nothing
                // about reaching `Failed` changes that value.
                append_audit(&root, &group, brand::AUDIT_ACTOR, "delivery-failed-idle", json!({
                    "to": agent, "confirm_source": "idle", "confirm_state": "failed",
                    // #539: the id the coalesced notice names this delivery by.
                    "delivery_id": submit_sent_ms,
                }));
                if let Some(r) = &reg {
                    r.notify_unconfirmed_delivery(&group, &agent, target_is_orchestrator, false, eaten, submit_sent_ms);
                }
                // #518: bounded (see `human_input_block`). Once this releases,
                // precedence carries the decision straight into the EXISTING
                // `box_holds_paste` arm below — which is already exactly
                // "deliver if the box is unchanged": our own text still at the
                // tail means there is nothing of the human's there to merge
                // with, and anything else still lands on a badge
                // (`NotHolding`), never on a blind Enter.
                let human_block = human_input_block_now(&ptys, pty_id, submit_sent_ms);
                if human_block == HumanInputBlock::BoundedOut {
                    append_audit(&root, &group, brand::AUDIT_ACTOR, "human-input-block-released", json!({
                        "to": agent, "stage": "stranded", "reason": HUMAN_INPUT_BLOCK_BOUND_REASON,
                        "bound_ms": HUMAN_INPUT_BLOCK_BOUND_MS, "box_holds_paste": holds_paste,
                        // #559: `box_holds_paste: false` alone would read as
                        // "we looked and it is gone" on a delivery where we
                        // could not look at all. The reading says which.
                        "box_reading": reading.as_str(),
                    }));
                }
                let human_typed_since = human_block.holds();
                paste_seen_in_box = holds_paste;
                let action = stranded_selfheal_action(
                    ledger.outstanding,
                    human_typed_since,
                    showing_question,
                    reading,
                    heals_used,
                    STRANDED_SELFHEAL_MAX_HEALS,
                );
                if let Some(r) = &reg {
                    if r.actuate_stranded(&group, &agent, &delivery_from, pty_id, action) {
                        heals_used += 1;
                    }
                } else {
                    // No registry (a bare/headless construction): the badge
                    // and the queue both live on it, so there is nothing to
                    // actuate — record the decision rather than dropping it.
                    append_audit(&root, &group, brand::AUDIT_ACTOR, "stranded-selfheal-skipped", json!({
                        "to": agent, "reason": "no-registry",
                    }));
                }

                // #517: `NotHolding` is the eaten-paste signature — the
                // delivery failed AND its text never reached the box, so
                // there is no Enter for #496's self-heal to press. For a
                // FRESH spawn's kickoff that is not the end of the story:
                // the brief has no other route to the agent, so re-deliver
                // it through the same front door. Scoped strictly INSIDE
                // that one outcome so every other `StrandedAction` keeps
                // its pre-#517 behavior untouched.
                //
                // #559: `Unverifiable` joins it rather than splitting off. The
                // eaten-paste signature is "the delivery failed and nothing
                // confirming it is in the box", and a paste too large to
                // verify presents identically — before this change an
                // oversized kickoff reached this test as `NotHolding` only
                // when a human happened to have a line in the box, and went
                // silent (`IdleAuditOnly`) otherwise, so the recovery it is
                // entitled to fired or not on an unrelated coincidence. Safe
                // to widen because the box reading was never the guard here:
                // `kickoff_recovery_action`'s own `output_since_submit`
                // discriminator is what separates a kickoff that landed from
                // one that was eaten, and a landed kickoff resolves via the
                // hook long before this arm.
                if matches!(
                    action,
                    StrandedAction::Attention(StrandedBlocker::NotHolding)
                        | StrandedAction::Attention(StrandedBlocker::Unverifiable)
                ) {
                    let recovery = kickoff_recovery_action(
                        recoverable_kickoff.is_some(),
                        // #454: the SAME atomic ledger observation the
                        // self-heal decision above judged from — never a
                        // second read, so the two decisions taken on one
                        // tick can never disagree about whether this
                        // delivery is still outstanding.
                        ledger.outstanding,
                        human_typed_since,
                        showing_question,
                        // #585: the SAME growth reading the disposition above
                        // judged from — see where it is bound for why it is
                        // never re-read.
                        output_since_submit,
                        KICKOFF_TURN_EVIDENCE_BYTES,
                        ready_observed,
                        redeliveries_used,
                        KICKOFF_REDELIVERY_MAX,
                    );
                    // `Redeliver` implies `recoverable_kickoff.is_some()`
                    // (it is the decision's first gate), so the zip below
                    // can only be `None` when there is no registry to admit
                    // through — never a silently skipped brief.
                    match (recovery, recoverable_kickoff.as_deref().zip(reg.as_ref())) {
                        (KickoffRecovery::Redeliver, Some((brief, r))) => {
                            if r.redeliver_lost_kickoff(&group, &agent, &delivery_from, pty_id, brief) {
                                redeliveries_used += 1;
                                // The badge `actuate_stranded` just raised
                                // says "its text is gone — check the pane".
                                // That is no longer the whole truth: loomux
                                // IS handling it. Re-word to the in-flight
                                // form (`None`), the same one an in-flight
                                // self-heal uses, so the human is neither
                                // told to act nor left thinking nothing is.
                                r.mark_stranded(&group, &agent, None);
                            }
                        }
                        (KickoffRecovery::Redeliver, None) => {
                            // No registry to admit through (bare/headless).
                            append_audit(&root, &group, brand::AUDIT_ACTOR, "kickoff-redelivery-skipped",
                                json!({ "to": agent, "reason": "no-registry" }));
                        }
                        (KickoffRecovery::Decline(why), _) => {
                            // Only worth a line when this delivery was a
                            // kickoff at all — every mid-session delivery
                            // reaching `NotHolding` would otherwise write a
                            // "declined: not-a-kickoff" record saying
                            // nothing about anything.
                            if recoverable_kickoff.is_some() {
                                append_audit(&root, &group, brand::AUDIT_ACTOR, "kickoff-redelivery-skipped", json!({
                                    "to": agent,
                                    "reason": why.as_str(),
                                    // Review F5: carried alongside the reason
                                    // so a growth-based decline is always
                                    // readable together with whether the
                                    // paste that produced it went in blind.
                                    "ready_observed": ready_observed,
                                }));
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The delivery body itself — paste, echo-verify, submit, confirm — pulled
/// out of `deliver_prompt` (#445) so a single pipeline serves every attempt.
/// As of #470, `run_queue_drainer` is the ONLY caller: every delivery is
/// admitted into `pty_id`'s queue at arrival (`deliver_prompt`'s front
/// door), and only the queue's front entry is ever handed to this function
/// — there is no more separate "fresh delivery races a raw mutex" path for
/// this to serve (see `docs/design/orchestration.md`'s Ordering subsection
/// for why that path was the actual ordering bug, not merely unfair). `reg`
/// is used only for the pre-existing (#103/#112)
/// `notify_unconfirmed_delivery`/late-monitor wiring, unrelated to the
/// queue — `deliver_now` never touches the queue itself; only its caller
/// does, based on the returned `DeliverOutcome`.
#[allow(clippy::too_many_arguments)]
pub(in crate::orchestration) fn deliver_now(
    app: AppHandle,
    root: PathBuf,
    group: GroupId,
    agent: String,
    pty_id: u32,
    text: String,
    delivery_from: String,
    confirm_autopilot: bool,
    wait_ready: bool,
    cli: String,
    lock: Arc<TrackedMutex<()>>,
    last_delivery: Arc<TrackedMutex<HashMap<u32, DeliveryOutcome>>>,
    target_is_orchestrator: bool,
    reg: Option<Arc<OrchRegistry>>,
    // #517: `Some(brief)` when this is a FRESH spawn's kickoff — the brief
    // itself, so the late-confirmation monitor can re-deliver it if it turns
    // out never to have reached the pane. `None` for every other delivery
    // (mid-session, resume re-sync, or any replay), which keeps the pre-#517
    // "badge and stop" behavior exactly as it was. Deliberately the TEXT and
    // not a bool: the monitor must re-send the brief, never the flush header
    // some attempt happened to prepend to it.
    recoverable_kickoff: Option<String>,
    // #903: the drainer's last-resort question override, decided by
    // `question_override_admits` against a FRESH read of this pane and audited
    // there. When set, this attempt's interactive-question gate is skipped —
    // and ONLY that gate: the human-typing backstop, the stranded-text flush's
    // own question check (a blind Enter, which must never run on an override's
    // word) and every box-occupancy read below are untouched.
    //
    // A bool decided by the caller rather than a clock re-read here, because
    // the decision belongs to the poll that observed the pane: re-deriving it
    // inside this function would describe a different instant than the one that
    // admitted the write, the same reason `wait_for_question_clear` returns its
    // witness rather than letting the abort site re-read.
    question_overridden: bool,
    // #903 B1' — what this delivery may contribute to the session prompt
    // record, decided by the CALLER and paired with the kind each piece was
    // admitted under.
    //
    // Not `(pasted_text, one kind)`, because a paste is not always one
    // delivery. A coalesced flush is loomux's FRAMING wrapped around N
    // constituent payloads (#533-A), and a lone delivery can carry a flush
    // header on its front too — so the bytes pasted are a mixture whose parts
    // have different authors and, in the flush case, different `Delivery`
    // kinds. Asking the record to infer that split from the text would be a
    // second spelling of #632's rule; the caller already knows it exactly,
    // which is why `unmaskable_framing_rows` takes the payloads rather than
    // deriving them.
    //
    // Each pair is admitted on its OWN merits — both #903 B1 terms run per
    // contribution — so a re-grounding notice riding in a batch is refused by
    // its kind and a `resume_kickoff_notice` by its marker-led first line, no
    // matter what it is flushed alongside.
    record_contributions: Vec<(String, Delivery)>,
) -> DeliverOutcome {
    // ⚠ #561 — READ THIS BEFORE ADDING A STAGE BELOW.
    //
    // `REINJECT_ACK_SETTLE_MS` (~line 1573) is a const expression that SUMS the
    // unconditional pre-Enter stages of this function — the echo-verified typing
    // loop, `PASTE_SUBMIT_DELAY`, `SUBMIT_MAX_WAIT`, `SUBMIT_CONFIRM_WINDOW` and
    // the sequential `SUBMIT_RETRY_DELAYS`. The membership rule is: **if you add
    // a stage that runs on EVERY delivery before the last Enter, it belongs in
    // that expression.** A conditional stage (a hold that only fires when a gate
    // trips) does not; see that constant's own doc for what it deliberately
    // excludes and why (#546).
    //
    // The rule is written at the constant too. It is repeated here because that
    // is not where it gets broken: the constant came out short TWICE (#535
    // rev-22 D1, rev-28 Q1), both times because someone editing THIS function
    // had no reason to scroll nine thousand lines up. A rule only discoverable
    // from the place you are not standing is a request, not a guard — and the
    // bidirectional test mirror catches a change to the CONSTANT, not the
    // addition of a STAGE, which is exactly this drift vector.
    //
    // Getting it wrong is not cosmetic. The constant is a settling FLOOR added
    // to the moment we decided to paste, and an MCP call from the target agent
    // is only counted as acknowledging that paste if it lands after the floor
    // (#535's re-grounding ack, `agent_acted_since`). Too short, and a call the
    // agent had already decided on during its PREVIOUS turn resolves the phase
    // for a notice it has not read yet — the same false landed-signal class #112
    // and #522 removed elsewhere. The two times this constant was wrong it was
    // short both times.
    //
    // #561 option 2 (making the stage list data the code consumes, so adding a
    // stage necessarily adds it to the bound) is the version that cannot be
    // forgotten; this comment is option 1, and it is a request, not a guard.
    let _guard = lock.lock_safe();

    // #2850 S3b — the structured branch, and it is FIRST for a structural
    // reason rather than a stylistic one.
    //
    // `PtyManager` is reached on the very next line. A structured pane holds a
    // reserved id from the same counter, so handing one to that state would
    // look perfectly valid and simply find nothing — or, if the id were ever
    // reused, find the WRONG pane. Returning above it means no reserved id can
    // travel further down this function, which is a property of the ordering
    // and not of a check anyone has to remember to keep.
    //
    // Everything below this point exists because a PTY pane cannot be ASKED
    // whether it took the bytes: the readiness wait, the question gate, the
    // typing loop, the Enter, the submit confirmation, the late monitor.
    // `AgentPane::send` returns a receipt, so none of them has anything to do
    // here — they are not SKIPPED, they are inapplicable.
    if let Some(reg) = reg.as_ref() {
        if let Some(pane) = reg.structured_by_pane_id(pty_id) {
            let kind = record_contributions
                .first()
                .map(|(_, k)| *k)
                .unwrap_or_default();
            let turn = structured::turn_for(kind, &delivery_from, &text);
            return match reg.deliver_structured(&pane, turn) {
                Ok(_) => {
                    reg.audit(
                        &group,
                        &delivery_from,
                        "delivery",
                        json!({
                            "agent": agent,
                            "pane": pty_id,
                            "kind": "structured",
                            "bytes": text.len(),
                        }),
                    );
                    DeliverOutcome::Done
                }
                Err(e) => {
                    crate::obs::breadcrumb(
                        "structured-delivery-failed",
                        &format!("agent={agent} err={e}"),
                    );
                    DeliverOutcome::AbortedPrePaste(queue::EnqueueReason::PaneSendFailed)
                }
            };
        }
    }

    let ptys = app.state::<crate::pty::PtyManager>();
    let paste = bracketed_paste(&text);
    let submit = submit_sequence(&cli);
    let pasted_text = text;

    // Delivery-held badge plumbing (#246): fired around each blocking
    // human-input hold below so a pane-header badge can appear the instant
    // a hold starts and drop the instant it resolves. Each call site first
    // checks (with zero elapsed hold) whether it's ABOUT to block, so the
    // badge only shows for holds that actually happen, never for the
    // common no-op case where the box was already clear.
    let emit_held = |reason: HeldReason| {
        let _ = app.emit("orch-delivery-held", delivery_held_event(&agent, &group, pty_id, reason));
        // #946 Q4 / #1091 slice H — the latched-attention belt. A blocking
        // dialog held on the ORCHESTRATOR's own pane (never a delegate's —
        // that stays a passive badge, since a human answering a delegate's
        // dialog in person never stalls anyone else) strands every delegate
        // report queued behind it: the `--disallowedTools` deny closes this
        // for Claude, but a CLI with no tool-level deny can still land here.
        // The `orch-delivery-held` event above only ever reaches a frontend that is
        // subscribed and rendering AT THIS INSTANT; mirroring it into
        // `attn_question_held` instead gives a client a state it can read at
        // any later point too, the same way every other attention reason is
        // polled rather than event-only. `reg` is `None` for a delivery with
        // no registry behind it (see this function's doc) — no belt without
        // one, same as every other `reg`-gated behavior here.
        if target_is_orchestrator && reason == HeldReason::InteractiveQuestion {
            if let Some(r) = &reg {
                r.latch_question_held(&agent);
            }
        }
    };
    let emit_held_cleared = || {
        let _ = app.emit("orch-delivery-held-cleared", delivery_held_cleared_event(pty_id));
        // Unconditional removal is safe, not merely convenient:
        // `attn_question_held`'s doc explains why every hold this function
        // enters is sequential within one locked delivery attempt, so
        // clearing an id the latch was never holding for (a Typing/
        // BoxOccupied hold's own clear) is a no-op, never a race with a
        // DIFFERENT hold's latch.
        if let Some(r) = &reg {
            r.unlatch_question_held(&agent);
        }
    };

    let start = std::time::Instant::now();
    // #517: whether the boot wait actually OBSERVED the CLI go ready, or
    // gave up at `READY_MAX_WAIT` and pasted blind. Audited below, because
    // "we pasted into a CLI we never saw become ready" is the single most
    // useful fact about a kickoff that then failed to confirm, and it was
    // previously thrown away — both exits from this loop looked identical
    // in the record.
    let mut ready_observed = !wait_ready;
    if wait_ready {
        // #517: the sampler is the pane's MONOTONIC output counter, never
        // the ring's current length — see `await_cli_ready`'s doc for why
        // that distinction is the lost-kickoff mechanism.
        // #1591: the per-CLI ready MARKER, read out of the capability table
        // rather than branched on here — `None` for every CLI but opencode,
        // which leaves this call exactly what it was. An unknown CLI has no
        // row and therefore no marker, which is the same answer.
        let ready_marker = cli_caps(&cli).and_then(|c| c.ready_marker);
        match await_cli_ready(
            ready_marker,
            || ptys.output_total(pty_id),
            // The pane's RENDERED rows — a footer is a region of the SCREEN,
            // and the byte ring cannot answer where a cursor-positioned repaint
            // put the count relative to its label (#1591 review, premortem 2).
            // Everything that DECIDES anything lives in `ready_screen`, which
            // is pure and tested; this closure is only the pty read.
            || {
                Some(ready_screen(
                    &ptys.output_tail_bounded(pty_id, READY_GRID_REPLAY_BYTES)?,
                    ptys.size(pty_id),
                ))
            },
            || std::thread::sleep(READY_POLL),
            || start.elapsed(),
        ) {
            ReadyWait::Ready => ready_observed = true,
            // Paste anyway — better a visible prompt the human can
            // re-submit than one silently withheld.
            ReadyWait::TimedOut => {}
            ReadyWait::PaneClosed => {
                append_audit(&root, &group, brand::AUDIT_ACTOR, "prompt-failed",
                    json!({ "to": agent, "reason": "terminal closed while waiting for CLI to become ready" }));
                return DeliverOutcome::Done;
            }
        }
    }

    // Copilot autopilot consent (#101/#179): the "Enable autopilot mode"
    // dialog does NOT open at boot — verified live against copilot 1.0.69,
    // a fresh --autopilot pane paints a normal input box, and the consent
    // dialog is triggered by the FIRST message submit. So the confirm is
    // answered AFTER the kickoff Enter (below), not here: selecting its
    // default "Enable all permissions" both enables autopilot and delivers
    // the pending brief in one step. Watching at boot (as this used to)
    // only burned the fail-soft wait on a dialog that never shows.

    // Human-typing backstop (#43, option A): if a human is typing
    // directly in this pane, hold the paste until they go quiet so a
    // report can't land inside their half-typed line. Capped so a long
    // compose session can't starve the queue.
    let will_hold_typing_prepaste = should_hold_for_user(
        ptys.last_user_input_ms(pty_id).unwrap_or(0), now_ms(),
        Duration::ZERO, USER_QUIET_HOLD, USER_QUIET_MAX_HOLD,
    );
    if will_hold_typing_prepaste {
        emit_held(HeldReason::Typing);
    }
    if let Some(held_ms) = wait_for_user_quiet(&ptys, pty_id) {
        append_audit(&root, &group, brand::AUDIT_ACTOR, "delivery-held-for-user", json!({
            "to": agent, "stage": "pre-paste", "held_ms": held_ms,
            "capped": held_ms >= USER_QUIET_MAX_HOLD.as_millis() as u64,
        }));
    }
    if will_hold_typing_prepaste {
        emit_held_cleared();
    }

    // Stranded-text flush (#81/#84): if the PREVIOUS delivery to this
    // pane was never confirmed as submitted, its text may still be
    // sitting in the input box — pasting now would append to it and the
    // two prompts would merge. Press submit once to clear it first, but
    // only if no human has typed since that delivery (else the box may
    // hold a person's line, which must never be blind-submitted — the
    // pre-paste hold above already waited for them to go quiet).
    //
    // #420 rev-15 B1: this Enter used to fire unconditionally — the
    // FIRST write this thread makes, before the interactive-question
    // checkpoint below ever runs. A question already on screen (from
    // BEFORE this delivery even started) would eat that Enter and
    // select whatever's highlighted, on the very path this PR exists
    // to guard. `question_active_now` is a plain snapshot — not a
    // hold — because holding here would be redundant: if it's active,
    // this flush is skipped (nothing lost — the previous delivery's
    // text just stays put a little longer) and the pre-paste
    // checkpoint immediately below is the one that actually holds,
    // aborts, and notifies.
    let prev = last_delivery.lock_safe().get(&pty_id).cloned();
    // #518: the ONE derivation (`human_input_block`), not an inline
    // timestamp compare — see its doc for why the old expression was an
    // unbounded latch. `BoundedOut` is audited rather than released
    // silently: "loomux pressed Enter into a pane a human had touched" is
    // exactly the decision a reader of the log must be able to find.
    let human_block = prev
        .as_ref()
        .map(|o| human_input_block_now(&ptys, pty_id, o.submit_sent_ms))
        .unwrap_or(HumanInputBlock::None);
    if human_block == HumanInputBlock::BoundedOut {
        append_audit(&root, &group, brand::AUDIT_ACTOR, "human-input-block-released", json!({
            "to": agent, "stage": "pre-paste", "reason": HUMAN_INPUT_BLOCK_BOUND_REASON,
            "bound_ms": HUMAN_INPUT_BLOCK_BOUND_MS,
        }));
    }
    let flushed = flush_stranded_text(
        &ptys, pty_id, prev.as_ref().map(|o| o.confirmed), human_block.holds(), submit,
        delivered_lines(&reg, pty_id),
    );
    if flushed {
        append_audit(&root, &group, brand::AUDIT_ACTOR, "delivery-flush",
            json!({ "to": agent, "reason": "previous delivery unconfirmed" }));
        std::thread::sleep(FLUSH_SETTLE);
    }
    // #824: the flush DECLINED and nothing below can see why it matters. No
    // guard between here and the paste can observe loomux's own text — see
    // `stranded_paste_guard` for why `input_pending` structurally cannot — so
    // without this, the paste lands on top of a prompt still waiting to be
    // submitted and the pre-Enter quiet wait sends both as one.
    //
    // Cost: this read is taken ONLY when the flush declined AND the ledger says
    // the previous delivery is unconfirmed AND it recorded text to look for.
    // A delivery into a pane with nothing stranded — the overwhelming majority
    // — pays a `matches!` and nothing else. Within that, the read is
    // `Tier1Scan`'s own bounded widening (`TIER1_SCAN_WIDEN_ROUNDS`, capped at
    // `TIER1_SCAN_WIDEN_MAX_BYTES`), the same one the late monitor and #819's
    // marker drain already take on this pane.
    let stranded_reading = (!flushed
        && matches!(prev.as_ref().map(|o| o.confirmed), Some(false)))
    .then(|| prev.as_ref().and_then(|o| o.stranded_text.clone()))
    .flatten()
    .map(|text| {
        let mut scan = Tier1Scan::for_paste(&text);
        let read = scan.read(|n| ptys.output_tail_bounded(pty_id, n));
        box_reading(read.as_ref().map(|r| r.stripped.as_str()), &text)
    });
    if stranded_paste_guard(prev.as_ref().map(|o| o.confirmed), flushed, stranded_reading)
        == StrandedPasteGuard::AbortStranded
    {
        append_audit(&root, &group, brand::AUDIT_ACTOR, "delivery-aborted-stranded-text", json!({
            "to": agent,
            "reason": "the previous delivery's text is still in the box and the flush declined — \
                       pasting would merge two prompts",
            // Which gate held the flush back, so a reader can tell a human-block
            // decline from a question one without re-deriving it.
            "flush_blocked_by": if human_block.holds() { "human-input" } else { "question-or-box" },
        }));
        // Re-queued and retried by the drainer, with no cap, exactly like every
        // other pre-paste abort. It converges on the same two events that would
        // have let the flush fire: the human clears the box themselves (the
        // reading turns `NotHolding`), or #518's bound releases the human block
        // and the flush presses. A pane where neither happens is a held pane,
        // and #560's escalation badges it — which is the outcome this issue
        // wants, in place of two prompts merged into one.
        return DeliverOutcome::AbortedPrePaste(queue::EnqueueReason::BoxOccupied);
    }

    // Human-input paste guard (#111): the quiet backstop above only waits
    // out ACTIVE typing — it doesn't stop a paste landing on top of a line
    // the human typed and LEFT sitting in the box. Pasting there and
    // pressing Enter merge-submits their line with the prompt (the live
    // `/model` + task-text collision). So hold for the box to clear
    // (they submit or clear it); if it never does, abort WITHOUT pasting
    // — the caller (#445) enqueues it and notifies once.
    //
    // #532: both pre-paste gates live inside a re-verify LOOP, not a straight
    // line. The question hold below can block for `QUESTION_HOLD_MAX`, and a
    // box-occupancy answer taken before it is a fact about a different
    // instant by the time it clears — see `write_admission` for the exact
    // interleaving this closes (it is the ordinary one, not a rare race: the
    // keystroke that releases a stale question hold is the same keystroke that
    // occupies the box). The loop exits only when BOTH gates read clear
    // together.
    let mut recheck_round = 0u32;
    // Carried across rounds so the recheck abort below can name what the
    // question guard last saw (#513(c)). That abort records `blocked_on:
    // "question"` and, without this, nothing about WHICH question — the same
    // blind spot as `delivery-aborted-question`, one exit down.
    let mut last_question_seen: Option<QuestionWitnessed> = None;
    let prepaste_admission = loop {
        recheck_round += 1;
        let will_hold_box = ptys.input_pending(pty_id).unwrap_or(false);
        if will_hold_box {
            emit_held(HeldReason::BoxOccupied);
        }
        let paste_decision = wait_for_box_clear(&ptys, pty_id);
        if will_hold_box {
            emit_held_cleared();
        }
        match paste_decision {
            PasteDecision::Paste { held_ms } if held_ms > 0 => {
                append_audit(&root, &group, brand::AUDIT_ACTOR, "delivery-held-for-input", json!({
                    "to": agent, "held_ms": held_ms, "outcome": "cleared",
                    "recheck_round": recheck_round,
                }));
            }
            PasteDecision::Paste { .. } => {}
            PasteDecision::Abort { held_ms } => {
                append_audit(&root, &group, brand::AUDIT_ACTOR, "delivery-aborted-human-input", json!({
                    "to": agent, "held_ms": held_ms, "recheck_round": recheck_round,
                }));
                return DeliverOutcome::AbortedPrePaste(queue::EnqueueReason::BoxOccupied);
            }
        }

        // Interactive-question paste guard (#420): a question/permission TUI
        // reads nothing like the box-occupied case above (no keystrokes, no
        // `input_pending`) but is just as unsafe to paste over — worse,
        // actually, since the Enter below wouldn't merge text, it would SELECT
        // whichever option is highlighted. Hold for `prompt_wait_detected` to
        // clear (the human answers) before pasting; if it never does, abort
        // without pasting, same recovery shape as the box-occupied guard.
        // Nothing of ours is on screen yet at this point in the delivery, so
        // there's no self-echo risk to gate against (`paste_baseline_total:
        // None` — see `question_hold_predicate`).
        // #903: an override skips the hold outright rather than entering it with
        // a shorter cap. Entering it would re-arm, for two more minutes, exactly
        // the reading the override exists because loomux has stopped believing —
        // and would then abort the delivery the drainer just admitted, putting
        // the pane straight back where #903 found it.
        let (question_decision, question_seen) = if question_overridden {
            (PasteDecision::Paste { held_ms: 0 }, None)
        } else {
            wait_for_question_clear(
                &ptys,
                pty_id,
                None,
                // #576: re-read at every checkpoint rather than snapshotted
                // once. This delivery records its OWN marker-led lines the
                // moment it writes them (below), so the pre-Enter and retry
                // checkpoints get a record that includes what this paste just
                // put on screen — which is what covers a notice of ours that
                // wrapped, a case `mask_own_paste`'s whole-line matching
                // structurally cannot see.
                delivered_lines(&reg, pty_id),
                &emit_held,
                &emit_held_cleared,
            )
        };
        if question_seen.is_some() {
            // Only overwrite with a real sighting: a later round that saw
            // nothing must not erase what an earlier one held for.
            last_question_seen = question_seen.clone();
        }
        match question_decision {
            PasteDecision::Paste { held_ms } if held_ms > 0 => {
                append_audit(&root, &group, brand::AUDIT_ACTOR, "delivery-held-for-question", json!({
                    "to": agent, "stage": "pre-paste", "held_ms": held_ms, "outcome": "cleared",
                    "recheck_round": recheck_round,
                    "matched": witness_audit(question_seen.as_ref()),
                }));
            }
            PasteDecision::Paste { .. } => {}
            PasteDecision::Abort { held_ms } => {
                append_audit(&root, &group, brand::AUDIT_ACTOR, "delivery-aborted-question", json!({
                    "to": agent, "stage": "pre-paste", "held_ms": held_ms,
                    "recheck_round": recheck_round,
                    "matched": witness_audit(question_seen.as_ref()),
                }));
                return DeliverOutcome::AbortedPrePaste(queue::EnqueueReason::Question);
            }
        }

        // #532: the gates, re-read TOGETHER. `unwrap_or(false)` for a closed
        // pty is deliberate and not the fail-safe direction reversed: a closed
        // pane has no input box and nobody typing into it, so there is nothing
        // for this guard to protect, and the paste below already fails and
        // audits `prompt-failed` — which is both the pre-existing behaviour
        // and the more informative one. Making it `true` would only convert a
        // dead pane into an extra enqueue/retry cycle.
        let admission = write_admission(
            ptys.input_pending(pty_id).unwrap_or(false),
            // #903: the override's second (and last) reach into this function.
            // Short-circuited rather than ANDed after the call so an overridden
            // attempt does not pay a pane read whose answer it would discard.
            !question_overridden
                && question_active_now(&ptys, pty_id, None, delivered_lines(&reg, pty_id)),
        );
        if admission.go() || recheck_round >= PREPASTE_RECHECK_ROUNDS {
            break admission;
        }
    };
    if !prepaste_admission.go() {
        // A gate re-armed as fast as the other one cleared, `PREPASTE_RECHECK_
        // ROUNDS` times over — a pane a human is actively working in. Hold the
        // delivery rather than paste into it; the entry stays at the front of
        // its queue and the drainer retries with no cap.
        append_audit(&root, &group, brand::AUDIT_ACTOR, "delivery-aborted-recheck", json!({
            "to": agent, "stage": "pre-paste", "rounds": recheck_round,
            "blocked_on": prepaste_admission.enqueue_reason().as_str(),
            "matched": witness_audit(last_question_seen.as_ref()),
        }));
        return DeliverOutcome::AbortedPrePaste(prepaste_admission.enqueue_reason());
    }

    // #445 rev-35 B1 USED to re-check the queue right here, immediately
    // before committing to paste — necessary back when a FRESH delivery
    // could reach this point never having touched the queue at all (it
    // raced a raw per-pty mutex instead), so an earlier-arriving delivery
    // that timed out its own hold and enqueued WHILE this one was blocked
    // could otherwise be overtaken. #470 removes the recheck rather than
    // widening it (a reviewer proved widening it to 3+ contenders
    // insufficient — see `docs/design/orchestration.md`'s Ordering
    // subsection): every delivery, including this one, is now admitted
    // into the SAME queue at `deliver_prompt`'s front door, atomically with
    // the emptiness check that decided whether it or something else runs
    // first. By the time ANY call reaches this function, its own entry is
    // — and structurally can only ever be — the front of `pty_id`'s queue
    // (nothing but `run_queue_drainer`'s own pop ever removes the front,
    // and nothing but a push to the BACK ever adds to it), so there is
    // nothing left to defer to. See `b1_ordering_property` in `queue.rs`
    // for the historical model of the mechanism this replaces and
    // `unified_admission_property` for the model of what replaced it.

    // #112: this delivery's OWN baseline into the `promptsubmit` hook
    // marker, snapshotted right before it pastes anything — see
    // `promptsubmit_records_since`'s doc for why a byte offset (not a
    // sequence number the shell script would have to maintain) makes
    // a record from an EARLIER delivery to this same pane, or a
    // human's own prompt, unable to satisfy THIS delivery's
    // confirmation by construction.
    //
    // #904: `group` is a validated `GroupId`, so `promptsubmit_marker_path`
    // builds an infallible path under this group's `hooks/` dir via
    // `group_dir_at` — no id it can be handed escapes the root. Every use of
    // this value is a READ (`promptsubmit_marker_len`, `poll_promptsubmit_hook`);
    // loomux never writes here, the hook script does, via `$ORX_GD`.
    //
    // #925 closed the other half: the AGENT id names the file inside that dir,
    // and it used to arrive here as a bare `&str`. An id that cannot name a
    // single component yields an empty path, whose reads degrade exactly as a
    // marker no hook has written yet already does — `promptsubmit_marker_len`
    // reports 0 and `poll_promptsubmit_hook` reports `None`.
    let hook_marker_path = PathSegment::parse(&agent)
        .map(|agent_seg| promptsubmit_marker_path(&root, &group, &agent_seg))
        .unwrap_or_default();
    let hook_baseline = promptsubmit_marker_len(&hook_marker_path);

    // Echo-verified typing: paste, then require the TUI to emit
    // output (its input box redrawing). No echo means the CLI
    // flushed the paste with its startup stdin buffer — retype.
    let mut echoed = false;
    let mut attempts = 0u32;
    while attempts < ECHO_ATTEMPTS {
        attempts += 1;
        let Some(before) = ptys.output_total(pty_id) else {
            append_audit(&root, &group, brand::AUDIT_ACTOR, "prompt-failed",
                json!({ "to": agent, "reason": "terminal closed before delivery" }));
            return DeliverOutcome::Done;
        };
        if ptys.write_bytes(pty_id, &paste).is_err() {
            append_audit(&root, &group, brand::AUDIT_ACTOR, "prompt-failed",
                json!({ "to": agent, "reason": "terminal closed before delivery" }));
            return DeliverOutcome::Done;
        }
        // #576: the ONE place loomux's own text becomes bytes in a pane is the
        // one place the record of it is written — immediately after the write
        // succeeds, so the record never claims text that failed to go out (see
        // `record_delivered_text`). Inside the retype loop rather than after
        // it, because an un-echoed attempt still painted its bytes; the record
        // de-duplicates a repeated line so a retype costs it nothing.
        if let Some(r) = &reg {
            r.record_delivered_text(pty_id, &pasted_text);
            // #903: and the SESSION-scoped prompt record beside it, from the
            // same bytes and at the same instant. Separate call rather than a
            // widening of the one above because the two records admit different
            // things for different reasons — see `delivered_prompt_lines`.
            for (text, kind) in &record_contributions {
                r.record_delivered_prompt(pty_id, text, *kind);
            }
        }
        let echo_deadline = std::time::Instant::now() + ECHO_WINDOW;
        while std::time::Instant::now() < echo_deadline {
            std::thread::sleep(Duration::from_millis(150));
            match ptys.output_total(pty_id) {
                Some(now_total) if now_total >= before + ECHO_MIN_BYTES => {
                    echoed = true;
                    break;
                }
                Some(_) => {}
                None => {
                    append_audit(&root, &group, brand::AUDIT_ACTOR, "prompt-failed",
                        json!({ "to": agent, "reason": "terminal closed during delivery" }));
                    return DeliverOutcome::Done;
                }
            }
        }
        if echoed {
            break;
        }
        std::thread::sleep(ECHO_RETRY_DELAY);
    }
    std::thread::sleep(PASTE_SUBMIT_DELAY);

    // Wait for the pane to go quiet before Enter: a busy CLI
    // (mid-turn) ignores the submit and the prompt would sit in
    // the input box until a human presses Enter.
    let submit_start = std::time::Instant::now();
    let mut last_total = ptys.output_total(pty_id).unwrap_or(0);
    let mut last_change = std::time::Instant::now();
    // Whether the pane went quiet before we press Enter. If it never
    // does (busy CLI, hit SUBMIT_MAX_WAIT), the Enter lands mid-stream
    // and submit confirmation can't be trusted off that stream (rev-32).
    let mut reached_quiet = false;
    while submit_start.elapsed() < SUBMIT_MAX_WAIT {
        std::thread::sleep(Duration::from_millis(200));
        match ptys.output_total(pty_id) {
            Some(t) if t != last_total => {
                last_total = t;
                last_change = std::time::Instant::now();
            }
            Some(_) => {
                if last_change.elapsed() >= SUBMIT_QUIET {
                    reached_quiet = true;
                    break;
                }
            }
            None => {
                append_audit(&root, &group, brand::AUDIT_ACTOR, "prompt-failed",
                    json!({ "to": agent, "reason": "terminal closed before submit" }));
                return DeliverOutcome::Done;
            }
        }
    }
    // #420 rev-19 B-A: the pre-Enter/retry question checkpoints below
    // no longer need a growth BASELINE at all — they mask our own
    // pasted text out of the tail by CONTENT (`mask_own_paste`), not
    // by comparing byte counts against a snapshot. A snapshot-based
    // gate (rev-15/rev-19-round-3's approach) was proven broken
    // twice: it can only mark ONE point in time as "before", so a
    // dialog that renders while the paste is still settling gets
    // baked into whichever number the checkpoint happened to
    // snapshot, and reads as invisible forever after. Content
    // doesn't have that blind spot — masking works regardless of
    // WHEN the dialog appeared relative to our own paste settling.
    // Re-check right before the first Enter: the human may have
    // started typing during the quiet-wait above, and a blind Enter
    // would submit their line. Hold again until they're quiet (#43).
    let will_hold_typing_preenter = should_hold_for_user(
        ptys.last_user_input_ms(pty_id).unwrap_or(0), now_ms(),
        Duration::ZERO, USER_QUIET_HOLD, USER_QUIET_MAX_HOLD,
    );
    if will_hold_typing_preenter {
        emit_held(HeldReason::Typing);
    }
    if let Some(held_ms) = wait_for_user_quiet(&ptys, pty_id) {
        append_audit(&root, &group, brand::AUDIT_ACTOR, "delivery-held-for-user", json!({
            "to": agent, "stage": "pre-enter", "held_ms": held_ms,
            "capped": held_ms >= USER_QUIET_MAX_HOLD.as_millis() as u64,
        }));
    }
    if will_hold_typing_preenter {
        emit_held_cleared();
    }
    // Re-check right before the first Enter (#420): the paste above may
    // have taken long enough for a question to appear since the
    // pre-paste check, and the Enter below would select whichever option
    // is now highlighted rather than merely submitting text. Guarded
    // against self-echo (rev-15 N1 / rev-19 B-A) via `mask_own_paste` —
    // this checkpoint's tail necessarily contains our OWN just-pasted,
    // not-yet-submitted text, so a bare `prompt_wait_detected` match
    // here would otherwise be indistinguishable from a genuine live
    // dialog; masking out our own known lines leaves only what the CLI
    // itself painted, whenever it painted it.
    let (question_decision_preenter, question_seen_preenter) = wait_for_question_clear(
        &ptys, pty_id, Some(&pasted_text), delivered_lines(&reg, pty_id),
        &emit_held, &emit_held_cleared,
    );
    match question_decision_preenter {
        PasteDecision::Paste { held_ms } if held_ms > 0 => {
            append_audit(&root, &group, brand::AUDIT_ACTOR, "delivery-held-for-question", json!({
                "to": agent, "stage": "pre-enter", "held_ms": held_ms, "outcome": "cleared",
                "matched": witness_audit(question_seen_preenter.as_ref()),
            }));
        }
        PasteDecision::Paste { .. } => {}
        PasteDecision::Abort { held_ms } => {
            // #903 B2: an attempt the drainer GRANTED an override to carries
            // that grant to its Enter, re-proved here on a FRESH read.
            //
            // **Why this arm existed as a dead end.** The grant's own
            // precondition is that the question gate has been reading a false
            // positive for fifteen minutes on a pane whose screen never changes.
            // Nothing about pasting into it changes that, so this checkpoint
            // re-reads the same screen, gets the same false positive by
            // construction, and aborts with the text already in the box. The
            // override therefore converted a bounded hold into an unrecoverable
            // wedge — a stranded paste every later delivery queues behind, which
            // is strictly worse than never having overridden at all. That is what
            // the live incident did, twice, before the pane was killed by hand.
            //
            // **What it is re-proved on, and what it is NOT.** The override's own
            // standard: the WEAK idleness reading
            // ([`idle_prompt_row_rendered`]), on a fresh sample, twice —
            // never a flag the drainer set minutes ago. The strong reading is not
            // used here for the reason [`QUESTION_HOLD_OVERRIDE_AFTER`] gives for
            // the grant itself: on the pane this exists for, the strong reading is
            // exactly the thing that is wrong, so requiring it would make this
            // code unreachable. The residual that leaves — a dialog painted above
            // a composer holding our paste, showing no token evidence, inside the
            // override window — is argued in
            // `docs/design/question-gate-authorship.md`; `h13`'s dialog is caught
            // by the menu-structure TOKEN clause and is not in it.
            if question_overridden
                && preenter_override_admits(
                    &ptys,
                    pty_id,
                    &pasted_text,
                    delivered_lines(&reg, pty_id),
                )
            {
                append_audit(&root, &group, brand::AUDIT_ACTOR, "delivery-question-override-enter", json!({
                    "to": agent, "stage": "pre-enter", "held_ms": held_ms,
                    "matched": witness_audit(question_seen_preenter.as_ref()),
                    "reads": QUESTION_OVERRIDE_CONSECUTIVE_READS,
                    "reason": "granted a question override; the composer still holds this paste",
                }));
            } else {
                append_audit(&root, &group, brand::AUDIT_ACTOR, "delivery-aborted-question", json!({
                    "to": agent, "stage": "pre-enter", "held_ms": held_ms,
                    "matched": witness_audit(question_seen_preenter.as_ref()),
                }));
                // #420 rev-15 B3: unlike the pre-paste abort, the text IS
                // already pasted at this point — only the Enter was
                // withheld. It's sitting unsubmitted in the box, exactly
                // the shape the stranded-text flush (#81/#84) exists to
                // clear on the NEXT delivery — but only if that delivery
                // can see it. Recording nothing here would leave
                // `last_delivery` holding whatever outcome (or none) an
                // EARLIER delivery left, so the next delivery's flush
                // could wrongly conclude nothing needs clearing and
                // append its own paste onto this one's abandoned text.
                record_aborted_preenter_outcome(
                    &last_delivery, pty_id, delivery_from, Some(pasted_text.clone()));
                return DeliverOutcome::AbortedPreEnter(queue::EnqueueReason::Question);
            }
        }
    }
    // #532: the LAST gate before the Enter, and the one that was missing.
    // Every other pre-Enter check above answers a different question —
    // `wait_for_user_quiet` asks whether the human has *stopped* typing, and
    // the question hold asks whether a dialog owns the Enter key. Neither asks
    // the one thing #510 is actually about: is there human-typed content in
    // this box *right now*, which this Enter would submit. A human who typed
    // during the pre-paste holds and then paused satisfies both checks above
    // while their line still sits in the box — which is precisely how #532's
    // prompt landed mid-typing.
    //
    // Aborting here is not a loss and not a new recovery path: the text IS
    // pasted, so this takes the SAME `AbortedPreEnter` route the question
    // checkpoint directly above already takes — a `StrandedSubmit` marker at
    // the front of the queue, retried with no cap, whose own press
    // (`drain_stranded_submit` → `flush_stranded_text`) now re-reads occupancy
    // too. So the delivery waits for the box to empty and then flushes.
    //
    // The reading itself lives in `preenter_admission` (rev-12 B1) so this
    // call site has nothing left to get wrong beyond calling it, and so the
    // gate is drivable by a test without an `AppHandle` — the same extraction
    // `flush_stranded_text`'s doc makes the argument for.
    let preenter = preenter_admission(&ptys, pty_id);
    if !preenter.go() {
        append_audit(&root, &group, brand::AUDIT_ACTOR, "delivery-aborted-human-input", json!({
            "to": agent, "stage": "pre-enter", "reason": "box occupied at submit time",
        }));
        record_aborted_preenter_outcome(
            &last_delivery, pty_id, delivery_from, Some(pasted_text.clone()));
        return DeliverOutcome::AbortedPreEnter(preenter.enqueue_reason());
    }
    // #112 round 2: Tier 1's precondition, OBSERVED right here rather
    // than assumed from `echoed` (round 1's mistake — `echoed` is a
    // raw >=8-byte growth check with no content comparison at all,
    // so it's satisfied identically whether the CLI echoed our
    // LITERAL text or collapsed a long paste to a `[Pasted text #N
    // +M lines]`-style placeholder; see the design note's "Tier 1
    // precondition" section for the live episode that caught this).
    // Tier 1 can only govern (two-sided) THIS delivery if our own
    // pasted text is actually findable, verbatim, in the box right
    // now — checked once, here, against the real tail. When it
    // isn't (a long/collapsed paste, or the echo genuinely never
    // landed), Tier 1 declines to govern and this delivery falls
    // back to the round-1 hook-or-burst precedence exactly as
    // before — audited explicitly as its own state, never silently.
    //
    // #559: the read is sized from the paste, not from the flat
    // `BOX_TAIL_SCAN_BYTES` — with a 4 KiB window and a flush cap of 24 KiB, a
    // coalesced paste made this precondition structurally false and Tier 1
    // declined every large delivery without ever looking. #685: and it is sized
    // in POST-STRIP characters, because the pane returns about half of what it
    // is asked for and a slack spent in raw bytes was under-covering every
    // paste past ~1.5 KiB — see `Tier1Scan`.
    //
    // Every OTHER box read for this delivery (the confirm loop, the retry loop,
    // and the late monitor's two) goes through a `Tier1Scan` built the same
    // way, and none of them can be NARROWER than this one: a precondition
    // verified against a wide tail and then polled against a narrow one would
    // read the box as cleared on the very first poll and confirm a delivery
    // nothing observed — the one failure mode strictly worse than declining.
    let mut tier1_scan = Tier1Scan::for_paste(&pasted_text);
    // #583: the raw byte count is kept beside the stripped text — one read,
    // measured as well as classified. A census taken from a SECOND read would
    // answer a question nobody asked (a different tail, a different instant),
    // so both come out of this one. (A read that widened itself is still ONE
    // read in this sense: only its final tail is classified or measured.)
    let tier1_read = tier1_scan.read(|n| ptys.output_tail_bounded(pty_id, n));
    let tier1_precondition =
        box_reading(tier1_read.as_ref().map(|r| r.stripped.as_str()), &pasted_text);
    let tier1_census = tier1_scan.census(tier1_read.as_ref(), &pasted_text);
    let tier1_governs = tier1_precondition == BoxReading::Holds;
    // The decline is stated, not inferred from an absent `true` (#559). It is
    // carried on `prompt-typed` below rather than as its own event because
    // every delivery writes that record exactly once, so "why did Tier 1 not
    // govern this one" is answerable for every delivery rather than only for
    // the ones that later go wrong.
    let tier1_decline = match tier1_precondition {
        BoxReading::Holds => None,
        declined => Some(declined.as_str()),
    };

    let submit_sent_ms = now_ms();
    // Baseline just before the first Enter, so the confirmation window
    // below measures only the burst that Enter produces.
    let submit_baseline = ptys.output_total(pty_id).unwrap_or(last_total);
    // #454: claim the pane in the ledger BEFORE pressing Enter, not after
    // this delivery's confirm window resolves. Any `promptsubmit` record the
    // Enter below produces is necessarily written after this insert, so an
    // OLDER delivery's late monitor — which reads the ledger after reading
    // the hook — cannot see that record while still believing it owns the
    // pane. See `record_inflight_delivery`'s doc for the full argument and
    // for why this is the same map rather than a second, separately-locked
    // in-flight marker. The final `confirmed` value is written to the same
    // key at the end of the window below; until then the honest state is
    // exactly what this records — a delivery in flight, not yet landed.
    record_inflight_delivery(
        &last_delivery, pty_id, submit_sent_ms, delivery_from.clone(),
        // #813: the paste has landed by this point, so this record can answer
        // "is our text still in that box?" for every later reader.
        Some(pasted_text.clone()),
    );
    let _ = ptys.write_bytes(pty_id, submit);

    // Copilot autopilot consent (#101/#179): a fresh --autopilot copilot
    // opens its "Enable autopilot mode" dialog in response to this FIRST
    // submit (not at boot). Answer it now — Enter selects the default
    // "Enable all permissions", which enables autopilot AND lets the brief
    // we just submitted proceed (verified live: the pending message is not
    // discarded). Gated to a kickoff (fresh OR resumed, #364) of an
    // unattended copilot boot; fail-soft, so if the dialog never shows
    // (already consented, flow changed) delivery just continues to the
    // retries. Must run before the confirm window so the dialog's Enter
    // has landed before we judge whether the turn began.
    //
    // NOT gated by the interactive-question guard above (#420): this dialog
    // can only appear AFTER the kickoff Enter this thread already sent
    // (`submit`, above), while both question-guard checkpoints run BEFORE
    // that Enter — pre-paste and pre-first-Enter — so they never see it. Kept
    // exempt on purpose, not by omission: this watcher answering the dialog
    // IS a deliberate programmatic answer to a pending question, exactly the
    // shape the general guard exists to hold for everyone else.
    if confirm_autopilot {
        confirm_copilot_autopilot_dialog(&ptys, pty_id, &root, &group, &agent, AUTOPILOT_DIALOG_WAIT);
    }

    // Confirm the submit landed (#112 round 2 — three-state redesign,
    // see the design note section of the same name). Tier 1 (box
    // consumption) governs TWO-SIDEDLY — confirmed OR definitively
    // vetoed — whenever `tier1_governs` verified its own precondition
    // above; Tier 2 (the `promptsubmit` hook) can independently
    // confirm at any point regardless of Tier 1's governance, since a
    // positive hook match is real evidence either way; Tier 3 (burst)
    // is consulted ONLY when Tier 1 does not govern this delivery —
    // its own evidence is too weak to override a box-based veto, and
    // the whole point of Tier 1 governing is that burst's usual "any
    // growth" bar can't be trusted to arbitrate against it. Burst's
    // OWN reading is still computed and recorded even when it isn't
    // consulted for the decision — the independent-per-tier
    // requirement this design owes the next live-validation pass.
    let confirm_deadline = std::time::Instant::now() + SUBMIT_CONFIRM_WINDOW;
    let mut confirm_source = ConfirmSource::None;
    let mut confirm_merged = false;
    let mut tier1_reading: Option<bool> = None; // Some(true) = still holds our paste
    let mut tier_hook_matched = false;
    let mut tier_burst_would_confirm = false;
    loop {
        let hook_match = poll_promptsubmit_hook(&hook_marker_path, hook_baseline, &pasted_text);
        if !matches!(hook_match, PromptLandedMatch::None) {
            tier_hook_matched = true;
        }
        if tier1_governs {
            let tail = tier1_scan.read(|n| ptys.output_tail_bounded(pty_id, n));
            match box_reading(tail.as_ref().map(|r| r.stripped.as_str()), &pasted_text) {
                BoxReading::Holds => tier1_reading = Some(true),
                BoxReading::NotHolding => {
                    tier1_reading = Some(false);
                    // #112 round 3 (rev-20 B3): a box-clear caused by
                    // the HUMAN (they typed/submitted/cancelled their
                    // own line) reads identically to one the CLI
                    // cleared for our delivery. Only promote to a
                    // Box confirm when no human input has landed
                    // since our own submit — checked fresh, right
                    // here, not inherited from an earlier tick.
                    if tier1_trusted(ptys.last_user_input_ms(pty_id).unwrap_or(0), submit_sent_ms) {
                        confirm_source = ConfirmSource::Box;
                    }
                }
                // #559: no usable read — the pty is gone (the `output_total`
                // read just below returns `None` too and breaks the loop), or
                // the tail came back shorter than our own paste. Leave
                // `tier1_reading` untouched: an unread box is neither a
                // confirm nor a veto. This is the strictly-safer half of the
                // three-state change — `ConfirmSource::Box` now requires an
                // OBSERVED absence, where the old `Some(_)` arm would take a
                // structurally-foregone `false` as proof the CLI accepted the
                // paste.
                BoxReading::Unverifiable => {}
            }
        }
        if matches!(confirm_source, ConfirmSource::None) && tier_hook_matched {
            confirm_source = ConfirmSource::Hook;
            confirm_merged = matches!(hook_match, PromptLandedMatch::Content { merged: true });
        }
        let Some(observed_total) = ptys.output_total(pty_id) else { break };
        // Tier 3 (burst): computed on every tick regardless of
        // governance (the independent-audit requirement), but only
        // allowed to DECIDE when Tier 1 isn't governing this
        // delivery — its evidence is too weak to arbitrate against a
        // box-based read.
        if submit_confirmed(reached_quiet, submit_baseline, observed_total) {
            tier_burst_would_confirm = true;
            if !tier1_governs && matches!(confirm_source, ConfirmSource::None) {
                confirm_source = ConfirmSource::Burst;
            }
        }
        if !matches!(confirm_source, ConfirmSource::None) {
            break;
        }
        if std::time::Instant::now() >= confirm_deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // #420 rev-19 N8: a CONFIRMED delivery is done — its Enter landed,
    // its turn started, there is nothing left to retry. Running the
    // retry loop (and its question-hold machinery) anyway used to be
    // harmless on the old "Enter on an empty box is a no-op" premise
    // this PR's whole existence disproves: a successful delivery to a
    // Copilot pane that then asks an UNRELATED question would show a
    // false "held: question pending" badge and hold the pane's
    // delivery mutex for up to `QUESTION_HOLD_MAX` — this guard
    // protects DELIVERIES, it is not a general question-watcher for a
    // pane that's already done receiving this one.
    // #112 round 3 (rev-20 B2): whether the retry loop ran to
    // NATURAL exhaustion — every delay slept, never cut short for a
    // question on screen, a human typing, or a failed retry write.
    // Starts true (covers the "never entered the loop at all"
    // case, where `confirm_source` is already decided and this
    // flag is moot) and is flipped false by the SAME three early
    // exits `final_window_outcome` requires ruled out before a veto
    // may fire — see that function's doc for why each one means the
    // box's end state can't be trusted as evidence of non-acceptance.
    let mut window_exhausted_naturally = true;
    if matches!(confirm_source, ConfirmSource::None) {
        'retries: for delay in SUBMIT_RETRY_DELAYS {
            std::thread::sleep(delay);
            // #112: a hook record landing during a retry's sleep means
            // the ORIGINAL Enter actually worked — some CLIs can take
            // longer than `SUBMIT_CONFIRM_WINDOW` to emit the hook
            // under load, exactly the busy-pane case this feature
            // exists for. Stop retrying immediately (plan-14 design:
            // "a hook match also breaks out of the retry loop
            // immediately") rather than risk a redundant Enter
            // blind-selecting whatever now sits highlighted in an
            // unrelated dialog. Checked before Tier 1 for the same
            // reason it's checked first in the main window above —
            // hook evidence is real regardless of which tier governs.
            let hook_match = poll_promptsubmit_hook(&hook_marker_path, hook_baseline, &pasted_text);
            if !matches!(hook_match, PromptLandedMatch::None) {
                tier_hook_matched = true;
                confirm_source = ConfirmSource::Hook;
                confirm_merged = matches!(hook_match, PromptLandedMatch::Content { merged: true });
                append_audit(&root, &group, brand::AUDIT_ACTOR, "submit-retries-skipped",
                    json!({ "to": agent, "reason": "hook confirmed since last check" }));
                break 'retries;
            }
            if tier1_governs {
                // #559: the same `Tier1Scan` as the precondition and the main
                // window — see `tier1_scan`'s comment for why a narrower read
                // here would manufacture a confirm, and `Tier1Scan`'s for why
                // #685's widening cannot produce one.
                let tail = tier1_scan.read(|n| ptys.output_tail_bounded(pty_id, n));
                match box_reading(tail.as_ref().map(|r| r.stripped.as_str()), &pasted_text) {
                    BoxReading::Holds => tier1_reading = Some(true),
                    BoxReading::NotHolding => {
                        tier1_reading = Some(false);
                        // #112 round 3 (rev-20 B3): same trust gate
                        // as the main window above.
                        if tier1_trusted(ptys.last_user_input_ms(pty_id).unwrap_or(0), submit_sent_ms) {
                            confirm_source = ConfirmSource::Box;
                            append_audit(&root, &group, brand::AUDIT_ACTOR, "submit-retries-skipped",
                                json!({ "to": agent, "reason": "box consumed since last check" }));
                            break 'retries;
                        }
                    }
                    // #559: no usable read — never a confirm, never a veto.
                    BoxReading::Unverifiable => {}
                }
            }
            // #420 rev-15 B2: these Enters used to fire unconditionally
            // on a timer, in exactly the window (a few seconds after the
            // first submit, while the agent is starting its turn)
            // Copilot most often paints a permission/question dialog —
            // this PR's own premise made concrete on the path it left
            // unguarded. A question appearing here means HOLD, not
            // retry: re-check and wait for it to clear (capped, same as
            // every other checkpoint) before pressing this retry's
            // Enter; if it never clears, stop retrying rather than risk
            // a later retry blind-selecting an option. `pasted_text`
            // (rev-19 B-A) is the self-echo content mask for every
            // retry — they're all still trying to land the SAME
            // already-pasted text, so masking it out leaves only
            // whatever the CLI itself painted. The human-typing check
            // stays cheap and short-circuits BEFORE touching the
            // (potentially minutes-long) question hold, exactly as it
            // did before this PR — a human mid-line means skip now, not
            // spend up to `QUESTION_HOLD_MAX` finding that out; `retry_
            // gate` (rev-19 N9) only ever sees the question outcome,
            // since this check has already fully handled its own case.
            // #518: the same single derivation as every other consumer. Inside
            // this retry window the bound can never have elapsed (see
            // `HUMAN_INPUT_BLOCK_BOUND_MS`'s doc — it is 2x the delivery
            // machinery's own longest legitimate window), so this call is
            // behaviour-identical to the inline compare it replaces; it is
            // here so no consumer is left deriving the latch its own way.
            if human_input_block_now(&ptys, pty_id, submit_sent_ms).holds() {
                append_audit(&root, &group, brand::AUDIT_ACTOR, "submit-retries-skipped",
                    json!({ "to": agent, "reason": "human typing in pane" }));
                // #112 round 3 (rev-20 B2): a human mid-line means
                // the box's contents aren't about our delivery
                // either way — this is NOT natural exhaustion, so a
                // veto must not fire off this exit.
                window_exhausted_naturally = false;
                break;
            }
            let (question_decision, question_seen_retry) = wait_for_question_clear(
                &ptys, pty_id, Some(&pasted_text), delivered_lines(&reg, pty_id),
                &emit_held, &emit_held_cleared,
            );
            match retry_gate(question_decision) {
                RetryGate::SkipQuestionPending { held_ms } => {
                    append_audit(&root, &group, brand::AUDIT_ACTOR, "submit-retries-skipped", json!({
                        "to": agent, "reason": "question pending", "held_ms": held_ms,
                        "matched": witness_audit(question_seen_retry.as_ref()),
                    }));
                    // #112 round 3 (rev-20 B2 — the blocking finding):
                    // a question on screen may be INTERCEPTING our
                    // Enter, not refusing it — the box's "still
                    // holds our paste" reading proves nothing about
                    // acceptance while a dialog sits in front of it.
                    // NOT natural exhaustion: this delivery must
                    // land `Pending`, never `Failed`, off this exit.
                    // The question-guarded late monitor re-observes
                    // the question state on every subsequent poll,
                    // which this one-shot end-of-window check can't.
                    window_exhausted_naturally = false;
                    break;
                }
                RetryGate::Write { held_ms } => {
                    if held_ms > 0 {
                        append_audit(&root, &group, brand::AUDIT_ACTOR, "delivery-held-for-question", json!({
                            "to": agent, "stage": "retry", "held_ms": held_ms, "outcome": "cleared",
                            "matched": witness_audit(question_seen_retry.as_ref()),
                        }));
                    }
                    if ptys.write_bytes(pty_id, submit).is_err() {
                        // A failed retry write means the pty is
                        // likely closing — the same "can't trust
                        // this as evidence" reasoning applies.
                        window_exhausted_naturally = false;
                        break;
                    }
                }
            }
            // Tier 3 (burst) is measured for the independent audit
            // field even mid-retry, matching the main window's own
            // record-regardless-of-governance discipline.
            if let Some(observed_total) = ptys.output_total(pty_id) {
                if submit_confirmed(reached_quiet, submit_baseline, observed_total) {
                    tier_burst_would_confirm = true;
                    if !tier1_governs {
                        confirm_source = ConfirmSource::Burst;
                        break 'retries;
                    }
                }
            }
        }
    }
    // Tier 1's VETO (#112 round 3 — rev-20 B2/B3, see
    // `final_window_outcome`'s doc for the full precondition list):
    // reachable only once the whole window+retries ran to NATURAL
    // exhaustion — never off an early exit for a question, human
    // typing, or a failed write — with NO other tier having
    // decided, Tier 1 governing this delivery, its own
    // precondition-verified check never once seeing the box clear,
    // AND no human input landing since our own submit. This is the
    // state at the natural end of a ~7.6s window across 15+ polls,
    // not a snap judgment.
    let tier1_trusted_at_end = tier1_trusted(ptys.last_user_input_ms(pty_id).unwrap_or(0), submit_sent_ms);
    confirm_source = final_window_outcome(
        confirm_source, tier1_governs, tier1_reading, window_exhausted_naturally, tier1_trusted_at_end,
    );
    let confirm_state = confirm_state_for(confirm_source);
    let confirmed = matches!(confirm_state, DeliveryConfirmState::Confirmed);

    // Record the outcome so the next delivery to this pane can flush a
    // prompt still stranded in the box (#81/#84). `Pending` reads as
    // `confirmed: false` here — the SAME value it already held before
    // #112 round 2 for an unresolved delivery — so this is not a
    // behavior change to the flush path: a genuinely-queued prompt's
    // box is actually empty by now anyway (Tier 1 not governing means
    // we have no box evidence either way for THIS specific check, but
    // a stray flush Enter on an empty/queued box is the existing,
    // already-accepted "safe: the next delivery's flush just no-ops"
    // posture, unchanged).
    last_delivery
        .lock_safe()
        .insert(pty_id, DeliveryOutcome {
            confirmed,
            submit_sent_ms,
            from: delivery_from.clone(),
            // #813: a CONFIRMED delivery's text went in, so there is nothing
            // stranded to look for; an unconfirmed one's may still be sitting
            // in the box, which is precisely the reading the marker needs.
            stranded_text: (!confirmed).then(|| pasted_text.clone()),
        });
    append_audit(&root, &group, brand::AUDIT_ACTOR, "prompt-typed", json!({
        "to": agent,
        "cli": cli,
        "waited_ms": start.elapsed().as_millis() as u64,
        // #517: `null` when this delivery had no boot wait at all (a
        // mid-session prompt), so "never waited" is distinguishable from
        // "waited and gave up" — the same never-conflate-two-facts rule
        // `tier1_still_holding_at_end` below already follows.
        "ready_observed": wait_ready.then_some(ready_observed),
        "fresh_kickoff": recoverable_kickoff.is_some(),
        "attempts": attempts,
        "echoed": echoed,
        "submit_waited_ms": submit_start.elapsed().as_millis() as u64,
        "submit_confirmed": confirmed,
        // #112 round 2: the 3-state outcome, replacing the old binary.
        "confirm_state": confirm_state.as_str(),
        // Which tier decided (if any) — "box"/"box_veto"/"hook"/
        // "burst" decided it; "none" means still `pending`.
        "confirm_source": confirm_source.as_str(),
        "confirm_merged": confirm_merged,
        // Independent per-tier readings (#112 round 2 hard
        // requirement) — recorded regardless of which tier actually
        // decided, so a live run can compare what EVERY tier said,
        // not just the winner. `tier1_governed` is false whenever
        // this delivery's paste didn't verify literally in the box
        // (a long/collapsed paste — see the `tier1_governs` precondition
        // check's own comment, above, right before the first Enter).
        "tier1_governed": tier1_governs,
        // #559: WHY Tier 1 declined — `null` when it governed, "not-holding"
        // when the tail was long enough to have held our paste and did not,
        // "unverifiable" when the tail we could read was shorter than the
        // paste itself, so the answer was arithmetic rather than observation.
        // The two used to be one indistinguishable `tier1_governed: false`,
        // which is what let an oversized coalesced flush opt out of Tier 1
        // silently. `paste_bytes`/`scan_bytes` are carried alongside so the
        // decline is checkable from the record instead of taken on faith.
        "tier1_decline": tier1_decline,
        "tier1_paste_bytes": pasted_text.len(),
        // #685: the request the precondition read ACTUALLY made, off the census
        // rather than computed beside it — a read that widened itself asked for
        // more than the `tier1_scan_bytes` floor, and two records of one number
        // that can disagree is how an audit stops being evidence.
        "tier1_scan_bytes": tier1_census.requested_bytes,
        // #583: the same read, measured — `Tier1ScanCensus`. Carried on EVERY
        // delivery, `Holds` included, because the question is a distribution:
        // "does the slack survive live ANSI density near the cap" cannot be
        // answered from the failures alone (that sample is selected on the
        // outcome). The successes are where the remaining headroom shows.
        "tier1_scan_census": tier1_census.to_json(),
        // #112 round 3 (rev-20 N3): `null` when Tier 1 never
        // governed this delivery at all (nothing was ever measured,
        // not "measured and false") — a bare `bool` here would
        // conflate "never watched" with "watched and saw it clear",
        // which are different facts a live-validation read needs to
        // tell apart.
        "tier1_still_holding_at_end": tier1_governs.then_some(tier1_reading == Some(true)),
        "tier2_hook_matched": tier_hook_matched,
        "tier3_burst_would_confirm": tier_burst_would_confirm,
    }));
    // Delivery outcome breadcrumb — timing + flags only, never the text.
    crate::obs::breadcrumb(
        "delivery",
        &format!(
            "agent={agent} pty={pty_id} outcome=typed echoed={echoed} confirm_state={} \
             confirm_source={} attempts={attempts} waited_ms={}",
            confirm_state.as_str(),
            confirm_source.as_str(),
            start.elapsed().as_millis() as u64
        ),
    );
    // #112 round 2: the notice fires ONLY for `Failed`, never for
    // `Pending` — the whole point of the three-state redesign. A
    // `Confirmed` delivery draws nothing (as before). A `Pending`
    // delivery (no tier decided within the window) spawns the
    // extended monitor below and gets NO notice yet — silence is the
    // correct behavior for "no evidence yet on a prompt that may
    // simply be queued", not a bug to route around.
    match confirm_state {
        DeliveryConfirmState::Confirmed => {
            // #454: a confirmed delivery is proof the pane is not wedged, so
            // any stranded badge an EARLIER delivery raised is stale and
            // comes down here. This used to happen a tick or two later, on
            // the earlier delivery's own monitor noticing it had been
            // superseded by a CONFIRMED outcome — but that monitor now exits
            // as soon as this delivery publishes itself (before its Enter),
            // which is strictly before this point, so the badge would
            // otherwise stay up for a delivery that demonstrably landed. A
            // `Pending`/`Failed` delivery still leaves it to the monitor
            // spawned below, which owns the pane's state from there.
            if let Some(r) = &reg {
                r.clear_stranded(&group, &agent, "delivery-confirmed");
            }
        }
        DeliveryConfirmState::Failed => {
            if let Some(r) = &reg {
                // #585: `eaten: false`. This is the EARLY alarm, raised inside
                // the submit window by Tier 1's box veto — which fires only
                // because our text WAS observed still sitting in the box. That
                // is the "sitting unsubmitted" case by construction, the exact
                // opposite of an eaten paste, so the original wording is the
                // accurate one here. Only the late monitor, which reads the
                // box again after the pane has gone quiet, can see a delivery
                // whose text is gone.
                // #539: this pane's in-window alarm coalesces with any raised
                // by a still-live monitor for an EARLIER delivery — same
                // (group, agent, eaten) bucket, which is the point.
                r.notify_unconfirmed_delivery(&group, &agent, target_is_orchestrator, false, false, submit_sent_ms);
            }
        }
        DeliveryConfirmState::Pending => {}
    }
    // Spawn the extended, unbounded-by-timeout monitor for anything
    // this window didn't resolve to `Confirmed` — a `Pending`
    // delivery still needs its eventual answer (hook-late-match or
    // genuine idle-without-evidence), and a `Failed` (Tier 1 veto)
    // delivery still needs the chance to be corrected by a late hook
    // match, since that veto isn't infallible (the rejection/error
    // residual). Deliberately a SEPARATE thread, not a continuation
    // of this one: this call's `_guard` holds the per-pty delivery
    // mutex for its own (bounded) lifetime, and the monitor's own
    // lifetime is bounded only by the pty staying alive — holding
    // the delivery mutex that long would block every subsequent
    // delivery to this pane. See `run_late_confirmation_monitor`'s
    // doc for the full mechanism.
    if !matches!(confirm_state, DeliveryConfirmState::Confirmed) {
        let app_for_monitor = app.clone();
        let root_for_monitor = root.clone();
        let group_for_monitor = group.clone();
        let agent_for_monitor = agent.clone();
        let hook_marker_path_for_monitor = hook_marker_path.clone();
        let pasted_text_for_monitor = pasted_text.clone();
        let delivery_from_for_monitor = delivery_from.clone();
        let last_delivery_for_monitor = last_delivery.clone();
        let reg_for_monitor = reg.clone();
        let already_failed = matches!(confirm_state, DeliveryConfirmState::Failed);
        std::thread::spawn(move || {
            run_late_confirmation_monitor(
                app_for_monitor, root_for_monitor, group_for_monitor, agent_for_monitor,
                pty_id, hook_marker_path_for_monitor, hook_baseline, pasted_text_for_monitor,
                delivery_from_for_monitor, submit_sent_ms, target_is_orchestrator,
                already_failed, last_delivery_for_monitor, reg_for_monitor,
                recoverable_kickoff, submit_baseline, ready_observed,
            );
        });
    }
    DeliverOutcome::Done
}

impl OrchRegistry {
    /// The badge janitor (#825 M2): production entry point, resolving the pty
    /// manager the same way every other attention read does. Without an
    /// `AppHandle` there are no ptys to read and nothing to decide, which is
    /// the honest headless answer rather than a degraded one — see
    /// [`Self::stranded_janitor_pass`], which tests drive directly.
    pub fn run_stranded_janitor(&self) {
        let Some(app) = self.app.lock_safe().clone() else { return };
        let ptys = app.state::<crate::pty::PtyManager>();
        let ledger = self.last_delivery.clone();
        self.stranded_janitor_pass(&ptys, &ledger);
    }

    /// One janitor pass over the raised stuck-prompt chips (#825 M2): the
    /// badge-honesty check `run_late_confirmation_monitor` performs every
    /// `LATE_MONITOR_POLL`, run from somewhere that outlives the monitor.
    ///
    /// **It is the same check, not a second one.** Both this and the monitor's
    /// `KeepWaiting` arm take the same `Tier1Scan` + [`box_reading`] every other
    /// reader of our own stranded text takes (#828 fixed that whole family at
    /// once, and a third implementation would leave one of them behind next
    /// time), and both hand the result to [`stranded_badge_release`], which
    /// owns the decision. The only thing this observer cannot supply is
    /// `saw_text_in_box`: it starts, by design, on panes whose monitor is long
    /// gone, so it never witnesses a present→absent transition and must clear
    /// on the stricter #819 bar instead. That asymmetry is load-bearing, and it
    /// is an input to the shared matrix rather than a fork of it.
    ///
    /// **What it reads, and why that survives the monitor.**
    /// `DeliveryOutcome::stranded_text` (#813) is the ledger's own record of
    /// the text we left in a pane's box. It outlives the monitor thread, so
    /// there is nothing new to bookkeep here — every input this needs already
    /// persists. A record carrying no text is skipped before any pane read: a
    /// confirmed delivery clears the field by construction, so "no text" means
    /// there is nothing to look for, not that the box is clear.
    ///
    /// **It can only ever take a chip down or re-word one.** It iterates
    /// entries that already exist in `attn_stranded` and re-words through
    /// [`Self::reword_stranded`], which cannot insert — so a chip a human
    /// dismissed stays dismissed even if this pass judged it microseconds
    /// earlier. Nothing here presses Enter, admits a queue entry, or releases a
    /// hold; the only state it touches is whether a warning is on screen.
    ///
    /// Takes its two impure sources as parameters — the shape
    /// [`drain_stranded_submit`] uses, and for the same reason: the attention
    /// loop's own pty reads go through the `AppHandle`, which a headless
    /// integration test does not have, so a pass wired only to `self.app` would
    /// be untestable end to end.
    #[doc(hidden)] // pub for integration tests
    pub fn stranded_janitor_pass(
        &self,
        ptys: &crate::pty::PtyManager,
        last_delivery: &TrackedMutex<HashMap<u32, DeliveryOutcome>>,
    ) {
        // Snapshot the badged panes, then drop both locks before touching a
        // pty: every read below takes the global `ptys` mutex, and holding
        // registry locks across them pins three locks for the length of the
        // whole pass instead of one at a time (#717's discipline, the shape
        // `attention_inputs` uses). A pane that changes in the gap is handled
        // where it is acted on — `clear_stranded` is a no-op on an absent
        // entry, and `reword_stranded` compares before it writes.
        let panes: Vec<(String, GroupId, u32, Option<StrandedBlocker>)> = {
            let notes = self.attn_stranded.lock_safe();
            // The common case by a wide margin: no chip anywhere, so the pass
            // costs one uncontended map lock and returns without ever looking
            // at the agents map, let alone a pty.
            if notes.is_empty() {
                return;
            }
            let agents = self.agents.lock_safe();
            notes
                .iter()
                .filter(|(_, note)| janitor_watches(note.blocker))
                .filter_map(|(id, note)| {
                    let a = agents.get(id)?;
                    // A pane with no running agent cannot be un-wedged by
                    // anyone, and `attention_tick` prunes its note anyway.
                    if a.status != AgentStatus::Running {
                        return None;
                    }
                    Some((id.clone(), a.group.clone(), a.pty_id?, note.blocker))
                })
                .collect()
        };

        for (agent_id, group, pty_id, judged) in panes {
            let Some(outcome) = last_delivery.lock_safe().get(&pty_id).cloned() else { continue };
            // The ledger's CURRENT stranded text, which is the only text still
            // findable in that box. A newer delivery having superseded the one
            // that raised the chip does not change the question this asks —
            // "is the text loomux most recently left in this pane still there"
            // — and `drain_stranded_submit` reads the record the same way.
            let Some(text) = outcome.stranded_text.clone() else { continue };
            let reading = {
                let mut scan = Tier1Scan::for_paste(&text);
                let read = scan.read(|n| ptys.output_tail_bounded(pty_id, n));
                box_reading(read.as_ref().map(|r| r.stripped.as_str()), &text)
            };
            // `tier1_trusted`'s inverse against THIS record's submit — the same
            // derivation `drain_stranded_submit` takes to name a retirement,
            // deliberately not `human_input_block_now`: that one decides
            // whether it is safe to WRITE, and this pass never writes to a pane.
            let human_stamped_since =
                !tier1_trusted(ptys.last_user_input_ms(pty_id).unwrap_or(0), outcome.submit_sent_ms);
            // `saw_text_in_box: false`, always and by construction — see the
            // header. This observer has no continuous watch to have seen a
            // transition with, so it clears on the stricter #819 bar instead.
            let verdict = stranded_badge_release(judged, Some(reading), human_stamped_since, false);
            if let BadgeRelease::Clear(_) = verdict {
                // A clear by INFERENCE owes its evidence, the way M1's
                // `stranded-dismissed` records the class behind a clear by
                // gesture. `stranded-cleared` alone says `why: human-resolved`,
                // which the drainer's own retirement (#819) also writes — so
                // without this line a human whose chip vanished could not tell
                // which mechanism took it down, nor what it read to decide.
                self.audit(&group, brand::AUDIT_ACTOR, "stranded-janitor", json!({
                    "to": agent_id,
                    "blocker": judged.map(|b| b.as_str()).unwrap_or("self-healing"),
                    "reading": reading.as_str(),
                    "human_since_submit": human_stamped_since,
                }));
            }
            self.apply_stranded_verdict(&group, &agent_id, judged, verdict);
        }
    }

    /// The `QueueFull` re-admission (#825 M3): production entry point, riding
    /// the same divided attention tick as [`Self::run_stranded_janitor`].
    ///
    /// Needs no `AppHandle` to decide anything — its evidence is queue-depth
    /// state, not a pane reading — so unlike the janitor it runs headless. An
    /// app handle is consulted only to nudge a drainer for the marker it
    /// admitted, exactly as `actuate_stranded` does.
    pub fn run_stranded_queuefull_readmit(&self) {
        let ledger = self.last_delivery.clone();
        self.stranded_queuefull_pass(&ledger);
    }

    /// One re-admission pass over the raised stuck-prompt chips (#825 M3): a
    /// pane whose chip says loomux could not even *queue* its re-send, and
    /// whose queue now has room, gets that re-send queued.
    ///
    /// **A retry, not a release — nothing here is cleared on inference.** The
    /// chip is re-worded to the in-flight heal wording (`None`, "loomux is
    /// re-sending it") because a submit really is pending again, and from there
    /// the existing machinery owns it: the marker's own
    /// [`drain_stranded_submit`] re-derives every gate at the press, a
    /// confirmed delivery clears the chip in `deliver_now`, and a marker that
    /// retires does so through #819's reasons. So M3 mints no `stranded-cleared`
    /// `why` of its own — it takes no chip down.
    ///
    /// **What it fixes is a repair gap, not only a badge gap.** Before this,
    /// the Enter that `actuate_stranded` decided to press and could not queue
    /// was never pressed at all, however much room opened later, on any pane
    /// whose late monitor had exited. The badge outliving its truth was the
    /// visible half of that.
    ///
    /// **Why the pass and not the drain edge** is [`queuefull_readmit_gate`]'s
    /// header, and it is the deviation from plan-312's letter: at every real
    /// drain edge a drainer is registered, so an admission there is refused by
    /// construction — and is redundant besides, because the deliveries still
    /// queued behind it each press that Enter on their way in.
    ///
    /// **Never a raise.** It writes through [`Self::reword_stranded`], which
    /// cannot insert, so a chip a human dismissed (M1) stays dismissed even if
    /// this pass admitted a marker microseconds earlier. The marker is still the
    /// honest thing to have queued in that case: a dismissal takes down a
    /// warning, it does not un-strand a prompt.
    ///
    /// Takes the ledger as a parameter, the shape [`Self::stranded_janitor_pass`]
    /// and [`drain_stranded_submit`] both use, so a headless integration test can
    /// drive it end to end.
    #[doc(hidden)] // pub for integration tests
    pub fn stranded_queuefull_pass(
        &self,
        last_delivery: &TrackedMutex<HashMap<u32, DeliveryOutcome>>,
    ) {
        // Snapshot first, then drop both locks before touching the queue map —
        // `attention_tick`'s lock order (`attn_stranded`, then `agents`), the
        // same discipline the janitor pass follows. Nothing is filtered by class
        // here on purpose: `queuefull_readmit_gate` is the one place that names
        // the class, so a second copy of its domain cannot go stale against it
        // (`janitor_watches`' reason, reached the other way round).
        let panes: Vec<(String, GroupId, u32, Option<StrandedBlocker>)> = {
            let notes = self.attn_stranded.lock_safe();
            // The common case by a wide margin: no chip anywhere, so the pass
            // costs one uncontended map lock and returns.
            if notes.is_empty() {
                return;
            }
            let agents = self.agents.lock_safe();
            notes
                .iter()
                .filter_map(|(id, note)| {
                    let a = agents.get(id)?;
                    // A pane with no running agent has nothing to submit into,
                    // and `attention_tick` prunes its note anyway.
                    if a.status != AgentStatus::Running {
                        return None;
                    }
                    Some((id.clone(), a.group.clone(), a.pty_id?, note.blocker))
                })
                .collect()
        };

        for (agent_id, group, pty_id, judged) in panes {
            let depth = self.queue_depth(pty_id);
            if queuefull_readmit_gate(judged, depth, self.drainer_active(pty_id)).is_some() {
                continue;
            }
            // The sender of the delivery whose text is stranded — the same
            // `delivery_from` the late monitor hands `actuate_stranded`, read
            // from the ledger rather than invented, so the marker's queue entry
            // and the `DeliveryOutcome` a successful press writes name the same
            // sender (#560's rule: a record that guesses is worse than none).
            // No record at all means nothing on this pane is still accountable
            // as stranded, and there is nothing to re-submit.
            let Some(from) = last_delivery.lock_safe().get(&pty_id).map(|o| o.from.clone()) else {
                continue;
            };
            let admitted = match self.admit_stranded_selfheal(&group, &agent_id, &from, pty_id) {
                Ok(admitted) => admitted,
                // The queue went back to cap between the gate read and the push
                // — the race the gate is a pre-check for, not a guarantee
                // against. `audit_stranded_push` has already recorded the
                // refusal; the chip stays `QueueFull`, which is still exactly
                // true, and the next pass tries again.
                Err(_) => continue,
            };
            self.audit(&group, brand::AUDIT_ACTOR, "stranded-readmit", json!({
                "to": agent_id,
                "depth": depth,
                "cap": queue::QUEUE_MAX_PER_PANE,
                // `false` = a marker was already queued for this pane
                // (`submit-already-queued`; the drainer-active decline cannot
                // reach here, the gate took it). Either way a submit is pending,
                // which is what the re-worded chip goes on to say — and this
                // line is what explains that wording change to whoever greps.
                "admitted": admitted,
            }));
            // AFTER the admission, never before. `admit_stranded_selfheal` calls
            // `note_queue_capacity`, which treats a chip whose blocker is `None`
            // as nobody's badge (`existing.is_none()`) and would stamp its own
            // depth badge over the in-flight wording. Re-wording second leaves
            // the `QueueFull` chip standing across that call, which that
            // function will not touch — the identical ordering, for the
            // identical reason, as `actuate_stranded`'s.
            self.reword_stranded(&group, &agent_id, judged, None);
            // Best-effort nudge, the shape `actuate_stranded` and
            // `redeliver_lost_kickoff` both use; `ensure_drainer` is idempotent.
            // Without an app handle (headless tests) nothing drains — the marker
            // stays queued, which is the honest state.
            if let (Some(app), Some(reg)) = (self.app.lock_safe().clone(), self.arc()) {
                reg.ensure_drainer(app, group.clone(), pty_id, None);
            }
        }
    }

    /// Type `text` into an agent's CLI: audit, then admit into `pty_id`'s
    /// delivery queue and (front door permitting) kick off the drainer that
    /// actually pastes it. `delivery` classifies the call (see [`Delivery`]):
    /// a kickoff to a just-booted CLI holds the paste until the pane has
    /// painted its UI and gone quiet, and a *fresh* copilot boot
    /// additionally answers the autopilot consent dialog its first submit
    /// triggers (#179); mid-session deliveries do neither.
    ///
    /// **The front door, unified (#470).** EVERY delivery — including the
    /// common case that ends up pasting with zero added latency — is
    /// admitted into `pty_id`'s queue right here, atomically with the check
    /// for whether the queue was empty (`enqueue_text`'s `was_first`). This
    /// replaces the pre-#470 design's two SEPARATE admission paths (race a
    /// raw per-pty mutex when the queue looked empty; append directly when
    /// it didn't) with exactly one, because that split was the actual bug: a
    /// delivery already racing the mutex had no representation in the queue
    /// at all, so a LATER arrival's direct queue-append could land ahead of
    /// it once the mutex-holder's own paste-point recheck deferred to the
    /// tail — losing its arrival position to something that arrived after
    /// it. A reviewer proved this survives even a perfectly FAIR mutex (see
    /// `docs/design/orchestration.md`'s Ordering subsection), which is why
    /// the fix is unifying admission, not fairing the old lock. Only the
    /// admission that observes an empty queue (`was_first`) is responsible
    /// for kicking off `run_queue_drainer`, below, which is now the ONLY
    /// path that ever calls `deliver_now`.
    pub fn deliver_prompt(
        &self,
        agent_id: &str,
        text: &str,
        from: &str,
        delivery: Delivery,
    ) -> Result<(), String> {
        self.deliver_prompt_as(agent_id, text, from, delivery, queue::EnqueueReason::Arrival)
    }

    /// `deliver_prompt` with the admission reason chosen by the caller (#569
    /// rev-128). Every ordinary delivery is `Arrival` and goes through
    /// `deliver_prompt` above; the ONE caller that needs anything else is
    /// `announce_pause_suppression`, whose notice must be admitted past the cap
    /// that destroyed the payloads it is reporting — see
    /// `queue::EnqueueReason::PauseLossNotice`.
    ///
    /// Private on purpose. The cap exemption is bounded by being reachable from
    /// exactly one call site, and a `pub` door with a reason parameter would
    /// make that bound a convention rather than a fact.
    pub(in crate::orchestration) fn deliver_prompt_as(
        &self,
        agent_id: &str,
        text: &str,
        from: &str,
        delivery: Delivery,
        reason: queue::EnqueueReason,
    ) -> Result<(), String> {
        // #633: an unknown agent is the ONE refusal here that stays unaudited,
        // and structurally rather than by choice — `audit` writes into a
        // GROUP's log, `deliver_prompt` is keyed by agent id alone, and an
        // agent that does not exist has no group to file the line under.
        // Writing it anywhere else (a "no group" bucket, the caller's own
        // group) would put a record where nothing reads and no derivation
        // scopes. It is also the one refusal that loses nothing an operator
        // could act on: there is no pane, no session and no payload owner to
        // re-target to — only a caller that named an id that never existed,
        // and that caller is told synchronously.
        let a = self.agent(agent_id).ok_or("unknown agent")?;
        // THE NO-INJECTION GUARANTEE (#1161 M2, requirement 4). A manager pane
        // is the human's own conversation, and no traffic from the fleet is ever
        // typed into it. Stated as the predicate rather than as an absolute,
        // because the absolute is false and the predicate is the contract:
        // `permitted_into_manager_pane` admits exactly three deliveries — the
        // two kickoffs, and `Regrounding`, the post-compact re-grounding notice
        // decision D2 carved out. Everything a producer sends is `MidSession`,
        // and `MidSession` is refused.
        //
        // It is enforced HERE, at the one door every producer already funnels
        // through, rather than at each of them: `channel_send`, `send_prompt`,
        // the watchdog and stall notices, `[loomux] answer to q-N`, the lock
        // and watch notices, the compact nudge and every other `[loomux]` line
        // all reach a pane by calling this function. One refusal covers them
        // all, including the ones a future slice writes without knowing this
        // rule exists — which is what a structural guarantee means and what N
        // conventions at N call sites would not give.
        //
        // Several of those producers are ALSO unreachable for a manager by
        // their own gates (it cannot join a channel, register a watch or take
        // a lock, and `send_prompt` names it with a better error). That is
        // belt-and-braces on purpose, and this is the braces: those gates are
        // convenience and DX, this one is the guarantee. If one of them is
        // ever relaxed, the property still holds.
        //
        // It is refused BEFORE the dead/no-terminal checks and before any
        // admission, so a manager-targeted payload never enters a queue at
        // all — which is what keeps `flush_paused_queues` (which replays
        // persisted entries WITHOUT passing back through this function) from
        // being a second, unguarded door.
        //
        // The permitted set is a property of `Delivery`, not a list here:
        // `permitted_into_manager_pane` is the whole of it, and it is pinned
        // as a set so a fourth carve-out cannot be added quietly. See
        // `docs/design/manager.md`.
        if a.role == Role::Manager && !delivery.permitted_into_manager_pane() {
            self.audit_delivery_refused(
                &a.group, agent_id, from, text, RefusalReason::ManagerPane,
            );
            return Err(format!(
                "{agent_id} is this group's manager — the human's own pane, which takes no \
                 delivery from any agent. Nothing was delivered. Put status in its mailbox with \
                 message_manager, or put a decision to the human with ask_human."
            ));
        }
        // The two PRE-admission refusals (#633). Both wrote nothing before —
        // #615 turned the second from a silent `Ok` into a silent `Err`, which
        // is a better contract for the sender and no record at all for anyone
        // reading afterwards. A refusal with no audit line cannot be surfaced
        // by `front_door_refusals`, by the orphan derivations, or by a human
        // grepping `audit.jsonl`: it is the #579 class exactly.
        //
        // The line carries its own `text`, unlike the queue-full refusal
        // further down, because the `prompt` line is written BELOW this point
        // and deliberately stays there — see `front_door_refusals`'s doc for
        // why `prompt` must keep meaning "offered to a pane".
        if a.status == AgentStatus::Dead {
            self.audit_delivery_refused(
                &a.group, agent_id, from, text, RefusalReason::AgentDead,
            );
            return Err(format!("agent {agent_id} is dead"));
        }
        // #2850 S3b: the delivery KEY, which is a pty id for a PTY pane and a
        // reserved pane id for a structured one. One queue, one drainer, one
        // front door — the two pane kinds differ at the far end of
        // `deliver_now`, not here.
        //
        // `or` and not `and`: exactly one of the pair is ever set, so this
        // cannot silently prefer the wrong one.
        let Some(pty_id) = a.pty_id.or(a.pane_id) else {
            self.audit_delivery_refused(
                &a.group, agent_id, from, text, RefusalReason::NoTerminal,
            );
            return Err("agent has no terminal yet".into());
        };
        // Pause guardrail (#569, option 2 — the human's call): while a group
        // is paused loomux still delivers NOTHING to its panes, so agents
        // finish their turn and idle out exactly as before. What changed is
        // where the payload goes in the meantime: it is ADMITTED to this
        // pane's durable queue and flushed, in arrival order, when the human
        // resumes — rather than destroyed with the caller told `Ok`, which
        // was the last non-crash path where something a sender was told
        // succeeded ceased to exist (#445/#523's "queued means safe" model,
        // with pause as the one hole in it).
        //
        // **No drainer is kicked here, and that is the whole design.** The
        // unpaused path below hands the `was_first` admission responsibility
        // for spawning `run_queue_drainer`; a paused group must not have one
        // running at all, because the point of a pause is that nothing pastes
        // and nothing spends. `resume_group` picks that obligation up
        // (`flush_paused_queues`) for every pane holding entries, which is
        // also what covers a loomux restart in the middle of a pause: the
        // entries come back through `recover_persisted_queue` and the resume
        // is what starts draining them.
        //
        // `run_queue_drainer` ALSO refuses to paste while paused, and that
        // second layer is load-bearing rather than belt-and-braces: a pane
        // rebinding after a restart calls `readmit_recovered`, which kicks a
        // drainer of its own with no idea the group is paused.
        //
        // The `pty_id` resolution moved ABOVE this branch on purpose: a queue
        // is keyed by pane, so a target with no terminal yet has nowhere to
        // hold anything, and the honest answer is the same `Err` an unpaused
        // delivery to that same agent already gets. Pre-#569 it returned `Ok`
        // — a success for a payload that was simply discarded.
        //
        // The `prompt` audit line is the SAME one the unpaused path writes,
        // in the same position relative to admission — because a pause-held
        // delivery is an ordinary delivery now. What distinguishes it lives
        // where it belongs: the `delivery-queued` line's `reason`.
        // DELIVERY TRIAGE (#3304 S1). The rule tier sits HERE — above the
        // `prompt` audit line and above every admission — because a deferred
        // notice must not look like one that was offered to a pane:
        // `front_door_refusals`'s doc makes `prompt` mean exactly that, and a
        // row written for a delivery nobody typed would corrupt every
        // derivation over it, this feature's own wake count first.
        //
        // It is below the manager and dead/no-terminal refusals on purpose.
        // Those are REFUSALS, and a refusal is a fact about the target that
        // triage has no opinion on; putting triage above them would mean a
        // notice bound for a dead pane could be "deferred" into a store that
        // will flush it at a pane that no longer exists.
        //
        // With no `triage:` block — every repo that has not opted in — this
        // resolves to the default before any I/O happens, returns
        // `Deliver { flush: None }`, and the rest of this function is byte for
        // byte what it was.
        //
        // A flush rides IN FRONT: it is admitted first, and the queue is FIFO,
        // so the orchestrator reads what it slept through before the thing
        // that woke it. It goes through `deliver_prompt` rather than being
        // spliced in here, so it is an ordinary delivery with an ordinary
        // `prompt` row — a framed summary of N notices IS a wake, and the
        // audit must be able to say so.
        match self.triage_delivery(&a.group, agent_id, from, text, a.role, delivery) {
            triagegate::Triaged::Deferred => return Ok(()),
            triagegate::Triaged::Deliver { flush } => {
                if let Some(notice) = flush {
                    let _ = self.deliver_prompt(
                        agent_id,
                        &notice,
                        brand::AUDIT_ACTOR,
                        Delivery::MidSession,
                    );
                }
            }
        }
        self.audit(&a.group, from, "prompt", json!({ "to": agent_id, "text": text }));
        if self.is_paused(&a.group) {
            // #620: the entry carries what KIND of delivery this is, because
            // this branch is exactly where that fact would otherwise be lost.
            // Nothing here spawns a drainer (see above), so the treatment a
            // kickoff needs — the boot wait, copilot's autopilot consent,
            // #517's late-kickoff recovery — cannot be threaded on the stack
            // the way the unpaused path below does it; `flush_paused_queues`
            // reads it back off the entry at resume instead.
            let admitted = self.enqueue_text_as(
                &a.group, agent_id, from, text, pty_id, queue::EnqueueReason::GroupPaused,
                delivery,
            )?;
            // #569 review B1: the pause flag is read ABOVE and the admission
            // lands HERE, with no lock across the gap and real I/O inside it
            // (`recover_persisted_queue` plus the durable snapshot). A resume
            // that slips through that gap sees a still-empty queue, so
            // `flush_paused_queues` starts nothing — and then this admission
            // lands in an unpaused group with no drainer running and, because
            // the pause branch is deliberately the one path that never calls
            // `ensure_drainer`, nobody left to start one. Queued, audited,
            // persisted, `Ok` returned to the sender, never delivered: #569's
            // own defect, reproduced by its fix, and self-healing only if some
            // later delivery happens to hit the same pane.
            //
            // Re-reading the flag AFTER the admission closes it without a lock,
            // because the two sides are now ordered against each other rather
            // than merely coexisting: resume-then-admit is caught here (we see
            // `false` and nudge), admit-then-resume is caught by
            // `flush_paused_queues` (it sees the entry). Both firing is
            // harmless — `ensure_drainer` no-ops for a pane already draining,
            // which is exactly why it can be spent freely. This is the same
            // shape as the `!admitted.was_first` nudge below, and the same
            // stranding class `commit_exit` fused a lock scope to close (#470
            // B1); it needs no lock here only because a redundant nudge costs
            // nothing while a missed one costs the payload.
            if !self.is_paused(&a.group) {
                // Audited because the alternative is an invisible race: this is
                // the ONLY evidence that the interleaving happened at all, and
                // a headless test cannot observe `ensure_drainer` (no
                // `AppHandle`, so no thread is ever spawned) — see
                // `a_resume_racing_an_admission_never_strands_it`.
                self.audit(&a.group, brand::AUDIT_ACTOR, "pause-race-nudge", json!({
                    "to": agent_id, "pty": pty_id, "id": admitted.id,
                }));
                if let (Some(app), Some(reg)) = (self.app.lock_safe().clone(), self.arc()) {
                    reg.ensure_drainer(app, a.group.clone(), pty_id, None);
                }
            }
            return Ok(());
        }

        // #470: `Arrival` regardless of whether this lands alone or behind
        // an existing entry — accurate either way (this call site IS the
        // front door, at this delivery's arrival) and unambiguous alongside
        // the SAME audit line's `depth` field, which already distinguishes
        // "landed alone" (depth 1) from "landed behind something" (depth
        // >1) without needing a second field to say the same thing.
        // #620: stamped here too, though nothing reads it back on this path —
        // the drainer this admission may spawn is handed the same facts
        // directly, below. It is recorded so the entry's account of itself is
        // the same whichever branch admitted it: a pause landing on a queue
        // that already holds an unpasted kickoff leaves an entry that still
        // says what it is, and `queue.json` never shows one kickoff as a
        // kickoff and another as a plain prompt purely by admission timing.
        let admitted =
            self.enqueue_text_as(&a.group, agent_id, from, text, pty_id, reason, delivery)?;

        if !admitted.was_first {
            // Landed behind an existing entry (or coalesced into one) — a
            // drainer is already running, or about to be, from whichever
            // delivery got here first; best-effort nudge in case one isn't
            // (the same probabilistic-not-structural gap #445 rev-35 NB2
            // documented: a drainer can pop its last entry and exit in the
            // window between that and this push landing behind "nothing").
            if let (Some(app), Some(reg)) = (self.app.lock_safe().clone(), self.arc()) {
                reg.ensure_drainer(app, a.group.clone(), pty_id, None);
            }
            return Ok(());
        }

        // This delivery landed alone at the front of an idle queue — it
        // owns kicking off the one processor thread that will attempt it.
        // Needs a real app handle to ever run (the same requirement the
        // pre-#470 direct path had); if none exists, undo the admission so
        // an empty queue really means empty rather than silently stranding
        // a payload nothing will ever drain.
        let Some(app) = self.app.lock_safe().clone() else {
            self.withdraw_unprocessable(
                &a.group, agent_id, from, text, pty_id, admitted.id,
                RefusalReason::NoAppHandle,
            );
            return Err("no app handle".into());
        };
        let Some(reg) = self.arc() else {
            // #633: its OWN reason. Both arms used to withdraw under
            // `no-app-handle`, so the log named a cause that had not fired —
            // an app handle plainly existed, since this line is below the one
            // that unwrapped it.
            self.withdraw_unprocessable(
                &a.group, agent_id, from, text, pty_id, admitted.id,
                RefusalReason::RegistryNotShared,
            );
            return Err("registry not shared".into());
        };

        // A booted group copilot agent is launched with `--autopilot`, so it
        // opens the "Enable autopilot mode" consent dialog; the worker thread
        // answers it before pasting the brief. Both kickoff deliveries can show
        // it — a fresh boot AND (#364) a resume, which does NOT reliably
        // restore the consent as previously assumed — while mid-session is long
        // past boot, and only an unattended copilot agent passes --autopilot —
        // so the confirm (and its fail-soft wait) is gated to exactly those cases.
        let confirm_autopilot = {
            let groups = self.groups.lock_safe();
            groups.get(&a.group).is_some_and(|g| {
                should_confirm_copilot_autopilot(
                    g.guardrails.cli_for_block(&a.block, a.role),
                    g.guardrails.auto_ops || a.role == Role::Planner,
                    delivery.confirms_autopilot_dialog(),
                )
            })
        };
        let fresh_first = FreshFirstAttempt {
            id: admitted.id,
            wait_ready: delivery.wait_ready(),
            confirm_autopilot,
            fresh_kickoff: delivery.recovers_lost_kickoff(),
        };
        reg.ensure_drainer(app, a.group.clone(), pty_id, Some(fresh_first));
        Ok(())
    }

    /// This pane's current stranded state, if any (#496 PR-C). Test/read-only
    /// seam; `attention_tick` reads the map directly under its own lock.
    #[doc(hidden)] // pub for integration tests
    pub fn stranded_note(&self, agent_id: &str) -> Option<StrandedNote> {
        self.attn_stranded.lock_safe().get(agent_id).copied()
    }

    /// Current queue depth for `pty_id` (0 if none exists) — a read-only
    /// accessor for tests and any future badge (#445 PR-C). Never mutates.
    #[doc(hidden)] // pub for integration tests
    pub fn queue_depth(&self, pty_id: u32) -> usize {
        self.queues.read().get(&pty_id).map(VecDeque::len).unwrap_or(0)
    }

    /// #814: every pane with something queued right now, as the header badge
    /// shows it — the payload of `orch-queue-depth`.
    ///
    /// **INV-5 (leaf locks).** Two uncontended map locks, taken and released in
    /// turn, never nested: the queue map is snapshotted to
    /// `(pty, depth, oldest, target)` tuples and released BEFORE
    /// `hold_episode_since` is consulted. That order matters because
    /// `hold_episodes` is written from the drainer poll while it holds nothing
    /// else — nesting the other way round would put a delivery path behind this
    /// read. The queue entries themselves are never cloned: what a badge needs
    /// from an 8-entry deque is a count, a minimum and one id, so the loop takes
    /// exactly that under the lock and the payloads stay where they are (P3,
    /// "read the tail you need").
    ///
    /// **The target comes from the queue, and nothing else is consulted for it.**
    /// `note_queue_capacity` needs a three-term resolution (front entry, then
    /// remembered pressure, then `by_pty`) because it also runs on the pop that
    /// EMPTIED the queue, when no entry is left to ask. This never does: an item
    /// exists only while the queue is non-empty, so the front entry is always
    /// there, and a `by_pty` fallback here would be unreachable code plus a map
    /// clone per tick for a case that cannot arise.
    ///
    /// Sorted by pty so a set that has not changed compares equal to the last
    /// one pushed — [`Self::queue_depth_push`]'s skip depends on it, and a
    /// `HashMap` iteration order does not.
    #[doc(hidden)] // pub for integration tests
    pub fn queue_depth_snapshot(&self, now_ms: u64) -> Vec<queue::QueueDepthItem> {
        // (pty, depth, oldest stamp, whose pane it is) — everything a badge
        // needs and nothing else. The payloads stay in the map.
        let panes: Vec<(u32, usize, u64, String)> = {
            let queues = self.queues.read();
            queues
                .iter()
                .filter_map(|(&pty, q)| {
                    let front = q.front()?;
                    let oldest = queue::oldest_enqueued_ms(q)?;
                    // `agent_id` is who the entry is queued FOR, not who sent it.
                    Some((pty, q.len(), oldest, front.agent_id.clone()))
                })
                .collect()
        };
        let mut items: Vec<queue::QueueDepthItem> = panes
            .into_iter()
            .filter_map(|(pty, depth, oldest, agent)| {
                queue::queue_depth_item(
                    pty,
                    &agent,
                    depth,
                    oldest,
                    self.hold_episode_since(pty),
                    now_ms,
                )
            })
            .collect();
        items.sort_by_key(|i| i.pty_id);
        items
    }

    /// #814: the reading to push on this attention tick, or `None` when the
    /// webview already has it.
    ///
    /// Split from the emit so the skip is testable headlessly — an `AppHandle`
    /// is what `run_attention` has and no test in this repo does. Records what
    /// it hands back, so the caller emitting it is the caller's obligation and
    /// a second call in the same state is a no-op either way.
    ///
    /// **The skip is the bound this stream declares** (`test/perfpolicy.test.ts`,
    /// INV-3): an emit is a JS compile on the webview thread, and the reading
    /// only changes when a delivery is queued or drained, when the coarsened
    /// wait ticks over (`queue::coarsen_waiting_ms`), or when a pane crosses the
    /// stalled threshold. An app with nothing queued — the ordinary state —
    /// emits nothing at all.
    ///
    /// **And the skip is itself bounded, because a suppression driven by a
    /// fallible signal owes an independent release** (`performance.md` §2 P4).
    /// The signal here is "the webview already has this set", and it is a
    /// memory of an emit rather than an acknowledgement of one: a webview that
    /// reloaded, an emit that never landed, or a pane restored/spawned after the
    /// last push, all leave this map remembering a badge nobody is wearing —
    /// that last case being why the release is owed even when the webview has
    /// been up the whole time. On a *changing* queue that self-corrects within
    /// a second; on the case that matters most — a queue stalled for an hour,
    /// whose coarsened reading is deliberately stable — it would never correct
    /// at all, and the pane the badge exists for would silently have none. So a
    /// non-empty set is re-pushed unconditionally every
    /// [`QUEUE_DEPTH_REPUSH_MS`], which caps the staleness at that window and
    /// costs, at worst, two emits a minute while anything is queued. An EMPTY
    /// set is not re-pushed: it paints no badge, so a webview that missed it has
    /// nothing to be wrong about — which is what keeps an idle app at zero.
    #[doc(hidden)] // pub for integration tests
    pub fn queue_depth_push(&self, now_ms: u64) -> Option<Vec<queue::QueueDepthItem>> {
        let items = self.queue_depth_snapshot(now_ms);
        let mut last = self.queue_depth_emitted.lock_safe();
        let stale = now_ms.saturating_sub(last.1) >= QUEUE_DEPTH_REPUSH_MS;
        if last.0 == items && !(stale && !items.is_empty()) {
            return None;
        }
        *last = (items.clone(), now_ms);
        Some(items)
    }

    /// A snapshot (oldest first) of `pty_id`'s queue contents.
    ///
    /// Originally test-only introspection (assert ordering/reason/coalesce
    /// count directly instead of only through side effects); #533 made it a
    /// production read as well, since `run_queue_drainer` now plans over the
    /// WHOLE queue rather than peeking one front entry. The clone is taken
    /// under the lock and the lock released before the caller does anything
    /// with it — an admission racing this read lands BEHIND everything in
    /// the snapshot, so the worst case is that it flushes on the next pass.
    #[doc(hidden)] // pub for integration tests
    pub fn queue_snapshot(&self, pty_id: u32) -> Vec<queue::QueuedDelivery> {
        self.queues.read().get(&pty_id).map(|q| q.iter().cloned().collect()).unwrap_or_default()
    }

    /// Path of `group`'s durable queue snapshot (#468). Sits beside
    /// `state.json`/`tasks.json`/`audit.jsonl` in the group dir, because it
    /// is the same kind of thing: state this group cannot afford to lose
    /// when the process does.
    pub(in crate::orchestration) fn queue_snapshot_path(&self, group: &GroupId) -> PathBuf {
        self.group_dir(group).join("queue.json")
    }

    /// Path of `group`'s staged-orphan archive (#547) — the append-only
    /// sibling of `queue.json` that holds staged entries which have rolled off
    /// the hot write path. Named for what it holds rather than for the
    /// mechanism, because the human who opens it after a crash is looking for
    /// orphans, not for an archive.
    pub(in crate::orchestration) fn queue_archive_path(&self, group: &GroupId) -> PathBuf {
        self.group_dir(group).join("queue-orphans-archive.jsonl")
    }

    /// Every delivery this group had queued that a restart caught mid-wait
    /// and that has NOT been re-bound to a live pane (#467) — what the
    /// orchestrator's session-start re-sync reads, via the `queue_orphans`
    /// MCP tool, so lost work is re-derived instead of silently dropped.
    ///
    /// Two derivations, merged (`queue::merge_orphans`, snapshot wins on a
    /// shared id):
    /// - the `queue.json` snapshot (#468), which carries the payload bytes;
    /// - the `audit.jsonl` scan #445 shipped (`delivery-queued` ids with no
    ///   `delivery-dequeued`/`delivery-dropped`), which needs no snapshot
    ///   and so still covers a group whose entries were queued by a build
    ///   before this one — at the cost of knowing the id and target but not
    ///   the text.
    ///
    /// Staged entries that a bind has already re-admitted are gone from the
    /// staging area and closed out in the audit (`delivery-queued` under a
    /// fresh id, then `delivery-dequeued` when it lands), so they do not
    /// appear here twice.
    #[doc(hidden)] // pub for integration tests
    pub fn queue_orphans(&self, group: &GroupId) -> Vec<queue::OrphanedQueueEntry> {
        self.recover_persisted_queue(group);
        let row = |e: &queue::PersistedEntry, reason: String| queue::OrphanedQueueEntry {
            id: e.delivery.id,
            agent_id: e.delivery.agent_id.clone(),
            enqueued_ms: e.delivery.enqueued_ms,
            reason,
            text: e.delivery.payload.text().map(str::to_string),
            source: queue::OrphanSource::Snapshot,
        };
        let mut from_snapshot: Vec<queue::OrphanedQueueEntry> = self
            .recovered_queue
            .lock_safe()
            .get(group)
            .map(|list| list.iter().map(|e| row(e, e.delivery.reason.as_str().to_string())).collect())
            .unwrap_or_default();
        // Markers report too, with `text: null` and their own reason — they
        // are the one case with no bytes left to re-send, and the tool's
        // description points a null payload at the audit log's `prompt`
        // line, which is exactly the right advice here.
        from_snapshot.extend(
            self.recovered_markers
                .lock_safe()
                .get(group)
                .map(|list| {
                    list.iter()
                        .map(|e| row(e, queue::STRANDED_ORPHAN_REASON.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        );
        // #547: and the staged entries that have rolled off the hot snapshot.
        // They belong in the SNAPSHOT half of the merge, not beside the audit
        // derivation, because like a staged row they carry the payload bytes —
        // the archive changed which file holds them, not what is known about
        // them. Filtered against ids still staged, which is the crash window
        // `archive_staged_overflow`'s append-then-remove ordering deliberately
        // leaves open: an entry in both stores is one delivery, reported once.
        let staged_ids: HashSet<u64> = from_snapshot.iter().map(|o| o.id).collect();
        from_snapshot.extend(
            self.archived_orphans(group).into_iter().filter(|o| !staged_ids.contains(&o.id)),
        );
        // Ids sitting in the live queue right now — see `merge_orphans`'s
        // doc for why excluding them is load-bearing and not a tidy-up: an
        // in-flight delivery satisfies the audit derivation's "queued, never
        // resolved" rule exactly, and reporting one as lost work invites the
        // orchestrator to re-send something that is about to arrive.
        let live: std::collections::HashSet<u64> = self
            .queues
            .read()
            .values()
            .flat_map(|q| q.iter())
            .filter(|d| d.group.as_ref() == Some(group))
            .map(|d| d.id)
            .collect();
        queue::merge_orphans(from_snapshot, self.audit_derived_orphans(group), &live)
    }

    /// `queue_orphans` as the MCP tool returns it (#467) — the shape the
    /// orchestrator's session-start re-sync reads.
    ///
    /// `text` is the FULL payload, not a preview, because the whole point of
    /// the tool is that the orchestrator can re-send what was lost, and a
    /// truncated brief is not re-sendable. It is capped at
    /// `ORPHAN_TEXT_CAP_BYTES` all the same — a queued payload can be a
    /// whole task brief and an orchestrator's context is the scarce resource
    /// this codebase spends most carefully — with the cut named in-band
    /// (`truncated`, `text_bytes`) and the audit log's own `prompt` line
    /// holding the original, so a capped entry says so instead of quietly
    /// handing back a shortened brief that reads complete.
    ///
    /// **`refused` is a SECOND list, added additively (#579).** `count`/
    /// `orphans` keep their exact pre-#579 shape — including `id: u64`,
    /// non-null — and refusals sit beside them under their own keys. The two
    /// are different facts and the reader has to act differently on each: an
    /// orphan is a payload loomux still holds and will re-admit if its pane
    /// comes back, a refusal is one that was declined outright and never
    /// existed as a queue entry. See `front_door_refusals` for why an
    /// `Option<u64>` id or a synthetic one was rejected.
    ///
    /// The audit log is read twice here (once for the orphan derivation, once
    /// for the refusals) rather than threaded through both. This tool is called
    /// once per orchestrator session by design, so the second parse is cheaper
    /// than widening `queue_orphans`'s signature to carry entries it does not
    /// otherwise need.
    #[doc(hidden)] // pub for integration tests
    pub fn queue_orphans_json(&self, group: &GroupId) -> Value {
        let orphans = self.queue_orphans(group);
        let now = now_ms();
        let rows: Vec<Value> = orphans
            .iter()
            .map(|o| {
                let full = o.text.as_deref().unwrap_or("");
                let truncated = full.len() > queue::ORPHAN_TEXT_CAP_BYTES;
                let text = queue::clamp_payload(full, queue::ORPHAN_TEXT_CAP_BYTES);
                json!({
                    "id": o.id,
                    "to": o.agent_id,
                    "queued_minutes_ago": now.saturating_sub(o.enqueued_ms) / 60_000,
                    "reason": o.reason,
                    "source": o.source.as_str(),
                    // `null`, never `""` — an audit-derived orphan has no
                    // payload to give, which is a different fact from an
                    // empty one and the orchestrator has to act differently
                    // on it (re-derive from the issue vs. re-send verbatim).
                    "text": o.text.as_ref().map(|_| Value::String(text)).unwrap_or(Value::Null),
                    "text_bytes": o.text.as_ref().map(|t| t.len()),
                    "truncated": truncated,
                })
            })
            .collect();
        let refusals = self.front_door_refusals(group);
        let refused: Vec<Value> = refusals
            .items
            .iter()
            .map(|r| {
                // Capped exactly like an orphan's payload, and named in-band
                // for the same reason: a shortened brief that reads complete
                // gets re-sent as if it were the whole thing.
                let truncated =
                    r.text.as_deref().is_some_and(|t| t.len() > queue::ORPHAN_TEXT_CAP_BYTES);
                json!({
                    "from": r.from,
                    "to": r.to,
                    "refused_minutes_ago": now.saturating_sub(r.refused_ms) / 60_000,
                    // #633: WHY, so the reader knows which of the four
                    // instructions applies. `queue-full-at-call` is the only
                    // value #630 could ever produce, so a reader written
                    // against that build sees no change on the rows it knew.
                    "reason": r.reason.as_str(),
                    // `null` (#633) when the refusal never reached the queue:
                    // a dead-target or no-terminal refusal took no depth
                    // measurement, and `0` would read as "the pane was empty".
                    "queue_depth": r.depth,
                    "enqueue_reason": r.enqueue_reason,
                    "payload": r.payload.as_str(),
                    // The payload's TRUE size as the refusal recorded it —
                    // still known when the bytes themselves are not.
                    "bytes": r.bytes,
                    "preview": r.preview,
                    // `null`, never `""`: "the bytes could not be verified" is
                    // a different instruction from "the payload was empty".
                    "text": r
                        .text
                        .as_deref()
                        .map(|t| Value::String(queue::clamp_payload(t, queue::ORPHAN_TEXT_CAP_BYTES)))
                        .unwrap_or(Value::Null),
                    "truncated": truncated,
                    "consequence": r.consequence,
                })
            })
            .collect();
        json!({
            "count": rows.len(),
            "orphans": rows,
            "refused_count": refusals.total,
            // What the LIST cap left out — stated, never implied by a short list.
            "refused_omitted": refusals.total.saturating_sub(refused.len()),
            // ...and whether the WINDOW the count itself came from was cut
            // (#579 review NB1). Without this, `refused_count: 0,
            // refused_omitted: 0` over a truncated read is the strongest
            // possible claim — "nothing was refused" — made by a scan that
            // never saw the older half of the log.
            "refused_window_truncated": refusals.window_truncated,
            "refused": refused,
        })
    }

    pub fn deliver_to_orchestrator(&self, group: &GroupId, text: &str, from: &str) -> Result<(), String> {
        let orch = self
            .agents
            .lock_safe()
            .values()
            .find(|a| a.group == group && a.role == Role::Orchestrator && a.status != AgentStatus::Dead)
            .map(|a| a.id.clone())
            .ok_or("no live orchestrator in this group")?;
        self.deliver_prompt(&orch, text, from, Delivery::MidSession)
    }

    /// `deliver_to_orchestrator` for a notice that relays ONE agent's own words
    /// to **this group's root** — a `report` note, a `message_orchestrator`
    /// body (#576 residual, rev-163 B1). Identical delivery; the difference is
    /// that this one opts the notice into the question mask's delivery record.
    ///
    /// **"The root", not "the orchestrator" (#2519).** A group has exactly one
    /// root and its class says what kind of group it is: `Role::Orchestrator`
    /// for an orchestration group, `Role::Lead` for one minted by the "orrerix
    /// subagents" toggle. The lookup asks [`Role::is_root`] rather than naming
    /// a class, so the `report` arm needs no branch on which kind of group it
    /// is running in — a child of a lead reports into the lead's pane by
    /// exactly the path a worker reports into an orchestrator's, with the same
    /// scrub, the same maskable marking, and the same `Delivery::MidSession`
    /// admission. The lead OPTED IN to that: receiving its children's reports
    /// is the stated effect of the toggle the human turned on.
    ///
    /// **`Role::Manager` is not a root, and that is load-bearing.** This
    /// function is the only thing that decides who a relayed line is addressed
    /// to, so widening it to "the human's pane" rather than "the root" would
    /// route a report at a manager — which `deliver_prompt` would then refuse,
    /// correctly, but only after the report had been addressed to a pane that
    /// can never receive one. [`Role::is_root`] and [`Role::is_fixture`] differ
    /// by exactly that class; see their docs, `docs/design/manager.md` and
    /// `docs/design/lead-pane.md`. Nothing here weakens the no-injection
    /// guarantee — it keys on `Role::Manager` and the `Delivery` kind at the
    /// door, and neither moves.
    ///
    /// **Why these two and not `deliver_to_orchestrator` generally.** The
    /// promise [`OrchRegistry::mark_notice_maskable`] wants is per FIELD, and
    /// these two notices are the ones whose fields can be enumerated and
    /// checked: `[orrerix] {agent_id} reports {outcome}: {body}` and
    /// `[orrerix] message from {agent_id}: {text}` embed a loomux-minted agent
    /// id, a fixed outcome word, and text `from` wrote — no agent NAME the
    /// orchestrator chose at spawn, no task title, no GitHub-supplied string.
    /// Every other orchestrator-directed notice (queue notices, pause
    /// suppression, flush framing) keeps the default and is never claimable,
    /// which costs only a hold.
    ///
    /// **`from == orch` is the whole check, and it is a CALLERSHIP check
    /// (rev-163 B3).** It closes an orchestrator relaying to itself directly.
    /// It does not — and cannot — close an orchestrator that instructs a worker
    /// to report words it chose: `from` is then the worker, the check passes,
    /// and the line lands in the orchestrator's own pane. Both call sites
    /// target the root, so every claimable line in the system is
    /// delivered to the pane of the one agent best placed to dictate its
    /// content. That is the accepted residual, argued in full at
    /// [`mask_loomux_notices_with_record`]; it is recorded here because this is
    /// where the check that does not cover it lives. It carries over to a lead
    /// unchanged and reads the same way: the human driving that pane is the one
    /// best placed to dictate what their own children say back to them.
    ///
    /// `message_orchestrator` refuses an orchestrator caller a layer up;
    /// `report` does not, which is why the check lives here rather than at
    /// either call site.
    pub(in crate::orchestration) fn deliver_relayed_to_root(&self, group: &GroupId, text: &str, from: &str) -> Result<(), String> {
        let root = self
            .agents
            .lock_safe()
            .values()
            .find(|a| a.group == group && a.role.is_root() && a.status != AgentStatus::Dead)
            .map(|a| a.id.clone())
            .ok_or("no live orchestrator or lead in this group")?;
        if from != root {
            self.mark_notice_maskable(&root, text);
        }
        self.deliver_prompt(&root, text, from, Delivery::MidSession)
    }

    /// `deliver_to_orchestrator` with a chosen admission reason — the one
    /// caller is `announce_pause_suppression` (#569 rev-128). Private for the
    /// reason `deliver_prompt_as` is.
    pub(in crate::orchestration) fn deliver_to_orchestrator_as(
        &self,
        group: &GroupId,
        text: &str,
        from: &str,
        reason: queue::EnqueueReason,
    ) -> Result<(), String> {
        let orch = self
            .agents
            .lock_safe()
            .values()
            .find(|a| a.group == group && a.role == Role::Orchestrator && a.status != AgentStatus::Dead)
            .map(|a| a.id.clone())
            .ok_or("no live orchestrator in this group")?;
        self.deliver_prompt_as(&orch, text, from, Delivery::MidSession, reason)
    }
}
