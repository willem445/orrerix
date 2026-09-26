//! Decision-grade structured reports and pane naming.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ---------- decision-grade structured reports (#398) ----------

#[test]
fn report_rejects_an_unknown_outcome() {
    let (reg, _d, _co, cw) = setup_mcp();
    let bad = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "report", "arguments": { "outcome": "shipped", "note": "x" } })).unwrap();
    assert_eq!(bad["isError"], true, "an outcome outside the enum must be rejected, not guessed");
}

#[test]
fn report_requires_status_or_outcome() {
    let (reg, _d, _co, cw) = setup_mcp();
    let bad = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "report", "arguments": { "summary": "x" } })).unwrap();
    assert_eq!(bad["isError"], true, "neither status nor outcome given must be rejected");
}

#[test]
fn report_requires_summary_or_note() {
    let (reg, _d, _co, cw) = setup_mcp();
    let bad = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "report", "arguments": { "outcome": "done" } })).unwrap();
    assert_eq!(bad["isError"], true, "neither summary nor note given must be rejected");
}

#[test]
fn report_outcome_alone_implies_status_for_idle_bookkeeping() {
    // A reviewer reporting a structured `approved`/`request_changes` outcome never
    // has to also pass legacy `status` — the tool derives it, and `approved` means
    // "this agent's turn is over", exactly like a worker's `done`.
    let (reg, _d, _co, cw) = setup_mcp();
    let idle_of = || -> Value {
        reg.list_agents(&cw.group).as_array().unwrap().iter()
            .find(|a| a["id"] == cw.agent_id).unwrap()["idle_since_ms"].clone()
    };
    assert!(idle_of().is_null(), "the tasked agent from setup_mcp starts not idle");
    let _ = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "report", "arguments": { "outcome": "approved", "ref": "#412", "note": "clean" } }));
    assert!(!idle_of().is_null(),
        "outcome: approved (no status given) must still re-idle the agent, derived from the outcome");
}

#[test]
fn report_outcome_progress_does_not_reidle() {
    let (reg, _d, _co, cw) = setup_mcp();
    let idle_of = || -> Value {
        reg.list_agents(&cw.group).as_array().unwrap().iter()
            .find(|a| a["id"] == cw.agent_id).unwrap()["idle_since_ms"].clone()
    };
    let _ = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "report", "arguments": { "outcome": "progress", "note": "still working" } }));
    assert!(idle_of().is_null(), "outcome: progress must not re-idle the agent");
}

#[test]
fn report_note_over_the_cap_does_not_fail_validation() {
    // The cap TRUNCATES (with a stated marker, see report::truncate_note's own
    // tests) rather than rejecting the call — structural enforcement, not a
    // guideline the caller could violate into an error.
    let (reg, _d, _co, cw) = setup_mcp();
    let long_note = "x".repeat(900);
    let r = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "report", "arguments": { "outcome": "done", "ref": "#1", "note": long_note } })).unwrap();
    // Delivery still fails at the PTY in test mode (no pane) — but that failure
    // must be the SAME "no terminal" error every other report gets, never a
    // validation rejection over the note's length.
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("terminal") || text.contains("reported"),
        "an over-cap note must truncate, not fail validation, got: {text}");
}

/// #203's contract, with #3040 N2's demotion folded in: a planner's `done`
/// closes its pane, the orchestrator receives the REPORT, and the exit is
/// recorded on the audit log AFTER that report was delivered.
///
/// **The ordering is still pinned, and this is the test the code cites for it.**
/// Before N2 it was an order between two pane deliveries (`report_at <
/// exit_at`); the exit notice is now an audit row, so the same property is now an
/// order between the delivered report and that row. It still has to hold —
/// a reader reconstructing what happened from the log must not meet the exit
/// before the plan it delivered — and dropping the assertion when the surface
/// moved would have retired a property rather than relocating it, while leaving
/// two comments citing this test for a guarantee it no longer made (rev-std
/// round 1, B1/N1).
///
/// The name changed with it: the old one said `reports_before_exit` about two
/// notices that no longer both exist.
#[test]
fn a_planner_exit_is_audited_after_the_report_is_delivered() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let planner = reg
        .spawn_agent(&g.id, Role::Planner, "plan", "plan issue #7", false, None)
        .unwrap();
    // Pause so deliveries are queued-and-audited, and observable in order —
    // test mode has no pane to type into.
    pause_with_pane(&reg, &g.id, &orch.id, 6200);
    let cp = reg.resolve_token(&planner.token).unwrap();

    let r = dispatch(
        &reg,
        &cp,
        "tools/call",
        &json!({ "name": "report", "arguments": { "status": "done", "summary": "issue #7: plan posted" } }),
    )
    .unwrap();
    assert_eq!(r["isError"], false, "the planner's done report must succeed");

    // The pane is closed: the planner is dead and no longer holds a slot.
    let dead = reg
        .list_agents(&g.id)
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["id"] == json!(planner.id) && a["status"] == json!("dead"));
    assert!(dead, "a planner's done report must close its pane (#203)");

    // The report still reaches the pane — it is the plan's arrival, which is the
    // whole point of the planner.
    let texts = delivered_texts(&reg, &g.id);
    assert!(
        texts.iter().any(|t| t.contains("reports done") && t.contains("plan posted")),
        "orchestrator must receive the done report: {texts:?}"
    );

    // ORDERING, relocated rather than retired. The audit log is append-ordered,
    // so the two rows' positions in it are the record of which happened first:
    // the `prompt` that delivered the report, then the `agent-exit-notice` that
    // replaced the second prompt.
    let log = reg.audit_log(&g.id);
    let report_at = log
        .iter()
        .position(|e| e.action == "prompt"
            && e.detail["text"].as_str().is_some_and(|t| t.contains("plan posted")))
        .expect("the done report must be on the log");
    let exit_at = log
        .iter()
        .position(|e| e.action == "agent-exit-notice"
            && e.detail["agent"] == json!(planner.id))
        .expect("the demoted exit notice must be on the log");
    assert!(report_at < exit_at,
        "the plan's report must be delivered before the exit is recorded — a reader \
         reconstructing this from the log must not meet the exit first (got report at \
         {report_at}, exit at {exit_at})");
    // **The exit notice no longer does** (#3040 N2). What #203 called an ordering
    // guarantee is now the whole argument for the demotion: the report the
    // orchestrator has just read IS the news, and a second prompt saying the same
    // pane is gone was acted on zero times in ten across #3040's census. The
    // audit-row half of this is `a_planner_exit_is_audited_not_announced`; what
    // this test still owns is that the report itself was not collateral.
    assert!(
        !texts.iter().any(|t| t.contains("posted its plan and exited")),
        "the slot-free notice must not reach the pane any more: {texts:?}"
    );
}

#[test]
fn only_a_planner_done_report_auto_closes_the_pane() {
    // The auto-close is scoped narrowly (#203): a *worker's* `done` (PR open,
    // awaiting human review — it stays for follow-ups) and a *planner's*
    // `progress` (still working) must both leave the pane alive.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let worker = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    let planner = reg.spawn_agent(&g.id, Role::Planner, "plan", "task", false, None).unwrap();
    // #569: queue delivery instead of pasting it; keep the report path exercised.
    pause_with_pane(&reg, &g.id, &orch.id, 6201);
    let cw = reg.resolve_token(&worker.token).unwrap();
    let cp = reg.resolve_token(&planner.token).unwrap();

    dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "report", "arguments": { "status": "done", "summary": "PR #1 open" } })).unwrap();
    dispatch(&reg, &cp, "tools/call",
        &json!({ "name": "report", "arguments": { "status": "progress", "summary": "still exploring" } })).unwrap();

    let alive = |id: &str| {
        reg.list_agents(&g.id).as_array().unwrap().iter().any(|a| {
            a["id"] == json!(id) && a["status"] != json!("dead")
        })
    };
    assert!(alive(&worker.id), "a worker's done report must not close its pane");
    assert!(alive(&planner.id), "a planner's progress report must not close its pane");
}

/// A group with a paused orchestrator pane (so deliveries are queued AND
/// audited, which is what `delivered_texts` reads), a worker, and one board row
/// bound to that worker's session. Returns (registry, tempdir, group, worker,
/// worker's caller, task id).
///
/// #1958's tests all need the same three things in place — a delivery PROBE that
/// works, a resolvable board row, and a delegate whose session is on it — and a
/// helper is what stops one of them being silently absent in a test whose whole
/// claim is an absence.
fn report_routing_setup(
) -> (OrchRegistry, tempfile::TempDir, GroupId, AgentEntry, Caller, String) {
    let (reg, d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    pause_with_pane(&reg, &g.id, &orch.id, 6250);
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "ship the thing", false, None).unwrap();
    let session = w.session_id.clone().expect("a spawn mints a session id");
    let t = reg
        .upsert_task(&g.id, "orch-1", None, patch(Some("Ship the thing"), None, None))
        .unwrap();
    reg.upsert_task(
        &g.id,
        "orch-1",
        Some(&t.id),
        TaskPatch { session: Some(session), pr: Some("#900".into()), ..Default::default() },
    )
    .unwrap();
    let cw = reg.resolve_token(&w.token).unwrap();
    (reg, d, g.id, w, cw, t.id)
}

/// The notes on one board row, oldest first.
fn task_notes(reg: &OrchRegistry, group: &GroupId, id: &str) -> Vec<String> {
    reg.tasks(group)
        .into_iter()
        .find(|t| t.id == id)
        .unwrap_or_else(|| panic!("task {id} must exist"))
        .notes
        .into_iter()
        .map(|n| n.text)
        .collect()
}

/// The status/outcome word of every `report` tool call this group audited.
fn audited_reports(reg: &OrchRegistry, group: &GroupId) -> Vec<String> {
    reg.audit_log(group)
        .into_iter()
        .filter(|e| e.action == "tool-call" && e.detail["tool"] == json!("report"))
        .filter_map(|e| {
            let a = e.detail["args"].clone();
            a["outcome"]
                .as_str()
                .or_else(|| a["status"].as_str())
                .map(str::to_string)
        })
        .collect()
}

/// **#1958.** A `progress` report is RECORDED and REROUTED, never delivered: the
/// audit row every MCP call writes, plus a note on the board row it resolves to,
/// and nothing typed into the orchestrator's pane.
///
/// **The positive control is the same session's `done`, in the same test.** An
/// assertion that no line reached the pane passes just as well when the probe is
/// broken, the orchestrator has no pane, or the report never dispatched at all —
/// so the `done` half is what proves the pane was reachable the whole time, and
/// the audit and note halves are what prove the progress report really ran.
#[test]
fn a_progress_report_is_recorded_and_noted_but_never_reaches_the_orchestrators_pane() {
    let (reg, _d, g, w, cw, tid) = report_routing_setup();

    let r = dispatch(&reg, &cw, "tools/call", &json!({ "name": "report", "arguments": {
        "outcome": "progress", "ref": "#900", "note": "rebasing onto main, CI next" } }))
        .unwrap();
    assert_eq!(r["isError"], false, "a progress report must still succeed: {r}");
    // The tool's own answer says what really happened. "reported to orchestrator"
    // would be a false claim to the one caller in a position to act on it.
    let answer = r["content"][0]["text"].as_str().unwrap();
    assert!(
        answer.contains("noted on your board task")
            && answer.contains("never reaches the orchestrator's pane"),
        "the answer must name the reroute, not claim a delivery: {answer}"
    );

    // 1. The audit row, exactly as before this change.
    assert_eq!(
        audited_reports(&reg, &g),
        vec!["progress".to_string()],
        "the tool-call audit row is the floor and must be unchanged"
    );

    // 2. The board note — the trail the human reads beside the pane, and the
    //    one the orchestrator reads on demand with get_task.
    let notes = task_notes(&reg, &g, &tid);
    assert_eq!(notes.len(), 1, "exactly one note, got {notes:?}");
    assert!(
        notes[0].contains("rebasing onto main"),
        "the note must carry the delegate's own words: {}",
        notes[0]
    );
    assert!(
        notes[0].contains(&w.id),
        "…attributed to the delegate that wrote it: {}",
        notes[0]
    );

    // 3. Nothing was typed into the orchestrator's pane.
    let delivered = delivered_texts(&reg, &g);
    assert!(
        !delivered.iter().any(|t| t.contains("reports progress")),
        "a progress report reached the orchestrator's pane: {delivered:#?}"
    );

    // 4. THE POSITIVE CONTROL. Same worker, same session, same pane: a `done`
    //    IS delivered. Without this, every assertion above is satisfied by a
    //    registry that delivers nothing to anybody.
    let r = dispatch(&reg, &cw, "tools/call", &json!({ "name": "report", "arguments": {
        "outcome": "done", "ref": "#900", "note": "CI green, ready for review" } }))
        .unwrap();
    assert_eq!(r["isError"], false, "the control report must succeed: {r}");
    let delivered = delivered_texts(&reg, &g);
    assert!(
        delivered.iter().any(|t| t.starts_with(&format!("[orrerix] {} reports done", w.id))),
        "the control `done` must reach the pane the progress report did not: {delivered:#?}"
    );
    assert!(
        !delivered.iter().any(|t| t.contains("reports progress")),
        "the control delivery must not have carried the progress line with it: {delivered:#?}"
    );
}

/// **#1958.** No resolvable board row is not an error and invents nothing: the
/// audit row is the floor, and a group that does not use the board reports
/// exactly as it did before.
#[test]
fn a_progress_report_with_no_resolvable_task_is_audit_only_and_not_an_error() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap().id;
    let orch = reg.spawn_agent(&g, Role::Orchestrator, "orch", "", false, None).unwrap();
    pause_with_pane(&reg, &g, &orch.id, 6251);
    let w = reg.spawn_agent(&g, Role::Worker, "w", "ship it", false, None).unwrap();
    let cw = reg.resolve_token(&w.token).unwrap();

    // A board row matching NEITHER the session nor the ref, so "resolved
    // nothing" is a real miss rather than an empty board.
    let other = reg
        .upsert_task(&g, "orch-1", None, patch(Some("Someone elses work"), None, None))
        .unwrap();
    reg.upsert_task(
        &g,
        "orch-1",
        Some(&other.id),
        TaskPatch {
            session: Some("11111111-2222-3333-4444-555555555555".into()),
            pr: Some("#4242".into()),
            ..Default::default()
        },
    )
    .unwrap();

    let r = dispatch(&reg, &cw, "tools/call", &json!({ "name": "report", "arguments": {
        "outcome": "progress", "ref": "#900", "note": "still going" } }))
        .unwrap();
    assert_eq!(r["isError"], false, "an unresolvable progress report must not fail: {r}");
    // …and the answer says so rather than naming a note that does not exist.
    let answer = r["content"][0]["text"].as_str().unwrap();
    assert!(
        answer.contains("No board task on this group's board matched"),
        "the answer must not claim a board note it did not write: {answer}"
    );
    assert_eq!(
        audited_reports(&reg, &g),
        vec!["progress".to_string()],
        "the audit row is the floor and is written regardless"
    );
    assert!(
        task_notes(&reg, &g, &other.id).is_empty(),
        "a report that resolves nothing must not land on somebody elses row"
    );
    assert_eq!(reg.tasks(&g).len(), 1, "…and must not invent a row of its own");
    assert!(
        !delivered_texts(&reg, &g).iter().any(|t| t.contains("reports progress")),
        "it is still not delivered"
    );
}

/// **#1958.** The `ref` fallback: a row the orchestrator never bound a session
/// to is still reachable through the PR the delegate names.
///
/// The session is tried FIRST and this test does not contradict that — the
/// worker's session is on no row at all here, which is what makes the fallback
/// the only thing that can resolve it.
#[test]
fn a_progress_reports_ref_resolves_a_board_row_when_no_session_is_bound() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap().id;
    let orch = reg.spawn_agent(&g, Role::Orchestrator, "orch", "", false, None).unwrap();
    pause_with_pane(&reg, &g, &orch.id, 6252);
    let w = reg.spawn_agent(&g, Role::Worker, "w", "ship it", false, None).unwrap();
    let cw = reg.resolve_token(&w.token).unwrap();
    let t = reg
        .upsert_task(&g, "orch-1", None, patch(Some("Ship the thing"), None, None))
        .unwrap();
    reg.upsert_task(
        &g,
        "orch-1",
        Some(&t.id),
        TaskPatch { pr: Some("https://github.com/o/r/pull/900".into()), ..Default::default() },
    )
    .unwrap();

    let _ = dispatch(&reg, &cw, "tools/call", &json!({ "name": "report", "arguments": {
        "outcome": "progress", "ref": "#900", "note": "pushed the fix" } }));
    let notes = task_notes(&reg, &g, &t.id);
    assert_eq!(notes.len(), 1, "the ref must resolve the row it names: {notes:?}");
    assert!(notes[0].contains("pushed the fix"), "{}", notes[0]);
}

/// **#1958.** `done` and `blocked` are untouched — both still typed into the
/// orchestrator's pane, because both need it to act.
///
/// **Driven from `report::STATUSES`, not from a literal list** (#1966 rev-final
/// N1). The earlier revision walked `["done", "blocked", "progress"]` written out
/// by hand while its own doc claimed to walk "the whole vocabulary" — so a fourth
/// status was invisible to it twice over: never driven, and never counted. Reading
/// the real constant makes the claim true of what the test DOES: every member of
/// the vocabulary is put through the live dispatch, and the delivered set is
/// asserted as a SET rather than a count, so a fourth status that delivered would
/// show up here as an unexpected member.
///
/// What it still cannot do is decide whether that fourth status SHOULD deliver —
/// the engine's catch-all says yes, deliberately, and the vocabulary pin in
/// `report::tests::every_status_is_classified` is the one thing that makes adding
/// a member a deliberate edit.
#[test]
fn done_and_blocked_still_reach_the_orchestrator_and_only_progress_does_not() {
    let (reg, _d, g, w, cw, _tid) = report_routing_setup();
    for status in report::STATUSES {
        let _ = dispatch(&reg, &cw, "tools/call", &json!({ "name": "report", "arguments": {
            "status": status, "summary": format!("status word {status}") } }));
    }
    let delivered = delivered_texts(&reg, &g);
    let prefix = format!("[orrerix] {} reports ", w.id);
    // The STATUS WORD of each line this worker got delivered, in vocabulary order
    // — a set, so an unexpected member reads as itself rather than as a count.
    let mut reached: Vec<&str> = report::STATUSES
        .iter()
        .copied()
        .filter(|s| delivered.iter().any(|t| t.starts_with(&format!("{prefix}{s}:"))))
        .collect();
    reached.sort_unstable();
    let mut want: Vec<&str> =
        report::STATUSES.iter().copied().filter(|s| *s != "progress").collect();
    want.sort_unstable();
    assert_eq!(reached, want, "delivered lines were: {delivered:#?}");
    // Non-vacuity: the walk really drove the dispatch. Without this, a probe that
    // delivered nothing at all would satisfy the set assertion for `progress` and
    // fail only by luck on the others.
    assert_eq!(
        delivered.iter().filter(|t| t.starts_with(&prefix)).count(),
        want.len(),
        "one delivered line per action-needing status, no more: {delivered:#?}"
    );
}

/// **#1966 rev-final N2.** "I could not read the board" is never reported as
/// "there was no matching row".
///
/// `tasks()` maps any read or parse failure to an empty `Vec`, so before this the
/// delegate was told the board's *contents* on a read that had simply failed —
/// the same defect one layer down that this change fixes by not saying "reported
/// to orchestrator". The note is still correctly declined and the audit row is
/// still written; what changes is the sentence.
#[test]
fn an_unreadable_board_is_not_reported_as_no_matching_task() {
    let (reg, _d, g, _w, cw, tid) = report_routing_setup();

    // The control: the row IS there and resolvable, so the difference below is
    // the read failing and nothing else.
    let r = dispatch(&reg, &cw, "tools/call", &json!({ "name": "report", "arguments": {
        "outcome": "progress", "ref": "#900", "note": "readable board" } })).unwrap();
    assert!(
        r["content"][0]["text"].as_str().unwrap().contains("noted on your board task"),
        "control: a readable board must resolve this row: {r}"
    );
    assert_eq!(task_notes(&reg, &g, &tid).len(), 1);

    // Now corrupt it. Same row, same session, same ref.
    let path = reg.state_root().join(g.as_str()).join("tasks.json");
    std::fs::write(&path, b"{ this is not json").unwrap();

    let r = dispatch(&reg, &cw, "tools/call", &json!({ "name": "report", "arguments": {
        "outcome": "progress", "ref": "#900", "note": "unreadable board" } })).unwrap();
    assert_eq!(r["isError"], false, "an unreadable board must not fail the report: {r}");
    let answer = r["content"][0]["text"].as_str().unwrap();
    assert!(
        answer.contains("could not be read"),
        "the answer must say the board was unreadable: {answer}"
    );
    assert!(
        !answer.contains("matched your session or ref"),
        "…and must NOT claim the board had no matching row: {answer}"
    );
    // The audit row is still the floor, and it carries the full text.
    assert_eq!(
        audited_reports(&reg, &g),
        vec!["progress".to_string(), "progress".to_string()],
        "both reports are audited regardless"
    );
}

/// **#1966 rev-final round 2, N1.** A row that RESOLVED and then failed to be
/// written is never answered as "no board task matched".
///
/// The read-side collapse was round 1's N2; this is the same defect one layer
/// over. `write_tasks` propagates `create_dir_all` and `atomic_write` errors —
/// the disk-full case #133 filed — and the first cut mapped every `Err` from
/// `upsert_task` to `NoRow`, which is a claim about the board's CONTENTS made on
/// a write that failed.
///
/// **The failure is induced for real, not faked**: the group directory is made
/// non-writable, so `atomic_write`'s `File::create` of its temp sibling fails
/// while `read_to_string` of the board still succeeds (reading a file needs the
/// directory's execute bit, not its write bit). That is what makes the read
/// resolve a row and the write fail on the same call — the interleaving a single
/// thread cannot otherwise produce.
///
/// **Unix only, and the reason is the platform not the property.** Windows
/// ignores the read-only attribute on a directory for the owner, so there is no
/// portable in-process lever; the arm under test is platform-independent code
/// and ubuntu + macos exercise it on every run. The control below is what stops
/// this passing for the wrong reason if the lever ever stops working.
#[cfg(unix)]
#[test]
fn a_board_write_that_fails_is_not_reported_as_no_matching_task() {
    use std::os::unix::fs::PermissionsExt;
    let (reg, _d, g, _w, cw, tid) = report_routing_setup();

    // Control: the row resolves and the note lands while the directory is
    // writable, so the only thing that changes below is the write failing.
    let r = dispatch(&reg, &cw, "tools/call", &json!({ "name": "report", "arguments": {
        "outcome": "progress", "ref": "#900", "note": "writable" } })).unwrap();
    assert!(
        r["content"][0]["text"].as_str().unwrap().contains("noted on your board task"),
        "control: the row must resolve and the note land: {r}"
    );
    assert_eq!(task_notes(&reg, &g, &tid).len(), 1);

    let dir = reg.state_root().join(g.as_str());
    let before = std::fs::metadata(&dir).unwrap().permissions();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let out = dispatch(&reg, &cw, "tools/call", &json!({ "name": "report", "arguments": {
        "outcome": "progress", "ref": "#900", "note": "unwritable" } }));
    // Restore BEFORE asserting, so a failure here does not leave the TempDir
    // undeletable and turn one red into a cleanup cascade.
    std::fs::set_permissions(&dir, before).unwrap();

    let r = out.unwrap();
    assert_eq!(r["isError"], false, "a failed board write must not fail the report: {r}");
    let answer = r["content"][0]["text"].as_str().unwrap();
    assert!(
        !answer.contains("matched your session or ref"),
        "a write failure must NOT be answered as 'no board task matched': {answer}"
    );
    assert!(
        answer.contains("could not be written"),
        "…it must say the note could not be written: {answer}"
    );
    // The lever really did stop the write: still one note, not two.
    assert_eq!(
        task_notes(&reg, &g, &tid).len(),
        1,
        "the second note must NOT have landed — otherwise this test passed with the write \
         succeeding, which measures nothing"
    );
    // And the audit row is still the floor, which is what makes the loss survivable.
    assert_eq!(
        audited_reports(&reg, &g),
        vec!["progress".to_string(), "progress".to_string()],
        "both reports are audited regardless"
    );
}

/// **#1966 rev-final premortem 1.** The `ref` fallback skips `done` rows.
///
/// Nothing clears `pr` when a task completes, so a long-lived board keeps finished
/// rows carrying the same PR as the live follow-up work. A first-match-wins scan
/// over raw board order would pile every note onto whichever sorts first — and the
/// finished row sorts first, because it was created first.
///
/// The fixture is built so the two operands COLLIDE: both rows carry `#900`, the
/// `done` one is EARLIER in board order, and neither carries the delegate's
/// session (so the `ref` fallback is the only thing that can resolve either).
#[test]
fn the_ref_fallback_skips_a_done_row_carrying_the_same_pr() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap().id;
    let orch = reg.spawn_agent(&g, Role::Orchestrator, "orch", "", false, None).unwrap();
    pause_with_pane(&reg, &g, &orch.id, 6253);
    let w = reg.spawn_agent(&g, Role::Worker, "w", "follow-up", false, None).unwrap();
    let cw = reg.resolve_token(&w.token).unwrap();

    let finished = reg
        .upsert_task(&g, "orch-1", None, patch(Some("First pass at #900"), None, None))
        .unwrap();
    reg.upsert_task(&g, "orch-1", Some(&finished.id),
        TaskPatch { pr: Some("#900".into()), status: Some("done".into()), ..Default::default() })
        .unwrap();
    let live = reg
        .upsert_task(&g, "orch-1", None, patch(Some("Follow-up on #900"), None, None))
        .unwrap();
    reg.upsert_task(&g, "orch-1", Some(&live.id),
        TaskPatch { pr: Some("#900".into()), ..Default::default() })
        .unwrap();
    // The collision is real: the done row really is first in board order.
    let order: Vec<String> = reg.tasks(&g).into_iter().map(|t| t.id).collect();
    assert_eq!(order, vec![finished.id.clone(), live.id.clone()], "fixture: done row sorts first");

    let _ = dispatch(&reg, &cw, "tools/call", &json!({ "name": "report", "arguments": {
        "outcome": "progress", "ref": "#900", "note": "on the follow-up" } }));

    assert_eq!(task_notes(&reg, &g, &live.id).len(), 1, "the note belongs on the LIVE row");
    assert!(
        task_notes(&reg, &g, &finished.id).is_empty(),
        "a finished row is never the row a delegate is reporting progress on"
    );
}

#[test]
fn closing_a_completed_planner_is_idempotent() {
    // #203 (review finding 4): two concurrent `done` reports must not
    // double-notify. `mark_dead` is the atomic claim inside
    // `close_completed_planner`, so only the caller that wins the live→dead
    // transition delivers the exit notice; a second (racing) call is a no-op.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let planner = reg.spawn_agent(&g.id, Role::Planner, "plan", "task", false, None).unwrap();
    pause_with_pane(&reg, &g.id, &orch.id, 6202); // queue+audit deliveries so notices are countable

    reg.close_completed_planner(&planner.id);
    reg.close_completed_planner(&planner.id); // the racing duplicate

    // Counted on the AUDIT row since #3040 N2 — the notice is demoted, not
    // deleted, so "exactly once" is still exactly the property #203 review
    // finding 4 asked for; only the surface it is counted on moved.
    let rows = audit_entries(&reg, &g.id, "agent-exit-notice");
    let closes: Vec<&Value> = rows
        .iter()
        .filter(|e| e["detail"]["notice"].as_str().is_some_and(|n| n.contains("posted its plan and exited")))
        .collect();
    assert_eq!(closes.len(), 1,
        "a completed planner must be closed and recorded exactly once: {rows:?}");
    assert_eq!(
        suppressed_notices(&reg, &g.id, "posted its plan and exited"),
        0,
        "…and the racing duplicate must not resurrect the pane notice either"
    );
}

#[test]
fn advisor_hinted_planner_auto_closes_on_report_done() {
    // #250/#324 slice D: the advisor is planner-kind (#203's "one plan → one
    // report → exit" contract), and the AUTO-CLOSE path never reads role_hint —
    // so `close_completed_planner`'s role gate (keyed on `Role::Planner` alone,
    // never on block id or role_hint) already covers an advisor-hinted block
    // for free. (Scoped to this path deliberately: since #891 S4 the idle
    // reaper DOES read the hint, so "lifecycle never reads role_hint" is no
    // longer true of lifecycle in general — see
    // `a_liaison_is_never_taken_by_the_idle_reaper`.) This pins that claim directly: no idle pane, no standing
    // consult process, exactly the #203 precedent the plan cites.
    let (reg, _d) = test_registry();
    let mut g_rails = rails();
    g_rails.blocks.push(workflow::Block {
        id: "advisor".into(),
        name: "advisor".into(),
        kind: Role::Planner,
        cli: String::new(),
        model: String::new(),
        prompt: None,
        profile: None,
        allow: vec![],
        role_hint: Some("advisor".into()),
        effort: String::new(),
        context: String::new(),
        remote: None,
        driver: None,
        cache_ttl_minutes: None,
    });
    let g = reg.create_group("C:/tmp/repo", g_rails).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let advisor = reg
        .spawn_agent_ex(&g.id, Role::Planner, Some("advisor".into()), "adv",
                        "is the worker stuck on X or Y?", false, None, None, None, None, None)
        .unwrap();
    pause_with_pane(&reg, &g.id, &orch.id, 6203);
    let ca = reg.resolve_token(&advisor.token).unwrap();

    let r = dispatch(&reg, &ca, "tools/call",
        &json!({ "name": "report", "arguments": { "status": "done", "summary": "go with X" } })).unwrap();
    assert_eq!(r["isError"], false, "the advisor's done report must succeed");

    let dead = reg.list_agents(&g.id).as_array().unwrap().iter()
        .any(|a| a["id"] == json!(advisor.id) && a["status"] == json!("dead"));
    assert!(dead, "an advisor-hinted block's done report must auto-close its pane, same as any planner (#203)");
}

#[test]
fn spawn_cap_rejection_lists_the_delegate_roster() {
    // #203: when spawn is refused at the delegate cap, the guardrail message
    // must name who holds the slots (id, role, idle vs working) so the
    // orchestrator can see which agent to reclaim — an idle planner squatting a
    // slot is the whole reason the cap was hit with no visible cause.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap(); // cap 2
    let worker = reg
        .spawn_agent(&g.id, Role::Worker, "w", "build the thing", false, None)
        .unwrap(); // has a task → working
    let planner = reg.spawn_agent(&g.id, Role::Planner, "plan", "", false, None).unwrap(); // no task → idle
    // Two live delegates == cap: the next spawn is refused with the roster.
    let err = reg.spawn_agent(&g.id, Role::Worker, "w2", "t", false, None).unwrap_err();
    assert!(err.contains("Live delegates:"), "rejection must list the roster, got: {err}");
    assert!(
        err.contains(&format!("{} (worker, working)", worker.id)),
        "a tasked worker must show as working, got: {err}"
    );
    assert!(
        err.contains(&format!("{} (planner, idle)", planner.id)),
        "an idle planner must show as idle — the obvious slot to reclaim, got: {err}"
    );
}

#[test]
fn every_tool_call_is_audited() {
    let (reg, _d, co, _cw) = setup_mcp();
    dispatch(&reg, &co, "tools/call", &json!({ "name": "list_agents", "arguments": {} })).unwrap();
    let log = fs::read_to_string(reg.state_root().join(co.group.as_str()).join("audit.jsonl")).unwrap();
    let lines: Vec<Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let call = lines
        .iter()
        .find(|e| e["action"] == "tool-call" && e["detail"]["tool"] == "list_agents")
        .expect("tool-call audit entry");
    assert_eq!(call["actor"], co.agent_id.as_str());
    assert!(lines.iter().any(|e| e["action"] == "tool-result" && e["detail"]["tool"] == "list_agents"));
}

// ---------- pane naming & rename precedence (#95r) ----------

#[test]
fn default_pane_name_derives_from_minted_id() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    // The orchestrator takes seq 1 (orch-1); the worker mints the next seq.
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    // No meaningful name → the title is derived from the minted id, so it
    // carries the SAME seq the "W <seq>" badge (#75) and the roster id show,
    // never the old per-launch "worker N" counter that drifted from the seq
    // (the "worker 1" pane wearing a "W 2" badge in the #95 screenshot).
    let w = reg.spawn_agent(&g.id, Role::Worker, "", "", false, None).unwrap();
    let seq = w.id.rsplit('-').next().unwrap();
    assert_eq!(w.id, format!("w-{seq}"));
    assert_eq!(
        w.name,
        format!("worker {seq}"),
        "default name must carry the id/badge seq, got id={} name={}",
        w.id, w.name
    );
}

#[test]
fn explicit_spawn_name_is_kept_verbatim() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    // A meaningful name the orchestrator chose is not derived away.
    let w = reg.spawn_agent(&g.id, Role::Worker, "gitwatch fix", "t", false, None).unwrap();
    assert_eq!(w.name, "gitwatch fix");
}

#[test]
fn rename_agent_updates_roster_and_audits() {
    let (reg, _d, co, cw) = setup_mcp();
    let ok = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "rename_agent", "arguments": { "agent_id": cw.agent_id, "name": "w-2: gitwatch fix" } }))
        .unwrap();
    assert_eq!(ok["isError"], false, "orchestrator rename must succeed: {ok}");
    assert!(ok["content"][0]["text"].as_str().unwrap().contains("gitwatch fix"));
    // The durable roster reflects the new name (title ↔ roster agree, #95r).
    let roster = reg.list_agents(&co.group);
    let row = roster.as_array().unwrap().iter().find(|a| a["id"] == cw.agent_id.as_str()).unwrap();
    assert_eq!(row["name"], "w-2: gitwatch fix", "roster name must follow the rename");
    // And it is audited.
    let log = fs::read_to_string(reg.state_root().join(co.group.as_str()).join("audit.jsonl")).unwrap();
    assert!(log.contains("agent-rename"), "rename must be audited");
}

/// A worker cannot rename a pane — and since #2519 the refusal names BOTH
/// classes that can, because "orchestrator-only" stopped being true of it.
///
/// The test is renamed with the property rather than repinned to whatever the
/// new message happens to share with the old: its old name asserted the claim
/// in the one place an assertion cannot reach it. `rename_agent` moved from
/// `require_orchestrator` to `require_spawner`, so a worker told this tool is
/// orchestrator-only would be learning something untrue about the system it is
/// in — the same correction `workers_cannot_use_privileged_tools_even_if_they_try`
/// makes for the other five fleet tools, and the reason both had to move
/// together is that they are one gate.
#[test]
fn rename_agent_is_for_a_spawner_only() {
    let (reg, _d, _co, cw) = setup_mcp();
    let denied = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "rename_agent", "arguments": { "agent_id": cw.agent_id, "name": "x" } }))
        .unwrap();
    assert_eq!(denied["isError"], true, "workers must not rename");
    let text = denied["content"][0]["text"].as_str().unwrap();
    // Flattened, because the gate's message is one paragraph produced by a
    // `\`-continued Rust literal: pinning the wrap position would be pinning
    // the source layout rather than the sentence.
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("for an orchestrator, or a lead pane"),
        "the refusal must name the classes that DO hold it, not a class that no longer \
         exclusively does: {flat}"
    );
}

#[test]
fn rename_agent_cannot_target_another_group() {
    let (reg, _d, co, _cw) = setup_mcp();
    let g2 = reg.create_group("C:/tmp/other-repo", rails()).unwrap();
    let foreign = reg.spawn_agent(&g2.id, Role::Worker, "fw", "t", false, None).unwrap();
    let r = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "rename_agent", "arguments": { "agent_id": foreign.id, "name": "x" } }))
        .unwrap();
    assert_eq!(r["isError"], true);
    // Cross-group is indistinguishable from a nonexistent agent (no id leak).
    assert!(r["content"][0]["text"].as_str().unwrap().contains("unknown agent"));
    assert_eq!(reg.agent(&foreign.id).unwrap().name, "fw", "foreign agent keeps its name");
}

#[test]
fn rename_agent_rejects_dead_target() {
    let (reg, _d, co, cw) = setup_mcp();
    reg.mark_dead(&cw.agent_id, None);
    let r = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "rename_agent", "arguments": { "agent_id": cw.agent_id, "name": "x" } }))
        .unwrap();
    assert_eq!(r["isError"], true, "a dead agent cannot be renamed");
    assert!(r["content"][0]["text"].as_str().unwrap().contains("not alive"));
}

#[test]
fn rename_agent_rejects_empty_name() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "", "t", false, None).unwrap();
    assert!(reg.rename_agent(&w.id, "   ", NameSource::Orchestrator).is_err());
}

#[test]
fn rename_precedence_human_beats_orchestrator_beats_default() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "", "t", false, None).unwrap();
    // Starts at the id-derived default.
    let seq = w.id.rsplit('-').next().unwrap().to_string();
    assert_eq!(w.name, format!("worker {seq}"));

    // Orchestrator outranks the default → applies.
    reg.rename_agent(&w.id, "w: parser", NameSource::Orchestrator).unwrap();
    assert_eq!(reg.agent(&w.id).unwrap().name, "w: parser");

    // Human outranks the orchestrator → applies and locks.
    reg.rename_agent(&w.id, "my parser work", NameSource::Human).unwrap();
    assert_eq!(reg.agent(&w.id).unwrap().name, "my parser work");

    // A later orchestrator rename must NOT override the human's title.
    let err = reg.rename_agent(&w.id, "w: something else", NameSource::Orchestrator).unwrap_err();
    assert!(err.contains("human"), "rejection must explain the precedence: {err}");
    assert_eq!(
        reg.agent(&w.id).unwrap().name,
        "my parser work",
        "human rename must survive a later orchestrator rename"
    );

    // The human can still re-rename their own pane (human ≥ human).
    reg.rename_agent(&w.id, "parser v2", NameSource::Human).unwrap();
    assert_eq!(reg.agent(&w.id).unwrap().name, "parser v2");
}

#[test]
fn rename_strips_control_characters() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "", "t", false, None).unwrap();
    // A pasted name can't smuggle newlines/escape codes into the title/roster.
    let applied = reg.rename_agent(&w.id, "w-2:\tgit\nfix\u{1b}[31m", NameSource::Orchestrator).unwrap();
    assert_eq!(applied, "w-2:gitfix[31m");
    assert!(!applied.chars().any(|c| c.is_control()));
    // An all-control name is rejected, not silently applied as empty.
    assert!(reg.rename_agent(&w.id, "\u{1b}\n\t", NameSource::Orchestrator).is_err());
}

#[test]
fn roster_persists_the_name_source_tier() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "", "t", false, None).unwrap();
    // A human rename must persist its tier, not just the text, so a later
    // rejoin can restore the "human wins" guarantee (#95r).
    reg.rename_agent(&w.id, "my parser work", NameSource::Human).unwrap();
    let roster: Value =
        serde_json::from_str(&fs::read_to_string(reg.state_root().join(g.id.as_str()).join("agents.json")).unwrap())
            .unwrap();
    let row = roster.as_array().unwrap().iter().find(|r| r["id"] == w.id.as_str()).unwrap();
    assert_eq!(row["name"], "my parser work");
    assert_eq!(row["name_source"], "human", "the tier must be durable, got: {row}");
}

#[test]
fn rejoined_session_restores_the_human_name_tier() {
    use loomux_lib::orchestration::resume_recorded_session;
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    // A REAL directory (#412) — see the comment in
    // `worker_session_rejoin_requires_live_group_then_reuses_session`.
    let repo = tempfile::tempdir().unwrap();
    let repo_path = repo.path().to_string_lossy().into_owned();
    let reg = Arc::new(relaunch_registry(dir.path()));
    reg.set_port(45999);
    let g = reg.create_group(&repo_path, rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "", "t", false, None).unwrap();
    let sid = w.session_id.clone().unwrap();
    // Human renames the pane, then it dies (pre-"restart").
    reg.rename_agent(&w.id, "my parser work", NameSource::Human).unwrap();
    reg.mark_dead(&w.id, Some(0));

    // Rejoin (background spawn) must come back at the human tier, not demoted
    // to orchestrator — otherwise the "human wins" guarantee dies on restart.
    assert!(resume_recorded_session(&reg, &sid, None, false).unwrap().is_none());
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let rejoined_id = loop {
        let hit = reg
            .list_agents(&g.id)
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["session"] == sid.as_str() && a["status"] == "running")
            .map(|a| a["id"].as_str().unwrap().to_string());
        if let Some(id) = hit {
            break id;
        }
        assert!(std::time::Instant::now() < deadline, "rejoin did not complete");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_ne!(rejoined_id, w.id, "rejoin mints a fresh id");
    assert_eq!(reg.agent(&rejoined_id).unwrap().name, "my parser work", "name restored");

    // The orchestrator cannot clobber the restored human name.
    let co = reg.resolve_token(&orch.token).unwrap();
    let r = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "rename_agent", "arguments": { "agent_id": rejoined_id, "name": "w: something else" } }))
        .unwrap();
    assert_eq!(r["isError"], true, "orchestrator must not override a restored human rename");
    assert!(r["content"][0]["text"].as_str().unwrap().contains("human"));
    assert_eq!(reg.agent(&rejoined_id).unwrap().name, "my parser work");
}
