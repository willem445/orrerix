//! Per-task sessions and resume, merge-gate actions, prototype status, review-round regressions, durable roster restore and group rejoin.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ---------- per-task sessions & resume ----------

#[test]
fn claude_agents_get_preassigned_resumable_sessions() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let sid = w.session_id.expect("claude agents must get a session id at spawn");
    // Valid UUID shape (claude --session-id requires it).
    assert_eq!(sid.len(), 36);
    assert_eq!(sid.split('-').map(str::len).collect::<Vec<_>>(), [8, 4, 4, 4, 12]);
    // The roster exposes session + cwd so the orchestrator can record them.
    let roster = reg.list_agents(&g.id).to_string();
    assert!(roster.contains(&sid) && roster.contains("cwd"));
    // The launch command pins the id.
    let cfg = Path::new("C:/x/cfg.json");
    let gdir = Path::new("C:/x/g");
    let cmd = reg.build_agent_command("claude", "sonnet", false, cfg, None, gdir, Path::new("C:/repo"), Some(&sid), false, Containment::None, &PersonaInject::default());
    assert!(cmd.contains(&format!("--session-id {sid}")));
    // Resume uses --resume instead.
    let cmd = reg.build_agent_command("claude", "sonnet", false, cfg, None, gdir, Path::new("C:/repo"), Some(&sid), true, Containment::None, &PersonaInject::default());
    assert!(cmd.contains(&format!("--resume {sid}")) && !cmd.contains("--session-id"));
}

#[test]
fn resume_spawn_requires_valid_session_and_existing_cwd() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let bad_session = reg.spawn_agent_ex(
        &g.id, Role::Worker, None, "w", "follow-up", false, None, None,
        Some("; rm -rf /".into()), None, None,
    );
    assert!(bad_session.is_err(), "shell-metachar session ids must be rejected");
    let bad_cwd = reg.spawn_agent_ex(
        &g.id, Role::Worker, None, "w", "follow-up", false, None, None,
        Some("abc-123".into()), Some("C:/definitely/not/a/dir".into()), None,
    );
    assert!(bad_cwd.unwrap_err().contains("cwd"), "resume cwd must exist");
    // Valid resume records the reused session on the agent.
    let dir = tempfile::tempdir().unwrap();
    let ok = reg
        .spawn_agent_ex(
            &g.id, Role::Worker, None, "w", "follow-up", false, None, None,
            Some("abc-123".into()), Some(dir.path().to_string_lossy().into_owned()), None,
        )
        .unwrap();
    assert_eq!(ok.session_id.as_deref(), Some("abc-123"));
    assert_eq!(ok.cwd, dir.path().to_string_lossy());
}

#[test]
fn task_board_tracks_sessions_for_followups() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("Add retries"), None, None)).unwrap();
    let mut p = patch(None, Some("in-progress"), None);
    p.session = Some("11112222-3333-4444-8555-666677778888".into());
    p.assignee = Some("w-1".into());
    let t = reg.upsert_task(&g.id, "orch-1", Some(&t.id), p).unwrap();
    assert_eq!(t.session.as_deref(), Some("11112222-3333-4444-8555-666677778888"));
    // Survives the round-trip through disk.
    let stored = &reg.tasks(&g.id)[0];
    assert_eq!(stored.session, t.session);
    assert_eq!(stored.assignee.as_deref(), Some("w-1"));
}

// ---------- merge-gate actions (#9) ----------

#[test]
fn remote_web_base_normalizes_every_git_url_shape() {
    // scp-like, https (with/without .git), ssh with a port, trailing slash.
    let cases = [
        ("git@github.com:willem445/orrerix.git", "https://github.com/willem445/orrerix"),
        ("https://github.com/willem445/orrerix.git", "https://github.com/willem445/orrerix"),
        ("https://github.com/willem445/orrerix", "https://github.com/willem445/orrerix"),
        ("ssh://git@github.com:22/willem445/orrerix.git", "https://github.com/willem445/orrerix"),
        ("https://token@github.com/o/r/", "https://github.com/o/r"),
        // Self-hosted host survives (GitHub path scheme is assumed downstream).
        ("git@git.example.com:team/app.git", "https://git.example.com/team/app"),
    ];
    for (url, want) in cases {
        assert_eq!(normalize_remote_web_base(url).as_deref(), Some(want), "for {url}");
    }
    // Junk that can't be turned into a link.
    for bad in ["", "not-a-url", "https://", "git@github.com", "file:///tmp/x"] {
        assert!(normalize_remote_web_base(bad).is_none(), "{bad:?} must not resolve");
    }
}

#[test]
fn resolve_ref_url_handles_numbers_and_passthrough() {
    let base = Some("https://github.com/o/r");
    // Bare number and #-prefixed both resolve; issue vs pr picks the segment.
    assert_eq!(resolve_ref_url(base, "issue", "#9").as_deref(), Some("https://github.com/o/r/issues/9"));
    assert_eq!(resolve_ref_url(base, "pr", "42").as_deref(), Some("https://github.com/o/r/pull/42"));
    // A `GH-12`-style prefix resolves to its digit run (comment ↔ behavior).
    assert_eq!(resolve_ref_url(base, "issue", "GH-12").as_deref(), Some("https://github.com/o/r/issues/12"));
    // A full URL is used verbatim — even with no remote base available.
    let url = "https://github.com/o/r/pull/7";
    assert_eq!(resolve_ref_url(None, "pr", url).as_deref(), Some(url));
    // A bare number with no remote can't be resolved.
    assert!(resolve_ref_url(None, "issue", "9").is_none());
    // Non-numeric junk resolves to nothing.
    assert!(resolve_ref_url(base, "issue", "later").is_none());
}

#[test]
fn approve_marks_done_and_records_signoff() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("Ship the parser"), None, None)).unwrap();
    // Move it to the merge gate first (as the orchestrator would).
    let mut p = patch(None, Some("pr"), None);
    p.pr = Some("#12".into());
    reg.upsert_task(&g.id, "orch-1", Some(&t.id), p).unwrap();
    // Approving is the human's sign-off: status → done, note recorded, actor human.
    let done = reg.approve_task(&g.id, &t.id, None).unwrap();
    assert_eq!(done.status, "done");
    let note = done.notes.last().unwrap();
    assert_eq!(note.author, "human");
    assert!(note.text.contains("Approved"), "sign-off must be auditable on the board");
}

#[test]
fn request_changes_records_findings_but_not_done() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("Ship the parser"), Some("pr"), None)).unwrap();
    // Empty findings are rejected — the notice would be useless.
    assert!(reg.request_changes(&g.id, &t.id, "   ").is_err());
    let after = reg.request_changes(&g.id, &t.id, "retries still leak a handle").unwrap();
    // Status stays at the gate (orchestrator re-dispatches); findings recorded.
    assert_eq!(after.status, "pr", "request-changes must not silently complete the item");
    assert!(after.notes.last().unwrap().text.contains("retries still leak a handle"));
    // Unknown task id is an error, not a silent no-op.
    assert!(reg.request_changes(&g.id, "t-999", "x").is_err());
}

#[test]
fn merge_gate_actions_are_guarded_to_gate_statuses() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    // A queued item is not at the merge gate — both actions must refuse, and
    // refuse without mutating (status unchanged, no note added).
    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("Ship it"), None, None)).unwrap();
    assert!(reg.approve_task(&g.id, &t.id, None).is_err(), "cannot approve a queued item");
    assert!(reg.request_changes(&g.id, &t.id, "nope").is_err(), "cannot request changes off-gate");
    let stored = &reg.tasks(&g.id)[0];
    assert_eq!(stored.status, "queued", "a refused action must not change status");
    assert!(stored.notes.is_empty(), "a refused action must not leave a note");
    // Both gate statuses are allowed.
    for gate in ["pr", "human-testing"] {
        reg.upsert_task(&g.id, "orch-1", Some(&t.id), patch(None, Some(gate), None)).unwrap();
        assert!(reg.request_changes(&g.id, &t.id, "one more thing").is_ok(), "{gate} is a gate status");
    }
    // And once approved (→ done) it's off the gate again.
    reg.upsert_task(&g.id, "orch-1", Some(&t.id), patch(None, Some("pr"), None)).unwrap();
    reg.approve_task(&g.id, &t.id, None).unwrap();
    assert!(reg.approve_task(&g.id, &t.id, None).is_err(), "a done item is past the gate");
}

#[test]
fn start_records_note_and_leaves_status_queued() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("Ship the parser"), None, None)).unwrap();
    // Starting is the human's nudge: a human-attributed note is recorded, but
    // the status deliberately stays queued — the orchestrator flips it to
    // in-progress when it actually assigns a worker.
    let after = reg.start_task(&g.id, &t.id).unwrap();
    assert_eq!(after.status, "queued", "start must not flip the status itself");
    let note = after.notes.last().unwrap();
    assert_eq!(note.author, "human");
    assert!(note.text.contains("Started"), "the nudge must be auditable on the board");
}

#[test]
fn start_is_guarded_to_queued_items() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("Ship it"), None, None)).unwrap();
    // An unknown id is an error, not a silent no-op.
    assert!(reg.start_task(&g.id, "t-999").is_err());
    // Every non-queued status must refuse, and refuse without mutating.
    for status in ["in-progress", "review", "pr", "human-testing", "done", "blocked"] {
        reg.upsert_task(&g.id, "orch-1", Some(&t.id), patch(None, Some(status), None)).unwrap();
        let before = reg.tasks(&g.id)[0].notes.len();
        assert!(reg.start_task(&g.id, &t.id).is_err(), "cannot start a {status} item");
        assert_eq!(reg.tasks(&g.id)[0].notes.len(), before, "a refused start must not leave a note");
    }
    // Back to queued, it's allowed again.
    reg.upsert_task(&g.id, "orch-1", Some(&t.id), patch(None, Some("queued"), None)).unwrap();
    assert!(reg.start_task(&g.id, &t.id).is_ok(), "queued is startable");
}

#[test]
fn start_is_rejected_up_front_when_the_group_is_paused() {
    // Start is a HUMAN clicking "begin work now" on a group they have paused,
    // and the two readings of that click ("now" vs "whenever I resume") are far
    // enough apart to be worth a synchronous error rather than a silent guess.
    // So Start rejects up front, like the steering strip (#43): a clear error,
    // NO note appended, and — the point — it never reaches delivery at all.
    //
    // #569 rewrote the ARGUMENT, not the behavior: pre-#569 the nudge would have
    // been destroyed by the pause, which is why the guard was added; it would
    // now be queued and delivered on resume. The assertion below moved with it,
    // from "no suppression audit" to "nothing was queued for the pane".
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    // An orchestrator is present AND has a pane: if the guard were missing,
    // start would reach delivery and admit an entry to that pane — so asserting
    // the pane's queue is empty proves the guard fires *before* delivery, not
    // that there was simply no target to deliver to.
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("Ship the parser"), None, None)).unwrap();

    pause_with_pane(&reg, &g.id, &orch.id, 6100);
    let err = reg.start_task(&g.id, &t.id).unwrap_err();
    assert!(err.contains("paused"), "paused rejection must say so: {err}");

    assert!(reg.tasks(&g.id)[0].notes.is_empty(), "a rejected start must not leave a note");
    assert!(
        reg.queue_snapshot(6100).is_empty(),
        "start must reject before delivery — nothing may be queued for the orchestrator"
    );
    assert!(
        delivered_texts(&reg, &g.id).is_empty(),
        "start must reject before delivery — no prompt audited"
    );

    // Resuming lets it through again.
    reg.resume_group(&g.id).unwrap();
    let after = reg.start_task(&g.id, &t.id).unwrap();
    assert_eq!(after.status, "queued");
    assert!(after.notes.last().unwrap().text.contains("Started"), "resumed start records the nudge");
}

// ---------- #147: prototype status + proceed workflow ----------

/// Count the notices the orchestrator received whose text contains `needle`.
/// The group is paused in these tests, so #569 QUEUES each delivery instead of
/// pasting it — the text is audited under the ordinary `prompt` action either
/// way, which is what makes a paused group a usable probe with no real PTY.
pub(crate) fn suppressed_notices(reg: &OrchRegistry, group: &GroupId, needle: &str) -> usize {
    delivered_texts(reg, group).into_iter().filter(|t| t.contains(needle)).count()
}

#[test]
fn prototype_is_a_valid_status() {
    // The board must accept `prototype` on a write — it's a first-class status,
    // not a free-text label. (The frontend picker mirrors TASK_STATUSES.)
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("Demo the thing"), None, None)).unwrap();
    let after = reg.upsert_task(&g.id, "orch-1", Some(&t.id), patch(None, Some("prototype"), None)).unwrap();
    assert_eq!(after.status, "prototype");
}

#[test]
fn proceed_flips_to_in_progress_audits_and_sends_one_notice() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    // An orchestrator must exist for the notice to have a target; pause the group
    // so delivery is queued-and-audited rather than pasted (test mode has no pane
    // to type into), letting us observe the exact notice — and prove there is
    // exactly one.
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    pause_with_pane(&reg, &g.id, &orch.id, 6402);

    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("Prototype the sidebar"), Some("prototype"), None)).unwrap();
    // Proceed is the human's promote verdict: unlike Start it is NOT rejected by
    // a paused group — the durable status flip carries the decision regardless.
    let after = reg.proceed_task(&g.id, &t.id).unwrap();
    assert_eq!(after.status, "in-progress", "proceed promotes the item back into active work");
    let note = after.notes.last().unwrap();
    assert_eq!(note.author, "human");
    assert!(note.text.contains("Proceed"), "the promote decision must be auditable on the board");

    // Exactly one PROCEED notice reaches the orchestrator — no spam.
    assert_eq!(
        suppressed_notices(&reg, &g.id, "PROCEED"), 1,
        "proceed delivers exactly one orchestrator notice"
    );
}

#[test]
fn proceed_is_guarded_to_prototype_items() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("Ship it"), None, None)).unwrap();
    // An unknown id is an error, not a silent no-op.
    assert!(reg.proceed_task(&g.id, "t-999").is_err());
    // Every non-prototype status must refuse, and refuse without mutating.
    for status in ["queued", "in-progress", "review", "pr", "human-testing", "done", "blocked"] {
        reg.upsert_task(&g.id, "orch-1", Some(&t.id), patch(None, Some(status), None)).unwrap();
        let before = reg.tasks(&g.id)[0].notes.len();
        assert!(reg.proceed_task(&g.id, &t.id).is_err(), "cannot proceed a {status} item");
        assert_eq!(reg.tasks(&g.id)[0].status, status, "a refused proceed must not change status");
        assert_eq!(reg.tasks(&g.id)[0].notes.len(), before, "a refused proceed must not leave a note");
    }
    // In prototype, it's allowed.
    reg.upsert_task(&g.id, "orch-1", Some(&t.id), patch(None, Some("prototype"), None)).unwrap();
    assert!(reg.proceed_task(&g.id, &t.id).is_ok(), "prototype is proceedable");
}

// ---------- review-round regression tests ----------

#[test]
fn concurrent_same_repo_launches_get_distinct_groups() {
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let repo_path = repo.path().to_string_lossy().into_owned();
    let reg = Arc::new(relaunch_registry(dir.path()));
    reg.set_port(45999);
    // The id is chosen by liveness, but the orchestrator that makes a group
    // live registers after the choice — without the creation lock, two
    // simultaneous launches share an id and cross-deliver reports.
    let mut handles = vec![];
    for _ in 0..2 {
        let reg = reg.clone();
        let repo = repo_path.clone();
        handles.push(std::thread::spawn(move || {
            create_orchestration_group(&reg, &repo, rails(), SessionOrigin::Fresh, None, None).map(|r| r.group_id)
        }));
    }
    let ids: Vec<GroupId> = handles.into_iter().map(|h| h.join().unwrap().unwrap()).collect();
    assert_ne!(ids[0], ids[1], "concurrent launches on one repo must not share a group");
}

#[test]
fn repo_paths_with_quotes_are_rejected() {
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let reg = Arc::new(relaunch_registry(dir.path()));
    reg.set_port(45999);
    let err = create_orchestration_group(&reg, "/tmp/evil\" ; rm -rf /", rails(), SessionOrigin::Fresh, None, None)
        .unwrap_err();
    assert!(err.contains("quote"), "the quote check must fire before anything else, got: {err}");
}

#[test]
fn roster_survives_agent_id_recycling_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let (s1, s2);
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
        s1 = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap().session_id.unwrap();
    }
    {
        // "Restart": agent ids start over at w-1, colliding with run 1.
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
        s2 = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap().session_id.unwrap();
    }
    let reg = relaunch_registry(dir.path());
    let sessions: Vec<String> = reg.session_roles().into_iter().map(|r| r.session_id).collect();
    assert!(sessions.contains(&s1), "run 1's session must survive id recycling");
    assert!(sessions.contains(&s2));
}

#[test]
fn audit_rotates_at_cap_and_backfill_reads_both_generations() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let gdir = reg.state_root().join(g.id.as_str());
    // Force a rotation with a tiny cap: the spawn entry moves to audit.1.
    rotate_audit_if_needed(&gdir, 1);
    assert!(gdir.join("audit.1.jsonl").is_file(), "rotation must produce the old generation");
    reg.audit(&g.id, "orrerix", "post-rotate", json!({}));
    assert!(gdir.join("audit.jsonl").is_file());
    // Session mapping still resolves from the rotated generation.
    let sessions: Vec<String> = reg.session_roles().into_iter().map(|r| r.session_id).collect();
    assert!(
        sessions.contains(&w.session_id.unwrap()),
        "backfill must read rotated audit generations"
    );
}

#[test]
fn session_roles_surfaces_task_branch_repo_and_pr_for_the_session_browser() {
    // #1: name/description/goal (the task text), repo, branch, and PR should
    // all be resolvable for the session browser off what orchestration
    // already tracks — no new user input required.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo-1", rails()).unwrap();
    let w = reg
        .spawn_agent(&g.id, Role::Worker, "builder", "implement the thing", false, Some("feat/thing".into()))
        .unwrap();
    let sid = w.session_id.clone().unwrap();

    // No task references this session yet — pr is None, everything else is
    // already resolvable from the roster + group.json alone.
    let before = reg.session_roles().into_iter().find(|r| r.session_id == sid).unwrap();
    assert_eq!(before.task, "implement the thing");
    assert_eq!(before.branch.as_deref(), Some("feat/thing"));
    assert_eq!(before.repo.as_deref(), Some("C:/tmp/repo-1"));
    assert_eq!(before.pr, None, "no board task references this session yet");

    // The orchestrator later opens a PR for this worker's task and records it
    // on the board, keyed by session — session_roles must pick it up live,
    // not just at spawn time.
    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("build feature"), None, None)).unwrap();
    reg.upsert_task(
        &g.id,
        "orch-1",
        Some(&t.id),
        TaskPatch { session: Some(sid.clone()), pr: Some("42".into()), ..Default::default() },
    )
    .unwrap();

    let after = reg.session_roles().into_iter().find(|r| r.session_id == sid).unwrap();
    assert_eq!(after.pr.as_deref(), Some("42"));
    // The rest is unaffected by the board edit.
    assert_eq!(after.task, "implement the thing");
    assert_eq!(after.branch.as_deref(), Some("feat/thing"));
}

#[test]
fn session_roles_never_fabricates_a_branch_for_roles_that_do_not_have_one() {
    // #1's "no fabrication" bar: the orchestrator (and a reviewer that never
    // got a worktree) must read as having no branch, not the internal
    // `agent/<id>` placeholder name every spawn computes regardless of role.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo-2", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let rev = reg.spawn_agent(&g.id, Role::Reviewer, "rev", "review PR #9", false, None).unwrap();

    let roles = reg.session_roles();
    let orch_role = roles.iter().find(|r| r.session_id == orch.session_id.clone().unwrap()).unwrap();
    let rev_role = roles.iter().find(|r| r.session_id == rev.session_id.clone().unwrap()).unwrap();
    assert_eq!(orch_role.branch, None, "the orchestrator works on the repo's own checkout, not a branch");
    assert_eq!(rev_role.branch, None, "a reviewer with no worktree never had a branch of its own");
    assert_eq!(rev_role.task, "review PR #9", "task text still surfaces even though branch does not");
}

#[test]
fn session_roles_backfills_task_and_branch_from_the_spawn_audit_for_pre_roster_groups() {
    // Graceful degrade (#1): a group whose agents.json predates this feature
    // (or predates the roster entirely — the records_from_audit fallback)
    // still surfaces task/branch by reading the spawn audit line directly,
    // applying the SAME role-gated rule live spawns use so a legacy
    // orchestrator row still never fabricates a branch.
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    reg.set_port(46011);
    let g = reg.create_group("C:/tmp/repo-legacy", rails()).unwrap();
    let gdir = reg.state_root().join(g.id.as_str());
    let line = json!({
        "ts_ms": 5, "actor": "loomux", "action": "agent-spawn",
        "detail": {
            "agent": "w-9", "role": "worker", "name": "legacy-worker",
            "cwd": "C:/tmp/repo-legacy", "worktree": false,
            "branch": "agent/w-9", "task": "fix the old bug",
            "session": "11111111-2222-4333-8444-555555555555",
        }
    });
    fs::write(gdir.join("audit.jsonl"), format!("{line}\n")).unwrap();

    let role = reg
        .session_roles()
        .into_iter()
        .find(|r| r.session_id == "11111111-2222-4333-8444-555555555555")
        .expect("backfilled from the audit line alone, no agents.json");
    assert_eq!(role.task, "fix the old bug");
    assert_eq!(role.branch.as_deref(), Some("agent/w-9"), "a worker's branch is trusted from the audit");
}

#[test]
fn parse_audit_lines_is_ordered_and_skips_malformed() {
    let text = "\
{\"ts_ms\":1,\"actor\":\"loomux\",\"action\":\"group-create\",\"detail\":{\"repo\":\"r\"}}
not json at all
{\"ts_ms\":2,\"actor\":\"human\",\"action\":\"prompt\",\"detail\":{\"to\":\"w-1\",\"text\":\"hi\\nthere\"}}

{\"ts_ms\":3,\"actor\":\"loomux\",\"action\":\"agent-spawn\"}";
    let entries = parse_audit_lines(text);
    // Malformed line and the blank line are skipped; the three valid ones
    // survive in file order.
    assert_eq!(entries.len(), 3, "malformed and blank lines must be skipped");
    assert_eq!(entries[0].action, "group-create");
    assert_eq!(entries[1].actor, "human");
    assert_eq!(entries[2].ts_ms, 3);
    // A line missing `detail` still parses (detail becomes null), not dropped.
    assert!(entries[2].detail.is_null());
    // Full prompt text is preserved for in-app expansion.
    assert_eq!(entries[1].detail["text"], "hi\nthere");
}

#[test]
fn parse_audit_lines_counts_what_it_skips() {
    // A real torn log: two whole records, one spliced pair (the #240 signature),
    // one blank line. Silence about the spliced line is what kept the corruption
    // invisible — the count is the fix (the viewer path breadcrumbs it).
    let text = "\
{\"ts_ms\":1,\"actor\":\"loomux\",\"action\":\"group-create\",\"detail\":{}}
{{\"\"actionaction\"\":\"\"agent-exitagent-exit\"\"

{\"ts_ms\":2,\"actor\":\"w-1\",\"action\":\"agent-exit\",\"detail\":{}}";
    let (entries, skipped) = parse_audit_lines_counted(text);
    assert_eq!(entries.len(), 2, "the whole records still parse — a torn log must not blank the viewer");
    assert_eq!(skipped, 1, "the spliced line is counted; the blank line is not");
    // The convenience wrapper stays the plain entry list.
    assert_eq!(parse_audit_lines(text).len(), 2);
}

#[test]
fn audit_log_reads_both_generations_oldest_first() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let gdir = reg.state_root().join(g.id.as_str());
    // Seed a group-create in the current log, then rotate it into audit.1 so
    // the two-generation read path is exercised.
    rotate_audit_if_needed(&gdir, 1);
    assert!(gdir.join("audit.1.jsonl").is_file());
    // Append a fresh entry to the new current log.
    reg.audit(&g.id, "human", "prompt", json!({ "to": "w-1", "text": "hello" }));

    let entries = reg.audit_log(&g.id);
    let actions: Vec<&str> = entries.iter().map(|e| e.action.as_str()).collect();
    // Rotated (older) generation first, then the current one.
    assert!(actions.contains(&"group-create"), "rotated generation must be included");
    assert_eq!(actions.last(), Some(&"prompt"), "current generation appends after the rotated one");
    let prompt = entries.iter().find(|e| e.action == "prompt").unwrap();
    assert_eq!(prompt.detail["text"], "hello");
}

#[test]
fn audit_log_of_unknown_group_is_empty() {
    let (reg, _d) = test_registry();
    assert!(reg.audit_log(&parse_gid("no-such-group")).is_empty());
}

/// A detail payload the size of a real one (agent-exit records carry summaries,
/// prompt records carry whole prompts). Fat details are what made #240 visible:
/// the wider the record, the wider the window for two writers to interleave.
fn fat_detail(thread: usize, seq: usize) -> Value {
    json!({ "thread": thread, "seq": seq, "summary": "x".repeat(4096) })
}

/// Every non-blank line of `text` must parse as JSON; returns the entries.
/// Panics with a truncated sample of the first bad line — the #240 signature is
/// character-level interleaving (`{{""actionaction""::…`), which is far easier
/// to recognize from the raw line than from a parse error.
fn assert_all_lines_parse(text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) => out.push(v),
            Err(e) => panic!(
                "audit line {i} is corrupt ({e}); every append must land as one whole line.\n\
                 first 160 bytes: {sample}",
                sample = line.chars().take(160).collect::<String>()
            ),
        }
    }
    out
}

/// #240: concurrent `audit` calls (mass agent-exit at shutdown, background
/// delivery threads) must each land as one whole line. The old writer
/// `Display`-formatted the record straight onto the file handle, which emits
/// many small writes per record — `O_APPEND` is atomic per *syscall*, so the
/// records interleaved token by token and the log became unparseable.
#[test]
fn concurrent_audit_appends_land_as_whole_lines() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    const THREADS: usize = 8;
    const PER_THREAD: usize = 40;

    std::thread::scope(|s| {
        for t in 0..THREADS {
            let reg = &reg;
            let gid = &g.id;
            s.spawn(move || {
                for i in 0..PER_THREAD {
                    reg.audit(gid, &format!("w-{t}"), "agent-exit", fat_detail(t, i));
                }
            });
        }
    });

    let text = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl")).unwrap();
    let entries = assert_all_lines_parse(&text);
    let exits = entries.iter().filter(|v| v["action"] == "agent-exit").count();
    assert_eq!(exits, THREADS * PER_THREAD, "every concurrent append must survive as one record");
}

/// #240: rotation renames the live log out from under concurrent appenders.
/// Contract: one rotation loses nothing — the appends that raced it are split
/// across the two generations (the viewer reads both), and none is corrupt.
/// Only *one* generation is kept, so a second rotation discarding the first is
/// the documented cap behavior, not a bug — this test rotates exactly once.
#[test]
fn audit_rotation_racing_appends_loses_no_lines() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let gdir = reg.state_root().join(g.id.as_str());
    const THREADS: usize = 6;
    const PER_THREAD: usize = 30;
    // Roughly a third of the total bytes the appenders will write, so the
    // rename lands mid-stream rather than before or after the burst.
    const ROTATE_CAP: u64 = 60 * 1024;

    let appending = std::sync::atomic::AtomicBool::new(true);
    std::thread::scope(|s| {
        let appenders: Vec<_> = (0..THREADS)
            .map(|t| {
                let reg = &reg;
                let gid = &g.id;
                s.spawn(move || {
                    for i in 0..PER_THREAD {
                        reg.audit(gid, &format!("w-{t}"), "agent-exit", fat_detail(t, i));
                    }
                })
            })
            .collect();
        let gdir = &gdir;
        let appending = &appending;
        s.spawn(move || {
            // Rotate once, as soon as the log crosses the cap. Stop at the first
            // rotation (a second would drop the first generation) and give up if
            // the appenders finish without ever crossing it.
            while appending.load(std::sync::atomic::Ordering::Relaxed) {
                rotate_audit_if_needed(gdir, ROTATE_CAP);
                if gdir.join("audit.1.jsonl").is_file() {
                    return;
                }
                std::thread::yield_now();
            }
        });
        for h in appenders {
            h.join().unwrap();
        }
        appending.store(false, std::sync::atomic::Ordering::Relaxed);
    });

    assert!(
        gdir.join("audit.1.jsonl").is_file(),
        "the rotator must have fired mid-burst, or this test proves nothing"
    );
    let mut text = String::new();
    for name in ["audit.1.jsonl", "audit.jsonl"] {
        if let Ok(t) = fs::read_to_string(gdir.join(name)) {
            text.push_str(&t);
            if !text.ends_with('\n') {
                text.push('\n');
            }
        }
    }
    let entries = assert_all_lines_parse(&text);
    let exits = entries.iter().filter(|v| v["action"] == "agent-exit").count();
    assert_eq!(
        exits,
        THREADS * PER_THREAD,
        "a single rotation must not lose appends — they split across the two generations"
    );
}

/// #240, the other half — the one the single `write_all` does NOT fix, so it
/// needs its own reproducer rather than an argument.
///
/// Rotation is check-then-rename. Two threads that both read a past-the-cap size
/// before either renames will BOTH rename: the first retires the full log to
/// `audit.1.jsonl`, appenders start refilling a fresh `audit.jsonl`, and then the
/// second — acting on its now-stale size check — renames that fresh, nearly-empty
/// log over `audit.1.jsonl`, discarding the generation the first just retained
/// (8 MB of history, in production). `AUDIT_LOCK` closes the window by making
/// check+rename atomic.
///
/// The window is a few instructions wide, so the test widens it through the
/// `set_rotate_check_pause_for_test` seam and staggers the two rotators: A
/// renames early, appenders write into the fresh log, B renames late. Without the
/// lock B's rename lands on a refilled log and the seeded generation is gone —
/// verified red (see the PR). With it, B's check runs *after* A's rename, sees a
/// log under the cap, and declines to rotate.
#[test]
fn concurrent_rotations_keep_the_retained_generation() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let gdir = reg.state_root().join(g.id.as_str());
    const SEEDED: usize = 50;
    // Staggered pauses, so the second rotator's rename lands well after the
    // first's — with appenders writing in between. Equal pauses would let both
    // renames fire within microseconds of each other, and the race could hide.
    const PAUSES_MS: [u64; 2] = [150, 600];

    for i in 0..SEEDED {
        reg.audit(&g.id, "w-0", "seeded", fat_detail(0, i));
    }
    let seeded_bytes = fs::metadata(gdir.join("audit.jsonl")).unwrap().len();
    // Past the cap for the seeded log (so both rotators' checks say "rotate"),
    // but far above anything the appenders below can add — a *legitimate* second
    // rotation would be the documented cap behavior, not the bug under test.
    let cap = seeded_bytes / 2;

    std::thread::scope(|s| {
        for pause in PAUSES_MS {
            let gdir = &gdir;
            s.spawn(move || {
                set_rotate_check_pause_for_test(Duration::from_millis(pause));
                rotate_audit_if_needed(gdir, cap);
            });
        }
        // Appenders refill the fresh log across the whole rotation window — this
        // is what gives a stale-check rotator something to clobber with. Small
        // details on purpose: they must not push the fresh log past `cap`.
        for t in 1..3 {
            let reg = &reg;
            let gid = &g.id;
            s.spawn(move || {
                for i in 0..15 {
                    reg.audit(gid, &format!("w-{t}"), "agent-exit", json!({ "seq": i }));
                    std::thread::sleep(Duration::from_millis(50));
                }
            });
        }
    });

    let rotated = fs::read_to_string(gdir.join("audit.1.jsonl")).unwrap();
    let kept = assert_all_lines_parse(&rotated);
    let seeded = kept.iter().filter(|v| v["action"] == "seeded").count();
    assert_eq!(
        seeded, SEEDED,
        "the retained generation must survive a rotation stampede — a second, stale-check rename \
         would move the refilled log over it and discard every seeded record"
    );
}

// ---------- durable roster & orchestration restore ----------

#[test]
fn roster_records_sessions_roles_and_liveness() {
    let dir = tempfile::tempdir().unwrap();
    let orch_sid;
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
        let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
        reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
        orch_sid = orch.session_id.unwrap();
        let roles = reg.session_roles();
        assert_eq!(roles.len(), 2);
        let o = roles.iter().find(|r| r.role == "orchestrator").unwrap();
        assert_eq!(o.session_id, orch_sid);
        assert!(o.group_live, "group with running agents must read as live");
    }
    // Fresh instance (app restart): roster survives, group reads dead.
    let reg = relaunch_registry(dir.path());
    let roles = reg.session_roles();
    assert!(roles.iter().any(|r| r.session_id == orch_sid && !r.group_live),
        "roster must survive restarts and report the group as not live");
}

#[test]
fn sessions_backfill_from_audit_when_roster_predates_it() {
    // Groups created before agents.json existed still have every spawn in
    // the audit log — their sessions must be markable and restorable too.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let sid = orch.session_id.unwrap();
    // Simulate the pre-roster era: the roster file never existed.
    fs::remove_file(reg.state_root().join(g.id.as_str()).join("agents.json")).unwrap();
    let roles = reg.session_roles();
    let o = roles
        .iter()
        .find(|r| r.session_id == sid)
        .expect("session must be discoverable via audit backfill");
    assert_eq!(o.role, "orchestrator");
}

#[test]
fn hint_restores_sessions_unknown_to_roster_and_audit() {
    // Pre-session-tracking orchestrators left no session id anywhere on
    // disk; the session browser identifies them from transcript signatures
    // and passes (group, role) hints. Restore must honor them — but only
    // for groups that actually exist.
    use loomux_lib::orchestration::resume_recorded_session;
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let repo_path = repo.path().to_string_lossy().into_owned();
    let reg = Arc::new(relaunch_registry(dir.path()));
    reg.set_port(45999);
    let g = reg.create_group(&repo_path, rails()).unwrap();
    let gid = g.id.clone();
    drop(g);
    let sid = "11112222-3333-4444-8555-666677778888";
    let hint = Some((gid.clone(), "orchestrator".to_string()));
    // The orchestrator branch pre-checks the session resolves in its CLI's
    // OWN store before opening a pane (#412 review B1) — fixture it.
    let store = scratch_dir("hint-restore-claude");
    fixture_claude_session(&store, sid, &repo_path);
    loomux_lib::sessions::set_claude_projects_root_for_test(Some(store.clone()));
    let req = resume_recorded_session(&reg, sid, hint, false).unwrap().expect("pane spec");
    loomux_lib::sessions::set_claude_projects_root_for_test(None);
    let _ = fs::remove_dir_all(&store);
    assert_eq!(req.group_id, gid);
    assert!(req.command.contains(&format!("--resume {sid}")));
    // A hint pointing at a nonexistent group is rejected, not trusted — this
    // fails on the group lookup itself, before any store check, so no
    // fixture is needed here.
    let reg2_dir = tempfile::tempdir().unwrap();
    let reg2 = Arc::new(relaunch_registry(reg2_dir.path()));
    let bad = resume_recorded_session(&reg2, sid, Some((parse_gid("ghost-1"), "orchestrator".into())), false);
    assert!(bad.is_err());
}

#[test]
fn orchestrator_session_restores_full_group_with_fresh_mcp_identity() {
    use loomux_lib::orchestration::resume_recorded_session;
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap(); // must exist for restore
    let repo_path = repo.path().to_string_lossy().into_owned();
    let (gid, orch_sid);
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        let g = reg.create_group(&repo_path, rails()).unwrap();
        gid = g.id.clone();
        let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
        orch_sid = orch.session_id.unwrap();
    }
    // "App restart": new registry, nothing live. Fixture the session into
    // claude's store so the orchestrator branch's existence pre-check
    // (#412 review B1) finds it, same as the other two orchestrator-resume
    // tests above.
    let store = scratch_dir("orch-full-restore");
    fixture_claude_session(&store, &orch_sid, &repo_path);
    loomux_lib::sessions::set_claude_projects_root_for_test(Some(store.clone()));
    let reg = Arc::new(relaunch_registry(dir.path()));
    reg.set_port(45999);
    let req = resume_recorded_session(&reg, &orch_sid, None, false).unwrap().expect("orchestrator returns a pane spec");
    loomux_lib::sessions::set_claude_projects_root_for_test(None);
    let _ = fs::remove_dir_all(&store);
    assert_eq!(req.group_id, gid, "restore must reattach to the recorded group (state/tasks/audit)");
    assert!(req.command.contains(&format!("--resume {orch_sid}")),
        "the orchestrator's conversation must be resumed, not cold-started");
    assert!(req.command.contains("--mcp-config"),
        "restore must re-wire MCP identity — the whole point");
    let g = reg.group(&gid).expect("group re-registered in memory");
    assert_eq!(g.guardrails.model_for(Role::Worker), "sonnet", "the block roster must be restored from group.json");
    // A second restore while live is refused.
    let err = resume_recorded_session(&reg, &orch_sid, None, false).unwrap_err();
    assert!(err.contains("already"), "got: {err}");
}

#[test]
fn worker_session_rejoin_requires_live_group_then_reuses_session() {
    use loomux_lib::orchestration::resume_recorded_session;
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    // A REAL directory (#412): the worker's cwd falls back to the group repo
    // itself here (`use_worktree: false`), and `resume_recorded_session` now
    // verifies that cwd still exists on disk before trusting it — a fake
    // path a real filesystem would never contain would (correctly) make the
    // resume fail loudly, which isn't what this test is exercising.
    let repo = tempfile::tempdir().unwrap();
    let repo_path = repo.path().to_string_lossy().into_owned();
    let reg = Arc::new(relaunch_registry(dir.path()));
    reg.set_port(45999);
    let g = reg.create_group(&repo_path, rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let sid = w.session_id.clone().unwrap();
    reg.mark_dead(&w.id, Some(0));
    // Group has no live agents → rejoin refused with guidance.
    let err = resume_recorded_session(&reg, &sid, None, false).unwrap_err();
    assert!(err.contains("orchestrator"), "must point at restarting the orchestrator, got: {err}");
    // With a live orchestrator, the rejoin spawns (background) reusing the session.
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    assert!(resume_recorded_session(&reg, &sid, None, false).unwrap().is_none(),
        "worker rejoin panes arrive via the spawn event, not the return value");
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let roster = reg.list_agents(&g.id).to_string();
        let rejoined = roster.matches(&sid).count() >= 1
            && reg.session_roles().iter().filter(|r| r.session_id == sid).count() >= 1;
        // The new agent entry must carry the SAME session id as the old one.
        if rejoined && roster.matches("\"status\":\"running\"").count() >= 2 {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "rejoin did not complete: {roster}");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(resume_recorded_session(&reg, "0000-not-recorded", None, false).is_err());
}

// ---------- #485: a session only ever rejoins its OWN group ----------
// The two-groups-one-tab bug: a tab can hold panes from two orchestration
// groups, and the dormant-group Resume click used to name the group from the
// TAB (one binding per tab) rather than from the pane's own captured record.
// Every rejoin funnels through `resume_recorded_session`, so that is where a
// caller's wrong group id has to be refused — a frontend that partitions its
// placeholders correctly is a happy path, not a guarantee.

#[test]
fn rejoining_a_session_into_another_groups_id_is_refused_loudly() {
    use loomux_lib::orchestration::resume_recorded_session;
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let repo_a = tempfile::tempdir().unwrap();
    let repo_b = tempfile::tempdir().unwrap();
    let reg = Arc::new(relaunch_registry(dir.path()));
    reg.set_port(45999);
    let group_a = reg.create_group(&repo_a.path().to_string_lossy(), rails()).unwrap().id.clone();
    let group_b = reg.create_group(&repo_b.path().to_string_lossy(), rails()).unwrap().id.clone();
    // Both groups live, so the rejoin is refused for the group id ALONE —
    // not incidentally, because the target group happened to be dead.
    reg.spawn_agent(&group_a, Role::Orchestrator, "orch-a", "", false, None).unwrap();
    reg.spawn_agent(&group_b, Role::Orchestrator, "orch-b", "", false, None).unwrap();
    let w = reg.spawn_agent(&group_b, Role::Worker, "w-b", "t", false, None).unwrap();
    let sid = w.session_id.clone().unwrap();
    reg.mark_dead(&w.id, Some(0));

    // Group A's id, group B's worker: the exact call the buggy tab-scoped
    // resume made for every delegate in the tab.
    let err = resume_recorded_session(&reg, &sid, Some((group_a.clone(), "worker".into())), false)
        .expect_err("a rejoin into a group the session does not belong to must fail");
    assert!(
        err.starts_with("resume-group-mismatch:"),
        "must carry the structured tag the frontend classifies on, got: {err}"
    );
    assert!(err.contains(group_b.as_str()), "must name the group the session DOES belong to, got: {err}");

    // And it must be a refusal, not a slower path to the same contamination:
    // A's roster never sees the session id. (The rejoin spawn is backgrounded,
    // so give a real one time to land before concluding it never happened.)
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !reg.list_agents(&group_a).to_string().contains(&sid),
        "group A must not have gained group B's session"
    );

    // The same session, named with its OWN group, still rejoins — the check
    // refuses a wrong group, it does not break rejoining.
    assert!(
        resume_recorded_session(&reg, &sid, Some((group_b.clone(), "worker".into())), false)
            .unwrap()
            .is_none(),
        "worker rejoin panes arrive via the spawn event, not the return value"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if reg.list_agents(&group_b).to_string().matches(&sid).count() >= 1 {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the same-group rejoin did not complete");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_record_less_delegate_is_refused_rather_than_rejoined_on_the_hint_alone() {
    // #485 review finding 1. The signature-only fallback below the roster/audit
    // lookups builds its record FROM THE HINT, so for a session with no recorded
    // membership anywhere the mismatch check is vacuous by construction — the
    // caller's claim is the only "evidence" there is. That left one reachable
    // route into the wrong group: a pre-#485 snapshot whose tab-derived hint
    // names group A while the placeholder is really group B's pre-roster worker.
    //
    // A DELEGATE rejoin is a membership operation, so with nothing to verify
    // against, the answer is a loud refusal rather than a rejoin on the caller's
    // say-so. (An ORCHESTRATOR resume is not a membership operation — it reopens
    // the control plane of a group whose group.json is on disk — so that arm of
    // the fallback survives; `hint_restores_sessions_unknown_to_roster_and_audit`
    // pins it.)
    use loomux_lib::orchestration::resume_recorded_session;
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let reg = Arc::new(relaunch_registry(dir.path()));
    reg.set_port(45999);
    let group = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap().id.clone();
    // A live group, so the refusal is about the UNVERIFIABLE membership alone —
    // not incidentally because there was nothing to rejoin into.
    reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();

    // A session id no roster row and no audit line anywhere names — but one that
    // DOES have a resumable transcript in the CLI's own store. That combination
    // is the whole point: without the store fixture the resume dies later on
    // `resume-not-found` (the cwd resolution), which would mask the corner
    // instead of testing it — exactly what this test asserted on its first run.
    let sid = "99998888-7777-6666-8555-444433332222";
    let store = scratch_dir("record-less-delegate");
    // Its recorded cwd is its OWN workspace, not the group's main clone — a
    // main-clone cwd is refused by a different guard (#338/#359), which would
    // again mask this one rather than exercise it.
    let workspace = tempfile::tempdir().unwrap();
    fixture_claude_session(&store, sid, &workspace.path().to_string_lossy());
    loomux_lib::sessions::set_claude_projects_root_for_test(Some(store.clone()));
    let outcome = resume_recorded_session(&reg, sid, Some((group.clone(), "worker".into())), false);
    loomux_lib::sessions::set_claude_projects_root_for_test(None);
    let _ = fs::remove_dir_all(&store);
    let err = outcome
        .expect_err("a delegate whose group cannot be verified must not be rejoined on the hint");
    assert!(
        err.starts_with("resume-group-unknown:"),
        "must carry its own structured tag — this is 'cannot be verified', not 'contradicted', got: {err}"
    );
    // The escape guidance must name a route that EXISTS (#485 review round 2).
    // "Resume it from the session browser" was circular: the browser classifies
    // a pre-roster session from its transcript signature and hints the same
    // group, landing back here. A dead end told as a door is worse than a dead
    // end told as one, so this pins that the message doesn't send the human in
    // that circle.
    let lowered = err.to_lowercase();
    assert!(
        !lowered.contains("resume it from the session browser"),
        "must not point at the session browser — that route returns to this same refusal: {err}"
    );
    assert!(
        lowered.contains("returns here") || lowered.contains("returns to"),
        "must say the browser leads back here rather than leaving it as an option: {err}"
    );
    assert!(
        lowered.contains("spawn a fresh agent") && lowered.contains("outside"),
        "must name the two routes that do exist — a fresh agent for the work, and reopening the \
         conversation outside orchestration: {err}"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !reg.list_agents(&group).to_string().contains(sid),
        "nothing may have been rejoined for a session whose membership is unverifiable"
    );
}

#[test]
fn two_groups_in_one_tab_both_restore_into_their_own_group() {
    // The round trip the tab-scoped resume could not do at all: TWO dormant
    // orchestrator groups resumed one after the other (the frontend now hints
    // with each placeholder's own captured group), then each group's worker
    // rejoined with that same per-member group id. Both groups end live, with
    // their own workers, and neither worker lands in the other's roster.
    use loomux_lib::orchestration::resume_recorded_session;
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let repo_a = tempfile::tempdir().unwrap();
    let repo_b = tempfile::tempdir().unwrap();
    let path_a = repo_a.path().to_string_lossy().into_owned();
    let path_b = repo_b.path().to_string_lossy().into_owned();
    let (group_a, group_b, orch_a_sid, orch_b_sid, w_a_sid, w_b_sid);
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        group_a = reg.create_group(&path_a, rails()).unwrap().id.clone();
        group_b = reg.create_group(&path_b, rails()).unwrap().id.clone();
        orch_a_sid = reg
            .spawn_agent(&group_a, Role::Orchestrator, "orch-a", "", false, None)
            .unwrap()
            .session_id
            .unwrap();
        orch_b_sid = reg
            .spawn_agent(&group_b, Role::Orchestrator, "orch-b", "", false, None)
            .unwrap()
            .session_id
            .unwrap();
        w_a_sid = reg
            .spawn_agent(&group_a, Role::Worker, "w-a", "t", false, None)
            .unwrap()
            .session_id
            .unwrap();
        w_b_sid = reg
            .spawn_agent(&group_b, Role::Worker, "w-b", "t", false, None)
            .unwrap()
            .session_id
            .unwrap();
    }
    // "App restart": a new registry, nothing live — both groups dormant, the
    // state a two-group tab comes back in.
    let store = scratch_dir("two-group-restore");
    fixture_claude_session(&store, &orch_a_sid, &path_a);
    fixture_claude_session(&store, &orch_b_sid, &path_b);
    loomux_lib::sessions::set_claude_projects_root_for_test(Some(store.clone()));
    let reg = Arc::new(relaunch_registry(dir.path()));
    reg.set_port(45999);

    let req_a = resume_recorded_session(&reg, &orch_a_sid, Some((group_a.clone(), "orchestrator".into())), false)
        .unwrap()
        .expect("group A's orchestrator returns a pane spec");
    let req_b = resume_recorded_session(&reg, &orch_b_sid, Some((group_b.clone(), "orchestrator".into())), false)
        .unwrap()
        .expect("group B's orchestrator must ALSO come back — not be dropped from the plan");
    loomux_lib::sessions::set_claude_projects_root_for_test(None);
    let _ = fs::remove_dir_all(&store);
    assert_eq!(req_a.group_id, group_a);
    assert_eq!(req_b.group_id, group_b);
    assert!(reg.group(&group_a).is_some() && reg.group(&group_b).is_some(),
        "both control planes must be live at once");

    // Each worker rejoins its own group, named by its own captured record.
    for (sid, gid) in [(&w_a_sid, &group_a), (&w_b_sid, &group_b)] {
        assert!(
            resume_recorded_session(&reg, sid, Some((gid.clone(), "worker".into())), false)
                .unwrap()
                .is_none()
        );
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let roster_a = reg.list_agents(&group_a).to_string();
        let roster_b = reg.list_agents(&group_b).to_string();
        if roster_a.contains(&w_a_sid) && roster_b.contains(&w_b_sid) {
            assert!(!roster_a.contains(&w_b_sid), "group B's worker must never appear in group A");
            assert!(!roster_b.contains(&w_a_sid), "group A's worker must never appear in group B");
            break;
        }
        assert!(std::time::Instant::now() < deadline, "both rejoins did not complete:\nA={roster_a}\nB={roster_b}");
        std::thread::sleep(Duration::from_millis(50));
    }
}
