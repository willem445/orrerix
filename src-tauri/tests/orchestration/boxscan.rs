//! The input-box scan: aggregate bounds, markers, idle notices, lost deliveries and the scan window.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ─────────────── #518: the aggregate bound on the human-input block ───────────────
//
// `human_typed_since` used to be five inline copies of
// `last_user_input_ms > submit_sent_ms` — `tier1_trusted`'s inverse, and an
// unbounded LATCH. One stamp after our own submit pinned it true for the whole
// life of the delivery AND its late monitor (`LATE_MONITOR_MAX_LIFETIME`, four
// hours), and the monitor re-asserted the badge off it on every poll. So a
// false stamp did not merely mislead one decision, it converted into an
// indefinite badged hold that only a human's physical Enter could clear —
// #518's live symptom, and the same class as #513.
//
// #500 bounded the idle tick's twin of this signal and stated the rule: a
// suppression driven by a fallible signal must be BOUNDED, because nothing
// else will ever clear it. This is that rule's delivery-hold sibling. It
// differs from #500's in one deliberate way: #500's clamp is
// classification-blind and releases purely on elapsed time, while this one
// releases only on POSITIVE EVIDENCE that there is nothing of the human's to
// clobber (`input_pending` false). That is what lets it coexist with #510's
// absolute — never submit over genuine human content — instead of eroding it.

#[test]
fn human_input_block_never_times_out_while_the_box_holds_human_content() {
    // THE safety property, and the one a future edit must not reorder. The
    // bound exists because a TIMESTAMP goes stale; occupancy does not. However
    // ancient the keystroke, if the human's characters are still sitting in
    // the box, the block stands — there is something there to merge-submit
    // over, which is exactly what #510/#111/#81 forbid.
    let ancient = 1_000u64;
    let now = ancient + HUMAN_INPUT_BLOCK_BOUND_MS * 100; // a hundred bounds later

    assert_eq!(
        human_input_block(ancient, 0, true, now, HUMAN_INPUT_BLOCK_BOUND_MS),
        HumanInputBlock::Blocked,
        "human characters outstanding in the box must outrank the bound, forever"
    );
    // Mutation this catches: moving the `box_pending` arm below the elapsed
    // check (or dropping it) makes this BoundedOut — i.e. makes loomux press
    // Enter on a person's half-written line, the exact clobber the whole
    // guard exists for.
    assert_eq!(
        human_input_block(ancient, 0, false, now, HUMAN_INPUT_BLOCK_BOUND_MS),
        HumanInputBlock::BoundedOut,
        "with the box empty, the same ancient timestamp IS stale — this is the pair that \
         proves the box reading, not the clock, is what held the block up above"
    );
}

#[test]
fn human_input_block_table() {
    let bound = HUMAN_INPUT_BLOCK_BOUND_MS;
    let submit = 10_000u64;

    // No keystroke evidence after our own submit: nothing to bound, and the
    // answer is `None` rather than a released block — the ordinary case, and
    // distinguishable in the audit from a block we decided had gone stale.
    assert_eq!(human_input_block(0, submit, false, submit + 1, bound), HumanInputBlock::None);
    assert_eq!(
        human_input_block(submit, submit, false, submit + 1, bound),
        HumanInputBlock::None,
        "a stamp AT our submit is not after it — `tier1_trusted` is <=, unchanged"
    );

    // Evidence after our submit, still fresh: blocked, exactly as before #518.
    assert_eq!(
        human_input_block(submit + 1, submit, false, submit + 1 + bound / 2, bound),
        HumanInputBlock::Blocked,
        "inside the bound nothing changes — #518 must not shorten any existing hold"
    );

    // The boundary itself, pinned: `>=` fires, one millisecond short does not.
    assert_eq!(
        human_input_block(submit + 1, submit, false, submit + bound, bound),
        HumanInputBlock::Blocked,
    );
    assert_eq!(
        human_input_block(submit + 1, submit, false, submit + 1 + bound, bound),
        HumanInputBlock::BoundedOut,
    );

    // `bound_ms == 0` disables the bound rather than making it fire instantly.
    // Getting this backwards would turn a mis-set constant into a clobber, so
    // the polarity is pinned rather than left to read naturally.
    assert_eq!(
        human_input_block(submit + 1, submit, false, submit + 1 + bound * 10, 0),
        HumanInputBlock::Blocked,
        "0 means the bound is OFF (pre-#518 behaviour), never 'always expired'"
    );

    // A clock that jumped backwards must not underflow into a huge elapsed.
    assert_eq!(
        human_input_block(submit + 1_000, submit, false, submit, bound),
        HumanInputBlock::Blocked,
        "saturating_sub: a backwards clock reads as no time passed, never as expired"
    );

    // And `holds()` — the boolean every call site actually consumes.
    assert!(!HumanInputBlock::None.holds());
    assert!(HumanInputBlock::Blocked.holds());
    assert!(
        !HumanInputBlock::BoundedOut.holds(),
        "a bounded-out block must read FALSE to its consumers — that is the entire fix"
    );
}

#[test]
fn a_stale_human_input_block_releases_the_real_stranded_press() {
    // The WIRING, not just the decision (#40's twice-bitten lesson): the real
    // replay function `run_queue_drainer` calls, against a real PtyManager
    // backed by a fake child, with production `now_ms()` and the shipped
    // bound. The pane is back-dated rather than slept out — see
    // `set_user_input_ms_for_test`.
    //
    // The scenario is #518's, exactly: a keystroke-shaped stamp landed after
    // our submit with no human present, the box holds nothing of the human's,
    // and eleven minutes have passed with no new evidence. Before this fix the
    // press declined here forever and the prompt sat unsubmitted behind a
    // badge.
    let pm = PtyManager::default();
    let pty = 518_10u32;
    let captured = pm.register_fake_for_test(pty, STRANDED_TAIL.as_bytes());
    let last_delivery: TrackedMutex<HashMap<u32, _>> = TrackedMutex::new("test_last_delivery", HashMap::new());
    // Our submit is twelve minutes ago; the phantom stamp landed a minute
    // after it, and nothing has touched the pane since. `last > submit` (the
    // latch is SET) and `now - last >= bound` (it has gone stale).
    let stamp = now_ms() - (HUMAN_INPUT_BLOCK_BOUND_MS + 60_000);
    record_stranded_outcome_at_for_test(&last_delivery, pty, "orrerix".to_string(), stamp - 60_000, None);
    pm.set_user_input_ms_for_test(pty, stamp);
    assert_eq!(
        pm.input_pending(pty),
        Some(false),
        "precondition: nothing of the human's is in the box — the bound may only release on this"
    );

    let action = drain_stranded_submit(&pm, &last_delivery, "orrerix".to_string(), pty, b"\r", Vec::new());

    assert_eq!(
        action,
        StrandedMarkerAction::Press,
        "stale evidence plus an empty box must release the press — without the bound this \
         declines forever and the prompt stays unsubmitted (#518's live symptom). #813 must \
         NOT retire this one: a `BoundedOut` stamp may be a phantom auto-reply, so our text \
         may really still be sitting in that box"
    );
    assert_eq!(&*captured.lock().unwrap(), b"\r", "exactly one submit, nothing else");
}

#[test]
fn a_fresh_human_keystroke_still_blocks_the_real_stranded_press() {
    // The red-line companion: same path, same fake pane, but the keystroke is
    // real and recent. #510's rule is untouched — loomux does not press Enter
    // over content a human may be mid-way through.
    let pm = PtyManager::default();
    let pty = 518_11u32;
    let captured = pm.register_fake_for_test(pty, STRANDED_TAIL.as_bytes());
    let last_delivery: TrackedMutex<HashMap<u32, _>> = TrackedMutex::new("test_last_delivery", HashMap::new());
    // Our submit is five seconds ago, not `now`: `tier1_trusted` is `<=`, so a
    // ledger stamped in the same millisecond as the keystroke below would read
    // as "no human input since our submit" and this test would pass for the
    // wrong reason (it did, on the first run — the press fired because the two
    // timestamps tied, not because the guard let it through).
    record_stranded_outcome_at_for_test(&last_delivery, pty, "orrerix".to_string(), now_ms() - 5_000, None);

    // A genuine keystroke, through the real signal path: a real key event
    // (`human_origin: true`) carrying real content.
    pm.note_user_input(pty, "wait, hold on", true);
    assert!(
        pm.last_user_input_ms(pty).unwrap_or(0) > now_ms() - 5_000,
        "precondition: the keystroke lands strictly after the delivery's own submit"
    );

    let action = drain_stranded_submit(&pm, &last_delivery, "orrerix".to_string(), pty, b"\r", Vec::new());

    assert_eq!(
        action,
        StrandedMarkerAction::Retry(queue::EnqueueReason::BoxOccupied),
        "a live human keystroke that left CHARACTERS in the box must still block the press"
    );
    assert!(captured.lock().unwrap().is_empty(), "no bytes may reach a pane a human is typing in");
}

#[test]
fn a_stale_block_with_human_text_still_in_the_box_never_releases_the_press() {
    // The wired form of the precedence property: the timestamp is as stale as
    // the pure test's, but this pane's occupancy counter says the human's
    // characters are still there. The press must still decline. This is the
    // test that fails if anyone "simplifies" the bound into a plain timeout.
    let pm = PtyManager::default();
    let pty = 518_12u32;
    let captured = pm.register_fake_for_test(pty, STRANDED_TAIL.as_bytes());
    let last_delivery: TrackedMutex<HashMap<u32, _>> = TrackedMutex::new("test_last_delivery", HashMap::new());
    record_stranded_outcome_at_for_test(
        &last_delivery,
        pty,
        "orrerix".to_string(),
        now_ms() - (HUMAN_INPUT_BLOCK_BOUND_MS + 120_000),
        None,
    );

    // Human types a line and leaves it sitting; then time passes. The
    // back-date happens AFTER the write, since the write stamps `now`.
    pm.note_user_input(pty, "half a thought", true);
    assert_eq!(pm.input_pending(pty), Some(true), "precondition: their line is in the box");
    let stamp = now_ms() - (HUMAN_INPUT_BLOCK_BOUND_MS + 60_000);
    pm.set_user_input_ms_for_test(pty, stamp);

    let action = drain_stranded_submit(&pm, &last_delivery, "orrerix".to_string(), pty, b"\r", Vec::new());

    assert_eq!(
        action,
        StrandedMarkerAction::Retry(queue::EnqueueReason::BoxOccupied),
        "an empty box is what licenses the bound — with the human's line still there, \
         staleness must buy nothing at all"
    );
    assert!(captured.lock().unwrap().is_empty());
}

// ---------- #813: a marker is a repair, not a payload ----------
//
// The live incident, in the human's own sequence: a workstation lock left a
// stranded prompt in three orchestrator panes; the human clicked into a pane
// and pressed Enter themselves to submit it; from that moment the pane's queue
// delivered NOTHING — every steering prompt behind the `StrandedSubmit` marker
// was silently held — and the "⚠ stuck prompt" chip stayed up until a restart.
//
// The mechanism is a deadlock by construction. `admit_stranded_selfheal` pushes
// the marker to the FRONT of the pane's queue; the drainer only ever drains the
// front; and the marker's press is gated on `human_input_block`, which re-arms
// for HUMAN_INPUT_BLOCK_BOUND_MS from the human's LAST keystroke. So the human's
// own recovery action — press Enter, then keep typing to talk to the CLI — is
// precisely what pins the head of the queue, for as long as they stay engaged.
//
// WHAT THE RETIREMENT IS ALLOWED TO REST ON is the whole of the review round
// that shaped these tests, and it is why every case below is written in terms of
// a `BoxReading` rather than a keystroke:
//
//   - Retiring on "a stamp landed and `input_pending` is false" cannot tell a
//     person from a #518 phantom, and it fires strictly INSIDE the bound, so it
//     made `BoundedOut` unreachable from this path while writing "human-resolved"
//     into the audit for a pane nobody had touched.
//   - And `!box_pending` is not "the box is empty": `input_box_len` counts what
//     the HUMAN typed, never loomux's own paste. Retiring there could leave OUR
//     prompt sitting in the box for the next queue entry to paste on top of and
//     submit as one merged prompt — the #81/#84/#111 collision, re-opened by the
//     repair meant to help.
//
// So a retirement requires a positive `BoxReading::NotHolding`, and the keystroke
// record only picks the NAME. `Holds`, `Unverifiable` and "no text on record" all
// fall through to the ordinary gates — nothing retires on an absence of evidence.

/// The pane tail a stranded prompt leaves: our text sitting unsubmitted after
/// the box's `>` marker. `box_holds_paste` reads this as `Holds`.
pub(crate) const STRANDED_PROMPT: &str = "please post the status roll-up on #496";

/// The same pane after the prompt went in: long enough post-normalize to be an
/// informative absence rather than `Unverifiable`, and containing no copy of
/// the prompt itself.
pub(crate) const CLEARED_TAIL: &str =
    "  ⎿  done\n\n● Posted it, linked both follow-ups, and left the thread open for review.\n\n> \n";

#[test]
fn a_human_who_submits_the_stranded_prompt_retires_the_marker() {
    // THE incident cell. Our text is verifiably GONE from the box, and a human
    // keystroke is on record since our submit — a person dealt with it, which is
    // exactly what the queue was waiting for and could not see. Before #813 this
    // was "retry forever", holding every steering prompt behind it.
    assert_eq!(
        stranded_marker_action(
            Some(false), Some(BoxReading::NotHolding), true,
            HumanInputBlock::Blocked, false, || false,
        ),
        StrandedMarkerAction::Retire(StrandedRetireReason::HumanResolved)
    );
    // Still retires with a question on screen: retiring writes nothing, so a
    // live dialog is not a reason to keep holding the queue.
    assert_eq!(
        stranded_marker_action(
            Some(false), Some(BoxReading::NotHolding), true,
            HumanInputBlock::Blocked, false, || true,
        ),
        StrandedMarkerAction::Retire(StrandedRetireReason::HumanResolved)
    );
    assert!(
        StrandedRetireReason::HumanResolved.resolves_the_pane(),
        "and this is the one retirement that takes the chip down"
    );
}

#[test]
fn text_gone_with_nobody_at_the_keyboard_retires_under_its_own_name() {
    // Same reading, no keystroke: the CLI consumed our text, collapsed it to a
    // placeholder, or it never really landed. Nothing left to press Enter
    // against either way — but calling it `human-resolved` would put a person in
    // the audit trail who was never there, and it must NOT take the badge down:
    // this is `StrandedBlocker::NotHolding`'s own situation, whose wording is
    // "never confirmed and its text is gone — check the pane".
    assert_eq!(
        stranded_marker_action(
            Some(false), Some(BoxReading::NotHolding), false,
            HumanInputBlock::None, false, || false,
        ),
        StrandedMarkerAction::Retire(StrandedRetireReason::TextGone)
    );
    assert!(!StrandedRetireReason::TextGone.resolves_the_pane());
    assert!(!StrandedRetireReason::NothingStranded.resolves_the_pane());
}

#[test]
fn a_marker_never_retires_while_our_text_is_still_in_the_box() {
    // The #111 collision guard, and the reason the retirement is keyed on the
    // TEXT. `input_pending` is false here — no HUMAN characters are outstanding
    // — and the first cut of this fix retired on exactly that. But our own
    // prompt is still sitting in the box (`Holds`), and `deliver_now` does not
    // abort when `flush_stranded_text` declines, so the next queue entry would
    // paste on top of it and submit both prompts merged as one.
    //
    // Every combination that could tempt a retirement must keep holding.
    for block in [HumanInputBlock::None, HumanInputBlock::Blocked, HumanInputBlock::BoundedOut] {
        for stamped in [false, true] {
            let action = stranded_marker_action(
                Some(false), Some(BoxReading::Holds), stamped, block, false, || false,
            );
            assert!(
                !matches!(action, StrandedMarkerAction::Retire(_)),
                "a `Holds` reading must never retire (block={block:?} stamped={stamped}), got {action:?}"
            );
        }
    }
    // And specifically: the exact shape the first cut retired on.
    assert_eq!(
        stranded_marker_action(
            Some(false), Some(BoxReading::Holds), true,
            HumanInputBlock::Blocked, false, || false,
        ),
        StrandedMarkerAction::Retry(queue::EnqueueReason::BoxOccupied)
    );
}

#[test]
fn a_marker_never_retires_on_a_reading_it_could_not_take() {
    // `Unverifiable` ("we looked and could not tell") and `None` ("we have
    // nothing to look for") are different facts, and neither is evidence. Both
    // take the conservative branch — the one direction that could re-open either
    // hazard is retiring on an absence.
    for reading in [Some(BoxReading::Unverifiable), None] {
        let action = stranded_marker_action(
            Some(false), reading, true, HumanInputBlock::Blocked, false, || false,
        );
        assert!(
            !matches!(action, StrandedMarkerAction::Retire(_)),
            "reading {reading:?} is not evidence of anything, got {action:?}"
        );
    }
}

#[test]
fn a_phantom_stamp_cannot_retire_a_marker_and_518s_bound_stays_reachable() {
    // #518's whole premise is that a terminal auto-reply can be misclassified as
    // a keystroke. The first cut of #813 retired on (stamp + empty occupancy),
    // which is precisely a phantom's shape — and since `BoundedOut` is only
    // reachable with `!box_pending`, that retirement fired strictly BEFORE the
    // bound could ever release, making #518 unreachable from this path.
    //
    // Now the phantom holds, exactly as #518 intends...
    assert_eq!(
        stranded_marker_action(
            Some(false), Some(BoxReading::Holds), true,
            HumanInputBlock::Blocked, false, || false,
        ),
        StrandedMarkerAction::Retry(queue::EnqueueReason::BoxOccupied),
        "a stamp that may be a phantom must hold, not retire"
    );
    // ...and the bound is genuinely reachable: once the stamp goes stale over an
    // empty box, `holds()` reads false and the press happens, which is the whole
    // of #518's own fix and the only thing that releases a phantom.
    assert_eq!(
        stranded_marker_action(
            Some(false), Some(BoxReading::Holds), true,
            HumanInputBlock::BoundedOut, false, || false,
        ),
        StrandedMarkerAction::Press,
        "#518's bound must still release the press — this is what the first cut made unreachable"
    );
}

#[test]
fn a_marker_with_nothing_stranded_is_retired_rather_than_retried_forever() {
    // `should_flush_before_paste` required `Some(false)` and every other ledger
    // state simply declined — forever, at the head of the queue. `Some(true)` is
    // a pane whose later delivery confirmed; `None` is the pane a restart left
    // with a persisted queue and no in-memory ledger. Neither can ever become
    // `Some(false)` again, so retrying is a permanent block.
    assert_eq!(
        stranded_marker_action(
            Some(true), Some(BoxReading::Holds), false,
            HumanInputBlock::None, false, || false,
        ),
        StrandedMarkerAction::Retire(StrandedRetireReason::NothingStranded)
    );
    assert_eq!(
        stranded_marker_action(None, None, false, HumanInputBlock::None, false, || false),
        StrandedMarkerAction::Retire(StrandedRetireReason::NothingStranded)
    );
}

#[test]
fn the_human_resolved_retire_never_fires_over_outstanding_human_characters() {
    // #510's absolute, unchanged: a human's own line is in the box, so nothing
    // here may write and nothing may conclude they are done with it — not even
    // on a `NotHolding` reading of OUR text, which says nothing about theirs.
    assert_eq!(
        stranded_marker_action(
            Some(false), Some(BoxReading::Holds), true,
            HumanInputBlock::Blocked, true, || false,
        ),
        StrandedMarkerAction::Retry(queue::EnqueueReason::BoxOccupied)
    );
}

#[test]
fn a_retry_names_the_gate_that_actually_declined() {
    // The drainer used to report EVERY declined marker as
    // `EnqueueReason::Question`, sending the orchestrator to look for a dialog
    // that a box-occupied hold never painted — the same mislabel #532 rev-12 NB1
    // closed on the `AbortedPreEnter` path, arriving here.
    assert_eq!(
        stranded_marker_action(
            Some(false), Some(BoxReading::Holds), false,
            HumanInputBlock::None, true, || true,
        ),
        StrandedMarkerAction::Retry(queue::EnqueueReason::BoxOccupied),
        "box occupancy outranks the question, and it is the box that gets named"
    );
    assert_eq!(
        stranded_marker_action(
            Some(false), Some(BoxReading::Holds), false,
            HumanInputBlock::None, false, || true,
        ),
        StrandedMarkerAction::Retry(queue::EnqueueReason::Question)
    );
}

#[test]
fn the_question_gate_is_not_sampled_by_a_decision_that_does_not_need_it() {
    // Answering the question gate costs a 64 KiB grid recomposition
    // (`question_sample` → `termgrid::render_visible`), paid on a 2 s poll. Every
    // decision except the last one reaches its answer without it, and a closure
    // that panics is the only way to pin that they do not ask — a `bool`
    // parameter would make this untestable by construction, which is exactly how
    // it would drift back.
    let never = || -> bool { panic!("the question gate must not be sampled on this path") };
    assert_eq!(
        stranded_marker_action(Some(true), None, false, HumanInputBlock::None, false, never),
        StrandedMarkerAction::Retire(StrandedRetireReason::NothingStranded)
    );
    assert_eq!(
        stranded_marker_action(
            Some(false), Some(BoxReading::NotHolding), true,
            HumanInputBlock::Blocked, false, never,
        ),
        StrandedMarkerAction::Retire(StrandedRetireReason::HumanResolved)
    );
    assert_eq!(
        stranded_marker_action(
            Some(false), Some(BoxReading::Holds), false, HumanInputBlock::None, true, never,
        ),
        StrandedMarkerAction::Retry(queue::EnqueueReason::BoxOccupied)
    );
    assert_eq!(
        stranded_marker_action(
            Some(false), Some(BoxReading::Holds), true, HumanInputBlock::Blocked, false, never,
        ),
        StrandedMarkerAction::Retry(queue::EnqueueReason::BoxOccupied)
    );
}

#[test]
fn a_human_pressing_enter_in_the_pane_retires_the_queued_marker() {
    // The WIRED form of the incident, through `drain_stranded_submit` — the
    // function `run_queue_drainer` actually calls — against a real `PtyManager`
    // with a fake child. A pure test cannot see this: the defect turns on the
    // human's Enter feeding BOTH halves of the old gate in one write (it stamps
    // `user_input_ms` AND zeroes `input_box_len`), and on the pane's tail no
    // longer holding our text, and only the real `PtyManager` produces both.
    let pm = PtyManager::default();
    let pty = 8131u32;
    // The pane AFTER the human submitted our prompt: the box no longer holds it.
    let captured = pm.register_fake_for_test(pty, CLEARED_TAIL.as_bytes());
    let last_delivery: TrackedMutex<HashMap<u32, _>> = TrackedMutex::new("test_last_delivery", HashMap::new());
    // The ledger a stranded delivery leaves — unconfirmed, carrying the text it
    // left in the box — dated a minute back. `tier1_trusted` compares with `<=`,
    // so a record and a keystroke stamped in the same millisecond would read as
    // "no human has typed since our submit": a fact about the test's clock, not
    // the pane.
    record_stranded_outcome_at_for_test(
        &last_delivery,
        pty,
        "orrerix".to_string(),
        now_ms() - 60_000,
        Some(STRANDED_PROMPT.to_string()),
    );

    // The human rescues the pane by hand: click in, press Enter. `\r` with
    // nothing after it classifies `Submit`, which stamps the keystroke clock and
    // empties the occupancy counter in the same write.
    pm.note_user_input(pty, "\r", true);
    assert_eq!(pm.input_pending(pty), Some(false), "precondition: their Enter emptied the box");
    assert!(
        pm.last_user_input_ms(pty).unwrap_or(0) > now_ms() - 5_000,
        "precondition: the keystroke lands strictly after the strand, so the block reads Blocked"
    );

    let action =
        drain_stranded_submit(&pm, &last_delivery, "orrerix".to_string(), pty, b"\r", Vec::new());

    assert_eq!(
        action,
        StrandedMarkerAction::Retire(StrandedRetireReason::HumanResolved),
        "the human already pressed the Enter this marker exists to press — it must leave the \
         queue instead of pinning every steering prompt behind it (#813)"
    );
    assert!(
        captured.lock().unwrap().is_empty(),
        "and it must retire WITHOUT writing: a second Enter into a box a human just cleared is \
         the stray blind press this guard exists to prevent, got {:?}",
        captured.lock().unwrap()
    );
}

#[test]
fn the_wired_marker_holds_while_the_pane_still_shows_our_stranded_text() {
    // The B4 regression, wired: the SAME keystroke evidence as the test above —
    // a human Enter, occupancy back to zero — but the pane's tail still shows our
    // prompt sitting in the box. `input_pending` cannot see our own text (it
    // counts only what the human typed), so the first cut of this fix retired
    // here, and the next queue entry would then paste on top of a prompt still
    // waiting to be submitted and send both as one.
    let pm = PtyManager::default();
    let pty = 8132u32;
    let captured = pm.register_fake_for_test(pty, STRANDED_TAIL.as_bytes());
    let last_delivery: TrackedMutex<HashMap<u32, _>> = TrackedMutex::new("test_last_delivery", HashMap::new());
    record_stranded_outcome_at_for_test(
        &last_delivery,
        pty,
        "orrerix".to_string(),
        now_ms() - 60_000,
        // STRANDED_TAIL's own prompt line, so the box reading is `Holds`.
        Some(STRANDED_PROMPT.to_string()),
    );
    pm.note_user_input(pty, "\r", true);
    assert_eq!(pm.input_pending(pty), Some(false), "precondition: no HUMAN characters outstanding");

    let action =
        drain_stranded_submit(&pm, &last_delivery, "orrerix".to_string(), pty, b"\r", Vec::new());

    assert!(
        !matches!(action, StrandedMarkerAction::Retire(_)),
        "our prompt is still in that box — retiring here hands the next queue entry a merge \
         collision (#81/#84/#111), got {action:?}"
    );
    assert!(
        captured.lock().unwrap().is_empty(),
        "and #518 still holds the press while the keystroke is fresh, got {:?}",
        captured.lock().unwrap()
    );
}
#[test]
fn a_bounded_out_block_lets_the_stranded_decision_reach_self_heal() {
    // The ACTION half of the bound (the brief's "downgrades to
    // deliver-if-box-unchanged"): with the block released, precedence carries
    // the decision into the EXISTING `box_holds_paste` arm, which is already
    // exactly that test. Our text still at the tail => heal it; anything else
    // still lands on a badge, never a blind Enter.
    let heal = |typed, holds: bool| {
        stranded_selfheal_action(
            true,
            typed,
            false,
            if holds { BoxReading::Holds } else { BoxReading::NotHolding },
            0,
            STRANDED_SELFHEAL_MAX_HEALS,
        )
    };

    assert_eq!(
        heal(HumanInputBlock::Blocked.holds(), true),
        StrandedAction::Attention(StrandedBlocker::HumanInput),
        "while the block stands, nothing changes — the badge, not the Enter"
    );
    assert_eq!(
        heal(HumanInputBlock::BoundedOut.holds(), true),
        StrandedAction::SelfHeal,
        "released block + our own text still in the box = the recovery #518 asks for"
    );
    assert_eq!(
        heal(HumanInputBlock::BoundedOut.holds(), false),
        StrandedAction::Attention(StrandedBlocker::NotHolding),
        "released block but a box we cannot account for still badges — the bound buys a \
         re-decision, never a blind press"
    );
}

// ───────────── #522: no actionable notice for a pane that is simply idle ─────────────
//
// The late monitor announced "the prompt may be sitting unsubmitted in its
// pane; get_output it and re-send if needed" about a worker that had finished
// its turn and gone idle at the CLI's rest prompt, waiting on a CI watch.
// Nothing was stuck. The monitor knew only that no `PromptSubmit` hook record
// had shown up — which copilot never produces at all and a claude pane can
// miss — and treated that absence as a strand.
//
// The cost is not noise: each one tells the orchestrator to `get_output` (the
// #520 token flood on an animated pane) and tempts a duplicate re-send, which
// is the double-delivery loop #451/#510 exist to prevent.

#[test]
fn an_idle_pane_produces_no_actionable_unconfirmed_notice() {
    // "Sitting unsubmitted" has a structural meaning, and this is it: our
    // pasted text still identifiably at the box's tail, or human characters
    // outstanding. Neither => the pane is done, not stuck.
    assert_eq!(
        unconfirmed_disposition(BoxReading::NotHolding, false, true, false, false),
        UnconfirmedDisposition::IdleAuditOnly,
        "our paste consumed and no human input outstanding: the delivery landed or is moot — \
         record it, never send the orchestrator to re-deliver something that is not stuck"
    );
}

#[test]
fn a_genuinely_stranded_pane_still_gets_the_notice() {
    // The half that must NOT be softened. #522 narrows WHEN the notice fires,
    // and a fix that narrowed it to nothing would re-open #496's silent wedge.
    assert_eq!(
        unconfirmed_disposition(BoxReading::Holds, false, true, false, false),
        UnconfirmedDisposition::Notify,
        "our text still sitting in the box IS the strand — announce it exactly as before"
    );
    assert_eq!(
        unconfirmed_disposition(BoxReading::NotHolding, true, true, false, false),
        UnconfirmedDisposition::Notify,
        "a human's own line in the box is not an idle pane, and they still need telling"
    );
    assert_eq!(
        unconfirmed_disposition(BoxReading::Holds, true, true, false, false),
        UnconfirmedDisposition::Notify,
        "both at once is the least idle a pane can be"
    );
}

// ───────── #559: a coalesced flush bigger than the box-scan window ─────────
//
// `QUEUE_FLUSH_MAX_BYTES` is 24 KiB; the Tier 1 box scan read a flat 4 KiB.
// Containment cannot hold when the haystack is smaller than the needle, so for
// any coalesced paste past that window `box_holds_paste` was false as a matter
// of arithmetic — decided before the pane was ever consulted. Three consumers
// read that foregone `false` as an observation: the Tier 1 precondition
// declined to govern, the confirm arm would have taken it as "the CLI consumed
// our paste", and `unconfirmed_disposition` classified the strand
// `IdleAuditOnly` — no notice, no self-heal, no badge. Two constants picked for
// unrelated reasons (token budget vs. question-scan cost) jointly decided
// whether a stranded delivery was ever noticed.
//
// The mechanism predates #533; what #533 changed is reachability, by
// deliberately constructing large pastes where they used to be incidental.

/// A paste the shape of a real coalesced flush, `bytes` long and free of
/// whitespace RUNS, so `normalize_prompt_text` is the identity on it and these
/// tests can do exact arithmetic on lengths. ASCII only — they slice it at
/// byte offsets to model a truncated tail read.
fn flush_paste(bytes: usize) -> String {
    let mut s = String::from("[orrerix] 3 queued while this pane was blocked");
    let mut i = 0usize;
    while s.len() < bytes {
        s.push_str(&format!(" from orch-{i}: post the status roll-up on #559 please"));
        i += 1;
    }
    s.truncate(bytes);
    while s.ends_with(' ') {
        s.pop();
    }
    assert!(s.is_ascii(), "the tests slice this at byte offsets");
    s
}

#[test]
fn a_paste_past_the_scan_window_is_unverifiable_not_absent() {
    // The mechanism, stated as arithmetic. The tail is the friendliest content
    // a 4 KiB read could possibly return — the paste's own leading bytes — and
    // the answer is still `false`, because it cannot be anything else.
    let pasted = flush_paste(8 * 1024);
    let truncated_tail = &pasted[..4096];
    assert!(
        !box_holds_paste(truncated_tail, &pasted),
        "a window smaller than the paste cannot contain it, whatever the pane holds"
    );
    assert_eq!(
        box_reading(Some(truncated_tail), &pasted),
        BoxReading::Unverifiable,
        "that `false` is arithmetic, not an observation about the pane — it must not be \
         reported as one"
    );
    // The contrast that makes the distinction load-bearing: a tail long enough
    // to have held the paste, that does not, IS an observation.
    let long_unrelated_tail = "  ⎿  done\n\n> ".to_string() + &"unrelated older output ".repeat(600);
    assert!(long_unrelated_tail.len() > pasted.len());
    assert_eq!(
        box_reading(Some(&long_unrelated_tail), &pasted),
        BoxReading::NotHolding,
        "room to have held it, and it is not there — that is real evidence"
    );
    assert_eq!(
        box_reading(None, &pasted),
        BoxReading::Unverifiable,
        "no read at all is the same nothing as a read too short to decide"
    );

    // The audit vocabulary: three states, three tokens, one spelling each.
    // A reading that lands in the log under a shared name is a reading a
    // later grep cannot tell apart — which is the defect, one layer up.
    let tokens =
        [BoxReading::Holds, BoxReading::NotHolding, BoxReading::Unverifiable].map(BoxReading::as_str);
    assert_eq!(tokens, ["holds", "not-holding", "unverifiable"]);
}

#[test]
fn an_unverifiable_reading_is_never_classified_as_an_idle_pane() {
    // The defect, at the one decision point that produced the silence. #522
    // withholds the notice for a pane that was OBSERVED at rest; a reading
    // that never happened is not that pane.
    assert_eq!(
        unconfirmed_disposition(BoxReading::Unverifiable, false, true, false, false),
        UnconfirmedDisposition::Notify,
        "a box we could not read is not an idle pane — the stranded batch must be announced"
    );
    assert_eq!(
        unconfirmed_disposition(BoxReading::Unverifiable, true, true, false, false),
        UnconfirmedDisposition::Notify
    );
    // #522's own behavior, unchanged: an INFORMATIVE empty box still stays
    // quiet. A fix that notified on everything would just re-open #522.
    assert_eq!(
        unconfirmed_disposition(BoxReading::NotHolding, false, true, false, false),
        UnconfirmedDisposition::IdleAuditOnly,
        "the observed-idle pane must still draw no notice"
    );
}

// ───────── #585: an empty box with no turn behind it is a LOST delivery ─────────
//
// `NotHolding` on a quiet pane has two causes and the reading cannot tell them
// apart: the CLI consumed our text and ran its turn (#522's case, where an
// alarm is noise), or the paste was never accepted at all (a blind paste into
// a CLI whose stdin reader had not attached; an Enter that landed on a busy
// CLI). #522 modelled only the first and returned from `DeclareFailed` ~90
// lines above the `NotHolding` test that gates #517's recovery — so across
// this project's whole recorded audit history `kickoff_recovery_action` fired
// zero times and declined-with-a-reason zero times, against 13
// `delivery-unconfirmed-idle-pane` records and 3 fresh kickoffs whose submit
// was never confirmed. One agent sat idle 11 minutes on a kickoff nobody knew
// was lost; two more had a follow-up brief silently eaten.
//
// The discriminator is `KICKOFF_TURN_EVIDENCE_BYTES` — already trusted, 90
// lines below, to gate the far more dangerous re-delivery decision.

#[test]
fn an_eaten_paste_is_not_an_idle_pane() {
    // THE regression. Box observed empty of our text, empty of the human's,
    // and the pane produced less than a turn's output since our Enter: our
    // text is gone and nothing ever ran on it. Pre-#585 this was
    // `IdleAuditOnly` — silence — which is how a lost kickoff stranded an
    // agent with no badge, no notice, and no recovery.
    assert_eq!(
        unconfirmed_disposition(BoxReading::NotHolding, false, false, false, false),
        UnconfirmedDisposition::EatenNotify,
        "an empty box with no turn behind it is a LOST delivery, not a finished pane — \
         it must escalate, never go quiet"
    );
}

#[test]
fn a_pane_that_finished_its_turn_still_stays_quiet() {
    // The half that must NOT regress: #522's flood stays suppressed exactly
    // where #522 aimed it. A worker that ran a turn and went idle has turn
    // evidence by construction, so it still says nothing. A fix that notified
    // on every empty box would simply re-open #522.
    assert_eq!(
        unconfirmed_disposition(BoxReading::NotHolding, false, true, false, false),
        UnconfirmedDisposition::IdleAuditOnly,
        "a pane that demonstrably ran a turn on our text is done, not eaten — stay quiet"
    );
}

#[test]
fn the_silence_bar_and_the_redelivery_bar_are_the_same_bar() {
    // The structural invariant, and the one this issue is really about: two
    // decisions on the same tick asking the same question ("did the agent act
    // on our text?") must never answer it differently. They disagreed by
    // omission before — the silence path did not consult the bar at all — so
    // pin them to the same threshold rather than trusting a comment.
    //
    // `kickoff_recovery_action`'s other inputs are held at the values that
    // reach the growth test: a fresh kickoff, still outstanding, no human, no
    // question, budget unspent.
    let recovery = |growth: u64| {
        kickoff_recovery_action(
            true, true, false, false, growth,
            KICKOFF_TURN_EVIDENCE_BYTES, true, 0, KICKOFF_REDELIVERY_MAX,
        )
    };

    // One byte below the bar: BOTH say "no turn happened".
    let below = KICKOFF_TURN_EVIDENCE_BYTES - 1;
    assert_eq!(
        unconfirmed_disposition(BoxReading::NotHolding, false, below >= KICKOFF_TURN_EVIDENCE_BYTES, false, false),
        UnconfirmedDisposition::EatenNotify,
        "below the bar the delivery is eaten and must escalate"
    );
    assert_eq!(
        recovery(below),
        KickoffRecovery::Redeliver,
        "below the bar the recovery must fire — the two paths agree the brief was lost"
    );

    // Exactly at the bar: BOTH say "a turn happened".
    let at = KICKOFF_TURN_EVIDENCE_BYTES;
    assert_eq!(
        unconfirmed_disposition(BoxReading::NotHolding, false, at >= KICKOFF_TURN_EVIDENCE_BYTES, false, false),
        UnconfirmedDisposition::IdleAuditOnly,
        "at the bar the pane ran a turn and silence is correct"
    );
    assert_eq!(
        recovery(at),
        KickoffRecovery::Decline(KickoffDecline::TurnStarted),
        "at the bar the recovery must decline — the two paths agree the brief landed"
    );
}

// ───────── #539: the detector finally gets evidence about the AGENT ─────────
//
// Every input `unconfirmed_disposition` had was about the BOX. So a pane whose
// agent had reported and pushed within the same minute — demonstrably working —
// could still draw the actionable "the prompt may be sitting unsubmitted;
// get_output it and re-send" alarm, because `input_pending` said a human had
// characters outstanding. That counter is known to latch >0 over an empty box
// (a bare ESC, a TUI line-clear, a CLI consuming the line), and there was no
// second opinion anywhere in the decision. ~28 of these in one session, each
// costing an orchestrator turn plus the `get_output` probe it prompts.
//
// #535 built the missing signal — an MCP activity clock stamped at the
// `tools/call` dispatch funnel, i.e. the agent's OWN process reaching loomux —
// and named this function as its intended second consumer.

/// `UNCONFIRMED_ACK_SETTLE_MS` — the settling floor #539 measures activity
/// from, spelled as addends rather than a total so it stays a tripwire rather
/// than a copy. Measured from `submit_sent_ms`, which is stamped immediately
/// BEFORE the first Enter, so unlike `REINJECT_ACK_SETTLE_MS` above it charges
/// only for what is still ahead of that stamp:
///   4. `SUBMIT_CONFIRM_WINDOW` 600
///   5. sum of `SUBMIT_RETRY_DELAYS` 2_500 + 4_500
const UNCONFIRMED_ACK_SETTLE_MS_DERIVED: u64 = 600 + 2_500 + 4_500;

#[test]
fn a_pane_whose_agent_kept_working_is_busy_not_unconfirmed() {
    // The live shape: our paste is gone from the box (observed, not inferred),
    // `input_pending` still reads true, and the agent has called a loomux tool
    // since. Two independent readings say the delivery is done with; one
    // fallible counter says otherwise. Audit it, do not alarm.
    assert_eq!(
        unconfirmed_disposition(BoxReading::NotHolding, true, true, true, false),
        UnconfirmedDisposition::ActiveAuditOnly,
        "box observed empty of our text AND the agent acted afterwards — the pane is working, \
         and telling the orchestrator to get_output and re-send costs a turn to learn nothing"
    );
    // The guarantee the fix rests on: NO activity signal changes nothing at
    // all. Same inputs, stamp absent, pre-#539 behaviour verbatim.
    assert_eq!(
        unconfirmed_disposition(BoxReading::NotHolding, true, true, false, false),
        UnconfirmedDisposition::Notify,
        "no evidence about the agent must leave the honest alarm exactly as it was"
    );
}

#[test]
fn an_eaten_paste_is_routed_to_the_recovery_not_past_it() {
    // The regression that the whole suite missed for two releases. #585 was
    // not a wrong decision — it was a correct one that a `return` preempted,
    // and no test could see it because the precedence lived only as control
    // flow. `failed_arm_route` makes it a value, so the ordering is readable.
    //
    // The property: an eaten paste must ESCALATE (notice + badge + the
    // kickoff recovery below it), never stop quietly. Before #585 this cell
    // stopped, which is why `kickoff_recovery_action` produced zero rows —
    // fired or declined — across the entire recorded audit history.
    assert_eq!(
        failed_arm_route(unconfirmed_disposition(BoxReading::NotHolding, false, false, false, false)),
        FailedArmRoute::Escalate { eaten: true },
        "an eaten paste must reach the notice, the badge and the kickoff recovery — \
         stopping here is exactly the defect #585 fixed"
    );

    // And the other side of the same precedence: a pane that ran a turn still
    // stops, so this does not become "escalate on everything" (which would
    // re-open #522's flood rather than fix #585).
    assert_eq!(
        failed_arm_route(unconfirmed_disposition(BoxReading::NotHolding, false, true, false, false)),
        FailedArmRoute::QuietStop,
        "a pane that demonstrably ran a turn is still not a strand"
    );

    // A genuine strand (our text visibly still in the box) escalates too, but
    // as the STUCK case — the wording the orchestrator acts on differs, and
    // routing an eaten delivery through the stuck wording is what sent it
    // hunting for text that was not there.
    assert_eq!(
        failed_arm_route(unconfirmed_disposition(BoxReading::Holds, false, false, false, false)),
        FailedArmRoute::Escalate { eaten: false }
    );
    // #559's honest-uncertainty arm keeps escalating, unchanged.
    assert_eq!(
        failed_arm_route(unconfirmed_disposition(BoxReading::Unverifiable, false, false, false, false)),
        FailedArmRoute::Escalate { eaten: false }
    );

    // End-to-end, with the routing decision REAL rather than assumed: given
    // the eaten-paste pane readings, the arm escalates AND the recovery it
    // reaches re-delivers. This is the composition #517's own test asserted by
    // hand — now with its first step taken from the shipped precedence.
    let route = failed_arm_route(unconfirmed_disposition(BoxReading::NotHolding, false, false, false, false));
    assert!(matches!(route, FailedArmRoute::Escalate { .. }), "the arm must not stop");
    assert_eq!(
        kickoff_recovery_action(
            Delivery::FreshKickoff.recovers_lost_kickoff(),
            true, false, false,
            0, // the pane produced nothing since our submit — no turn started
            KICKOFF_TURN_EVIDENCE_BYTES, true, 0, KICKOFF_REDELIVERY_MAX,
        ),
        KickoffRecovery::Redeliver,
        "and the recovery the escalation reaches must actually re-deliver the lost brief"
    );
}

#[test]
fn activity_never_outranks_evidence_that_points_the_other_way() {
    // The half that must NOT be softened, one arm at a time. Liveness is
    // evidence about the AGENT; it is never evidence about the BOX, so it can
    // only ever break a tie the box did not decide.
    assert_eq!(
        unconfirmed_disposition(BoxReading::Holds, true, true, true, false),
        UnconfirmedDisposition::Notify,
        "our text is sitting there unsubmitted — an agent busy for some OTHER reason does not \
         make a visible strand go away"
    );
    assert_eq!(
        unconfirmed_disposition(BoxReading::Holds, false, true, true, false),
        UnconfirmedDisposition::Notify
    );
    // #559's honest-uncertainty arm, untouched. Suppressing here would
    // re-create the exact defect #559 fixed — silence drawn from an answer the
    // pane was never consulted about — merely sourced from a different signal.
    assert_eq!(
        unconfirmed_disposition(BoxReading::Unverifiable, false, true, true, false),
        UnconfirmedDisposition::Notify,
        "a box we could not read is not a box we watched go empty; activity is not a box reading"
    );
    assert_eq!(
        unconfirmed_disposition(BoxReading::Unverifiable, true, true, true, false),
        UnconfirmedDisposition::Notify
    );
    // And the pre-existing silent arm keeps its OWN name: an idle pane and a
    // busy one are different observations, and a shared verdict would make the
    // audit log unable to say which one suppressed the alarm.
    assert_eq!(
        unconfirmed_disposition(BoxReading::NotHolding, false, true, true, false),
        UnconfirmedDisposition::IdleAuditOnly,
        "an empty box with no human input is idle whether or not the agent is also busy"
    );
}

#[test]
fn the_eaten_notice_says_the_text_is_gone_rather_than_stuck() {
    // `.loomux/lessons.md`, "a claim is a deliverable". The pre-#585 notice
    // tells the orchestrator the prompt "may be sitting unsubmitted in its
    // pane" — for an eaten paste that is false, and acting on it sends the
    // orchestrator to `get_output`, where an idle pane reads as "nothing is
    // wrong". That misreading is exactly how #585's two live losses were
    // missed, and the speculative re-sends it invites are the #455
    // duplicate-kickoff class.
    let eaten = delivery_eaten_notice("w-14", &[77]);
    assert!(eaten.contains("w-14"), "the notice must name the agent that lost the delivery");
    assert!(
        !eaten.contains("sitting unsubmitted"),
        "the eaten notice must not repeat the stuck-in-the-box claim: we looked, and it is gone"
    );
    assert!(
        eaten.to_lowercase().contains("idle pane"),
        "it must pre-empt the idle-pane misreading, which is the symptom and not a refutation"
    );
    // And the two notices stay distinguishable — a caller that picked the
    // wrong one would send the orchestrator hunting for text that is not there.
    assert_ne!(eaten, unconfirmed_delivery_notice("w-14", &[77]));
    assert!(unconfirmed_delivery_notice("w-14", &[77]).contains("sitting unsubmitted"));
}

#[test]
fn an_eaten_delivery_notifies_with_the_eaten_wording_and_records_which_it_was() {
    // The wiring, at the seam that actually sends it. The suppression rules
    // are unchanged for both wordings (an eaten delivery is not a different
    // KIND of event to the orchestrator-loop gate), so only the text and the
    // audit's `eaten` discriminator differ.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();

    reg.notify_unconfirmed_delivery(&g.id, &w.id, false, false, true, 5501);

    let notices = reg
        .audit_log(&g.id)
        .into_iter()
        .filter(|e| e.action == "delivery-unconfirmed-notice")
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 1, "an eaten delivery notifies exactly once");
    assert_eq!(
        notices[0].detail["eaten"], true,
        "the audit must record WHICH kind this was — \"how often is a delivery actually eaten?\" \
         is the question #585 turned on and the pre-#585 log could not answer it"
    );

    // And the discriminator actually discriminates: an ordinary unconfirmed
    // delivery on the same seam must still record `eaten: false`. Without this
    // half, a hard-coded `true` would pass the assertion above.
    let w2 = reg.spawn_agent(&g.id, Role::Worker, "w2", "t", false, None).unwrap();
    reg.notify_unconfirmed_delivery(&g.id, &w2.id, false, false, false, 5502);
    let stuck = reg
        .audit_log(&g.id)
        .into_iter()
        .filter(|e| e.action == "delivery-unconfirmed-notice" && e.detail["to"] == w2.id)
        .collect::<Vec<_>>();
    assert_eq!(stuck.len(), 1);
    assert_eq!(
        stuck[0].detail["eaten"], false,
        "a merely-unconfirmed delivery must stay distinguishable from an eaten one"
    );
}

#[test]
fn activity_can_never_silence_an_eaten_delivery() {
    // The hazard of merging #585 and #539, pinned directly. Both split the
    // `NotHolding` row, and a mechanical resolution that kept #539's
    // `IdleAuditOnly` arm would have re-deaded the kickoff recovery #585 had
    // just un-deaded — from a different input, so #585's own tests would all
    // still pass.
    //
    // The reason activity must not reach these two arms is not conservatism:
    // an MCP call is a sign the AGENT is alive, never a sign it acted on OUR
    // delivery. An agent whose paste was eaten still calls loomux — every
    // role's instructions end with "if you have no task yet, report progress
    // and wait" — so under evidence-OR the report proving the brief was lost
    // would be the thing that silenced the alarm about losing it.
    for acted in [false, true] {
        for kickoff in [false, true] {
            assert_eq!(
                unconfirmed_disposition(BoxReading::NotHolding, false, false, acted, kickoff),
                UnconfirmedDisposition::EatenNotify,
                "no turn ran and the box lost our text: the delivery was EATEN, and neither an \
                 activity stamp ({acted}) nor the kickoff flag ({kickoff}) may downgrade that"
            );
            assert_eq!(
                unconfirmed_disposition(BoxReading::NotHolding, false, true, acted, kickoff),
                UnconfirmedDisposition::IdleAuditOnly,
                "and with a turn behind it, #585 owns the silence — activity changes nothing here"
            );
        }
    }
    // The eaten arm keeps escalating all the way through the route, so the
    // recovery below it stays reachable (#585's whole point).
    assert_eq!(
        failed_arm_route(unconfirmed_disposition(BoxReading::NotHolding, false, false, true, false)),
        FailedArmRoute::Escalate { eaten: true },
        "an active agent must not turn a lost delivery into a quiet stop"
    );
}

#[test]
fn activity_alone_is_not_enough_a_turn_must_also_have_run() {
    // The second restriction the merge adds: inside the one cell activity DOES
    // govern, it must be accompanied by turn evidence. Two independent
    // post-submit observations — the pane painted a turn, and the agent
    // reached loomux — neither of which the other can manufacture.
    assert_eq!(
        unconfirmed_disposition(BoxReading::NotHolding, true, false, true, false),
        UnconfirmedDisposition::Notify,
        "an agent that called a tool while the pane ran NO turn on our text is the previous \
         turn talking; silence there would be the eaten case wearing a human-input hat"
    );
    assert_eq!(
        unconfirmed_disposition(BoxReading::NotHolding, true, true, true, false),
        UnconfirmedDisposition::ActiveAuditOnly,
        "with both observations present the alarm is the false one #539 exists to remove"
    );
    // And the route agrees, so the two silent dispositions cannot diverge in
    // control flow even though they carry different audit actions.
    assert_eq!(
        failed_arm_route(UnconfirmedDisposition::ActiveAuditOnly),
        FailedArmRoute::QuietStop,
        "ActiveAuditOnly is silent for the same reason IdleAuditOnly is"
    );
}

#[test]
fn a_recoverable_kickoff_is_never_silenced_by_the_agents_own_idle_report() {
    // The one case where an activity stamp is evidence of nothing. #517's
    // lost-kickoff re-delivery is reached through the notify path, and an
    // agent whose brief never arrived is precisely the agent that calls a
    // loomux tool anyway — every role's instructions end with "if you have no
    // task yet, report progress and wait". So on a fresh spawn's kickoff, a
    // stamp is as consistent with "the brief was eaten and the agent announced
    // itself idle" as with "the brief landed".
    assert_eq!(
        unconfirmed_disposition(BoxReading::NotHolding, true, true, true, true),
        UnconfirmedDisposition::Notify,
        "a recoverable kickoff must reach #517's recovery — the agent reporting itself idle is \
         the SYMPTOM of the lost brief, and must not be read as proof it arrived"
    );
    // Without the kickoff, the identical inputs suppress — so this test is
    // pinning the veto, not a coincidence of the other three inputs.
    assert_eq!(
        unconfirmed_disposition(BoxReading::NotHolding, true, true, true, false),
        UnconfirmedDisposition::ActiveAuditOnly
    );
}

#[test]
fn activity_inside_the_settling_floor_is_the_previous_turn_talking() {
    // The floor, at the boundary and on both sides of it. `submit_sent_ms` is
    // stamped immediately before the first Enter, and our own submit machinery
    // can still be pressing Enter for `UNCONFIRMED_ACK_SETTLE_MS` after that —
    // a call arriving inside that window was decided during the agent's
    // PREVIOUS turn and says nothing about our delivery.
    // The shipped constant, checked against the derivation above rather than
    // mirrored on trust: a stage added to (or removed from) the post-submit
    // chain must move both or fail here.
    assert_eq!(
        UNCONFIRMED_ACK_SETTLE_MS, UNCONFIRMED_ACK_SETTLE_MS_DERIVED,
        "the floor must be exactly the submit-burst watch plus the blind-retry tail — no more          (stages already spent before `submit_sent_ms`) and no less (our own Enter is still landing)"
    );
    let submit = 900_000u64;
    let floor = submit + UNCONFIRMED_ACK_SETTLE_MS;
    assert!(
        !agent_acted_since(floor - 1, floor),
        "one millisecond inside the floor is still the previous turn"
    );
    assert!(agent_acted_since(floor, floor), "at the floor it counts");
    // The seed value can never satisfy a real floor, so an agent that has
    // never called a tool falls through to the unchanged path rather than
    // accidentally suppressing anything.
    assert!(!agent_acted_since(0, floor), "never-called is never an ack");
}

#[test]
fn an_unverifiable_reading_badges_its_own_blocker_and_never_presses_enter() {
    let action = stranded_selfheal_action(
        true,  // ledger: still outstanding
        false, // no human typed
        false, // no question
        BoxReading::Unverifiable,
        0,
        STRANDED_SELFHEAL_MAX_HEALS,
    );
    assert_eq!(
        action,
        StrandedAction::Attention(StrandedBlocker::Unverifiable),
        "the human is told, and told the truth about WHY loomux cannot act"
    );
    assert_ne!(
        action,
        StrandedAction::SelfHeal,
        "loomux must never press Enter into a pane whose box it could not read"
    );
    assert_ne!(
        action,
        StrandedAction::Attention(StrandedBlocker::NotHolding),
        "borrowing NotHolding would tell the human their text is gone — which is exactly \
         what this reading does not establish"
    );

    // The wording carries the same discipline: it must not resolve the
    // uncertainty it exists to report, and its action must be safe either way.
    let detail = stranded_detail("w-6", Some(StrandedBlocker::Unverifiable));
    assert!(!detail.contains("text is gone"), "never claim the text is gone: {detail}");
    assert!(detail.contains("check the pane"), "the human needs an action: {detail}");
    assert_ne!(
        detail,
        stranded_detail("w-6", Some(StrandedBlocker::NotHolding)),
        "two different states must not read identically to the human"
    );
    assert_eq!(StrandedBlocker::Unverifiable.as_str(), "box-unverifiable");
    assert_ne!(
        StrandedBlocker::Unverifiable.as_str(),
        StrandedBlocker::NotHolding.as_str(),
        "the audit token is how the two are told apart after the fact"
    );
}

#[test]
fn a_truncated_read_can_never_reach_the_box_confirm_arm() {
    // `ConfirmSource::Box` — "the CLI consumed our paste, so it landed" — is
    // set on exactly one reading, `NotHolding`. Nothing here may produce it:
    // a false confirm lets a stranded batch merge with the next delivery, the
    // one outcome strictly worse than declining to govern.
    for kib in [5usize, 8, 16, 24] {
        let pasted = flush_paste(kib * 1024);
        for window in [512usize, 4096, 12 * 1024] {
            if window >= pasted.len() {
                continue;
            }
            assert_eq!(
                box_reading(Some(&pasted[..window]), &pasted),
                BoxReading::Unverifiable,
                "a {window}-byte read of a {kib} KiB paste must decide nothing"
            );
        }
    }
}

#[test]
fn the_tier1_scan_is_derived_from_the_paste_and_bounded_by_the_flush_cap() {
    // The floor: an ordinary delivery asks for exactly the window it always
    // did, so nothing about the common path moves.
    let base = tier1_scan_bytes("");
    let ordinary = "please post the status roll-up on #559";
    assert_eq!(
        tier1_scan_bytes(ordinary),
        base + ordinary.len(),
        "the read is the paste plus the base window's worth of box chrome"
    );

    // The relation the issue asks for: whatever ONE coalesced flush may paste,
    // Tier 1 must be able to ask for a window that can contain it. This is the
    // coupling that was missing — a 4 KiB window against a 24 KiB cap.
    let at_cap = flush_paste(queue::QUEUE_FLUSH_MAX_BYTES);
    assert!(
        tier1_scan_bytes(&at_cap) >= at_cap.len(),
        "a scan window smaller than the flush cap makes box_holds_paste structurally false \
         for every large coalesced delivery"
    );

    // The ceiling, derived from the flush cap rather than picked: a single
    // constituent larger than the cap still delivers alone, and the read stops
    // growing there rather than following the paste without bound.
    let over_cap = flush_paste(queue::QUEUE_FLUSH_MAX_BYTES + 8 * 1024);
    assert_eq!(
        tier1_scan_bytes(&over_cap),
        queue::QUEUE_FLUSH_MAX_BYTES + base,
        "the ceiling is the flush cap plus the base window, not an independent number"
    );
}

#[test]
fn a_flush_sized_paste_is_verifiable_once_the_scan_is_derived_from_it() {
    // The end the fix is FOR: at the flush cap, an echoed paste is findable,
    // so Tier 1 governs the delivery two-sidedly instead of declining blind.
    let pasted = flush_paste(queue::QUEUE_FLUSH_MAX_BYTES);
    let scan = tier1_scan_bytes(&pasted);
    // What the pane returns for a read of `scan` bytes: older output, then our
    // paste echoed into the box, nothing after it.
    let echoed = format!("  ⎿  done\n\n> {pasted}\n");
    assert!(
        echoed.len() <= scan,
        "the whole pane tail must fit in the window Tier 1 asks for, or the read truncates it"
    );
    assert_eq!(
        box_reading(Some(&echoed), &pasted),
        BoxReading::Holds,
        "a paste at the flush cap must be findable in the window Tier 1 asks for"
    );
}

#[test]
fn an_ordinary_delivery_classifies_exactly_as_it_did_before() {
    // The no-regression half. Everything at or under the old fixed window
    // behaves identically — the change only moves pastes that the old window
    // could not have contained in the first place.
    let pasted = "please post the status roll-up on #559";
    let holding_tail = "  ⎿  done\n\n> please post the status roll-up on #559\n";
    let consumed_tail = "  ⎿  done\n\n".to_string() + &"the agent is answering now ".repeat(40);
    assert_eq!(box_reading(Some(holding_tail), pasted), BoxReading::Holds);
    assert_eq!(box_reading(Some(&consumed_tail), pasted), BoxReading::NotHolding);
    assert_eq!(
        unconfirmed_disposition(box_reading(Some(&consumed_tail), pasted), false, true, false, false),
        UnconfirmedDisposition::IdleAuditOnly,
        "the #522 quiet path is reached by exactly the deliveries it always was"
    );
    assert_eq!(
        unconfirmed_disposition(box_reading(Some(holding_tail), pasted), false, true, false, false),
        UnconfirmedDisposition::Notify
    );
}

#[test]
fn an_unverifiable_badge_defers_to_an_in_flight_redelivery() {
    // Same reasoning as #517's `NotHolding` arm: a badge telling the human to
    // go act on a pane loomux is actively re-delivering to is a lie about
    // whose problem it is, and the reading that produced it does not change
    // that.
    assert_eq!(stranded_reword(StrandedBlocker::Unverifiable, true), None);
    assert_eq!(
        stranded_reword(StrandedBlocker::Unverifiable, false),
        Some(StrandedBlocker::Unverifiable),
        "with nothing in flight the human must still be told"
    );
}

#[test]
fn an_oversized_stranded_batch_reaches_the_badge_and_the_audit() {
    // The wiring, not just the decision: the same `actuate_stranded` seam
    // #496/#517 use must carry this state through to a raised badge and an
    // audit record. Before #559 this delivery returned at `IdleAuditOnly` and
    // none of it happened.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5591u32;

    let action =
        stranded_selfheal_action(true, false, false, BoxReading::Unverifiable, 0, STRANDED_SELFHEAL_MAX_HEALS);
    let healed = reg.actuate_stranded(&g.id, &w.id, "orrerix", pty, action);

    assert!(!healed, "an unreadable box must never produce a blind submit");
    assert_eq!(reg.queue_depth(pty), 0, "nothing queued for a pane loomux cannot account for");
    let note = reg
        .stranded_note(&w.id)
        .expect("an oversized stranded batch must raise the badge, not go silent");
    assert_eq!(note.blocker, Some(StrandedBlocker::Unverifiable));
    assert!(
        reg.audit_log(&g.id).iter().any(|e| e.action == "stranded-attention"),
        "the badge must be audited too — the record is how a wedge is reconstructed later"
    );
}

// ───────── #583: is `Unverifiable` the near-cap NORM? ─────────
//
// #559 accepted a residual rather than tuning against a guess: the scan slack
// is a constant counted in RAW bytes, but the containment it protects runs on
// the tail AFTER `strip_ansi` + `normalize_prompt_text`. So the slack has to
// buy through a retention rate nobody has measured, and near the flush cap
// there is very little of it to spend — at a 24 KiB paste the read is 28 KiB,
// and a tail that keeps fewer than ~85 chars per 100 raw bytes is already
// short of its own needle.
//
// These tests pin the INSTRUMENTATION, not a threshold: `Tier1ScanCensus` has
// to measure the same cliff `box_reading` decides (or the audit answers a
// different question than the code asks), report a missing read as missing
// rather than as zero, and keep the field names the design note's jq recipe
// reads. The one regime that needs no live data — a paste past the scan
// CEILING, unverifiable at any density whatsoever — is pinned here too, since
// it is the half of #583 that arithmetic already closes.

/// One coalesced paste echoed the way a TUI actually repaints it — the text in
/// chunks, each led by an SGR sequence — with older output ahead of it, cut to
/// exactly the `want_bytes` the ring would have returned. Raw bytes, escapes
/// included, as `output_tail_bounded` hands them over.
fn repainted_tail(pasted: &str, want_bytes: usize, chunk: usize) -> Vec<u8> {
    let mut raw = String::new();
    // Older output ahead of the echo, so a SHORT RING is never the reason this
    // read comes up short — the fixture fills the window Tier 1 asked for, and
    // what is missing at the end is missing to stripping alone.
    while raw.len() < want_bytes {
        raw.push_str("\x1b[2m  done\x1b[0m\r\n");
    }
    for c in pasted.as_bytes().chunks(chunk) {
        raw.push_str("\x1b[0m");
        raw.push_str(std::str::from_utf8(c).expect("flush_paste is ASCII"));
    }
    let start = raw.len().saturating_sub(want_bytes);
    raw.as_bytes()[start..].to_vec()
}

#[test]
fn the_census_margin_is_the_same_cliff_the_reading_decides() {
    // The audit's number and the code's decision must be one fact. If they can
    // disagree, a live distribution of `margin_chars` is a distribution of
    // something other than "how often Tier 1 could not verify", and the
    // measurement #583 is for would answer the wrong question convincingly.
    //
    // **#821 weakened this from an equivalence to an IMPLICATION, deliberately**
    // (rev-305 B2). A negative margin still means the length arm fired; the
    // reverse no longer holds, because #821 added a second `Unverifiable` arm —
    // the partial-match probe — that fires on a tail LONGER than the paste and
    // is therefore invisible to a margin. `margin_chars`' own doc says exactly
    // that, and this assertion used to say the opposite in the same commit,
    // passing only because none of the four fixtures below reach the new arm
    // (a single-line all-ASCII paste leading with `[orrerix]`, and a `truncated`
    // case cut from the FRONT, which removes the very characters the probe
    // samples). The counterexample is pinned below rather than left as the case
    // that would break this, so the weakening is enforced instead of stated.
    let pasted = flush_paste(8 * 1024);
    let scan = tier1_scan_bytes(&pasted);
    let echoed = format!("  done\n\n> {pasted}\n");
    let cases = vec![
        ("holds", echoed.clone()),
        ("consumed", "the agent is answering now ".repeat(400)),
        // One character short of the needle: the arithmetic case, decided
        // before the pane was ever consulted.
        ("truncated", echoed[echoed.len() - (pasted.len() - 1)..].to_string()),
        ("empty ring", String::new()),
    ];
    for (name, tail) in cases {
        let census = Tier1ScanCensus::measure(scan, Some((tail.len(), &tail)), &pasted);
        let reading = box_reading(Some(&tail), &pasted);
        if census.margin_chars().is_some_and(|m| m < 0) {
            assert_eq!(
                reading,
                BoxReading::Unverifiable,
                "{name}: a negative margin IS the length arm — the number and the decision \
                 must not disagree about the cliff the number describes"
            );
        }
    }

    // The one `Unverifiable` the margin cannot speak for, stated rather than
    // left to the implication above: no read happened at all (pty gone), so
    // every measurement is null. That is the same distinction `tail_bytes`
    // carries between `None` and `Some(0)` — nothing measured is not zero.
    let blind = Tier1ScanCensus::measure(scan, None, &pasted);
    assert_eq!(box_reading(None, &pasted), BoxReading::Unverifiable);
    assert_eq!(blind.margin_chars(), None);
    assert_eq!(blind.retained_pct(), None);

    // ...and the OTHER one, which is #821's and is the reason the assertion
    // above is an implication rather than an equivalence. `h3`'s scrollbar
    // fixture: the tail is comfortably longer than the paste, so the margin is
    // positive and the length arm never fires — yet the reading is
    // `Unverifiable`, because our own line is visibly on screen behind
    // decoration `deframe` cannot reach. A margin histogram counts the length
    // arm, not "how often Tier 1 could not verify", and pinning the gap here is
    // what stops a future contributor reading this test's name and "fixing" a
    // red by making the READING agree with the census.
    let decorated = strip_ansi(FIX_BOX_GUTTER_SCROLLBAR.as_bytes());
    let probed = Tier1ScanCensus::measure(
        tier1_scan_bytes(GUTTER_PASTE_TEXT),
        Some((decorated.len(), &decorated)),
        GUTTER_PASTE_TEXT,
    );
    assert!(
        probed.margin_chars().is_some_and(|m| m >= 0),
        "precondition: this read had ample headroom — the length arm is not what fired: {:?}",
        probed.margin_chars()
    );
    assert_eq!(
        box_reading(Some(&decorated), GUTTER_PASTE_TEXT),
        BoxReading::Unverifiable,
        "the probe arm fires on a tail LONGER than the paste, so no margin can ever see it"
    );
}

#[test]
fn a_repaint_heavy_tail_is_the_residual_the_census_explains() {
    // #583's mechanism in one fixture, at the size it bites: the slack is
    // spent in RAW bytes before the comparison, which only ever sees what
    // survived stripping. Nothing here asserts that live panes look like this
    // — that is exactly what the instrumentation is for — only that when a
    // tail does, the record says WHY the read was short instead of leaving a
    // reader to infer it.
    let pasted = flush_paste(queue::QUEUE_FLUSH_MAX_BYTES);
    let scan = tier1_scan_bytes(&pasted);
    let raw = repainted_tail(&pasted, scan, 16);
    let stripped = strip_ansi(&raw);
    let census = Tier1ScanCensus::measure(scan, Some((raw.len(), &stripped)), &pasted);

    assert_eq!(
        census.tail_bytes,
        Some(scan),
        "the fixture must FILL the window, or a short ring explains the shortfall instead"
    );
    assert_eq!(
        box_reading(Some(&stripped), &pasted),
        BoxReading::Unverifiable,
        "a full window of repaint-heavy tail still strips below the paste it should contain"
    );

    // The bound the live numbers get read against: at the flush cap the read
    // is the paste plus a fixed slack, so it survives only down to
    // `paste_chars / requested_bytes` retention — ~85%, which is not a lot of
    // ANSI to be wrong about.
    let breakeven = (census.paste_chars as u64 * 100 / census.requested_bytes as u64) as u32;
    assert!(
        census.retained_pct().is_some_and(|pct| pct < breakeven),
        "the census must report the density that explains the reading: {:?} vs breakeven {breakeven}%",
        census.retained_pct()
    );
    assert!(
        census.margin_chars().is_some_and(|m| m < 0),
        "and how far short it fell, which is what would size a wider slack"
    );
}

#[test]
fn a_paste_past_the_scan_ceiling_is_unverifiable_at_any_density() {
    // The half of #583 that needs no live data. `tier1_scan_bytes` is
    // ceilinged at the flush cap plus the base window, and a single queue
    // entry larger than the cap still delivers alone (`plan_flush`'s "the cap
    // is a CEILING, never a floor"). Above that ceiling the window is smaller
    // than the needle before any tail is read, so 100% retention — a pane
    // emitting pure text, not one escape — is still short. No slack tuning
    // reaches this regime; only a different ceiling would, which is a
    // different decision from the one the measurement informs.
    let ceiling = queue::QUEUE_FLUSH_MAX_BYTES + tier1_scan_bytes("");
    let pasted = flush_paste(ceiling + 1024);
    let scan = tier1_scan_bytes(&pasted);
    assert_eq!(scan, ceiling, "the read stops growing at the ceiling, the paste does not");

    let tail = pasted[pasted.len() - scan..].to_string();
    let census = Tier1ScanCensus::measure(scan, Some((tail.len(), &tail)), &pasted);
    assert!(
        census.retained_pct().is_some_and(|pct| pct >= 99),
        "the fixture's tail is pure text — stripping costs it nothing, so the ceiling is the \
         only thing left to blame"
    );
    assert_eq!(box_reading(Some(&tail), &pasted), BoxReading::Unverifiable);
    assert!(census.margin_chars().is_some_and(|m| m < 0));
}

#[test]
fn the_census_records_an_absent_read_as_absent_rather_than_zero() {
    // The audit shape itself — these key names are the design note's jq
    // recipe, and a rename that keeps the code compiling silently breaks every
    // aggregation written against a live group's log.
    let pasted = "please post the status roll-up on #583";
    let scan = tier1_scan_bytes(pasted);

    let blind = Tier1ScanCensus::measure(scan, None, pasted).to_json();
    assert_eq!(blind["requested_bytes"], json!(scan));
    assert_eq!(blind["paste_chars"], json!(pasted.len()));
    assert_eq!(blind["tail_bytes"], Value::Null);
    assert_eq!(blind["tail_chars"], Value::Null);
    assert_eq!(blind["margin_chars"], Value::Null);
    assert_eq!(blind["retained_pct"], Value::Null);

    // A read that HAPPENED and came back empty is a different fact: a pane
    // that has not spoken yet, not a pane loomux could not read. An aggregate
    // that cannot tell those apart would read every dead pty as 0% retention.
    let empty = Tier1ScanCensus::measure(scan, Some((0, "")), pasted).to_json();
    assert_eq!(empty["tail_bytes"], json!(0));
    assert_eq!(empty["tail_chars"], json!(0));
    assert_eq!(empty["margin_chars"], json!(-(pasted.len() as i64)));
    assert_eq!(
        empty["retained_pct"],
        Value::Null,
        "a ratio over zero bytes read is undefined, and 0% would read as a total-loss tail"
    );
}

// ---------- #685: the scan window, sized in POST-STRIP characters ----------
//
// #583's measurement came back: retention is ~50%, so the raw-byte slack was
// under-covering from ~1.5 KiB of paste up, and `Unverifiable` was being
// decided by arithmetic before the pane was consulted. `Tier1Scan` re-reads
// scaled by the retention the tail itself demonstrates.
//
// What these pin, in the order they matter: a paste in the formerly-collapsing
// range now VERIFIES; the widening never manufactures a verification it cannot
// take (a ring with nothing more in it, a paste past the ceiling); and no read
// is ever narrower than the one #559 took, which is the property the whole
// same-size invariant rested on.

/// A pane ring the way a repaint-heavy TUI leaves one: `lead_bytes` of older
/// output, then our paste echoed in `chunk`-sized runs each led by an SGR
/// sequence, then the box's own trailing frame. Retention is `chunk / (chunk +
/// 4)` — at `chunk = 4`, the ~50% #583 measured live.
///
/// Unlike `repainted_tail` above (which cuts to exactly the window under test,
/// so a read can never come up short for want of ring), this builds the WHOLE
/// ring: a read has more to find whenever it asks for more, which is the
/// difference between "the slack was too small" and "the pane had not spoken
/// yet" — the two causes #685 has to keep apart.
fn repaint_ring(pasted: &str, chunk: usize, lead_bytes: usize) -> Vec<u8> {
    let mut raw = String::new();
    let filler = "waiting on the previous turn, still streaming output here ";
    while raw.len() < lead_bytes {
        for c in filler.as_bytes().chunks(chunk) {
            raw.push_str("\x1b[0m");
            raw.push_str(std::str::from_utf8(c).expect("filler is ASCII"));
        }
    }
    for c in pasted.as_bytes().chunks(chunk) {
        raw.push_str("\x1b[0m");
        raw.push_str(std::str::from_utf8(c).expect("flush_paste is ASCII"));
    }
    // Box chrome after our text, well inside `BOX_TAIL_WINDOW_SLACK` so the
    // containment window still reaches back over the whole paste.
    raw.push_str("\x1b[2m\r\n> \x1b[0m");
    raw.into_bytes()
}

/// The reading the code took BEFORE #685, spelled out rather than described:
/// one request of `tier1_scan_bytes` raw bytes, stripped, classified. Every
/// fixture below asserts this, so a test can never quietly stop being about a
/// paste that used to collapse.
fn pre_685_reading(pm: &PtyManager, pty: u32, pasted: &str) -> BoxReading {
    box_reading(
        pm.output_tail_bounded(pty, tier1_scan_bytes(pasted))
            .map(|b| strip_ansi(&b))
            .as_deref(),
        pasted,
    )
}

#[test]
fn a_repaint_heavy_pane_verifies_once_the_window_is_sized_in_post_strip_chars() {
    // The headline: an 8 KiB paste sitting IN THE BOX, on a pane whose tail
    // retains half its bytes through stripping. The old read asked for
    // paste + 4 KiB raw, got half of it in characters, and declined to govern
    // a delivery whose text was right there.
    let pasted = flush_paste(8 * 1024);
    let pty = 6851;
    let pm = PtyManager::default();
    pm.register_fake_for_test(pty, &repaint_ring(&pasted, 4, 32 * 1024));

    assert_eq!(
        pre_685_reading(&pm, pty, &pasted),
        BoxReading::Unverifiable,
        "the fixture must still be a member of the class it witnesses: one fixed-size read of \
         this pane could not verify a paste that is plainly in its box"
    );

    let mut scan = Tier1Scan::for_paste(&pasted);
    let read = scan.read(|n| pm.output_tail_bounded(pty, n)).expect("the fake pty is alive");
    assert_eq!(
        box_reading(Some(&read.stripped), &pasted),
        BoxReading::Holds,
        "sized in post-strip chars, the same pane and the same paste now VERIFY"
    );
    assert!(
        read.requested_bytes > tier1_scan_bytes(&pasted),
        "and it got there by widening, not by the fixture being generous: {} vs the {} floor",
        read.requested_bytes,
        tier1_scan_bytes(&pasted)
    );
    assert_eq!(
        read.tail_bytes, read.requested_bytes,
        "the ring FILLED the widened request — a short pane is a different cause (#583's \
         confounder) and is not what this test is about"
    );
}

#[test]
fn a_paste_in_the_measured_collapsing_band_verifies() {
    // The band #583 actually measured, given its own witness: 1.5-2 KiB, where
    // 58% of live deliveries came back `Unverifiable`, on the way to 100% from
    // 2.5 KiB up. The headline test above is an 8 KiB paste because that is the
    // smallest that collapses at ~50% retention — but the issue is written
    // about THIS band, and a suite that only brackets it would not notice a
    // future `BOX_TAIL_SCAN_BYTES` or slack edit moving the boundary back into
    // it. The band is reachable at a denser pane instead of a bigger paste:
    // `chunk = 1` is one escape per text byte, ~20% retention, which is what a
    // repaint-heavy TUI looks like when it is painting per character.
    let pasted = flush_paste(1800);
    assert!(
        (1536..=2560).contains(&pasted.len()),
        "this fixture's whole point is WHICH bucket it lands in — {} chars is outside the \
         1.5-2.5 KiB band #583 measured",
        pasted.len()
    );
    let pty = 6857;
    let pm = PtyManager::default();
    pm.register_fake_for_test(pty, &repaint_ring(&pasted, 1, 32 * 1024));

    assert_eq!(
        pre_685_reading(&pm, pty, &pasted),
        BoxReading::Unverifiable,
        "the fixture must be a member of the class it witnesses: a paste in the measured band, \
         unverifiable under the old fixed-size read"
    );

    let mut scan = Tier1Scan::for_paste(&pasted);
    let read = scan.read(|n| pm.output_tail_bounded(pty, n)).expect("the fake pty is alive");
    assert_eq!(
        box_reading(Some(&read.stripped), &pasted),
        BoxReading::Holds,
        "the band the issue is written about is the band that now verifies"
    );
    assert_eq!(
        read.tail_bytes, read.requested_bytes,
        "the ring FILLED the widened request — a short pane is the other cause, and not this one"
    );
}

#[test]
fn an_ordinary_delivery_still_takes_exactly_one_read() {
    // The cost argument, pinned. The target is the containment window, not the
    // whole 4 KiB budget, so an ordinary prompt clears it on the first read and
    // widens not at all — only a read that was truncating the comparison pays
    // for anything. If this ever fails, every delivery just started re-reading
    // the ring several times per poll.
    let pasted = "please post the status roll-up on #685";
    let pty = 6852;
    let pm = PtyManager::default();
    pm.register_fake_for_test(pty, &repaint_ring(pasted, 4, 32 * 1024));

    let mut requests = Vec::new();
    let mut scan = Tier1Scan::for_paste(pasted);
    let read = scan
        .read(|n| {
            requests.push(n);
            pm.output_tail_bounded(pty, n)
        })
        .expect("the fake pty is alive");

    assert_eq!(requests, vec![tier1_scan_bytes(pasted)], "one read, at the unchanged floor");
    assert_eq!(box_reading(Some(&read.stripped), pasted), BoxReading::Holds);
}

#[test]
fn a_ring_with_nothing_more_in_it_ends_the_widening_rather_than_re_asking() {
    // #685's OTHER cause, which this change deliberately does not paper over:
    // the tail under-delivers against the request because the pane has not
    // produced that much output yet. There is no retention to scale by and no
    // wider request that could return a byte the ring does not hold, so the
    // reader stops at one read and the delivery stays `Unverifiable` — stated,
    // notified, never a confirm.
    let pasted = flush_paste(4 * 1024);
    let pty = 6853;
    let pm = PtyManager::default();
    // A ring holding a fraction of the paste: the live shape #583 logged as
    // 6153 requested, 1408 returned.
    pm.register_fake_for_test(pty, &repaint_ring(&pasted, 4, 0)[..1408].to_vec());

    let mut requests = Vec::new();
    let mut scan = Tier1Scan::for_paste(&pasted);
    let read = scan
        .read(|n| {
            requests.push(n);
            pm.output_tail_bounded(pty, n)
        })
        .expect("the fake pty is alive");

    assert_eq!(requests.len(), 1, "an under-filled read is the end of the widening, not the start");
    assert!(read.tail_bytes < read.requested_bytes, "the fixture must under-fill, or it proves nothing");
    assert_eq!(
        box_reading(Some(&read.stripped), &pasted),
        BoxReading::Unverifiable,
        "fail-safe is unchanged: a delivery loomux could not read is never reported verified"
    );
}

#[test]
fn a_paste_past_the_ceiling_is_still_unverifiable_however_wide_the_ring_is() {
    // The ceiling regime `a_paste_past_the_scan_ceiling_is_unverifiable_at_any_
    // density` proves for the old sizing, re-asked of the new one — because a
    // re-reading window is exactly the mechanism that could have quietly
    // raised a ceiling nobody decided to raise. `target_chars` is capped at
    // `TIER1_SCAN_MAX_BYTES` too, so the widening never fires and the answer is
    // the same one arithmetic gave before: what verification should CLAIM up
    // here is #685's posture half, and it is not decided by this change.
    let ceiling = queue::QUEUE_FLUSH_MAX_BYTES + tier1_scan_bytes("");
    let pasted = flush_paste(ceiling + 1024);
    let pty = 6854;
    let pm = PtyManager::default();
    // Pure text, no escapes at all, and a ring far larger than the paste: 100%
    // retention with everything to read. Nothing but the ceiling is left to
    // blame.
    let mut ring = "older output\r\n".repeat(2048).into_bytes();
    ring.extend_from_slice(pasted.as_bytes());
    pm.register_fake_for_test(pty, &ring);

    let mut scan = Tier1Scan::for_paste(&pasted);
    assert!(
        scan.target_chars() < pasted.len(),
        "the ceiling caps the TARGET as well as the floor, so no read this scan can take — \
         however wide, however clean — covers the needle: {} chars aimed for, {} needed",
        scan.target_chars(),
        pasted.len()
    );
    let read = scan.read(|n| pm.output_tail_bounded(pty, n)).expect("the fake pty is alive");
    assert!(
        ring.len() > read.requested_bytes,
        "the ring had more to give, so a short pane is not what makes this unverifiable"
    );
    assert_eq!(box_reading(Some(&read.stripped), &pasted), BoxReading::Unverifiable);
}

#[test]
fn no_widened_read_is_ever_narrower_than_the_one_it_replaced() {
    // The invariant #559 stated as "every box read for a delivery uses the same
    // size", in the stronger form #685 leaves it: reads are MONOTONE. The
    // hazard it guards is a precondition verified against a wide tail and then
    // polled against a narrow one — the poll sees the box cut mid-paste, reads
    // `NotHolding`, and confirms a delivery nothing observed. It needs a
    // narrower read, and this asserts there is no way to take one: not across
    // the widening rounds of a single read, and not across the successive reads
    // one delivery's confirm loop takes.
    let pasted = flush_paste(8 * 1024);
    let pty = 6855;
    let pm = PtyManager::default();
    // Dense enough that one widening is not enough: ~20% retention, so the
    // scaled re-request has to converge over more than one round.
    pm.register_fake_for_test(pty, &repaint_ring(&pasted, 1, 64 * 1024));

    let floor = tier1_scan_bytes(&pasted);
    let mut requests = Vec::new();
    let mut scan = Tier1Scan::for_paste(&pasted);
    for _ in 0..3 {
        let _ = scan.read(|n| {
            requests.push(n);
            pm.output_tail_bounded(pty, n)
        });
    }

    assert!(requests.len() > 3, "the fixture must actually widen, or this asserts nothing");
    assert!(
        requests.windows(2).all(|w| w[1] >= w[0]),
        "requests must never shrink, within a read or between reads: {requests:?}"
    );
    assert!(
        requests.iter().all(|n| *n >= floor),
        "and never below the size #559's own read used: {requests:?} vs floor {floor}"
    );
}

#[test]
fn the_widening_is_bounded_and_fails_safe_on_a_tail_it_cannot_cover() {
    // Termination, on the worst tail this fixture set can build: a coalesced
    // paste at the flush cap against an almost entirely-escape ring, where even
    // the widest request the ring can serve strips to less than the needle. The
    // reader must widen (it is short), must stop (bounded), and must leave the
    // reading `Unverifiable` — a wider window buying nothing is exactly the
    // residual `Unverifiable` exists to say out loud.
    let pasted = flush_paste(queue::QUEUE_FLUSH_MAX_BYTES);
    let pty = 6856;
    let pm = PtyManager::default();
    let mut ring = Vec::new();
    while ring.len() < 250 * 1024 {
        ring.extend_from_slice(b"\x1b[2m\x1b[0m\x1b[Kx");
    }
    pm.register_fake_for_test(pty, &ring);

    let mut requests = Vec::new();
    let mut scan = Tier1Scan::for_paste(&pasted);
    let read = scan
        .read(|n| {
            requests.push(n);
            pm.output_tail_bounded(pty, n)
        })
        .expect("the fake pty is alive");

    assert!(requests.len() >= 2, "a read this short must have tried to widen: {requests:?}");
    assert!(
        requests.len() as u32 <= 1 + TIER1_SCAN_WIDEN_ROUNDS,
        "one read plus the bounded re-reads, and not one more: {requests:?}"
    );
    assert_eq!(
        box_reading(Some(&read.stripped), &pasted),
        BoxReading::Unverifiable,
        "a window that cannot be covered classifies the widest read taken — fail-safe, as before"
    );
}

#[test]
fn the_sizing_stops_for_a_reason_it_can_state() {
    // `widen` is the whole decision; the reader is the loop around it. Each
    // `None` is a distinct "a wider request cannot change the answer", and
    // pinning them separately is what keeps the reader's stopping conditions
    // from collapsing into one another under a later edit.
    // Sized by hand rather than from `flush_paste`, so the arithmetic below is
    // exact and readable: 1,000 chars of needle, a 1,200-char window, and a
    // first request of 1,000 + `BOX_TAIL_SCAN_BYTES`.
    let pasted = "x".repeat(1000);
    let scan = Tier1Scan::for_paste(&pasted);
    let target = scan.target_chars();
    let floor = scan.request_floor();
    assert_eq!(target, 1200, "the window is the needle plus the containment slack");
    assert_eq!(floor, tier1_scan_bytes(&pasted), "the first request is exactly #559's");

    // Covered: the read delivered the whole containment window.
    assert_eq!(scan.widen(floor, floor, target), None);
    // Under-filled: the ring holds no more, so no wider request can add a byte.
    assert_eq!(scan.widen(floor, floor - 1, target / 4), None);
    // Nothing survived stripping: the ratio is undefined, and a guess dressed
    // as a measurement is worse than the honest `Unverifiable` that follows.
    assert_eq!(scan.widen(floor, floor, 0), None);

    // And when it does widen, it widens by what the shortfall measured: half
    // the characters back means twice the bytes asked for — the measured
    // retention, not a doubling constant (a quarter back asks for four times).
    assert_eq!(scan.widen(floor, floor, target / 2), Some(floor * 2));
    assert_eq!(scan.widen(floor, floor, target / 4), Some(floor * 4));
}
