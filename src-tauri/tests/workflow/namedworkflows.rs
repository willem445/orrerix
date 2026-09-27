//! Named workflows: the group's ACTIVE file, and the list and preview commands (#1689).
//!
//! One module of the `workflow` integration-test target (`main.rs`),
//! split out of the former single-file `tests/workflow.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ───────────────── named workflows: the ACTIVE file (#1689) ─────────────────
//
// A group pins the NAME of the workflow it runs (`Guardrails.workflow`), and
// every group-scoped reader resolves through `load_active_workflow` /
// `active_workflow_path` rather than through `workflow::load_workflow`, which
// answers only for `default`.
//
// **Every test in this section carries the same negative control**, and it is
// the point of the section rather than a garnish: the repo also has a
// `.orrerix/workflow.yml` declaring something DIFFERENT, so a reader that had
// silently stayed on the default file reads the wrong value and fails. A test
// that only wrote `workflows/b.yml` would pass just as well against the
// pre-#1689 code for most of these readers, because `Ok(None)` and "the default
// is what I wanted" produce the same answer.

/// A workflow named `b`, declaring one of everything a group-scoped reader
/// asks about, with values chosen so none of them coincides with a default.
const B_WORKFLOW: &str = r#"
version: 1
name: bee
blocks:
  - id: orch
    kind: orchestrator
  - id: w
    kind: worker
  - id: rev
    kind: reviewer
merge_queue:
  enabled: true
  max_batch: 3
resources:
  buildbox:
    slots: 1
board:
  wip:
    review: 2
gates:
  merge:
    require: all-pass
    reviewers: [rev]
"#;

/// The DECOY: `.orrerix/workflow.yml`, declaring the opposite of `b` on every
/// axis. A group pinned to `b` must never read a single value out of this file.
const DECOY_WORKFLOW: &str = r#"
version: 1
name: the-decoy
blocks:
  - id: orch
    kind: orchestrator
  - id: decoy-worker
    kind: worker
  - id: decoy-rev
    kind: reviewer
merge_queue:
  enabled: false
board:
  wip:
    review: 9
"#;

/// A repo declaring BOTH: `b` under `.orrerix/workflows/`, and the decoy at
/// `.orrerix/workflow.yml`.
fn repo_with_b_and_a_decoy() -> Repo {
    let repo = Repo::new();
    let dir = repo.repo.join(".orrerix").join("workflows");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("b.yml"), B_WORKFLOW).unwrap();
    fs::write(repo.repo.join(".orrerix").join("workflow.yml"), DECOY_WORKFLOW).unwrap();
    repo
}

/// `rails()`, pinned to the workflow named `b`.
fn rails_on_b() -> Guardrails {
    Guardrails { workflow: workflow::WorkflowName::parse("b").unwrap(), ..rails() }
}

/// Overwrite the DECOY mid-session, so a reader that re-reads the wrong file
/// picks up a value nothing else in the tree carries.
fn poison_the_decoy(repo: &Repo) {
    let poisoned = DECOY_WORKFLOW
        .replace("review: 9", "review: 7")
        .replace("enabled: false", "enabled: true");
    fs::write(repo.repo.join(".orrerix").join("workflow.yml"), poisoned).unwrap();
}

#[test]
fn a_group_pinned_to_a_named_workflow_persists_that_name_and_resumes_on_it() {
    let repo = repo_with_b_and_a_decoy();
    let (reg, dir) = test_registry();
    let g = reg.create_group(&repo.path(), rails_on_b()).unwrap();

    // The roster came from `b`, not from the decoy sitting next to it.
    let ids: Vec<&str> = g.guardrails.blocks.iter().map(|b| b.id.as_str()).collect();
    assert!(ids.contains(&"w"), "the roster is b's: {ids:?}");
    assert!(!ids.contains(&"decoy-worker"), "and NOT the decoy's: {ids:?}");

    // …and the name is in `group.json`, beside the roster it produced.
    let persisted: Value = serde_json::from_str(
        &fs::read_to_string(dir.path().join(g.id.as_str()).join("group.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(persisted["guardrails"]["workflow"], "b");

    // A resume reads it back and comes up on b, with nothing from a launcher.
    let reg2 = relaunch_registry(dir.path());
    let (_, rails) = reg2.load_group_file(&g.id).expect("group.json");
    assert_eq!(rails.workflow.as_str(), "b");
    let resumed = reg2.create_group_ex(&repo.path(), rails, Launch::Resume).unwrap();
    let ids: Vec<&str> = resumed.guardrails.blocks.iter().map(|b| b.id.as_str()).collect();
    assert!(ids.contains(&"w") && !ids.contains(&"decoy-worker"), "{ids:?}");
}

#[test]
fn a_group_json_written_before_named_workflows_reads_as_default() {
    let repo = Repo::new().workflow(DECOY_WORKFLOW);
    let (reg, dir) = test_registry();
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let path = dir.path().join(g.id.as_str()).join("group.json");

    // Strip the key entirely, exactly as a group.json written before #1689 is.
    let mut v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        v["guardrails"].as_object_mut().unwrap().remove("workflow").is_some(),
        "positive control: the key must be THERE before this test removes it, or the \
         assertion below says nothing about the migration"
    );
    fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();

    let (_, rails) = relaunch_registry(dir.path()).load_group_file(&g.id).expect("group.json");
    assert_eq!(
        rails.workflow.as_str(),
        "default",
        "an absent key is the one file that has always been there"
    );

    // And a value no `WorkflowName` can be made of falls back the same way,
    // rather than making the group unrejoinable — or reaching a path join.
    let mut v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    v["guardrails"]["workflow"] = json!("../escaped");
    fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    let (_, rails) = relaunch_registry(dir.path()).load_group_file(&g.id).expect("group.json");
    assert_eq!(
        rails.workflow.as_str(),
        "default",
        "a hand-edited traversal is refused, not joined"
    );
}

#[test]
fn workflow_status_reads_the_groups_own_file_and_not_the_default_one() {
    let repo = repo_with_b_and_a_decoy();
    let (reg, _dir) = test_registry();
    let g = reg.create_group(&repo.path(), rails_on_b()).unwrap();

    assert_eq!(reg.workflow_status(&g.id)["name"], "bee", "the display name is b's");
    poison_the_decoy(&repo);
    assert_eq!(
        reg.workflow_status(&g.id)["name"],
        "bee",
        "and an edit to `.orrerix/workflow.yml` does not move it"
    );
}

#[test]
fn the_board_policy_reads_the_groups_own_file_and_not_the_default_one() {
    let repo = repo_with_b_and_a_decoy();
    let (reg, _dir) = test_registry();
    let g = reg.create_group(&repo.path(), rails_on_b()).unwrap();

    let limit_of = |reg: &OrchRegistry, id: &GroupId| -> Option<u64> {
        reg.wip_status_for_agents(id)
            .iter()
            .find(|r| r["status"] == "review")
            .and_then(|r| r["limit"].as_u64())
    };
    assert_eq!(limit_of(&reg, &g.id), Some(2), "b declares `review: 2`");
    poison_the_decoy(&repo);
    assert_eq!(limit_of(&reg, &g.id), Some(2), "and the decoy's `review: 7` is not read");
}

#[test]
fn the_lock_resources_read_the_groups_own_file_and_not_the_default_one() {
    let repo = repo_with_b_and_a_decoy();
    let (reg, _dir) = test_registry();
    let g = reg.create_group(&repo.path(), rails_on_b()).unwrap();

    let names = |reg: &OrchRegistry, id: &GroupId| -> Vec<String> {
        reg.lock_state(id)["resources"]
            .as_array()
            .map(|a| a.iter().filter_map(|r| r["name"].as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    assert_eq!(names(&reg, &g.id), vec!["buildbox".to_string()], "b declares one resource");
    poison_the_decoy(&repo);
    assert_eq!(
        names(&reg, &g.id),
        vec!["buildbox".to_string()],
        "and the decoy — which declares none — is not what is being read"
    );
}

#[test]
fn the_merge_queue_policy_reads_the_groups_own_file_and_not_the_default_one() {
    let repo = repo_with_b_and_a_decoy();
    let (reg, _dir) = test_registry();
    let g = reg.create_group(&repo.path(), rails_on_b()).unwrap();

    assert_eq!(
        reg.merge_queue_status(&g.id)["enabled"],
        json!(true),
        "b enables the queue; the decoy disables it"
    );
    poison_the_decoy(&repo);
    assert_eq!(reg.merge_queue_status(&g.id)["enabled"], json!(true));
}

#[test]
fn the_gate_reload_re_arms_the_groups_own_file_and_not_the_default_one() {
    let repo = repo_with_b_and_a_decoy();
    let (reg, _dir) = test_registry();
    let g = reg.create_group(&repo.path(), rails_on_b()).unwrap();
    let gate = reg.merge_gate(&g.id).expect("b declares a merge gate at launch");
    assert_eq!(gate.reviewers, vec!["rev".to_string()]);

    // Give the DECOY a gate the group must never adopt, and edit b's own file
    // so the reload has something real to pick up. The reload is what proves
    // WHICH file it re-reads: a reader still on `.orrerix/workflow.yml` would
    // take the decoy's reviewer instead of b's.
    let decoy_gate = "gates:\n  merge:\n    require: all-pass\n    reviewers: [decoy-rev]\n";
    fs::write(
        repo.repo.join(".orrerix").join("workflow.yml"),
        format!("{DECOY_WORKFLOW}{decoy_gate}"),
    )
    .unwrap();
    let b_edited = B_WORKFLOW
        .replace(
            "  - id: rev\n    kind: reviewer\n",
            "  - id: rev\n    kind: reviewer\n  - id: rev2\n    kind: reviewer\n",
        )
        .replace("reviewers: [rev]", "reviewers: [rev, rev2]");
    fs::write(repo.repo.join(".orrerix").join("workflows").join("b.yml"), &b_edited).unwrap();

    reg.reload_merge_gate_if_changed(&g.id);
    let gate = reg.merge_gate(&g.id).expect("still armed");
    assert_eq!(
        gate.reviewers,
        vec!["rev".to_string(), "rev2".to_string()],
        "the reload took b's edit, and never the decoy's gate"
    );
}

#[test]
fn the_workflow_path_a_delegate_is_told_about_is_the_groups_own_file() {
    let repo = repo_with_b_and_a_decoy();
    let (reg, dir) = test_registry();
    let g = reg.create_group(&repo.path(), rails_on_b()).unwrap();

    // `{{WORKFLOW_PATH}}` renders into `workflow.md` (the orchestrator's
    // workflow section) and `block.md` (a delegate's). Neither is one of the
    // four `pre222`-pinned role templates, which is why this text may move at
    // all — see `tests/fixtures/pre222/README.md`.
    let group_dir = dir.path().join(g.id.as_str());
    let mut named = 0usize;
    let mut scanned = 0usize;
    for entry in fs::read_dir(&group_dir).unwrap().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        scanned += 1;
        let text = fs::read_to_string(&path).unwrap();
        if text.contains(".orrerix/workflows/b.yml") {
            named += 1;
        }
        assert!(
            !text.contains(".orrerix/workflow.yml"),
            "{} names the file this group is NOT running",
            path.display()
        );
    }
    // Positive controls on that absence assertion: instruction files were
    // written at all, and at least one really does name b's file — an empty
    // group dir would satisfy the loop above just as well.
    assert!(scanned > 0, "the group dir has instruction files");
    assert!(named > 0, "and at least one names b's file");
}

// ───────────── named workflows: the two commands (#1689 slice A) ─────────────

#[test]
fn orch_workflow_list_reports_every_declared_workflow_with_its_findings() {
    let repo = repo_with_b_and_a_decoy();
    let v = loomux_lib::orchestration::orch_workflow_list_sync(repo.path());
    let rows = v["workflows"].as_array().unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["b", "default"], "sorted by name");
    assert_eq!(rows[0]["path"], ".orrerix/workflows/b.yml");
    assert_eq!(rows[0]["display_name"], "bee");
    assert_eq!(rows[0]["valid"], json!(true));
    assert_eq!(rows[1]["path"], ".orrerix/workflow.yml");
    assert!(v["findings"].as_array().unwrap().is_empty());

    // A broken file is still offered, carrying why — with a positive control on
    // that `is_empty` above: this listing really did open both files.
    fs::write(
        repo.repo.join(".orrerix").join("workflows").join("b.yml"),
        "version: 1\nblocks: [[[\n",
    )
    .unwrap();
    let v = loomux_lib::orchestration::orch_workflow_list_sync(repo.path());
    let rows = v["workflows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["valid"], json!(false));
    assert!(!rows[0]["errors"].as_array().unwrap().is_empty());

    // And a repo with nothing declares nothing — which is not an error.
    let empty = loomux_lib::orchestration::orch_workflow_list_sync(Repo::new().path());
    assert!(empty["workflows"].as_array().unwrap().is_empty());
    assert!(empty["findings"].as_array().unwrap().is_empty());
}

#[test]
fn orch_workflow_preview_without_a_name_is_what_it_always_was() {
    let repo = repo_with_b_and_a_decoy();
    // The launcher's existing call, which sends no name at all.
    let default =
        loomux_lib::orchestration::orch_workflow_preview_sync(repo.path(), "claude".into(), None);
    assert_eq!(default["path"], ".orrerix/workflow.yml");
    assert_eq!(default["name"], "the-decoy");
    assert_eq!(default["present"], json!(true));
    // Naming `default` explicitly is the SAME call, which is what makes the
    // omitted case a default rather than a second code path.
    let named = loomux_lib::orchestration::orch_workflow_preview_sync(
        repo.path(),
        "claude".into(),
        Some("default".into()),
    );
    assert_eq!(named, default, "`None` and `Some(\"default\")` are one answer");

    // …and a name picks its own file.
    let b = loomux_lib::orchestration::orch_workflow_preview_sync(
        repo.path(),
        "claude".into(),
        Some("b".into()),
    );
    assert_eq!(b["path"], ".orrerix/workflows/b.yml");
    assert_eq!(b["name"], "bee");
    assert_ne!(b["blocks"], default["blocks"], "two files, two rosters");

    // A name this repo does not declare is ABSENCE, which is not invalidity.
    let missing = loomux_lib::orchestration::orch_workflow_preview_sync(
        repo.path(),
        "claude".into(),
        Some("nope".into()),
    );
    assert_eq!(missing["present"], json!(false));
    assert_eq!(missing["valid"], json!(true));
    assert_eq!(missing["path"], ".orrerix/workflows/nope.yml");

    // A name that is not a usable one is refused — and nothing untrusted is
    // echoed back into `path`, which is a display surface.
    let bad = loomux_lib::orchestration::orch_workflow_preview_sync(
        repo.path(),
        "claude".into(),
        Some("../escaped".into()),
    );
    assert_eq!(bad["present"], json!(false));
    assert_eq!(bad["valid"], json!(false), "a refused name is a finding, not silence");
    assert_eq!(bad["path"], "", "the raw name is not reflected into a path");
    assert!(
        bad["errors"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| !e.as_str().unwrap().contains("../escaped")),
        "nor into the errors: {:?}",
        bad["errors"]
    );
}
