//! Holds and notices: queue-full, the orchestrator notice relay, hold episodes, undeliverable notices, pause and resume.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ---------- #563: no invisible hold, no silent queue-full ----------
//
// The incident: a copilot orchestrator's deliveries were held against a pane
// that looked EMPTY, with no warning at all, until the delivery queue filled
// completely and work was dropped. The investigation (issue comment) found two
// structural causes, and each test below pins one of them.

#[test]
fn a_held_pane_is_reported_from_the_first_poll_not_only_after_the_bound() {
    // #563's core defect, at the decision that owns it.
    //
    // The pane-header chip only ever existed INSIDE one `deliver_now` attempt,
    // around its individually-capped waits. The hold a human actually sits in
    // front of lives BETWEEN attempts, in `run_queue_drainer`'s poll loop —
    // and that loop consulted `held_escalation`, which returned `None` for
    // every poll inside `QUESTION_HOLD_STALE_AFTER`. Ten minutes of a pane
    // saying nothing while loomux withheld every delivery to it, and (per that
    // constant's own doc) up to ~19 in practice because the bound is only
    // sampled between attempts.
    //
    // A hold that is visible is an inconvenience; a hold that is invisible
    // until the queue overflows is a work-loss event.
    let bound = QUESTION_HOLD_STALE_AFTER.as_millis() as u64;
    let held = 1_000_000u64;
    let one_poll_in = held + 2_000; // a single QUEUE_DRAIN_POLL tick

    let box_hold = held_escalation(WriteAdmission::HoldBoxOccupied, held, one_poll_in, bound, false);
    assert_ne!(
        box_hold,
        HeldEscalation::None,
        "a pane held two seconds into an hours-long hold must report SOMETHING — reporting \
         nothing until the bound is #563"
    );
    assert_eq!(
        box_hold,
        HeldEscalation::Chip(HeldReason::BoxOccupied),
        "and what it reports is the chip, naming the gate that is actually blocking"
    );

    let question_hold = held_escalation(WriteAdmission::HoldQuestion, held, one_poll_in, bound, false);
    assert_ne!(question_hold, HeldEscalation::None);
    assert_eq!(
        question_hold,
        HeldEscalation::Chip(HeldReason::InteractiveQuestion),
        "a question hold chips as a question, not as leftover typing — the two ask the human \
         for different actions"
    );

    // The escalation is unchanged: still at the bound, still one-shot. #563 is
    // about the silence BEFORE it, and a fix that moved the badge earlier
    // instead would trade one wrong behaviour for another (a badge on every
    // ordinary two-second hold).
    assert_eq!(
        held_escalation(WriteAdmission::HoldBoxOccupied, held, held + bound, bound, false),
        HeldEscalation::Badge(StrandedBlocker::HumanInput),
        "the ten-minute escalation still fires exactly where it did"
    );
}

#[test]
fn a_recovered_pane_is_cleared_even_when_it_was_never_escalated() {
    // The mirror-image bug the fix must not introduce. With a chip that can be
    // up without a badge ever having fired, "we badged, so there is something
    // to clear" stopped being a safe inference: a pane held for two minutes
    // and then released would keep a permanent ⏸ chip, which is worse than no
    // chip at all — a stale "held" badge trains a human to ignore the real one.
    let bound = QUESTION_HOLD_STALE_AFTER.as_millis() as u64;
    let (held, now) = (1_000_000u64, 1_060_000u64);

    assert_eq!(
        held_escalation(WriteAdmission::Go, held, now, bound, false),
        HeldEscalation::Clear,
        "a writable pane must clear, even though nothing was ever escalated — the chip is what \
         is coming down. Callers guard their own clears, so this costs no spurious audit line"
    );
    assert_eq!(
        held_escalation(WriteAdmission::Go, held, now, bound, true),
        HeldEscalation::Clear,
        "and the badged case still clears, exactly as before"
    );
}

#[test]
fn every_hold_class_reaches_a_human_who_is_not_the_orchestrator_channel() {
    // The enumeration, executable. #563 happened because "which holds are
    // visible" lived nowhere a compiler or test could check it — so a hold
    // classification with no channel at all (the drainer's poll hold) could
    // exist for as long as it did without anything failing.
    //
    // Two properties, and the second is the one that matters: `notify_queue`
    // returns early whenever the target IS the group's orchestrator, so a
    // classification whose only channel is the in-band notice is silent on
    // precisely the pane #563 was reported on.
    assert_eq!(
        HoldClass::ALL.len(),
        14,
        "a new HoldClass must be added to HoldClass::ALL (and this count bumped) or the \
         completeness of this test is a fiction"
    );
    for (i, class) in HoldClass::ALL.iter().enumerate() {
        assert_eq!(class.ordinal(), i, "HoldClass::ALL must be in ordinal order: {class:?}");
        let channels = hold_channels(*class);
        assert!(!channels.is_empty(), "{class:?} has no channel at all — that is an invisible hold");
        assert!(
            channels.iter().any(|c| c.survives_orchestrator_target()),
            "{class:?} is reported ONLY through channels an orchestrator target suppresses \
             ({channels:?}) — invisible exactly where it matters most"
        );
    }

    // And the predicate itself is not vacuously true, or the loop above proves
    // nothing.
    assert!(!HoldChannel::OrchestratorNotice.survives_orchestrator_target());
    assert!(HoldChannel::HeldChip.survives_orchestrator_target());
    assert!(HoldChannel::AttentionBadge.survives_orchestrator_target());
}

// ---------- #578: an orchestrator channel that is not a delivery ----------
//
// `notify_queue` returns early with `notice-suppressed` whenever the target IS
// the group's own orchestrator, and that suppression is *correct*: a prompt
// announcing the orchestrator's own blocked delivery would queue behind the
// very block it is reporting. What was wrong is what happened next — the
// notice was DISCARDED. #563/#572 routed around it with channels that survive
// an orchestrator target (the held chip, the attention badge), but both of
// those reach a HUMAN AT THE WINDOW; on an unattended overnight run nobody is
// told at all, which is what `StrandedBlocker::QuestionStale`'s doc predicted
// in exactly those words.
//
// The channel added here is not a delivery: the notice is parked and rides
// back on the orchestrator's next MCP tool result — a call the orchestrator
// itself made, which is also the proof that it is running and reading at that
// instant. Nothing is typed into the blocked pane, so nothing can queue behind
// the block it reports.

#[test]
fn an_orchestrator_target_queue_notice_rides_back_on_its_next_tool_result() {
    let (reg, _d, co, _cw) = setup_mcp();
    let notice = queue::dropped_notice(&co.agent_id, 3, queue::DropReason::QueueFull);

    // Exactly the call `announce_dropped` makes when the queue it just threw
    // away belonged to the group's own orchestrator. Before #578 this line was
    // the whole story: audited, and gone.
    reg.notify_queue(&co.group, &co.agent_id, true, &notice);

    let r = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "list_agents", "arguments": {} })).unwrap();
    let blocks = r["content"].as_array().expect("content is a block list").clone();
    assert_eq!(
        blocks.len(), 2,
        "the orchestrator's own suppressed queue notice must ride back on its next tool \
         result — got {blocks:?}"
    );
    let relay = blocks[1]["text"].as_str().unwrap();
    assert!(relay.contains(&notice), "the relay must carry the notice VERBATIM: {relay}");
    assert!(
        relay.contains("YOUR OWN pane"),
        "the relay must say whose pane it is about — every other [orrerix] notice an \
         orchestrator reads is about somebody else: {relay}"
    );
    assert!(
        relay.contains("nothing needs re-sending"),
        "the payloads these notices describe are already queued or already gone, so the \
         relay must not invite a re-send: {relay}"
    );

    // Block 0 is untouched, byte for byte. Several tools return JSON their
    // caller parses; a notice glued onto that string would corrupt it, which
    // is why the relay is a SECOND block and not an append.
    let listing = blocks[0]["text"].as_str().unwrap();
    serde_json::from_str::<Value>(listing).expect("the tool's own block must still parse as JSON");

    // The durable half. The relay lives in memory, so after a loomux restart
    // the audit line is all there is — it has to carry the text, not just the
    // fact that a notice once existed.
    let sup = reg
        .audit_log(&co.group)
        .into_iter()
        .find(|e| e.action == "notice-suppressed"
            && e.detail["reason"] == json!("target-is-orchestrator"))
        .expect("the suppression is still audited");
    assert_eq!(sup.detail["text"], json!(notice), "the suppressed text must be recoverable");
    assert_eq!(sup.detail["parked"], json!(true), "and must say it was parked, not dropped");
    let relayed = reg
        .audit_log(&co.group)
        .into_iter()
        .find(|e| e.action == "notice-relayed")
        .expect("the relay itself must be audited, not silent");
    assert_eq!(relayed.detail["count"], json!(1), "got: {}", relayed.detail);
}

#[test]
fn the_relayed_notice_drains_once_and_never_becomes_a_delivery_to_the_pane_it_reports() {
    let (reg, _d, co, _cw) = setup_mcp();
    reg.notify_queue(&co.group, &co.agent_id, true,
        &queue::queued_notice(&co.agent_id, queue::EnqueueReason::BoxOccupied));

    let first = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "list_agents", "arguments": {} })).unwrap();
    assert_eq!(first["content"].as_array().unwrap().len(), 2, "the first call relays it");

    let second = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "list_agents", "arguments": {} })).unwrap();
    assert_eq!(
        second["content"].as_array().unwrap().len(), 1,
        "a relayed notice must not repeat on every later call — that is a nag, not a channel, \
         and an orchestrator's context is what pays for it"
    );

    // The property the whole design is bounded by: the fix must not
    // reintroduce the loop. Nothing may be enqueued for the pane the notice is
    // about, or the report is once again stuck behind the block it reports.
    assert!(
        reg.audit_log(&co.group)
            .iter()
            .all(|e| !(e.action == "delivery-queued" && e.detail["to"] == json!(co.agent_id))),
        "the relay must never enqueue anything for the pane it reports on"
    );
}

#[test]
fn a_worker_tool_call_never_drains_the_orchestrators_notice_inbox() {
    let (reg, _d, co, cw) = setup_mcp();
    reg.notify_queue(&co.group, &co.agent_id, true,
        &queue::dropped_notice(&co.agent_id, 1, queue::DropReason::AgentDied));

    let w = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "list_agents", "arguments": {} })).unwrap();
    assert_eq!(
        w["content"].as_array().unwrap().len(), 1,
        "a worker draining the orchestrator's inbox would consume the relay and never deliver \
         it — a silent loss dressed as a fix"
    );

    let o = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "list_agents", "arguments": {} })).unwrap();
    assert_eq!(o["content"].as_array().unwrap().len(), 2, "so it is still there to relay");
}

#[test]
fn a_failed_tool_call_still_carries_the_relay() {
    // The relay is attached on `isError` results too: the orchestrator is
    // demonstrably alive and reading either way, and withholding the notice
    // because an unrelated call failed would put it back in the hole this
    // exists to fill.
    let (reg, _d, co, _cw) = setup_mcp();
    reg.notify_queue(&co.group, &co.agent_id, true,
        &queue::dropped_notice(&co.agent_id, 2, queue::DropReason::QueueFull));

    let r = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "get_task", "arguments": { "id": "no-such-task" } })).unwrap();
    assert_eq!(r["isError"], json!(true), "the call really did fail");
    assert_eq!(r["content"].as_array().unwrap().len(), 2, "and the relay rode back anyway");
}

#[test]
fn during_a_pause_a_worker_target_is_only_audited_but_an_orchestrator_target_still_parks() {
    // The BRANCH ORDER inside `notify_queue` is the behavior here, so it is
    // asserted directly rather than implied by a case that could not have
    // parked anyway (review NB2 — the earlier version of this test passed
    // `target_is_orchestrator: false` and then claimed, by its name, that a
    // paused group never parks. Both halves of that are now pinned, and they
    // differ).
    //
    // `target_is_orchestrator` is checked BEFORE `is_paused`, so an
    // orchestrator-target notice raised during a pause is PARKED. That is the
    // right way round: a pause suppresses deliveries, and the relay is not one
    // — a paused orchestrator can still call tools, and its own pane's queue
    // pressure is exactly what it should learn about while everything else is
    // held. The live path is `note_queue_capacity` on the orchestrator's own
    // pane, which resolves the target itself and is not gated on the pause.
    let (reg, _d, co, cw) = setup_mcp();
    reg.pause_group(&co.group).unwrap();

    // Worker target — the pause branch. Audited, never parked. #615 is why:
    // a pause is now a DELAY, so the delivery behind it is flushed at resume
    // and `flush_header_text` announces it then; relaying this minutes later
    // out of a buffer would say "queued" about a delivery that has landed.
    reg.notify_queue(&co.group, &cw.agent_id, false,
        &queue::queued_notice(&cw.agent_id, queue::EnqueueReason::BoxOccupied));
    let sup = reg
        .audit_log(&co.group)
        .into_iter()
        .find(|e| e.action == "notice-suppressed" && e.detail["to"] == json!(cw.agent_id))
        .expect("a paused group's queue notice is suppressed AND audited");
    assert_eq!(sup.detail["reason"], json!("group-paused"), "got: {}", sup.detail);
    assert_eq!(sup.detail["parked"], Value::Null, "the pause branch parks nothing");
    let r = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "list_agents", "arguments": {} })).unwrap();
    assert_eq!(r["content"].as_array().unwrap().len(), 1, "so there is nothing to relay");

    // Orchestrator target, same pause — the branch above it, which parks.
    let own = queue::at_capacity_notice(&co.agent_id, queue::QUEUE_MAX_PER_PANE);
    reg.notify_queue(&co.group, &co.agent_id, true, &own);
    let sup = reg
        .audit_log(&co.group)
        .into_iter()
        .filter(|e| e.action == "notice-suppressed" && e.detail["to"] == json!(co.agent_id))
        .next_back()
        .expect("still audited");
    assert_eq!(
        sup.detail["reason"], json!("target-is-orchestrator"),
        "the orchestrator-target branch runs first, so the pause never sees this one: {}",
        sup.detail
    );
    assert_eq!(sup.detail["parked"], json!(true), "and it parks: {}", sup.detail);

    let r = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "list_agents", "arguments": {} })).unwrap();
    let blocks = r["content"].as_array().unwrap();
    assert_eq!(
        blocks.len(), 2,
        "a paused orchestrator must still learn about its OWN pane — the relay is not a \
         delivery, so the pause has no reason to withhold it"
    );
    assert!(blocks[1]["text"].as_str().unwrap().contains(&own), "verbatim: {blocks:?}");
}

#[test]
fn the_notice_inbox_names_what_its_cap_elided_instead_of_hiding_it() {
    // A relay that silently held back N notices would read as complete while
    // being short — the "looks complete, isn't" defect the whole
    // #445/#467/#563 lineage exists to eliminate. The cap is real, so what it
    // drops has to be counted and said out loud, with a pointer at the log
    // that still holds every one of them verbatim.
    let (reg, _d, co, _cw) = setup_mcp();
    let over = ORCH_NOTICE_INBOX_MAX + 3;
    for i in 0..over {
        reg.notify_queue(&co.group, &co.agent_id, true, &format!("[orrerix] notice #{i}"));
    }

    let r = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "list_agents", "arguments": {} })).unwrap();
    let relay = r["content"][1]["text"].as_str().expect("the relay block").to_string();
    assert!(relay.contains("3 earlier notices elided"), "the cut must be named: {relay}");
    assert!(relay.contains("audit.jsonl"),
        "and must point at the record that still has them: {relay}");
    assert!(relay.contains(&format!("[orrerix] notice #{}", over - 1)),
        "the NEWEST notice — the one whose claim is still true — must survive: {relay}");
    assert!(!relay.contains("[orrerix] notice #0"), "the oldest is the one evicted: {relay}");

    // Every notice, including the elided ones, is still in the log.
    let logged = reg
        .audit_log(&co.group)
        .into_iter()
        .filter(|e| e.action == "notice-suppressed")
        .count();
    assert_eq!(logged, over, "the audit is the record; the relay is only the notification");
}

#[test]
fn every_row_of_the_relay_block_is_maskable_by_the_question_gates_notice_rule() {
    // #576/#621: `mask_loomux_notices` drops any row that LEADS with the
    // `[orrerix]` marker once de-framed, and every reader of a live pane is
    // masked with it. This relay never reaches a pane on its own — it rides an
    // MCP tool result, not the pty — but #621's own doc names the path that
    // puts it there anyway: an agent can print marker text itself, and an
    // orchestrator quoting its own relay back into a summary is precisely that
    // case. The rows would then be text ABOUT a question sitting in the tail of
    // the pane most exposed to it, which is #576 exactly.
    //
    // So every row this block emits has to be one the mask can claim. `deframe`
    // strips whitespace and `│ ┃ | * ● • ◆` — and NOT `-`: a `- [orrerix] …` row
    // de-frames to `- [orrerix] …`, leads with the dash, and survives the mask.
    // That is why the bullet here is `•` and why the elision line carries the
    // marker rather than opening with prose.
    let notices = vec![
        queue::queued_notice("o-1", queue::EnqueueReason::Question),
        queue::dropped_notice("o-1", 2, queue::DropReason::QueueFull),
    ];
    let relay = orch_notice_relay_text(&notices, 4).expect("a relay with content");
    assert_eq!(
        mask_loomux_notices(&relay).trim(),
        "",
        "every row of the relay must be maskable, or an orchestrator echoing it into its own \
         pane re-arms the very question gate this text is about — got leftovers from: {relay}"
    );
}

#[test]
fn every_notice_that_can_reach_the_inbox_is_a_single_marker_led_line() {
    // The other half of the maskability invariant (review NB3), and it is a
    // different failure from the one above: that test pins how the BLOCK is
    // rendered, this one pins what may go INTO it. A constructor's wording
    // drifting — a second line, a lost prefix — would break maskability with
    // every call site and the renderer unchanged, so neither test alone covers
    // it.
    //
    // These seven are the complete set of text `notify_queue` can be handed,
    // one per notice constructor across its eight call sites. The list is
    // maintained by hand against those call sites, which is exactly why
    // `OrchNoticeInbox::park`'s `debug_assert` exists as well: an eighth
    // constructor added without touching this list still fails, at the door,
    // in any debug build — and CI's test builds are debug.
    let all = [
        queue::queued_notice("o-1", queue::EnqueueReason::BoxOccupied),
        queue::still_queued_notice("o-1", 2, 30),
        queue::dropped_notice("o-1", 2, queue::DropReason::QueueFull),
        queue::recovered_notice("o-1", 2, 30),
        queue::at_capacity_notice("o-1", queue::QUEUE_MAX_PER_PANE),
        queue::pressure_notice("o-1", queue::QUEUE_NEAR_FULL_AT, queue::QUEUE_MAX_PER_PANE),
        // #590 L2: the eighth call site, on `hold_escalation_step`.
        undeliverable_notice("o-1", 1, 2, 10, UndeliverableCause::PaneMidTurn),
    ];
    for n in &all {
        assert!(
            mask_loomux_notices(n).is_empty(),
            "a notice that can be parked must be ONE marker-led line, or the row it lands on \
             survives the question gate's mask: {n:?}"
        );
    }
    // And a relay built out of every one of them still masks away entirely —
    // the composition, not just the parts.
    let relay = orch_notice_relay_text(&all, 2).expect("a relay with content");
    assert_eq!(mask_loomux_notices(&relay).trim(), "", "leftovers from: {relay}");
}

// ---- #632: the two MULTI-ROW producers that actually ride the pty ----
//
// #624's relay never touches a pane on its own. These two do: the resume
// notice is an in-band delivery pasted into an orchestrator's pane, and a
// coalesced flush IS the prompt. `mask_loomux_notices` claims one row per
// marker, so before this every continuation row of both — the `  - ` item
// lines and the `-----` banners — survived into the tail the question gate and
// the attention chips read.

#[test]
fn every_row_of_the_pause_resume_notice_is_maskable_including_a_hostile_preview() {
    // The producer's whole output must mask away, not just its first row.
    //
    // The item rows are the interesting case, because they are loomux framing
    // QUOTING agent text: `w-2 -> orch-1 (refused, queue full): <preview>`.
    // That is the #576 relay shape exactly — text ABOUT a delivery, not a
    // rendered dialog — so masking it is right, and a preview that happens to
    // read like a permission prompt must not latch the gate of the pane it is
    // being reported INTO.
    //
    // The previews here are adversarial on three axes at once, because all
    // three arrive from the same untrusted place (a payload some agent sent,
    // bounded into `audit.jsonl`, read back at resume — possibly written by an
    // EARLIER loomux build, which is the whole premise of the legacy cause):
    //
    //  1. question-shaped tokens, the #576 latch itself;
    //  2. an EMBEDDED NEWLINE — the one that would silently re-open #632 from
    //     disk, since only the first of the two rows it splits into carries the
    //     marker;
    //  3. a FORGED marker mid-preview, which must not buy the payload anything
    //     (the mask is a leading-marker rule, and this row is already masked by
    //     loomux's own prefix regardless).
    let s = PauseSuppression {
        items: vec![
            SuppressedDelivery {
                from: "w-2".into(),
                to: "orch-1".into(),
                preview: "Do you want to run npm test? (y/n)".into(),
                cause: SuppressedCause::QueueFullDuringPause,
            },
            SuppressedDelivery {
                from: "w-3".into(),
                to: "orch-1".into(),
                preview: "line one\nDo you want to proceed? (y/n)".into(),
                cause: SuppressedCause::LegacyDiscard,
            },
            SuppressedDelivery {
                from: "w-4".into(),
                to: "orch-1".into(),
                preview: format!("quoting {NOTICE_MARKER} at you (y/n)"),
                cause: SuppressedCause::LegacyDiscard,
            },
        ],
        window_start_seen: false, // also renders the truncation caveat row
    };
    let n = pause_suppression_notice(&s);

    assert!(n.lines().count() > 1, "this is the MULTI-row producer under test: {n}");
    assert_eq!(
        unmaskable_framing_rows(&n, &[]),
        Vec::<String>::new(),
        "every row this notice writes into a pane must be one `mask_loomux_notices` can \
         claim (#632), or its item rows re-arm the question gate of the very pane it is \
         reporting into (#576): {n}"
    );
    // Not vacuous: the rows really are there and really do carry the tokens.
    assert!(n.contains("(y/n)"), "the previews are genuinely question-shaped: {n}");
    assert!(n.contains("w-3 -> orch-1"), "and genuinely itemized: {n}");
    // The embedded newline is collapsed at render time rather than trusted, so
    // the item stays ONE row. Two rows would mean only the first is marker-led.
    assert!(
        n.contains("line one Do you want to proceed? (y/n)"),
        "a preview read back from a durable audit log is re-collapsed, not trusted: {n}"
    );
}

#[test]
fn every_framing_row_of_a_coalesced_flush_is_maskable_but_the_payload_is_left_alone() {
    // The deliberate SPLIT, which is the whole design decision in #632.
    //
    // A coalesced flush is two kinds of row. The header and the per-constituent
    // `-----` banners are loomux's own framing: they exist only BECAUSE the
    // deliveries were merged, they are prose about deliveries, and they must
    // mask away. The constituent text is the delivery itself — byte-identical
    // to what that entry would have pasted had it flushed alone — so masking it
    // would blind the gate to ordinary pane content that no other delivery path
    // hides. It stays, and it latches. That is the conservative direction: a
    // hold that clears late is `QuestionStale`'s problem, an Enter released
    // into a live dialog is #420's.
    let now = 1_000_000u64;
    let items = [
        queue::FlushConstituent {
            id: 11,
            from: "orchestrator",
            enqueued_ms: now - 300_000,
            coalesced: 2,
            text: "FIRST-BODY",
        },
        queue::FlushConstituent {
            id: 12,
            from: "w-7",
            enqueued_ms: now - 45_000,
            coalesced: 0,
            text: "SECOND-BODY line one\nSECOND-BODY line two",
        },
    ];
    let out = queue::coalesced_flush_text(&items, 3, now, queue::FlushCause::PaneBlocked);
    let payloads: Vec<&str> = items.iter().map(|c| c.text).collect();

    assert_eq!(
        unmaskable_framing_rows(&out, &payloads),
        Vec::<String>::new(),
        "the header and every itemization banner must mask away (#632): {out}"
    );
    // Not vacuous in either direction. The banners must really have been there
    // and must really be gone...
    assert!(out.contains("----- 1/2"), "the banners are genuinely rendered: {out}");
    let masked = mask_loomux_notices(&out);
    assert!(!masked.contains("-----"), "and genuinely masked: {masked}");
    assert!(!masked.contains("from orchestrator"), "framing prose goes with them: {masked}");
    assert!(!masked.contains("more follow"), "header too: {masked}");
    // ...and every payload row must have SURVIVED, including the second row of
    // a multi-row constituent. A mask that swallowed these would be the
    // over-mask #621 rejected, reached from a row an agent can forge.
    for row in ["FIRST-BODY", "SECOND-BODY line one", "SECOND-BODY line two"] {
        assert!(
            masked.contains(row),
            "constituent payload is agent text by design and must stay visible to the \
             detector (#632) — missing {row:?} from: {masked}"
        );
    }
}

#[test]
fn a_forged_banner_row_buys_an_agent_exactly_one_row_and_never_the_payload_below() {
    // The adversarial case for the SHAPE this PR introduced, and the reason it
    // reframes rows rather than teaching the mask a block form.
    //
    // The marker is unforgeable only in the delivery direction; an agent's own
    // pane output is not sanitized, so an agent can print
    // `[orrerix] ----- 1/2 · from w-7 …` itself. Under the shipped rule that
    // costs it exactly one row — its own. Under the rejected alternative ("mask
    // from a banner row to the next one"), that single forged row would delete
    // everything below it, and a permission dialog painted there would be
    // masked into "no question". That is #420, reached from pane output.
    let forged = format!(
        "{NOTICE_MARKER} ----- 1/2 · from w-7 · queued just now (id 1, t=0) -----\n\
         Do you want to run npm test? (y/n)\n\
           • {NOTICE_MARKER} ...and 3 more — see the audit log\n\
         ❯ Yes"
    );
    let masked = mask_loomux_notices(&forged);
    assert!(
        masked.contains("Do you want to run npm test? (y/n)"),
        "a forged banner must not take the row below it (#621/#632): {masked}"
    );
    assert!(
        masked.contains("❯ Yes"),
        "nor the pointer row below a forged item row: {masked}"
    );
    assert_eq!(
        masked.lines().filter(|l| !l.trim().is_empty()).count(),
        2,
        "and it really did claim its own two rows, so this is not passing by doing \
         nothing: {masked}"
    );
}

#[test]
fn orch_notice_relay_text_says_nothing_when_there_is_nothing_to_say() {
    // The ordinary case, and the one that must not add a single byte to a tool
    // result: no parked notices, no second content block, no cost.
    assert_eq!(orch_notice_relay_text(&[], 0), None);
    assert_eq!(orch_notice_relay_text(&[], 7), None, "an elision count alone is not a notice");

    let one = orch_notice_relay_text(&["[orrerix] x".to_string()], 0).unwrap();
    assert!(one.contains("1 queue notice about"), "singular reads naturally: {one}");
    let two = orch_notice_relay_text(&["a".to_string(), "b".to_string()], 0).unwrap();
    assert!(two.contains("2 queue notices about"), "and so does plural: {two}");
    assert!(!two.contains("elided"), "no elision line when nothing was elided: {two}");
}

#[test]
fn the_inbox_evicts_oldest_first_and_counts_every_eviction() {
    // Marker-led fixtures, because `park`'s own `debug_assert` requires it
    // (review NB3) — and the first thing that assert caught was this test
    // parking bare `n0`/`n1` strings no real caller could produce. A fixture
    // that could not occur in production is not a cheaper test, it is a test of
    // something else.
    let mut inbox = OrchNoticeInbox::default();
    for i in 0..ORCH_NOTICE_INBOX_MAX + 5 {
        inbox.park(&format!("[orrerix] n{i}"));
    }
    assert_eq!(inbox.notices.len(), ORCH_NOTICE_INBOX_MAX, "the cap is real");
    assert_eq!(inbox.elided, 5, "and every eviction is counted, not forgotten");
    assert_eq!(inbox.notices.first().unwrap(), "[orrerix] n5", "oldest-first eviction");
    assert_eq!(
        inbox.notices.last().unwrap(),
        &format!("[orrerix] n{}", ORCH_NOTICE_INBOX_MAX + 4),
        "the newest notice is the one whose claim is still true"
    );
}

#[test]
fn the_notify_queue_hold_classes_gain_a_channel_that_reaches_the_orchestrator_agent() {
    // #572 gave the orchestrator-target case channels that survive it, but
    // both reach a HUMAN AT THE WINDOW. The inbox is the first one in this
    // table that reaches the orchestrator AGENT — the difference between an
    // attended session and an unattended overnight run.
    assert!(
        HoldChannel::OrchestratorInbox.survives_orchestrator_target(),
        "the whole point is that it is not a delivery"
    );

    // Listed on exactly the classifications whose notice goes through
    // `notify_queue` — the function that parks. Anything else would make this
    // table say something untrue.
    for class in [
        HoldClass::PrePasteRecheckExhausted,
        HoldClass::PreEnterBoxOccupied,
        HoldClass::QueueNearFull,
        HoldClass::QueueFull,
    ] {
        let ch = hold_channels(class);
        assert!(
            ch.contains(&HoldChannel::OrchestratorNotice)
                && ch.contains(&HoldChannel::OrchestratorInbox),
            "{class:?} notifies through `notify_queue`, so its suppressed notice is parked: {ch:?}"
        );
    }

    // `GroupPaused`'s notice is `announce_pause_suppression` at resume, a
    // different call on a different path that `notify_queue` never sees.
    let paused = hold_channels(HoldClass::GroupPaused);
    assert!(
        paused.contains(&HoldChannel::OrchestratorNotice)
            && !paused.contains(&HoldChannel::OrchestratorInbox),
        "a pause notice is not parked, so the table must not claim it is: {paused:?}"
    );

    // And the completeness invariant #563 established still holds for every
    // classification, unchanged by the new channel.
    for class in HoldClass::ALL {
        assert!(
            hold_channels(*class).iter().any(|c| c.survives_orchestrator_target()),
            "{class:?} must still reach someone an orchestrator target does not suppress"
        );
    }
}

// ---------- #560: the escalation clock is the PANE's hold episode ----------
//
// #532 measured its ten-minute escalation from the front queue ENTRY's
// `enqueued_ms` while keying its one-shot on `pty_id`. Those two do not describe
// the same thing, and both reported symptoms are that mismatch:
//
//  1. a writable poll dropped the one-shot but not the clock, so the very next
//     held poll found the bound already elapsed and re-badged — audit churn on
//     exactly the pane the feature exists for (a human typing with a delivery
//     queued behind them toggles occupancy on every Enter);
//  2. `enqueue_stranded_front` pushes a `StrandedSubmit` marker at the FRONT
//     carrying a fresh `now_ms()`, so a pasted-but-unsubmitted delivery handed
//     the bound a brand-new clock on a pane that had been blocked continuously.
//
// Each test below drives one of them through the real code path — the registry
// methods `run_queue_drainer` actually calls, since the drainer itself needs an
// `AppHandle` no headless test can build.

/// How many audit lines of `action` this group has written. Distinct from the
/// substring-counting `audit_count` above on purpose: a churn assertion has to
/// match the `action` FIELD exactly, or a detail value that happens to contain
/// the same text would inflate it.
pub(crate) fn hold_audit_count(reg: &OrchRegistry, group: &GroupId, action: &str) -> usize {
    reg.audit_log(group).iter().filter(|e| e.action == action).count()
}

#[test]
fn only_an_accepted_delivery_ends_a_hold_episode() {
    // The lifecycle rule itself, which is the whole of symptom 1's fix. Stated
    // as a closed set so that a future observation cannot be added without
    // deciding, in one place, what it does to the clock.
    assert!(
        ends_hold_episode(HoldObservation::Delivered),
        "a delivery LANDING is the pane proving it can accept a write — that, and nothing \
         weaker, ends the episode"
    );
    assert!(
        !ends_hold_episode(HoldObservation::WritablePoll),
        "a writable POLL is provisional: the drainer goes straight on to deliver_now, which \
         re-reads both gates and can still abort. Ending the episode here is #560 symptom 1 — \
         and if it also restarted the clock, a pane flickering faster than the bound would \
         never badge AT ALL, which is the #532 bug class over again"
    );
    assert!(!ends_hold_episode(HoldObservation::HeldPoll));
    assert!(!ends_hold_episode(HoldObservation::Aborted));
    assert!(
        ends_hold_episode(HoldObservation::Retired),
        "#813: retiring a StrandedSubmit marker also ends the episode — whatever loomux was \
         holding this pane for is over. It is a SEPARATE observation from `Delivered` because \
         the pane accepted no write, and reusing `Delivered` would put that false claim into \
         the audit trail"
    );

    // Both ways of failing to deliver start a clock. `Aborted` matters on its
    // own: a hold that lives entirely inside `deliver_now` (every poll reads
    // writable, every attempt then aborts) would otherwise never open an
    // episode, and that pane is exactly as stuck as one that fails at the poll.
    assert!(opens_hold_episode(HoldObservation::HeldPoll));
    assert!(opens_hold_episode(HoldObservation::Aborted));
    assert!(!opens_hold_episode(HoldObservation::WritablePoll));
    assert!(
        !opens_hold_episode(HoldObservation::Delivered),
        "and the one observation that ends an episode must never also open one"
    );
    assert!(!opens_hold_episode(HoldObservation::Retired), "same for #813's second ender");
}

#[test]
fn retiring_a_marker_ends_the_episode_without_clearing_the_badge() {
    // #813 review F2. `note_hold` drops a badged pane's stranded note when an
    // episode ENDS, and #813 added a second ender — so `Retired` inherited a
    // badge clear that its own retire arm had already decided against, taking a
    // `TextGone`/`NothingStranded` badge down behind that decision's back. The
    // two paths disagreed, and the code was the one that was wrong.
    //
    // The rule: a DELIVERY licenses the clear here, because the pane proved it
    // can accept a write. A retirement proves nothing of the sort — it is the
    // whole reason `HoldObservation::Retired` is not `Delivered` — so the badge
    // decision belongs to the retire arm alone (`resolves_the_pane`).
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 8134u32;
    let bound = QUESTION_HOLD_STALE_AFTER.as_millis() as u64;
    let t0 = 1_000_000u64;
    // #820: the trailing `None` is the question-gate witness, which this test
    // has nothing to say about — its admission is `HoldBoxOccupied`, so no
    // question ever matched. Supplying it changes nothing about #813's
    // retirement semantics, which are what this test is pinning.
    let step =
        |admission, now| reg.hold_escalation_step(&g.id, &w.id, pty, admission, 1, now, bound, None, None);

    // Drive a real hold episode to its escalation, so a badge is genuinely up
    // and genuinely owned by THIS episode — the only case `note_hold` clears.
    step(WriteAdmission::HoldBoxOccupied, t0);
    assert_eq!(
        step(WriteAdmission::HoldBoxOccupied, t0 + bound),
        HeldEscalation::Badge(StrandedBlocker::HumanInput)
    );
    assert!(reg.hold_episode_badged(pty), "precondition: this episode raised the badge");
    assert!(reg.stranded_note(&w.id).is_some(), "precondition: the chip is up");

    reg.note_hold(&g.id, &w.id, pty, HoldObservation::Retired, t0 + bound + 6_000);

    assert_eq!(
        reg.hold_episode_since(pty),
        None,
        "the episode still ENDS — loomux is no longer holding this pane on that entry, so a \
         later hold clocks and badges afresh"
    );
    assert!(
        reg.stranded_note(&w.id).is_some(),
        "but the chip stays: retiring a marker is not the pane accepting a write, and the \
         retire arm — not this function — decides per reason whether the human is done here"
    );
    assert_eq!(
        hold_audit_count(&reg, &g.id, "stranded-cleared"), 0,
        "and nothing was audited as cleared, because nothing was"
    );
}

#[test]
fn a_writable_poll_never_re_badges_a_pane_that_was_already_escalated() {
    // Symptom 1, end to end, through the step the drainer calls.
    //
    // The pre-#560 sequence: badge at ten minutes → the gates clear for a single
    // poll → `Clear` drops the one-shot → the next held poll finds the bound
    // (measured from an entry timestamp that did not move) already elapsed and
    // badges again. One `stranded-attention` + one `stranded-cleared` per
    // flicker, on a pane that never stopped being stuck.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5601u32;
    let bound = QUESTION_HOLD_STALE_AFTER.as_millis() as u64;
    let t0 = 1_000_000u64;
    let step = |admission, now| reg.hold_escalation_step(&g.id, &w.id, pty, admission, 1, now, bound, None, None);

    assert_eq!(
        step(WriteAdmission::HoldBoxOccupied, t0),
        HeldEscalation::Chip(HeldReason::BoxOccupied),
        "the first held poll opens the episode and chips (#563)"
    );
    assert_eq!(reg.hold_episode_since(pty), Some(t0), "the clock is the PANE's, stamped once");
    assert_eq!(step(WriteAdmission::HoldBoxOccupied, t0 + 2_000), HeldEscalation::Chip(HeldReason::BoxOccupied));
    assert_eq!(
        step(WriteAdmission::HoldBoxOccupied, t0 + bound),
        HeldEscalation::Badge(StrandedBlocker::HumanInput),
        "and the escalation still fires exactly where #532 put it"
    );
    assert!(reg.hold_episode_badged(pty));

    // THE FLICKER — the human presses Enter, the box empties for one poll.
    assert_eq!(step(WriteAdmission::Go, t0 + bound + 2_000), HeldEscalation::Clear);
    assert_eq!(
        reg.hold_episode_since(pty),
        Some(t0),
        "a writable poll does NOT restart the clock — it is a reading, not a delivery"
    );
    assert!(
        reg.hold_episode_badged(pty),
        "and it does not disarm the one-shot either; the clock and the one-shot are now the \
         SAME record, so they cannot come apart"
    );
    assert_eq!(
        step(WriteAdmission::HoldBoxOccupied, t0 + bound + 4_000),
        HeldEscalation::None,
        "so the next held poll has nothing new to say — no second badge"
    );

    assert_eq!(
        hold_audit_count(&reg, &g.id, "stranded-attention"), 1,
        "one escalation per hold episode, across the flicker — the churn is the defect"
    );
    assert_eq!(
        hold_audit_count(&reg, &g.id, "stranded-cleared"), 0,
        "and nothing was cleared: the pane never accepted a delivery"
    );
    assert_eq!(
        hold_audit_count(&reg, &g.id, "delivery-held-in-queue"), 1,
        "the once-per-episode hold line churned the same way — it was keyed on the pane-header \
         chip, which is LIVE state that comes down on every writable poll"
    );

    // Only the delivery landing ends it — and then a LATER hold is a new
    // episode, with its own clock and its own badge.
    reg.note_hold(&g.id, &w.id, pty, HoldObservation::Delivered, t0 + bound + 6_000);
    assert_eq!(reg.hold_episode_since(pty), None);
    assert!(!reg.hold_episode_badged(pty));
    assert_eq!(
        hold_audit_count(&reg, &g.id, "stranded-cleared"), 1,
        "the badge comes down where the pane PROVED it could accept a write"
    );

    let t1 = t0 + bound + 8_000;
    assert_eq!(step(WriteAdmission::HoldQuestion, t1), HeldEscalation::Chip(HeldReason::InteractiveQuestion));
    assert_eq!(reg.hold_episode_since(pty), Some(t1), "a later hold clocks afresh, never from t0");
    assert_eq!(
        step(WriteAdmission::HoldQuestion, t1 + bound),
        HeldEscalation::Badge(StrandedBlocker::QuestionStale),
        "and badges afresh — suppressing the SECOND escalation would be the mirror-image bug"
    );
    assert_eq!(hold_audit_count(&reg, &g.id, "delivery-held-in-queue"), 2, "one line per episode, two episodes");
}

#[test]
fn a_stranded_front_marker_never_pushes_the_escalation_clock_forward() {
    // Symptom 2, against the real `enqueue_stranded_front`.
    //
    // A delivery that pastes and then has its Enter withheld pops its batch and
    // pushes a `StrandedSubmit` marker at the FRONT, stamped `now_ms()`. Read
    // the bound off the front entry and the ten-minute clock restarts from that
    // moment — on a pane whose whole problem is that it has been blocked the
    // entire time.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5602u32;
    let bound = QUESTION_HOLD_STALE_AFTER.as_millis() as u64;
    let t0 = 2_000_000u64;

    // The pane is held; the episode opens. Then the paste lands and the Enter is
    // withheld: the text entry is replaced by a marker minted right now.
    reg.hold_escalation_step(&g.id, &w.id, pty, WriteAdmission::HoldQuestion, 1, t0, bound, None, None);
    reg.enqueue_text(&g.id, &w.id, "orrerix", "the brief", pty, queue::EnqueueReason::Arrival).unwrap();
    reg.enqueue_stranded_front(&g.id, &w.id, "orrerix", pty, queue::EnqueueReason::Question)
        .unwrap();
    reg.note_hold(&g.id, &w.id, pty, HoldObservation::Aborted, t0 + 1_000);

    let front = reg.queue_snapshot(pty).remove(0);
    assert_eq!(front.payload, queue::QueuedPayload::StrandedSubmit, "the marker is the front entry");
    assert!(
        front.enqueued_ms > t0,
        "and it really is younger than the hold — otherwise this test proves nothing about a \
         clock that could be pushed (front.enqueued_ms = {}, hold began {t0})",
        front.enqueued_ms
    );
    assert_eq!(
        reg.hold_episode_since(pty),
        Some(t0),
        "the pane's clock is untouched by the marker: an aborted attempt is not a recovery"
    );

    // At t0 + bound the pane has been blocked for the full ten minutes, so the
    // escalation is due. Read off the marker instead and it is not — that
    // difference IS the defect.
    let now = t0 + bound;
    assert_eq!(
        held_escalation(WriteAdmission::HoldQuestion, reg.hold_episode_since(pty).unwrap(), now, bound, false),
        HeldEscalation::Badge(StrandedBlocker::QuestionStale),
        "the episode clock escalates on time"
    );
    assert_eq!(
        held_escalation(WriteAdmission::HoldQuestion, front.enqueued_ms, now, bound, false),
        HeldEscalation::Chip(HeldReason::InteractiveQuestion),
        "while the marker's own stamp would still be inside the bound — the badge deferred by \
         the very event that proves the pane is stuck"
    );
    assert_eq!(
        reg.hold_escalation_step(&g.id, &w.id, pty, WriteAdmission::HoldQuestion, 2, now, bound, None, None),
        HeldEscalation::Badge(StrandedBlocker::QuestionStale),
        "and the step the drainer actually calls takes the episode clock, not the front entry"
    );
}

#[test]
fn the_still_queued_clock_is_not_restarted_by_a_stranded_marker() {
    // The same defect, on the 30-minute notice. It was measured from the FRONT
    // entry's stamp, and `enqueue_stranded_front` makes the front the YOUNGEST
    // entry in the queue — so a pasted-but-unsubmitted delivery pushed the
    // notice out by the very event that proves the pane is stuck.
    //
    // An integration test rather than one of `queue.rs`'s own inline unit tests
    // purely so it shares a target with the four above: `cargo test` stops at
    // the first failing target, and a red-evidence run that dies in the lib
    // suite never reaches `tests/orchestration.rs` at all.
    let threshold_ms = queue::QUEUE_STILL_QUEUED_NOTICE_AFTER.as_millis() as u64;
    let held_since = 1_000_000u64;
    let now = held_since + threshold_ms;
    // The marker was minted one second ago; it is the only entry left, because
    // the batch it stood for was popped when the paste landed.
    let marker_ms = now - 1_000;

    assert!(
        !queue::should_fire_still_queued_notice(marker_ms, now, false),
        "reading the marker's own stamp, the pane looks one second old — this is the defect"
    );
    assert!(
        queue::should_fire_still_queued_notice(queue::undelivered_since(marker_ms, Some(held_since)), now, false),
        "the pane has had undelivered work for the full threshold, and the hold episode is the \
         only reading that still says so"
    );

    // The other direction, which is why the entry term is kept: a delivery
    // landing ends the episode, but entries the flush could not fit stay queued
    // and must keep their own age.
    let old_entry = now - threshold_ms;
    assert_eq!(queue::undelivered_since(old_entry, None), old_entry);
    assert!(queue::should_fire_still_queued_notice(queue::undelivered_since(old_entry, None), now, false));
    // And with both, the EARLIER wins in either arrangement.
    assert_eq!(queue::undelivered_since(old_entry, Some(now)), old_entry);
    assert_eq!(queue::undelivered_since(now, Some(old_entry)), old_entry);
}

#[test]
fn a_pane_whose_queue_goes_away_ends_its_hold_episode() {
    // The other end of the lifecycle: the queue emptying, not a delivery
    // landing. Exercised here through `drop_queue` (the standalone path); the
    // drainer's own exits do the same inside `commit_exit`'s generation-guarded
    // step, which cannot go through `note_hold` because that removal has to be
    // atomic with the emptiness check. A record left behind would hand the NEXT
    // drainer on this pty a clock from a queue that no longer exists.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5603u32;
    let bound = QUESTION_HOLD_STALE_AFTER.as_millis() as u64;
    let t0 = 3_000_000u64;

    reg.hold_escalation_step(&g.id, &w.id, pty, WriteAdmission::HoldBoxOccupied, 1, t0, bound, None, None);
    reg.hold_escalation_step(&g.id, &w.id, pty, WriteAdmission::HoldBoxOccupied, 1, t0 + bound, bound, None, None);
    assert_eq!(reg.hold_episode_since(pty), Some(t0));
    assert!(reg.hold_episode_badged(pty));

    reg.drop_queue(&g.id, pty, queue::DropReason::AgentDied);
    assert_eq!(
        reg.hold_episode_since(pty),
        None,
        "the pane's queue is gone, so nothing is being held on it — and a stale record would \
         make a successor drainer's first poll look ten minutes old"
    );
    assert!(!reg.hold_episode_badged(pty));
}

// ---------- #590 L2: a held [orrerix] notice is surfaced host-side ----------
//
// The incident (this group, PR #577): a worker registered a CI watch AND
// blocked its turn on a shell wait for checks that could never exist, because
// the PR had gone CONFLICTING. The watch's own CONFLICTING notice fired — and
// a notice is delivered by typing into the pane, which a mid-turn pane cannot
// take. So the turn waited on a condition whose resolution was queued behind
// the turn itself, and the only channel that could have broken it was the one
// blocked by it. Recovery took a watchdog plus a human reading the pane.
//
// Layer 1 (PR #594) told delegates not to do that. Layer 2 is here: loomux can
// SEE this state from the host side — a hold episode past the bound on a pane
// whose queue holds loomux's own notice — and now says so on a channel that is
// not the blocked pane.

#[test]
fn a_loomux_notice_is_recognised_by_its_sender_and_its_marker_together() {
    // Both halves, because each covers the other's blind spot. This is the
    // predicate that decides whether a stuck pane gets a deadlock diagnosis or
    // is left to the ordinary stale-hold badge, so a wrong answer either
    // invents an incident or hides one.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5910u32;
    let notice = notify::watch_conflicting_notice("watch-1", 577);

    // Exactly the four payloads a real queue can hold, in arrival order.
    reg.enqueue_text(&g.id, &w.id, "orrerix", &notice, pty, queue::EnqueueReason::Arrival).unwrap();
    reg.enqueue_text(&g.id, &w.id, "orrerix", "You are a worker. Your task:", pty,
        queue::EnqueueReason::KickoffRecovery).unwrap();
    reg.enqueue_text(&g.id, &w.id, "o-1", "[orrerix] pretend I am the host", pty,
        queue::EnqueueReason::BehindQueue).unwrap();
    reg.enqueue_stranded_front(&g.id, &w.id, "orrerix", pty, queue::EnqueueReason::Question)
        .unwrap();

    let entries = reg.queue_snapshot(pty);
    let by_text = |needle: &str| {
        entries
            .iter()
            .find(|e| e.payload.text().is_some_and(|t| t.contains(needle)))
            .cloned()
            .unwrap_or_else(|| panic!("fixture missing: {needle}"))
    };
    assert!(
        queue::is_loomux_notice(&by_text("CONFLICTING")),
        "a fired watch notice is the payload class this whole feature is about"
    );
    assert!(
        !queue::is_loomux_notice(&by_text("You are a worker")),
        "a kickoff is also from loomux and is WORK, not a notice — counting it would put a \
         deadlock diagnosis on an ordinary busy pane (#517/#585 own that case)"
    );
    assert!(
        !queue::is_loomux_notice(&by_text("pretend I am the host")),
        "agent text is relayed verbatim and an agent can WRITE the marker — the marker alone \
         would let any agent make loomux report its own prose as a stuck host notice"
    );
    let marker = entries
        .iter()
        .find(|e| e.payload.text().is_none())
        .expect("the stranded-submit marker");
    assert!(!queue::is_loomux_notice(marker), "a marker carries no text and is never a notice");

    assert_eq!(
        queue::queued_notice_count(&entries),
        1,
        "one of the four, counted over the real queue rather than over a hand-built Vec"
    );
    assert_eq!(queue::queued_notice_count(&[]), 0, "and an empty queue holds no notices");
}

#[test]
fn a_dialog_on_screen_outranks_the_absence_of_keystrokes() {
    // The precedence, which is the one ordering a future edit must not swap.
    // `last_user_input_ms` is stamped only for keystroke-like input (#496), and
    // its stated tradeoff is that arrow keys and menu navigation classify
    // Neutral with a zero occupancy delta and never stamp it. So a human
    // sitting in a permission dialog looks EXACTLY like a pane with no human
    // at all — and reading the dialog first is what keeps that from being
    // reported as a mid-turn stall.
    let started = 1_000_000u64;
    assert_eq!(
        undeliverable_cause(Some(HeldReason::InteractiveQuestion), Some(0), started),
        UndeliverableCause::QuestionOnScreen,
        "a rendered dialog is an observation; the keystroke clock is an inference from absence"
    );
    assert_eq!(
        undeliverable_cause(Some(HeldReason::InteractiveQuestion), Some(started + 5_000), started),
        UndeliverableCause::QuestionOnScreen,
        "and a human answering it does not turn it into the box-occupied case"
    );

    // The box-occupied cases, split on the one question that matters: has a
    // human touched this pane at any point in the whole time it has been
    // refusing our delivery?
    assert_eq!(
        undeliverable_cause(Some(HeldReason::BoxOccupied), Some(started - 1), started),
        UndeliverableCause::PaneMidTurn,
        "a stamp from BEFORE the episode is a fact about an earlier session at this pane — it \
         says nothing about what is in the box now"
    );
    assert_eq!(
        undeliverable_cause(Some(HeldReason::BoxOccupied), Some(0), started),
        UndeliverableCause::PaneMidTurn,
        "and a pane no human has ever typed into reads 0, not None"
    );
    assert_eq!(
        undeliverable_cause(Some(HeldReason::BoxOccupied), Some(started), started),
        UndeliverableCause::HumanTyping,
        "a keystroke AT the episode boundary counts as during it — the safe direction, since \
         the cost of being wrong here is a softer notice, not a missed one"
    );
    assert_eq!(
        undeliverable_cause(Some(HeldReason::Typing), Some(started + 60_000), started),
        UndeliverableCause::HumanTyping,
        "#43's typing gate lands in the same cell as #111's box gate — both mean the box"
    );

    // And the two ways to have nothing to classify from.
    assert_eq!(
        undeliverable_cause(Some(HeldReason::BoxOccupied), None, started),
        UndeliverableCause::Unknown,
        "a closed pty has no stamp — and a notice must never assert a cause it did not observe"
    );
    assert_eq!(
        undeliverable_cause(None, Some(0), started),
        UndeliverableCause::Unknown,
        "no gate, no diagnosis"
    );
}

#[test]
fn the_undeliverable_notice_is_one_marker_led_line_that_names_the_pane_and_the_cause() {
    // Maskability first (#621/#624): this text can be parked in the
    // orchestrator inbox, and `OrchNoticeInbox::park`'s own debug_assert
    // enforces the same invariant at the door. A second row here would ride an
    // orchestrator's pane unmasked and could re-arm the question gate the
    // notice is about.
    // Asserted through #632's `unmaskable_framing_rows` rather than through a
    // bare `mask_loomux_notices(..).is_empty()`, so this notice sits under the
    // SAME expression of the rule as the multi-row producers #638 brought under
    // it. The two are equivalent for a one-row notice; what the shared helper
    // buys is that a later sweep over "every loomux-authored row is maskable"
    // finds this one where it expects to, and that a regression names the rows
    // that survived instead of only asserting that something did.
    for cause in [
        UndeliverableCause::PaneMidTurn,
        UndeliverableCause::HumanTyping,
        UndeliverableCause::QuestionOnScreen,
        UndeliverableCause::Unknown,
    ] {
        let n = undeliverable_notice("w-8", 1, 3, 10, cause);
        assert_eq!(
            unmaskable_framing_rows(&n, &[]),
            Vec::<String>::new(),
            "every row of every cause must be one `mask_loomux_notices` can claim: {n:?}"
        );
        // And the distinct claim the helper does NOT make: that there is only
        // ever one row to begin with. `park`'s `debug_assert` enforces it at the
        // door, and this is the constructor-side statement of the same thing.
        assert_eq!(n.lines().count(), 1, "a parked notice must be a single line: {n:?}");
    }

    let n = undeliverable_notice("w-8", 1, 3, 10, UndeliverableCause::PaneMidTurn);
    assert!(n.contains("notice undeliverable 10 min"), "the headline the issue asked for: {n}");
    assert!(n.contains("w-8"), "which pane: {n}");
    assert!(n.contains("1 of 3"), "how much of the backlog is loomux's own: {n}");
    assert!(n.contains("pane mid-turn"), "the diagnosis: {n}");
    assert!(
        n.contains("no human keystroke since the hold began"),
        "and the evidence behind it, so a reader can check the claim rather than trust it: {n}"
    );
    assert!(
        n.contains("do not wait on it"),
        "the whole point is that this pane will not clear itself: {n}"
    );

    let human = undeliverable_notice("w-8", 1, 1, 10, UndeliverableCause::HumanTyping);
    assert!(
        human.contains("will clear when they submit it"),
        "a human mid-line is NOT a stall and must not read like one: {human}"
    );
    let many = undeliverable_notice("w-8", 2, 4, 10, UndeliverableCause::PaneMidTurn);
    assert!(many.contains("2 of 4 deliveries"), "plural reads naturally too: {many}");
    assert!(many.contains("are loomux's own notices"), "{many}");
}

#[test]
fn a_pane_holding_loomuxs_own_notice_past_the_bound_tells_the_orchestrator() {
    // The wiring, through the exact step the drainer calls. Before #590 L2 this
    // pane produced a chip and a badge — both of which reach a HUMAN AT THE
    // WINDOW — and nothing at all for the orchestrator agent until
    // `still_queued_notice` at THIRTY minutes. The live incident was diagnosed
    // by hand at ~20, so on this timeline nothing would ever have fired.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let _orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5911u32;
    let bound = QUESTION_HOLD_STALE_AFTER.as_millis() as u64;
    let t0 = 4_000_000u64;

    reg.enqueue_text(&g.id, &w.id, "orrerix", &notify::watch_conflicting_notice("watch-1", 577),
        pty, queue::EnqueueReason::Arrival).unwrap();
    reg.enqueue_text(&g.id, &w.id, "o-1", "any word yet?", pty, queue::EnqueueReason::BehindQueue)
        .unwrap();

    // A pane no human has touched since long before the hold opened: the #590
    // shape, where the box belongs to the CLI's own turn and not to a person.
    let step = |now| {
        reg.hold_escalation_step(&g.id, &w.id, pty, WriteAdmission::HoldBoxOccupied, 2, now, bound,
            Some(t0 - 60_000), None)
    };

    assert_eq!(step(t0), HeldEscalation::Chip(HeldReason::BoxOccupied), "the episode opens");
    assert!(
        audit_entries(&reg, &g.id, "notice-undeliverable").is_empty(),
        "and says nothing yet — inside the bound this is an ordinary held pane, and a notice \
         per poll would be the audit flood every one-shot in this file exists to prevent"
    );

    assert_eq!(step(t0 + bound), HeldEscalation::Badge(StrandedBlocker::HumanInput));
    let lines = audit_entries(&reg, &g.id, "notice-undeliverable");
    assert_eq!(lines.len(), 1, "the bound elapses and loomux says so, once");
    assert_eq!(lines[0]["detail"]["to"], w.id);
    assert_eq!(lines[0]["detail"]["cause"], "pane-mid-turn");
    assert_eq!(lines[0]["detail"]["notices"], 1, "one of the two queued entries is ours");
    assert_eq!(lines[0]["detail"]["depth"], 2);
    assert_eq!(lines[0]["detail"]["minutes"], bound / 60_000);
    assert_eq!(
        lines[0]["detail"]["blocked_on"], "box-occupied",
        "the gate the drainer actually stopped on, so the diagnosis and the reading agree"
    );

    // ONE per episode, not one per poll — and the pane stays stuck, which is
    // precisely when a re-notifying implementation would flood.
    step(t0 + bound + 2_000);
    step(t0 + bound + 4_000);
    assert_eq!(
        audit_entries(&reg, &g.id, "notice-undeliverable").len(),
        1,
        "a stuck pane is polled every couple of seconds for as long as it is stuck"
    );

    // A delivery landing ends the episode; a LATER hold is a new one, and gets
    // its own notice. Suppressing the second would be the mirror-image bug —
    // the pane went stuck again and nobody would be told.
    reg.note_hold(&g.id, &w.id, pty, HoldObservation::Delivered, t0 + bound + 6_000);
    let t1 = t0 + bound + 8_000;
    reg.hold_escalation_step(&g.id, &w.id, pty, WriteAdmission::HoldBoxOccupied, 2, t1, bound,
        Some(t0 - 60_000), None);
    reg.hold_escalation_step(&g.id, &w.id, pty, WriteAdmission::HoldBoxOccupied, 2, t1 + bound,
        bound, Some(t1 + 1_000), None);
    let lines = audit_entries(&reg, &g.id, "notice-undeliverable");
    assert_eq!(lines.len(), 2, "a fresh episode is a fresh incident");
    assert_eq!(
        lines[1]["detail"]["cause"], "human-typing",
        "and the cause is re-read, not remembered: this time a human HAS typed since the hold \
         began, which is not a stall and must not be reported as one"
    );
}

#[test]
fn the_notice_still_fires_when_another_mechanism_already_owns_the_badge() {
    // The one-shot cannot be the badge's. `hold_escalation_step` declines to
    // RAISE over another mechanism's badge (#532 rev-12 NB5) and therefore
    // leaves `HoldEpisode::badged` false, so `held_escalation` keeps returning
    // `Badge` on every later poll. Keying the orchestrator notice on the badge
    // would then either fire it every two seconds or — if nested inside the
    // decline — never fire it at all, on a pane that is doubly stuck.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let _orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5912u32;
    let bound = QUESTION_HOLD_STALE_AFTER.as_millis() as u64;
    let t0 = 5_000_000u64;

    reg.enqueue_text(&g.id, &w.id, "orrerix", &notify::watch_conflicting_notice("watch-2", 577),
        pty, queue::EnqueueReason::Arrival).unwrap();
    reg.mark_stranded(&g.id, &w.id, Some(StrandedBlocker::QueueNearFull));

    let step = |now| {
        reg.hold_escalation_step(&g.id, &w.id, pty, WriteAdmission::HoldBoxOccupied, 1, now, bound,
            Some(0), None)
    };
    step(t0);
    step(t0 + bound);
    assert!(
        !reg.hold_episode_badged(pty),
        "the badge was declined, exactly as #532 intends — someone else's is already up"
    );
    assert_eq!(
        audit_entries(&reg, &g.id, "notice-undeliverable").len(),
        1,
        "and the orchestrator is told anyway: the badge is the HUMAN's channel and this is not"
    );
    step(t0 + bound + 2_000);
    step(t0 + bound + 4_000);
    assert_eq!(
        audit_entries(&reg, &g.id, "notice-undeliverable").len(),
        1,
        "still once per episode, on the very path where the badge one-shot cannot help"
    );
}

#[test]
fn a_held_kickoff_is_not_reported_as_an_undeliverable_notice() {
    // The gate, and it is what keeps this from being a second, noisier copy of
    // `QueueStaleEscalation`. A kickoff is `from: "loomux"` too; it is work
    // waiting on a busy pane, with its own recovery (#517/#585), and the pane
    // holding it is not deadlocked against loomux by construction the way a
    // pane holding a notice is.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let _orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5913u32;
    let bound = QUESTION_HOLD_STALE_AFTER.as_millis() as u64;
    let t0 = 6_000_000u64;

    reg.enqueue_text(&g.id, &w.id, "orrerix", "You are a worker. Your task: fix #1", pty,
        queue::EnqueueReason::KickoffRecovery).unwrap();
    reg.hold_escalation_step(&g.id, &w.id, pty, WriteAdmission::HoldBoxOccupied, 1, t0, bound,
        Some(0), None);
    let escalation = reg.hold_escalation_step(&g.id, &w.id, pty, WriteAdmission::HoldBoxOccupied, 1,
        t0 + bound, bound, Some(0), None);

    assert_eq!(
        escalation,
        HeldEscalation::Badge(StrandedBlocker::HumanInput),
        "the ordinary stale-hold escalation is untouched — this pane IS stuck and the human is \
         still told about it"
    );
    assert!(
        audit_entries(&reg, &g.id, "notice-undeliverable").is_empty(),
        "but nothing loomux said is trapped here, so there is no deadlock to diagnose"
    );
}

#[test]
fn a_bound_crossed_with_nothing_queued_does_not_burn_the_report() {
    // rev-128's blocking finding, and the scenario is one step from the filed
    // incident: the orchestrator sends a worker a follow-up while it is
    // mid-turn, the delivery is held, and the episode opens on WORK. The bound
    // elapses with no notice queued — correctly silent. THEN the worker's own
    // CI watch fires and queues `watch_conflicting_notice` behind that entry:
    // #590's exact payload, on a pane that cannot take it. That must be
    // reported, and before this fix it never was, for as long as the pane
    // stayed held (unbounded).
    //
    // **Deliberately run in the configuration where the badge SUCCEEDS**, which
    // is the common one and the one the fix has to cover. Nothing else owns this
    // agent's badge, so `mark_stranded` applies, `HoldEpisode::badged` goes true
    // — and `held_escalation` then returns `None`, not `Badge`, on every later
    // poll. So a fix that only reorders the one-shot inside the `Badge` arm
    // would still be silent here: there is no second `Badge` poll to try again.
    // That is why the report is evaluated on the BOUND, not on the verdict, and
    // why this test asserts the verdict is `None` at the moment it fires.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let _orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5916u32;
    let bound = QUESTION_HOLD_STALE_AFTER.as_millis() as u64;
    let t0 = 9_000_000u64;
    let step = |now| {
        reg.hold_escalation_step(&g.id, &w.id, pty, WriteAdmission::HoldBoxOccupied, 1, now, bound,
            Some(0), None)
    };

    // An ordinary follow-up from the orchestrator — work, not a notice.
    reg.enqueue_text(&g.id, &w.id, "o-1", "any word on the rebase?", pty,
        queue::EnqueueReason::Arrival).unwrap();

    assert_eq!(step(t0), HeldEscalation::Chip(HeldReason::BoxOccupied), "the episode opens");
    assert_eq!(
        step(t0 + bound),
        HeldEscalation::Badge(StrandedBlocker::HumanInput),
        "the bound elapses and the human-facing escalation fires as it always did"
    );
    assert!(
        reg.hold_episode_badged(pty),
        "and the badge was RAISED, not declined — this is the common configuration, and the one \
         in which no further poll ever returns Badge again"
    );
    assert!(
        audit_entries(&reg, &g.id, "notice-undeliverable").is_empty(),
        "nothing loomux said is queued here, so there is correctly nothing to report yet — the \
         defect is what this silence used to COST"
    );

    // The watch fires. Its notice lands behind the work already waiting, on a
    // pane that is still held — #590, exactly.
    reg.enqueue_text(&g.id, &w.id, "orrerix", &notify::watch_conflicting_notice("watch-5", 577),
        pty, queue::EnqueueReason::BehindQueue).unwrap();

    assert_eq!(
        step(t0 + bound + 2_000),
        HeldEscalation::None,
        "the badge is already up, so the escalation verdict is None — the report must NOT be \
         riding on the Badge arm, or it can never fire from here"
    );
    let lines = audit_entries(&reg, &g.id, "notice-undeliverable");
    assert_eq!(
        lines.len(),
        1,
        "the report survived the poll that had nothing to say, and fires on the poll that does"
    );
    assert_eq!(lines[0]["detail"]["notices"], 1, "one of the two queued entries is loomux's");
    assert_eq!(lines[0]["detail"]["depth"], 2);
    assert_eq!(lines[0]["detail"]["cause"], "pane-mid-turn");

    // And it is still at-most-once per episode: the one-shot is claimed by the
    // poll that reports, not by the poll that merely looked.
    step(t0 + bound + 4_000);
    step(t0 + bound + 6_000);
    assert_eq!(
        audit_entries(&reg, &g.id, "notice-undeliverable").len(),
        1,
        "a stuck pane is polled every couple of seconds — claiming on report rather than on the \
         bound must not turn into a report per poll"
    );
}

#[test]
fn an_undeliverable_notice_is_never_reported_as_a_front_door_refusal() {
    // The #579/#630 seam, pinned rather than argued. #630's refused list and
    // this classification are about opposite halves of one distinction: a
    // refusal is `enqueue_text`'s `RejectFull` arm, which returns before
    // `queue_seq.fetch_add`, so it has NO id and no queue entry; this fires
    // only for a payload that IS queued, holds an id, and cannot land. Showing
    // one event in both would be a duplicate-delivery generator in a tool whose
    // documented response is "re-send what still applies" — the same failure
    // #630's own `recovered` exclusion and its
    // `a_parked_orchestrator_notice_is_not_a_refusal` exist to prevent.
    //
    // Written as a test because the scan is a string match over audit actions:
    // nothing about `notice-undeliverable` being a different word from
    // `delivery-dropped` is enforced by a type, and a later widening to "any
    // dropped-ish line" would silently swallow this one.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5915u32;
    let bound = QUESTION_HOLD_STALE_AFTER.as_millis() as u64;
    let t0 = 8_000_000u64;

    // #633: the escalation below delivers its notice TO THE ORCHESTRATOR, so
    // that pane has to be able to take one or the fixture generates a refusal
    // of its own — a real one, correctly reported, and nothing to do with what
    // this test is about. Pre-#633 an orchestrator with no pane swallowed that
    // notice in silence, which is exactly the loss #633 made visible; the
    // assertion below stayed at `total == 0` rather than being widened to
    // tolerate it, because the incidental refusal is a fixture gap and not the
    // property under test. A pane plus one entry already queued is what makes
    // the notice land: the admission is not `was_first`, so it needs no app
    // handle to be accepted (test mode has none, and an empty-queue admission
    // would be withdrawn as `no-app-handle` instead).
    let orch_pty = 5916u32;
    reg.set_pty_for_test(&orch.id, orch_pty);
    reg.enqueue_text(&g.id, &orch.id, "orrerix", "already queued for the orchestrator", orch_pty,
        queue::EnqueueReason::Arrival).unwrap();

    reg.enqueue_text(&g.id, &w.id, "orrerix", &notify::watch_conflicting_notice("watch-4", 577),
        pty, queue::EnqueueReason::Arrival).unwrap();
    reg.hold_escalation_step(&g.id, &w.id, pty, WriteAdmission::HoldBoxOccupied, 1, t0, bound,
        Some(0), None);
    reg.hold_escalation_step(&g.id, &w.id, pty, WriteAdmission::HoldBoxOccupied, 1, t0 + bound,
        bound, Some(0), None);
    assert_eq!(
        audit_entries(&reg, &g.id, "notice-undeliverable").len(),
        1,
        "the diagnosis fired — otherwise this test proves nothing about what the scan does with it"
    );

    let refusals = reg.front_door_refusals(&g.id);
    assert_eq!(
        refusals.total, 0,
        "a queued-but-undeliverable notice is not a front-door refusal: it holds an id and is \
         still in the pane's queue, so nothing was declined and there is nothing to re-send — \
         got {:?}",
        refusals.items
    );
    assert!(
        refusals.items.is_empty(),
        "and the capped list agrees with the total it reports"
    );
}

#[test]
fn an_orchestrators_own_stuck_pane_parks_its_notice_in_the_inbox() {
    // The case the whole issue turns on, and the one that would be silent
    // without #578's inbox. `notify_queue` refuses to type a notice into an
    // orchestrator's own pane — correctly, since it would queue behind the very
    // block it reports — so before #578 the diagnosis would have been written
    // and discarded. It parks instead and rides back on the orchestrator's next
    // MCP tool result: a call the orchestrator itself made, which is also proof
    // it is running and reading at that instant.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let pty = 5914u32;
    let bound = QUESTION_HOLD_STALE_AFTER.as_millis() as u64;
    let t0 = 7_000_000u64;

    reg.enqueue_text(&g.id, &orch.id, "orrerix", &notify::watch_conflicting_notice("watch-3", 577),
        pty, queue::EnqueueReason::Arrival).unwrap();
    reg.hold_escalation_step(&g.id, &orch.id, pty, WriteAdmission::HoldBoxOccupied, 1, t0, bound,
        Some(0), None);
    reg.hold_escalation_step(&g.id, &orch.id, pty, WriteAdmission::HoldBoxOccupied, 1, t0 + bound,
        bound, Some(0), None);

    let suppressed = audit_entries(&reg, &g.id, "notice-suppressed");
    assert_eq!(suppressed.len(), 1, "the in-band notice is suppressed, as it must be");
    assert_eq!(suppressed[0]["detail"]["reason"], "target-is-orchestrator");
    assert_eq!(suppressed[0]["detail"]["parked"], true, "and parked rather than discarded (#578)");

    let relay = reg.take_orchestrator_notices(&g.id).expect("the relay carries it");
    assert!(relay.contains("notice undeliverable"), "the diagnosis reaches the agent: {relay}");
    assert!(relay.contains(orch.id.as_str()), "about its own pane: {relay}");
    assert_eq!(
        mask_loomux_notices(&relay).trim(),
        "",
        "and every row of the relay stays maskable with this notice in it (#621): {relay}"
    );
    assert!(
        reg.take_orchestrator_notices(&g.id).is_none(),
        "drained once — a relayed notice must not repeat on the next tool call"
    );
    assert_eq!(
        reg.queue_depth(pty),
        1,
        "nothing was enqueued for the blocked pane: the notice about a pane cannot be allowed to \
         queue behind that pane's own block"
    );
}

// ---------- #569: the LEGACY discard scan, and what resume says about it -------
//
// Option 2 (enqueue-while-paused) made `prompt-suppressed-paused` a line no
// build writes any more: a pause queues. The scan below it is kept, and so are
// these tests, for the one window where a payload can still have been
// destroyed — a group paused under an older loomux and resumed under this one.
// Every fixture here is therefore a LEGACY timeline by construction, which is
// why they are built by hand rather than driven through `deliver_prompt`
// (whose pause branch can no longer produce one). The live, queueing behavior
// is covered in the `#569 option 2` section further down.

/// One audit entry, as `suppressed_during_pause` reads them.
pub(crate) fn audit(actor: &str, action: &str, detail: Value) -> AuditEntry {
    AuditEntry { ts_ms: 0, actor: actor.to_string(), action: action.to_string(), detail }
}

/// A `prompt-suppressed-paused` line: actor is the SENDER, `to`/`text` the
/// payload — exactly the shape `deliver_prompt`'s pause branch wrote BEFORE
/// #569 option 2. Nothing writes it now; that is what makes it a legacy
/// fixture rather than a stale duplicate of live behavior.
pub(crate) fn suppressed(from: &str, to: &str, text: &str) -> AuditEntry {
    audit(from, "prompt-suppressed-paused", json!({ "to": to, "text": text }))
}

/// The full text of every prompt this group actually offered its panes, read
/// off the `prompt` audit line `deliver_prompt` writes BEFORE it admits. That
/// line is the only place a headless test can see delivered wording: with no
/// `AppHandle` the admission is withdrawn again and `deliver_prompt` returns
/// `Err("no app handle")`, so nothing survives in the queue to read back.
fn offered_prompts(reg: &OrchRegistry, group: &GroupId) -> Vec<String> {
    reg.audit_log(group)
        .into_iter()
        .filter(|e| e.action == "prompt")
        .filter_map(|e| e.detail["text"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn suppressed_during_pause_reads_only_the_window_the_last_pause_opened() {
    // The window arithmetic, on a synthetic timeline — a pause, a resume, a
    // SECOND pause, and the scan must report the second window's two payloads
    // and not the first window's one. Getting this wrong re-reports deliveries
    // the orchestrator was already told about on an earlier resume, which
    // trains a reader to ignore the notice.
    let entries = vec![
        audit("human", "group-pause", json!({})),
        suppressed("w-1", "orch-1", "stale: from the FIRST window"),
        audit("human", "group-resume", json!({})),
        audit("orch-1", "prompt", json!({ "to": "w-1", "text": "unrelated" })),
        audit("human", "group-pause", json!({})),
        suppressed("w-2", "orch-1", "report: done, PR #123 is green"),
        audit("orrerix", "delivery-queued", json!({ "to": "w-2" })),
        suppressed("human", "w-3", "also update the README"),
    ];

    let s = suppressed_during_pause(&entries);
    assert!(s.window_start_seen, "the opening `group-pause` was right there in the timeline");
    assert_eq!(
        s.items,
        vec![
            SuppressedDelivery {
                from: "w-2".into(),
                to: "orch-1".into(),
                preview: "report: done, PR #123 is green".into(),
                cause: SuppressedCause::LegacyDiscard,
            },
            SuppressedDelivery {
                from: "human".into(),
                to: "w-3".into(),
                preview: "also update the README".into(),
                cause: SuppressedCause::LegacyDiscard,
            },
        ],
        "only THIS window's suppressions, in arrival order, sender and target both named"
    );
}

#[test]
fn suppressed_during_pause_does_not_stop_at_a_group_restore_that_shares_the_resume_name() {
    // `create_group` audits `group-resume` for a group RESTORED from disk —
    // the same action string `resume_group` writes for a human resuming a
    // pause. A scan that treated it as a window boundary would silently
    // truncate every pause that outlived an app restart, which is exactly the
    // long unattended pause with the most to report. `group-pause` has no such
    // collision, so it is the only boundary.
    let entries = vec![
        audit("human", "group-pause", json!({})),
        suppressed("w-1", "orch-1", "sent before the restart"),
        audit("orrerix", "group-resume", json!({ "repo": "C:/tmp/repo", "max_agents": 4 })),
        suppressed("w-1", "orch-1", "sent after it"),
    ];

    let s = suppressed_during_pause(&entries);
    assert!(s.window_start_seen);
    assert_eq!(
        s.items.len(),
        2,
        "a restart mid-pause must not cut the window in half: {:?}",
        s.items
    );
}

#[test]
fn suppressed_during_pause_says_so_when_the_window_start_has_rotated_away() {
    // `audit_log` returns at most AUDIT_VIEW_LIMIT entries off a rotating
    // file, so the `group-pause` that opened the window can genuinely be gone.
    // The count is then a possible OVER-count (it may reach back into an
    // earlier pause) and must be reported as one — a list presented as exact
    // when it isn't is the unbacked claim `.loomux/lessons.md` catalogues.
    let entries = vec![suppressed("w-1", "orch-1", "no opening marker above me")];

    let s = suppressed_during_pause(&entries);
    assert_eq!(s.items.len(), 1, "the readable suppressions are still reported");
    assert!(!s.window_start_seen, "and the missing boundary is flagged, not assumed");
    assert!(
        pause_suppression_notice(&s).contains("may reach back into an earlier pause"),
        "the caveat has to reach the reader, not just the struct: {}",
        pause_suppression_notice(&s)
    );
}

#[test]
fn the_resume_notice_says_discarded_and_never_promises_a_replay() {
    // The one job this copy has that the audit line cannot do: correct the
    // "queued means safe" model every other #445/#523 path established. A
    // reader who assumes a replay is coming waits forever — the exact stall
    // #569 was filed for.
    let s = PauseSuppression {
        items: vec![SuppressedDelivery {
            from: "w-2".into(),
            to: "orch-1".into(),
            preview: "report: done, PR #123 is green".into(),
            cause: SuppressedCause::LegacyDiscard,
        }],
        window_start_seen: true,
    };
    let n = pause_suppression_notice(&s);

    assert!(n.contains("1 delivery was LOST"), "loud, and singular reads as singular: {n}");
    assert!(n.contains("w-2 -> orch-1"), "names sender and target: {n}");
    assert!(n.contains("PR #123 is green"), "and the payload, so it can be re-requested: {n}");
    assert!(n.contains("re-requested"), "the recovery is a RE-SEND, not a replay loomux does: {n}");
    // #569 option 2: the two behaviors now coexist in one group's history, so
    // the notice has to say WHICH pause it is about. Without this, a reader
    // takes it as the current rule and re-requests work that is already on its
    // way — the duplicate `queued_notice`'s "do NOT re-send" exists to prevent,
    // arriving from the opposite direction.
    assert!(
        n.contains("EARLIER loomux version"),
        "must scope the discard to the build that did it: {n}"
    );
    assert!(
        n.contains("delivering on its own right now"),
        "and must say the current rule, or it teaches the wrong one: {n}"
    );
    // Review B2: a legacy-only window must NOT mention the queue-full refusal —
    // explaining a failure mode this window did not have is one more paragraph
    // between the reader and the one that matters.
    assert!(
        !n.contains("REFUSED"),
        "a legacy-only window must not describe a refusal that did not happen: {n}"
    );
    assert!(
        !n.contains("may reach back"),
        "no truncation caveat when the window start was seen: {n}"
    );

    let many = PauseSuppression {
        items: (0..PAUSE_SUPPRESSION_LIST_MAX + 3)
            .map(|i| SuppressedDelivery {
                from: format!("w-{i}"),
                to: "orch-1".into(),
                preview: format!("payload {i}"),
                cause: SuppressedCause::LegacyDiscard,
            })
            .collect(),
        window_start_seen: true,
    };
    let n = pause_suppression_notice(&many);
    assert!(n.contains("11 deliveries were LOST"), "plural, and the full count: {n}");
    assert!(n.contains("...and 3 more"), "the list is capped and says so: {n}");
    assert!(
        n.contains("prompt-suppressed-paused"),
        "and points at the log that holds the rest in full: {n}"
    );
    assert!(!n.contains("payload 9"), "beyond the cap is summarized, not listed: {n}");
}

#[test]
fn the_resume_notice_reports_a_queue_full_refusal_as_a_LIVE_risk_not_as_history() {
    // Review B2: option 2 was documented as making a pause incapable of
    // destroying a payload, and that was false — a pane at QUEUE_MAX_PER_PANE
    // refuses further admissions for as long as the pause lasts. The notice has
    // to separate the two causes, because what a reader should DO about them
    // differs: the legacy discard cannot recur on this build, the refusal can
    // and will on the next long pause.
    let s = PauseSuppression {
        items: vec![SuppressedDelivery {
            from: "w-2".into(),
            to: "orch-1".into(),
            preview: "report: done, PR #77 is green".into(),
            cause: SuppressedCause::QueueFullDuringPause,
        }],
        window_start_seen: true,
    };
    let n = pause_suppression_notice(&s);
    assert!(n.contains("REFUSED"), "the refusal must be named as such: {n}");
    assert!(n.contains("queue was\
         already full") || n.contains("already full"),
        "and must say why it was refused: {n}");
    assert!(n.contains("expect it again"),
        "a live risk must read as live — this is the half a reader can still act on: {n}");
    assert!(
        !n.contains("EARLIER loomux version"),
        "a refusal-only window must not blame a build that had nothing to do with it: {n}"
    );
    assert!(n.contains("refused, queue full"), "and the per-item cause is named: {n}");

    // A MIXED window names both, since one window can genuinely contain both.
    let mixed = PauseSuppression {
        items: vec![
            SuppressedDelivery {
                from: "w-1".into(),
                to: "orch-1".into(),
                preview: "old loss".into(),
                cause: SuppressedCause::LegacyDiscard,
            },
            SuppressedDelivery {
                from: "w-2".into(),
                to: "orch-1".into(),
                preview: "new loss".into(),
                cause: SuppressedCause::QueueFullDuringPause,
            },
        ],
        window_start_seen: true,
    };
    let m = pause_suppression_notice(&mixed);
    assert!(m.contains("2 deliveries were LOST"), "{m}");
    assert!(m.contains("EARLIER loomux version") && m.contains("REFUSED"), "both causes named: {m}");
    assert!(m.contains("discarded by an earlier loomux") && m.contains("refused, queue full"),
        "and each item says which one it was: {m}");
}

#[test]
fn the_window_scan_picks_up_a_queue_full_refusal_but_not_an_ordinary_one() {
    // The `enqueue_reason` filter is what keeps this scoped to the pause.
    // `delivery-dropped` is written for ordinary queue-full rejections too, and
    // those are the sender's own synchronous `Err` to deal with — re-reporting
    // them at every resume would be the notice-becomes-noise failure the cap on
    // this list exists to prevent.
    let dropped = |reason: &str, text: &str| {
        audit("orrerix", "delivery-dropped", json!({
            "to": "orch-1", "from": "w-2", "reason": "queue-full-at-call",
            "enqueue_reason": reason, "preview": text,
        }))
    };
    let entries = vec![
        audit("human", "group-pause", json!({})),
        dropped("group-paused", "report: done, PR #77 is green"),
        dropped("arrival", "an ordinary overflow, not this window's business"),
        suppressed("w-9", "orch-1", "an older build's discard"),
    ];

    let s = suppressed_during_pause(&entries);
    assert!(s.window_start_seen);
    assert_eq!(s.items.len(), 2, "the pause refusal and the legacy discard, not the arrival: {:?}", s.items);
    assert_eq!(
        s.items[0],
        SuppressedDelivery {
            // `from` comes off the DETAIL here: the actor is `loomux`, which did
            // the dropping — not the sender who lost the work.
            from: "w-2".into(),
            to: "orch-1".into(),
            preview: "report: done, PR #77 is green".into(),
            cause: SuppressedCause::QueueFullDuringPause,
        },
        "the pause-scoped refusal, attributed to its SENDER"
    );
    assert_eq!(s.items[1].cause, SuppressedCause::LegacyDiscard);
    assert!(
        !s.items.iter().any(|i| i.preview.contains("ordinary overflow")),
        "an unpaused overflow must not be re-reported at resume: {:?}",
        s.items
    );
}

#[test]
fn the_fallback_badge_fires_only_when_the_notice_did_not_land_on_a_free_live_pane() {
    // Extracted precisely so this table is assertable: with no `AppHandle` a
    // headless registry can never produce a DELIVERED notice, so
    // "delivered => never badge" is unreachable through the registry and would
    // otherwise be a claim in a comment.
    let others = [
        None,                                        // an in-flight heal
        Some(StrandedBlocker::HumanInput),
        Some(StrandedBlocker::QuestionStale),
        Some(StrandedBlocker::QueueAtCapacity),
    ];

    assert!(
        pause_badge_decision(false, true, None),
        "undelivered notice + live unbadged pane is the whole point of the fallback"
    );
    assert!(
        !pause_badge_decision(true, true, None),
        "a notice the orchestrator took makes a chip on every pane pure noise"
    );
    assert!(!pause_badge_decision(false, false, None), "a dead pane's chip helps nobody");
    for blocker in others {
        assert!(
            !pause_badge_decision(false, true, Some(StrandedNote { blocker, since_ms: 1 })),
            "another mechanism's badge is not ours to overwrite: {blocker:?}"
        );
    }
    assert!(
        pause_badge_decision(
            false,
            true,
            Some(StrandedNote { blocker: Some(StrandedBlocker::PauseSuppressed), since_ms: 1 })
        ),
        "our OWN badge is re-stamped, so a second lossy pause does not go quiet"
    );
}

// ---------- #569 option 2: a pause QUEUES, and resume flushes ----------
//
// The human's decision on this issue: paused-group deliveries are held in the
// target pane's durable queue and delivered on resume, not destroyed. What
// follows pins the behavior end to end; the legacy-discard scan that option 1
// shipped is covered in its own section above and is now reachable only from a
// group paused by an older build.

#[test]
fn a_pause_queues_every_delivery_instead_of_destroying_it() {
    // The defect, stated as its fix: three deliveries arrive during a pause and
    // all three are still there afterwards, on the pane each was addressed to,
    // in arrival order. Pre-option-2 every one of these calls returned Ok and
    // destroyed its payload — a worker's `report("done")` fired mid-pause
    // evaporated and the orchestrator went on waiting for it forever.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, 5690);
    reg.set_pty_for_test(&w.id, 5691);

    reg.pause_group(&g.id).unwrap();
    reg.deliver_prompt(&orch.id, "report: done, PR #123 is green", "w-1", Delivery::MidSession)
        .unwrap();
    reg.deliver_prompt(&w.id, "also update the README", "orch-1", Delivery::MidSession).unwrap();
    reg.deliver_prompt(&orch.id, "blocked: needs a human call", "w-2", Delivery::MidSession)
        .unwrap();

    // Nothing was destroyed: each payload is on its target's queue, and the two
    // addressed to the orchestrator are in the order they arrived.
    let held: Vec<String> = reg
        .queue_snapshot(5690)
        .into_iter()
        .filter_map(|e| e.payload.text().map(str::to_string))
        .collect();
    assert_eq!(
        held,
        vec![
            "report: done, PR #123 is green".to_string(),
            "blocked: needs a human call".to_string(),
        ],
        "both orchestrator-bound payloads are held, oldest first"
    );
    let held_w: Vec<String> = reg
        .queue_snapshot(5691)
        .into_iter()
        .filter_map(|e| e.payload.text().map(str::to_string))
        .collect();
    assert_eq!(held_w, vec!["also update the README".to_string()], "and the worker's own");

    // Each is admitted under its OWN reason, so the audit says WHY it waited
    // rather than leaving a reader to infer it from a timestamp.
    assert!(
        reg.queue_snapshot(5690).iter().all(|e| e.reason == queue::EnqueueReason::GroupPaused),
        "a pause-held entry must be admitted as `group-paused`, not as an ordinary arrival"
    );
    let queued: Vec<Value> = reg
        .audit_log(&g.id)
        .into_iter()
        .filter(|e| e.action == "delivery-queued")
        .map(|e| e.detail)
        .collect();
    assert_eq!(queued.len(), 3, "every one of the three is recorded as queued: {queued:?}");
    assert!(
        queued.iter().all(|d| d["reason"] == json!("group-paused")),
        "and each names the pause as its reason: {queued:?}"
    );

    // The pause is still a pause: nothing pasted, so no agent was woken.
    assert!(
        reg.audit_log(&g.id).iter().all(|e| e.action != "delivery-dequeued"),
        "a paused group must deliver nothing to any pane, however much it holds"
    );
}

#[test]
fn a_pause_held_delivery_survives_a_loomux_restart() {
    // "Queued means safe" is a claim about the process dying too (#467/#468).
    // A pause is the longest a delivery can sit unattended, so it is the case
    // most likely to meet a restart — and the one where losing the payload
    // costs most, because nobody is watching.
    let dir = tempfile::tempdir().unwrap();
    let gid;
    let snap_path;
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45997);
        let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
        gid = g.id.clone();
        let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
        reg.set_pty_for_test(&w.id, 5696);
        reg.pause_group(&g.id).unwrap();
        reg.deliver_prompt(&w.id, "held across the restart", "orch-1", Delivery::MidSession)
            .unwrap();
        assert_eq!(reg.queue_depth(5696), 1, "precondition: the pause queued it");
        snap_path = reg.state_root().join(g.id.as_str()).join("queue.json");
    }
    // The durable snapshot is what carries it, so it must name the payload.
    let snap = fs::read_to_string(&snap_path)
        .expect("a pause-held delivery must be persisted, not held only in memory");
    assert!(snap.contains("held across the restart"), "the payload itself is on disk: {snap}");
    assert!(snap.contains("group-paused"), "and so is the reason it is waiting: {snap}");

    // A fresh process reads it back as an orphan awaiting its pane, exactly as
    // any other queued delivery caught by a restart would be.
    let reg = relaunch_registry(dir.path());
    reg.set_port(45997);
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    assert_eq!(g.id, gid);
    assert!(reg.is_paused(&g.id), "and the group is still paused");
    let orphans = reg.queue_orphans(&g.id);
    assert_eq!(orphans.len(), 1, "the held payload comes back: {orphans:?}");
    assert_eq!(orphans[0].text.as_deref(), Some("held across the restart"));
}

#[test]
fn resume_starts_a_drain_for_every_pane_the_pause_left_holding() {
    // `deliver_prompt`'s pause branch deliberately does NOT spawn a drainer —
    // a paused group must have nothing running that could paste. That leaves
    // #470's "the admission that saw an empty queue starts the processor"
    // invariant unmet on purpose, and `flush_paused_queues` is the single place
    // that discharges it. If it stops firing, every payload a pause held is
    // stranded: queued, audited, persisted, and never delivered.
    //
    // A headless registry has no `AppHandle`, so no thread can actually be
    // spawned; what is assertable — and what the stranding would break — is
    // that the resume identifies exactly the panes that need one.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let idle = reg.spawn_agent(&g.id, Role::Worker, "idle", "t", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, 5710);
    reg.set_pty_for_test(&w.id, 5711);
    reg.set_pty_for_test(&idle.id, 5712);

    reg.pause_group(&g.id).unwrap();
    reg.deliver_prompt(&orch.id, "a report", "w-1", Delivery::MidSession).unwrap();
    reg.deliver_prompt(&w.id, "a task", "orch-1", Delivery::MidSession).unwrap();
    assert!(
        reg.audit_log(&g.id).iter().all(|e| e.action != "pause-flush"),
        "precondition: a pause that is still on has flushed nothing"
    );

    reg.resume_group(&g.id).unwrap();

    let flush = reg
        .audit_log(&g.id)
        .into_iter()
        .find(|e| e.action == "pause-flush")
        .expect("resume must record which panes it set draining");
    assert_eq!(
        flush.detail["panes"],
        json!([5710, 5711]),
        "exactly the panes holding entries, ascending — never the idle third pane"
    );
}

// ---------- #620: a kickoff held through a pause is still a kickoff ----------
//
// #569 made a paused delivery SURVIVE; it did not make it survive as what it
// is. `flush_paused_queues` started every drainer with `None`, so a
// `FreshKickoff` queued during a pause pasted at resume with `wait_ready:
// false, confirm_autopilot: false, fresh_kickoff: false`. For a copilot pane
// under `--autopilot` that is a wedge rather than a timing nit: the consent
// dialog appears AFTER the kickoff Enter, so nobody dismisses it, and #517's
// late-kickoff recovery is unarmed because the drainer was never told this was
// a kickoff. What follows pins the kind onto the entry (the only thing that
// outlives the `deliver_prompt` call) and the treatment back off it.

/// A copilot group in the unattended posture — the one that launches panes
/// with `--autopilot` and therefore meets the consent dialog #620 is about.
fn autopilot_copilot_rails() -> Guardrails {
    let mut r = copilot_rails();
    r.auto_ops = true;
    r
}

#[test]
fn a_kickoff_held_through_a_pause_keeps_its_kickoff_treatment_at_resume() {
    // The defect, stated as its fix. Nothing guards `spawn_agent` against a
    // paused group, so a spawn during a pause routes its brief through
    // `deliver_prompt`'s pause branch like any other delivery — and the brief
    // is the one payload with no other route to the agent.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/copilot-repo", autopilot_copilot_rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&w.id, 5720);

    reg.pause_group(&g.id).unwrap();
    reg.deliver_prompt(&w.id, "your brief: fix the thing", "orch-1", Delivery::FreshKickoff)
        .unwrap();

    let held = reg.queue_snapshot(5720);
    assert_eq!(held.len(), 1, "precondition: the pause queued the brief rather than destroying it");
    assert_eq!(
        held[0].delivery_kind,
        Delivery::FreshKickoff,
        "the entry has to record WHAT it is — by resume, it is the only thing that still knows"
    );

    let (id, t) = reg
        .paused_flush_kickoff(&g.id, 5720)
        .expect("a held FreshKickoff must flush WITH kickoff treatment, not as a plain prompt");
    assert_eq!(id, held[0].id, "the treatment belongs to the entry the drainer's first pass picks up");
    assert_eq!(
        t,
        KickoffTreatment { wait_ready: true, confirm_autopilot: true, fresh_kickoff: true },
        "all three by name — the boot wait, copilot's consent dialog answered before the paste, \
         and #517's late-kickoff recovery armed; they are the same type and mean opposite things"
    );

    // And it is DURABLE, spelled out rather than serde-shaped: this entry is
    // the on-disk record (#468), and a human reading `queue.json` after a
    // crash should see what the entry is.
    let snap = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("queue.json"))
        .expect("a pause-held kickoff is persisted like any other entry");
    assert!(snap.contains("fresh-kickoff"), "the kind is written out in full: {snap}");
}

#[test]
fn an_unpaused_kickoff_records_its_kind_on_the_entry_too() {
    // Review NB2. `deliver_prompt` stamps the kind on its NON-paused admission
    // as well, so an entry's account of itself never depends on which branch
    // admitted it — and until this test, that line had neither a reader nor a
    // test: reverting it left the suite green, and the red run could not
    // attribute anything to it because the scratch commit neutered both sites
    // at once. It matters if the reachability argument ("nothing reads it back
    // on this path") ever stops holding: a pause landing on a queue that
    // already holds an unpasted kickoff would then meet an entry that had
    // quietly forgotten what it was.
    //
    // The kickoff is admitted BEHIND a seeded entry on purpose: a `was_first`
    // admission with no `AppHandle` is withdrawn again
    // (`withdraw_unprocessable`), and a headless registry has none — so the
    // only way to observe a stamped entry on this branch is to keep the
    // delivery off the front.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/copilot-repo", autopilot_copilot_rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&w.id, 5726);
    reg.enqueue_text(&g.id, &w.id, "orch-1", "already queued", 5726, queue::EnqueueReason::Arrival)
        .unwrap();
    assert!(!reg.is_paused(&g.id), "precondition: this is the UNPAUSED branch");

    reg.deliver_prompt(&w.id, "your brief: fix the thing", "orch-1", Delivery::FreshKickoff)
        .unwrap();

    let held = reg.queue_snapshot(5726);
    assert_eq!(held.len(), 2, "precondition: the kickoff landed behind the seeded entry: {held:?}");
    assert_eq!(
        held[1].delivery_kind,
        Delivery::FreshKickoff,
        "an unpaused admission records the kind too — the entry says the same thing either way"
    );
    assert_eq!(
        held[0].delivery_kind,
        Delivery::MidSession,
        "and stamping the arriving delivery must not rewrite what was already queued"
    );
}

#[test]
fn an_ordinary_prompt_held_through_a_pause_still_gets_no_kickoff_treatment() {
    // The other half, and the one that keeps the fix from being worse than the
    // bug: a boot wait and a stray Enter aimed at a consent dialog are ACTIONS
    // against a live pane. Every routine held delivery — a worker's report, a
    // steer — must still reach the drainer with none of them.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/copilot-repo", autopilot_copilot_rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, 5721);

    reg.pause_group(&g.id).unwrap();
    reg.deliver_prompt(&orch.id, "report: done, PR #123 is green", "w-1", Delivery::MidSession)
        .unwrap();

    assert_eq!(reg.queue_snapshot(5721)[0].delivery_kind, Delivery::MidSession);
    assert_eq!(
        reg.paused_flush_kickoff(&g.id, 5721),
        None,
        "a mid-session prompt is long past boot — the resume must hand its drainer nothing"
    );
}

#[test]
fn a_held_kickoff_on_claude_waits_for_boot_but_confirms_no_copilot_dialog() {
    // `confirm_autopilot` is the one flag the delivery kind cannot decide on
    // its own — it also depends on the group's CLI and posture, and the resume
    // must reach the same verdict `deliver_prompt`'s front door would. A claude
    // pane has no "Enable autopilot mode" dialog to answer, so firing that
    // watcher would arm a stray Enter for a pane that will never show one.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap(); // claude, not copilot
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&w.id, 5722);

    reg.pause_group(&g.id).unwrap();
    reg.deliver_prompt(&w.id, "your brief: fix the thing", "orch-1", Delivery::FreshKickoff)
        .unwrap();

    let (_, t) = reg.paused_flush_kickoff(&g.id, 5722).expect("still a kickoff, still held");
    assert_eq!(
        t,
        KickoffTreatment { wait_ready: true, confirm_autopilot: false, fresh_kickoff: true },
        "the boot wait and #517's recovery are the kind's; the consent confirm is the CLI's"
    );
}

#[test]
fn a_resume_kickoff_held_through_a_pause_is_not_itself_armed_for_recovery() {
    // #517's narrowing, preserved across the pause. A resume's re-sync notice
    // is re-derived from durable state (`resume_kickoff_notice`), so losing it
    // is recoverable by other means — only a FRESH spawn's brief exists
    // nowhere else. The boot wait and the consent confirm still apply (#364: a
    // resumed copilot pane does show the dialog again).
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/copilot-repo", autopilot_copilot_rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&w.id, 5723);

    reg.pause_group(&g.id).unwrap();
    reg.deliver_prompt(&w.id, "you were resumed; here is where you left off", "orrerix",
                       Delivery::ResumeKickoff)
        .unwrap();

    let (_, t) = reg.paused_flush_kickoff(&g.id, 5723).expect("a resume kickoff is a kickoff too");
    assert_eq!(
        t,
        KickoffTreatment { wait_ready: true, confirm_autopilot: true, fresh_kickoff: false },
        "a re-derivable payload must not arm the one-shot late-kickoff re-delivery"
    );
}

#[test]
fn a_resume_records_which_panes_it_is_flushing_a_kickoff_back_into() {
    // The boot wait and the consent confirm happen inside a thread no test can
    // observe, and `audit.jsonl` is the only record a human will ever have that
    // they were armed at all — which is exactly what was missing while this was
    // broken. Two panes, one kickoff and one ordinary report, so the line has
    // to discriminate rather than merely echo `panes`.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/copilot-repo", autopilot_copilot_rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, 5724);
    reg.set_pty_for_test(&w.id, 5725);

    reg.pause_group(&g.id).unwrap();
    reg.deliver_prompt(&orch.id, "report: still working", "w-1", Delivery::MidSession).unwrap();
    reg.deliver_prompt(&w.id, "your brief: fix the thing", "orch-1", Delivery::FreshKickoff)
        .unwrap();
    reg.resume_group(&g.id).unwrap();

    let flush = reg
        .audit_log(&g.id)
        .into_iter()
        .find(|e| e.action == "pause-flush")
        .expect("resume must record what it set draining");
    assert_eq!(flush.detail["panes"], json!([5724, 5725]), "both panes are drained");
    assert_eq!(
        flush.detail["kickoff_panes"],
        json!([5725]),
        "only the pane whose held delivery is a kickoff is flushed as one: {:?}",
        flush.detail
    );
}

#[test]
fn a_queue_record_written_before_the_kind_existed_reads_as_a_plain_prompt() {
    // `QueuedDelivery` IS the on-disk record (#468), so a field added to it is
    // a change to a persisted format: a `queue.json` written by a build that
    // predates #620 has to keep parsing. It comes back as `MidSession` — the
    // conservative direction, because every treatment the kind can switch ON is
    // an action taken against a live pane, and a record that cannot say what it
    // was must trigger none of them.
    let legacy = json!({
        "version": queue::SNAPSHOT_VERSION,
        "written_ms": 1,
        "entries": [{
            "pty_id": 9, "id": 1, "agent_id": "w-1", "from": "orch-1",
            "payload": { "kind": "text", "text": "queued by an older build" },
            "reason": "group-paused", "enqueued_ms": 5, "coalesced": 0,
            "group": "g-1", "to_orchestrator": false, "session_id": null,
        }],
    });
    let (back, skipped) = queue::parse_snapshot(&legacy.to_string());
    assert_eq!((back.len(), skipped), (1, 0), "an older build's entry must still parse: {back:?}");
    assert_eq!(back[0].delivery.payload.text(), Some("queued by an older build"));
    assert_eq!(
        back[0].delivery.delivery_kind,
        Delivery::MidSession,
        "a record with no kind on it says nothing, and nothing means take no kickoff action"
    );

    // And the round trip is symmetric: what this build writes, this build reads
    // back as the same kind — the property the legacy default is the fallback
    // FOR, not a substitute for it.
    let current = json!({
        "version": queue::SNAPSHOT_VERSION,
        "written_ms": 1,
        "entries": [{
            "pty_id": 9, "id": 2, "agent_id": "w-1", "from": "orch-1",
            "payload": { "kind": "text", "text": "queued by this build" },
            "reason": "group-paused", "enqueued_ms": 5, "coalesced": 0,
            "group": "g-1", "to_orchestrator": false, "session_id": null,
            "delivery_kind": "fresh-kickoff",
        }],
    });
    let (back, skipped) = queue::parse_snapshot(&current.to_string());
    assert_eq!((back.len(), skipped), (1, 0));
    assert_eq!(back[0].delivery.delivery_kind, Delivery::FreshKickoff);
}

#[test]
fn a_resume_with_nothing_held_flushes_nothing_and_says_nothing() {
    // The routine case — a human pauses to look at something and resumes a
    // minute later — must cost the group no turn at all. A "0 deliveries" line
    // on every pause/resume cycle is exactly how a notice trains its reader to
    // skim past the one that matters.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, 5713);

    reg.pause_group(&g.id).unwrap();
    reg.resume_group(&g.id).unwrap();

    let log = reg.audit_log(&g.id);
    assert!(log.iter().all(|e| e.action != "pause-flush"), "nothing held, nothing to flush");
    assert!(
        log.iter().all(|e| e.action != "pause-suppression-notice"),
        "and nothing was discarded, so no discard notice either"
    );
    assert!(delivered_texts(&reg, &g.id).is_empty(), "the group is offered nothing at all");
}

#[test]
fn a_pause_held_flush_tells_the_receiver_it_was_the_pause_and_not_a_blocked_pane() {
    // A pause-held payload arriving under "queued while this pane was blocked"
    // sends its reader hunting for a box or a dialog that never existed. The
    // header is chosen from the ENTRIES' admission reasons, not from the live
    // paused flag, because by flush time the group is unpaused by definition.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&w.id, 5714);

    reg.pause_group(&g.id).unwrap();
    reg.deliver_prompt(&w.id, "the brief", "orch-1", Delivery::MidSession).unwrap();
    reg.resume_group(&g.id).unwrap();

    // The flush itself needs a real pane to paste into, so the assertion is on
    // the pure function the drainer feeds — with the queue the pause actually
    // built, not a hand-made one.
    let held = reg.queue_snapshot(5714);
    assert_eq!(queue::flush_cause(&held), queue::FlushCause::GroupPaused);
    let header = queue::flush_header_text(held.len(), 0, queue::flush_cause(&held));
    assert!(header.contains("queued while this group was paused"), "got: {header}");
    assert!(!header.contains("pane was blocked"), "got: {header}");
}

#[test]
fn a_pause_taken_by_this_build_has_nothing_to_report_as_discarded() {
    // The transitional notice must not fire for a pause that discarded nothing
    // — otherwise every resume tells the orchestrator that work it is about to
    // receive is gone, and the correct response to that sentence (re-request
    // it) produces the duplicate `queued_notice`'s "do NOT re-send" exists to
    // prevent.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, 5715);
    reg.set_pty_for_test(&w.id, 5716);

    reg.pause_group(&g.id).unwrap();
    reg.deliver_prompt(&orch.id, "report: done", "w-1", Delivery::MidSession).unwrap();
    reg.deliver_prompt(&w.id, "a task", "orch-1", Delivery::MidSession).unwrap();
    reg.resume_group(&g.id).unwrap();

    assert!(
        reg.audit_log(&g.id).iter().all(|e| e.action != "pause-suppression-notice"),
        "a pause that queued everything has nothing to call discarded"
    );
    assert!(
        !delivered_texts(&reg, &g.id).iter().any(|t| t.contains("DISCARDED")),
        "and must never offer the orchestrator a discard notice"
    );
    assert!(reg.stranded_note(&w.id).is_none(), "nor badge a pane whose work is safe");
    assert!(
        reg.audit_log(&g.id).iter().all(|e| e.action != "prompt-suppressed-paused"),
        "the action that named the destroyed payload has no writer left"
    );
}

#[test]
fn a_group_paused_by_an_older_loomux_is_still_told_what_that_build_destroyed() {
    // The one window where a payload can still be gone: paused under a build
    // that discarded, resumed under one that queues. Nothing in this process
    // can produce a `prompt-suppressed-paused` line any more, so the legacy
    // timeline is written directly — which is the honest way to test a path
    // whose only real input comes from a previous version's log.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, 5717);
    reg.set_pty_for_test(&w.id, 5718);

    reg.pause_group(&g.id).unwrap(); // writes the `group-pause` line that bounds the window
    reg.audit(&g.id, "w-1", "prompt-suppressed-paused",
        json!({ "to": orch.id, "text": "report: done, PR #123 is green" }));
    reg.audit(&g.id, "orch-1", "prompt-suppressed-paused",
        json!({ "to": w.id, "text": "also update the README" }));

    reg.resume_group(&g.id).unwrap();

    let notice = delivered_texts(&reg, &g.id)
        .into_iter()
        .find(|t| t.contains("DISCARDED"))
        .expect("resume must still name what the older build lost");
    for payload in ["PR #123 is green", "update the README"] {
        assert!(notice.contains(payload), "every discarded payload is named: {notice}");
    }
    assert!(
        notice.contains("EARLIER loomux version"),
        "and must scope the loss to the build that caused it, or a reader takes it as \
         the current rule and re-requests work that is already queued: {notice}"
    );

    let tally = reg
        .audit_log(&g.id)
        .into_iter()
        .find(|e| e.action == "pause-suppression-notice")
        .expect("the tally is recorded whether or not the notice lands");
    assert_eq!(tally.detail["count"], json!(2));
    assert_eq!(tally.detail["window_start_seen"], json!(true));
}

#[test]
fn the_legacy_discard_notice_still_badges_when_no_orchestrator_can_take_it() {
    // The notice is an in-band delivery, so it fails exactly when a pause has
    // run long enough for the orchestrator to idle out or be killed — the
    // longest pause, with the most to report. A fix whose only channel goes
    // missing in its own worst case is the defect it was meant to close, one
    // level up. Option 2 did not change that; it only narrowed which pauses
    // can reach it.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let dead = reg.spawn_agent(&g.id, Role::Worker, "dead", "t", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, 5694);

    reg.pause_group(&g.id).unwrap();
    for text in ["you have a new task", "and a second one"] {
        reg.audit(&g.id, "orch-1", "prompt-suppressed-paused", json!({ "to": w.id, "text": text }));
    }
    reg.audit(&g.id, "orch-1", "prompt-suppressed-paused",
        json!({ "to": dead.id, "text": "never arrived" }));
    // The orchestrator is gone by the time the human comes back.
    reg.mark_dead(&orch.id, Some(0));
    reg.mark_dead(&dead.id, Some(0));

    reg.resume_group(&g.id).unwrap();

    let tally = reg
        .audit_log(&g.id)
        .into_iter()
        .find(|e| e.action == "pause-suppression-notice")
        .expect("the tally is recorded even when nothing can be told");
    assert_eq!(tally.detail["delivered"], json!(false), "and it must not claim it was told");
    assert!(
        tally.detail["error"].as_str().unwrap_or_default().contains("orchestrator"),
        "naming why: {:?}",
        tally.detail
    );

    let note = reg.stranded_note(&w.id).expect("the pane that lost work must be badged");
    assert_eq!(note.blocker, Some(StrandedBlocker::PauseSuppressed));
    assert!(
        reg.stranded_note(&dead.id).is_none(),
        "a dead pane's badge is a chip nobody can act on"
    );
    // One badge for a pane that lost two payloads, not one per payload.
    assert_eq!(
        reg.audit_log(&g.id)
            .iter()
            .filter(|e| e.action == "stranded-attention" && e.detail["to"] == json!(w.id))
            .count(),
        1,
        "one badge per pane, however many payloads it lost"
    );
}

#[test]
fn a_resume_racing_an_admission_never_strands_it() {
    // Review BLOCKING 1. `deliver_prompt` reads `is_paused`, then admits — with
    // no lock across the gap and real I/O inside it. A resume landing in that
    // gap clears the flag and finds the pane queue still empty, so
    // `flush_paused_queues` starts nothing; the admission then lands in an
    // unpaused group with no drainer and, because the pause branch is the one
    // path that deliberately never calls `ensure_drainer`, nobody left to start
    // one. Queued, audited, persisted, `Ok` to the sender, never delivered —
    // #569's own defect, reproduced by its fix.
    //
    // The invariant, and it is the ONLY thing that has to hold: for every
    // interleaving, SOMETHING claims responsibility for draining the pane.
    // Either resume saw the entry (`pause-flush` names the pane) or the
    // admission saw the resume (`pause-race-nudge` names the entry). Never
    // neither. A headless registry cannot observe `ensure_drainer` itself — no
    // `AppHandle`, so no thread is ever spawned, which is exactly why both
    // branches audit.
    //
    // Driven with real threads and a real barrier rather than a simulated
    // interleaving: the whole finding is that the two sides are unsynchronized,
    // so a model with a seam in it would be proving the seam. Repeated, because
    // which side wins is the OS's decision and a single run pins nothing.
    //
    // **What this test is NOT, stated so nobody reads it as more than it is.**
    // It does not go red when the fix is removed — verified, not assumed: run
    // 30704213481 deleted the post-admit re-check and this test still passed,
    // because the losing interleaving needs the resume to land between the flag
    // read and the admission, and the stagger that makes the pause branch
    // reachable at all also gives the entry time to be queued before
    // `flush_paused_queues` looks. Forcing that window open would take a seam
    // in the code under test, and a test of a seam is not a test of the race.
    // So this is an invariant GUARD over real interleavings — it will catch a
    // future change that breaks the property on a common ordering — and not
    // evidence that the re-check is load-bearing. The argument for the re-check
    // is in `deliver_prompt`'s comment; the argument that it costs nothing is
    // that `ensure_drainer` no-ops on a pane already draining.
    use std::sync::{Arc, Barrier};

    let (mut held, mut raced) = (0u32, 0u32);
    for round in 0..24 {
        let dir = tempfile::tempdir().unwrap();
        let reg = Arc::new(relaunch_registry(dir.path()));
        reg.set_self_arc();
        let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
        let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
        let pty = 5800 + round;
        reg.set_pty_for_test(&w.id, pty);
        reg.pause_group(&g.id).unwrap();

        // Both threads leave the barrier together; the loser of the coin gets a
        // 3ms head start. Not synchronization — a STAGGER, and it is here
        // because an unstaggered race is not actually a fair sample: after the
        // barrier `resume_group`'s very first act is to clear the flag, while
        // `deliver_prompt` must audit (a file write) before it reads the flag,
        // so resume wins nearly every time and the pause branch — the branch
        // this test exists for — would almost never be entered. Alternating
        // rounds samples both sides on purpose.
        let admit_first = round % 2 == 0;
        let stagger = std::time::Duration::from_millis(3);
        let gate = Arc::new(Barrier::new(2));
        let admit = {
            let (reg, wid, gate) = (reg.clone(), w.id.clone(), gate.clone());
            std::thread::spawn(move || {
                gate.wait();
                if !admit_first {
                    std::thread::sleep(stagger);
                }
                // The payload the issue is about: a worker's completion report.
                reg.deliver_prompt(&wid, "report: done, PR #99 is green", "w-1", Delivery::MidSession)
            })
        };
        let resume = {
            let (reg, gid, gate) = (reg.clone(), g.id.clone(), gate.clone());
            std::thread::spawn(move || {
                gate.wait();
                if admit_first {
                    std::thread::sleep(stagger);
                }
                reg.resume_group(&gid).unwrap();
            })
        };
        let admitted = admit.join().unwrap();
        resume.join().unwrap();

        let log = reg.audit_log(&g.id);
        let flushed = log.iter().any(|e| {
            e.action == "pause-flush"
                && e.detail["panes"].as_array().is_some_and(|p| p.contains(&json!(pty)))
        });
        let nudged = log
            .iter()
            .any(|e| e.action == "pause-race-nudge" && e.detail["pty"] == json!(pty));

        if reg.queue_depth(pty) == 0 {
            // The resume won the flag read outright, so `deliver_prompt` never
            // entered the pause branch at all — it took the ordinary front door,
            // which in a headless registry withdraws its own admission (there is
            // no `AppHandle` to ever drain it) and returns `Err`. Nothing is
            // held, so nothing can be stranded, and the SENDER was told rather
            // than getting a false `Ok` — which is the property that matters on
            // that path. Not this finding's branch; #470 owns it.
            assert!(
                admitted.is_err(),
                "round {round}: nothing was queued, so the sender must have been told — \
                 a silent Ok here would be the same class of loss one door over"
            );
            continue;
        }

        held += 1;
        if nudged {
            raced += 1;
        }
        assert_eq!(reg.queue_depth(pty), 1, "round {round}: exactly the one report is held");
        assert!(
            admitted.is_ok(),
            "round {round}: the entry is queued, so the sender was told Ok — and it must have been"
        );
        assert!(
            flushed || nudged,
            "round {round}: nobody owns draining pty {pty} — the delivery is stranded. \
             pause-flush={flushed} pause-race-nudge={nudged}; log actions: {:?}",
            log.iter().map(|e| e.action.as_str()).collect::<Vec<_>>()
        );
    }

    // Without this the whole loop could pass vacuously by never once reaching
    // the pause branch — every round losing the flag read, every round
    // `continue`ing, and the invariant above never evaluated. The barrier makes
    // that vanishingly unlikely, but "unlikely" is not an assertion.
    assert!(
        held > 0,
        "no round ever held a pause-admitted entry — the admit-first stagger is not working and \
         the invariant below was never once evaluated"
    );
    // `raced` is reported, never asserted on. Whether the resume lands INSIDE
    // the flag-read-to-admit window is the OS's decision and cannot be forced
    // without a seam in the code under test, which would make this a test of the
    // seam. What is asserted is the invariant that must hold for every
    // interleaving; the nudge is one of the two ways it can be satisfied.
    println!("interleavings: {held} held, of which {raced} were closed by the post-admit re-check");
}

#[test]
fn a_queue_full_refusal_during_a_pause_is_reported_to_the_orchestrator_on_resume() {
    // Review BLOCKING 2, end to end. The cap is per PANE and the orchestrator's
    // pane is where a whole fleet converges, so a long pause fills it and the
    // ninth delivery is destroyed — `Err` to the sender, and before this nothing
    // at all to the orchestrator. That is the #569 stall arriving through the
    // queue instead of around it.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, 5820);
    reg.set_pty_for_test(&w.id, 5821);
    reg.pause_group(&g.id).unwrap();

    // Fill the orchestrator's pane to the cap. Distinct texts, or `admit`'s
    // byte-identical coalescing would fold them into one entry and never reach
    // capacity at all.
    for i in 0..queue::QUEUE_MAX_PER_PANE {
        reg.deliver_prompt(&orch.id, &format!("[orrerix] advisory {i}"), "orrerix", Delivery::MidSession)
            .unwrap();
    }
    assert_eq!(reg.queue_depth(5820), queue::QUEUE_MAX_PER_PANE, "precondition: the pane is at capacity");

    // The ninth: a worker's completion report, refused.
    let err = reg
        .deliver_prompt(&orch.id, "report: done, PR #77 is green", &w.id, Delivery::MidSession)
        .unwrap_err();
    assert!(err.contains("queue"), "the SENDER is told synchronously: {err}");

    reg.resume_group(&g.id).unwrap();

    // …and so, now, is the orchestrator.
    let notice = delivered_texts(&reg, &g.id)
        .into_iter()
        .find(|t| t.contains("LOST"))
        .expect("resume must tell the orchestrator a delivery was refused during the pause");
    assert!(notice.contains("REFUSED"), "named as a refusal, not a legacy discard: {notice}");
    assert!(notice.contains("PR #77 is green"), "and the payload, so it can be re-requested: {notice}");
    assert!(
        notice.contains(&format!("{} -> {}", w.id, orch.id)),
        "sender and target both named: {notice}"
    );
    assert!(
        !notice.contains("EARLIER loomux version"),
        "this build refused it — blaming an older one would send the reader nowhere: {notice}"
    );

    let tally = reg
        .audit_log(&g.id)
        .into_iter()
        .find(|e| e.action == "pause-suppression-notice")
        .expect("and the tally is recorded");
    assert_eq!(tally.detail["count"], json!(1));
}

#[test]
fn the_loss_notice_lands_even_when_the_overflowing_pane_is_the_orchestrators_own() {
    // rev-128's finding, and the case #569 is actually about: a fleet's reports
    // converge on the orchestrator's pane, so that is the pane a long pause
    // fills — and it is STILL at capacity when the human resumes. Before this,
    // `announce_pause_suppression` was therefore not merely at risk of being
    // refused, it was CERTAIN to be: the notice about the destroyed payloads was
    // destroyed by the same cap that destroyed them.
    //
    // Worse, and the reason a badge fallback did not cover it: by resume the
    // pane already carries `note_queue_capacity`'s at-capacity badge, so
    // `pause_badge_decision`'s never-stomp-another-mechanism's-badge rule
    // suppresses the pause badge too. Notice refused, badge skipped, audit tally
    // the only trace — the #569 stall, one level up.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, 5840);
    reg.set_pty_for_test(&w.id, 5841);
    reg.pause_group(&g.id).unwrap();

    for i in 0..queue::QUEUE_MAX_PER_PANE {
        reg.deliver_prompt(&orch.id, &format!("[orrerix] advisory {i}"), "orrerix", Delivery::MidSession)
            .unwrap();
    }
    reg.deliver_prompt(&orch.id, "report: done, PR #77 is green", &w.id, Delivery::MidSession)
        .unwrap_err();
    // The precondition that makes this the flagship case rather than a variant:
    // the pane is full AND already badged, so neither channel is free.
    assert_eq!(reg.queue_depth(5840), queue::QUEUE_MAX_PER_PANE);
    assert_eq!(
        reg.stranded_note(&orch.id).and_then(|n| n.blocker),
        Some(StrandedBlocker::QueueAtCapacity),
        "the capacity badge is up, which is what would suppress the pause badge"
    );

    reg.resume_group(&g.id).unwrap();

    // The notice is admitted past the cap — the one exemption, one entry deep.
    let tally = reg
        .audit_log(&g.id)
        .into_iter()
        .find(|e| e.action == "pause-suppression-notice")
        .expect("the tally is always recorded");
    assert_eq!(
        tally.detail["delivered"], json!(true),
        "the notice must actually LAND on a full orchestrator pane, not be refused by the cap \
         that caused the loss it reports: {:?}",
        tally.detail
    );
    assert_eq!(
        reg.queue_depth(5840),
        queue::QUEUE_MAX_PER_PANE + queue::PAUSE_LOSS_NOTICE_HEADROOM,
        "exactly one entry past the cap, never more"
    );
    let notice = delivered_texts(&reg, &g.id)
        .into_iter()
        .find(|t| t.contains("LOST"))
        .expect("and it is a real delivery the orchestrator will read");
    assert!(notice.contains("REFUSED"), "naming the cause: {notice}");
    assert!(notice.contains("PR #77 is green"), "and the payload: {notice}");

    // It is admitted with its own reason, so the audit says why it was allowed
    // past a cap that had just refused a worker's report.
    let queued = reg
        .audit_log(&g.id)
        .into_iter()
        .filter(|e| e.action == "delivery-queued")
        .filter(|e| e.detail["reason"] == json!("pause-loss-notice"))
        .count();
    assert_eq!(queued, 1, "exactly one exempt admission, and it is labelled as one");
}

#[test]
fn the_loss_notice_headroom_is_one_entry_and_a_second_resume_is_refused() {
    // The exemption is a bound, not a bypass. If a second resume finds the
    // headroom still occupied — nothing has drained, because nothing can here —
    // its notice is refused like any other delivery, and the tally says so
    // rather than claiming the orchestrator was told. That is the refusal path
    // rev-128 asked to see covered, reached honestly instead of by pretending
    // the first notice could not land.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, 5850);
    reg.set_pty_for_test(&w.id, 5851);

    // Round 1: fill, overflow, resume — the notice takes the headroom.
    reg.pause_group(&g.id).unwrap();
    for i in 0..queue::QUEUE_MAX_PER_PANE {
        reg.deliver_prompt(&orch.id, &format!("[orrerix] advisory {i}"), "orrerix", Delivery::MidSession)
            .unwrap();
    }
    reg.deliver_prompt(&orch.id, "first lost report", &w.id, Delivery::MidSession).unwrap_err();
    reg.resume_group(&g.id).unwrap();
    assert_eq!(reg.queue_depth(5850), queue::QUEUE_MAX_PER_PANE + queue::PAUSE_LOSS_NOTICE_HEADROOM);

    // Round 2: pause again, lose another, resume again. Nothing drained in
    // between, so the headroom is gone.
    reg.pause_group(&g.id).unwrap();
    reg.deliver_prompt(&orch.id, "second lost report", &w.id, Delivery::MidSession).unwrap_err();
    reg.resume_group(&g.id).unwrap();

    let tallies: Vec<Value> = reg
        .audit_log(&g.id)
        .into_iter()
        .filter(|e| e.action == "pause-suppression-notice")
        .map(|e| e.detail)
        .collect();
    assert_eq!(tallies.len(), 2, "both resumes record a tally: {tallies:?}");
    assert_eq!(tallies[0]["delivered"], json!(true), "the first landed");
    assert_eq!(
        tallies[1]["delivered"], json!(false),
        "the second is refused — and the tally must NOT claim the orchestrator was told: {:?}",
        tallies[1]
    );
    assert!(
        tallies[1]["error"].as_str().unwrap_or_default().contains("full"),
        "naming why it could not land: {:?}",
        tallies[1]
    );
    assert_eq!(
        reg.queue_depth(5850),
        queue::QUEUE_MAX_PER_PANE + queue::PAUSE_LOSS_NOTICE_HEADROOM,
        "and the queue never grows past the one-entry headroom, however many resumes happen"
    );
}

#[test]
fn a_resume_flush_keeps_held_entries_ahead_of_post_resume_traffic() {
    // Review N3. The design note promises pause-held entries drain "in original
    // order"; FIFO makes that structurally true, but nothing asserted it across
    // the resume boundary, so a future `flush_paused_queues` that pushed rather
    // than appended — or a resume that re-admitted its backlog — would land
    // unnoticed. Ordering is the whole reason the queue exists: "here is the
    // context" then "now go" inverts into nonsense.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&w.id, 5830);

    reg.pause_group(&g.id).unwrap();
    reg.deliver_prompt(&w.id, "held first", "orch-1", Delivery::MidSession).unwrap();
    reg.deliver_prompt(&w.id, "held second", "orch-1", Delivery::MidSession).unwrap();
    reg.resume_group(&g.id).unwrap();
    // Nothing drains here (no `AppHandle`), which is what lets the queue be read
    // as a whole: the post-resume arrival has to land BEHIND the backlog.
    reg.deliver_prompt(&w.id, "arrived after the resume", "orch-1", Delivery::MidSession).unwrap();

    let texts: Vec<String> = reg
        .queue_snapshot(5830)
        .into_iter()
        .filter_map(|e| e.payload.text().map(str::to_string))
        .collect();
    assert_eq!(
        texts,
        vec![
            "held first".to_string(),
            "held second".to_string(),
            "arrived after the resume".to_string(),
        ],
        "pause-held entries keep their arrival order AND stay ahead of post-resume traffic"
    );
}

#[test]
fn a_pause_held_admission_persists_exactly_like_any_other() {
    // #523's persistence table: every `queues` mutation owes a `persist_queues`
    // call. Option 2 routes the pause through `enqueue_text`, which is already
    // a row in that table, precisely so this path adds no new obligation — the
    // discriminator is that what memory holds and what the file holds agree,
    // and that the resume adds nothing of its own on top.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, 5719);
    reg.set_pty_for_test(&w.id, 5695);

    reg.pause_group(&g.id).unwrap();
    reg.deliver_prompt(&w.id, "held for the worker", "orch-1", Delivery::MidSession).unwrap();
    assert_eq!(reg.queue_depth(5695), 1, "precondition: the pause queued it");

    let snap = reg.state_root().join(g.id.as_str()).join("queue.json");
    let before = fs::read_to_string(&snap).expect("the admission owes a snapshot, and paid it");
    assert!(before.contains("held for the worker"), "and the snapshot carries the payload: {before}");

    reg.resume_group(&g.id).unwrap();

    assert_eq!(
        reg.queue_depth(5695),
        1,
        "resume STARTS the drain (no `AppHandle` here, so nothing drains) — it must never \
         pop, re-admit, or duplicate an entry itself"
    );
    assert_eq!(
        reg.queue_depth(5719),
        0,
        "and it must not enqueue anything for the orchestrator: there is no discard to report"
    );
    assert!(
        reg.audit_log(&g.id).iter().all(|e| e.action != "queue-persist-failed"),
        "whatever was persisted, persisted"
    );
}

#[test]
fn queue_pressure_warns_before_the_cap_and_releases_on_evidence() {
    // "The delivery queue filled completely" was, before this, a fact loomux
    // only ever learned at the instant it was already throwing a payload away.
    // There was no state between "fine" and "work has just been lost", so
    // nothing could warn while warning was still useful.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5631u32;

    // Fill to one below the near-full threshold: still ordinary.
    for i in 0..(queue::QUEUE_NEAR_FULL_AT - 1) {
        reg.enqueue_text(&g.id, &w.id, "orrerix", &format!("d-{i}"), pty, queue::EnqueueReason::BehindQueue)
            .unwrap();
    }
    assert!(
        !reg.audit_log(&g.id).iter().any(|e| e.action == "delivery-queue-pressure"),
        "a queue with headroom must stay quiet, or the warning becomes noise nobody reads"
    );
    assert!(reg.stranded_note(&w.id).is_none(), "and unbadged");

    // The admission that crosses the threshold is the last moment a warning
    // can still be a warning.
    reg.enqueue_text(&g.id, &w.id, "orrerix", "crosses", pty, queue::EnqueueReason::BehindQueue).unwrap();
    let pressure: Vec<_> = reg
        .audit_log(&g.id)
        .into_iter()
        .filter(|e| e.action == "delivery-queue-pressure")
        .collect();
    assert_eq!(pressure.len(), 1, "exactly one line on the transition — not one per arrival");
    assert_eq!(pressure[0].detail["state"], "approaching");
    assert_eq!(
        pressure[0].detail["depth"].as_u64(),
        Some(queue::QUEUE_NEAR_FULL_AT as u64)
    );
    assert_eq!(
        reg.stranded_note(&w.id).and_then(|n| n.blocker),
        Some(StrandedBlocker::QueueNearFull),
        "badged through a channel `notify_queue`'s orchestrator suppression cannot swallow"
    );

    // Sitting at pressure is not a new event every time something arrives.
    reg.enqueue_text(&g.id, &w.id, "orrerix", "another", pty, queue::EnqueueReason::BehindQueue).unwrap();
    assert_eq!(
        reg.audit_log(&g.id).iter().filter(|e| e.action == "delivery-queue-pressure").count(),
        1,
        "edge-triggered: a pane parked at 7/8 must not narrate every arrival"
    );

    // Release is on EVIDENCE — a depth that actually came back down — never on
    // elapsed time (.loomux/lessons.md). Drain back under the threshold.
    let entries = reg.queue_snapshot(pty);
    for e in entries.iter().take(2) {
        reg.pop_front_dequeued(&g.id, pty, e.id, e.enqueued_ms);
    }
    assert_eq!(
        reg.stranded_note(&w.id).and_then(|n| n.blocker),
        None,
        "a queue that has genuinely drained releases its pressure badge"
    );
    assert!(
        reg.audit_log(&g.id)
            .iter()
            .any(|e| e.action == "delivery-queue-pressure" && e.detail["state"] == "normal"),
        "and the release is in the record, not just the absence of a badge"
    );
}

#[test]
fn a_queue_full_drop_names_what_it_dropped_and_badges_the_pane() {
    // The work-loss half. At the cap the NEWEST arrival is rejected, and
    // before #563 its audit line recorded only `{to, reason, depth}`: enough
    // to know something was lost, never enough to know what. No id is minted
    // for a rejected entry, so there is nothing else in the log to join
    // against — and `audit.jsonl` rotates, so that line is the only record
    // that will ever exist. A sender can re-send a delivery it can identify.
    //
    // The other half: the rejection was announced only through `notify_queue`
    // (suppressed on an orchestrator target) and a synchronous `Err` to the
    // CALLING agent. On the pane #563 was reported on, that is nothing at all.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5632u32;
    for i in 0..queue::QUEUE_MAX_PER_PANE {
        reg.enqueue_text(&g.id, &w.id, "orrerix", &format!("d-{i}"), pty, queue::EnqueueReason::BehindQueue)
            .unwrap();
    }

    let lost = "review round 3 findings: the gate re-arms on every retry";
    let err = reg
        .enqueue_text(&g.id, &w.id, "orchestrator", lost, pty, queue::EnqueueReason::Arrival)
        .expect_err("at cap, the newest arrival is rejected");
    assert!(err.contains(w.id.as_str()), "the synchronous error still names the target: {err}");

    let dropped = reg
        .audit_log(&g.id)
        .into_iter()
        .filter(|e| e.action == "delivery-dropped")
        .next_back()
        .expect("a rejected delivery must be audited");
    assert_eq!(
        dropped.detail["preview"], lost,
        "the drop must name WHAT was lost — an anonymous drop cannot be re-sent"
    );
    assert_eq!(dropped.detail["from"], "orchestrator", "and who sent it");
    assert_eq!(dropped.detail["bytes"].as_u64(), Some(lost.len() as u64), "and how much was lost");

    assert_eq!(
        reg.stranded_note(&w.id).and_then(|n| n.blocker),
        Some(StrandedBlocker::QueueAtCapacity),
        "and the pane is badged — on an orchestrator pane the badge is the ONLY channel that \
         survives, and before #563 the front door raised none. `QueueAtCapacity`, not \
         `QueueFull`: this caller read a DEPTH, not a refused re-send (rev-10 finding 1)"
    );
}

#[test]
fn a_dropped_preview_is_bounded_single_line_and_marks_its_own_truncation() {
    // A preview that looks complete but isn't is the same unbacked claim
    // .loomux/lessons.md catalogues — a reader would conclude they had seen
    // the whole payload.
    assert_eq!(
        queue::dropped_payload_preview("one\ntwo   three"),
        "one two three",
        "collapsed to one line so it stays readable in a log viewer"
    );
    let long = "x".repeat(queue::DROPPED_PREVIEW_MAX * 3);
    let preview = queue::dropped_payload_preview(&long);
    assert!(preview.ends_with('…'), "truncation is marked, never silent: {preview}");
    assert_eq!(preview.chars().count(), queue::DROPPED_PREVIEW_MAX + 1);
    // Chars, not bytes: slicing UTF-8 at an arbitrary byte offset panics, and
    // this runs on the delivery path where a panic is a silently dead pane.
    let multibyte = "é".repeat(queue::DROPPED_PREVIEW_MAX * 2);
    assert!(queue::dropped_payload_preview(&multibyte).ends_with('…'));
}

#[test]
fn a_dropped_preview_is_idempotent_so_re_collapsing_a_stored_one_is_free() {
    // #632 applies this function a SECOND time, at render time, to a preview
    // that was already bounded when it was audited: `pause_suppression_notice`
    // reads previews back out of a durable `audit.jsonl` that an EARLIER
    // loomux build may have written, and a preview carrying a newline would
    // split an item into two rows with only the first marker-led. Re-collapsing
    // is the cheap structural fix — but only if it is a no-op for text this
    // build wrote, or the notice would silently disagree with the audit line it
    // is reporting, and #579/#630's refusal matcher recomputes this exact
    // function and compares it to the stored `preview` for equality.
    //
    // Both branches have to be checked. The short branch is the easy one; the
    // TRUNCATED branch is the one that could plausibly differ, since its output
    // is `MAX` chars plus a `…` that a second pass has to not re-truncate or
    // double.
    for original in [
        "report: done, PR #123 is green",
        "one\ntwo   three",
        "",
        "   leading and trailing   ",
    ] {
        let once = queue::dropped_payload_preview(original);
        assert_eq!(
            queue::dropped_payload_preview(&once),
            once,
            "re-collapsing a stored preview must be a no-op: {original:?}"
        );
    }
    // The truncated branch, including the case where the cut lands on a space
    // (so the second pass sees `…` preceded by whitespace and must not shift).
    for filler in ["x", "word ", "é", "a b "] {
        let long = filler.repeat(queue::DROPPED_PREVIEW_MAX * 2);
        let once = queue::dropped_payload_preview(&long);
        assert!(once.ends_with('…'), "precondition — this case truncates: {once}");
        assert_eq!(
            queue::dropped_payload_preview(&once),
            once,
            "a truncated preview must survive a second pass unchanged, or #632's render-time \
             re-collapse would rewrite what the audit line recorded: {once:?}"
        );
    }
}

#[test]
fn capacity_state_leaves_headroom_to_warn_in() {
    // A threshold at the cap could never warn in advance, and one too close to
    // it would be crossed and exhausted inside a single drain poll.
    assert!(queue::QUEUE_NEAR_FULL_AT < queue::QUEUE_MAX_PER_PANE);
    assert_eq!(queue::capacity_state(0), queue::CapacityState::Normal);
    assert_eq!(queue::capacity_state(queue::QUEUE_NEAR_FULL_AT - 1), queue::CapacityState::Normal);
    assert_eq!(queue::capacity_state(queue::QUEUE_NEAR_FULL_AT), queue::CapacityState::Approaching);
    assert_eq!(queue::capacity_state(queue::QUEUE_MAX_PER_PANE - 1), queue::CapacityState::Approaching);
    assert_eq!(queue::capacity_state(queue::QUEUE_MAX_PER_PANE), queue::CapacityState::Full);
    assert_eq!(
        queue::capacity_state(queue::QUEUE_MAX_PER_PANE + 5),
        queue::CapacityState::Full,
        "a depth past the cap (recovery re-admission racing an arrival) is still Full, not a panic"
    );
    assert!(
        queue::QUEUE_MAX_PER_PANE - queue::QUEUE_NEAR_FULL_AT >= 2,
        "at least two slots of headroom, or the warning arrives with the loss rather than before it"
    );
}

/// #814: a queued entry, built by hand because these tests are about the
/// READING taken off a queue and what is in it is incidental — the shape
/// `queuestate.rs`'s own tests use, for the same reason.
fn queued_at(id: u64, enqueued_ms: u64) -> queue::QueuedDelivery {
    queue::QueuedDelivery {
        id,
        agent_id: "w-1".to_string(),
        from: "orch-1".to_string(),
        payload: queue::QueuedPayload::Text("hi".to_string()),
        reason: queue::EnqueueReason::Arrival,
        enqueued_ms,
        coalesced: 0,
        group: Some(parse_gid("g1")),
        to_orchestrator: false,
        session_id: None,
        delivery_kind: Delivery::MidSession,
    }
}

#[test]
fn the_queue_badge_measures_the_oldest_entry_not_the_queue_front() {
    // #814 inherits #560's defect exactly, and would have re-introduced it by
    // reading `front().enqueued_ms` — the obvious spelling. A `StrandedSubmit`
    // marker is pushed to the FRONT with a fresh stamp, so on the pane whose
    // prompt has been sitting unsubmitted the longest, the front is the
    // YOUNGEST thing in the queue: a badge keyed on it would reset its own age
    // display at the exact moment the pane got stuck, and read "0s" on a queue
    // that had been waiting an hour.
    let mut q: VecDeque<queue::QueuedDelivery> = VecDeque::new();
    q.push_back(queued_at(1, 1_000_000)); // waiting since long ago
    q.push_front(queued_at(2, 4_000_000)); // the marker, stamped just now
    assert_eq!(
        queue::oldest_enqueued_ms(&q),
        Some(1_000_000),
        "the reading must take the MINIMUM stamp, never the front entry's"
    );

    let item = queue::queue_depth_item(7, "w-1", q.len(), 1_000_000, None, 4_000_000)
        .expect("a non-empty queue always has a reading");
    assert_eq!(item.depth, 2);
    assert_eq!(item.cap, queue::QUEUE_MAX_PER_PANE, "the cap is carried, never re-spelled frontend-side");
    assert_eq!(item.waiting_ms, 3_000_000, "the age is measured from the oldest entry (50 min)");
    assert!(item.stalled, "and 50 minutes of no delivery is exactly what 'stalled' means");
    assert_eq!(queue::oldest_enqueued_ms(&VecDeque::new()), None, "an empty queue has no oldest");
    assert!(
        queue::queue_depth_item(7, "w-1", 0, 0, None, 4_000_000).is_none(),
        "and no badge at all — a pane with nothing queued must be absent from the pushed set"
    );
}

#[test]
fn the_queue_badge_never_reports_a_wait_from_a_clock_that_stepped_backward() {
    // `now_ms` and `enqueued_ms` are the same wall clock, which an NTP
    // correction can step BACKWARD mid-session. An unguarded subtraction wraps
    // to ~584 million years and the badge announces a permanently stalled queue
    // on a pane that is perfectly healthy — the "trains a human to ignore the
    // real one" harm, delivered by arithmetic.
    let item = queue::queue_depth_item(7, "w-1", 1, 5_000_000, None, 4_000_000).unwrap();
    assert_eq!(item.waiting_ms, 0, "a stamp in the future reads as no wait yet");
    assert!(!item.stalled, "and must never be reported as stalled");
}

#[test]
fn the_stall_threshold_sits_between_a_drain_poll_and_the_thirty_minute_notice() {
    // The two neighbours that size it, asserted against the constants
    // themselves so a future retune of either moves this pin instead of
    // breaking it. Too low and ordinary traffic — a burst of worker reports
    // clearing over a few 2 s polls — reads as stalled; too high and the badge
    // is no earlier than the agent-facing notice it exists to pre-empt, which
    // is the whole of #814's complaint.
    assert!(
        queue::QUEUE_STALLED_AFTER > queue::QUEUE_DRAIN_POLL * 4,
        "a handful of drain polls must not be enough to call a queue stalled"
    );
    assert!(
        queue::QUEUE_STALLED_AFTER < queue::QUEUE_STILL_QUEUED_NOTICE_AFTER,
        "a human at the window must learn this before the 30-minute agent notice does"
    );
    let stall = queue::QUEUE_STALLED_AFTER.as_millis() as u64;
    let just_under = queue::queue_depth_item(7, "w-1", 1, 0, None, stall - 1).unwrap();
    assert!(!just_under.stalled, "one millisecond short of the threshold is still merely busy");
    let at = queue::queue_depth_item(7, "w-1", 1, 0, None, stall).unwrap();
    assert!(at.stalled, "and the threshold itself trips it");
}

#[test]
fn the_badge_age_is_coarsened_to_what_it_can_render_and_never_rounds_up() {
    // The rate bound (INV-3's row for `orch-queue-depth`): the reading is
    // re-pushed on the attention tick and skipped when unchanged, so a raw
    // millisecond age — never twice the same — would defeat the skip entirely
    // and emit on every tick for as long as anything is queued. Coarsening to
    // the badge's own resolution is what makes the skip real.
    assert_eq!(queue::coarsen_waiting_ms(0), 0);
    assert_eq!(queue::coarsen_waiting_ms(999), 0, "sub-second is 0s, not a spurious 1s");
    assert_eq!(queue::coarsen_waiting_ms(12_499), 12_000, "1 s resolution under a minute");
    assert_eq!(queue::coarsen_waiting_ms(59_999), 59_000, "and never rounds up across the minute line");
    assert_eq!(queue::coarsen_waiting_ms(60_000), 60_000);
    assert_eq!(queue::coarsen_waiting_ms(119_999), 60_000, "1 min resolution above a minute");
    assert_eq!(queue::coarsen_waiting_ms(3_659_999), 3_600_000);
    for raw in [0u64, 1, 999, 1_000, 59_999, 60_000, 61_500, 600_000, 86_400_000] {
        assert!(
            queue::coarsen_waiting_ms(raw) <= raw,
            "a coarsened age may only ever UNDERSTATE the wait: {raw}"
        );
    }
    // The bound itself, stated as the property that matters: a pane stuck for an
    // hour must not produce a distinct reading on every 3 s tick.
    let a = queue::coarsen_waiting_ms(3_600_000);
    let b = queue::coarsen_waiting_ms(3_603_000);
    assert_eq!(a, b, "two ticks 3 s apart, an hour in, must coarsen to the SAME value or the skip never fires");
}

#[test]
fn queue_depth_snapshot_reports_every_pane_that_has_a_queue_and_no_others() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let a = reg.spawn_agent(&g.id, Role::Worker, "a", "t", false, None).unwrap();
    let b = reg.spawn_agent(&g.id, Role::Worker, "b", "t", false, None).unwrap();
    let (pty_a, pty_b, pty_idle) = (310u32, 305u32, 311u32);

    assert!(
        reg.queue_depth_snapshot(now_ms()).is_empty(),
        "a registry with nothing queued badges nothing — the ordinary state, and the one that has \
         to cost no event at all"
    );

    reg.enqueue_text(&g.id, &a.id, "orrerix", "one", pty_a, queue::EnqueueReason::Arrival).unwrap();
    reg.enqueue_text(&g.id, &a.id, "orrerix", "two", pty_a, queue::EnqueueReason::BehindQueue).unwrap();
    reg.enqueue_text(&g.id, &b.id, "orrerix", "solo", pty_b, queue::EnqueueReason::Arrival).unwrap();

    let items = reg.queue_depth_snapshot(now_ms());
    assert_eq!(
        items.iter().map(|i| i.pty_id).collect::<Vec<_>>(),
        vec![pty_b, pty_a],
        "sorted by pty, so an unchanged reading compares equal to the last one pushed instead of \
         depending on HashMap iteration order"
    );
    assert!(
        !items.iter().any(|i| i.pty_id == pty_idle),
        "a pane with no queue must be ABSENT, not present with depth 0 — absence is how the \
         frontend clears a badge"
    );
    let deep = items.iter().find(|i| i.pty_id == pty_a).unwrap();
    assert_eq!(deep.depth, 2);
    assert_eq!(deep.agent_id, a.id, "the reading names whose pane it is, for the tooltip");
    assert_eq!(deep.cap, queue::QUEUE_MAX_PER_PANE);
    assert!(!deep.stalled, "a queue that has existed for milliseconds is not stalled");

    reg.drop_queue(&g.id, pty_a, queue::DropReason::AgentDied);
    let after = reg.queue_depth_snapshot(now_ms());
    assert_eq!(
        after.iter().map(|i| i.pty_id).collect::<Vec<_>>(),
        vec![pty_b],
        "a pane whose queue is gone leaves the set entirely"
    );
}

#[test]
fn a_pane_held_for_twenty_minutes_reads_stalled_even_when_every_queued_entry_is_young() {
    // The stranded case, end to end through real code: the entries left behind a
    // pasted-but-unsubmitted delivery can all be seconds old while the PANE has
    // been stuck for twenty minutes. Keying the badge on entry stamps alone
    // would show "1s" there — the exact pane #814's incident was about — which
    // is why the reading takes `undelivered_since`, the same clock the
    // 30-minute still-queued notice is measured from.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 312u32;
    let now = now_ms();
    let held_since = now - 20 * 60 * 1_000;

    reg.note_hold(&g.id, &w.id, pty, HoldObservation::HeldPoll, held_since);
    reg.enqueue_text(&g.id, &w.id, "orrerix", "fresh", pty, queue::EnqueueReason::Question).unwrap();

    let item = reg
        .queue_depth_snapshot(now)
        .into_iter()
        .find(|i| i.pty_id == pty)
        .expect("a queued delivery must produce a reading");
    assert!(
        item.waiting_ms >= 19 * 60 * 1_000,
        "the age must come from the pane's open hold episode, not from the young entry: {}",
        item.waiting_ms
    );
    assert!(item.stalled, "twenty minutes of nothing delivered is the state the badge exists to show");
}

#[test]
fn queue_depth_push_emits_only_when_the_reading_the_human_would_see_changed() {
    // INV-3's declared bound for `orch-queue-depth`, pinned rather than asserted
    // in prose: an emit is a JS compile on the webview thread, and this stream
    // rides a 3 s tick, so "skip an unchanged set" is the whole of what keeps it
    // cheap. Nothing else in the app can see this decision — `run_attention`
    // needs an AppHandle no test has — which is exactly why it is a separate,
    // headless function.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 313u32;

    assert!(reg.queue_depth_push(now_ms()).is_none(), "an idle app must emit nothing at all");
    assert!(
        reg.queue_depth_push(now_ms() + 3_000).is_none(),
        "and must keep emitting nothing, tick after tick"
    );

    reg.enqueue_text(&g.id, &w.id, "orrerix", "one", pty, queue::EnqueueReason::Arrival).unwrap();
    // Every `now` below is measured from the entry's OWN stamp rather than from
    // a wall-clock reading taken before the enqueue: the admission does disk I/O
    // (`queue.json`), so on a loaded runner the two can be tens of milliseconds
    // apart — enough to straddle a coarsening boundary and make an assertion
    // about "the same second" depend on how busy CI was.
    let queued_at = reg.queue_snapshot(pty)[0].enqueued_ms;
    let first =
        reg.queue_depth_push(queued_at).expect("a new queue is a change the webview has to be told");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].depth, 1);
    assert!(
        reg.queue_depth_push(queued_at).is_none(),
        "the same tick's identical reading must not be pushed twice"
    );
    assert!(
        reg.queue_depth_push(queued_at + 500).is_none(),
        "nor a reading half a second later, which coarsens to the same second"
    );

    let ticked = reg.queue_depth_push(queued_at + 1_100).expect("a second of age IS a visible change");
    assert_eq!(ticked[0].waiting_ms, 1_000, "…and it is the coarsened age that is pushed");

    reg.enqueue_text(&g.id, &w.id, "orrerix", "two", pty, queue::EnqueueReason::BehindQueue).unwrap();
    let deeper =
        reg.queue_depth_push(queued_at + 1_100).expect("a depth change must reach the webview immediately");
    assert_eq!(deeper[0].depth, 2);

    reg.drop_queue(&g.id, pty, queue::DropReason::AgentDied);
    let drained = reg.queue_depth_push(queued_at + 1_100).expect("a drained queue is a change too");
    assert!(
        drained.is_empty(),
        "and it is pushed as an EMPTY set — the frontend clears a pane's badge by its absence, so \
         skipping this push would leave a dead badge on screen forever"
    );
    assert!(reg.queue_depth_push(queued_at + 1_100).is_none(), "then quiet again");
    assert!(
        reg.queue_depth_push(queued_at + 10 * 60 * 1_000).is_none(),
        "and an EMPTY set is never re-pushed on the heal window below — it paints no badge, so a \
         webview that missed it has nothing to be wrong about, and re-pushing would put an idle \
         app back on a cadence"
    );
}

#[test]
fn the_queue_badge_skip_releases_itself_rather_than_waiting_for_the_reading_to_change() {
    // The independent release (performance.md §2 P4), and the defect it fixes is
    // specific: the skip's signal is a MEMORY of an emit, not an acknowledgement
    // of one. A webview that reloaded — or an emit that never landed — wears no
    // badge, and on a queue stalled for an hour the coarsened reading is
    // deliberately stable, so nothing would ever change to correct it. The pane
    // the badge exists for would be the one pane with no badge.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 314u32;

    reg.enqueue_text(&g.id, &w.id, "orrerix", "one", pty, queue::EnqueueReason::Arrival).unwrap();
    let queued_at = reg.queue_snapshot(pty)[0].enqueued_ms;
    // An hour in, where the coarsening has made two ticks 3 s apart identical —
    // exactly the state an unbounded skip goes silent in.
    let hour = queued_at + 3_600_000;
    let first = reg.queue_depth_push(hour).expect("the reading itself changed, so this pushes");
    assert!(first[0].stalled);
    assert!(reg.queue_depth_push(hour + 3_000).is_none(), "3 s later nothing visible has changed");
    assert!(reg.queue_depth_push(hour + 6_000).is_none(), "…nor 6 s later");

    let healed = reg
        .queue_depth_push(hour + 30_000)
        .expect("but the suppression must release on its own window, not on the reading changing");
    assert_eq!(
        healed, first,
        "and it re-pushes the SAME reading — this is a re-assertion for a webview that may have \
         missed it, not a new fact"
    );
    assert!(reg.queue_depth_push(hour + 32_000).is_none(), "then quiet again until the next window");
}

#[test]
fn the_near_full_badge_does_not_claim_a_delivery_was_already_lost() {
    // A claim is a deliverable, even in a tooltip (.loomux/lessons.md).
    // `QueueFull`'s wording asserts loomux COULD NOT queue something, which is
    // false while the queue is still accepting — the same unbacked-claim class
    // that made `QuestionStale` a separate variant from `Question`.
    let near = stranded_detail("w-1", Some(StrandedBlocker::QueueNearFull));
    let full = stranded_detail("w-1", Some(StrandedBlocker::QueueFull));
    assert_ne!(near, full, "the two states must not read identically");
    assert!(
        !near.contains("could not"),
        "nothing has been lost yet — the badge must not say otherwise: {near}"
    );
    assert!(near.contains("nearly full"), "it names the real state: {near}");
    assert!(
        near.contains("dropped"),
        "and what happens if the pane is left alone, which is the whole point of warning early: {near}"
    );
}

#[test]
fn the_capacity_badges_state_a_hold_as_a_condition_never_as_a_fact() {
    // rev-10 finding 1, and it is the same false-claim class this whole issue
    // exists to eliminate — reached from the other side.
    //
    // `note_queue_capacity` decides on DEPTH ALONE. It never reads
    // `write_admission`, `held_escalation`, or any hold state; the raise sits
    // in `enqueue_text`'s `Admit` arm and runs on every admission. So a pane
    // whose drainer is perfectly healthy but whose senders outrun it — a fleet
    // of workers reporting into one orchestrator pane — reaches these
    // thresholds with NO hold at all. A badge that tells that human to "press
    // Enter or answer what's on screen" sends them hunting for a dialog that
    // does not exist, and a badge that wastes someone's time once is a badge
    // they discount next time. That is the "trains a human to ignore the real
    // one" harm the drainer's ChipGuard exists to prevent.
    for blocker in [StrandedBlocker::QueueNearFull, StrandedBlocker::QueueAtCapacity] {
        let detail = stranded_detail("w-1", Some(blocker));
        assert!(
            detail.contains("If it is held"),
            "{blocker:?} must offer the hold as a condition to check, not assert it: {detail}"
        );
        // The unconditional forms the first cut shipped. Each would be a claim
        // about state this caller has not read.
        for forbidden in ["is held and", "the pane is held —", "press Enter in the pane"] {
            assert!(
                !detail.contains(forbidden),
                "{blocker:?} asserts a hold it never established ({forbidden:?}): {detail}"
            );
        }
    }

    // ...and the depth-derived at-capacity badge is NOT `QueueFull`, whose
    // wording is about a refused self-heal re-send and stranded text in the
    // box — both true where `actuate_stranded` raises it, neither established
    // by a depth reading.
    let at_capacity = stranded_detail("w-1", Some(StrandedBlocker::QueueAtCapacity));
    let refused_resend = stranded_detail("w-1", Some(StrandedBlocker::QueueFull));
    assert_ne!(at_capacity, refused_resend, "two different facts must not share one sentence");
    assert!(
        !at_capacity.contains("re-send"),
        "no re-send was attempted on the depth path: {at_capacity}"
    );

    // The notices carry the same discipline — they are the other half of the
    // same report and would otherwise contradict the badge beside them.
    let pressure = queue::pressure_notice("w-1", 6, 8);
    assert!(pressure.contains("If the pane is held"), "conditional, not asserted: {pressure}");
    assert!(pressure.contains("6/8"), "and it names the numbers it actually read: {pressure}");
    let at_cap = queue::at_capacity_notice("w-1", 8);
    assert!(
        !at_cap.contains("until the pane is released"),
        "a full queue does not imply a held pane: {at_cap}"
    );
    assert!(at_cap.contains("until it drains"), "it says what actually ends the state: {at_cap}");
}

#[test]
fn a_dead_agents_stranded_badge_is_pruned() {
    // The map is latched (nothing about a wedged pane changes on its own), so
    // the one thing that must prune it is the agent going away — otherwise a
    // dead pane's badge outlives it in the registry forever.
    let (reg, _d, g, wid) = attention_setup();
    reg.mark_stranded(&g, &wid, Some(StrandedBlocker::Exhausted));
    assert!(reg.stranded_note(&wid).is_some());

    reg.mark_dead(&wid, Some(1));
    reg.attention_tick(1_000, &HashMap::new(), &no_tails(), &HashMap::new());

    assert!(reg.stranded_note(&wid).is_none(), "a dead agent's badge must not linger in the registry");
}
