//! The notification backend, the unified gh poller, bounded poll ticks and group watches.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ---------- notification backend (#243) ----------
//
// No test here shells out to `gh`: `notify_tick(now, &results)` is the seam
// (the `watchdog_tick` shape), so every test drives it with a synthetic
// `PollResult` map. Pure predicate/notice-sanitation coverage (the "no
// checks reported" → Pending regression, the SUCCESS/FAILURE/IN_PROGRESS
// table, the forged-prefix/newline sanitation) lives inline in the `notify`
// module's own `#[cfg(test)]` module — those are pure functions with no
// registry/Tauri dependency, exactly the `gh.rs` precedent for keeping pure-fn
// tests out of this integration file. That module now lives in the
// `loomux-engine` crate (#888 slice A2 batch 3) and is re-exported here under
// its old `orchestration::notify` path, which is why the `use` above and every
// test below are untouched by the move.

/// Call `notify_when` through the real MCP dispatch and return the tool's
/// text (Ok on success, Err on a rejection) — mirrors how an agent actually
/// reaches this tool, so authz/validation are exercised for real.
pub(crate) fn register_notify(reg: &OrchRegistry, c: &Caller, args: Value) -> Result<String, String> {
    let r = dispatch(reg, c, "tools/call", &json!({ "name": "notify_when", "arguments": args })).unwrap();
    let text = r["content"][0]["text"].as_str().unwrap().to_string();
    if r["isError"] == true { Err(text) } else { Ok(text) }
}

/// Pull the watch id (`n-3`) out of `notify_when`'s confirmation text
/// (`"registered n-3 (PR #241 checks), polled every 30s, …"`).
fn extract_watch_id(text: &str) -> String {
    text.split_whitespace().nth(1).unwrap().to_string()
}

#[test]
fn notify_register_tick_fires_once_and_delists() {
    let (reg, _d, _co, cw) = setup_mcp();
    let text =
        register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "241", "note": "merge if green" })).unwrap();
    let id = extract_watch_id(&text);

    let mut results = HashMap::new();
    results.insert(id.clone(), notify::PollResult::Met { summary: "SUCCESS — all 6 checks passed".into() }.into());
    assert_eq!(reg.notify_tick(now_ms(), &results), vec![id.clone()], "a Met result must fire exactly once");

    let listed = reg.list_notifications(&cw.agent_id).to_string();
    assert!(!listed.contains(&id), "a fired watch must be delisted, got: {listed}");

    let log = fs::read_to_string(reg.state_root().join(cw.group.as_str()).join("audit.jsonl")).unwrap();
    assert!(log.contains("watch-fired"), "the fire must be audited, got: {log}");
    assert!(log.contains("SUCCESS"), "the audit must carry the summary, got: {log}");

    // A second tick with the same result set is a no-op — the watch is gone.
    assert!(reg.notify_tick(now_ms(), &results).is_empty(), "must not fire twice");
}

/// The head SHA a `pr_checks` watch was registered against (#531's live
/// incident: the note named this one) and the one it was re-pushed to before
/// the checks resolved.
const HEAD_AT_REGISTRATION: &str = "ccf191c00000000000000000000000000000beef";
const HEAD_AFTER_REPUSH: &str = "a77c4d1e5f00000000000000000000000000abcd";

/// Drive one watch from a first (baseline-setting) `Pending` poll to a `Met`
/// poll, each with its own observed head, and return the group's audit log —
/// which carries the delivered notice text verbatim under `watch-fired`.
fn notify_fire_with_heads(
    reg: &OrchRegistry,
    cw: &Caller,
    id: &str,
    first_head: &str,
    head_at_fire: &str,
) -> String {
    let mut pending = HashMap::new();
    pending.insert(id.to_string(), notify::Poll::new(notify::PollResult::Pending, Some(first_head.to_string())));
    assert!(reg.notify_tick(now_ms(), &pending).is_empty(), "a Pending poll must not fire, it only baselines the head");

    let mut met = HashMap::new();
    met.insert(
        id.to_string(),
        notify::Poll::new(
            notify::PollResult::Met { summary: "SUCCESS — all 6 checks passed".into() },
            Some(head_at_fire.to_string()),
        ),
    );
    assert_eq!(reg.notify_tick(now_ms(), &met), vec![id.to_string()], "the Met poll must fire");

    fs::read_to_string(reg.state_root().join(cw.group.as_str()).join("audit.jsonl")).unwrap()
}

#[test]
fn notify_fired_notice_states_the_head_its_verdict_was_read_at() {
    // #531, unmoved case: the head never changed, so the notice reads as it
    // always did PLUS the SHA the verdict was read at — the reader can compare
    // that against the branch head instead of trusting a frozen note.
    let (reg, _d, _co, cw) = setup_mcp();
    let text =
        register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "241", "note": "merge ccf191c if green" }))
            .unwrap();
    let id = extract_watch_id(&text);

    let log = notify_fire_with_heads(&reg, &cw, &id, HEAD_AT_REGISTRATION, HEAD_AT_REGISTRATION);

    assert!(log.contains("watch-fired"), "the fire must be audited, got: {log}");
    assert!(log.contains("SUCCESS — all 6 checks passed"), "the verdict must still lead, got: {log}");
    assert!(log.contains("Head at this poll: ccf191c"), "the notice must state the head, got: {log}");
    assert!(!log.contains("MOVED"), "an unmoved head must not raise the marker, got: {log}");
    // The note is echoed, labelled as of registration, and never rewritten.
    assert!(log.contains("Note (registered)"), "the note must be marked as-of-registration, got: {log}");
    assert!(log.contains("merge ccf191c if green"), "the agent's own words must survive verbatim, got: {log}");
}

#[test]
fn notify_fired_notice_flags_a_head_that_moved_since_the_note_was_written() {
    // #531's actual incident: registered while the branch was at ccf191c,
    // re-pushed to a77c4d1 before the checks resolved. The delivered notice
    // used to carry a CURRENT verdict under a STALE SHA (the note's), with
    // nothing saying they had diverged — a green run on an old head reads as a
    // merge license when it isn't. Both SHAs must now appear, with the move
    // called out explicitly.
    let (reg, _d, _co, cw) = setup_mcp();
    let text =
        register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "241", "note": "merge ccf191c if green" }))
            .unwrap();
    let id = extract_watch_id(&text);

    let log = notify_fire_with_heads(&reg, &cw, &id, HEAD_AT_REGISTRATION, HEAD_AFTER_REPUSH);

    assert!(log.contains("Head at this poll: a77c4d1"), "must state the head the verdict was read at, got: {log}");
    assert!(log.contains("MOVED from ccf191c"), "must say the head moved, and from where, got: {log}");
    assert!(log.contains("re-verify"), "must say what to do about it, got: {log}");
    // Still the agent's own note, unrewritten — only its as-of is clarified.
    assert!(log.contains("Note (registered)"), "got: {log}");
    assert!(log.contains("merge ccf191c if green"), "got: {log}");
}

#[test]
fn notify_fire_without_an_observed_head_is_unchanged() {
    // A `workflow_run` watch (a run id is pinned to one commit) — and equally
    // any poll where `gh` reported no usable oid: no head clause, no marker,
    // no hedge. The #531 change must be additive, never a new noise floor.
    let (reg, _d, _co, cw) = setup_mcp();
    let text =
        register_notify(&reg, &cw, json!({ "kind": "workflow_run", "run": "17812", "note": "ship it" })).unwrap();
    let id = extract_watch_id(&text);

    let mut met = HashMap::new();
    met.insert(id.clone(), notify::PollResult::Met { summary: "completed — conclusion: success".into() }.into());
    assert_eq!(reg.notify_tick(now_ms(), &met), vec![id.clone()]);

    let log = fs::read_to_string(reg.state_root().join(cw.group.as_str()).join("audit.jsonl")).unwrap();
    assert!(log.contains("watch-fired"), "got: {log}");
    assert!(!log.contains("Head at this poll"), "no observed head means no head clause, got: {log}");
    assert!(!log.contains("MOVED"), "and no move marker, got: {log}");
    assert!(log.contains("Note (registered)"), "the note is still labelled as-of-registration, got: {log}");
}

#[test]
fn notify_conflicting_fires_distinct_notice_promptly_and_delists() {
    // #337: a PR that goes CONFLICTING must fire an immediately actionable,
    // distinct notice — never the standard SUCCESS/FAILURE summary — and
    // resolve the watch right away, not wait toward the TTL every other
    // pr_checks watch gets.
    let (reg, _d, _co, cw) = setup_mcp();
    let text = register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "329", "expires_minutes": 60 }))
        .unwrap();
    let id = extract_watch_id(&text);

    let mut results = HashMap::new();
    results.insert(id.clone(), notify::PollResult::Conflicting.into());
    // Tick immediately (well within the 60-minute TTL) — a standard Met/Failure
    // wait would never fire here; this must resolve without any expiry.
    assert_eq!(
        reg.notify_tick(now_ms(), &results),
        vec![id.clone()],
        "a Conflicting result must fire immediately, not wait for expiry"
    );

    let listed = reg.list_notifications(&cw.agent_id).to_string();
    assert!(!listed.contains(&id), "a conflicting watch must be delisted, got: {listed}");

    let log = fs::read_to_string(reg.state_root().join(cw.group.as_str()).join("audit.jsonl")).unwrap();
    assert!(log.contains("watch-conflicting"), "the conflict must be audited distinctly, got: {log}");
    assert!(!log.contains("watch-fired"), "must not be recorded as a standard fire, got: {log}");
    assert!(log.contains("PR #329 is CONFLICTING"), "the notice must name the PR and the conflict, got: {log}");
    assert!(log.contains("rebased"), "the notice must be immediately actionable, got: {log}");

    // A second tick with the same result set is a no-op — the watch is gone.
    assert!(reg.notify_tick(now_ms(), &results).is_empty(), "must not fire twice");
}

#[test]
fn notify_pending_does_not_fire_and_stays_listed() {
    let (reg, _d, _co, cw) = setup_mcp();
    let text = register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "5" })).unwrap();
    let id = extract_watch_id(&text);

    let mut results = HashMap::new();
    results.insert(id.clone(), notify::PollResult::Pending.into());
    assert!(reg.notify_tick(now_ms(), &results).is_empty(), "Pending must never fire");
    assert!(reg.notify_tick(now_ms(), &results).is_empty(), "two Pending ticks in a row still must not fire");

    let listed = reg.list_notifications(&cw.agent_id).to_string();
    assert!(listed.contains(&id), "a Pending watch must remain listed, got: {listed}");
}

#[test]
fn notify_expires_after_ttl_with_injected_now_and_tells_the_owner() {
    let (reg, _d, _co, cw) = setup_mcp();
    let text = register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "88", "expires_minutes": 5 })).unwrap();
    let id = extract_watch_id(&text);

    // 6 minutes later, no poll result for this watch at all (it wasn't due,
    // or gh returned nothing usable this tick) — expiry is purely
    // time-based, so this alone must drop it.
    let future = now_ms() + 6 * 60_000;
    assert_eq!(reg.notify_tick(future, &HashMap::new()), vec![id.clone()], "must expire past the TTL");

    let listed = reg.list_notifications(&cw.agent_id).to_string();
    assert!(!listed.contains(&id), "an expired watch must be delisted, got: {listed}");

    let log = fs::read_to_string(reg.state_root().join(cw.group.as_str()).join("audit.jsonl")).unwrap();
    assert!(log.contains("watch-expired"), "expiry must be audited, got: {log}");
}

#[test]
fn list_notifications_is_oldest_registered_first() {
    // Pre-existing behavior on `feat/notify-when` (#247), unpinned until now
    // (rev-tests, PR #252): reversing `list_notifications`' sort left all of
    // #247's notify tests green. #252 is what makes watch ORDER user-visible
    // (`group_watches` documents "oldest-registered first, matching
    // `list_notifications`" and the group view relies on it for its
    // soonest-first display), so pin the order both consumers depend on.
    let (reg, _d, _co, cw) = setup_mcp();
    // Deliberately NO sleeps between registrations (a prior version of this
    // test used `std::thread::sleep`s to force distinct `registered_ms`
    // values — that went red in CI: on a fast runner two of the three still
    // land in the same real millisecond, and with a `registered_ms`-only sort
    // key a tie falls back to the input vec's order, which comes from HashMap
    // iteration — arbitrary and randomized per process. Registering all three
    // back to back, with no artificial spacing, is what actually exercises
    // that tie: `registered_ms` for all three is very likely identical here,
    // so this only passes if the `(registered_ms, seq)` tie-break in
    // `list_notifications` is doing real work (see `Watch::seq`'s doc).
    let t1 = register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "1" })).unwrap();
    let id1 = extract_watch_id(&t1);
    let t2 = register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "2" })).unwrap();
    let id2 = extract_watch_id(&t2);
    let t3 = register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "3" })).unwrap();
    let id3 = extract_watch_id(&t3);

    let listed = reg.list_notifications(&cw.agent_id);
    let ids: Vec<&str> = listed.as_array().unwrap().iter().map(|w| w["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![id1, id2, id3], "must list registration order, oldest first, got: {listed}");
}

// ---------- unified `gh` poller (#406): one background loop, one clock ----------
//
// Both `gh`-polling features (the notification backend #243 and the idle-tick
// intake gate #332) spend the SAME account's GitHub API budget, so they must
// not be clocked by two independent threads that share no accounting of it.
// The tests below pin the two halves of that: the app really does start
// exactly one such loop, and one wake of it services both features.
//
// Same "no test shells out to `gh`" discipline as the notify section above:
// `gh_poll_tick(now, &results)` is the decision seam, driven with a synthetic
// result map. The intake half runs for real here (an autonomous group with
// the gate on, so its effect is observable and not merely its decision flag —
// rev-157 blocking 1) and still spawns nothing: `gh_capture` returns at its
// "no such directory" guard for the test group's nonexistent repo path,
// before a program is resolved, so `poll_intake` executes its whole body
// against two `Err`s exactly as it would against a rate-limited `gh`.

/// One wake of the single loop services BOTH features: the notify half
/// resolves a due watch and the intake half runs its due-group scan, off the
/// same injected instant. Before #406 these were two threads on two clocks,
/// so "one tick did both" was not a statement that could be made at all.
///
/// Asserts the intake half's EFFECT (rev-157 blocking 1), not the
/// `intake_scanned` flag: the flag is only the scheduler's decision and stays
/// true even if the `poll_intake(now)` call beneath it is deleted, which
/// would silently stop every autonomous group from ever being scanned. The
/// per-group stamp only moves if the scan actually ran.
#[test]
fn one_gh_poll_tick_services_both_notify_and_intake() {
    // An AUTONOMOUS group with the intake gate on, so `poll_intake` runs its
    // whole body instead of returning at the no-due-groups guard. Still no
    // subprocess: the group's repo path does not exist, so `gh_capture`
    // returns at its "no such directory" check before a program is even
    // resolved — the section's "no test shells out to `gh`" rule holds.
    let (reg, _d, gid, oid) = autonomous_setup_with_gate(Some(1), 180);
    let watch = reg
        .register_notification(&gid, &oid, notify::Condition::PrChecks { pr: 406 }, "merge if green".into(), 5)
        .unwrap();
    assert_eq!(reg.intake_last_poll_at(&gid), None, "nothing has scanned this group yet");

    // 6 minutes on: the watch is past its TTL (the notify half's own,
    // unchanged, time-based decision) and the intake scan has never run.
    let wake = now_ms() + 6 * 60_000;
    let tick = reg.gh_poll_tick(wake, &HashMap::new());
    assert_eq!(tick.fired, vec![watch.id], "the notify half must still resolve a due watch on this wake");
    assert!(tick.intake_scanned, "and the SAME wake must decide the intake scan is due");
    assert_eq!(
        reg.intake_last_poll_at(&gid),
        Some(wake),
        "…and must actually RUN it: only a scan that reached the gh call stamps the group it polled"
    );
}

/// The unification must not change either feature's cadence: the notify half
/// runs on EVERY wake (watch-firing latency stays the loop's 30s), while the
/// intake half keeps its own coarser 60s scan floor instead of inheriting the
/// faster wake it now rides on.
#[test]
fn intake_half_keeps_its_scan_floor_while_notify_runs_every_wake() {
    // Pure cadence first: never-scanned is due, then the 60s floor.
    let t = 1_700_000_000_000u64;
    assert!(intake_scan_due(t, None), "the first wake after launch must scan");
    assert!(!intake_scan_due(t + 59_999, Some(t)), "a wake inside the floor must not scan");
    assert!(intake_scan_due(t + 60_000, Some(t)), "the floor is INTAKE_POLL_SCAN_INTERVAL, exactly");
    // rev-157 non-blocking 2: a backwards clock jump (NTP correction, VM
    // resume, a manual set) leaves the stamp in the FUTURE. A plain elapsed
    // check stalls the scan for the whole size of the jump — the old
    // `sleep`-driven thread could not be stalled this way, so the unification
    // must not introduce it.
    assert!(intake_scan_due(t, Some(t + 5 * 60_000)), "a stamp in the future must rescan, not stall for the jump");

    // And through the tick, on a live registry: wake 1 scans, wake 2 (one
    // 30s wake later) does not — but still fires a watch that resolved in
    // between, which is the half that must never be starved by the other's
    // slower cadence. As above, the group is autonomous with the gate on so
    // the intake half's effect (its per-group stamp) is observable, and its
    // `gh` calls stop at the nonexistent repo path with no subprocess.
    let (reg, _d, gid, oid) = autonomous_setup_with_gate(Some(1), 180);
    let expiring = reg
        .register_notification(&gid, &oid, notify::Condition::PrChecks { pr: 406 }, "expires first".into(), 5)
        .unwrap()
        .id;
    let long_lived = reg
        .register_notification(&gid, &oid, notify::Condition::PrChecks { pr: 407 }, "resolves later".into(), 240)
        .unwrap()
        .id;

    let wake1 = now_ms() + 6 * 60_000;
    let first = reg.gh_poll_tick(wake1, &HashMap::new());
    assert_eq!(first.fired, vec![expiring], "wake 1 expires the short-TTL watch");
    assert!(first.intake_scanned, "wake 1 is the first scan ever, so it scans");
    assert_eq!(reg.intake_last_poll_at(&gid), Some(wake1), "wake 1 really polled the group, not merely flagged it");

    let mut met = HashMap::new();
    met.insert(long_lived.clone(), notify::PollResult::Met { summary: "SUCCESS — all 3 checks passed".into() }.into());
    let second = reg.gh_poll_tick(wake1 + 30_000, &met);
    assert_eq!(second.fired, vec![long_lived], "wake 2 must still deliver the notify half — every wake does");
    assert!(!second.intake_scanned, "…while the intake scan holds its own 60s floor on that same wake");
    assert_eq!(
        reg.intake_last_poll_at(&gid),
        Some(wake1),
        "and the scan really did NOT run on wake 2: the stamp is still wake 1's"
    );

    // A wake past the floor scans again — the floor is a floor, not a latch.
    let third = reg.gh_poll_tick(wake1 + 60_000, &HashMap::new());
    assert!(third.intake_scanned, "the next wake past the floor scans again");
    assert_eq!(reg.intake_last_poll_at(&gid), Some(wake1 + 60_000), "…and that scan ran too, restamping the group");
}

// ---------- bounding one poll tick (#656): the subprocess wait, the intake fan-out ----------
//
// Still no `gh` and no agent CLI anywhere in this section. The subprocess
// tests drive `capture_with_timeout` — the exact body `gh_capture` runs, with
// the `gh` argv swapped for a deliberately slow / chatty / failing shell
// command — and the intake test reaches `poll_intake` on a live registry
// whose groups point at nonexistent repo paths, so every `gh_capture` inside
// it returns at the "no such directory" guard before a program is resolved.

/// A `Command` running `script` through the host shell: the portable way to
/// spawn a child that behaves a specific way (stalls, floods its pipe, exits
/// non-zero) without shelling out to `gh`, which no test here may do.
fn shell_command(script: &str) -> std::process::Command {
    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C");
        c
    } else {
        let mut c = std::process::Command::new("sh");
        c.arg("-c");
        c
    };
    cmd.arg(script);
    cmd
}

/// A shell script that does nothing for `secs` seconds. `ping` is the
/// dependency-free Windows sleep: `-n N` sends N pings a second apart, so
/// N = secs + 1 loopback pings take about `secs`.
fn sleep_script(secs: u32) -> String {
    if cfg!(windows) { format!("ping -n {} 127.0.0.1 >NUL", secs + 1) } else { format!("sleep {secs}") }
}

/// A `Command` that streams `path` to stdout. Deliberately NOT built through
/// [`shell_command`]: a path baked into a `cmd /C <script>` string has to be
/// quoted, and `Command` then escapes those quotes the MSVC way (`\"`), which
/// `cmd.exe` does not understand — the child fails with "The filename,
/// directory name, or volume label syntax is incorrect" instead of reading
/// the file. Passing the path as its own argv element lets `Command` quote it
/// the one way `cmd` parses.
fn cat_command(path: &Path) -> std::process::Command {
    if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "type"]).arg(path);
        c
    } else {
        let mut c = std::process::Command::new("cat");
        c.arg(path);
        c
    }
}

/// A shell script whose child leaves a GRANDCHILD holding the inherited
/// pipes after the direct child is killed — the one case that justifies not
/// joining the readers, and therefore the one that can actually grow the
/// backlog. On Windows `cmd` spawns `ping` as a separate process (no `>NUL`
/// here, deliberately: both pipes stay inherited); on Unix the backgrounded
/// `sleep` survives the shell that started it.
fn hold_pipes_script() -> String {
    if cfg!(windows) { "ping -n 31 127.0.0.1".to_string() } else { "sleep 30 & sleep 30".to_string() }
}

/// A child still running at the deadline must be REPORTED, not waited out —
/// this is the primitive every wait on a child now goes through, so a
/// regression here is a regression in both the main wait and the post-kill
/// reap at once.
#[test]
fn wait_bounded_reports_none_for_a_child_still_running_at_the_deadline() {
    let mut child = shell_command(&sleep_script(20))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the test child");
    let started = std::time::Instant::now();
    let verdict = wait_bounded(&mut child, Duration::from_millis(500)).expect("polling a live child is not an error");
    let waited = started.elapsed();
    let _ = child.kill();
    assert!(verdict.is_none(), "a child alive at the deadline must report None rather than block until it exits");
    assert!(waited < Duration::from_secs(5), "…and must report it AT the deadline (waited {waited:?} on a 20s child)");
}

/// …while a child that finishes inside the bound is reported with its real
/// status, so the bound never costs a result that was there to be had.
#[test]
fn wait_bounded_reports_the_status_of_a_child_that_exits_first() {
    let mut child = shell_command("exit 7")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the test child");
    let status = wait_bounded(&mut child, Duration::from_secs(10))
        .expect("polling is not an error")
        .expect("a child that exits inside the bound must be reported, not timed out");
    assert_eq!(status.code(), Some(7), "the child's own status must survive the bounded wait");
}

/// A stalled `gh` — the whole reason this bound exists — must not hold the
/// caller for the child's lifetime. Since #406 there is ONE loop making `gh`
/// calls, so an unbounded wait here stops every `notify_when` notice in the
/// process, and the fleet's "register the watch, end the turn" discipline
/// rests on those notices arriving.
///
/// The child outlives its bound by 20x, so returning early cannot be luck,
/// and the assertion is on ELAPSED time rather than on the message alone: an
/// implementation that reported "timed out" only after `output()` finally
/// returned would satisfy the string and none of the point.
#[test]
fn capture_with_timeout_stops_waiting_on_a_stalled_child() {
    let _serial = capture_lock();
    let started = std::time::Instant::now();
    let err = OrchRegistry::capture_with_timeout(shell_command(&sleep_script(20)), Duration::from_secs(1))
        .expect_err("a child that outlives the bound must fail the capture, not be waited out");
    let waited = started.elapsed();
    assert!(err.contains("timed out"), "the error must name the bound as the cause, not look like a gh failure: {err}");
    assert!(
        waited < Duration::from_secs(10),
        "the wait must be bounded by the timeout, not by the child (slept {waited:?} against a 20s child)"
    );
}

/// The bound is a ceiling, not a schedule: a child that finishes returns its
/// own stdout, immediately, exactly as `Command::output()` did.
#[test]
fn capture_with_timeout_returns_a_finished_childs_stdout() {
    let _serial = capture_lock();
    let out = OrchRegistry::capture_with_timeout(shell_command("echo loomux-656"), Duration::from_secs(10))
        .expect("a fast child must succeed");
    assert!(out.contains("loomux-656"), "stdout must be captured verbatim, not summarized: {out:?}");
}

/// The failure mode the reader threads exist to prevent, and the one a
/// "simplify it to try_wait + wait_with_output" edit would reintroduce: a
/// child whose output exceeds the OS pipe buffer blocks writing until someone
/// drains it. Nobody draining means the child never exits, so a HEALTHY `gh
/// issue list` on a busy repo would time out every single tick — the bound
/// turned into a new outage instead of a backstop.
///
/// 512 KiB is comfortably past both platforms' default buffer (64 KiB
/// Windows, 64 KiB Linux, 16 KiB macOS) on stdout alone.
#[test]
fn capture_with_timeout_drains_output_larger_than_a_pipe_buffer() {
    let _serial = capture_lock();
    let dir = tempfile::tempdir().unwrap();
    let big = dir.path().join("big.txt");
    let payload = "loomux-656 ".repeat(48 * 1024); // ~512 KiB
    fs::write(&big, &payload).unwrap();
    let started = std::time::Instant::now();
    let out = OrchRegistry::capture_with_timeout(cat_command(&big), Duration::from_secs(10))
        .expect("a chatty but healthy child must complete, not trip the bound");
    assert!(
        out.len() >= payload.len(),
        "the whole stream must be captured, not one buffer's worth: got {} of {} bytes",
        out.len(),
        payload.len()
    );
    assert!(started.elapsed() < Duration::from_secs(5), "…and must not have needed the bound to get there");
}

/// A child that fails on its own is still a child failure: the timeout must
/// not swallow, relabel, or delay it. `poll_watches` turns this `Err` into a
/// fail-streak increment and `poll_intake` into a skipped half-diff, and both
/// classifications predate the bound.
#[test]
fn capture_with_timeout_reports_a_nonzero_exit_as_the_childs_own_failure() {
    let _serial = capture_lock();
    let started = std::time::Instant::now();
    let err = OrchRegistry::capture_with_timeout(shell_command("exit 3"), Duration::from_secs(10))
        .expect_err("a non-zero exit is an Err, exactly as `output()` made it");
    assert!(!err.contains("timed out"), "a child that exited must never be reported as a timeout: {err}");
    assert!(err.contains('3'), "the exit status must survive into the message the caller surfaces: {err}");
    assert!(started.elapsed() < Duration::from_secs(5), "an immediate failure must return immediately");
}

/// **The raw variant hands back what the child did, not a verdict on it**
/// (#698) — which is the entire reason `capture_with_timeout` was split.
///
/// The merge queue's `git ls-remote --exit-code` answers "does this ref exist"
/// as `0` vs `2`, so collapsing a non-zero exit into `Err` (right for a `gh`
/// read, and what the test above pins) would make "no such ref" and "the
/// network is down" the same value — the one confusion the queue's collision
/// check must not have, since one means *push* and the other means *refuse*.
/// Both go through the same bounded machinery, so there is one implementation
/// of the delicate part and two readings of its result.
#[test]
fn capture_raw_with_timeout_reports_a_nonzero_exit_as_data_not_as_an_error() {
    let _serial = capture_lock();
    // `;` is a command separator in `sh` and an ordinary character to
    // `cmd.exe`, which would echo the whole string and exit 0 — a green
    // assertion for the wrong reason. `&` is the separator `cmd` parses.
    let script =
        if cfg!(windows) { "echo keep-me& exit 3" } else { "echo keep-me; exit 3" };
    let (status, stdout, _stderr) = loomux_lib::orchestration::capture_raw_with_timeout(
        shell_command(script),
        Duration::from_secs(10),
    )
    .expect("a child that ran and failed is not an Err here — that is the point");
    assert_eq!(status.code(), Some(3), "the exit CODE is the answer, not a collapsed Err");
    assert!(
        stdout.contains("keep-me"),
        "and its stdout survives a non-zero exit: {stdout:?}"
    );

    // The bound still applies to it — it is the same machinery underneath.
    let started = std::time::Instant::now();
    let err = loomux_lib::orchestration::capture_raw_with_timeout(
        shell_command(&sleep_script(20)),
        Duration::from_secs(1),
    )
    .expect_err("a stalled child must not be waited on forever");
    assert!(err.contains("timed out"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(10), "the bound is what returned, not the child");
}

/// The ceiling policy itself, pure: a capture is admitted while abandoned
/// readers are below the ceiling and refused at it. Stated here so the
/// boundary is pinned without arranging a real leak.
#[test]
fn gh_capture_admission_stops_exactly_at_the_reader_ceiling() {
    assert!(gh_capture_admitted(0), "an empty backlog must never refuse a poll");
    assert!(
        gh_capture_admitted(GH_CAPTURE_MAX_LEAKED_READERS - 1),
        "one slot short of the ceiling is still a poll worth making"
    );
    assert!(!gh_capture_admitted(GH_CAPTURE_MAX_LEAKED_READERS), "the ceiling is a ceiling, not a target");
    assert!(!gh_capture_admitted(GH_CAPTURE_MAX_LEAKED_READERS + 9), "and nothing above it is admitted either");
}

/// The leak really is bounded per PROCESS, not merely per tick: once the
/// backlog is at the ceiling a capture is refused outright — with an error
/// naming the reason, since both callers surface an `Err` — and once those
/// readers end, captures resume on their own.
///
/// Uses the seam rather than a grandchild that holds a pipe: seeded readers
/// block until this test releases them, so both the refusal and the drain are
/// deterministic on every platform instead of depending on how one OS
/// inherits handles.
#[test]
fn a_full_reader_backlog_refuses_captures_until_it_drains() {
    let _serial = capture_lock();

    // Baseline: whatever earlier capture tests left behind (normally zero —
    // killing a child closes its pipes and its readers end at once).
    let holds = seed_leaked_readers_for_test(GH_CAPTURE_MAX_LEAKED_READERS);
    assert!(
        gh_capture_live_readers() >= GH_CAPTURE_MAX_LEAKED_READERS,
        "the seam must fill the backlog the way an abandoned reader does"
    );

    let err = OrchRegistry::capture_with_timeout(shell_command("echo unreachable"), Duration::from_secs(10))
        .expect_err("a full backlog must refuse the capture rather than spawn another child");
    assert!(err.contains("backlog"), "the refusal must name itself, not look like a gh failure: {err}");

    // Releasing them is the whole point of the bound: this is a ceiling, not
    // a latch, so polling must resume without a restart.
    drop(holds);
    let settle = std::time::Instant::now();
    while gh_capture_live_readers() >= GH_CAPTURE_MAX_LEAKED_READERS && settle.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = OrchRegistry::capture_with_timeout(shell_command("echo loomux-656-drained"), Duration::from_secs(10))
        .expect("once the backlog drains, captures must resume on their own");
    assert!(out.contains("loomux-656-drained"), "…and resume as real captures, not as a stub: {out:?}");
}

/// The registration itself: a timed-out capture whose child left a grandchild
/// holding the pipes must PARK its readers where the ceiling can see them.
/// Without this, deleting the two lines that push into the backlog would
/// leave every other test in this section passing — the ceiling would simply
/// never be reached by anything real.
#[test]
fn a_timeout_that_cannot_close_its_pipes_parks_its_readers_in_the_backlog() {
    let _serial = capture_lock();
    let before = gh_capture_live_readers();

    let err = OrchRegistry::capture_with_timeout(shell_command(&hold_pipes_script()), Duration::from_secs(1))
        .expect_err("the child outlives the bound, so this is a timeout");
    assert!(err.contains("timed out"), "precondition: this must be the timeout arm, not another failure: {err}");

    assert!(
        gh_capture_live_readers() > before,
        "a reader still blocked on a grandchild's pipe must be counted, not forgotten — \
         that is the difference between a leak bounded per tick and one bounded per process"
    );
}

/// …and the SAME accounting on the arm that isn't a timeout (#699): when the
/// wait itself errors, the capture used to return through a bare `?`, which
/// dropped both readers on the floor. Uncounted is worse than parked, not
/// better — the ceiling's admission predicate is "how many readers are still
/// blocked", so readers the sweep can never see are exactly the ones that make
/// the bound lie about the process it is supposed to bound.
///
/// Driven through the injected-wait seam because reaching this arm for real
/// needs `Child::try_wait` to error, which no supported platform does on
/// demand; every other step (spawn, readers, kill, reap, accounting) is the
/// same code the production entry point runs.
///
/// Counted on the backlog list **unswept**, and from a drained baseline —
/// unlike the timeout test above, which can afford a swept before/after delta
/// because its grandchild holds the pipes open across the whole call. Neither
/// shortcut survives here, and both were tried:
/// - a *swept* count reads the same whether the handles were parked or
///   dropped, because the forced error arrives at once and the kill closes
///   both pipes within milliseconds. That race was green on macos and red on
///   ubuntu/windows.
/// - a *delta* against whatever earlier capture tests left parked is measured
///   against a baseline the call itself moves: its opening sweep drops the
///   handles that have since ended (CI: before 4, after 4, with both readers
///   correctly parked).
///
/// So: start from zero and assert the exact number. What is being pinned is
/// the accounting, not how long the readers happened to survive it.
#[test]
fn a_wait_error_parks_both_its_readers_where_the_ceiling_can_see_them() {
    let _serial = capture_lock();
    let _drained = drain_parked_readers_for_test();
    assert_eq!(gh_capture_parked_readers(), 0, "precondition: the baseline this test measures against is empty");

    let err = loomux_lib::orchestration::capture_raw_with_failing_wait_for_test(
        shell_command(&sleep_script(20)),
        Duration::from_secs(1),
    )
    .expect_err("a wait that fails is a failed capture");
    assert!(err.contains("forced wait failure"), "precondition: this must be the wait-error arm, not the timeout: {err}");

    assert_eq!(
        gh_capture_parked_readers(),
        2,
        "BOTH readers a failed wait abandons must be handed to the ceiling, not dropped where the \
         sweep can never see them — one or none is how a bound comes to understate its own backlog"
    );
}

/// The other half of "exactly once" (rev-lead N1): the success arm **joins**
/// both readers, so it must park neither. The two tests above pin "at least
/// once" on the abandonment arm and say nothing about this one — an edit that
/// parked the handles on the way out as well as joining them would pass every
/// other test in this section while growing the backlog on the *healthy* path,
/// until the ceiling refused captures because captures had been succeeding.
///
/// Same instruments as the parking test, for the same reasons: drained
/// baseline, unswept count.
#[test]
fn a_successful_capture_parks_neither_of_its_readers() {
    let _serial = capture_lock();
    let _drained = drain_parked_readers_for_test();
    assert_eq!(gh_capture_parked_readers(), 0, "precondition: the baseline this test measures against is empty");

    let out = OrchRegistry::capture_with_timeout(shell_command("echo loomux-699"), Duration::from_secs(10))
        .expect("a fast child must succeed");
    assert!(out.contains("loomux-699"), "precondition: this must be the success arm, not a refusal: {out:?}");

    assert_eq!(
        gh_capture_parked_readers(),
        0,
        "the success arm joins both readers, so it must park neither — a handle both joined and \
         parked is counted twice, and the ceiling then shrinks on captures that WORKED"
    );
}

/// The other half of "abandon" on that arm: the child is KILLED, not simply
/// forgotten (#699). Dropping a `std::process::Child` does not kill the
/// process, so the bare-`?` return left a live child behind holding both pipes
/// — which is also why its readers would never have ended on their own.
///
/// Observed through the child's own side effect rather than a PID: the script
/// writes a sentinel a beat after it starts, so a child that survived the
/// early return leaves proof, and one that was killed cannot. `current_dir` +
/// a relative name keeps the redirect target out of the script string, which
/// `cmd.exe` and `Command`'s MSVC quoting disagree about (see `cat_command`).
#[test]
fn a_wait_error_kills_the_child_it_abandons() {
    let _serial = capture_lock();
    let dir = tempfile::tempdir().unwrap();
    let sentinel = dir.path().join("survived.txt");
    let script = if cfg!(windows) {
        "ping -n 3 127.0.0.1 >NUL & echo x > survived.txt"
    } else {
        "sleep 2; echo x > survived.txt"
    };
    let mut cmd = shell_command(script);
    cmd.current_dir(dir.path());

    let err = loomux_lib::orchestration::capture_raw_with_failing_wait_for_test(cmd, Duration::from_secs(1))
        .expect_err("a wait that fails is a failed capture");
    assert!(err.contains("forced wait failure"), "precondition: this must be the wait-error arm: {err}");

    // Well past the child's own delay: if it is still alive it has had every
    // chance to write, so this cannot pass by being fast.
    std::thread::sleep(Duration::from_secs(5));
    assert!(
        !sentinel.exists(),
        "the abandoned child must be killed on this arm too — it wrote {} after the capture returned, \
         so the capture left a live child (and two readers that could never end) behind",
        sentinel.display()
    );
}

/// The intake half's per-scan cap, on a live registry rather than only in the
/// pure selector: N autonomous groups falling due on one scan wake used to be
/// 2N sequential `gh` round-trips inside a single tick of the one loop that
/// also delivers every watch notice.
///
/// Asserts the EFFECT (`intake_last_poll_at` moves only for a group the scan
/// actually reached the `gh` call for) and the deferral: the groups the cap
/// left out are polled by the very next scan, so the cap is a queue and not a
/// starvation. Every group's repo path is nonexistent, so the scan runs its
/// whole body against `Err`s and spawns nothing.
#[test]
fn poll_intake_caps_one_scan_and_polls_the_deferred_groups_on_the_next() {
    let (reg, _d) = test_registry();
    let due_groups = MAX_INTAKE_POLLS_PER_TICK + 3;
    let mut gids = Vec::new();
    for i in 0..due_groups {
        // Distinct repo paths: the group id is repo-derived, and a group with
        // no live agents is reused rather than duplicated.
        let g = reg
            .create_group(
                &format!("C:/tmp/repo-656-{i}"),
                Guardrails { intake_poll_minutes: Some(1), idle_tick_fallback_minutes: 180, ..rails() },
            )
            .unwrap();
        reg.set_autonomous(&g.id, true).unwrap();
        gids.push(g.id);
    }

    let scanned = |now: u64| gids.iter().filter(|g| reg.intake_last_poll_at(g) == Some(now)).count();

    let first = now_ms() + 60 * 60_000; // every group is due: none has ever been polled
    reg.poll_intake(first);
    assert_eq!(
        scanned(first),
        MAX_INTAKE_POLLS_PER_TICK,
        "one scan must poll at most the cap, however many groups came due together"
    );

    // The same instant, so nothing has newly become due and the groups just
    // polled are inside their own per-group floor: this scan can only be the
    // deferred ones.
    reg.poll_intake(first);
    let polled_at_all = gids.iter().filter(|g| reg.intake_last_poll_at(g).is_some()).count();
    assert_eq!(polled_at_all, due_groups, "the groups the cap deferred must be picked up by the next scan, not starved");
}

// ---------- group_watches: the group view's "⏳ waiting on …" indicator (#248) ----------

#[test]
fn group_watches_lists_every_agents_live_watch_for_the_group_view() {
    let (reg, _d, _co, cw) = setup_mcp();
    // A second agent in the SAME group with its own watch — this command reads
    // across the whole roster, unlike the self-scoped `list_notifications`.
    let w2 = reg.spawn_agent(&cw.group, Role::Worker, "w2", "task2", false, None).unwrap();
    let cw2 = reg.resolve_token(&w2.token).unwrap();

    let t1 =
        register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "241", "note": "merge if green" })).unwrap();
    let id1 = extract_watch_id(&t1);
    let t2 = register_notify(&reg, &cw2, json!({ "kind": "workflow_run", "run": "17812" })).unwrap();
    let id2 = extract_watch_id(&t2);

    let watches = reg.group_watches(&cw.group);
    let list = watches.as_array().unwrap();
    assert_eq!(list.len(), 2, "must surface both agents' watches, got: {watches}");

    // Oldest-registered first (id1 before id2), matching `list_notifications`.
    // This is the CI-flaky assertion that reddened on ubuntu (fast runner: both
    // registrations landed in the same `registered_ms` millisecond, and a
    // registered_ms-only sort tie-broke on HashMap iteration order — arbitrary
    // and randomized per process). `group_watches`' sort now tie-breaks on
    // `Watch::seq` (a strictly monotonic registration counter), which makes
    // this deterministic without adding a sleep — see that field's doc.
    assert_eq!(list[0]["id"], id1);
    assert_eq!(list[0]["agent"], cw.agent_id);
    assert_eq!(list[0]["kind"], "pr_checks");
    assert_eq!(list[0]["target"], "PR #241 checks");
    assert_eq!(list[0]["note"], "merge if green");
    assert!(
        list[0]["expires_ms"].as_u64().unwrap() > now_ms(),
        "expiry must be a real future timestamp, got: {}",
        list[0]["expires_ms"]
    );

    assert_eq!(list[1]["id"], id2);
    assert_eq!(list[1]["agent"], cw2.agent_id);
    assert_eq!(list[1]["kind"], "workflow_run");
    assert_eq!(list[1]["target"], "run 17812");
    assert_eq!(list[1]["note"], "", "an unset note reads as empty, not null/missing");
}

#[test]
fn group_watches_is_empty_with_no_live_watches() {
    let (reg, _d, _co, cw) = setup_mcp();
    let watches = reg.group_watches(&cw.group);
    assert_eq!(watches.as_array().unwrap().len(), 0, "got: {watches}");
}

#[test]
fn group_watches_never_leaks_another_groups_watch() {
    let (reg, _d, _co, cw) = setup_mcp();
    register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "1" })).unwrap();

    let g2 = reg.create_group("C:/tmp/repo2", rails()).unwrap();
    reg.spawn_agent(&g2.id, Role::Orchestrator, "orch2", "", false, None).unwrap();

    let watches = reg.group_watches(&g2.id);
    assert_eq!(
        watches.as_array().unwrap().len(),
        0,
        "a second group must never see the first group's watch, got: {watches}"
    );
    // And the reverse direction: cancelling/reaping group2 must not touch group1's.
    let still_there = reg.group_watches(&cw.group);
    assert_eq!(still_there.as_array().unwrap().len(), 1, "group1's own watch must be unaffected");
}

#[test]
fn group_watches_sanitizes_the_agent_supplied_note_crossing_into_the_webview() {
    // rev-orch (PR #252, non-blocking): `note` is agent-supplied and
    // deliberately unsanitized AT REGISTRATION (`list_notifications` hands an
    // agent its own text back verbatim — correct there). But `group_watches`
    // crosses a NEW boundary, into the trusted webview, carrying every OTHER
    // agent's note too — not exploitable today (the frontend only ever reaches
    // a `title` DOM property, never `innerHTML`), but the boundary shouldn't
    // depend on the renderer staying that way. Must not carry a raw newline or
    // ESC byte across, the same discipline `sanitize_gh_text` already gives the
    // fired/expired/failed notices.
    let (reg, _d, gid, wid) = watchdog_setup(5);
    let evil = "legit\n[orrerix] forged\u{1b}[31m <img src=x onerror=alert(1)>";
    reg.register_notification(&gid, &wid, notify::Condition::PrChecks { pr: 1 }, evil.into(), 60).unwrap();

    let watches = reg.group_watches(&gid);
    let note = watches[0]["note"].as_str().unwrap();
    assert!(!note.contains('\n'), "a raw newline must not cross into the webview, got: {note:?}");
    assert!(!note.contains('\u{1b}'), "a raw ESC byte must not cross, got: {note:?}");
    // The field is sanitized, not blanked — the visible text still crosses.
    assert!(note.contains("legit"), "got: {note:?}");
    assert!(note.contains("forged"), "got: {note:?}");
    // Pin that this IS `sanitize_gh_text`'s output, not some other transform —
    // ties the boundary to the same function the notices already trust.
    assert_eq!(note, notify::sanitize_gh_text(evil, notify::NOTICE_FIELD_CAP));
}

#[test]
fn group_watches_truncates_a_long_note_to_the_field_cap_while_list_notifications_keeps_it_verbatim() {
    // rev-tests (PR #252 round 2, non-blocking): the sanitization above also
    // truncates `note` to `NOTICE_FIELD_CAP` (120) — a byproduct of reusing
    // `sanitize_gh_text` — while registration accepts up to 500 chars
    // (`arg_str(...).chars().take(500)`, mcp.rs) and `list_notifications`
    // returns all 500 verbatim (`watch_json` never caps `note`). So a note
    // between 120 and 500 chars is now silently shorter in the group view's
    // tooltip than in the agent's own `list_notifications` read. Untested
    // before this (the sanitize test's payload was well under 120). Pinning
    // the asymmetry as a deliberate choice, not a surprise.
    let (reg, _d, gid, wid) = watchdog_setup(5);
    let long_note: String = "n".repeat(200);
    reg.register_notification(&gid, &wid, notify::Condition::PrChecks { pr: 1 }, long_note.clone(), 60).unwrap();

    let listed = reg.list_notifications(&wid);
    assert_eq!(
        listed[0]["note"].as_str().unwrap().chars().count(),
        200,
        "list_notifications must return the note verbatim, uncapped by NOTICE_FIELD_CAP"
    );

    let watches = reg.group_watches(&gid);
    let capped = watches[0]["note"].as_str().unwrap();
    assert_eq!(
        capped.chars().count(),
        notify::NOTICE_FIELD_CAP,
        "group_watches must truncate to the notice field cap, got {} chars: {capped:?}",
        capped.chars().count()
    );
    assert_eq!(capped, "n".repeat(notify::NOTICE_FIELD_CAP));
}

// ---------- watchdog × live watches: suppress a stall while a watch is live (#852, was #248's annotation) ----------

#[test]
fn watchdog_suppresses_the_stall_notice_for_an_agent_holding_a_live_watch() {
    // (a) live watch → no notice, but an audited suppression line.
    let (reg, _d, gid, wid) = watchdog_setup(5);
    // Register a watch directly against the registry (no MCP round-trip
    // needed — this test is about `watchdog_tick`'s wiring to the same
    // `watches` state, not `notify_when` authz, which is covered elsewhere).
    let watch = reg
        .register_notification(&gid, &wid, notify::Condition::PrChecks { pr: 241 }, "merge if green".into(), 60)
        .unwrap();

    // `run_watchdog` (not the lower-level `watchdog_tick`) so the has-watch
    // map is built from the SAME registry read `group_watches` uses — no
    // second store, exercised end to end.
    assert!(
        reg.run_watchdog(FAR).is_empty(),
        "a live watch must SUPPRESS the stall notice (#852), not merely annotate a delivered one"
    );

    let log = fs::read_to_string(reg.state_root().join(gid.as_str()).join("audit.jsonl")).unwrap();
    assert!(
        !log.lines().any(|l| l.contains("watchdog-stall")),
        "no stall notice may be audited while the watch is live, got: {log}"
    );
    let suppressed_line = log
        .lines()
        .find(|l| l.contains("watchdog-suppressed"))
        .unwrap_or_else(|| panic!("the suppression itself must still be audited (diagnosable), got: {log}"));
    assert!(suppressed_line.contains(&wid), "must name the agent, got: {suppressed_line}");
    assert!(suppressed_line.contains(watch.id.as_str()), "must name the watch id, got: {suppressed_line}");
}

#[test]
fn watchdog_stall_audit_does_not_flag_a_watchless_agent() {
    // (c) no watch → unchanged behavior: a plain stalled agent (the common
    // case) is still flagged and audited exactly as before #852.
    let (reg, _d, gid, wid) = watchdog_setup(5);
    assert_eq!(reg.run_watchdog(FAR), vec![wid.clone()]);

    let log = fs::read_to_string(reg.state_root().join(gid.as_str()).join("audit.jsonl")).unwrap();
    let stall_line = log.lines().find(|l| l.contains("watchdog-stall")).unwrap();
    assert!(
        stall_line.contains(&wid),
        "a watchless agent's stall must still be audited and notified, got: {stall_line}"
    );
    assert!(
        !log.lines().any(|l| l.contains("watchdog-suppressed")),
        "no suppression line for an agent with no watch, got: {log}"
    );
}

#[test]
fn watchdog_suppression_does_not_bleed_across_agents_in_the_same_group() {
    // Two stalled workers in the same group, only one holding a watch: the
    // per-agent has_watch lookup must not bleed across agents (e.g. a naive
    // "group has any watch" check would wrongly suppress or flag both).
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", watchdog_rails(5)).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let watching = reg.spawn_agent(&g.id, Role::Worker, "watcher", "work", false, None).unwrap();
    let plain = reg.spawn_agent(&g.id, Role::Worker, "plain", "work", false, None).unwrap();
    reg.register_notification(&g.id, &watching.id, notify::Condition::WorkflowRun { run: 1 }, "".into(), 60)
        .unwrap();

    let flagged = reg.run_watchdog(FAR);
    assert_eq!(
        flagged,
        vec![plain.id.clone()],
        "only the watchless agent may be notified; the watching one is suppressed, got: {flagged:?}"
    );

    let log = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl")).unwrap();
    assert!(
        log.lines().any(|l| l.contains(watching.id.as_str()) && l.contains("watchdog-suppressed")),
        "got: {log}"
    );
    assert!(
        !log.lines().any(|l| l.contains(watching.id.as_str()) && l.contains("watchdog-stall")),
        "the watching agent must never get a watchdog-stall line, got: {log}"
    );
    assert!(
        log.lines().any(|l| l.contains(plain.id.as_str()) && l.contains("watchdog-stall")),
        "the watchless agent must still be flagged, got: {log}"
    );
}

#[test]
fn watchdog_earns_a_fresh_stall_window_once_the_suppressing_watch_resolves() {
    // (b) watch expired + still silent past a FRESH window → notice fires.
    let (reg, _d, gid, wid) = watchdog_setup(5);
    let watch = reg.register_notification(&gid, &wid, notify::Condition::PrChecks { pr: 1 }, "note".into(), 5).unwrap();

    // Stall trips while the watch is live: suppressed, not notified.
    assert!(reg.run_watchdog(FAR).is_empty(), "must suppress while the watch is live");
    let log = fs::read_to_string(reg.state_root().join(gid.as_str()).join("audit.jsonl")).unwrap();
    assert!(log.contains("watchdog-suppressed"), "sanity: must have suppressed, got: {log}");

    // The watch expires — purely time-based (`notify_tick`'s own seam, #243),
    // so FAR (far past any real deadline_ms) expires it regardless of the
    // real wall-clock `register_notification` used to compute that deadline.
    assert_eq!(reg.notify_tick(FAR, &HashMap::new()), vec![watch.id.clone()], "the watch must expire");
    let log = fs::read_to_string(reg.state_root().join(gid.as_str()).join("audit.jsonl")).unwrap();
    assert!(log.contains("watch-expired"), "expiry must be audited, got: {log}");

    // The very next watchdog tick observes the watch is gone: it must hand
    // the agent a FRESH full stall window, not immediately re-fire off
    // whatever was left of the old, already-expired one.
    assert!(
        reg.run_watchdog(FAR).is_empty(),
        "the tick that notices the watch resolved must reset the clock, not fire immediately"
    );

    // Short of a whole fresh window since the reset: still quiet.
    assert!(
        reg.run_watchdog(FAR + 60_000).is_empty(),
        "must not fire before a whole fresh window has elapsed since the watch resolved"
    );

    // A full fresh window elapses with the agent still silent: that
    // combination — resolved watch, still nothing — is a real stall (#852).
    assert_eq!(
        reg.run_watchdog(FAR + 5 * 60_000 + 1),
        vec![wid.clone()],
        "silent through a full fresh window after the watch resolved is a real stall"
    );
    let log = fs::read_to_string(reg.state_root().join(gid.as_str()).join("audit.jsonl")).unwrap();
    let stall_line = log.lines().rev().find(|l| l.contains("watchdog-stall")).unwrap();
    assert!(stall_line.contains(&wid), "got: {stall_line}");
}

#[test]
fn watchdog_message_activity_clears_the_suppression_latch_too() {
    // Review finding 3 on #852: a sign of life unrelated to the watch (a
    // `message_orchestrator` call) must clear `watchdog_watch_suppressed`
    // alongside `watchdog_notified`, exactly like `note_agent_activity`'s
    // other caller. If it didn't: suppress a stall, message the
    // orchestrator, then let the watch resolve with no further activity —
    // the NEXT tick would wrongly take the watch-resolved reset branch on a
    // latch the message already made stale, silently eating that tick (and
    // its rightful notice) and delaying a genuine stall by up to one whole
    // extra window.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", watchdog_rails(5)).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "work", false, None).unwrap();
    let cw = reg.resolve_token(&w.token).unwrap();
    reg.register_notification(&g.id, &w.id, notify::Condition::PrChecks { pr: 1 }, "note".into(), 5).unwrap();

    // Stall trips while the watch is live: suppressed (both latches set).
    assert!(reg.run_watchdog(FAR).is_empty(), "must suppress while the watch is live");
    let log = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl")).unwrap();
    assert!(log.contains("watchdog-suppressed"), "sanity: must have suppressed, got: {log}");

    // A free-form message — a real sign of life, nothing to do with the
    // watch — must reset BOTH latches, not just `watchdog_notified`.
    let _ = dispatch(
        &reg,
        &cw,
        "tools/call",
        &json!({ "name": "message_orchestrator", "arguments": { "text": "checking in" } }),
    );

    // The watch resolves later, with no further activity from the agent.
    assert!(!reg.notify_tick(FAR, &HashMap::new()).is_empty(), "sanity: the watch must expire");

    // If the suppression latch survived the message, this tick would wrongly
    // take the watch-resolved reset branch (latch true, watch gone) and come
    // back empty — silently eating the tick. With the latch correctly
    // cleared by the message, `last_progress_ms` is already the message's
    // real timestamp, and `FAR` is unambiguously past a fresh window from
    // it, so the stall must fire on THIS tick, not the next one.
    assert_eq!(
        reg.run_watchdog(FAR),
        vec![w.id.clone()],
        "a message must fully clear the suppression latch, so the tick after the watch \
         later resolves fires immediately instead of eating one silent tick first"
    );
}

#[test]
fn notify_mark_dead_drops_the_watch_with_no_delivery_attempt() {
    let (reg, _d, _co, cw) = setup_mcp();
    let text = register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "1" })).unwrap();
    let id = extract_watch_id(&text);

    reg.mark_dead(&cw.agent_id, Some(0));

    // Even a Met result for the now-dead agent's watch fires nothing — the
    // watch was already dropped when the agent died, so notify_tick never
    // sees it (covers idle-kill / kill_agent / a crash / planner auto-close
    // identically, since they all funnel through mark_dead).
    let mut results = HashMap::new();
    results.insert(id.clone(), notify::PollResult::Met { summary: "SUCCESS".into() }.into());
    assert!(reg.notify_tick(now_ms(), &results).is_empty(), "a dead agent's watch must never fire");

    let log = fs::read_to_string(reg.state_root().join(cw.group.as_str()).join("audit.jsonl")).unwrap();
    assert!(log.contains("watch-cleanup"), "mark_dead must audit the watch cleanup, got: {log}");
    assert!(!log.contains("watch-fired"), "no delivery/fire may be attempted, got: {log}");
}

#[test]
fn notify_fail_streak_of_three_consecutive_failures_cancels_the_watch() {
    let (reg, _d, _co, cw) = setup_mcp();
    let text = register_notify(&reg, &cw, json!({ "kind": "workflow_run", "run": "555" })).unwrap();
    let id = extract_watch_id(&text);
    let mut fail = HashMap::new();
    fail.insert(id.clone(), notify::PollResult::Failed { why: "gh-not-found".into() }.into());

    assert!(reg.notify_tick(now_ms(), &fail).is_empty(), "one failure must not cancel");
    assert!(reg.notify_tick(now_ms(), &fail).is_empty(), "two failures must not cancel yet");
    assert!(reg.list_notifications(&cw.agent_id).to_string().contains(&id), "must survive two failures");

    assert_eq!(reg.notify_tick(now_ms(), &fail), vec![id.clone()], "the third consecutive failure must cancel");
    let log = fs::read_to_string(reg.state_root().join(cw.group.as_str()).join("audit.jsonl")).unwrap();
    assert!(log.contains("watch-failed"), "the cancellation must be audited, got: {log}");
    assert!(log.contains("gh-not-found"), "the audit must carry the reason, got: {log}");
}

#[test]
fn notify_fail_streak_resets_on_an_intervening_healthy_poll() {
    // rev-tests (PR #247): the prior version of this test drove fail, fail,
    // Met and asserted a fire — but the Met arm in notify_tick fires
    // UNCONDITIONALLY; it never reads fail_streak, so that assertion passed
    // whether or not the reset existed and could never catch a regression.
    // The actual "consecutive" contract only shows up with a HEALTHY poll
    // (Pending) between two failures: that must zero the streak, so a third,
    // non-consecutive failure afterward must NOT cancel the watch. Verified:
    // passes on the shipped code; goes red if the `PollResult::Pending =>
    // fail_streak = 0` line is deleted (a real gh rate-limit blip followed by
    // a healthy poll followed by another blip would otherwise wrongly
    // cancel a watch that never had two failures in a row).
    let (reg, _d, _co, cw) = setup_mcp();
    let text = register_notify(&reg, &cw, json!({ "kind": "workflow_run", "run": "777" })).unwrap();
    let id = extract_watch_id(&text);
    let mut fail = HashMap::new();
    fail.insert(id.clone(), notify::PollResult::Failed { why: "transient".into() }.into());
    let mut pending = HashMap::new();
    pending.insert(id.clone(), notify::PollResult::Pending.into());

    reg.notify_tick(now_ms(), &fail);
    reg.notify_tick(now_ms(), &fail);
    reg.notify_tick(now_ms(), &pending); // a healthy poll resets the streak to 0
    assert!(
        reg.notify_tick(now_ms(), &fail).is_empty(),
        "the limit is CONSECUTIVE failures: a healthy poll in between must reset the streak, \
         so this 3rd non-consecutive failure must NOT cancel the watch"
    );
    assert!(reg.list_notifications(&cw.agent_id).to_string().contains(&id), "watch must survive");
}

#[test]
fn notify_paused_group_does_not_fire_or_poll() {
    let (reg, _d, _co, cw) = setup_mcp();
    let text = register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "1", "expires_minutes": 5 })).unwrap();
    let id = extract_watch_id(&text);
    reg.pause_group(&cw.group).unwrap();

    let mut met = HashMap::new();
    met.insert(id.clone(), notify::PollResult::Met { summary: "SUCCESS".into() }.into());
    // Tick well past the deadline WITH a Met result in hand — a paused group
    // must not fire (the delivery would be into a pane the human deliberately
    // silenced).
    let far_future = now_ms() + 60 * 60_000;
    assert!(reg.notify_tick(far_future, &met).is_empty(), "a paused group must not fire");
    assert!(reg.list_notifications(&cw.agent_id).to_string().contains(&id), "the watch must survive the pause");

    reg.resume_group(&cw.group).unwrap();
    assert_eq!(reg.notify_tick(far_future, &met), vec![id], "resuming must let the outstanding Met result fire");
}

#[test]
fn notify_paused_group_freezes_the_ttl_clock_across_a_long_pause() {
    // rev-orch (PR #247): the prior version of this test resumed and ticked
    // with a MET result in hand — the Met arm fires and `continue`s BEFORE
    // the expiry check ever runs, so that assertion passed whether or not
    // the TTL clock was actually frozen during the pause. This is the case
    // that actually discriminates: pause, let a tick observe the pause
    // (mirroring the live poller's cadence — this is what lets notify_tick
    // learn the pause started here), a long simulated pause with NO further
    // ticks, then resume and tick with Pending/no result in hand. A watch
    // whose TTL clock was not frozen expires the instant this tick runs;
    // one whose clock WAS frozen is still listed with its TTL effectively
    // intact.
    let (reg, _d, _co, cw) = setup_mcp();
    let text = register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "1", "expires_minutes": 5 })).unwrap();
    let id = extract_watch_id(&text);
    let t0 = now_ms();

    reg.pause_group(&cw.group).unwrap();
    // A tick shortly after the pause begins — the poller ticks every 30s
    // regardless of any group's pause state, so this is what the live
    // system actually does; skipping it would test a scenario the freeze
    // was never meant to handle (see notify_tick's doc).
    assert!(reg.notify_tick(t0 + 1_000, &HashMap::new()).is_empty(), "paused: no poll, no fire");

    // A long pause — 1 simulated hour, twelve times the 5-minute TTL — with
    // no further ticks at all while paused.
    reg.resume_group(&cw.group).unwrap();
    let after_resume = t0 + 60 * 60_000;
    let fired = reg.notify_tick(after_resume, &HashMap::new());
    assert!(fired.is_empty(), "a paused watch's TTL clock must not advance while paused, got: {fired:?}");
    assert!(
        reg.list_notifications(&cw.agent_id).to_string().contains(&id),
        "the watch must survive the pause with its TTL intact"
    );

    // And it still fires normally once its condition is actually met.
    let mut met = HashMap::new();
    met.insert(id.clone(), notify::PollResult::Met { summary: "SUCCESS".into() }.into());
    assert_eq!(reg.notify_tick(after_resume + 1_000, &met), vec![id], "must still fire once genuinely met");
}

#[test]
fn notify_stale_pause_entry_is_reconciled_even_while_its_group_has_no_watches() {
    // rev-orch (PR #247 round 2), "B1": the freeze reconcile used to build its
    // scan from "groups that currently hold a watch". A group that emptied
    // out entirely while still paused (its one worker idle-killed, cancelled,
    // or crashed — all routine, all funnel through mark_dead) dropped out of
    // that scan completely — no tick could even see the group to reconcile
    // its `paused_watch_since` entry, so it sat stranded, untouched, straight
    // through the resume. The entry only got consumed once SOME watch
    // finally reappeared in that group, at which point the elapsed span was
    // computed from the ORIGINAL pause observation all the way to that much
    // later moment — charging a completely unrelated, freshly-registered
    // watch for time it never lived through.
    let (reg, _d, _co, cw) = setup_mcp();
    register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "1", "expires_minutes": 5 })).unwrap();
    reg.pause_group(&cw.group).unwrap();
    let pause_observed_at = now_ms();
    // Observe the pause starting (mirrors the live poller's cadence).
    reg.notify_tick(pause_observed_at, &HashMap::new());

    // The group empties out entirely while still paused.
    reg.mark_dead(&cw.agent_id, Some(0));
    assert!(reg.list_notifications(&cw.agent_id).as_array().unwrap().is_empty());

    reg.resume_group(&cw.group).unwrap();
    // A tick occurs while the group is resumed but OWNS ZERO WATCHES — the
    // exact gap the old "scan groups with watches" reconcile skipped
    // entirely. A real pause of only ~5 seconds.
    let resumed_tick_at = pause_observed_at + 5_000;
    assert!(reg.notify_tick(resumed_tick_at, &HashMap::new()).is_empty());

    // Much later, a fresh, entirely unrelated watch registers into this
    // (long since resumed) group, via a fresh agent (the old one is dead).
    let w2 = reg.spawn_agent(&cw.group, Role::Worker, "w2", "t", false, None).unwrap();
    let c2 = reg.resolve_token(&w2.token).unwrap();
    let text2 = register_notify(&reg, &c2, json!({ "kind": "pr_checks", "pr": "2", "expires_minutes": 5 })).unwrap();
    let id2 = extract_watch_id(&text2);
    let registered_ms2 =
        reg.list_notifications(&c2.agent_id).as_array().unwrap()[0]["registered_ms"].as_u64().unwrap();

    // 6 minutes past ITS OWN registration — past its ordinary 5-min TTL, and
    // nowhere near the multi-minute-plus-the-whole-original-pause span the
    // bug would have granted it via the stale entry.
    let fired = reg.notify_tick(registered_ms2 + 6 * 60_000, &HashMap::new());
    assert_eq!(
        fired,
        vec![id2.clone()],
        "a watch registered into a group long since resumed must expire on its own ordinary TTL, \
         not inherit a stale pre-resume pause span from a watch it never coexisted with, got: {fired:?}"
    );
}

#[test]
fn notify_watch_registered_mid_pause_is_credited_only_the_span_it_actually_lived_through() {
    // rev-orch (PR #247 round 2), "B2": the per-group pause span used to be
    // applied to EVERY watch in the group with no regard for when it
    // registered. A watch registered mid-pause — panes keep running while
    // paused, only prompt DELIVERY is suppressed, so `notify_when` still
    // works — got charged for the part of the pause that elapsed before it
    // even existed.
    let (reg, _d, _co, cw) = setup_mcp();
    reg.pause_group(&cw.group).unwrap();

    // The tick mechanism observes the pause starting well "before" the watch
    // below will register (paused_watch_since is keyed off whatever `now` a
    // tick is called with, never real wall-clock — see notify_tick's doc).
    let pause_observed_at = now_ms() - 3_600_000; // 1h "before", in that timeline
    reg.notify_tick(pause_observed_at, &HashMap::new());

    // The watch registers well INTO that pause (registered_ms is real
    // wall-clock, stamped by register_notification itself, so it lands well
    // after pause_observed_at in the same timeline).
    let text = register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "1", "expires_minutes": 5 })).unwrap();
    let id = extract_watch_id(&text);
    let registered_ms = reg.list_notifications(&cw.agent_id).as_array().unwrap()[0]["registered_ms"].as_u64().unwrap();

    // Resume exactly 1 minute after the watch actually registered: it only
    // ever overlapped ~1 minute of the pause, even though the GROUP's
    // observed pause span (from pause_observed_at) is over an hour.
    reg.resume_group(&cw.group).unwrap();
    let resume_tick_at = registered_ms + 60_000;
    assert!(reg.notify_tick(resume_tick_at, &HashMap::new()).is_empty(), "must not fire on the resuming tick itself");

    // 9 minutes after its own registration: past its ordinary 5-min TTL plus
    // the ~1 minute it could legitimately have earned mid-pause (6 min
    // total) — nowhere near the ~61 minutes the bug would have credited it.
    let fired = reg.notify_tick(registered_ms + 9 * 60_000, &HashMap::new());
    assert_eq!(
        fired,
        vec![id.clone()],
        "a watch registered mid-pause must be credited only the pause span it actually lived \
         through (~1 min here), not the group's whole ~61-minute observed pause span, got: {fired:?}"
    );
}

#[test]
fn notify_tools_are_denied_to_a_planner_in_listing_and_dispatch() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let planner = reg.spawn_agent(&g.id, Role::Planner, "plan", "plan issue #7", false, None).unwrap();
    let cp = reg.resolve_token(&planner.token).unwrap();

    // Cosmetic filter: not even listed.
    let tools: Vec<String> = dispatch(&reg, &cp, "tools/list", &Value::Null).unwrap()["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    for name in ["notify_when", "list_notifications", "cancel_notification"] {
        assert!(!tools.contains(&name.to_string()), "a planner must not see {name}");
    }

    // The real gate: a direct call is denied, not silently accepted, because
    // the listing filter is cosmetic and the dispatch arm re-checks.
    let err = register_notify(&reg, &cp, json!({ "kind": "pr_checks", "pr": "1" })).unwrap_err();
    assert!(err.contains("permission denied"), "got: {err}");
}

#[test]
fn cancel_notification_is_owner_scoped_with_no_id_leak() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w1 = reg.spawn_agent(&g.id, Role::Worker, "w1", "t1", false, None).unwrap();
    let w2 = reg.spawn_agent(&g.id, Role::Worker, "w2", "t2", false, None).unwrap();
    let c1 = reg.resolve_token(&w1.token).unwrap();
    let c2 = reg.resolve_token(&w2.token).unwrap();
    let text = register_notify(&reg, &c1, json!({ "kind": "pr_checks", "pr": "1" })).unwrap();
    let id = extract_watch_id(&text);

    // w2 tries to cancel w1's watch. The rejection must echo back exactly
    // "unknown notification: <id>" — the same shape a genuinely nonexistent
    // id gets (see below) — never anything that confirms the id exists but
    // belongs to someone else (e.g. "not yours", "owned by w-1").
    let cross = dispatch(&reg, &c2, "tools/call", &json!({ "name": "cancel_notification", "arguments": { "id": id } })).unwrap();
    assert_eq!(cross["isError"], true);
    let cross_text = cross["content"][0]["text"].as_str().unwrap();
    assert_eq!(cross_text, format!("unknown notification: {id}"), "must not leak that the id exists, got: {cross_text}");

    // A truly nonexistent id, from the actual owner, hits the exact same
    // template (only the id itself differs) — the anti-leak property is
    // that "not yours" and "never existed" are the same wording, never a
    // distinguishing suffix.
    let missing = dispatch(&reg, &c1, "tools/call", &json!({ "name": "cancel_notification", "arguments": { "id": "n-999" } })).unwrap();
    assert_eq!(
        missing["content"][0]["text"].as_str().unwrap(),
        "unknown notification: n-999",
        "a nonexistent id must get the identical template as the cross-owner rejection above"
    );

    // The true owner can still cancel it.
    let ok = dispatch(&reg, &c1, "tools/call", &json!({ "name": "cancel_notification", "arguments": { "id": id } })).unwrap();
    assert_eq!(ok["isError"], false);
    assert!(!reg.list_notifications(&c1.agent_id).to_string().contains(&id));
}

#[test]
fn notify_per_agent_cap_rejects_a_fifth_naming_the_cap() {
    let (reg, _d, _co, cw) = setup_mcp();
    for i in 1..=4u32 {
        register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": i.to_string() })).unwrap();
    }
    let err = register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "99" })).unwrap_err();
    assert!(err.contains("guardrail"), "got: {err}");
    assert!(err.contains(&notify::MAX_WATCHES_PER_AGENT.to_string()), "must name the cap, got: {err}");
}

#[test]
fn notify_group_cap_rejects_even_when_the_agent_itself_has_room() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", Guardrails { max_agents: 5, ..rails() }).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w1 = reg.spawn_agent(&g.id, Role::Worker, "w1", "t", false, None).unwrap();
    let w2 = reg.spawn_agent(&g.id, Role::Worker, "w2", "t", false, None).unwrap();
    let w3 = reg.spawn_agent(&g.id, Role::Worker, "w3", "t", false, None).unwrap();
    let callers = [
        reg.resolve_token(&orch.token).unwrap(),
        reg.resolve_token(&w1.token).unwrap(),
        reg.resolve_token(&w2.token).unwrap(),
        reg.resolve_token(&w3.token).unwrap(),
    ];
    // 4 agents x 3 watches each = 12, the group cap, with every agent still
    // 1 under its own per-agent cap of 4.
    let mut n = 0u32;
    for c in &callers {
        for _ in 0..3 {
            n += 1;
            register_notify(&reg, c, json!({ "kind": "pr_checks", "pr": n.to_string() })).unwrap();
        }
    }
    let err = register_notify(&reg, &callers[0], json!({ "kind": "pr_checks", "pr": "999" })).unwrap_err();
    assert!(err.contains("guardrail"), "got: {err}");
    assert!(
        err.contains(&notify::MAX_WATCHES_PER_GROUP.to_string()),
        "must name the GROUP cap (the agent itself has room), got: {err}"
    );
}

#[test]
fn notify_rejects_unknown_kind_and_bad_targets_but_clamps_expires_minutes() {
    let (reg, _d, _co, cw) = setup_mcp();

    // Unrecognized kind: rejected, never defaulted to either real kind.
    let err = register_notify(&reg, &cw, json!({ "kind": "pr_merged", "pr": "1" })).unwrap_err();
    assert!(err.contains("unrecognized notification kind"), "got: {err}");
    assert!(err.contains("pr_merged"), "must echo the bad value, not silently default, got: {err}");

    // A pr value that isn't a number/#n/URL is rejected before a watch is
    // ever created (never silently coerced to 0 or dropped).
    let err = register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "abc" })).unwrap_err();
    assert!(err.contains("cannot parse a PR number"), "got: {err}");
    let err = register_notify(&reg, &cw, json!({ "kind": "workflow_run", "run": "not-a-run" })).unwrap_err();
    assert!(err.contains("cannot parse a run id"), "got: {err}");

    // expires_minutes is clamped, not rejected, past the ceiling.
    let text =
        register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "1", "expires_minutes": 9999 })).unwrap();
    assert!(text.contains("expires in 240 min"), "must clamp to the max, got: {text}");
}

#[test]
fn notify_rejects_a_present_but_non_integer_expires_minutes_instead_of_silently_defaulting() {
    // A STRING "30" or a fraction is a value the caller actually supplied —
    // silently discarding it to the 60-min default (clamp_expires_minutes(None))
    // would be indistinguishable from the caller never having set it at all.
    // Only an ABSENT key legitimately defaults.
    let (reg, _d, _co, cw) = setup_mcp();
    let err = register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "1", "expires_minutes": "30" })).unwrap_err();
    assert!(err.contains("whole number"), "a string value must be rejected, not defaulted, got: {err}");
    let err = register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "1", "expires_minutes": 30.5 })).unwrap_err();
    assert!(err.contains("whole number"), "a fractional value must be rejected, not defaulted, got: {err}");
    // Absent entirely still defaults, unaffected.
    let text = register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "1" })).unwrap();
    assert!(text.contains("expires in 60 min"), "an absent key must still default, got: {text}");
}

#[test]
fn notify_note_is_capped_at_registration_so_a_watch_cannot_stash_an_unbounded_string() {
    let (reg, _d, _co, cw) = setup_mcp();
    let huge_note = "x".repeat(2000);
    register_notify(&reg, &cw, json!({ "kind": "pr_checks", "pr": "1", "note": huge_note })).unwrap();
    let listed = reg.list_notifications(&cw.agent_id);
    let note = listed.as_array().unwrap()[0]["note"].as_str().unwrap();
    assert_eq!(note.chars().count(), 500, "note must be capped at registration, got {} chars", note.chars().count());
}

#[test]
fn run_id_from_a_job_url_is_correct_end_to_end_through_the_mcp_tool() {
    // notify.rs's `run_id_from` unit tests already pin the pure parse; this
    // confirms the fix is actually wired into the dispatch path (the tool
    // used to call the bare `pr_number` tail-digits parse here, which would
    // have registered against the JOB id, not the run).
    let (reg, _d, _co, cw) = setup_mcp();
    let text = register_notify(
        &reg,
        &cw,
        json!({ "kind": "workflow_run", "run": "https://github.com/o/r/actions/runs/17812345/job/98765" }),
    )
    .unwrap();
    assert!(text.contains("run 17812345"), "must resolve to the RUN id, not the job id, got: {text}");
}
