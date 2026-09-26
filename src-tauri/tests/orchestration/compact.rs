//! The compact nudge, its hooks (PreCompact, promptsubmit) and the delivery-confirmation tiers.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ---------- compact-nudge: periodic /compact at natural lulls (#287) ----------

/// Guardrails on Claude with compact-nudge set to `minutes` (0 = off) for the
/// given `roles` (lowercase `Role::as_str()` names), other fields mirroring
/// `rails()`.
pub(crate) fn compact_rails(minutes: u32, roles: &[&str]) -> Guardrails {
    Guardrails {
        compact_nudge_minutes: minutes,
        compact_nudge_roles: roles.iter().map(|s| s.to_string()).collect(),
        // #429: explicit opt-out — this helper is for compact-nudge/idle-tick
        // coexistence tests, not the intake gate; without this, `rails()`'s
        // unset `intake_poll_minutes` smart-defaults ON the moment
        // `set_autonomous` is called and a repeated idle-tick assertion below
        // would silently start exercising suppression instead.
        intake_poll_minutes: Some(0),
        ..rails()
    }
}

/// rev-42 delta (round 2): a `delivery_confirmations` map reporting a
/// CONFIRMED delivery to `agent_id` at `submit_sent_ms` — what a real
/// `deliver_prompt` background thread would eventually record, supplied
/// directly since unit tests have no live pty/app handle to exercise it for
/// real (see `DeliveryConfirmation`'s doc). `from` is the PRE-RENAME host
/// sender on purpose (#1153 phase 3): every existing caller uses this to
/// simulate this app's OWN reinjection delivery confirming, never a
/// human/other-agent message, and a confirmation restored from a record
/// written before the flag day is exactly what `brand::is_host_actor` has to
/// keep recognising for the inference-arm cooldown to hold across an upgrade.
pub(crate) fn confirmed_delivery(agent_id: &str, submit_sent_ms: u64) -> HashMap<String, DeliveryConfirmation> {
    [(agent_id.to_string(), DeliveryConfirmation { submit_sent_ms, confirmed: true, from: "loomux".to_string() })]
        .into_iter()
        .collect()
}

/// Group with a live (Running, headless) orchestrator and compact-nudge
/// enabled for `minutes` on the default (orchestrator-only) role set. Returns
/// (reg, tempdir, group id, orchestrator id).
pub(crate) fn compact_nudge_setup(minutes: u32) -> (OrchRegistry, tempfile::TempDir, GroupId, String) {
    let (reg, dir) = test_registry();
    let g = reg.create_group("C:/tmp/repo", compact_rails(minutes, &["orchestrator"])).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    (reg, dir, g.id, o.id)
}

#[test]
fn compact_context_threshold_defaults_to_45_and_persisted_zero_stays_off() {
    let (reg, dir) = test_registry();
    let reg = std::sync::Arc::new(reg);
    let repo = tempfile::tempdir().unwrap();
    let repo_path = repo.path().to_string_lossy().into_owned();
    let request = launch_with_workflow(&reg, &repo_path, false, None)
        .expect("launch through the production create-orchestration path");
    let gid = request.group_id;
    let fresh = reg.load_group_file(&gid).expect("fresh group.json").1;
    assert_eq!(DEFAULT_COMPACT_CONTEXT_THRESHOLD_PERCENT, 45);
    assert_eq!(
        fresh.compact_context_threshold_percent,
        DEFAULT_COMPACT_CONTEXT_THRESHOLD_PERCENT,
        "new group creation uses the shared 45% default"
    );

    let path = dir.path().join(gid.as_str()).join("group.json");
    let mut persisted: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    persisted["guardrails"].as_object_mut().unwrap().remove("compact_context_threshold_percent");
    fs::write(&path, serde_json::to_vec_pretty(&persisted).unwrap()).unwrap();
    assert_eq!(reg.load_group_file(&gid).unwrap().1.compact_context_threshold_percent, 45,
        "a legacy file missing the key receives the new default");
    persisted["guardrails"]["compact_context_threshold_percent"] = serde_json::json!(0);
    fs::write(&path, serde_json::to_vec_pretty(&persisted).unwrap()).unwrap();
    let loaded = reg.load_group_file(&gid).expect("edited group.json").1;
    assert_eq!(loaded.compact_context_threshold_percent, 0, "explicit off choice survives load");
}

#[test]
fn compact_escalation_default_role_gate_only_escalates_the_orchestrator() {
    let (reg, _dir) = test_registry();
    let group = reg
        .create_group(
            "C:/tmp/repo",
            Guardrails {
                compact_context_threshold_percent: DEFAULT_COMPACT_CONTEXT_THRESHOLD_PERCENT,
                ..rails()
            },
        )
        .unwrap();
    let orchestrator = reg
        .spawn_agent(&group.id, Role::Orchestrator, "orch", "", false, None)
        .unwrap();
    let worker = reg
        .spawn_agent(&group.id, Role::Worker, "worker", "", false, None)
        .unwrap();
    let contexts = HashMap::from([(orchestrator.id.clone(), 80), (worker.id.clone(), 80)]);

    reg.compact_nudge_tick(
        FAR,
        &HashMap::new(),
        &HashMap::new(),
        &contexts,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    );

    let escalated: HashSet<String> = audit_entries(&reg, &group.id, "compact-escalation")
        .iter()
        .filter_map(|entry| entry["detail"]["agent"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        escalated,
        HashSet::from([orchestrator.id]),
        "default threshold escalation applies only to the default eligible role"
    );
}

#[test]
fn compact_nudge_role_gate_defaults_to_orchestrator_only() {
    let default_roles = vec!["orchestrator".to_string()];
    assert!(compact_nudge_role_allowed(Role::Orchestrator, &default_roles));
    assert!(!compact_nudge_role_allowed(Role::Worker, &default_roles),
        "workers are short-lived — not eligible unless explicitly configured");
    assert!(!compact_nudge_role_allowed(Role::Reviewer, &default_roles));
    assert!(!compact_nudge_role_allowed(Role::Planner, &default_roles));
    // Config-selectable: a group that opts a worker in gets exactly that.
    let widened = vec!["orchestrator".to_string(), "worker".to_string()];
    assert!(compact_nudge_role_allowed(Role::Worker, &widened));
    assert!(!compact_nudge_role_allowed(Role::Reviewer, &widened));
}

#[test]
fn compact_nudge_cli_gate_covers_claude_and_copilot() {
    // Both currently-supported CLIs have a real `/compact` command (Copilot's
    // confirmed via docs.github.com/en/copilot/reference/copilot-cli-reference/
    // cli-command-reference — an earlier round of this feature wrongly
    // asserted Copilot has none). Anything outside `SUPPORTED_CLIS` still
    // gets no nudge.
    assert!(compact_nudge_cli_supported("claude"));
    assert!(compact_nudge_cli_supported("copilot"));
    assert!(!compact_nudge_cli_supported("codex"));
    assert!(!compact_nudge_cli_supported(""));
}

#[test]
fn compact_nudge_defaults_off_and_orchestrator_only_and_zero_survives_clamping() {
    // The conservative shipped default: off, and — if ever turned on without an
    // explicit role list — orchestrator-only.
    let g = Guardrails::default().clamped();
    assert_eq!(g.compact_nudge_minutes, 0, "off by default");
    assert_eq!(g.compact_nudge_roles, vec!["orchestrator".to_string()]);
    // Unlike `idle_tick_minutes`, 0 here is a real "off", not "unset → default"
    // (there is no separate on/off marker) — clamped() must never float it up.
    let g = Guardrails { compact_nudge_minutes: 0, ..rails() }.clamped();
    assert_eq!(g.compact_nudge_minutes, 0);
    let g = Guardrails { compact_nudge_minutes: 99_999, ..rails() }.clamped();
    assert_eq!(g.compact_nudge_minutes, 1440, "clamps to the 24h ceiling");
    // Unrecognized role names are dropped and duplicates deduped; an all-bogus
    // list falls back to orchestrator-only rather than disabling every role.
    let g = Guardrails {
        compact_nudge_roles: vec!["bogus".into(), "worker".into(), "worker".into()],
        ..rails()
    }
    .clamped();
    assert_eq!(g.compact_nudge_roles, vec!["worker".to_string()]);
    let g = Guardrails { compact_nudge_roles: vec!["bogus".into()], ..rails() }.clamped();
    assert_eq!(g.compact_nudge_roles, vec!["orchestrator".to_string()]);
}

#[test]
fn compact_nudge_off_by_default_never_fires() {
    let (reg, _d, gid, _oid) = compact_nudge_setup(0);
    let empty = HashMap::new();
    assert!(reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty(),
        "compact_nudge_minutes 0 must never fire, no matter how long the pane is quiet");
    assert_eq!(audit_count(&reg, &gid, "compact-nudge"), 0);
}

#[test]
fn compact_nudge_fires_once_per_window_and_rearms_on_output() {
    let (reg, _d, gid, oid) = compact_nudge_setup(20);
    let empty = HashMap::new();
    // Idle-at-prompt (output-quiet) far past the window → exactly one nudge,
    // audited, delivered via the same idleness signal idle-tick reads.
    assert_eq!(reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()), vec![oid.clone()],
        "an eligible pane quiet past the window must be nudged");
    assert_eq!(audit_count(&reg, &gid, "compact-nudge"), 1, "the nudge must be audited once");
    // Rate limit: still quiet, already latched → no second nudge in the same
    // window (this is also the "held is skipped, not queued" property at the
    // tick layer — there is no retry loop, just the one-shot latch).
    assert!(reg.compact_nudge_tick(FAR + 60_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty(), "one nudge per quiet window");
    // The pane produces real output (mid-turn / busy): clock + latch both
    // reset, and this very tick can't also fire — "never mid-turn" pinned
    // directly against the tick's own fire decision.
    let grew: HashMap<String, u64> = [(oid.clone(), 4096u64)].into_iter().collect();
    assert!(reg.compact_nudge_tick(FAR, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty(), "output growth is activity, not idle-at-prompt");
    // Quiet again shortly after: busy-then-quiet resolves (trusted arm) into
    // the delivery-confirmation phase — NOT itself a re-fire, and `compact_
    // pending` still gates the heuristic until that delivery confirms
    // (rev-42 delta, round 2).
    assert!(reg.compact_nudge_tick(FAR + 1_000, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty(),
        "resolving into the confirmation-wait phase is not itself a fire");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1, "the first arm's reinjection was attempted");
    assert!(reg.agent(&oid).unwrap().compact_pending, "still pending — waiting on confirmed delivery");
    // Confirm that delivery: the latch releases.
    let confirmed = confirmed_delivery(&oid, FAR + 1_000);
    assert!(reg.compact_nudge_tick(FAR + 2_000, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &confirmed).is_empty());
    assert!(!reg.agent(&oid).unwrap().compact_pending, "confirmed delivery releases the latch");
    // No further growth; a whole fresh window elapses (from the busy tick's
    // last_progress_ms) → a brand-new nudge.
    assert_eq!(reg.compact_nudge_tick(FAR + 20 * 60_000 + 1, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()), vec![oid.clone()],
        "a new quiet window after activity earns a new nudge");
    assert_eq!(audit_count(&reg, &gid, "compact-nudge"), 2);
}

#[test]
fn compact_nudge_ignores_subfloor_repaint_growth() {
    // Same repaint-vs-real-turn discrimination idle-tick uses (shared
    // `idle_activity_floor_bytes` guardrail, shared `idle_output_is_activity`):
    // a statusline/spinner frame must not indefinitely defer the nudge.
    let (reg, _d, _gid, oid) = compact_nudge_setup(5);
    let m = |total: u64| -> HashMap<String, u64> { [(oid.clone(), total)].into_iter().collect() };
    assert!(reg.compact_nudge_tick(1_000, &m(500), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty(), "an early sub-floor repaint is not a tick");
    assert_eq!(reg.compact_nudge_tick(FAR, &m(900), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()), vec![oid.clone()],
        "sub-floor creep never resets the clock, so the window still elapses");
}

/// Write a hook marker file with an EXPLICIT mtime, `offset_ms` (signed)
/// after `base_ms` (both real wall-clock ms since the epoch) — never a bare
/// `fs::write` right after spawning an agent, whose own `started_ms` is
/// sampled from the SAME real clock a few statements earlier: two real-clock
/// reads landing in the same millisecond (or a filesystem mtime-write
/// buffering delay) would otherwise make the product's `ts >= a.started_ms`
/// gate an intermittent test flake instead of a deterministic pass/fail.
fn write_hook_marker(path: &Path, base_ms: u64, offset_ms: i64) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let f = fs::File::create(path).unwrap();
    let ms = (base_ms as i64 + offset_ms).max(0) as u64;
    f.set_modified(UNIX_EPOCH + Duration::from_millis(ms)).unwrap();
}

#[test]
fn compact_nudge_tick_treats_a_precompact_hook_marker_as_trusted_evidence() {
    // #417: a PreCompact hook marker (a marker FILE the generic hook script
    // writes — see `COMPACT_HOOK_SCRIPT`) is DIRECT evidence. It arms exactly
    // like the loomux-initiated path (trusted, no busy-then-quiet inference
    // gate) AND is treated as already busy — so with the pane already quiet
    // (no output growth), arm and resolve-into-reinjection happen on the
    // SAME tick, unlike a banner/manual inference arm which always needs a
    // later quiet observation.
    let (reg, _d, gid, oid) = compact_nudge_setup(20);
    let started_ms = reg.agent(&oid).unwrap().started_ms;
    let empty = HashMap::new();

    // No marker yet: an ordinary quiet tick fires nothing (the heuristic
    // window is 20 minutes away; nothing else armed it either).
    assert!(reg.compact_nudge_tick(1_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    assert!(!reg.agent(&oid).unwrap().compact_pending);

    // Simulate the hook firing: write the marker the script would have.
    let marker = reg.state_root().join(gid.as_str()).join("hooks").join(format!("{oid}.precompact.json"));
    write_hook_marker(&marker, started_ms, 1_000);

    // The marker arms AND resolves in this one tick (loomux pasted nothing
    // itself, so this is not a "nudge"; the reinjection notice is a separate
    // `deliver_prompt`, not something `compact_nudge_tick`'s return value
    // reports).
    assert!(reg.compact_nudge_tick(2_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    assert_eq!(audit_count(&reg, &gid, "compact-hook-evidence"), 1, "the arm itself is audited, distinctly from the reinjection");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1);
    let a = reg.agent(&oid).unwrap();
    assert!(a.compact_pending, "still pending — waiting on confirmed delivery");
    assert!(a.compact_reinject_attempted_ms.is_some(), "moved into the delivery-confirmation phase");
    assert_eq!(a.compact_pending_evidence, Some("hook"), "the evidence tag survives into that phase");

    // Confirm delivery: the arm resolves and the evidence tag clears.
    let confirmed = confirmed_delivery(&oid, 2_000);
    assert!(reg.compact_nudge_tick(3_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &confirmed).is_empty());
    let a = reg.agent(&oid).unwrap();
    assert!(!a.compact_pending, "confirmed delivery releases the latch");
    assert_eq!(a.compact_pending_evidence, None, "cleared on resolution, like the trust flag it rides alongside");

    // The SAME marker (unchanged mtime) must never re-arm.
    assert!(reg.compact_nudge_tick(4_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    assert!(!reg.agent(&oid).unwrap().compact_pending, "a stale, already-consumed marker must not re-arm");
}

#[test]
fn compact_nudge_tick_treats_a_sessionstart_hook_marker_as_an_immediate_confirm() {
    // #417: the SessionStart(compact) marker is even STRONGER direct proof —
    // Claude Code itself restarted the session specifically because of a
    // compact — so it arms AND confirms in one step, unconditionally (unlike
    // the PreCompact marker, which only arms while nothing is already
    // pending). This covers a hook config with only SessionStart wired, or a
    // PreCompact marker a tick loop raced past.
    //
    // rev-4 review (N3): the SessionStart hook is also the ONE script branch
    // that emits native `additionalContext` (see `COMPACT_HOOK_SCRIPT`) — so
    // Claude Code has ALREADY re-grounded the agent by the time this marker
    // is observed. loomux's OWN reinjection is therefore skipped entirely
    // here (a double re-grounding would spend exactly the tokens native
    // delivery exists to save) — this resolves straight to `compact_pending
    // = false`, not into the delivery-confirmation phase.
    let (reg, _d, gid, oid) = compact_nudge_setup(20);
    let started_ms = reg.agent(&oid).unwrap().started_ms;
    let empty = HashMap::new();

    let marker = reg.state_root().join(gid.as_str()).join("hooks").join(format!("{oid}.sessionstart-compact.json"));
    write_hook_marker(&marker, started_ms, 1_000);

    assert!(reg.compact_nudge_tick(1_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    let a = reg.agent(&oid).unwrap();
    assert!(!a.compact_pending, "native re-grounding already landed — nothing left pending");
    assert!(a.compact_reinject_attempted_ms.is_none(), "no loomux-side delivery to confirm");
    assert_eq!(a.compact_pending_evidence, None, "cleared on this terminal resolution");
    assert_eq!(audit_count(&reg, &gid, "compact-hook-evidence"), 1, "the arm is still audited");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 0, "no duplicate re-grounding is pasted");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-skipped-native"), 1);
}

#[test]
fn compact_nudge_tick_sessionstart_evidence_resolves_even_while_the_pane_is_busy() {
    // Round 7, the live-demo incident reproduced directly: both hooks fired
    // for real, N3 suppression correctly held (no reinjection), but the
    // lifecycle badge then showed "compact timed out (no evidence)" anyway.
    // Root cause — the SessionStart resolution used to be nested inside
    // `else if currently_quiet`, so it only took effect on a tick where the
    // pane happened to show NO growth. A fast compact whose agent keeps
    // working continuously afterward (this test's shape: real, large output
    // growth on the SAME tick the marker is consumed) never satisfies that,
    // and `compact-arm-timeout` fires instead at precisely `ARM_PENDING_
    // TIMEOUT_MS` after the precompact arm — exactly the incident's audit
    // timeline. SessionStart evidence must resolve unconditionally: it is
    // proof the compaction already finished AND that native re-grounding
    // already reached the agent, so there is nothing left for a quiet
    // observation to confirm.
    let (reg, _d, gid, oid) = compact_nudge_setup(20);
    let started_ms = reg.agent(&oid).unwrap().started_ms;

    let marker = reg.state_root().join(gid.as_str()).join("hooks").join(format!("{oid}.sessionstart-compact.json"));
    write_hook_marker(&marker, started_ms, 1_000);

    // The pane is BUSY on this exact tick — real growth well past any
    // activity floor, i.e. `currently_quiet` is false.
    let busy: HashMap<String, u64> = [(oid.clone(), 500_000u64)].into_iter().collect();
    assert!(reg.compact_nudge_tick(1_000, &busy, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    let a = reg.agent(&oid).unwrap();
    assert!(!a.compact_pending, "resolves regardless of busy/quiet — there is no confirmation left to wait on");
    assert_eq!(a.compact_pending_evidence, None);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-skipped-native"), 1);
    assert_eq!(audit_count(&reg, &gid, "compact-arm-timeout"), 0);

    // The agent then keeps being busy for well past ARM_PENDING_TIMEOUT_MS
    // (5 minutes) — since the arm already resolved, this must never trip
    // the timeout the incident's audit log showed firing.
    let still_busy: HashMap<String, u64> = [(oid.clone(), 2_000_000u64)].into_iter().collect();
    assert!(reg.compact_nudge_tick(1_000 + 6 * 60_000, &still_busy, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    assert_eq!(audit_count(&reg, &gid, "compact-arm-timeout"), 0, "already resolved — nothing left to time out");
}

#[test]
fn compact_nudge_tick_fast_compact_both_hook_events_in_one_poll_gap_resolves_not_abandoned() {
    // Round 7: the OTHER real-world shape from the same incident — a fast
    // compaction where both PreCompact and SessionStart markers already
    // exist by the time loomux's tick loop gets around to polling (a slow
    // poll cadence, or a compaction fast enough to finish between two
    // ticks). Both markers land in the SAME `compact_nudge_tick` call: the
    // PreCompact block arms, then the SessionStart block — running right
    // after it, same iteration — resolves immediately. Must never leave
    // `compact_pending` open long enough to reach `ARM_PENDING_TIMEOUT_MS`.
    let (reg, _d, gid, oid) = compact_nudge_setup(20);
    let started_ms = reg.agent(&oid).unwrap().started_ms;

    let precompact_marker = reg.state_root().join(gid.as_str()).join("hooks").join(format!("{oid}.precompact.json"));
    let sessionstart_marker = reg.state_root().join(gid.as_str()).join("hooks").join(format!("{oid}.sessionstart-compact.json"));
    write_hook_marker(&precompact_marker, started_ms, 1_000);
    write_hook_marker(&sessionstart_marker, started_ms, 1_000);

    let empty = HashMap::new();
    assert!(reg.compact_nudge_tick(1_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    let a = reg.agent(&oid).unwrap();
    assert!(!a.compact_pending, "both events land, then resolve, in the SAME tick");
    assert_eq!(audit_count(&reg, &gid, "compact-hook-evidence"), 2, "both arm events are still individually audited");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-skipped-native"), 1, "exactly one terminal resolution, not one per event");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 0);

    // Ticking well past ARM_PENDING_TIMEOUT_MS must never abandon something
    // that already resolved.
    assert!(reg.compact_nudge_tick(1_000 + 6 * 60_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    assert_eq!(audit_count(&reg, &gid, "compact-arm-timeout"), 0);
}

#[test]
fn compact_nudge_tick_sessionstart_evidence_after_a_reinject_was_already_decided_clears_the_confirmation_phase() {
    // rev-10 review (B1, blocking), round 7: the SessionStart terminal path
    // runs BEFORE the delivery-confirmation block, every tick — reachable
    // ordering matching the incident's own ~2-minute gap: a PreCompact arm
    // resolves into a DECIDED loomux reinjection (busy-then-quiet, trusted)
    // before any SessionStart marker exists, then the SessionStart marker
    // lands on a LATER tick. If the SessionStart block only cleared the arm
    // fields (not `compact_reinject_attempted_ms`/`attempts`), the
    // confirmation phase stayed live against a `compact_pending` it had
    // already reset to `false` — retrying, double-confirming, or falsely
    // abandoning a compaction that in fact succeeded via native evidence.
    let (reg, _d, gid, oid) = compact_nudge_setup(20);
    let started_ms = reg.agent(&oid).unwrap().started_ms;
    let empty = HashMap::new();

    // Arm via PreCompact, quiet immediately — resolves into a DECIDED
    // loomux reinjection on this same tick (same shape as `compact_nudge_
    // tick_a_precompact_only_arm_still_gets_loomuxs_own_reinjection`).
    let precompact_marker = reg.state_root().join(gid.as_str()).join("hooks").join(format!("{oid}.precompact.json"));
    write_hook_marker(&precompact_marker, started_ms, 1_000);
    assert!(reg.compact_nudge_tick(1_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    let a = reg.agent(&oid).unwrap();
    assert!(a.compact_pending, "still pending — waiting on the reinjection's own delivery to confirm");
    assert!(a.compact_reinject_attempted_ms.is_some(), "a loomux reinjection was already DECIDED");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1);

    // SessionStart evidence lands later — the delivery-confirmation phase
    // is still live (no `confirmed` delivery map passed) when it does.
    let sessionstart_marker = reg.state_root().join(gid.as_str()).join("hooks").join(format!("{oid}.sessionstart-compact.json"));
    write_hook_marker(&sessionstart_marker, started_ms, 120_000);
    assert!(reg.compact_nudge_tick(121_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    let a = reg.agent(&oid).unwrap();
    assert!(!a.compact_pending, "the sessionstart terminal path resolves it");
    assert!(a.compact_reinject_attempted_ms.is_none(), "B1: the confirmation-phase bookkeeping must clear too");
    assert_eq!(a.compact_reinject_attempts, 0);
    // The reinjection prompt already went out (attempt #1, above) — that
    // was not "skipped", so it must not be double-counted as a fresh
    // native-skip.
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-skipped-native"), 0);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1, "no retry");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-confirmed"), 0, "no stale double-confirm");
    // #546 split the terminal resolution across two actions; asserting only
    // the `-confirmed` one would now pass by omission if a resolution had
    // been written under the other name.
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-liveness-only"), 0, "nor under the liveness action");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-abandoned"), 0);

    // A late-arriving confirmation for the ORIGINAL (now-cleared) attempt
    // must not resurrect the confirmation phase.
    let late_confirm = confirmed_delivery(&oid, 1_000);
    assert!(reg.compact_nudge_tick(122_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &late_confirm).is_empty());
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-confirmed"), 0, "nothing left to confirm");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-liveness-only"), 0, "and nothing to close on liveness either");

    // Ticking well past ARM_PENDING_TIMEOUT_MS must never falsely abandon a
    // compaction that already resolved via native evidence.
    assert!(reg
        .compact_nudge_tick(121_000 + 6 * 60_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new())
        .is_empty());
    assert_eq!(audit_count(&reg, &gid, "compact-arm-timeout"), 0);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-abandoned"), 0);

    // A genuinely NEW compaction can still re-arm cleanly — the state
    // machine is not wedged.
    write_hook_marker(&precompact_marker, started_ms, 121_000 + 6 * 60_000 + 1_000);
    assert!(reg
        .compact_nudge_tick(121_000 + 6 * 60_000 + 2_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new())
        .is_empty());
    assert!(reg.agent(&oid).unwrap().compact_pending, "a fresh marker can re-arm a new cycle");
}

#[test]
fn compact_nudge_tick_a_precompact_only_arm_still_gets_loomuxs_own_reinjection() {
    // rev-4 review (N3): the fallback half of the same fix. A PreCompact
    // marker with NO SessionStart marker (a hook config that only wires the
    // one event, or a SessionStart marker this tick loop simply hasn't seen
    // yet) never delivered native `additionalContext` — the script's
    // `precompact` branch only ever writes a marker, nothing else — so
    // loomux's own reinjection must still fire. Never silently drop the ONLY
    // re-grounding channel just because SOME hook evidence exists.
    let (reg, _d, gid, oid) = compact_nudge_setup(20);
    let started_ms = reg.agent(&oid).unwrap().started_ms;
    let empty = HashMap::new();

    let marker = reg.state_root().join(gid.as_str()).join("hooks").join(format!("{oid}.precompact.json"));
    write_hook_marker(&marker, started_ms, 1_000);

    assert!(reg.compact_nudge_tick(1_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    let a = reg.agent(&oid).unwrap();
    assert!(a.compact_pending, "still pending — waiting on the reinjection's own delivery to confirm");
    assert!(a.compact_reinject_attempted_ms.is_some());
    assert_eq!(a.compact_pending_evidence, Some("hook"));
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1, "the ONLY re-grounding channel available must still fire");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-skipped-native"), 0);
}

#[test]
fn compact_nudge_tick_ignores_a_hook_marker_older_than_the_agent_itself() {
    // rev-4 review (B1, blocking): the exact failure sequence a live restart
    // can hit. `compact_hook_*_seen_ms` is in-memory (resets to `None` on a
    // fresh `AgentEntry`), and agent ids come from an in-memory counter too
    // — so a fresh boot can mint an id a PREVIOUS process already used,
    // while the group's `hooks/` marker files (on disk) survive the
    // restart untouched. Without the `ts > a.started_ms` gate, the very
    // first tick after such a restart would read the OLD process's marker
    // as "fresh evidence" (`ts > None.unwrap_or(0)` is true for any real
    // mtime) and arm TRUSTED with no compaction having happened at all.
    // Uses the PreCompact marker specifically (not SessionStart): #417's N3
    // fix resolves a fresh SessionStart marker straight to a terminal state
    // in the SAME tick (native re-grounding already delivered — see
    // `compact_nudge_tick_treats_a_sessionstart_hook_marker_as_an_immediate_
    // confirm`), which would make this test's own "still arms normally"
    // half read the wrong signal. PreCompact's arm-then-wait-for-delivery
    // shape keeps this test's B1 story (the `started_ms` gate itself)
    // decoupled from N3's separate concern.
    let (reg, _d, gid, oid) = compact_nudge_setup(20);
    let started_ms = reg.agent(&oid).unwrap().started_ms;

    let marker = reg.state_root().join(gid.as_str()).join("hooks").join(format!("{oid}.precompact.json"));
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    let f = fs::File::create(&marker).unwrap();
    // Backdate the marker to well before this agent's own `started_ms` —
    // exactly the shape of one surviving from a PREVIOUS process.
    f.set_modified(UNIX_EPOCH + Duration::from_millis(started_ms.saturating_sub(60_000))).unwrap();
    drop(f);

    let empty = HashMap::new();
    assert!(reg.compact_nudge_tick(1_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    let a = reg.agent(&oid).unwrap();
    assert!(!a.compact_pending, "a marker older than the agent's own started_ms must never arm");
    assert_eq!(a.compact_pending_evidence, None);
    assert_eq!(audit_count(&reg, &gid, "compact-hook-evidence"), 0);
    // Never treated as evidence at all, so never consumed either.
    assert!(marker.exists(), "a rejected stale marker is left alone, not silently deleted");

    // A GENUINELY fresh marker (mtime after started_ms) from the SAME agent
    // still arms normally — the fix excludes only markers that PREDATE it.
    // Mtime set explicitly (not just "written just now") so this assertion
    // can never flake on two real-clock reads landing in the same
    // millisecond as `started_ms` itself.
    let f = fs::File::create(&marker).unwrap();
    f.set_modified(UNIX_EPOCH + Duration::from_millis(started_ms + 1_000)).unwrap();
    drop(f);
    assert!(reg.compact_nudge_tick(2_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    let a = reg.agent(&oid).unwrap();
    assert!(a.compact_pending, "a marker written during this agent's own lifetime still arms");
    assert_eq!(a.compact_pending_evidence, Some("hook"));
    assert!(!marker.exists(), "delete-on-consume: an ACTUALLY-used marker is removed from disk");
}

// ─────────────── #417 correction round: Copilot's own PreCompact hook ───────────────
//
// An earlier round of this feature wrongly asserted Copilot has no compaction hooks at
// all upstream. It does: `preCompact` (docs.github.com/en/copilot/reference/hooks-
// reference), with a payload nearly identical to Claude's. What it does NOT have is any
// `sessionStart` source value indicating a post-compaction resume (`source` is exactly
// `"startup" | "resume" | "new"` — confirmed by the docs, not ambiguous) or a
// `postCompact` event of any kind — so Copilot gets the TRUSTED ARM half of #417
// (`compact_pending_trusted = true`, same as Claude's own `precompact`-only case) but
// never the native-`additionalContext` / N3-suppression half, which stays Claude-only.
// The marker-consumption path itself (the B1 fix: `ts >= a.started_ms`, delete-on-
// consume) is CLI-agnostic — it reads `<group_dir>/hooks/<agent_id>.precompact.json`
// with no idea which CLI wrote it, so the tests below reuse it unchanged; only the
// SETUP (a copilot-CLI group) and the one new CLI-admission gate differ from the
// Claude tests above.

fn compact_copilot_rails(minutes: u32, roles: &[&str]) -> Guardrails {
    Guardrails {
        compact_nudge_minutes: minutes,
        compact_nudge_roles: roles.iter().map(|s| s.to_string()).collect(),
        ..copilot_rails()
    }
}

fn compact_nudge_setup_copilot(minutes: u32) -> (OrchRegistry, tempfile::TempDir, GroupId, String) {
    let (reg, dir) = test_registry();
    let g = reg.create_group("C:/tmp/copilot-repo", compact_copilot_rails(minutes, &["orchestrator"])).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    (reg, dir, g.id, o.id)
}

#[test]
fn copilot_precompact_hook_marker_is_trusted_evidence_too() {
    // Same evidence class as Claude's `precompact` case (`compact_pending_trusted =
    // true`) — a hook telling loomux a compaction is starting is equally trustworthy
    // regardless of which CLI's hook fired it. Before #417's CLI-gate widening
    // (`compact_nudge_cli_supported`), a copilot agent never even reached this code —
    // the per-agent loop's admission gate used to be claude-only, so this marker
    // would have been silently ignored forever.
    let (reg, _d, gid, oid) = compact_nudge_setup_copilot(20);
    let started_ms = reg.agent(&oid).unwrap().started_ms;
    let empty = HashMap::new();

    assert!(reg.compact_nudge_tick(1_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    assert!(!reg.agent(&oid).unwrap().compact_pending);

    let marker = reg.state_root().join(gid.as_str()).join("hooks").join(format!("{oid}.precompact.json"));
    write_hook_marker(&marker, started_ms, 1_000);

    // Arms AND resolves into the delivery-confirmation phase on this one tick — a
    // Copilot precompact arm has no native re-grounding to make redundant (unlike a
    // Claude SessionStart-confirmed one), so loomux's OWN reinjection is the only
    // re-grounding channel here, same as Claude's precompact-only case.
    assert!(reg.compact_nudge_tick(2_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    assert_eq!(audit_count(&reg, &gid, "compact-hook-evidence"), 1);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-skipped-native"), 0);
    let a = reg.agent(&oid).unwrap();
    assert!(a.compact_pending, "still pending — waiting on confirmed delivery");
    assert!(a.compact_reinject_attempted_ms.is_some());
    assert_eq!(a.compact_pending_evidence, Some("hook"));

    let confirmed = confirmed_delivery(&oid, 2_000);
    assert!(reg.compact_nudge_tick(3_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &confirmed).is_empty());
    assert!(!reg.agent(&oid).unwrap().compact_pending);
}

#[test]
fn copilot_precompact_marker_gets_the_same_b1_restart_protection() {
    // The exact B1 regression, replayed for a Copilot marker — "one mechanism, two
    // writers" only actually holds if the SAME gate protects both.
    let (reg, _d, gid, oid) = compact_nudge_setup_copilot(20);
    let started_ms = reg.agent(&oid).unwrap().started_ms;

    let marker = reg.state_root().join(gid.as_str()).join("hooks").join(format!("{oid}.precompact.json"));
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    let f = fs::File::create(&marker).unwrap();
    f.set_modified(UNIX_EPOCH + Duration::from_millis(started_ms.saturating_sub(60_000))).unwrap();
    drop(f);

    let empty = HashMap::new();
    assert!(reg.compact_nudge_tick(1_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    assert!(!reg.agent(&oid).unwrap().compact_pending, "a marker older than started_ms must never arm, Copilot included");
    assert_eq!(audit_count(&reg, &gid, "compact-hook-evidence"), 0);
    assert!(marker.exists());
}

#[test]
fn copilot_compaction_marker_resolves_the_arm_even_while_the_pane_stays_busy() {
    // #428 (round 9): the exact live-incident shape, reproduced. Marker
    // text is quoted directly from the issue's own report of the user's
    // live observation (its body and comment), not reconstructed from
    // memory. Continuously busy every tick (never quiet) is the fast-path
    // proof: this must be a TERMINAL path resolved at marker-consume, not
    // merely a faster busy-then-quiet that happened to get lucky.
    let (reg, _d, gid, oid) = compact_nudge_setup_copilot(20);
    let started_ms = reg.agent(&oid).unwrap().started_ms;
    let empty_tail: HashMap<String, (String, u64)> = HashMap::new();

    let marker = reg.state_root().join(gid.as_str()).join("hooks").join(format!("{oid}.precompact.json"));
    write_hook_marker(&marker, started_ms, 1_000);

    // Arm: precompact evidence, pane already busy — matches the issue's
    // own "+0s" timeline.
    let busy: HashMap<String, u64> = [(oid.clone(), 500_000u64)].into_iter().collect();
    assert!(reg.compact_nudge_tick(1_000, &busy, &empty_tail, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    let a = reg.agent(&oid).unwrap();
    assert!(a.compact_pending, "armed, waiting on resolution");
    assert!(a.compact_reinject_attempted_ms.is_none(), "not yet decided — no marker seen yet");

    // The marker appears now — resolved on THIS tick, with the pane STILL
    // busy (never went quiet in between).
    let tail_with_marker: HashMap<String, (String, u64)> = [(
        oid.clone(),
        ("A new checkpoint has been added to your session.".to_string(), 2_000),
    )]
    .into_iter()
    .collect();
    let still_busy: HashMap<String, u64> = [(oid.clone(), 900_000u64)].into_iter().collect();
    assert!(reg.compact_nudge_tick(2_000, &still_busy, &tail_with_marker, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    let a = reg.agent(&oid).unwrap();
    assert!(a.compact_pending, "still pending — waiting on the reinjection's own delivery to confirm");
    assert!(
        a.compact_reinject_attempted_ms.is_some(),
        "the marker decided a reinjection immediately — no quiet tick needed"
    );
    assert_eq!(a.compact_reinject_attempts, 1);
    assert_eq!(audit_count(&reg, &gid, "compact-resolved-copilot-marker"), 1);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1);
    assert_eq!(audit_count(&reg, &gid, "compact-arm-timeout"), 0);
    // rev-21 review: this path must reset the SAME fields the sibling
    // busy-then-quiet "confirmed" branch resets, no more — `compact_
    // pending_evidence` (the "hook" chip label) stays put through the
    // confirmation phase either way, so which path resolved the arm never
    // changes what the badge shows afterward.
    assert_eq!(a.compact_pending_evidence, Some("hook"), "the hook chip label must survive into the confirmation phase");

    // Ticking well past ARM_PENDING_TIMEOUT_MS (5 min), still no marker,
    // still busy — must never time out something already resolved. Also
    // proves the delivery-confirmation phase (not this new block) is what
    // now owns resolution — exactly like every other terminal path.
    let confirmed = confirmed_delivery(&oid, 2_000);
    assert!(reg
        .compact_nudge_tick(2_000 + 6 * 60_000, &still_busy, &empty_tail, &HashMap::new(), &HashMap::new(), &HashMap::new(), &confirmed)
        .is_empty());
    assert_eq!(audit_count(&reg, &gid, "compact-arm-timeout"), 0);
    assert!(!reg.agent(&oid).unwrap().compact_pending, "confirmed delivery resolves it cleanly");
}

#[test]
fn copilot_compaction_marker_with_no_arm_is_a_no_op() {
    // #428, the B1-shaped safety check: a marker sighting alone must never
    // conjure an arm out of nothing. The whole block is gated on `a.
    // compact_pending`, which nothing here can set `true` from `false` —
    // a stale mention from a long-past, already-resolved compaction
    // sitting in scrollback must never resurrect anything.
    let (reg, _d, gid, oid) = compact_nudge_setup_copilot(20);
    let tail_with_marker: HashMap<String, (String, u64)> =
        [(oid.clone(), ("Compaction completed".to_string(), 1_000))].into_iter().collect();
    let empty: HashMap<String, u64> = HashMap::new();
    assert!(reg.compact_nudge_tick(1_000, &empty, &tail_with_marker, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    let a = reg.agent(&oid).unwrap();
    assert!(!a.compact_pending, "no arm existed — the marker alone must never create one");
    assert_eq!(audit_count(&reg, &gid, "compact-resolved-copilot-marker"), 0);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 0);
}

#[test]
fn copilot_compaction_marker_while_a_reinjection_is_already_in_flight_is_a_no_op() {
    // rev-10's B1 lesson, applied to the new terminal path: once busy-
    // then-quiet already decided a reinjection (`compact_reinject_
    // attempted_ms` is `Some`), a LATER marker sighting for the SAME cycle
    // must never re-decide or reset it — no double dispatch, no lost
    // attempt count, no fresh audit line for something that was already
    // resolved.
    let (reg, _d, gid, oid) = compact_nudge_setup_copilot(20);
    let started_ms = reg.agent(&oid).unwrap().started_ms;
    let empty_outputs: HashMap<String, u64> = HashMap::new();
    let empty_tail: HashMap<String, (String, u64)> = HashMap::new();

    let marker = reg.state_root().join(gid.as_str()).join("hooks").join(format!("{oid}.precompact.json"));
    write_hook_marker(&marker, started_ms, 1_000);

    // Arm + quiet resolves into the confirmation phase via ordinary
    // busy-then-quiet, same shape as `copilot_precompact_hook_marker_is_
    // trusted_evidence_too`.
    assert!(reg.compact_nudge_tick(1_000, &empty_outputs, &empty_tail, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    assert!(reg.compact_nudge_tick(2_000, &empty_outputs, &empty_tail, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    let a = reg.agent(&oid).unwrap();
    assert!(a.compact_reinject_attempted_ms.is_some(), "already decided via busy-then-quiet");
    assert_eq!(a.compact_reinject_attempts, 1);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1);

    // The marker appears NOW, for the same still-open cycle, delivery not
    // yet confirmed.
    let tail_with_marker: HashMap<String, (String, u64)> =
        [(oid.clone(), ("Compaction completed".to_string(), 3_000))].into_iter().collect();
    assert!(reg.compact_nudge_tick(3_000, &empty_outputs, &tail_with_marker, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    let a = reg.agent(&oid).unwrap();
    assert_eq!(a.compact_reinject_attempts, 1, "must not double-dispatch");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1, "no second reinjection");
    assert_eq!(
        audit_count(&reg, &gid, "compact-resolved-copilot-marker"), 0,
        "already in flight — this is a no-op, not a fresh resolution"
    );
}

#[test]
fn copilot_busy_then_quiet_still_resolves_when_the_marker_never_appears() {
    // #428 regression pin: the marker is an ACCELERATOR, not a
    // replacement — with no marker ever painted (a future Copilot build
    // changed the wording, or this tick's read simply missed it), the
    // pre-existing busy-then-quiet resolution must keep working exactly
    // as it did before this round.
    let (reg, _d, gid, oid) = compact_nudge_setup_copilot(20);
    let started_ms = reg.agent(&oid).unwrap().started_ms;
    let empty_tail: HashMap<String, (String, u64)> = HashMap::new();

    let marker = reg.state_root().join(gid.as_str()).join("hooks").join(format!("{oid}.precompact.json"));
    write_hook_marker(&marker, started_ms, 1_000);

    let busy: HashMap<String, u64> = [(oid.clone(), 500_000u64)].into_iter().collect();
    assert!(reg.compact_nudge_tick(1_000, &busy, &empty_tail, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    assert!(reg.agent(&oid).unwrap().compact_reinject_attempted_ms.is_none(), "not yet — no marker, no quiet tick yet");

    // The pane goes quiet (same total as the last baseline = no growth) —
    // busy-then-quiet resolves it, unchanged from before this round.
    let quiet: HashMap<String, u64> = [(oid.clone(), 500_000u64)].into_iter().collect();
    assert!(reg.compact_nudge_tick(2_000, &quiet, &empty_tail, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    let a = reg.agent(&oid).unwrap();
    assert!(a.compact_reinject_attempted_ms.is_some(), "busy-then-quiet still resolves with no marker present");
    assert_eq!(audit_count(&reg, &gid, "compact-resolved-copilot-marker"), 0);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1);
}

#[test]
fn compact_nudge_tick_does_paste_slash_compact_into_a_copilot_pane() {
    // #417 correction round 2: an earlier round of this feature asserted
    // Copilot has no `/compact` command at all and pinned a NEGATIVE test
    // here proving loomux must never paste it into a Copilot pane. That
    // claim was wrong — GitHub's own CLI command reference documents
    // `/compact [FOCUS-INSTRUCTIONS]` as a real Copilot CLI command,
    // identical in spirit to Claude's — so this test now inverts to prove
    // the OPPOSITE: a copilot agent quiet past the heuristic window DOES get
    // nudged with a real `/compact` paste, exactly like a Claude agent would.
    let (reg, _d, _gid, oid) = compact_nudge_setup_copilot(1);
    let empty = HashMap::new();
    assert_eq!(
        reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()),
        vec![oid.clone()],
        "a copilot agent quiet past the heuristic window gets the same /compact paste a Claude agent would"
    );
    assert!(reg.agent(&oid).unwrap().compact_pending);
}

#[test]
fn compact_nudge_tick_copilot_full_loop_paste_then_hook_confirms_then_reinjects() {
    // #417 correction round 2, the FULL loop this correction completes for
    // Copilot, in its two independent halves — exactly mirroring how the
    // pre-existing Claude tests cover the same two halves separately
    // (`compact_nudge_tick_reinjects_after_a_loomux_initiated_fire_too` and
    // `compact_nudge_tick_treats_a_precompact_hook_marker_as_trusted_
    // evidence`), now proven back-to-back for the SAME copilot agent:
    //
    // Half 1 — loomux-initiated: the heuristic fire pastes `/compact` into
    // the copilot pane (now possible — see the test above); this arm is
    // trusted by PROVENANCE alone (loomux has positive knowledge the
    // command was submitted — the rev-42 delta), so it resolves via
    // busy-then-quiet, with no hook marker needed at all.
    //
    // Half 2 — hook-initiated: independently of anything loomux pasted,
    // Copilot's own `preCompact` hook can fire for a real compaction (an
    // autonomous auto-compact, or a human-typed `/compact` loomux's manual
    // detector missed) and the marker is TRUSTED evidence on its own,
    // arming AND resolving into the delivery-confirmation phase in one
    // tick — loomux's own reinjection is the only re-grounding channel
    // (Copilot has no native additionalContext path for compaction, unlike
    // Claude's SessionStart branch).
    let (reg, _d, gid, oid) = compact_nudge_setup_copilot(5);
    let empty = HashMap::new();

    // Half 1: heuristic fire pastes /compact, then resolves via busy-then-quiet.
    assert_eq!(
        reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()),
        vec![oid.clone()],
        "the heuristic fire pastes /compact itself"
    );
    assert!(reg.agent(&oid).unwrap().compact_pending);
    assert_eq!(audit_count(&reg, &gid, "compact-nudge"), 1);
    let baseline: HashMap<String, u64> = [(oid.clone(), 100_000u64)].into_iter().collect();
    let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64)].into_iter().collect();
    let dropped: HashMap<String, u64> = [(oid.clone(), 5_000u64)].into_iter().collect();
    let _ = reg.compact_nudge_tick(FAR + 1_000, &grew, &HashMap::new(), &HashMap::new(), &baseline, &HashMap::new(), &HashMap::new());
    let _ = reg.compact_nudge_tick(FAR + 2_000, &grew, &HashMap::new(), &HashMap::new(), &dropped, &HashMap::new(), &HashMap::new());
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1);
    let confirmed = confirmed_delivery(&oid, FAR + 2_000);
    let _ = reg.compact_nudge_tick(FAR + 3_000, &grew, &HashMap::new(), &HashMap::new(), &dropped, &HashMap::new(), &confirmed);
    assert!(!reg.agent(&oid).unwrap().compact_pending, "half 1 resolved cleanly before half 2 starts");

    // Half 2: Copilot's own preCompact hook fires independently and writes
    // the marker `ensure_copilot_compact_hook`'s script provisions — the
    // SAME marker-consumption path Claude's hook uses, CLI-agnostic by design.
    let started_ms = reg.agent(&oid).unwrap().started_ms;
    let marker = reg.state_root().join(gid.as_str()).join("hooks").join(format!("{oid}.precompact.json"));
    // The marker-arm gate only ever compares `ts` against `started_ms` and
    // the last-seen marker ts, never against `now` — so the mtime stays a
    // small, real offset from `started_ms` (matching every other marker
    // test) even while `now` keeps advancing on the FAR-scale clock half 1
    // already established.
    write_hook_marker(&marker, started_ms, 1_000);

    assert!(reg.compact_nudge_tick(FAR + 4_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    assert_eq!(audit_count(&reg, &gid, "compact-hook-evidence"), 1);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 2, "the second, hook-confirmed cycle reinjects too");
    let a = reg.agent(&oid).unwrap();
    assert!(a.compact_pending, "still pending — waiting on confirmed delivery");
    assert_eq!(a.compact_pending_evidence, Some("hook"));

    let confirmed2 = confirmed_delivery(&oid, FAR + 4_000);
    assert!(reg.compact_nudge_tick(FAR + 5_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &confirmed2).is_empty());
    assert!(!reg.agent(&oid).unwrap().compact_pending, "the full detection/recovery loop resolves cleanly, both halves");
}

#[test]
fn ensure_copilot_compact_hook_writes_an_additive_generic_precompact_entry() {
    // Copilot's own docs confirm multiple hook-config sources are MERGED — "When
    // the same event appears in multiple sources, all hook entries from all
    // sources are run" — so writing this file is proven safe against a user's own
    // hooks, unlike Claude's `--settings` (whose merge semantics this repo could
    // not verify empirically). Written to the SAME user-level directory Copilot
    // itself auto-loads (no CLI flag needed), never the repo's `.github/hooks/`.
    let (reg, dir) = test_registry();
    let g = reg.create_group("C:/tmp/copilot-repo", copilot_rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();

    let cfg_path = dir.path().join("copilot-hooks").join("loomux-compact.json");
    let cfg: Value = serde_json::from_str(&fs::read_to_string(&cfg_path).unwrap()).unwrap();
    assert_eq!(cfg["version"], json!(1));
    let entry = &cfg["hooks"]["preCompact"][0];
    assert_eq!(entry["type"], json!("command"));
    // Both variants always provided — Copilot itself picks the one for its host
    // OS; loomux never resolves its own interpreter path for Copilot (unlike
    // Claude's hook command, which bakes in an absolute `sh.exe`).
    let bash = entry["bash"].as_str().unwrap();
    let ps1 = entry["powershell"].as_str().unwrap();
    assert!(bash.contains("LOOMUX_GROUP_DIR") && bash.contains("LOOMUX_AGENT_ID"), "{bash}");
    assert!(ps1.contains("LOOMUX_GROUP_DIR") && ps1.contains("LOOMUX_AGENT_ID"), "{ps1}");
    assert!(bash.contains("precompact.json") && ps1.contains("precompact.json"));

    // No payload parsing happens on loomux's side at all — the script only ever
    // reads its OWN inherited environment (never Copilot's camelCase/snake_case
    // JSON piped to stdin), so there is no casing distinction for loomux's code
    // to get wrong. That's a deliberate simplification: the marker's mere
    // existence is the whole signal (`read_hook_marker_ts` reads the FILE's own
    // mtime), so nothing here needs to understand the event's payload shape.

    // A Claude-CLI group must never get this file at all.
    let (reg2, dir2) = test_registry();
    let g2 = reg2.create_group("C:/tmp/claude-repo", rails()).unwrap();
    reg2.spawn_agent(&g2.id, Role::Worker, "w", "t", false, None).unwrap();
    assert!(!dir2.path().join("copilot-hooks").join("loomux-compact.json").exists());
}

#[test]
fn ensure_copilot_compact_hook_also_writes_the_promptsubmit_entry() {
    // #112: the real prompt-landed signal rides the SAME global file
    // alongside `preCompact` (still one small additive file — Copilot's own
    // "all hook entries from all sources are run" guarantee covers both
    // events identically).
    let (reg, dir) = test_registry();
    let g = reg.create_group("C:/tmp/copilot-repo", copilot_rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();

    let cfg_path = dir.path().join("copilot-hooks").join("loomux-compact.json");
    let cfg: Value = serde_json::from_str(&fs::read_to_string(&cfg_path).unwrap()).unwrap();
    // preCompact must be completely untouched by this addition.
    assert!(cfg["hooks"]["preCompact"][0]["bash"].as_str().unwrap().contains("precompact.json"));

    let entry = &cfg["hooks"]["userPromptSubmitted"][0];
    assert_eq!(entry["type"], json!("command"));
    let bash = entry["bash"].as_str().unwrap();
    let ps1 = entry["powershell"].as_str().unwrap();
    assert!(bash.contains("LOOMUX_GROUP_DIR") && bash.contains("LOOMUX_AGENT_ID"), "{bash}");
    assert!(ps1.contains("LOOMUX_GROUP_DIR") && ps1.contains("LOOMUX_AGENT_ID"), "{ps1}");
    // Existence-only: the marker path is `.promptsubmit.jsonl` (same file the
    // Claude script writes real JSON records into), but this command never
    // reads Copilot's own payload — the docs don't nail down its transport
    // for this event (unlike Claude's stdin-JSON contract). Confirmed by
    // there being no field-name text (`user_input`/`prompt`/etc) anywhere in
    // either command string — the whole point of `PromptSubmitRecord::text`
    // being `None` for a Copilot record.
    assert!(bash.contains("promptsubmit.jsonl") && ps1.contains("promptsubmit.jsonl"));
    for field in ["user_input", "user_prompt", "\"prompt\"", "stdin", "ReadToEnd"] {
        assert!(!bash.contains(field) && !ps1.contains(field), "no payload read attempted: field={field} bash={bash} ps1={ps1}");
    }
}

// ───────── rev-4 review round 3: PreCompact can BLOCK compaction — exit-0 safety ─────────
//
// Claude's own hooks reference (code.claude.com/docs/en/hooks) confirms `PreCompact` is a
// BLOCKING event: exit code 2 (or `{"decision":"block"}`) prevents the compaction from
// happening at all. Copilot's reference documents the same for its own `preCompact`. A
// marker-write failure (a full disk, a permissions problem, a path collision) must degrade
// to "no hook signal, the inference tier still catches it" — NEVER to "the user's
// compaction silently didn't happen." These tests run the ACTUAL generated script/command
// text under an induced failure and assert the real process exit code, rather than
// reasoning about shell semantics from the Rust side only.

/// `sh` to run a hook script with — `locate_sh_exe`'s absolute Windows path, or a bare `sh`
/// (a POSIX guarantee) everywhere else. `None` only when Windows genuinely has no `sh.exe`
/// anywhere (same "skip, don't fail" precedent as the #335 shim tests).
fn resolve_test_sh() -> Option<String> {
    #[cfg(windows)]
    {
        locate_sh_exe()
    }
    #[cfg(not(windows))]
    {
        Some("sh".to_string())
    }
}

#[test]
fn compact_hook_script_sh_exits_zero_when_the_marker_dir_cant_be_created() {
    let Some(sh) = resolve_test_sh() else {
        eprintln!("SKIP compact_hook_script_sh_exits_zero_when_the_marker_dir_cant_be_created: no sh found");
        return;
    };
    let td = tempfile::tempdir().unwrap();
    // A regular FILE occupying the path the script wants to `mkdir -p` a
    // subdirectory under — fails `mkdir -p "$group_dir/hooks"` portably, with
    // no reliance on chmod/permission APIs (unreliable to set up on Windows).
    let blocked_group_dir = td.path().join("blocked-group-dir");
    fs::write(&blocked_group_dir, "occupies the path; not a directory").unwrap();
    let script_path = td.path().join("compact-hook.sh");
    fs::write(&script_path, COMPACT_HOOK_SCRIPT).unwrap();

    for event in ["precompact", "sessionstart-compact"] {
        let output = std::process::Command::new(&sh)
            .arg(&script_path)
            .arg(event)
            .arg(blocked_group_dir.display().to_string())
            .arg("agent-1")
            .output()
            .expect("sh must run");
        assert_eq!(
            output.status.code(),
            Some(0),
            "event={event}: the hook must never fail PreCompact's blocking lifecycle event just \
             because its own marker write failed — stdout={:?} stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

#[test]
fn copilot_precompact_hook_bash_exits_zero_when_the_marker_dir_cant_be_created() {
    let Some(sh) = resolve_test_sh() else {
        eprintln!("SKIP copilot_precompact_hook_bash_exits_zero_when_the_marker_dir_cant_be_created: no sh found");
        return;
    };
    let td = tempfile::tempdir().unwrap();
    let blocked_group_dir = td.path().join("blocked-group-dir");
    fs::write(&blocked_group_dir, "occupies the path; not a directory").unwrap();

    let output = std::process::Command::new(&sh)
        .arg("-c")
        .arg(COPILOT_PRECOMPACT_HOOK_BASH)
        .env("LOOMUX_GROUP_DIR", blocked_group_dir.display().to_string())
        .env("LOOMUX_AGENT_ID", "agent-1")
        .output()
        .expect("sh must run");
    assert_eq!(
        output.status.code(),
        Some(0),
        "Copilot's preCompact is ALSO a blocking event per its own docs — stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[cfg(windows)]
#[test]
fn copilot_precompact_hook_powershell_exits_zero_when_the_marker_dir_cant_be_created() {
    let td = tempfile::tempdir().unwrap();
    let blocked_group_dir = td.path().join("blocked-group-dir");
    fs::write(&blocked_group_dir, "occupies the path; not a directory").unwrap();

    let output = std::process::Command::new("powershell")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(COPILOT_PRECOMPACT_HOOK_POWERSHELL)
        .env("LOOMUX_GROUP_DIR", blocked_group_dir.display().to_string())
        .env("LOOMUX_AGENT_ID", "agent-1")
        .output()
        .expect("powershell must run on a Windows CI runner");
    assert_eq!(
        output.status.code(),
        Some(0),
        "the try/catch around both New-Item calls must make this deterministic regardless of \
         PowerShell's own terminating-vs-non-terminating error classification — stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

// ─────── #112: the `promptsubmit` hook arm — real-execution safety + content tests ───────
//
// SAFETY-CRITICAL (see `COMPACT_HOOK_SCRIPT`'s doc): `UserPromptSubmit` exit code 2
// ERASES the user's prompt, and exit-0 stdout is injected as context the model sees.
// So this arm is held to a STRICTER bar than precompact/sessionstart-compact above —
// not just "always exits 0" but "never prints anything to its own stdout", on every
// path including the induced-failure one.

#[test]
fn promptsubmit_hook_script_sh_exits_zero_and_prints_nothing_when_the_marker_dir_cant_be_created() {
    let Some(sh) = resolve_test_sh() else {
        eprintln!("SKIP promptsubmit_hook_script_sh_exits_zero_and_prints_nothing_when_the_marker_dir_cant_be_created: no sh found");
        return;
    };
    use std::io::Write;
    use std::process::Stdio;
    let td = tempfile::tempdir().unwrap();
    let blocked_group_dir = td.path().join("blocked-group-dir");
    fs::write(&blocked_group_dir, "occupies the path; not a directory").unwrap();
    let script_path = td.path().join("compact-hook.sh");
    fs::write(&script_path, COMPACT_HOOK_SCRIPT).unwrap();

    let mut child = std::process::Command::new(&sh)
        .arg(&script_path)
        .arg("promptsubmit")
        .arg(blocked_group_dir.display().to_string())
        .arg("agent-1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("sh must run");
    child.stdin.take().unwrap().write_all(br#"{"prompt":"do the thing"}"#).unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "must never erase the user's prompt (exit 2) just because its own marker write failed — \
         stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.stdout.is_empty(),
        "stdout on exit 0 is injected as context the model sees — this arm must print NOTHING, \
         got: {:?}",
        String::from_utf8_lossy(&output.stdout),
    );
}

#[test]
fn promptsubmit_hook_script_sh_appends_stdin_verbatim_with_no_stdout() {
    let Some(sh) = resolve_test_sh() else {
        eprintln!("SKIP promptsubmit_hook_script_sh_appends_stdin_verbatim_with_no_stdout: no sh found");
        return;
    };
    use std::io::Write;
    use std::process::Stdio;
    let td = tempfile::tempdir().unwrap();
    let group_dir = td.path().join("group");
    fs::create_dir_all(&group_dir).unwrap();
    let script_path = td.path().join("compact-hook.sh");
    fs::write(&script_path, COMPACT_HOOK_SCRIPT).unwrap();
    let payload = r#"{"session_id":"abc123","hook_event_name":"UserPromptSubmit","prompt":"implement #112"}"#;

    // Two firings, to prove APPEND (not overwrite) — the whole point of a
    // byte-offset baseline.
    for _ in 0..2 {
        let mut child = std::process::Command::new(&sh)
            .arg(&script_path)
            .arg("promptsubmit")
            .arg(group_dir.display().to_string())
            .arg("agent-1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("sh must run");
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stdout.is_empty(), "no stdout: {:?}", String::from_utf8_lossy(&output.stdout));
    }

    let marker = group_dir.join("hooks").join("agent-1.promptsubmit.jsonl");
    let content = fs::read_to_string(&marker).unwrap();
    let records = promptsubmit_records_since(&content, 0);
    assert_eq!(records.len(), 2, "two firings must yield two APPENDED lines, not one overwritten line: {content:?}");
    for r in &records {
        assert_eq!(r.text.as_deref(), Some("implement #112"));
    }
}

#[test]
fn copilot_promptsubmit_hook_bash_exits_zero_when_the_marker_dir_cant_be_created() {
    let Some(sh) = resolve_test_sh() else {
        eprintln!("SKIP copilot_promptsubmit_hook_bash_exits_zero_when_the_marker_dir_cant_be_created: no sh found");
        return;
    };
    let td = tempfile::tempdir().unwrap();
    let blocked_group_dir = td.path().join("blocked-group-dir");
    fs::write(&blocked_group_dir, "occupies the path; not a directory").unwrap();

    let output = std::process::Command::new(&sh)
        .arg("-c")
        .arg(COPILOT_PROMPTSUBMIT_HOOK_BASH)
        .env("LOOMUX_GROUP_DIR", blocked_group_dir.display().to_string())
        .env("LOOMUX_AGENT_ID", "agent-1")
        .output()
        .expect("sh must run");
    assert_eq!(output.status.code(), Some(0), "stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(output.stdout.is_empty());
}

#[test]
fn copilot_promptsubmit_hook_bash_appends_an_existence_marker() {
    let Some(sh) = resolve_test_sh() else {
        eprintln!("SKIP copilot_promptsubmit_hook_bash_appends_an_existence_marker: no sh found");
        return;
    };
    let td = tempfile::tempdir().unwrap();
    let group_dir = td.path().join("group");

    for _ in 0..2 {
        let output = std::process::Command::new(&sh)
            .arg("-c")
            .arg(COPILOT_PROMPTSUBMIT_HOOK_BASH)
            .env("LOOMUX_GROUP_DIR", group_dir.display().to_string())
            .env("LOOMUX_AGENT_ID", "agent-1")
            .output()
            .expect("sh must run");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stdout.is_empty());
    }
    let marker = group_dir.join("hooks").join("agent-1.promptsubmit.jsonl");
    let content = fs::read_to_string(&marker).unwrap();
    let records = promptsubmit_records_since(&content, 0);
    assert_eq!(records.len(), 2, "two firings, two existence records: {content:?}");
    assert!(records.iter().all(|r| r.text.is_none()), "Copilot never captures prompt text: {records:?}");
}

#[cfg(windows)]
#[test]
fn copilot_promptsubmit_hook_powershell_exits_zero_when_the_marker_dir_cant_be_created() {
    let td = tempfile::tempdir().unwrap();
    let blocked_group_dir = td.path().join("blocked-group-dir");
    fs::write(&blocked_group_dir, "occupies the path; not a directory").unwrap();

    let output = std::process::Command::new("powershell")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(COPILOT_PROMPTSUBMIT_HOOK_POWERSHELL)
        .env("LOOMUX_GROUP_DIR", blocked_group_dir.display().to_string())
        .env("LOOMUX_AGENT_ID", "agent-1")
        .output()
        .expect("powershell must run on a Windows CI runner");
    assert_eq!(output.status.code(), Some(0), "stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(output.stdout.is_empty());
}

// ───────────────────── #112: pure decision-function tests ─────────────────────

#[test]
fn promptsubmit_records_since_parses_jsonl_and_tolerates_a_partial_trailing_line() {
    let content = "{\"prompt\":\"first\"}\n{\"prompt\":\"second\"}\n{\"prom";
    let all = promptsubmit_records_since(content, 0);
    assert_eq!(all.len(), 3, "the torn trailing line still counts as existence evidence, not dropped");
    assert_eq!(all[0].text.as_deref(), Some("first"));
    assert_eq!(all[1].text.as_deref(), Some("second"));
    assert_eq!(all[2].text, None, "unparseable JSON degrades to an existence-only record");

    // Offset baseline: a record BEFORE the offset must never appear, even
    // though it's syntactically identical to one after — this is the whole
    // mechanism that makes a stale/earlier-delivery record unable to satisfy
    // a later delivery's confirmation.
    let first_line_len = content.find('\n').unwrap() + 1;
    let since = promptsubmit_records_since(content, first_line_len);
    assert_eq!(since.len(), 2);
    assert_eq!(since[0].text.as_deref(), Some("second"));

    // Offset past the end (or on an invalid boundary) degrades to "no new
    // records" rather than panicking.
    assert!(promptsubmit_records_since(content, content.len() + 100).is_empty());
    assert!(promptsubmit_records_since(content, content.len()).is_empty());
}

#[test]
fn promptsubmit_records_since_tolerates_unknown_fields_and_legacy_field_names() {
    // The documented Claude field is `prompt` — verbatim from the live
    // hooks reference (code.claude.com/docs/en/hooks, "UserPromptSubmit
    // input"): "UserPromptSubmit hooks receive the `prompt` field
    // containing the text the user submitted." `user_input`/`user_prompt`
    // are tolerated fallback field names only (neither appears anywhere on
    // that page — round 1 review caught an earlier version of this module
    // citing `user_input` as primary, which was simply wrong), and
    // unrelated fields never break parsing.
    let content = "{\"session_id\":\"x\",\"prompt\":\"a\"}\n\
                   {\"user_input\":\"b\"}\n\
                   {\"user_prompt\":\"c\"}\n\
                   {\"hook_event_name\":\"UserPromptSubmit\"}\n";
    let records = promptsubmit_records_since(content, 0);
    assert_eq!(records.len(), 4);
    assert_eq!(records[0].text.as_deref(), Some("a"), "the documented field, `prompt`");
    assert_eq!(records[1].text.as_deref(), Some("b"), "legacy fallback `user_input`");
    assert_eq!(records[2].text.as_deref(), Some("c"), "legacy fallback `user_prompt`");
    assert_eq!(records[3].text, None, "no recognized text field at all — existence only");
}

#[test]
fn promptsubmit_records_since_prefers_the_documented_field_over_legacy_fallbacks() {
    // If a single record somehow carried more than one of these fields, the
    // documented `prompt` field must win — the fallbacks exist for CLIs/
    // versions that DON'T send `prompt`, never to shadow it when it's present.
    let content = "{\"prompt\":\"real\",\"user_input\":\"stale-or-wrong\"}\n";
    let records = promptsubmit_records_since(content, 0);
    assert_eq!(records[0].text.as_deref(), Some("real"));
}

#[test]
fn prompt_landed_matches_exact_and_crlf_and_trailing_whitespace() {
    let exact = vec![PromptSubmitRecord { text: Some("implement #112".to_string()) }];
    assert_eq!(prompt_landed(&exact, "implement #112"), PromptLandedMatch::Content { merged: false });

    let crlf = vec![PromptSubmitRecord { text: Some("implement\r\n#112\r\n".to_string()) }];
    assert_eq!(prompt_landed(&crlf, "implement\n#112\n"), PromptLandedMatch::Content { merged: false },
        "CRLF-vs-LF and trailing whitespace must wash out of the comparison");

    let mismatched = vec![PromptSubmitRecord { text: Some("something else entirely".to_string()) }];
    assert_eq!(prompt_landed(&mismatched, "implement #112"), PromptLandedMatch::None);
}

#[test]
fn prompt_landed_containment_flags_a_merged_submission() {
    // The exact #112 root-cause shape: our paste merged with human-typed
    // `/model` text, and the CLI's own record shows the LARGER merged string.
    // Still landed (the agent DID receive the task text — plan-14 decision
    // #3), but flagged as merged rather than a clean exact match.
    let merged = vec![PromptSubmitRecord { text: Some("/modelimplement #112".to_string()) }];
    assert_eq!(prompt_landed(&merged, "implement #112"), PromptLandedMatch::Content { merged: true });

    // The reverse — our paste is a SUPERSET of the record — must NOT match:
    // containment only goes one direction (the record must contain what we
    // pasted, not the other way around).
    let too_short = vec![PromptSubmitRecord { text: Some("implement".to_string()) }];
    assert_eq!(prompt_landed(&too_short, "implement #112"), PromptLandedMatch::None);
}

#[test]
fn prompt_landed_existence_tier_for_textless_records() {
    // Copilot's shape: a record fired, but carries no text at all.
    let copilot = vec![PromptSubmitRecord { text: None }];
    assert_eq!(prompt_landed(&copilot, "implement #112"), PromptLandedMatch::Existence);

    // No records at all: nothing to trust.
    assert_eq!(prompt_landed(&[], "implement #112"), PromptLandedMatch::None);

    // A textless record mixed with a non-matching text record still resolves
    // to Existence (never silently promoted to Content, never demoted to None).
    let mixed = vec![
        PromptSubmitRecord { text: Some("unrelated".to_string()) },
        PromptSubmitRecord { text: None },
    ];
    assert_eq!(prompt_landed(&mixed, "implement #112"), PromptLandedMatch::Existence);
}

#[test]
fn prompt_landed_never_trivially_matches_empty_pasted_text() {
    // Guards the containment check: an empty normalized paste would
    // trivially be a substring of ANY text, which must never happen (deliver_
    // prompt never pastes empty text, but the pure fn must not rely on that).
    let records = vec![PromptSubmitRecord { text: Some("anything at all".to_string()) }];
    assert_eq!(prompt_landed(&records, ""), PromptLandedMatch::None);
    assert_eq!(prompt_landed(&records, "   \n\t  "), PromptLandedMatch::None);
}

// ───────────────────── #112 round 2: Tier 1 (box consumption) + 3-state ─────────────────────

#[test]
fn box_holds_paste_detects_presence_and_absence_at_the_tail_end() {
    let pasted = "implement #112 end to end";
    // Present, verbatim, at the tail end -- still holding.
    assert!(box_holds_paste("some earlier output\n> implement #112 end to end", pasted));
    // Gone -- the box redrew to something else entirely.
    assert!(!box_holds_paste("some earlier output\n[thinking...]", pasted));
}

#[test]
fn box_holds_paste_tolerates_crlf_and_whitespace_noise() {
    let pasted = "line one\nline two";
    assert!(box_holds_paste("boxed:\r\nline one\r\nline two\r\n", pasted));
}

#[test]
fn box_holds_paste_ignores_a_match_scrolled_out_of_the_tail_window() {
    // The whole point of windowing on the TAIL END: once enough new content
    // has appeared after our paste, its presence far earlier in the buffer
    // must not still read as "still holding" -- that would mean Tier 1 could
    // never confirm a delivery that got accepted and echoed into scrollback
    // history, which is exactly the false-negative the tail-end restriction
    // (vs. "appears anywhere") exists to prevent.
    let pasted = "short prompt";
    let filler = "x ".repeat(500); // far more than BOX_TAIL_WINDOW_SLACK + len(pasted)
    let tail = format!("short prompt\n{filler}\n[a fresh empty prompt]");
    assert!(!box_holds_paste(&tail, pasted));
}

#[test]
fn box_holds_paste_never_trivially_matches_an_empty_paste() {
    assert!(!box_holds_paste("anything at all sitting in the tail", ""));
    assert!(!box_holds_paste("anything at all sitting in the tail", "   \n\t  "));
}

#[test]
fn box_holds_paste_is_the_documented_precondition_check_and_post_enter_signal_alike() {
    // The SAME function serves both of Tier 1's uses -- verified by using it
    // for both without any special-casing needed.
    let pasted = "the orchestrator's kickoff brief";
    let pre_enter_tail = "> the orchestrator's kickoff brief";
    assert!(box_holds_paste(pre_enter_tail, pasted), "precondition check: literal text present");
    let post_enter_tail_accepted = "[orrerix] thinking...";
    assert!(!box_holds_paste(post_enter_tail_accepted, pasted), "post-Enter: box consumed it");
}

// ───────── #821: a per-row gutter through a multi-row paste is not an absence ─────────
//
// `normalize_prompt_text` flattens the whole tail, so copilot's framed composer
// — which draws `┃ ` down EVERY row it wrapped a paste onto — puts its gutter
// *inside* the haystack, interleaved through the needle. Containment fails on
// text plainly still in the box, and the reading is not merely wrong but
// CONFIDENTLY wrong: the only guard between a failed containment and
// `NotHolding` is a length test, and decoration ADDS characters, so the tail is
// always longer than the paste it just failed to contain.
//
// Post-#819 that reading LICENSES retirement (`stranded_marker_action` retires
// on `Some(BoxReading::NotHolding)`), and #819's safety argument — "nothing
// retires on an absence of evidence" — does not cover counterfeit evidence OF
// an absence. So a false negative here retires a marker whose text is still in
// the box: the Enter that would have submitted it is never pressed, and the
// next queue entry pastes on top and submits both merged, which is the
// #81/#84/#111 collision that licence exists to prevent.

/// Copilot's framed composer holding a multi-row loomux paste. Reconstructed
/// from the shapes cited in #820 (`copilot-cli#4116`'s verbatim composer
/// paste), not captured — constraint 3 forbids a live copilot.
pub(crate) const FIX_BOX_GUTTER_MULTIROW: &str =
    include_str!("../fixtures/attention/copilot-framed-composer-multirow-paste.txt");
/// The same, plus the scrollbar track on the RIGHT edge (`copilot-cli#4009` —
/// `┃` serves as both composer border and scrollbar). `deframe` strips LEADING
/// decoration only, so this stays unreadable by design: it is the shape the
/// partial-match probe exists for.
pub(crate) const FIX_BOX_GUTTER_SCROLLBAR: &str =
    include_str!("../fixtures/attention/copilot-framed-composer-scrollbar.txt");
/// The exact text loomux pasted into the pane both fixtures show.
pub(crate) const GUTTER_PASTE_TEXT: &str = "Review requested changes on PR #814.\n\
     Read the findings on the PR itself and address every item.\n\
     Push fixes to the same branch and report when ready for re-review.";

#[test]
fn h1_a_gutter_through_a_multirow_paste_does_not_read_as_gone() {
    let tail = strip_ansi(FIX_BOX_GUTTER_MULTIROW.as_bytes());

    // Preconditions, so this fails for the reason it claims: the gutter really
    // is on every row of our paste, and the tail really is longer than the
    // paste — which is what makes the length guard useless here.
    assert_eq!(
        tail.lines().filter(|l| l.contains('┃')).count(),
        3,
        "precondition: the gutter is on EVERY row of the paste, not just the first: {tail:?}"
    );
    assert!(
        tail.split_whitespace().collect::<Vec<_>>().join(" ").len()
            > GUTTER_PASTE_TEXT.split_whitespace().collect::<Vec<_>>().join(" ").len(),
        "precondition: the decorated tail is LONGER than the paste, so the length guard \
         cannot catch the miss — that is the whole shape of #821"
    );

    assert!(
        box_holds_paste(&tail, GUTTER_PASTE_TEXT),
        "our text is sitting in the box behind a per-row gutter — that is still holding"
    );
}

#[test]
fn h2_a_gutter_through_our_paste_never_licenses_a_retirement() {
    // The consequence, at the decision #819 keyed on the reading. This is the
    // test that says what the bug COSTS rather than that a helper is wrong.
    let tail = strip_ansi(FIX_BOX_GUTTER_MULTIROW.as_bytes());
    let reading = box_reading(Some(&tail), GUTTER_PASTE_TEXT);
    assert_eq!(reading, BoxReading::Holds, "the box holds our text; say so");

    let action = stranded_marker_action(
        Some(false),
        Some(reading),
        false,
        HumanInputBlock::None,
        false,
        || false,
    );
    assert!(
        !matches!(action, StrandedMarkerAction::Retire(_)),
        "retiring here strands our own text forever AND lets the next delivery paste on top \
         of it — the #81/#84/#111 collision #819's licence exists to prevent: got {action:?}"
    );
}

#[test]
fn h3_decoration_we_cannot_account_for_reads_unverifiable_rather_than_absent() {
    // The arm that makes this robust to decoration nobody has catalogued.
    // `deframe` removes LEADING decoration, so a trailing scrollbar `┃` still
    // defeats containment — and it must not therefore read as absence.
    let tail = strip_ansi(FIX_BOX_GUTTER_SCROLLBAR.as_bytes());
    assert!(
        !box_holds_paste(&tail, GUTTER_PASTE_TEXT),
        "precondition: trailing decoration is genuinely NOT handled by de-framing — this test \
         is about the fallback, and would prove nothing if containment already succeeded"
    );
    assert_eq!(
        box_reading(Some(&tail), GUTTER_PASTE_TEXT),
        BoxReading::Unverifiable,
        "our paste's own line is still visibly on screen: we looked and could not tell, which \
         is not the same as it being gone"
    );
    let action = stranded_marker_action(
        Some(false),
        Some(BoxReading::Unverifiable),
        true,
        HumanInputBlock::Blocked,
        false,
        || false,
    );
    assert!(
        !matches!(action, StrandedMarkerAction::Retire(_)),
        "#819 already treats Unverifiable as no evidence — that is what makes this arm safe"
    );
}

#[test]
fn h4_a_genuinely_consumed_box_still_reads_as_gone_and_still_retires() {
    // The floor, and the cost of getting the probe wrong. `Unverifiable` falls
    // through to the ordinary gates, so a probe that fired on everything would
    // erode #813/#819's repair back toward the deadlock it exists to break.
    // Long enough to clear the length guard on its own, or this would pass as
    // `Unverifiable` for a reason that has nothing to do with the probe.
    let consumed = "● Reading src-tauri/src/orchestration/mod.rs to find the delivery gate.\n\
         ● The queue drainer re-reads write_admission every poll, so the hold lives there\n\
           rather than inside deliver_now's own capped waits.\n\
         ● Working on it.\n\n\
         ┃                                                                  ┃\n\
         \x20 @ files · # issues";
    assert!(
        consumed.split_whitespace().collect::<Vec<_>>().join(" ").len()
            > GUTTER_PASTE_TEXT.split_whitespace().collect::<Vec<_>>().join(" ").len(),
        "precondition: the tail clears the length guard, so `NotHolding` below is the probe's \
         verdict and not the length test's"
    );
    assert_eq!(
        box_reading(Some(consumed), GUTTER_PASTE_TEXT),
        BoxReading::NotHolding,
        "no fragment of our paste is on screen — this is a real absence and must stay one"
    );
    assert!(
        matches!(
            stranded_marker_action(
                Some(false), Some(BoxReading::NotHolding), false,
                HumanInputBlock::None, false, || false,
            ),
            StrandedMarkerAction::Retire(StrandedRetireReason::TextGone)
        ),
        "and #819's repair still fires"
    );

    // A paste with no line long enough to be evidence yields no probe at all,
    // so it keeps the pre-#821 reading rather than a coincidence-prone one.
    assert_eq!(
        box_reading(Some("● idle\n┃ ok ┃\n  @ files"), "ok\nfine"),
        BoxReading::NotHolding,
        "short lines are not evidence; absence of a probe is not a licence to invent one"
    );
}

/// Copilot's chevron composer in a NARROW pane (~35 columns — a four-way
/// split, which is this product's premise), wrapping a brief line immediately
/// before a mid-line `|`. Reconstructed from #820's cited shapes, not captured.
const FIX_BOX_NARROW_PIPE_WRAP: &str =
    include_str!("../fixtures/attention/copilot-narrow-pane-pipe-wrap.txt");
/// The line that fixture's composer is holding. The `|` is MID-line here and
/// row-LEADING there, which is the whole mechanism.
const NARROW_PIPE_PASTE_TEXT: &str = "Check the queue depth with ps aux | grep loomux and report it";

#[test]
fn h6_de_framing_never_costs_a_match_the_flat_comparison_would_have_found() {
    // rev-306 B1. De-framing is a strict IMPROVEMENT or it is a regression;
    // there is no third option, because the reading it feeds is one where a
    // false `NotHolding` is the expensive error.
    //
    // A wrap that pushes a mid-line `|` (or `*`, `•`, `●`, `◆`, `│`, `┃`) to a
    // row START strips it from the TAIL — `deframe` sees a row-leading frame
    // char — while the NEEDLE keeps it, because there it is mid-line and
    // `deframe` is leading-only. Containment then fails on text that is
    // verbatim on screen. The length arm cannot fire (the tail is longer, as
    // always here), and the probe carries the same `|` from the same line, so
    // it fails for the same reason: `NotHolding`, where the pre-#821 flat
    // comparison read `Holds`.
    //
    // A wrap boundary lands inside the probe's 48-character sample exactly when
    // the composer is under 48 columns, so this needs no degenerate pane — a
    // 25-47 column split is ordinary. An earlier residual bounded this at
    // "sub-24-character lines", which governs only whether a probe is FORMED,
    // never whether a formed probe MATCHES.
    let tail = strip_ansi(FIX_BOX_NARROW_PIPE_WRAP.as_bytes());

    // Preconditions — the geometry this depends on, asserted rather than
    // assumed, since a fixture that drifts would pass for the wrong reason.
    let widest = tail.lines().map(|l| l.chars().count()).max().unwrap_or(0);
    assert!(
        (25..=47).contains(&widest),
        "precondition: an ORDINARY narrow pane, not a degenerate one — that is the whole \
         disagreement about reach: {widest} columns"
    );
    assert!(
        tail.lines().any(|l| l.trim_start().starts_with('|')),
        "precondition: a wrapped row LEADS with the pipe, which is what `deframe` strips: {tail:?}"
    );
    assert!(
        NARROW_PIPE_PASTE_TEXT.contains(" | "),
        "precondition: and the same pipe is MID-line in our own text, where `deframe` keeps it"
    );

    assert!(
        box_holds_paste(&tail, NARROW_PIPE_PASTE_TEXT),
        "the text is verbatim on screen and the pre-#821 comparison found it — de-framing must \
         never LOSE a match, only add one"
    );
    assert_eq!(box_reading(Some(&tail), NARROW_PIPE_PASTE_TEXT), BoxReading::Holds);

    // The consequence, at the decision that spends the reading — the same shape
    // as `h2`, because this is the same harm arriving by a different route.
    let action = stranded_marker_action(
        Some(false),
        Some(box_reading(Some(&tail), NARROW_PIPE_PASTE_TEXT)),
        false,
        HumanInputBlock::None,
        false,
        || false,
    );
    assert!(
        !matches!(action, StrandedMarkerAction::Retire(_)),
        "a regression into `NotHolding` retires the marker over our own un-submitted text — the \
         #81/#84/#111 collision this change exists to close: got {action:?}"
    );
}

/// A frame-heavy paste: every line is a markdown table row, so `deframe`
/// strips a leading `|` from each and the de-framed needle is materially
/// shorter than the flat one (192 vs 204 characters normalized — a 12-char
/// gap, two per line).
const FRAME_HEAVY_PASTE: &str = "| record | rev-306 | re-record the verdict on the final head |\n\
     | step | owner | notes |\n\
     | rebase | w-300 | onto main |\n\
     | verify | rev-306 | matrix |\n\
     | merge | human | gated |\n\
     | sweep | w-300 | disjuncts |";

#[test]
fn h7_a_short_read_is_unverifiable_on_either_routes_arithmetic() {
    // rev-307. Once `box_holds_paste` became a disjunction, every arm BELOW it
    // had to be asked of both routes too — and the length arm was still asking
    // one. De-framing shrinks a frame-heavy needle (a markdown table: 204 flat,
    // 192 de-framed) more than it shrinks an un-gutted tail, so a truncated read
    // whose length falls in that 12-character gap is:
    //
    //   * too short for the FLAT needle      -> pre-#821 said `Unverifiable`
    //   * long enough for the DE-FRAMED one  -> the de-framed arm stays silent
    //
    // and the probe cannot rescue it, because truncating from the FRONT removes
    // the longest line the probe is sampled from. `NotHolding` — our text may
    // well be in that box, and we have just told `stranded_marker_action` it is
    // not. Same direction as the containment defect, narrower trigger (it needs
    // a short read as well), same class.
    let screen = format!("● Ready.\n{FRAME_HEAVY_PASTE}\n  @ files · # issues");
    let flat = screen.split_whitespace().collect::<Vec<_>>().join(" ");
    // Truncate from the FRONT — a real short read keeps the tail END.
    let tail: String = flat.chars().skip(flat.chars().count() - 197).collect();

    // Preconditions: the shape this depends on, so a drifting fixture fails
    // loudly rather than passing for an unrelated reason.
    assert!(
        FRAME_HEAVY_PASTE.lines().all(|l| l.trim_start().starts_with('|')),
        "precondition: FRAME-heavy — every line leads with a character `deframe` strips, which \
         is what opens the gap between the two normalized needle lengths"
    );
    assert!(
        !box_holds_paste(&tail, FRAME_HEAVY_PASTE),
        "precondition: neither containment route finds it, so the reading is decided by the \
         arms below — this test is about those, not about containment"
    );
    let flat_needle = FRAME_HEAVY_PASTE.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        tail.chars().count() < flat_needle.chars().count(),
        "precondition: a genuinely SHORT read — the flat arithmetic says we could not have seen \
         the whole paste, which is exactly what `Unverifiable` means"
    );

    assert_eq!(
        box_reading(Some(&tail), FRAME_HEAVY_PASTE),
        BoxReading::Unverifiable,
        "a read too short for either route's needle is one we could not take — `NotHolding` here \
         is a confident absence asserted from a partial view"
    );

    // The consequence, at the decision that spends it — the third time this
    // same harm arrives by a new route (`h2` gutter, `h6` wrap, `h7` short read).
    let action = stranded_marker_action(
        Some(false),
        Some(box_reading(Some(&tail), FRAME_HEAVY_PASTE)),
        false,
        HumanInputBlock::None,
        false,
        || false,
    );
    assert!(
        !matches!(action, StrandedMarkerAction::Retire(_)),
        "#819 retires only on a POSITIVE `NotHolding`; a truncated read is not one: got {action:?}"
    );
}

#[test]
fn h5_a_markdown_bullet_in_a_brief_is_not_mistaken_for_box_framing() {
    // The trap in de-framing only the TAIL. `is_frame_char` counts `*`, `•` and
    // `|` as framing, so a brief carrying an ordinary bullet list or table
    // would have those stripped from the tail and kept in the needle — every
    // such paste would fail containment, clear the length guard (the tail being
    // longer, exactly as in the gutter case) and read as a confident absence.
    // That is the #821 failure re-introduced by the #821 fix, so it is pinned.
    let bulleted = "Do these in order:\n\
         * rebase onto main and re-read the findings\n\
         * push the fix and wait for the full matrix\n\
         | step | owner |";
    let rendered = "● Ready.\n\
         ┃ Do these in order:\n\
         ┃ * rebase onto main and re-read the findings\n\
         ┃ * push the fix and wait for the full matrix\n\
         ┃ | step | owner |\n\
         \x20 @ files · # issues";
    assert!(
        box_holds_paste(rendered, bulleted),
        "a bullet is our own text, not the CLI's frame — and it is on both sides, so it cancels"
    );
    assert_eq!(box_reading(Some(rendered), bulleted), BoxReading::Holds);
}

#[test]
fn confirm_state_for_maps_each_source_to_the_correct_three_state_outcome() {
    // Pinned directly: a mutation swapping any one of these arms is exactly
    // backwards and must fail this test, not just some downstream behavior.
    assert_eq!(confirm_state_for(ConfirmSource::Box), DeliveryConfirmState::Confirmed);
    assert_eq!(confirm_state_for(ConfirmSource::Hook), DeliveryConfirmState::Confirmed);
    assert_eq!(confirm_state_for(ConfirmSource::Burst), DeliveryConfirmState::Confirmed);
    assert_eq!(confirm_state_for(ConfirmSource::BoxVeto), DeliveryConfirmState::Failed);
    assert_eq!(confirm_state_for(ConfirmSource::Idle), DeliveryConfirmState::Failed);
    assert_eq!(confirm_state_for(ConfirmSource::None), DeliveryConfirmState::Pending);
}

#[test]
fn confirm_source_and_state_as_str_cover_every_variant() {
    for (src, state, src_str, state_str) in [
        (ConfirmSource::Box, DeliveryConfirmState::Confirmed, "box", "confirmed"),
        (ConfirmSource::Hook, DeliveryConfirmState::Confirmed, "hook", "confirmed"),
        (ConfirmSource::Burst, DeliveryConfirmState::Confirmed, "burst", "confirmed"),
        (ConfirmSource::BoxVeto, DeliveryConfirmState::Failed, "box_veto", "failed"),
        (ConfirmSource::Idle, DeliveryConfirmState::Failed, "idle", "failed"),
        (ConfirmSource::None, DeliveryConfirmState::Pending, "none", "pending"),
    ] {
        assert_eq!(src.as_str(), src_str);
        assert_eq!(state.as_str(), state_str);
        assert_eq!(confirm_state_for(src), state);
    }
}

#[test]
fn delivery_confirmed_late_notice_names_the_agent_and_reads_as_a_correction() {
    let notice = delivery_confirmed_late_notice("w-13");
    assert!(notice.contains("w-13"));
    assert!(notice.to_lowercase().contains("correction"), "{notice}");
    // Must NOT read as a fresh success notice -- it corrects a specific
    // prior alarm, so it should reference that the earlier one was wrong.
    assert!(notice.to_lowercase().contains("wrong") || notice.to_lowercase().contains("unconfirmed"), "{notice}");
}

// ─────────── #112 round 3 (rev-20): the polarity pins for B1/B2/B3 ───────────

#[test]
fn tier1_trusted_requires_no_human_input_since_our_own_submit() {
    assert!(tier1_trusted(1000, 1000), "input at exactly submit time is not AFTER it");
    assert!(tier1_trusted(999, 1000), "input strictly before submit is fine");
    assert!(!tier1_trusted(1001, 1000), "any input after submit contaminates the reading");
}

#[test]
fn final_window_outcome_vetoes_only_on_natural_exhaustion_with_everything_else_aligned() {
    // The one case that SHOULD veto: nothing else decided, Tier 1 governs,
    // it never saw the box clear, the window ran to natural exhaustion, and
    // no human touched the pane since submit.
    assert_eq!(
        final_window_outcome(ConfirmSource::None, true, Some(true), true, true),
        ConfirmSource::BoxVeto,
    );
}

#[test]
fn final_window_outcome_never_vetoes_off_an_early_exit_rev20_b2() {
    // THE blocking finding, pinned directly: a question-pending or human-
    // typing exit from the retry loop must never produce BoxVeto, no matter
    // how "still holding" Tier 1's own reading looks. Every other input is
    // held at the values that WOULD veto if this one flag didn't override —
    // proving this flag alone is what's protecting the polarity.
    assert_eq!(
        final_window_outcome(ConfirmSource::None, true, Some(true), /* exhausted */ false, true),
        ConfirmSource::None,
        "an early exit must leave the delivery Pending (None), never Failed (BoxVeto)",
    );
}

#[test]
fn final_window_outcome_never_vetoes_or_confirms_once_a_human_touched_the_pane_rev20_b3() {
    // The other blocking finding: human input since submit must suppress a
    // veto too (not just a confirm) -- Tier 1's reading is contaminated in
    // BOTH directions, exactly as the design note (now truthfully) claims.
    assert_eq!(
        final_window_outcome(ConfirmSource::None, true, Some(true), true, /* trusted */ false),
        ConfirmSource::None,
    );
}

#[test]
fn final_window_outcome_never_invents_an_outcome_when_a_precondition_fails() {
    // Tier 1 not governing this delivery at all: never vetoes even with a
    // "still holding" reading and a natural-exhaustion, trusted window --
    // that reading was never authoritative for this delivery to begin with.
    assert_eq!(
        final_window_outcome(ConfirmSource::None, /* governs */ false, Some(true), true, true),
        ConfirmSource::None,
    );
    // Tier 1's own reading was never "still holding" at the end (it cleared,
    // or was never observed) -- no veto.
    assert_eq!(
        final_window_outcome(ConfirmSource::None, true, Some(false), true, true),
        ConfirmSource::None,
    );
    assert_eq!(
        final_window_outcome(ConfirmSource::None, true, None, true, true),
        ConfirmSource::None,
    );
}

#[test]
fn final_window_outcome_never_overrides_a_decision_already_made() {
    // A veto is only for the genuinely undecided case -- an already-decided
    // `confirm_source` (hook/burst/box already confirmed it, e.g.) must ride
    // through completely unchanged, even with every other veto precondition
    // satisfied.
    for already in [ConfirmSource::Hook, ConfirmSource::Burst, ConfirmSource::Box] {
        assert_eq!(final_window_outcome(already, true, Some(true), true, true), already);
    }
}

#[test]
fn late_monitor_tick_supersession_wins_over_everything_including_a_hook_match() {
    // rev-20 B1: a stale monitor must exit before it can write or notify
    // ANYTHING, even a hook match that would otherwise look like a genuine
    // resolution -- that match may well be the RE-SEND's own record, and
    // confirming off it would still be the clobber/false-correction hazard.
    assert_eq!(
        late_monitor_tick(
            /* superseded */ true,
            PromptLandedMatch::Existence,
            /* already_failed */ true,
            /* expired */ true,
            /* quiet_long_enough */ true,
            /* showing_question */ false,
        ),
        MonitorAction::Superseded,
    );
}

#[test]
fn late_monitor_tick_a_hook_match_confirms_and_flags_correction_only_when_already_failed() {
    assert_eq!(
        late_monitor_tick(false, PromptLandedMatch::Existence, false, false, false, false),
        MonitorAction::Confirm { merged: false, correction: false },
        "upgrading a still-pending delivery is not a correction -- no alarm was ever sent",
    );
    assert_eq!(
        late_monitor_tick(false, PromptLandedMatch::Content { merged: true }, true, false, false, false),
        MonitorAction::Confirm { merged: true, correction: true },
        "resolving an already-failed delivery IS a correction, and the merge flag must ride through",
    );
}

#[test]
fn late_monitor_tick_never_declares_failed_while_a_question_is_on_screen() {
    // The guard the human specified must hold in the monitor's own decision
    // function, not just in the ad-hoc code that used to implement it.
    assert_eq!(
        late_monitor_tick(false, PromptLandedMatch::None, false, false, true, /* showing_question */ true),
        MonitorAction::KeepWaiting,
        "quiet long enough is not sufficient on its own -- a visible question must override it",
    );
    assert_eq!(
        late_monitor_tick(false, PromptLandedMatch::None, false, false, true, false),
        MonitorAction::DeclareFailed,
        "quiet long enough AND no question is what actually declares failure",
    );
}

#[test]
fn late_monitor_tick_never_redeclares_failed_and_never_expires_over_a_hook_match() {
    // Already failed, no new hook: nothing left to do but keep watching.
    assert_eq!(
        late_monitor_tick(false, PromptLandedMatch::None, true, false, true, false),
        MonitorAction::KeepWaiting,
    );
    // Expired AND a hook match on the same tick: the match must still win --
    // resolving on the very last possible tick beats timing out.
    assert_eq!(
        late_monitor_tick(false, PromptLandedMatch::Existence, false, true, false, false),
        MonitorAction::Confirm { merged: false, correction: false },
    );
    // Expired with nothing else going on: times out.
    assert_eq!(
        late_monitor_tick(false, PromptLandedMatch::None, false, true, false, false),
        MonitorAction::Expired,
    );
}

#[test]
fn poll_promptsubmit_hook_degrades_to_none_for_a_missing_or_unreadable_file() {
    let td = tempfile::tempdir().unwrap();
    let missing = td.path().join("nope").join("agent.promptsubmit.jsonl");
    assert_eq!(poll_promptsubmit_hook(&missing, 0, "implement #112"), PromptLandedMatch::None);
}

#[test]
fn poll_promptsubmit_hook_respects_a_baseline_that_excludes_a_stale_record() {
    // The end-to-end impure glue: a record from an EARLIER delivery (or a
    // human's own prompt) sitting before this delivery's baseline offset must
    // never satisfy confirmation, even though its text matches exactly.
    let td = tempfile::tempdir().unwrap();
    let marker = td.path().join("agent.promptsubmit.jsonl");
    fs::write(&marker, "{\"prompt\":\"implement #112\"}\n").unwrap();
    let baseline = promptsubmit_marker_len(&marker);

    assert_eq!(
        poll_promptsubmit_hook(&marker, baseline, "implement #112"),
        PromptLandedMatch::None,
        "a record entirely before this delivery's own baseline must not confirm it",
    );

    // A NEW record appended after the baseline (this delivery's own submit)
    // does confirm.
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new().append(true).open(&marker).unwrap();
    f.write_all(b"{\"prompt\":\"implement #112\"}\n").unwrap();
    drop(f);
    assert_eq!(
        poll_promptsubmit_hook(&marker, baseline, "implement #112"),
        PromptLandedMatch::Content { merged: false },
    );
}

#[test]
fn promptsubmit_marker_path_matches_the_hooks_dir_convention() {
    let root = Path::new("C:/state");
    let path = promptsubmit_marker_path(
        root,
        &parse_gid("group-1"),
        &loomux_lib::orchestration::PathSegment::parse("agent-1").unwrap(),
    );
    assert_eq!(path, root.join("group-1").join("hooks").join("agent-1.promptsubmit.jsonl"));
}

#[test]
fn compact_nudge_skips_a_role_not_in_the_eligible_set() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", compact_rails(20, &["orchestrator"])).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "work", false, None).unwrap();
    let empty = HashMap::new();
    let nudged = reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert_eq!(nudged, vec![o.id.clone()], "only the configured (orchestrator) role is eligible");
    assert!(!nudged.contains(&w.id), "a worker must never be nudged with the default role set");
}

#[test]
fn compact_nudge_no_longer_skips_copilot_which_has_its_own_compact_equivalent() {
    // #417 correction round 2: this test used to prove copilot was skipped
    // entirely ("/compact has no copilot equivalent"). That claim was wrong
    // — see `compact_nudge_cli_supported`'s doc — so it now proves the
    // opposite: a copilot agent gets nudged exactly like a Claude one.
    let (reg, _d) = test_registry();
    let copilot_rails = Guardrails {
        agent_cli: "copilot".into(),
        compact_nudge_minutes: 20,
        compact_nudge_roles: vec!["orchestrator".into()],
        ..rails()
    };
    let g = reg.create_group("C:/tmp/repo", copilot_rails).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let empty = HashMap::new();
    assert_eq!(reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()), vec![o.id.clone()],
        "Copilot has its own /compact — the nudge fires for it too");
    assert_eq!(audit_count(&reg, &g.id, "compact-nudge"), 1);
}

#[test]
fn compact_nudge_skips_a_paused_group_preserving_the_latch() {
    let (reg, _d, gid, oid) = compact_nudge_setup(20);
    let empty = HashMap::new();
    reg.pause_group(&gid).unwrap();
    assert!(reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty(), "a paused group is never nudged");
    assert_eq!(audit_count(&reg, &gid, "compact-nudge"), 0);
    // The one-shot latch/rate budget must be intact: pausing must not have
    // burned it, so on resume the outstanding quiet window still earns its
    // first nudge.
    reg.resume_group(&gid).unwrap();
    assert_eq!(reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()), vec![oid.clone()],
        "resuming a still-idle eligible pane earns its first nudge");
}

#[test]
fn compact_nudge_rate_limit_holds_under_the_per_hour_cap() {
    let (reg, _d, gid, oid) = compact_nudge_setup(5); // 5-minute quiet window
    let m = |t: u64| -> HashMap<String, u64> { [(oid.clone(), t)].into_iter().collect() };
    let none = HashMap::new();
    let win = 5 * 60_000u64;
    let mut t = FAR;
    // The per-hour cap is 4: drive four full fire/reset cycles, each needing
    // the latch cleared (a real burst) then a full window of quiet.
    for i in 0u64..4 {
        assert_eq!(reg.compact_nudge_tick(t, &none, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()), vec![oid.clone()], "nudge {i} under the cap");
        t += 1_000;
        let burst = m(1_000_000 + i * 200_000);
        assert!(reg.compact_nudge_tick(t, &burst, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty(), "burst {i} resets the latch");
        t += 1_000;
        // Quiet again: busy-then-quiet resolves (trusted arm) into the
        // delivery-confirmation phase — `compact_pending` still gates the
        // next fire until that delivery confirms (rev-42 delta, round 2).
        assert!(reg.compact_nudge_tick(t, &burst, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty(), "resolve {i} into the confirmation-wait phase");
        let confirmed = confirmed_delivery(&oid, t);
        t += 1_000;
        assert!(reg.compact_nudge_tick(t, &burst, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &confirmed).is_empty(), "confirm {i}");
        assert!(!reg.agent(&oid).unwrap().compact_pending, "confirmed {i}: latch released");
        t += win + 1;
    }
    assert_eq!(audit_count(&reg, &gid, "compact-nudge"), 4);
    // Latch is clear and the quiet window has fully elapsed again — the ONLY
    // thing standing between here and a fifth nudge is the per-hour cap.
    assert!(reg.compact_nudge_tick(t, &none, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty(),
        "the per-hour cap must hold even though the quiet window elapsed again");
    assert_eq!(audit_count(&reg, &gid, "compact-nudge"), 4, "cap must not be exceeded");
}

#[test]
fn compact_nudge_minutes_and_roles_are_configurable_persisted_and_audited() {
    let (reg, dir, gid, _oid) = compact_nudge_setup(0);
    assert_eq!(reg.group(&gid).unwrap().guardrails.compact_nudge_minutes, 0, "off at launch");
    assert_eq!(reg.set_compact_nudge_minutes(&gid, 15).unwrap(), 15);
    assert_eq!(reg.group(&gid).unwrap().guardrails.compact_nudge_minutes, 15);
    assert_eq!(audit_count(&reg, &gid, "compact-nudge-minutes-set"), 1);
    assert_eq!(reg.set_compact_nudge_minutes(&gid, 99_999).unwrap(), 1440, "clamps to the ceiling");
    assert_eq!(reg.set_compact_nudge_minutes(&gid, 0).unwrap(), 0, "0 stays off, never floored to a default");
    assert!(reg.set_compact_nudge_minutes(&parse_gid("no-such-group"), 5).is_err());
    reg.set_compact_nudge_minutes(&gid, 15).unwrap();

    let roles = reg.set_compact_nudge_roles(&gid, vec!["orchestrator".into(), "worker".into()]).unwrap();
    assert_eq!(roles, vec!["orchestrator".to_string(), "worker".to_string()]);
    assert_eq!(reg.group(&gid).unwrap().guardrails.compact_nudge_roles, roles);
    assert_eq!(audit_count(&reg, &gid, "compact-nudge-roles-set"), 1);
    // Unrecognized entries dropped; an empty result falls back to orchestrator-only.
    assert_eq!(reg.set_compact_nudge_roles(&gid, vec!["bogus".into()]).unwrap(), vec!["orchestrator".to_string()]);
    assert!(reg.set_compact_nudge_roles(&parse_gid("no-such-group"), vec![]).is_err());

    reg.set_compact_nudge_roles(&gid, vec!["orchestrator".into(), "worker".into()]).unwrap();
    // Persisted across restart (live-set values win over the launch default).
    let reg2 = relaunch_registry(dir.path());
    reg2.set_port(45999);
    reg2.create_group("C:/tmp/repo", rails()).unwrap();
    let persisted = &reg2.group(&gid).unwrap().guardrails;
    assert_eq!(persisted.compact_nudge_minutes, 15, "a live-set window survives restart");
    assert_eq!(persisted.compact_nudge_roles, vec!["orchestrator".to_string(), "worker".to_string()],
        "live-set roles survive restart");
}

#[test]
fn compact_nudge_roles_are_canonicalized_to_lowercase_on_set() {
    // rev-24 review: `kind_from_str` validates a role name case-insensitively
    // but does not normalize it, so a mixed-case name would persist as-typed
    // and then never match `compact_nudge_role_allowed`'s lowercase
    // comparison — silently disabling the very role it was meant to enable.
    let (reg, _d, gid, _oid) = compact_nudge_setup(0);
    let applied =
        reg.set_compact_nudge_roles(&gid, vec!["Orchestrator".into(), "WORKER".into()]).unwrap();
    assert_eq!(applied, vec!["orchestrator".to_string(), "worker".to_string()],
        "mixed-case role names must be canonicalized, not stored as-typed");
    assert!(compact_nudge_role_allowed(Role::Worker, &applied),
        "a canonicalized role must actually match the gate it configures");
    // `clamped()` applies the same canonicalization to a hand-edited/persisted list.
    let g = Guardrails { compact_nudge_roles: vec!["Orchestrator".into()], ..rails() }.clamped();
    assert_eq!(g.compact_nudge_roles, vec!["orchestrator".to_string()]);
}

#[test]
fn compact_nudge_and_idle_tick_both_rearm_in_the_combined_configuration() {
    // rev-24 review: the primary configuration the feature is FOR — autonomous
    // idle-tick AND compact-nudge both watching the same orchestrator — was
    // never exercised together. An earlier revision had both ticks rebaseline
    // the SAME pty-output counter, so whichever tick polled a burst first
    // consumed it and the other's activity detection (and, transitively, its
    // own anti-nag latch) never saw it: compact-nudge fired at most once per
    // pane lifetime. This drives BOTH ticks across one burst and pins that
    // BOTH independently re-arm afterward.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", compact_rails(5, &["orchestrator"])).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    reg.set_autonomous(&g.id, true).unwrap();
    let empty = HashMap::new();

    // Both earn their first fire off the initial (spawn-time) quiet clock.
    assert_eq!(reg.idle_tick_tick(FAR, &empty, &empty), vec![o.id.clone()], "idle-tick's first tick");
    assert_eq!(reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()), vec![o.id.clone()], "compact-nudge's first nudge");

    // A real burst: idle-tick observes it first (mirrors `lib.rs`'s setup
    // order — `start_idle_tick` is registered before `start_compact_nudge`).
    let grew: HashMap<String, u64> = [(o.id.clone(), 100_000u64)].into_iter().collect();
    assert!(reg.idle_tick_tick(FAR, &grew, &empty).is_empty(), "idle-tick sees the burst, no re-fire this tick");
    assert!(reg.compact_nudge_tick(FAR, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty(),
        "compact-nudge independently sees the SAME burst — not idle-tick's leftovers");

    // Quiet again shortly after: the first arm resolves (trusted, busy-then-
    // quiet) into the delivery-confirmation phase — `compact_pending` still
    // gates the next fire until that delivery confirms (rev-42 delta,
    // round 2), independently of idle-tick's own re-arm above.
    assert!(reg.compact_nudge_tick(FAR + 1_000, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty(),
        "resolving into the confirmation-wait phase is not itself a fire");
    let confirmed = confirmed_delivery(&o.id, FAR + 1_000);
    assert!(reg.compact_nudge_tick(FAR + 2_000, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &confirmed).is_empty());
    assert!(!reg.agent(&o.id).unwrap().compact_pending, "confirmed delivery releases the latch");

    // A fresh quiet window after the burst (both windows default to 5 min) —
    // BOTH must earn a second fire. Under the shared-counter bug this is the
    // genuine red: compact-nudge's latch was never cleared by the burst above
    // (idle-tick had already rebaselined the shared counter to the same
    // value), so it stayed latched and never fired again.
    let later = FAR + 5 * 60_000 + 1;
    assert_eq!(reg.idle_tick_tick(later, &grew, &empty), vec![o.id.clone()], "idle-tick re-arms after the burst");
    assert_eq!(reg.compact_nudge_tick(later, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()), vec![o.id.clone()],
        "compact-nudge must ALSO re-arm after the burst, independently of idle-tick");
}

// ---------- compact-nudge expansion: request_compact, checklist warning, ----
// ---------- context escalation, mandatory re-injection (#328) --------------

#[test]
fn compact_request_should_fire_bypasses_the_minutes_threshold_but_not_the_cap() {
    let none: Vec<u64> = vec![];
    assert!(compact_request_should_fire(true, &none, 1_000_000, 4), "quiet + under cap fires immediately");
    assert!(!compact_request_should_fire(false, &none, 1_000_000, 4), "busy this tick never fires");
    let at_cap = vec![100, 200, 300, 400];
    assert!(!compact_request_should_fire(true, &at_cap, 1_000_000, 4), "the shared per-hour cap still applies");
    assert!(compact_request_should_fire(true, &at_cap, 1_000_000, 0), "cap 0 = uncapped");
}

#[test]
fn context_percent_used_clamps_and_rounds_down() {
    assert_eq!(context_percent_used(0, 200_000), 0);
    assert_eq!(context_percent_used(100_000, 200_000), 50);
    assert_eq!(context_percent_used(199_999, 200_000), 99, "rounds down, never overstates");
    assert_eq!(context_percent_used(200_000, 200_000), 100);
    assert_eq!(context_percent_used(999_999, 200_000), 100, "over-window clamps to 100, never overflows past it");
    assert_eq!(context_percent_used(50_000, 0), 0, "a zero window never divides by zero");
}

#[test]
fn compact_escalation_should_fire_latches_and_respects_threshold_off() {
    assert!(!compact_escalation_should_fire(90, 0, false), "threshold 0 = escalation disabled entirely");
    assert!(!compact_escalation_should_fire(50, 80, false), "under threshold — purely opportunistic");
    assert!(compact_escalation_should_fire(80, 80, false), "exactly at threshold escalates");
    assert!(compact_escalation_should_fire(95, 80, false));
    assert!(!compact_escalation_should_fire(95, 80, true), "already notified — one escalation per crossing");
}

#[test]
fn compact_escalation_notice_names_the_percent_and_the_recovery_move() {
    let n = compact_escalation_notice(87);
    assert!(n.starts_with("[orrerix]"));
    assert!(n.contains("87%"), "got: {n}");
    assert!(n.contains("request_compact"), "got: {n}");
}

// ---------- compact-nudge min-context floor (benchtest finding + smart default) ----------

#[test]
fn compact_nudge_context_floor_met_unset_applies_the_smart_default_only_when_nudge_is_on() {
    // None (unset) + parent feature ON: the 50% smart default applies —
    // zero config needed to get the fix a live benchtest showed was missing.
    assert!(!compact_nudge_context_floor_met(Some(30), None, 20), "30% is under the smart default (50%)");
    assert!(compact_nudge_context_floor_met(Some(60), None, 20), "60% clears the smart default (50%)");
    // None (unset) + parent feature OFF: inert — there's nothing to gate
    // either way (the heuristic itself never fires when compact_nudge_minutes
    // is 0), but the function must still read as "met" on its own terms.
    assert!(compact_nudge_context_floor_met(Some(5), None, 0));
}

#[test]
fn compact_nudge_context_floor_met_explicit_zero_disables_regardless_of_parent() {
    assert!(compact_nudge_context_floor_met(Some(1), Some(0), 20), "explicit Some(0) = disabled, any reading passes");
    assert!(compact_nudge_context_floor_met(None, Some(0), 20));
}

#[test]
fn compact_nudge_context_floor_met_explicit_value_gates_at_that_value() {
    assert!(!compact_nudge_context_floor_met(Some(30), Some(70), 20), "below an explicit 70% floor");
    assert!(compact_nudge_context_floor_met(Some(70), Some(70), 20), "exactly at an explicit floor allows it");
    assert!(compact_nudge_context_floor_met(Some(80), Some(70), 20));
}

#[test]
fn compact_nudge_context_floor_met_fails_open_with_no_reading() {
    // A missing/stale context reading must never silently disable the whole
    // heuristic nudge — degrade, don't deny (the same posture #332's intake
    // gate takes on a `gh` failure) — true under every floor state.
    assert!(compact_nudge_context_floor_met(None, None, 20), "smart default, no reading");
    assert!(compact_nudge_context_floor_met(None, Some(70), 20), "explicit floor, no reading");
}

#[test]
fn compact_nudge_heuristic_fire_gated_by_the_smart_default_with_zero_config() {
    // The whole point of the smart default: enabling ONLY compact_nudge_minutes
    // (no min-context field touched at all — `..rails()`'s derived None) must
    // already reproduce the fix the benchtest needed.
    let (reg, _d) = test_registry();
    let g = reg
        .create_group(
            "C:/tmp/repo",
            Guardrails {
                compact_nudge_minutes: 20,
                compact_nudge_roles: vec!["orchestrator".to_string()],
                // compact_nudge_min_context_percent deliberately untouched — None.
                ..rails()
            },
        )
        .unwrap();
    assert_eq!(reg.group(&g.id).unwrap().guardrails.compact_nudge_min_context_percent, None,
        "sanity: nothing configured it — still unset after clamped()");
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let empty = HashMap::new();

    // Below the smart default (50%): the lull alone must not fire.
    let low_context: HashMap<String, u32> = [(o.id.clone(), 30u32)].into_iter().collect();
    assert!(
        reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &low_context, &HashMap::new(), &HashMap::new(), &HashMap::new())
            .is_empty(),
        "context 30% is under the smart 50% default — the heuristic nudge must not fire, with NO config"
    );
    assert_eq!(audit_count(&reg, &g.id, "compact-nudge"), 0);

    // Above the smart default: fires normally.
    let high_context: HashMap<String, u32> = [(o.id.clone(), 60u32)].into_iter().collect();
    assert_eq!(
        reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &high_context, &HashMap::new(), &HashMap::new(), &HashMap::new()),
        vec![o.id.clone()],
        "context 60% clears the smart 50% default — the heuristic nudge fires"
    );
}

#[test]
fn compact_nudge_heuristic_fire_gated_by_an_explicit_min_context_percent() {
    let (reg, _d) = test_registry();
    let g = reg
        .create_group(
            "C:/tmp/repo",
            Guardrails {
                compact_nudge_minutes: 20,
                compact_nudge_roles: vec!["orchestrator".to_string()],
                compact_nudge_min_context_percent: Some(70),
                ..rails()
            },
        )
        .unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let empty = HashMap::new();
    let mid_context: HashMap<String, u32> = [(o.id.clone(), 60u32)].into_iter().collect();
    assert!(
        reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &mid_context, &HashMap::new(), &HashMap::new(), &HashMap::new())
            .is_empty(),
        "60% clears the smart default but not an explicit 70% floor"
    );
    let high_context: HashMap<String, u32> = [(o.id.clone(), 75u32)].into_iter().collect();
    assert_eq!(
        reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &high_context, &HashMap::new(), &HashMap::new(), &HashMap::new()),
        vec![o.id.clone()]
    );
}

#[test]
fn compact_nudge_heuristic_fire_not_gated_when_explicitly_disabled() {
    let (reg, _d) = test_registry();
    let g = reg
        .create_group(
            "C:/tmp/repo",
            Guardrails {
                compact_nudge_minutes: 20,
                compact_nudge_roles: vec!["orchestrator".to_string()],
                compact_nudge_min_context_percent: Some(0), // explicit opt-out
                ..rails()
            },
        )
        .unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let empty = HashMap::new();
    let low_context: HashMap<String, u32> = [(o.id.clone(), 5u32)].into_iter().collect();
    assert_eq!(
        reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &low_context, &HashMap::new(), &HashMap::new(), &HashMap::new()),
        vec![o.id.clone()],
        "explicit Some(0) must restore the pre-smart-default behavior — fire on the lull alone"
    );
}

#[test]
fn compact_nudge_request_compact_fires_below_the_smart_default_agent_judgment_wins() {
    // The floor gates loomux's OWN unprompted (lull-timer) judgment only — an
    // agent that explicitly asked via `request_compact` is always honored,
    // regardless of context%, even under the zero-config smart default.
    let (reg, _d) = test_registry();
    let g = reg
        .create_group(
            "C:/tmp/repo",
            Guardrails {
                compact_nudge_minutes: 20,
                compact_nudge_roles: vec!["orchestrator".to_string()],
                ..rails()
            },
        )
        .unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    reg.request_compact(&o.id).unwrap();

    let empty = HashMap::new();
    let low_context: HashMap<String, u32> = [(o.id.clone(), 10u32)].into_iter().collect();
    assert_eq!(
        reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &low_context, &HashMap::new(), &HashMap::new(), &HashMap::new()),
        vec![o.id.clone()],
        "an agent-requested compact must fire even at 10% context — the floor never gates request_compact"
    );
}

#[test]
fn compact_nudge_min_context_percent_stays_unset_when_the_parent_heuristic_is_off() {
    // None + compact_nudge_minutes == 0: nothing to gate, and clamped() must
    // NOT resolve the smart default just because the field is unset — the
    // resolution is deliberately deferred to gate-evaluation time.
    let g = Guardrails { compact_nudge_minutes: 0, ..rails() }.clamped();
    assert_eq!(g.compact_nudge_min_context_percent, None);
}

#[test]
fn compact_nudge_min_context_percent_explicit_values_survive_clamping() {
    let g = Guardrails { compact_nudge_min_context_percent: Some(0), ..rails() }.clamped();
    assert_eq!(g.compact_nudge_min_context_percent, Some(0), "an explicit 0 (opt-out) must survive, not become None");
    let g = Guardrails { compact_nudge_min_context_percent: Some(250), ..rails() }.clamped();
    assert_eq!(g.compact_nudge_min_context_percent, Some(100), "clamps to the 100% ceiling");
    let g = Guardrails { compact_nudge_min_context_percent: None, ..rails() }.clamped();
    assert_eq!(g.compact_nudge_min_context_percent, None, "unset stays unset — clamped() never resolves it");
}

#[test]
fn compact_checklist_warning_is_recency_gated_never_blocking() {
    assert!(compact_checklist_warning(0, 1_000_000, 900_000).is_some(), "never observed = warn");
    assert!(compact_checklist_warning(100_000, 1_000_000, 900_000).is_some(), "stale (at the window edge) = warn");
    assert!(compact_checklist_warning(950_000, 1_000_000, 900_000).is_none(), "recent = no warning");
}

#[test]
fn human_typed_compact_detected_matches_standalone_tokens_only() {
    assert!(human_typed_compact_detected("> /compact\nCompacting conversation..."));
    assert!(human_typed_compact_detected("/compact"));
    assert!(!human_typed_compact_detected("editing src/compact_nudge.rs"), "embedded in a path/word must not match");
    assert!(!human_typed_compact_detected("I'll call request_compact for you"), "a different token entirely");
    assert!(!human_typed_compact_detected(""));
}

#[test]
fn compact_reinjection_notice_is_slim_when_the_contract_rides_the_system_layer() {
    // #417 correction round 5: the common case (`ReinjectShape::Slim`) —
    // the block's contract already rides the CLI's own system-prompt layer
    // (#416), so nothing about it needs re-embedding after a compaction.
    let n = compact_reinjection_notice(&ReinjectShape::Slim, "C:/g/worker.md", "C:/g/ledger-w-1.log", None);
    assert!(n.starts_with("[orrerix]"));
    assert!(n.contains("list_tasks") && n.contains("get_state") && n.contains("list_agents"), "got: {n}");
    assert!(n.contains("C:/g/ledger-w-1.log"), "the ledger path is still named as a pointer, got: {n}");
    assert!(!n.contains("You are"), "must never embed instructions/persona prose when there's nothing to embed, got: {n}");
    assert!(n.len() < 700, "the slim notice should be short — got {} bytes: {n}", n.len());
}

#[test]
fn compact_reinjection_notice_slim_still_inlines_the_ledger_tail() {
    // Belt-and-braces: even in the slim shape, the ledger TAIL is still
    // inlined directly (highest value-per-byte of anything left to resend),
    // not reduced to a bare pointer like the rest of the notice.
    let n = compact_reinjection_notice(
        &ReinjectShape::Slim,
        "C:/g/worker.md",
        "C:/g/ledger-w-1.log",
        Some("Your directive ledger:\n[1] scope to auth only"),
    );
    assert!(n.contains("scope to auth only"), "got: {n}");
}

#[test]
fn compact_reinjection_notice_is_a_pointer_when_only_the_core_rides_the_system_layer() {
    // rev-16 review (N2), round 8: the THIRD shape — Copilot's generated-
    // wrapper happy path. Must name the instructions path and the live-
    // state re-sync steps, but must NEVER embed the instructions body
    // itself (that would re-spend exactly the tokens a compaction is
    // supposed to reclaim, which is the whole reason this shape exists
    // instead of collapsing into `Verbose`).
    let n = compact_reinjection_notice(&ReinjectShape::Pointer, "C:/g/orchestrator.md", "C:/g/ledger-o-1.log", None);
    assert!(n.starts_with("[orrerix]"));
    assert!(n.contains("C:/g/orchestrator.md"), "must name the instructions path to re-read: {n}");
    assert!(n.contains("re-read"), "must instruct the agent to actually go read it: {n}");
    assert!(n.contains("list_tasks") && n.contains("get_state") && n.contains("list_agents"), "got: {n}");
    assert!(n.contains("C:/g/ledger-o-1.log"), "the ledger path is still named as a pointer, got: {n}");
    assert!(n.len() < 700, "the pointer notice should stay short — it does NOT embed the instructions body: got {} bytes: {n}", n.len());
}

#[test]
fn compact_reinjection_notice_pointer_still_inlines_the_ledger_tail() {
    let n = compact_reinjection_notice(
        &ReinjectShape::Pointer,
        "C:/g/orchestrator.md",
        "C:/g/ledger-o-1.log",
        Some("Your directive ledger:\n[1] scope to auth only"),
    );
    assert!(n.contains("scope to auth only"), "got: {n}");
}

#[test]
fn compact_reinjection_notice_embeds_the_instructions_verbatim_when_the_contract_is_not_durable() {
    // The true fallback (`ReinjectShape::Verbose`) — a Copilot block on a
    // user-authored native persona, the `~/.copilot/agents`-unwritable
    // fallback, or an over-cap generated body — still gets the full
    // verbose embedding, since there is no system-prompt-layer copy of
    // ANYTHING loomux authored to trust instead.
    let instructions = "You are the orchestrator...\nNever merge without a gate.";
    let n = compact_reinjection_notice(&ReinjectShape::Verbose(instructions.to_string()), "C:/g/orchestrator.md", "C:/g/ledger-o-1.log", None);
    assert!(n.starts_with("[orrerix]"));
    assert!(n.contains(instructions), "must embed the FULL instructions text, not a pointer to go read it");
    assert!(n.contains("list_tasks") && n.contains("get_state") && n.contains("list_agents"), "got: {n}");
    assert!(!n.contains("directive ledger"), "None must embed nothing — no ledger header for an agent that never used it");
}

#[test]
fn reinject_shape_on_system_layer_full_never_touches_the_filesystem() {
    // The common (Claude) case: a path that doesn't exist would be an
    // `Err` if this ever actually read it — the short-circuit must return
    // `Slim` without trying.
    assert_eq!(
        reinject_shape(Path::new("C:/does/not/exist.md"), ContractCarrier::SystemLayerFull),
        ReinjectShape::Slim
    );
}

#[test]
fn reinject_shape_system_layer_core_never_touches_the_filesystem_either() {
    // The pointer shape doesn't need the file's CONTENTS, only its path
    // (already known to the caller) — no read, same short-circuit as Full.
    assert_eq!(
        reinject_shape(Path::new("C:/does/not/exist.md"), ContractCarrier::SystemLayerCore),
        ReinjectShape::Pointer
    );
}

#[test]
fn reinject_shape_reads_the_file_when_nothing_is_durable() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("worker.md");
    fs::write(&p, "You are a worker...\nNever merge without a gate.").unwrap();
    assert_eq!(
        reinject_shape(&p, ContractCarrier::KickoffOnly),
        ReinjectShape::Verbose("You are a worker...\nNever merge without a gate.".to_string())
    );
}

#[test]
fn reinject_shape_degrades_to_pointer_not_slim_when_unreadable_or_empty() {
    // rev-10 review (N3), round 7; widened to a real three-way choice by
    // rev-16 review (N2), round 8: this is the fix itself, isolated — a
    // missing file (e.g. the #423 sweep reclaiming a block's file out from
    // under a still-live agent) and an empty-but-present file must BOTH
    // degrade to `Pointer`, never `Slim` (which would falsely claim this
    // `KickoffOnly` agent's system prompt already holds the contract) and
    // never `Verbose("")` (a verbose notice with nothing embedded under
    // it, which used to happen silently via `unwrap_or_default()`).
    let d = tempfile::tempdir().unwrap();
    assert_eq!(
        reinject_shape(&d.path().join("gone.md"), ContractCarrier::KickoffOnly),
        ReinjectShape::Pointer,
        "a missing file must not become Verbose(\"\")"
    );
    let empty = d.path().join("empty.md");
    fs::write(&empty, "").unwrap();
    assert_eq!(
        reinject_shape(&empty, ContractCarrier::KickoffOnly),
        ReinjectShape::Pointer,
        "an empty file must not become Verbose(\"\")"
    );
    let whitespace_only = d.path().join("whitespace.md");
    fs::write(&whitespace_only, "   \n\n  ").unwrap();
    assert_eq!(
        reinject_shape(&whitespace_only, ContractCarrier::KickoffOnly),
        ReinjectShape::Pointer,
        "whitespace-only must not read as real content"
    );
}

#[test]
fn compact_reinjection_notice_verbose_folds_in_the_ledger_when_present() {
    let instructions = "You are a worker...";
    let n = compact_reinjection_notice(
        &ReinjectShape::Verbose(instructions.to_string()),
        "C:/g/worker.md",
        "C:/g/ledger-w-1.log",
        Some("Your directive ledger:\n[1] scope to auth only"),
    );
    assert!(n.contains(instructions), "instructions still embedded");
    assert!(n.contains("scope to auth only"), "got: {n}");
    // The ledger must land AFTER the instructions and BEFORE the re-sync
    // line — re-grounding first, then the diary, then the live-state pointer.
    let ins_pos = n.find(instructions).unwrap();
    let ledger_pos = n.find("scope to auth only").unwrap();
    let resync_pos = n.find("Now re-sync live state").unwrap();
    assert!(ins_pos < ledger_pos && ledger_pos < resync_pos, "got: {n}");
}

#[test]
fn directive_ledger_embed_is_none_for_empty_or_whitespace_only() {
    assert!(directive_ledger_embed("", 2048, "C:/g/ledger-w-1.log").is_none());
    assert!(directive_ledger_embed("   \n  \n", 2048, "C:/g/ledger-w-1.log").is_none());
}

#[test]
fn directive_ledger_embed_keeps_the_tail_and_states_truncation() {
    // Ten one-line entries, cap tight enough that only the last few fit.
    let ledger: String = (1..=10).map(|i| format!("[{i}] entry number {i}\n")).collect();
    let cap = 40; // enough for ~2 short lines, not all 10
    let embed = directive_ledger_embed(&ledger, cap, "C:/g/ledger-w-1.log").unwrap();
    assert!(embed.contains("entry number 10"), "must keep the NEWEST entry: {embed}");
    assert!(!embed.contains("entry number 1\n"), "oldest entry must be the one dropped: {embed}");
    assert!(embed.contains("full history at C:/g/ledger-w-1.log"), "truncation must name the file: {embed}");
    assert!(embed.contains("most recent"), "truncation must be stated, not silent: {embed}");
}

#[test]
fn resume_kickoff_notice_embeds_the_ledger_and_reproduces_the_old_string_without_one() {
    // #411: the orchestration-restore kickoff didn't embed the directive
    // ledger, unlike the post-compact reinjection notice — an app restart is
    // the other surprise discontinuity the ledger exists to survive.
    assert_eq!(
        resume_kickoff_notice(None),
        "[orrerix] Orchestration restored: your MCP tools, the task board, and the audit log are \
         live again in this session. Re-sync now: list_tasks, list_agents, get_state. Your \
         previous worker panes are gone; resume a task session with spawn_agent(resume_session, \
         cwd) when follow-ups need it. Then give the human a short status summary.",
        "no ledger ⇒ byte-identical to the pre-#411 fixed string"
    );
    let embed = directive_ledger_embed("[1] scope: only touch the auth module", 2048, "C:/g/ledger-orch.log").unwrap();
    let notice = resume_kickoff_notice(Some(&embed));
    assert!(notice.contains("only touch the auth module"), "{notice}");
    assert!(notice.starts_with("[orrerix] Orchestration restored"), "the base notice is unchanged: {notice}");
}

#[test]
fn directive_ledger_embed_never_drops_the_single_newest_entry_even_over_cap() {
    let ledger = "a much longer single directive entry than the tiny cap below allows for";
    let embed = directive_ledger_embed(ledger, 8, "C:/g/ledger-w-1.log").unwrap();
    assert!(embed.contains(ledger), "the newest entry is never silently dropped for being long: {embed}");
}

#[test]
fn directive_ledger_embed_omits_truncation_wording_when_everything_fits() {
    let ledger = "[1] only one short entry";
    let embed = directive_ledger_embed(ledger, 2048, "C:/g/ledger-w-1.log").unwrap();
    assert!(embed.contains(ledger));
    assert!(!embed.contains("most recent"), "no truncation language when nothing was cut: {embed}");
    assert!(!embed.contains("full history at"), "no file pointer needed when nothing was cut: {embed}");
}

#[test]
fn auto_compact_banner_detected_matches_claudes_stable_substring_only() {
    assert!(auto_compact_banner_detected("claude", "✢ Compacting conversation… (esc to interrupt · 8s · ↓ 172 tokens)"));
    assert!(auto_compact_banner_detected("claude", "Compacting conversation"));
    assert!(!auto_compact_banner_detected("claude", "editing src/compact_nudge.rs"), "unrelated pane text must not match");
    assert!(!auto_compact_banner_detected("claude", ""));
    assert!(!auto_compact_banner_detected("copilot", "Compacting conversation"), "no known banner for copilot yet — never guessed");
}

#[test]
fn copilot_compaction_marker_detected_matches_either_stable_sentence_only() {
    // #428 (round 9): the completion-side counterpart. Text quoted
    // directly from the issue's own report, not reconstructed.
    assert!(copilot_compaction_marker_detected("copilot", "Compaction completed"));
    assert!(copilot_compaction_marker_detected(
        "copilot",
        "A new checkpoint has been added to your session."
    ));
    assert!(
        !copilot_compaction_marker_detected("copilot", "Use /session checkpoints 3 to view the compaction summary."),
        "the checkpoint-number fragment is deliberately not matched — it's never stable text"
    );
    assert!(!copilot_compaction_marker_detected("copilot", "editing src/compact_nudge.rs"), "unrelated pane text must not match");
    assert!(!copilot_compaction_marker_detected("copilot", ""));
    assert!(
        !copilot_compaction_marker_detected("claude", "Compaction completed"),
        "no known completion marker for claude — it has SessionStart instead, never guessed"
    );
}

#[test]
fn compact_nudge_poll_interval_is_fast_only_while_something_is_pending() {
    // Round 10 (#428 follow-up, user-directed): a live re-test showed the
    // badge sitting in "awaiting evidence" limbo for up to a full poll cycle
    // even after a hook had already confirmed the outcome — the poll
    // thread's own cadence was the bottleneck, not the state machine. This
    // is the pure decision `start_compact_nudge`'s loop makes every
    // iteration, both directions pinned.
    assert_eq!(compact_nudge_poll_interval(true), Duration::from_secs(10));
    assert_eq!(compact_nudge_poll_interval(false), Duration::from_secs(60));
}

#[test]
fn any_compact_pending_is_true_only_while_an_arm_is_actually_open() {
    // The single input `compact_nudge_poll_interval` needs — proven against
    // a real registry, not just the pure function in isolation: opens the
    // moment an arm does (including through the whole delivery-confirmation
    // phase, which still benefits from fast polling), closes the moment the
    // last open cycle actually resolves.
    let (reg, _d, gid, oid) = compact_nudge_setup_copilot(20);
    assert!(!reg.any_compact_pending(), "nothing armed yet");

    let started_ms = reg.agent(&oid).unwrap().started_ms;
    let marker = reg.state_root().join(gid.as_str()).join("hooks").join(format!("{oid}.precompact.json"));
    write_hook_marker(&marker, started_ms, 1_000);
    let empty = HashMap::new();
    assert!(reg.compact_nudge_tick(1_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    assert!(reg.any_compact_pending(), "the precompact arm just opened (still true through the confirmation phase)");

    let confirmed = confirmed_delivery(&oid, 2_000);
    assert!(reg.compact_nudge_tick(2_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &confirmed).is_empty());
    assert!(!reg.any_compact_pending(), "resolved and confirmed — nothing left open, fast polling has no reason to continue");
}

#[test]
fn compaction_confirmed_requires_a_real_drop_and_fails_closed_without_evidence() {
    // Production bug fix (D2): busy-then-quiet alone is not evidence a real
    // compaction ran — this is the confirmation gate that closes that gap.
    assert!(compaction_confirmed(Some(100_000), Some(5_000)), "a dramatic drop is confirmed");
    assert!(compaction_confirmed(Some(100_000), Some(69_000)), "just under the ratio threshold confirms");
    assert!(!compaction_confirmed(Some(100_000), Some(70_001)), "just over the ratio threshold does not confirm");
    assert!(!compaction_confirmed(Some(100_000), Some(100_000)), "no change at all is not evidence of anything");
    assert!(!compaction_confirmed(Some(50_000), Some(80_000)), "growth is the opposite of evidence");
    // Fails CLOSED: no reading available in either direction must never be
    // read as "trust it anyway" — a missed reinjection is acceptable, an
    // ungrounded one is the production incident this function exists to stop.
    assert!(!compaction_confirmed(None, Some(5_000)), "no baseline captured — never a guess");
    assert!(!compaction_confirmed(Some(100_000), None), "no current reading available — never a guess");
    assert!(!compaction_confirmed(None, None), "neither reading available — never a guess");
    assert!(!compaction_confirmed(Some(0), Some(0)), "a zero baseline has nothing meaningful to compare against");
}

#[test]
fn compaction_status_narrates_every_real_state_machine_phase() {
    // Lifecycle-panel surfacing (PR #329 round 6): pure derivation from
    // already-tracked state — pin each real phase maps to the right variant,
    // never a phase the state machine can't actually be in.
    assert_eq!(
        compaction_status(false, false, false, None, 0, None, None, 1_000, None, None, None),
        CompactionStatus::None,
        "no arm, no reinjection, no lost outcome"
    );
    assert_eq!(
        compaction_status(true, true, false, None, 0, None, None, 1_000, None, None, None),
        CompactionStatus::Armed { trusted: true, source: None },
        "pending, not yet observed busy — trusted arm"
    );
    assert_eq!(
        compaction_status(true, false, false, None, 0, None, None, 1_000, None, None, None),
        CompactionStatus::Armed { trusted: false, source: None },
        "pending, not yet observed busy — inference arm"
    );
    assert_eq!(
        compaction_status(true, false, true, None, 0, None, None, 1_000, None, None, None),
        CompactionStatus::AwaitingEvidence { trusted: false, source: None },
        "pending, busy observed — waiting on quiet to resolve"
    );
    assert_eq!(
        // #417: a hook-armed pending is ALSO trusted (no inference gate), but
        // carries `source: Some("hook")` so the panel can distinguish it from
        // the loomux-initiated trusted arm above.
        compaction_status(true, true, false, None, 0, None, None, 1_000, Some("hook"), None, None),
        CompactionStatus::Armed { trusted: true, source: Some("hook") },
        "pending, not yet observed busy — hook-confirmed arm"
    );
    assert_eq!(
        compaction_status(true, true, false, Some(900), 2, None, None, 1_000, None, None, None),
        CompactionStatus::Reinjecting { attempt: 2, max_attempts: 3 },
        "reinject_attempted_ms set takes priority over the arm phase"
    );
    assert_eq!(
        compaction_status(false, false, false, None, 0, Some("arm-timeout"), Some(500), 1_000, None, None, None),
        CompactionStatus::Abandoned { reason: "arm-timeout".to_string(), since_ms: 500 },
        "a recent lost outcome, no active arm"
    );
    assert_eq!(
        compaction_status(false, false, false, None, 0, Some("arm-timeout"), Some(500), 500 + 10 * 60 * 1000, None, None, None),
        CompactionStatus::None,
        "a lost outcome outside the recency window reads as none, not a stale problem"
    );
}

#[test]
fn a_resolved_re_grounding_surfaces_which_evidence_closed_it() {
    // #546. The phase resolves on one of two signals that are NOT equally
    // strong: `Delivered` is loomux watching its own Enter land; `LivenessOnly`
    // is the agent's own process reaching loomux afterwards, which proves it is
    // alive and executing — never that it READ the re-grounding, and not even
    // that our paste arrived. The uncovered case #546 names is a genuinely lost
    // paste on an agent that is busy for some other reason: it resolves this
    // way, the agent carries on without the contract, and nothing says so.
    // Before #588 the distinction existed only inside an audit line, so the
    // panel a human actually watches could not show it.
    assert_eq!(
        compaction_status(false, false, false, None, 0, None, None, 1_000, None, Some(ReinjectAck::LivenessOnly), Some(600)),
        CompactionStatus::Resolved { evidence: ReinjectAck::LivenessOnly, since_ms: 600 },
        "a recently-resolved re-grounding is a real state, carrying the evidence that closed it"
    );
    assert_eq!(
        compaction_status(false, false, false, None, 0, None, None, 1_000, None, Some(ReinjectAck::Delivered), Some(600)),
        CompactionStatus::Resolved { evidence: ReinjectAck::Delivered, since_ms: 600 },
        "the stronger evidence must be distinguishable from the weaker one"
    );
    // Same recency rule as `Abandoned` — an old resolution is not today's news.
    assert_eq!(
        compaction_status(
            false, false, false, None, 0, None, None, 600 + 10 * 60 * 1000, None,
            Some(ReinjectAck::LivenessOnly), Some(600),
        ),
        CompactionStatus::None,
        "a resolution outside the recency window reads as none, not as a lingering claim"
    );
    // A live arm still outranks both: a NEW compaction in progress is what the
    // panel must show, not the outcome of the last one.
    assert_eq!(
        compaction_status(true, true, false, None, 0, None, None, 1_000, None, Some(ReinjectAck::LivenessOnly), Some(600)),
        CompactionStatus::Armed { trusted: true, source: None },
        "a fresh arm is not hidden behind the previous cycle's resolution"
    );
}

#[test]
fn every_reinject_ack_states_what_it_proves_and_what_it_does_not() {
    // #546's honest-labeling contract, pinned at the one place the vocabulary
    // lives. The finding was that a claim ("confirmed", "acked") outran the
    // evidence behind it in three surfaces at once, each having re-derived the
    // wording for itself. This is the guard against the fourth.

    // The action name IS the claim, and the two must not share one.
    assert_eq!(ReinjectAck::Delivered.audit_action(), "compact-reinjection-confirmed");
    assert_eq!(ReinjectAck::LivenessOnly.audit_action(), "compact-reinjection-liveness-only");
    assert_ne!(
        ReinjectAck::Delivered.audit_action(),
        ReinjectAck::LivenessOnly.audit_action(),
        "one action covering both means anyone counting confirmations in audit.jsonl counts \
         liveness closes among them — #546's finding in the surface that outlives the badge"
    );
    assert!(
        !ReinjectAck::LivenessOnly.audit_action().contains("confirmed"),
        "a liveness close confirmed nothing; the action name must not say it did"
    );

    // The wire values are UNCHANGED from #535/#588 — they name the evidence
    // source, which was always accurate. Only the claims around them moved.
    // Pinned so a future rename here doesn't silently break the badge's
    // `evidence` union or an operator's saved audit query.
    assert_eq!(ReinjectAck::Delivered.wire(), "delivery");
    assert_eq!(ReinjectAck::LivenessOnly.wire(), "activity");

    // Both arms owe a residual. The stronger one's is the easy one to forget:
    // watching our own Enter land proves the text reached the box, and nothing
    // loomux can observe proves the agent then read it.
    for ack in [ReinjectAck::Delivered, ReinjectAck::LivenessOnly] {
        assert!(!ack.proves().is_empty());
        assert!(
            ack.does_not_prove().contains("read"),
            "neither signal proves the re-grounding was read, and the record must say so: {ack:?}"
        );
    }
    assert!(
        ReinjectAck::LivenessOnly.does_not_prove().contains("delivered"),
        "liveness says nothing about our paste either — the larger residual #546 filed"
    );

    // The stronger evidence wins when both are in hand: a resolve that HAD a
    // delivery confirmation must never be recorded as the weaker close.
    assert_eq!(ReinjectAck::from_evidence(true), ReinjectAck::Delivered);
    assert_eq!(ReinjectAck::from_evidence(false), ReinjectAck::LivenessOnly);
}

#[test]
fn a_fresh_loss_is_never_hidden_behind_an_older_resolution() {
    // An agent that has compacted more than once carries a stamp for each
    // outcome, so the two recent-terminal states can both be in range. The
    // MORE RECENT wins rather than a fixed ranking: a fixed one would either
    // let a resolved re-grounding hide a fresh loss (unsafe) or let an old
    // loss hide today's resolution (misleading).
    let lost = |lost_ms, ack_ms| {
        compaction_status(
            false, false, false, None, 0, Some("reinjection-abandoned"), Some(lost_ms),
            10_000, None, Some(ReinjectAck::LivenessOnly), Some(ack_ms),
        )
    };
    assert_eq!(
        lost(9_000, 5_000),
        CompactionStatus::Abandoned { reason: "reinjection-abandoned".to_string(), since_ms: 9_000 },
        "a loss AFTER the last resolution is the current state — surfacing the stale one would \
         be a claim the state machine has already withdrawn"
    );
    assert_eq!(
        lost(5_000, 9_000),
        CompactionStatus::Resolved { evidence: ReinjectAck::LivenessOnly, since_ms: 9_000 },
        "and a resolution after an older loss is equally the current state"
    );
    // Ties go to the loss: it is the louder of the two and the only one that
    // asks a human for anything.
    assert_eq!(
        lost(7_000, 7_000),
        CompactionStatus::Abandoned { reason: "reinjection-abandoned".to_string(), since_ms: 7_000 }
    );
}

#[test]
fn auto_compact_banner_detected_is_position_anchored_to_the_last_line_only() {
    // rev review B1: a bare substring scan false-positives on any busy pane
    // that PRINTS or DISCUSSES the banner text rather than actually running
    // it — the growth from rendering the mention IS the growth a `!
    // currently_quiet` gate would see, so recency alone can't separate them.
    // The fix is positional: only the tail's LAST non-blank line counts,
    // since the real spinner is the live status line with nothing after it
    // while it runs, whereas a quoted mention sits in scrolled content with
    // more lines following it.

    // A `gh pr diff` hunk quoting the doc comment — banner text mid-diff,
    // more diff lines after it.
    let diff_tail = "\
diff --git a/src-tauri/src/orchestration/mod.rs b/src-tauri/src/orchestration/mod.rs
+/// Claude Code's own emergency auto-compact renders a spinner line while it
+/// runs, observed (1.0.x) as `Compacting conversation` — a leading spinner
+/// glyph and a trailing elapsed-time/token-count suffix.
+pub fn auto_compact_banner_detected(cli: &str, tail: &str) -> bool {";
    assert!(!auto_compact_banner_detected("claude", diff_tail), "a diff hunk quoting the string must not match: not the last line");

    // A rust string literal inside a code listing — banner text mid-file,
    // more code after it.
    let code_tail = "\
fn auto_compact_banner_substrings(cli: &str) -> &'static [&'static str] {
    match cli {
        \"claude\" => &[\"Compacting conversation\"],
        _ => &[],
    }
}";
    assert!(!auto_compact_banner_detected("claude", code_tail), "a string literal in a code listing must not match: not the last line");

    // Prose discussion of the feature — banner text mid-sentence, more
    // sentences after it.
    let prose_tail = "\
The detector matches the stable \"Compacting conversation\" substring.
This is expected to survive across CLI point releases even if the
spinner or counters' exact formatting doesn't change.";
    assert!(!auto_compact_banner_detected("claude", prose_tail), "prose discussing the string must not match: not the last line");

    // The real banner, as the tail's actual final line, with prior turn
    // output before it — this MUST still trigger.
    let real_tail = "\
Reading src-tauri/src/orchestration/mod.rs...
Found the compact-nudge tick function.
✢ Compacting conversation… (esc to interrupt · 12s · ↓ 340 tokens)";
    assert!(auto_compact_banner_detected("claude", real_tail), "the real banner as the last line must still trigger");

    // A trailing blank line (e.g. a stray newline from the tail's own ring)
    // must not hide a real banner sitting just above it.
    let real_tail_trailing_blank = "✢ Compacting conversation… (esc to interrupt · 3s · ↓ 40 tokens)\n\n";
    assert!(auto_compact_banner_detected("claude", real_tail_trailing_blank), "a trailing blank line must not mask the real last content line");
}

#[test]
fn request_compact_is_self_scoped_and_cli_gated() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "work", false, None).unwrap();
    let msg = reg.request_compact(&o.id).unwrap();
    assert!(msg.contains("compact requested"), "got: {msg}");
    assert!(reg.agent(&o.id).unwrap().compact_requested, "flag set on the CALLING agent");
    assert!(!reg.agent(&w.id).unwrap().compact_requested, "never set on another pane in the same group");
}

#[test]
fn note_directive_appends_are_self_scoped_and_isolated_per_agent() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "work", false, None).unwrap();

    reg.note_directive(&o.id, "scope to auth only", false).unwrap();
    reg.note_directive(&o.id, "second directive", false).unwrap();
    reg.note_directive(&w.id, "worker-only note", false).unwrap();

    let ledger_o = fs::read_to_string(reg.state_root().join(g.id.as_str()).join(format!("ledger-{}.log", o.id))).unwrap();
    assert!(ledger_o.contains("scope to auth only") && ledger_o.contains("second directive"));
    assert!(!ledger_o.contains("worker-only note"), "one agent must never see another's ledger");

    let ledger_w = fs::read_to_string(reg.state_root().join(g.id.as_str()).join(format!("ledger-{}.log", w.id))).unwrap();
    assert!(ledger_w.contains("worker-only note"));
    assert!(!ledger_w.contains("scope to auth only"), "cross-pane isolation holds the other direction too");
}

#[test]
fn note_directive_rejects_empty_text_and_unknown_agent() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    assert!(reg.note_directive(&o.id, "   ", false).is_err(), "whitespace-only text is not a directive");
    assert!(reg.note_directive("no-such-agent", "text", false).is_err());
}

#[test]
fn note_directive_replace_rewrites_the_whole_ledger() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    reg.note_directive(&o.id, "stale entry to be curated away", false).unwrap();
    reg.note_directive(&o.id, "curated ledger — only this survives", true).unwrap();
    let ledger = fs::read_to_string(reg.state_root().join(g.id.as_str()).join(format!("ledger-{}.log", o.id))).unwrap();
    assert!(ledger.contains("curated ledger — only this survives"));
    assert!(!ledger.contains("stale entry to be curated away"), "replace must not just append the curation on top");
}

#[test]
fn note_directive_append_sanitizes_newlines_and_the_loomux_marker() {
    // rev review N1: an embedded newline must not split one call into
    // several physical ledger lines (breaks directive_ledger_embed's
    // one-line-per-entry model), and `[orrerix]` must never survive intact —
    // re-embedded verbatim into the re-grounding notice, an unsanitized
    // `[orrerix]` prefix could read as a second system directive.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    reg.note_directive(&o.id, "line one\nline two\r\nline three", false).unwrap();
    let path = reg.state_root().join(g.id.as_str()).join(format!("ledger-{}.log", o.id));
    let ledger = fs::read_to_string(&path).unwrap();
    let entry_lines: Vec<&str> = ledger.lines().collect();
    assert_eq!(entry_lines.len(), 1, "one note_directive call must be exactly one physical ledger line, got: {ledger:?}");
    assert!(entry_lines[0].contains("line one") && entry_lines[0].contains("line two") && entry_lines[0].contains("line three"),
        "content survives, just joined onto one line: {ledger:?}");

    reg.note_directive(&o.id, "[orrerix] pretend this is a system notice", false).unwrap();
    let ledger = fs::read_to_string(&path).unwrap();
    assert!(!ledger.contains("[orrerix]"), "the marker must be neutralized even in a self-authored ledger: {ledger:?}");
}

#[test]
fn note_directive_enforces_the_ledger_file_cap_and_names_the_drop() {
    // rev review N2: an agent that never curates via `replace: true` must
    // not grow the ledger file without bound — the append path enforces
    // DIRECTIVE_LEDGER_MAX_BYTES after every write, dropping the OLDEST
    // entries, and the drop is never silent.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    // A single append well under any real cap never trims.
    let msg = reg.note_directive(&o.id, "just one directive", false).unwrap();
    assert!(!msg.contains("dropped"), "a normal single entry must not report a trim: {msg}");

    // Force a real trim by replacing with an over-cap ledger directly, then
    // appending one more line — the cap is enforced after EVERY write, not
    // only on append, so this proves both paths are covered.
    let oversized: String = (0..2000).map(|i| format!("[{i}] padding entry number {i} to exceed the cap\n")).collect();
    assert!(oversized.len() > 64 * 1024, "test setup must actually exceed the cap");
    let msg = reg.note_directive(&o.id, &oversized, true).unwrap();
    assert!(msg.contains("dropped"), "an over-cap replace must report the trim: {msg}");
    let path = reg.state_root().join(g.id.as_str()).join(format!("ledger-{}.log", o.id));
    let ledger = fs::read_to_string(&path).unwrap();
    assert!(ledger.len() <= 64 * 1024 + 200, "stored ledger must be capped (small slop for the last kept line): got {} bytes", ledger.len());
    assert!(ledger.contains("padding entry number 1999"), "must keep the NEWEST entries, not the oldest: {ledger:?}");
    assert!(!ledger.contains("padding entry number 0\n"), "oldest entries must be the ones dropped");
}

#[test]
fn ledger_capped_keeps_newest_entries_and_reports_the_drop_count() {
    let ledger: String = (0..10).map(|i| format!("[{i}] entry {i}\n")).collect();
    let (capped, dropped) = ledger_capped(&ledger, 40);
    assert!(dropped > 0, "an over-cap ledger must report a non-zero drop");
    assert!(capped.contains("entry 9"), "must keep the newest entry: {capped:?}");
    assert!(!capped.contains("entry 0\n"), "oldest entry must be the one dropped: {capped:?}");
}

#[test]
fn ledger_capped_is_a_no_op_under_cap() {
    let ledger = "[1] a short ledger\n[2] with two entries\n";
    let (capped, dropped) = ledger_capped(ledger, 4096);
    assert_eq!(dropped, 0);
    assert_eq!(capped, ledger, "under cap must return the content unchanged, not re-serialized");
}

#[test]
fn mcp_note_directive_dispatch_is_self_scoped() {
    let (reg, _d, co, cw) = setup_mcp();
    let out = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "note_directive", "arguments": { "text": "human said: focus on #42" } })).unwrap();
    assert_eq!(out["isError"], false);
    let text = out["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("recorded"), "got: {text}");
    let ledger_o = fs::read_to_string(reg.state_root().join(co.group.as_str()).join(format!("ledger-{}.log", co.agent_id))).unwrap();
    assert!(ledger_o.contains("human said: focus on #42"));

    // Self-scoped: the worker's own call must never land in the orchestrator's ledger.
    dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "note_directive", "arguments": { "text": "worker-only note" } })).unwrap();
    let ledger_o_after = fs::read_to_string(reg.state_root().join(co.group.as_str()).join(format!("ledger-{}.log", co.agent_id))).unwrap();
    assert!(!ledger_o_after.contains("worker-only note"), "cross-pane leak via dispatch");
}

#[test]
fn mcp_note_directive_requires_text() {
    let (reg, _d, co, _cw) = setup_mcp();
    let out = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "note_directive", "arguments": {} })).unwrap();
    assert_eq!(out["isError"], true, "missing text must error, not silently no-op");
}

// The previous version of this test used "copilot" as its non-Claude
// example CLI, proving `request_compact` rejected it — #417 correction
// round 2 made that claim wrong (Copilot has its own `/compact` too, see
// `compact_nudge_cli_supported`'s doc), so that assertion inverted to
// `request_compact_now_accepts_a_copilot_caller` above. There is no
// remaining way to construct this rejection through the public API: any
// group's `agent_cli` is coerced into `SUPPORTED_CLIS` by `clamped()`, and
// any per-role CLI override is rejected at `spawn_agent` time rather than
// reaching a live agent (see `clamped()`'s own doc, issue #4) — so a
// successfully spawned agent's CLI is always a `compact_nudge_cli_
// supported` member today. The gate itself (and its "codex"/""-return-false
// cases) stays covered directly at the pure-function level by
// `compact_nudge_cli_gate_covers_claude_and_copilot` above; `request_
// compact`'s own `if !compact_nudge_cli_supported` branch is defensive
// belt-and-braces against a hand-edited or pre-#4 persisted group.json, not
// something a live test can trigger without bypassing the public API.

#[test]
fn request_compact_now_accepts_a_copilot_caller() {
    // #417 correction round 2: Copilot's own `/compact [FOCUS-INSTRUCTIONS]`
    // (docs.github.com/en/copilot/reference/copilot-cli-reference/
    // cli-command-reference) means `request_compact` must no longer reject
    // a copilot caller the way it used to.
    let (reg, _d) = test_registry();
    let copilot = Guardrails { agent_cli: "copilot".into(), ..rails() };
    let g = reg.create_group("C:/tmp/repo", copilot).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let msg = reg.request_compact(&o.id).unwrap();
    assert!(msg.contains("compact requested"), "got: {msg}");
    assert!(reg.agent(&o.id).unwrap().compact_requested, "flag set on the calling copilot agent");
}

#[test]
fn request_compact_offload_checklist_warning_is_recency_gated_on_set_state() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let first = reg.request_compact(&o.id).unwrap();
    assert!(first.contains("warning"), "no set_state call yet — must warn, got: {first}");
    reg.note_state_write(&o.id);
    let second = reg.request_compact(&o.id).unwrap();
    assert!(!second.contains("warning"), "set_state was just called — no warning, got: {second}");
    // Non-orchestrator callers never see the warning (set_state isn't even
    // available to them).
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "work", false, None).unwrap();
    let worker_msg = reg.request_compact(&w.id).unwrap();
    assert!(!worker_msg.contains("warning"), "the checklist warning is orchestrator-only, got: {worker_msg}");
}

#[test]
fn compact_nudge_tick_fires_a_requested_worker_regardless_of_role_eligibility() {
    let (reg, _d) = test_registry();
    // Default role set is orchestrator-only; the worker is NOT eligible for
    // the heuristic path, but request_compact is self-initiated and role-
    // agnostic.
    let g = reg.create_group("C:/tmp/repo", compact_rails(20, &["orchestrator"])).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "work", false, None).unwrap();
    reg.request_compact(&w.id).unwrap();
    let empty = HashMap::new();
    let nudged = reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert_eq!(nudged, vec![w.id.clone()], "a requested fire is role-agnostic");
    assert!(!reg.agent(&w.id).unwrap().compact_requested, "the request is consumed on fire");
}

#[test]
fn compact_nudge_tick_requested_fire_waits_for_quiet_not_a_minutes_threshold() {
    let (reg, _d, _gid, oid) = compact_nudge_setup(9999); // heuristic threshold effectively unreachable
    reg.request_compact(&oid).unwrap();
    let empty = HashMap::new();
    // now=1 is far too early for ANY minutes-scale heuristic threshold — a
    // requested fire needs no elapsed quiet-window time, only "quiet on this
    // observation".
    assert_eq!(reg.compact_nudge_tick(1, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()), vec![oid.clone()],
        "a request fires at the next quiet observation, independent of the heuristic minutes threshold");
}

#[test]
fn compact_nudge_tick_escalation_notice_then_falls_back_to_requesting_next_tick() {
    let (reg, _d, gid, oid) = compact_nudge_setup(9999);
    reg.set_compact_context_threshold(&gid, 80).unwrap();
    let empty = HashMap::new();
    let percents: HashMap<String, u32> = [(oid.clone(), 85u32)].into_iter().collect();
    // Tick 1: crosses threshold — notice fires, no auto-request yet (the
    // agent gets a chance to self-request first — see the fn doc for why
    // this is split across two ticks rather than done together).
    let nudged = reg.compact_nudge_tick(1, &empty, &HashMap::new(), &percents, &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(nudged.is_empty(), "escalation alone doesn't paste /compact");
    assert_eq!(audit_count(&reg, &gid, "compact-escalation"), 1);
    assert!(!reg.agent(&oid).unwrap().compact_requested, "no auto-request on the SAME tick as the notice");
    // Tick 2: still over threshold, agent still hasn't self-requested —
    // loomux falls back on its behalf, setting `compact_requested`. #410
    // (round 6): the fire-check that consumes it now runs BEFORE this
    // escalation block in per-agent iteration order (fixing a request-
    // starvation incident), so the fallback-set request is no longer
    // visible to a fire-check until the NEXT tick.
    let nudged2 = reg.compact_nudge_tick(2, &empty, &HashMap::new(), &percents, &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(nudged2.is_empty(), "the fallback sets compact_requested but cannot fire until the next tick (round-6 reorder)");
    assert!(reg.agent(&oid).unwrap().compact_requested, "fallback set the request on the agent's behalf");
    // Tick 3: the fire-check now sees the request set on tick 2.
    let nudged3 = reg.compact_nudge_tick(3, &empty, &HashMap::new(), &percents, &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert_eq!(nudged3, vec![oid.clone()], "fallback request fires the tick after it's set");
    assert_eq!(audit_count(&reg, &gid, "compact-escalation"), 1, "still just one notice — no re-nag");
}

#[test]
fn compact_nudge_tick_escalation_latch_clears_once_back_under_threshold() {
    let (reg, _d, gid, oid) = compact_nudge_setup(9999);
    reg.set_compact_context_threshold(&gid, 80).unwrap();
    let empty = HashMap::new();
    let over: HashMap<String, u32> = [(oid.clone(), 90u32)].into_iter().collect();
    let _ = reg.compact_nudge_tick(1, &empty, &HashMap::new(), &over, &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert_eq!(audit_count(&reg, &gid, "compact-escalation"), 1);
    let under: HashMap<String, u32> = [(oid.clone(), 10u32)].into_iter().collect();
    let _ = reg.compact_nudge_tick(2, &empty, &HashMap::new(), &under, &HashMap::new(), &HashMap::new(), &HashMap::new()); // e.g. after a compact landed
    let _ = reg.compact_nudge_tick(3, &empty, &HashMap::new(), &over, &HashMap::new(), &HashMap::new(), &HashMap::new()); // crosses again
    assert_eq!(audit_count(&reg, &gid, "compact-escalation"), 2, "a fresh crossing after clearing earns a new notice");
}

#[test]
fn compact_nudge_tick_never_fires_a_second_compact_while_the_first_is_still_pending() {
    // rev-12 review: neither the context-escalation fallback nor the final
    // fire decision checked `compact_pending`, so a still-over-threshold
    // context% reading on the tick right after a fallback-triggered fire —
    // before the pane has visibly gone busy from loomux's point of view, so
    // it still reads quiet — re-armed `compact_requested` (reset to false
    // when the first /compact fired) and typed a SECOND `/compact` into a
    // pane whose first compact hadn't resolved yet.
    let (reg, _d, gid, oid) = compact_nudge_setup(9999); // heuristic effectively unreachable
    reg.set_compact_context_threshold(&gid, 80).unwrap();
    let empty = HashMap::new();
    let over: HashMap<String, u32> = [(oid.clone(), 90u32)].into_iter().collect();

    // Tick 1: first crossing — notice only, no fire.
    assert!(reg.compact_nudge_tick(1, &empty, &HashMap::new(), &over, &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    assert_eq!(audit_count(&reg, &gid, "compact-escalation"), 1);
    assert!(!reg.agent(&oid).unwrap().compact_pending);

    // Tick 2: still over threshold, notice already latched — the fallback
    // sets `compact_requested`. #410 (round 6): the fire-check now runs
    // BEFORE this escalation block, so it isn't visible to fire until the
    // NEXT tick (see the escalation-notice test above for the same shift).
    let set_only = reg.compact_nudge_tick(2, &empty, &HashMap::new(), &over, &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(set_only.is_empty(), "the fallback sets the request but cannot fire until the next tick");
    assert!(reg.agent(&oid).unwrap().compact_requested);

    // Tick 3: the fire-check now sees it — the legitimate fallback fires the
    // FIRST compact.
    let nudged = reg.compact_nudge_tick(3, &empty, &HashMap::new(), &over, &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert_eq!(nudged, vec![oid.clone()], "the fallback's first fire is expected");
    assert!(reg.agent(&oid).unwrap().compact_pending, "now pending — unresolved");
    assert_eq!(audit_count(&reg, &gid, "compact-nudge"), 1);

    // Tick 4: STILL over threshold (a stale/unchanged reading — the compact
    // from tick 3 hasn't produced visible output yet, so the pane still
    // reads quiet). This is exactly the window the bug lived in. Must NOT
    // fire a second /compact, and must not even re-notify or re-arm.
    let nudged2 = reg.compact_nudge_tick(4, &empty, &HashMap::new(), &over, &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(nudged2.is_empty(), "must not fire a second /compact while the first is still pending");
    assert_eq!(audit_count(&reg, &gid, "compact-nudge"), 1, "still just the one fire");
    assert_eq!(audit_count(&reg, &gid, "compact-escalation"), 1, "no re-notify while pending either");
    assert!(!reg.agent(&oid).unwrap().compact_requested,
        "compact_requested must not be silently re-armed by escalation while pending");
}

#[test]
fn compact_nudge_tick_detects_a_manually_typed_compact_and_reinjects_after_it_finishes() {
    let (reg, _d, gid, oid) = compact_nudge_setup(0); // heuristic off — isolate manual detection
    let signals: HashMap<String, (String, u64)> =
        [(oid.clone(), ("> /compact\n".to_string(), 500u64))].into_iter().collect();
    let empty = HashMap::new();
    // Production bug fix (D2): reinjection now also requires a confirmed
    // context-token drop, not just busy-then-quiet. `baseline` is captured
    // at the ARM tick (500); `dropped` is what the resolve tick (700) reads —
    // well under the confirmation ratio, simulating a real compaction.
    let baseline: HashMap<String, u64> = [(oid.clone(), 100_000u64)].into_iter().collect();
    let dropped: HashMap<String, u64> = [(oid.clone(), 5_000u64)].into_iter().collect();
    // Detected: pending starts, but nothing fires yet (no busy observed).
    let nudged = reg.compact_nudge_tick(500, &empty, &signals, &HashMap::new(), &baseline, &HashMap::new(), &HashMap::new());
    assert!(nudged.is_empty());
    assert!(reg.agent(&oid).unwrap().compact_pending, "manual /compact must start pending tracking");
    assert_eq!(audit_count(&reg, &gid, "compact-nudge"), 0, "loomux pastes nothing — the human already did");
    // Compaction runs: a real output burst.
    let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64)].into_iter().collect();
    let _ = reg.compact_nudge_tick(600, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(reg.agent(&oid).unwrap().compact_pending, "still pending — busy, not finished");
    // Compaction finishes: quiet again (same total, no further growth), AND
    // the token reading confirms a real drop happened.
    let _ = reg.compact_nudge_tick(700, &grew, &HashMap::new(), &HashMap::new(), &dropped, &HashMap::new(), &HashMap::new());
    assert!(reg.agent(&oid).unwrap().compact_pending, "confirmed, but waiting on the delivery to confirm (rev-42 delta, round 2)");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1);
    // Delivery confirms: the latch finally releases.
    let confirmed = confirmed_delivery(&oid, 700);
    let _ = reg.compact_nudge_tick(800, &grew, &HashMap::new(), &HashMap::new(), &dropped, &HashMap::new(), &confirmed);
    assert!(!reg.agent(&oid).unwrap().compact_pending, "resolved");
}

#[test]
fn compact_nudge_tick_detects_the_clis_own_auto_compact_banner_and_reinjects_after_it_finishes() {
    let (reg, _d, gid, oid) = compact_nudge_setup(0); // heuristic off — isolate banner detection
    let banner_tail: HashMap<String, (String, u64)> =
        [(oid.clone(), ("✢ Compacting conversation… (esc to interrupt · 8s · ↓ 172 tokens)".to_string(), 0u64))]
            .into_iter()
            .collect();
    // The banner appears WHILE the pane is busy — real output growth on the
    // very same tick, exactly like the CLI actually rendering it.
    let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64)].into_iter().collect();
    // Production bug fix (D2): baseline captured at the arm tick, a confirmed
    // drop at the resolve tick — this is the exact scenario the production
    // incident showed CANNOT be trusted on busy-then-quiet alone (a mention
    // of the banner text produces the identical busy-then-quiet shape with
    // NO real compaction and NO token drop — see the `_ignores_a_busy_pane_
    // that_merely_mentions_the_banner` test for that negative case).
    let baseline: HashMap<String, u64> = [(oid.clone(), 100_000u64)].into_iter().collect();
    let dropped: HashMap<String, u64> = [(oid.clone(), 5_000u64)].into_iter().collect();
    let nudged = reg.compact_nudge_tick(500, &grew, &banner_tail, &HashMap::new(), &baseline, &HashMap::new(), &HashMap::new());
    assert!(nudged.is_empty(), "loomux pastes nothing — the CLI is already compacting on its own");
    assert!(reg.agent(&oid).unwrap().compact_pending, "the CLI's own auto-compact must start pending tracking");
    assert_eq!(audit_count(&reg, &gid, "compact-nudge"), 0);
    // Detected mid-compaction, so `compact_seen_busy` is already true — the
    // very next quiet tick resolves, no separate busy tick needed first
    // (unlike the manual-`/compact` path, which only starts pending and
    // waits for a later busy observation).
    let _ = reg.compact_nudge_tick(600, &grew, &HashMap::new(), &HashMap::new(), &dropped, &HashMap::new(), &HashMap::new());
    assert!(reg.agent(&oid).unwrap().compact_pending, "confirmed on the first quiet tick, but waiting on delivery (rev-42 delta, round 2)");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1);
    let confirmed = confirmed_delivery(&oid, 600);
    let _ = reg.compact_nudge_tick(700, &grew, &HashMap::new(), &HashMap::new(), &dropped, &HashMap::new(), &confirmed);
    assert!(!reg.agent(&oid).unwrap().compact_pending, "resolved");
}

#[test]
fn compact_nudge_tick_ignores_an_auto_compact_banner_on_a_quiet_tick() {
    // The tail holds the banner substring, but NO fresh growth landed this
    // tick — the whole point of gating on `!currently_quiet` rather than a
    // duration window (see the fn doc): a banner sitting in unchanged tail
    // content is not news, however long it's been there.
    let (reg, _d, gid, oid) = compact_nudge_setup(0);
    let banner_tail: HashMap<String, (String, u64)> =
        [(oid.clone(), ("Compacting conversation".to_string(), 0u64))].into_iter().collect();
    let empty = HashMap::new();
    let _ = reg.compact_nudge_tick(500, &empty, &banner_tail, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(!reg.agent(&oid).unwrap().compact_pending, "no growth this tick — must not trigger on a quiet read");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 0);
}

#[test]
fn compact_nudge_tick_ignores_a_busy_pane_that_merely_mentions_the_banner() {
    // rev review B1, the test gap the review named directly: a `!
    // currently_quiet` growth gate alone does NOT close the false positive,
    // because a busy pane that PRINTS or DISCUSSES the banner text satisfies
    // the growth gate BY the mention itself — the mention and the trigger
    // are the same event. This is the scenario that must NOT fire once the
    // position-anchored fix (last-line-only) is in place: real growth this
    // tick, but the banner substring sits mid-scrollback, not as the tail's
    // final line.
    let (reg, _d, gid, oid) = compact_nudge_setup(0);
    let mention_tail: HashMap<String, (String, u64)> = [(
        oid.clone(),
        (
            "grep results:\n\
             docs/design/orchestration.md: rendered as `Compacting conversation` — a leading spinner\n\
             src-tauri/src/orchestration/mod.rs: \"claude\" => &[\"Compacting conversation\"],\n\
             Done — 2 matches."
                .to_string(),
            0u64,
        ),
    )]
    .into_iter()
    .collect();
    // Real growth this tick (the grep output itself), same as any normal turn.
    let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64)].into_iter().collect();
    let _ = reg.compact_nudge_tick(500, &grew, &mention_tail, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(
        !reg.agent(&oid).unwrap().compact_pending,
        "a busy pane merely mentioning the banner text mid-scrollback must not be read as a real auto-compact"
    );
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 0);
}

#[test]
fn compact_nudge_tick_never_reads_claudes_auto_compact_banner_for_a_copilot_agent() {
    // Banner detection is keyed PER-CLI (`auto_compact_banner_substrings`),
    // independent of the broader `compact_nudge_cli_supported` admission
    // gate — a copilot agent is admitted to the loop now (#417 correction
    // round 2: Copilot has its own `/compact` too), but it still has no
    // banner substring of its own registered, so Claude's exact "Compacting
    // conversation" string appearing in a copilot pane (coincidence,
    // unrelated tooling) must never be misread as an auto-compact.
    let (reg, _d) = test_registry();
    let rails = Guardrails { agent_cli: "copilot".into(), ..compact_rails(0, &["orchestrator"]) };
    let g = reg.create_group("C:/tmp/repo", rails).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let banner_tail: HashMap<String, (String, u64)> =
        [(o.id.clone(), ("Compacting conversation".to_string(), 0u64))].into_iter().collect();
    let grew: HashMap<String, u64> = [(o.id.clone(), 50_000u64)].into_iter().collect();
    let _ = reg.compact_nudge_tick(500, &grew, &banner_tail, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(!reg.agent(&o.id).unwrap().compact_pending, "non-Claude CLI must never be read for Claude's banner");
}

#[test]
fn compact_nudge_tick_ignores_a_stale_manual_compact_tail_outside_the_recency_window() {
    let (reg, _d, gid, oid) = compact_nudge_setup(0);
    // The tail still shows an OLD /compact echo, but the human hasn't typed
    // anything in a very long time — must not (re-)trigger detection.
    let signals: HashMap<String, (String, u64)> = [(oid.clone(), ("/compact".to_string(), 0u64))].into_iter().collect();
    let empty = HashMap::new();
    let _ = reg.compact_nudge_tick(FAR, &empty, &signals, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(!reg.agent(&oid).unwrap().compact_pending, "stale tail content outside the recency window must not trigger");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 0);
}

#[test]
fn compact_nudge_tick_reinjects_after_a_loomux_initiated_fire_too() {
    // The busy-then-quiet detector is shared by all three trigger paths —
    // pin it for the heuristic/requested path, not just the manual one.
    let (reg, _d, gid, oid) = compact_nudge_setup(5);
    let empty = HashMap::new();
    // Production bug fix (D2): baseline at the arm tick, confirmed drop at
    // the resolve tick — same requirement now applies uniformly to every
    // trigger path, not just the two externally-observed ones.
    let baseline: HashMap<String, u64> = [(oid.clone(), 100_000u64)].into_iter().collect();
    let dropped: HashMap<String, u64> = [(oid.clone(), 5_000u64)].into_iter().collect();
    assert_eq!(reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &HashMap::new(), &baseline, &HashMap::new(), &HashMap::new()), vec![oid.clone()],
        "the heuristic fire pastes /compact and starts pending tracking");
    assert!(reg.agent(&oid).unwrap().compact_pending);
    let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64)].into_iter().collect();
    let _ = reg.compact_nudge_tick(FAR + 1_000, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    let _ = reg.compact_nudge_tick(FAR + 2_000, &grew, &HashMap::new(), &HashMap::new(), &dropped, &HashMap::new(), &HashMap::new());
    assert!(reg.agent(&oid).unwrap().compact_pending, "confirmed once busy-then-quiet is observed, but waiting on delivery (rev-42 delta, round 2)");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1);
    let confirmed = confirmed_delivery(&oid, FAR + 2_000);
    let _ = reg.compact_nudge_tick(FAR + 3_000, &grew, &HashMap::new(), &HashMap::new(), &dropped, &HashMap::new(), &confirmed);
    assert!(!reg.agent(&oid).unwrap().compact_pending, "resolved");
}

#[test]
fn compact_nudge_tick_discards_an_unconfirmed_pending_state_without_reinjecting() {
    // Production bug fix (D2/D3): the exact production incident, reproduced
    // as a regression case. An orchestrator asked to discuss the compact-
    // nudge feature produced output whose busy-then-quiet cycle satisfied the
    // banner detector (mention, not a real auto-compact — see the B1 fix)
    // with NO context-token drop. The state machine must resolve this to a
    // DISCARD, not a reinjection: `compact_pending` clears (one-shot, D3),
    // no `compact-reinjection` audit lands, and the discard itself IS
    // observable (`compact-pending-discarded`) — the exact visibility gap
    // that made the live incident hard to root-cause from the audit log
    // alone.
    let (reg, _d, gid, oid) = compact_nudge_setup(0);
    let banner_tail: HashMap<String, (String, u64)> =
        [(oid.clone(), ("✢ Compacting conversation… (esc to interrupt · 8s · ↓ 172 tokens)".to_string(), 0u64))]
            .into_iter()
            .collect();
    let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64)].into_iter().collect();
    // No real token drop: baseline and "current" read the same, ordinary-turn
    // growth, not a compaction.
    let unchanged: HashMap<String, u64> = [(oid.clone(), 100_000u64)].into_iter().collect();
    let nudged = reg.compact_nudge_tick(500, &grew, &banner_tail, &HashMap::new(), &unchanged, &HashMap::new(), &HashMap::new());
    assert!(nudged.is_empty());
    assert!(reg.agent(&oid).unwrap().compact_pending, "detection still arms pending — only the RESOLUTION changes");

    let resolved = reg.compact_nudge_tick(600, &grew, &HashMap::new(), &HashMap::new(), &unchanged, &HashMap::new(), &HashMap::new());
    assert!(resolved.is_empty());
    assert!(!reg.agent(&oid).unwrap().compact_pending, "one-shot: cleared regardless of confirmation (D3)");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 0, "no reinjection without confirmed evidence (D2)");
    assert_eq!(audit_count(&reg, &gid, "compact-pending-discarded"), 1, "the discard itself must be visible");
}

#[test]
fn compact_nudge_tick_never_loops_when_the_false_signal_repeats_every_cycle() {
    // Production bug fix (D4, the review's named regression test): "reinjection
    // delivery → agent responds → detector must NOT fire again." The live
    // incident's actual shape was worse than one false positive — it repeated
    // every response cycle for as long as the orchestrator kept discussing the
    // feature. This drives THREE full arm-resolve cycles with the identical
    // unconfirmed signal and pins that context never grows via a reinjection:
    // `compact-reinjection` stays at 0 throughout, no matter how many times
    // the same false condition re-satisfies detection.
    //
    // rev-42 delta (round 7): each cycle now needs to clear `INFERENCE_ARM_
    // COOLDOWN_MS` before the NEXT one can arm (the round-7 fix for exactly
    // this repeating-false-signal shape re-arming instantly) — the gaps
    // between cycles below account for that; the CORE invariant this test
    // pins (zero reinjections, ever) is unaffected.
    let (reg, _d, gid, oid) = compact_nudge_setup(0);
    let banner_tail: HashMap<String, (String, u64)> =
        [(oid.clone(), ("Compacting conversation".to_string(), 0u64))].into_iter().collect();
    let unchanged: HashMap<String, u64> = [(oid.clone(), 100_000u64)].into_iter().collect();
    let cooldown_ms = 3 * 60_000u64; // INFERENCE_ARM_COOLDOWN_MS
    let mut t = 500u64;
    for cycle in 0..3u32 {
        let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64 * (cycle as u64 + 1))].into_iter().collect();
        // Arm: banner (mention) detected alongside fresh growth.
        let _ = reg.compact_nudge_tick(t, &grew, &banner_tail, &HashMap::new(), &unchanged, &HashMap::new(), &HashMap::new());
        assert!(reg.agent(&oid).unwrap().compact_pending, "cycle {cycle}: arms once the cooldown has cleared, same as production");
        t += 100;
        // Resolve: quiet again, same total — no confirmed drop.
        let _ = reg.compact_nudge_tick(t, &grew, &HashMap::new(), &HashMap::new(), &unchanged, &HashMap::new(), &HashMap::new());
        assert!(!reg.agent(&oid).unwrap().compact_pending, "cycle {cycle}: resolves (discarded), never stuck pending");
        assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 0,
            "cycle {cycle}: a reinjection loop must never start, however many times the false signal repeats");
        t += cooldown_ms + 1;
    }
    assert_eq!(audit_count(&reg, &gid, "compact-pending-discarded"), 3, "every cycle's discard is individually visible");
}

#[test]
fn compact_nudge_tick_suppresses_an_immediate_rearm_of_the_same_false_signal_within_the_cooldown() {
    // rev-42 delta (round 7): the OTHER half of D4's fix — unlike the test
    // above (which lets the cooldown clear between cycles), the identical
    // false signal repeating BEFORE the cooldown clears must not even
    // re-arm at all — this is what actually closes the live incident's
    // "discard, then immediately re-arm" cycling that starved a queued
    // request_compact (#410).
    let (reg, _d, gid, oid) = compact_nudge_setup(0);
    let banner_tail: HashMap<String, (String, u64)> =
        [(oid.clone(), ("Compacting conversation".to_string(), 0u64))].into_iter().collect();
    let unchanged: HashMap<String, u64> = [(oid.clone(), 100_000u64)].into_iter().collect();
    let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64)].into_iter().collect();
    let _ = reg.compact_nudge_tick(500, &grew, &banner_tail, &HashMap::new(), &unchanged, &HashMap::new(), &HashMap::new());
    assert!(reg.agent(&oid).unwrap().compact_pending);
    let _ = reg.compact_nudge_tick(600, &grew, &HashMap::new(), &HashMap::new(), &unchanged, &HashMap::new(), &HashMap::new());
    assert!(!reg.agent(&oid).unwrap().compact_pending, "resolved (discarded)");
    assert_eq!(audit_count(&reg, &gid, "compact-pending-discarded"), 1);

    // The identical banner mention recurs, WITH fresh real growth this tick
    // (banner detection's own busy requirement genuinely satisfied) —
    // still within the cooldown, so it must not re-arm.
    let grew2: HashMap<String, u64> = [(oid.clone(), 100_000u64)].into_iter().collect();
    let _ = reg.compact_nudge_tick(700, &grew2, &banner_tail, &HashMap::new(), &unchanged, &HashMap::new(), &HashMap::new());
    assert!(!reg.agent(&oid).unwrap().compact_pending, "cooldown suppresses the immediate rearm");
    assert_eq!(audit_count(&reg, &gid, "compact-pending-discarded"), 1, "no second discard — it never armed at all");

    // A genuinely NEW banner mention, after the cooldown has cleared, still
    // arms normally — the cooldown suppresses noise, it doesn't disable the
    // detector.
    let cooldown_ms = 3 * 60_000u64; // INFERENCE_ARM_COOLDOWN_MS
    let grew3: HashMap<String, u64> = [(oid.clone(), 150_000u64)].into_iter().collect();
    let _ = reg.compact_nudge_tick(600 + cooldown_ms + 1, &grew3, &banner_tail, &HashMap::new(), &unchanged, &HashMap::new(), &HashMap::new());
    assert!(reg.agent(&oid).unwrap().compact_pending, "a genuinely new signal after the cooldown clears still arms");
}

#[test]
fn compact_nudge_tick_never_reads_its_own_compact_paste_echo_as_a_manually_typed_one() {
    // #410/round-7, the user's own disambiguating hypothesis: `human_typed_
    // compact_detected` scans the WHOLE tail for a standalone `/compact`
    // token, not just fresh growth — so loomux's OWN `/compact` paste from a
    // genuine, already-resolved compaction can still be sitting in the
    // bounded tail when the human later types something completely
    // UNRELATED (satisfying manual detection's recency-of-ANY-keystroke
    // gate) — misreading this app's own stale echo as a freshly human-typed
    // `/compact`. The provenance fix: a CONFIRMED delivery whose `from` is
    // this app's own (either spelling, `brand::is_host_actor`) extends the
    // inference-arm cooldown, independent of and in addition to the
    // post-resolve cooldown above.
    let (reg, _d, gid, oid) = compact_nudge_setup(5); // heuristic on — the loomux-initiated arm
    let empty = HashMap::new();
    // Loomux's own fire, busy-then-quiet resolve — a genuine, real compaction.
    assert_eq!(
        reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()),
        vec![oid.clone()],
        "the heuristic fire pastes /compact itself"
    );
    let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64)].into_iter().collect();
    let _ = reg.compact_nudge_tick(FAR + 1_000, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    let _ = reg.compact_nudge_tick(FAR + 2_000, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    let confirmed = confirmed_delivery(&oid, FAR + 2_000);
    let _ = reg.compact_nudge_tick(FAR + 3_000, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &confirmed);
    assert!(!reg.agent(&oid).unwrap().compact_pending, "resolved once the trusted arm's delivery confirms");

    // Minutes later (past MANUAL_COMPACT_DETECT_WINDOW_MS's own recency gate
    // if that were the only check), the human types something UNRELATED —
    // the tail still contains loomux's OWN `/compact` paste from earlier in
    // the SAME pane (it hasn't scrolled out of the bounded tail yet), and the
    // human's fresh keystroke alone would satisfy the OLD recency-only check.
    let stale_tail_with_our_own_paste: HashMap<String, (String, u64)> =
        [(oid.clone(), ("some unrelated output\n> /compact\nmore unrelated output".to_string(), FAR + 5_000))]
            .into_iter()
            .collect();
    let _ = reg.compact_nudge_tick(
        FAR + 5_000, &grew, &stale_tail_with_our_own_paste, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(
        !reg.agent(&oid).unwrap().compact_pending,
        "loomux's own recently-confirmed paste must never satisfy its own manual detector"
    );
    assert_eq!(audit_count(&reg, &gid, "compact-nudge"), 1, "no second /compact typed — the arm never even started");
}

#[test]
fn compact_nudge_tick_reinjects_a_loomux_initiated_fire_even_with_no_confirmed_token_drop() {
    // rev-42 delta (the deadlock this round fixes): `usage::latest_context_
    // tokens`'s drop is a NEXT-TURN phenomenon (proved against a real
    // transcript in `usage::tests::real_transcript_proves_the_token_drop_is_
    // a_next_turn_phenomenon_rev42_q1`). On the loomux-initiated arm (the one
    // that pastes `/compact` itself, below), the ONLY next turn available is
    // the reinjection this gate would authorize — requiring a confirmed drop
    // here deadlocks forever: no reading is ever taken that could show the
    // drop, because no turn happens before the (gated) reinjection. Before
    // `compact_pending_trusted`, this test would have failed (red): the flat
    // token reading below never satisfies `compaction_confirmed`, so the
    // resolve would silently discard a genuine loomux-initiated compaction on
    // every occurrence — worse than the original reinjection-loop incident,
    // because it is silent identity loss on the PRIMARY path, not a loop.
    let (reg, _d, gid, oid) = compact_nudge_setup(5);
    let empty = HashMap::new();
    // Token reading never drops — same value at arm, busy, and resolve ticks,
    // simulating "no next turn ever came to reveal the drop".
    let flat: HashMap<String, u64> = [(oid.clone(), 100_000u64)].into_iter().collect();
    assert_eq!(
        reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &HashMap::new(), &flat, &HashMap::new(), &HashMap::new()),
        vec![oid.clone()],
        "the heuristic fire pastes /compact and starts pending tracking"
    );
    assert!(reg.agent(&oid).unwrap().compact_pending_trusted, "loomux-initiated arm must be marked trusted");
    let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64)].into_iter().collect();
    let _ = reg.compact_nudge_tick(FAR + 1_000, &grew, &HashMap::new(), &HashMap::new(), &flat, &HashMap::new(), &HashMap::new());
    assert!(reg.agent(&oid).unwrap().compact_pending, "still pending — busy, not finished");
    let resolved = reg.compact_nudge_tick(FAR + 2_000, &grew, &HashMap::new(), &HashMap::new(), &flat, &HashMap::new(), &HashMap::new());
    assert!(resolved.is_empty(), "resolution never pastes /compact itself, only the fire tick does");
    assert!(reg.agent(&oid).unwrap().compact_pending,
        "confirmed on busy-then-quiet alone, no token drop needed — but waiting on delivery (rev-42 delta, round 2)");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1,
        "trusted arm must reinject even with a flat token reading — the exact deadlock this test regresses against");
    assert_eq!(audit_count(&reg, &gid, "compact-pending-discarded"), 0,
        "must never silently discard a genuine loomux-initiated compaction");
    let confirmed = confirmed_delivery(&oid, FAR + 2_000);
    let _ = reg.compact_nudge_tick(FAR + 3_000, &grew, &HashMap::new(), &HashMap::new(), &flat, &HashMap::new(), &confirmed);
    assert!(!reg.agent(&oid).unwrap().compact_pending, "resolved once the delivery confirms");
}

#[test]
fn compact_nudge_tick_recovers_via_a_re_request_after_an_inference_arm_is_discarded() {
    // Non-blocking hardening (rev-42 item 3, end-to-end coverage): an
    // inference-arm discard is not a dead end. The agent (or the heuristic
    // fallback) can re-request, and the loomux-initiated (trusted) arm
    // resolves normally even against the SAME flat token reading that just
    // discarded the first arm — proof the two paths compose within one
    // agent's lifetime rather than only in isolation.
    let (reg, _d, gid, oid) = compact_nudge_setup(0); // heuristic off — isolate the two arms
    let signals: HashMap<String, (String, u64)> =
        [(oid.clone(), ("> /compact\n".to_string(), 500u64))].into_iter().collect();
    let empty = HashMap::new();
    let flat: HashMap<String, u64> = [(oid.clone(), 100_000u64)].into_iter().collect();

    // First arm: manual `/compact` detection — an inference arm, untrusted.
    let _ = reg.compact_nudge_tick(500, &empty, &signals, &HashMap::new(), &flat, &HashMap::new(), &HashMap::new());
    assert!(reg.agent(&oid).unwrap().compact_pending);
    assert!(!reg.agent(&oid).unwrap().compact_pending_trusted, "manual detection is an inference arm");
    let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64)].into_iter().collect();
    let _ = reg.compact_nudge_tick(600, &grew, &HashMap::new(), &HashMap::new(), &flat, &HashMap::new(), &HashMap::new());
    // No drop, no marker: resolves to a DISCARD, not a reinjection.
    let _ = reg.compact_nudge_tick(700, &grew, &HashMap::new(), &HashMap::new(), &flat, &HashMap::new(), &HashMap::new());
    assert!(!reg.agent(&oid).unwrap().compact_pending, "one-shot: cleared");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 0);
    assert_eq!(audit_count(&reg, &gid, "compact-pending-discarded"), 1);

    // Agent re-requests explicitly — this arm is loomux-initiated, trusted.
    reg.request_compact(&oid).unwrap();
    let nudged = reg.compact_nudge_tick(800, &grew, &HashMap::new(), &HashMap::new(), &flat, &HashMap::new(), &HashMap::new());
    assert_eq!(nudged, vec![oid.clone()], "the re-request fires a fresh /compact paste");
    assert!(reg.agent(&oid).unwrap().compact_pending_trusted, "the re-request arm is loomux-initiated, trusted");
    let grew2: HashMap<String, u64> = [(oid.clone(), 100_000u64)].into_iter().collect();
    let _ = reg.compact_nudge_tick(900, &grew2, &HashMap::new(), &HashMap::new(), &flat, &HashMap::new(), &HashMap::new());
    let _ = reg.compact_nudge_tick(1000, &grew2, &HashMap::new(), &HashMap::new(), &flat, &HashMap::new(), &HashMap::new());
    assert!(reg.agent(&oid).unwrap().compact_pending,
        "confirmed against the same flat token reading that discarded the first arm, but waiting on delivery (rev-42 delta, round 2)");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1,
        "the trusted re-request arm resolves even against the same flat token reading that discarded the first arm");
    let confirmed = confirmed_delivery(&oid, 1000);
    let _ = reg.compact_nudge_tick(1100, &grew2, &HashMap::new(), &HashMap::new(), &flat, &HashMap::new(), &confirmed);
    assert!(!reg.agent(&oid).unwrap().compact_pending, "resolved");
}

#[test]
fn compact_nudge_tick_resolves_a_loomux_initiated_fire_via_the_boundary_marker_when_output_never_clears_the_busy_floor() {
    // PR #329 round-5 re-demo, the ACTUAL root cause: the forensic timeline
    // (audit.jsonl + breadcrumbs.log around a genuine, CONFIRMED `/compact`
    // paste) showed no reinjection audit, no discard audit — nothing — for
    // over three minutes while the agent was demonstrably back to normal
    // work. The busy-then-quiet resolver never even reached its confirm/
    // discard branch: `compact_seen_busy` depended ENTIRELY on real
    // terminal-output growth clearing `idle_activity_floor_bytes`, and a
    // real but small/fast compaction can fail to clear it — unlike the two
    // INFERENCE arms (which set `compact_seen_busy = true` immediately from
    // the very evidence that armed them: the banner text, the manual
    // typing), the loomux-initiated arm had no such alternate evidence and
    // was purely waiting on a later tick's byte-growth observation that may
    // simply never come. This pins the fix: a rise in the `compact_boundary`
    // transcript marker counts as "seen busy" too — direct, floor-
    // independent proof a compaction actually happened, for every arm.
    let (reg, _d, gid, oid) = compact_nudge_setup(5);
    let empty = HashMap::new();
    let markers_baseline: HashMap<String, u64> = [(oid.clone(), 1u64)].into_iter().collect();
    assert_eq!(
        reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &markers_baseline, &HashMap::new()),
        vec![oid.clone()],
        "the heuristic fire pastes /compact and starts pending tracking"
    );
    // NO real output growth is EVER observed on any subsequent tick
    // (simulating a compaction whose own terminal rendering never clears the
    // floor) — only the transcript's compact_boundary marker advances.
    let markers_after: HashMap<String, u64> = [(oid.clone(), 2u64)].into_iter().collect();
    let resolved = reg.compact_nudge_tick(
        FAR + 60_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &markers_after, &HashMap::new());
    assert!(resolved.is_empty(), "resolution never pastes /compact itself, only the fire tick does");
    assert!(reg.agent(&oid).unwrap().compact_pending, "confirmed via the marker, waiting on delivery to confirm");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1,
        "a marker rise alone must resolve the arm even though compact_seen_busy (byte-growth) was never observed");
    let confirmed = confirmed_delivery(&oid, FAR + 60_000);
    let _ = reg.compact_nudge_tick(
        FAR + 61_000, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &markers_after, &confirmed);
    assert!(!reg.agent(&oid).unwrap().compact_pending, "resolved");
}

#[test]
fn compact_nudge_tick_retries_a_reinjection_whose_delivery_never_confirms_then_delivers_exactly_once() {
    // rev-42 delta (round 2): `deliver_prompt` is fire-and-forget — a fired
    // reinjection's delivery can be held (a human typing), never confirm, or
    // otherwise not land on the first attempt. The latch must not release
    // until a delivery genuinely confirms; a stuck delivery gets a bounded,
    // audited retry rather than being silently treated as "done" the instant
    // it was attempted — the fix contract the round-5 re-demo required
    // regardless of which mechanism caused that specific re-demo's gap.
    // Heuristic off, armed via `request_compact` instead — isolates the
    // confirmation-retry mechanics from the heuristic's OWN minutes window,
    // which would otherwise re-elapse (and spuriously re-arm) during the
    // multi-minute confirmation wait this test drives.
    let (reg, _d, gid, oid) = compact_nudge_setup(0);
    let empty = HashMap::new();
    reg.request_compact(&oid).unwrap();
    assert_eq!(
        reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()),
        vec![oid.clone()],
    );
    let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64)].into_iter().collect();
    let _ = reg.compact_nudge_tick(FAR + 1_000, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    let _ = reg.compact_nudge_tick(FAR + 2_000, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1, "first attempt fires");
    assert!(reg.agent(&oid).unwrap().compact_pending, "still pending — waiting on confirmation");

    // No delivery ever confirms (a held/lost delivery) — right up to, but not
    // past, the retry timeout.
    let timeout_ms = 5 * 60 * 1000u64; // REINJECT_CONFIRM_TIMEOUT_MS
    let still_waiting = reg.compact_nudge_tick(
        FAR + 2_000 + timeout_ms - 1, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(still_waiting.is_empty());
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1, "must not retry before the timeout — a slow delivery may still be legitimately in flight");

    // Timeout elapses with STILL no confirmation: a bounded retry fires.
    let retry_time = FAR + 2_000 + timeout_ms;
    let _ = reg.compact_nudge_tick(retry_time, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 2, "a stuck delivery gets a retry");
    assert!(reg.agent(&oid).unwrap().compact_pending, "still pending — waiting on THIS attempt's confirmation");

    // THIS retry's delivery confirms: the latch finally releases, exactly once.
    let confirmed = confirmed_delivery(&oid, retry_time);
    let _ = reg.compact_nudge_tick(retry_time + 1_000, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &confirmed);
    assert!(!reg.agent(&oid).unwrap().compact_pending, "confirmed — resolved");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 2, "exactly one retry, no runaway re-firing");
    // #546: this is the arm that genuinely DID confirm — our submit sampler
    // watched the Enter land — so it keeps the `-confirmed` action. The
    // negative half matters as much as the positive: without it, "rename every
    // resolution to `-liveness-only`" would pass the honest-labeling tests and
    // destroy the distinction they exist to draw.
    let confirmed = audit_entries(&reg, &gid, "compact-reinjection-confirmed");
    assert_eq!(confirmed.len(), 1);
    assert_eq!(confirmed[0]["detail"]["source"], "delivery");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-liveness-only"), 0,
        "a delivery-confirmed resolution must never be downgraded to a liveness close");
    assert_eq!(reg.agent(&oid).unwrap().compact_last_ack, Some(ReinjectAck::Delivered),
        "and the badge must be able to show the stronger evidence as the stronger evidence");
}

#[test]
fn compact_nudge_tick_abandons_a_stuck_reinjection_after_the_retry_budget_and_frees_the_latch() {
    // rev-42 delta (round 2): a reinjection that never confirms despite
    // retries must not wedge the state machine forever — a lost re-grounding
    // is a real gap, but a PERMANENTLY stuck agent (no future compaction can
    // ever arm again) is worse. Bounded give-up, audited, latch released.
    // Heuristic off, armed via `request_compact` instead — same isolation
    // reason as the retry test above (avoids the heuristic's own minutes
    // window spuriously re-elapsing across this test's ~20-minute span).
    let (reg, _d, gid, oid) = compact_nudge_setup(0);
    let empty = HashMap::new();
    let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64)].into_iter().collect();
    let timeout_ms = 5 * 60 * 1000u64; // REINJECT_CONFIRM_TIMEOUT_MS
    let mut t = FAR;
    reg.request_compact(&oid).unwrap();
    assert_eq!(
        reg.compact_nudge_tick(t, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()),
        vec![oid.clone()],
    );
    t += 1_000;
    let _ = reg.compact_nudge_tick(t, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    t += 1_000;
    let _ = reg.compact_nudge_tick(t, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1, "first attempt");
    // MAX_REINJECT_ATTEMPTS is 3 total (1 initial + 2 retries) — never
    // confirmed, so each timeout re-fires until the budget is spent.
    for attempt in 2..=3u32 {
        t += timeout_ms;
        let _ = reg.compact_nudge_tick(t, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), attempt as usize, "attempt {attempt}");
    }
    assert!(reg.agent(&oid).unwrap().compact_pending, "still pending after the 3rd attempt, awaiting its own confirm window");
    // A 4th timeout, still unconfirmed: the budget is exhausted — abandon.
    t += timeout_ms;
    let _ = reg.compact_nudge_tick(t, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(!reg.agent(&oid).unwrap().compact_pending, "abandoned — latch released rather than stuck forever");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 3, "no 4th attempt — bounded");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-abandoned"), 1);
}

/// Drive a reinjection to the point where its delivery-confirmation phase is
/// live (attempt 1 fired at `FAR + 2_000`, nothing confirmed), returning that
/// attempt's `attempted_ms`. The heuristic is off and the arm comes from
/// `request_compact` — same isolation reason as the two pre-#535 tests above:
/// a heuristic window would spuriously re-arm across the multi-minute spans
/// these tests cross.
///
/// `grew` must be the SAME map on every later tick: `compact_nudge_tick`
/// rebaselines its own output counter on every observation, so a constant
/// total reads as "no growth" — i.e. a QUIET pane — from the third tick on.
/// That is what the pre-#535 tests relied on and what #535's busy-deferral
/// test deliberately breaks by advancing the total instead.
pub(crate) fn reinject_awaiting_confirmation(reg: &OrchRegistry, oid: &str, grew: &HashMap<String, u64>) -> u64 {
    let empty = HashMap::new();
    reg.request_compact(oid).unwrap();
    let _ = reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    let _ = reg.compact_nudge_tick(FAR + 1_000, grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    let _ = reg.compact_nudge_tick(FAR + 2_000, grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    FAR + 2_000
}

pub(crate) const REINJECT_TIMEOUT_MS: u64 = 5 * 60 * 1000; // REINJECT_CONFIRM_TIMEOUT_MS
const REINJECT_BUSY_DEFER_MS: u64 = 5 * 60 * 1000; // REINJECT_BUSY_DEFER_MAX_MS
/// REINJECT_ACK_SETTLE_MS — the ordinary (i.e. UNCONDITIONAL) submit path's
/// worst case, stage by stage, deliberately spelled as addends rather than a
/// total so this stays a tripwire rather than a copy:
///   1. echo-verified typing: `ECHO_ATTEMPTS` (3) × (`ECHO_WINDOW` 2000 +
///      `ECHO_RETRY_DELAY` 1500) = 10_500 — the retry sleep runs after the
///      final attempt too
///   2. `PASTE_SUBMIT_DELAY` 500
///   3. `SUBMIT_MAX_WAIT` 45_000
///   4. `SUBMIT_CONFIRM_WINDOW` 600
///   5. sum of `SUBMIT_RETRY_DELAYS` 2_500 + 4_500 — sequential sleeps, so the
///      last blind Enter lands at +7s, not +4.5s
/// Activity before this cannot be a response to the paste on that path
/// (#535, rev-22 D1 + rev-28 Q1).
pub(crate) const REINJECT_ACK_SETTLE_MS: u64 =
    3 * (2_000 + 1_500) + 500 + 45_000 + 600 + 2_500 + 4_500;

#[test]
fn a_landed_re_grounding_is_never_re_sent_once_the_agent_itself_answers() {
    // #535, the live defect. A copilot worker that had compacted sat at 2/3
    // re-grounding attempts while visibly on-track: it had ALREADY received
    // the contract and correctly ignored the duplicate. The retry loop keyed
    // on `delivery_confirmations[..].submit_sent_ms >= attempted_ms &&
    // confirmed` — loomux watching its own submit — and that sampler misses
    // routinely on a busy/repainting pane (~25 false `delivery unconfirmed`
    // alarms in one observed session). So a re-grounding that LANDED was
    // re-pasted into a working agent, and could finish as a
    // `reinjection-abandoned` record claiming a contract restore that had in
    // fact happened.
    //
    // The fix observes the AGENT instead of our own delivery machinery: its
    // own post-attempt MCP call is the acknowledgment. Note what is NOT
    // supplied anywhere below — `confirmed_delivery(..)`. This whole test runs
    // with the old success signal permanently absent, which is exactly the
    // production case that misbehaved.
    let (reg, _d, gid, oid) = compact_nudge_setup(0);
    let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64)].into_iter().collect();
    let attempted = reinject_awaiting_confirmation(&reg, &oid, &grew);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1, "attempt 1 fired");
    assert!(reg.agent(&oid).unwrap().compact_pending, "awaiting confirmation");

    // The agent answers: it calls a loomux MCP tool once the notice has had
    // time to be submitted and read — past `REINJECT_ACK_SETTLE_MS` (rev-15
    // finding 2). That is the re-sync the notice asks for.
    reg.set_last_mcp_activity_ms_for_test(&oid, attempted + REINJECT_ACK_SETTLE_MS + 1_000);

    // Cross the retry timeout — twice over, so this cannot pass merely by
    // being early. Pre-#535 this fired attempt 2, then attempt 3.
    let _ = reg.compact_nudge_tick(attempted + REINJECT_TIMEOUT_MS, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    let _ = reg.compact_nudge_tick(attempted + 2 * REINJECT_TIMEOUT_MS, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());

    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1,
        "the agent answered after the paste — the re-grounding LANDED, so it must never be re-sent");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-abandoned"), 0,
        "and it must never be recorded as a lost re-grounding");
    let a = reg.agent(&oid).unwrap();
    assert!(!a.compact_pending, "the latch resolves on the acknowledgment, not on a delivery confirmation");
    assert_eq!(a.compact_reinject_attempts, 0, "phase closed");
    assert!(a.compact_last_lost_reason.is_none(), "nothing was lost");
    // Provenance: "the agent answered us" and "our submit sampler saw the
    // Enter" are different facts, and a timeline that conflated them could not
    // show whether this fix is working in the field. #546: the ACTION NAME
    // carries that difference — see `a_re_grounding_closed_on_liveness_is_
    // never_audited_as_confirmed` for why the record cannot use `-confirmed`
    // here.
    let liveness = audit_entries(&reg, &gid, "compact-reinjection-liveness-only");
    assert_eq!(liveness.len(), 1);
    assert_eq!(liveness[0]["detail"]["source"], "activity",
        "resolved by the agent's own activity, not by a delivery confirmation");
    // #546: the same provenance is kept on the ENTRY, not only in the audit
    // line, because the audit log is not what a human watching a pane reads.
    // Pinned at the resolve site rather than only against `compaction_status`:
    // a test that stops at the pure derivation would stay green if this wiring
    // were never written at all.
    assert_eq!(a.compact_last_ack, Some(ReinjectAck::LivenessOnly),
        "the panel must be able to say this phase closed on liveness, not on a proven read");
    assert!(a.compact_last_ack_ms.is_some(), "and when, so the surfacing can age out");
}

#[test]
fn an_mcp_call_before_the_re_grounding_is_not_an_acknowledgment_of_it() {
    // The pair that proves the test above is reading the COMPARISON and not
    // merely "the field is non-zero". An agent that was chatty right up to the
    // moment it compacted, then went silent, has not acknowledged anything —
    // its re-grounding is genuinely unanswered and must still retry.
    let (reg, _d, gid, oid) = compact_nudge_setup(0);
    let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64)].into_iter().collect();
    let attempted = reinject_awaiting_confirmation(&reg, &oid, &grew);
    reg.set_last_mcp_activity_ms_for_test(&oid, attempted - 1);

    let _ = reg.compact_nudge_tick(attempted + REINJECT_TIMEOUT_MS, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 2,
        "an ack from BEFORE the attempt says nothing about this attempt — retry, exactly as before #535");
}

#[test]
fn an_in_flight_tool_call_from_the_previous_turn_is_not_an_acknowledgment() {
    // rev-15 finding 2. `attempted_ms` is when loomux DECIDED to paste — the
    // agent cannot have read anything until the paste is written and the Enter
    // actually pressed, which `deliver_prompt` may defer by up to
    // `SUBMIT_MAX_WAIT` (45s) plus its blind-retry tail. So a tool call the
    // agent had already decided on during its PREVIOUS turn, landing
    // milliseconds after the decision, must not resolve the phase: that is a
    // false landed-signal, the same family #112 (output-burst) and #522
    // (idle-pane) exist to kill, arriving through a different door.
    //
    // This is the pair to `a_landed_re_grounding_is_never_re_sent_once_the_
    // agent_itself_answers` above — identical setup, the ONLY difference being
    // where the stamp falls relative to the settling floor.
    //
    // It is also the DRIFT TRIPWIRE for that floor (rev-22 D2), and the guard is
    // bidirectional. Production's `REINJECT_ACK_SETTLE_MS` is a const expression
    // over all five unconditional submit-path stages; the mirror above is spelled
    // as those same addends and deliberately does NOT track. So:
    //   - a stage lengthened in production (a third `SUBMIT_RETRY_DELAYS` entry,
    //     a longer `ECHO_WINDOW`) widens the real floor, the "exactly at the
    //     floor" stamp lands inside it, and the resolve assertion fails;
    //   - a stage shortened lets the "1ms short" stamp clear the narrower floor,
    //     resolve early, and the attempt-count assertion fails.
    // Either way a human is told the acknowledgment window moved. Before the
    // const expression existed, changing any of those constants could widen the
    // real false-ack window with the whole suite green.
    let (reg, _d, gid, oid) = compact_nudge_setup(0);
    let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64)].into_iter().collect();
    let attempted = reinject_awaiting_confirmation(&reg, &oid, &grew);

    // 200ms after the paste decision — an HTTP round trip, not a turn.
    reg.set_last_mcp_activity_ms_for_test(&oid, attempted + 200);
    let _ = reg.compact_nudge_tick(attempted + REINJECT_TIMEOUT_MS, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 2,
        "a call that was in flight before the Enter was pressed is not a response to it — retry");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-confirmed"), 0,
        "and it must never be recorded as a landing");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-liveness-only"), 0,
        "nor as a liveness close — nothing resolved at all here");

    // The boundary itself, on the retry that just fired: one millisecond short
    // of the floor still does not count…
    let retried = attempted + REINJECT_TIMEOUT_MS;
    reg.set_last_mcp_activity_ms_for_test(&oid, retried + REINJECT_ACK_SETTLE_MS - 1);
    let _ = reg.compact_nudge_tick(retried + REINJECT_TIMEOUT_MS, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 3, "still inside the floor — attempt 3");

    // …and one millisecond past it does. Without this half, a floor set so
    // wide that NOTHING ever acks would pass the assertions above.
    let retried2 = retried + REINJECT_TIMEOUT_MS;
    reg.set_last_mcp_activity_ms_for_test(&oid, retried2 + REINJECT_ACK_SETTLE_MS);
    let _ = reg.compact_nudge_tick(retried2 + REINJECT_TIMEOUT_MS, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(!reg.agent(&oid).unwrap().compact_pending, "at the floor, an ack resolves — the floor delays the signal, it does not remove it");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-abandoned"), 0);
    let liveness = audit_entries(&reg, &gid, "compact-reinjection-liveness-only");
    assert_eq!(liveness.len(), 1);
    assert_eq!(liveness[0]["detail"]["source"], "activity");
}

#[test]
fn a_genuinely_lost_re_grounding_still_retries_and_still_abandons_at_the_bound() {
    // Scope item 3: #535 narrows WHEN a retry fires; it does not remove the
    // safety net. An agent that never answers and never confirms — a truly
    // lost re-grounding — must still get its bounded retries and must still
    // end in the visible `reinjection-abandoned` record, unchanged.
    //
    // The pane is silent throughout (`grew` constant ⇒ no growth from the
    // third tick on), so the busy deferral is never in play here; that arm has
    // its own test below.
    let (reg, _d, gid, oid) = compact_nudge_setup(0);
    let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64)].into_iter().collect();
    let mut t = reinject_awaiting_confirmation(&reg, &oid, &grew);
    assert_eq!(reg.agent(&oid).unwrap().last_mcp_activity_ms, 0, "this agent never calls a tool");

    for attempt in 2..=3u32 {
        t += REINJECT_TIMEOUT_MS;
        let _ = reg.compact_nudge_tick(t, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), attempt as usize, "attempt {attempt}");
    }
    t += REINJECT_TIMEOUT_MS;
    let _ = reg.compact_nudge_tick(t, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());

    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 3, "still bounded at MAX_REINJECT_ATTEMPTS");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-abandoned"), 1, "the safety net survives #535");
    let a = reg.agent(&oid).unwrap();
    assert!(!a.compact_pending, "latch released rather than wedged");
    assert_eq!(a.compact_last_lost_reason.as_deref(), Some("reinjection-abandoned"));
}

#[test]
fn a_retry_is_never_spent_into_a_live_turn_but_the_deferral_is_bounded() {
    // #535 scope item 2: the 5-minute timer fires blind today, so a retry can
    // land mid-turn — the single worst moment for it, since the agent is
    // demonstrably working. Defer instead. And bound the deferral: a pane that
    // never goes quiet (an animated statusline above the activity floor) must
    // not be able to suppress the retry forever, or scope item 3's safety net
    // dies quietly on exactly the panes that misreport most.
    let (reg, _d, gid, oid) = compact_nudge_setup(0);
    let mut total = 50_000u64;
    let flat: HashMap<String, u64> = [(oid.clone(), total)].into_iter().collect();
    let attempted = reinject_awaiting_confirmation(&reg, &oid, &flat);
    // Output ADVANCING on every tick — a live turn, unlike every other test
    // here, which holds the counter constant so the pane reads as quiet.
    let mut busy_tick = |t: u64| {
        total += 50_000;
        let m: HashMap<String, u64> = [(oid.clone(), total)].into_iter().collect();
        let _ = reg.compact_nudge_tick(t, &m, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    };

    // Timeout elapsed, but the pane is mid-turn: no attempt is spent.
    busy_tick(attempted + REINJECT_TIMEOUT_MS);
    busy_tick(attempted + REINJECT_TIMEOUT_MS + 10_000);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1,
        "no retry pasted into a live turn");
    assert_eq!(reg.agent(&oid).unwrap().compact_reinject_attempts, 1, "the attempt was deferred, not spent");
    // Audited once per attempt, not once per 10s poll — "no retry fired" must
    // read as a deliberate choice, and must not flood the timeline to say so.
    let deferred = audit_entries(&reg, &gid, "compact-reinjection-deferred-busy");
    assert_eq!(deferred.len(), 1, "anti-nag latch: one line per attempt, not one per tick");
    assert_eq!(deferred[0]["detail"]["attempt"], 1);

    // The deferral does NOT reset the clock: it is bounded from the ORIGINAL
    // attempt, so a pane that keeps painting cannot extend it a tick at a time.
    busy_tick(attempted + REINJECT_TIMEOUT_MS + REINJECT_BUSY_DEFER_MS);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 2,
        "the busy deferral is bounded — a permanently noisy pane cannot suppress the retry forever");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-deferred-busy"), 1,
        "the retry that finally fired is not itself a deferral");
    assert_eq!(reg.agent(&oid).unwrap().compact_reinject_attempts, 2);
}

#[test]
fn a_permanently_busy_pane_still_reaches_the_abandoned_record_and_the_lost_badge() {
    // rev-15 finding 3. `REINJECT_BUSY_DEFER_MAX_MS`'s doc makes a load-bearing
    // claim — a pane that never goes quiet "must not be able to suppress the
    // retry forever, or scope item 3's safety net for a genuinely LOST
    // re-grounding dies with it". The test above only drives that as far as
    // attempt 2, and the pure-function table only pins one transition. Neither
    // watches the ESCALATION actually happen, and "a bound whose exhaustion
    // nobody has watched" is the #513 shape wearing a bound: a future edit that
    // reset `compact_reinject_attempted_ms` in the `DeferBusy` arm would make
    // `elapsed` restart every tick and hang forever, with the whole suite green.
    //
    // So: output advancing on EVERY tick from first attempt to terminal state,
    // never once quiet, and assert the terminal state plus the reason the
    // frontend actually renders — `compactionstatus.ts` keys the "re-grounding
    // lost" badge off `compact_last_lost_reason`, so that field, not the audit
    // line, is what makes this visible to a human.
    let (reg, _d, gid, oid) = compact_nudge_setup(0);
    let mut total = 50_000u64;
    let flat: HashMap<String, u64> = [(oid.clone(), total)].into_iter().collect();
    let attempted = reinject_awaiting_confirmation(&reg, &oid, &flat);
    let mut busy_tick = |t: u64| {
        total += 50_000; // always above `idle_activity_floor_bytes` — never quiet
        let m: HashMap<String, u64> = [(oid.clone(), total)].into_iter().collect();
        let _ = reg.compact_nudge_tick(t, &m, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    };

    // Each attempt costs its full window plus its full busy deferral, since the
    // pane is never quiet: 3 attempts, then a 4th expiry that abandons.
    let cycle = REINJECT_TIMEOUT_MS + REINJECT_BUSY_DEFER_MS;
    let mut t = attempted;
    for attempt in 2..=3u32 {
        busy_tick(t + REINJECT_TIMEOUT_MS); // deferred, not spent
        assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), attempt as usize - 1,
            "attempt {attempt} must still be deferred while the pane is mid-turn");
        t += cycle;
        busy_tick(t);
        assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), attempt as usize,
            "the deferral is bounded — attempt {attempt} fires even though the pane never went quiet");
    }
    assert!(reg.agent(&oid).unwrap().compact_pending, "3rd attempt still awaiting its own window");

    // The 4th expiry, pane STILL busy: the budget is spent, so it must abandon
    // rather than defer forever.
    busy_tick(t + REINJECT_TIMEOUT_MS);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 3, "still bounded at MAX_REINJECT_ATTEMPTS");
    t += cycle;
    busy_tick(t);

    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 3, "no 4th attempt");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-abandoned"), 1,
        "a permanently busy pane still reaches the give-up — the deferral bounds the wait, it does not remove the escalation");
    let a = reg.agent(&oid).unwrap();
    assert!(!a.compact_pending, "latch released rather than wedged");
    assert_eq!(a.compact_reinject_attempts, 0);
    assert!(!a.compact_reinject_busy_deferred, "the deferral latch is cleared with the phase");
    assert_eq!(a.compact_last_lost_reason.as_deref(), Some("reinjection-abandoned"),
        "the badge-visible reason — this is what `compactionstatus.ts` renders as 're-grounding lost'");
    assert!(a.compact_last_lost_ms.is_some(), "and it is stamped, so the badge can age out");
}

#[test]
fn a_deferral_yields_to_the_acknowledgment_it_was_waiting_for() {
    // Ordering, stated as behaviour rather than as a claim in a doc comment:
    // evidence of a landing outranks both the clock and busy-ness. A pane that
    // is mid-turn AND has answered must resolve, never sit in DeferBusy —
    // otherwise #535's own fix would be gated behind a pane going quiet.
    let (reg, _d, gid, oid) = compact_nudge_setup(0);
    let mut total = 50_000u64;
    let flat: HashMap<String, u64> = [(oid.clone(), total)].into_iter().collect();
    let attempted = reinject_awaiting_confirmation(&reg, &oid, &flat);
    reg.set_last_mcp_activity_ms_for_test(&oid, attempted + REINJECT_ACK_SETTLE_MS + 500);

    total += 50_000;
    let busy: HashMap<String, u64> = [(oid.clone(), total)].into_iter().collect();
    let _ = reg.compact_nudge_tick(attempted + REINJECT_TIMEOUT_MS, &busy, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());

    assert!(!reg.agent(&oid).unwrap().compact_pending, "resolved on the ack even though the pane is busy");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-deferred-busy"), 0, "an answered attempt is not a deferred one");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1);
}

#[test]
fn every_mcp_tool_call_stamps_the_shared_activity_clock_for_every_role() {
    // Pins the WIRING, not the predicate: the clock is stamped once at the
    // `tools/call` dispatch funnel, so it covers every tool and every role
    // with no per-tool opt-in to forget. `note_agent_activity` — the clock
    // that already existed — is stamped from exactly ONE tool
    // (`message_orchestrator`) and is an explicit no-op for the orchestrator,
    // which is why it could not serve this: an orchestrator compacts and gets
    // re-grounded like anything else.
    let (reg, _d, _gid, oid) = compact_nudge_setup(0);
    assert_eq!(reg.last_mcp_activity_ms(&oid), Some(0), "never called a tool yet");

    let token = reg.agent(&oid).unwrap().token;
    let caller = reg.resolve_token(&token).unwrap();
    dispatch(&reg, &caller, "tools/call", &json!({ "name": "get_state", "arguments": {} })).unwrap();
    let after_ok = reg.last_mcp_activity_ms(&oid).unwrap();
    assert!(after_ok > 0, "an ORCHESTRATOR's tool call stamps the clock — the watchdog clock skips this role entirely");

    // A REJECTED call stamps too: the claim is "this agent's own process is
    // alive and executing", which a permission denial proves exactly as well.
    reg.set_last_mcp_activity_ms_for_test(&oid, 0);
    let rejected = dispatch(&reg, &caller, "tools/call",
        &json!({ "name": "report", "arguments": { "outcome": "done", "note": "x" } })).unwrap();
    assert_eq!(rejected["isError"], json!(true),
        "precondition: `report` is workers/reviewers only, so this call really is rejected");
    assert!(reg.last_mcp_activity_ms(&oid).unwrap() > 0,
        "a REJECTED call still stamps — the claim is 'this agent's process is executing', which a denial proves too");

    // Monotone: a later stamp never rewinds the clock, so one consumer cannot
    // erase another's evidence.
    let high = u64::MAX;
    reg.set_last_mcp_activity_ms_for_test(&oid, high);
    dispatch(&reg, &caller, "tools/call", &json!({ "name": "get_state", "arguments": {} })).unwrap();
    assert_eq!(reg.last_mcp_activity_ms(&oid), Some(high), "note_agent_ack is max(), never a bare assignment");

    // Unknown ids are simply absent rather than a special case.
    assert_eq!(reg.last_mcp_activity_ms("nope"), None);
}

#[test]
fn reinject_disposition_table() {
    use ReinjectDisposition::*;
    const T: u64 = 5 * 60 * 1000;
    const B: u64 = 5 * 60 * 1000;

    // Evidence of a landing beats the clock — the whole point of #535. Both
    // sources, at every stage of the window.
    for elapsed in [0, T - 1, T, T + B, 10 * T] {
        assert_eq!(reinject_disposition(true, false, false, elapsed, T, B), Resolved, "confirmed delivery @{elapsed}");
        assert_eq!(reinject_disposition(false, true, false, elapsed, T, B), Resolved, "agent acked @{elapsed}");
        assert_eq!(reinject_disposition(false, true, true, elapsed, T, B), Resolved,
            "an ack resolves even mid-turn @{elapsed} — the fix must not be gated behind a pane going quiet");
    }
    // Inside the window: wait, whatever the pane is doing. A delivery may
    // legitimately still be in flight, and our own pasted notice echoing back
    // is exactly the output that would otherwise read as "busy" here.
    assert_eq!(reinject_disposition(false, false, false, T - 1, T, B), Wait);
    assert_eq!(reinject_disposition(false, false, true, T - 1, T, B), Wait,
        "the clock outranks busy-ness — a chatty pane must not defer before it has even waited");
    // Window spent: quiet retries, busy defers.
    assert_eq!(reinject_disposition(false, false, false, T, T, B), Retry);
    assert_eq!(reinject_disposition(false, false, true, T, T, B), DeferBusy);
    // …and the deferral is bounded, measured from the attempt.
    assert_eq!(reinject_disposition(false, false, true, T + B - 1, T, B), DeferBusy);
    assert_eq!(reinject_disposition(false, false, true, T + B, T, B), Retry,
        "a permanently noisy pane cannot suppress the retry forever");
    // `busy_defer_max_ms == 0` degrades to pre-#535 behaviour (fire blind),
    // never to "defer nothing, ever" — the same convention as #518's bound.
    assert_eq!(reinject_disposition(false, false, true, T, T, 0), Retry);
    assert_eq!(reinject_disposition(false, false, true, T - 1, T, 0), Wait);
}

#[test]
fn agent_acted_since_reads_the_comparison_not_the_presence() {
    // `>=`, not `>`: the stamp and the attempt are both `now_ms()` readings
    // and a fast agent can genuinely answer inside the same millisecond.
    // Erring the other way discards the very ack this exists to notice — the
    // same off-by-one that made one of #518's tests pass for the wrong reason.
    assert!(agent_acted_since(1_000, 1_000), "same-millisecond ack counts");
    assert!(agent_acted_since(1_001, 1_000));
    assert!(!agent_acted_since(999, 1_000), "an ack from before the attempt says nothing about it");
    // `0` is "never called" and can never satisfy a real attempt…
    assert!(!agent_acted_since(0, 1_000));
    // …including the degenerate `attempted_ms == 0`, which a `>=` alone would
    // wrongly call an ack for an agent that has never made a single call.
    assert!(!agent_acted_since(0, 0));
    assert!(agent_acted_since(1, 0));
}

#[test]
fn compact_nudge_tick_times_out_a_stuck_arm_that_never_reaches_a_busy_then_quiet_resolution() {
    // #410 (round 6): live evidence (a user demo, testbed group `loomux-
    // testbed-cc077f09`) showed `request_compact` answered "a compact is
    // already in flight for this pane" for 10+ minutes — an inference arm
    // kept re-arming (most likely the auto-compact banner, given the
    // session's accumulated size) and correctly resolving to a DISCARD each
    // time (D4 held — no reinjection loop), but the cycling meant `compact_
    // pending` was essentially never open long enough for the queued
    // request to win the race. Whatever the exact mechanism, an arm that
    // never reaches a busy-then-quiet resolution at all must not wedge the
    // state machine forever — symmetric to the delivery-confirmation
    // phase's own bounded retry/abandon.
    let (reg, _d, gid, oid) = compact_nudge_setup(0); // heuristic off — isolate manual detection
    let signals: HashMap<String, (String, u64)> =
        [(oid.clone(), ("> /compact\n".to_string(), 500u64))].into_iter().collect();
    let empty = HashMap::new();
    let _ = reg.compact_nudge_tick(500, &empty, &signals, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(reg.agent(&oid).unwrap().compact_pending, "manual /compact starts pending tracking");

    // No growth is EVER observed (compact_seen_busy never sets) and no
    // marker ever rises — a genuinely stuck arm. Right up to, but not past,
    // the arm-pending timeout.
    let timeout_ms = 5 * 60 * 1000u64; // ARM_PENDING_TIMEOUT_MS
    let still_stuck = reg.compact_nudge_tick(
        500 + timeout_ms - 1, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(still_stuck.is_empty());
    assert!(reg.agent(&oid).unwrap().compact_pending, "still pending — not yet timed out");
    assert_eq!(audit_count(&reg, &gid, "compact-arm-timeout"), 0);

    // Timeout elapses: forced abandon, latch released.
    let _ = reg.compact_nudge_tick(
        500 + timeout_ms, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(!reg.agent(&oid).unwrap().compact_pending, "timed out — latch released rather than stuck forever");
    assert_eq!(audit_count(&reg, &gid, "compact-arm-timeout"), 1);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 0, "never a reinjection — no compaction was ever confirmed");
    assert_eq!(audit_count(&reg, &gid, "compact-pending-discarded"), 0, "distinct from an ordinary discard — this is a stuck-arm timeout");
    // Round 7: no hook evidence was ever recorded for this manual-detection
    // arm, so the plain "no evidence" label is honest here — unlike the
    // precompact-only case right below, which gets the `-with-evidence`
    // variant.
    assert_eq!(reg.agent(&oid).unwrap().compact_last_lost_reason.as_deref(), Some("arm-timeout"));
}

#[test]
fn compact_nudge_tick_precompact_only_arm_that_stalls_times_out_labeled_with_evidence() {
    // Round 7 (label-honesty finding): unlike a SessionStart-evidenced arm
    // (which now resolves immediately — see the round-7 tests above), a
    // PreCompact-ONLY arm (no SessionStart wired — e.g. Copilot, or a
    // Claude config missing that one hook) still genuinely needs a later
    // quiet observation, and can still legitimately hit `ARM_PENDING_
    // TIMEOUT_MS` if the agent's own turn simply never settles. Hook
    // evidence WAS recorded here, so the abandoned reason must say so —
    // "no evidence" (the plain `arm-timeout` label) would be factually
    // wrong for this case.
    let (reg, _d, gid, oid) = compact_nudge_setup(20);
    let started_ms = reg.agent(&oid).unwrap().started_ms;

    let marker = reg.state_root().join(gid.as_str()).join("hooks").join(format!("{oid}.precompact.json"));
    write_hook_marker(&marker, started_ms, 1_000);
    // Busy on the arming tick too — otherwise a quiet first observation
    // would resolve it via reinjection immediately, same as `compact_nudge_
    // tick_a_precompact_only_arm_still_gets_loomuxs_own_reinjection` already
    // covers; this test wants the genuinely-stuck shape instead.
    let busy: HashMap<String, u64> = [(oid.clone(), 500_000u64)].into_iter().collect();
    assert!(reg.compact_nudge_tick(1_000, &busy, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    assert!(reg.agent(&oid).unwrap().compact_pending, "precompact arms and waits for quiet");
    assert_eq!(reg.agent(&oid).unwrap().compact_pending_evidence, Some("hook"));

    // The pane stays BUSY on the next observed tick too — never quiet —
    // right up through ARM_PENDING_TIMEOUT_MS.
    let timeout_ms = 5 * 60 * 1000u64;
    let still_busy: HashMap<String, u64> = [(oid.clone(), 5_000_000u64)].into_iter().collect();
    let _ = reg.compact_nudge_tick(
        1_000 + timeout_ms, &still_busy, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(!reg.agent(&oid).unwrap().compact_pending, "timed out — latch released");
    assert_eq!(audit_count(&reg, &gid, "compact-arm-timeout"), 1);
    assert_eq!(
        reg.agent(&oid).unwrap().compact_last_lost_reason.as_deref(),
        Some("arm-timeout-with-evidence"),
        "hook evidence was recorded — the reason must say so, not claim none was seen"
    );
}

#[test]
fn compact_nudge_tick_arms_cleanly_via_request_compact_after_an_arm_timeout() {
    // #410 end-to-end: an arm-timeout is not a dead end — a subsequent
    // request_compact arms and fires normally, proving the two mechanisms
    // compose within one agent's lifetime.
    let (reg, _d, gid, oid) = compact_nudge_setup(0);
    let signals: HashMap<String, (String, u64)> =
        [(oid.clone(), ("> /compact\n".to_string(), 500u64))].into_iter().collect();
    let empty = HashMap::new();
    let _ = reg.compact_nudge_tick(500, &empty, &signals, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    let timeout_ms = 5 * 60 * 1000u64; // ARM_PENDING_TIMEOUT_MS
    let _ = reg.compact_nudge_tick(
        500 + timeout_ms, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(!reg.agent(&oid).unwrap().compact_pending);
    assert_eq!(audit_count(&reg, &gid, "compact-arm-timeout"), 1);

    // Re-request: arms and fires cleanly.
    reg.request_compact(&oid).unwrap();
    let t2 = 500 + timeout_ms + 1_000;
    let nudged = reg.compact_nudge_tick(t2, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert_eq!(nudged, vec![oid.clone()], "a subsequent request arms cleanly after the timeout");
    assert!(reg.agent(&oid).unwrap().compact_pending_trusted);
}

#[test]
fn compact_nudge_tick_lets_a_queued_request_win_the_race_against_a_same_tick_inference_rearm() {
    // #410 (round 6) forensic finding: the live incident's discards (three
    // `compact-pending-discarded` audits, ~2-3 minutes apart, each with an
    // unchanged token reading) prove D4 held — no reinjection loop — but the
    // user's `request_compact` still sat "already in flight" for 10+
    // minutes. The mechanism: on the exact tick a discard clears `compact_
    // pending`, an inference arm (manual detection, gated only on recency +
    // a tail match — no `!currently_quiet` requirement, unlike the banner
    // detector) can re-satisfy its OWN condition on that SAME tick and
    // re-arm BEFORE the already-queued, deterministic request ever gets a
    // turn — in the OLD per-agent iteration order. This pins the fix: the
    // loomux-initiated fire-check now runs FIRST, so a queued request always
    // gets first refusal on the tick its predecessor resolves.
    let (reg, _d, gid, oid) = compact_nudge_setup(0); // heuristic off — isolate the two arms
    let signals: HashMap<String, (String, u64)> =
        [(oid.clone(), ("> /compact\n".to_string(), 500u64))].into_iter().collect();
    let empty = HashMap::new();
    // Arm via manual detection.
    let _ = reg.compact_nudge_tick(500, &empty, &signals, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(reg.agent(&oid).unwrap().compact_pending);
    // Busy tick.
    let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64)].into_iter().collect();
    let _ = reg.compact_nudge_tick(600, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    // The user's explicit request queues BEFORE the resolve tick.
    reg.request_compact(&oid).unwrap();
    // Resolve tick: quiet (no further growth) — the arm resolves to a
    // DISCARD (no token drop, no marker). The tail STILL shows a fresh
    // `/compact`-matching line at THIS exact tick's timestamp — simulating
    // the human still actively typing near the discard, exactly the
    // condition that let manual detection re-arm on the same tick under the
    // old ordering.
    let still_typing: HashMap<String, (String, u64)> =
        [(oid.clone(), ("> /compact\n".to_string(), 700u64))].into_iter().collect();
    let nudged = reg.compact_nudge_tick(700, &grew, &still_typing, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert_eq!(audit_count(&reg, &gid, "compact-pending-discarded"), 1, "the stale arm still resolves to a discard first");
    assert_eq!(nudged, vec![oid.clone()],
        "the queued request must win the race against a same-tick manual re-arm, not be starved by it");
    assert!(reg.agent(&oid).unwrap().compact_pending_trusted,
        "the winning arm is the loomux-initiated request, not a manual-detection re-arm");
}

#[test]
fn mcp_request_compact_dispatches_end_to_end() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let caller = reg.resolve_token(&o.token).unwrap();
    let resp =
        dispatch(&reg, &caller, "tools/call", &json!({ "name": "request_compact", "arguments": {} })).unwrap();
    assert_eq!(resp["isError"], false);
    let text = resp["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("compact requested"), "got: {text}");
    assert!(reg.agent(&o.id).unwrap().compact_requested);
}

#[test]
fn mcp_request_compact_is_listed_for_every_non_solo_role() {
    for role in [Role::Orchestrator, Role::Worker, Role::Reviewer, Role::Planner] {
        let (reg, _d) = test_registry();
        let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
        let task = if role == Role::Orchestrator { "" } else { "t" };
        let a = reg.spawn_agent(&g.id, role, "a", task, false, None).unwrap();
        let caller = reg.resolve_token(&a.token).unwrap();
        let resp = dispatch(&reg, &caller, "tools/list", &json!({})).unwrap();
        let names: Vec<&str> = resp["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"request_compact"), "{role:?} must see request_compact, got: {names:?}");
    }
}

#[test]
fn set_state_stamps_last_state_write_for_the_checklist_warning() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let caller = reg.resolve_token(&o.token).unwrap();
    assert_eq!(reg.agent(&o.id).unwrap().last_state_write_ms, 0, "never called yet");
    let _ = dispatch(&reg, &caller, "tools/call",
        &json!({ "name": "set_state", "arguments": { "state": "{}" } }));
    assert!(reg.agent(&o.id).unwrap().last_state_write_ms > 0, "set_state via MCP must stamp the timestamp");
}
