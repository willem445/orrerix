//! Applying workflows to a live group: the advanced toggle, named workflows, list_blocks and merge-gate hot-reload.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ───────── #316: the LIVE advanced-orchestrator toggle ─────────
//
// `set_advanced_orchestrator` is modeled EXACTLY on `set_max_agents`/
// `set_autonomous`: persist-first, in-memory swap, audit, notice. What's new
// versus the launch-time-only toggle (#222) is that flipping it now arms or
// clears the merge gate LIVE, via the identical `load_workflow` +
// `sync_merge_gate` sequence a fresh launch runs — and never silently arms a
// gate the running roster cannot satisfy (the incident behind #316).

/// A repo whose workflow declares a worker tier under an id (`worker-deep`)
/// the BUILT-IN roster does not carry — so a delegate spawned under the
/// built-in `worker` block before the toggle has an identity the custom
/// roster does not reuse, making "did the toggle re-persona a live agent" and
/// "does a new spawn use the new roster" both directly observable.
fn tiered_workflow_repo() -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    let dir = td.path().join(".loomux");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("workflow.yml"),
        "version: 1\nname: focused-review\n\
         blocks:\n\
         \x20 - id: worker-deep\n    kind: worker\n\
         \x20 - id: rev-security\n    kind: reviewer\n    prompt: Security only.\n\
         \x20 - id: rev-tests\n    kind: reviewer\n    prompt: Test quality only.\n\
         gates:\n  merge:\n    reviewers: [rev-security, rev-tests]\n",
    )
    .unwrap();
    td
}

/// A `.loomux/workflow.yml` that fails to parse — an unknown block kind, which
/// `parse_workflow` rejects outright rather than coercing (workflow.rs's
/// capability-closure rule: an unrecognized `kind` must never become a worker).
fn broken_workflow_repo() -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    let dir = td.path().join(".loomux");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("workflow.yml"),
        "version: 1\nname: broken\nblocks:\n  - id: mystery\n    kind: wizard\n",
    )
    .unwrap();
    td
}

#[test]
fn advanced_orchestrator_toggle_on_arms_the_gate_swaps_blocks_and_notifies() {
    let (reg, _d) = test_registry();
    let repo = gated_repo("");
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    assert!(!g.guardrails.advanced_orchestrator, "the group launches plain");
    assert!(!reg.merge_gate_declared(&g.id), "no gate before the toggle");

    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    pause_with_pane(&reg, &g.id, &orch.id, 7101);

    let status = reg.set_advanced_orchestrator(&g.id, true, "human").unwrap();
    assert_eq!(status["advanced"], json!(true));
    assert_eq!(status["name"], json!("focused-review"));

    let g2 = reg.group(&g.id).unwrap();
    assert!(g2.guardrails.advanced_orchestrator);
    let ids: Vec<&str> = g2.guardrails.blocks.iter().map(|b| b.id.as_str()).collect();
    assert!(
        ids.contains(&"rev-security") && ids.contains(&"rev-tests"),
        "the repo's roster must replace the built-in one: {ids:?}"
    );

    assert!(reg.merge_gate_declared(&g.id), "the gate file must be written");
    let gate = reg.merge_gate(&g.id).unwrap();
    assert_eq!(gate.reviewers, vec!["rev-security".to_string(), "rev-tests".to_string()]);
    assert_eq!(status["gate"]["satisfiable"], json!(true));
    assert_eq!(status["gate"]["missing_blocks"], json!(Vec::<String>::new()));

    let entries = reg.audit_log(&g.id);
    assert!(entries.iter().any(|e| e.action == "advanced-orchestrator-on"));
    assert!(entries.iter().any(|e| e.action == "workflow-loaded"));
    assert!(entries.iter().any(|e| e.action == "merge-gate-declared"));
    let notice = entries
        .iter()
        .find(|e| {
            e.action == "prompt"
                && e.detail["text"].as_str().unwrap_or("").contains("workflow mode changed")
        })
        .unwrap_or_else(|| panic!("the orchestrator must receive the toggle notice: {entries:?}"));
    assert!(notice.detail["text"].as_str().unwrap().contains("'focused-review' active"));
}

#[test]
fn advanced_orchestrator_toggle_off_clears_the_gate_and_restores_builtin_roster() {
    let (reg, _d) = test_registry();
    let repo = gated_repo("");
    let g = reg
        .create_group(&repo.path().to_string_lossy(), Guardrails { advanced_orchestrator: true, ..rails() })
        .unwrap();
    assert!(reg.merge_gate_declared(&g.id), "the launch arms the gate");

    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    pause_with_pane(&reg, &g.id, &orch.id, 7102);

    let status = reg.set_advanced_orchestrator(&g.id, false, "human").unwrap();
    assert_eq!(status["advanced"], json!(false));
    assert_eq!(status["gate"], Value::Null);

    assert!(!reg.merge_gate_declared(&g.id), "the gate file must be removed");
    let g2 = reg.group(&g.id).unwrap();
    assert!(!g2.guardrails.advanced_orchestrator);
    let ids: Vec<&str> = g2.guardrails.blocks.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(ids.len(), 4, "restored to the built-in four: {ids:?}");
    for want in ["orchestrator", "worker", "reviewer", "planner"] {
        assert!(ids.contains(&want), "{want} missing from the restored roster: {ids:?}");
    }

    let entries = reg.audit_log(&g.id);
    assert!(entries.iter().any(|e| e.action == "advanced-orchestrator-off"));
    assert!(entries.iter().any(|e| e.action == "merge-gate-cleared"));
    assert!(entries.iter().any(|e| {
        e.action == "prompt"
            && e.detail["text"].as_str().unwrap_or("").contains("built-in roster, no merge gate")
    }));
}

#[test]
fn advanced_orchestrator_toggle_on_refuses_when_the_repo_declares_no_workflow_file() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/no-workflow-repo", rails()).unwrap();
    let err = reg.set_advanced_orchestrator(&g.id, true, "human").unwrap_err();
    assert!(err.contains("workflow.yml"), "the refusal must say why: {err}");
    let g2 = reg.group(&g.id).unwrap();
    assert!(!g2.guardrails.advanced_orchestrator, "a refused toggle must not flip the flag");
    assert!(!reg.merge_gate_declared(&g.id), "and must not arm a gate");
}

#[test]
fn advanced_orchestrator_toggle_on_refuses_on_a_broken_workflow_file() {
    let (reg, _d) = test_registry();
    let repo = broken_workflow_repo();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let err = reg.set_advanced_orchestrator(&g.id, true, "human").unwrap_err();
    assert!(err.contains("workflow.yml"), "{err}");
    let g2 = reg.group(&g.id).unwrap();
    assert!(!g2.guardrails.advanced_orchestrator);
    assert_eq!(
        g2.guardrails.blocks.len(),
        4,
        "a broken file must arm nothing — the built-in roster stands untouched: {:?}",
        g2.guardrails.blocks
    );
    assert!(!reg.merge_gate_declared(&g.id));
}

#[test]
fn advanced_orchestrator_toggle_persists_and_preserves_other_guardrails() {
    let dir = tempfile::tempdir().unwrap();
    let repo = gated_repo("");
    let repo_path = repo.path().to_string_lossy().to_string();
    let gid;
    let path;
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        let g = reg.create_group(&repo_path, Guardrails { max_agents: 5, ..rails() }).unwrap();
        gid = g.id.clone();
        path = reg.state_root().join(g.id.as_str()).join("group.json");
        reg.set_advanced_orchestrator(&g.id, true, "human").unwrap();
    }
    let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(v["guardrails"]["advanced_orchestrator"], json!(true));
    assert_eq!(v["guardrails"]["max_agents"], json!(5), "unrelated guardrails must survive the patch");
    assert!(v["created_ms"].as_u64().is_some(), "created_ms must survive the patch");
    let blocks = v["guardrails"]["blocks"].as_array().unwrap();
    assert!(blocks.iter().any(|b| b["id"] == "rev-security"), "the new roster must be persisted: {blocks:?}");
    assert!(blocks.iter().any(|b| b["id"] == "rev-tests"));

    // A resumed session (session browser) rebuilds its guardrails from
    // `load_group_file` — the real production path — and rejoins with
    // `Launch::Resume`. The toggle and the roster/gate it produced must survive
    // that rejoin: `advanced_orchestrator` is a launch-time input, not something
    // `create_group_ex` self-heals from disk the way it does `max_agents`, so
    // the caller (here, the real `load_group_file`) must carry it forward.
    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    let (repo2, loaded) = reg.load_group_file(&gid).expect("group.json must still load");
    assert!(loaded.advanced_orchestrator, "the persisted flag must read back true");
    let g = reg.create_group_ex(&repo2, loaded, Launch::Resume).unwrap();
    assert_eq!(g.id, gid, "resume reattaches the same group");
    assert!(g.guardrails.advanced_orchestrator, "the toggle survives a restart");
    assert!(g.guardrails.blocks.iter().any(|b| b.id == "rev-security"), "resume keeps the persisted roster");
    assert!(reg.merge_gate_declared(&gid), "resume keeps the armed gate file");
}

#[test]
fn advanced_orchestrator_toggle_is_a_noop_when_already_at_target() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.set_advanced_orchestrator(&g.id, false, "human").unwrap();
    let log = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl")).unwrap_or_default();
    assert!(!log.contains("advanced-orchestrator"), "already-off must not audit a no-op toggle");
}

#[test]
fn advanced_orchestrator_toggle_fails_soft_on_corrupt_group_file() {
    let (reg, _d) = test_registry();
    let repo = gated_repo("");
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    // A valid-JSON but non-object root (e.g. from corruption) must error rather
    // than panic on the in-place field assignment.
    fs::write(reg.state_root().join(g.id.as_str()).join("group.json"), "null").unwrap();
    let err = reg.set_advanced_orchestrator(&g.id, true, "human").unwrap_err();
    assert!(err.contains("not a JSON object"), "non-object root must fail soft, got: {err}");
    assert!(!reg.merge_gate_declared(&g.id), "a failed persist must never arm a gate");
    // rev-24 N2: `load_workflow` succeeded here (the repo carries a valid
    // workflow.yml) before the persist failed on the corrupt group.json — the
    // "workflow-loaded" audit must not have been written for a load that never
    // stuck, or the trail would claim a state change that didn't happen.
    assert!(
        !reg.audit_log(&g.id).iter().any(|e| e.action == "workflow-loaded"),
        "a failed persist must not leave a stray workflow-loaded audit line"
    );
}

#[test]
fn advanced_orchestrator_toggle_never_reidentifies_a_live_delegate() {
    let (reg, _d) = test_registry();
    let repo = tiered_workflow_repo();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    // Spawned under the BUILT-IN roster, before the toggle.
    let w = reg.spawn_agent(&g.id, Role::Worker, "w1", "t", false, None).unwrap();
    assert_eq!(w.block, "worker");

    reg.set_advanced_orchestrator(&g.id, true, "human").unwrap();

    // The live delegate keeps its original block identity — the roster swap
    // never re-personas an agent a human already consented to.
    assert_eq!(
        reg.agent(&w.id).unwrap().block, "worker",
        "a toggle must not retro-swap a live delegate's persona"
    );

    // The custom roster no longer HAS a "worker" id at all — the swap applies
    // to FUTURE spawns only: a new default-block worker resolves to the new tier.
    let w2 = reg.spawn_agent(&g.id, Role::Worker, "w2", "t", false, None).unwrap();
    assert_eq!(w2.block, "worker-deep", "a spawn after the toggle must use the NEW roster");
    assert!(
        reg.spawn_agent_ex(
            &g.id, Role::Worker, Some("worker".into()), "w3", "t", false, None, None, None, None, None
        )
        .is_err(),
        "the built-in 'worker' block id must be gone from the roster after the toggle"
    );
}

// ───────── #1689 slice B: applying a NAMED workflow to a live group ─────────
//
// `apply_workflow` is `set_advanced_orchestrator`'s shape with a named file and
// a diff. What these pin is the half a toggle never had to answer: a switch
// both ADDS and REMOVES block ids, so a live pane, a bare resume, the group
// dir's instruction files and the armed gate each need a stated answer.

/// A repo declaring TWO workflows with DISJOINT delegate ids and different
/// gates: `default` (`.orrerix/workflow.yml`, ids `w-a`/`rev-a`) and `b`
/// (`.orrerix/workflows/b.yml`, ids `w-b`/`rev-b`).
///
/// Disjoint on purpose — it is what makes "did a live pane keep its block",
/// "does a new spawn use the new roster" and "is the old id refused now" three
/// separately observable facts rather than one.
fn two_workflow_repo() -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    let dir = td.path().join(".orrerix");
    fs::create_dir_all(dir.join("workflows")).unwrap();
    fs::write(
        dir.join("workflow.yml"),
        "version: 1\nname: plain\n\
         blocks:\n\
         \x20 - id: w-a\n    kind: worker\n\
         \x20 - id: rev-a\n    kind: reviewer\n    prompt: Everything.\n\
         gates:\n  merge:\n    reviewers: [rev-a]\n",
    )
    .unwrap();
    write_workflow_b(td.path(), "sonnet");
    td
}

/// `b.yml`, with `w-b`'s model as the one thing a caller varies — the
/// same-name re-apply case (#1566's apply-an-edit path) edits exactly this.
fn write_workflow_b(repo: &std::path::Path, worker_model: &str) {
    fs::write(
        repo.join(".orrerix").join("workflows").join("b.yml"),
        format!(
            "version: 1\nname: focused-b\n\
             blocks:\n\
             \x20 - id: w-b\n    kind: worker\n    model: {worker_model}\n\
             \x20 - id: rev-b\n    kind: reviewer\n    prompt: Security only.\n\
             gates:\n  merge:\n    reviewers: [rev-b]\n\
             intake:\n  labels:\n    hold: do-not-touch\n"
        ),
    )
    .unwrap();
}

pub(crate) fn wf(name: &str) -> workflow::WorkflowName {
    workflow::WorkflowName::parse(name).unwrap()
}

/// A group launched in workflow mode on `default`, with a paused orchestrator
/// pane so every notice this section asserts on is observable.
fn switchable_group(reg: &OrchRegistry, repo: &tempfile::TempDir) -> GroupInfo {
    let g = reg
        .create_group(
            &repo.path().to_string_lossy(),
            Guardrails { advanced_orchestrator: true, max_agents: 8, ..rails() },
        )
        .unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    pause_with_pane(reg, &g.id, &orch.id, 7301);
    g
}

fn group_json(reg: &OrchRegistry, g: &GroupInfo) -> Value {
    serde_json::from_str(
        &fs::read_to_string(reg.state_root().join(g.id.as_str()).join("group.json")).unwrap(),
    )
    .unwrap()
}

/// **#2663, end to end through the apply that makes it reachable.** `b.yml`
/// declares `intake.labels.hold: do-not-touch`; `.orrerix/workflow.yml` — the
/// file this group launched on, and the one `gh.rs` used to resolve for every
/// caller — declares no `intake:` block at all, so it means `agent-hold`.
///
/// The four surfaces that had to agree, and did not: the lifecycle panel
/// (`autonomy_state_within`'s `hold_label`, which is `hold_label_of(group)` —
/// the same value the intake poller checks and the contract prose names), the
/// issues view's vocabulary, the writable allow-list, and the create spec.
///
/// Two negative controls, and they answer different questions. **Before the
/// apply**, the same group on `default` resolves the built-in on every surface —
/// which is what makes the assertions below detect a MOVE rather than a value
/// that was always `do-not-touch`. **With no group in hand**, the same repo
/// still resolves `default`'s file, byte for byte the pre-#2663 answer that a
/// plain (non-orchestration) pane's issues view gets.
#[test]
fn an_applied_workflow_renaming_the_hold_veto_moves_every_label_surface() {
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let path = repo.path().to_string_lossy().to_string();
    let g = switchable_group(&reg, &repo);

    let panel = |reg: &OrchRegistry| -> String {
        reg.autonomy_state_within(&g.id, Duration::from_secs(60))["hold_label"]
            .as_str()
            .unwrap()
            .to_string()
    };

    // ── control 1: this group, before the apply, on `default` ──
    let before = reg.guardrails_of(&g.id).expect("the registry holds the group");
    assert_eq!(panel(&reg), "agent-hold");
    assert_eq!(gh::gh_label_vocabulary_sync(&path, Some(&before)).hold, "agent-hold");
    assert_eq!(gh::allowed_labels(&path, Some(&before)), gh::allowed_labels(&path, None));

    reg.apply_workflow(&g.id, &wf("b"), None, "human").unwrap();

    // ── the claim: one spelling across all four ──
    let rails = reg.guardrails_of(&g.id).expect("still held after the apply");
    let hold = panel(&reg);
    assert_eq!(hold, "do-not-touch", "the apply rewrote guardrails.intake from b.yml");
    assert_eq!(
        gh::gh_label_vocabulary_sync(&path, Some(&rails)).hold,
        hold,
        "the issues view must be TOLD the spelling this group's poller watches"
    );
    let allowed = gh::allowed_labels(&path, Some(&rails));
    assert!(
        gh::validate_labels(&allowed, &[hold.clone()]).is_ok(),
        "the write path must ACCEPT it: {allowed:?}"
    );
    assert!(
        gh::validate_labels(&allowed, &["agent-hold".to_string()]).is_err(),
        "and must refuse the spelling this group is no longer running: {allowed:?}"
    );
    assert!(
        gh::label_spec_for(&path, Some(&rails), &hold).is_some(),
        "and it must be creatable on a repo that has never defined it"
    );

    // ── control 2: the same repo with no group in hand is unchanged ──
    assert_eq!(gh::gh_label_vocabulary_sync(&path, None).hold, "agent-hold");
    assert!(gh::label_spec_for(&path, None, "do-not-touch").is_none());
    assert_eq!(
        gh::allowed_labels(&path, None),
        vec!["agent-ready", "agent-investigation", "agent-managed", "agent-hold"],
        "a plain pane's issues view still gets the pre-#2663 four-label set"
    );
}

/// A group this registry no longer holds is the no-group arm, not a panic and
/// not a stale answer — the case a pane outlives (an ended orchestration whose
/// issues view is still open).
#[test]
fn a_group_the_registry_no_longer_holds_has_no_guardrails_to_scope_labels_with() {
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let g = switchable_group(&reg, &repo);
    assert!(reg.guardrails_of(&g.id).is_some(), "positive control: it is held now");

    let gone = GroupId::parse("no-such-group").unwrap();
    assert!(reg.guardrails_of(&gone).is_none());
}

#[test]
fn applying_a_named_workflow_swaps_the_roster_for_future_spawns_only() {
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let g = switchable_group(&reg, &repo);
    assert_eq!(g.guardrails.workflow.as_str(), "default", "the launch pins the plain file");

    // Spawned under the OLD roster, before the switch.
    let w = reg.spawn_agent(&g.id, Role::Worker, "w1", "t", false, None).unwrap();
    assert_eq!(w.block, "w-a");

    let status = reg.apply_workflow(&g.id, &wf("b"), None, "human").unwrap();
    assert_eq!(status["workflow"], json!("b"), "{status}");
    assert_eq!(status["name"], json!("focused-b"), "the file's own name: field: {status}");
    assert_eq!(status["drift"], Value::Null, "right after an apply the group runs what the file says");

    // The live delegate keeps the identity a human already approved.
    assert_eq!(
        reg.agent(&w.id).unwrap().block,
        "w-a",
        "a switch must not retro-swap a live delegate's block"
    );

    // FUTURE spawns resolve against the new roster: the b-only id works…
    let w2 = reg
        .spawn_agent_ex(&g.id, Role::Worker, Some("w-b".into()), "w2", "t", false, None, None, None, None, None)
        .unwrap();
    assert_eq!(w2.block, "w-b");
    // …and the a-only id is refused, by the roster's own unknown-block path,
    // NAMING what the group can spawn now.
    let err = reg
        .spawn_agent_ex(&g.id, Role::Worker, Some("w-a".into()), "w3", "t", false, None, None, None, None, None)
        .unwrap_err();
    assert!(err.contains("unknown block"), "{err}");
    assert!(err.contains("w-b"), "the refusal must name the ACTIVE roster: {err}");

    // group.json carries the name, the roster and the intake profile that came
    // out of the same document — one atomic write, so nothing on disk can
    // disagree about which file this group consented to.
    let v = group_json(&reg, &g);
    assert_eq!(v["guardrails"]["workflow"], json!("b"));
    let ids: Vec<&str> =
        v["guardrails"]["blocks"].as_array().unwrap().iter().map(|b| b["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"w-b") && ids.contains(&"rev-b"), "{ids:?}");
    assert!(!ids.contains(&"w-a"), "the old roster must be gone from disk too: {ids:?}");
    assert_eq!(
        v["guardrails"]["intake"]["labels"]["hold"],
        json!("do-not-touch"),
        "the intake vocabulary travels with the roster it was resolved from (#382 NB2)"
    );

    // The gate is re-armed from the new file — the state `gate_status_line`
    // and `list_verdicts` read.
    let gate = reg.merge_gate(&g.id).unwrap();
    assert_eq!(gate.reviewers, vec!["rev-b".to_string()]);

    let entries = reg.audit_log(&g.id);
    let switched = entries
        .iter()
        .find(|e| e.action == "workflow-switched")
        .unwrap_or_else(|| panic!("no workflow-switched row: {entries:?}"));
    assert_eq!(switched.detail["from"], json!("default"));
    assert_eq!(switched.detail["to"], json!("b"));
    assert_eq!(switched.detail["actor"], json!("human"));
    assert_eq!(switched.detail["diff"]["added"], json!(["rev-b", "w-b"]));
    assert_eq!(switched.detail["diff"]["removed"], json!(["rev-a", "w-a"]));
    assert_eq!(switched.detail["diff"]["gate_changed"], json!(true));
    assert_eq!(switched.detail["diff"]["intake_changed"], json!(true));
}

#[test]
fn the_orchestrator_is_told_which_ids_it_may_spawn_by_now() {
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let g = switchable_group(&reg, &repo);
    reg.apply_workflow(&g.id, &wf("b"), None, "human").unwrap();

    let notice = delivered_texts(&reg, &g.id)
        .into_iter()
        .find(|t| t.contains("workflow switched"))
        .unwrap_or_else(|| panic!("the orchestrator must receive the switch notice"));
    assert!(notice.contains("'default' → 'b'"), "{notice}");
    assert!(notice.contains("w-b (worker"), "the roster rows must name kind and model: {notice}");
    assert!(notice.contains("removed: rev-a, w-a"), "{notice}");
    assert!(notice.contains("merge gate requires all of [rev-b]"), "{notice}");
    assert!(
        notice.contains("a bare resume of its session will be refused"),
        "the one consequence the pane cannot discover for itself: {notice}"
    );
    // #1689 slice C: the notice deliberately names NO tool — the read-back
    // pointer lives in the kickoff's workflow section (see the design note),
    // because a notice must stay one paragraph and is a delivery, not the
    // place an agent is taught a tool. Pin the negative so the clause cannot
    // drift back in silently.
    assert!(!notice.contains("list_blocks"), "{notice}");
    assert!(is_one_paragraph(&notice), "a delivered notice is ONE paragraph: {notice:?}");
}

#[test]
fn a_bare_resume_of_a_removed_blocks_session_is_refused_by_the_new_roster() {
    // The consent rule's other half, and it needs no new mechanism: the bare
    // resume inherits the RECORDED block id (#254) and `spawn_agent_bound`'s
    // unknown-block path then refuses it against the roster that is live now.
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let g = switchable_group(&reg, &repo);
    let orch_id = reg
        .list_agents(&g.id)
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["role"] == json!("orchestrator"))
        .and_then(|a| a["id"].as_str().map(str::to_string))
        .unwrap();
    let co = reg.resolve_token(&reg.agent(&orch_id).unwrap().token).unwrap();
    let cwd = repo.path().to_string_lossy().to_string();

    let w = reg.spawn_agent(&g.id, Role::Worker, "w1", "t", false, None).unwrap();
    assert_eq!(w.block, "w-a");
    let session = reg
        .list_agents(&g.id)
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == json!(w.id))
        .and_then(|a| a["session"].as_str().map(str::to_string))
        .expect("the spawned worker must have a session id");
    reg.mark_dead(&w.id, Some(0));

    // Positive control: the SAME bare resume succeeds while `w-a` is still
    // declared, so the refusal below is the switch and not the resume shape.
    let ok = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": { "resume_session": session, "cwd": cwd, "task": "round 2" },
    }))
    .unwrap();
    assert_eq!(ok["isError"], false, "{ok:?}");
    let resumed = reg
        .list_agents(&g.id)
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["session"] == json!(session) && a["status"] != json!("dead"))
        .and_then(|a| a["id"].as_str().map(str::to_string))
        .unwrap();
    reg.mark_dead(&resumed, Some(0));

    reg.apply_workflow(&g.id, &wf("b"), None, "human").unwrap();

    let refused = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": { "resume_session": session, "cwd": cwd, "task": "round 3" },
    }))
    .unwrap();
    assert_eq!(refused["isError"], true, "a removed block's bare resume must be refused: {refused:?}");
    let text = refused["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("w-a"), "the refusal must name the block it could not find: {text}");
    assert!(text.contains("w-b"), "and the roster it CAN spawn from: {text}");
}

// The plan's open unknown, answered in `docs/design/workflows.md`:
// `set_advanced_orchestrator` writes no instruction files at all, and the
// per-spawn render (#1187) never REMOVES one. Only `write_instruction_files`
// reconciles the group dir against the manifest (#423) — so an apply has to
// call it.
//
// TWO tests, not one, and the split is the point. A single test asserting the
// write and then the sweep stops at whichever fails FIRST, so a mutation that
// removes the call reddens on the WRITE and never reaches the sweep — and a red
// evidences only the assertion it moved (CLAUDE.md). Split, the same mutation
// moves both, separately.

#[test]
fn a_switch_writes_the_new_blocks_instruction_files() {
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let g = switchable_group(&reg, &repo);
    let dir = reg.state_root().join(g.id.as_str());
    assert!(!dir.join("w-b.md").is_file(), "precondition: b's files are not there yet");

    reg.apply_workflow(&g.id, &wf("b"), None, "human").unwrap();

    assert!(dir.join("w-b.md").is_file(), "the new block's instructions must exist before its first spawn");
    assert!(dir.join("rev-b.md").is_file());
}

#[test]
fn a_switch_sweeps_a_removed_blocks_instruction_file() {
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let g = switchable_group(&reg, &repo);
    let dir = reg.state_root().join(g.id.as_str());
    // Positive control: the file this test is about really is on disk first, so
    // an absence assertion cannot pass against a group dir that never had it.
    assert!(dir.join("w-a.md").is_file(), "the launch wrote the old roster's files");
    assert!(dir.join("rev-a.md").is_file());

    reg.apply_workflow(&g.id, &wf("b"), None, "human").unwrap();

    assert!(!dir.join("w-a.md").is_file(), "the removed block's file must be SWEPT, not left stale");
    assert!(!dir.join("rev-a.md").is_file());
}

#[test]
fn a_refused_apply_changes_nothing_at_all() {
    // Three refusals, one negative control: group.json's BYTES before and
    // after, so "nothing changed" is checked rather than sampled field by
    // field.
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    fs::write(
        repo.path().join(".orrerix").join("workflows").join("broken.yml"),
        "version: 1\nblocks:\n  - id: m\n    kind: wizard\n",
    )
    .unwrap();
    let g = switchable_group(&reg, &repo);
    let path = reg.state_root().join(g.id.as_str()).join("group.json");
    let before = fs::read(&path).unwrap();
    let gate_before = reg.merge_gate(&g.id).unwrap();

    let missing = reg.apply_workflow(&g.id, &wf("nope"), None, "human").unwrap_err();
    assert!(missing.contains("workflows/nope.yml"), "the refusal names the file it looked for: {missing}");
    let broken = reg.apply_workflow(&g.id, &wf("broken"), None, "human").unwrap_err();
    assert!(broken.contains("is invalid"), "{broken}");

    assert_eq!(fs::read(&path).unwrap(), before, "a refused apply must not touch group.json");
    assert_eq!(reg.merge_gate(&g.id).unwrap(), gate_before, "nor the armed gate");
    let ids: Vec<String> = reg.group(&g.id).unwrap().guardrails.blocks.iter().map(|b| b.id.clone()).collect();
    assert!(ids.contains(&"w-a".to_string()), "nor the live roster: {ids:?}");
    assert!(
        !reg.audit_log(&g.id).iter().any(|e| e.action == "workflow-switched"),
        "nor leave a trail claiming a switch that never happened"
    );
}

#[test]
fn a_switch_that_moves_the_orchestrators_cli_is_refused_but_still_previewed() {
    // A live session cannot change the program it is running — the same reason
    // `promote_orchestrator_cli` refuses it. The PREVIEW still resolves, so the
    // modal can say what would change and why it cannot, rather than a bare
    // error naming neither.
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    fs::write(
        repo.path().join(".orrerix").join("workflows").join("c.yml"),
        "version: 1\nname: other-cli\n\
         blocks:\n\
         \x20 - id: orchestrator\n    kind: orchestrator\n    cli: copilot\n\
         \x20 - id: w-c\n    kind: worker\n",
    )
    .unwrap();
    let g = switchable_group(&reg, &repo);
    let before = fs::read(reg.state_root().join(g.id.as_str()).join("group.json")).unwrap();

    let preview = reg.workflow_switch_preview(&g.id, &wf("c")).unwrap();
    assert_eq!(preview["diff"]["orchestrator_cli_changed"], json!(true), "{preview}");
    let refusal = preview["refusal"].as_str().unwrap_or("");
    assert!(refusal.contains("different CLI"), "the preview explains: {refusal}");
    assert!(!preview["diff"]["added"].as_array().unwrap().is_empty(), "and still carries the diff: {preview}");

    let err = reg.apply_workflow(&g.id, &wf("c"), None, "human").unwrap_err();
    assert_eq!(err, refusal, "the apply refuses with exactly the sentence the preview showed");
    assert_eq!(
        fs::read(reg.state_root().join(g.id.as_str()).join("group.json")).unwrap(),
        before,
        "a refused apply changes nothing"
    );
}

#[test]
fn apply_refuses_while_workflow_mode_is_off_and_names_the_fix() {
    // The design call recorded in `docs/design/workflows.md`: the toggle is this
    // group's ONE consent surface for "does this group obey repo-authored
    // workflow files", and an apply answers only WHICH one. Arming the toggle
    // from a picker would be the app granting itself that consent.
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    assert!(!g.guardrails.advanced_orchestrator);

    let err = reg.apply_workflow(&g.id, &wf("b"), None, "human").unwrap_err();
    assert!(err.contains("workflow mode is off"), "{err}");
    assert!(err.contains("turn it on first"), "the refusal names the fix: {err}");
    assert_eq!(
        reg.workflow_switch_preview(&g.id, &wf("b")).unwrap_err(),
        err,
        "the preview refuses identically, so the picker can explain before the click"
    );
    let ids: Vec<String> = reg.group(&g.id).unwrap().guardrails.blocks.iter().map(|b| b.id.clone()).collect();
    assert!(ids.contains(&"worker".to_string()), "the built-in roster stands: {ids:?}");
}

#[test]
fn re_applying_the_active_name_after_a_model_edit_is_one_row_and_reaches_the_next_spawn() {
    // #1566's apply-an-edit path and AC 4's one-row model swap are the SAME
    // mechanism, deliberately: there is no second way to adopt an edit.
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let g = switchable_group(&reg, &repo);
    reg.apply_workflow(&g.id, &wf("b"), None, "human").unwrap();

    // An unedited re-apply is a no-op, and says so by writing nothing.
    let audits_before = reg.audit_log(&g.id).len();
    reg.apply_workflow(&g.id, &wf("b"), None, "human").unwrap();
    assert_eq!(
        reg.audit_log(&g.id).len(),
        audits_before,
        "re-applying an unedited file must not audit or notify"
    );

    write_workflow_b(repo.path(), "opus");
    let preview = reg.workflow_switch_preview(&g.id, &wf("b")).unwrap();
    assert_eq!(preview["empty"], json!(false));
    assert_eq!(
        preview["diff"]["changed"],
        json!([{ "id": "w-b", "fields": ["model"] }]),
        "a model edit is exactly one row naming one key: {preview}"
    );
    assert!(preview["diff"]["added"].as_array().unwrap().is_empty(), "{preview}");
    assert!(preview["diff"]["removed"].as_array().unwrap().is_empty(), "{preview}");

    reg.apply_workflow(&g.id, &wf("b"), None, "human").unwrap();
    let a = reg
        .spawn_agent_ex(&g.id, Role::Worker, Some("w-b".into()), "w", "t", false, None, None, None, None, None)
        .unwrap();
    let req = reg.spawn_request_for_test(&a.id).expect("a spawn request");
    let i = req.argv.iter().position(|t| t == "--model").expect("a --model flag");
    assert_eq!(req.argv[i + 1], "opus", "the edit must reach the next spawn's command line: {:?}", req.argv);
}

#[test]
fn drift_is_null_after_an_apply_set_by_an_edit_and_cleared_by_re_applying() {
    // The badge and the resume audit read ONE comparison (`roster_drift`), so
    // they cannot disagree about whether this group has drifted.
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let g = switchable_group(&reg, &repo);
    reg.apply_workflow(&g.id, &wf("b"), None, "human").unwrap();
    assert_eq!(reg.workflow_status(&g.id)["drift"], Value::Null);

    write_workflow_b(repo.path(), "opus");
    let drifted = reg.workflow_status(&g.id);
    assert_eq!(
        drifted["drift"]["note"],
        json!("the file has changed since this group was launched"),
        "{drifted}"
    );
    // Sorted for the comparison: `on_disk` is the roster in FILE order with the
    // synthesized orchestrator prepended (`clamped`), and pinning that order here
    // would be pinning a detail of a different function.
    let mut on_disk: Vec<String> = drifted["drift"]["on_disk_blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    on_disk.sort();
    assert_eq!(on_disk, vec!["orchestrator", "rev-b", "w-b"], "{drifted}");
    let ids: Vec<String> = reg.group(&g.id).unwrap().guardrails.blocks.iter().map(|b| b.id.clone()).collect();
    assert!(ids.contains(&"w-b".to_string()), "the PINNED roster still runs — drift is a badge: {ids:?}");

    reg.apply_workflow(&g.id, &wf("b"), None, "human").unwrap();
    assert_eq!(reg.workflow_status(&g.id)["drift"], Value::Null, "adopting the edit clears it");
}

#[test]
fn the_status_offers_every_declared_name_and_pins_the_one_that_runs() {
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let g = switchable_group(&reg, &repo);
    let status = reg.workflow_status(&g.id);
    assert_eq!(status["available"], json!(["b", "default"]), "{status}");
    assert_eq!(status["workflow"], json!("default"));
    reg.apply_workflow(&g.id, &wf("b"), None, "human").unwrap();
    assert_eq!(reg.workflow_status(&g.id)["workflow"], json!("b"));
}

#[test]
fn the_toggle_after_a_switch_arms_the_switched_file() {
    // The toggle reads `guardrails.workflow`, which the apply persisted — so
    // OFF and back ON returns to `b`, not to `default`. This is what makes the
    // toggle-off refusal above a two-step rather than a dead end.
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let g = switchable_group(&reg, &repo);
    reg.apply_workflow(&g.id, &wf("b"), None, "human").unwrap();

    reg.set_advanced_orchestrator(&g.id, false, "human").unwrap();
    assert!(!reg.merge_gate_declared(&g.id), "toggle-off clears the gate");
    assert_eq!(
        reg.group(&g.id).unwrap().guardrails.workflow.as_str(),
        "b",
        "toggle-off leaves the NAME recorded, so ON comes back to the same file"
    );

    let status = reg.set_advanced_orchestrator(&g.id, true, "human").unwrap();
    assert_eq!(status["name"], json!("focused-b"), "the toggle re-armed b, not default: {status}");
    assert_eq!(reg.merge_gate(&g.id).unwrap().reviewers, vec!["rev-b".to_string()]);
}

// ───────── #2659 review round 1 ─────────

/// A repo with ONE workflow, declaring a custom intake vocabulary — the fixture
/// `switchable_group` structurally cannot be, because it launches with
/// `advanced_orchestrator: true` and so takes the Fresh branch, the one path
/// that already resolved intake. Reaching the toggle needs a group that starts
/// with workflow mode OFF.
fn intake_workflow_repo() -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    let dir = td.path().join(".orrerix");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("workflow.yml"),
        "version: 1\nname: vocab\n\
         blocks:\n\
         \x20 - id: w-v\n    kind: worker\n\
         intake:\n  labels:\n    hold: do-not-touch\n",
    )
    .unwrap();
    td
}

#[test]
fn the_live_toggle_adopts_the_files_intake_not_just_its_roster() {
    // rev-final 1. The toggle took `blocks` from the file and left `intake` at
    // the built-in profile, so a group that arrived in workflow mode by TOGGLE
    // rather than by launch ran a declared roster beside a built-in label
    // vocabulary — which `roster_drift` reads, correctly, as drift against a
    // file nobody had edited. This PR is what made that visible, by publishing
    // drift as a field, so it is this PR's to fix.
    let (reg, _d) = test_registry();
    let repo = intake_workflow_repo();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    assert!(!g.guardrails.advanced_orchestrator, "the group launches plain");
    assert_eq!(
        g.guardrails.intake.hold, "agent-hold",
        "precondition: a plain launch runs the built-in vocabulary"
    );

    reg.set_advanced_orchestrator(&g.id, true, "human").unwrap();

    let after = reg.group(&g.id).unwrap();
    assert!(after.guardrails.blocks.iter().any(|b| b.id == "w-v"), "the roster came from the file");
    assert_eq!(
        after.guardrails.intake.hold, "do-not-touch",
        "and so must the vocabulary resolved from the SAME document (#778: `hold` is a live veto)"
    );
    let v: Value = serde_json::from_str(
        &fs::read_to_string(reg.state_root().join(g.id.as_str()).join("group.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(v["guardrails"]["intake"]["labels"]["hold"], json!("do-not-touch"), "persisted too");

    // The symptom this PR would otherwise have shipped: a permanent drift badge
    // on a file nobody has edited.
    assert_eq!(
        reg.workflow_status(&g.id)["drift"],
        Value::Null,
        "a group that just adopted the file is not drifted from it"
    );
}

#[test]
fn the_toggle_off_restores_the_builtin_intake_vocabulary() {
    // The ON fix's other half. Restoring the built-in roster while KEEPING the
    // file's labels is the same asymmetry pointing the other way — the intake
    // poller and the human's veto would answer to a spelling only the repo file
    // ever named, on a group that is no longer obeying that file.
    let (reg, _d) = test_registry();
    let repo = intake_workflow_repo();
    let g = reg
        .create_group(
            &repo.path().to_string_lossy(),
            Guardrails { advanced_orchestrator: true, ..rails() },
        )
        .unwrap();
    assert_eq!(g.guardrails.intake.hold, "do-not-touch", "precondition: the launch adopted it");

    reg.set_advanced_orchestrator(&g.id, false, "human").unwrap();

    let after = reg.group(&g.id).unwrap();
    assert_eq!(after.guardrails.blocks.len(), 4, "the built-in roster is back");
    assert_eq!(
        after.guardrails.intake.hold, "agent-hold",
        "and the built-in vocabulary with it — off means the file is not obeyed, all of it"
    );
}

/// `f.yml`: the same roster as `default`, with the orchestrator block given
/// knobs a RUNNING pane cannot pick up until it is resumed.
///
/// `model: sonnet` and not `opus`, deliberately: an orchestrator block's
/// resolved default on claude IS `opus`, so declaring it would be the value the
/// group already runs and `roster_diff` would — correctly — report no `model`
/// change at all. The first cut of this fixture did exactly that and the test
/// asserted a key the diff had no reason to carry.
fn write_workflow_f(repo: &std::path::Path) {
    fs::write(
        repo.join(".orrerix").join("workflows").join("f.yml"),
        "version: 1\nname: knobs\n\
         blocks:\n\
         \x20 - id: orchestrator\n    kind: orchestrator\n    model: sonnet\n    effort: high\n\
         \x20 - id: w-a\n    kind: worker\n\
         \x20 - id: rev-a\n    kind: reviewer\n    prompt: Everything.\n\
         gates:\n  merge:\n    reviewers: [rev-a]\n",
    )
    .unwrap();
}

#[test]
fn a_preview_names_the_orchestrator_keys_that_wait_for_a_resume() {
    // rev-std 1. `next_resume` had no assertion anywhere, so a regression in its
    // field filter — letting `cli` through, say, which is a REFUSAL and not a
    // next-resume note — would have shipped green.
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    write_workflow_f(repo.path());
    let g = switchable_group(&reg, &repo);

    let preview = reg.workflow_switch_preview(&g.id, &wf("f")).unwrap();
    assert_eq!(preview["next_resume"], json!(["effort", "model"]), "sorted: {preview}");
    assert_eq!(preview["refusal"], Value::Null, "knobs alone do not refuse: {preview}");
    assert_eq!(preview["empty"], json!(false), "it is a real change, just a deferred one");

    // THE DISCRIMINATING HALF, and the first cut of this test did not have it
    // (#2659 round-2 mutation m9 came back GREEN). `f.yml` moves `model` and
    // `effort` and leaves `cli` alone, so filtering `cli` out of that row is a
    // no-op and the assertion above holds with or without the filter — which is
    // exactly the regression rev-std 1 named. `g.yml` moves `cli` TOO, so the
    // filter is the only thing keeping it out of the list.
    fs::write(
        repo.path().join(".orrerix").join("workflows").join("g.yml"),
        "version: 1\nname: knobs-and-cli\n\
         blocks:\n\
         \x20 - id: orchestrator\n    kind: orchestrator\n    cli: copilot\n    model: sonnet\n\
         \x20 - id: w-a\n    kind: worker\n\
         \x20 - id: rev-a\n    kind: reviewer\n    prompt: Everything.\n\
         gates:\n  merge:\n    reviewers: [rev-a]\n",
    )
    .unwrap();
    let both = reg.workflow_switch_preview(&g.id, &wf("g")).unwrap();
    assert_eq!(both["diff"]["orchestrator_cli_changed"], json!(true), "{both}");
    assert!(both["refusal"].is_string(), "a CLI move refuses: {both}");
    let keys: Vec<&str> =
        both["next_resume"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert!(
        !keys.contains(&"cli"),
        "`cli` is a REFUSAL, never a key that waits for a resume: {both}"
    );
    assert!(keys.contains(&"model"), "and the deferred knob beside it is still reported: {both}");
}

#[test]
fn previewing_the_active_unedited_workflow_reports_an_empty_diff() {
    // rev-std 1. `empty` was asserted only `false`; the `true` case is the one
    // the modal must be able to say instead of offering a blank confirmation.
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let g = switchable_group(&reg, &repo);

    let preview = reg.workflow_switch_preview(&g.id, &wf("default")).unwrap();
    assert_eq!(preview["empty"], json!(true), "{preview}");
    assert_eq!(preview["from"], json!("default"));
    assert!(preview["diff"]["added"].as_array().unwrap().is_empty(), "{preview}");
    assert!(preview["diff"]["removed"].as_array().unwrap().is_empty(), "{preview}");
    assert!(preview["diff"]["changed"].as_array().unwrap().is_empty(), "{preview}");
    assert_eq!(preview["diff"]["gate_changed"], json!(false));
    assert_eq!(preview["diff"]["intake_changed"], json!(false));

    // Negative control: the same call for a DIFFERENT name is not empty, so the
    // assertion above is about this file and not about `empty` being stuck.
    assert_eq!(reg.workflow_switch_preview(&g.id, &wf("b")).unwrap()["empty"], json!(false));
}

#[test]
fn a_previews_gate_satisfiability_is_computed_against_the_roster_it_would_install() {
    // rev-std 1. The discriminating part is WHICH roster: `b`'s gate names
    // `rev-b`, which the group's CURRENT roster does not carry. Computed against
    // the live roster this would report unsatisfiable and the modal would warn
    // about a gate that is fine.
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let g = switchable_group(&reg, &repo);
    assert!(
        !reg.group(&g.id).unwrap().guardrails.blocks.iter().any(|b| b.id == "rev-b"),
        "precondition: the live roster cannot spawn b's reviewer"
    );

    let preview = reg.workflow_switch_preview(&g.id, &wf("b")).unwrap();
    assert_eq!(preview["gate"]["reviewers"], json!(["rev-b"]), "{preview}");
    assert_eq!(preview["gate"]["satisfiable"], json!(true), "{preview}");
    assert_eq!(preview["gate"]["missing_blocks"], json!(Vec::<String>::new()), "{preview}");
}

#[test]
fn an_apply_refuses_a_confirmation_taken_before_the_file_changed() {
    // Premortem 1. Without this the apply installs whatever the file says at
    // click time and the audit row records a diff nobody approved.
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let g = switchable_group(&reg, &repo);

    let preview = reg.workflow_switch_preview(&g.id, &wf("b")).unwrap();
    let confirmed = preview["digest"].as_str().expect("a preview carries a digest").to_string();

    // The human reads the diff; somebody edits the file; the human clicks.
    write_workflow_b(repo.path(), "opus");
    let err = reg.apply_workflow(&g.id, &wf("b"), Some(&confirmed), "human").unwrap_err();
    assert!(err.contains("changed since the preview"), "{err}");
    assert_eq!(
        reg.group(&g.id).unwrap().guardrails.workflow.as_str(),
        "default",
        "a refused apply changes nothing"
    );

    // Re-previewing is what clears it — and the fresh digest applies.
    let fresh = reg.workflow_switch_preview(&g.id, &wf("b")).unwrap()["digest"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(fresh, confirmed, "the edit really did move the digest");
    reg.apply_workflow(&g.id, &wf("b"), Some(&fresh), "human").unwrap();
    assert_eq!(reg.group(&g.id).unwrap().guardrails.workflow.as_str(), "b");

    let switched = reg
        .audit_log(&g.id)
        .into_iter()
        .find(|e| e.action == "workflow-switched")
        .expect("a workflow-switched row");
    assert_eq!(switched.detail["digest"], json!(fresh), "the trail records what landed");
    assert_eq!(
        switched.detail["confirmed_digest"],
        json!(fresh),
        "and what the human was shown, so a disagreement is reconstructable"
    );
}

#[test]
fn an_apply_with_no_confirmation_records_that_rather_than_a_match() {
    // `None` is a caller with nothing to honour — a script, a test. It must not
    // read back as "the two agreed".
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let g = switchable_group(&reg, &repo);
    reg.apply_workflow(&g.id, &wf("b"), None, "human").unwrap();

    let switched = reg
        .audit_log(&g.id)
        .into_iter()
        .find(|e| e.action == "workflow-switched")
        .expect("a workflow-switched row");
    assert!(switched.detail["digest"].is_string(), "what landed is always recorded");
    assert_eq!(switched.detail["confirmed_digest"], Value::Null, "no confirmation was given");
}

#[test]
fn an_unknown_group_offers_no_workflows_rather_than_the_process_working_directory() {
    // rev-std 3. `available` used to pass the group's repo `unwrap_or_default()`,
    // and `""` is not "no repo": `Path::new("").join(".orrerix")` resolves against
    // whatever directory the PROCESS happens to be running in, so the answer was
    // about the wrong machine entirely. Read-only and bounded, but an answer
    // about the wrong tree is worse than no answer.
    //
    // WHAT THIS TEST CANNOT SEE, and it is not a small caveat: the answer under
    // the OLD code depends on the process working directory, and a cargo
    // integration test runs with that set to `src-tauri/`, which declares no
    // workflow file. So the defective code answers `[]` here too, and the
    // round-2 mutation that restored it came back GREEN (#2725, run 33974519428)
    // — this assertion pins the intended answer, not the guard. What actually
    // stops the empty string reaching a relative join is `scan_workflows`'
    // own empty-root guard, and nothing in this harness can redden that either:
    // proving it needs a CWD that declares a workflow, which means mutating a
    // process-global from a parallel test suite. Stated rather than papered over.
    let (reg, _d) = test_registry();
    let status = reg.workflow_status(&parse_gid("no-such-group"));
    assert_eq!(status["available"], json!(Vec::<String>::new()), "{status}");

    // Positive control, in the same process and therefore the same CWD: a group
    // that DOES have a repo still lists that repo's workflows, so the assertion
    // above is the unknown-group arm and not `available` having gone silent.
    let repo = two_workflow_repo();
    let g = switchable_group(&reg, &repo);
    assert_eq!(reg.workflow_status(&g.id)["available"], json!(["b", "default"]));
}

#[test]
fn a_no_op_reapply_still_reconciles_the_group_dir() {
    // Premortem 2. `write_instruction_files` is audited rather than propagated,
    // so an apply whose write failed leaves the switch live with a stale or
    // missing `<id>.md` — and the obvious human response, applying the same
    // unedited file again, used to take the early return and never touch the
    // disk. Nothing else self-heals it: the per-spawn render writes a new
    // block's file but never REMOVES a departed one.
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let g = switchable_group(&reg, &repo);
    reg.apply_workflow(&g.id, &wf("b"), None, "human").unwrap();
    let dir = reg.state_root().join(g.id.as_str());
    assert!(dir.join("w-b.md").is_file(), "precondition: the apply wrote it");

    // Stand in for the write that failed: the file the last render owned is gone.
    fs::remove_file(dir.join("w-b.md")).unwrap();

    // The no-op arm — same name, unedited file — must still reconcile.
    reg.apply_workflow(&g.id, &wf("b"), None, "human").unwrap();
    assert!(
        dir.join("w-b.md").is_file(),
        "re-applying the same unedited workflow is what retries a reconciliation that failed"
    );
}
#[test]
fn workflow_status_recomputes_satisfiability_live_against_drifted_gate_state() {
    // The #316 incident, reproduced directly: a group's persisted `merge_gate`
    // file names reviewers the CURRENT roster cannot spawn — group.json and the
    // gate file came from different moments (whatever produced the drift: a
    // relaunch that reset the roster but kept the last-known gate, a hand-edit,
    // anything). `workflow_status` must recompute satisfiability fresh against
    // the roster that is ACTUALLY running, every time — never trust a cached
    // flag from whenever the gate was armed — and say so loudly rather than
    // report SATISFIED while the shim would refuse every merge.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap(); // built-in 4-block roster
    fs::write(
        reg.state_root().join(g.id.as_str()).join("merge_gate"),
        "require all-pass\nreviewer rev-orch\nreviewer rev-ui\nreviewer rev-tests\n",
    )
    .unwrap();

    let status = reg.workflow_status(&g.id);
    assert_eq!(
        status["gate"]["satisfiable"], json!(false),
        "the built-in roster can spawn none of the three named reviewers: {status}"
    );
    let missing: Vec<String> = status["gate"]["missing_blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(missing, vec!["rev-orch", "rev-ui", "rev-tests"]);
}

#[test]
fn workflow_status_shapes_differ_for_a_workflow_group_and_a_builtin_group() {
    let (reg, _d) = test_registry();
    let repo = gated_repo("");
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();

    let off = reg.workflow_status(&g.id);
    assert_eq!(off["advanced"], json!(false));
    assert_eq!(off["gate"], Value::Null);
    assert_eq!(off["blocks"].as_array().unwrap().len(), 4, "the built-in roster: {off}");

    reg.set_advanced_orchestrator(&g.id, true, "human").unwrap();
    let on = reg.workflow_status(&g.id);
    assert_eq!(on["advanced"], json!(true));
    assert_eq!(on["name"], json!("focused-review"));
    assert!(on["gate"].is_object());
    assert_eq!(on["gate"]["satisfiable"], json!(true));
    let on_blocks: Vec<&str> =
        on["blocks"].as_array().unwrap().iter().map(|b| b["id"].as_str().unwrap()).collect();
    assert!(on_blocks.contains(&"rev-security") && on_blocks.contains(&"rev-tests"));
    for b in on["blocks"].as_array().unwrap() {
        assert!(
            b.get("kind").is_some() && b.get("cli").is_some() && b.get("model").is_some()
                && b.get("persona").is_some(),
            "every block in the wire shape must carry id/kind/cli/model/persona: {b}"
        );
    }
}

// ───────── #1689 slice C: `list_blocks`, the orchestrator's read-back ─────────
//
// The switch notice is a delivery, not a record — a compact can take it, while
// the roster a switch installed outlives any one delivery. The durable half is
// this orchestrator-only read of the ACTIVE roster, driven here through the real
// `dispatch()` so both gates (role, membership) are the ones that actually run.
// The teaching paragraph rides templates/workflow.md — unpinned, behind
// `workflow_section`'s `roster_is_custom` gate — and its silence on a built-in
// group is pinned beside the tool tests, on the same fixture the refusals run
// against.

/// The Caller for the orchestrator `switchable_group` spawned — that helper
/// keeps the AgentEntry to itself, so resolve the row the roster still carries.
fn orch_caller_of(reg: &OrchRegistry, g: &GroupInfo) -> Caller {
    let id = reg
        .list_agents(&g.id)
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["role"] == json!("orchestrator"))
        .and_then(|a| a["id"].as_str())
        .unwrap_or_else(|| panic!("no orchestrator in the roster"))
        .to_string();
    let a = reg.agent(&id).unwrap();
    reg.resolve_token(&a.token).unwrap()
}

fn list_blocks_call(reg: &OrchRegistry, caller: &Caller) -> Value {
    dispatch(reg, caller, "tools/call", &json!({ "name": "list_blocks", "arguments": {} })).unwrap()
}

#[test]
fn list_blocks_after_an_apply_returns_the_new_roster() {
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let g = switchable_group(&reg, &repo);
    let co = orch_caller_of(&reg, &g);

    // Before the switch the read-back answers with the roster the group
    // launched on — the pre-switch state is part of the contract, not setup.
    // Driven through dispatch like everything else in this section, so the
    // tests-first commit compiled and ran against the tree WITHOUT the tool and
    // reddened on `unknown tool: list_blocks` — a behavioral red, not a
    // compile error masking one.
    //
    // The declared ids are compared in the file's own order AFTER filtering the
    // `orchestrator` block `clamped()` synthesizes when a roster declares none
    // (workflow.rs: a roster with no orchestrator *kind* gets one, id
    // `orchestrator`) — that row is engine behavior this slice's fixture shares
    // with every other two-workflow test, and the red run at 876c8265 caught the
    // first cut asserting the unfiltered exact vec.
    let declared_ids = |v: &Value| -> Vec<String> {
        v["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["id"].as_str().unwrap().to_string())
            .filter(|id| id != "orchestrator")
            .collect()
    };
    let before_raw = list_blocks_call(&reg, &co);
    assert_eq!(before_raw["isError"], json!(false), "{before_raw}");
    let before: Value =
        serde_json::from_str(before_raw["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(before["workflow"], json!("default"), "{before}");
    assert_eq!(declared_ids(&before), vec!["w-a", "rev-a"], "{before}");

    reg.apply_workflow(&g.id, &wf("b"), None, "human").unwrap();

    // Through the real dispatch, so the role gate and the membership gate are
    // the shipped ones, not a bypass of them.
    let after = list_blocks_call(&reg, &co);
    assert_eq!(after["isError"], json!(false), "{after}");
    let text: Value =
        serde_json::from_str(after["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["workflow"], json!("b"), "{text}");
    assert_eq!(text["name"], json!("focused-b"), "the file's own name: field: {text}");
    let rows = text["blocks"].as_array().unwrap();
    assert_eq!(
        declared_ids(&text),
        vec!["w-b", "rev-b"],
        "the NEW roster's declared blocks, in the file's own order: {text}"
    );
    // The wire rows are the same `roster_json` shape `workflow_status`
    // publishes — that is what makes the read-back comparable against the
    // notice and the human's status payload with no second vocabulary.
    let by_id = |id: &str| rows.iter().find(|b| b["id"] == json!(id)).unwrap_or_else(|| panic!("{id}: {text}"));
    for b in rows {
        assert!(
            b.get("kind").is_some() && b.get("cli").is_some() && b.get("model").is_some()
                && b.get("persona").is_some(),
            "every row must carry id/kind/cli/model/persona: {b}"
        );
    }
    assert_eq!(by_id("rev-b")["persona"], json!(true), "rev-b declares a prompt: {text}");
    assert_eq!(
        by_id("w-b")["persona"], json!(false),
        "presence, not content — w-b declares only a model: {text}"
    );
    assert_eq!(by_id("w-b")["model"], json!("sonnet"), "{text}");
}

#[test]
fn list_blocks_refuses_a_reviewer_caller_and_stays_unlisted_to_delegates() {
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let g = switchable_group(&reg, &repo);
    let rev = reg.spawn_agent(&g.id, Role::Reviewer, "rev", "t", false, None).unwrap();
    let cr = reg.resolve_token(&rev.token).unwrap();

    // The real gate: a reviewer's CALL is refused, with the orchestrator-only
    // wording every require_orchestrator refusal carries.
    let denied = list_blocks_call(&reg, &cr);
    assert_eq!(denied["isError"], true, "{denied}");
    assert!(
        denied["content"][0]["text"].as_str().unwrap().contains("orchestrator-only"),
        "{denied}"
    );

    // …and the cosmetic half: the listing agrees, so a delegate is not even
    // offered the tool.
    let listed = dispatch(&reg, &cr, "tools/list", &json!({})).unwrap();
    let names: Vec<&str> =
        listed["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(!names.contains(&"list_blocks"), "{names:?}");
    let orch_listed = dispatch(&reg, &orch_caller_of(&reg, &g), "tools/list", &json!({})).unwrap();
    let orch_names: Vec<&str> =
        orch_listed["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(orch_names.contains(&"list_blocks"), "{orch_names:?}");
}

#[test]
fn list_blocks_refuses_a_caller_whose_membership_does_not_check_out() {
    // `require_in_group` on the CALLER'S OWN id, because there is no target
    // argument to resolve: a forged or mis-routed caller — agent id from one
    // group, token claiming another — is refused with the same unknown-agent
    // wording every target-taking tool uses, so the refusal leaks no other
    // group's roster. The caller is built by hand the same way the liaison
    // test builds its smuggled one: no producer reaches this state today, and
    // the pin is against one that does (the remote-engine daemon).
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let g = switchable_group(&reg, &repo);
    let co = orch_caller_of(&reg, &g);
    let other = reg.create_group("C:/tmp/other-repo", rails()).unwrap();

    let smuggled = Caller {
        agent_id: co.agent_id.clone(),
        group: other.id.clone(),
        role: Role::Orchestrator,
        role_hint: None,
    };
    let denied = list_blocks_call(&reg, &smuggled);
    assert_eq!(denied["isError"], true, "{denied}");
    assert!(
        denied["content"][0]["text"].as_str().unwrap().contains("unknown agent"),
        "cross-group must be indistinguishable from nonexistent — no id leak: {denied}"
    );

    // Same wording for an id that is in no registry at all — the second arm of
    // the same gate, refused before the group is ever consulted.
    let unregistered = Caller {
        agent_id: "no-such-agent".into(),
        group: g.id.clone(),
        role: Role::Orchestrator,
        role_hint: None,
    };
    let denied = list_blocks_call(&reg, &unregistered);
    assert_eq!(denied["isError"], true, "{denied}");
    assert!(
        denied["content"][0]["text"].as_str().unwrap().contains("unknown agent"),
        "{denied}"
    );
}

#[test]
fn the_workflow_kickoff_teaches_the_notice_and_the_read_back_only_when_a_roster_is_declared() {
    // The `advisor_and_process_prose_stays_silent_unless_a_block_declares_the_hint`
    // rule applied to slice C: a group running the built-in roster must not read
    // a word about a switch notice or a read-back it will never receive, and the
    // pinned orchestrator template is untouched — the sentence rides
    // templates/workflow.md (deliberately not fixture-pinned) behind
    // `workflow_section`'s `roster_is_custom` gate.
    let (reg, _d) = test_registry();
    let repo = two_workflow_repo();
    let g = reg
        .create_group(&repo.path().to_string_lossy(), advanced_rails())
        .unwrap();
    let custom = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("orchestrator.md"))
        .unwrap();
    assert!(custom.contains("[orrerix] workflow switched:"), "{custom}");
    assert!(custom.contains("list_blocks()"), "{custom}");
    assert!(!custom.contains("{{"), "{custom}");

    // The built-in roster: silent on both, and the placeholder gate left
    // nothing raw behind.
    let (reg2, _d2) = test_registry();
    let g2 = reg2.create_group("C:/tmp/repo", rails()).unwrap();
    let builtin =
        fs::read_to_string(reg2.state_root().join(g2.id.as_str()).join("orchestrator.md")).unwrap();
    assert!(!builtin.contains("list_blocks"), "{builtin}");
    assert!(!builtin.contains("workflow switched"), "{builtin}");

    // The TOOL is the other half of the docs sentence, and it is deliberately
    // NOT gated: `tool_defs` is never handed the roster, so a built-in group's
    // orchestrator IS offered `list_blocks` (rev-final round 3 — the first cut
    // of the docs claimed otherwise, which the listing structurally cannot do).
    // Pin the listing half so a future attempt to gate it reddens rather than
    // silently making the sentence true again…
    let orch2 = reg2.spawn_agent(&g2.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co2 = reg2.resolve_token(&orch2.token).unwrap();
    let listed = dispatch(&reg2, &co2, "tools/list", &json!({})).unwrap();
    let names: Vec<&str> =
        listed["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"list_blocks"), "{names:?}");
    // …and the answer half, because the docs sentence now promises the built-in
    // group gets the truth rather than a refusal: `workflow: "default"` with
    // the built-in four-block roster, the group not in workflow mode.
    let honest = list_blocks_call(&reg2, &co2);
    assert_eq!(honest["isError"], json!(false), "{honest}");
    let answer: Value =
        serde_json::from_str(honest["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(answer["workflow"], json!("default"), "{answer}");
    assert_eq!(answer["advanced"], json!(false), "{answer}");
    assert_eq!(answer["blocks"].as_array().unwrap().len(), 4, "{answer}");
}

// ───────── #385: merge-gate hot-reload ─────────
//
// Before this fix, `sync_merge_gate` only ever ran at group create/resume and
// on the live `set_advanced_orchestrator` toggle (#316) — so an in-place edit
// to an ALREADY-RUNNING group's `.loomux/workflow.yml` never took effect until
// one of those two discrete events happened again. `run_workflow_gate_reload`
// is the new trigger: a periodic background pass (`reload_merge_gate_if_
// changed`, called on a timer by `start_workflow_gate_reload`) that re-derives
// each advanced-orchestrator group's gate from the CURRENT file and re-arms it
// through the exact same `sync_merge_gate` seam the other two callers use —
// these tests call it directly rather than sleeping through the real
// `WORKFLOW_GATE_POLL_INTERVAL`, the same "drive the tick function, don't wait
// on the thread" pattern `idle_tick_tick`/`run_disk_monitor` already use above.
//
// This is a SECURITY GATE: a reload path that can leave the gate OPEN on a
// malformed/unreadable/vanished file would be worse than the bug it fixes, so
// half of these pin the fail-closed side as hard as the happy path.

/// Overwrite a gated repo's workflow file in place — the test's stand-in for
/// a human (or an editor's atomic save) editing `.loomux/workflow.yml` while
/// the group is already running.
fn edit_workflow(repo: &Path, contents: &str) {
    fs::write(repo.join(".loomux").join("workflow.yml"), contents).unwrap();
}

#[test]
fn run_workflow_gate_reload_picks_up_an_edited_also_clause_without_a_relaunch() {
    // The literal #385 repro: the human edits workflow.yml to DROP the
    // `also: [ci-green]` clause mid-session. No relaunch, no toggle — the next
    // reload pass alone must reflect the edit.
    let (reg, d, repo, gid) = gated_group("    also: [ci-green]\n");
    let before = reg.merge_gate(&gid).expect("armed at launch");
    assert_eq!(before.also, vec!["ci-green".to_string()], "test setup sanity");

    edit_workflow(
        repo.path(),
        "version: 1\nname: focused-review\n\
         blocks:\n\
         \x20 - id: worker\n    kind: worker\n\
         \x20 - id: rev-security\n    kind: reviewer\n    prompt: Security only.\n\
         \x20 - id: rev-tests\n    kind: reviewer\n    prompt: Test quality only.\n\
         gates:\n  merge:\n    reviewers: [rev-security, rev-tests]\n",
    );
    reg.run_workflow_gate_reload();

    let after = reg.merge_gate(&gid).expect("still armed — just the also: clause dropped");
    assert!(after.also.is_empty(), "the edit removed ci-green — the reload must reflect that: {after:?}");
    assert_eq!(after.reviewers, vec!["rev-security".to_string(), "rev-tests".to_string()]);
    let _ = &d;
}

#[test]
fn run_workflow_gate_reload_does_not_rewrite_or_reaudit_an_unchanged_file() {
    // The "simpler, no invalidation to get wrong" argument for re-deriving
    // instead of caching only holds if an UNCHANGED file really is a no-op —
    // otherwise every tick would spam `audit.jsonl` (#240 discipline) forever
    // for every advanced-orchestrator group, whether its file ever moves or
    // not. `reload_merge_gate_if_changed` compares against the gate already
    // armed before writing anything.
    let (reg, _d, _repo, gid) = gated_group("    also: [ci-green]\n");
    let declared_before =
        reg.audit_log(&gid).iter().filter(|e| e.action == "merge-gate-declared").count();
    assert_eq!(declared_before, 1, "one arm at launch");

    reg.run_workflow_gate_reload();
    reg.run_workflow_gate_reload();
    reg.run_workflow_gate_reload();

    let declared_after =
        reg.audit_log(&gid).iter().filter(|e| e.action == "merge-gate-declared").count();
    assert_eq!(declared_after, 1, "an unedited file must never re-declare the same gate");
}

#[test]
fn run_workflow_gate_reload_retains_the_last_known_gate_on_malformed_yaml() {
    // Fail-closed case 1 (explicitly called for by #385): a mid-edit or
    // mid-write can leave workflow.yml syntactically broken for one tick. That
    // must never read as "no gate" — a syntax error is not consent to widen
    // what a running session enforces, the same rule `create_group`'s
    // `merge-gate-retained` branch already applies at launch.
    let (reg, _d, repo, gid) = gated_group("");
    let before = reg.merge_gate(&gid).expect("armed at launch");

    edit_workflow(repo.path(), "version: 1\nname: broken\nblocks:\n  - id: mystery\n    kind: wizard\n");
    reg.run_workflow_gate_reload();

    assert_eq!(
        reg.merge_gate(&gid),
        Some(before),
        "a malformed file must retain the last-known gate, not clear or corrupt it"
    );
    assert!(reg.merge_gate_declared(&gid), "the gate file itself must still be present and usable");
}

#[test]
fn run_workflow_gate_reload_retains_the_gate_when_the_workflow_file_vanishes_mid_session() {
    // Fail-closed case 2, and the one place reload's semantics deliberately
    // diverge from a FRESH LAUNCH's: at launch, no workflow file is a
    // legitimate zero-config choice and clears any stale gate. Mid-session,
    // with a gate already armed, a file disappearing is indistinguishable
    // from an editor's ordinary unlink-then-recreate save — so it must NOT
    // read as "the repo just declared no gate". This is #385's explicit
    // "file deleted mid-session" fail-closed case.
    let (reg, _d, repo, gid) = gated_group("");
    let before = reg.merge_gate(&gid).expect("armed at launch");

    fs::remove_file(repo.path().join(".loomux").join("workflow.yml")).unwrap();
    reg.run_workflow_gate_reload();

    assert_eq!(
        reg.merge_gate(&gid),
        Some(before),
        "a vanished file mid-session must retain the last-known gate — only an explicit \
         toggle-off (or a fresh relaunch, where no file is a real choice) may clear it"
    );
    assert!(reg.merge_gate_declared(&gid));
}

#[test]
fn run_workflow_gate_reload_arms_a_gate_that_appears_mid_session() {
    // The strictly SAFE direction of drift — a file gaining a `gates.merge` it
    // didn't declare before — must also take effect live, and must audit when
    // the newly-named reviewers can't be satisfied by the running roster, the
    // same `merge-gate-unsatisfiable` check `set_advanced_orchestrator`'s live
    // toggle already runs (#316).
    let (reg, _d) = test_registry();
    let repo = tempfile::tempdir().unwrap();
    let loomux_dir = repo.path().join(".loomux");
    fs::create_dir_all(&loomux_dir).unwrap();
    fs::write(
        loomux_dir.join("workflow.yml"),
        "version: 1\nname: plain\nblocks:\n  - id: worker\n    kind: worker\n",
    )
    .unwrap();
    let g = reg
        .create_group(&repo.path().to_string_lossy(), Guardrails { advanced_orchestrator: true, ..rails() })
        .unwrap();
    assert!(!reg.merge_gate_declared(&g.id), "no gates.merge declared yet");

    // `rev-security` must be declared as a real `reviewer` block HERE — a
    // gate naming a block this same file doesn't declare fails to PARSE at
    // all (`parse_workflow`'s own validation), which would exercise the
    // fail-closed path above, not this one. The "unsatisfiable" case this
    // test wants is different: the file is perfectly valid, but the ROSTER
    // this reload deliberately never touches (roster reload is out of #385's
    // scope — see `reload_merge_gate_if_changed`'s doc comment) is still the
    // one-block roster from the FIRST launch, which never spawned a reviewer
    // at all.
    fs::write(
        loomux_dir.join("workflow.yml"),
        "version: 1\nname: plain\nblocks:\n  - id: worker\n    kind: worker\n\
         \x20 - id: rev-security\n    kind: reviewer\n    prompt: Security only.\n\
         gates:\n  merge:\n    reviewers: [rev-security]\n",
    )
    .unwrap();
    reg.run_workflow_gate_reload();

    assert!(reg.merge_gate_declared(&g.id), "the gate must arm the moment the file declares one");
    assert_eq!(reg.merge_gate(&g.id).unwrap().reviewers, vec!["rev-security".to_string()]);
    let entries = reg.audit_log(&g.id);
    assert!(
        entries.iter().any(|e| e.action == "merge-gate-unsatisfiable"),
        "the pinned one-block roster cannot spawn 'rev-security' — that must be audited, not silent: {entries:?}"
    );
}

#[test]
fn run_workflow_gate_reload_skips_groups_with_the_toggle_off() {
    // The gate is scoped to workflow mode (#229): a group that never turned it
    // on has no live gate to keep in sync, even if its repo's workflow.yml
    // declares one — reload must not reach past that boundary any more than
    // the launch path does.
    let (reg, _d) = test_registry();
    let repo = gated_repo("");
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    assert!(!g.guardrails.advanced_orchestrator);
    assert!(!reg.merge_gate_declared(&g.id));

    reg.run_workflow_gate_reload();

    assert!(!reg.merge_gate_declared(&g.id), "a plain group must never have a gate armed under it");
}

#[test]
fn run_workflow_gate_reload_skips_paused_groups() {
    // Same reasoning `watchdog_tick`/`idle_tick_tick` already apply to every
    // other background pass: a paused group is inactive by the human's own
    // choice, so it should not get live background work done to it either.
    let (reg, _d, repo, gid) = gated_group("    also: [ci-green]\n");
    reg.spawn_agent(&gid, Role::Orchestrator, "orch", "", false, None).unwrap();
    reg.pause_group(&gid).unwrap();
    let before = reg.merge_gate(&gid).unwrap();

    edit_workflow(
        repo.path(),
        "version: 1\nname: focused-review\n\
         blocks:\n\
         \x20 - id: worker\n    kind: worker\n\
         \x20 - id: rev-security\n    kind: reviewer\n    prompt: Security only.\n\
         \x20 - id: rev-tests\n    kind: reviewer\n    prompt: Test quality only.\n\
         gates:\n  merge:\n    reviewers: [rev-security, rev-tests]\n",
    );
    reg.run_workflow_gate_reload();

    assert_eq!(
        reg.merge_gate(&gid),
        Some(before),
        "a paused group must not be reloaded until the human resumes it"
    );
}

#[test]
fn run_workflow_gate_reload_retains_an_armed_gate_across_a_valid_yaml_prefix_missing_gates() {
    // #385/B1, the concrete finding: `gates` — and every field inside a
    // `Gate` — is `#[serde(default)]`, so cutting a well-formed workflow.yml
    // short right before its `gates:` key parses as a perfectly VALID
    // document, not garbage, not a YAML syntax error, that simply happens to
    // omit `gates.merge`. That is exactly what a truncate-then-write save or
    // a `git checkout` landing mid-flush produces, and it is byte-for-byte
    // indistinguishable — by type alone — from a human deliberately editing
    // the file to remove the gate entirely. Before the fix, the reloader
    // read that absence as consent and cleared an already-armed gate for up
    // to one poll interval; the fix is a POLICY (absence never clears an
    // armed gate on the reload path), not a parser trick, so it must hold
    // whether this exact text arrived by truncation or by deliberate edit —
    // this test doesn't need to prove which one happened, only that neither
    // one opens the gate.
    let (reg, _d, repo, gid) = gated_group("");
    let before = reg.merge_gate(&gid).expect("armed at launch");

    let full = fs::read_to_string(repo.path().join(".loomux").join("workflow.yml")).unwrap();
    let cut_at = full.find("gates:").expect("test fixture must declare gates:");
    let truncated = full[..cut_at].to_string();
    // Prove the fixture is doing what it claims — a genuine, valid YAML
    // document, not garbage — or this test would silently be exercising the
    // already-covered `Err`/malformed-YAML path instead of the one #385/B1
    // is actually about.
    assert!(
        workflow::parse_workflow(&truncated).is_ok(),
        "the truncated prefix must still be well-formed YAML, not garbage: {truncated:?}"
    );
    edit_workflow(repo.path(), &truncated);

    reg.run_workflow_gate_reload();

    assert_eq!(
        reg.merge_gate(&gid),
        Some(before),
        "a valid document that never reaches gates.merge must retain the last-known gate — \
         truncated or deliberate, only an explicit toggle-off may clear an armed gate"
    );
}

#[test]
fn run_workflow_gate_reload_never_clears_an_armed_gate_via_a_deliberately_gateless_rewrite() {
    // The same policy, pinned from the OTHER angle: even a document that is
    // unambiguously complete and well-formed, and unambiguously declares no
    // `gates:` at all (not a truncation artifact — a human just rewrote the
    // file), still does not clear an already-armed gate via the background
    // reload. #385/B1's fix does not try to tell "deliberate" and
    // "truncated" apart — it can't, from the bytes alone — so it treats them
    // the same on purpose. Clearing an armed gate is only reachable through
    // the explicit `set_advanced_orchestrator(..., false, ...)` toggle.
    let (reg, _d, repo, gid) = gated_group("");
    let before = reg.merge_gate(&gid).expect("armed at launch");

    edit_workflow(
        repo.path(),
        "version: 1\nname: focused-review\n\
         blocks:\n\
         \x20 - id: worker\n    kind: worker\n\
         \x20 - id: rev-security\n    kind: reviewer\n    prompt: Security only.\n\
         \x20 - id: rev-tests\n    kind: reviewer\n    prompt: Test quality only.\n",
    );
    reg.run_workflow_gate_reload();

    assert_eq!(
        reg.merge_gate(&gid),
        Some(before),
        "a background reload must never be the thing that clears an armed gate to none"
    );

    // The explicit path still works — this is a policy about WHO may clear
    // the gate, not a claim that it can never be cleared at all.
    reg.set_advanced_orchestrator(&gid, false, "human").unwrap();
    assert!(!reg.merge_gate_declared(&gid), "the toggle-off must still clear it");
}

fn gateless_workflow_text() -> &'static str {
    "version: 1\nname: focused-review\n\
     blocks:\n\
     \x20 - id: worker\n    kind: worker\n\
     \x20 - id: rev-security\n    kind: reviewer\n    prompt: Security only.\n\
     \x20 - id: rev-tests\n    kind: reviewer\n    prompt: Test quality only.\n"
}

#[test]
fn reload_never_rearms_a_gate_after_the_toggle_has_already_turned_it_off() {
    // #385: the toggle-off race, from rev-33's review. `run_workflow_gate_
    // reload`'s outer filter snapshots `advanced_orchestrator` before
    // iterating every group; if a toggle-off completes on ANOTHER thread
    // (the human clicking the groupview button, handled by a separate Tauri
    // command) between that snapshot and this group's own turn, the read +
    // parse `reload_merge_gate_if_changed` does could finish AFTER the
    // toggle-off and try to re-arm the gate it just cleared — wedging a
    // gate back onto a group that explicitly turned workflow mode off.
    //
    // A genuine cross-thread race isn't deterministically reproducible in a
    // test, so this pins the actual FIX instead: call the reload directly,
    // as if the outer filter's snapshot were already stale, against a group
    // whose guardrails NOW say the toggle is off. The recheck inside
    // `reload_merge_gate_if_changed` must see that and refuse to write,
    // regardless of what the file on disk still says.
    let (reg, _d, repo, gid) = gated_group("");
    assert!(reg.merge_gate_declared(&gid), "test setup sanity");

    reg.set_advanced_orchestrator(&gid, false, "human").unwrap();
    assert!(!reg.merge_gate_declared(&gid), "the toggle-off must clear it first");

    // The file on disk still declares the gate untouched — exactly what a
    // reload tick sandwiched right after the toggle-off would see.
    reg.reload_merge_gate_if_changed(&gid);

    assert!(
        !reg.merge_gate_declared(&gid),
        "a reload must never re-arm a gate for a group whose guardrails now say workflow mode is off"
    );
    let _ = &repo;
}

#[test]
fn reload_audits_a_silently_ignored_removal_exactly_once() {
    // rev-33: silence about a deliberate action is a failure mode this repo
    // keeps re-learning. A human who deletes gates.merge, saves, and sees
    // NOTHING happen will reasonably conclude the reload is broken. One
    // audit line makes "I removed it and nothing happened" answerable from
    // the trail — but it must fire ONCE per transition, not every tick the
    // file happens to stay gateless (that would just be log spam, the exact
    // thing `run_workflow_gate_reload_does_not_rewrite_or_reaudit_an_
    // unchanged_file` already guards against for the normal case).
    let (reg, _d, repo, gid) = gated_group("");
    edit_workflow(repo.path(), gateless_workflow_text());

    reg.run_workflow_gate_reload();
    let warns = |reg: &OrchRegistry| {
        reg.audit_log(&gid).iter().filter(|e| e.action == "merge-gate-removal-ignored").count()
    };
    assert_eq!(warns(&reg), 1, "must audit the transition exactly once");

    // Repeated ticks against the SAME still-gateless file must stay quiet.
    reg.run_workflow_gate_reload();
    reg.run_workflow_gate_reload();
    assert_eq!(warns(&reg), 1, "must not re-audit every tick while the file stays gateless");
}

#[test]
fn reload_removal_audit_relatches_after_the_gate_returns_and_is_removed_again() {
    // The latch must reset when the group leaves the "ignored removal"
    // state, so a SECOND, later, genuinely new removal is not swallowed by
    // the first one's now-stale latch.
    let (reg, _d, repo, gid) = gated_group("");
    edit_workflow(repo.path(), gateless_workflow_text());
    reg.run_workflow_gate_reload();
    let warns = |reg: &OrchRegistry| {
        reg.audit_log(&gid).iter().filter(|e| e.action == "merge-gate-removal-ignored").count()
    };
    assert_eq!(warns(&reg), 1);

    // The gate comes back (a further edit re-adds it) — this must clear the
    // latch, and must NOT itself count as a removal.
    edit_workflow(
        repo.path(),
        "version: 1\nname: focused-review\n\
         blocks:\n\
         \x20 - id: worker\n    kind: worker\n\
         \x20 - id: rev-security\n    kind: reviewer\n    prompt: Security only.\n\
         \x20 - id: rev-tests\n    kind: reviewer\n    prompt: Test quality only.\n\
         gates:\n  merge:\n    reviewers: [rev-security, rev-tests]\n",
    );
    reg.run_workflow_gate_reload();
    assert!(reg.merge_gate_declared(&gid), "the gate must re-arm once the file names it again");
    assert_eq!(warns(&reg), 1, "re-arming is not a removal — the count must not move");

    // Removed again — a genuinely NEW transition, must audit again.
    edit_workflow(repo.path(), gateless_workflow_text());
    reg.run_workflow_gate_reload();
    assert_eq!(warns(&reg), 2, "a later, distinct removal must audit again, not stay silent forever");
}
