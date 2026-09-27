//! #413 S5: trusted compaction-DONE signals — Claude's `PostCompact` hook marker
//! (settle, pairing with `SessionStart(compact)`, the script arm and its
//! `--settings` wiring). The pi and codex count pins live beside their readers,
//! in `tests/piusage.rs` and `tests/codexusage.rs`.
//!
//! One module of the `orchestration` integration-test target (`main.rs`); split
//! from `compact.rs` to keep that file under the Rust-test line ceiling
//! (`test/filebudget.test.ts`). Layout rules: docs/design/module-layout.md.

use super::*;
use loomux_lib::orchestration::{
    claude_supports_postcompact, postcompact_marker_disposition, set_claude_version_for_test, PostCompactDisposition,
    CLAUDE_POSTCOMPACT_MIN_VERSION, POSTCOMPACT_SESSIONSTART_PAIR_MS, POSTCOMPACT_SETTLE_MS,
};

/// Answers the probe-cache version lookup with `version` on this thread until
/// dropped — restored from `Drop`, so a failing assertion cannot leak it.
struct ClaudeVersionOverride;
impl Drop for ClaudeVersionOverride {
    fn drop(&mut self) {
        set_claude_version_for_test(None);
    }
}
fn claude_version(version: Option<&str>) -> ClaudeVersionOverride {
    set_claude_version_for_test(Some(version.map(str::to_string)));
    ClaudeVersionOverride
}

/// The hooks settings file a fresh worker's spawn wrote, parsed.
fn spawned_hooks_settings() -> Value {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let p = reg.state_root().join(g.id.as_str()).join("configs").join(format!("{}-hooks.json", w.id));
    serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
}

fn postcompact_marker(reg: &OrchRegistry, gid: &GroupId, oid: &str) -> PathBuf {
    reg.state_root().join(gid.as_str()).join("hooks").join(format!("{oid}.postcompact.json"))
}

fn sessionstart_marker(reg: &OrchRegistry, gid: &GroupId, oid: &str) -> PathBuf {
    reg.state_root().join(gid.as_str()).join("hooks").join(format!("{oid}.sessionstart-compact.json"))
}

fn quiet_tick(reg: &OrchRegistry, now: u64) {
    let _ = reg.compact_nudge_tick(now, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
}

#[test]
fn postcompact_disposition_absorbs_settles_then_resolves() {
    use PostCompactDisposition::*;
    let ts = 1_000_000u64;
    // Paired with a consumed SessionStart(compact), in EITHER write order, up
    // to the window and not one millisecond past it.
    assert_eq!(postcompact_marker_disposition(ts, Some(ts - 5), false, None, 0), Absorb, "SessionStart wrote first");
    assert_eq!(postcompact_marker_disposition(ts, Some(ts + 5), false, None, 0), Absorb, "PostCompact wrote first");
    assert_eq!(postcompact_marker_disposition(ts, Some(ts + POSTCOMPACT_SESSIONSTART_PAIR_MS), false, None, 0), Absorb);
    assert_eq!(
        postcompact_marker_disposition(ts, Some(ts - POSTCOMPACT_SESSIONSTART_PAIR_MS - 1), false, None, 0),
        Settle,
        "an earlier compaction's SessionStart does not speak for this one"
    );
    // A reinjection already decided is never re-decided (rev-10 B1's rule).
    assert_eq!(postcompact_marker_disposition(ts, None, true, Some(0), 1_000_000), Absorb);
    // First sight settles; the window is measured on the tick's clock.
    assert_eq!(postcompact_marker_disposition(ts, None, false, None, 50), Settle);
    assert_eq!(postcompact_marker_disposition(ts, None, false, Some(50), 50 + POSTCOMPACT_SETTLE_MS - 1), Settle);
    assert_eq!(postcompact_marker_disposition(ts, None, false, Some(50), 50 + POSTCOMPACT_SETTLE_MS), Resolve);
    // ...and never on the marker's: an mtime far in the future does not hold
    // the arm open once the tick's own window has passed.
    assert_eq!(postcompact_marker_disposition(u64::MAX, None, false, Some(50), 50 + POSTCOMPACT_SETTLE_MS), Resolve);
}

#[test]
fn postcompact_marker_alone_settles_then_resolves_into_loomuxs_own_reinjection() {
    // No PreCompact arm, no SessionStart(compact): the PostCompact marker is the
    // only word that a compaction finished, and it is enough — trusted, like
    // the SessionStart marker, and unconditional on an arm being open.
    let (reg, _d, gid, oid) = compact_nudge_setup(20);
    let started_ms = reg.agent(&oid).unwrap().started_ms;
    let marker = postcompact_marker(&reg, &gid, &oid);
    write_hook_marker(&marker, started_ms, 1_000);

    quiet_tick(&reg, 1_000);
    let a = reg.agent(&oid).unwrap();
    assert_eq!(a.compact_hook_postcompact_first_seen_ms, Some(1_000), "first sight opens the settle window");
    assert!(!a.compact_pending && a.compact_reinject_attempted_ms.is_none(), "nothing decided while it settles");
    assert!(marker.exists(), "a settling marker stays on disk");
    assert!(reg.any_compact_pending(), "a settling marker keeps the fast poll cadence");
    assert_eq!(audit_count(&reg, &gid, "compact-hook-evidence"), 1, "the evidence is audited once, on first sight");

    quiet_tick(&reg, 1_000 + POSTCOMPACT_SETTLE_MS - 1);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 0, "still inside the settle window");

    quiet_tick(&reg, 1_000 + POSTCOMPACT_SETTLE_MS);
    let a = reg.agent(&oid).unwrap();
    assert!(a.compact_pending && a.compact_reinject_attempted_ms.is_some(), "resolved into the delivery-confirmation phase");
    assert_eq!(a.compact_pending_evidence, Some("hook"));
    assert_eq!(a.compact_hook_postcompact_first_seen_ms, None);
    assert!(!marker.exists(), "consumed on resolution");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1);
    assert_eq!(audit_count(&reg, &gid, "compact-resolved-postcompact"), 1);
    assert_eq!(audit_count(&reg, &gid, "compact-hook-evidence"), 1, "not re-audited at resolution");

    let confirmed = confirmed_delivery(&oid, 1_000 + POSTCOMPACT_SETTLE_MS);
    let _ = reg.compact_nudge_tick(1_000 + POSTCOMPACT_SETTLE_MS + 1_000, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &confirmed);
    assert!(!reg.agent(&oid).unwrap().compact_pending, "an ordinary delivery confirmation closes it");
}

#[test]
fn postcompact_marker_resolves_an_inference_arm_the_token_gate_would_discard() {
    // The same banner-armed shape `compact_nudge_tick_discards_an_unconfirmed_
    // pending_state_without_reinjecting` DISCARDS (no token drop, no marker
    // rise, quiet): with a PostCompact marker beside it the hook's word wins —
    // the arm is made trusted on first sight, held while the marker settles,
    // and resolved into a reinjection instead of a discard.
    let (reg, _d, gid, oid) = compact_nudge_setup(0);
    let started_ms = reg.agent(&oid).unwrap().started_ms;
    let banner_tail: HashMap<String, (String, u64)> =
        [(oid.clone(), ("✢ Compacting conversation… (esc to interrupt · 8s · ↓ 172 tokens)".to_string(), 0u64))]
            .into_iter()
            .collect();
    let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64)].into_iter().collect();
    let unchanged: HashMap<String, u64> = [(oid.clone(), 100_000u64)].into_iter().collect();
    let _ = reg.compact_nudge_tick(500, &grew, &banner_tail, &HashMap::new(), &unchanged, &HashMap::new(), &HashMap::new());
    let a = reg.agent(&oid).unwrap();
    assert!(a.compact_pending && !a.compact_pending_trusted, "positive control: an untrusted inference arm");

    write_hook_marker(&postcompact_marker(&reg, &gid, &oid), started_ms, 1_000);
    let _ = reg.compact_nudge_tick(600, &grew, &HashMap::new(), &HashMap::new(), &unchanged, &HashMap::new(), &HashMap::new());
    let a = reg.agent(&oid).unwrap();
    assert!(a.compact_pending, "held, not discarded, while the marker settles");
    assert!(a.compact_pending_trusted, "the hook made the arm trusted");
    assert_eq!(a.compact_pending_evidence, Some("hook"));
    assert_eq!(audit_count(&reg, &gid, "compact-pending-discarded"), 0);

    let _ = reg.compact_nudge_tick(600 + POSTCOMPACT_SETTLE_MS, &grew, &HashMap::new(), &HashMap::new(), &unchanged, &HashMap::new(), &HashMap::new());
    assert_eq!(audit_count(&reg, &gid, "compact-pending-discarded"), 0, "the token gate never got to discard it");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1);
    assert_eq!(audit_count(&reg, &gid, "compact-resolved-postcompact"), 1);
}

#[test]
fn postcompact_marker_then_sessionstart_resolves_natively_with_no_duplicate_reinjection() {
    // PostCompact written first, SessionStart(compact) a moment later and read
    // on the NEXT tick, inside the settle window. SessionStart is terminal and
    // carries native re-grounding; the PostCompact marker is then absorbed, so
    // loomux never pastes its own reinjection on top (rev-4 N3).
    let (reg, _d, gid, oid) = compact_nudge_setup(20);
    let started_ms = reg.agent(&oid).unwrap().started_ms;
    let marker = postcompact_marker(&reg, &gid, &oid);
    write_hook_marker(&marker, started_ms, 1_000);
    quiet_tick(&reg, 1_000);
    assert!(marker.exists(), "positive control: it is settling");

    write_hook_marker(&sessionstart_marker(&reg, &gid, &oid), started_ms, 1_050);
    quiet_tick(&reg, 2_000);
    assert!(!marker.exists(), "absorbed beside its paired SessionStart");
    assert_eq!(reg.agent(&oid).unwrap().compact_hook_postcompact_first_seen_ms, None);
    assert!(!reg.any_compact_pending());
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-skipped-native"), 1);

    quiet_tick(&reg, 2_000 + 10 * POSTCOMPACT_SETTLE_MS);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 0, "no duplicate re-grounding");
    assert_eq!(audit_count(&reg, &gid, "compact-resolved-postcompact"), 0);
    assert!(!reg.agent(&oid).unwrap().compact_pending);
}

#[test]
fn postcompact_marker_after_a_consumed_sessionstart_is_absorbed_at_once() {
    // The reverse write order: SessionStart(compact) consumed first, the
    // PostCompact marker for the same compaction read a tick later.
    let (reg, _d, gid, oid) = compact_nudge_setup(20);
    let started_ms = reg.agent(&oid).unwrap().started_ms;
    write_hook_marker(&sessionstart_marker(&reg, &gid, &oid), started_ms, 1_000);
    quiet_tick(&reg, 1_000);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection-skipped-native"), 1, "positive control: resolved natively");

    let marker = postcompact_marker(&reg, &gid, &oid);
    write_hook_marker(&marker, started_ms, 1_200);
    quiet_tick(&reg, 2_000);
    assert!(!marker.exists(), "absorbed on sight, no settle");
    assert_eq!(reg.agent(&oid).unwrap().compact_hook_postcompact_first_seen_ms, None);
    quiet_tick(&reg, 2_000 + 10 * POSTCOMPACT_SETTLE_MS);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 0);
    assert!(!reg.agent(&oid).unwrap().compact_pending);
}

#[test]
fn postcompact_marker_from_before_this_agent_started_is_not_evidence() {
    // rev-4 B1's cross-restart layer, for the new marker: an id handed out
    // again after a restart must not inherit the previous process's marker.
    let (reg, _d, gid, oid) = compact_nudge_setup(20);
    let started_ms = reg.agent(&oid).unwrap().started_ms;
    let marker = postcompact_marker(&reg, &gid, &oid);
    write_hook_marker(&marker, started_ms, -60_000);
    quiet_tick(&reg, 1_000);
    quiet_tick(&reg, 1_000 + 2 * POSTCOMPACT_SETTLE_MS);
    let a = reg.agent(&oid).unwrap();
    assert_eq!(a.compact_hook_postcompact_first_seen_ms, None);
    assert!(!a.compact_pending);
    assert_eq!(audit_count(&reg, &gid, "compact-hook-evidence"), 0);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 0);

    // Positive control: the same file touched inside this agent's lifetime IS.
    write_hook_marker(&marker, started_ms, 1_000);
    quiet_tick(&reg, 30_000);
    assert_eq!(audit_count(&reg, &gid, "compact-hook-evidence"), 1);
}

#[test]
fn postcompact_hook_script_sh_touches_its_marker_drains_stdin_and_prints_nothing() {
    let Some(sh) = resolve_test_sh() else {
        eprintln!("SKIP postcompact_hook_script_sh_touches_its_marker_drains_stdin_and_prints_nothing: no sh found");
        return;
    };
    use std::io::Write;
    use std::process::Stdio;
    let td = tempfile::tempdir().unwrap();
    let group_dir = td.path().join("group");
    fs::create_dir_all(&group_dir).unwrap();
    let blocked_group_dir = td.path().join("blocked-group-dir");
    fs::write(&blocked_group_dir, "occupies the path; not a directory").unwrap();
    let script_path = td.path().join("compact-hook.sh");
    fs::write(&script_path, COMPACT_HOOK_SCRIPT).unwrap();
    // A payload well past any pipe buffer: `compact_summary` is the whole
    // conversation's summary, and a hook that stopped reading would leave its
    // caller's write failing or blocked.
    let summary = "s".repeat(1 << 20);
    let payload = format!(
        r#"{{"session_id":"abc123","hook_event_name":"PostCompact","trigger":"auto","compact_summary":"{summary}"}}"#
    );

    for dir in [&group_dir, &blocked_group_dir] {
        let mut child = std::process::Command::new(&sh)
            .arg(&script_path)
            .arg("postcompact")
            .arg(dir.display().to_string())
            .arg("agent-1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("sh must run");
        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn({
            let payload = payload.clone();
            move || stdin.write_all(payload.as_bytes()).is_ok()
        });
        let output = child.wait_with_output().unwrap();
        assert!(writer.join().unwrap(), "dir={}: the script read the whole payload", dir.display());
        assert_eq!(output.status.code(), Some(0), "dir={}: stderr={:?}", dir.display(), String::from_utf8_lossy(&output.stderr));
        assert!(output.stdout.is_empty(), "no stdout: {:?}", String::from_utf8_lossy(&output.stdout));
    }

    let marker = group_dir.join("hooks").join("agent-1.postcompact.json");
    assert!(marker.exists(), "the marker is touched");
    assert_eq!(fs::read_to_string(&marker).unwrap(), "", "existence-only: the summary is never copied onto disk");
}

#[test]
fn postcompact_hook_is_wired_into_claudes_settings_file() {
    #[cfg(windows)]
    if locate_sh_exe().is_none() {
        eprintln!("SKIP postcompact_hook_is_wired_into_claudes_settings_file: no sh.exe found via `where`");
        return;
    }
    // A Claude Code the probe knows has the event (#413 S5 review r2).
    let _v = claude_version(Some("2.1.76"));
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let p = reg.state_root().join(g.id.as_str()).join("configs").join(format!("{}-hooks.json", w.id));
    let cfg: Value = serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap();
    let entry = &cfg["hooks"]["PostCompact"][0];
    assert!(entry.get("matcher").is_none(), "manual and auto alike: {entry}");
    let cmd = entry["hooks"][0]["command"].as_str().unwrap_or_else(|| panic!("no PostCompact command: {cfg}"));
    assert!(cmd.contains(" postcompact \"") && cmd.ends_with(&format!("\"{}\"", w.id)), "{cmd}");
    assert!(cfg["hooks"]["PreCompact"][0]["hooks"][0]["command"].as_str().unwrap().contains(" precompact \""), "positive control");
}

// ---------- review round 1: the settle hold's reach, and the window's two edges ----------

/// A claude orchestrator with a context reading over an 80% escalation threshold
/// and, when `marker`, a fresh PostCompact marker beside it. The heuristic is out
/// of reach (9999 minutes), so the only things that can act on the pane this tick
/// are the escalation and whatever the caller queues.
fn settling_fixture(marker: bool) -> (OrchRegistry, tempfile::TempDir, GroupId, String) {
    let (reg, d, gid, oid) = compact_nudge_setup(9999);
    reg.set_compact_context_threshold(&gid, 80).unwrap();
    if marker {
        let started_ms = reg.agent(&oid).unwrap().started_ms;
        write_hook_marker(&postcompact_marker(&reg, &gid, &oid), started_ms, 1_000);
    }
    (reg, d, gid, oid)
}

#[test]
fn postcompact_settle_holds_a_queued_compact_for_the_window() {
    // rev-final N2: while a marker settles, nothing after the resolver acts on
    // the pane — a compaction just FINISHED. The queued request here is the
    // fire site; `postcompact_settle_holds_the_escalation_for_the_window` is the
    // escalation site. Each is shown firing without the marker first, so the
    // hold is what stops it and not the fixture.
    for marker in [false, true] {
        let (reg, _d, _gid, oid) = settling_fixture(marker);
        reg.request_compact(&oid).unwrap();
        let nudged = reg.compact_nudge_tick(1_000, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
        if marker {
            assert!(nudged.is_empty(), "no `/compact` is pasted while the marker settles: {nudged:?}");
            let a = reg.agent(&oid).unwrap();
            assert!(a.compact_requested, "held, not consumed: the request is still queued");
            assert_eq!(a.compact_hook_postcompact_first_seen_ms, Some(1_000), "positive control: it IS settling");
        } else {
            assert_eq!(nudged, vec![oid.clone()], "control: with no marker the queued request fires on this tick");
        }
    }
}

#[test]
fn postcompact_settle_holds_the_escalation_for_the_window() {
    for marker in [false, true] {
        let (reg, _d, gid, oid) = settling_fixture(marker);
        let over: HashMap<String, u32> = [(oid.clone(), 90u32)].into_iter().collect();
        let _ = reg.compact_nudge_tick(1_000, &HashMap::new(), &HashMap::new(), &over, &HashMap::new(), &HashMap::new(), &HashMap::new());
        let expected = if marker { 0 } else { 1 };
        assert_eq!(
            audit_count(&reg, &gid, "compact-escalation"),
            expected,
            "marker={marker}: a pre-compaction reading escalates only when no PostCompact marker is settling"
        );
    }
}

#[test]
fn postcompact_marker_with_a_future_mtime_still_resolves_on_the_tick_clock() {
    // rev-std's skew edge, the direction the code handles: the settle window is
    // measured from first sight on the tick's own clock, so a marker whose mtime
    // is a day ahead of the host still resolves one window later instead of
    // holding the pane until the clock catches up.
    let (reg, _d, gid, oid) = compact_nudge_setup(20);
    let started_ms = reg.agent(&oid).unwrap().started_ms;
    let marker = postcompact_marker(&reg, &gid, &oid);
    write_hook_marker(&marker, started_ms, 86_400_000);

    quiet_tick(&reg, 1_000);
    assert_eq!(reg.agent(&oid).unwrap().compact_hook_postcompact_first_seen_ms, Some(1_000), "positive control: seen and settling");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 0);

    quiet_tick(&reg, 1_000 + POSTCOMPACT_SETTLE_MS);
    assert_eq!(audit_count(&reg, &gid, "compact-resolved-postcompact"), 1, "resolved on the tick clock, a day before the mtime");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1);
    assert!(!marker.exists());
}

#[test]
fn postcompact_a_sessionstart_after_the_settle_window_is_the_disclosed_duplicate() {
    // rev-final N1 / rev-std premortem 1, pinned as the residual the design note
    // discloses rather than claimed away: once the marker has resolved, loomux's
    // reinjection is already queued, and a SessionStart(compact) arriving later
    // only clears the delivery phase (rev-10 B1). It is not counted as a skip,
    // because the paste went; Claude's hook still prints its native context, so
    // this pane is re-grounded twice. A change that makes the window wait longer
    // or pair across it turns this red — and must retire the residual with it.
    let (reg, _d, gid, oid) = compact_nudge_setup(20);
    let started_ms = reg.agent(&oid).unwrap().started_ms;
    write_hook_marker(&postcompact_marker(&reg, &gid, &oid), started_ms, 1_000);
    quiet_tick(&reg, 1_000);
    quiet_tick(&reg, 1_000 + POSTCOMPACT_SETTLE_MS);
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1, "the settle window closed: loomux's own reinjection is queued");

    // Claude's SessionStart(compact) lands 20s after PostCompact, past the window.
    write_hook_marker(&sessionstart_marker(&reg, &gid, &oid), started_ms, 21_000);
    quiet_tick(&reg, 1_000 + POSTCOMPACT_SETTLE_MS + 10_000);
    let a = reg.agent(&oid).unwrap();
    assert!(!a.compact_pending && a.compact_reinject_attempted_ms.is_none(), "the late SessionStart still closes the phase");
    assert_eq!(audit_count(&reg, &gid, "compact-reinjection"), 1, "no retry of loomux's paste");
    assert_eq!(
        audit_count(&reg, &gid, "compact-reinjection-skipped-native"),
        0,
        "not a skip: loomux's reinjection already went, beside the native one — the duplicate"
    );
}

// ---------- review round 2: a Claude Code too old for PostCompact never sees it ----------

#[test]
fn claude_supports_postcompact_compares_versions_as_numbers() {
    // CHANGELOG 2.1.76 added the event; below it, and for any version the probe
    // could not read, the entry is withheld. `2.1.8` and `2.1.100` are the rows
    // a string comparison gets backwards.
    assert_eq!(CLAUDE_POSTCOMPACT_MIN_VERSION, [2, 1, 76]);
    for (version, expected) in [
        (None, false),
        (Some("2.1.75"), false),
        (Some("2.1.8"), false),
        (Some("1.99.99"), false),
        (Some("2.1.76"), true),
        (Some("2.1.100"), true),
        (Some("2.2.0"), true),
        (Some("3.0.0"), true),
        (Some("2.1"), false),
        (Some("2.1.x"), false),
        (Some(""), false),
    ] {
        assert_eq!(claude_supports_postcompact(version), expected, "{version:?}");
    }
}

#[test]
fn postcompact_hook_is_written_only_for_a_claude_known_to_have_the_event() {
    #[cfg(windows)]
    if locate_sh_exe().is_none() {
        eprintln!("SKIP postcompact_hook_is_written_only_for_a_claude_known_to_have_the_event: no sh.exe found via `where`");
        return;
    }
    // Before 2.1.101 an unknown hook event made Claude ignore the WHOLE settings
    // file, so on a Claude older than 2.1.76 a PostCompact entry would cost every
    // other hook and the status line. Unknown — a cold probe cache, or a version
    // the probe could not read — is treated as too old.
    for (version, expected) in [(None, false), (Some("2.1.75"), false), (Some("2.1.8"), false), (Some("2.1.76"), true), (Some("2.1.101"), true)] {
        let _v = claude_version(version);
        let cfg = spawned_hooks_settings();
        assert_eq!(cfg["hooks"].get("PostCompact").is_some(), expected, "version {version:?}: {cfg}");
        // Positive control, every row: the rest of the file is written either way.
        for event in ["PreCompact", "SessionStart", "UserPromptSubmit"] {
            assert!(cfg["hooks"][event][0]["hooks"][0]["command"].is_string(), "version {version:?}: {event} missing: {cfg}");
        }
        assert!(cfg["statusLine"]["command"].is_string(), "version {version:?}: statusLine missing: {cfg}");
    }
}
