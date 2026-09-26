//! The stuck-prompt chip: dismiss, the badge janitor, QueueFull retries, the polled usage view and per-block CLIs.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ---- #825 M1: the explicit human dismiss ----

#[test]
fn an_explicit_dismiss_releases_every_blocker_class_and_names_it_in_the_audit() {
    // The gesture is a first-class release for EVERY class, and the two ends of
    // that set are why: `PauseSuppressed` describes a loss that already
    // happened, so no reading of the pane could ever release it, while
    // `Unverifiable` describes a box loomux could not read, so no reading of the
    // pane is trusted TO. Between them they rule out every automatic release,
    // and the human saying "seen" is the one piece of evidence that is neither
    // an inference nor a timer.
    //
    // The audit half is not decoration. For the classes that claim the box may
    // still hold an unsubmitted prompt, the chip can be the only trace of it —
    // so what the human took down has to survive the taking-down, or a
    // dismissed badge becomes exactly the silent loss the badge exists to
    // prevent. The class token is what makes it diagnosable afterwards.
    for (blocker, token) in [
        (Some(StrandedBlocker::NotHolding), "not-holding"),
        (Some(StrandedBlocker::Unverifiable), "box-unverifiable"),
        (Some(StrandedBlocker::Exhausted), "heal-budget-spent"),
        (Some(StrandedBlocker::QueueFull), "queue-full"),
        (Some(StrandedBlocker::PauseSuppressed), "pause-suppressed"),
        (Some(StrandedBlocker::HumanInput), "human-input"),
        (Some(StrandedBlocker::Question), "question"),
        (Some(StrandedBlocker::QuestionStale), "question-hold-stale"),
        (Some(StrandedBlocker::QueueNearFull), "queue-near-full"),
        (Some(StrandedBlocker::QueueAtCapacity), "queue-at-capacity"),
        // The in-flight heal badge (`blocker: None`) is a chip like any other
        // and a human looking at a stale one has the same complaint.
        (None, "self-healing"),
    ] {
        let (reg, _d, g, wid) = attention_setup();
        reg.mark_stranded(&g, &wid, blocker);
        assert!(reg.stranded_note(&wid).is_some(), "{token}: precondition — the chip is up");

        assert!(
            reg.dismiss_stranded(&wid),
            "{token}: the dismiss must report that it took a badge down"
        );
        assert!(
            reg.stranded_note(&wid).is_none(),
            "{token}: an explicit dismiss releases every class — a class the gesture cannot \
             clear is a chip the human has no way to get rid of, which is the live complaint"
        );

        let log = reg.audit_log(&g);
        let dismissed = log
            .iter()
            .find(|e| e.action == "stranded-dismissed")
            .unwrap_or_else(|| panic!("{token}: the dismissal must be in the audit, not silent"));
        assert_eq!(
            dismissed.detail["blocker"],
            json!(token),
            "{token}: the class is the diagnosis a dismissed badge has to leave behind"
        );
        assert_eq!(
            dismissed.actor, "human",
            "{token}: a dismissal is a human act — attributing it to loomux would make the log \
             say loomux decided something it deliberately refuses to decide"
        );
        assert_eq!(dismissed.detail["to"], json!(wid), "{token}: and it names the pane");
    }
}

#[test]
fn a_dismissed_badges_clear_names_the_gesture_as_its_reason() {
    // Deliberately its own test rather than a tail assertion on the per-class
    // one above, so that a red here says exactly which half moved: not "the
    // dismissal happened" but "the CLEAR is attributed to the gesture". The two
    // records are separate on purpose — `stranded-dismissed` reports a human
    // act, `stranded-cleared` a state change — and a grep starting from either
    // has to land on the same story.
    let (reg, _d, g, wid) = attention_setup();
    reg.mark_stranded(&g, &wid, Some(StrandedBlocker::PauseSuppressed));
    assert!(reg.dismiss_stranded(&wid), "precondition: the gesture released the badge");

    let cleared = reg
        .audit_log(&g)
        .into_iter()
        .find(|e| e.action == "stranded-cleared")
        .expect("the clear is audited, as every stranded clear is");
    assert_eq!(
        cleared.detail["why"],
        json!("human-dismissed"),
        "a clear naming some other mechanism would put loomux's name on a decision a human made"
    );
}

#[test]
fn focusing_a_pane_is_not_evidence_that_releases_its_stuck_prompt_chip() {
    // The "cheapest partial improvement" #825 floats and plan-312 rejects.
    // `ack_attention` fires on pane FOCUS — the gesture the human performs all
    // day merely to type — so wiring the chip to it would take down badges
    // nobody read. On `Unverifiable` that is the worst case in the issue made
    // real: loomux could not read the box, so the chip may be the only sign
    // that a prompt is still sitting in it unsubmitted.
    let (reg, _d, g, wid) = attention_setup();
    reg.mark_stranded(&g, &wid, Some(StrandedBlocker::Unverifiable));

    reg.ack_attention(&wid);
    assert!(
        reg.stranded_note(&wid).is_some(),
        "focus is not evidence: only a gesture aimed at the chip itself releases it"
    );
    assert_eq!(
        hold_audit_count(&reg, &g, "stranded-cleared"),
        0,
        "and nothing was audited as cleared, because nothing was"
    );

    assert!(reg.dismiss_stranded(&wid), "the deliberate gesture is the one that releases it");
    assert!(reg.stranded_note(&wid).is_none());
}

#[test]
fn dismissing_a_chip_that_is_not_up_changes_nothing_and_audits_nothing() {
    // A dismissal line is a record of a human releasing a warning. Writing one
    // for a click that released nothing would put gestures in the log that
    // never took anything down, which is what makes the log worth reading.
    let (reg, _d, g, wid) = attention_setup();

    assert!(!reg.dismiss_stranded(&wid), "no chip is up — there is nothing to release");
    assert!(
        !reg.dismiss_stranded("w-not-an-agent"),
        "and an id with no agent record cannot have had a chip on screen at all"
    );
    assert_eq!(hold_audit_count(&reg, &g, "stranded-dismissed"), 0);
    assert_eq!(hold_audit_count(&reg, &g, "stranded-cleared"), 0);

    // The second half of an impatient double-click.
    reg.mark_stranded(&g, &wid, Some(StrandedBlocker::Unverifiable));
    assert!(reg.dismiss_stranded(&wid));
    assert!(!reg.dismiss_stranded(&wid), "the second click has nothing left to take down");
    assert_eq!(
        hold_audit_count(&reg, &g, "stranded-dismissed"),
        1,
        "one gesture that released something, one line"
    );
    assert_eq!(hold_audit_count(&reg, &g, "stranded-cleared"), 1);
}

#[test]
fn a_dismiss_takes_down_one_panes_chip_and_leaves_the_rest_of_the_group_alone() {
    // The chip names a pane, and the dismiss is scoped to the pane the human
    // clicked. A dismiss that swept the group would silently retire warnings
    // about panes the human never looked at — the same false-release harm as
    // clearing on focus, one blast radius up.
    let (reg, _d, g, wid) = attention_setup();
    let other = reg.spawn_agent(&g, Role::Worker, "w2", "more work", false, None).unwrap();
    reg.mark_stranded(&g, &wid, Some(StrandedBlocker::NotHolding));
    reg.mark_stranded(&g, &other.id, Some(StrandedBlocker::Unverifiable));

    assert!(reg.dismiss_stranded(&wid));

    assert!(reg.stranded_note(&wid).is_none(), "the clicked pane's chip is down");
    assert_eq!(
        reg.stranded_note(&other.id).and_then(|n| n.blocker),
        Some(StrandedBlocker::Unverifiable),
        "and the other pane's is untouched — nobody has said they saw THAT one"
    );
}

// ---- #825 M2: the badge janitor — the honesty check, hoisted ----
//
// The gap this closes, precisely. `run_late_confirmation_monitor`'s
// `KeepWaiting` arm has kept a raised chip honest every 5s since #496 PR-C, so
// the latch was never really "no release logic exists" — it was that the
// release logic DIES WITH THE MONITOR at `LATE_MONITOR_MAX_LIFETIME` (4h), and
// on an idle pane nothing ever looks again. So the fix is a hoist: one matrix
// (`stranded_badge_release`), one writer (`apply_stranded_verdict`), and two
// observers that ask it the same question — the monitor while it lives, and
// `stranded_janitor_pass` for as long as the chip is up.
//
// WHAT A CLEAR IS ALLOWED TO REST ON is the whole of this section, and it is
// #819's bar unchanged: a positive `BoxReading::NotHolding` on the text the
// ledger still records, AND a human keystroke since our submit. Neither half
// alone:
//
//   - `NotHolding` alone is `StrandedRetireReason::TextGone`, which #819
//     deliberately declined to clear on — our text left the box with nobody at
//     the keyboard is exactly `StrandedBlocker::NotHolding`'s own sentence.
//   - The keystroke alone is #518's phantom, which is why the stamp only ever
//     NAMES a release a box reading has already licensed.
//
// The asymmetry between the two observers is deliberate and load-bearing.
// `saw_text_in_box` is a fact about the OBSERVER: the monitor watches one
// delivery continuously and can witness a present→absent transition; the
// janitor starts on panes whose monitor is already gone and never can, so it
// clears on the stricter bar instead.

/// The three classes #825 leaves with no release, and their audit tokens.
const JANITOR_CLASSES: [(StrandedBlocker, &str); 3] = [
    (StrandedBlocker::NotHolding, "not-holding"),
    (StrandedBlocker::Unverifiable, "box-unverifiable"),
    (StrandedBlocker::Exhausted, "heal-budget-spent"),
];

/// Every class a pane reading must NOT release: the queue and hold classes,
/// `PauseSuppressed` (a loss that already happened, so no reading of the pane
/// could ever make it untrue), and the in-flight-heal wording.
const NOT_THE_JANITORS_BUSINESS: [Option<StrandedBlocker>; 8] = [
    Some(StrandedBlocker::QueueFull),
    Some(StrandedBlocker::PauseSuppressed),
    Some(StrandedBlocker::HumanInput),
    Some(StrandedBlocker::Question),
    Some(StrandedBlocker::QuestionStale),
    Some(StrandedBlocker::QueueNearFull),
    Some(StrandedBlocker::QueueAtCapacity),
    None,
];

#[test]
fn the_janitor_clears_a_stuck_chip_only_on_819s_human_resolved_bar() {
    // The cell the whole slice exists for, and the one that has to survive the
    // monitor: text verifiably gone, a person on record at the keyboard since
    // our submit. Nothing about this depends on a monitor being alive — that
    // is the point.
    for (blocker, token) in JANITOR_CLASSES {
        assert_eq!(
            stranded_badge_release(Some(blocker), Some(BoxReading::NotHolding), true, false),
            BadgeRelease::Clear(StrandedRetireReason::HumanResolved.as_str()),
            "{token}: a gone text plus a human keystroke is the release #819 already licenses \
             on the marker path — the badge-only pane is the same pane"
        );
    }
    // The reason string is TAKEN from #819's enum, never spelled again. Two
    // vocabularies for one fact is how the drainer's retirement and this clear
    // would come to disagree in a log a human is reading to find out what
    // happened to their prompt.
    assert_eq!(StrandedRetireReason::HumanResolved.as_str(), "human-resolved");
}

#[test]
fn a_gone_text_with_nobody_at_the_keyboard_re_words_and_never_clears() {
    // The must-not-clear edge, and #819's declined cell restated where it now
    // has a second caller. `NotHolding` alone is `TextGone`: our text left the
    // box and no person is on record, which is precisely what
    // `StrandedBlocker::NotHolding` already says out loud. Clearing on it would
    // answer a question loomux cannot answer — and it would turn #828's
    // remaining false `NotHolding` (a hard mid-word wrap) into a SILENT clear
    // instead of a wording change.
    assert_eq!(
        stranded_badge_release(
            Some(StrandedBlocker::Unverifiable),
            Some(BoxReading::NotHolding),
            false,
            false,
        ),
        BadgeRelease::Reword(StrandedBlocker::NotHolding),
        "\"we could not read the box\" is a stale sentence about a box we have now read"
    );
    assert_eq!(
        stranded_badge_release(
            Some(StrandedBlocker::Exhausted),
            Some(BoxReading::NotHolding),
            false,
            false,
        ),
        BadgeRelease::Reword(StrandedBlocker::NotHolding),
        "\"a heal fired and the text still read Holds\" is likewise no longer what we know"
    );
    assert_eq!(
        stranded_badge_release(
            Some(StrandedBlocker::NotHolding),
            Some(BoxReading::NotHolding),
            false,
            false,
        ),
        BadgeRelease::Keep,
        "already the truest thing available — a re-word to itself would be audit noise"
    );
    // Said once more as the property, so a future edit that "simplifies" the
    // no-keystroke arm into a clear cannot pass by fixing three assertions.
    for (blocker, token) in JANITOR_CLASSES {
        assert!(
            !matches!(
                stranded_badge_release(Some(blocker), Some(BoxReading::NotHolding), false, false),
                BadgeRelease::Clear(_)
            ),
            "{token}: no chip comes down on a text-gone reading with nobody at the keyboard"
        );
    }
}

#[test]
fn a_reading_the_janitor_could_not_take_changes_nothing_at_all() {
    // The second must-not-clear edge, and the one where being wrong is worst.
    // `Holds` says our prompt is demonstrably still sitting in that box;
    // `Unverifiable` says we looked and could not tell; `None` says the ledger
    // has no text to look for. Three different facts, one conservative branch —
    // nothing is ever released on an absence of evidence, which is the single
    // direction that could hide an unsubmitted prompt behind a chip that went
    // away.
    for (blocker, token) in JANITOR_CLASSES {
        for reading in [Some(BoxReading::Holds), Some(BoxReading::Unverifiable), None] {
            for stamped in [true, false] {
                assert_eq!(
                    stranded_badge_release(Some(blocker), reading, stamped, false),
                    BadgeRelease::Keep,
                    "{token}: reading {reading:?} (stamped={stamped}) is not evidence of anything"
                );
            }
        }
    }
}

#[test]
fn the_classes_no_pane_reading_can_answer_are_left_alone_by_the_janitor() {
    // Each of these is somebody else's release, and saying so is the reason
    // this is a per-class matrix rather than one rule for every chip:
    // `PauseSuppressed` describes a loss that has already happened and cannot
    // become untrue, so no reading of the pane may release it (M1's explicit
    // dismiss is its release); `QueueFull` wants a RETRY when room opens, not a
    // clear (M3); the hold and depth classes already have releases keyed to the
    // condition they name.
    for blocker in NOT_THE_JANITORS_BUSINESS {
        for stamped in [true, false] {
            assert_eq!(
                stranded_badge_release(blocker, Some(BoxReading::NotHolding), stamped, false),
                BadgeRelease::Keep,
                "{blocker:?} (stamped={stamped}) is not released by a box reading"
            );
        }
    }
}

#[test]
fn only_the_monitor_can_clear_on_a_transition_and_it_outranks_the_keystroke() {
    // `saw_text_in_box` is a fact about the OBSERVER, not the pane, and this
    // pins both halves of that. The monitor watched our text sit in the box and
    // now sees it gone: a genuine present→absent transition, the strongest
    // reading available, and #496 PR-C's own clear — it holds for EVERY class
    // because a transition un-wedges the pane whatever the chip happened to say.
    for blocker in JANITOR_CLASSES
        .iter()
        .map(|(b, _)| Some(*b))
        .chain(NOT_THE_JANITORS_BUSINESS)
    {
        for stamped in [true, false] {
            assert_eq!(
                stranded_badge_release(blocker, Some(BoxReading::NotHolding), stamped, true),
                BadgeRelease::Clear("text-left-the-box"),
                "{blocker:?} (stamped={stamped}): the transition clear is unchanged by the hoist \
                 and is not narrowed to the three classes"
            );
        }
    }
    // And it OUTRANKS the keystroke-named clear: the transition is about where
    // the text went, which is a stronger claim than who was at the keyboard.
    assert_eq!(
        stranded_badge_release(
            Some(StrandedBlocker::NotHolding),
            Some(BoxReading::NotHolding),
            true,
            true,
        ),
        BadgeRelease::Clear("text-left-the-box"),
    );
    // Even a transition needs the reading: nothing clears on `Holds`.
    assert_eq!(
        stranded_badge_release(Some(StrandedBlocker::NotHolding), Some(BoxReading::Holds), true, true),
        BadgeRelease::Keep,
    );
}

#[test]
fn the_janitor_clears_a_stuck_chip_long_after_the_monitor_that_raised_it_is_gone() {
    // THE headline cell of #825, wired: an idle pane whose delivery stranded,
    // whose monitor exited hours ago, and whose human quietly rescued it by
    // pressing Enter themselves. Before this the chip stayed up until a
    // restart — the live complaint, and the thing that trains a human to stop
    // reading chips at all.
    let (reg, _d, g, wid, pm, pty, captured, ledger) =
        janitor_pane!(StrandedBlocker::NotHolding, CLEARED_TAIL);
    // The human's own recovery: click in, press Enter. `\r` with nothing after
    // it classifies `Submit`, which stamps the keystroke clock.
    pm.note_user_input(pty, "\r", true);

    reg.stranded_janitor_pass(&pm, &ledger);

    assert!(
        reg.stranded_note(&wid).is_none(),
        "text verifiably gone plus a human keystroke since our submit is #819's own release bar \
         — a chip that survives it is claiming a pane is wedged that demonstrably is not"
    );
    let cleared = reg
        .audit_log(&g)
        .into_iter()
        .find(|e| e.action == "stranded-cleared")
        .expect("every stranded clear is audited");
    assert_eq!(
        cleared.detail["why"],
        json!("human-resolved"),
        "and it is NOT `human-dismissed` — that reason belongs to a gesture a human made on the \
         chip (M1). This one is loomux's own inference from the pane, and a log that spelled the \
         two the same way could not tell them apart"
    );
    // A clear by inference owes the evidence it inferred from.
    let judged = reg
        .audit_log(&g)
        .into_iter()
        .find(|e| e.action == "stranded-janitor")
        .expect("the janitor states what it read, or a vanished chip is unexplainable");
    assert_eq!(judged.detail["reading"], json!("not-holding"));
    assert_eq!(judged.detail["human_since_submit"], json!(true));
    assert_eq!(judged.detail["blocker"], json!("not-holding"));
    // The safety line, asserted: nothing in this mechanism writes to a pane.
    assert!(
        captured.lock().unwrap().is_empty(),
        "the janitor takes chips down; it must never press Enter, got {:?}",
        captured.lock().unwrap()
    );
}

#[test]
fn the_janitor_never_takes_a_chip_down_while_our_text_is_still_in_the_box() {
    // The direction where being wrong is unrecoverable. Our prompt is visibly
    // sitting in that box unsubmitted, and the chip is the only trace of it —
    // clear here and the prompt is lost with nothing left to say so. The human
    // keystroke is present precisely so that this test fails if the keystroke
    // is ever allowed to license a release on its own (#518's phantom).
    let (reg, _d, g, wid, pm, pty, captured, ledger) =
        janitor_pane!(StrandedBlocker::NotHolding, STRANDED_TAIL);
    pm.note_user_input(pty, "\r", true);

    reg.stranded_janitor_pass(&pm, &ledger);

    assert_eq!(
        reg.stranded_note(&wid).and_then(|n| n.blocker),
        Some(StrandedBlocker::NotHolding),
        "our text is still there — the chip stays up and unchanged"
    );
    assert_eq!(hold_audit_count(&reg, &g, "stranded-cleared"), 0);
    assert_eq!(hold_audit_count(&reg, &g, "stranded-janitor"), 0);
    assert!(captured.lock().unwrap().is_empty());
}

#[test]
fn an_unreadable_box_leaves_the_stuck_prompt_chip_exactly_as_it_found_it() {
    // #559's third state, arriving at a new consumer. A tail shorter than our
    // own paste makes containment false by arithmetic — the answer was fixed
    // before the pane was consulted — so it is neither "holds" nor "gone", and
    // the one thing it must never become is a release. This is the class the
    // issue's own warning is literal about: loomux could not read the box, so
    // the chip may be the only sign a prompt is still sitting in it.
    let (reg, _d, g, wid, pm, pty, _cap, ledger) =
        janitor_pane!(StrandedBlocker::Unverifiable, "> \n");
    pm.note_user_input(pty, "\r", true);

    reg.stranded_janitor_pass(&pm, &ledger);

    assert_eq!(
        reg.stranded_note(&wid).and_then(|n| n.blocker),
        Some(StrandedBlocker::Unverifiable),
        "not cleared, and not re-worded either: a reading that never happened may not upgrade a \
         chip's wording any more than it may take it down"
    );
    assert_eq!(hold_audit_count(&reg, &g, "stranded-cleared"), 0);
    assert_eq!(
        hold_audit_count(&reg, &g, "stranded-attention"),
        1,
        "one line, from the raise in the fixture — nothing was written this pass"
    );
}

#[test]
fn a_gone_text_with_nobody_at_the_keyboard_upgrades_the_wording_and_keeps_the_chip() {
    // #819's declined cell (`TextGone`), wired. The box no longer holds our
    // text and no human keystroke is on record: not a release — but
    // "loomux could not read the box" has become a false sentence about a box
    // we have now read. The chip stays and says the truer thing, which is also
    // what contains #828's residual false `NotHolding`: a lone bad reading can
    // only ever change wording here, never take a warning away.
    let (reg, _d, g, wid, pm, _pty, _cap, ledger) =
        janitor_pane!(StrandedBlocker::Unverifiable, CLEARED_TAIL);

    reg.stranded_janitor_pass(&pm, &ledger);

    assert_eq!(
        reg.stranded_note(&wid).and_then(|n| n.blocker),
        Some(StrandedBlocker::NotHolding),
        "re-worded to what we now know, and still up — nobody has shown a human dealt with it"
    );
    assert_eq!(
        hold_audit_count(&reg, &g, "stranded-cleared"),
        0,
        "a keystroke is the other half of the bar and there was none"
    );
    assert_eq!(
        hold_audit_count(&reg, &g, "stranded-attention"),
        2,
        "the raise, then the re-word — the badge's history stays one grep"
    );
}

#[test]
fn the_janitor_never_hands_back_a_chip_a_human_has_dismissed() {
    // The hazard M1 created and this slice must not walk into: after an
    // explicit dismiss, an absent `attn_stranded` entry can mean "a human said
    // they had seen it". A janitor that re-raised on a fresh reading would hand
    // the human back the chip they just took down — the live complaint,
    // reopened by the mechanism meant to close it.
    //
    // Set up the ONE verdict that writes to the map at all (the re-word), then
    // dismiss before the pass runs.
    let (reg, _d, g, wid, pm, _pty, _cap, ledger) =
        janitor_pane!(StrandedBlocker::Unverifiable, CLEARED_TAIL);
    assert!(reg.dismiss_stranded(&wid), "precondition: the human took the chip down");

    reg.stranded_janitor_pass(&pm, &ledger);

    assert!(
        reg.stranded_note(&wid).is_none(),
        "an absent entry means nothing to do — never something to raise"
    );
    assert_eq!(
        hold_audit_count(&reg, &g, "stranded-attention"),
        1,
        "only the fixture's original raise: a re-raise would be a second line here"
    );
}

#[test]
fn a_re_word_is_never_a_raise_and_never_overwrites_a_fresher_diagnosis() {
    // The primitive both observers now write through, pinned directly — because
    // the same gap exists on the late monitor's live re-check, which reads the
    // note at the top of its arm and writes it back at the bottom, every 5s.
    // Through `mark_stranded` that write is an INSERT, so a chip dismissed in
    // between comes straight back; through `reword_stranded` an absent entry
    // stays absent.
    let (reg, _d, g, wid) = attention_setup();

    assert!(
        !reg.reword_stranded(&g, &wid, None, Some(StrandedBlocker::HumanInput)),
        "no chip is up — a re-word has nothing to re-word and must not create one"
    );
    assert!(reg.stranded_note(&wid).is_none());
    assert_eq!(hold_audit_count(&reg, &g, "stranded-attention"), 0, "and it audits nothing");

    // The other half of the same gap: the chip is up, but another mechanism
    // re-raised it with a different diagnosis since the caller last looked.
    reg.mark_stranded(&g, &wid, Some(StrandedBlocker::PauseSuppressed));
    assert!(
        !reg.reword_stranded(
            &g,
            &wid,
            Some(StrandedBlocker::Unverifiable),
            Some(StrandedBlocker::NotHolding),
        ),
        "the verdict was about a chip that is no longer there"
    );
    assert_eq!(
        reg.stranded_note(&wid).and_then(|n| n.blocker),
        Some(StrandedBlocker::PauseSuppressed),
        "the fresher diagnosis survives — a stale verdict must not overwrite it"
    );

    // And it does update in place when it is still the chip that was judged.
    assert!(reg.reword_stranded(
        &g,
        &wid,
        Some(StrandedBlocker::PauseSuppressed),
        Some(StrandedBlocker::NotHolding),
    ));
    assert_eq!(
        reg.stranded_note(&wid).and_then(|n| n.blocker),
        Some(StrandedBlocker::NotHolding)
    );
}

#[test]
fn the_janitor_leaves_the_classes_no_pane_reading_can_answer_alone() {
    // `PauseSuppressed` is the sharp end: it reports deliveries that were
    // DISCARDED while the group was paused. That loss is historical — it cannot
    // become untrue — and it never put text in anyone's box, so a box reading
    // says precisely nothing about it. Releasing it on the strongest pane
    // evidence available would still be releasing it on evidence about
    // something else. `QueueFull` wants a retry when room opens (M3), not a
    // clear.
    for blocker in [StrandedBlocker::PauseSuppressed, StrandedBlocker::QueueFull] {
        let (reg, _d, g, wid, pm, pty, _cap, ledger) = janitor_pane!(blocker, CLEARED_TAIL);
        pm.note_user_input(pty, "\r", true);

        reg.stranded_janitor_pass(&pm, &ledger);

        assert_eq!(
            reg.stranded_note(&wid).and_then(|n| n.blocker),
            Some(blocker),
            "{blocker:?} must survive a reading that has nothing to do with it"
        );
        assert_eq!(hold_audit_count(&reg, &g, "stranded-cleared"), 0);
    }
}

#[test]
fn a_pane_with_no_recorded_stranded_text_is_never_judged() {
    // "We have nothing to look for" is not "the box is clear" — the distinction
    // `DeliveryOutcome::stranded_text`'s own doc insists on. A pane whose ledger
    // carries no text (a path that pasted nothing, a record from before the
    // field existed, or a restart that dropped the in-memory ledger) yields no
    // reading, so there is nothing to decide from and no pane read to pay for.
    let (reg, _d, g, wid) = attention_setup();
    let pty = 8252u32;
    let pm = PtyManager::default();
    pm.register_fake_for_test(pty, CLEARED_TAIL.as_bytes());
    reg.set_pty_for_test(&wid, pty);
    reg.mark_stranded(&g, &wid, Some(StrandedBlocker::NotHolding));
    pm.note_user_input(pty, "\r", true);

    // An empty ledger first: the pane has no delivery record at all.
    let empty: TrackedMutex<HashMap<u32, _>> = TrackedMutex::new("test_last_delivery", HashMap::new());
    record_stranded_outcome_at_for_test(&empty, 9_999, "orrerix".to_string(), now_ms() - 60_000, None);
    reg.stranded_janitor_pass(&pm, &empty);
    assert!(reg.stranded_note(&wid).is_some(), "no record for this pane — nothing to judge");

    // Then a record for THIS pane that carries no text.
    let textless: TrackedMutex<HashMap<u32, _>> = TrackedMutex::new("test_last_delivery", HashMap::new());
    record_stranded_outcome_at_for_test(&textless, pty, "orrerix".to_string(), now_ms() - 60_000, None);
    reg.stranded_janitor_pass(&pm, &textless);
    assert!(
        reg.stranded_note(&wid).is_some(),
        "a record with no stranded text establishes nothing about the box either"
    );
    assert_eq!(hold_audit_count(&reg, &g, "stranded-cleared"), 0);
}

#[test]
fn a_dead_agents_chip_is_not_the_janitors_to_clear() {
    // A pane with no running agent cannot be un-wedged by anyone, and its note
    // is `attention_tick`'s to prune (that is the one thing which prunes the
    // latched map). The janitor reasoning about a dead pane would be reading a
    // ring nobody is writing to and attributing the result to a person.
    let (reg, _d, g, wid, pm, pty, _cap, ledger) =
        janitor_pane!(StrandedBlocker::NotHolding, CLEARED_TAIL);
    pm.note_user_input(pty, "\r", true);
    reg.mark_dead(&wid, Some(1));

    reg.stranded_janitor_pass(&pm, &ledger);

    assert_eq!(
        hold_audit_count(&reg, &g, "stranded-cleared"),
        0,
        "the janitor does not clear a dead pane's chip — the attention scan prunes it"
    );
}

// ---------- #825 M3: `QueueFull`'s release is a RETRY, not a reading ----------
//
// The last of #825's five unreleased classes, and the only one whose badge is
// not a claim about where our text is. `QueueFull` says loomux could not even
// QUEUE the re-send it had already decided to make — so the honest answer is to
// queue it once there is room, not to infer anything from the pane. Nothing here
// clears a chip: a successful re-admission RE-WORDS it to the in-flight heal
// wording and hands the pane back to the confirm/retire machinery that owns
// every other marker.
//
// It also closes a REPAIR gap, not only a badge gap. Before this, the Enter
// `actuate_stranded` decided to press and could not queue was never pressed at
// all, however much room opened afterwards, on any pane whose late monitor had
// exited.
//
// WHY THIS IS NOT A HOOK ON THE #658 DRAIN EDGE is pinned below rather than
// argued in prose, because it is the deviation from plan-312's letter: every
// production caller that can produce the `Full` → not-`Full` transition runs on
// the drainer thread, which holds its `queue_draining` registration until
// `commit_exit` — so an admission there is refused by construction, every time.

/// Dequeue the way the drainer does — the ONLY thing that produces the `Full` →
/// not-`Full` transition on a live pane — until the depth is at most `target`.
///
/// More than one pop can be needed to come off the cap, and the reason is
/// itself #658: the first drain edge is where the refusal roster is announced,
/// and on a headless registry that announcement is admitted to this very queue
/// and never drained (`deliver_prompt_as` only withdraws an entry that landed
/// FIRST). So the pane bounces back to its cap once, and the second pop is the
/// one that sticks. Bounded, so a queue that never comes down fails the test
/// rather than hanging it.
fn drain_to_depth(reg: &OrchRegistry, group: &GroupId, pty: u32, target: usize) -> usize {
    for _ in 0..(queue::QUEUE_MAX_PER_PANE * 4) {
        let depth = reg.queue_depth(pty);
        if depth <= target {
            return depth;
        }
        let front = reg.queue_snapshot(pty).into_iter().next().expect("a non-empty queue has a front");
        reg.pop_front_dequeued(group, pty, front.id, front.enqueued_ms);
    }
    panic!("the pane's queue never came down to {target}");
}

fn drain_below_cap(reg: &OrchRegistry, group: &GroupId, pty: u32) -> usize {
    drain_to_depth(reg, group, pty, queue::QUEUE_MAX_PER_PANE - 1)
}

fn marker_count(reg: &OrchRegistry, pty: u32) -> usize {
    reg.queue_snapshot(pty)
        .iter()
        .filter(|e| matches!(e.payload, queue::QueuedPayload::StrandedSubmit))
        .count()
}

#[test]
fn the_drain_edge_itself_could_never_have_admitted_the_refused_re_send() {
    // THE PIN for this slice's deviation from plan-312, and the reason it is a
    // test rather than a paragraph: the plan put M3 at `note_queue_capacity`'s
    // `Full` → not-`Full` edge, beside `announce_refusal_roster` (#658). A
    // read-only plan cannot see runtime registrations, and this is one: every
    // production caller able to produce that edge — `pop_front_dequeued`,
    // `pop_batch_dequeued`, `drop_superseded` — runs on the drainer thread,
    // which holds `queue_draining` from `ensure_drainer` through to
    // `commit_exit`. A hook there would decline `drainer-active` on every real
    // drain, forever, and repair nothing.
    //
    // So this drives the real edge with the registration a real drainer holds,
    // and then releases it — same pane, same queue, same chip; only the
    // drainer's lifetime differs — which is exactly the difference between the
    // plan's placement and this one.
    let (reg, _d, g, wid, pty, ledger) = queuefull_pane!();
    let generation = reg.register_drainer_for_test(pty).expect("no drainer holds this pty yet");

    let depth = drain_below_cap(&reg, &g, pty);
    assert!(depth < queue::QUEUE_MAX_PER_PANE, "the pane's depth really did come down from its cap");

    // The #658 edge fired: this IS the moment plan-312 aimed at.
    let pressure = reg
        .audit_log(&g)
        .into_iter()
        .find(|e| e.action == "delivery-queue-pressure" && e.detail["was"] == json!("full"))
        .expect("a pane leaving its cap audits the transition — the edge M3 was planned onto");
    assert_ne!(pressure.detail["state"], json!("full"), "and it is a transition OUT of full");

    // And at that exact instant the admission is refused, whoever asks.
    assert!(reg.drainer_active(pty), "the thread that produced the edge is still registered");
    assert_eq!(
        queuefull_readmit_gate(Some(StrandedBlocker::QueueFull), depth, reg.drainer_active(pty)),
        Some("drainer-active"),
        "a drain-edge hook is a no-op by construction, not merely usually"
    );

    // Wired: a pass running here changes nothing at all.
    reg.stranded_queuefull_pass(&ledger);
    assert_eq!(marker_count(&reg, pty), 0, "nothing may be pushed in front of a live drainer");
    assert_eq!(
        reg.stranded_note(&wid).and_then(|n| n.blocker),
        Some(StrandedBlocker::QueueFull),
        "and the chip is still exactly true — the re-send is still not queued"
    );
    assert_eq!(hold_audit_count(&reg, &g, "stranded-readmit"), 0);

    // The drainer exits. THIS is the moment that can admit, and the moment no
    // hook on the transition above would ever have seen.
    assert!(reg.release_drainer_for_test(pty, generation), "the drainer deregisters on exit");
    reg.stranded_queuefull_pass(&ledger);
    assert_eq!(
        marker_count(&reg, pty),
        1,
        "with room in the queue and nobody draining it, the refused re-send is finally queued"
    );
}

#[test]
fn queuefull_readmit_gate_admits_one_class_and_only_with_room_and_no_drainer() {
    // The matrix, directly. Three conditions, and each one is a different kind
    // of "not yet": the wrong chip is somebody else's release entirely, a full
    // queue means the chip is still telling the truth, and a live drainer is
    // #496 PR-C rev-47 B1's fused-admission rule, which this pass obeys rather
    // than re-litigates.
    let cap = queue::QUEUE_MAX_PER_PANE;
    assert_eq!(
        queuefull_readmit_gate(Some(StrandedBlocker::QueueFull), cap - 1, false),
        None,
        "the one cell that admits: the chip says the re-send was refused, and there is now room"
    );

    // Every other chip, including the in-flight wording, is left alone. A pass
    // that widened to `NotHolding` / `Unverifiable` / `Exhausted` would be
    // pressing Enter on M2's evidence, which M2 deliberately never does.
    for blocker in [
        Some(StrandedBlocker::NotHolding),
        Some(StrandedBlocker::Unverifiable),
        Some(StrandedBlocker::Exhausted),
        Some(StrandedBlocker::PauseSuppressed),
        Some(StrandedBlocker::HumanInput),
        Some(StrandedBlocker::Question),
        Some(StrandedBlocker::QuestionStale),
        Some(StrandedBlocker::QueueNearFull),
        Some(StrandedBlocker::QueueAtCapacity),
        None,
    ] {
        assert_eq!(
            queuefull_readmit_gate(blocker, 0, false),
            Some("not-queue-full"),
            "{blocker:?} names no refused re-send, so there is nothing here to re-admit"
        );
    }

    // Still at cap: the condition that raised the chip has not changed, so the
    // chip is right and an attempt would only write a `delivery-dropped` line
    // every pass.
    assert_eq!(
        queuefull_readmit_gate(Some(StrandedBlocker::QueueFull), cap, false),
        Some("still-full"),
    );
    assert_eq!(
        queuefull_readmit_gate(Some(StrandedBlocker::QueueFull), cap + 1, false),
        Some("still-full"),
        "over cap counts as full too — the push asks `>=`, and so does this"
    );
    // A live drainer, named with `stranded_admission_gate`'s own word so the
    // pre-check and the check under the lock cannot drift into two vocabularies.
    assert_eq!(
        queuefull_readmit_gate(Some(StrandedBlocker::QueueFull), 0, true),
        Some("drainer-active"),
    );
    assert_eq!(stranded_admission_gate(true, false), Some("drainer-active"));
}

#[test]
fn a_drained_queue_finally_gets_the_refused_re_send_queued_and_the_chip_says_so() {
    // The headline cell, wired: the pane whose re-send was refused at cap, whose
    // queue has since drained, and whose late monitor is long gone. Before this
    // the marker was never pushed and the chip sat there claiming a refusal that
    // had stopped being true — the exact "loomux gave up and nothing re-attempts"
    // #825 names for this class.
    let (reg, _d, g, wid, pty, ledger) = queuefull_pane!();
    drain_below_cap(&reg, &g, pty);

    reg.stranded_queuefull_pass(&ledger);

    let snap = reg.queue_snapshot(pty);
    let marker = snap.first().expect("the queue is not empty");
    assert!(
        matches!(marker.payload, queue::QueuedPayload::StrandedSubmit),
        "the re-send goes to the FRONT: it presses Enter on text already in the box, so anything \
         pasted ahead of it would land on top of that text"
    );
    assert_eq!(
        marker.reason.as_str(),
        "stranded-self-heal",
        "and it carries the trigger that admitted it (#560), not the shared helper's old guess"
    );
    assert_eq!(
        marker.from, "orch",
        "the sender comes from the ledger record, so the queue line and the `DeliveryOutcome` a \
         press writes name the same person"
    );

    assert_eq!(
        reg.stranded_note(&wid).map(|n| n.blocker),
        Some(None),
        "the chip is RE-WORDED to the in-flight heal, never taken down: a submit really is \
         pending again, and the confirm/retire machinery owns it from here"
    );
    let readmit = reg
        .audit_log(&g)
        .into_iter()
        .find(|e| e.action == "stranded-readmit")
        .expect("the wording changed, so the log has to say what changed it");
    assert_eq!(readmit.detail["admitted"], json!(true));
    assert_eq!(readmit.detail["cap"], json!(queue::QUEUE_MAX_PER_PANE));
    // M3 takes no chip down, so it mints no `stranded-cleared` reason of its own
    // — the distinct-token-per-mechanism rule, satisfied by not needing one.
    assert_eq!(
        hold_audit_count(&reg, &g, "stranded-cleared"),
        0,
        "a retry is not a release: nothing here clears on inference"
    );
}

#[test]
fn a_queue_still_at_its_cap_keeps_the_chip_and_costs_nothing() {
    // The chip is still exactly true — there is still no room — so the pass must
    // do nothing AND write nothing. Attempting-and-failing every 30s would fill
    // `audit.jsonl` with `delivery-dropped` lines about a refusal already
    // recorded once, which is why the depth pre-check exists at all.
    let (reg, _d, g, wid, pty, ledger) = queuefull_pane!();
    let before = reg.audit_log(&g).len();

    reg.stranded_queuefull_pass(&ledger);

    assert_eq!(reg.queue_depth(pty), queue::QUEUE_MAX_PER_PANE, "nothing was pushed");
    assert_eq!(marker_count(&reg, pty), 0);
    assert_eq!(
        reg.stranded_note(&wid).and_then(|n| n.blocker),
        Some(StrandedBlocker::QueueFull),
        "the chip stays, because what it says is still what is happening"
    );
    assert_eq!(reg.audit_log(&g).len(), before, "a declined pass is silent, not merely harmless");
}

#[test]
fn a_marker_already_queued_re_words_the_chip_without_pushing_a_second_one() {
    // The `submit-already-queued` decline, which is a decline about the WORLD,
    // not about this pass: the submit the chip says was refused is in fact
    // pending. `actuate_stranded` calls that an in-flight heal (`None`) and does
    // not burn its budget on it; this must say the same thing, or the same state
    // would read two ways depending on which mechanism looked.
    let (reg, _d, g, wid, pty, ledger) = queuefull_pane!();
    // Two slots, not one: the marker below takes one, and the pass must still
    // find room when it looks — otherwise this would decline `still-full` and
    // test nothing it claims to.
    drain_to_depth(&reg, &g, pty, queue::QUEUE_MAX_PER_PANE - 2);
    assert!(
        reg.admit_stranded_selfheal(&g, &wid, "orch", pty).expect("there is room now"),
        "precondition: a marker is queued for this pane"
    );
    assert!(reg.queue_depth(pty) < queue::QUEUE_MAX_PER_PANE, "and the pane is not back at its cap");

    reg.stranded_queuefull_pass(&ledger);

    assert_eq!(marker_count(&reg, pty), 1, "one pending submit, not two");
    assert_eq!(
        reg.stranded_note(&wid).map(|n| n.blocker),
        Some(None),
        "a submit IS pending, so the chip says so rather than going on claiming a refusal"
    );
    let readmit = reg
        .audit_log(&g)
        .into_iter()
        .find(|e| e.action == "stranded-readmit")
        .expect("the wording still changed, so it is still explained");
    assert_eq!(
        readmit.detail["admitted"],
        json!(false),
        "and the line says this pass pushed nothing — the marker was already there"
    );
}

#[test]
fn the_re_admission_never_hands_back_a_chip_a_human_has_dismissed() {
    // The hazard M1 created, which every #825 mechanism has to walk past: an
    // absent `attn_stranded` entry can mean "a human said they had seen it", so
    // a write that INSERTED would hand back the chip they just took down. This
    // pass writes through `reword_stranded` for that reason.
    //
    // A second pane keeps a chip up so the pass runs its whole loop instead of
    // returning on the empty-map fast path — otherwise this would pass for a
    // reason that has nothing to do with the guarantee.
    let (reg, _d, g, wid, pty, ledger) = queuefull_pane!();
    let other = reg.spawn_agent(&g, Role::Worker, "w2", "t", false, None).unwrap();
    reg.set_pty_for_test(&other.id, 8262);
    reg.mark_stranded(&g, &other.id, Some(StrandedBlocker::PauseSuppressed));
    drain_below_cap(&reg, &g, pty);
    assert!(reg.dismiss_stranded(&wid), "precondition: the human took the chip down");

    reg.stranded_queuefull_pass(&ledger);

    assert!(
        reg.stranded_note(&wid).is_none(),
        "an absent entry means nothing to do — never something to raise"
    );
    // And the dismissal is a fact about a WARNING, not about the pane: a
    // dismissed chip does not un-strand the prompt, so nothing was queued either.
    assert_eq!(marker_count(&reg, pty), 0);
}

#[test]
fn a_pane_with_no_delivery_record_has_nothing_to_re_submit() {
    // "We have no record" is not "there is text to re-submit". Without the
    // ledger there is no sender to name on the queue entry, and a marker that
    // guessed one would put a lie in the line a human greps for who sent the
    // wedged prompt (#560) — so the pass declines instead.
    let (reg, _d, g, wid, pty, _ledger) = queuefull_pane!();
    drain_below_cap(&reg, &g, pty);
    let empty: TrackedMutex<HashMap<u32, _>> = TrackedMutex::new("test_last_delivery", HashMap::new());
    record_stranded_outcome_at_for_test(&empty, 9_999, "orch".to_string(), now_ms() - 60_000, None);

    reg.stranded_queuefull_pass(&empty);

    assert_eq!(marker_count(&reg, pty), 0, "no record for this pane — nothing to re-submit");
    assert_eq!(
        reg.stranded_note(&wid).and_then(|n| n.blocker),
        Some(StrandedBlocker::QueueFull),
        "and the chip stays: declining to act is not evidence that the refusal is over"
    );
}

#[test]
fn a_dead_agents_chip_is_not_re_admitted() {
    // A pane with no running agent has nothing to submit INTO, and its note is
    // `attention_tick`'s to prune. Queueing a marker for it would be queueing
    // an Enter press for a pane that is gone.
    let (reg, _d, g, wid, pty, ledger) = queuefull_pane!();
    drain_below_cap(&reg, &g, pty);
    reg.mark_dead(&wid, Some(1));

    reg.stranded_queuefull_pass(&ledger);

    assert_eq!(marker_count(&reg, pty), 0);
    assert_eq!(hold_audit_count(&reg, &g, "stranded-readmit"), 0);
}

#[test]
fn prompt_wait_detected_ignores_quiet_non_prompts() {
    // Ordinary streaming output — even when it ends on a numbered summary list —
    // is not a selection menu, or we'd get a false-positive storm.
    assert!(
        !prompt_wait_detected(&strip_ansi(FIX_STREAMING.as_bytes())),
        "a quiet numbered summary must not be mistaken for a selection menu"
    );
    // A CLI idling at its empty input box (turn finished, no question asked) must
    // not be flagged — otherwise every parked pane lights up.
    assert!(
        !prompt_wait_detected(&strip_ansi(FIX_IDLE_BOX.as_bytes())),
        "an idle input box is not an interactive question"
    );
}

#[test]
fn prompt_wait_detected_ignores_finished_turn_prose_about_ui() {
    // #40 review: finished-turn agent output that *describes* keyboard UIs, pastes
    // a `❯` shell prompt, or echoes a `›` breadcrumb must NOT flag once the CLI's
    // idle input box has redrawn below it. These flagged before the fix — the new
    // signals are anchored (pointer must lead a line; footer read from the last
    // few lines only), so the phrases/glyphs now fall out of range.
    for (name, fixture) in [
        ("keyboard-nav prose", FIX_FP_PROSE),
        ("pasted ❯ shell prompt", FIX_FP_SHELL),
        ("› UI breadcrumb", FIX_FP_BREADCRUMB),
        ("leading ❯ repro steps", FIX_FP_LEADING_PTR),
        ("fenced ❯ command block", FIX_FP_FENCED_PTR),
        // #727: not prose about a UI — the CLI's OWN empty input box, whose
        // prompt glyph is a bare `❯` in the last painted lines. Unlike the four
        // above it cannot be pushed out of range by anything, because it IS the
        // thing the CLI redraws underneath.
        ("empty ❯ prompt box", FIX_FP_RESUMED_IDLE),
    ] {
        assert!(
            !prompt_wait_detected(&strip_ansi(fixture.as_bytes())),
            "{name}: finished-turn prose must not be mistaken for a live prompt"
        );
    }
}

#[test]
fn attention_flags_a_pane_parked_on_a_question_fixture() {
    // End-to-end through attention_tick with a real captured Copilot question:
    // once the pane's output is quiet past the window and unattended, it must
    // surface as `waiting` — and carry the pty_id the pane header indicator and
    // the #26/#31 dock-tab dot both key off.
    let (reg, _d, _g, wid) = attention_setup();
    let now = 1_000_000_000_000u64;
    let out: HashMap<String, u64> = [(wid.clone(), 512u64)].into_iter().collect();
    let tail: HashMap<String, String> =
        [(wid.clone(), strip_ansi(FIX_COPILOT_ASK.as_bytes()))].into_iter().collect();
    let no_input = HashMap::new();

    // Debounced on first sighting (the menu may still be painting).
    let first = reg.attention_tick(now, &out, &tail, &no_input);
    assert!(first.iter().all(|i| i.reason != "waiting"), "first quiet tick is debounced");

    // Stable past the quiet window → the pane needs the human.
    let waited = reg.attention_tick(now + 5000, &out, &tail, &no_input);
    let item = waited
        .iter()
        .find(|i| i.agent_id == wid && i.reason == "waiting")
        .expect("a quiet pane parked on a question must be flagged");
    assert_eq!(
        item.pty_id,
        reg.agent(&wid).unwrap().pty_id,
        "the attention item must carry the pane's pty_id for the header + dock-tab dot"
    );
}

#[test]
fn attention_does_not_flag_streaming_or_idle_fixtures() {
    // The two negatives, driven all the way through attention_tick: neither a
    // quiet stream of ordinary output nor an idle input box may raise `waiting`,
    // even long after they've gone quiet.
    let (reg, _d, _g, wid) = attention_setup();
    let now = 1_000_000_000_000u64;
    let out: HashMap<String, u64> = [(wid.clone(), 256u64)].into_iter().collect();
    let no_input = HashMap::new();
    for fixture in [
        FIX_STREAMING,
        FIX_IDLE_BOX,
        FIX_FP_PROSE,
        FIX_FP_SHELL,
        FIX_FP_BREADCRUMB,
        FIX_FP_LEADING_PTR,
        FIX_FP_FENCED_PTR,
        FIX_FP_RESUMED_IDLE,
    ] {
        let tail: HashMap<String, String> =
            [(wid.clone(), strip_ansi(fixture.as_bytes()))].into_iter().collect();
        reg.attention_tick(now, &out, &tail, &no_input);
        let later = reg.attention_tick(now + 60_000, &out, &tail, &no_input);
        assert!(
            later.iter().all(|i| i.reason != "waiting"),
            "ordinary/idle/prose output must not raise attention, got: {later:?}"
        );
    }
}

#[test]
fn plain_pane_parked_on_a_question_flags_waiting_by_pty() {
    // #40 (human repro): a plain pane the human opened by hand — no orchestration
    // agent/group — running a CLI that's now parked on a question must flag
    // `waiting`, keyed only by its pty id (empty agent_id, no role).
    let (reg, _d, _g, _w) = attention_setup();
    let now = 1_000_000_000_000u64;
    let pty = 77u32;
    let out: HashMap<u32, u64> = [(pty, 512u64)].into_iter().collect();
    let tail: HashMap<u32, String> =
        [(pty, strip_ansi(FIX_CLAUDE_ASK.as_bytes()))].into_iter().collect();
    let no_input = HashMap::new();
    let no_agents = HashSet::new();

    // Debounced on first sighting, then flags once quiet past the window.
    let first = reg.plain_pane_attention(now, &out, &tail, &no_input, &no_agents);
    assert!(first.iter().all(|i| i.reason != "waiting"), "first quiet tick is debounced");
    let waited = reg.plain_pane_attention(now + 5000, &out, &tail, &no_input, &no_agents);
    let item = waited
        .iter()
        .find(|i| i.pty_id == Some(pty) && i.reason == "waiting")
        .expect("a plain pane parked on a question must be flagged");
    assert!(item.agent_id.is_empty(), "a plain pane has no agent identity");
    assert!(item.role.is_none(), "a plain pane has no orchestration role");
}

#[test]
fn plain_pane_scan_skips_agent_ptys_and_quiet_non_prompts() {
    // A pty that belongs to a registered agent is handled by attention_tick, so
    // the plain scan must skip it (no double-flag). And ordinary/idle/prose
    // output on a plain pane must not flag.
    let (reg, _d, _g, _w) = attention_setup();
    let now = 1_000_000_000_000u64;
    let agent_pty = 5u32;
    let out: HashMap<u32, u64> = [(agent_pty, 100u64)].into_iter().collect();
    let tail: HashMap<u32, String> =
        [(agent_pty, strip_ansi(FIX_CLAUDE_ASK.as_bytes()))].into_iter().collect();
    let agents: HashSet<u32> = [agent_pty].into_iter().collect();
    reg.plain_pane_attention(now, &out, &tail, &HashMap::new(), &agents);
    assert!(
        reg.plain_pane_attention(now + 60_000, &out, &tail, &HashMap::new(), &agents)
            .is_empty(),
        "an agent's pty must not be double-flagged by the plain scan"
    );

    let no_agents = HashSet::new();
    for fixture in [FIX_STREAMING, FIX_IDLE_BOX, FIX_FP_PROSE] {
        let pty = 9u32;
        let out: HashMap<u32, u64> = [(pty, 100u64)].into_iter().collect();
        let tail: HashMap<u32, String> =
            [(pty, strip_ansi(fixture.as_bytes()))].into_iter().collect();
        reg.plain_pane_attention(now, &out, &tail, &HashMap::new(), &no_agents);
        assert!(
            reg.plain_pane_attention(now + 60_000, &out, &tail, &HashMap::new(), &no_agents)
                .iter()
                .all(|i| i.reason != "waiting"),
            "ordinary/idle/prose on a plain pane must not flag"
        );
    }
}

#[test]
fn plain_pane_waiting_ack_by_pty_sticks_until_output_changes() {
    // Turning to a plain pane (ack by pty) must make the ack stick until the pane
    // repaints, mirroring the agent path.
    let (reg, _d, _g, _w) = attention_setup();
    let now = 1_000_000_000_000u64;
    let pty = 12u32;
    let out: HashMap<u32, u64> = [(pty, 100u64)].into_iter().collect();
    let tail: HashMap<u32, String> =
        [(pty, strip_ansi(FIX_CLAUDE_ASK.as_bytes()))].into_iter().collect();
    let no_input = HashMap::new();
    let no_agents = HashSet::new();

    reg.plain_pane_attention(now, &out, &tail, &no_input, &no_agents);
    assert_eq!(
        reg.plain_pane_attention(now + 5000, &out, &tail, &no_input, &no_agents)
            .iter()
            .filter(|i| i.pty_id == Some(pty) && i.reason == "waiting")
            .count(),
        1,
        "a quiet plain pane on a menu flags waiting"
    );

    reg.ack_attention_pty(pty);
    assert!(
        reg.plain_pane_attention(now + 8000, &out, &tail, &no_input, &no_agents)
            .iter()
            .all(|i| i.reason != "waiting"),
        "ack by pty must stick while the same menu is on screen"
    );

    // Repaint (menu answered / new prompt) re-arms.
    let grew: HashMap<u32, u64> = [(pty, 200u64)].into_iter().collect();
    reg.plain_pane_attention(now + 9000, &grew, &tail, &no_input, &no_agents);
    assert_eq!(
        reg.plain_pane_attention(now + 14_000, &grew, &tail, &no_input, &no_agents)
            .iter()
            .filter(|i| i.pty_id == Some(pty) && i.reason == "waiting")
            .count(),
        1,
        "a fresh prompt after the plain pane repainted flags again"
    );
}

#[test]
fn attention_scan_surfaces_plain_panes_without_double_covering_agents() {
    // #40 review: exercise run_attention's merge wiring (the layer where the
    // scope bug lived) — attention_scan combines the roster scan with the
    // plain-pane scan. A plain pty parked on a menu must surface (keyed only by
    // pty), and an agent-owned pty must NOT be double-covered by the plain pass.
    let (reg, _d, _g, _w) = attention_setup();
    let now = 1_000_000_000_000u64;
    let no_agent_in: HashMap<String, u64> = HashMap::new();
    let no_agent_tail: HashMap<String, String> = HashMap::new();
    // pty 7 = a plain hand-opened pane; pty 5 stands in for an agent's pty.
    let p_out: HashMap<u32, u64> = [(7u32, 10u64), (5u32, 10u64)].into_iter().collect();
    let p_tails: HashMap<u32, String> = [
        (7u32, strip_ansi(FIX_CLAUDE_ASK.as_bytes())),
        (5u32, strip_ansi(FIX_CLAUDE_ASK.as_bytes())),
    ]
    .into_iter()
    .collect();
    let p_ins = HashMap::new();
    let agent_ptys: HashSet<u32> = [5u32].into_iter().collect();

    let scan = |t: u64| {
        reg.attention_scan(
            t, &no_agent_in, &no_agent_tail, &HashMap::new(), &p_out, &p_tails, &p_ins, &agent_ptys,
        )
    };
    scan(now); // establish the quiet clock
    let items = scan(now + 5000);
    assert!(
        items.iter().any(|i| i.pty_id == Some(7) && i.reason == "waiting" && i.agent_id.is_empty()),
        "a plain pane parked on a question must surface through run_attention's merge"
    );
    assert!(
        items.iter().all(|i| i.pty_id != Some(5)),
        "an agent's pty must not be double-covered by the plain pass"
    );
}

#[test]
fn pane_attention_inputs_from_strips_ansi_and_skips_agent_ptys() {
    // #40 review: drive the gather wiring with a fake live-ids source. Raw bytes
    // carry ANSI + box drawing; the built tail must be stripped, agent ptys must
    // be skipped (not gathered), and the stripped tail must still detect a prompt.
    //
    // #717: the reader is injected rather than pre-read, so this also pins WHAT
    // the gather asks each pane's ring for — a bounded tail, never the whole
    // (up to 256 KB) ring — and that an agent pty's ring is never read AT ALL,
    // which the old shape could only assert by the absence of an output entry.
    let (reg, _d, _g, _w) = attention_setup();
    let raw_menu = FIX_COPILOT_ASK.as_bytes().to_vec();
    let live = vec![
        (7u32, 42u64, 0u64),   // a plain pane
        (5u32, 99u64, 123u64), // an agent's pty — must be skipped
    ];
    let agent_ptys: HashSet<u32> = [5u32].into_iter().collect();
    let requests: std::cell::RefCell<Vec<(u32, usize)>> = std::cell::RefCell::new(Vec::new());
    let (outs, tails, ins) = reg.pane_attention_inputs_from(
        &live,
        |pid, n| {
            requests.borrow_mut().push((pid, n));
            Some(raw_menu.clone())
        },
        &agent_ptys,
    );

    assert!(!outs.contains_key(&5) && !tails.contains_key(&5), "agent pty must be skipped");
    assert_eq!(outs.get(&7), Some(&42u64));
    assert_eq!(ins.get(&7), Some(&0u64));
    let tail7 = tails.get(&7).expect("plain pty tail present");
    assert!(!tail7.contains('\u{1b}'), "ANSI escapes must be stripped from the gathered tail");
    assert!(prompt_wait_detected(tail7), "the stripped tail still detects the menu");
    assert_eq!(
        *requests.borrow(),
        vec![(7u32, ATTENTION_SCAN_BYTES)],
        "the gather must make exactly one BOUNDED read, for the plain pane only: an \
         unbounded request (or any request at all against the agent's pty) is a whole-ring \
         clone taken under the same mutex every keystroke takes (#717)"
    );
}

#[test]
fn a_pane_the_scan_cannot_read_still_gets_an_entry_rather_than_vanishing() {
    // The failure edge of the injected read (#717): a pty that died between
    // `live_ids` and the tail read hands back `None`. That must not drop the
    // pane out of the maps — `plain_pane_attention` compares this tick's
    // output_total against the last one, and a pane that vanishes from `outs`
    // is a pane whose quiet-streak bookkeeping restarts. An empty tail is the
    // honest answer (nothing readable => nothing prompt-shaped), and it is what
    // the pre-#717 `output_tail(pid).unwrap_or_default()` produced too.
    let (reg, _d, _g, _w) = attention_setup();
    let live = vec![(9u32, 7u64, 0u64)];
    let (outs, tails, ins) =
        reg.pane_attention_inputs_from(&live, |_, _| None, &HashSet::new());
    assert_eq!(outs.get(&9), Some(&7u64), "an unreadable pane keeps its output counter entry");
    assert_eq!(ins.get(&9), Some(&0u64));
    assert_eq!(tails.get(&9).map(String::as_str), Some(""), "and an empty tail, not a missing one");
    assert!(!prompt_wait_detected(tails.get(&9).unwrap()), "an empty tail is never a question");
}

#[test]
fn the_attention_scan_reads_only_the_tail_of_a_saturated_ring_never_the_whole_of_it() {
    // #717. The attention tick's ring read happens under the global `ptys`
    // mutex — the same one `write_pty`/`note_user_input` take on every
    // keystroke, and the same one the pane's reader thread queues behind to
    // append. It used to clone the WHOLE ring (up to 256 KB) and then slice the
    // last 4 KB off the result.
    //
    // The slice makes the two shapes produce byte-identical text, so no
    // assertion about the returned STRING can tell them apart. What separates
    // them is the size of the request, and that is what this pins — against a
    // real `PtyManager` whose ring is genuinely saturated, so "bounded" is a
    // measurement and not an arithmetic identity on a short buffer.
    let pm = PtyManager::default();
    let pty = 7717u32;
    pm.register_fake_for_test(pty, b"");
    // Saturate the ring the way the reader thread does (cap arithmetic and
    // all), then paint a real question as the last thing on screen.
    let chunk = vec![b'x'; 64 * 1024];
    for _ in 0..5 {
        pm.append_fake_output_for_test(pty, &chunk);
    }
    pm.append_fake_output_for_test(pty, FIX_COPILOT_ASK.as_bytes());
    assert_eq!(
        pm.output_tail(pty).unwrap().len(),
        256 * 1024,
        "the ring must actually be saturated — otherwise a bounded read proves nothing"
    );

    let requested = std::cell::Cell::new(0usize);
    let handed_back = std::cell::Cell::new(0usize);
    let tail = attention_tail(|n| {
        requested.set(n);
        let raw = pm.output_tail_bounded(pty, n);
        handed_back.set(raw.as_ref().map_or(0, Vec::len));
        raw
    })
    .expect("the fake pty is alive");

    assert_eq!(
        requested.get(),
        ATTENTION_SCAN_BYTES,
        "the scan must ask for a bounded tail; asking for the ring is the defect"
    );
    assert_eq!(
        handed_back.get(),
        ATTENTION_SCAN_BYTES,
        "and on a saturated ring that request must copy 4 KB under the lock, not 256 KB"
    );
    assert!(prompt_wait_detected(&tail), "the bounded tail still carries the live question");

    // The pre-#717 shape, written out as a control rather than described:
    // clone the whole ring, then slice the same bytes off the end. The text is
    // IDENTICAL — which is the point. A future edit that reverts the call sites
    // to `output_tail` is reverting to something this file states the cost of,
    // and no output assertion anywhere would notice.
    let whole = pm.output_tail(pty).expect("the fake pty is alive");
    let start = whole.len().saturating_sub(ATTENTION_SCAN_BYTES);
    assert_eq!(
        strip_ansi(&whole[start..]),
        tail,
        "bounding the read must not change one byte of what the detectors see"
    );
    assert_eq!(
        whole.len() / handed_back.get(),
        64,
        "the copy the old shape made under the lock was 64x this one on a saturated ring"
    );
}

#[test]
fn attention_scan_flags_a_plain_pane_with_no_orchestration_group_at_all() {
    // #40 review: the human's repro had NO orchestration group — plain panes only.
    // A fresh registry (no group, no agents) must still flag a plain pane parked
    // on a question, confirming the scan doesn't depend on any group existing.
    let (reg, _d) = test_registry();
    let now = 1_000_000_000_000u64;
    let empty_in: HashMap<String, u64> = HashMap::new();
    let empty_tail: HashMap<String, String> = HashMap::new();
    let p_out: HashMap<u32, u64> = [(3u32, 10u64)].into_iter().collect();
    let p_tails: HashMap<u32, String> =
        [(3u32, strip_ansi(FIX_CLAUDE_ASK.as_bytes()))].into_iter().collect();
    let no_agents = HashSet::new();

    let scan = |t: u64| {
        reg.attention_scan(
            t, &empty_in, &empty_tail, &HashMap::new(), &p_out, &p_tails, &HashMap::new(), &no_agents,
        )
    };
    scan(now);
    assert!(
        scan(now + 5000).iter().any(|i| i.pty_id == Some(3) && i.reason == "waiting"),
        "attention must fire for a plain pane even with no orchestration group present"
    );
}

#[test]
fn attention_waiting_ack_sticks_until_the_prompt_changes() {
    // #40 review: focusing/acking a pane parked on a live menu must make the ack
    // *stick* — the next scan must not re-light `waiting` on the pane the human is
    // already on. The suppression lifts only when the pane's output changes.
    let (reg, _d, _g, wid) = attention_setup();
    let now = 1_000_000_000_000u64;
    let out: HashMap<String, u64> = [(wid.clone(), 100u64)].into_iter().collect();
    let tail: HashMap<String, String> =
        [(wid.clone(), strip_ansi(FIX_COPILOT_ASK.as_bytes()))].into_iter().collect();
    let no_input = HashMap::new();

    // Establish the quiet clock, then flag `waiting`.
    reg.attention_tick(now, &out, &tail, &no_input);
    assert_eq!(
        reg.attention_tick(now + 5000, &out, &tail, &no_input)
            .iter()
            .filter(|i| i.agent_id == wid && i.reason == "waiting")
            .count(),
        1,
        "a quiet pane parked on a menu must flag waiting"
    );

    // The human focuses the pane (auto-ack). Even with the menu still on screen
    // (output unchanged), the next scan must not re-raise waiting.
    reg.ack_attention(&wid);
    assert!(
        reg.attention_tick(now + 8000, &out, &tail, &no_input)
            .iter()
            .all(|i| !(i.agent_id == wid && i.reason == "waiting")),
        "ack must stick while the same menu is on screen"
    );
    assert!(
        reg.attention_tick(now + 11_000, &out, &tail, &no_input)
            .iter()
            .all(|i| !(i.agent_id == wid && i.reason == "waiting")),
        "ack stays sticky across further quiet scans"
    );

    // The pane repaints (menu answered → a *new* prompt appears): re-arm and flag.
    let grew: HashMap<String, u64> = [(wid.clone(), 200u64)].into_iter().collect();
    reg.attention_tick(now + 12_000, &grew, &tail, &no_input); // observe the change, reset quiet clock
    assert_eq!(
        reg.attention_tick(now + 17_000, &grew, &tail, &no_input)
            .iter()
            .filter(|i| i.agent_id == wid && i.reason == "waiting")
            .count(),
        1,
        "a fresh prompt after the pane repainted flags again"
    );
}

#[test]
fn attention_flags_idle_with_prompt_only_when_quiet_and_unattended() {
    let (reg, _d, _g, wid) = attention_setup();
    let now = 1_000_000_000_000u64;
    let out: HashMap<String, u64> = [(wid.clone(), 100u64)].into_iter().collect();
    let prompt: HashMap<String, String> =
        [(wid.clone(), "Do you want to proceed?\n❯ 1. Yes\n  2. No".to_string())]
            .into_iter()
            .collect();
    let no_input = HashMap::new();

    // First sighting starts the quiet clock — not yet flagged even though the
    // tail is prompt-shaped (could be a prompt that just appeared, still painting).
    let first = reg.attention_tick(now, &out, &prompt, &no_input);
    assert!(first.iter().all(|i| i.reason != "waiting"), "must debounce the first quiet tick");

    // Output stable past the quiet window → idle-with-prompt.
    let waited = reg.attention_tick(now + 5000, &out, &prompt, &no_input);
    assert_eq!(
        waited.iter().filter(|i| i.agent_id == wid && i.reason == "waiting").count(),
        1,
        "a quiet pane parked on a prompt needs the human"
    );

    // The human typing into the pane means they are already on it — suppressed.
    let typed: HashMap<String, u64> = [(wid.clone(), now + 5000)].into_iter().collect();
    let while_typing = reg.attention_tick(now + 5000, &out, &prompt, &typed);
    assert!(
        while_typing.iter().all(|i| i.reason != "waiting"),
        "a recent human keystroke suppresses the idle-with-prompt badge"
    );

    // Fresh output (the CLI is painting again) resets the quiet clock.
    let grew: HashMap<String, u64> = [(wid.clone(), 200u64)].into_iter().collect();
    let painting = reg.attention_tick(now + 6000, &grew, &prompt, &no_input);
    assert!(
        painting.iter().all(|i| i.reason != "waiting"),
        "new output is activity, not an idle prompt"
    );
}

#[test]
fn attention_does_not_flag_a_quiet_pane_without_a_prompt() {
    let (reg, _d, _g, wid) = attention_setup();
    let now = 1_000_000_000_000u64;
    let out: HashMap<String, u64> = [(wid.clone(), 100u64)].into_iter().collect();
    let busy_tail: HashMap<String, String> =
        [(wid.clone(), "   Compiling loomux\ntest result: ok".to_string())].into_iter().collect();
    let no_input = HashMap::new();
    reg.attention_tick(now, &out, &busy_tail, &no_input);
    let later = reg.attention_tick(now + 60_000, &out, &busy_tail, &no_input);
    assert!(
        later.is_empty(),
        "a quiet pane whose tail is not a prompt must not be flagged, got: {later:?}"
    );
}

#[test]
fn attention_latches_worker_reports_until_ack_or_progress() {
    let (reg, _d, _g, wid) = attention_setup();
    let w = reg.agent(&wid).unwrap();
    let cw = reg.resolve_token(&w.token).unwrap();
    let now = 1_000_000_000_000u64;
    let empty = HashMap::new();

    // A blocked report badges the pane "blocked".
    let _ = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "report", "arguments": { "status": "blocked", "summary": "stuck" } }));
    let flagged = reg.attention_tick(now, &empty, &no_tails(), &empty);
    assert_eq!(
        flagged.iter().filter(|i| i.agent_id == wid && i.reason == "blocked").count(),
        1,
        "a blocked report must badge the reporting pane"
    );

    // The human acks (focuses the pane): the latch clears.
    reg.ack_attention(&wid);
    assert!(
        reg.attention_tick(now, &empty, &no_tails(), &empty).iter().all(|i| i.agent_id != wid),
        "ack must clear the report badge"
    );

    // A done report badges "report"; a later progress report (worker resumed)
    // clears it.
    let _ = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "report", "arguments": { "status": "done", "summary": "pr up" } }));
    assert_eq!(
        reg.attention_tick(now, &empty, &no_tails(), &empty)
            .iter()
            .filter(|i| i.agent_id == wid && i.reason == "report")
            .count(),
        1,
        "a done report awaits the human's review"
    );
    let _ = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "report", "arguments": { "status": "progress", "summary": "back at it" } }));
    assert!(
        reg.attention_tick(now, &empty, &no_tails(), &empty).iter().all(|i| i.agent_id != wid),
        "a progress report means the worker is active again — no badge"
    );
}

#[test]
fn attention_flags_a_worker_whose_task_is_at_a_human_gate() {
    let (reg, _d, gid, wid) = attention_setup();
    let now = 1_000_000_000_000u64;
    let empty = HashMap::new();

    // A board task assigned to the worker, sitting at the PR merge gate.
    reg.upsert_task(&gid, "orch", None, TaskPatch {
        title: Some("ship it".into()),
        status: Some("pr".into()),
        assignee: Some(wid.clone()),
        ..Default::default()
    })
    .unwrap();
    let flagged = reg.attention_tick(now, &empty, &no_tails(), &empty);
    assert_eq!(
        flagged.iter().filter(|i| i.agent_id == wid && i.reason == "gate").count(),
        1,
        "an assigned task at a human gate must flag its worker's pane"
    );

    // Approving it (status → done) drops the gate.
    let tid = reg.tasks(&gid)[0].id.clone();
    reg.upsert_task(&gid, "orch", Some(&tid),
        TaskPatch { status: Some("done".into()), ..Default::default() }).unwrap();
    assert!(
        reg.attention_tick(now, &empty, &no_tails(), &empty).iter().all(|i| i.agent_id != wid),
        "an off-gate task no longer flags its worker"
    );
}

#[test]
fn attention_toasts_once_per_onset_only_for_optin_groups() {
    let (reg, _d, gid, wid) = attention_setup();
    let blocked = vec![AttentionItem {
        agent_id: wid.clone(),
        group: gid.to_string(),
        name: "w".into(),
        role: Some(Role::Worker),
        pty_id: None,
        reason: "blocked",
        detail: "stuck".into(),
    }];

    // Notifications off by default → nothing toasts.
    assert!(reg.attention_toast_targets(&blocked).is_empty(), "no toasts until opted in");

    // Opt in → the blocked event toasts once, then dedups for the same stall.
    reg.set_notify(&gid, true).unwrap();
    assert_eq!(reg.attention_toast_targets(&blocked), vec![wid.clone()]);
    assert!(
        reg.attention_toast_targets(&blocked).is_empty(),
        "the same reason must not re-toast every scan"
    );

    // A persistent gate state never toasts — the board highlight covers it.
    let gate = vec![AttentionItem { reason: "gate", ..blocked[0].clone() }];
    assert!(reg.attention_toast_targets(&gate).is_empty(), "gate is not a toastable event");
}

/// #1091 slice D: a pending `ask_human` question DERIVES a `question`
/// attention item on the asker's own pane (orchestrator-only today). RED on
/// base: `question` is not a reason `attention_tick` ever emits there, so
/// `.find(...)` returns `None` and `.expect(...)` panics — a real behavioral
/// miss, not a compile error.
///
/// Split into four separate `#[test]`s (review finding N1 on #1123) rather
/// than one long one: `cargo test` stops a test function at its first
/// panicking assertion, so a single function covering "flags", "bumps the
/// count", "clears on settle", and "toasts" would have only ever evidenced
/// whichever assertion the RED run's mutation happened to reach — the other
/// three would carry no red evidence at all despite reading as asserted.
/// Four functions means four independent reds, each attributable to its own
/// assertion.
#[test]
fn attention_flags_the_asker_with_a_pending_question() {
    let (reg, _d, _g, co, _cw, orch_id) = setup_questions();
    let now = 1_000_000_000_000u64;
    let empty = HashMap::new();

    // No pending question yet — no badge.
    assert!(
        reg.attention_tick(now, &empty, &no_tails(), &empty).iter().all(|i| i.agent_id != orch_id),
        "nothing pending, nothing to flag"
    );

    let out = q_call(&reg, &co, "ask_human", json!({ "text": "ship it here or split it?" }));
    assert_eq!(out["isError"], false, "{}", q_text(&out));

    let flagged = reg.attention_tick(now, &empty, &no_tails(), &empty);
    let item = flagged
        .iter()
        .find(|i| i.agent_id == orch_id && i.reason == "question")
        .expect("a pending question must flag the asker's pane");
    assert!(item.detail.contains('1'), "detail should say how many are pending: {}", item.detail);
}

#[test]
fn a_second_pending_question_bumps_the_count_on_the_same_item() {
    let (reg, _d, _g, co, _cw, orch_id) = setup_questions();
    let now = 1_000_000_000_000u64;
    let empty = HashMap::new();

    q_call(&reg, &co, "ask_human", json!({ "text": "ship it here or split it?" }));
    let out2 = q_call(&reg, &co, "ask_human", json!({ "text": "another one?" }));
    assert_eq!(out2["isError"], false, "{}", q_text(&out2));

    let flagged = reg.attention_tick(now, &empty, &no_tails(), &empty);
    let item = flagged
        .iter()
        .find(|i| i.agent_id == orch_id && i.reason == "question")
        .expect("two pending questions must still flag the asker's pane");
    assert!(item.detail.contains('2'), "two pending: {}", item.detail);
}

/// Pins the slice's central "non-latched by design" claim: the registry
/// itself is the latch, so settling every pending row must clear the badge
/// with no separate ack — unlike `stranded`. The predicate this guards is
/// `!q.status.is_settled()` in `attention_tick`'s `question_of` build
/// (mod.rs): drop it (or invert it) and this is the one assertion that
/// reddens, because a withdrawn/answered row would keep counting.
#[test]
fn settling_every_pending_question_clears_the_badge_with_nothing_latched() {
    let (reg, _d, _g, co, _cw, orch_id) = setup_questions();
    let now = 1_000_000_000_000u64;
    let empty = HashMap::new();

    q_call(&reg, &co, "ask_human", json!({ "text": "ship it here or split it?" }));
    q_call(&reg, &co, "ask_human", json!({ "text": "another one?" }));
    assert!(
        reg.attention_tick(now, &empty, &no_tails(), &empty).iter().any(|i| i.agent_id == orch_id),
        "sanity: two pending questions must flag the pane before withdrawal"
    );

    // Withdrawing both settles them — the badge clears with no separate ack.
    q_call(&reg, &co, "withdraw_question", json!({ "id": "q-1" }));
    q_call(&reg, &co, "withdraw_question", json!({ "id": "q-2" }));
    assert!(
        reg.attention_tick(now, &empty, &no_tails(), &empty).iter().all(|i| i.agent_id != orch_id),
        "settling every pending question must clear the badge with nothing latched"
    );
}

/// **Forward guard, not a new-behaviour pin** (review finding N2 on #1123):
/// `attention_toast_targets` already toasts every reason but `gate`, and this
/// PR adds no logic to that function — the guard below reddens only if a
/// FUTURE change adds `question` to an exclusion list (or otherwise special-
/// cases it), not from anything in this diff. Kept as its own test, separate
/// from the three above, precisely so the red-before-green evidence for those
/// three is never read as covering this one too.
#[test]
fn a_question_attention_item_toasts_like_any_other_event_not_like_gate() {
    let (reg, _d, g, _co, _cw, orch_id) = setup_questions();
    let pending = vec![AttentionItem {
        agent_id: orch_id.clone(),
        group: g.to_string(),
        name: "orch".into(),
        role: Some(Role::Orchestrator),
        pty_id: None,
        reason: "question",
        detail: "1 pending question — needs your answer".into(),
    }];
    assert!(reg.attention_toast_targets(&pending).is_empty(), "no toasts until opted in");
    reg.set_notify(&g, true).unwrap();
    assert_eq!(reg.attention_toast_targets(&pending), vec![orch_id.clone()]);
}

#[test]
fn notify_optin_is_durable_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let gid;
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        let g = reg.create_group("C:/tmp/repo", watchdog_rails(0)).unwrap();
        gid = g.id.clone();
        assert!(!reg.notify_enabled(&gid), "off by default");
        reg.set_notify(&gid, true).unwrap();
        assert!(reg.notify_enabled(&gid));
    }
    // A fresh registry over the same root re-seeds the opt-in from the marker.
    let reg2 = relaunch_registry(dir.path());
    reg2.set_port(45999);
    let g2 = reg2.create_group("C:/tmp/repo", watchdog_rails(0)).unwrap();
    assert_eq!(g2.id, gid);
    assert!(reg2.notify_enabled(&gid), "notification opt-in must survive a restart");

    // Turning it off removes the marker.
    reg2.set_notify(&gid, false).unwrap();
    assert!(!reg2.notify_enabled(&gid));
}

#[test]
fn spawn_opens_minimized_exempts_the_orchestrator_and_the_manager() {
    // #260: every delegate role docks by default...
    for role in [Role::Worker, Role::Reviewer, Role::Planner] {
        assert!(spawn_opens_minimized(role, false), "{role:?} should dock by default");
        assert!(!spawn_opens_minimized(role, true), "{role:?} must expand once the group opts out");
    }
    // ...but the orchestrator's own pane never does, even if a caller somehow
    // passed `group_opted_expanded=false` for it (the exemption is unconditional,
    // not just "expanded happens to be the group default").
    assert!(!spawn_opens_minimized(Role::Orchestrator, false));
    assert!(!spawn_opens_minimized(Role::Orchestrator, true));
    // #1161: and neither does the manager, on the same unconditional terms.
    // The delegate loop above is this pair's non-vacuity control — it is what
    // makes "exempt" mean something rather than "the function returns false".
    assert!(!spawn_opens_minimized(Role::Manager, false), "a docked manager is a conversation the human cannot see");
    assert!(!spawn_opens_minimized(Role::Manager, true));
}

#[test]
fn spawn_expanded_optout_is_durable_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let gid;
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(46001);
        let g = reg.create_group("C:/tmp/repo-spawn-expanded", watchdog_rails(0)).unwrap();
        gid = g.id.clone();
        assert!(!reg.spawn_expanded(&gid), "minimize-on-spawn is the default — nothing opted out yet");
        reg.set_spawn_expanded(&gid, true).unwrap();
        assert!(reg.spawn_expanded(&gid));
    }
    // A fresh registry over the same root re-seeds the opt-out from the marker.
    let reg2 = relaunch_registry(dir.path());
    reg2.set_port(46001);
    let g2 = reg2.create_group("C:/tmp/repo-spawn-expanded", watchdog_rails(0)).unwrap();
    assert_eq!(g2.id, gid);
    assert!(reg2.spawn_expanded(&gid), "the expand-instead-of-dock opt-out must survive a restart");

    // Turning it back off (re-enabling minimize-on-spawn) removes the marker.
    reg2.set_spawn_expanded(&gid, false).unwrap();
    assert!(!reg2.spawn_expanded(&gid));
}

#[test]
fn group_usage_summarizes_agents_with_null_cost_without_panes() {
    let (reg, _d, _co, _cw) = setup_mcp();
    let usage = reg.group_usage(&_co.group);
    // No live panes and no transcripts in test mode → no dollar figures.
    assert!(usage["live_cost_usd"].is_null());
    assert!(usage["lifetime_cost_usd"].is_null());
    assert_eq!(usage["live_tokens"].as_u64(), Some(0));
    let agents = usage["agents"].as_array().unwrap();
    assert!(agents.iter().any(|a| a["id"] == "orch-1"), "orchestrator must appear");
    assert!(agents.iter().all(|a| a["cost_usd"].is_null()));
    // Exposed to the orchestrator over MCP too.
    let via_mcp = dispatch(&reg, &_co, "tools/call",
        &json!({ "name": "group_usage", "arguments": {} })).unwrap();
    assert_eq!(via_mcp["isError"], false);
    assert!(via_mcp["content"][0]["text"].as_str().unwrap().contains("lifetime_cost_usd"));
    // Workers cannot pull the group-wide usage summary.
    let denied = dispatch(&reg, &_cw, "tools/call",
        &json!({ "name": "group_usage", "arguments": {} })).unwrap();
    assert_eq!(denied["isError"], true,
        "usage aggregation is orchestrator-only — the one other tier that has it is a \
         declared liaison block (#891 S2, `a_liaison_block_may_read_the_groups_usage`), \
         never a worker");
}

#[test]
fn group_usage_mcp_defaults_to_top_n_summary_with_explicit_rest_count() {
    // #866: a large lifetime roster made the full per-agent table unreadable
    // in-context (654 agents / 173,245 chars on the reporting group). The
    // MCP tool must default to top_agents + an explicit rest rollup instead
    // of the raw `agents` array, and never truncate silently.
    let (reg, _d, co, _cw) = setup_mcp();
    // setup_mcp already spawned an orchestrator + a worker (both live, zero
    // tokens in test mode with no transcript). Add 13 more historical agents
    // with distinct, increasing token totals so top-N selection is provable.
    for i in 0..13u64 {
        let tokens = (i + 1) * 100;
        let cost = (i + 1) as f64 * 0.1;
        reg.upsert_usage_snapshot(
            &co.group,
            usage_snap(&format!("session-{i}"), &format!("agent-{i}"), cost, tokens, 0),
        );
    }
    // 2 live (orch + worker, 0 tokens each) + 13 synthetic = 15 total.
    let full = reg.group_usage(&co.group);
    assert_eq!(full["agents"].as_array().unwrap().len(), 15, "sanity: full table has all 15 agents");

    let via_mcp = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "group_usage", "arguments": {} })).unwrap();
    assert_eq!(via_mcp["isError"], false);
    let body: Value = serde_json::from_str(via_mcp["content"][0]["text"].as_str().unwrap()).unwrap();

    assert!(body["agents"].is_null(), "summary mode must not include the raw per-agent table");
    assert_eq!(body["agent_count"].as_u64(), Some(15), "must state the whole lifetime roster size");

    let top = body["top_agents"].as_array().expect("top_agents present");
    assert_eq!(top.len(), 10, "default top-N is 10 (#866)");
    // Sorted descending by total tokens: the 10 highest synthetic snapshots
    // (400..=1300), not the lowest-id-first order the full table uses.
    let top_tokens: Vec<u64> = top.iter().map(|a| a["tokens"]["total"].as_u64().unwrap()).collect();
    assert_eq!(top_tokens, vec![1300, 1200, 1100, 1000, 900, 800, 700, 600, 500, 400]);

    // The 3 lowest synthetic agents (100, 200, 300) plus the 2 zero-token
    // live agents were folded out of top_agents — the rollup must say so
    // explicitly rather than silently dropping them.
    assert_eq!(body["rest"]["count"].as_u64(), Some(5), "no silent truncation: rest states its own count");
    assert_eq!(body["rest"]["tokens"].as_u64(), Some(600));
    assert!((body["rest"]["cost_usd"].as_f64().unwrap() - 0.6).abs() < 1e-9);
    // usage_snap's synthetic rows are all `estimated: true` and are the only
    // rows in `rest` with a cost at all (the live orch/worker rows are
    // costless), so the basis must read as a clean "estimated", never "mixed".
    assert_eq!(body["rest"]["cost_basis"], "estimated");
    // #866 review finding 1: top-N is picked by lifetime tokens, so the two
    // zero-token LIVE agents (orch + worker) land in `rest` alongside the 3
    // lowest historical ones — `rest.live` must keep them attributable
    // rather than folding them into one undifferentiated count.
    assert_eq!(body["rest"]["live"]["count"].as_u64(), Some(2), "orch + worker are live but not in top_agents");
    assert_eq!(body["rest"]["live"]["tokens"].as_u64(), Some(0));
    assert_eq!(body["rest"]["historical"]["count"].as_u64(), Some(3), "the 3 lowest synthetic agents");
    assert_eq!(body["rest"]["historical"]["tokens"].as_u64(), Some(600));

    // Group/live totals pass through unchanged in summary mode.
    assert_eq!(body["lifetime_tokens"].as_u64(), Some(9_100));
    assert_eq!(body["live_tokens"].as_u64(), Some(0));
}

#[test]
fn group_usage_mcp_summary_when_roster_fits_under_top_n() {
    // #866 review finding 2: every prior summary-mode test built 15 agents,
    // so only the "there is a rest" branch was ever exercised. Most real
    // groups have FEWER than `GROUP_USAGE_SUMMARY_TOP_N` agents — the path
    // where `split_off(top_n.min(agent_count))` takes its `.min()` guard
    // (a bare `split_off(top_n)` panics past the Vec's length) and `rest`
    // must degrade to an honest, all-empty rollup rather than erroring or
    // fabricating a count.
    let (reg, _d, co, _cw) = setup_mcp();
    for i in 0..3u64 {
        reg.upsert_usage_snapshot(
            &co.group,
            usage_snap(&format!("session-{i}"), &format!("agent-{i}"), (i + 1) as f64 * 0.1, (i + 1) * 100, 0),
        );
    }
    // 2 live (orch + worker) + 3 synthetic = 5, under the top-10 default.
    let via_mcp = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "group_usage", "arguments": {} })).unwrap();
    assert_eq!(via_mcp["isError"], false);
    let body: Value = serde_json::from_str(via_mcp["content"][0]["text"].as_str().unwrap()).unwrap();

    assert_eq!(body["agent_count"].as_u64(), Some(5));
    assert_eq!(body["top_agents"].as_array().unwrap().len(), 5, "the whole roster fits inside top_n");
    assert_eq!(body["rest"]["count"].as_u64(), Some(0), "nothing left over to fold in");
    assert_eq!(body["rest"]["tokens"].as_u64(), Some(0));
    assert!(body["rest"]["cost_usd"].is_null(), "no rows in rest means no cost figure, not zero");
    assert!(body["rest"]["cost_basis"].is_null());
    assert_eq!(body["rest"]["live"]["count"].as_u64(), Some(0));
    assert_eq!(body["rest"]["live"]["tokens"].as_u64(), Some(0));
    assert_eq!(body["rest"]["historical"]["count"].as_u64(), Some(0));
    assert_eq!(body["rest"]["historical"]["tokens"].as_u64(), Some(0));
}

#[test]
fn group_usage_mcp_detail_flag_returns_full_agents_table() {
    // #866: `detail: true` is the escape hatch back to the full per-agent
    // table for when the orchestrator actually needs one agent's row.
    let (reg, _d, co, _cw) = setup_mcp();
    for i in 0..13u64 {
        reg.upsert_usage_snapshot(
            &co.group,
            usage_snap(&format!("session-{i}"), &format!("agent-{i}"), (i + 1) as f64 * 0.1, (i + 1) * 100, 0),
        );
    }

    let via_mcp = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "group_usage", "arguments": { "detail": true } })).unwrap();
    assert_eq!(via_mcp["isError"], false);
    let body: Value = serde_json::from_str(via_mcp["content"][0]["text"].as_str().unwrap()).unwrap();

    assert_eq!(body["agents"].as_array().unwrap().len(), 15, "detail:true keeps the full per-agent table");
    assert!(body["top_agents"].is_null(), "summary fields absent in detail mode");
    assert!(body["rest"].is_null());
    assert!(body["agent_count"].is_null());
}

#[test]
fn group_usage_mcp_rest_cost_basis_covers_reported_and_mixed_arms() {
    // #866 review finding 3 (round 2): every prior test's synthetic rows were
    // `estimated: true` (`usage_snap`'s hardcoded default), so `rest.cost_basis`
    // was only ever exercised on the "estimated" and "no cost at all" arms.
    // This pins the other two — an opencode-style CLI-priced row (`estimated:
    // false`) folded into `rest` alone gives "reported"; folded in alongside
    // an estimated row gives "mixed" — against `OrchRegistry::usage_cost_basis`,
    // the function `compute_group_usage`'s own `*_cost_basis` fields and
    // `rest.cost_basis` now share (round-2 finding 2).
    let (reg, _d, co, _cw) = setup_mcp();
    // 10 "top" agents, tokens 100..1000, comfortably above everything below —
    // they fill top_agents regardless of what lands in `rest`.
    for i in 1..=10u64 {
        reg.upsert_usage_snapshot(&co.group, usage_snap(&format!("top-{i}"), &format!("top-agent-{i}"), 1.0, i * 100, 0));
    }
    // One CLI-priced (estimated: false) row, low enough to fall into `rest`
    // alongside the 2 zero-token live agents (orch + worker).
    reg.upsert_usage_snapshot(
        &co.group,
        UsageSnapshot { estimated: false, ..usage_snap("reported-1", "reported-agent", 0.7, 50, 0) },
    );

    let via_mcp = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "group_usage", "arguments": {} })).unwrap();
    let body: Value = serde_json::from_str(via_mcp["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["rest"]["count"].as_u64(), Some(3), "1 reported synthetic + 2 zero-token live agents");
    assert_eq!(body["rest"]["cost_basis"], "reported", "rest's only cost-bearing row is CLI-priced");

    // Add a second, even-lower-token row that IS estimated — now `rest` has
    // one reported and one estimated row, so the basis must read "mixed"
    // rather than staying "reported" or flipping to "estimated".
    reg.upsert_usage_snapshot(&co.group, usage_snap("mixed-1", "mixed-agent", 0.3, 25, 0));

    let via_mcp = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "group_usage", "arguments": {} })).unwrap();
    let body: Value = serde_json::from_str(via_mcp["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["rest"]["count"].as_u64(), Some(4), "now 2 synthetic + 2 live");
    assert_eq!(body["rest"]["cost_basis"], "mixed", "rest blends an estimated row and a reported row");
}

/// Build a durable usage snapshot for a session, as a fresh transcript read
/// would produce.
pub(crate) fn usage_snap(key: &str, agent_id: &str, cost: f64, input: u64, output: u64) -> UsageSnapshot {
    UsageSnapshot {
        key: key.to_string(),
        agent_id: agent_id.to_string(),
        name: agent_id.to_string(),
        role: "worker".to_string(),
        source: "transcript".to_string(),
        block: "worker".to_string(),
        cli: "claude".to_string(),
        input_tokens: input,
        output_tokens: output,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        cost_usd: Some(cost),
        estimated: true,
        model: Some("claude-opus-4-8".to_string()),
        current_model: Some("claude-opus-4-8".to_string()),
        updated_ms: 0,
        activity: Default::default(),
    }
}

#[test]
fn killed_agent_stays_in_lifetime_total_but_not_live() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    let sid = w.session_id.clone().expect("claude worker gets a session id");

    // Simulate a transcript read having captured this session's spend.
    reg.upsert_usage_snapshot(&g.id, usage_snap(&sid, &w.id, 1.50, 1000, 2000));

    let before = reg.group_usage(&g.id);
    let row = before["agents"].as_array().unwrap().iter()
        .find(|a| a["id"] == w.id.as_str()).expect("worker row present");
    assert_eq!(row["live"], true);
    assert_eq!(row["tokens"]["total"].as_u64(), Some(3000));
    assert!((before["live_cost_usd"].as_f64().unwrap() - 1.50).abs() < 1e-9);
    assert!((before["lifetime_cost_usd"].as_f64().unwrap() - 1.50).abs() < 1e-9);

    // Kill the worker. mark_dead re-reads usage (no transcript in test mode →
    // empty), but the merge must keep the captured spend rather than zero it.
    reg.mark_dead(&w.id, Some(0));

    let after = reg.group_usage(&g.id);
    let row = after["agents"].as_array().unwrap().iter()
        .find(|a| a["id"] == w.id.as_str()).expect("dead worker still listed");
    assert_eq!(row["live"], false, "killed agent is no longer live");
    // Lifetime keeps the spend; live no longer counts the dead worker.
    assert!((after["lifetime_cost_usd"].as_f64().unwrap() - 1.50).abs() < 1e-9,
        "lifetime total must survive the kill");
    assert!(after["live_cost_usd"].is_null(), "no live agents contribute cost now");
    assert_eq!(after["lifetime_tokens"].as_u64(), Some(3000));
    assert_eq!(after["live_tokens"].as_u64(), Some(0));
}

// ---------- #1317: the POLLED usage view ----------
//
// `orch_group_usage` answered the whole LIFETIME roster — one row per agent
// the group ever had, rebuilt and re-serialized every 2 s by the group view
// and every 4 s per group-bound tab when #1317 measured it (one publisher pass
// per second since #1608). `groupview.ts` indexes that array by id
// and looks up only the agents `orch_group_summary` reports LIVE, so every
// historical row was payload with no reader, growing with session length on a
// fixed cadence. `live_usage_view` is the cut; these two tests pin that it
// folds the right rows away and that nothing goes missing when it does.

fn usage_ids_where(v: &Value, key: &str, live: bool) -> Vec<String> {
    const NO_ROWS: &[Value] = &[];
    let mut ids: Vec<String> = v[key]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(NO_ROWS)
        .iter()
        .filter(|a| a["live"].as_bool().unwrap_or(false) == live)
        .map(|a| a["id"].as_str().unwrap_or_default().to_string())
        .collect();
    ids.sort();
    ids
}

#[test]
fn the_polled_usage_view_carries_the_live_rows_and_names_the_roster_it_folded() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo-usage-live-view", rails()).unwrap();

    let live_w = reg.spawn_agent(&g.id, Role::Worker, "live-w", "t", false, None).unwrap();
    let live_sid = live_w.session_id.clone().expect("claude worker gets a session id");
    reg.upsert_usage_snapshot(&g.id, usage_snap(&live_sid, &live_w.id, 1.00, 1000, 0));

    let dead_w = reg.spawn_agent(&g.id, Role::Worker, "dead-w", "t", false, None).unwrap();
    let dead_sid = dead_w.session_id.clone().unwrap();
    reg.upsert_usage_snapshot(&g.id, usage_snap(&dead_sid, &dead_w.id, 2.00, 2000, 0));
    reg.mark_dead(&dead_w.id, Some(0));

    let full = reg.group_usage(&g.id);
    // POSITIVE CONTROL for every absence assertion below: the roster this is
    // cut from must really hold BOTH kinds of row, or "the historical rows are
    // gone" would pass just as well against a roster that never had one.
    let full_live = usage_ids_where(&full, "agents", true);
    let full_hist = usage_ids_where(&full, "agents", false);
    assert_eq!(full_live, vec![live_w.id.clone()], "sanity: one live row in the full roster");
    assert_eq!(full_hist, vec![dead_w.id.clone()], "sanity: one historical row in the full roster");

    let view = reg.group_usage_live_within(&g.id, Duration::ZERO);

    // The old whole-roster key is GONE rather than filtered in place: a reader
    // written against `agents` must fail loudly, never quietly render a subset.
    assert!(view.get("agents").is_none(), "the polled view must not answer the old whole-roster key");

    // Exactly the rows the full value marks live — the set equality, not a
    // count, so a filter that kept the wrong rows cannot pass.
    assert_eq!(usage_ids_where(&view, "live_agents", true), full_live);
    assert!(usage_ids_where(&view, "live_agents", false).is_empty(),
        "a historical row in the LIVE array would be a row the group view then renders as running");

    // Not a silent truncation: the roster's real size is named, and it does
    // not equal what was shipped.
    assert_eq!(view["agent_count"].as_u64(), Some(2));
    assert_eq!(view["live_agents"].as_array().unwrap().len(), 1);

    // And no spend disappears with the rows. The group view's headline figure
    // is the LIFETIME total, which still sums the whole roster.
    assert_eq!(view["lifetime_tokens"], full["lifetime_tokens"]);
    assert_eq!(view["lifetime_cost_usd"], full["lifetime_cost_usd"]);
    assert_eq!(view["lifetime_cost_basis"], full["lifetime_cost_basis"]);
    assert_eq!(view["live_tokens"], full["live_tokens"]);
    assert_eq!(view["live_cost_usd"], full["live_cost_usd"]);
    assert_eq!(view["lifetime_tokens"].as_u64(), Some(3000), "1000 live + 2000 historical");
}

#[test]
fn a_group_whose_agents_have_all_exited_still_reports_its_lifetime_spend() {
    // The case where a silent truncation and an honest fold look identical
    // from the array alone: nothing live, so `live_agents` is empty either
    // way. `agent_count` and the lifetime totals are what tell them apart —
    // an empty array beside a $3.00 lifetime figure is a group that HAS
    // spent, not a group with nothing to report.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo-usage-all-exited", rails()).unwrap();
    for (i, cost) in [1.00f64, 2.00].into_iter().enumerate() {
        let w = reg.spawn_agent(&g.id, Role::Worker, &format!("w{i}"), "t", false, None).unwrap();
        let sid = w.session_id.clone().unwrap();
        reg.upsert_usage_snapshot(&g.id, usage_snap(&sid, &w.id, cost, 1000, 0));
        reg.mark_dead(&w.id, Some(0));
    }

    let view = reg.group_usage_live_within(&g.id, Duration::ZERO);
    assert!(view["live_agents"].as_array().unwrap().is_empty(), "nothing is running");
    assert_eq!(view["agent_count"].as_u64(), Some(2), "both exited agents are still on the roster");
    assert_eq!(view["lifetime_tokens"].as_u64(), Some(2000));
    assert!((view["lifetime_cost_usd"].as_f64().unwrap() - 3.00).abs() < 1e-9,
        "the lifetime figure must survive the fold that dropped the rows it came from");
    assert_eq!(view["live_tokens"].as_u64(), Some(0));
}

#[test]
fn mark_dead_captures_usage_from_transcript() {
    // Point the usage reader at a fixture transcript tree instead of ~/.claude,
    // via a per-registry override (no global env — safe under parallel runs).
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    let sid = w.session_id.clone().unwrap();

    // Write a synthetic Claude transcript for this session under an
    // encoded-cwd folder (any name — the reader scans all of them).
    let encoded = proj.path().join("C--tmp-repo");
    fs::create_dir_all(&encoded).unwrap();
    let transcript = format!(
        "{}\n{}\n",
        json!({"type":"user","message":{"content":"hi"}}),
        json!({"type":"assistant","message":{"id":"m1","model":"claude-opus-4-8",
            "usage":{"input_tokens":1000,"output_tokens":500,
                     "cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}),
    );
    fs::write(encoded.join(format!("{sid}.jsonl")), transcript).unwrap();

    // Kill without ever calling group_usage first: mark_dead must snapshot it.
    reg.mark_dead(&w.id, Some(0));

    let usage = reg.group_usage(&g.id);
    let row = usage["agents"].as_array().unwrap().iter()
        .find(|a| a["id"] == w.id.as_str()).expect("dead worker captured");
    assert_eq!(row["live"], false);
    assert_eq!(row["source"], "transcript");
    assert_eq!(row["estimated"], true);
    assert_eq!(row["tokens"]["total"].as_u64(), Some(1500));
    // Opus: (1000*5 + 500*25) / 1e6 = 0.0175
    let expect = (1000.0 * 5.0 + 500.0 * 25.0) / 1_000_000.0;
    assert!((usage["lifetime_cost_usd"].as_f64().unwrap() - expect).abs() < 1e-9);
    // Token-derived → labelled estimated, not reported.
    assert_eq!(usage["lifetime_cost_basis"], "estimated");
}

// ---------- #2167: an agent's CLI is its BLOCK's, not its class default's ----------

/// The `rails()` roster with the class-default worker block pinned to opencode
/// and a SECOND worker block, `worker-adv`, on claude.
///
/// **The ORDERING is the fixture.** `Guardrails::cli_for(Role::Worker)` asks
/// "what does this class's DEFAULT block run" and answers `opencode` here,
/// while a pane spawned from `worker-adv` runs claude. A roster declaring one
/// block per class cannot tell those two questions apart — which is exactly why
/// every pre-#2167 usage test stayed green while every claude delegate in a
/// two-block class was being read as an opencode one.
fn rails_two_worker_blocks_cheap_first() -> Guardrails {
    let mut g = rails();
    let w = g.blocks.iter_mut().find(|b| b.kind == Role::Worker).expect("default roster has a worker");
    w.cli = "opencode".into();
    w.model = String::new();
    g.blocks.push(workflow::Block {
        id: "worker-adv".into(),
        name: "worker-adv".into(),
        kind: Role::Worker,
        cli: "claude".into(),
        model: "opus".into(),
        prompt: None,
        profile: None,
        allow: vec![],
        role_hint: None,
        effort: String::new(),
        context: String::new(),
        remote: None,
        driver: None,
        cache_ttl_minutes: None,
    });
    g
}

#[test]
fn a_second_block_of_a_class_resolves_its_own_cli_not_the_class_default() {
    let g = rails_two_worker_blocks_cheap_first().clamped();
    // The class question and the agent question have different answers, and both
    // are pinned: a "fix" that made `cli_for` return the pane's CLI would be a
    // different (and wrong) change — `cli_for` still answers about the class,
    // `cli_for_block` about one agent.
    assert_eq!(g.cli_for(Role::Worker), "opencode", "the class default is the FIRST worker block");
    assert_eq!(g.cli_for_block("worker-adv", Role::Worker), "claude", "a named block runs its own CLI");
    assert_eq!(g.cli_for_block("worker", Role::Worker), "opencode", "...including when that IS the default");
    // A block id this roster does not declare — renamed, dropped, or an
    // `AgentRecord` persisted before #222 with an empty `block` — falls back to
    // the class default, which is what every caller got before.
    assert_eq!(g.cli_for_block("gone", Role::Worker), "opencode");
    assert_eq!(g.cli_for_block("", Role::Worker), "opencode");
}

#[test]
fn a_claude_pane_reads_its_transcript_when_the_class_default_block_runs_another_cli() {
    // #2167. The pane is `worker-adv` (claude); the class default is opencode.
    // Resolving the CLI from the ROLE hands `compute_usage_snapshot` "opencode",
    // so its claude arm never runs, the opencode store has no such session, and
    // the row lands as `none`/`statusline` with zero tokens — for a session
    // whose transcript is sitting on disk the whole time.
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let g = reg.create_group("C:/tmp/repo-2167", rails_two_worker_blocks_cheap_first()).unwrap();

    let rails = reg.group(&g.id).unwrap().guardrails;
    assert_eq!(rails.cli_for(Role::Worker), "opencode", "fixture: the class default disagrees");

    let w = reg
        .spawn_agent_ex(&g.id, Role::Worker, Some("worker-adv".into()), "w", "task", false, None, None, None, None, None)
        .unwrap();
    assert_eq!(w.block, "worker-adv");
    // The SPAWN path already resolved per-block (`workflow::cli_of(&block, ..)`),
    // which is why the pane has a claude session id at all: the two halves
    // disagreed, and only the reader was wrong.
    let sid = w.session_id.clone().expect("a claude block pre-assigns a session id");

    // A worktree-cwd project folder, mixed separators and all — the shape #2167
    // first suspected. It is a NON-REGRESSION witness, not the cause:
    // `claude_transcript_path` scans every folder under the projects root and
    // never derives a slug, so this name is found exactly as `C--tmp-repo` is.
    let encoded = proj.path().join("C--Projects-loomux-worktrees-agent-rev-1919");
    fs::create_dir_all(&encoded).unwrap();
    fs::write(
        encoded.join(format!("{sid}.jsonl")),
        format!(
            "{}\n{}\n",
            json!({"type":"user","message":{"content":"hi"}}),
            json!({"type":"assistant","message":{"id":"m1","model":"claude-opus-4-8",
                "usage":{"input_tokens":1000,"output_tokens":500,
                         "cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}),
        ),
    )
    .unwrap();

    // The live path (`compute_group_usage`) and the exit path (`mark_dead`)
    // resolved the CLI separately and were wrong separately, so both are pinned.
    let live = reg.group_usage(&g.id);
    let live_row = live["agents"].as_array().unwrap().iter()
        .find(|a| a["id"] == w.id.as_str()).expect("live worker on the roster");
    assert_eq!(live_row["source"], "transcript", "live poll must read the claude transcript");
    assert_eq!(live_row["tokens"]["total"].as_u64(), Some(1500));

    reg.mark_dead(&w.id, Some(0));
    let usage = reg.group_usage(&g.id);
    let row = usage["agents"].as_array().unwrap().iter()
        .find(|a| a["id"] == w.id.as_str()).expect("dead worker captured");
    assert_eq!(row["source"], "transcript", "mark_dead must read it too");
    assert_eq!(row["tokens"]["total"].as_u64(), Some(1500));
    assert_eq!(usage["lifetime_tokens"].as_u64(), Some(1500));
}

#[test]
fn a_zero_dollar_statusline_row_never_overwrites_captured_transcript_tokens() {
    // #2167, second half. `merge_usage_entry` used to call a row "empty" by its
    // SOURCE (`== "none"`), so the one fallback that reports a genuine zero — a
    // statusline parse on a subscription/Max account, where the CLI prints
    // `$0.00` whatever was really spent — replaced a transcript row's real
    // tokens with zeros.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo-2167-merge", rails()).unwrap();
    reg.upsert_usage_snapshot(&g.id, usage_snap("s1", "w-1", 1.25, 1000, 500));
    assert_eq!(reg.group_usage(&g.id)["lifetime_tokens"].as_u64(), Some(1500),
        "control: the transcript row really is in the store before the clobber");

    let mut bare = usage_snap("s1", "w-1", 0.0, 0, 0);
    bare.source = "statusline".into();
    bare.estimated = false;
    bare.model = None;
    reg.upsert_usage_snapshot(&g.id, bare);

    let after = reg.group_usage(&g.id);
    assert_eq!(after["lifetime_tokens"].as_u64(), Some(1500),
        "a row carrying no figures at all must not win over one that has them");
    assert!((after["lifetime_cost_usd"].as_f64().unwrap() - 1.25).abs() < 1e-9);

    // Negative control — "keep the old row" is not the answer either: a read
    // that really did come back richer still replaces.
    reg.upsert_usage_snapshot(&g.id, usage_snap("s1", "w-1", 2.50, 2000, 1000));
    assert_eq!(reg.group_usage(&g.id)["lifetime_tokens"].as_u64(), Some(3000));
}

#[test]
fn the_disclosed_residual_holds_a_priced_statusline_row_still_replaces_tokens() {
    // #2167 review, premortem. The fix above narrows "empty" to the FIGURES, and
    // the residual it deliberately leaves is stated in `merge_usage_entry`, in
    // `docs/design/group-cost-tracking.md` and in the PR body: a statusline read
    // carrying a NON-zero dollar figure still replaces a token-bearing row,
    // trading exact tokens for a price-table estimate.
    //
    // This pins the BLIND SPOT ITSELF (CLAUDE.md: "a documented escape hatch is
    // a counterfactual — only a test that performs the edit pins it"). Without
    // it the suite pins only the arms that work, and the disclosure could go
    // false — in either direction — with nothing red to say so.
    //
    // It matters more after the fix, not less: the transcript row this can now
    // destroy carries the millions of tokens the collector was previously
    // failing to capture at all. The trigger is an account that pays per token
    // (the CLI prints a real figure rather than the `$0.00` a subscription
    // shows) on a tick where the transcript read misses.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo-2167-residual", rails()).unwrap();
    reg.upsert_usage_snapshot(&g.id, usage_snap("s1", "w-1", 1.25, 1000, 500));
    assert_eq!(reg.group_usage(&g.id)["lifetime_tokens"].as_u64(), Some(1500),
        "control: the transcript row is in the store first");

    // What an API-key account's statusline parse produces: no tokens, a real
    // dollar figure. `new_empty` is false because `cost_usd > 0`, so it wins.
    let mut priced = usage_snap("s1", "w-1", 0.42, 0, 0);
    priced.source = "statusline".into();
    priced.estimated = false;
    priced.model = None;
    reg.upsert_usage_snapshot(&g.id, priced);

    let after = reg.group_usage(&g.id);
    assert_eq!(after["lifetime_tokens"].as_u64(), Some(0),
        "RESIDUAL, not a bug report: a priced statusline row still replaces the \
         tokens. If this ever reads 1500, the residual was closed and every \
         surface that discloses it has gone stale — fix the prose, not this test.");
    assert!((after["lifetime_cost_usd"].as_f64().unwrap() - 0.42).abs() < 1e-9);
}
