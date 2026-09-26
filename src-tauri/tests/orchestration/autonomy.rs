//! Cost containment, the watchdog, autonomous mode and the idle-tick intake gate.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ---------- cost containment (#7): pause, idle-kill, spawn-rate, usage ----------

/// Guardrails with the two cost knobs set; other fields mirror `rails()`
/// but with a roomier agent cap so the spawn-rate guardrail can be exercised
/// without tripping the live-agent cap first.
pub(crate) fn costed_rails(idle_kill_minutes: u32, max_spawns_per_hour: u32) -> Guardrails {
    Guardrails {
        max_agents: 6,
        idle_kill_minutes,
        max_spawns_per_hour,
        ..rails()
    }
}

/// Guardrails with a watchdog stall window set (other fields mirror
/// `costed_rails`); a roomy agent cap so several workers can be watched.
pub(crate) fn watchdog_rails(watchdog_stall_minutes: u32) -> Guardrails {
    Guardrails { watchdog_stall_minutes, ..costed_rails(0, 0) }
}

#[test]
fn idle_should_kill_respects_threshold_and_disable() {
    let min = 60_000u64;
    // Disabled (0) never kills, no matter how long idle.
    assert!(!idle_should_kill(Some(0), 100 * min, 0));
    // An agent with work (None) is never idle-killed.
    assert!(!idle_should_kill(None, 100 * min, 5));
    // Under the threshold: safe. At/over: kill.
    assert!(!idle_should_kill(Some(0), 4 * min, 5));
    assert!(!idle_should_kill(Some(min), 5 * min, 5)); // exactly 4 min idle < 5
    assert!(idle_should_kill(Some(0), 5 * min, 5)); // exactly at threshold
    assert!(idle_should_kill(Some(0), 10 * min, 5));
}

#[test]
fn spawn_rate_exceeded_counts_only_the_trailing_window() {
    let window = 60 * 60 * 1000u64;
    let now = 10 * window; // 10h in
    // Unlimited (0) never trips.
    assert!(!spawn_rate_exceeded(&[now, now, now, now], now, 0, window));
    // Three within the last hour, limit 3 → next is refused.
    let recent = [now - 1000, now - 2000, now - 3000];
    assert!(spawn_rate_exceeded(&recent, now, 3, window));
    // The same three but limit 4 → still room.
    assert!(!spawn_rate_exceeded(&recent, now, 4, window));
    // Old spawns (outside the window) don't count toward the cap.
    let stale = [now - window - 1, now - 2 * window, now - 500];
    assert!(!spawn_rate_exceeded(&stale, now, 2, window));
}

#[test]
fn parse_session_cost_reads_the_lowest_statusline_dollar() {
    // Typical Claude statusline at the bottom of the pane.
    let pane = "some agent output\n$ ran a command\nmodel: sonnet · $0.42 · 12k tokens";
    assert_eq!(parse_session_cost(pane), Some(0.42));
    // Thousands separators tolerated; bottom-most render wins.
    assert_eq!(parse_session_cost("cost $1.00\ntotal $1,234.56 session"), Some(1234.56));
    // A bare "$" or "$." with no digits is not a cost.
    assert_eq!(parse_session_cost("price: $ TBD\nsee $.foo"), None);
    // No dollar figure at all.
    assert_eq!(parse_session_cost("just some\noutput lines"), None);
    // Whole-dollar amount.
    assert_eq!(parse_session_cost("session cost $3"), Some(3.0));
}

#[test]
fn pause_holds_delivery_and_persists_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let gid;
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
        gid = g.id.clone();
        let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
        // A target with no pane has nowhere to hold anything, paused or not —
        // #569 made those two answers the same `Err` rather than an `Ok` that
        // quietly dropped the payload on the paused side.
        let err = reg.deliver_prompt(&w.id, "hello", "orrerix", Delivery::MidSession).unwrap_err();
        assert!(err.contains("terminal"), "unpaused delivery must reach the pty step, got: {err}");
        reg.pause_group(&g.id).unwrap();
        let paused_err =
            reg.deliver_prompt(&w.id, "hello", "orrerix", Delivery::MidSession).unwrap_err();
        assert!(
            paused_err.contains("terminal"),
            "a paused delivery to a pane-less agent must say so too, not report false success:              {paused_err}"
        );

        // With a pane, the pause HOLDS it: audited as an ordinary prompt, on
        // the queue, and nothing pasted.
        reg.set_pty_for_test(&w.id, 5698);
        assert!(reg.is_paused(&g.id));
        reg.deliver_prompt(&w.id, "hello again", "orrerix", Delivery::MidSession).unwrap();
        assert_eq!(reg.queue_depth(5698), 1, "a paused delivery is queued, never destroyed");
        let log = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl")).unwrap();
        assert!(
            !log.contains("prompt-suppressed-paused"),
            "the discard action must have no writer left: {log}"
        );
        assert!(log.contains("group-paused"), "and the admission names the pause: {log}");
        assert!(reg.state_root().join(g.id.as_str()).join("paused").is_file(), "pause marker must be written");
    }
    // Restart: the pause survives (marker re-seeds the in-memory flag).
    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    assert_eq!(g.id, gid);
    assert!(reg.is_paused(&g.id), "a paused group must stay paused across restarts");
    // Resume clears the flag and the marker.
    reg.resume_group(&g.id).unwrap();
    assert!(!reg.is_paused(&g.id));
    assert!(!reg.state_root().join(g.id.as_str()).join("paused").is_file(), "resume must remove the marker");
}

#[test]
fn idle_workers_are_reap_candidates_but_busy_ones_and_orchestrator_are_not() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", costed_rails(5, 0)).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let idle = reg.spawn_agent(&g.id, Role::Worker, "idle", "", false, None).unwrap();
    let busy = reg.spawn_agent(&g.id, Role::Worker, "busy", "do work", false, None).unwrap();
    // Read the idle worker's stamped idle-since so the test is time-relative.
    let roster = reg.list_agents(&g.id);
    let idle_since = roster
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == idle.id.as_str())
        .and_then(|a| a["idle_since_ms"].as_u64())
        .expect("an idle-spawned worker must carry idle_since_ms");
    // The busy worker (spawned with a task) has no idle clock.
    let busy_idle = roster
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == busy.id.as_str())
        .unwrap()["idle_since_ms"]
        .clone();
    assert!(busy_idle.is_null(), "a worker given a task must not start the idle clock");
    let threshold_ms = 5 * 60_000u64;
    // Just before the threshold: nobody is reaped.
    let before = reg.idle_reap_candidates(idle_since + threshold_ms - 1);
    assert!(before.is_empty(), "must not reap before the timeout, got: {before:?}");
    // At/after the threshold: only the idle worker (never the orchestrator or
    // the busy worker).
    let after = reg.idle_reap_candidates(idle_since + threshold_ms);
    assert_eq!(after, vec![idle.id.clone()], "only the idle worker crosses the timeout");
}

#[test]
fn idle_kill_disabled_reaps_nothing() {
    let (reg, _d) = test_registry();
    // idle_kill_minutes = 0 → the guardrail is off.
    let g = reg.create_group("C:/tmp/repo", costed_rails(0, 0)).unwrap();
    reg.spawn_agent(&g.id, Role::Worker, "idle", "", false, None).unwrap();
    // Even absurdly far in the future, nothing is a candidate.
    assert!(reg.idle_reap_candidates(u64::MAX / 2).is_empty());
}

#[test]
fn reaper_spares_a_worker_reactivated_before_the_kill() {
    // Selection and kill happen under separate locks; a worker prompted in
    // that window (idle clock cleared) must not be reaped. reap_idle_agents
    // re-checks idle_should_kill immediately before killing.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", costed_rails(5, 0)).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let idle = reg.spawn_agent(&g.id, Role::Worker, "idle", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();
    let far_future = u64::MAX / 2;
    // The idle worker is a genuine candidate at that time.
    assert_eq!(reg.idle_reap_candidates(far_future), vec![idle.id.clone()]);
    // The orchestrator hands it work — send_prompt clears its idle clock
    // (delivery then fails in test mode with no pane, which is fine).
    let _ = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "send_prompt", "arguments": { "agent_id": idle.id, "text": "here is a task" } }));
    // Now it is no longer idle, so the reaper kills nothing.
    assert!(reg.reap_idle_agents(far_future).is_empty(),
        "a re-activated worker must not be reaped");
    // And it is still alive in the roster.
    let roster = reg.list_agents(&g.id).to_string();
    assert!(roster.contains(idle.id.as_str()));
}

#[test]
fn a_liaison_is_never_taken_by_the_idle_reaper() {
    // #891 S4, the lifecycle slice's open question answered. Every signal that
    // clears the idle clock is machine-side — a task at spawn, `send_prompt` —
    // and a HUMAN typing into a pane clears none of them. So the liaison's own
    // `report` stamps the clock and the pane's next hour of conversation is
    // invisible to the reaper, which then kills the human's correspondent and
    // audits it as "a slot the orchestrator wasn't using was reclaimed" —
    // sending its only notice to the other pane.
    //
    // Written through the REAL path that stamps the clock (a `report` over MCP
    // dispatch), not by poking `idle_since_ms`: the stamp being genuine is what
    // makes the exemption load-bearing rather than a skip of something that was
    // never a candidate anyway — hence the assertion on the stamp itself, and
    // the plain reviewer beside it as the control.
    let (reg, _d) = test_registry();
    let mut g_rails = costed_rails(5, 0);
    g_rails.blocks.push(workflow::Block {
        id: "desk".into(),
        name: "desk".into(),
        kind: Role::Reviewer,
        cli: String::new(),
        model: String::new(),
        prompt: None,
        profile: None,
        allow: vec![],
        role_hint: Some("liaison".into()),
        effort: String::new(),
        context: String::new(),
        remote: None,
        driver: None,
        cache_ttl_minutes: None,
    });
    let g = reg.create_group("C:/tmp/repo", g_rails).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    // The control: a plain reviewer, idle exactly as the liaison is.
    let rev = reg.spawn_agent(&g.id, Role::Reviewer, "rev", "review #900", false, None).unwrap();
    // The liaison, spawned the way its own fragment tells the orchestrator to —
    // by block id, with a task, so it starts with NO idle clock.
    let desk = reg
        .spawn_agent_ex(
            &g.id, Role::Reviewer, Some("desk".into()), "desk", "the human is asking about #891",
            false, None, None, None, None, None,
        )
        .unwrap();
    assert_eq!(desk.block, "desk");
    // The standard probe here, and required rather than cosmetic: an
    // orchestrator with no bound pane REFUSES the report's notice ("no app
    // handle") and the tool call comes back an error, which says nothing about
    // the idle clock this test is about. Paused delivery queues and audits
    // exactly what an unpaused one does, and the pause touches nothing the
    // reaper reads.
    pause_with_pane(&reg, &g.id, &orch.id, 987);

    // Both report, both go idle. This is the trap in one line: for the liaison
    // that is the moment the human's conversation starts, not the moment it ends.
    for (agent, note) in [(&rev, "verdict recorded"), (&desk, "relayed the human's answer")] {
        let c = reg.resolve_token(&agent.token).unwrap();
        let r = dispatch(&reg, &c, "tools/call", &json!({ "name": "report",
            "arguments": { "outcome": "done", "note": note } })).unwrap();
        assert_eq!(r["isError"], false, "the report must land, or no idle clock was stamped: {r:?}");
    }
    let idle_stamped = |id: &str| {
        reg.list_agents(&g.id)
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == json!(id))
            .expect("a live roster row")["idle_since_ms"]
            .as_u64()
            .is_some()
    };
    assert!(idle_stamped(&desk.id), "the liaison really is idle by the reaper's own measure");
    assert!(idle_stamped(&rev.id), "and so is the control reviewer");

    let far_future = u64::MAX / 2;
    assert_eq!(
        reg.idle_reap_candidates(far_future),
        vec![rev.id.clone()],
        "only the plain reviewer is reclaimable — the liaison is a standing conversation"
    );
    // ...and the reaper itself, not just its selection. The audit is what is
    // asserted on rather than the roster's `dead` flag: the kill's own
    // `kill_agent_as` cannot complete in test mode (no `AppHandle` to reach
    // `PtyManager`), which is the same boundary
    // `the_idle_reaper_routes_both_of_its_notices_to_the_audit` states.
    assert_eq!(reg.reap_idle_agents(far_future), vec![rev.id.clone()]);
    let log = reg.audit_log(&g.id);
    let killed = |id: &str| {
        log.iter().any(|e| e.action == "idle-kill" && e.detail["agent"] == json!(id))
    };
    assert!(killed(&rev.id), "the control must really have been reaped, or the sweep did nothing");
    assert!(!killed(&desk.id), "no idle-kill may ever name the liaison");
}

#[test]
fn spawn_rate_guardrail_backstops_a_burst() {
    let (reg, _d) = test_registry();
    // Cap 2 spawns/hour, roomy agent cap so the rate limit is what bites.
    let g = reg.create_group("C:/tmp/repo", costed_rails(0, 2)).unwrap();
    reg.spawn_agent(&g.id, Role::Worker, "w1", "t", false, None).unwrap();
    reg.spawn_agent(&g.id, Role::Worker, "w2", "t", false, None).unwrap();
    let err = reg.spawn_agent(&g.id, Role::Worker, "w3", "t", false, None).unwrap_err();
    assert!(err.contains("spawn-rate"), "third spawn within the hour must be refused, got: {err}");
    // The orchestrator is exempt from the spawn-rate backstop.
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
}

#[test]
fn report_completion_reidles_worker_and_send_prompt_reactivates() {
    let (reg, _d, co, cw) = setup_mcp();
    // The worker from setup_mcp was spawned with a task → not idle.
    let idle_of = |id: &str| -> Value {
        reg.list_agents(&cw.group)
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == id)
            .unwrap()["idle_since_ms"]
            .clone()
    };
    assert!(idle_of(&cw.agent_id).is_null(), "a tasked worker is not idle");
    // Reporting done re-idles it (delivery to the orchestrator fails in test
    // mode with no pane, but the idle transition happens first).
    let _ = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "report", "arguments": { "status": "done", "summary": "PR up" } }));
    assert!(!idle_of(&cw.agent_id).is_null(), "a worker that reported done becomes idle again");
    // The orchestrator sending it a fresh prompt clears the idle clock.
    let _ = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "send_prompt", "arguments": { "agent_id": cw.agent_id, "text": "next" } }));
    assert!(idle_of(&cw.agent_id).is_null(), "send_prompt must re-activate an idle worker");
}

// ---------- watchdog: stalled-agent detection (#10) ----------

/// A time far past any real `now_ms()` (year ~33658), so a `watchdog_tick` at
/// this instant is unambiguously past the stall window for an agent whose
/// clock was stamped at spawn/report with the real wall clock.
pub(crate) const FAR: u64 = 1_000_000_000_000_000;

/// Group with an orchestrator and one working (tasked) worker under a watchdog
/// with the given stall window (minutes). Returns (reg, tempdir, group, worker).
pub(crate) fn watchdog_setup(stall_min: u32) -> (OrchRegistry, tempfile::TempDir, GroupId, String) {
    let (reg, dir) = test_registry();
    let g = reg.create_group("C:/tmp/repo", watchdog_rails(stall_min)).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "do work", false, None).unwrap();
    (reg, dir, g.id, w.id)
}

#[test]
fn watchdog_should_notify_respects_threshold_anti_nag_and_disable() {
    let min = 60_000u64;
    // A 0 window disables the guardrail entirely.
    assert!(!watchdog_should_notify(0, 100 * min, 0, false));
    // Inside the window: not yet.
    assert!(!watchdog_should_notify(0, 4 * min, 5, false));
    // At and past the window: notify.
    assert!(watchdog_should_notify(0, 5 * min, 5, false), "exactly at the window notifies");
    assert!(watchdog_should_notify(0, 10 * min, 5, false));
    // Past the window but already notified: the anti-nag latch suppresses it.
    assert!(!watchdog_should_notify(0, 10 * min, 5, true), "one notice per stall");
}

#[test]
fn watchdog_flags_a_silent_worker_once_per_stall() {
    let (reg, _d, gid, wid) = watchdog_setup(5);
    let no_output = HashMap::new();
    // Long past the stall window with no output and no report → one notice.
    assert_eq!(reg.watchdog_tick(FAR, &no_output, &HashMap::new()), vec![wid.clone()],
        "a silent working agent must be flagged");
    let log = fs::read_to_string(reg.state_root().join(gid.as_str()).join("audit.jsonl")).unwrap();
    assert!(log.contains("watchdog-stall"), "the stall must be audited, got: {log}");
    // Anti-nag: still silent, but already notified for this same stall.
    assert!(reg.watchdog_tick(FAR + 60_000, &no_output, &HashMap::new()).is_empty(),
        "must not nag twice for one uninterrupted stall");
}

#[test]
fn watchdog_stall_resets_when_the_agent_produces_output() {
    let (reg, _d, _gid, wid) = watchdog_setup(5);
    let empty = HashMap::new();
    assert_eq!(reg.watchdog_tick(FAR, &empty, &HashMap::new()), vec![wid.clone()]);
    // The CLI emits output: a grown pty counter is activity — clock and latch
    // both reset, and this very tick must not also flag a stall.
    let grew: HashMap<String, u64> = [(wid.clone(), 1024u64)].into_iter().collect();
    assert!(reg.watchdog_tick(FAR, &grew, &HashMap::new()).is_empty(), "output growth is activity, not a stall");
    // No further growth; a whole fresh window elapses → a brand-new notice.
    let later = FAR + 5 * 60_000 + 1;
    assert_eq!(reg.watchdog_tick(later, &grew, &HashMap::new()), vec![wid.clone()],
        "a new stall after activity earns a new notice");
}

#[test]
fn watchdog_ignores_idle_dead_and_disabled_agents() {
    // A 0 stall window disables the watchdog for the whole group.
    let (off, _d0, _g0, _w0) = watchdog_setup(0);
    assert!(off.watchdog_tick(FAR, &HashMap::new(), &HashMap::new()).is_empty(),
        "stall window 0 disables the watchdog");
    // With the guardrail on, idle and dead agents are still out of scope: idle
    // is the reaper's concern, and a dead/reaped pane must never be nudged.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo2", watchdog_rails(5)).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    reg.spawn_agent(&g.id, Role::Worker, "idle", "", false, None).unwrap();
    let dead = reg.spawn_agent(&g.id, Role::Worker, "dead", "work", false, None).unwrap();
    reg.mark_dead(&dead.id, Some(1));
    let flagged = reg.watchdog_tick(FAR, &HashMap::new(), &HashMap::new());
    assert!(flagged.is_empty(),
        "neither an idle nor a dead agent may be watchdog-flagged, got: {flagged:?}");
}

#[test]
fn watchdog_stays_quiet_for_a_paused_group() {
    let (reg, _d, gid, wid) = watchdog_setup(5);
    reg.pause_group(&gid).unwrap();
    assert!(reg.watchdog_tick(FAR, &HashMap::new(), &HashMap::new()).is_empty(),
        "a paused group's agents idle out on purpose — no watchdog notices");
    // Crucially, the one-notice budget must be intact: pausing must not have
    // burned the latch, so on resume the outstanding stall still earns its
    // first notice.
    reg.resume_group(&gid).unwrap();
    assert_eq!(reg.watchdog_tick(FAR, &HashMap::new(), &HashMap::new()), vec![wid.clone()],
        "resuming an unattended stall must still earn its first notice");
}

#[test]
fn watchdog_stall_resets_when_the_agent_reports_or_messages() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", watchdog_rails(5)).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "work", false, None).unwrap();
    let cw = reg.resolve_token(&w.token).unwrap();
    // Stalled and flagged (anti-nag latch now set).
    assert_eq!(reg.watchdog_tick(FAR, &HashMap::new(), &HashMap::new()), vec![w.id.clone()]);
    // A progress report is a sign of life: it clears the latch (via re-idle
    // bookkeeping), so a later silence re-notifies. If the latch had NOT been
    // cleared this tick would be empty — that's the discriminator.
    let _ = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "report", "arguments": { "status": "progress", "summary": "still going" } }));
    assert_eq!(reg.watchdog_tick(FAR + 60_000, &HashMap::new(), &HashMap::new()), vec![w.id.clone()],
        "a report must reset the stall, then a later silence re-notifies");
    // A free-form message likewise counts as activity and clears the latch.
    let _ = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "message_orchestrator", "arguments": { "text": "checking in" } }));
    assert_eq!(reg.watchdog_tick(FAR + 120_000, &HashMap::new(), &HashMap::new()), vec![w.id.clone()],
        "a message must also reset the stall, then a later silence re-notifies");
}

// ---------- autonomous mode: idle-tick, toggles, budget (#83) ----------

/// An autonomous group with a live (Running, headless) orchestrator. Returns
/// (reg, tempdir, group id, orchestrator id). Autonomous mode is ON, so
/// `idle_tick_tick` considers it. Intake gate explicitly OPTED OUT
/// (`intake_poll_minutes: Some(0)`) — this helper exists to test the base
/// #83 idle-tick mechanism (quiet window, activity floor, latch, per-hour
/// cap) in isolation; a group that wants the #429 smart default or an
/// explicit cadence uses `autonomous_setup_with_gate` instead. Without this,
/// EVERY multi-tick test below would silently start exercising the intake
/// gate too the moment #429's smart default shipped, since `rails()`'s
/// unset `intake_poll_minutes` now means "on while autonomous," not "off".
pub(crate) fn autonomous_setup() -> (OrchRegistry, tempfile::TempDir, GroupId, String) {
    let (reg, dir) = test_registry();
    let g = reg
        .create_group("C:/tmp/repo", Guardrails { intake_poll_minutes: Some(0), ..rails() })
        .unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    reg.set_autonomous(&g.id, true).unwrap();
    (reg, dir, g.id, o.id)
}

/// Count audit entries whose action is exactly `action`. Matches the
/// quote-delimited JSON value so a prefix action (`autonomous-off`) doesn't also
/// count its superset (`autonomous-off-failed`).
pub(crate) fn audit_count(reg: &OrchRegistry, group: &GroupId, action: &str) -> usize {
    fs::read_to_string(reg.state_root().join(group.as_str()).join("audit.jsonl"))
        .unwrap_or_default()
        .matches(&format!("\"{action}\""))
        .count()
}

/// The parsed audit entries for one action, in order — for assertions about a
/// line's `detail` payload rather than merely how many lines exist.
pub(crate) fn audit_entries(reg: &OrchRegistry, group: &GroupId, action: &str) -> Vec<Value> {
    fs::read_to_string(reg.state_root().join(group.as_str()).join("audit.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e["action"] == action)
        .collect()
}

/// A durable usage snapshot carrying `tokens` input tokens under a unique key, to
/// seed a group's lifetime spend without a real transcript.
pub(crate) fn seed_usage(reg: &OrchRegistry, group: &GroupId, key: &str, tokens: u64) {
    reg.upsert_usage_snapshot(group, UsageSnapshot {
        key: key.to_string(),
        agent_id: format!("agent-{key}"),
        name: key.to_string(),
        role: "worker".to_string(),
        source: "transcript".to_string(),
        block: "worker".to_string(),
        cli: "claude".to_string(),
        input_tokens: tokens,
        output_tokens: 0,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        cost_usd: None,
        estimated: true,
        model: Some("claude-opus-4-8".to_string()),
        current_model: Some("claude-opus-4-8".to_string()),
        updated_ms: now_ms(),
        activity: Default::default(),
    });
}

#[test]
fn idle_tick_should_fire_respects_threshold_latch_cap_and_skew() {
    let min = 60_000u64;
    let none: &[u64] = &[];
    // A 0 threshold disables the tick entirely.
    assert!(!idle_tick_should_fire(0, 100 * min, 0, false, none, 6));
    // Inside the window: not yet. At/past: fire.
    assert!(!idle_tick_should_fire(0, 14 * min, 15, false, none, 6));
    assert!(idle_tick_should_fire(0, 15 * min, 15, false, none, 6), "exactly at the window fires");
    assert!(idle_tick_should_fire(0, 30 * min, 15, false, none, 6));
    // The one-notice latch suppresses a re-fire until output growth clears it.
    assert!(!idle_tick_should_fire(0, 30 * min, 15, true, none, 6), "latched → no re-fire");
    // Per-hour cap: at the cap (6 ticks inside the trailing hour) the backstop
    // blocks even a legitimately-due tick; a stale timestamp outside the hour
    // doesn't count.
    let now = 100 * min;
    let at_cap: Vec<u64> = (0..6).map(|i| now - i * min).collect(); // 6 within the hour
    assert!(!idle_tick_should_fire(0, now, 15, false, &at_cap, 6), "per-hour cap is a hard backstop");
    let under_cap: Vec<u64> = (0..5).map(|i| now - i * min).collect();
    assert!(idle_tick_should_fire(0, now, 15, false, &under_cap, 6), "under the cap fires");
    let cap_0 = at_cap.clone();
    assert!(idle_tick_should_fire(0, now, 15, false, &cap_0, 0), "cap 0 = uncapped");
    // Clock skew: now before the quiet clock reads as zero elapsed, never a huge
    // interval that would spuriously fire.
    assert!(!idle_tick_should_fire(50 * min, 10 * min, 15, false, none, 6), "no underflow on skew");
}

#[test]
fn autonomy_budget_exhausted_rule() {
    // 0 budget = no cap, never exhausted.
    assert!(!autonomy_budget_exhausted(1_000_000, 0));
    // Under budget: fine. At/over: exhausted (inclusive boundary).
    assert!(!autonomy_budget_exhausted(499, 500));
    assert!(autonomy_budget_exhausted(500, 500), "exactly at budget suspends");
    assert!(autonomy_budget_exhausted(999, 500));
}

#[test]
fn idle_tick_fires_once_per_window_and_rearms_on_output() {
    let (reg, _d, gid, oid) = autonomous_setup();
    let empty = HashMap::new();
    // Output-quiet far past the window → exactly one tick, audited.
    assert_eq!(reg.idle_tick_tick(FAR, &empty, &empty), vec![oid.clone()],
        "an idle autonomous orchestrator must be idle-ticked");
    assert_eq!(audit_count(&reg, &gid, "idle-tick"), 1, "the tick must be audited once");
    // Anti-nag: still quiet, already notified → no second tick.
    assert!(reg.idle_tick_tick(FAR + 60_000, &empty, &empty).is_empty(),
        "one tick per idle window");
    // The orchestrator produces output (it acted on the tick): clock + latch
    // both reset, and this very tick can't also fire.
    let grew: HashMap<String, u64> = [(oid.clone(), 4096u64)].into_iter().collect();
    assert!(reg.idle_tick_tick(FAR, &grew, &empty).is_empty(),
        "output growth is activity, not an idle window");
    // No further growth; a whole fresh window elapses → a brand-new tick.
    assert_eq!(reg.idle_tick_tick(FAR + 15 * 60_000 + 1, &grew, &empty), vec![oid.clone()],
        "a new idle window after activity earns a new tick");
    assert_eq!(audit_count(&reg, &gid, "idle-tick"), 2);
}

#[test]
fn idle_tick_skips_non_autonomous_group() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    // Autonomous mode never enabled → the loop must ignore the group wholesale.
    let empty = HashMap::new();
    assert!(reg.idle_tick_tick(FAR, &empty, &empty).is_empty(),
        "a group without autonomous mode is never idle-ticked");
    assert_eq!(audit_count(&reg, &g.id, "idle-tick"), 0);
}

#[test]
fn idle_tick_skips_paused_group_preserving_latch() {
    let (reg, _d, gid, oid) = autonomous_setup();
    let empty = HashMap::new();
    reg.pause_group(&gid).unwrap();
    assert!(reg.idle_tick_tick(FAR, &empty, &empty).is_empty(),
        "a paused autonomous group is not idle-ticked");
    // The one-notice latch must be intact: pausing must not have burned it, so on
    // resume the outstanding idle window still earns its first tick.
    reg.resume_group(&gid).unwrap();
    assert_eq!(reg.idle_tick_tick(FAR, &empty, &empty), vec![oid.clone()],
        "resuming a still-idle autonomous group earns its first tick");
}

#[test]
fn idle_tick_defers_on_recent_human_input() {
    let (reg, _d, _gid, oid) = autonomous_setup();
    let empty = HashMap::new();
    // #496: anchor the output-based baseline (`last_output_progress_ms`) at FAR
    // first, with a real output burst — otherwise this test's own FAR time-skip
    // (from the agent's real spawn-time baseline) would itself look like the
    // orchestrator had gone output-silent for eons, and the input-defer bound
    // below would fire immediately regardless of "just typed" input. Seeding a
    // real burst here is what a live group's spawn + first tick already does;
    // it isn't new test scaffolding for the bound, just making this test's
    // synthetic clock skip land at a realistic starting point.
    let grew: HashMap<String, u64> = [(oid.clone(), 4096u64)].into_iter().collect();
    assert!(reg.idle_tick_tick(FAR, &grew, &empty).is_empty(), "output growth resets the clock, not a tick");
    // Review fix (round 1, NB-1): the defer assert MUST land past the ordinary
    // 5-minute quiet threshold, or the assertion passes even with the input
    // fold deleted entirely — vacuous coverage of the exact property this test
    // exists to guard. 6 minutes is past the threshold (proving the FOLD, not
    // mere quiet-window timing, is what defers here) while still comfortably
    // inside the 15-minute input-defer bound (so this stays the ORDINARY
    // defer, not `idle_tick_input_defer_max_minutes` kicking in — that's the
    // separate `idle_tick_fires_despite_perpetual_input_after_bound` test).
    let just_typed: HashMap<String, u64> = [(oid.clone(), FAR + 6 * 60_000)].into_iter().collect();
    assert!(reg.idle_tick_tick(FAR + 6 * 60_000, &empty, &just_typed).is_empty(),
        "never tick while the human is actively steering the pane, even though 6 minutes of \
         output-quiet alone (> the 5m threshold) would otherwise have fired");
    // Once the human input recedes past the window (its timestamp stays put
    // while real time moves on), the tick fires again — still comfortably
    // inside the bound, so this is the ordinary quiet-window recovery, not
    // `idle_tick_input_defer_max_minutes` kicking in.
    assert_eq!(reg.idle_tick_tick(FAR + 6 * 60_000 + 5 * 60_000 + 1, &empty, &just_typed), vec![oid.clone()],
        "after the human-input window elapses, the idle tick resumes");
}

#[test]
fn idle_tick_fires_despite_perpetual_input_after_bound() {
    // #496 root cause 2/2 (the guarantee, not the mechanism): xterm's own
    // automatic replies to a copilot pane's terminal queries (OSC colour, DA,
    // DSR/CPR, focus reports) restamp `last_user_input_ms` to "now" on EVERY
    // scan, forever, with no human present and no real output ever following
    // (PR-A fixes that mechanism at the source). This test doesn't assume
    // which mechanism refreshes the clock — it simulates the WORST case any
    // such refresher produces: input recency that tracks `now` exactly, every
    // single tick, unboundedly. Without a bound, the fold in `idle_tick_tick`
    // would set `last_progress_ms` to `now` every call, so `now - last_progress_ms`
    // never reaches the threshold and the tick can never fire — the deadlock
    // the human reported (#496): nothing recovers the group but a physical Enter.
    let (reg, _d, gid, oid) = autonomous_setup();
    let empty = HashMap::new();
    let grew: HashMap<String, u64> = [(oid.clone(), 4096u64)].into_iter().collect();
    let base = FAR;
    // One real output burst anchors the output-based baseline both clocks now
    // share; `idle_tick_input_defer_max_minutes` clamps the input fold against
    // THIS timestamp (`last_output_progress_ms`), never against whatever the
    // input clock itself claims.
    assert!(reg.idle_tick_tick(base, &grew, &empty).is_empty(), "output growth resets the clock, not a tick");

    let min = 60_000u64;
    // Perpetual "input": every scan, `inputs` reports the input timestamp as
    // exactly `now` — unbroken, advancing recency with no real output ever
    // following (`grew` stays flat: no growth = no activity). Companion
    // property (the plan's edge case): input still legitimately defers WITHIN
    // the bound, so genuine human steering is unaffected — asserted for every
    // minute up to the 15-minute default bound.
    for minute in 1..=19u64 {
        let now = base + minute * min;
        let inputs: HashMap<String, u64> = [(oid.clone(), now)].into_iter().collect();
        assert!(reg.idle_tick_tick(now, &grew, &inputs).is_empty(),
            "perpetual input must still defer the tick before the bound is reached (minute {minute})");
    }
    // Default bound (15m, `DEFAULT_IDLE_TICK_INPUT_DEFER_MAX_MINUTES`) plus the
    // default quiet threshold (5m, `DEFAULT_IDLE_TICK_MINUTES`) elapses since
    // the last real output. Input is STILL arriving every scan, right up to
    // `now` — genuinely perpetual, never lapsing — yet the clamp caps how far
    // it can defer and the tick must fire anyway.
    let now = base + 20 * min;
    let inputs: HashMap<String, u64> = [(oid.clone(), now)].into_iter().collect();
    assert_eq!(reg.idle_tick_tick(now, &grew, &inputs), vec![oid.clone()],
        "the input-defer bound must make perpetual input non-absorbing — the tick must fire eventually");
    assert_eq!(audit_count(&reg, &gid, "idle-tick-input-defer-bound"), 1,
        "a tick that fires because of the bound must be audited with its own distinct reason");
}

#[test]
fn idle_output_activity_ignores_subfloor_repaint_growth() {
    // The repaint-tolerant quiet signal: only a burst >= floor counts as the
    // orchestrator working; sub-floor creep is idle repaint noise.
    let floor = 2048u64;
    // Boundary is a `>=`: floor-1 is noise, floor is activity (rev-59 pin).
    assert!(!idle_output_is_activity(0, 2047, floor), "one byte under the floor is still noise");
    assert!(idle_output_is_activity(0, 2048, floor), "exactly at the floor is activity");
    assert!(idle_output_is_activity(1_000, 10_000, floor));
    assert!(!idle_output_is_activity(0, 200, floor), "a 200-byte statusline repaint is noise");
    assert!(!idle_output_is_activity(5_000, 5_200, floor), "sub-floor creep is not work");
    assert!(!idle_output_is_activity(5_000, 5_000, floor), "no growth is not activity");
    assert!(!idle_output_is_activity(5_000, 10, floor), "a counter reset (pty swap) is not activity");
}

#[test]
fn default_activity_floor_clears_a_real_idle_repaint_frame() {
    // Justify the 2048-byte default from real data: a captured full idle Claude
    // Code input-box render (box-drawing + ANSI) is the largest idle repaint frame
    // we have, and it must sit comfortably under the floor so it reads as noise.
    // (No raw idle-pane byte *stream* is captured anywhere and spawning a live CLI
    // is forbidden, so this rendered-frame size is the honest available measurement;
    // the tunable floor is the runtime remedy for a chattier CLI.)
    let frame = FIX_IDLE_BOX.len() as u64;
    assert!(frame < 2048, "idle box render is {frame}B — must be under the 2048B default floor");
    assert!(frame * 4 < 2048, "with ~4x headroom for a richer statusline, got {frame}B");
}

#[test]
fn idle_tick_tolerates_repaint_noise_but_resets_on_real_output() {
    // Root cause (b) regression: an idle orchestrator that emits periodic sub-floor
    // repaints (statusline/spinner) kept `output_total` creeping, so treating any
    // growth as activity reset the quiet clock every time and the tick never fired.
    let (reg, _d, gid, oid) = autonomous_setup();
    let m = |total: u64| -> HashMap<String, u64> { [(oid.clone(), total)].into_iter().collect() };
    let none = HashMap::new();
    // Sub-floor repaint growth over time must NOT reset the quiet clock: the tick
    // still fires after the threshold.
    assert!(reg.idle_tick_tick(1_000, &m(500), &none).is_empty(), "an early sub-floor repaint is not a tick");
    assert_eq!(reg.idle_tick_tick(FAR, &m(900), &none), vec![oid.clone()],
        "repaint-only growth must not starve the tick — it fires after the threshold");
    assert_eq!(audit_count(&reg, &gid, "idle-tick"), 1);
    // A REAL burst (>= floor) after the tick IS genuine activity: it resets the
    // clock and re-arms the latch, so this very pass can't fire.
    assert!(reg.idle_tick_tick(FAR, &m(5_000), &none).is_empty(),
        "a real output burst re-arms the latch and resets the clock");
    // No immediate re-fire right after real activity...
    assert!(reg.idle_tick_tick(FAR + 60_000, &m(5_000), &none).is_empty(),
        "no re-fire within the window after real output");
    // ...but after a fresh full (5-min) window of quiet — repaint noise tolerated —
    // it fires again.
    assert_eq!(reg.idle_tick_tick(FAR + 5 * 60_000 + 1, &m(5_000), &none), vec![oid.clone()],
        "a fresh threshold of quiet after activity earns a new tick");
    assert_eq!(audit_count(&reg, &gid, "idle-tick"), 2);
}

#[test]
fn idle_tick_minutes_is_configurable_persisted_and_surfaced() {
    // Root cause (a) fix: the window is a live-adjustable per-group knob (default 5),
    // so the human can drop it to 1–2 min to verify quickly, and the panel can see it.
    let (reg, dir, gid, _oid) = autonomous_setup();
    assert_eq!(reg.autonomy_state(&gid)["idle_tick_minutes"].as_u64().unwrap(), 5,
        "shipped default window is 5 minutes");
    // Live-set to 2 min: applied, persisted to the live guardrail, surfaced, audited.
    assert_eq!(reg.set_idle_tick_minutes(&gid, 2).unwrap(), 2);
    assert_eq!(reg.group(&gid).unwrap().guardrails.idle_tick_minutes, 2);
    assert_eq!(reg.autonomy_state(&gid)["idle_tick_minutes"].as_u64().unwrap(), 2);
    assert_eq!(audit_count(&reg, &gid, "idle-tick-minutes-set"), 1);
    // 0 coerces to the default (never "off" — the marker is the switch); huge clamps.
    assert_eq!(reg.set_idle_tick_minutes(&gid, 0).unwrap(), 5);
    assert_eq!(reg.set_idle_tick_minutes(&gid, 100_000).unwrap(), 1440);
    assert!(reg.set_idle_tick_minutes(&parse_gid("no-such-group"), 5).is_err());
    // Observability while ON: quiet_secs + eligible_in_secs are live, and the
    // countdown never exceeds the window.
    reg.set_idle_tick_minutes(&gid, 5).unwrap();
    let st = reg.autonomy_state(&gid);
    assert!(st["quiet_secs"].as_u64().is_some(), "quiet_secs is live while autonomous is on");
    let eligible = st["eligible_in_secs"].as_u64().unwrap();
    assert!(eligible <= 5 * 60, "eligible_in_secs counts down within the window, got {eligible}");
    // Persisted across restart (live-set value wins over the launch default).
    let reg2 = relaunch_registry(dir.path());
    reg2.set_port(45999);
    reg2.create_group("C:/tmp/repo", rails()).unwrap();
    assert_eq!(reg2.group(&gid).unwrap().guardrails.idle_tick_minutes, 5,
        "a live-set window survives restart");
    // OFF: no live meter.
    reg.set_autonomous(&gid, false).unwrap();
    let off = reg.autonomy_state(&gid);
    assert!(off["quiet_secs"].is_null(), "no quiet meter while autonomous is off");
    assert!(off["eligible_in_secs"].is_null());
    assert_eq!(off["tick_status"], "off");
}

#[test]
fn idle_tick_input_defer_bound_defaults_floors_caps_and_persists() {
    // #496: 0 (unset) resolves to the 15-minute default (3x DEFAULT_IDLE_TICK_MINUTES).
    let g = Guardrails { idle_tick_minutes: 5, ..rails() }.clamped();
    assert_eq!(g.idle_tick_input_defer_max_minutes, 15, "0 (unset) resolves to the 15m default");

    // Floored at the group's own (already-normalized) tick window: the bound can
    // never be tighter than the ordinary threshold — that would let it fire the
    // tick BEFORE `idle_tick_minutes` itself ever would.
    let g = Guardrails { idle_tick_minutes: 20, idle_tick_input_defer_max_minutes: 5, ..rails() }.clamped();
    assert_eq!(g.idle_tick_input_defer_max_minutes, 20, "floored at idle_tick_minutes, never tighter");

    // Capped at 24h — the same ceiling `idle_tick_minutes` uses.
    let g = Guardrails { idle_tick_input_defer_max_minutes: 100_000, ..rails() }.clamped();
    assert_eq!(g.idle_tick_input_defer_max_minutes, 1440, "capped at 24h");

    // No live setter this round (see the field's own doc), but it is still a
    // persisted, hand-editable guardrail: written to group.json and honored on
    // reload — same precedent as `idle_tick_fallback_minutes`/`context_window_
    // tokens_override`.
    let (reg, dir) = test_registry();
    let g = reg
        .create_group("C:/tmp/repo", Guardrails { idle_tick_input_defer_max_minutes: 45, ..rails() })
        .unwrap();
    assert_eq!(g.guardrails.idle_tick_input_defer_max_minutes, 45, "an explicit in-range value is honored, unclamped");
    let body = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("group.json")).unwrap();
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["guardrails"]["idle_tick_input_defer_max_minutes"].as_u64().unwrap(), 45,
        "the field is written to group.json, not silently dropped");
    let reg2 = relaunch_registry(dir.path());
    reg2.set_port(45999);
    let (_, persisted) = reg2.load_group_file(&g.id).unwrap();
    assert_eq!(persisted.idle_tick_input_defer_max_minutes, 45, "round-trips through group.json on a fresh load");
}

#[test]
fn idle_tick_input_defer_bound_floor_survives_launcher_relaunch() {
    // Round-1 review fix (rev-23): `load_group_file` alone doesn't exercise the
    // bug — the defect was in `create_group_ex`'s `resumed` re-hydration branch,
    // which resolved an unset (0) bound straight to `DEFAULT_IDLE_TICK_INPUT_
    // DEFER_MAX_MINUTES` (15) WITHOUT the `idle_tick_minutes` floor `clamped()`
    // applies everywhere else. A group with a tick window above 15 minutes and
    // no explicit bound — i.e. EVERY group.json written before this PR, since
    // the field didn't exist — got a tighter bound on relaunch than on a fresh
    // launch, silently disagreeing with the field's own doc and this PR's
    // design note. This is the relaunch leg the persistence test above was
    // missing (mirrors `intake_gate_config_survives_launcher_relaunch`'s
    // restart-a-fresh-registry shape).
    let dir = tempfile::tempdir().unwrap();
    let gid;
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        // idle_tick_minutes above the 15m default; the bound left UNSET (0) at
        // the call site.
        let g = reg.create_group("C:/tmp/repo", Guardrails { idle_tick_minutes: 30, ..rails() }).unwrap();
        assert_eq!(g.guardrails.idle_tick_input_defer_max_minutes, 30,
            "fresh create: an unset bound floors at the group's own 30m tick window, not the 15m default");
        gid = g.id.clone();
    }
    // Round-2 review fix (rev-23): a fresh `create_group` writes group.json
    // with the ALREADY-RESOLVED value (30, from `clamped()`), so without this
    // step the relaunch leg below would read `persisted == 30` and only ever
    // exercise the non-zero branch — the branch that was never broken. The
    // round-1 bug lived in the UNSET (`persisted == 0`) branch, which a
    // pre-#500 group.json hits because the key never existed. Strip the key
    // here so the file actually matches that shape and the relaunch leg below
    // exercises the branch the bug was in — a test that passed with the key
    // still present passed for the wrong reason (confirmed: it stayed green
    // with the round-1 defect restored).
    let path = dir.path().join(gid.as_str()).join("group.json");
    let mut v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    v["guardrails"].as_object_mut().unwrap().remove("idle_tick_input_defer_max_minutes");
    fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    // A fresh registry (app restart) + a launcher relaunch on the same repo,
    // with the launcher's bare defaults (`rails()`, no field for this at all):
    // the persisted `idle_tick_minutes` must resolve the SAME unset-bound
    // floor a fresh launch would give it.
    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    assert_eq!(g.id, gid, "restart resumes the same group");
    assert_eq!(g.guardrails.idle_tick_minutes, 30, "the tick window itself already survives relaunch");
    assert_eq!(g.guardrails.idle_tick_input_defer_max_minutes, 30,
        "resume: an unset bound must get the SAME floor clamped() applies on a fresh launch — never a bare 15 \
         that quietly promises less protection than the docs state for this group");
}

#[test]
fn idle_activity_floor_is_configurable_persisted_and_surfaced() {
    // rev-59 MODERATE: the activity floor is a live-tunable guardrail, not a bare
    // const — the runtime remedy if a chatty CLI's idle repaints exceed the default.
    let (reg, dir, gid, _oid) = autonomous_setup();
    assert_eq!(reg.autonomy_state(&gid)["idle_activity_floor_bytes"].as_u64().unwrap(), 2048,
        "shipped default floor is 2048 bytes");
    assert_eq!(reg.set_idle_activity_floor(&gid, 8192).unwrap(), 8192);
    assert_eq!(reg.group(&gid).unwrap().guardrails.idle_activity_floor_bytes, 8192);
    assert_eq!(reg.autonomy_state(&gid)["idle_activity_floor_bytes"].as_u64().unwrap(), 8192);
    assert_eq!(audit_count(&reg, &gid, "idle-activity-floor-set"), 1);
    // 0 → default; huge clamps to 1 MiB; unknown group errors.
    assert_eq!(reg.set_idle_activity_floor(&gid, 0).unwrap(), 2048);
    assert_eq!(reg.set_idle_activity_floor(&gid, 999_999_999).unwrap(), 1024 * 1024);
    assert!(reg.set_idle_activity_floor(&parse_gid("no-such-group"), 4096).is_err());
    // Persisted across restart (live value wins over the launch default).
    reg.set_idle_activity_floor(&gid, 4096).unwrap();
    let reg2 = relaunch_registry(dir.path());
    reg2.set_port(45999);
    reg2.create_group("C:/tmp/repo", rails()).unwrap();
    assert_eq!(reg2.group(&gid).unwrap().guardrails.idle_activity_floor_bytes, 4096,
        "a live-set activity floor survives restart");
}

#[test]
fn a_higher_activity_floor_treats_bigger_growth_as_noise() {
    // The floor actually governs the tick: raise it above a 5 KB burst and that
    // growth reads as repaint noise (doesn't reset the quiet clock), so the tick
    // still fires — while a burst >= the floor is activity and re-arms the latch.
    let (reg, _d, gid, oid) = autonomous_setup();
    reg.set_idle_activity_floor(&gid, 8192).unwrap();
    let m = |t: u64| -> HashMap<String, u64> { [(oid.clone(), t)].into_iter().collect() };
    let none = HashMap::new();
    assert!(reg.idle_tick_tick(1_000, &m(5_000), &none).is_empty());
    assert_eq!(reg.idle_tick_tick(FAR, &m(9_000), &none), vec![oid.clone()],
        "with an 8 KB floor, 4–5 KB growth is repaint noise and the tick still fires");
    assert!(reg.idle_tick_tick(FAR, &m(20_000), &none).is_empty(),
        "a growth >= the floor (11 KB) is activity and re-arms the latch");
}

// ---------- idle-tick intake gate (#332): host-side, zero-token pre-check ----------

/// An autonomous group with a caller-chosen `intake_poll_minutes` (tri-state:
/// `Some(n)` for an explicit cadence — `n == 0` is the explicit opt-out,
/// `n > 0` an explicit value — or `None` to exercise the #429 smart default,
/// which resolves to `DEFAULT_INTAKE_POLL_MINUTES` since this helper always
/// turns autonomous mode on) and a caller-chosen `fallback_minutes`. Returns
/// (reg, tempdir, group id, orchestrator id) — the same shape as `autonomous_setup`.
pub(crate) fn autonomous_setup_with_gate(intake_poll_minutes: Option<u32>, fallback_minutes: u32) -> (OrchRegistry, tempfile::TempDir, GroupId, String) {
    let (reg, dir) = test_registry();
    let g = reg
        .create_group(
            "C:/tmp/repo",
            Guardrails { intake_poll_minutes, idle_tick_fallback_minutes: fallback_minutes, ..rails() },
        )
        .unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    reg.set_autonomous(&g.id, true).unwrap();
    (reg, dir, g.id, o.id)
}

/// Whether `audit.jsonl` contains a line for `action` whose raw JSON also
/// contains `needle` — a coarser version of `audit_count` that lets a test
/// assert on the SHAPE of one entry (e.g. the skip reason, the intake summary
/// folded into an `idle-tick` entry) without parsing full JSON.
fn audit_line_contains(reg: &OrchRegistry, group: &GroupId, action: &str, needle: &str) -> bool {
    fs::read_to_string(reg.state_root().join(group.as_str()).join("audit.jsonl"))
        .unwrap_or_default()
        .lines()
        .any(|l| l.contains(&format!("\"{action}\"")) && l.contains(needle))
}

#[test]
fn intake_gate_skips_a_quiet_tick_then_fires_once_a_signal_lands() {
    // Gate ON, generous fallback (won't fire on its own inside this test).
    let (reg, _d, gid, oid) = autonomous_setup_with_gate(Some(5), 180);
    let empty = HashMap::new();
    // Establish a reference fire point so the fallback isn't trivially due against
    // the unseeded (0) default — see `seed_idle_tick_last_fired`'s doc.
    reg.seed_idle_tick_last_fired(&gid, FAR);

    // Quiet window elapsed, nothing new, no other wake reason, fallback not due:
    // the tick must be SKIPPED, not fired — and the skip is audited with a reason.
    assert!(reg.idle_tick_tick(FAR + 15 * 60_000 + 1, &empty, &empty).is_empty(),
        "nothing new + no other wake reason + fallback not due must SKIP, not fire");
    assert_eq!(audit_count(&reg, &gid, "idle-tick"), 0, "a skip must never be counted as a fire");
    assert!(audit_line_contains(&reg, &gid, "idle-tick-skipped", "fallback"),
        "the skip must be audited with its reason, not silent");

    // The host-side poller finds something: seed the pending summary a real
    // `poll_intake` would have composed, then the NEXT quiet window must fire —
    // and the notice must carry the summary (folded into the audited entry).
    reg.seed_intake_pending(&gid, "issue #42 labeled agent-ready (\"Do the thing\")");
    assert_eq!(reg.idle_tick_tick(FAR + 2 * (15 * 60_000 + 1), &empty, &empty), vec![oid.clone()],
        "an intake signal must fire even though nothing else changed");
    assert!(audit_line_contains(&reg, &gid, "idle-tick", "issue #42 labeled agent-ready"),
        "the fired tick's audit entry must carry what the intake poll found");
}

#[test]
fn intake_gate_fires_regardless_when_a_ci_watch_is_outstanding() {
    // The lost-notification-degrades-to-poll-on-sweep invariant (orchestrator.md):
    // an outstanding CI watch means the tick's fallback-sweep duty still has a job,
    // even with zero label/PR news.
    let (reg, _d, gid, oid) = autonomous_setup_with_gate(Some(5), 180);
    reg.seed_idle_tick_last_fired(&gid, FAR);
    reg.register_notification(&gid, &oid, notify::Condition::PrChecks { pr: 7 }, "watch it".into(), 60).unwrap();

    let empty = HashMap::new();
    assert_eq!(reg.idle_tick_tick(FAR + 15 * 60_000 + 1, &empty, &empty), vec![oid.clone()],
        "a pending notification must force a fire even with no intake signal");
}

#[test]
fn intake_gate_fires_regardless_when_a_watchdog_stall_is_unresolved() {
    let (reg, _d) = test_registry();
    let g = reg
        .create_group(
            "C:/tmp/repo",
            Guardrails { intake_poll_minutes: Some(5), idle_tick_fallback_minutes: 180, watchdog_stall_minutes: 10, ..rails() },
        )
        .unwrap();
    let oid = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap().id;
    reg.set_autonomous(&g.id, true).unwrap();
    reg.seed_idle_tick_last_fired(&g.id, FAR);
    // Drive a REAL watchdog stall on a worker in the same group — the gate reads
    // `AgentEntry.watchdog_notified`, not a test-only backdoor.
    reg.spawn_agent(&g.id, Role::Worker, "w", "do work", false, None).unwrap();
    let watch_set = std::collections::HashMap::new();
    let fired = reg.watchdog_tick(FAR, &HashMap::new(), &watch_set);
    assert!(!fired.is_empty(), "sanity: the worker must actually be flagged stalled");

    let empty = HashMap::new();
    assert_eq!(reg.idle_tick_tick(FAR + 15 * 60_000 + 1, &empty, &empty), vec![oid.clone()],
        "an unresolved watchdog stall must force a fire even with no intake signal");
}

#[test]
fn intake_gate_fallback_fires_unconditionally_once_it_comes_due() {
    // No intake signal, no pending notification, no watchdog stall — but enough
    // real time has passed since the last fire that the bounded fallback covers it,
    // so a poller bug (or a permanently-quiet group) can never silence the tick.
    let (reg, _d, gid, oid) = autonomous_setup_with_gate(Some(5), 30); // 30 min = the floor
    reg.seed_idle_tick_last_fired(&gid, FAR);
    let empty = HashMap::new();

    // Just under the fallback: still skipped.
    assert!(reg.idle_tick_tick(FAR + 15 * 60_000 + 1, &empty, &empty).is_empty());
    // Past the fallback (30 min since the seeded last-fire): fires even with
    // nothing new.
    assert_eq!(reg.idle_tick_tick(FAR + 30 * 60_000 + 1, &empty, &empty), vec![oid.clone()],
        "the bounded fallback must fire regardless once its own interval elapses");
}

#[test]
fn intake_gate_off_fires_unconditionally_exactly_like_before_332() {
    // #429 reversed the migration-safety default (absent/None now smart-
    // defaults ON while autonomous — see the next test), but the explicit
    // opt-out must still work exactly like the pre-#429 "off" behavior for an
    // operator who deliberately wants the polling load off: `Some(0)` is that
    // explicit opt-out, distinct from `None`.
    let (reg, _d, gid, oid) = autonomous_setup_with_gate(Some(0), 30);
    let empty = HashMap::new();
    assert_eq!(reg.idle_tick_tick(FAR, &empty, &empty), vec![oid.clone()],
        "gate OFF must fire unconditionally, matching pre-#332 behavior");
    assert!(!audit_line_contains(&reg, &gid, "idle-tick-skipped", "fallback"),
        "a disabled gate must never produce a skip");
    assert!(audit_line_contains(&reg, &gid, "idle-tick", "\"gate_enabled\":false"),
        "a bypass fire (gate off) must be observably distinct from a gated fire — the #429 \
         benchtest finding was exactly that these two looked identical in the audit log");
}

// ---------- #864: the fallback backs off while a group stays delta-free ----------
//
// Evidence from a real parked weekend (group loomux-68435179, Aug 9–11): ~30
// consecutive idle ticks, every open item human-gated, every tick's sweep
// finding zero change, each costing 1–2 API turns over the orchestrator's whole
// ~180k-token prefix. The gate was working — there was genuinely nothing to
// report — but the UNCONDITIONAL fallback underneath it fired on a cadence that
// ignored how long the group had been delta-free.
//
// These tests assert on WHEN the orchestrator is actually woken across a
// simulated timeline, not on the counter behind it: "the cadence decays" is the
// behaviour #864 asks for, and a test that only read the streak field would
// pass just as happily if the interval it feeds were never consulted.

/// Like `autonomous_setup_with_gate`, plus #864's backoff ceiling.
fn autonomous_setup_with_backoff(
    intake_poll_minutes: Option<u32>,
    fallback_minutes: u32,
    fallback_max_minutes: u32,
) -> (OrchRegistry, tempfile::TempDir, GroupId, String) {
    let (reg, dir) = test_registry();
    let g = reg
        .create_group(
            "C:/tmp/repo",
            Guardrails {
                intake_poll_minutes,
                idle_tick_fallback_minutes: fallback_minutes,
                idle_tick_fallback_max_minutes: fallback_max_minutes,
                ..rails()
            },
        )
        .unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    reg.set_autonomous(&g.id, true).unwrap();
    (reg, dir, g.id, o.id)
}

/// Drive `idle_tick_tick` across a simulated timeline and return the minute
/// offsets (from `start_ms`) at which the orchestrator was actually woken.
///
/// **The orchestrator's reply to each wake is simulated** — a real burst of
/// output on the scan after every fire — because that burst is what clears the
/// one-notice latch in production. A timeline that never produced it would fire
/// exactly once and then sit latched forever, proving nothing about cadence;
/// worse, it would hide the case that matters most here, since a parked
/// orchestrator's output is overwhelmingly its own answer to the previous wake.
///
/// `before_step` runs at each simulated minute and returns that scan's
/// human-input map, so a test can also inject a mid-timeline event (a
/// keystroke, a host-side signal landing) at a chosen offset.
fn idle_tick_wake_offsets(
    reg: &OrchRegistry,
    orch: &str,
    start_ms: u64,
    span_minutes: u64,
    step_minutes: u64,
    mut before_step: impl FnMut(u64) -> HashMap<String, u64>,
) -> Vec<u64> {
    let mut wakes = Vec::new();
    let mut output_total = 0u64;
    let mut replying = false;
    let mut t = 0;
    while t <= span_minutes {
        if replying {
            // A real orchestrator turn dumps many KB — comfortably over the
            // activity floor, so this reads as "it acted", exactly like the
            // sweep a delivered wake provokes.
            output_total += 100_000;
            replying = false;
        }
        let inputs = before_step(t);
        let outputs: HashMap<String, u64> = [(orch.to_string(), output_total)].into_iter().collect();
        if !reg.idle_tick_tick(start_ms + t * 60_000, &outputs, &inputs).is_empty() {
            wakes.push(t);
            replying = true;
        }
        t += step_minutes;
    }
    wakes
}

/// Consecutive gaps between wake offsets — the cadence itself, which is what
/// every assertion below is really about.
fn wake_gaps(wakes: &[u64]) -> Vec<u64> {
    wakes.windows(2).map(|w| w[1] - w[0]).collect()
}

#[test]
fn a_delta_free_group_wakes_less_and_less_often_up_to_the_ceiling() {
    // Base 30 min, ceiling 240 min: the cadence must double per delta-free
    // wake and then hold — 30, 60, 120, 240, 240 — instead of charging the
    // base cadence forever the way a fixed fallback does.
    let (reg, _d, gid, oid) = autonomous_setup_with_backoff(Some(5), 30, 240);
    reg.seed_idle_tick_last_fired(&gid, FAR);

    let wakes = idle_tick_wake_offsets(&reg, &oid, FAR, 12 * 60, 5, |_| HashMap::new());

    assert_eq!(wakes, vec![30, 90, 210, 450, 690],
        "a delta-free group's wakes must spread out geometrically, not repeat every {} min", 30);
    assert_eq!(wake_gaps(&wakes), vec![60, 120, 240, 240],
        "each delta-free wake doubles the next interval until the ceiling holds it");
    // The economics, stated as the thing the issue actually asked for: over
    // half a day this is 5 wakes, where a fixed 30-minute fallback is 24.
    assert!(wakes.len() < 12 * 60 / 30 / 2, "the whole point is a decayed COST, got {} wakes", wakes.len());
}

#[test]
fn the_ceiling_is_a_ceiling_not_a_silence() {
    // The fallback's own reason for existing survives the backoff: a group
    // that never sees another delta is still woken unconditionally, just
    // coarsely. Run four ceiling-widths past saturation and count.
    let (reg, _d, gid, oid) = autonomous_setup_with_backoff(Some(5), 30, 120);
    reg.seed_idle_tick_last_fired(&gid, FAR);

    let wakes = idle_tick_wake_offsets(&reg, &oid, FAR, 12 * 60, 5, |_| HashMap::new());
    let gaps = wake_gaps(&wakes);
    assert_eq!(gaps.last(), Some(&120), "the cadence must settle AT the ceiling, got {gaps:?}");
    assert!(gaps.iter().all(|&g| g <= 120), "no interval may ever exceed the ceiling: {gaps:?}");
    assert!(wakes.len() >= 6, "a saturated group must keep being woken once per ceiling, got {wakes:?}");
}

#[test]
fn a_host_side_delta_resets_the_cadence_to_the_base() {
    // The reset that matters most: the group decays while nothing happens,
    // then something DOES (the host-side poller finds a labeled issue), and
    // the very next unconditional wake is back at the base interval — not
    // still 120 minutes out because of quiet that is no longer true.
    let (reg, _d, gid, oid) = autonomous_setup_with_backoff(Some(5), 30, 240);
    reg.seed_idle_tick_last_fired(&gid, FAR);

    let wakes = idle_tick_wake_offsets(&reg, &oid, FAR, 140, 5, |t| {
        // Two delta-free wakes have landed by t=90 (streak 2, next interval
        // 120). Land a real signal just after, mid-decay.
        if t == 100 {
            reg.seed_intake_pending(&gid, "issue #42 labeled agent-ready (\"Do the thing\")");
        }
        HashMap::new()
    });

    assert_eq!(&wakes[..2], &[30, 90], "sanity: the cadence decays before the signal lands");
    assert_eq!(wakes[2], 100, "a host-side signal wakes the orchestrator immediately, not on the fallback");
    assert_eq!(reg.idle_tick_empty_streak_of(&gid), 1,
        "the signal wake resets the streak, and the delta-free wake after it re-counts from zero");
    assert_eq!(wakes[3], 130, "the wake after a real signal must be one BASE interval later, not a decayed one");
}

#[test]
fn human_input_resets_the_cadence_to_the_base() {
    // "Reset on any delta or human input": someone at the keyboard means the
    // group is no longer parked, whether or not anything on GitHub moved.
    let (reg, _d, gid, oid) = autonomous_setup_with_backoff(Some(5), 30, 240);
    reg.seed_idle_tick_last_fired(&gid, FAR);
    let typed_at = FAR + 100 * 60_000;

    let wakes = idle_tick_wake_offsets(&reg, &oid, FAR, 4 * 60, 5, |t| {
        // The pty reports the LAST input timestamp, so once the human types it
        // keeps reporting that instant on every later scan — modelled here.
        if t >= 100 { [(oid.clone(), typed_at)].into_iter().collect() } else { HashMap::new() }
    });

    assert_eq!(&wakes[..2], &[30, 90], "sanity: the cadence decays while nobody is there");
    assert_eq!(wakes[2], 120,
        "after a keystroke the next unconditional wake is one BASE interval after the last fire, \
         not the 120 minutes the decayed cadence had earned");
}

#[test]
fn a_group_with_a_live_delegate_never_backs_off() {
    // A group with agents in flight is not parked by any definition, and the
    // heartbeat wake is exactly how an orchestrator notices a delegate that
    // went quiet without reporting. The backoff must not touch it.
    let (reg, _d, gid, oid) = autonomous_setup_with_backoff(Some(5), 30, 240);
    reg.spawn_agent(&gid, Role::Worker, "w", "do work", false, None).unwrap();
    reg.seed_idle_tick_last_fired(&gid, FAR);

    let wakes = idle_tick_wake_offsets(&reg, &oid, FAR, 3 * 60, 5, |_| HashMap::new());

    assert_eq!(wakes, vec![30, 60, 90, 120, 150, 180],
        "with a delegate alive the fallback stays at its base cadence, got {wakes:?}");
    assert_eq!(reg.idle_tick_empty_streak_of(&gid), 0, "and no streak is ever accumulated");
}

#[test]
fn the_backoff_starts_clean_once_the_last_delegate_exits() {
    // The window a reset-only suppressor got wrong (caught by the assertion in
    // the test above, on run 31556665796): a wake writes the streak AFTER the
    // suppressor's reset lands, so between a fire and the next scan a group
    // with a live delegate carried a `1` it never earned — and a delegate
    // exiting right there handed the newly-parked group a doubling for free.
    // With the fix the streak is 0 at all times while suppressed, so the
    // backoff starts from the base interval the moment the group is parked.
    let (reg, _d, gid, oid) = autonomous_setup_with_backoff(Some(5), 30, 240);
    let w = reg.spawn_agent(&gid, Role::Worker, "w", "do work", false, None).unwrap();
    reg.seed_idle_tick_last_fired(&gid, FAR);

    let wakes = idle_tick_wake_offsets(&reg, &oid, FAR, 4 * 60, 5, |t| {
        // The delegate exits on the scan immediately after the first wake.
        if t == 35 {
            reg.mark_dead(&w.id, Some(0));
        }
        HashMap::new()
    });

    assert_eq!(wakes, vec![30, 60, 120, 240],
        "the first wake after the delegate exits must be one BASE interval later — a stale streak \
         would push it to 90 and skew every interval after it — got {wakes:?}");
}

#[test]
fn backoff_is_off_when_the_ceiling_equals_the_base() {
    // The documented opt-out — one value, no separate enable flag — must
    // reproduce the pre-#864 fixed cadence exactly.
    let (reg, _d, gid, oid) = autonomous_setup_with_backoff(Some(5), 30, 30);
    reg.seed_idle_tick_last_fired(&gid, FAR);

    let wakes = idle_tick_wake_offsets(&reg, &oid, FAR, 3 * 60, 5, |_| HashMap::new());

    assert_eq!(wakes, vec![30, 60, 90, 120, 150, 180],
        "ceiling == base must pin the old fixed-cadence behaviour, got {wakes:?}");
}

#[test]
fn a_backed_off_wake_is_audited_with_the_cadence_that_produced_it() {
    // A cadence that decays silently is a cadence nobody can debug — the same
    // observability argument #429 made for `heartbeat`/`gate_enabled`.
    let (reg, _d, gid, oid) = autonomous_setup_with_backoff(Some(5), 30, 240);
    reg.seed_idle_tick_last_fired(&gid, FAR);
    idle_tick_wake_offsets(&reg, &oid, FAR, 12 * 60, 5, |_| HashMap::new());

    assert!(audit_line_contains(&reg, &gid, "idle-tick", "\"empty_streak\":3"),
        "each delta-free wake must record the streak it reached");
    assert!(audit_line_contains(&reg, &gid, "idle-tick", "\"fallback_minutes\":240"),
        "and the effective interval that produced it");
    assert!(audit_line_contains(&reg, &gid, "idle-tick-skipped", "delta-free wake(s)"),
        "a skip must state the effective cadence it is waiting on, not just 'not yet due'");
}

#[test]
fn the_backoff_ceiling_is_normalized_and_survives_a_relaunch() {
    // Same shape as `intake_gate_config_survives_launcher_relaunch`: the
    // ceiling is hand-edited into group.json with no live setter, so a
    // launcher relaunch (whose caller Guardrails have no field for it) must
    // not silently reset it.
    let g = Guardrails { idle_tick_fallback_minutes: 180, idle_tick_fallback_max_minutes: 0, ..rails() }.clamped();
    assert_eq!(g.idle_tick_fallback_max_minutes, 60 * 24,
        "0 = unset must resolve to the default ceiling, so a pre-#864 group.json gets the backoff");
    let g = Guardrails { idle_tick_fallback_minutes: 180, idle_tick_fallback_max_minutes: 30, ..rails() }.clamped();
    assert_eq!(g.idle_tick_fallback_max_minutes, 180,
        "a ceiling below the base is floored at the base — the backoff may never fire FASTER than configured");

    let dir = tempfile::tempdir().unwrap();
    let gid;
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        let g = reg
            .create_group(
                "C:/tmp/repo",
                Guardrails { idle_tick_fallback_minutes: 45, idle_tick_fallback_max_minutes: 90, ..rails() },
            )
            .unwrap();
        gid = g.id.clone();
    }
    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    assert_eq!(g.id, gid, "restart resumes the same group");
    assert_eq!(g.guardrails.idle_tick_fallback_max_minutes, 90,
        "a hand-edited ceiling must survive a launcher relaunch, not reset to the default");
}

// ---------- #429 benchtest follow-up: suppressed/heartbeat observability ----------
//
// rev-95's live testbed run (loomux-testbed-cc077f09) showed six idle ticks in a row,
// each with `intake_summary: null`, EVERY one still delivering the full generic wake
// prompt — "the gate computes but does not gate". The gate's actual fire/skip DECISION
// was already correct (proved by the suite above); what was missing is that a skip, a
// real-signal fire, and a fallback-only fire were audited identically, so nobody could
// tell them apart after the fact. These tests pin the three new audit fields
// (`suppressed`, `heartbeat`, `gate_enabled`) that make the three cases distinguishable,
// and the transition/consecutive-tick shapes the fix brief asked for directly.

#[test]
fn intake_gate_null_intake_is_suppressed_and_audited() {
    let (reg, _d, gid, _oid) = autonomous_setup_with_gate(Some(5), 180); // generous fallback
    reg.seed_idle_tick_last_fired(&gid, FAR);
    let empty = HashMap::new();

    assert!(reg.idle_tick_tick(FAR + 15 * 60_000 + 1, &empty, &empty).is_empty(),
        "null intake + no other reason must suppress, not deliver");
    assert_eq!(audit_count(&reg, &gid, "idle-tick"), 0, "a suppressed tick is never counted as a fire");
    assert!(audit_line_contains(&reg, &gid, "idle-tick-skipped", "\"suppressed\":true"),
        "a suppressed tick must be observably marked, not just implied by the action name");
}

#[test]
fn intake_gate_heartbeat_fires_after_consecutive_suppressed_ticks() {
    // intake_minutes=10, fallback=30 → the bounded backstop comes due after exactly 2
    // consecutive suppressed ticks (each skip re-arms the gate 10 minutes out), proving
    // "after N consecutive suppressed ticks, deliver a real wake anyway" against the
    // ACTUAL per-tick cadence rather than one big wall-clock jump.
    let (reg, _d, gid, oid) = autonomous_setup_with_gate(Some(10), 30);
    reg.seed_idle_tick_last_fired(&gid, FAR);
    let empty = HashMap::new();

    let tick1 = FAR + 15 * 60_000 + 1;
    assert!(reg.idle_tick_tick(tick1, &empty, &empty).is_empty(), "1st consecutive suppressed tick");
    let tick2 = tick1 + 10 * 60_000;
    assert!(reg.idle_tick_tick(tick2, &empty, &empty).is_empty(), "2nd consecutive suppressed tick");
    assert_eq!(audit_count(&reg, &gid, "idle-tick"), 0, "still nothing but suppressions so far");

    let tick3 = tick2 + 10 * 60_000;
    assert_eq!(reg.idle_tick_tick(tick3, &empty, &empty), vec![oid.clone()],
        "the bounded fallback must deliver a real wake once it comes due, even with nothing new — \
         a gate bug (or a genuinely quiet group) must never silence the orchestrator forever");
    assert!(audit_line_contains(&reg, &gid, "idle-tick", "\"heartbeat\":true"),
        "a fallback-only fire must be marked as a heartbeat, distinct from a real signal");
}

#[test]
fn intake_gate_transition_from_suppressed_to_signal_fires_immediately_with_summary_embedded() {
    // (null, null, intake) → the signal must fire on the very next eligible tick, not
    // wait for the heartbeat's consecutive-tick count — a fresh signal is never held
    // hostage by the backstop timer just because prior ticks were suppressed.
    let (reg, _d, gid, oid) = autonomous_setup_with_gate(Some(10), 180); // generous fallback: won't fire on its own
    reg.seed_idle_tick_last_fired(&gid, FAR);
    let empty = HashMap::new();

    let tick1 = FAR + 15 * 60_000 + 1;
    assert!(reg.idle_tick_tick(tick1, &empty, &empty).is_empty(), "1st: null intake, suppressed");
    let tick2 = tick1 + 10 * 60_000;
    assert!(reg.idle_tick_tick(tick2, &empty, &empty).is_empty(), "2nd: null intake, suppressed");

    reg.seed_intake_pending(&gid, "issue #77 labeled agent-ready (\"Fix the thing\")");
    let tick3 = tick2 + 10 * 60_000;
    assert_eq!(reg.idle_tick_tick(tick3, &empty, &empty), vec![oid.clone()],
        "a fresh intake signal must fire immediately, not wait out the fallback window");
    assert!(audit_line_contains(&reg, &gid, "idle-tick", "issue #77 labeled agent-ready"),
        "the delivered notice must carry the summary so the orchestrator acts on it directly");
    assert!(audit_line_contains(&reg, &gid, "idle-tick", "\"heartbeat\":false"),
        "a real-signal fire must never be mistaken for a heartbeat");
}

#[test]
fn intake_gate_smart_default_activates_while_autonomous_and_reproduces_the_benchtest_scenario() {
    // User-directed fix (option 1): the gate must be ON by default the moment a group is
    // autonomous, with NO config at all — `intake_poll_minutes: None`. This end-to-end
    // reproduction pins the actual reported shape: `loomux-testbed-cc077f09` logged SIX
    // idle ticks in a row with `intake_summary: null`, every one still delivering a wake.
    // On this fix, the same six null ticks must now be suppressed (zero wake prompts,
    // `suppressed:true` each time) and only the 7th — the bounded heartbeat — delivers.
    let (reg, _d, gid, oid) = autonomous_setup_with_gate(None, 45); // smart default cadence (5 min) x fallback
    reg.seed_idle_tick_last_fired(&gid, FAR);
    let empty = HashMap::new();

    let mut now = FAR + 15 * 60_000 + 1;
    for i in 1..=6 {
        assert!(reg.idle_tick_tick(now, &empty, &empty).is_empty(),
            "consecutive suppressed tick {i}/6 — no config was set, only autonomous mode");
        now += 5 * 60_000; // DEFAULT_INTAKE_POLL_MINUTES, smart-defaulted with no config at all
    }
    assert_eq!(audit_count(&reg, &gid, "idle-tick"), 0,
        "zero wake prompts delivered across all six null-intake ticks — this is the exact \
         benchtest finding, now fixed");
    assert_eq!(audit_count(&reg, &gid, "idle-tick-skipped"), 6);

    assert_eq!(reg.idle_tick_tick(now, &empty, &empty), vec![oid.clone()],
        "the 7th tick is the bounded heartbeat — a gate bug (or a genuinely quiet group) must \
         never silence the orchestrator forever, even under the smart default");
    assert!(audit_line_contains(&reg, &gid, "idle-tick", "\"heartbeat\":true"));
    assert!(audit_line_contains(&reg, &gid, "idle-tick", "\"gate_enabled\":true"),
        "the smart default counts as the gate being genuinely engaged, not the legacy bypass");
}

#[test]
fn intake_gate_skip_for_one_group_never_starves_another_groups_tick_in_the_same_scan() {
    // rev-31 finding 2 (#329 coexistence seam), written while #329 was still not
    // in this tree: `idle_tick_tick` scans every autonomous orchestrator in ONE
    // pass, and any other per-tick mechanism sharing this loop needs a SKIP
    // decision for one agent to have ZERO effect on any other agent processed in
    // the same call. A gated-and-skipped group and a plain (gate-off) group,
    // ticked in the SAME `idle_tick_tick` call, must each resolve independently
    // — the skip must never short-circuit the scan. See the test right below
    // for the direct #329 cross-feature check, now that #329 has landed and
    // `compact_nudge_tick` is real code in this tree, not a description of one.
    let (reg, _d, gid_gated, _oid_gated) = autonomous_setup_with_gate(Some(5), 180);
    reg.seed_idle_tick_last_fired(&gid_gated, FAR);
    // #429: `rails()`'s default `intake_poll_minutes: None` now smart-defaults ON the
    // moment a group is autonomous — so "plain, gate OFF" needs an EXPLICIT opt-out
    // (`Some(0)`) to keep meaning what this test's comment says, instead of silently
    // also becoming a gated group via the new default.
    let g2 = reg
        .create_group("C:/tmp/repo-plain", Guardrails { intake_poll_minutes: Some(0), ..rails() })
        .unwrap(); // gate explicitly OFF
    let oid_plain = reg.spawn_agent(&g2.id, Role::Orchestrator, "orch2", "", false, None).unwrap().id;
    reg.set_autonomous(&g2.id, true).unwrap();

    let empty = HashMap::new();
    let fired = reg.idle_tick_tick(FAR + 15 * 60_000 + 1, &empty, &empty);
    assert_eq!(fired, vec![oid_plain.clone()],
        "the gated group must skip (nothing new) while the plain group in the SAME scan still \
         fires unconditionally — one group's skip must never starve another's tick");
}

#[test]
fn intake_gate_skip_never_starves_a_coexisting_compact_nudge_for_the_same_agent() {
    // rev-31 finding 2, re-verified against #329's ACTUAL final shape (now
    // merged): compact-nudge is NOT woven into `idle_tick_tick` at all — it is
    // its own background thread/function (`start_compact_nudge` /
    // `run_compact_nudge` / `compact_nudge_tick`) with its own anti-nag latch
    // (`compact_nudge_notified`) and its own output baseline
    // (`compact_nudge_last_output_total`, deliberately separate from this
    // gate's `last_output_total` — see that field's doc for the rev-24 finding
    // this avoids). The only thing the two mechanisms share is a READ of
    // `AgentEntry.last_progress_ms`, and this gate's skip path never writes to
    // it differently depending on the gate's verdict. So the direct proof: the
    // SAME agent, gated-and-skipped on `idle_tick_tick`, must still get a real
    // `compact_nudge_tick` fire in the same tick window — the skip has zero
    // effect on the coexisting mechanism.
    let (reg, _d) = test_registry();
    let g = reg
        .create_group(
            "C:/tmp/repo",
            Guardrails {
                intake_poll_minutes: Some(5),
                idle_tick_fallback_minutes: 180,
                compact_nudge_minutes: 20,
                compact_nudge_roles: vec!["orchestrator".to_string()],
                ..rails()
            },
        )
        .unwrap();
    let oid = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap().id;
    reg.set_autonomous(&g.id, true).unwrap();
    reg.seed_idle_tick_last_fired(&g.id, FAR);

    let empty = HashMap::new();
    let now = FAR + 15 * 60_000 + 1;
    assert!(reg.idle_tick_tick(now, &empty, &empty).is_empty(),
        "sanity: the intake gate must actually skip here (nothing new, fallback not due)");
    assert_eq!(
        reg.compact_nudge_tick(now, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()),
        vec![oid.clone()],
        "a gated idle-tick SKIP for this exact agent, at this exact tick, must not stop the \
         coexisting compact-nudge check from firing for it — the two are fully independent"
    );
}

#[test]
fn idle_tick_status_is_honest_about_latch_and_cap() {
    // rev-59 LOW: eligible_in_secs must never render a lying 0 while a non-time gate
    // (latch / per-hour cap) holds the tick. tick_status carries the honest reason.
    let (reg, _d, gid, oid) = autonomous_setup();
    let m = |t: u64| -> HashMap<String, u64> { [(oid.clone(), t)].into_iter().collect() };
    let none = HashMap::new();
    let win = 5 * 60_000u64; // default 5-min window, in ms

    // 1) Fresh: counting down toward the first tick.
    let s = reg.autonomy_state(&gid);
    assert_eq!(s["tick_status"], "counting_down");
    assert!(s["eligible_in_secs"].as_u64().unwrap() <= 5 * 60);

    // 2) After a tick fires the latch is set: waiting_for_activity, secs NULL — the
    //    core rev-59 case (a countdown here would hit 0 while nothing fires).
    assert_eq!(reg.idle_tick_tick(FAR, &none, &none), vec![oid.clone()]);
    let s = reg.autonomy_state(&gid);
    assert_eq!(s["tick_status"], "waiting_for_activity");
    assert!(s["eligible_in_secs"].is_null(), "a latched tick must not render a countdown");

    // 3) A real burst clears the latch and resets the clock so far in the (synthetic)
    //    past that it reads as eligible now.
    assert!(reg.idle_tick_tick(1_000, &m(100_000), &none).is_empty());
    let s = reg.autonomy_state(&gid);
    assert_eq!(s["tick_status"], "eligible");
    assert_eq!(s["eligible_in_secs"].as_u64().unwrap(), 0);

    // 4) Fill the per-hour cap to MAX_IDLE_TICKS_PER_HOUR (6). Step 2 already fired
    //    one, so 5 more here reach the cap. Each needs the latch cleared (a real
    //    burst that also resets the clock) then a full window of quiet.
    for i in 0..5u64 {
        let base = FAR + i * (win + 10);
        assert!(reg.idle_tick_tick(base, &m(1_000_000 + i * 100_000), &none).is_empty(),
            "burst i={i} resets, no fire");
        assert_eq!(reg.idle_tick_tick(base + win + 1, &none, &none), vec![oid.clone()],
            "a fresh window after the burst fires (i={i})");
    }
    // Cap now full; clear the last fire's latch with a burst (adds no tick_time) so
    // the CAP is the sole remaining gate.
    assert!(reg.idle_tick_tick(FAR + 100 * win, &m(9_000_000), &none).is_empty());
    let s = reg.autonomy_state(&gid);
    assert_eq!(s["tick_status"], "rate_capped", "cap full + latch clear → rate_capped");
    assert!(s["eligible_in_secs"].as_u64().is_some(),
        "rate_capped still yields a real (cap-based) countdown, not null");
}

#[test]
fn idle_tick_status_reports_paused_with_no_countdown() {
    // rev-59 re-check: autonomous and paused are INDEPENDENT markers. A paused
    // autonomous group suppresses all delivery, so the tick never fires — the panel
    // must not render a live countdown (the exact lying-countdown class).
    let (reg, _d, gid, _oid) = autonomous_setup();
    reg.pause_group(&gid).unwrap();
    let s = reg.autonomy_state(&gid);
    assert_eq!(s["tick_status"], "paused", "a paused autonomous group reports paused");
    assert!(s["eligible_in_secs"].is_null(), "paused must not render a ticking countdown");
    // Resuming restores a live countdown.
    reg.resume_group(&gid).unwrap();
    let s = reg.autonomy_state(&gid);
    assert_eq!(s["tick_status"], "counting_down", "resume restores the live countdown");
    assert!(s["eligible_in_secs"].as_u64().is_some());
}
