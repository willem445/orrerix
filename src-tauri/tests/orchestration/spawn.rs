//! Worker spawn and resume placement: worktrees, cwd resolution, block identity, capability classes and resume prefixes.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ───────── #338: worker spawns always land in a dedicated worktree ─────────

#[test]
fn spawn_agent_mcp_worker_defaults_to_a_worktree() {
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();

    // No `worktree` argument at all: the default must still cut one. The
    // main clone is the human's environment, and a worker must not run there
    // unasked.
    let out = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "spawn_agent", "arguments": { "kind": "worker", "task": "t", "branch": "feat/x" } }))
        .unwrap();
    assert_eq!(out["isError"], false, "{out:?}");

    let agents = reg.list_agents(&g.id);
    let worker = agents.as_array().unwrap().iter().find(|a| a["role"] == "worker").unwrap();
    let cwd = worker["cwd"].as_str().unwrap();
    assert_ne!(
        Path::new(cwd), repo.path(),
        "a worker must not run in the main clone by default: {worker}"
    );
    assert!(Path::new(cwd).is_dir(), "the worktree directory must actually exist: {cwd}");
    let entry = reg.agent(worker["id"].as_str().unwrap()).unwrap();
    assert_eq!(
        entry.branch.as_deref(), Some("feat/x"),
        "the worker's own branch must be recorded: {entry:?}"
    );
}

#[test]
fn spawn_agent_mcp_rejects_explicit_worktree_false_for_a_worker() {
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();

    let out = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "spawn_agent", "arguments": { "kind": "worker", "task": "t", "worktree": false } }))
        .unwrap();
    assert_eq!(
        out["isError"], true,
        "an explicit worktree=false for a worker must be rejected, not silently coerced: {out:?}"
    );
    let text = out["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("#338"), "error must cite the guardrail, got: {text}");
    assert!(
        reg.list_agents(&g.id).as_array().unwrap().iter().all(|a| a["role"] != "worker"),
        "a rejected spawn must not have created a worker as a side effect"
    );

    // Naming the built-in worker block explicitly must be covered the same
    // way as the bare `kind: worker` default — the guard reads the
    // EFFECTIVE role (the block's kind), not just the `kind` fallback.
    let out = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "spawn_agent", "arguments": { "block": "worker", "task": "t", "worktree": false } }))
        .unwrap();
    assert_eq!(out["isError"], true, "a worker-kind BLOCK must be covered too: {out:?}");
    assert!(out["content"][0]["text"].as_str().unwrap().contains("#338"));
}

#[test]
fn spawn_agent_mcp_planner_is_unaffected_by_the_worktree_default() {
    // #338/#359 force a worktree for workers and reviewers; a planner's
    // structural "never a worktree" survives untouched — it doesn't error on
    // an explicit `worktree: false` either, since it never reaches the guard
    // (only Worker/Reviewer do, per `needs_dedicated_workspace`).
    let (reg, _d, co, _cw) = setup_mcp();

    let plan = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "spawn_agent", "arguments": { "kind": "planner", "task": "plan #1", "worktree": false } }))
        .unwrap();
    assert_eq!(plan["isError"], false, "{plan:?}");
    let agents = reg.list_agents(&co.group);
    let planner = agents.as_array().unwrap().iter().find(|a| a["role"] == "planner").unwrap();
    assert_eq!(planner["cwd"], json!("C:/tmp/repo"), "planner must never get a worktree: {planner}");
}

#[test]
fn spawn_agent_mcp_reviewer_defaults_to_a_worktree() {
    // #359: extending #338's worker guarantee to reviewers — a reviewer
    // spawned via the MCP tool with no `worktree` argument must still land in
    // a dedicated worktree, never the main clone (the rev-36/rev-38 incident:
    // two reviewers, or a reviewer and the orchestrator's own git traffic,
    // contending on the shared clone's checkout state).
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();

    let out = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "spawn_agent", "arguments": { "kind": "reviewer", "task": "review #1" } }))
        .unwrap();
    assert_eq!(out["isError"], false, "{out:?}");
    let agents = reg.list_agents(&g.id);
    let reviewer = agents.as_array().unwrap().iter().find(|a| a["role"] == "reviewer").unwrap();
    let cwd = reviewer["cwd"].as_str().unwrap();
    assert_ne!(
        Path::new(cwd), repo.path(),
        "a reviewer must not run in the main clone by default: {reviewer}"
    );
    assert!(Path::new(cwd).is_dir(), "the worktree directory must actually exist: {cwd}");
}

#[test]
fn spawn_agent_mcp_rejects_explicit_worktree_false_for_a_reviewer() {
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();

    let out = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "spawn_agent", "arguments": { "kind": "reviewer", "task": "t", "worktree": false } }))
        .unwrap();
    assert_eq!(
        out["isError"], true,
        "an explicit worktree=false for a reviewer must be rejected, not silently coerced: {out:?}"
    );
    let text = out["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("#359"), "error must cite the guardrail, got: {text}");
    assert!(
        reg.list_agents(&g.id).as_array().unwrap().iter().all(|a| a["role"] != "reviewer"),
        "a rejected spawn must not have created a reviewer as a side effect"
    );
}

#[test]
fn spawn_agent_mcp_rejects_cwd_on_a_fresh_reviewer_spawn() {
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();
    let elsewhere = tempfile::tempdir().unwrap();

    let out = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": { "kind": "reviewer", "task": "t", "cwd": elsewhere.path().to_string_lossy() },
    })).unwrap();
    assert_eq!(
        out["isError"], true,
        "a fresh reviewer spawn with an explicit cwd must be rejected, not silently honored: {out:?}"
    );
    assert!(out["content"][0]["text"].as_str().unwrap().contains("#359"));
}

#[test]
fn spawn_agent_mcp_reviewer_resume_with_no_cwd_inherits_or_rejects_like_a_worker() {
    // #359: the resume-side half of the reviewer guarantee mirrors the
    // worker one exactly — inherit the recorded workspace when one exists,
    // reject when nothing is recorded, never fall back to the main clone.
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();

    let spawn = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "spawn_agent", "arguments": { "kind": "reviewer", "task": "t" } }))
        .unwrap();
    assert_eq!(spawn["isError"], false, "{spawn:?}");
    let before = reg.list_agents(&g.id);
    let rev = before.as_array().unwrap().iter().find(|a| a["role"] == "reviewer").unwrap().clone();
    let (session, original_cwd) = (
        rev["session"].as_str().unwrap().to_string(),
        rev["cwd"].as_str().unwrap().to_string(),
    );
    reg.mark_dead(rev["id"].as_str().unwrap(), Some(0));

    let resumed = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": { "kind": "reviewer", "resume_session": session, "task": "follow-up" },
    })).unwrap();
    assert_eq!(resumed["isError"], false, "{resumed:?}");
    let after = reg.list_agents(&g.id);
    let resumed_agent = after.as_array().unwrap().iter()
        .find(|a| a["session"] == json!(session) && a["status"] != json!("dead")).unwrap();
    assert_eq!(
        resumed_agent["cwd"], json!(original_cwd),
        "a cwd-less reviewer resume must inherit the session's recorded workspace: {after}"
    );

    // No recorded workspace at all: rejected, not a main-clone fall-back.
    let out = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": {
            "kind": "reviewer",
            "resume_session": "33333333-3333-4333-8333-333333333333",
            "task": "follow-up",
        },
    })).unwrap();
    assert_eq!(out["isError"], true, "{out:?}");
    assert!(out["content"][0]["text"].as_str().unwrap().contains("#359"));
}

#[test]
fn spawn_agent_mcp_two_concurrent_reviewers_get_distinct_worktrees_not_the_main_clone() {
    // Direct regression pin for the #359 incident: rev-36 and rev-38 both in
    // the SAME main clone at once, one restoring `main` out from under the
    // other's mid-review checkout. Two reviewers spawned into one group must
    // land in two DIFFERENT directories, neither of which is the main clone.
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), Guardrails { max_agents: 5, ..rails() }).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();

    let a = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "spawn_agent", "arguments": { "task": "review PR #355", "kind": "reviewer", "branch": "rev-a" } }))
        .unwrap();
    assert_eq!(a["isError"], false, "{a:?}");
    let b = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "spawn_agent", "arguments": { "task": "review PR #358", "kind": "reviewer", "branch": "rev-b" } }))
        .unwrap();
    assert_eq!(b["isError"], false, "{b:?}");

    let agents = reg.list_agents(&g.id);
    let cwds: Vec<&str> = agents.as_array().unwrap().iter()
        .filter(|x| x["role"] == "reviewer")
        .map(|x| x["cwd"].as_str().unwrap())
        .collect();
    assert_eq!(cwds.len(), 2, "both reviewers must be on the roster: {agents}");
    assert_ne!(cwds[0], cwds[1], "concurrent reviewers must not share a workspace: {agents}");
    for cwd in &cwds {
        assert_ne!(
            Path::new(cwd), repo.path(),
            "no reviewer may be in the main clone, the #359 incident verbatim: {agents}"
        );
    }
}

#[test]
fn spawn_agent_mcp_resume_ignores_the_worker_worktree_guard() {
    // A follow-up resume's workspace is governed entirely by `cwd` — passing
    // `worktree: false` alongside it must not trip the #338 guard, since the
    // flag can't do anything once `cwd_override` is set.
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();
    let w = reg.list_agents(&g.id).as_array().unwrap().iter().find(|a| a["role"] == "worker").unwrap().clone();
    let (session, cwd) = (w["session"].as_str().unwrap().to_string(), w["cwd"].as_str().unwrap().to_string());
    reg.mark_dead(w["id"].as_str().unwrap(), Some(0));

    let out = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": { "kind": "worker", "resume_session": session, "cwd": cwd, "task": "follow-up", "worktree": false },
    })).unwrap();
    assert_eq!(out["isError"], false, "a resume must not trip the worker worktree guard: {out:?}");
}

// ───────── rev-13 finding on #345: a cwd-less worker resume must not land in the main clone ─────────

#[test]
fn spawn_agent_mcp_resume_with_no_cwd_inherits_the_recorded_workspace() {
    // The seam: a worker RESUME that omits `cwd` used to fall straight through
    // to spawn_agent_ex's main-clone default. It must instead inherit the
    // session's recorded workspace from the roster, same as #254 does for the
    // block identity.
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();

    // Fresh worker spawn via the MCP tool: worktree defaults on (#338), so its
    // recorded cwd is a real dedicated worktree, never the main clone.
    let spawn = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "spawn_agent", "arguments": { "kind": "worker", "task": "t", "branch": "feat/x" } }))
        .unwrap();
    assert_eq!(spawn["isError"], false, "{spawn:?}");
    let before = reg.list_agents(&g.id);
    let w = before.as_array().unwrap().iter().find(|a| a["role"] == "worker").unwrap().clone();
    let (session, original_cwd) = (
        w["session"].as_str().unwrap().to_string(),
        w["cwd"].as_str().unwrap().to_string(),
    );
    assert_ne!(Path::new(&original_cwd), repo.path(), "test setup sanity: must start outside the main clone");
    reg.mark_dead(w["id"].as_str().unwrap(), Some(0));

    // Resume with NO cwd at all.
    let resumed = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": { "kind": "worker", "resume_session": session, "task": "follow-up" },
    })).unwrap();
    assert_eq!(resumed["isError"], false, "{resumed:?}");
    let after = reg.list_agents(&g.id);
    let resumed_agent = after
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["session"] == json!(session) && a["status"] != json!("dead"))
        .unwrap();
    assert_eq!(
        resumed_agent["cwd"], json!(original_cwd),
        "a cwd-less resume must inherit the session's recorded workspace: {after}"
    );
}

#[test]
fn spawn_agent_mcp_rejects_a_worker_resume_with_no_cwd_and_no_recorded_workspace() {
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();

    // A full-length session id this group's roster has never heard of —
    // `resolve_session_ref` accepts a full id verbatim (#190), so there is no
    // roster record and therefore no recorded workspace to inherit. Explicit
    // `kind` so the #254 block-inheritance path's OWN "unknown session" error
    // (a different message) isn't what fires here.
    let out = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": {
            "kind": "worker",
            "resume_session": "11111111-1111-4111-8111-111111111111",
            "task": "follow-up",
        },
    })).unwrap();
    assert_eq!(
        out["isError"], true,
        "a worker resume with no cwd and nothing recorded must be rejected, not fall back to \
         the main clone: {out:?}"
    );
    let text = out["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("#338"), "error must cite the guardrail, got: {text}");
    assert!(
        reg.list_agents(&g.id).as_array().unwrap().iter().all(|a| a["role"] != "worker"),
        "a rejected resume must not have spawned a worker as a side effect"
    );
    // #412c: distinguishable, machine-parseable tag alongside the human prose —
    // an orchestrator reading this can branch on it instead of string-sniffing
    // "#338".
    assert!(
        text.contains("resume-not-found") || text.contains("resume-store-unreadable"),
        "error must carry a structured #412 tag, got: {text}"
    );
}

// ───────── #412: resume resolution must be robust to a moved/deleted worktree ─────────
//
// The confirmed repro (issue #412, second comment): a restore pane claims a
// session "can't be found" while the exact same session is plainly visible in
// the session browser (which scans the CLI's WHOLE store) — because the
// roster's cached cwd pointed at a worktree that no longer exists, and the
// old code silently fell through to the group's main clone instead of
// re-resolving from the CLI's own store or failing loudly.

#[test]
fn resume_recorded_session_fails_loudly_not_silently_when_the_worktree_is_gone() {
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let reg = std::sync::Arc::new(reg);
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    // A REAL dedicated worktree (mirrors how every worker actually runs).
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", true, None).unwrap();
    let sid = w.session_id.clone().unwrap();
    let cwd_before = reg
        .list_agents(&g.id)
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == w.id.as_str())
        .unwrap()["cwd"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(Path::new(&cwd_before).is_dir(), "test setup sanity: worktree must exist before removal");
    reg.mark_dead(&w.id, Some(0));

    // The worktree is removed from disk — the #412 scenario exactly (a
    // merged/cleaned-up branch's worktree gone, but the roster still names
    // its old path).
    std::fs::remove_dir_all(&cwd_before).unwrap();

    // An EMPTY fixture store (#412 review N6): pins exactly which tag this
    // reproduces (`resume-not-found`) instead of accepting any of three, and
    // is hermetic — it doesn't depend on this session's random uuid genuinely
    // being absent from whatever machine happens to run the suite.
    let store = scratch_dir("worktree-gone-empty-store");
    loomux_lib::sessions::set_claude_projects_root_for_test(Some(store.clone()));
    use loomux_lib::orchestration::resume_recorded_session;
    let err = resume_recorded_session(&reg, &sid, None, false).unwrap_err();
    loomux_lib::sessions::set_claude_projects_root_for_test(None);
    let _ = fs::remove_dir_all(&store);
    assert!(err.starts_with("resume-not-found:"), "must fail with the not-found tag, got: {err}");
    assert!(!Path::new(&cwd_before).is_dir(), "must not resurrect the deleted worktree");
    // No side effect: no new running worker (the OLD silent-fallback bug
    // would have spawned one in a background thread, in the main clone).
    let roster = reg.list_agents(&g.id);
    assert!(
        roster.as_array().unwrap().iter().all(|a| a["role"] != "worker" || a["status"] == "dead"),
        "a loudly-failed resume must not have spawned a worker as a side effect, got: {roster}"
    );
}

#[test]
fn resume_recorded_session_recovers_via_the_store_when_the_roster_cwd_is_stale() {
    // The fix's actual, user-visible success path (#412 review B2): when the
    // roster's cached cwd is gone but the session genuinely IS in the CLI's
    // own store (recorded under some OTHER, still-real directory — the
    // ordinary case: the worktree moved, or loomux's cached copy just
    // disagrees with what the CLI itself wrote), the resume must SUCCEED,
    // launched from the STORE's cwd — not fail, and not silently default to
    // the main clone.
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let reg = std::sync::Arc::new(reg);
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", true, None).unwrap();
    let sid = w.session_id.clone().unwrap();
    let stale_cwd = reg
        .list_agents(&g.id)
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == w.id.as_str())
        .unwrap()["cwd"]
        .as_str()
        .unwrap()
        .to_string();
    reg.mark_dead(&w.id, Some(0));
    std::fs::remove_dir_all(&stale_cwd).unwrap();

    // The store's OWN record: a DIFFERENT, still-real directory — never the
    // deleted one, never `group.repo` — so a passing assertion can only mean
    // the store's cwd was actually used, not a coincidence.
    let real_cwd = scratch_dir("store-recovers-real-cwd");
    assert_ne!(Path::new(&real_cwd), repo.path());
    let store = scratch_dir("store-recovers-claude-store");
    fixture_claude_session(&store, &sid, &real_cwd.to_string_lossy());
    loomux_lib::sessions::set_claude_projects_root_for_test(Some(store.clone()));

    use loomux_lib::orchestration::resume_recorded_session;
    let outcome = resume_recorded_session(&reg, &sid, None, false);
    loomux_lib::sessions::set_claude_projects_root_for_test(None);
    let _ = fs::remove_dir_all(&store);
    assert!(outcome.is_ok(), "must succeed once the store resolves it, got: {outcome:?}");

    // Worker/reviewer rejoins spawn in a background thread — poll for it.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let resumed_cwd = loop {
        let roster = reg.list_agents(&g.id);
        let hit = roster
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["session"] == json!(sid) && a["status"] == "running")
            .map(|a| a["cwd"].as_str().unwrap().to_string());
        if let Some(cwd) = hit {
            break cwd;
        }
        assert!(std::time::Instant::now() < deadline, "resumed worker never appeared: {roster}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        Path::new(&resumed_cwd),
        real_cwd.as_path(),
        "must launch from the STORE's recorded cwd, not the deleted one and not the main clone"
    );
    let _ = fs::remove_dir_all(&real_cwd);
}

// ───────── follow-up finding: a FRESH worker spawn's cwd is the other half of the #338 door ─────────

#[test]
fn spawn_agent_mcp_rejects_cwd_on_a_fresh_worker_spawn() {
    // No resume_session at all — `cwd` on a fresh spawn is `spawn_agent_ex`'s
    // `cwd_override`, which wins over `worktree` unconditionally, so this is
    // just as complete a bypass of the dedicated-worktree guarantee as
    // `worktree: false` would be.
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();
    let elsewhere = tempfile::tempdir().unwrap();

    let out = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": { "kind": "worker", "task": "t", "cwd": elsewhere.path().to_string_lossy() },
    })).unwrap();
    assert_eq!(
        out["isError"], true,
        "a fresh worker spawn with an explicit cwd must be rejected, not silently honored: {out:?}"
    );
    let text = out["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("#338"), "error must cite the guardrail, got: {text}");
    assert!(
        reg.list_agents(&g.id).as_array().unwrap().iter().all(|a| a["role"] != "worker"),
        "a rejected spawn must not have created a worker as a side effect"
    );

    // Still rejected even paired with worktree:true — an explicit cwd is the
    // problem regardless of what worktree says, since cwd_override wins over
    // it either way.
    let out = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": { "kind": "worker", "task": "t", "cwd": elsewhere.path().to_string_lossy(), "worktree": true },
    })).unwrap();
    assert_eq!(out["isError"], true, "worktree:true must not excuse an explicit cwd either: {out:?}");

    // Naming the built-in worker block explicitly is covered the same way.
    let out = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": { "block": "worker", "task": "t", "cwd": elsewhere.path().to_string_lossy() },
    })).unwrap();
    assert_eq!(out["isError"], true, "a worker-kind BLOCK must be covered too: {out:?}");
}

#[test]
fn spawn_agent_mcp_planner_fresh_spawn_with_cwd_is_unaffected() {
    // A planner is the only role left unaffected by the fresh-spawn cwd
    // guard — it never gets a worktree under any circumstance, so an
    // explicit cwd is still honored as a raw override for it, unchanged.
    let (reg, _d, co, _cw) = setup_mcp();
    let elsewhere = tempfile::tempdir().unwrap();
    let elsewhere_path = elsewhere.path().to_string_lossy().to_string();

    let out = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": { "kind": "planner", "task": "plan #1", "cwd": elsewhere_path },
    })).unwrap();
    assert_eq!(out["isError"], false, "a fresh planner spawn with cwd is unaffected: {out:?}");
    let agents = reg.list_agents(&co.group);
    let planner = agents.as_array().unwrap().iter().find(|a| a["role"] == "planner").unwrap();
    assert_eq!(
        planner["cwd"], json!(elsewhere_path),
        "planner's explicit cwd is honored as-is, unchanged: {planner}"
    );
}

#[test]
fn spawn_agent_mcp_accepts_planner_kind() {
    let (reg, _d, co, _cw) = setup_mcp();
    // The orchestrator can spawn a planner via the shared spawn_agent tool (#47).
    let r = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "spawn_agent", "arguments": { "kind": "planner", "task": "plan issue #47" } }))
        .unwrap();
    assert_eq!(r["isError"], false, "planner spawn must succeed");
    assert!(
        r["content"][0]["text"].as_str().unwrap().contains("Planner"),
        "spawn result must report the planner role"
    );
    // The planner is on the roster with the planner role.
    let planner = reg
        .list_agents(&co.group)
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["role"] == "planner");
    assert!(planner, "spawned planner must appear on the roster");
}

// ───────── #254: a block-less resume must inherit its identity, not guess it ─────────
//
// Root cause was three individually-reasonable defaults composing into a silent
// role change: mcp.rs defaulted an absent `kind` to worker, mod.rs's `block_for`
// then picked the *default* block for that (wrong) role, and `block_for` itself
// picks the first block of a kind in file order. Together: resume a reviewer
// with no `block` and it comes back a worker running `worker-deep` — wrong
// model, wrong persona, and (since `review_verdict` is reviewer-only) unable to
// ever record its verdict, with no error anywhere.

#[test]
fn resume_of_reviewer_session_inherits_reviewer_block_not_default_worker() {
    let (reg, _d) = test_registry();
    // A real repo: the fresh reviewer spawn below goes through the MCP tool,
    // and a reviewer spawn's worktree now defaults on too (#359).
    let repo = real_repo();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();

    // A reviewer is spawned and then killed (mirrors the real incident: the
    // orchestrator killed a live reviewer pane and now wants it back).
    let spawn = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "spawn_agent", "arguments": { "kind": "reviewer", "task": "review PR #7" } }))
        .unwrap();
    assert_eq!(spawn["isError"], false, "{spawn:?}");
    let before = reg.list_agents(&co.group);
    let rev = before.as_array().unwrap().iter().find(|a| a["role"] == "reviewer").unwrap();
    let (rev_id, session, cwd, block) = (
        rev["id"].as_str().unwrap().to_string(),
        rev["session"].as_str().unwrap().to_string(),
        rev["cwd"].as_str().unwrap().to_string(),
        rev["block"].as_str().unwrap().to_string(),
    );
    reg.mark_dead(&rev_id, Some(0));

    // The orchestrator resumes it exactly as the tool description instructs
    // for a follow-up: resume_session + cwd, NEITHER kind NOR block.
    let resumed = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": { "resume_session": session, "cwd": cwd, "task": "round-2 fix pushed" },
    })).unwrap();
    assert_eq!(resumed["isError"], false, "block-less resume must succeed: {resumed:?}");
    let text = resumed["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Reviewer"), "resumed agent must stay a REVIEWER, got: {text}");
    assert!(
        text.contains(&format!("block {block}")),
        "resumed agent must keep its ORIGINAL block {block:?}, got: {text}"
    );

    // The proof that matters: the resumed agent can still record a verdict —
    // a bare-defaulted worker would be structurally denied this tool.
    let cr = reg.resolve_token(
        &reg.list_agents(&co.group)
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["session"] == json!(session) && a["status"] != json!("dead"))
            .and_then(|a| reg.agent(a["id"].as_str().unwrap()))
            .unwrap()
            .token,
    ).unwrap();
    let verdict = dispatch(&reg, &cr, "tools/call", &json!({
        "name": "review_verdict",
        "arguments": { "pr": "#7", "verdict": "pass", "summary": "looks good" },
    })).unwrap();
    assert_eq!(verdict["isError"], false, "resumed reviewer must still be able to record a verdict: {verdict:?}");
}

#[test]
fn resume_with_an_empty_string_block_still_inherits_instead_of_defaulting() {
    // Round-1 review finding (B1): `arg_str` returns `Some("")` for an
    // explicit `"block": ""`, which must be indistinguishable from an
    // omitted `block` — otherwise `{"resume_session": .., "block": ""}`
    // (kind absent) slips past the `block.is_none()` inheritance guard, and
    // mod.rs's own block resolution then trims/discards the empty id and
    // falls back to `block_for(Worker)`: the #254 bug verbatim.
    let (reg, _d) = test_registry();
    // A real repo: the fresh reviewer spawn below goes through the MCP tool,
    // and a reviewer spawn's worktree now defaults on too (#359).
    let repo = real_repo();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();

    let spawn = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "spawn_agent", "arguments": { "kind": "reviewer", "task": "review PR #7" } }))
        .unwrap();
    assert_eq!(spawn["isError"], false, "{spawn:?}");
    let before = reg.list_agents(&co.group);
    let rev = before.as_array().unwrap().iter().find(|a| a["role"] == "reviewer").unwrap();
    let (rev_id, session, cwd, block) = (
        rev["id"].as_str().unwrap().to_string(),
        rev["session"].as_str().unwrap().to_string(),
        rev["cwd"].as_str().unwrap().to_string(),
        rev["block"].as_str().unwrap().to_string(),
    );
    reg.mark_dead(&rev_id, Some(0));

    // Same resume as the bare case, but with an explicit empty-string block.
    let resumed = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": { "resume_session": session, "cwd": cwd, "task": "round-2", "block": "" },
    })).unwrap();
    assert_eq!(resumed["isError"], false, "{resumed:?}");
    let text = resumed["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Reviewer"), "empty-string block must not defeat inheritance, got: {text}");
    assert!(
        text.contains(&format!("block {block}")),
        "must keep its ORIGINAL block {block:?}, got: {text}"
    );
}

#[test]
fn resume_of_unknown_session_with_no_block_or_kind_hard_errors() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();
    let dir = tempfile::tempdir().unwrap();

    let r = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": {
            "resume_session": "00000000-0000-4000-8000-000000000000",
            "cwd": dir.path().to_string_lossy(),
            "task": "follow-up",
        },
    })).unwrap();
    assert_eq!(r["isError"], true, "an unrecorded session with no block must never silently default: {r:?}");
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("unknown session"), "error must name the problem, got: {text}");

    // And no agent must have been spawned as a side effect of the failed call.
    let agents = reg.list_agents(&co.group);
    assert!(
        agents.as_array().unwrap().iter().all(|a| a["role"] != "worker"),
        "a failed resume must not have spawned a default worker: {agents}"
    );
}

#[test]
fn resume_of_worker_session_keeps_its_original_block_not_the_roster_default() {
    // A custom workflow with TWO worker blocks, `worker-deep` declared first —
    // `block_for(Worker)` (mod.rs) picks the first block of a kind in file
    // order, so this is the exact trap the issue names: a naive fix that
    // re-derives "the worker default" for a bare resume would silently
    // relabel a `worker-fast` session as `worker-deep`.
    // #464 B1: a bare `tempfile::tempdir()` git-init'd fixture (the shape
    // this test used before) leaks its `-worktrees` sibling exactly like
    // `real_repo()` did pre-fix — the worker-fast spawn below goes through
    // the MCP tool, whose worktree defaults on (#338), same as every other
    // leaking test. Use `real_repo()` (already nested under its own private
    // temp root) and layer `.loomux/workflow.yml` on top with a second
    // commit — nothing about this test needs the file in the FIRST commit.
    let repo = real_repo();
    let loomux = repo.path().join(".loomux");
    fs::create_dir_all(&loomux).unwrap();
    fs::write(
        loomux.join("workflow.yml"),
        "version: 1\nname: multi-worker\nblocks:\n\
         \x20 - id: worker-deep\n    kind: worker\n\
         \x20 - id: worker-fast\n    kind: worker\n",
    )
    .unwrap();
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .current_dir(repo.path())
            .args(args)
            .output()
            .expect("git must be installed for this test");
        assert!(ok.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&ok.stderr));
    };
    git(&["add", "-A"]);
    git(&["commit", "-qm", "add workflow.yml"]);

    let (reg, _d) = test_registry();
    let g = reg
        .create_group(
            &repo.path().to_string_lossy(),
            Guardrails { advanced_orchestrator: true, ..rails() },
        )
        .unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();

    // Confirm the trap is live: the roster's file-order default really is
    // worker-deep, not worker-fast.
    assert_eq!(
        reg.group(&g.id).unwrap().guardrails.block_for(Role::Worker).unwrap().id,
        "worker-deep",
        "test setup must reproduce the file-order default the issue names"
    );

    // Spawn explicitly under the SECOND block, worker-fast, then kill it.
    let spawn = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "spawn_agent", "arguments": { "block": "worker-fast", "task": "fix #9" } }))
        .unwrap();
    assert_eq!(spawn["isError"], false, "{spawn:?}");
    let before = reg.list_agents(&co.group);
    let w = before.as_array().unwrap().iter().find(|a| a["block"] == "worker-fast").unwrap();
    let (wid, session, cwd) = (
        w["id"].as_str().unwrap().to_string(),
        w["session"].as_str().unwrap().to_string(),
        w["cwd"].as_str().unwrap().to_string(),
    );
    reg.mark_dead(&wid, Some(0));

    // A block-less, kind-less resume must keep worker-fast — NOT fall back to
    // the roster's file-order default, worker-deep.
    let resumed = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": { "resume_session": session, "cwd": cwd, "task": "follow-up" },
    })).unwrap();
    assert_eq!(resumed["isError"], false, "{resumed:?}");
    let after = reg.list_agents(&co.group);
    let resumed_agent = after
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["session"] == json!(session) && a["status"] != json!("dead"))
        .unwrap();
    assert_eq!(
        resumed_agent["block"], "worker-fast",
        "resume must keep the ORIGINAL block, not the roster's file-order default: {after}"
    );
}

// ───────── #544: a capability class is never acquired by OMISSION ─────────

#[test]
fn spawn_agent_never_defaults_to_the_privileged_class() {
    // The incident, verbatim (#544): three reviewer-shaped briefs were spawned
    // with `kind` omitted and came back as WORKERS — read-write panes with
    // edit tools and git commit/push, pointed at "review this PR / record your
    // verdict" tasks, one of them literally named `rev: …`. `kind` defaulted to
    // `Role::Worker`, the MOST-privileged class, so forgetting a parameter
    // meant silently receiving more capability than intended: a fail-open
    // default on a capability boundary. Every containment guardrail this repo
    // has (#448/#462/#465) protects a pane that was correctly *classified*;
    // none of them fire when the classification itself was acquired by
    // omission.
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();

    let out = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": {
            "name": "rev: #536 question-guard",
            "task": "Fresh review of PR #536. Record your verdict with review_verdict.",
        },
    })).unwrap();

    // LOUD, per the issue's acceptance: a refusal, not a quieter pane.
    assert_eq!(
        out["isError"], true,
        "a fresh spawn naming no capability class must be refused, not defaulted: {out:?}"
    );
    let text = out["content"][0]["text"].as_str().unwrap();
    // ...and the refusal has to say what to pass, or it just moves the guessing
    // to the caller.
    assert!(text.contains("#544"), "refusal must cite the guardrail, got: {text}");
    for want in ["kind", "block", "worker", "reviewer", "planner", "resume_session"] {
        assert!(text.contains(want), "refusal must name {want:?} as the way out, got: {text}");
    }

    // The half that actually matters: nothing privileged came into existence.
    let agents = reg.list_agents(&g.id);
    assert!(
        agents.as_array().unwrap().iter().all(|a| a["role"] != "worker"),
        "a refused spawn must not have created a worker as a side effect: {agents}"
    );

    // An empty-string `block` is an omission too, not a class. `arg_str`
    // returns `Some("")` for `"block": ""`, and mod.rs's own block resolution
    // trims-and-discards an empty id before falling back to `block_for(...)` —
    // so a guard that only checked `is_none()` would wave this straight through
    // to the very default it exists to remove. Exactly the hole #254's own
    // review found on the resume side (B1); pinned here for the fresh one.
    let out = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": { "block": "  ", "task": "t" },
    })).unwrap();
    assert_eq!(
        out["isError"], true,
        "an empty/whitespace block names no class and must be refused like an omission: {out:?}"
    );
    assert!(out["content"][0]["text"].as_str().unwrap().contains("#544"));

    // The same brief WITH the class named spawns what was actually intended,
    // and it is a reviewer — the refusal is a prompt to be explicit, not a
    // dead end.
    let ok = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": {
            "kind": "reviewer",
            "name": "rev: #536 question-guard",
            "task": "Fresh review of PR #536. Record your verdict with review_verdict.",
        },
    })).unwrap();
    assert_eq!(ok["isError"], false, "an explicit class must still spawn: {ok:?}");
    assert!(
        ok["content"][0]["text"].as_str().unwrap().contains("Reviewer"),
        "the explicit spawn must be a REVIEWER: {:?}", ok["content"][0]["text"]
    );
}

#[test]
fn a_fresh_spawn_naming_only_a_block_is_still_accepted() {
    // #544 requires a *class*, not the `kind` argument specifically: a block
    // carries its own kind (and it is authoritative over `kind`), so naming one
    // is just as deliberate. Pinned because the cheap version of this fix —
    // making `kind` mandatory outright — would break every block-based spawn
    // in a custom-workflow group, which is the whole spawn surface #222 built.
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();

    let out = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": { "block": "worker", "task": "fix #1", "branch": "feat/x" },
    })).unwrap();
    assert_eq!(out["isError"], false, "a block names the class as deliberately as kind: {out:?}");
    assert!(out["content"][0]["text"].as_str().unwrap().contains("block worker"));
}

#[test]
fn the_explicit_class_requirement_does_not_break_resume_inheritance() {
    // The #254 contract is deliberately untouched by #544: a resume that names
    // neither `kind` nor `block` is NOT an omission to be refused — it inherits
    // the resumed session's own block, which is a stricter answer than any
    // default (it re-derives nothing, and an unknown session is already a hard
    // error). Pinned here because the obvious over-broad version of this fix —
    // "refuse every spawn with no kind and no block" — silently deletes that
    // inheritance and re-roles every bare follow-up.
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();

    let spawn = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "spawn_agent", "arguments": { "kind": "reviewer", "task": "review PR #7" } }))
        .unwrap();
    assert_eq!(spawn["isError"], false, "{spawn:?}");
    let before = reg.list_agents(&g.id);
    let rev = before.as_array().unwrap().iter().find(|a| a["role"] == "reviewer").unwrap();
    let (rev_id, session, cwd) = (
        rev["id"].as_str().unwrap().to_string(),
        rev["session"].as_str().unwrap().to_string(),
        rev["cwd"].as_str().unwrap().to_string(),
    );
    reg.mark_dead(&rev_id, Some(0));

    let resumed = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": { "resume_session": session, "cwd": cwd, "task": "round-2 fix pushed" },
    })).unwrap();
    assert_eq!(
        resumed["isError"], false,
        "a bare resume must still inherit, not hit the fresh-spawn refusal: {resumed:?}"
    );
    assert!(
        resumed["content"][0]["text"].as_str().unwrap().contains("Reviewer"),
        "and it comes back a REVIEWER: {:?}", resumed["content"][0]["text"]
    );
}

#[test]
fn the_spawn_tool_schema_states_the_explicit_class_contract() {
    // The MCP tool description IS the contract every orchestrator reads — a
    // schema still advertising "default worker" would keep producing exactly
    // the omission #544 refuses, and the agent would read the refusal as a bug.
    let (reg, _d, co, _cw) = setup_mcp();
    let tools = dispatch(&reg, &co, "tools/list", &Value::Null).unwrap();
    let spawn = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "spawn_agent")
        .expect("the orchestrator must see spawn_agent")
        .clone();
    let doc = spawn["description"].as_str().unwrap().to_string();
    let kind_doc = spawn["inputSchema"]["properties"]["kind"]["description"]
        .as_str()
        .unwrap()
        .to_string();

    assert!(
        !doc.to_lowercase().contains("default worker")
            && !kind_doc.to_lowercase().contains("default worker"),
        "the schema must not still advertise a default class: {doc} / {kind_doc}"
    );
    assert!(doc.contains("#544"), "the description must cite the contract change: {doc}");
    assert!(
        doc.contains("REFUSED") || doc.contains("refused"),
        "the description must say an omitted class is refused: {doc}"
    );
    assert!(
        kind_doc.contains("REQUIRED") && kind_doc.contains("block"),
        "`kind` must be documented as required unless a block names the class: {kind_doc}"
    );
}

#[test]
fn a_bare_resume_of_an_unparseable_recorded_role_refuses_instead_of_defaulting() {
    // rev-157 NB2: this PR's SECOND "never guess a class" enforcement point,
    // and the one nothing pinned. A pre-#222 roster row records a role string
    // and no block id, so a bare resume of it has to derive the class from that
    // string — and when the string doesn't parse (a row written by a future
    // build, a hand-edited or corrupted `agents.json`), the old code fell back
    // to `kind`, which on a bare resume is the omitted-`kind` default: Worker.
    // The most-privileged class, from a value nobody supplied and nobody could
    // read.
    //
    // The failure this guards is a future one: swap the refusal back for
    // `.unwrap_or(Role::Worker)` and every other test in this suite still
    // passes, restoring the exact fail-open #544 exists to remove, on the path
    // the change describes as exempt.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();

    // A roster row shaped exactly like the pre-#222 ones the inheritance path
    // exists for — a role string, no block identity — whose role THIS build
    // cannot parse.
    let session = "7c9f2b10-2222-4222-8222-222222222222";
    let record = AgentRecord {
        id: "w-0".into(),
        role: "supervisor".into(),
        block: String::new(),
        name: "an agent from a build that isn't this one".into(),
        name_source: NameSource::default(),
        session: Some(session.to_string()),
        cwd: ".".into(),
        status: "dead".into(),
        updated_ms: 0,
        task: String::new(),
        branch: None,
        pane_kind: None,
        forked_from: None,
    };
    fs::write(
        reg.state_root().join(g.id.as_str()).join("agents.json"),
        serde_json::to_string(&[record]).unwrap(),
    )
    .unwrap();

    // The documented follow-up shape: resume_session + cwd, neither kind nor
    // block — the one call that asks loomux to derive a class on its own.
    let dir = tempfile::tempdir().unwrap();
    let out = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": {
            "resume_session": session,
            "cwd": dir.path().to_string_lossy(),
            "task": "follow-up",
        },
    })).unwrap();

    assert_eq!(
        out["isError"], true,
        "an unreadable recorded role must be refused, not resolved to the privileged \
         default: {out:?}"
    );
    let text = out["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("#544"), "the refusal must cite the guardrail, got: {text}");
    assert!(
        text.contains("supervisor"),
        "...and must name the role it could not read, so the operator can act on it: {text}"
    );
    assert!(
        reg.list_agents(&g.id).as_array().unwrap().iter().all(|a| a["role"] != "worker"),
        "and above all it must not have spawned a worker: {}",
        reg.list_agents(&g.id)
    );
}

// ---------- #190: resume_session prefix resolution ----------

/// Write a synthetic roster (`agents.json`) directly, bypassing spawn/kill —
/// the prefix-resolution tests need FULL session ids that share a chosen
/// prefix, which real (randomly-minted) session ids can't be made to do.
fn write_roster(reg: &OrchRegistry, group: &GroupId, sessions: &[&str]) {
    let records: Vec<AgentRecord> = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| AgentRecord {
            id: format!("w-{i}"),
            role: "worker".into(),
            block: "worker".into(),
            name: format!("worker {i}"),
            name_source: NameSource::default(),
            session: Some(s.to_string()),
            cwd: ".".into(),
            status: "dead".into(),
            updated_ms: 0,
            task: String::new(),
            branch: None,
            pane_kind: None,
            forked_from: None,
        })
        .collect();
    fs::write(
        reg.state_root().join(group.as_str()).join("agents.json"),
        serde_json::to_string(&records).unwrap(),
    )
    .unwrap();
}

#[test]
fn resume_session_unique_prefix_resolves_to_the_full_id() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();
    let full = "e3bc3b80-1111-4111-8111-111111111111";
    write_roster(&reg, &g.id, &[full]);
    let dir = tempfile::tempdir().unwrap();

    // Only the 8-char prefix a human would have copied/logged — issue #190's
    // exact scenario — with an explicit kind so block-inheritance isn't in play.
    let r = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": {
            "kind": "worker",
            "resume_session": "e3bc3b80",
            "cwd": dir.path().to_string_lossy(),
            "task": "follow-up",
        },
    })).unwrap();
    assert_eq!(r["isError"], false, "a unique prefix must resolve, got: {r:?}");

    // The spawned agent must be resumed under the FULL id, not the truncated
    // prefix verbatim — proof the resolution actually substituted it, since
    // `sanitize_session` alone would happily accept the bare 8-char string too.
    let agents = reg.list_agents(&co.group);
    let resumed = agents.as_array().unwrap().iter().find(|a| a["role"] == "worker").unwrap();
    assert_eq!(
        resumed["session"], json!(full),
        "resumed agent must carry the resolved FULL session id, got: {agents}"
    );
}

/// #722: a COMPLETE opencode session id this group's roster never recorded
/// must pass through, exactly as a complete claude one does. Opencode's ids
/// are 30 characters, so the length-only test that used to answer "is this a
/// full id?" called every one of them a truncated prefix — and a prefix that
/// matches no roster entry is rejected as an unknown session. The equivalent
/// claude id (36 chars) sails through, which is the asymmetry this closes.
#[test]
fn a_complete_opencode_session_id_is_not_mistaken_for_a_truncated_prefix() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();
    // A roster holding some OTHER session, so nothing here can resolve by
    // exact match or by prefix — the pass-through is the only way out.
    write_roster(&reg, &g.id, &["e3bc3b80-1111-4111-8111-111111111111"]);
    let dir = tempfile::tempdir().unwrap();
    let full_opencode = "ses_03bd2d53dffeiBvu9PvuCPjxT7"; // ses_ + 12 hex + 14 base62

    let r = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": {
            "kind": "worker",
            "resume_session": full_opencode,
            "cwd": dir.path().to_string_lossy(),
            "task": "follow-up",
        },
    })).unwrap();
    assert_eq!(
        r["isError"], false,
        "a complete opencode id must pass through, not be resolved as a prefix: {r:?}"
    );

    let agents = reg.list_agents(&co.group);
    let resumed = agents.as_array().unwrap().iter().find(|a| a["role"] == "worker").unwrap();
    assert_eq!(
        resumed["session"], json!(full_opencode),
        "the id must arrive intact, not rewritten to some roster entry: {agents}"
    );

    // And the shape check must not decay into a blanket "anything starting
    // with `ses_`". Both of these are still prefixes, and both match nothing
    // in the roster, so both must be refused rather than passed through.
    //
    // Neither shares a prefix with `full_opencode` on purpose: the spawn above
    // put that id ON the roster, so `ses_03bd2d53` would now resolve against
    // it perfectly legitimately (#190's whole feature) and would be pinning
    // test order rather than the shape rule.
    for not_full in [
        "ses_99999999",                   // truncated
        "ses_9zbd2d53dffeiBvu9PvuCPjxT7", // right length, non-hex timestamp
    ] {
        let r = dispatch(&reg, &co, "tools/call", &json!({
            "name": "spawn_agent",
            "arguments": {
                "kind": "worker",
                "resume_session": not_full,
                "cwd": dir.path().to_string_lossy(),
                "task": "follow-up",
            },
        })).unwrap();
        assert_eq!(
            r["isError"], true,
            "{not_full:?} is not a complete opencode id and must resolve as a prefix \
             (here: fail as unknown), never pass through: {r:?}"
        );
    }
}

#[test]
fn resume_session_ambiguous_prefix_fails_and_lists_candidates() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();
    let (a, b) = (
        "abc12345-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        "abc12345-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
    );
    write_roster(&reg, &g.id, &[a, b]);
    let dir = tempfile::tempdir().unwrap();

    let r = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": {
            "kind": "worker",
            "resume_session": "abc12345",
            "cwd": dir.path().to_string_lossy(),
            "task": "follow-up",
        },
    })).unwrap();
    assert_eq!(r["isError"], true, "an ambiguous prefix must never silently pick one: {r:?}");
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("ambiguous"), "error must say it's ambiguous, got: {text}");
    assert!(text.contains(a) && text.contains(b), "error must list every candidate, got: {text}");

    // And no agent must have been spawned as a side effect of the failed call.
    let agents = reg.list_agents(&co.group);
    assert!(
        agents.as_array().unwrap().iter().all(|ag| ag["role"] != "worker"),
        "an ambiguous resume must not have spawned anything: {agents}"
    );
}

#[test]
fn resume_session_unknown_prefix_is_distinguished_from_ambiguous() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();
    write_roster(&reg, &g.id, &["e3bc3b80-1111-4111-8111-111111111111"]);
    let dir = tempfile::tempdir().unwrap();

    let r = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": {
            "kind": "worker",
            "resume_session": "deadbeef",
            "cwd": dir.path().to_string_lossy(),
            "task": "follow-up",
        },
    })).unwrap();
    assert_eq!(r["isError"], true, "a never-seen prefix must fail, got: {r:?}");
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("unknown session"), "error must name it as unknown, got: {text}");
    assert!(!text.contains("ambiguous"), "unknown and ambiguous must be distinguishable, got: {text}");
}

#[test]
fn cross_group_targets_are_invisible() {
    let (reg, _d, co, _cw) = setup_mcp();
    let g2 = reg.create_group("C:/tmp/other-repo", rails()).unwrap();
    let foreign = reg.spawn_agent(&g2.id, Role::Worker, "fw", "t", false, None).unwrap();
    let r = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "kill_agent", "arguments": { "agent_id": foreign.id } })).unwrap();
    assert_eq!(r["isError"], true);
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("unknown agent"),
        "cross-group access must be indistinguishable from a nonexistent agent, got: {text}"
    );
    // And the foreign agent never appears in this group's roster.
    let roster = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "list_agents", "arguments": {} })).unwrap();
    assert!(!roster["content"][0]["text"].as_str().unwrap().contains(foreign.id.as_str()));
}

#[test]
fn unknown_method_and_tool_are_rejected() {
    let (reg, _d, co, _cw) = setup_mcp();
    let err = dispatch(&reg, &co, "resources/list", &Value::Null).unwrap_err();
    assert_eq!(err.0, -32601);
    let r = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "no_such_tool", "arguments": {} })).unwrap();
    assert_eq!(r["isError"], true);
}

#[test]
fn report_validates_status_and_role() {
    let (reg, _d, co, cw) = setup_mcp();
    let bad = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "report", "arguments": { "status": "finished", "summary": "x" } })).unwrap();
    assert_eq!(bad["isError"], true, "invalid status must be rejected");
    let from_orch = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "report", "arguments": { "status": "done", "summary": "x" } })).unwrap();
    assert_eq!(from_orch["isError"], true, "orchestrator has no one to report to");
    // A valid worker report fails only at PTY delivery in test mode (no
    // panes), which proves routing reached the orchestrator lookup.
    let ok_path = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "report", "arguments": { "status": "done", "summary": "PR #1 open" } })).unwrap();
    let text = ok_path["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("terminal") || text.contains("reported"),
        "report must route to the orchestrator's pane, got: {text}"
    );
}
