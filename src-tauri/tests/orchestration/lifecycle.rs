//! Session digests, group lifecycle, worktree-leak fixtures, compose-strip steering and attachments.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ---------- session_digest (#250/#324 slice B, gate tightened in slice D) ----------
//
// Slice B shipped an interim worker-kind-wide gate (role_hint hadn't landed
// yet). Slice D's binding rider tightens it to `role_hint == process` — these
// tests spawn their process-pro caller against a declared `process`-hinted
// block via `process_caller`/`rails_with_process_block`, not a plain worker.

/// The default 4-block roster plus one extra `proc` block, `kind: worker`
/// with `role_hint: process` — the process-pro's own identity, distinct from
/// the plain `worker` block. Mirrors how `rails()` builds the default roster.
fn rails_with_process_block() -> Guardrails {
    let mut g = rails();
    g.blocks.push(workflow::Block {
        id: "proc".into(),
        name: "process-pro".into(),
        kind: Role::Worker,
        cli: String::new(),
        model: String::new(),
        prompt: None,
        profile: None,
        allow: vec![],
        role_hint: Some("process".into()),
        effort: String::new(),
        context: String::new(),
        remote: None,
        driver: None,
        cache_ttl_minutes: None,
    });
    g
}

/// Spawn an agent against the `proc` (`role_hint: process`) block declared by
/// [`rails_with_process_block`] and return its MCP caller.
fn process_caller(reg: &OrchRegistry, group: &GroupId) -> Caller {
    let a = reg
        .spawn_agent_ex(group, Role::Worker, Some("proc".into()), "proc", "",
                        false, None, None, None, None, None)
        .unwrap();
    reg.resolve_token(&a.token).unwrap()
}

#[test]
fn session_digest_returns_friction_windows_for_a_finished_worker() {
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let g = reg.create_group("C:/tmp/repo", rails_with_process_block()).unwrap();
    let worker = reg.spawn_agent(&g.id, Role::Worker, "w", "fix the flaky test", false, None).unwrap();
    let sid = worker.session_id.clone().unwrap();

    let encoded = proj.path().join("C--tmp-repo");
    fs::create_dir_all(&encoded).unwrap();
    let transcript = format!(
        "{}\n{}\n{}\n{}\n{}\n",
        json!({"type":"user","timestamp":"2026-07-15T10:00:00.000Z",
            "message":{"role":"user","content":"fix the flaky test"}}),
        json!({"type":"assistant","timestamp":"2026-07-15T10:00:01.000Z","message":{"role":"assistant",
            "content":[{"type":"tool_use","id":"c1","name":"Bash","input":{"command":"npm test"}}]}}),
        json!({"type":"user","timestamp":"2026-07-15T10:00:02.000Z","message":{"role":"user",
            "content":[{"type":"tool_result","tool_use_id":"c1","is_error":true,"content":"npm: command not found"}]}}),
        json!({"type":"assistant","timestamp":"2026-07-15T10:00:03.000Z","message":{"role":"assistant",
            "content":[{"type":"tool_use","id":"c2","name":"Bash","input":{"command":"pnpm test"}}]}}),
        json!({"type":"user","timestamp":"2026-07-15T10:00:04.000Z","message":{"role":"user",
            "content":[{"type":"tool_result","tool_use_id":"c2","is_error":false,"content":"1 passing"}]}}),
    );
    fs::write(encoded.join(format!("{sid}.jsonl")), transcript).unwrap();

    // A separate process-hinted worker plays the process-pro reading it cold,
    // after the worker that did the work is reaped — mirroring "spawned at
    // merge-gate resolution" (#324): session_digest must still find a dead
    // agent's recorded session, not just a live one.
    let cp = process_caller(&reg, &g.id);
    reg.mark_dead(&worker.id, Some(0));

    // The listing agrees with the dispatch gate: a process-hinted worker DOES
    // see the tool (the plain-worker negative case is pinned separately by
    // `session_digest_denied_to_a_plain_worker_without_the_process_hint`).
    let tools = dispatch(&reg, &cp, "tools/list", &json!({})).unwrap();
    let names: Vec<&str> =
        tools["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"session_digest"), "a process-hinted worker must see the tool: {names:?}");

    let r = dispatch(&reg, &cp, "tools/call",
        &json!({ "name": "session_digest", "arguments": { "agent": worker.id } })).unwrap();
    assert_eq!(r["isError"], false, "{r}");
    let digest: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(digest["initial_prompt"], "fix the flaky test");
    let windows = digest["windows"].as_array().unwrap();
    assert!(!windows.is_empty(), "expected at least one friction window, got {digest}");
    assert!(
        windows.iter().any(|w| w["signature"] == "tool_error"),
        "expected a tool_error window, got {digest}"
    );
}

#[test]
fn session_digest_resolves_via_task_id_and_via_pr_number() {
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let g = reg.create_group("C:/tmp/repo", rails_with_process_block()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();
    let worker = reg.spawn_agent(&g.id, Role::Worker, "w", "fix the flaky test", false, None).unwrap();
    let sid = worker.session_id.clone().unwrap();

    let encoded = proj.path().join("C--tmp-repo");
    fs::create_dir_all(&encoded).unwrap();
    fs::write(
        encoded.join(format!("{sid}.jsonl")),
        format!(
            "{}\n",
            json!({"type":"user","timestamp":"2026-07-15T10:00:00.000Z",
                "message":{"role":"user","content":"fix the flaky test"}}),
        ),
    )
    .unwrap();

    // The orchestrator files the task the way it would once the worker
    // reports done and opens a PR.
    let up = dispatch(&reg, &co, "tools/call", &json!({
        "name": "upsert_task",
        "arguments": {
            "title": "flaky test", "status": "pr", "pr": "#42",
            "assignee": worker.id, "session": sid,
        },
    })).unwrap();
    assert_eq!(up["isError"], false, "{up}");
    let task_id = reg.task_summaries(&g.id)[0].id.clone();

    let cp = process_caller(&reg, &g.id);

    let by_task = dispatch(&reg, &cp, "tools/call",
        &json!({ "name": "session_digest", "arguments": { "task": task_id } })).unwrap();
    assert_eq!(by_task["isError"], false, "{by_task}");
    let digest: Value = serde_json::from_str(by_task["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(digest["initial_prompt"], "fix the flaky test");
    assert_eq!(digest["final_diff_ref"], "#42");
    assert_eq!(digest["outcome"], "pr");

    // `pr` is sugar for looking up the same task by its PR number.
    let by_pr = dispatch(&reg, &cp, "tools/call",
        &json!({ "name": "session_digest", "arguments": { "pr": "42" } })).unwrap();
    assert_eq!(by_pr["isError"], false, "{by_pr}");
    let digest2: Value = serde_json::from_str(by_pr["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(digest2["initial_prompt"], "fix the flaky test");
}

#[test]
fn session_digest_denied_to_non_worker_roles() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let reviewer = reg.spawn_agent(&g.id, Role::Reviewer, "rev", "", false, None).unwrap();
    let planner = reg.spawn_agent(&g.id, Role::Planner, "pln", "", false, None).unwrap();
    for entry in [&orch, &reviewer, &planner] {
        let caller = reg.resolve_token(&entry.token).unwrap();
        let r = dispatch(&reg, &caller, "tools/call",
            &json!({ "name": "session_digest", "arguments": { "agent": "whatever" } })).unwrap();
        assert_eq!(r["isError"], true, "{:?} must be denied session_digest", entry.role);
    }
}

#[test]
fn session_digest_denied_to_a_plain_worker_without_the_process_hint() {
    // The slice D binding rider: slice B's interim gate was worker-kind-wide;
    // this tightens it to `role_hint == process` now that role_hint (slice A)
    // has landed on this branch. A plain `worker` block — no hint — is a
    // worker like any other, and must be refused exactly like a
    // reviewer/planner/orchestrator, not waved through on kind alone.
    let (reg, _d, _co, cw) = setup_mcp();
    let r = dispatch(&reg, &cw, "tools/call",
        &json!({ "name": "session_digest", "arguments": { "agent": "whatever" } })).unwrap();
    assert_eq!(r["isError"], true, "a plain worker with no role_hint must be denied session_digest");
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("process"), "the refusal should name the required hint: {text}");
    // The listing agrees with the dispatch gate: a plain worker never even
    // sees the tool (cosmetic, but must not disagree with the real check).
    let tools = dispatch(&reg, &cw, "tools/list", &json!({})).unwrap();
    let names: Vec<&str> =
        tools["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(!names.contains(&"session_digest"), "a plain worker must not even see the tool: {names:?}");
}

#[test]
fn session_digest_cross_group_agent_is_unknown_not_leaked() {
    let (reg, _d) = test_registry();
    let g1 = reg.create_group("C:/tmp/repo1", rails_with_process_block()).unwrap();
    let g2 = reg.create_group("C:/tmp/repo2", rails()).unwrap();
    let w2 = reg.spawn_agent(&g2.id, Role::Worker, "w", "task", false, None).unwrap();
    let c1 = process_caller(&reg, &g1.id);

    let r = dispatch(&reg, &c1, "tools/call",
        &json!({ "name": "session_digest", "arguments": { "agent": w2.id.clone() } })).unwrap();
    assert_eq!(r["isError"], true);
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("unknown agent"));
    assert!(!text.contains(g2.id.as_str()), "must not leak the foreign group's id: {text}");
}

#[test]
fn session_digest_requires_exactly_one_identifier() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails_with_process_block()).unwrap();
    let cp = process_caller(&reg, &g.id);
    let none = dispatch(&reg, &cp, "tools/call",
        &json!({ "name": "session_digest", "arguments": {} })).unwrap();
    assert_eq!(none["isError"], true);
    assert!(none["content"][0]["text"].as_str().unwrap().contains("exactly one"));

    let both = dispatch(&reg, &cp, "tools/call",
        &json!({ "name": "session_digest", "arguments": { "agent": "w-1", "pr": "1" } })).unwrap();
    assert_eq!(both["isError"], true);
    assert!(both["content"][0]["text"].as_str().unwrap().contains("exactly one"));
}

/// A wrongly-TYPED identifier must say so, not report itself as a missing one
/// (#642's `arg_bool`/`arg_str_array` rule, applied to this arm on rebase).
/// `pr` is the realistic case: the tool's own description invites "PR number,
/// #n, or URL", so a bare JSON number is the natural thing to send — and
/// through plain `arg_str` it reads as absent, answering a call that DID pass
/// exactly one identifier with "exactly one … is required". That message
/// sends the caller hunting for a bug in its call shape instead of its arg
/// type, which is the silent-skip failure #582 legislated against.
#[test]
fn session_digest_rejects_a_wrongly_typed_identifier_instead_of_reading_it_as_absent() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails_with_process_block()).unwrap();
    let cp = process_caller(&reg, &g.id);

    let numeric = dispatch(&reg, &cp, "tools/call",
        &json!({ "name": "session_digest", "arguments": { "pr": 646 } })).unwrap();
    assert_eq!(numeric["isError"], true);
    let msg = numeric["content"][0]["text"].as_str().unwrap();
    assert!(msg.contains("pr must be a string"), "must name the type problem, got: {msg}");
    assert!(!msg.contains("exactly one"), "must NOT report a supplied identifier as missing: {msg}");

    // Explicit null keeps meaning "not supplied" — otherwise a caller that
    // spells an absent optional as `null` could never call this tool at all.
    let nulls = dispatch(&reg, &cp, "tools/call",
        &json!({ "name": "session_digest", "arguments": { "task": "t-1", "agent": null, "pr": null } })).unwrap();
    let nulls_msg = nulls["content"][0]["text"].as_str().unwrap();
    assert!(!nulls_msg.contains("must be a string"), "null is absent, not a type error: {nulls_msg}");
    assert!(!nulls_msg.contains("exactly one"), "one identifier WAS supplied: {nulls_msg}");
}

/// Review finding NB4: `Task.session` is agent-settable via `upsert_task`,
/// and it reaches a filesystem path join — a `..`/separator-laden value must
/// be rejected before that join, end to end through the real MCP tool, not
/// just at the pure `is_safe_session_id` unit level.
#[test]
fn session_digest_rejects_a_path_traversal_session_id() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails_with_process_block()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();
    let worker = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();

    let up = dispatch(&reg, &co, "tools/call", &json!({
        "name": "upsert_task",
        "arguments": { "title": "t", "assignee": worker.id, "session": "../../../../etc/passwd" },
    })).unwrap();
    assert_eq!(up["isError"], false, "{up}");
    let task_id = reg.task_summaries(&g.id)[0].id.clone();

    let cp = process_caller(&reg, &g.id);
    let r = dispatch(&reg, &cp, "tools/call",
        &json!({ "name": "session_digest", "arguments": { "task": task_id } })).unwrap();
    assert_eq!(r["isError"], true);
    assert!(r["content"][0]["text"].as_str().unwrap().contains("invalid session id"), "{r}");
}

/// **The shapes the old predicate let through, refused end to end** (#925 N8).
///
/// The sibling test above pins the separator/traversal case, which
/// `digest::is_safe_session_id` already caught. This one pins the shapes it did
/// **not**: it was `!empty && !contains(['/','\\']) && != "." && != ".."`, so a
/// Windows drive prefix, a device name, a leading dash, an unbounded length, a
/// NUL and every non-ASCII byte all satisfied it and travelled on to
/// `Path::join`.
///
/// # What "refused" is asserted as, and why it is not just `isError`
///
/// Both before and after the fix these return an error, so `isError` alone
/// cannot tell the two apart — before, the id reached the filesystem and the
/// error was the *lookup* failing ("no Claude transcript found for session …");
/// after, it is refused at the gate and never reaches a path at all. The
/// message is therefore the observable that distinguishes "never became a path"
/// from "became a path that happened to miss", which is exactly the property
/// #925 is about. Same idiom as the sibling test above.
#[test]
fn session_digest_refuses_every_id_shape_the_old_predicate_admitted() {
    let long = "a".repeat(65);
    let hostile: &[(&str, &str)] = &[
        // The sharpest one: on Windows a `Prefix` component makes `Path::join`
        // REPLACE its receiver, so this walked out of the session-state root
        // with no separator anywhere in it.
        ("C:", "a Windows drive prefix"),
        ("C:stream", "a drive-relative path"),
        ("sess:ads", "an NTFS alternate data stream"),
        // Opens a device rather than naming a file.
        ("CON", "a reserved device name"),
        ("nul", "a reserved device name, lowercased"),
        // An option to any command line the id is interpolated into.
        ("-rf", "a leading dash"),
        // The old predicate had no length cap at all.
        (long.as_str(), "an over-length id"),
        // Truncates at the syscall.
        ("sess\0evil", "an embedded NUL"),
        // Where normalization and homoglyph confusion live.
        ("sessiön", "a non-ASCII byte"),
        // Still refused, and still by the alphabet rather than a special case.
        ("..", "the bare traversal"),
        // The empty id is deliberately NOT here, and finding out why was worth
        // the round: `upsert_task` records `session: ""` as *no session at
        // all*, so `session_digest` refuses it upstream with "task … has no
        // recorded session" and it never reaches the path layer to be refused
        // by the gate this test is about. Asserting the gate's message for it
        // would have been asserting the wrong guard. `SegmentError::Empty` is
        // pinned directly in `tests/pathseg.rs` instead.
    ];

    for (bad, why) in hostile {
        let (reg, _d) = test_registry();
        let g = reg.create_group("C:/tmp/repo", rails_with_process_block()).unwrap();
        let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
        let co = reg.resolve_token(&orch.token).unwrap();
        let worker = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();

        let up = dispatch(&reg, &co, "tools/call", &json!({
            "name": "upsert_task",
            "arguments": { "title": "t", "assignee": worker.id, "session": bad },
        })).unwrap();
        assert_eq!(up["isError"], false, "upsert_task should store {why} verbatim: {up}");
        let task_id = reg.task_summaries(&g.id)[0].id.clone();

        let cp = process_caller(&reg, &g.id);
        let r = dispatch(&reg, &cp, "tools/call",
            &json!({ "name": "session_digest", "arguments": { "task": task_id } })).unwrap();

        assert_eq!(r["isError"], true, "{why} ({bad:?}) must be refused: {r}");
        let msg = r["content"][0]["text"].as_str().unwrap();
        assert!(
            msg.contains("invalid session id"),
            "{why} ({bad:?}) must be refused AT THE GATE, not looked up on disk — got: {msg}"
        );
    }
}

// ---------- session_digest recurrence (#324) ----------
//
// The demo (#358) answered "what went wrong in THIS session". The
// process-pro's actual filter — "would a fresh worker on a different task in
// this repo hit the same wall?" — is a question about OTHER sessions, and
// with n=1 it could only ever be answered from the agent's own impression of
// how hard the session looked. That is the self-assessment bias #324's issue
// body was written to design out. These tests pin the mechanical answer.

/// Write a Claude transcript for `sid` under the fake projects root, with one
/// tool error carrying `err` — the "wall" a session hit.
fn write_wall_transcript(proj: &std::path::Path, sid: &str, err: &str) {
    let encoded = proj.join("C--tmp-repo");
    fs::create_dir_all(&encoded).unwrap();
    let transcript = format!(
        "{}\n{}\n{}\n",
        json!({"type":"user","timestamp":"2026-07-15T10:00:00.000Z",
            "message":{"role":"user","content":"do the thing"}}),
        json!({"type":"assistant","timestamp":"2026-07-15T10:00:01.000Z","message":{"role":"assistant",
            "content":[{"type":"tool_use","id":"c1","name":"Bash","input":{"command":"cargo check"}}]}}),
        json!({"type":"user","timestamp":"2026-07-15T10:00:02.000Z","message":{"role":"user",
            "content":[{"type":"tool_result","tool_use_id":"c1","is_error":true,"content":err}]}}),
    );
    fs::write(encoded.join(format!("{sid}.jsonl")), transcript).unwrap();
}

/// Spawn a worker, record a session that hit `err`, and reap it — the state
/// the process-pro actually reads. Reaping is not test-scaffolding
/// convenience: `session_digest` exists to read sessions after the worker
/// that produced them is gone, and the live-agent guardrail (`rails()` caps
/// at 2) would otherwise make a multi-session fixture impossible to build at
/// all.
fn finished_session(reg: &OrchRegistry, g: &GroupId, proj: &std::path::Path, name: &str, err: &str) -> String {
    let w = reg.spawn_agent(g, Role::Worker, name, "task", false, None).unwrap();
    write_wall_transcript(proj, w.session_id.as_ref().unwrap(), err);
    reg.mark_dead(&w.id, Some(0));
    w.id
}

fn digest_for_agent(reg: &OrchRegistry, cp: &Caller, agent: &str) -> Value {
    let r = dispatch(reg, cp, "tools/call",
        &json!({ "name": "session_digest", "arguments": { "agent": agent } })).unwrap();
    assert_eq!(r["isError"], false, "{r}");
    serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap()
}

/// The core of #324: a second session that hit the SAME wall turns a window
/// from "one worker struggled" into evidence a fresh worker would hit it too
/// — and it names which session, so the claim is checkable.
#[test]
fn session_digest_corroborates_a_wall_a_second_session_also_hit() {
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let g = reg.create_group("C:/tmp/repo", rails_with_process_block()).unwrap();

    // Two workers, two different worktrees, the SAME wall — the error text
    // even carries each one's own absolute path, exactly as a real pair of
    // transcripts would, which is what the key normalization has to survive.
    let a = finished_session(&reg, &g.id, proj.path(), "wa",
        "error[E0433]: failed to resolve at C:/wt/feat-a/src/lib.rs:12:5");
    let b = finished_session(&reg, &g.id, proj.path(), "wb",
        "error[E0433]: failed to resolve at C:/wt/feat-b/src/lib.rs:87:9");

    let cp = process_caller(&reg, &g.id);
    let digest = digest_for_agent(&reg, &cp, &a);
    let w = digest["windows"].as_array().unwrap().iter().find(|w| w["signature"] == "tool_error")
        .unwrap_or_else(|| panic!("expected a tool_error window: {digest}"));
    assert_eq!(w["recurrence"], 1, "the other worker hit the same wall: {digest}");
    assert_eq!(w["corroborated_by"], json!([b]), "and it must say WHICH session: {digest}");
    assert_eq!(digest["sessions_scanned"], 1, "{digest}");
    assert_eq!(digest["corroboration_capped"], false, "{digest}");
}

/// The negative that gives the positive its meaning: a wall nobody else hit
/// must come back 0, even though another session WAS read. A digest that
/// corroborated everything would be worth nothing.
#[test]
fn session_digest_reports_a_one_off_as_a_one_off() {
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let g = reg.create_group("C:/tmp/repo", rails_with_process_block()).unwrap();

    let a = finished_session(&reg, &g.id, proj.path(), "wa", "error[E0433]: failed to resolve");
    finished_session(&reg, &g.id, proj.path(), "wb", "error[E0599]: no method named foo");

    let cp = process_caller(&reg, &g.id);
    let digest = digest_for_agent(&reg, &cp, &a);
    let w = digest["windows"].as_array().unwrap().iter().find(|w| w["signature"] == "tool_error").unwrap();
    assert_eq!(w["recurrence"], 0, "a different error is a different wall: {digest}");
    assert_eq!(w["corroborated_by"], json!([]), "{digest}");
    // …and the reader can tell this apart from "nothing to compare against":
    // a session WAS scanned, it just didn't match.
    assert_eq!(digest["sessions_scanned"], 1, "{digest}");
}

/// A session is not evidence about itself. An agent that rejoined its own
/// session has two roster rows carrying one session id, and counting the
/// second row would let a session corroborate itself — a number that only
/// ever says "yes" is not a filter.
#[test]
fn session_digest_never_corroborates_a_session_with_itself() {
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let g = reg.create_group("C:/tmp/repo", rails_with_process_block()).unwrap();

    let a = reg.spawn_agent(&g.id, Role::Worker, "wa", "task a", false, None).unwrap();
    let sid = a.session_id.clone().unwrap();
    write_wall_transcript(proj.path(), &sid, "error[E0433]: failed to resolve");
    reg.mark_dead(&a.id, Some(0));
    // A SECOND roster row pointing at the same session — what a resume/rejoin
    // records. It is the same evidence read twice, not a second opinion, so
    // `session_digest` must still see zero OTHER sessions.
    let b = reg
        .spawn_agent_ex(&g.id, Role::Worker, None, "wb", "task b", false, None, None, Some(sid.clone()), None, None)
        .unwrap();
    assert_eq!(b.session_id.as_deref(), Some(sid.as_str()), "the resumed row must carry the same session id");
    reg.mark_dead(&b.id, Some(0));

    let cp = process_caller(&reg, &g.id);
    let digest = digest_for_agent(&reg, &cp, &a.id);
    let w = digest["windows"].as_array().unwrap().iter().find(|w| w["signature"] == "tool_error").unwrap();
    assert_eq!(w["recurrence"], 0, "the target's own session must never corroborate it: {digest}");
    assert_eq!(digest["sessions_scanned"], 0, "{digest}");
}

/// Recurrence is derived on read from a CAPPED scan, so a count is a floor,
/// not a total — and the digest has to say so, or a capped scan reads as an
/// exhaustive one (the same reason `dropped_windows` exists).
#[test]
fn session_digest_reports_when_the_corroboration_scan_was_capped() {
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let g = reg.create_group("C:/tmp/repo", rails_with_process_block()).unwrap();

    let target = finished_session(&reg, &g.id, proj.path(), "wt", "error[E0433]: failed to resolve");
    // Comfortably more other sessions than the scan reads, all hitting the
    // same wall — so a scan that read them all would report every one.
    for i in 0..12 {
        finished_session(&reg, &g.id, proj.path(), &format!("wo{i}"), "error[E0433]: failed to resolve");
    }

    let cp = process_caller(&reg, &g.id);
    let digest = digest_for_agent(&reg, &cp, &target);
    assert_eq!(digest["corroboration_capped"], true, "{digest}");
    let scanned = digest["sessions_scanned"].as_u64().unwrap();
    assert!(scanned > 0 && scanned < 13, "a capped scan reads some, not all: {digest}");
    let w = digest["windows"].as_array().unwrap().iter().find(|w| w["signature"] == "tool_error").unwrap();
    assert_eq!(w["recurrence"].as_u64().unwrap(), scanned, "every scanned session hit the same wall: {digest}");
}

#[test]
fn usage_json_write_is_atomic_and_leaves_no_temp() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.upsert_usage_snapshot(&g.id, usage_snap("sess-a", "w-1", 0.50, 100, 200));

    let gdir = reg.state_root().join(g.id.as_str());
    let path = gdir.join("usage.json");
    assert!(path.is_file(), "usage.json must exist after upsert");
    // The atomic write renames its temp into place, so no `.tmp` scratch sibling
    // is left behind (the temp name now carries a pid/seq suffix, so match the
    // extension rather than a fixed name).
    let leftover_tmp = fs::read_dir(&gdir)
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| e.path().extension().is_some_and(|x| x == "tmp"));
    assert!(!leftover_tmp, "temp file must be cleaned up");
    // Round-trips as valid JSON with the snapshot.
    let list: Vec<UsageSnapshot> =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].key, "sess-a");
}

#[test]
fn corrupt_usage_json_is_preserved_not_silently_wiped() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let gdir = reg.state_root().join(g.id.as_str());
    fs::create_dir_all(&gdir).unwrap();
    let path = gdir.join("usage.json");
    // Simulate a half-written / hand-mangled file.
    fs::write(&path, "{ this is not valid json ").unwrap();

    // The next upsert must not treat corruption as "empty and overwrite" — it
    // preserves the bad file so no killed-agent history is silently lost.
    reg.upsert_usage_snapshot(&g.id, usage_snap("sess-b", "w-2", 1.25, 300, 400));

    let bad = gdir.join("usage.json.bad");
    assert!(bad.is_file(), "corrupt file must be preserved as usage.json.bad");
    assert_eq!(fs::read_to_string(&bad).unwrap(), "{ this is not valid json ");
    // usage.json is now valid and holds the new snapshot.
    let list: Vec<UsageSnapshot> =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].key, "sess-b");
}

#[test]
fn mixed_estimated_and_reported_totals_are_labelled_mixed() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    // One transcript-estimated snapshot and one statusline-reported one.
    reg.upsert_usage_snapshot(&g.id, usage_snap("sess-est", "w-1", 1.00, 100, 100));
    let mut reported = usage_snap("sess-rep", "w-2", 2.00, 0, 0);
    reported.source = "statusline".to_string();
    reported.estimated = false;
    reg.upsert_usage_snapshot(&g.id, reported);

    let usage = reg.group_usage(&g.id);
    // Neither agent is live (no panes), so this exercises the lifetime total.
    assert!((usage["lifetime_cost_usd"].as_f64().unwrap() - 3.00).abs() < 1e-9);
    assert_eq!(usage["lifetime_cost_basis"], "mixed",
        "estimated + reported dollars must not hide under one label");
}

#[test]
fn group_json_records_cost_guardrails() {
    let (reg, _d) = test_registry();
    let mut rails = costed_rails(15, 30);
    rails.watchdog_stall_minutes = 12;
    let g = reg.create_group("C:/tmp/repo", rails).unwrap();
    let gj: Value = serde_json::from_str(
        &fs::read_to_string(reg.state_root().join(g.id.as_str()).join("group.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(gj["guardrails"]["idle_kill_minutes"], 15);
    assert_eq!(gj["guardrails"]["max_spawns_per_hour"], 30);
    assert_eq!(gj["guardrails"]["watchdog_stall_minutes"], 12);
}

// ---------- group lifecycle: summary & end-orchestration (#8) ----------

#[test]
fn worktree_cleanup_targets_dedupes_and_spares_the_repo_root() {
    let repo = "C:/Projects/loomux";
    let cwds = vec![
        // The orchestrator's cwd == the repo root, in a different spelling —
        // must never be a removal target (it's the user's real checkout).
        r"C:\Projects\loomux".to_string(),
        // Two workers sharing one worktree (resume reuses cwd) → one target.
        "C:/Projects/loomux-worktrees/a".to_string(),
        "C:/Projects/loomux-worktrees/a/".to_string(),
        "C:/Projects/loomux-worktrees/b".to_string(),
        "".to_string(), // an unbound agent with no cwd is skipped
    ];
    let targets = worktree_cleanup_targets(repo, &cwds);
    assert_eq!(
        targets,
        vec![
            "C:/Projects/loomux-worktrees/a".to_string(),
            "C:/Projects/loomux-worktrees/b".to_string(),
        ],
        "repo root excluded, case/separator/trailing-slash duplicates collapsed"
    );
}

#[test]
fn group_summary_counts_live_agents_roles_and_uptime() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    reg.spawn_agent(&g.id, Role::Worker, "w1", "do a thing", false, None).unwrap();
    let dead = reg.spawn_agent(&g.id, Role::Worker, "w2", "", false, None).unwrap();
    // A dead agent must drop out of the live count and role breakdown.
    reg.mark_dead(&dead.id, Some(0));

    let s = reg.group_summary(&g.id);
    assert_eq!(s["live_agents"], 2);
    assert_eq!(s["roles"]["orchestrator"], 1);
    assert_eq!(s["roles"]["worker"], 1);
    assert_eq!(s["roles"]["reviewer"], 0);
    assert_eq!(s["paused"], false);
    // Uptime is present (measured from the earliest live agent) and every live
    // agent carries its own uptime; the dead one is gone.
    assert!(s["uptime_ms"].as_u64().is_some(), "group uptime must be reported");
    let agents = s["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 2);
    assert!(agents.iter().all(|a| a["uptime_ms"].as_u64().is_some()));
    assert!(!agents.iter().any(|a| a["id"] == dead.id.as_str()));
    // Lifecycle-panel surfacing (round 6): an agent with no compact-nudge
    // activity yet reads as idle on both new fields, not absent/null-shaped
    // in some other way the frontend would have to special-case.
    assert!(agents.iter().all(|a| a["compaction"]["status"] == "none"));
    assert!(agents.iter().all(|a| a["context"]["tokens"].is_null() && a["context"]["percent"].is_null()));
    // Pausing the group is reflected so the panel can compose the two actions.
    reg.pause_group(&g.id).unwrap();
    assert_eq!(reg.group_summary(&g.id)["paused"], true);
}

#[test]
fn group_summary_surfaces_live_compaction_state_and_cached_context_usage() {
    // Lifecycle-panel surfacing (PR #329 round 6): the payload the panel
    // actually polls must reflect the REAL, live state machine — not just
    // the pure function in isolation.
    let (reg, _d, gid, oid) = compact_nudge_setup(5);
    let empty = HashMap::new();
    let tokens: HashMap<String, u64> = [(oid.clone(), 40_000u64)].into_iter().collect();
    // Fire: arms the trusted (loomux-initiated) path, and caches the context
    // reading from the SAME tick's `context_tokens` map — no extra read.
    let _ = reg.compact_nudge_tick(FAR, &empty, &HashMap::new(), &HashMap::new(), &tokens, &HashMap::new(), &HashMap::new());
    let s = reg.group_summary(&gid);
    let a = s["agents"].as_array().unwrap().iter().find(|a| a["id"] == oid.as_str()).unwrap();
    assert_eq!(a["compaction"]["status"], "armed");
    assert_eq!(a["compaction"]["trusted"], true);
    assert_eq!(a["context"]["tokens"], 40_000);
    assert!(a["context"]["percent"].as_u64().is_some(), "percent derived from the cached token reading");

    // Busy then quiet: the trusted arm confirms on busy-then-quiet alone
    // (rev-42) — moves into the reinjecting phase, not discarded.
    let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64)].into_iter().collect();
    let _ = reg.compact_nudge_tick(FAR + 1_000, &grew, &HashMap::new(), &HashMap::new(), &tokens, &HashMap::new(), &HashMap::new());
    let _ = reg.compact_nudge_tick(FAR + 2_000, &grew, &HashMap::new(), &HashMap::new(), &tokens, &HashMap::new(), &HashMap::new());
    let s2 = reg.group_summary(&gid);
    let a2 = s2["agents"].as_array().unwrap().iter().find(|a| a["id"] == oid.as_str()).unwrap();
    assert_eq!(a2["compaction"]["status"], "reinjecting");
    assert_eq!(a2["compaction"]["attempt"], 1);
    assert_eq!(a2["compaction"]["max_attempts"], 3);

    // Delivery confirms: the in-flight phase is gone — no lingering `armed`,
    // `awaiting_evidence` or `reinjecting`. #546: what remains for the recency
    // window is the RESOLUTION, carrying which evidence closed it. Here that is
    // our own submit sampler, so the panel says `delivery` — the stronger of
    // the two claims, and the one a reader must be able to tell apart from a
    // resolution that only ever saw the agent stay alive.
    //
    // This is the only test that asserts the SERIALIZED shape end to end, so it
    // is where the frontend's `CompactionStatus` union is actually pinned: a
    // tag or field renamed on one side and not the other is a badge that
    // silently stops rendering, and nothing else in either suite would catch it.
    let confirmed = confirmed_delivery(&oid, FAR + 2_000);
    let _ = reg.compact_nudge_tick(FAR + 3_000, &grew, &HashMap::new(), &HashMap::new(), &tokens, &HashMap::new(), &confirmed);
    let s3 = reg.group_summary(&gid);
    let a3 = s3["agents"].as_array().unwrap().iter().find(|a| a["id"] == oid.as_str()).unwrap();
    assert_eq!(a3["compaction"]["status"], "resolved", "resolved — the phase is closed, not in flight");
    assert_eq!(a3["compaction"]["evidence"], "delivery", "and the panel says WHICH evidence closed it");
    assert!(a3["compaction"]["since_ms"].as_u64().is_some(), "stamped, so the surfacing ages out");
    assert!(a3["compaction"]["attempt"].is_null(), "no in-flight attempt survives the resolution");
    // #546: the wire tag was `"acked"`, and that word is the overclaim this
    // issue is named after — on the liveness arm the agent acknowledged
    // nothing. Renaming only the rendered label would have left the assertion
    // living in the payload a human can also read (audit viewer, saved
    // queries), so the tag had to move too, and this pins that it did.
    assert_ne!(a3["compaction"]["status"], "acked", "the overclaiming tag must be gone from the wire, not just the label");
    assert!(a3["compaction"]["source"].is_null(), "renamed to `evidence` — a stale `source` alongside it would let both spellings drift");
}

#[test]
fn run_compact_nudge_reads_a_model_aware_window_from_the_real_transcript() {
    // PR #329 round 7, live demo evidence: the lifecycle gauge showed 26%
    // for a token count the CLI's own `/context` reported as ~5% — loomux
    // was dividing by a flat 200K window while the agent ran Opus (1M-tier
    // in the reporting deployment). This drives the REAL impure path
    // (`run_compact_nudge`, a real fixture transcript) end to end: both
    // `group_summary`'s displayed percent AND the escalation threshold that
    // shares the same denominator.
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    // Escalation threshold at 50%: under the OLD flat-200K assumption,
    // 150_000 tokens reads as 75% and would escalate immediately (wrongly
    // early, by ~5x, for a real 1M-context session). Under the model-aware
    // fix, 150_000 / 1_000_000 = 15% — nowhere near the threshold.
    let rails = Guardrails { compact_context_threshold_percent: 50, ..compact_rails(0, &["orchestrator"]) };
    let g = reg.create_group("C:/tmp/repo", rails).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let sid = o.session_id.clone().unwrap();

    let encoded = proj.path().join("C--tmp-repo");
    fs::create_dir_all(&encoded).unwrap();
    let transcript = format!(
        "{}\n",
        json!({"type":"assistant","message":{"id":"m1","model":"claude-opus-4-8",
            "usage":{"input_tokens":150000,"output_tokens":500,
                     "cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}),
    );
    fs::write(encoded.join(format!("{sid}.jsonl")), transcript).unwrap();

    let _ = reg.run_compact_nudge(1);
    assert_eq!(audit_count(&reg, &g.id, "compact-escalation"), 0,
        "must NOT escalate — 15% of the real (1M-tier) window is nowhere near the 50% threshold");

    let s = reg.group_summary(&g.id);
    let a = s["agents"].as_array().unwrap().iter().find(|a| a["id"] == o.id.as_str()).unwrap();
    assert_eq!(a["context"]["tokens"], 150_000);
    assert_eq!(a["context"]["percent"], 15, "the lifecycle panel must show the SAME model-aware percent, not the old flat-200K ~75%");
}

#[test]
fn run_compact_nudge_honors_an_explicit_context_window_override() {
    // The escape hatch: an explicit human override wins outright over the
    // model-based guess, for a deployment where the guess would be wrong.
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let rails = Guardrails {
        compact_context_threshold_percent: 50,
        context_window_tokens_override: Some(200_000), // this deployment's Opus is NOT 1M-tier
        ..compact_rails(0, &["orchestrator"])
    };
    let g = reg.create_group("C:/tmp/repo", rails).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let sid = o.session_id.clone().unwrap();

    let encoded = proj.path().join("C--tmp-repo");
    fs::create_dir_all(&encoded).unwrap();
    let transcript = format!(
        "{}\n",
        json!({"type":"assistant","message":{"id":"m1","model":"claude-opus-4-8",
            "usage":{"input_tokens":150000,"output_tokens":500,
                     "cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}),
    );
    fs::write(encoded.join(format!("{sid}.jsonl")), transcript).unwrap();

    let _ = reg.run_compact_nudge(1);
    assert_eq!(audit_count(&reg, &g.id, "compact-escalation"), 1,
        "the override (200K) correctly reads 75% and escalates, overriding the model-based 1M guess");
    let s = reg.group_summary(&g.id);
    let a = s["agents"].as_array().unwrap().iter().find(|a| a["id"] == o.id.as_str()).unwrap();
    assert_eq!(a["context"]["percent"], 75);
}

#[test]
fn end_group_kills_everyone_including_the_orchestrator() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    reg.spawn_agent(&g.id, Role::Worker, "w1", "t", false, None).unwrap();
    // kill_agent refuses the orchestrator; end_group must not.
    let result = reg.end_group(&g.id, false).unwrap();
    assert_eq!(result["killed"].as_array().unwrap().len(), 2);
    // Every agent is now dead — the group reads as fully torn down.
    for a in reg.list_agents(&g.id).as_array().unwrap() {
        assert_eq!(a["status"], "dead", "end must kill every role");
    }
    assert_eq!(reg.group_summary(&g.id)["live_agents"], 0);
    // The teardown is audited as a human action.
    let log = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl")).unwrap();
    let end = log
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|e| e["action"] == "group-end")
        .expect("end must be audited");
    assert_eq!(end["actor"], "human");
    // Unknown group: an error, not a silent success.
    assert!(reg.end_group(&parse_gid("ghost-group"), false).is_err());
}

#[test]
fn end_group_clears_pause_so_relaunch_starts_clean() {
    let dir = tempfile::tempdir().unwrap();
    let gid;
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
        gid = g.id.clone();
        reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
        // A paused group that gets ended: the pause marker must not outlive it,
        // or a relaunch on the same repo id would silently resume paused.
        reg.pause_group(&g.id).unwrap();
        assert!(reg.state_root().join(g.id.as_str()).join("paused").is_file());
        reg.end_group(&g.id, false).unwrap();
        assert!(!reg.is_paused(&g.id), "ending must drop the in-memory pause");
        assert!(
            !reg.state_root().join(g.id.as_str()).join("paused").is_file(),
            "ending must remove the pause marker"
        );
    }
    // Relaunch on the same repo → same id, not paused.
    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    assert_eq!(g.id, gid);
    assert!(!reg.is_paused(&g.id), "a relaunched group must not inherit the old pause");
}

#[test]
fn end_group_removes_worktrees_of_dead_and_live_agents() {
    // A real git repo with two worktrees; end_group(cleanup=true) must reclaim
    // both — including the one whose worker already exited — while leaving the
    // main checkout intact.
    //
    // #464 B1: this used a bare `tempfile::tempdir()` repo root directly, so
    // even though `end_group` reclaims the two NAMED worktree dirs, the
    // `<repo>-worktrees/` PARENT container `git worktree add` creates beside
    // the repo — a %TEMP%-level sibling on the pre-fix shape — was never
    // itself removed and leaked on every run. `real_repo()` nests the repo
    // one level under its own temp root, so that parent container (empty or
    // not) dies with the fixture regardless of what `end_group` reclaimed.
    let repo = real_repo();
    let repo_path = repo.path().to_string_lossy().replace('\\', "/");

    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo_path, rails()).unwrap();
    // Two worktree-backed workers (spawn creates the worktree via git).
    let live = reg
        .spawn_agent(&g.id, Role::Worker, "live", "t", true, Some("wt-live".into()))
        .unwrap();
    let dead = reg
        .spawn_agent(&g.id, Role::Worker, "dead", "t", true, Some("wt-dead".into()))
        .unwrap();
    assert!(Path::new(&live.cwd).is_dir() && Path::new(&dead.cwd).is_dir());
    // One worker has already exited — its worktree must still be reclaimed.
    reg.mark_dead(&dead.id, Some(0));

    let result = reg.end_group(&g.id, true).unwrap();
    assert!(result["worktree_errors"].as_array().unwrap().is_empty(), "got: {result}");
    assert_eq!(result["worktrees_removed"].as_array().unwrap().len(), 2);
    assert!(!Path::new(&live.cwd).exists(), "live agent's worktree must be gone");
    assert!(!Path::new(&dead.cwd).exists(), "exited agent's worktree must be gone");
    // The main checkout is untouched.
    assert!(repo.path().join("f.txt").is_file(), "the repo root must survive teardown");
}

// ---------- #464: worktree-cutting fixtures must not leak %TEMP% dirs ----------

#[test]
fn real_repo_worktree_fixture_leaves_nothing_in_temp_on_success() {
    // The regression itself, on the ordinary passing path — no failure, no
    // panic, nothing but a worker spawn through the MCP-default worktree
    // (#338) and the fixture going out of scope normally. Before nesting
    // `real_repo()` one level under its own temp root, THIS is exactly the
    // shape of test that leaked its `<repo>-worktrees/<name>` sibling
    // directory into `%TEMP%` on every run — 2,438 survivors (growing to
    // 2,702) were found there, none of them from a failing test.
    // This test's OWN sanity check (not the cleanup this test exists to prove)
    // needs a path-CONTAINMENT comparison, which a raw string `starts_with`
    // gets wrong whenever the OS hands back two different spellings of the
    // same directory: macOS resolves `/var` to `/private/var` (a symlink),
    // and Windows CI can return an 8.3 short name (`RUNNER~1`) on one side
    // and the long name on the other. Canonicalized ONCE here, right after
    // the fixture is created (while the directory still exists — `canonicalize`
    // errors on a path that's gone), and reused for both comparisons below,
    // rather than re-deriving it a second time from a possibly-different
    // source at assert time.
    //
    // The actual teardown this test proves is NOT at risk from this same
    // aliasing: `RealRepo`'s cleanup is `tempfile::TempDir::drop()` recursively
    // deleting the ONE path it stored at creation — no comparison against a
    // second, separately-derived path anywhere in that call. A symlink or a
    // short name is still a valid handle onto the SAME underlying directory,
    // so `remove_dir_all` through either spelling reaches and removes
    // everything nested under it (the cut worktree included), regardless of
    // which spelling `git worktree add` happened to use when it created that
    // nested directory. String-prefix comparison and filesystem deletion are
    // different operations; only the former is fooled by aliasing.
    let (reg, _d) = test_registry();
    let root_path;
    {
        let repo = real_repo();
        root_path = repo.root().canonicalize().unwrap();
        let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
        let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
        let co = reg.resolve_token(&orch.token).unwrap();
        // No `worktree` argument at all: the MCP surface defaults a worker
        // spawn's worktree ON (#338) — exactly the path that used to leak.
        let out = dispatch(&reg, &co, "tools/call",
            &json!({ "name": "spawn_agent", "arguments": { "kind": "worker", "task": "t", "branch": "feat/x" } }))
            .unwrap();
        assert_eq!(out["isError"], false, "{out:?}");
        let agents = reg.list_agents(&g.id);
        let worker = agents.as_array().unwrap().iter().find(|a| a["role"] == "worker").unwrap();
        let cwd = worker["cwd"].as_str().unwrap();
        assert!(Path::new(cwd).is_dir(), "sanity: the worktree must actually exist before teardown");
        assert!(
            Path::new(cwd).canonicalize().unwrap().starts_with(&root_path),
            "sanity: the cut worktree must live inside the fixture's own temp root, not beside it: {cwd} vs {}",
            root_path.display()
        );
        // `repo` drops here, at the end of this block.
    }
    assert!(
        !root_path.exists(),
        "the whole fixture tree (repo + cut worktree + its git-worktree admin \
         registration) must be gone once the fixture drops: {}",
        root_path.display()
    );
}

#[test]
fn real_repo_worktree_fixture_leaves_nothing_in_temp_when_the_test_panics() {
    // Same shape, but the test fails partway through — a panic inside the
    // block that holds the fixture, AFTER the worktree exists on disk. Rust
    // unwinds through `Drop` on a panic (this binary uses the default unwind
    // panic strategy, like the rest of loomux — `catch_unwind` is already
    // used in production code, e.g. `obs.rs`), so this must clean up exactly
    // like the success path above. Proving that needs `catch_unwind`: a
    // genuinely failing `#[test]` would abort the test binary before any
    // assertion here could run.
    let (reg, _d) = test_registry();
    let root_path = std::sync::Mutex::new(None::<PathBuf>);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let repo = real_repo();
        *root_path.lock().unwrap() = Some(repo.root().to_path_buf());
        let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
        reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
        let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", true, Some("agent-x".into())).unwrap();
        assert!(Path::new(&w.cwd).is_dir(), "sanity: the worktree must exist before the panic");
        panic!("deliberate failure, after the worktree exists, to prove teardown still runs");
    }));
    assert!(result.is_err(), "the inner closure must have actually panicked");
    let root_path = root_path.into_inner().unwrap().expect("root captured before the panic fired");
    assert!(
        !root_path.exists(),
        "a panicking test must still leave nothing behind: {}",
        root_path.display()
    );
}

#[test]
fn sweep_orphaned_agent_files_reclaims_orphans_but_refuses_a_live_group() {
    // #464 item 3: a conservative startup sweep of generated custom-agent
    // files (`loomux-<group>-<block>.md`) whose group this registry has no
    // record of at all — the from-disk cleanup a real launch now runs once,
    // to reclaim exactly the shape of file `end_group` already reclaims for
    // a group that DOES end cleanly (1,111 + 161 such orphans were found on
    // a real dev machine, left by test runs that never call `end_group`).
    let (reg, dir) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();

    let claude_dir = dir.path().join("claude-agents");
    fs::create_dir_all(&claude_dir).unwrap();
    // A file for the group this registry DOES still know about (its state
    // dir exists under `state_root()`) — must survive the sweep untouched,
    // exactly like a merely-paused or not-yet-ended live group's file would.
    let live_file = claude_dir.join(format!("loomux-{}-worker.md", g.id));
    fs::write(&live_file, "---\nname: x\ndescription: x\n---\nbody").unwrap();
    // A file for a group id nothing under `state_root()` corresponds to —
    // the orphan shape a crashed or `end_group`-skipping run leaves behind.
    let orphan_file = claude_dir.join("loomux-ghost-group-worker.md");
    fs::write(&orphan_file, "---\nname: x\ndescription: x\n---\nbody").unwrap();
    // A file that merely starts with a known group id as a substring, not a
    // `-`/`.`-delimited prefix — must NOT be treated as belonging to `g.id`
    // (no false negative from a naive `starts_with`).
    let lookalike_file = claude_dir.join(format!("loomux-{}extra-worker.md", g.id));
    fs::write(&lookalike_file, "---\nname: x\ndescription: x\n---\nbody").unwrap();
    // Never ours at all (no `loomux-` prefix) — must never be considered,
    // let alone removed.
    let foreign_file = claude_dir.join("some-other-tools-agent.md");
    fs::write(&foreign_file, "not loomux's").unwrap();
    // #464 round-2 review N2: `loomux-` prefixed but the WRONG extension —
    // `write_claude_agent_file`/`write_copilot_agent_file` only ever write
    // `.md`/`.agent.md`, never anything else, so a hand-authored file that
    // merely happens to share the prefix (a note, a scratch file) must be
    // invisible to this sweep even when it also names an orphaned group.
    let wrong_extension_file = claude_dir.join("loomux-ghost-group-notes.txt");
    fs::write(&wrong_extension_file, "not a generated agent file").unwrap();

    let result = reg.sweep_orphaned_agent_files();
    assert!(result["errors"].as_array().unwrap().is_empty(), "got: {result}");
    let reclaimed = result["reclaimed"].as_array().unwrap();
    assert_eq!(
        reclaimed.len(),
        2,
        "must reclaim exactly the two orphans (ghost-group and the lookalike), got: {result}"
    );

    assert!(!orphan_file.exists(), "an orphaned group's file must be reclaimed");
    assert!(!lookalike_file.exists(), "a lookalike that is not actually this group's file must be reclaimed too");
    assert!(live_file.exists(), "a still-known group's file must never be touched");
    assert!(foreign_file.exists(), "a file with no `loomux-` prefix must never be considered, let alone removed");
    assert!(
        wrong_extension_file.exists(),
        "a `loomux-`-prefixed file with the wrong extension must never be considered, even when it names an orphaned group"
    );
}

#[test]
fn sweep_orphaned_agent_files_deletes_nothing_when_group_enumeration_fails() {
    // #464 review B3: an enumeration failure over `state_root()` must never
    // be read as "there are no live groups" — that misreading WIDENS the
    // blast radius instead of narrowing it, converting "I could not list my
    // groups" into "I have no groups" and deleting live ones' files. The
    // rule is "if unsure, delete nothing."
    //
    // The claude-agents dir is pointed at an INDEPENDENT temp root — not
    // nested under state_root, unlike `relaunch_registry`'s usual layout —
    // specifically so that breaking state_root (to force `read_dir` to
    // fail) cannot also destroy the very files this test checks survive.
    let state_dir = tempfile::tempdir().unwrap();
    let agents_dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(state_dir.path());
    let claude_dir = agents_dir.path().join("claude-agents");
    fs::create_dir_all(&claude_dir).unwrap();
    reg.set_claude_agents_dir_override(claude_dir.clone());
    reg.set_copilot_agents_dir_override(agents_dir.path().join("copilot-agents"));

    // A file that WOULD look orphaned under a normal (working) enumeration —
    // nothing under state_root corresponds to "ghost-group" even when it CAN
    // be read. The point of this test is that it must survive anyway,
    // because enumeration never gets the chance to run cleanly.
    let would_be_orphan = claude_dir.join("loomux-ghost-group-worker.md");
    fs::write(&would_be_orphan, "body").unwrap();

    // Break enumeration itself: remove state_root entirely so `read_dir`
    // on it errors (stands in for a transient AV/EDR lock, a permissions
    // error, or — this issue's own trigger — disk exhaustion).
    fs::remove_dir_all(state_dir.path()).unwrap();

    let result = reg.sweep_orphaned_agent_files();
    assert!(
        result["reclaimed"].as_array().unwrap().is_empty(),
        "an enumeration failure must reclaim NOTHING — treating it as \"no known groups\" \
         would delete a live group's file: {result}"
    );
    assert!(
        !result["errors"].as_array().unwrap().is_empty(),
        "the enumeration failure must be surfaced, not swallowed: {result}"
    );
    assert!(
        would_be_orphan.exists(),
        "nothing may be deleted when the sweep cannot enumerate live groups, even a file that \
         WOULD be a legitimate orphan under a working enumeration"
    );
}

#[test]
fn spawn_worktree_cuts_from_default_branch_not_primary_head() {
    // #204 end-to-end: a spawn_agent worktree must be cut from origin/<default>,
    // never the primary checkout's incidental HEAD. Simulate with a bare remote
    // (default branch `main`) and a clone parked on a stray feature branch.
    let git = |dir: &Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "")
            .env("GIT_CONFIG_SYSTEM", "")
            .output()
            .expect("git must be installed for this test");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };

    let bare = tempfile::tempdir().unwrap();
    git(bare.path(), &["init", "-q", "--bare"]);
    git(bare.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);

    // Seed `main` on the remote.
    let seed = tempfile::tempdir().unwrap();
    git(seed.path(), &["init", "-q"]);
    git(seed.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(seed.path(), &["config", "user.email", "t@t"]);
    git(seed.path(), &["config", "user.name", "t"]);
    fs::write(seed.path().join("base.txt"), "base").unwrap();
    git(seed.path(), &["add", "-A"]);
    git(seed.path(), &["commit", "-qm", "base on main"]);
    git(seed.path(), &["remote", "add", "origin", &bare.path().to_string_lossy()]);
    git(seed.path(), &["push", "-qu", "origin", "main"]);

    // Primary clone wandered onto a stray branch with a stray commit.
    let cloneparent = tempfile::tempdir().unwrap();
    git(cloneparent.path(), &["clone", "-q", &bare.path().to_string_lossy(), "wc"]);
    let primary = cloneparent.path().join("wc");
    git(&primary, &["config", "user.email", "t@t"]);
    git(&primary, &["config", "user.name", "t"]);
    git(&primary, &["checkout", "-q", "-b", "docs/stray"]);
    fs::write(primary.join("stray.txt"), "stray").unwrap();
    git(&primary, &["add", "-A"]);
    git(&primary, &["commit", "-qm", "stray docs commit"]);

    let repo_path = primary.to_string_lossy().replace('\\', "/");
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo_path, rails()).unwrap();

    // Default base: the worktree is cut from origin/main, not the stray HEAD.
    let w = reg
        .spawn_agent(&g.id, Role::Worker, "w", "t", true, Some("agent-x".into()))
        .unwrap();
    assert!(Path::new(&w.cwd).join("base.txt").exists(), "worktree should carry main");
    assert!(
        !Path::new(&w.cwd).join("stray.txt").exists(),
        "#204: worktree must NOT inherit the primary checkout's stray HEAD"
    );

    // An explicit base stacks a worktree on the feature branch deliberately.
    let stacked = reg
        .spawn_agent_ex(
            &g.id, Role::Worker, None, "s", "t", true, Some("agent-y".into()),
            Some("docs/stray".into()), None, None, None,
        )
        .unwrap();
    assert!(
        Path::new(&stacked.cwd).join("stray.txt").exists(),
        "an explicit base must place the worktree on top of the feature branch"
    );
}

#[test]
fn spawn_worktree_fails_loudly_when_existing_branch_diverges_from_base() {
    // #227: `base` was silently ignored whenever the requested `branch` name
    // collided with a leftover local branch — `git_worktree_add`'s
    // already-exists fallback checked that branch out as-is, regardless of
    // whether its history had anything to do with `base`. A spawn hitting
    // that collision must now fail loudly (naming both shas) instead of
    // handing back a worker whose worktree is silently cut from the wrong
    // history.
    let git = |dir: &Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "")
            .env("GIT_CONFIG_SYSTEM", "")
            .output()
            .expect("git must be installed for this test");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };

    // #464 B1 (residual, found by the whole-suite CI leak check this PR
    // adds): a bare `tempfile::tempdir()` repo root leaked an EMPTY
    // intermediate `<repo>-worktrees/stacked/` directory even though
    // `git_worktree_add`'s own #227 failure path correctly removes the LEAF
    // worktree it creates (`stacked/leftover`) — a slash in a branch name
    // makes git create the INTERMEDIATE directory as part of the nested
    // worktree path, and nothing ever removes that once its only child is
    // gone. `real_repo()` (nested under its own private temp root) makes
    // this moot: the empty intermediate dies with the fixture regardless of
    // which directories `git_worktree_add`'s own cleanup does or doesn't
    // reach — exactly the same reasoning as the other two B1 fixes.
    let repo = real_repo();

    // The desired base: a feature branch with its own commit, stacked on
    // `real_repo()`'s own initial commit. Whatever `real_repo()`'s default
    // branch happens to be named is irrelevant here — this test's
    // assertions never check that name, only that a named branch exists to
    // `checkout -` back to.
    git(repo.path(), &["checkout", "-q", "-b", "feat/base"]);
    fs::write(repo.path().join("feat.txt"), "feat").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "feature work"]);
    git(repo.path(), &["checkout", "-q", "-"]);

    // A stale leftover branch sharing the name a new spawn will request —
    // cut from the default branch (current HEAD after the checkout above),
    // never touching feat/base.
    git(repo.path(), &["branch", "stacked/leftover"]);

    let repo_path = repo.path().to_string_lossy().replace('\\', "/");
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo_path, rails()).unwrap();

    let err = reg
        .spawn_agent_ex(
            &g.id, Role::Worker, None, "w", "t", true, Some("stacked/leftover".into()),
            Some("feat/base".into()), None, None, None,
        )
        .unwrap_err();
    assert!(err.contains("stacked/leftover"), "should name the branch: {err}");
    assert!(err.contains("feat/base"), "should name the requested base: {err}");
}

// ---------- #43: compose-strip steering + human-typing hold backstop ----------

#[test]
fn steer_rejects_empty_text() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    // Whitespace-only is also empty — the strip must not enqueue a blank line.
    let err = reg.steer_orchestrator(&g.id, "   ").unwrap_err();
    assert!(err.contains("empty"), "got: {err}");
}

#[test]
fn steer_rejects_paused_group_so_the_human_gets_feedback() {
    // A paused group suppresses delivery silently; steering must surface that
    // as an error the strip shows, not vanish (the whole point of the strip is
    // that the human's message is never silently lost).
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    reg.pause_group(&g.id).unwrap();
    let err = reg.steer_orchestrator(&g.id, "do the thing").unwrap_err();
    assert!(err.contains("paused"), "got: {err}");
}

#[test]
fn steer_without_a_live_orchestrator_errors() {
    // A group with only workers (no orchestrator) must NOT fall through to a
    // worker — steering targets the orchestrator or nothing.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let err = reg.steer_orchestrator(&g.id, "steer me").unwrap_err();
    assert!(err.contains("no live orchestrator"), "got: {err}");
    // An unknown group is likewise not steerable.
    let err = reg.steer_orchestrator(&parse_gid("no-such-group"), "steer me").unwrap_err();
    assert!(err.contains("no live orchestrator"), "got: {err}");
}

#[test]
fn steer_of_a_healthy_group_reaches_delivery() {
    // Empty/paused/no-orch guards all pass → steering delegates to the
    // serialized delivery path, which in test mode (no real PTY) fails at the
    // terminal step. That error proves the guards let a healthy steer through.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let err = reg.steer_orchestrator(&g.id, "steer me").unwrap_err();
    assert!(err.contains("terminal"), "steer must reach the pty step, got: {err}");
}

#[test]
fn steering_targets_the_orchestrator_and_is_audited_under_its_group() {
    // Resolution + isolation + audit attribution in one: a paused group makes
    // delivery record a suppression audit (reachable without a real PTY) whose
    // `to` names the resolved target. It must be the ORCHESTRATOR (not the
    // worker in the same group), attributed to `human`, and written only under
    // this group's log.
    let (reg, _d) = test_registry();
    let a = reg.create_group("C:/tmp/repo-a", rails()).unwrap();
    let orch = reg.spawn_agent(&a.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    reg.spawn_agent(&a.id, Role::Worker, "w", "t", false, None).unwrap();
    let b = reg.create_group("C:/tmp/repo-b", rails()).unwrap();
    reg.spawn_agent(&b.id, Role::Orchestrator, "orch-b", "", false, None).unwrap();

    pause_with_pane(&reg, &a.id, &orch.id, 6300);
    // Go through deliver_to_orchestrator directly (steer_orchestrator's own
    // paused-guard would short-circuit before delivery) to observe resolution.
    reg.deliver_to_orchestrator(&a.id, "hello orchestrator", "human").unwrap();

    let entries = reg.audit_log(&a.id);
    let sup = entries
        .iter()
        .find(|e| e.action == "prompt")
        .expect("the steer must be audited");
    assert_eq!(sup.actor, "human", "steer must be attributed to the human");
    assert_eq!(sup.detail["to"], orch.id, "steer must resolve to the orchestrator, not the worker");
    assert_eq!(sup.detail["text"], "hello orchestrator");
    // #569: and it is HELD, not discarded — the pause queues it on the
    // orchestrator's own pane for delivery at resume.
    let held = reg.queue_snapshot(6300);
    assert_eq!(held.len(), 1, "the steer must be queued for the orchestrator's pane");
    assert_eq!(held[0].agent_id, orch.id);
    // Group isolation: nothing landed in group B's log.
    assert!(
        reg.audit_log(&b.id).iter().all(|e| e.action != "prompt"),
        "a steer to group A must not touch group B"
    );
}

#[test]
fn unconfirmed_delivery_notifies_the_orchestrator_and_suppresses_the_exceptions() {
    // #103: the emission/suppression gate, driven directly (the live emission
    // point sits in the delivery thread, which test mode never reaches without a
    // real PTY). One notice per call — the thread invokes this exactly once per
    // delivery, past all the submit retries, so retries never multiply it.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();

    let notices = |reg: &OrchRegistry| {
        reg.audit_log(&g.id)
            .into_iter()
            .filter(|e| e.action == "delivery-unconfirmed-notice")
            .collect::<Vec<_>>()
    };

    // Confirmed delivery to the worker: the prompt landed, nothing to chase.
    reg.notify_unconfirmed_delivery(&g.id, &w.id, false, true, false, 1_001);
    assert!(notices(&reg).is_empty(), "a confirmed delivery must not notify");

    // Unconfirmed delivery TO the orchestrator: a notice about it would itself be
    // a delivery to the orchestrator — an endless loop. Suppressed.
    reg.notify_unconfirmed_delivery(&g.id, &orch.id, true, false, false, 1_002);
    assert!(notices(&reg).is_empty(), "an unconfirmed delivery to the orchestrator must not notify");

    // The real case: an unconfirmed delivery to the worker → exactly one notice,
    // audited to this app itself, naming the stranded agent.
    reg.notify_unconfirmed_delivery(&g.id, &w.id, false, false, false, 1_003);
    let after = notices(&reg);
    assert_eq!(after.len(), 1, "an unconfirmed worker delivery notifies exactly once");
    assert_eq!(after[0].actor, "orrerix", "the notice is a system message from this app");
    assert_eq!(after[0].detail["to"], w.id, "the notice names the stranded worker");
}

#[test]
fn several_alarms_on_one_pane_cost_the_orchestrator_one_turn_not_n() {
    // #539, the other half of the cost. Two deliveries to the same pane can
    // each declare failure within one `LATE_MONITOR_POLL` of the other —
    // supersession only retires the older monitor at its NEXT tick — and an
    // in-window `Failed` from `deliver_now` can land alongside a monitor's.
    // Pre-#539 that was N notices asking the orchestrator the SAME question
    // (is anything actually stuck on that pane?), each costing a turn plus a
    // `get_output` probe.
    //
    // The two halves are driven exactly as production composes them; the only
    // thing skipped is the sleep between them.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    // The notice is a real delivery to the orchestrator, and `deliver_prompt`
    // refuses a pane with no terminal before it audits anything — so the text
    // this test reads back only exists once the orchestrator is bound.
    reg.set_pty_for_test(&orch.id, 941);

    assert!(reg.buffer_unconfirmed_delivery(&g.id, &w.id, false, 7_001), "the first alarm opens the window");
    assert!(!reg.buffer_unconfirmed_delivery(&g.id, &w.id, false, 7_002), "a joiner must not arm a second timer");
    assert!(!reg.buffer_unconfirmed_delivery(&g.id, &w.id, false, 7_003));
    assert!(
        reg.audit_log(&g.id).iter().all(|e| e.action != "delivery-unconfirmed-notice"),
        "nothing is announced until the window closes"
    );

    reg.flush_unconfirmed_notices(&g.id, &w.id, false);
    let notices = reg
        .audit_log(&g.id)
        .into_iter()
        .filter(|e| e.action == "delivery-unconfirmed-notice")
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 1, "three alarms, one notice");
    assert_eq!(notices[0].detail["coalesced"], 3);
    assert_eq!(
        notices[0].detail["delivery_ids"],
        json!([7_001, 7_002, 7_003]),
        "every constituent is named — coalescing must not lose which deliveries it stood for"
    );

    // The text the orchestrator actually reads names all three, so it can tell
    // them apart in a line that no longer arrives once per delivery.
    let delivered = reg
        .audit_log(&g.id)
        .into_iter()
        .filter(|e| e.action == "prompt")
        .filter_map(|e| e.detail["text"].as_str().map(str::to_string))
        .find(|t| t.contains("unconfirmed"))
        .expect("the coalesced notice is delivered to the orchestrator");
    for id in ["7001", "7002", "7003"] {
        assert!(delivered.contains(id), "id {id} missing from: {delivered}");
    }

    // Idempotent: the bucket is TAKEN, so a second flush (a duplicate timer, a
    // retry) cannot deliver an empty or repeated notice.
    reg.flush_unconfirmed_notices(&g.id, &w.id, false);
    assert_eq!(
        reg.audit_log(&g.id).iter().filter(|e| e.action == "delivery-unconfirmed-notice").count(),
        1,
        "flushing an already-drained bucket must announce nothing"
    );
}

#[test]
fn the_coalesced_notice_reads_as_one_ask_and_names_every_delivery() {
    // The wording, pinned directly. The plural changes the ASK — one
    // `get_output`, not one per id — which is the entire point of coalescing,
    // and a line that told the orchestrator to probe N times would spend the
    // turns the feature exists to save.
    let one = unconfirmed_delivery_notice("w-3", &[1_753_000_000_123]);
    assert!(one.contains("delivery to w-3 unconfirmed"), "got: {one}");
    assert!(one.contains("(id 1753000000123)"), "the single id is named too: {one}");
    assert!(one.contains("get_output it and re-send if needed"), "the recovery move is unchanged: {one}");

    let many = unconfirmed_delivery_notice("w-3", &[11, 22, 33]);
    assert!(many.contains("3 deliveries to w-3 unconfirmed"), "the verb agrees with the count: {many}");
    assert!(many.contains("(ids 11, 22, 33)"), "every constituent named: {many}");
    assert!(many.contains("ONCE"), "one probe answers all of them: {many}");
    assert!(!many.contains("1 delivery to"), "no singular wording on a coalesced line: {many}");

    // rev-13 N3: this notice is itself a paste into a pane, so the id list is
    // bounded and the remainder points at the audit log — which keeps every
    // id. Same argument, and the same number, as `PAUSE_SUPPRESSION_LIST_MAX`.
    let ids: Vec<u64> = (1..=(UNCONFIRMED_NOTICE_IDS_MAX as u64 + 3)).collect();
    let capped = unconfirmed_delivery_notice("w-3", &ids);
    assert!(capped.contains(&format!("{} deliveries", ids.len())), "the COUNT is never truncated: {capped}");
    assert!(capped.contains("and 3 more — see the audit log"), "the remainder is named, not dropped: {capped}");
    assert!(
        !capped.contains(&format!(", {}", ids.len())),
        "the last id is past the cap and must not be listed: {capped}"
    );
}

#[test]
fn the_notice_audit_records_whether_it_actually_reached_anyone() {
    // rev-13 N4. `deliver_to_orchestrator` is best-effort and its "no live
    // orchestrator" branch audits nothing of its own, so auditing before the
    // attempt and discarding the result asserts a notice that never went
    // anywhere — and coalescing makes that single false record stand for every
    // id in the batch at once. Same shape as #569's `pause-suppression-notice`.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();

    // Orchestrator not bound to a pane yet: the notice cannot land.
    reg.buffer_unconfirmed_delivery(&g.id, &w.id, false, 6_001);
    reg.flush_unconfirmed_notices(&g.id, &w.id, false);
    let undelivered = reg
        .audit_log(&g.id)
        .into_iter()
        .find(|e| e.action == "delivery-unconfirmed-notice")
        .expect("the attempt is recorded either way");
    assert_eq!(undelivered.detail["delivered"], false, "it did not land, and the record must say so");
    assert!(
        undelivered.detail["error"].as_str().is_some_and(|e| e.contains("terminal")),
        "and why: {:?}",
        undelivered.detail["error"]
    );

    // Bind the pane and the FIRST failure mode is gone — the record must move
    // with it rather than repeat a stale reason. A headless registry has no
    // `AppHandle`, so `deliver_prompt` still cannot complete (`Err("no app
    // handle")`) and `delivered: true` is unreachable here by construction;
    // what IS assertable, and what N4 is actually about, is that the flag and
    // the reason are read back from the real attempt instead of asserted ahead
    // of it. A hardcoded record could not change its reason.
    reg.set_pty_for_test(&orch.id, 944);
    reg.buffer_unconfirmed_delivery(&g.id, &w.id, false, 6_002);
    reg.flush_unconfirmed_notices(&g.id, &w.id, false);
    let second = reg
        .audit_log(&g.id)
        .into_iter()
        .filter(|e| e.action == "delivery-unconfirmed-notice")
        .next_back()
        .expect("second notice");
    assert_eq!(second.detail["delivered"], false, "still undeliverable headless — and it says so");
    let why = second.detail["error"].as_str().unwrap_or_default().to_string();
    assert!(!why.contains("terminal"), "the terminal-less reason is stale now: {why}");
    assert!(!why.is_empty(), "an undelivered notice always names why: {why}");
    // The pane DID get the paste attempt both times — `deliver_prompt` audits
    // `prompt` before it can fail on the app handle — so the honest record is
    // "attempted, not confirmed delivered", which is exactly the distinction
    // this field exists to draw.
    assert_eq!(
        reg.audit_log(&g.id).iter().filter(|e| e.action == "prompt").count(),
        1,
        "only the bound attempt gets far enough to be pasted at all"
    );
}

#[test]
fn a_delivery_corrected_inside_the_window_is_never_announced_as_unconfirmed() {
    // rev-13 finding A — a false alarm the coalescing delay CREATED, in the
    // change whose whole purpose is removing false alarms.
    //
    // `late_monitor_tick` checks `hook_match` before `already_failed`, so a
    // late prompt-landed record arriving after a failure was declared is a
    // first-class outcome (`Confirm { correction: true }` is named for it),
    // and the monitor polls every 5s — two or three of those ticks land inside
    // a 15s window. Pre-#539 the alarm was already out before any correction
    // could exist, so the orchestrator could only ever read them in that
    // order. Buffered, the correction overtakes the alarm and it reads them
    // backwards: "it landed" at t+5s, then "unconfirmed … id N …" at t+15s —
    // a `get_output` probe plus the tempting re-send #451/#510 exist to stop.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, 942);

    // t+0: two deliveries on the same pane both declare failure.
    reg.buffer_unconfirmed_delivery(&g.id, &w.id, false, 9_001);
    reg.buffer_unconfirmed_delivery(&g.id, &w.id, false, 9_002);
    // t+5s: a late hook record proves 9_001 landed after all.
    assert!(
        reg.retract_unconfirmed_delivery(&g.id, &w.id, 9_001),
        "an id still in the bucket must be withdrawable — the window has not closed"
    );
    // t+15s: the window closes.
    reg.flush_unconfirmed_notices(&g.id, &w.id, false);

    let notices = reg
        .audit_log(&g.id)
        .into_iter()
        .filter(|e| e.action == "delivery-unconfirmed-notice")
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 1, "the surviving alarm still goes out");
    assert_eq!(
        notices[0].detail["delivery_ids"],
        json!([9_002]),
        "the corrected delivery must not be named — it demonstrably landed"
    );
    let delivered = reg
        .audit_log(&g.id)
        .into_iter()
        .filter(|e| e.action == "prompt")
        .filter_map(|e| e.detail["text"].as_str().map(str::to_string))
        .find(|t| t.contains("unconfirmed"))
        .expect("the surviving alarm is delivered");
    assert!(!delivered.contains("9001"), "the corrected id must not reach the pane: {delivered}");
    assert!(delivered.contains("9002"), "the real one must: {delivered}");

    // The withdrawal is discoverable after the fact (#445), naming what is left.
    let retracted = reg
        .audit_log(&g.id)
        .into_iter()
        .find(|e| e.action == "delivery-unconfirmed-retracted")
        .expect("a withdrawn alarm leaves a trace");
    assert_eq!(retracted.detail["delivery_id"], 9_001);
    assert_eq!(retracted.detail["remaining"], 1);
}

#[test]
fn a_bucket_emptied_by_retraction_announces_nothing_at_all() {
    // The other half rev-13 named: when the correction takes the LAST id, the
    // already-armed timer still fires. It must find nothing and say nothing —
    // an empty coalesced notice would be an alarm about no delivery.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, 943);

    reg.buffer_unconfirmed_delivery(&g.id, &w.id, false, 9_100);
    assert!(reg.retract_unconfirmed_delivery(&g.id, &w.id, 9_100));
    reg.flush_unconfirmed_notices(&g.id, &w.id, false);
    assert!(
        reg.audit_log(&g.id).iter().all(|e| e.action != "delivery-unconfirmed-notice"),
        "the only alarm was withdrawn — the window must close in silence"
    );

    // Withdrawing something that was never buffered — or was already flushed —
    // is a no-op, NOT a claim. That `false` is what tells the caller the
    // orchestrator has already read the alarm and is owed a correction.
    assert!(!reg.retract_unconfirmed_delivery(&g.id, &w.id, 9_100), "already gone");
    assert!(!reg.retract_unconfirmed_delivery(&g.id, &w.id, 4_242), "never buffered");
    reg.buffer_unconfirmed_delivery(&g.id, &w.id, false, 9_200);
    reg.flush_unconfirmed_notices(&g.id, &w.id, false);
    assert!(
        !reg.retract_unconfirmed_delivery(&g.id, &w.id, 9_200),
        "an id whose notice HAS gone out cannot be withdrawn — that alarm is already read, \
         and a correction is what it is owed"
    );

    // And a fresh alarm after all that still opens a window of its own, so a
    // retraction cannot wedge the pane into permanent silence.
    assert!(
        reg.buffer_unconfirmed_delivery(&g.id, &w.id, false, 9_300),
        "a later alarm opens a new bucket and arms its own timer"
    );
}

#[test]
fn a_group_paused_during_the_window_suppresses_the_flush_and_says_which_ids() {
    // #532's rule applied to the deferral this adds: the paused gate is
    // re-verified at the moment of submit, not trusted from when the alarm was
    // raised — a window is 15s of wall clock in which a human can pause the
    // group. And per #445 the suppression names every id it swallowed, so the
    // deliveries stay discoverable after the fact.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();

    reg.buffer_unconfirmed_delivery(&g.id, &w.id, false, 8_001);
    reg.buffer_unconfirmed_delivery(&g.id, &w.id, false, 8_002);
    reg.pause_group(&g.id).unwrap();
    reg.flush_unconfirmed_notices(&g.id, &w.id, false);

    assert!(
        reg.audit_log(&g.id).iter().all(|e| e.action != "delivery-unconfirmed-notice"),
        "a group paused mid-window must not spend the notice budget"
    );
    let suppressed = reg
        .audit_log(&g.id)
        .into_iter()
        .find(|e| e.action == "notice-suppressed" && e.detail["kind"] == "unconfirmed-delivery")
        .expect("the suppression is audited");
    assert_eq!(suppressed.detail["reason"], "group-paused");
    assert_eq!(suppressed.detail["delivery_ids"], json!([8_001, 8_002]));
}

#[test]
fn unconfirmed_notice_is_suppressed_while_the_group_is_paused() {
    // Paused groups suppress all pane delivery; the unconfirmed notice follows
    // the same semantics as the watchdog nudge and must not spend the notice
    // budget while paused.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.pause_group(&g.id).unwrap();

    reg.notify_unconfirmed_delivery(&g.id, &w.id, false, false, false, 1_003);
    assert!(
        reg.audit_log(&g.id).iter().all(|e| e.action != "delivery-unconfirmed-notice"),
        "a paused group must not raise the unconfirmed notice"
    );
}

#[test]
fn notify_queue_fires_for_a_worker_but_suppresses_and_audits_for_the_orchestrator_and_while_paused() {
    // #445: `notify_queue` is what an aborted-then-queued delivery uses to
    // tell the orchestrator once — but never for an orchestrator target
    // (that would loop) and never while paused (delivery is suppressed
    // there anyway) — and EVERY suppression leaves a `notice-suppressed`
    // audit line (the routed #451 finding this issue owns: a suppressed
    // notification must be discoverable after the fact).
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();

    // Worker target, group live: the notice actually attempts delivery (it
    // fails at the pty step in test mode, same as every other delivery —
    // `deliver_to_orchestrator`'s own coverage already pins the happy path).
    reg.notify_queue(&g.id, &w.id, false, &queue::queued_notice(&w.id, queue::EnqueueReason::BoxOccupied));
    assert!(
        reg.audit_log(&g.id).iter().all(|e| e.action != "notice-suppressed"),
        "a live worker target must not be suppressed"
    );

    // Orchestrator target: suppressed and audited (a notice to it is a
    // delivery to it — a loop).
    reg.notify_queue(&g.id, &o.id, true, &queue::queued_notice(&o.id, queue::EnqueueReason::BoxOccupied));
    let sup = reg
        .audit_log(&g.id)
        .into_iter()
        .find(|e| e.action == "notice-suppressed" && e.detail["to"] == json!(o.id))
        .expect("an orchestrator-target queue notice must be suppressed AND audited");
    assert_eq!(sup.detail["reason"], json!("target-is-orchestrator"), "got: {}", sup.detail);

    // Paused group: suppressed and audited even for a worker.
    reg.pause_group(&g.id).unwrap();
    reg.notify_queue(&g.id, &w.id, false, &queue::queued_notice(&w.id, queue::EnqueueReason::BoxOccupied));
    let sup = reg
        .audit_log(&g.id)
        .into_iter()
        .filter(|e| e.action == "notice-suppressed" && e.detail["to"] == json!(w.id))
        .next_back()
        .expect("a paused group's queue notice must be suppressed AND audited");
    assert_eq!(sup.detail["reason"], json!("group-paused"), "got: {}", sup.detail);
}

// ---------- #72: steering-strip image attachments ----------

#[test]
fn sanitize_attachment_ext_allows_only_vetted_image_types() {
    // Case- and dot-insensitive; jpeg folds to jpg; everything else is refused.
    assert_eq!(sanitize_attachment_ext("png"), Some("png"));
    assert_eq!(sanitize_attachment_ext(".PNG"), Some("png"));
    assert_eq!(sanitize_attachment_ext("JPEG"), Some("jpg"));
    assert_eq!(sanitize_attachment_ext("jpg"), Some("jpg"));
    assert_eq!(sanitize_attachment_ext("webp"), Some("webp"));
    assert_eq!(sanitize_attachment_ext("gif"), Some("gif"));
    assert_eq!(sanitize_attachment_ext("bmp"), Some("bmp"));
    // Path-traversal / executable / script extensions are rejected outright.
    assert_eq!(sanitize_attachment_ext("exe"), None);
    assert_eq!(sanitize_attachment_ext("svg"), None);
    assert_eq!(sanitize_attachment_ext("../etc/passwd"), None);
    assert_eq!(sanitize_attachment_ext(""), None);
}

#[test]
fn save_attachment_writes_bytes_verbatim_under_the_group_dir() {
    let (reg, dir) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    // A tiny "PNG" — bytes are written as-is, never decoded, so any payload works.
    let bytes = [0x89u8, b'P', b'N', b'G', 1, 2, 3, 0, 255];
    let path = reg.save_attachment(&g.id, "png", &bytes).unwrap();
    let p = Path::new(&path);
    assert!(p.is_file(), "the attachment file must exist");
    assert_eq!(fs::read(p).unwrap(), bytes, "bytes must be stored verbatim");
    // It lives under <root>/<group>/attachments/ and carries the .png extension.
    assert_eq!(p.extension().and_then(|e| e.to_str()), Some("png"));
    let attach_dir = dir.path().join(g.id.as_str()).join("attachments");
    assert!(p.starts_with(&attach_dir), "path {p:?} must be under {attach_dir:?}");
    // The save is audited so there's a human-attributed trail of what was sent.
    assert!(
        reg.audit_log(&g.id).iter().any(|e| e.action == "attachment-save" && e.actor == "human"),
        "the save must be audited under the human actor",
    );
}

#[test]
fn save_attachment_rejects_bad_type_empty_and_oversize() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    // Unsupported extension.
    assert!(reg.save_attachment(&g.id, "exe", &[1, 2, 3]).unwrap_err().contains("unsupported"));
    // Empty payload.
    assert!(reg.save_attachment(&g.id, "png", &[]).unwrap_err().contains("empty"));
    // One byte past the cap is refused (and nothing is written).
    let huge = vec![0u8; MAX_ATTACHMENT_BYTES + 1];
    assert!(reg.save_attachment(&g.id, "png", &huge).unwrap_err().contains("too large"));
    // Exactly at the cap is accepted.
    let at_cap = vec![0u8; MAX_ATTACHMENT_BYTES];
    assert!(reg.save_attachment(&g.id, "png", &at_cap).is_ok());
}

#[test]
fn save_attachment_gives_each_image_a_distinct_path_in_a_burst() {
    // A multi-image paste saves several files back-to-back (possibly within one
    // millisecond); the per-process sequence must keep their names unique so one
    // image never clobbers another.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let mut paths = std::collections::HashSet::new();
    for _ in 0..20 {
        let p = reg.save_attachment(&g.id, "png", &[7u8]).unwrap();
        assert!(paths.insert(p.clone()), "duplicate attachment path: {p:?}");
    }
    assert_eq!(paths.len(), 20);
}

#[test]
fn save_attachment_rejects_an_unknown_group() {
    // Membership guard (#72 review): a group id that was never created must be
    // refused before anything is written.
    let (reg, dir) = test_registry();
    assert!(reg.save_attachment(&parse_gid("never-made"), "png", &[1, 2, 3]).unwrap_err().contains("unknown"));
    // #904 — the traversal half of this test is GONE, deliberately, and this is
    // the record of it. It used to pass `"../escape"` here to prove the
    // membership guard also stopped a traversal. `save_attachment` now takes a
    // `GroupId`, so that line no longer COMPILES: a traversal id is not
    // expressible at this call site. The property did not weaken, it moved —
    // from a runtime assertion here to a type, pinned by
    // `parse_refuses_every_path_shaped_group_id` in `tests/groupid.rs`. What
    // remains below is what the type does NOT cover: a well-formed id that is
    // not a live group.
    // Nothing was written anywhere under the root.
    assert!(!dir.path().join("never-made").exists());
    assert!(!dir.path().join("attachments").exists());
}

#[test]
fn orchestrator_cli_resolves_the_groups_cli_for_reference_formatting() {
    // The save command returns this so the frontend formats image references the
    // way the orchestrator's CLI reads them (#72 review note 3).
    let (reg, _d) = test_registry();
    let claude = reg.create_group("C:/tmp/claude-repo", rails()).unwrap();
    let copilot = reg.create_group("C:/tmp/copilot-repo", copilot_rails()).unwrap();
    assert_eq!(reg.orchestrator_cli(&claude.id), "claude");
    assert_eq!(reg.orchestrator_cli(&copilot.id), "copilot");
    // Unknown group → the safe default wording, never a panic.
    assert_eq!(reg.orchestrator_cli(&parse_gid("nope")), "claude");
}

#[test]
fn end_group_sweeps_the_attachments_scratch_dir() {
    let (reg, dir) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    // A durable file (state) and an attachment: teardown reclaims the scratch
    // dir but leaves the rest of the group state alone.
    reg.set_state(&g.id, "{\"k\":1}").unwrap();
    let att = reg.save_attachment(&g.id, "png", &[1, 2, 3]).unwrap();
    let attach_dir = dir.path().join(g.id.as_str()).join("attachments");
    assert!(Path::new(&att).is_file() && attach_dir.is_dir());

    reg.end_group(&g.id, false).unwrap();
    assert!(!attach_dir.exists(), "attachments dir must be swept on group end");
    assert!(
        dir.path().join(g.id.as_str()).join("state.json").is_file(),
        "non-attachment group state must survive teardown",
    );
}

#[test]
fn end_group_reclaims_generated_copilot_agent_files() {
    // rev-4 review (N4): without this, a #416-generated `~/.copilot/agents/
    // loomux-<group>-<block>.agent.md` outlives the group it was written
    // for — accumulating forever and cluttering the user's real Copilot
    // agent list with dead groups' names.
    let (reg, dir) = test_registry();
    let g = reg.create_group("C:/tmp/copilot-repo", copilot_rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();

    let generated = dir.path().join("copilot-agents").join(format!("loomux-{}-{}.agent.md", g.id, w.block));
    assert!(generated.is_file(), "the default-roster worker must have gotten a generated wrapper file: {generated:?}");

    reg.end_group(&g.id, false).unwrap();
    assert!(!generated.exists(), "the generated file must be reclaimed on group end");
}

#[test]
fn end_group_reclaims_generated_claude_agent_files() {
    // Round #417 correction 6's own analog of N4 above: the generated
    // `~/.claude/agents/loomux-<group>-<block>.md` file must not outlive
    // the group either, or it accumulates forever and clutters the user's
    // real Claude agent list with dead groups' names, exactly the N4
    // concern this round widened the fix for.
    let (reg, dir) = test_registry();
    let g = reg.create_group("C:/tmp/claude-repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();

    let generated = dir.path().join("claude-agents").join(format!("loomux-{}-{}.md", g.id, w.block));
    assert!(generated.is_file(), "the default-roster worker must have gotten a generated agent file: {generated:?}");

    reg.end_group(&g.id, false).unwrap();
    assert!(!generated.exists(), "the generated file must be reclaimed on group end");
}
