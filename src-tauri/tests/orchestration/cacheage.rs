//! The pane cache-age timer's backend half.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// #3407 — the pane cache-age timer's backend half: the activity fold on the
// usage merge, the resolved TTL on each usage row, the human "Compact now",
// and the orchestrator's idle-compact backstop.
// ---------------------------------------------------------------------------

const MIN: u64 = 60_000;

/// One usage row, `input` tokens, observed at `at` — the shape the usage tick
/// writes, with a distinctive key so it cannot collide with a live agent's.
fn cache_snap(input: u64, cache_read: u64, cost: f64, at: u64) -> UsageSnapshot {
    UsageSnapshot {
        cache_read_tokens: cache_read,
        updated_ms: at,
        ..usage_snap("sess-cacheage", "w-cacheage", cost, input, 0)
    }
}

fn usage_row<'a>(usage: &'a Value, key_agent: &str) -> &'a Value {
    usage["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == key_agent)
        .unwrap_or_else(|| panic!("no usage row for {key_agent}: {usage}"))
}

#[test]
fn a_usage_row_records_when_its_counters_last_moved_and_what_the_wake_cost() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let t0 = 1_000 * MIN;
    // First sighting: the row arrives carrying history, so nothing is known
    // about when its last request was — the fold refuses to charge a
    // cumulative total to one wake.
    reg.upsert_usage_snapshot(&g.id, cache_snap(100, 0, 1.0, t0));
    let u = reg.group_usage(&g.id);
    let row = usage_row(&u, "w-cacheage");
    assert_eq!(row["last_active_ms"], Value::Null, "first sighting is unknown: {row}");
    assert_eq!(row["last_wake"], Value::Null);

    // Ten quiet minutes, then the counters move: a wake, carrying the DELTA.
    let t1 = t0 + 10 * MIN;
    reg.upsert_usage_snapshot(&g.id, cache_snap(150, 900_000, 1.5, t1));
    let u = reg.group_usage(&g.id);
    let row = usage_row(&u, "w-cacheage");
    assert_eq!(row["last_active_ms"], json!(t1));
    assert_eq!(row["last_wake"]["at_ms"], json!(t1));
    assert_eq!(row["last_wake"]["input_tokens"], json!(50));
    assert_eq!(row["last_wake"]["cache_read_tokens"], json!(900_000));
    assert_eq!(row["last_wake"]["idle_before_ms"], Value::Null, "the gap before a first-ever movement is unknown");

    // Movement inside the wake gap is the same turn: the clock advances, the
    // recorded wake stands.
    let t2 = t1 + 20_000;
    reg.upsert_usage_snapshot(&g.id, cache_snap(160, 900_000, 1.6, t2));
    let row = usage_row(&reg.group_usage(&g.id), "w-cacheage").clone();
    assert_eq!(row["last_active_ms"], json!(t2));
    assert_eq!(row["last_wake"]["at_ms"], json!(t1));

    // An unmoved reading is not a request, whatever its timestamp.
    reg.upsert_usage_snapshot(&g.id, cache_snap(160, 900_000, 1.6, t2 + 30 * MIN));
    let row = usage_row(&reg.group_usage(&g.id), "w-cacheage").clone();
    assert_eq!(row["last_active_ms"], json!(t2), "no growth, no activity");

    // The next wake after a real gap carries its measured gap.
    let t3 = t2 + 7 * MIN;
    reg.upsert_usage_snapshot(&g.id, cache_snap(170, 1_000_000, 1.7, t3));
    let row = usage_row(&reg.group_usage(&g.id), "w-cacheage").clone();
    assert_eq!(row["last_wake"]["idle_before_ms"], json!(7 * MIN));
    assert_eq!(row["last_wake"]["cache_read_tokens"], json!(100_000));

    // The activity is durable: it is on disk in usage.json, not only in the
    // value this call computed.
    let disk = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("usage.json")).unwrap();
    assert!(disk.contains(&format!("\"last_active_ms\": {t3}")), "{disk}");
}

#[test]
fn a_usage_row_carries_its_resolved_ttl_from_the_cli_or_the_block() {
    let (reg, _d) = test_registry();
    let mut r = rails();
    let g = reg.create_group("C:/tmp/repo", r.clone()).unwrap();
    reg.upsert_usage_snapshot(&g.id, cache_snap(1, 0, 0.0, MIN));
    let row = usage_row(&reg.group_usage(&g.id), "w-cacheage").clone();
    // The row's block is `worker`, on claude: CliCaps' conservative five.
    assert_eq!(row["cache_ttl_minutes"], json!(5));
    assert_eq!(row["cache_cooling_after_ms"], json!(3 * MIN));
    assert_eq!(row["compact_supported"], json!(true));

    // The block override wins over the CLI default.
    r.blocks.iter_mut().find(|b| b.id == "worker").unwrap().cache_ttl_minutes = Some(60);
    let g2 = reg.create_group("C:/tmp/repo2", r.clone()).unwrap();
    reg.upsert_usage_snapshot(&g2.id, cache_snap(1, 0, 0.0, MIN));
    let row = usage_row(&reg.group_usage(&g2.id), "w-cacheage").clone();
    assert_eq!(row["cache_ttl_minutes"], json!(60));
    assert_eq!(row["cache_cooling_after_ms"], json!(48 * MIN));

    // `0` is "unknown": no TTL, so no cooling threshold either.
    r.blocks.iter_mut().find(|b| b.id == "worker").unwrap().cache_ttl_minutes = Some(0);
    let g3 = reg.create_group("C:/tmp/repo3", r).unwrap();
    reg.upsert_usage_snapshot(&g3.id, cache_snap(1, 0, 0.0, MIN));
    let row = usage_row(&reg.group_usage(&g3.id), "w-cacheage").clone();
    assert_eq!(row["cache_ttl_minutes"], Value::Null);
    assert_eq!(row["cache_cooling_after_ms"], Value::Null);
}

#[test]
fn the_humans_compact_now_rides_the_request_compact_path_and_refuses_another_group() {
    let (reg, _d, gid, oid) = compact_nudge_setup(0);
    let other = reg.create_group("C:/tmp/other", rails()).unwrap();
    // Membership is a separate check from holding a valid group id.
    let err = reg.human_request_compact(&other.id, &oid).unwrap_err();
    assert!(err.contains("not in this group"), "{err}");
    assert_eq!(audit_count(&reg, &gid, "compact-requested"), 0);
    assert!(reg.human_request_compact(&gid, "no-such-agent").is_err());

    let ok = reg.human_request_compact(&gid, &oid).expect("a claude orchestrator can compact");
    assert!(ok.contains("next idle moment"), "{ok}");
    assert_eq!(audit_count(&reg, &gid, "compact-requested"), 1);
    // The request is honoured by the ONE place that types /compact — the
    // compact-nudge tick — on its next quiet observation, even with the
    // heuristic lull timer off.
    let empty = HashMap::new();
    assert_eq!(
        reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()),
        vec![oid.clone()],
        "a human's Compact now must fire /compact at the next quiet tick"
    );
    assert_eq!(audit_count(&reg, &gid, "compact-nudge"), 1);
}

/// A group whose orchestrator went output-quiet at `quiet_since`: one
/// compact-nudge tick with real growth stamps the quiet clock there. The lull
/// timer is off, so nothing else in that tick can fire.
fn idle_orch_setup(quiet_since: u64) -> (OrchRegistry, tempfile::TempDir, GroupId, String) {
    let (reg, d, gid, oid) = compact_nudge_setup(0);
    let grew: HashMap<String, u64> = [(oid.clone(), 64 * 1024u64)].into_iter().collect();
    let none = HashMap::new();
    reg.compact_nudge_tick(quiet_since, &grew, &none, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    (reg, d, gid, oid)
}

pub(crate) fn pct(oid: &str, p: u32) -> HashMap<String, u32> {
    [(oid.to_string(), p)].into_iter().collect()
}

#[test]
fn an_idle_orchestrator_with_nothing_in_flight_is_told_once_to_compact_before_the_cache_cools() {
    let t0 = 1_000 * MIN;
    let (reg, _d, gid, oid) = idle_orch_setup(t0);
    // Below the band (claude's 5 min TTL cools from 3 min): nothing yet.
    assert!(reg.cache_idle_nudge_tick(t0 + 2 * MIN, &pct(&oid, 70)).is_empty());
    // Inside the band: one nudge, audited.
    assert_eq!(reg.cache_idle_nudge_tick(t0 + 4 * MIN, &pct(&oid, 70)), vec![oid.clone()]);
    assert_eq!(audit_count(&reg, &gid, "cache-idle-nudge"), 1);
    // Latched: the same stretch never nudges twice.
    assert!(reg.cache_idle_nudge_tick(t0 + 4 * MIN + 30_000, &pct(&oid, 70)).is_empty());
    // The orchestrator ANSWERING the nudge is output, and output alone does not
    // re-arm it — an orchestrator that read it and chose not to compact is not
    // re-nudged every TTL.
    let grew: HashMap<String, u64> = [(oid.clone(), 128 * 1024u64)].into_iter().collect();
    let none = HashMap::new();
    reg.compact_nudge_tick(t0 + 5 * MIN, &grew, &none, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(reg.cache_idle_nudge_tick(t0 + 9 * MIN, &pct(&oid, 70)).is_empty(), "output does not release the latch");
    assert_eq!(audit_count(&reg, &gid, "cache-idle-nudge"), 1);
    // Evidence the stretch ended — the context fell under the floor, i.e. a
    // compact landed — releases it, and the next stretch can nudge again.
    assert!(reg.cache_idle_nudge_tick(t0 + 9 * MIN, &pct(&oid, 10)).is_empty());
    assert_eq!(reg.cache_idle_nudge_tick(t0 + 9 * MIN, &pct(&oid, 70)), vec![oid.clone()]);
    assert_eq!(audit_count(&reg, &gid, "cache-idle-nudge"), 2);
}

#[test]
fn the_idle_compact_backstop_refuses_past_the_ttl_and_without_a_context_reading() {
    let t0 = 1_000 * MIN;
    let (reg, _d, gid, oid) = idle_orch_setup(t0);
    // Already (inferred) cold: compacting now would pay the cold read it was
    // meant to save.
    assert!(reg.cache_idle_nudge_tick(t0 + 6 * MIN, &pct(&oid, 90)).is_empty());
    // No reading at all fails closed, and so does a reading under the floor.
    assert!(reg.cache_idle_nudge_tick(t0 + 4 * MIN, &HashMap::new()).is_empty());
    assert!(reg.cache_idle_nudge_tick(t0 + 4 * MIN, &pct(&oid, 20)).is_empty());
    assert_eq!(audit_count(&reg, &gid, "cache-idle-nudge"), 0);
    // The positive control on the same fixture: in the band, over the floor.
    assert_eq!(reg.cache_idle_nudge_tick(t0 + 4 * MIN, &pct(&oid, 90)), vec![oid]);
}

#[test]
fn the_idle_compact_backstop_holds_while_a_delegate_or_a_watch_is_in_flight() {
    let t0 = 1_000 * MIN;
    let (reg, _d, gid, oid) = idle_orch_setup(t0);
    let w = reg.spawn_agent(&gid, Role::Worker, "w", "task", false, None).unwrap();
    assert!(reg.cache_idle_nudge_tick(t0 + 4 * MIN, &pct(&oid, 70)).is_empty(), "a live delegate is in flight");
    // The delegate exits (a headless one has no pty to kill, so its exit is
    // recorded the way the pty-exit path records it).
    reg.mark_dead(&w.id, Some(0));
    assert_eq!(
        reg.cache_idle_nudge_tick(t0 + 4 * MIN, &pct(&oid, 70)),
        vec![oid.clone()],
        "the same stretch nudges once the delegate is gone — the positive control"
    );

    let (reg, _d, gid, oid) = idle_orch_setup(t0);
    let co = reg.resolve_token(&reg.agent(&oid).unwrap().token).unwrap();
    register_notify(&reg, &co, json!({ "kind": "pr_checks", "pr": "241" })).unwrap();
    assert!(reg.cache_idle_nudge_tick(t0 + 4 * MIN, &pct(&oid, 70)).is_empty(), "a pending watch is in flight");
    assert_eq!(audit_count(&reg, &gid, "cache-idle-nudge"), 0);
}

#[test]
fn a_block_ttl_override_moves_the_backstop_band_and_zero_turns_it_off() {
    let t0 = 1_000 * MIN;
    let (reg, _d) = test_registry();
    let mut r = compact_rails(0, &["orchestrator"]);
    r.blocks.iter_mut().find(|b| b.id == "orchestrator").unwrap().cache_ttl_minutes = Some(60);
    let g = reg.create_group("C:/tmp/repo", r.clone()).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let grew: HashMap<String, u64> = [(o.id.clone(), 64 * 1024u64)].into_iter().collect();
    reg.compact_nudge_tick(t0, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    assert!(reg.cache_idle_nudge_tick(t0 + 4 * MIN, &pct(&o.id, 70)).is_empty(), "4m is hot on a 60m TTL");
    assert_eq!(reg.cache_idle_nudge_tick(t0 + 50 * MIN, &pct(&o.id, 70)), vec![o.id.clone()]);

    let (reg, _d) = test_registry();
    r.blocks.iter_mut().find(|b| b.id == "orchestrator").unwrap().cache_ttl_minutes = Some(0);
    let g = reg.create_group("C:/tmp/repo", r).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let grew: HashMap<String, u64> = [(o.id.clone(), 64 * 1024u64)].into_iter().collect();
    reg.compact_nudge_tick(t0, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
    for m in [3, 4, 30, 50] {
        assert!(reg.cache_idle_nudge_tick(t0 + m * MIN, &pct(&o.id, 70)).is_empty(), "ttl 0 is unknown: {m}m");
    }
}

// ---------------------------------------------------------------------------
// #3407 review round 1: the drive arms of "in flight" (N1), the re-baseline
// across a source flip (N2), and the honest Compact-now reply (N3).
// ---------------------------------------------------------------------------

#[test]
fn the_idle_compact_backstop_holds_for_a_live_review_drive() {
    let t0 = 1_000 * MIN;
    // A live review drive (`DriveEntry::new` lands in ci-wait) is in flight.
    let (reg, _d, gid, oid) = idle_orch_setup(t0);
    let dir = reg.state_root().join(gid.as_str());
    let live = reviewdrive::DriveEntry::new(1758, "sess-drive", &oid, reviewdrive::Counters::default(), 1_000);
    let state = reviewdrive::ReviewDrivesState { entries: vec![live], ..Default::default() };
    reviewdrive::store_state(&dir, &state).unwrap();
    assert!(reg.cache_idle_nudge_tick(t0 + 4 * MIN, &pct(&oid, 70)).is_empty(), "a live review drive is in flight");
    // The positive control on the same fixture: the file gone, the same pane
    // at the same moment is nudged.
    fs::remove_file(reviewdrive::state_path(&dir)).unwrap();
    assert_eq!(reg.cache_idle_nudge_tick(t0 + 4 * MIN, &pct(&oid, 70)), vec![oid.clone()]);

}

#[test]
fn the_idle_compact_backstop_counts_an_unreadable_drive_file_as_in_flight() {
    // An unreadable drive file — review or plan — counts as in flight: "I could
    // not look" is not "nothing there". Its own test, so a red here is not hidden
    // behind the live-drive assertion above it (review round 1, N1).
    let t0 = 1_000 * MIN;
    for file in [reviewdrive::REVIEW_DRIVES_FILE, loomux_lib::orchestration::plandrive::PLAN_DRIVES_FILE] {
        let (reg, _d, gid, oid) = idle_orch_setup(t0);
        let dir = reg.state_root().join(gid.as_str());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(file), "{ not json").unwrap();
        assert!(
            reg.cache_idle_nudge_tick(t0 + 4 * MIN, &pct(&oid, 70)).is_empty(),
            "an unreadable {file} must count as in flight"
        );
        assert_eq!(audit_count(&reg, &gid, "cache-idle-nudge"), 0);
        // Non-vacuity: the same pane, same moment, with the file gone, fires.
        fs::remove_file(dir.join(file)).unwrap();
        assert_eq!(reg.cache_idle_nudge_tick(t0 + 4 * MIN, &pct(&oid, 70)), vec![oid.clone()], "{file}");
    }
}

#[test]
fn a_statusline_read_between_two_transcript_reads_never_reports_the_whole_history_as_a_wake() {
    // N2's sequence through the real merge: a transcript row with a session's
    // history, one tick where a zero-token statusline read carrying a dollar
    // figure replaces it (the no-downgrade rule lets a priced read win), then the
    // transcript again with ONE request's growth.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let t0 = 1_000 * MIN;
    reg.upsert_usage_snapshot(&g.id, cache_snap(100_000, 5_000_000, 9.0, t0));
    reg.upsert_usage_snapshot(&g.id, cache_snap(100_000, 5_000_000, 9.0, t0 + MIN));
    let statusline = UsageSnapshot {
        source: "statusline".to_string(),
        input_tokens: 0,
        output_tokens: 0,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        ..cache_snap(0, 0, 9.0, t0 + 2 * MIN)
    };
    reg.upsert_usage_snapshot(&g.id, statusline);
    reg.upsert_usage_snapshot(&g.id, cache_snap(100_010, 5_900_000, 9.4, t0 + 12 * MIN));
    let row = usage_row(&reg.group_usage(&g.id), "w-cacheage").clone();
    assert_eq!(row["last_active_ms"], json!(t0 + 12 * MIN), "the one request is activity: {row}");
    assert_eq!(row["last_wake"]["input_tokens"], json!(10), "the wake is ONE request, not the session: {row}");
    assert_eq!(row["last_wake"]["cache_read_tokens"], json!(900_000));
}

#[test]
fn compact_now_says_queued_on_a_paused_group_instead_of_promising_a_paste() {
    let (reg, _d, gid, oid) = compact_nudge_setup(0);
    reg.pause_group(&gid).unwrap();
    let reply = reg.human_request_compact(&gid, &oid).unwrap();
    assert!(reply.starts_with("queued") && reply.contains("paused"), "{reply}");
    assert!(!reply.starts_with("requested"), "a paused group is not typed into: {reply}");
    assert!(reply.contains("resume the group"), "the reply names what releases it: {reply}");
    // The flag is still set: the request is honoured, just later. Nothing fires
    // while paused…
    let empty = HashMap::new();
    assert!(reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    // …and it fires once the group resumes.
    reg.resume_group(&gid).unwrap();
    assert_eq!(
        reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new()),
        vec![oid]
    );
}

#[test]
fn the_compact_now_reply_names_every_condition_holding_the_request() {
    use loomux_lib::orchestration::human_compact_reply;
    let r = |p, c, b| human_compact_reply(p, c, b);
    assert!(r(false, false, false).starts_with("requested") && r(false, false, false).contains("next idle moment"));
    // The three conditions are a conjunction in `compact_nudge_tick` (a paused
    // group is skipped; a fire needs `!compact_pending && requested_fires`, and
    // `requested_fires` carries the budget), so the request waits for EVERY one
    // that holds. Each combination must name each condition it has, and never
    // one it has not: all eight rows, not a sample.
    for p in [false, true] {
        for c in [false, true] {
            for b in [false, true] {
                let reply = r(p, c, b);
                assert_eq!(reply.contains("paused"), p, "{p} {c} {b}: {reply}");
                assert_eq!(reply.contains("already in flight"), c, "{p} {c} {b}: {reply}");
                assert_eq!(reply.contains("compacts for the hour"), b, "{p} {c} {b}: {reply}");
                assert_eq!(reply.starts_with("queued"), p || c || b, "{p} {c} {b}: {reply}");
                assert!(!reply.contains('\n'), "one paragraph: {reply}");
            }
        }
    }
    // W1 (review round 2): a pending compact with the budget spent does NOT fire
    // once the compact resolves. The reply must say both must clear, and must
    // never promise the single release that is false here.
    let w1 = r(false, true, true);
    assert!(w1.contains("only once all of these"), "{w1}");
    assert!(!w1.contains("fires once it resolves"), "the round-1 promise is false with the budget spent: {w1}");
}

// #3831 PR A — the chip on solo, lead and plain agent panes. A solo pane's
// usage is read as ITS OWN CLI and ITS OWN session: `__solo__` is one group
// across panes running different CLIs, so the group's class default answers
// nothing for them.

/// One Claude assistant turn for `sid` with `input` tokens, under `proj`. The
/// transcript is found by session id under every project directory, so the
/// directory name only has to be one.
fn write_solo_claude_turn(proj: &Path, sid: &str, input: u64) {
    let dir = proj.join("C--tmp-solo");
    fs::create_dir_all(&dir).unwrap();
    let line = json!({"type":"assistant","message":{"id":format!("m{input}"),
        "model":"claude-opus-4-8",
        "usage":{"input_tokens":input,"output_tokens":0,
                 "cache_creation_input_tokens":0,"cache_read_input_tokens":0}}});
    fs::write(dir.join(format!("{sid}.jsonl")), format!("{line}\n")).unwrap();
}

#[test]
fn a_solo_panes_usage_row_is_read_from_its_own_session() {
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let prepared = reg.solo_prepare("claude", "C:/tmp/solo", "my solo").unwrap();
    let agent = prepared["agent_id"].as_str().unwrap().to_string();
    reg.solo_bind(&agent, 7001).unwrap();
    reg.human_pane_session(&agent, "solo-claude-session")
        .expect("a bound solo pane takes its session id");

    // Before the first prompt there is no transcript to read.
    let before = reg.group_usage(solo_group_id());
    assert_eq!(usage_row(&before, &agent)["source"], "none");

    // The first prompt lands in the transcript; the next tick reads it.
    write_solo_claude_turn(proj.path(), "solo-claude-session", 1000);
    let after = reg.group_usage(solo_group_id());
    let row = usage_row(&after, &agent);
    assert_eq!(row["source"], "transcript", "the pane's own session is the source: {row}");
    assert_eq!(row["tokens"]["input"], json!(1000));
    assert!(row["last_active_ms"].is_number(), "the request is observed, so the chip has a reading: {row}");
}

#[test]
fn the_usage_collector_reads_a_solo_panes_cli_off_its_own_record() {
    // The class default is `claude`. A pi pane answering to that default would
    // read the wrong transcript store and carry the wrong TTL on its chip.
    let (reg, _d) = test_registry();
    let prepared = reg.solo_prepare("pi", "C:/tmp/solo", "pi solo").unwrap();
    let agent = prepared["agent_id"].as_str().unwrap().to_string();
    let u = reg.group_usage(solo_group_id());
    let row = usage_row(&u, &agent);
    assert_eq!(row["cli"], "pi", "a pi pane is read as pi: {row}");
    assert_eq!(row["cache_ttl_minutes"], Value::Null, "pi documents no fixed cache lifetime: {row}");
}

#[test]
fn an_adopted_plain_pane_is_read_as_its_own_cli_and_session() {
    // A plain agent pane is adopted at spawn with the CLI and session the
    // launcher already holds, so it reads the same way a solo pane does.
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let adopted = reg
        .solo_adopt(7101, "plain claude", "C:/tmp/solo", Some("claude"), Some("adopted-session"))
        .unwrap();
    let agent = adopted["agent_id"].as_str().unwrap().to_string();
    write_solo_claude_turn(proj.path(), "adopted-session", 500);
    let u = reg.group_usage(solo_group_id());
    let row = usage_row(&u, &agent);
    assert_eq!(row["source"], "transcript", "{row}");
    assert_eq!(row["cli"], "claude", "{row}");
    assert!(row["cache_ttl_minutes"].is_number(), "claude's documented TTL reaches the row: {row}");

    // A CLI loomux does not know is refused rather than stored as a name
    // nothing reads.
    assert!(
        reg.solo_adopt(7102, "mystery", "C:/tmp/solo", Some("not-a-cli"), None).is_err(),
        "an unknown CLI must not be recorded on the pane"
    );
}

#[test]
fn a_session_id_is_taken_once_by_a_solo_or_lead_pane_and_only_as_a_path_segment() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let worker = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    assert!(
        reg.human_pane_session(&worker.id, "worker-session").is_err(),
        "a delegate learns its session from its own CLI, never from the human"
    );

    let prepared = reg.solo_prepare("claude", "C:/tmp/solo", "solo").unwrap();
    let agent = prepared["agent_id"].as_str().unwrap().to_string();
    assert!(
        reg.human_pane_session(&agent, "../escape").is_err(),
        "a session id names a transcript file, so it must be one path component"
    );
    assert_eq!(reg.agent(&agent).unwrap().session_id, None, "a refused id is not recorded");

    reg.human_pane_session(&agent, "first-session").unwrap();
    assert!(
        reg.human_pane_session(&agent, "second-session").is_err(),
        "an id already set is never overwritten"
    );
    assert_eq!(reg.agent(&agent).unwrap().session_id.as_deref(), Some("first-session"));
}

#[test]
fn a_session_a_live_pane_holds_is_refused_to_a_second_pane_until_that_pane_exits() {
    // Two live panes on one transcript merge into one usage row, so the second
    // claim is refused and the holder keeps its session. The claim frees up once
    // the holder's pty has exited.
    let (reg, _d) = test_registry();
    let first = reg.solo_prepare("claude", "C:/tmp/solo", "first").unwrap();
    let first_id = first["agent_id"].as_str().unwrap().to_string();
    reg.human_pane_session(&first_id, "shared-session").expect("the first claim lands");

    let second = reg.solo_prepare("claude", "C:/tmp/solo", "second").unwrap();
    let second_id = second["agent_id"].as_str().unwrap().to_string();
    assert!(reg.human_pane_session(&second_id, "shared-session").is_err(), "a live pane holds this session");
    assert!(
        reg.solo_adopt(7301, "third", "C:/tmp/solo", Some("claude"), Some("shared-session")).is_err(),
        "adoption refuses a held session the same way"
    );
    assert_eq!(reg.agent(&second_id).unwrap().session_id, None, "a refused claim is not recorded");
    assert_eq!(reg.agent(&first_id).unwrap().session_id.as_deref(), Some("shared-session"), "the holder keeps it");
    assert!(
        reg.human_pane_session(&first_id, "shared-session").is_ok(),
        "the holder re-reporting its own id is still a success"
    );

    reg.mark_dead(&first_id, Some(0));
    reg.human_pane_session(&second_id, "shared-session")
        .expect("a dead pane has given its session up, so the claim frees");
}
