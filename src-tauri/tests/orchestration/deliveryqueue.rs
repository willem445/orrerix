//! The delivery queue: coalesced flush, kill-exit notices, restart survival, the staged-orphan archive, read_playbook, and front-door refusals with their roster.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ---------------------------------------------------------------------------
// #533-A — coalesced queue flush: the registry half (the pure planning half
// lives in `queue.rs`'s own unit tests). `run_queue_drainer` itself still
// cannot be exercised here for the reasons the block comment above gives —
// what IS driven below is the exact registry seam the drainer calls once its
// plan is made, which is where a coalesced flush's bookkeeping can actually
// go wrong.
// ---------------------------------------------------------------------------

#[test]
fn a_coalesced_flush_closes_out_every_constituent_on_the_one_submit() {
    // #451 three-state confirmation is per-PANE (`last_delivery` keyed by
    // pty), so one combined paste yields ONE outcome — and that outcome is
    // the outcome of every constituent it carried. Leaving any of them
    // queued would replay text the pane already received.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 141u32;

    for t in ["here is the context", "now do the task", "one correction"] {
        reg.enqueue_text(&g.id, &w.id, "orch", t, pty, queue::EnqueueReason::BehindQueue).unwrap();
    }
    let batch: Vec<(u64, u64)> =
        reg.queue_snapshot(pty).iter().map(|e| (e.id, e.enqueued_ms)).collect();
    assert_eq!(batch.len(), 3);

    reg.pop_batch_dequeued(&g.id, pty, &batch);

    assert_eq!(reg.queue_depth(pty), 0, "one submit must close out ALL of its constituents");
    let dequeued: Vec<_> = reg
        .audit_log(&g.id)
        .into_iter()
        .filter(|e| e.action == "delivery-dequeued")
        .collect();
    assert_eq!(dequeued.len(), 3, "each payload's own id must still close out individually");
    for e in &dequeued {
        assert_eq!(e.detail["combined"], json!(3), "each line names the batch it went out in: {:?}", e.detail);
        assert_eq!(
            e.detail["combined_ids"].as_array().map(|a| a.len()),
            Some(3),
            "a later reader must be able to tell one paste from three: {:?}",
            e.detail
        );
    }
}

#[test]
fn a_single_entry_flush_audits_exactly_as_it_did_before_coalescing_existed() {
    // The uncontended case must be untouched: no batch fields, no claim
    // that anything was combined, because nothing was.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 142u32;

    reg.enqueue_text(&g.id, &w.id, "orch", "alone", pty, queue::EnqueueReason::Arrival).unwrap();
    let e = reg.queue_snapshot(pty)[0].clone();
    reg.pop_batch_dequeued(&g.id, pty, &[(e.id, e.enqueued_ms)]);

    let line = reg
        .audit_log(&g.id)
        .into_iter()
        .find(|a| a.action == "delivery-dequeued")
        .expect("the pop must still be audited");
    assert!(line.detail["combined"].is_null(), "a lone delivery was not combined with anything: {:?}", line.detail);
    assert!(line.detail["combined_ids"].is_null(), "{:?}", line.detail);
    assert_eq!(reg.queue_depth(pty), 0);
}

#[test]
fn drop_superseded_removes_by_id_anywhere_in_the_queue_and_audits_each() {
    // Superseded constituents must leave the queue BEFORE anything is
    // combined — and never silently: a dropped payload is audited on every
    // other drop path, and this one is no different.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 143u32;

    for t in ["first", "second", "third"] {
        reg.enqueue_text(&g.id, &w.id, "orch", t, pty, queue::EnqueueReason::BehindQueue).unwrap();
    }
    let snap = reg.queue_snapshot(pty);
    let (first, middle) = (snap[0].id, snap[1].id);
    reg.drop_superseded(&g.id, pty, &[queue::Superseded { id: middle, by: first }]);

    let snap = reg.queue_snapshot(pty);
    assert_eq!(snap.len(), 2, "only the superseded entry leaves");
    assert_eq!(snap[0].payload.text(), Some("first"), "surviving order is unchanged");
    assert_eq!(snap[1].payload.text(), Some("third"), "surviving order is unchanged");

    let dropped = reg
        .audit_log(&g.id)
        .into_iter()
        .find(|e| e.action == "delivery-dropped" && e.detail["id"] == json!(middle))
        .expect("a superseded drop must never be silent");
    assert_eq!(dropped.detail["reason"], json!("superseded"));
    assert_eq!(dropped.detail["superseded_by"], json!(first), "the record must name the survivor");
}

#[test]
fn a_superseded_drop_moves_its_repeat_count_onto_the_survivor() {
    // rev-13 F4: `admit` bumps the survivor's `coalesced` when it catches a
    // byte-identical repeat at admission; a drain-time drop must fold the
    // same way or the flush header under-reports for exactly the case the
    // drain-time re-check exists to catch.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 146u32;

    reg.enqueue_text(&g.id, &w.id, "orch", "status?", pty, queue::EnqueueReason::Question).unwrap();
    // Two admission-time repeats fold into the survivor the normal way.
    reg.enqueue_text(&g.id, &w.id, "orch", "status?", pty, queue::EnqueueReason::BehindQueue).unwrap();
    reg.enqueue_text(&g.id, &w.id, "orch", "status?", pty, queue::EnqueueReason::BehindQueue).unwrap();
    reg.enqueue_text(&g.id, &w.id, "orch", "unrelated", pty, queue::EnqueueReason::BehindQueue).unwrap();
    let survivor = reg.queue_snapshot(pty)[0].id;
    assert_eq!(reg.queue_snapshot(pty)[0].coalesced, 2, "precondition: admission folded two");

    // An id that is not in the queue must not invent a fold out of nothing.
    reg.drop_superseded(&g.id, pty, &[queue::Superseded { id: survivor + 999, by: survivor }]);
    assert_eq!(
        reg.queue_snapshot(pty)[0].coalesced, 2,
        "an id that isn't in the queue must not invent a fold"
    );

    // Now the real transfer: the second entry is dropped as superseded by
    // the first, and its own weight (itself, +1) moves onto the survivor.
    let other = reg.queue_snapshot(pty)[1].id;
    reg.drop_superseded(&g.id, pty, &[queue::Superseded { id: other, by: survivor }]);
    assert_eq!(
        reg.queue_snapshot(pty)[0].coalesced, 3,
        "the survivor absorbs the dropped entry itself (+1) on top of what it already had"
    );
    let dropped = reg
        .audit_log(&g.id)
        .into_iter()
        .find(|e| e.action == "delivery-dropped" && e.detail["id"] == json!(other))
        .expect("audited");
    assert_eq!(dropped.detail["folded_coalesced"], json!(1), "the transfer is reconstructible");
}

#[test]
fn drop_superseded_is_a_noop_when_nothing_is_superseded() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 144u32;
    reg.enqueue_text(&g.id, &w.id, "orch", "only", pty, queue::EnqueueReason::Arrival).unwrap();

    reg.drop_superseded(&g.id, pty, &[]);
    assert_eq!(reg.queue_depth(pty), 1);
    assert!(
        !reg.audit_log(&g.id).iter().any(|e| e.detail["reason"] == json!("superseded")),
        "an empty supersession list must not manufacture an audit line"
    );
}

#[test]
fn the_planner_and_the_registry_agree_on_what_one_flush_takes() {
    // The seam itself: what `queue::plan_flush` says goes out is exactly
    // what the registry then closes out. Pinned together because a drift
    // between them is silent — the queue would simply keep an entry the
    // paste already carried, and replay it.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 145u32;

    for t in ["a", "b", "c"] {
        reg.enqueue_text(&g.id, &w.id, "orch", t, pty, queue::EnqueueReason::BehindQueue).unwrap();
    }
    let snap = reg.queue_snapshot(pty);
    let plan = queue::plan_flush(&snap, queue::QUEUE_FLUSH_MAX_BYTES);
    assert_eq!(plan.batch.len(), 3, "the whole flushable backlog goes in one pass");

    reg.drop_superseded(&g.id, pty, &plan.superseded);
    let batch: Vec<(u64, u64)> = plan
        .batch
        .iter()
        .filter_map(|id| snap.iter().find(|e| e.id == *id).map(|e| (e.id, e.enqueued_ms)))
        .collect();
    reg.pop_batch_dequeued(&g.id, pty, &batch);
    assert_eq!(reg.queue_depth(pty), 0, "nothing the paste carried may stay queued");
}

// ---------------------------------------------------------------------------
// #533-B — kill-exit notices: audit-only for an exit the orchestrator (or the
// idle reaper) initiated, still a prompt for one nobody asked for.
// ---------------------------------------------------------------------------

#[test]
fn exit_notice_route_demotes_only_a_recorded_initiator() {
    assert_eq!(exit_notice_route(Some(ExitInitiator::Orchestrator)), ExitNoticeRoute::AuditOnly);
    assert_eq!(exit_notice_route(Some(ExitInitiator::IdleTimeout)), ExitNoticeRoute::AuditOnly);
    // #3040 N2's variant. Its own row rather than a reuse of `Orchestrator`, so
    // the audit's `initiator` says what really happened (`planner-completed`).
    assert_eq!(
        exit_notice_route(Some(ExitInitiator::PlannerCompleted)),
        ExitNoticeRoute::AuditOnly
    );
    // A crash, a watchdog-driven death, an agent quitting on its own, a
    // human closing the pane — nobody in this process asked for it, so it
    // is still worth interrupting the orchestrator for.
    assert_eq!(exit_notice_route(None), ExitNoticeRoute::Prompt);
}

#[test]
fn record_exit_initiator_is_first_writer_wins() {
    // Whoever actually caused the exit got there first; a second stamp
    // would rewrite history rather than record it.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();

    assert_eq!(reg.exit_initiator(&w.id), None, "a live agent has no initiator");
    reg.record_exit_initiator(&w.id, ExitInitiator::IdleTimeout);
    reg.record_exit_initiator(&w.id, ExitInitiator::Orchestrator);
    assert_eq!(reg.exit_initiator(&w.id), Some(ExitInitiator::IdleTimeout));
}

#[test]
fn an_orchestrator_initiated_kill_exit_is_audited_not_prompted() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, 950);
    reg.set_pty_for_test(&w.id, 951);

    // The orchestrator asked for this exit (what `kill_agent` records).
    reg.record_exit_initiator(&w.id, ExitInitiator::Orchestrator);
    reg.on_pty_exit(951, Some(0), "", 0, true);

    let log = reg.audit_log(&g.id);
    let demoted = log
        .iter()
        .find(|e| e.action == "agent-exit-notice")
        .expect("the notice must still be recorded, just not delivered");
    assert_eq!(demoted.detail["routed"], json!("audit-only"));
    assert_eq!(demoted.detail["initiator"], json!("orchestrator"));
    assert!(
        demoted.detail["notice"].as_str().unwrap_or_default().contains("exited"),
        "the full notice text must be readable on demand: {:?}",
        demoted.detail
    );
    assert!(
        !log.iter().any(|e| e.action == "prompt"
            && e.detail["text"].as_str().unwrap_or_default().contains("exited")),
        "an exit the orchestrator itself initiated must not cost it a turn"
    );
}

#[test]
fn a_kill_in_the_spawn_to_bind_window_records_no_initiator_and_a_later_crash_still_prompts() {
    // rev-13 F1 (treated as blocking). An agent between `spawn_agent`'s
    // registry insert and its bind has `pty_id: None`. The pre-fix shape
    // stamped `killed_by` unconditionally and killed conditionally, so a
    // `kill_agent` landing in that window killed NOTHING yet left the stamp
    // — and because the stamp is first-writer-wins and never cleared, every
    // later exit of that pane, including a real panic, was demoted to the
    // audit log and the orchestrator was never told. A demotion path that
    // can swallow a crash contradicts the whole reason #533-B demotes
    // anything, so this drives that exact window.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, 990);

    // The window itself: registered, alive, no pty bound yet.
    assert!(
        reg.agent(&w.id).expect("registered").pty_id.is_none(),
        "precondition: this test is only meaningful while the agent is unbound"
    );

    let err = reg.kill_agent_as(&w.id, ExitInitiator::Orchestrator).unwrap_err();
    assert!(
        err.contains("no terminal yet"),
        "a kill that cannot kill must say so rather than report success: {err}"
    );
    assert_eq!(
        reg.exit_initiator(&w.id),
        None,
        "a kill that killed nothing must record no initiator — the stamp is what would demote \
         this pane's next exit forever"
    );
    assert!(
        reg.audit_log(&g.id).iter().any(|e| e.action == "agent-kill-noop"),
        "the refused kill is still audited, so a no-op is discoverable after the fact"
    );

    // The bind completes and the agent runs normally... then genuinely dies.
    reg.set_pty_for_test(&w.id, 991);
    reg.on_pty_exit(991, Some(101), "thread 'main' panicked at 'boom'", 8_192, false);

    let log = reg.audit_log(&g.id);
    assert!(
        log.iter().any(|e| e.action == "prompt"
            && e.detail["to"] == json!(orch.id)
            && e.detail["text"].as_str().unwrap_or_default().contains("exited")),
        "the crash must still reach the orchestrator: {:?}",
        log.iter().map(|e| e.action.clone()).collect::<Vec<_>>()
    );
    assert!(
        !log.iter().any(|e| e.action == "agent-exit-notice"),
        "a real crash must never be demoted to audit-only"
    );
}

#[test]
fn an_exit_nobody_initiated_still_prompts_the_orchestrator() {
    // The polarity that must survive #533: a crash, a watchdog kill, or an
    // unexpected death of a working agent is exactly what the orchestrator
    // cannot reconstruct on its own.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, 960);
    reg.set_pty_for_test(&w.id, 961);

    assert_eq!(reg.exit_initiator(&w.id), None, "nothing in this process asked for this exit");
    reg.on_pty_exit(961, Some(1), "thread 'main' panicked", 4_096, false);

    let log = reg.audit_log(&g.id);
    assert!(
        log.iter().any(|e| e.action == "prompt"
            && e.detail["to"] == json!(orch.id)
            && e.detail["text"].as_str().unwrap_or_default().contains("exited")),
        "an uninitiated exit must still be delivered: {:?}",
        log.iter().map(|e| e.action.clone()).collect::<Vec<_>>()
    );
    assert!(
        !log.iter().any(|e| e.action == "agent-exit-notice"),
        "it was prompted, so there is no demoted record to write"
    );
}

#[test]
fn an_expected_pane_close_with_no_recorded_initiator_still_prompts() {
    // The mutation that proves the routing reads the RECORDED initiator and
    // not `expected`: this exit arrives with `expected: true` (loomux closed
    // the pane — a human closing it, or `end_group`'s teardown, looks
    // exactly like `kill_agent` at the pty layer) but nobody recorded an
    // initiator, so it must still prompt. Routing on `expected` would demote
    // it, which is precisely the misrouting #533-B's recorded state exists
    // to prevent.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, 970);
    reg.set_pty_for_test(&w.id, 971);

    reg.on_pty_exit(971, Some(0), "", 0, true);

    let log = reg.audit_log(&g.id);
    assert!(
        log.iter().any(|e| e.action == "prompt"
            && e.detail["text"].as_str().unwrap_or_default().contains("exited")),
        "`expected` alone must never demote a notice"
    );
    assert!(!log.iter().any(|e| e.action == "agent-exit-notice"));
}

#[test]
fn the_idle_reaper_routes_both_of_its_notices_to_the_audit() {
    // Both notices an idle kill used to produce — the guardrail's own
    // "respawn a worker" line and the exit notice that follows — are
    // audit-only now. The reaper only ever takes IDLE agents, so the whole
    // event is "a slot you weren't using was reclaimed", which
    // `list_agents` answers on demand.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", costed_rails(5, 0)).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let idle = reg.spawn_agent(&g.id, Role::Worker, "idle", "", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, 980);
    reg.set_pty_for_test(&idle.id, 981);

    let killed = reg.reap_idle_agents(u64::MAX / 2);
    assert_eq!(killed, vec![idle.id.clone()], "the idle worker is still reaped");

    let log = reg.audit_log(&g.id);
    let demoted = log
        .iter()
        .find(|e| e.action == "agent-exit-notice" && e.detail["initiator"] == json!("idle-timeout"))
        .expect("the guardrail notice must be recorded where it can be read on demand");
    assert!(
        demoted.detail["notice"].as_str().unwrap_or_default().contains("idle-kill guardrail"),
        "the full wording must survive the demotion: {:?}",
        demoted.detail
    );
    assert!(
        !log.iter().any(|e| e.action == "prompt"
            && e.detail["text"].as_str().unwrap_or_default().contains("idle-kill guardrail")),
        "an idle kill must not cost the orchestrator a turn"
    );
    assert!(
        log.iter().any(|e| e.action == "idle-kill"),
        "the kill itself is audited exactly as before"
    );
    // Residual, stated rather than glossed: the reaper's own
    // `kill_agent_as(.., IdleTimeout)` call cannot complete in test mode
    // (`kill_agent_as` needs an `AppHandle` to reach `PtyManager`, and there
    // is none here — the same boundary every pty-bound path in this file
    // has), so the INITIATOR RECORDING on that path is not pinned here.
    // `record_exit_initiator_is_first_writer_wins` covers the recorder
    // itself and `an_orchestrator_initiated_kill_exit_is_audited_not_
    // prompted` covers what a recorded initiator does to routing; the one
    // untested link is the literal call between them.
}

#[test]
fn drop_queue_audits_every_entry_individually_and_sends_one_coalesced_notice() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 107u32;

    reg.enqueue_text(&g.id, &w.id, "orrerix", "one", pty, queue::EnqueueReason::Question).unwrap();
    reg.enqueue_text(&g.id, &w.id, "orrerix", "two", pty, queue::EnqueueReason::BehindQueue).unwrap();
    let ids: Vec<u64> = reg.queue_snapshot(pty).iter().map(|e| e.id).collect();

    reg.drop_queue(&g.id, pty, queue::DropReason::AgentDied);
    assert_eq!(reg.queue_depth(pty), 0, "the whole queue must be gone");

    let dropped: Vec<_> =
        reg.audit_log(&g.id).into_iter().filter(|e| e.action == "delivery-dropped").collect();
    assert_eq!(dropped.len(), 2, "each entry closes out with its OWN audit line (orphan-scan relies on this)");
    for id in ids {
        assert!(
            dropped.iter().any(|e| e.detail["id"] == json!(id) && e.detail["reason"] == json!("agent-died")),
            "entry {id} must have its own delivery-dropped line"
        );
    }
    // Dropping an already-empty queue must be a true no-op — no duplicate
    // notices or audit spam for a pane with nothing left to drop.
    let before = reg.audit_log(&g.id).len();
    reg.drop_queue(&g.id, pty, queue::DropReason::AgentDied);
    assert_eq!(reg.audit_log(&g.id).len(), before, "dropping an empty queue must not re-audit");
}

#[test]
fn queue_orphans_finds_an_enqueued_entry_with_no_terminal_event() {
    // #445 intake finding: the persistence argument claims a restart's loss
    // is mechanically derivable from audit.jsonl — this is what makes that
    // literally true, exercised end to end through the real audit file
    // (not a hand-built fixture — `queue.rs`'s own unit tests already cover
    // the pure scan logic in isolation).
    //
    // #467 tightened the definition this asserts: an orphan is queued,
    // unresolved AND no longer live. The pre-#467 version of this test
    // simulated a "restart" inside one process, so its unresolved entry was
    // still sitting in the live queue — which the audit derivation cannot
    // tell apart from a lost one, and which `queue_orphans` now excludes
    // (see `queue::merge_orphans`'s doc: reporting an in-flight delivery as
    // lost invites a re-send of something about to arrive). So the restart
    // here is now a real one.
    let dir = tempfile::tempdir().unwrap();
    let (gid, agent, orphaned_id) = {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
        let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
        let pty = 108u32;

        reg.enqueue_text(&g.id, &w.id, "orrerix", "resolved", pty, queue::EnqueueReason::Question).unwrap();
        reg.enqueue_text(&g.id, &w.id, "orrerix", "orphaned", pty, queue::EnqueueReason::BehindQueue).unwrap();
        let snap = reg.queue_snapshot(pty);
        let (resolved_id, orphaned_id) = (snap[0].id, snap[1].id);

        // Only the FIRST entry reaches a terminal audit event before the
        // process dies.
        reg.pop_front_dequeued(&g.id, pty, resolved_id, snap[0].enqueued_ms);
        // While both are still in this process, NEITHER is an orphan: the
        // survivor is pending, not lost.
        assert!(reg.queue_orphans(&g.id).is_empty(),
            "an entry still in the live queue must never be reported as lost work");
        (g.id.clone(), w.id.clone(), orphaned_id)
    };

    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    let orphans = reg.queue_orphans(&gid);
    assert_eq!(orphans.len(), 1, "exactly the unresolved entry must surface, got: {orphans:?}");
    assert_eq!(orphans[0].id, orphaned_id);
    assert_eq!(orphans[0].agent_id, agent);
}

// ---------------------------------------------------------------------------
// #468 / #467 — the delivery queue survives a loomux restart.
//
// Every test below simulates a restart the way the rest of this file already
// does (`relaunch_registry` over the SAME state dir — a fresh in-memory
// registry reading the previous process's files), because that is the only
// honest simulation available: a real restart re-mints pty ids and agent ids,
// and a test that reused either would prove a rebinding mechanism this code
// deliberately does not have. The one thing carried across is what a real
// restore carries across — the group id on disk and a resumed CLI session id.
// ---------------------------------------------------------------------------

/// Enqueue `texts` for a worker in a fresh group and hand back everything a
/// post-restart assertion needs. Kept as a helper because four tests need the
/// identical "a process died with a non-empty queue" starting state and the
/// interesting part of each is what happens AFTER.
fn queued_then_crashed(dir: &Path, texts: &[&str]) -> (GroupId, String, String) {
    let reg = relaunch_registry(dir);
    reg.set_port(45999);
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 220u32;
    for t in texts {
        reg.enqueue_text(&g.id, &w.id, "orch-1", t, pty, queue::EnqueueReason::Arrival).unwrap();
    }
    assert_eq!(reg.queue_depth(pty), texts.len(), "fixture must actually have queued everything");
    let session = w.session_id.clone().expect("a claude worker is assigned a session id at spawn");
    (g.id.clone(), w.id.clone(), session)
}

#[test]
fn a_queued_delivery_survives_a_restart_and_is_re_queued_in_order_for_a_resumed_session() {
    // THE acceptance criterion for #467+#468, end to end: three prompts are
    // queued behind a blocked pane, loomux dies, and after the restart they
    // are back in the live queue — same payloads, same order — for the pane
    // that came back with the same CLI session id.
    //
    // Order is asserted on the queue itself rather than on delivery, because
    // the queue IS the ordering mechanism (#470): every delivery, recovered
    // or fresh, is attempted only from the front of it, so a recovered entry
    // sitting at position 0 is what "delivers first" means here. The paste
    // itself needs a live pty and is hand-validation, per the design note.
    let dir = tempfile::tempdir().unwrap();
    let (gid, _old_agent, session) = queued_then_crashed(dir.path(), &["first", "second", "third"]);

    // ---- the restart ----
    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    assert_eq!(g.id, gid, "the restart must resume the same group");
    assert_eq!(reg.queue_depth(220), 0, "a fresh registry starts with nothing in memory");

    // The orchestrator resumes that worker's session — a NEW agent id and a
    // NEW pty, which is the whole point: neither is what the entries rebind
    // by.
    let resumed = reg
        .spawn_agent_ex(&gid, Role::Worker, None, "w", "", false, None, None,
                        Some(session.clone()), Some(dir.path().to_string_lossy().to_string()), None)
        .unwrap();
    let new_pty = 999u32;
    // This assertion is INVERTED as of #524, and the inversion is the point.
    //
    // It used to read `assert_eq!(resumed.id, _old_agent)` — agent ids were
    // minted off an in-memory counter that restarted with the process, so the
    // first worker of every launch was `w-1` whether or not it was the same
    // worker, and this test pinned that collision as expected behavior. #524
    // made the counter durable, so a restart now resumes ABOVE the previous
    // run's high-water mark and the collision cannot happen.
    //
    // What the line was really defending is unchanged and still defended here:
    // `agent_id` is not the rebinding key, the session id is. That is now shown
    // the stronger way — the recovery below re-binds three payloads to an agent
    // whose id does NOT match the one they were queued for, which no
    // id-comparing `rebinds_to` could do. Restoring an id-equality shortcut
    // would fail on the next four assertions rather than pass silently.
    assert_ne!(resumed.id, _old_agent,
        "a restart must never re-mint a spent agent id (#524) — and the recovery below must \
         still work across the change of id, because it keys on the session");

    let n = reg.readmit_recovered(&gid, &resumed.id, new_pty);
    assert_eq!(n, 3, "every queued payload must come back");

    let snap = reg.queue_snapshot(new_pty);
    let texts: Vec<Option<&str>> = snap.iter().map(|e| e.payload.text()).collect();
    assert_eq!(texts, [Some("first"), Some("second"), Some("third")],
        "arrival order must survive the restart — a recovered backlog delivers oldest first");
    assert!(snap.iter().all(|e| e.reason == queue::EnqueueReason::Recovered),
        "a recovered entry must audit as `recovered`, never as a fresh `arrival`");
    // The payloads are bytes, not summaries: the whole value of persisting is
    // that the orchestrator does not have to re-derive what it already sent.
    assert_eq!(snap[0].from, "orch-1", "provenance survives too");
}

#[test]
fn recovery_reads_the_persisted_snapshot_not_the_audit_log() {
    // The mutation check the brief asks for, made structural rather than
    // procedural: delete the durable artifact and the recovery above must
    // fail. `audit.jsonl` is left completely intact, so if this still
    // recovered three payloads it would prove recovery was reading the audit
    // log (which cannot carry payloads through `queue_orphans`'s snapshot
    // half at all) and that #468's persistence was doing nothing.
    let dir = tempfile::tempdir().unwrap();
    let (gid, _old, session) = queued_then_crashed(dir.path(), &["first", "second", "third"]);

    let snapshot = dir.path().join(gid.as_str()).join("queue.json");
    assert!(snapshot.exists(), "#468: every admission must leave a durable snapshot");
    let body = fs::read_to_string(&snapshot).unwrap();
    assert!(body.contains("\"first\"") && body.contains("\"third\""),
        "the snapshot must carry payload BYTES, not just ids: {body}");
    fs::remove_file(&snapshot).unwrap();

    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    reg.create_group("C:/tmp/repo", rails()).unwrap();
    let resumed = reg
        .spawn_agent_ex(&gid, Role::Worker, None, "w", "", false, None, None,
                        Some(session), Some(dir.path().to_string_lossy().to_string()), None)
        .unwrap();
    assert_eq!(reg.readmit_recovered(&gid, &resumed.id, 999), 0,
        "with no snapshot there is nothing to replay — this is what reddens if persistence is removed");

    // ...but the loss is still VISIBLE, which is the #445 derivation this
    // builds on rather than replaces: the audit scan alone still names the
    // three ids, with no payload to offer.
    let orphans = reg.queue_orphans(&gid);
    assert_eq!(orphans.len(), 3, "the audit-derived fallback still surfaces the loss");
    assert!(orphans.iter().all(|o| o.source == queue::OrphanSource::Audit && o.text.is_none()),
        "an audit-derived orphan knows the id and target but never the payload: {orphans:?}");
}

#[test]
fn an_unbindable_queued_delivery_is_surfaced_as_an_orphan_with_its_payload() {
    // The other half of the acceptance criterion: a worker pane does NOT
    // come back after a restart (loomux says so in its own restore kickoff),
    // so its backlog has nothing to rebind to. It must be SURFACED, not
    // silently dropped — with the payload, so the orchestrator can re-send
    // rather than reconstruct from memory.
    let dir = tempfile::tempdir().unwrap();
    let (gid, old_agent, _session) = queued_then_crashed(dir.path(), &["go do the thing", "and then this"]);

    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    reg.create_group("C:/tmp/repo", rails()).unwrap();

    let orphans = reg.queue_orphans(&gid);
    assert_eq!(orphans.len(), 2, "nothing may vanish: {orphans:?}");
    assert!(orphans.iter().all(|o| o.source == queue::OrphanSource::Snapshot),
        "the snapshot derivation must win over the audit one — only it has the payloads");
    assert_eq!(orphans[0].text.as_deref(), Some("go do the thing"));
    assert_eq!(orphans[1].text.as_deref(), Some("and then this"));
    assert_eq!(orphans[0].agent_id, old_agent, "the orphan names who it was for, stale id and all");
    assert!(orphans[0].id < orphans[1].id, "oldest ask first");

    // A DIFFERENT agent binding must not vacuum up somebody else's backlog:
    // a fresh (non-resumed) worker shares neither the orchestrator role nor
    // the session id, and `agent_id` alone proves nothing after a restart.
    let other = reg.spawn_agent(&gid, Role::Worker, "other", "t", false, None).unwrap();
    assert_eq!(reg.readmit_recovered(&gid, &other.id, 777), 0,
        "an unrelated pane must never inherit a queued backlog");
    assert_eq!(reg.queue_orphans(&gid).len(), 2, "and the orphans must still be there afterwards");
}

#[test]
fn orchestrator_targeted_deliveries_rebind_to_the_restarted_orchestrator_pane() {
    // The live incident this pair of issues was filed from: a restart mid-
    // session wiped in-flight delivery state. Worker reports and loomux
    // notices are queued to the ORCHESTRATOR, whose agent id is re-minted on
    // restore just like everyone else's — `to_orchestrator` is what carries
    // that target across, since a group has exactly one.
    let dir = tempfile::tempdir().unwrap();
    let gid;
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
        gid = g.id.clone();
        let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
        reg.enqueue_text(&g.id, &orch.id, "w-1", "PR #123 is green", 300, queue::EnqueueReason::Arrival).unwrap();
    }

    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    reg.create_group("C:/tmp/repo", rails()).unwrap();
    // A FRESH orchestrator (no resume_session at all) — the entry still
    // rebinds, because "this group's orchestrator" is the durable identity,
    // not the session and not the id.
    let orch = reg.spawn_agent(&gid, Role::Orchestrator, "orch", "", false, None).unwrap();
    assert_eq!(reg.readmit_recovered(&gid, &orch.id, 301), 1,
        "a delivery addressed to the orchestrator must find the restarted orchestrator pane");
    assert_eq!(reg.queue_snapshot(301)[0].payload.text(), Some("PR #123 is green"));
    assert!(reg.queue_orphans(&gid).is_empty(),
        "a re-queued entry must not ALSO be reported as an orphan — it is not lost, it is delivering");
}

#[test]
fn recovery_never_double_delivers() {
    // Three distinct double-delivery routes, all closed:
    //   1. an entry that was already delivered before the crash;
    //   2. re-binding the same pane twice (a second bind, a retry);
    //   3. an entry recovered into a queue that already holds the same text.
    let dir = tempfile::tempdir().unwrap();
    // (1) The pre-crash process delivers the first entry, then dies with the
    // second still queued. The pop is driven through the live registry, the
    // way a real drain does it, so the snapshot update under test is the one
    // production takes.
    let (gid, session) = {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
        let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
        reg.enqueue_text(&g.id, &w.id, "orch-1", "delivered already", 220, queue::EnqueueReason::Arrival).unwrap();
        reg.enqueue_text(&g.id, &w.id, "orch-1", "still waiting", 220, queue::EnqueueReason::Arrival).unwrap();
        let front = reg.queue_snapshot(220)[0].clone();
        reg.pop_front_dequeued(&g.id, 220, front.id, front.enqueued_ms);
        assert_eq!(reg.queue_depth(220), 1, "the delivered entry left the live queue");
        (g.id.clone(), w.session_id.clone().unwrap())
    };

    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    reg.create_group("C:/tmp/repo", rails()).unwrap();
    let resumed = reg
        .spawn_agent_ex(&gid, Role::Worker, None, "w", "", false, None, None,
                        Some(session), Some(dir.path().to_string_lossy().to_string()), None)
        .unwrap();

    // (3) The pane already holds a byte-identical ask when recovery runs —
    // the queue's existing coalesce is what makes recovery idempotent
    // against it, rather than a second dedup mechanism invented here.
    reg.enqueue_text(&gid, &resumed.id, "orch-1", "still waiting", 998, queue::EnqueueReason::Arrival).unwrap();

    let n = reg.readmit_recovered(&gid, &resumed.id, 998);
    assert_eq!(n, 1, "only the undelivered entry is offered back");
    assert_eq!(reg.queue_depth(998), 1,
        "a recovered payload identical to one already queued must COALESCE, never queue twice");
    assert_eq!(reg.queue_snapshot(998)[0].coalesced, 1, "and the collapse must be counted");
    let live = reg.queue_snapshot(998);
    let texts: Vec<Option<&str>> = live.iter().map(|e| e.payload.text()).collect();
    assert!(!texts.contains(&Some("delivered already")),
        "an entry that already reached the pane before the crash must never come back");

    // (2) A second bind of the same pane finds nothing left staged.
    assert_eq!(reg.readmit_recovered(&gid, &resumed.id, 998), 0,
        "re-binding must not replay an already-recovered backlog");
    assert_eq!(reg.queue_depth(998), 1, "and must not grow the queue");
}

#[test]
fn a_stranded_submit_marker_is_never_replayed_across_a_restart() {
    // The one genuinely lossy case, and the reason it must not be quietly
    // replayed: a marker means "text is already in that pane's input box,
    // press Enter." After a restart the box is gone; pressing Enter would
    // submit whatever the NEW session has typed there — a human's half-
    // written line, most likely. Dropped, audited, and announced as lost.
    let dir = tempfile::tempdir().unwrap();
    let gid;
    let session;
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
        gid = g.id.clone();
        let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
        session = w.session_id.clone().unwrap();
        reg.enqueue_text(&g.id, &w.id, "orch-1", "a real payload", 220, queue::EnqueueReason::Arrival).unwrap();
        reg.enqueue_stranded_front(&g.id, &w.id, "orch-1", 220, queue::EnqueueReason::Question)
            .unwrap();
        assert_eq!(reg.queue_snapshot(220)[0].payload, queue::QueuedPayload::StrandedSubmit);
    }

    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    reg.create_group("C:/tmp/repo", rails()).unwrap();
    let resumed = reg
        .spawn_agent_ex(&gid, Role::Worker, None, "w", "", false, None, None,
                        Some(session), Some(dir.path().to_string_lossy().to_string()), None)
        .unwrap();

    assert_eq!(reg.readmit_recovered(&gid, &resumed.id, 999), 1,
        "the TEXT entry comes back; the marker must not");
    let snap = reg.queue_snapshot(999);
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].payload.text(), Some("a real payload"));
    assert!(snap.iter().all(|e| e.payload != queue::QueuedPayload::StrandedSubmit),
        "no marker may ever be re-admitted after a restart");

    // Audited under the SAME reason string the orphan row and the tool
    // description use (review round 1, finding 3 — they had drifted apart,
    // so a human grepping the audit for what the tool showed found nothing),
    // and under a NON-terminal action: a marker never leaves staging, so it
    // must never stop being reported by the audit-derived view either.
    let stranded: Vec<_> = reg
        .audit_log(&gid)
        .into_iter()
        .filter(|e| e.action == "queue-stranded-unreplayable"
            && e.detail["reason"] == json!("stranded-submit-not-replayable"))
        .collect();
    assert_eq!(stranded.len(), 1, "the loss must be named in the audit, not silent");

    // ...and reported through the DURABLE channel, not only the audit line
    // and a best-effort notice: recovery can run at a moment when no
    // orchestrator pane is bound, and `deliver_to_orchestrator` is allowed
    // to fail. "Never silently dropped" cannot rest on a delivery that may
    // not happen.
    let orphans = reg.queue_orphans(&gid);
    assert_eq!(orphans.len(), 1, "the marker must still be surfaced: {orphans:?}");
    assert_eq!(orphans[0].reason, "stranded-submit-not-replayable",
        "and must say WHY it can't be replayed, not why it was queued");
    assert!(orphans[0].text.is_none(), "there are no bytes left to hand back — saying otherwise would be a lie");
}

#[test]
fn a_corrupt_snapshot_costs_only_the_entries_it_ate() {
    // A snapshot is written by a process that may be killed mid-life on a
    // disk that may be full, and read at startup where a panic takes the
    // whole orchestration with it. One unreadable entry must cost that entry
    // and nothing beside it, and the skip must be audited rather than
    // leaving the count silently short.
    let dir = tempfile::tempdir().unwrap();
    let (gid, _old, session) = queued_then_crashed(dir.path(), &["keep me", "and me"]);

    let path = dir.path().join(gid.as_str()).join("queue.json");
    let mut v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    v["entries"].as_array_mut().unwrap().insert(1, json!({ "pty_id": "not-a-number" }));
    fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();

    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    reg.create_group("C:/tmp/repo", rails()).unwrap();
    let resumed = reg
        .spawn_agent_ex(&gid, Role::Worker, None, "w", "", false, None, None,
                        Some(session), Some(dir.path().to_string_lossy().to_string()), None)
        .unwrap();

    assert_eq!(reg.readmit_recovered(&gid, &resumed.id, 999), 2,
        "both readable entries must still recover around the corrupt one");
    let skipped = reg
        .audit_log(&gid)
        .into_iter()
        .find(|e| e.action == "queue-recover-skipped")
        .expect("a skipped entry must be audited, never silently short");
    assert_eq!(skipped.detail["entries"], json!(1));
}

#[test]
fn an_unknown_snapshot_version_recovers_nothing_rather_than_guessing() {
    // Refusing a version this build does not know is the deliberate choice:
    // the failure direction of guessing at an unknown shape is replaying the
    // wrong bytes into somebody's terminal. The audit derivation still
    // surfaces the loss, so refusing is not the same as losing.
    let dir = tempfile::tempdir().unwrap();
    let (gid, _old, session) = queued_then_crashed(dir.path(), &["one", "two"]);
    let path = dir.path().join(gid.as_str()).join("queue.json");
    let mut v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    v["version"] = json!(queue::SNAPSHOT_VERSION + 1);
    fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();

    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    reg.create_group("C:/tmp/repo", rails()).unwrap();
    let resumed = reg
        .spawn_agent_ex(&gid, Role::Worker, None, "w", "", false, None, None,
                        Some(session), Some(dir.path().to_string_lossy().to_string()), None)
        .unwrap();
    assert_eq!(reg.readmit_recovered(&gid, &resumed.id, 999), 0);
    assert_eq!(reg.queue_orphans(&gid).len(), 2,
        "the audit fallback still reports what the snapshot refused to interpret");
}

#[test]
fn the_snapshot_tracks_the_live_queue_through_every_mutation() {
    // #468's actual invariant, stated as one property rather than asserted
    // once at enqueue: whatever the live queue holds is what the file holds.
    // Every mutation path is walked (admit, coalesce, pop, drop) — a future
    // mutator added without a `persist_queues` call reddens here.
    let (reg, dir) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 240u32;
    let path = dir.path().join(g.id.as_str()).join("queue.json");

    let on_disk = |label: &str| -> Vec<String> {
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{label}: no snapshot: {e}"));
        let (entries, skipped) = queue::parse_snapshot(&text);
        assert_eq!(skipped, 0, "{label}: our own writer must never emit an unreadable entry");
        entries.iter().filter_map(|e| e.delivery.payload.text().map(str::to_string)).collect()
    };

    reg.enqueue_text(&g.id, &w.id, "orch-1", "a", pty, queue::EnqueueReason::Arrival).unwrap();
    reg.enqueue_text(&g.id, &w.id, "orch-1", "b", pty, queue::EnqueueReason::Arrival).unwrap();
    assert_eq!(on_disk("after admits"), ["a", "b"]);

    reg.enqueue_text(&g.id, &w.id, "orch-1", "a", pty, queue::EnqueueReason::Arrival).unwrap();
    assert_eq!(on_disk("after coalesce"), ["a", "b"], "a coalesce must not duplicate on disk either");

    let front = reg.queue_snapshot(pty)[0].clone();
    reg.pop_front_dequeued(&g.id, pty, front.id, front.enqueued_ms);
    assert_eq!(on_disk("after pop"), ["b"], "a delivered entry must leave the snapshot immediately");

    reg.drop_queue(&g.id, pty, queue::DropReason::AgentDied);
    assert!(on_disk("after drop").is_empty(), "a dropped queue must leave nothing behind to replay");
}

#[test]
fn the_snapshot_tracks_the_coalesced_flush_paths_too() {
    // #533 (merged as #537) added two queue mutators that this feature's own
    // invariant covers — "every mutation of `queues` rewrites the snapshot" —
    // and it was written against a base where `persist_queues` did not exist,
    // so neither inherited it. Both are load-bearing for durability:
    //
    // - `drop_superseded` removes byte-identical duplicates and moves their
    //   coalesce counts onto the survivor. Without a write, a restart
    //   RESURRECTS a delivery that was deliberately superseded and
    //   under-reports the survivor's fold count.
    // - `pop_batch_dequeued`'s multi-entry branch pops directly rather than
    //   delegating to `pop_front_dequeued`, so it does not inherit that
    //   function's write; a restart would replay an entry already pasted.
    //
    // Asserted against the FILE, not `queue_snapshot`, because the whole
    // question is what survives the process, not what is in memory.
    let (reg, dir) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 260u32;
    let path = dir.path().join(g.id.as_str()).join("queue.json");
    let on_disk = || -> Vec<String> {
        let text = fs::read_to_string(&path).expect("a snapshot must exist");
        queue::parse_snapshot(&text).0.iter()
            .filter_map(|e| e.delivery.payload.text().map(str::to_string)).collect()
    };

    reg.enqueue_text(&g.id, &w.id, "orch-1", "folded away", pty, queue::EnqueueReason::Arrival).unwrap();
    reg.enqueue_text(&g.id, &w.id, "orch-1", "survivor", pty, queue::EnqueueReason::Arrival).unwrap();
    let snap = reg.queue_snapshot(pty);
    let (first, second) = (snap[0].clone(), snap[1].clone());

    // The `Superseded` list is hand-built and fed straight in, deliberately:
    // WHEN `plan_flush` decides something is superseded is #533's policy and
    // #533's tests' business. What is asserted here is only the durability
    // contract this feature imposes on the mutator — that whatever it removes
    // from the live queue also leaves the file.
    reg.drop_superseded(&g.id, pty, &[queue::Superseded { id: first.id, by: second.id }]);
    assert_eq!(on_disk(), ["survivor"],
        "a superseded entry must leave the snapshot, or a restart re-delivers what was folded away");
    assert_eq!(reg.queue_snapshot(pty)[0].coalesced, 1,
        "and the survivor's folded count must be what the snapshot now carries");

    // And a multi-entry batch pop must not leave delivered entries on disk.
    reg.enqueue_text(&g.id, &w.id, "orch-1", "third ask", pty, queue::EnqueueReason::Arrival).unwrap();
    let live = reg.queue_snapshot(pty);
    let batch: Vec<(u64, u64)> = live.iter().map(|e| (e.id, e.enqueued_ms)).collect();
    assert_eq!(batch.len(), 2, "need a real multi-entry batch to exercise the non-delegating branch");
    reg.pop_batch_dequeued(&g.id, pty, &batch);
    assert!(on_disk().is_empty(),
        "entries delivered in one coalesced paste must leave the snapshot, or a restart replays them");
}

#[test]
fn one_groups_snapshot_never_contains_another_groups_queue() {
    // The live queue map is keyed by pty id, which is registry-GLOBAL: two
    // groups share one `OrchRegistry`. Without the entry's own `group` stamp
    // a snapshot would be written per group out of a map that isn't, and a
    // restart would replay one repo's prompts into another's panes.
    let (reg, dir) = test_registry();
    let a = reg.create_group("C:/tmp/repo-a", rails()).unwrap();
    let b = reg.create_group("C:/tmp/repo-b", rails()).unwrap();
    assert_ne!(a.id, b.id);
    let wa = reg.spawn_agent(&a.id, Role::Worker, "wa", "t", false, None).unwrap();
    let wb = reg.spawn_agent(&b.id, Role::Worker, "wb", "t", false, None).unwrap();

    reg.enqueue_text(&a.id, &wa.id, "orch-1", "for group a", 250, queue::EnqueueReason::Arrival).unwrap();
    reg.enqueue_text(&b.id, &wb.id, "orch-1", "for group b", 251, queue::EnqueueReason::Arrival).unwrap();

    for (gid, mine, theirs) in [(&a.id, "for group a", "for group b"), (&b.id, "for group b", "for group a")] {
        let body = fs::read_to_string(dir.path().join(gid.as_str()).join("queue.json")).unwrap();
        assert!(body.contains(mine), "{gid} must persist its own entry: {body}");
        assert!(!body.contains(theirs), "{gid} must NOT persist another group's entry: {body}");
    }
}

#[test]
fn snapshot_order_survives_a_second_restart_when_staging_and_live_mix() {
    // Review round 2, N5. Appending staged entries after live ones is not
    // restart-invariant: an entry rejected at the cap during restart 1 would
    // be written BEHIND deliveries queued after it, and restart 2 would then
    // re-admit them in that inverted order — silently breaking the per-pane
    // arrival order this feature exists to preserve. Ids are monotonic per
    // group and seeded past the snapshot on every recovery, so sorting the
    // file by id is exactly arrival order across any number of restarts.
    let dir = tempfile::tempdir().unwrap();
    let (gid, _old, session) = queued_then_crashed(dir.path(), &["oldest, queued first"]);

    // Restart 1: fill the pane so the recovered entry is rejected back into
    // staging, and queue something NEW after it.
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        reg.create_group("C:/tmp/repo", rails()).unwrap();
        let resumed = reg
            .spawn_agent_ex(&gid, Role::Worker, None, "w", "", false, None, None,
                            Some(session.clone()), Some(dir.path().to_string_lossy().to_string()), None)
            .unwrap();
        let pty = 960u32;
        for i in 0..queue::QUEUE_MAX_PER_PANE {
            reg.enqueue_text(&gid, &resumed.id, "orch-1", &format!("newer-{i}"), pty,
                             queue::EnqueueReason::Arrival).unwrap();
        }
        assert_eq!(reg.readmit_recovered(&gid, &resumed.id, pty), 0, "the full pane rejects it back to staging");
    }

    // The file must read oldest-first regardless of which store each entry
    // was in when it was written.
    let body = fs::read_to_string(dir.path().join(gid.as_str()).join("queue.json")).unwrap();
    let (entries, skipped) = queue::parse_snapshot(&body);
    assert_eq!(skipped, 0);
    let ids: Vec<u64> = entries.iter().map(|e| e.delivery.id).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "the snapshot must be written in id (= arrival) order: {ids:?}");
    assert_eq!(entries[0].delivery.payload.text(), Some("oldest, queued first"),
        "the pre-restart entry arrived first and must still be written first");
}

#[test]
fn a_second_restart_still_reports_a_backlog_nobody_had_read_yet() {
    // Review round 1, finding 1. Restarts cluster — a crash loop, or a fleet
    // restart followed by another — and the exposed set here is the case the
    // feature exists for: entries whose pane did not come back.
    //
    // The sequence that used to lose them permanently and silently: process
    // 2's first touch is an ADMISSION (a notice, a worker report — immediate
    // in a live group), which stages the backlog into memory and, in the
    // same call, rewrote `queue.json` from live state only. Process 2 then
    // dies before the orchestrator's session-start `queue_orphans` call. The
    // audit trail had already been closed by a `delivery-recovered` written
    // at STAGE time, so process 3 found nothing in either view and reported
    // `count: 0` — worse than pre-#468, where the audit scan would at least
    // have kept naming the ids forever.
    let dir = tempfile::tempdir().unwrap();
    let (gid, _old, _session) = queued_then_crashed(dir.path(), &["still owed", "also still owed"]);

    // ---- restart 1: touched only by an admission, then dies ----
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        reg.create_group("C:/tmp/repo", rails()).unwrap();
        let fresh = reg.spawn_agent(&gid, Role::Worker, "unrelated", "t", false, None).unwrap();
        reg.enqueue_text(&gid, &fresh.id, "orch-1", "new work", 930, queue::EnqueueReason::Arrival).unwrap();
        // Nothing here ever calls queue_orphans or binds the original pane.
    }

    // ---- restart 2: the backlog must still be there ----
    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orphans = reg.queue_orphans(&gid);
    let texts: Vec<Option<&str>> = orphans.iter().map(|o| o.text.as_deref()).collect();
    assert!(texts.contains(&Some("still owed")) && texts.contains(&Some("also still owed")),
        "a backlog nobody has read yet must survive any number of restarts, payloads intact: {orphans:?}");
    // Restart 1's own unread admission is owed too — it was live when that
    // process died, which is the ordinary single-restart case.
    assert!(texts.contains(&Some("new work")), "restart 1's own queued work is owed as well: {orphans:?}");
}

#[test]
fn a_readmitted_entry_stops_being_reported_but_a_staged_one_does_not() {
    // The other half of finding 1's fix: closing an id in the audit is now
    // tied to it actually LEAVING staging, so the two must be checked
    // together — a fix that simply stopped closing ids would leak
    // re-delivered work back into the orphan list forever.
    let dir = tempfile::tempdir().unwrap();
    let (gid, _old, session) = queued_then_crashed(dir.path(), &["for the resumed pane"]);

    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    reg.create_group("C:/tmp/repo", rails()).unwrap();
    let resumed = reg
        .spawn_agent_ex(&gid, Role::Worker, None, "w", "", false, None, None,
                        Some(session), Some(dir.path().to_string_lossy().to_string()), None)
        .unwrap();

    assert_eq!(reg.queue_orphans(&gid).len(), 1, "staged and unread: reported");
    assert_eq!(reg.readmit_recovered(&gid, &resumed.id, 931), 1);
    assert!(reg.queue_orphans(&gid).is_empty(),
        "re-queued and delivering: NOT lost work, and must not invite a re-send");

    let closed = reg
        .audit_log(&gid)
        .into_iter()
        .find(|e| e.action == "delivery-recovered")
        .expect("the old id must be closed out exactly when it leaves staging");
    assert!(closed.detail["readmitted_as"].as_u64().is_some(),
        "and must name the fresh id now tracking the payload: {:?}", closed.detail);
}

#[test]
fn a_concurrent_first_touch_after_a_restart_cannot_re_mint_a_snapshot_id() {
    // Review round 1, finding 2. MCP dispatch is thread-per-request, and the
    // moment right after a restart is exactly when every agent in a group
    // reports at once — what #524's fleet restart produced. The guard used
    // to publish "recovered" before the `queue_seq` seed landed, so a second
    // thread arriving mid-read saw "already done", returned, and minted id 1
    // — an id the snapshot still held, silently defeating the earlier
    // hazard-2 fix (orphan hidden by the live-id filter; audit scan
    // cross-closing old ids).
    let dir = tempfile::tempdir().unwrap();
    let backlog: Vec<String> = (0..6).map(|i| format!("recovered-{i}")).collect();
    let refs: Vec<&str> = backlog.iter().map(String::as_str).collect();
    let (gid, _old, _session) = queued_then_crashed(dir.path(), &refs);
    let recovered_ids: Vec<u64> = {
        // Read the ids straight off disk so the expectation does not depend
        // on the recovery path under test.
        let body = fs::read_to_string(dir.path().join(gid.as_str()).join("queue.json")).unwrap();
        queue::parse_snapshot(&body).0.iter().map(|e| e.delivery.id).collect()
    };
    assert_eq!(recovered_ids.len(), 6);

    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    reg.create_group("C:/tmp/repo", rails()).unwrap();
    // Synthetic target ids, deliberately not spawned agents. Two reasons,
    // both worth stating so this doesn't get "fixed" back into six spawns:
    // `rails()` caps a group at 2 live agents, so six is not a state this
    // group can be in; and the race under test is inside recovery's own
    // guard, which every `enqueue_text` runs before it mints regardless of
    // whether the target resolves (an unknown id just yields no durable
    // identity — `durable_target`'s documented fallback).
    let agents: Vec<String> = (0..6).map(|i| format!("w-concurrent-{i}")).collect();

    // Every thread's FIRST act on this group is an admission, all racing the
    // one recovery pass. The barrier is load-bearing (review round 2, N3):
    // without it, thread-spawn skew alone can let the winner finish phase 1 —
    // a small file read plus a parse — before the last threads have even
    // started, so a regression to the pre-fix ordering would slip through a
    // green run. Releasing all six at the same instant is what gives this
    // tripwire a real chance of tripping. It is still probabilistic; the
    // guarantee itself is structural (the guard is held across the seed), and
    // this test's only job is to notice if someone dismantles that structure.
    let gate = std::sync::Barrier::new(agents.len());
    let minted: Vec<u64> = std::thread::scope(|s| {
        let handles: Vec<_> = agents
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let reg = &reg;
                let gid = &gid;
                let gate = &gate;
                s.spawn(move || {
                    gate.wait();
                    reg.enqueue_text(gid, id, "orch-1", &format!("fresh-{i}"), 940 + i as u32,
                                     queue::EnqueueReason::Arrival)
                        .unwrap()
                        .id
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    for id in &minted {
        assert!(!recovered_ids.contains(id),
            "a concurrent first touch minted {id}, which the snapshot still holds ({recovered_ids:?})");
    }
    // And the backlog itself must be intact — one recovery pass, not six.
    let orphans = reg.queue_orphans(&gid);
    assert_eq!(orphans.len(), 6, "the backlog must be staged exactly once: {orphans:?}");
}

#[test]
fn a_restart_never_re_mints_a_queue_id_the_snapshot_already_holds() {
    // `queue_seq` is in-memory and restarts at zero, so a fresh admission
    // after a restart would otherwise be handed id 1 — the same id a
    // recovered entry already carries. Both consumers of an id break
    // silently on that: `queue_orphans`'s live-id filter hides a real orphan
    // behind an unrelated fresh delivery that reused its number, and the
    // audit scan lets the NEW id's `delivery-dequeued` close out the OLD
    // id's `delivery-queued`. Recovery seeds the counter past the snapshot's
    // high-water mark, and every id-minting path runs recovery first.
    let dir = tempfile::tempdir().unwrap();
    let (gid, _old, session) = queued_then_crashed(dir.path(), &["one", "two", "three"]);

    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    reg.create_group("C:/tmp/repo", rails()).unwrap();
    let resumed = reg
        .spawn_agent_ex(&gid, Role::Worker, None, "w", "", false, None, None,
                        Some(session), Some(dir.path().to_string_lossy().to_string()), None)
        .unwrap();

    let fresh = reg
        .enqueue_text(&gid, &resumed.id, "orch-1", "brand new", 910, queue::EnqueueReason::Arrival)
        .unwrap();
    let recovered_ids: Vec<u64> = reg.queue_orphans(&gid).iter().map(|o| o.id).collect();
    assert_eq!(recovered_ids.len(), 3, "the backlog must still be visible: {recovered_ids:?}");
    assert!(!recovered_ids.contains(&fresh.id),
        "a fresh id must never collide with one the snapshot still holds — got {} vs {recovered_ids:?}",
        fresh.id);
    assert!(fresh.id > *recovered_ids.iter().max().unwrap(),
        "the counter must resume ABOVE the snapshot's high-water mark, not merely miss it");
}

#[test]
fn an_admission_never_overwrites_a_snapshot_nobody_has_read_yet() {
    // Recovery is lazy, so the first thing to touch a group after a restart
    // is not guaranteed to be a bind or a `queue_orphans` call — it can be a
    // plain admission, which rewrites `queue.json`. If that write ran before
    // the read, the previous process's whole backlog would be destroyed by
    // the act of queueing one new thing, silently, with nobody having looked
    // at it. `persist_queues` reads first for exactly this.
    let dir = tempfile::tempdir().unwrap();
    let (gid, _old, session) = queued_then_crashed(dir.path(), &["from before the restart"]);

    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    reg.create_group("C:/tmp/repo", rails()).unwrap();
    let resumed = reg
        .spawn_agent_ex(&gid, Role::Worker, None, "w", "", false, None, None,
                        Some(session), Some(dir.path().to_string_lossy().to_string()), None)
        .unwrap();

    // FIRST touch of this group in the new process is an admission, not a
    // bind and not a queue_orphans call.
    reg.enqueue_text(&gid, &resumed.id, "orch-1", "brand new", 900, queue::EnqueueReason::Arrival).unwrap();

    assert_eq!(reg.readmit_recovered(&gid, &resumed.id, 900), 1,
        "the pre-restart backlog must still be there after an unrelated admission overwrote the file");
    let live = reg.queue_snapshot(900);
    let texts: Vec<Option<&str>> = live.iter().map(|e| e.payload.text()).collect();
    assert!(texts.contains(&Some("from before the restart")) && texts.contains(&Some("brand new")));
}

#[test]
fn a_recovered_entry_that_cannot_be_re_admitted_stays_an_orphan() {
    // Re-admission is capped like every other admission (8 per pane). An
    // entry rejected at that cap must go BACK to the staging area and keep
    // being reported — dropping it would be a worse silent loss than the
    // bug this feature fixes, since the sender was told long ago that it was
    // safely queued.
    let dir = tempfile::tempdir().unwrap();
    let backlog: Vec<String> = (0..3).map(|i| format!("recovered-{i}")).collect();
    let refs: Vec<&str> = backlog.iter().map(String::as_str).collect();
    let (gid, _old, session) = queued_then_crashed(dir.path(), &refs);

    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    reg.create_group("C:/tmp/repo", rails()).unwrap();
    let resumed = reg
        .spawn_agent_ex(&gid, Role::Worker, None, "w", "", false, None, None,
                        Some(session), Some(dir.path().to_string_lossy().to_string()), None)
        .unwrap();

    // Fill the pane's queue to the cap with unrelated live traffic first, so
    // the recovered backlog has nowhere to go.
    let pty = 901u32;
    for i in 0..queue::QUEUE_MAX_PER_PANE {
        reg.enqueue_text(&gid, &resumed.id, "orch-1", &format!("live-{i}"), pty, queue::EnqueueReason::Arrival).unwrap();
    }
    assert_eq!(reg.queue_depth(pty), queue::QUEUE_MAX_PER_PANE);

    assert_eq!(reg.readmit_recovered(&gid, &resumed.id, pty), 0, "a full pane can take none of it");
    assert_eq!(reg.queue_depth(pty), queue::QUEUE_MAX_PER_PANE, "and must not be pushed over its cap");

    let orphans = reg.queue_orphans(&gid);
    assert_eq!(orphans.len(), 3, "every rejected entry must still be reported: {orphans:?}");
    assert!(orphans.iter().all(|o| o.text.is_some()), "with its payload intact, still re-sendable");

    // Review round 3, blocking 1: a rejected entry goes back into staging AND
    // onto disk as it is rejected, not at the end of the loop. The design
    // note claims every recovered payload is always in staging, live, or the
    // single entry in flight; if rejections were parked locally until the
    // loop ended, a full pane would put all three outside both durable stores
    // at once and that claim would be false. Reading the snapshot straight
    // off disk is what distinguishes "back in memory" from "durable again".
    let body = fs::read_to_string(dir.path().join(gid.as_str()).join("queue.json")).unwrap();
    let (persisted, skipped) = queue::parse_snapshot(&body);
    assert_eq!(skipped, 0);
    let persisted_texts: Vec<Option<&str>> =
        persisted.iter().map(|e| e.delivery.payload.text()).collect();
    for t in ["recovered-0", "recovered-1", "recovered-2"] {
        assert!(persisted_texts.contains(&Some(t)),
            "rejected entry {t} must be back on disk, not parked in memory: {persisted_texts:?}");
    }
    let rejected = reg
        .audit_log(&gid)
        .into_iter()
        .find(|e| e.action == "delivery-requeue-rejected")
        .expect("a rejected re-admission must be audited, not silent");
    assert_eq!(rejected.detail["count"], json!(3));
}

// ---------- #547: the staged-orphan archive ----------
//
// These helpers all edit `queue.json` as JSON rather than through
// `queue::serialize_snapshot`, for the reason the corrupt-snapshot test above
// does: what is being pinned is what a RESTART reads off disk, and going
// through the writer would test the writer's round trip instead. They also
// spell the policy numbers as literals rather than importing
// `queue::STAGED_*`, so every test here compiles against a build WITHOUT the
// archive and its red run is a behavioral failure rather than a missing name.

/// Make every entry in `group`'s snapshot look `by_ms` older than it is —
/// the only way to reach the age rule without a test that sleeps for a day.
fn age_snapshot(dir: &Path, group: &GroupId, by_ms: u64) {
    let path = dir.join(group.as_str()).join("queue.json");
    let mut v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    for e in v["entries"].as_array_mut().unwrap() {
        let cur = e["enqueued_ms"].as_u64().unwrap();
        e["enqueued_ms"] = json!(cur.saturating_sub(by_ms));
    }
    fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();
}

/// Fan `group`'s snapshot out into `n` fresh-but-unbindable entries carrying
/// `text`, ids ascending from the existing one. Reaching the entry/byte
/// backstops needs more entries than `QUEUE_MAX_PER_PANE` allows a fixture to
/// queue for real, and their AGE must stay recent or the age rule would be
/// what fired.
fn fan_out_snapshot(dir: &Path, group: &GroupId, n: usize, text: &str) -> Vec<u64> {
    let path = dir.join(group.as_str()).join("queue.json");
    let mut v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let template = v["entries"][0].clone();
    let base = template["id"].as_u64().unwrap();
    let mut entries = Vec::new();
    let mut ids = Vec::new();
    for i in 0..n {
        let mut e = template.clone();
        e["id"] = json!(base + i as u64);
        e["payload"] = json!({ "kind": "text", "text": text });
        entries.push(e);
        ids.push(base + i as u64);
    }
    v["entries"] = json!(entries);
    fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    ids
}

/// Every record currently in `group`'s archive file, in file order.
fn archive_rows(dir: &Path, group: &GroupId) -> Vec<Value> {
    let path = dir.join(group.as_str()).join("queue-orphans-archive.jsonl");
    match fs::read_to_string(&path) {
        Ok(t) => t
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Value>(l).expect("every archive line must be one JSON record"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Payload texts `group`'s snapshot currently holds.
fn snapshot_texts(dir: &Path, group: &GroupId) -> Vec<String> {
    let body = fs::read_to_string(dir.join(group.as_str()).join("queue.json")).unwrap();
    let (entries, skipped) = queue::parse_snapshot(&body);
    assert_eq!(skipped, 0, "the fixture must leave a readable snapshot");
    entries.iter().filter_map(|e| e.delivery.payload.text().map(str::to_string)).collect()
}

/// Restart into `dir` and make ONE ordinary admission — the cheapest thing
/// that runs `persist_queues`, which is where recovery stages and where the
/// roll happens. Returns the live registry so the caller can keep asking it
/// questions.
fn restart_and_admit(dir: &Path, gid: &GroupId, pty: u32) -> OrchRegistry {
    let reg = relaunch_registry(dir);
    reg.set_port(45999);
    reg.create_group("C:/tmp/repo", rails()).unwrap();
    let fresh = reg.spawn_agent(gid, Role::Worker, "unrelated", "t", false, None).unwrap();
    reg.enqueue_text(gid, &fresh.id, "orch-1", "ordinary new work", pty, queue::EnqueueReason::Arrival)
        .unwrap();
    reg
}

#[test]
fn a_staged_orphan_past_the_hot_window_moves_off_the_snapshot_and_says_so() {
    // #547's defect, stated as the property it broke: `queue.json` is
    // rewritten and fsynced on EVERY admission and carries the staged set,
    // staging is never cleared, and worker panes do not survive a restart —
    // so every restart permanently adds that restart's backlog to the cost of
    // every future delivery. An entry past the hot window must leave the
    // file, and its departure must be a per-entry audit line naming what and
    // why, not an inference from a file that got smaller.
    let dir = tempfile::tempdir().unwrap();
    let (gid, _old, _session) = queued_then_crashed(dir.path(), &["ancient ask", "also ancient"]);
    // Two days: comfortably past `queue::STAGED_ARCHIVE_AFTER_MS` (24h).
    age_snapshot(dir.path(), &gid, 2 * 24 * 60 * 60 * 1000);

    let reg = restart_and_admit(dir.path(), &gid, 940);

    let texts = snapshot_texts(dir.path(), &gid);
    assert!(!texts.contains(&"ancient ask".to_string()) && !texts.contains(&"also ancient".to_string()),
        "an entry past the hot window must stop being fsynced on every admission: {texts:?}");
    assert!(texts.contains(&"ordinary new work".to_string()),
        "...while the live queue is untouched: {texts:?}");

    let rows = archive_rows(dir.path(), &gid);
    let archived: Vec<&str> =
        rows.iter().filter_map(|r| r["entry"]["payload"]["text"].as_str()).collect();
    assert_eq!(archived, ["ancient ask", "also ancient"],
        "and it must be IN the archive, oldest first, payload intact — moved, not evicted: {rows:?}");

    let lines: Vec<_> = reg.audit_log(&gid).into_iter()
        .filter(|e| e.action == "queue-orphan-archived").collect();
    assert_eq!(lines.len(), 2, "every roll is named individually, never as a silent compaction");
    assert_eq!(lines[0].detail["reason"], json!("staged-past-hot-window"),
        "the audit line says WHY it moved: {:?}", lines[0].detail);
    assert!(lines[0].detail["id"].as_u64().is_some() && lines[0].detail["to"].as_str().is_some(),
        "...and WHAT moved: {:?}", lines[0].detail);
}

#[test]
fn an_archived_orphan_is_still_reported_with_its_payload_and_says_which_file() {
    // The half that makes this a move rather than an eviction. #547 rules out
    // a cap and rules out an audited drop: the staged set exists to report
    // work nobody received, so anything that stops reporting reintroduces the
    // silent loss #523 removed. An archived entry must come back from
    // `queue_orphans` with the same bytes it had, distinguishable only by
    // `source` — which is a THIRD value on that field, so a reader keyed on
    // "snapshot" learns it must look somewhere else rather than seeing a row
    // vanish.
    let dir = tempfile::tempdir().unwrap();
    let (gid, _old, _session) = queued_then_crashed(dir.path(), &["re-send me verbatim"]);
    age_snapshot(dir.path(), &gid, 2 * 24 * 60 * 60 * 1000);
    let reg = restart_and_admit(dir.path(), &gid, 941);

    let out = reg.queue_orphans_json(&gid);
    let rows = out["orphans"].as_array().unwrap();
    let row = rows
        .iter()
        .find(|r| r["text"] == json!("re-send me verbatim"))
        .unwrap_or_else(|| panic!("an archived orphan must still be reported: {out}"));
    assert_eq!(row["source"], json!("archive"),
        "and must say which file holds it, so a human doing forensics opens the right one: {row}");
    assert_eq!(row["truncated"], json!(false));
    assert!(row["queued_minutes_ago"].as_u64().unwrap() >= 24 * 60,
        "staleness is still the reader's judgement to make, so the age must survive the move: {row}");
}

#[test]
fn an_archived_orphan_is_still_re_admitted_when_its_pane_comes_back() {
    // Rolling an entry off the hot file must not quietly cost it its
    // automatic rebind — that would be a recoverable delivery destroyed, just
    // slowly. The archive is read on bind exactly like staging is, oldest id
    // first, and the archived line is removed only once the fresh live entry
    // is already durable.
    let dir = tempfile::tempdir().unwrap();
    let (gid, _old, session) = queued_then_crashed(dir.path(), &["owed to the resumed pane"]);
    age_snapshot(dir.path(), &gid, 2 * 24 * 60 * 60 * 1000);

    let reg = restart_and_admit(dir.path(), &gid, 942);
    assert_eq!(archive_rows(dir.path(), &gid).len(), 1, "precondition: it rolled off the hot file");

    let resumed = reg
        .spawn_agent_ex(&gid, Role::Worker, None, "w", "", false, None, None,
                        Some(session), Some(dir.path().to_string_lossy().to_string()), None)
        .unwrap();
    let pty = 943u32;
    assert_eq!(reg.readmit_recovered(&gid, &resumed.id, pty), 1,
        "an archived entry rebinds on exactly the rule a staged one does");
    let queued = reg.queue_snapshot(pty);
    let live: Vec<Option<&str>> = queued.iter().map(|e| e.payload.text()).collect();
    assert_eq!(live, [Some("owed to the resumed pane")], "with its bytes, through the ordinary front door");

    assert!(archive_rows(dir.path(), &gid).is_empty(),
        "and leaves the archive once it is live again — otherwise it is reported as lost while delivering");
    assert!(reg.queue_orphans(&gid).is_empty(),
        "re-queued and delivering is not lost work, and must not invite a re-send");
    let closed = reg.audit_log(&gid).into_iter()
        .find(|e| e.action == "delivery-recovered" && e.detail["source"] == json!("archive"))
        .expect("the old id must be closed out, and say which store it came from");
    assert!(closed.detail["readmitted_as"].as_u64().is_some(), "{:?}", closed.detail);
}

#[test]
fn a_burst_of_recent_staged_orphans_is_bounded_by_the_entry_backstop() {
    // The age rule alone is not a bound: a crash loop stages a fresh backlog
    // every few seconds, and every one of them is younger than the window. A
    // bound with an "unless it is recent" exception is not a bound, so the
    // entry backstop (`queue::STAGED_HOT_MAX_ENTRIES`, 64) fires regardless of
    // age and takes the OLDEST ids — the ones with the weakest remaining claim
    // on a live pane.
    let dir = tempfile::tempdir().unwrap();
    let (gid, _old, _session) = queued_then_crashed(dir.path(), &["burst"]);
    let ids = fan_out_snapshot(dir.path(), &gid, 70, "burst");

    let reg = restart_and_admit(dir.path(), &gid, 944);

    let rows = archive_rows(dir.path(), &gid);
    let archived: Vec<u64> =
        rows.iter().map(|r| r["entry"]["id"].as_u64().unwrap()).collect();
    assert_eq!(archived, ids[..6], "70 staged, 64 kept hot — the 6 oldest roll: {archived:?}");
    assert_eq!(rows[0]["why"], json!("staged-entry-backstop"),
        "and say which of the three rules moved them: {:?}", rows[0]);
    // The live admission is in the file too, so the staged count is what is
    // being bounded here, not the file's total length.
    assert_eq!(snapshot_texts(dir.path(), &gid).iter().filter(|t| t.as_str() == "burst").count(), 64,
        "the hot snapshot must not carry more staged entries than the backstop allows");
    assert_eq!(reg.queue_orphans(&gid).len(), 70, "and all 70 are still reported — nothing was capped away");
}

#[test]
fn large_staged_payloads_are_bounded_by_the_byte_backstop() {
    // The entry count does not bound the WRITE: a queued payload is a whole
    // task brief and nothing clamps what is stored (`ORPHAN_TEXT_CAP_BYTES`
    // clamps only what `queue_orphans` hands back). Ten 16 KiB briefs are
    // well inside the 64-entry backstop and 2.5x over the 64 KiB one.
    let dir = tempfile::tempdir().unwrap();
    let (gid, _old, _session) = queued_then_crashed(dir.path(), &["big"]);
    let big = "x".repeat(16 * 1024);
    let ids = fan_out_snapshot(dir.path(), &gid, 10, &big);

    restart_and_admit(dir.path(), &gid, 945);

    let rows = archive_rows(dir.path(), &gid);
    let archived: Vec<u64> = rows.iter().map(|r| r["entry"]["id"].as_u64().unwrap()).collect();
    assert_eq!(archived, ids[..6],
        "163_840 staged bytes against a 65_536 cap: the 6 oldest roll until it fits: {archived:?}");
    assert_eq!(rows[0]["why"], json!("staged-byte-backstop"), "{:?}", rows[0]);
    let staged_bytes: usize = snapshot_texts(dir.path(), &gid)
        .iter().filter(|t| t.len() == 16 * 1024).map(|t| t.len()).sum();
    assert!(staged_bytes <= 64 * 1024,
        "the per-admission fsync must stay bounded, not grow with the install: {staged_bytes}");
}

#[test]
fn archiving_the_whole_staged_set_still_cannot_let_a_fresh_id_collide_with_it() {
    // The hazard the archive INTRODUCES, and the one nothing else would
    // catch. `queue_seq` restarts at zero every process and is seeded past
    // the highest id the snapshot holds — but archiving takes ids OUT of the
    // snapshot, so a group whose whole staged set has rolled off presents an
    // empty file and would re-mint ids an archived orphan still carries.
    // Both consequences of that collision are silent: `queue_orphans`'s
    // live-id filter hides a real orphan behind an unrelated fresh delivery,
    // and the audit scan reads the NEW id's `delivery-dequeued` as closing
    // the OLD id's `delivery-queued`.
    let dir = tempfile::tempdir().unwrap();
    let (gid, _old, _session) = queued_then_crashed(dir.path(), &["archived, id still spent"]);
    age_snapshot(dir.path(), &gid, 2 * 24 * 60 * 60 * 1000);

    // Process 2: the roll happens, and its own live entry dies with it.
    {
        restart_and_admit(dir.path(), &gid, 946);
    }
    let archived_ids: Vec<u64> = archive_rows(dir.path(), &gid)
        .iter().map(|r| r["entry"]["id"].as_u64().unwrap()).collect();
    assert_eq!(archived_ids.len(), 1, "precondition: the staged set rolled off entirely");

    // ...and the live admission that triggered the roll was delivered before
    // process 2 died, so what process 3 opens is an EMPTY snapshot beside a
    // non-empty archive. That is the state the seed's blind spot needs, and
    // it is the ordinary one: a delivered entry leaves the file.
    let path = dir.path().join(gid.as_str()).join("queue.json");
    let mut v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    v["entries"] = json!([]);
    fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();

    // Process 3 sees a snapshot with no id in it at all.
    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    reg.create_group("C:/tmp/repo", rails()).unwrap();
    let fresh = reg.spawn_agent(&gid, Role::Worker, "unrelated", "t", false, None).unwrap();
    let minted = reg
        .enqueue_text(&gid, &fresh.id, "orch-1", "brand new", 947, queue::EnqueueReason::Arrival)
        .unwrap();
    assert!(minted.id > archived_ids[0],
        "the id seed must read the archive too — {} would re-use an archived orphan's id {}",
        minted.id, archived_ids[0]);
    let orphans = reg.queue_orphans(&gid);
    assert!(orphans.iter().any(|o| o.id == archived_ids[0] && o.text.is_some()),
        "and the archived orphan must still be reported — a re-used id makes the live filter \
         hide it behind an unrelated fresh delivery: {orphans:?}");
}

/// Append one line to `group`'s archive exactly as given — the only way to put
/// a record this build would NOT have written into the file, which is the
/// whole point of the two tests below.
fn append_archive_line(dir: &Path, group: &GroupId, line: &str) {
    let path = dir.join(group.as_str()).join("queue-orphans-archive.jsonl");
    let mut body = fs::read_to_string(&path).unwrap_or_default();
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(line);
    body.push('\n');
    fs::write(&path, body).unwrap();
}

/// The archive as raw text lines. Deliberately NOT `archive_rows`, which
/// parses every line and would panic on the unreadable one these tests
/// deliberately plant.
fn archive_raw_lines(dir: &Path, group: &GroupId) -> Vec<String> {
    let path = dir.join(group.as_str()).join("queue-orphans-archive.jsonl");
    fs::read_to_string(&path)
        .map(|t| t.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

/// One record from a LATER build, derived from a real one so only the version
/// and the unknown field are unusual: a `v` this build refuses to interpret, a
/// `why` it has never heard of, a field that did not exist, and a high id.
fn newer_build_record(template: &Value, id: u64) -> String {
    let mut newer = template.clone();
    newer["v"] = json!(2);
    newer["why"] = json!("a-reason-invented-later");
    newer["expiry_ms"] = json!(1234);
    newer["entry"]["id"] = json!(id);
    newer["entry"]["payload"] = json!({ "kind": "text", "text": "written by a newer build" });
    serde_json::to_string(&newer).unwrap()
}

#[test]
fn a_rewrite_carries_lines_this_build_cannot_read_through_byte_for_byte() {
    // Review B1. The per-line `v` exists so an append-only file can outlive
    // the build that wrote its older records — so the case it is FOR is a
    // newer build's line being read by an older one. Rebuilding the file from
    // `parse_archive`'s output deletes exactly those lines, which turns the
    // compatibility mechanism into the thing that destroys what it protects,
    // and does it silently. The rewrite must therefore work over raw lines and
    // parse only to answer "is this the record I am removing?".
    let dir = tempfile::tempdir().unwrap();
    let (gid, _old, session) = queued_then_crashed(dir.path(), &["owed to the resumed pane"]);
    age_snapshot(dir.path(), &gid, 2 * 24 * 60 * 60 * 1000);
    let reg = restart_and_admit(dir.path(), &gid, 950);
    let rows = archive_rows(dir.path(), &gid);
    assert_eq!(rows.len(), 1, "precondition: one readable record rolled off");

    let newer_line = newer_build_record(&rows[0], 9000);
    append_archive_line(dir.path(), &gid, &newer_line);
    // ...and a line NO build can ever read — a record torn by a full disk.
    // It cannot be matched, so it must never be removed either.
    let torn = "{\"v\":1,\"archived_ms\":1,\"why\":\"tru";
    append_archive_line(dir.path(), &gid, torn);

    // The bind is what rewrites the archive: it removes the record it just
    // re-admitted, and must touch nothing else.
    let resumed = reg
        .spawn_agent_ex(&gid, Role::Worker, None, "w", "", false, None, None,
                        Some(session), Some(dir.path().to_string_lossy().to_string()), None)
        .unwrap();
    assert_eq!(reg.readmit_recovered(&gid, &resumed.id, 951), 1,
        "precondition: the readable record re-binds, which is what triggers the rewrite");

    let lines = archive_raw_lines(dir.path(), &gid);
    assert!(lines.contains(&newer_line),
        "a newer build's record must survive this build's rewrite BYTE FOR BYTE — not re-serialized, \
         not dropped for being unreadable: {lines:?}");
    assert!(lines.contains(&torn.to_string()),
        "...and so must a line nothing can parse: unreadable is not the same as unwanted: {lines:?}");
    assert!(!lines.iter().any(|l| l.contains("owed to the resumed pane")),
        "while the re-admitted record IS removed — carrying everything through must not mean \
         carrying through the one line the rewrite exists to drop: {lines:?}");
}

#[test]
fn the_id_seed_accounts_for_an_archived_id_on_a_line_it_cannot_interpret() {
    // Review B1's second consequence, same root cause. Reading archived ids
    // through `parse_archive` skips a newer build's record, so its id is
    // invisible to the `queue_seq` seed and a fresh delivery can be minted
    // carrying it — the exact collision the seed exists to prevent, arriving
    // through the compatibility mechanism meant to prevent it.
    let dir = tempfile::tempdir().unwrap();
    let (gid, _old, _session) = queued_then_crashed(dir.path(), &["rolled off"]);
    age_snapshot(dir.path(), &gid, 2 * 24 * 60 * 60 * 1000);
    {
        restart_and_admit(dir.path(), &gid, 952);
    }
    let rows = archive_rows(dir.path(), &gid);
    assert_eq!(rows.len(), 1, "precondition: something rolled off to append beside");
    append_archive_line(dir.path(), &gid, &newer_build_record(&rows[0], 9000));

    // A fresh process: the seed runs once, off the snapshot and the archive.
    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    reg.create_group("C:/tmp/repo", rails()).unwrap();
    let fresh = reg.spawn_agent(&gid, Role::Worker, "unrelated", "t", false, None).unwrap();
    let minted = reg
        .enqueue_text(&gid, &fresh.id, "orch-1", "brand new", 953, queue::EnqueueReason::Arrival)
        .unwrap();
    assert!(minted.id > 9000,
        "the seed must read archived ids RAW — minting {} re-uses an id a newer build's record \
         still carries, and every consequence of that collision is silent", minted.id);
}

#[test]
fn the_per_admission_write_stops_growing_with_the_staged_backlog() {
    // #547's own instruction was to MEASURE before picking a mechanism. This
    // is that measurement, synthetically: 500 staged orphans of 2 KiB each is
    // about what a year of daily restarts leaves on a fleet, and `queue.json`
    // as crafted here is literally the file the pre-#547 writer would have
    // rewritten and fsynced on EVERY admission — so the before/after is over
    // the same staging set rather than over two different fixtures.
    let dir = tempfile::tempdir().unwrap();
    let (gid, _old, _session) = queued_then_crashed(dir.path(), &["flood"]);
    let payload = "z".repeat(2048);
    fan_out_snapshot(dir.path(), &gid, 500, &payload);

    let path = dir.path().join(gid.as_str()).join("queue.json");
    let unbounded = fs::metadata(&path).unwrap().len();

    let reg = restart_and_admit(dir.path(), &gid, 954);
    let bounded = fs::metadata(&path).unwrap().len();

    // A ratio, not a byte count: the point is that the cost stops TRACKING the
    // backlog, and the floor is a constant (`STAGED_HOT_MAX_BYTES`), so the
    // bigger the backlog the wider this gets. 8x is the assertion; the flood
    // above is well past it.
    assert!(bounded * 8 < unbounded,
        "the per-admission write must stop tracking the backlog: {bounded} B after vs {unbounded} B before");
    assert_eq!(reg.queue_orphans(&gid).len(), 500,
        "...and the reduction must be a MOVE, not a saving: every one of the 500 is still reported");
}

#[test]
fn queue_orphans_is_an_orchestrator_only_tool_and_reports_what_a_restart_lost() {
    // The #467 wiring itself: the derivation only becomes a recovery when
    // something the orchestrator's session-start re-sync actually calls can
    // read it. Role gate re-checked at dispatch (the listing filter is
    // cosmetic, not the gate — `add-orch-tool`'s checklist).
    let (reg, _d, co, cw) = setup_mcp();

    let empty = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "queue_orphans", "arguments": {} })).unwrap();
    assert_eq!(empty["isError"], false);
    let v: Value = serde_json::from_str(empty["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(v["count"], json!(0), "the normal case is empty and must say so cleanly");

    let denied = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "queue_orphans", "arguments": {} })).unwrap();
    assert_eq!(denied["isError"], true, "a worker must not read the group's lost-delivery list");

    let listed = dispatch(&reg, &co, "tools/list", &json!({})).unwrap();
    let names: Vec<&str> =
        listed["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"queue_orphans"), "the orchestrator must be able to SEE the tool: {names:?}");
    let worker_listed = dispatch(&reg, &cw, "tools/list", &json!({})).unwrap();
    let worker_names: Vec<&str> =
        worker_listed["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(!worker_names.contains(&"queue_orphans"));
}

#[test]
fn queue_orphans_tool_hands_back_the_payload_a_restart_could_not_deliver() {
    let dir = tempfile::tempdir().unwrap();
    let (gid, _old, _session) = queued_then_crashed(dir.path(), &["re-send me verbatim"]);

    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&gid, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();

    let r = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "queue_orphans", "arguments": {} })).unwrap();
    let v: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(v["count"], json!(1));
    assert_eq!(v["orphans"][0]["text"], json!("re-send me verbatim"),
        "the payload must come back verbatim — a summary is not re-sendable");
    assert_eq!(v["orphans"][0]["source"], json!("snapshot"));
    assert_eq!(v["orphans"][0]["truncated"], json!(false));
    assert!(v["orphans"][0]["queued_minutes_ago"].as_u64().is_some());
}

#[test]
fn an_oversized_orphan_payload_says_it_was_truncated() {
    // A capped payload that read as complete would be the same claim-vs-
    // reality defect this repo's lessons file names: the orchestrator would
    // re-send a silently-shortened brief.
    let dir = tempfile::tempdir().unwrap();
    let big = "x".repeat(queue::ORPHAN_TEXT_CAP_BYTES + 500);
    let (gid, _old, _session) = queued_then_crashed(dir.path(), &[big.as_str()]);

    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&gid, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();
    let r = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "queue_orphans", "arguments": {} })).unwrap();
    let v: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(v["orphans"][0]["truncated"], json!(true));
    assert_eq!(v["orphans"][0]["text_bytes"], json!(big.len()), "the true size is reported, not the cut one");
    assert!(v["orphans"][0]["text"].as_str().unwrap().contains("truncated"),
        "the cut must be visible in the text itself, not only in a sibling field");
}

// ---------------------------------------------------------------------------
// #1683 slice 1 — `read_playbook(section)`, the on-demand playbook mechanism.
//
// The orchestrator's contract splits into a resident core (the system prompt)
// and an on-demand playbook (`<group dir>/orchestrator-playbook.md`), served
// one `## ` section at a time by this orchestrator-only tool. The failure the
// mechanism exists for is NOT an unreadable section — it is an orchestrator
// that never knows to ask — so the resident core keeps the rules and every
// moved section leaves a resident stub naming its trigger. These tests pin
// the tool half of that shape; the stub half is pinned over in
// tests/workflow.rs (`every_playbook_section_has_a_resident_stub_naming_it`).
// ---------------------------------------------------------------------------

/// The cap only binds delegates and `rails()` pins it at 2, and the
/// orchestrator-only test below needs one of each delegate class live.
fn playbook_rails() -> Guardrails {
    Guardrails { max_agents: 8, ..rails() }
}

fn playbook_caller(reg: &OrchRegistry, group: &GroupId, role: Role) -> Caller {
    let a = reg.spawn_agent(group, role, "a", "", false, None).unwrap();
    reg.resolve_token(&a.token).unwrap()
}

fn read_playbook_call(reg: &OrchRegistry, caller: &Caller, section: &str) -> Value {
    dispatch(reg, caller, "tools/call",
        &json!({ "name": "read_playbook", "arguments": { "section": section } })).unwrap()
}

#[test]
fn read_playbook_returns_one_section_by_id_with_vars_substituted() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", playbook_rails()).unwrap();
    let co = playbook_caller(&reg, &g.id, Role::Orchestrator);

    let r = read_playbook_call(&reg, &co, "about-this-playbook");
    assert_eq!(r["isError"], json!(false), "a known section reads: {r}");
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("## About this playbook"),
        "the section comes back whole, heading line included: {text}"
    );
    assert!(
        !text.contains("{{"),
        "the tool serves the RENDERED file, never template bytes: {text}"
    );
    assert!(
        text.contains(g.id.as_str()),
        "value variables are substituted like any instruction file ({{GROUP_ID}}): {text}"
    );
}

#[test]
fn read_playbook_refuses_an_unknown_section_and_names_the_valid_ids() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", playbook_rails()).unwrap();
    let co = playbook_caller(&reg, &g.id, Role::Orchestrator);

    let r = read_playbook_call(&reg, &co, "no-such-section");
    assert_eq!(
        r["isError"],
        json!(true),
        "an unknown section is an ERROR, never an empty-string success — the vacuity \
         control: {r}"
    );
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("no-such-section"), "the refusal names what was asked: {text}");
    assert!(
        text.contains("about-this-playbook"),
        "the refusal names the valid ids, so a mistyped ask is self-correcting: {text}"
    );
}

#[test]
fn read_playbook_is_orchestrator_only() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", playbook_rails()).unwrap();
    // The playbook is the orchestrator's own contract half: a delegate has no
    // business reading it, and the dispatch gate — not the role-filtered
    // listing, which is cosmetic — is the real check (add-orch-tool layer 2).
    for role in [Role::Worker, Role::Reviewer, Role::Planner] {
        let c = playbook_caller(&reg, &g.id, role);
        let r = read_playbook_call(&reg, &c, "about-this-playbook");
        assert_eq!(r["isError"], json!(true), "{role:?} must be refused: {r}");
        let text = r["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("orchestrator-only"), "{role:?} refusal says why: {text}");
    }
}

#[test]
fn read_playbook_never_crosses_groups() {
    let (reg, _d) = test_registry();
    let ga = reg.create_group("C:/tmp/repo-a", playbook_rails()).unwrap();
    let gb = reg.create_group("C:/tmp/repo-b", playbook_rails()).unwrap();
    let ca = playbook_caller(&reg, &ga.id, Role::Orchestrator);

    // The tool takes NO group argument: the group comes from the caller's
    // token, exactly like `group_usage` (#891 S2) — so there is no parameter
    // to misuse, and the section served is the caller's own group's rendered
    // copy (its `{{GROUP_ID}}` names it).
    let r = read_playbook_call(&reg, &ca, "about-this-playbook");
    assert_eq!(r["isError"], json!(false), "{r}");
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains(ga.id.as_str()) && !text.contains(gb.id.as_str()),
        "the served section is the CALLER's group's playbook, never another group's: {text}"
    );
}

#[test]
fn read_playbook_writes_one_audit_line() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", playbook_rails()).unwrap();
    let co = playbook_caller(&reg, &g.id, Role::Orchestrator);
    let before = audit_count(&reg, &g.id, "playbook-read");

    read_playbook_call(&reg, &co, "about-this-playbook");

    assert_eq!(
        audit_count(&reg, &g.id, "playbook-read"),
        before + 1,
        "exactly one playbook-read line per served section — the read is observable \
         (INVARIANT-11 detector feed, #1683 §6)"
    );
    let audit = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl")).unwrap();
    let line = audit
        .lines()
        .filter(|l| l.contains("playbook-read"))
        .last()
        .expect("asserted present above");
    assert!(line.contains("about-this-playbook"), "the line names the section: {line}");
}

/// #1683 slice 2b (#3367 item 4): the long form of the resident tool list and
/// the task-board procedure moved into two playbook sections. Pinned on a REAL
/// group's rendered files (CLAUDE.md: a template-level pin cannot tell "moved"
/// from "lost", and the pre222 strip hides a rendered fragment) and through the
/// tool itself, from the orchestrator's side of the wire.
///
/// Three halves, each able to fail on its own:
/// - every moved marker is SERVED by `read_playbook` under the section its
///   resident stub names — so the stub leads somewhere that carries it;
/// - the resident core names both stubs and keeps the rules those bullets
///   state (the guardrail parenthetical, `claim: true`, `ready: true`,
///   `hot_only`), so nothing a compacted orchestrator must obey went on demand;
/// - the long-form markers are GONE from the core, so this is a move, not a
///   copy that left the resident bytes where they were.
///
/// Every comparison runs on whitespace-collapsed text (review round 1,
/// finding 3): a marker that straddles a hand-wrap makes the ABSENCE half
/// vacuous, since any re-wrap of a copy left in the core would pass it. Collapsed,
/// a marker is a phrase, and no re-wrap on either side changes the verdict.
#[test]
fn the_tool_reference_and_task_board_procedure_are_served_from_the_playbook() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", playbook_rails()).unwrap();
    let co = playbook_caller(&reg, &g.id, Role::Orchestrator);
    // Whitespace-collapsed (see the doc above): line endings and wraps alike.
    fn ws(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }
    let core = ws(&fs::read_to_string(
        reg.state_root().join(g.id.as_str()).join("orchestrator.md"),
    )
    .unwrap());

    // (section id, markers only the moved long form carries)
    let moved: [(&str, &[&str]); 2] = [
        (
            "tool-reference",
            &[
                "## Tool reference",
                "`gh pr checkout <n> --detach`",
                "`include_all: true`",
                "**receive-only**",
                "Capped at 4 live per agent / 12 per group",
                "`top_agents` (top 10 by total",
                "`withdraw_question(id)` takes back one overtaken by events",
                "**Acting on a report — the long form.**",
            ],
        ),
        (
            "task-board",
            &[
                "## Task board",
                "`sprint: 0` to send it back to the backlog",
                "a dep edge that would close a cycle is rejected",
                "It is DISPLAY metadata and nothing gates on it",
                "naming a live task on this board is refused",
                "Deleting a task also strips its id",
            ],
        ),
    ];
    for (id, markers) in moved {
        let r = read_playbook_call(&reg, &co, id);
        assert_eq!(r["isError"], json!(false), "`{id}` must be a served section: {r}");
        let text = ws(r["content"][0]["text"].as_str().unwrap());
        assert!(!text.contains("{{"), "`{id}` is served rendered: {text}");
        for m in markers {
            let m = ws(m);
            assert!(text.contains(&m), "playbook `{id}` has lost moved marker {m:?}:\n{text}");
            if !m.starts_with("## ") {
                assert!(
                    !core.contains(&m),
                    "{m:?} is still in the resident core — the long form was copied, not moved"
                );
            }
        }
        assert!(
            core.contains(&format!("read_playbook(\"{id}\")")),
            "the resident core has no stub naming `{id}`"
        );
    }

    // The rules stay resident, in the rendered core a compacted orchestrator re-reads.
    for rule in [
        "live delegates (workers+reviewers+planners count",
        "**Every fresh spawn must name its capability class**",
        "`worktree: false` is rejected",
        "**No tool on your surface can answer one.**",
        "**No tool on your surface can resolve one**",
        "**Register and immediately move on to other work**",
        "Pass `hot_only: true`",
        "**Assign with `claim: true`, never a plain `assignee` write.**",
        "**\"What's startable\" is `ready: true`, top-of-board first — never a re-derivation.**",
        "**Encode ordering as `deps`, not as prose**",
        "**`blocked` is for blockers OUTSIDE the board.**",
        "**Reopening is a transition too",
        // Review round 1, finding 1: updating `pr_base` is an obligation, not
        // procedure, and a retarget happens long after the board was set up.
        "`pr_base` in the same call as `pr` (and again on a retarget)",
        "**Act on the report; don't re-derive it.**",
    ] {
        assert!(core.contains(&ws(rule)), "the resident core has lost the rule {rule:?}");
    }
    assert!(!core.contains("{{"), "the rendered core carries no raw placeholder");
}

/// The resident core's byte budget (#1683) — ONE definition, quoted by both the
/// assertion and the message it prints.
///
/// The literal used to appear three times: in the comparison, in the failure
/// message's prose, and in the surrounding comment. A change to one of them
/// leaves the message asserting a budget that is not the one enforced, and a
/// message is exactly the surface nobody re-derives (review round 2, N1).
///
/// 35,000 since #1683 slice 2b (#3367 item 4), down from the 45,000 slice 2a
/// set: slice 2a moved whole sections, 2b moved the long form of the tool
/// list and the task-board procedure into the playbook's `tool-reference` and
/// `task-board` sections, leaving every rule resident in a shortened bullet.
const RESIDENT_CORE_BUDGET: usize = 35_000;

#[test]
fn the_resident_core_is_under_the_byte_budget() {
    // The point of #1683: the resident orchestrator template is paid on EVERY
    // model call, in the system cache block. Measured off the template const,
    // never derived — the number in this assertion is the budget, the byte
    // count is the fact.
    //
    // RAW, and raw is honest only because the endings are pinned (#1845).
    // `include_str!` embeds the ON-DISK bytes, so before `.gitattributes`
    // pinned `src-tauri/src/orchestration/templates/**/*.md` to `eol=lf` the same
    // document measured 622 bytes bigger on a CRLF checkout than on an LF one
    // (run 33345301036: 45,327 B on windows-latest, 44,705 B on ubuntu, same
    // blob). #1813 answered that by normalizing before asserting, which made
    // the pin stable and blinded it in the same stroke: the Windows build
    // genuinely paid the bigger prompt on every model call and the assertion
    // could not see it. With the endings pinned, raw and normalized agree on
    // every platform, and raw is the number the product actually pays.
    //
    // What breaks if the pin is removed: this goes back to measuring the
    // checkout. It does so LOUDLY today, but only INCIDENTALLY — measured on
    // `orchestrator.md` at blob ef585635, the LF file is 34,992 B over 509
    // lines, so there are 8 B of margin under this budget against the 509 CR
    // bytes a CRLF checkout adds. The stale worktree therefore fails here
    // rather than passing quietly, but shorten the template past that margin
    // and it goes quiet again, on exactly the platform that pays more. The
    // polarity is a property of the current margin, not a guarantee, which is
    // why `every_prompt_template_is_checked_out_with_lf_endings` below asserts
    // the endings themselves and does not depend on the margin at all.
    assert!(
        ORCHESTRATOR_TPL.len() <= RESIDENT_CORE_BUDGET,
        "the resident core is {} bytes against a {RESIDENT_CORE_BUDGET}-byte budget — \
         sections move to the playbook, they do not get rewritten longer in place \
         (#1683)",
        ORCHESTRATOR_TPL.len()
    );
}

/// The source scan that keeps the tool's section enum honest (#1683 slice 1).
///
/// **The axis is the heading, not a name.** The id set is DERIVED from
/// `ORCHESTRATOR_PLAYBOOK_TPL`'s own `## ` headings — the same
/// `loomux_engine::lessons` splitter `lessons.rs` uses, fenced code excluded
/// so a quoted `## ` line inside an example never becomes a section — and
/// then three denials are asserted, in the default-deny shape the
/// source-scanning-guard convention requires:
///
/// 1. every derived id is unique (two headings that slug to the same id would
///    make one of them unservable while both stubs claim it);
/// 2. `PLAYBOOK_SECTION_IDS` equals the derived set exactly — a row whose
///    section is gone is refused as loudly as a section with no row, because
///    the tool description's index is what tells the orchestrator what exists;
/// 3. the orchestrator's actual tool listing carries exactly those ids and no
///    delegate's listing carries the tool at all — the wiring proof that the
///    const reaches the surface the agent sees.
///
/// **Residual, stated where it is implemented:** this scan reads the TEMPLATE
/// const and the LISTING's description text; it cannot see a description
/// that names ids the template lacks in prose it does not parse, and it does
/// not prove a model reads the index — the stub test above is the pairing
/// half, and the `playbook-read` audit line is the measurement half.
#[test]
fn every_playbook_heading_yields_a_unique_id_and_the_tool_enum_lists_exactly_them() {
    let ids = playbook_section_ids(ORCHESTRATOR_PLAYBOOK_TPL);
    assert!(!ids.is_empty(), "the playbook must carry at least one section");
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "two headings derive the same id: {ids:?}");

    assert_eq!(
        PLAYBOOK_SECTION_IDS, ids.as_slice(),
        "PLAYBOOK_SECTION_IDS has drifted from the playbook's own headings — the enum is a \
         mirror of the template, never an independent list (#1683)"
    );

    let (reg, _d, co, cw) = setup_mcp();
    let listed = dispatch(&reg, &co, "tools/list", &json!({})).unwrap();
    let def = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == json!("read_playbook"))
        .expect("the orchestrator must be able to SEE the tool")
        .clone();
    let desc = def["description"].as_str().unwrap().to_string();
    for id in &ids {
        assert!(
            desc.contains(id),
            "the tool description's index must list `{id}` — a section the stubs name but \
             the index omits is one an orchestrator scanning the surface will never find: {desc}"
        );
    }
    let worker_listed = dispatch(&reg, &cw, "tools/list", &json!({})).unwrap();
    let worker_names: Vec<&str> =
        worker_listed["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(!worker_names.contains(&"read_playbook"), "orchestrator-only surface");
}

// ---------------------------------------------------------------------------
// #579 — deliveries REFUSED at the front door.
//
// `enqueue_text`'s `RejectFull` arm returns before `queue_seq.fetch_add`, so a
// refused delivery never gets an id — and both orphan derivations open and
// close on an id (`queue::orphaned_queue_entries`, `merge_orphans`). It is
// therefore structurally invisible to them, which is why #563 split this out
// rather than folding it into #572's visibility fix. `queue_orphans` reports
// them as a second, audit-derived list.
// ---------------------------------------------------------------------------

/// A refusal line as `enqueue_text`'s `RejectFull` arm writes it. Hand-built
/// only for the shapes a real refusal cannot produce in a headless test (a
/// pre-#563 line, a marker refusal, a recovery re-admission's own reason); the
/// end-to-end tests below drive the real one.
pub(crate) fn refusal_line(ts_ms: u64, from: &str, to: &str, text: &str, enqueue_reason: &str) -> AuditEntry {
    AuditEntry {
        ts_ms,
        actor: "loomux".into(),
        action: "delivery-dropped".into(),
        detail: json!({
            "to": to, "reason": "queue-full-at-call", "depth": queue::QUEUE_MAX_PER_PANE,
            "from": from, "enqueue_reason": enqueue_reason,
            "bytes": text.len(), "preview": queue::dropped_payload_preview(text),
        }),
    }
}

/// The `prompt` line `deliver_prompt` writes — with the FULL text — immediately
/// before it admits, which is what makes a refused payload recoverable at all.
pub(crate) fn prompt_line(ts_ms: u64, from: &str, to: &str, text: &str) -> AuditEntry {
    AuditEntry {
        ts_ms,
        actor: from.into(),
        action: "prompt".into(),
        detail: json!({ "to": to, "text": text }),
    }
}

/// `queue_orphans` through the real MCP dispatch, as an orchestrator reads it.
fn orphans_tool(reg: &OrchRegistry, caller: &Caller) -> Value {
    let r = dispatch(reg, caller, "tools/call",
        &json!({ "name": "queue_orphans", "arguments": {} })).unwrap();
    assert_eq!(r["isError"], false);
    serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap()
}

/// A group whose orchestrator pane is already at capacity, plus the worker that
/// is about to lose a report to it — the case #579 is actually about, since a
/// fleet's reports all converge on that one pane. Returns the group, the two
/// agents and the orchestrator's pty.
fn orch_pane_at_capacity(reg: &OrchRegistry, pty: u32) -> (GroupId, AgentEntry, AgentEntry) {
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, pty);
    // Distinct texts, or `admit`'s byte-identical coalescing would fold them
    // into one entry and the pane would never reach its cap.
    for i in 0..queue::QUEUE_MAX_PER_PANE {
        reg.enqueue_text(&g.id, &orch.id, "orrerix", &format!("[orrerix] advisory {i}"), pty,
            queue::EnqueueReason::Arrival).unwrap();
    }
    assert_eq!(reg.queue_depth(pty), queue::QUEUE_MAX_PER_PANE, "precondition: the pane is full");
    (g.id.clone(), orch, w)
}

#[test]
fn queue_orphans_surfaces_a_front_door_refusal_with_the_payload_it_declined() {
    // #579's acceptance criterion, end to end and through the real tool: a
    // worker's completion report is refused at a full orchestrator pane, and
    // before this nothing the orchestrator ever calls could enumerate it. The
    // `count == 0` assertion is the point of the whole issue — the pre-#579
    // view is not merely unhelpful here, it is structurally blind, because
    // there is no id for it to key on.
    let (reg, _d) = test_registry();
    let pty = 5870u32;
    let (gid, orch, w) = orch_pane_at_capacity(&reg, pty);

    let report = "report: done, PR #77 is green — CI green on all three platforms";
    let err = reg.deliver_prompt(&orch.id, report, &w.id, Delivery::MidSession).unwrap_err();
    assert!(err.contains("NOT queued"), "the SENDER is still told synchronously: {err}");

    let co = reg.resolve_token(&orch.token).unwrap();
    let v = orphans_tool(&reg, &co);
    assert_eq!(v["count"], json!(0),
        "a refusal mints no id, so neither id-keyed orphan derivation can ever see it: {v}");
    assert_eq!(v["refused_count"], json!(1), "and it must not therefore be invisible: {v}");
    assert_eq!(v["refused_omitted"], json!(0));

    let row = &v["refused"][0];
    assert_eq!(row["from"], json!(w.id),
        "the SENDER who lost the work — the audit actor is `loomux`, which did the refusing");
    assert_eq!(row["to"], json!(orch.id));
    assert_eq!(row["text"], json!(report),
        "the payload verbatim, recovered from its own `prompt` line — a preview is not re-sendable");
    assert_eq!(row["bytes"], json!(report.len()));
    assert_eq!(row["truncated"], json!(false));
    assert_eq!(row["payload"], json!("prompt"));
    assert_eq!(row["queue_depth"], json!(queue::QUEUE_MAX_PER_PANE));
    assert_eq!(row["enqueue_reason"], json!("arrival"));
    assert_eq!(row["consequence"], Value::Null, "only a marker refusal has one");
    assert!(row["refused_minutes_ago"].as_u64().is_some(), "age is reported: {row}");
    assert!(row["preview"].as_str().unwrap().contains("PR #77"),
        "the bounded preview is there too, for when `text` cannot be verified: {row}");
    // The claim in `front_door_refusals`'s doc, checked rather than asserted in
    // prose: the group id is what scopes this, not the audit file at large.
    assert_eq!(reg.front_door_refusals(&gid).total, 1);
}

#[test]
fn reporting_a_refusal_never_re_admits_it() {
    // #579 acceptance 3, and the reason refusals are a separate list rather
    // than optional-id rows in the orphan one: an orphan is a payload loomux
    // still HOLDS and re-admits the moment its pane rebinds, while a refusal
    // was declined outright. Re-admitting a declined delivery later would
    // reorder it against everything the pane accepted in the meantime. This
    // holds by construction (the derivation is read-only over `audit.jsonl`),
    // which is exactly the kind of property a later refactor can spend
    // silently — so it is pinned.
    let (reg, _d) = test_registry();
    let pty = 5871u32;
    let (_gid, orch, w) = orch_pane_at_capacity(&reg, pty);
    let before: Vec<u64> = reg.queue_snapshot(pty).iter().map(|e| e.id).collect();

    let report = "report: blocked, needs a human call on #42";
    reg.deliver_prompt(&orch.id, report, &w.id, Delivery::MidSession).unwrap_err();

    let co = reg.resolve_token(&orch.token).unwrap();
    // Twice: staged orphans are never cleared, and a refusal must be just as
    // stable — reading it is not an acknowledgement and must not consume it.
    for pass in 1..=2 {
        let v = orphans_tool(&reg, &co);
        assert_eq!(v["refused_count"], json!(1), "pass {pass}: {v}");
    }
    assert_eq!(reg.queue_depth(pty), queue::QUEUE_MAX_PER_PANE,
        "the pane must not have grown by the refused entry");
    assert_eq!(reg.queue_snapshot(pty).iter().map(|e| e.id).collect::<Vec<u64>>(), before,
        "and not one queued entry may have moved");
    assert!(reg.queue_snapshot(pty).iter().all(|e| e.payload.text() != Some(report)),
        "the refused payload must never appear in the queue it was declined from");
}

#[test]
fn a_recovery_re_admission_refused_at_the_cap_is_reported_once_as_an_orphan() {
    // A rejected re-admission (`readmit_recovered`) writes the SAME
    // `queue-full-at-call` line as a fresh refusal, but that entry goes back
    // into staging and keeps being reported as an orphan — with its payload
    // and by an id. Listing it in `refused` as well would show one lost
    // payload twice in one tool result, and the documented response to both
    // lists is to re-send: a duplicate-delivery generator, the same defect
    // `merge_orphans`'s live-id filter exists to prevent.
    let dir = tempfile::tempdir().unwrap();
    let backlog: Vec<String> = (0..3).map(|i| format!("recovered-{i}")).collect();
    let refs: Vec<&str> = backlog.iter().map(String::as_str).collect();
    let (gid, _old, session) = queued_then_crashed(dir.path(), &refs);

    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    reg.create_group("C:/tmp/repo", rails()).unwrap();
    let resumed = reg
        .spawn_agent_ex(&gid, Role::Worker, None, "w", "", false, None, None,
                        Some(session), Some(dir.path().to_string_lossy().to_string()), None)
        .unwrap();
    let pty = 902u32;
    for i in 0..queue::QUEUE_MAX_PER_PANE {
        reg.enqueue_text(&gid, &resumed.id, "orch-1", &format!("live-{i}"), pty,
            queue::EnqueueReason::Arrival).unwrap();
    }
    assert_eq!(reg.readmit_recovered(&gid, &resumed.id, pty), 0, "a full pane can take none of it");

    // The refusals really were written — this is not a test that passes because
    // nothing happened.
    let refused_lines = reg
        .audit_log(&gid)
        .into_iter()
        .filter(|e| e.action == "delivery-dropped" && e.detail["reason"] == json!("queue-full-at-call"))
        .count();
    assert_eq!(refused_lines, 3, "three re-admissions were refused at the cap");

    assert_eq!(reg.queue_orphans(&gid).len(), 3, "and all three are reported as orphans");
    assert_eq!(reg.front_door_refusals(&gid).total, 0,
        "so none of them may be reported as a refusal too — one loss, one row");
}

#[test]
fn a_refusal_reports_no_text_rather_than_bytes_it_cannot_verify() {
    // The pairing rule is verified, not positional: a `prompt` line is accepted
    // as a refusal's payload only if BOTH its byte length and its recomputed
    // preview match what the refusal itself recorded. The failure direction of
    // guessing here is handing an orchestrator the wrong bytes to paste into
    // somebody's terminal, so a mismatch reports `text: None` and the reader
    // falls back to the preview.
    let matching = refusal_line(9_000, "w-1", "orch-1", "the payload that was refused", "arrival");
    let verified = front_door_refusals(
        &[prompt_line(8_000, "w-1", "orch-1", "the payload that was refused"), matching.clone()],
        false,
    );
    assert_eq!(verified.items[0].text.as_deref(), Some("the payload that was refused"),
        "the happy path: both fingerprints agree");

    // Same sender, same target, same LENGTH — a different payload. The preview
    // check is what refuses it; a length-only join would hand back these bytes.
    let same_length = "the payload that was REFUSED".to_string();
    assert_eq!(same_length.len(), "the payload that was refused".len(), "fixture must be length-equal");
    let wrong =
        front_door_refusals(&[prompt_line(8_000, "w-1", "orch-1", &same_length), matching.clone()], false);
    assert_eq!(wrong.items[0].text, None, "a length-only match must not be trusted");
    assert_eq!(wrong.items[0].preview, queue::dropped_payload_preview("the payload that was refused"),
        "and the preview the refusal recorded is still what the reader gets");

    // A `prompt` line to a DIFFERENT target never pairs, however well it
    // matches — that pane's delivery is not this one.
    let other_target =
        front_door_refusals(&[prompt_line(8_000, "w-1", "w-2", "the payload that was refused"), matching.clone()], false);
    assert_eq!(other_target.items[0].text, None);
    // Nor does one from a different sender.
    let other_sender =
        front_door_refusals(&[prompt_line(8_000, "w-9", "orch-1", "the payload that was refused"), matching], false);
    assert_eq!(other_sender.items[0].text, None);
}

#[test]
fn a_pre_563_refusal_line_is_still_reported_and_says_what_it_cannot_name() {
    // `audit.jsonl` outlives the build that wrote it. Before #563 a refusal
    // recorded only `{to, reason, depth}` — enough to know something was lost,
    // never enough to know what. Such a row must still be REPORTED (the loss
    // is real) while naming nothing it cannot back: no sender, no size, no
    // payload. A row that invented a `from` would be worse than a row that
    // says it does not know.
    let legacy = AuditEntry {
        ts_ms: 7_000,
        actor: "loomux".into(),
        action: "delivery-dropped".into(),
        detail: json!({ "to": "orch-1", "reason": "queue-full-at-call", "depth": 8 }),
    };
    // A `prompt` line that WOULD have matched a modern line is present, and
    // must still not be attached — with no `bytes` recorded there is nothing to
    // verify against, and an unverified guess is the one thing this must not do.
    let r = front_door_refusals(&[prompt_line(6_000, "w-1", "orch-1", "something"), legacy], false);
    assert_eq!(r.total, 1, "the loss is real and must be reported");
    assert_eq!(r.items[0].from, "?", "unknowable, and said so rather than invented");
    assert_eq!(r.items[0].to, "orch-1");
    assert_eq!(r.items[0].bytes, None);
    assert_eq!(r.items[0].text, None);
    assert_eq!(r.items[0].preview, "");
    assert_eq!(r.items[0].payload, RefusedPayload::Prompt);
}

#[test]
fn a_refused_stranded_marker_reports_the_pane_state_it_left_behind() {
    // The other front-door refusal: `audit_stranded_push`'s rejection of a
    // `StrandedSubmit` marker. It never carried text — the bytes were already
    // pasted into the pane and only the Enter was queued — so "re-send it" is
    // the wrong instruction and the row must not read like the prompt case.
    // Its `consequence` is carried through verbatim from the audit line rather
    // than re-worded here: one string, every channel.
    let marker = AuditEntry {
        ts_ms: 5_000,
        actor: "loomux".into(),
        action: "delivery-dropped".into(),
        detail: json!({
            "to": "w-3", "reason": "queue-full-at-call", "depth": 8,
            "payload": "stranded-submit",
            "consequence": "text is pasted in the pane with nothing queued to submit it",
        }),
    };
    let r = front_door_refusals(&[marker], false);
    assert_eq!(r.items[0].payload, RefusedPayload::StrandedSubmit);
    assert_eq!(r.items[0].text, None, "there are no bytes to re-send, and never were");
    assert_eq!(r.items[0].bytes, None);
    assert_eq!(r.items[0].enqueue_reason, None, "a marker is not admitted under an EnqueueReason");
    // Deliberately the PRE-RENAME sender (#1153 phase 3): this row is read back
    // out of a record written before the flag day, and `is_host_actor` accepting
    // it is the compatibility claim. The current spelling is covered by every
    // other host-send specimen in this file.
    assert_eq!(r.items[0].from, "loomux", "the marker push is this app's own act");
    assert_eq!(
        r.items[0].consequence.as_deref(),
        Some("text is pasted in the pane with nothing queued to submit it"),
        "the reader needs to know to look at the pane, not to re-send"
    );
}

#[test]
fn a_parked_orchestrator_notice_is_not_a_refusal() {
    // #624 (#578) landed a notice channel for the orchestrator's own pane while
    // #579 was in review, and its audit line reads temptingly like this list's
    // subject: a queue notice that was not delivered. It is not one. It was
    // suppressed because of WHO the target is, not refused at the cap, and it
    // is not lost — it is parked and relayed on that pane's next tool result.
    // Listing a delivered notice in a list whose documented response is
    // "re-send what still applies" would manufacture duplicate notices, the
    // same failure the `recovered` exclusion prevents.
    //
    // Pinned on the real shape `notify_queue` writes (`notice-suppressed`,
    // `parked: true`, `reason: target-is-orchestrator`) so this stays a
    // property of the filter rather than an accident of it: if someone widens
    // the scan from `queue-full-at-call` to "any dropped-ish line", this fails.
    let parked = AuditEntry {
        ts_ms: 4_000,
        actor: "loomux".into(),
        action: "notice-suppressed".into(),
        detail: json!({
            "kind": "queue", "to": "orch-1", "reason": "target-is-orchestrator",
            "parked": true,
            "text": "[orrerix] delivery to orch-1 queued (an interactive question is on screen)",
        }),
    };
    let real = refusal_line(4_100, "w-1", "orch-1", "a genuinely refused report", "arrival");
    let r = front_door_refusals(&[parked, real], false);
    assert_eq!(r.total, 1, "only the real refusal counts: {:?}", r.items);
    assert_eq!(r.items[0].preview, queue::dropped_payload_preview("a genuinely refused report"),
        "and it is the refusal, not the parked notice");
}

#[test]
fn the_refused_list_is_capped_and_says_how_many_it_left_out() {
    // Unlike orphans, which the per-pane cap of 8 already bounds, refusals
    // accumulate without limit: a pane held at capacity refuses every arrival
    // for as long as it stays there. So the list is capped — and a capped list
    // reported as complete is the silent truncation `.loomux/lessons.md` names,
    // which is what `refused_count`/`refused_omitted` exist to prevent.
    let (reg, _d) = test_registry();
    let pty = 5872u32;
    let (_gid, orch, w) = orch_pane_at_capacity(&reg, pty);
    let extra = 2;
    for i in 0..REFUSED_LIST_MAX + extra {
        reg.deliver_prompt(&orch.id, &format!("lost report {i}"), &w.id, Delivery::MidSession)
            .unwrap_err();
    }

    let co = reg.resolve_token(&orch.token).unwrap();
    let v = orphans_tool(&reg, &co);
    assert_eq!(v["refused_count"], json!(REFUSED_LIST_MAX + extra), "the true total, not the listed one");
    assert_eq!(v["refused"].as_array().unwrap().len(), REFUSED_LIST_MAX);
    assert_eq!(v["refused_omitted"], json!(extra), "and what was left in audit.jsonl is stated");
    // The MOST RECENT survive, in chronological order: a pane at capacity keeps
    // refusing, so the newest refusals are the ones still likely to matter.
    assert_eq!(v["refused"][0]["text"], json!(format!("lost report {extra}")));
    assert_eq!(v["refused"][REFUSED_LIST_MAX - 1]["text"],
        json!(format!("lost report {}", REFUSED_LIST_MAX + extra - 1)));
}

/// One audit entry as a `audit.jsonl` line — the four keys
/// `parse_audit_lines` reads. Lets a test lay down a timeline LONGER than
/// `AUDIT_VIEW_LIMIT` in one write instead of 5000 `reg.audit()` appends.
pub(crate) fn audit_jsonl_line(e: &AuditEntry) -> String {
    json!({ "ts_ms": e.ts_ms, "actor": e.actor, "action": e.action, "detail": e.detail }).to_string()
}

#[test]
fn a_truncated_audit_window_is_reported_as_truncated_not_as_complete() {
    // Review NB1. `audit_log` keeps only the most recent `AUDIT_VIEW_LIMIT`
    // entries, so `refused_count` was never a count of a group's refusals —
    // only of those in the readable tail. Composed with `refused_omitted`
    // (total minus listed) that produced the strongest claim the shape can
    // make from the weakest evidence: a group whose refusals sit in the older
    // half reports `refused_count: 0, refused_omitted: 0`, which reads as
    // "nothing was ever refused" from a scan that never saw the evidence —
    // the exact silent truncation #579's own design note promised not to do.
    //
    // The fixture puts three refusals at the START of a timeline longer than
    // the window and one at the END, so the window provably cuts real
    // refusals away: the test asserts the flag is what tells the reader so.
    let (reg, d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();

    let mut lines: Vec<String> = Vec::new();
    for i in 0..3 {
        let text = format!("an OLD refused report {i}");
        lines.push(audit_jsonl_line(&prompt_line(1_000, "w-1", &orch.id, &text)));
        lines.push(audit_jsonl_line(&refusal_line(1_001, "w-1", &orch.id, &text, "arrival")));
    }
    // Enough filler to push those six entries out of the window entirely.
    for i in 0..AUDIT_VIEW_LIMIT {
        lines.push(audit_jsonl_line(&AuditEntry {
            ts_ms: 2_000,
            actor: "loomux".into(),
            action: "seeded".into(),
            detail: json!({ "seq": i }),
        }));
    }
    let recent = "the one refused report still in the window";
    lines.push(audit_jsonl_line(&prompt_line(3_000, "w-2", &orch.id, recent)));
    lines.push(audit_jsonl_line(&refusal_line(3_001, "w-2", &orch.id, recent, "arrival")));
    // Written last: `create_group`/`spawn_agent` audit lines of their own, and
    // this replaces the whole log with a timeline whose length is the point.
    fs::write(d.path().join(g.id.as_str()).join("audit.jsonl"), lines.join("\n") + "\n").unwrap();

    let r = reg.front_door_refusals(&g.id);
    assert!(r.window_truncated,
        "the window really was cut ({} entries written, limit {AUDIT_VIEW_LIMIT}) and the \
         derivation must say so", lines.len());
    assert_eq!(r.total, 1, "only the refusal inside the window is countable: {:?}", r.items);
    assert_eq!(r.items[0].text.as_deref(), Some(recent));

    let v = orphans_tool(&reg, &co);
    assert_eq!(v["refused_window_truncated"], json!(true),
        "and the flag must reach the tool result, not stop at the Rust struct: {v}");
    // The trap this closes, spelled out: these two fields are the ones a reader
    // would otherwise take as "that was all of them".
    assert_eq!(v["refused_count"], json!(1));
    assert_eq!(v["refused_omitted"], json!(0),
        "the LIST cap dropped nothing here — which is exactly why a reader needs a separate \
         signal for the WINDOW cap having dropped three");
}

#[test]
fn an_untruncated_window_says_its_count_is_complete() {
    // The other half of the flag, and the reason it is not simply hardcoded
    // true: an ordinary group's count really is complete, and a derivation that
    // always hedged would train a reader to ignore the hedge. Also pins that
    // the flag is not inferred from `entries.len()` — this log is far short of
    // the cap, and a real refusal is present, so nothing about the refusal
    // itself may set it.
    let (reg, _d) = test_registry();
    let pty = 5874u32;
    let (gid, orch, w) = orch_pane_at_capacity(&reg, pty);
    reg.deliver_prompt(&orch.id, "a refused report", &w.id, Delivery::MidSession).unwrap_err();

    let r = reg.front_door_refusals(&gid);
    assert_eq!(r.total, 1);
    assert!(!r.window_truncated, "a short log is not a truncated one");
    let co = reg.resolve_token(&orch.token).unwrap();
    assert_eq!(orphans_tool(&reg, &co)["refused_window_truncated"], json!(false));
}

#[test]
fn an_oversized_refused_payload_says_it_was_truncated() {
    // Same rule as an orphan's payload, for the same reason: a shortened brief
    // that reads complete gets re-sent as if it were the whole thing.
    let (reg, _d) = test_registry();
    let pty = 5873u32;
    let (_gid, orch, w) = orch_pane_at_capacity(&reg, pty);
    let big = "x".repeat(queue::ORPHAN_TEXT_CAP_BYTES + 500);
    reg.deliver_prompt(&orch.id, &big, &w.id, Delivery::MidSession).unwrap_err();

    let co = reg.resolve_token(&orch.token).unwrap();
    let v = orphans_tool(&reg, &co);
    let row = &v["refused"][0];
    assert_eq!(row["truncated"], json!(true));
    assert_eq!(row["bytes"], json!(big.len()), "the true size is reported, not the cut one");
    assert!(row["text"].as_str().unwrap().contains("truncated"),
        "the cut must be visible in the text itself, not only in a sibling field");
}

#[test]
fn the_orphan_list_keeps_its_pre_579_shape_beside_the_refused_one() {
    // #579's contract decision, pinned: refusals were added as a SECOND list
    // rather than by widening `OrphanedQueueEntry.id` to `Option<u64>` or by
    // minting a synthetic id. `id` is the wire shape of this tool AND the join
    // key both orphan derivations run on, so a nullable id would be a cost paid
    // by every existing row for the benefit of rows that can never have one.
    // The day someone "unifies" the two lists, this says why not.
    let dir = tempfile::tempdir().unwrap();
    let (gid, _old, _session) = queued_then_crashed(dir.path(), &["re-send me verbatim"]);
    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&gid, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();

    let v = orphans_tool(&reg, &co);
    assert_eq!(v["count"], json!(1));
    assert_eq!(v["orphans"][0]["text"], json!("re-send me verbatim"));
    assert!(v["orphans"][0]["id"].as_u64().is_some(),
        "an orphan's id must stay a number — never null, never synthetic: {v}");
    // The new keys are always present, so a reader never has to distinguish
    // "no refusals" from "this loomux does not report them".
    assert_eq!(v["refused_count"], json!(0));
    assert_eq!(v["refused_omitted"], json!(0));
    assert_eq!(v["refused"], json!([]));
}

// ---------------------------------------------------------------------------
// #633 — the refusals that wrote no audit line at all.
//
// #579/#630 could only ever surface ONE refusal reason, because one refusal
// reason was all that wrote a record. `deliver_prompt_as` refuses two ways
// BEFORE admission — the target is dead, the target has no terminal bound yet —
// and both returned `Err` in silence; #615 created the second by turning a
// silent `Ok` into a silent `Err`, which fixed the sender's contract and left
// the log exactly as empty. A refusal with no line cannot be enumerated by the
// refusal derivation, by the id-keyed orphan derivations, or by a human
// grepping `audit.jsonl`: it is the #579 class exactly.
// ---------------------------------------------------------------------------

#[test]
fn a_delivery_to_a_dead_agent_is_audited_and_surfaced_as_a_refusal() {
    // The first of the two silent Errs. Before this, an orchestrator that
    // briefed a worker whose pane had already exited got an error string and
    // NOTHING else: no audit line, so no derivation, so no way for anyone to
    // find out afterwards that a task brief had evaporated.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&w.id, 5880);
    reg.mark_dead(&w.id, Some(0)).expect("precondition: the worker really is dead");

    let brief = "task brief: rebase #641 onto main and re-run CI";
    let err = reg.deliver_prompt(&w.id, brief, &orch.id, Delivery::MidSession).unwrap_err();
    assert!(err.contains("is dead"), "the sender is still told synchronously: {err}");

    let log = reg.audit_log(&g.id);
    let line = log
        .iter()
        .find(|e| e.action == "delivery-dropped"
            && e.detail["reason"] == json!(RefusalReason::AgentDead.as_str()))
        .expect("the refusal must leave a record — that is the whole issue");
    assert_eq!(line.detail["to"], json!(w.id));
    assert_eq!(line.detail["from"], json!(orch.id), "who lost the work, not who refused it");
    assert_eq!(line.detail["bytes"], json!(brief.len()));
    // The line carries the payload itself, because there is no `prompt` line to
    // pair with — `deliver_prompt` writes that one further down, AFTER this
    // refusal has already returned.
    assert!(
        !log.iter().any(|e| e.action == "prompt" && e.detail["text"] == json!(brief)),
        "a delivery to a dead pane was never offered to anything, so `prompt` must not claim it was"
    );

    let co = reg.resolve_token(&orch.token).unwrap();
    let v = orphans_tool(&reg, &co);
    assert_eq!(v["count"], json!(0),
        "nothing was queued, so neither id-keyed orphan derivation can ever see it: {v}");
    assert_eq!(v["refused_count"], json!(1), "and it must not therefore be invisible: {v}");
    let row = &v["refused"][0];
    assert_eq!(row["reason"], json!("agent-dead-at-call"));
    assert_eq!(row["text"], json!(brief), "re-sendable verbatim, off the refusal's own line");
    assert_eq!(row["bytes"], json!(brief.len()));
    assert_eq!(row["queue_depth"], Value::Null,
        "no depth was measured — `0` would read as \"the pane was empty\", which nobody checked");
    assert_eq!(row["enqueue_reason"], Value::Null, "admission was never attempted");
    assert!(row["consequence"].as_str().unwrap_or_default().contains("re-target"),
        "a dead target is not a re-send, and the row has to say so: {row}");
}

#[test]
fn a_delivery_to_an_agent_with_no_terminal_is_audited_and_surfaced_as_a_refusal() {
    // The second silent Err, and the one #615 created: a delivery landing in
    // the spawn-to-bind window has no pane to queue against, so it is refused.
    // That is the right answer for the sender and was, until now, no answer at
    // all for anyone reading the log.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    assert!(reg.agent(&w.id).expect("registered").pty_id.is_none(),
        "precondition: this test is only meaningful while the agent is unbound");

    let brief = "task brief: issue #633, harden the refusal audit";
    let err = reg.deliver_prompt(&w.id, brief, &orch.id, Delivery::MidSession).unwrap_err();
    assert!(err.contains("no terminal yet"), "the sender is still told synchronously: {err}");

    let co = reg.resolve_token(&orch.token).unwrap();
    let v = orphans_tool(&reg, &co);
    assert_eq!(v["count"], json!(0));
    assert_eq!(v["refused_count"], json!(1), "the loss is real and must be enumerable: {v}");
    let row = &v["refused"][0];
    assert_eq!(row["reason"], json!("no-terminal-at-call"));
    assert_eq!(row["to"], json!(w.id));
    assert_eq!(row["from"], json!(orch.id));
    assert_eq!(row["text"], json!(brief));
    assert_eq!(row["queue_depth"], Value::Null, "a queue is keyed by pane, and there was no pane");
    // The advice differs from the dead-target case, which is the whole reason
    // the reasons are distinct rather than one "refused" bucket: this one was
    // simply too early and IS worth re-sending as-is.
    assert!(row["consequence"].as_str().unwrap_or_default().contains("re-send once the pane binds"),
        "an unbound target is a retry, not a re-target: {row}");
}

#[test]
fn a_withdrawn_admission_is_reported_once_and_names_the_cause_that_fired() {
    // `withdraw_unprocessable`'s undo — #630 excluded it from the refused list
    // silently, on the argument that it is unreachable in production. #633
    // surfaces it instead: the argument is a claim about today's startup order
    // (`set_app`/`set_self_arc` both run in `lib.rs`'s setup block, before the
    // MCP thread that is the only way an agent can call `deliver_prompt`) that
    // nothing enforces, and the failure mode of it going stale is a loss
    // nothing can enumerate.
    //
    // Test mode is the case that mint that line for real: a bare registry has
    // no `AppHandle` at all.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5882u32;
    reg.set_pty_for_test(&orch.id, pty);

    let report = "report: done, PR #644 is green on all three platforms";
    let err = reg.deliver_prompt(&orch.id, report, &w.id, Delivery::MidSession).unwrap_err();
    assert!(err.contains("no app handle"), "the sender is told synchronously: {err}");
    assert_eq!(reg.queue_depth(pty), 0, "the admission was withdrawn, not left to strand");

    let line = reg
        .audit_log(&g.id)
        .into_iter()
        .find(|e| e.action == "delivery-dropped"
            && e.detail["reason"] == json!(RefusalReason::NoAppHandle.as_str()))
        .expect("the withdrawal is audited");
    assert!(line.detail["id"].as_u64().is_some(),
        "this refusal DID mint an id, and the line has to carry it: that is what closes the id \
         for `queue::orphaned_queue_entries` and keeps one loss to one row");
    // #633 added the fields that make the row actionable — pre-#633 the line was
    // `{id, reason}` and named neither the target, the sender, nor the size.
    assert_eq!(line.detail["to"], json!(orch.id));
    assert_eq!(line.detail["from"], json!(w.id));
    assert_eq!(line.detail["bytes"], json!(report.len()));

    let co = reg.resolve_token(&orch.token).unwrap();
    let v = orphans_tool(&reg, &co);
    assert_eq!(v["count"], json!(0),
        "the withdrawal's own line closed the id, so it is NOT also an orphan: {v}");
    assert_eq!(v["refused_count"], json!(1));
    let row = &v["refused"][0];
    assert_eq!(row["reason"], json!("no-app-handle"));
    assert_eq!(row["text"], json!(report),
        "recovered by #579's verified pairing — this path DOES write a `prompt` line first");
    assert_eq!(row["queue_depth"], Value::Null, "the depth is not what refused this one");
    assert!(row["consequence"].as_str().unwrap_or_default().contains("withdrawn"),
        "the reader has to know nothing is left queued: {row}");
}

#[test]
fn a_dropped_reason_the_refusal_list_does_not_model_is_never_folded_into_it() {
    // The filter is an ENUMERATION over `RefusalReason`, not a widening to
    // "anything dropped-ish". `delivery-dropped` is also written for whole-queue
    // drops (`agent-died`, `queue-full`), and each of those carries an `id` that
    // the orphan derivations already report — listing them here too would show
    // one loss twice in a tool result whose documented response is "re-send",
    // i.e. a duplicate-delivery generator. An unmodelled reason from a future
    // build is skipped by the same rule rather than guessed at.
    let dropped = |reason: &str| AuditEntry {
        ts_ms: 1_000,
        actor: "loomux".into(),
        action: "delivery-dropped".into(),
        detail: json!({ "id": 7, "to": "w-1", "reason": reason }),
    };
    let r = front_door_refusals(
        &[dropped("agent-died"), dropped("queue-full"), dropped("a-reason-from-a-later-build")],
        false,
    );
    assert_eq!(r.total, 0, "none of these are front-door refusals: {:?}", r.items);

    // ...and a modelled one sitting in the same timeline still lands, so this is
    // a property of the filter rather than of an empty input.
    let r = front_door_refusals(
        &[dropped("agent-died"), refusal_line(2_000, "w-1", "orch-1", "a real loss", "arrival")],
        false,
    );
    assert_eq!(r.total, 1);
    assert_eq!(r.items[0].reason, RefusalReason::QueueFull);
    assert_eq!(r.items[0].depth, Some(queue::QUEUE_MAX_PER_PANE),
        "the one reason that DID measure a depth still reports it");
}

#[test]
fn a_missed_withdrawal_stays_an_orphan_and_is_never_listed_as_a_refusal() {
    // `withdraw_unprocessable`'s pop is conditional — the id may no longer be
    // the front of the queue — and that branch used to write nothing at all,
    // leaving a sender told "failed" while its payload sat queued, with no line
    // joining the two.
    //
    // It now writes `delivery-withdraw-missed`, and the action is the load-
    // bearing part: `queue::orphaned_queue_entries` CLOSES an id on
    // `delivery-dropped`, so reusing that action here would tell the orphan
    // derivation a still-queued entry had been resolved — turning a silent gap
    // into a false all-clear, which is strictly worse. The entry stays an
    // orphan (correctly: it is still there) and the refusal list skips it, so
    // the one-loss-one-row rule holds from both sides.
    let (reg, d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();

    let queued = AuditEntry {
        ts_ms: 1_000,
        actor: "loomux".into(),
        action: "delivery-queued".into(),
        detail: json!({ "to": &orch.id, "id": 7, "reason": "arrival", "depth": 1 }),
    };
    let missed = AuditEntry {
        ts_ms: 1_001,
        actor: "loomux".into(),
        action: "delivery-withdraw-missed".into(),
        detail: json!({
            "queued_id": 7, "to": &orch.id, "from": "w-1",
            "reason": RefusalReason::NoAppHandle.as_str(), "pty": 5883,
        }),
    };
    fs::write(
        d.path().join(g.id.as_str()).join("audit.jsonl"),
        [audit_jsonl_line(&queued), audit_jsonl_line(&missed)].join("\n") + "\n",
    )
    .unwrap();

    let v = orphans_tool(&reg, &co);
    assert_eq!(v["count"], json!(1),
        "the entry is still queued, so the id-keyed derivation must keep reporting it: {v}");
    assert_eq!(v["orphans"][0]["id"], json!(7));
    assert_eq!(v["refused_count"], json!(0),
        "and it must NOT also be listed as a refusal — one loss, one row: {v}");
}

// ---------------------------------------------------------------------------
// #658 — the drain-time refusal roster.
//
// A refusal to a LIVE pane had no push path to that pane at all. The sender was
// told synchronously, the loss was audited (#563/#633), and #578's rider told
// an orchestrator its own queue was FULL — but nothing ever named WHO was
// refused to the pane that refused them, so a mid-session refusal surfaced only
// if an orchestrator happened to call `queue_orphans`, which its own contract
// frames as a start-of-session recovery step. These tests pin the missing half:
// the moment the pane's depth comes back below the cap, it is told.
// ---------------------------------------------------------------------------

#[test]
fn a_pane_that_drains_below_its_cap_is_told_who_it_refused() {
    // #658's live instance, end to end: the orchestrator's pane hits 8/8 during
    // a report burst, a worker's done-report is refused, and the pane drains a
    // moment later. Before this, that drain said nothing — the pressure notices
    // announce the pane's STATE ("FULL", "6/8 and backing up") and never the
    // roster of what was lost, which is the whole gap.
    let (reg, _d) = test_registry();
    let pty = 5890u32;
    let (gid, orch, w) = orch_pane_at_capacity(&reg, pty);

    let report = "report: done, PR #654 is ready for review — CI green on all three platforms";
    let err = reg.deliver_prompt(&orch.id, report, &w.id, Delivery::MidSession).unwrap_err();
    assert!(err.contains("NOT queued"), "the sender is still told synchronously: {err}");

    // One entry drains: 8/8 -> 7/8. That edge is the first moment loomux can
    // tell this pane anything at all without queueing behind the block it is
    // reporting on.
    let front = reg.queue_snapshot(pty).remove(0);
    reg.pop_front_dequeued(&gid, pty, front.id, front.enqueued_ms);

    let relay = reg
        .take_orchestrator_notices(&gid)
        .expect("the drain must relay something to the orchestrator");
    assert!(relay.contains("REFUSED and never queued"),
        "the roster itself, not just a queue-state notice: {relay}");
    assert!(relay.contains(w.id.as_str()),
        "it has to name the SENDER — the recipient's only way to ask for the work again: {relay}");
    assert!(relay.contains("PR #654"),
        "and enough of the payload to tell WHICH delivery was lost: {relay}");
    assert!(relay.contains("queue-full-at-call"),
        "and the reason, which is what makes the row actionable: {relay}");
    assert!(relay.contains("NOT re-sent"),
        "nobody has re-sent it, so the roster must ask for it rather than imply it is handled: {relay}");
}

#[test]
fn a_worker_pane_that_drains_is_told_by_a_delivery_rather_than_a_relay() {
    // The channel is chosen the way #578 chose it: an orchestrator's own pane
    // cannot be told by a delivery (it would queue behind the very block it
    // reports), so its roster rides a tool result — but every OTHER pane is
    // told the ordinary way, by a delivery into its own queue. Same words, both
    // channels; this pins that the non-orchestrator half exists at all.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5891u32;
    reg.set_pty_for_test(&w.id, pty);
    for i in 0..queue::QUEUE_MAX_PER_PANE {
        reg.enqueue_text(&g.id, &w.id, "orrerix", &format!("[orrerix] advisory {i}"), pty,
            queue::EnqueueReason::Arrival).unwrap();
    }

    let brief = "review finding: PR #661 masks on what a row claims, not on what loomux wrote";
    let err = reg.deliver_prompt(&w.id, brief, &orch.id, Delivery::MidSession).unwrap_err();
    assert!(err.contains("NOT queued"), "the sender is told synchronously: {err}");

    let front = reg.queue_snapshot(pty).remove(0);
    reg.pop_front_dequeued(&g.id, pty, front.id, front.enqueued_ms);

    let snap = reg.queue_snapshot(pty);
    let roster = snap
        .last()
        .and_then(|e| e.payload.text())
        .expect("the drain must queue a roster for this pane");
    assert!(roster.contains("REFUSED and never queued"), "the roster, not a pressure notice: {roster}");
    assert!(roster.contains(orch.id.as_str()), "naming the sender to ask: {roster}");
    assert!(roster.contains("PR #661"), "and which delivery it was: {roster}");
    assert!(roster.starts_with("[orrerix]"),
        "one marker-led line, so the same string can also be parked in the relay block: {roster}");
    assert!(!roster.contains('\n'), "…and a single line at that: {roster}");
    // It obeys the cap it reports on: the roster took the slot the drain freed
    // and nothing more.
    assert_eq!(reg.queue_depth(pty), queue::QUEUE_MAX_PER_PANE,
        "one entry out, one roster in — never past the cap: {snap:?}");
    assert_eq!(snap.last().unwrap().reason, queue::EnqueueReason::RefusalRoster,
        "under its own admission reason, which is half of what stops a refused roster \
         from recurring into the next one");
}

/// The roster's own audit line — the ONE piece of state the audit log did not
/// already hold, written back into that same log. `delivered` is what makes it a
/// watermark: a roster nobody received must not advance anything.
fn roster_line(ts_ms: u64, to: &str, through_ms: u64, at_through: usize, delivered: bool) -> AuditEntry {
    AuditEntry {
        ts_ms,
        actor: "loomux".into(),
        action: REFUSAL_ROSTER_ACTION.into(),
        detail: json!({
            "to": to, "through_ms": through_ms, "at_through": at_through,
            "delivered": delivered,
        }),
    }
}

#[test]
fn the_roster_marks_a_refusal_its_sender_already_re_sent_rather_than_hiding_it() {
    // #658 says this in as many words, and the reason is the recipient's point
    // of view: it cannot tell a re-send from a first send, so a list that
    // silently dropped the ones already handled would read as "these are all
    // still missing" while being short — the silent-truncation defect this
    // whole lineage exists to eliminate. Marked, not suppressed.
    let landed = "report: done, PR #654 is ready for review";
    let lost = "report: blocked, #641 needs a human call on option A vs B";
    let entries = vec![
        prompt_line(1_000, "w-1", "orch-1", landed),
        refusal_line(1_001, "w-1", "orch-1", landed, "arrival"),
        prompt_line(1_002, "w-2", "orch-1", lost),
        refusal_line(1_003, "w-2", "orch-1", lost, "arrival"),
        // w-1 tried again once the pane had room; nothing refused it this time,
        // which is the only evidence that exists that it landed.
        prompt_line(2_000, "w-1", "orch-1", landed),
    ];
    let r = refusal_roster(&entries, "orch-1", false);
    assert_eq!(r.total, 2, "both are still listed: {:?}", r.items);
    let a = r.items.iter().find(|i| i.from == "w-1").expect("w-1's row survives");
    let b = r.items.iter().find(|i| i.from == "w-2").expect("w-2's row is there too");
    assert!(a.resent, "a later matching `prompt` with no refusal after it IS the re-send");
    assert!(!b.resent, "w-2 never tried again, so this one is still missing");

    let text = refusal_roster_notice(&r).expect("two refusals is something to say");
    assert!(text.contains("w-1 has since re-sent it — nothing to do"),
        "the row that is handled says so rather than asking for it again: {text}");
    assert!(text.contains("NOT re-sent — ask w-2 for it"),
        "and the row that is not handled names who to ask: {text}");
    assert!(!text.contains('\n'), "one line, or `OrchNoticeInbox::park` cannot take it: {text}");
    assert!(text.starts_with(REFUSAL_ROSTER_OPENER));
}

#[test]
fn a_re_send_that_was_refused_again_is_not_counted_as_delivered() {
    // The credit-spending half of `refusal_was_resent`, and the case that made
    // it a walk rather than an existence check: #658's live instance refused
    // the SAME report twice. A later `prompt` line proves an attempt, never an
    // arrival — what proves the arrival is that no refusal followed it.
    let report = "report: done, PR #654 is ready for review";
    let twice = vec![
        prompt_line(1_000, "w-1", "orch-1", report),
        refusal_line(1_001, "w-1", "orch-1", report, "arrival"),
        prompt_line(1_100, "w-1", "orch-1", report),
        refusal_line(1_101, "w-1", "orch-1", report, "arrival"),
    ];
    let r = refusal_roster(&twice, "orch-1", false);
    assert_eq!(r.total, 2, "two refusals, two rows");
    assert!(r.items.iter().all(|i| !i.resent),
        "the second attempt was refused too, so nothing has landed: {:?}", r.items);

    // …and a third attempt that nothing refuses flips both rows, because both
    // were waiting on the same payload.
    let mut third = twice.clone();
    third.push(prompt_line(1_200, "w-1", "orch-1", report));
    let r = refusal_roster(&third, "orch-1", false);
    assert!(r.items.iter().all(|i| i.resent),
        "one surviving attempt accounts for the payload: {:?}", r.items);
}

#[test]
fn a_pre_admission_refusal_never_spends_a_re_send_credit() {
    // #633's two refusals return BEFORE `deliver_prompt` writes its `prompt`
    // line — they carry the payload inline instead. Charging them a credit
    // would consume a re-send that really did land, so the roster would tell a
    // pane to chase a report it already has.
    // The spawn-to-bind window really does refuse repeatedly — an orchestrator
    // retrying a brief at a pane that has not bound yet gets the same answer
    // each time — and then the pane binds and one attempt lands. THREE refusals
    // before the landing, not one, is what makes this test discriminate the
    // rule rather than a sign flip: an implementation that charges a
    // pre-admission refusal ends up at -1 here, which no threshold reads as
    // "delivered".
    let brief = "task brief: rebase #641 onto main and re-run CI";
    let too_early = |ts_ms: u64| AuditEntry {
        ts_ms,
        actor: "loomux".into(),
        action: "delivery-dropped".into(),
        detail: json!({
            "to": "w-9", "reason": RefusalReason::NoTerminal.as_str(), "from": "orch-1",
            "bytes": brief.len(), "preview": queue::dropped_payload_preview(brief),
            "text": brief,
        }),
    };
    let entries = vec![
        too_early(1_000),
        too_early(1_100),
        too_early(1_200),
        // The pane bound, and this one was admitted — nothing refused it.
        prompt_line(2_000, "orch-1", "w-9", brief),
    ];
    let r = refusal_roster(&entries, "w-9", false);
    assert_eq!(r.total, 3, "a no-terminal refusal is a refusal: {:?}", r.items);
    assert!(r.items.iter().all(|i| i.resent),
        "the attempt that landed accounts for all three — none of those refusals wrote a \
         `prompt` line of its own for it to spend: {:?}", r.items);
}

#[test]
fn the_roster_never_repeats_what_a_delivered_roster_already_named() {
    // Once per drain, not once per pop. The watermark lives in the audit log
    // like everything else here, and it is a timestamp AND a count: a bare
    // timestamp would either drop a refusal stamped in the same millisecond as
    // the last one reported (a report burst is exactly when that happens) or
    // repeat one forever.
    let a = "first report";
    let b = "second report, same millisecond";
    let entries = vec![
        prompt_line(1_000, "w-1", "orch-1", a),
        refusal_line(1_000, "w-1", "orch-1", a, "arrival"),
        prompt_line(1_000, "w-2", "orch-1", b),
        refusal_line(1_000, "w-2", "orch-1", b, "arrival"),
    ];
    let first = refusal_roster(&entries, "orch-1", false);
    assert_eq!((first.total, first.through_ms, first.at_through), (2, 1_000, 2),
        "both refusals share the millisecond, and the roster records how many did");

    // Replayed with its own delivered roster line in the timeline: nothing new.
    let mut after = entries.clone();
    after.push(roster_line(1_500, "orch-1", first.through_ms, first.at_through, true));
    let second = refusal_roster(&after, "orch-1", false);
    assert_eq!(second.total, 0, "already told: {:?}", second.items);
    assert!(refusal_roster_notice(&second).is_none(),
        "and an empty roster must cost the pane nothing at all");

    // A THIRD refusal landing in that same millisecond, appended after the
    // roster ran, is the one a `>` comparison would have eaten.
    let c = "third report, also that millisecond";
    after.push(prompt_line(1_000, "w-3", "orch-1", c));
    after.push(refusal_line(1_000, "w-3", "orch-1", c, "arrival"));
    let third = refusal_roster(&after, "orch-1", false);
    assert_eq!(third.total, 1, "the count skips the two already named, not the millisecond");
    assert_eq!(third.items[0].from, "w-3");
}

#[test]
fn a_roster_that_was_never_delivered_does_not_advance_the_watermark() {
    // The roster obeys the cap it reports on, which means it can itself be
    // refused. Letting that line advance the mark would lose exactly the
    // payloads it was written to name — the failure this whole issue is about,
    // re-introduced by the fix for it.
    let a = "a report nobody has seen";
    let entries = vec![
        prompt_line(1_000, "w-1", "orch-1", a),
        refusal_line(1_001, "w-1", "orch-1", a, "arrival"),
        roster_line(1_500, "orch-1", 1_001, 1, false),
    ];
    let r = refusal_roster(&entries, "orch-1", false);
    assert_eq!(r.total, 1, "a roster that reached nobody reported nothing: {:?}", r.items);
    assert_eq!(r.items[0].from, "w-1");
}

#[test]
fn a_refused_roster_never_becomes_a_line_in_the_next_roster() {
    // The recursion #658 asks this mechanism not to have: a roster refused at a
    // still-full pane writes its own `delivery-dropped` line, and including it
    // would put the previous roster's text inside the next roster's preview,
    // and that one inside the one after. Two exclusions, because a refusal has
    // two shapes: the queue-full shape records the admission reason, and the
    // pre-admission shapes record no reason at all.
    let roster_text = format!("{REFUSAL_ROSTER_OPENER} While it was full, 1 delivery to you was \
        REFUSED and never queued");
    let real = "report: done, PR #654 is ready";
    let by_reason = refusal_line(1_000, "orrerix", "orch-1", &roster_text, "refusal-roster");
    let by_opener = AuditEntry {
        ts_ms: 1_001,
        actor: "loomux".into(),
        action: "delivery-dropped".into(),
        detail: json!({
            "to": "orch-1", "reason": RefusalReason::NoTerminal.as_str(), "from": "loomux",
            "bytes": roster_text.len(), "preview": queue::dropped_payload_preview(&roster_text),
            "text": roster_text,
        }),
    };
    let entries = vec![
        by_reason,
        by_opener,
        prompt_line(1_002, "w-1", "orch-1", real),
        refusal_line(1_003, "w-1", "orch-1", real, "arrival"),
    ];
    let r = refusal_roster(&entries, "orch-1", false);
    assert_eq!(r.total, 1, "only the real loss is a row: {:?}", r.items);
    assert_eq!(r.items[0].from, "w-1");
    // …and `queue_orphans` still sees all three, because a refused roster IS a
    // refusal and hiding it there would be a second silence.
    assert_eq!(front_door_refusals(&entries, false).total, 3);
}

#[test]
fn a_stranded_submit_marker_refusal_is_not_folded_into_the_roster() {
    // Every other row means "ask this agent to send it again". A marker refusal
    // has no sender to ask and no payload to re-send — what it means is "there
    // is unsubmitted text in this pane's box", which is a different instruction
    // with its own `consequence` string that `queue_orphans` already carries
    // verbatim. Folding it in would misdirect the reader.
    let marker = AuditEntry {
        ts_ms: 1_000,
        actor: "loomux".into(),
        action: "delivery-dropped".into(),
        detail: json!({
            "to": "w-1", "reason": RefusalReason::QueueFull.as_str(),
            "depth": queue::QUEUE_MAX_PER_PANE, "payload": RefusedPayload::StrandedSubmit.as_str(),
            "consequence": "text is pasted in the pane with nothing queued to submit it",
        }),
    };
    let r = refusal_roster(&[marker.clone()], "w-1", false);
    assert_eq!(r.total, 0, "not a roster row: {:?}", r.items);
    assert!(refusal_roster_notice(&r).is_none());
    assert_eq!(front_door_refusals(&[marker], false).total, 1,
        "…but still a refusal everywhere it was already reported — this is a routing \
         decision, not a new silence");
}

#[test]
fn the_roster_is_capped_says_what_it_left_out_and_clamps_each_preview() {
    // One LINE carries these, so the cap is tighter than `queue_orphans`' — and
    // what it drops is counted and said out loud, never silently cut. The
    // watermark still covers the dropped rows: they are in `audit.jsonl`, and
    // re-reporting them at every future drain would be its own noise.
    let long = "x".repeat(ROSTER_PREVIEW_MAX * 3);
    let mut entries = Vec::new();
    let n = ROSTER_LIST_MAX + 2;
    for i in 0..n {
        let text = format!("report {i}: {long}");
        entries.push(prompt_line(1_000 + i as u64, &format!("w-{i}"), "orch-1", &text));
        entries.push(refusal_line(1_001 + i as u64, &format!("w-{i}"), "orch-1", &text, "arrival"));
    }
    let r = refusal_roster(&entries, "orch-1", false);
    assert_eq!(r.total, n, "everything in the window is counted");
    assert_eq!(r.items.len(), ROSTER_LIST_MAX, "only this many are worded");
    assert_eq!(r.omitted, 2);
    assert_eq!(r.items[0].from, "w-2", "the MOST RECENT that many — a held pane keeps refusing");
    assert!(r.items.iter().all(|i| i.preview.chars().count() <= ROSTER_PREVIEW_MAX + 1),
        "each preview is re-clamped, marker included: {:?}", r.items);
    assert_eq!(r.through_ms, 1_001 + (n - 1) as u64,
        "the watermark covers the rows the cap dropped too, or they repeat forever");

    let text = refusal_roster_notice(&r).expect("something to say");
    assert!(text.contains("plus 2 earlier refusals not listed here"),
        "a capped list that reads as complete is the defect, not the cap: {text}");
    assert!(text.contains("audit.jsonl"), "and it says where the rest are: {text}");
}

#[test]
fn a_roster_over_a_cut_audit_window_says_it_could_not_see_everything() {
    // Same rule as `FrontDoorRefusals::window_truncated`: a scan that ran off
    // the start of its timeline has to say so, or "that is all that was
    // refused" is the strongest possible claim made from evidence nobody read.
    let a = "the one refusal in the readable tail";
    let entries = vec![
        prompt_line(1_000, "w-1", "orch-1", a),
        refusal_line(1_001, "w-1", "orch-1", a, "arrival"),
    ];
    let text = refusal_roster_notice(&refusal_roster(&entries, "orch-1", true))
        .expect("something to say");
    assert!(text.contains("older refusals it could not see"), "{text}");
    let clean = refusal_roster_notice(&refusal_roster(&entries, "orch-1", false)).unwrap();
    assert!(!clean.contains("could not see"),
        "and it must NOT say so when the window was whole: {clean}");
}

#[test]
fn the_roster_is_emitted_once_per_drain_and_never_recurses() {
    // The bound, end to end and on the live path. The roster is a delivery, so
    // the mechanism could feed itself: emit -> enqueue -> re-enter
    // `note_queue_capacity` -> emit. It cannot, for two reasons this pins
    // together — the emission is the LAST thing that transition does, and a
    // delivery only pushes depth UP, which is the one edge that never triggers
    // a roster.
    let (reg, _d) = test_registry();
    let pty = 5892u32;
    let (gid, orch, w) = orch_pane_at_capacity(&reg, pty);

    let report = "report: done, PR #654 is ready for review";
    reg.deliver_prompt(&orch.id, report, &w.id, Delivery::MidSession).unwrap_err();

    let front = reg.queue_snapshot(pty).remove(0);
    reg.pop_front_dequeued(&gid, pty, front.id, front.enqueued_ms);
    let first = reg.take_orchestrator_notices(&gid).expect("the drain relays the roster");
    assert!(first.contains("REFUSED and never queued"), "{first}");

    // Back to the cap and down again, with nothing refused in between: the
    // second drain has nothing to say and must say nothing.
    reg.enqueue_text(&gid, &orch.id, "orrerix", "[orrerix] advisory refill", pty,
        queue::EnqueueReason::Arrival).unwrap();
    assert_eq!(reg.queue_depth(pty), queue::QUEUE_MAX_PER_PANE);
    let front = reg.queue_snapshot(pty).remove(0);
    reg.pop_front_dequeued(&gid, pty, front.id, front.enqueued_ms);
    let second = reg.take_orchestrator_notices(&gid).unwrap_or_default();
    assert!(!second.contains("REFUSED and never queued"),
        "the same refusal must not be relayed at every drain forever: {second}");

    let rosters: Vec<_> = reg.audit_log(&gid).into_iter()
        .filter(|e| e.action == REFUSAL_ROSTER_ACTION)
        .collect();
    assert_eq!(rosters.len(), 1, "one drain that had something to say, one line: {rosters:?}");
    assert_eq!(rosters[0].detail["to"], json!(orch.id));
    assert_eq!(rosters[0].detail["count"], json!(1));
    assert_eq!(rosters[0].detail["delivered"], json!(true));
    assert_eq!(rosters[0].detail["channel"], json!("orchestrator-inbox"),
        "an orchestrator's own pane is told by the #578 relay, never by a delivery");
    assert!(rosters[0].detail["text"].as_str().unwrap().contains(w.id.as_str()),
        "the relay is held in memory, so the line carries what it relayed (#578's rule)");
}

#[test]
fn deliver_prompt_front_door_enqueues_behind_a_non_empty_queue_without_an_app_handle() {
    // #445/#470: landing BEHIND an existing entry never needs an app handle
    // of its own — a queue can only be non-empty because an EARLIER
    // delivery already had one (to spawn its drainer), so this admission
    // only best-effort nudges `ensure_drainer` and returns.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 109u32;
    reg.set_pty_for_test(&w.id, pty);

    reg.enqueue_text(&g.id, &w.id, "orrerix", "already queued", pty, queue::EnqueueReason::Question).unwrap();
    assert_eq!(reg.queue_depth(pty), 1);

    reg.deliver_prompt(&w.id, "a fresh prompt", "orch", Delivery::MidSession)
        .expect("the front door must succeed with no app handle at all");

    assert_eq!(reg.queue_depth(pty), 2, "the fresh prompt must land BEHIND the existing entry");
    let snap = reg.queue_snapshot(pty);
    assert_eq!(snap[0].payload.text(), Some("already queued"), "existing entry must not be overtaken");
    assert_eq!(snap[1].payload.text(), Some("a fresh prompt"));
    // #470: the front door always admits with `Arrival` (it IS the arrival,
    // whether or not it lands alone) — `depth` at admission time (not
    // `reason`) is what distinguishes "landed behind something" from
    // "landed alone," and this entry's own snapshot position (behind
    // "already queued") already proves that directly.
    assert_eq!(snap[1].reason, queue::EnqueueReason::Arrival);
}

#[test]
fn deliver_prompt_front_door_is_a_noop_when_the_queue_is_empty() {
    // #470: with an EMPTY queue, this delivery is admitted (briefly — it's
    // now the ONLY entry) and then, because it needs an app handle to ever
    // be processed and test mode has none, the admission is withdrawn and
    // the call fails exactly as the pre-#470 direct path always did.
    // Net-observable behavior is unchanged: no app handle, empty queue in
    // -> error out, empty queue still.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 110u32;
    reg.set_pty_for_test(&w.id, pty);

    let err = reg.deliver_prompt(&w.id, "hello", "orch", Delivery::MidSession).unwrap_err();
    assert!(err.contains("no app handle"), "must reach the normal direct-delivery path, got: {err}");
    assert_eq!(reg.queue_depth(pty), 0, "nothing should have been queued");
}

#[test]
fn deliver_prompt_front_door_propagates_the_synchronous_full_error() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 111u32;
    reg.set_pty_for_test(&w.id, pty);

    for i in 0..queue::QUEUE_MAX_PER_PANE {
        reg.enqueue_text(&g.id, &w.id, "orrerix", &format!("d-{i}"), pty, queue::EnqueueReason::BehindQueue).unwrap();
    }
    let err = reg.deliver_prompt(&w.id, "one too many", "orch", Delivery::MidSession).unwrap_err();
    assert!(err.contains("NOT queued"), "the ORIGINAL caller must see the truthful rejection, got: {err}");
}

#[test]
fn strip_ansi_removes_csi_osc_and_controls() {
    let raw = b"\x1b[31mred\x1b[0m and \x1b]0;title\x07plain\r\nnext";
    assert_eq!(strip_ansi(raw), "red and plain\nnext");
}

// #480/#496 PR-E: `get_output` on a pane mid-animation (a long-running
// "thinking" turn, a compaction spinner) returns thousands of tokens of
// near-identical redraw lines, because each repaint survives `strip_ansi`
// as its own "line" — the same shape documented at `auto_compact_banner_
// substrings` above (`✻ Thinking… (esc to interrupt · Ns · ↑ M tokens)`:
// a leading glyph and a trailing elapsed-time/token-count suffix that both
// change every repaint, wrapped around a stable core). A caller asking for
// a small `lines` budget to see recent history gets back nothing but the
// spinner burst — the informational content from *before* the burst is
// pushed out even though it would easily fit the same budget once the
// repeats collapse to one entry.
fn spinner_garbled_fixture() -> String {
    let mut s = String::new();
    for i in 1..=5 {
        s.push_str(&format!("real line {i}\n"));
    }
    // A long animated turn: ~200 redraws of the same spinner, each textually
    // distinct (elapsed seconds + token count tick up every frame) so a
    // byte-for-byte line dedup would never catch it — only the stable core
    // ("Thinking…") repeating is the actual signal.
    for i in 1..=200 {
        s.push_str(&format!("✻ Thinking… (esc to interrupt · {i}s · ↑ {} tokens)\n", i * 7));
    }
    s
}

#[test]
fn get_output_tail_is_not_swamped_by_a_spinner_burst() {
    // A caller triaging a busy pane asks for a modest budget (well under the
    // ~200-line spinner burst) expecting to see recent REAL activity, not
    // 200 near-identical redraws. Before spinner-frame collapsing this is
    // exactly what happens: the last 8 raw lines are 100% spinner, and the
    // five real lines that led up to it are invisible at this budget —
    // this is the actual reported cost (#480/#496 section 4 point 4), not a
    // hypothetical. This assertion is a behavior check against `format_
    // output_tail` as it exists right now (pre-collapsing): it fails today.
    let fixture = spinner_garbled_fixture();
    let out = format_output_tail(&fixture, 8);
    assert!(
        out.contains("real line 5"),
        "a small `lines` budget must still be able to reach real content \
         behind a spinner burst once repeats collapse — got only spinner \
         noise:\n{out}"
    );
    // Collapsing must never LOSE the freshest frame — that's the one a
    // triager actually needs to know the pane is still alive and how far
    // along it is.
    assert!(
        out.contains("200s") || out.contains("↑ 1400 tokens"),
        "the most recent spinner frame must survive collapsing, got:\n{out}"
    );
}

#[test]
fn get_output_tail_never_merges_genuinely_different_lines() {
    // Conservatism check: two lines that share a leading glyph but say
    // different things (not a redraw of the same frame) must both survive,
    // verbatim, never merged into one just because they look similar.
    let fixture = "✻ Thinking about the plan…\n✻ Thinking about the tests…\n\
                   ✻ Thinking about the tests…\n✻ Thinking about the tests…\n";
    let out = format_output_tail(fixture, 10);
    assert!(out.contains("Thinking about the plan…"), "distinct line dropped, got:\n{out}");
    assert!(out.contains("Thinking about the tests…"), "distinct line dropped, got:\n{out}");
    // The genuine repeat (three consecutive identical cores) collapses to
    // one entry plus a marker, never silently vanishes.
    assert!(
        out.to_lowercase().contains("collaps"),
        "a collapsed run must say so, not disappear silently, got:\n{out}"
    );
}

#[test]
fn get_output_tail_never_merges_code_lines_that_only_differ_inside_parens() {
    // #501 review finding N1 (rev-29): a worker pane's tail is full of code
    // listings, rustc diagnostics, and function signatures — lines that are
    // genuinely different but happen to end in `)` and share a prefix. None
    // of these begin with a spinner-style leading glyph, so the trailing-
    // paren strip must never touch them: gating that strip on "did this line
    // actually have a leading glyph stripped" is what fixes it.
    let fixture = "fn parse(input: &str)\nfn parse(input: &[u8])\n";
    let out = format_output_tail(fixture, 10);
    assert!(out.contains("fn parse(input: &str)"), "distinct signature dropped, got:\n{out}");
    assert!(out.contains("fn parse(input: &[u8])"), "distinct signature dropped, got:\n{out}");
    assert!(
        !out.to_lowercase().contains("collaps"),
        "two genuinely different lines must never be mislabeled as repeated frames, got:\n{out}"
    );
}
