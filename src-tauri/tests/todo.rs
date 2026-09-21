//! The To-Do store (#3263 slice S1): the model, the versioned single file, and
//! the workspace key.
//!
//! An integration test and a NEW file, both deliberately. Integration because
//! anything linking the lib has to be one on Windows (CLAUDE.md constraint 4 —
//! the comctl32-v6 manifest rides on `-tests`-scoped link args); a new file
//! because `tests/orchestration.rs` is the end-of-file-append conflict class,
//! and #3263's later slices append their own tests here.
//!
//! Every test drives the REAL host path (`orchestration::todo::apply_to` /
//! `load_store` / `snapshot_at`) against a file in a tempdir, rather than the
//! process-global data root: `todo_path()` reads `obs::data_root()`, which is
//! one value for the whole test binary, so a test that redirected it would be
//! redirecting it for every test running beside it.

use loomux_engine::pathseg;
use loomux_engine::todo::{
    self, Actor, OrderAfter, Scope, StepPatch, TodoAdd, TodoError, TodoOp, TodoStore, TodoUpdate,
    ITEMS_MAX, NOTES_MAX, ORDER_GAP, PURGE_AFTER_MS, STEPS_MAX, TAGS_MAX, TITLE_MAX,
};
use loomux_lib::orchestration::todo::{apply_to, load_store, snapshot_at, todo_path_in};
use std::path::{Path, PathBuf};

/// A fixed clock. Every test that cares about time states its own offsets from
/// this, so nothing depends on when the suite runs.
const T0: u64 = 1_700_000_000_000;

fn human() -> Actor {
    Actor::Human
}

fn agent() -> Actor {
    Actor::Agent {
        id: "a-7".to_string(),
        name: "worker-3".to_string(),
        group: "g-1".to_string(),
        role: "worker".to_string(),
    }
}

fn store_path(dir: &Path) -> PathBuf {
    dir.join("todo.json")
}

fn add(title: &str) -> TodoOp {
    TodoOp::Add(TodoAdd {
        scope: Scope::Global,
        title: title.to_string(),
        ..TodoAdd::default()
    })
}

/// `unwrap_err`, with the table row's field name in the panic message — a cap
/// row that unexpectedly SUCCEEDS should say which one.
fn unwrap_err_for(r: Result<todo::Applied, TodoError>, what: &str) -> TodoError {
    match r {
        Ok(_) => panic!("{what}: the cap should have refused this write"),
        Err(e) => e,
    }
}

fn read_raw(path: &Path) -> String {
    std::fs::read_to_string(path).expect("store file should exist")
}

// ---------- round trip ----------

#[test]
fn an_added_item_survives_a_write_and_a_read() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());

    let applied = apply_to(
        &path,
        TodoOp::Add(TodoAdd {
            scope: Scope::Workspace("c:/projects/loomux".to_string()),
            title: "ship the todo pane".to_string(),
            notes: Some("the first one".to_string()),
            due_ms: Some(T0 + 86_400_000),
            priority: Some(2),
            important: Some(true),
            tags: Some(vec!["infra".to_string()]),
            steps: Some(vec!["model".to_string(), "store".to_string()]),
            ..TodoAdd::default()
        }),
        &agent(),
        T0,
        Some(("c:/projects/loomux", r"C:\Projects\loomux")),
    )
    .expect("add should be accepted");

    let id = applied.ids[0].clone();
    // The id is `td-` + 16 hex and passes the one identifier gate this repo has
    // (CLAUDE.md constraint 6), so it stays usable if a later slice ever makes
    // it a path component.
    assert!(id.starts_with("td-"), "id should be td-prefixed, got {id}");
    assert_eq!(id.len(), 19, "td- plus 16 hex, got {id}");
    assert!(
        pathseg::check_segment(&id).is_ok(),
        "minted id must pass check_segment: {id}"
    );

    // Re-read from disk, not from the returned value: the point of the test is
    // that the bytes made it.
    let reread = load_store(&path);
    assert!(reread.readable);
    assert_eq!(reread.store.version, todo::CURRENT_VERSION);
    let item = reread
        .store
        .items
        .iter()
        .find(|i| i.id == id)
        .expect("item should be on disk");
    assert_eq!(item.title, "ship the todo pane");
    assert_eq!(item.notes, "the first one");
    assert_eq!(item.due_ms, Some(T0 + 86_400_000));
    assert_eq!(item.priority, 2);
    assert!(item.important);
    assert_eq!(item.tags, vec!["infra".to_string()]);
    assert_eq!(item.steps.len(), 2);
    assert_eq!(item.steps[0].title, "model");
    assert!(!item.steps[0].done);
    assert_eq!(item.rev, 1);
    assert_eq!(item.created_by, agent());
    assert_eq!(item.created_ms, T0);
    assert_eq!(item.scope, Scope::Workspace("c:/projects/loomux".to_string()));

    // The workspace record the scope switch reads.
    let ws = reread
        .store
        .workspaces
        .get("c:/projects/loomux")
        .expect("workspace should be recorded");
    assert_eq!(ws.label, "loomux");
    assert_eq!(ws.root, r"C:\Projects\loomux");
    assert_eq!(ws.first_seen_ms, T0);
}

#[test]
fn a_second_write_keeps_the_first_items() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    let first = apply_to(&path, add("one"), &human(), T0, None).unwrap().ids[0].clone();
    let second = apply_to(&path, add("two"), &human(), T0 + 1, None)
        .unwrap()
        .ids[0]
        .clone();

    let store = load_store(&path).store;
    let ids: Vec<&str> = store.items.iter().map(|i| i.id.as_str()).collect();
    assert!(ids.contains(&first.as_str()), "first item was lost: {ids:?}");
    assert!(ids.contains(&second.as_str()));
    // Gaps of ORDER_GAP, so a reorder is one field write.
    let live = store.live(&Scope::Global);
    assert_eq!(live[0].order, ORDER_GAP);
    assert_eq!(live[1].order, 2 * ORDER_GAP);
}

// ---------- version ----------

#[test]
fn a_newer_store_is_read_but_never_written() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    // A store from a build that writes version 2, carrying one item a v1 build
    // understands and one key it does not.
    std::fs::write(
        &path,
        r#"{
          "version": 2,
          "workspaces": {},
          "items": [{"id":"td-0000000000000001","title":"from the future","rev":9}],
          "recurrence_rules": []
        }"#,
    )
    .unwrap();

    // READ: the human still sees their list, and the pane is told it is
    // read-only rather than discovering it one refused keystroke at a time.
    let snap = snapshot_at(&path, None);
    assert_eq!(snap.version, 2);
    assert!(snap.read_only, "a newer store must be flagged read-only");
    assert_eq!(snap.items.len(), 1);
    assert_eq!(snap.items[0].title, "from the future");
    assert!(snap.quarantined.is_none(), "a newer store is NOT corrupt");

    // WRITE: refused, with the version in the message.
    let before = read_raw(&path);
    let err = apply_to(&path, add("mine"), &human(), T0, None).unwrap_err();
    assert_eq!(err, TodoError::NewerVersion(2));
    assert!(
        err.to_string().contains("version 2"),
        "message should name the version it found: {err}"
    );
    // And the bytes are untouched — the refusal is the whole guarantee.
    assert_eq!(read_raw(&path), before, "a refused write must not rewrite the file");
}

// ---------- corruption ----------

#[test]
fn a_file_that_is_not_json_is_quarantined_and_the_store_opens_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    std::fs::write(&path, "{\"version\": 1, \"items\": [ truncated").unwrap();

    let loaded = load_store(&path);
    let quarantine = loaded.quarantined.expect("corrupt file must be quarantined");
    assert_eq!(quarantine.file_name().unwrap(), "todo.corrupt.json");
    assert!(quarantine.exists(), "the evidence must survive under its own name");
    assert!(loaded.store.items.is_empty());
    assert!(
        !path.exists(),
        "the corrupt file is renamed aside, not copied"
    );

    // And a write is allowed afterwards, because nothing is at risk any more.
    apply_to(&path, add("fresh start"), &human(), T0, None).expect("write after quarantine");
    assert_eq!(load_store(&path).store.items.len(), 1);
    assert!(
        quarantine.exists(),
        "the new write must not disturb the quarantined evidence"
    );
}

#[test]
fn json_of_the_wrong_shape_is_quarantined_too() {
    // The half `uistate::load_or_quarantine` structurally cannot catch: a
    // perfectly valid JSON document that is not a store. It is only catchable
    // here because this is the one blob whose schema the backend owns.
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    std::fs::write(&path, r#"["not", "a", "store"]"#).unwrap();

    let loaded = load_store(&path);
    // The assertion is on the FILESYSTEM, not on the returned field. A
    // `quarantined: Some(path)` is only a CLAIM that a rename happened —
    // removing the rename and leaving the field is a mutation this test passed
    // when it read the field alone (#3263 S1 scratch, M9).
    let quarantine = loaded
        .quarantined
        .expect("valid JSON of the wrong shape is still a corrupt store");
    assert!(
        quarantine.exists(),
        "the evidence must exist under its own name, not merely be named"
    );
    assert!(!path.exists(), "the corrupt file is renamed aside, not copied");
    assert!(loaded.store.items.is_empty());
}

#[test]
fn a_store_that_cannot_be_read_declines_the_write_instead_of_replacing_it() {
    // The counterfactual the module doc promises ("I could not look" is not
    // "there was nothing there"). A DIRECTORY at the store's path is the
    // portable way to make a read fail with something other than NotFound.
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    std::fs::create_dir(&path).unwrap();

    let loaded = load_store(&path);
    assert!(!loaded.readable, "an unreadable store must not read as empty");
    assert!(
        loaded.quarantined.is_none(),
        "nothing is renamed: the bytes may be readable next time"
    );

    let err = apply_to(&path, add("would erase the list"), &human(), T0, None).unwrap_err();
    assert!(
        err.to_string().contains("refusing to overwrite"),
        "the refusal must say why: {err}"
    );
    assert!(path.is_dir(), "the refused write must have touched nothing");
}

// ---------- conflict ----------

#[test]
fn if_rev_refuses_a_stale_update_and_names_both_revs() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    let id = apply_to(&path, add("groom me"), &human(), T0, None).unwrap().ids[0].clone();

    // Someone else edits it first: rev 1 -> 2.
    apply_to(
        &path,
        TodoOp::Update(TodoUpdate {
            id: id.clone(),
            title: Some("groomed by the human".to_string()),
            ..TodoUpdate::default()
        }),
        &human(),
        T0 + 1,
        None,
    )
    .unwrap();

    // The agent still believes rev 1.
    let err = apply_to(
        &path,
        TodoOp::Update(TodoUpdate {
            id: id.clone(),
            if_rev: Some(1),
            title: Some("groomed by the agent".to_string()),
            ..TodoUpdate::default()
        }),
        &agent(),
        T0 + 2,
        None,
    )
    .unwrap_err();
    assert_eq!(err, TodoError::Conflict(id.clone(), 2, 1));
    assert_eq!(err.to_string(), format!("conflict: {id} is at rev 2 (you sent 1)"));

    // The human's title is intact — the refusal happened before any mutation.
    let store = load_store(&path).store;
    let item = store.items.iter().find(|i| i.id == id).unwrap();
    assert_eq!(item.title, "groomed by the human");
    assert_eq!(item.rev, 2);

    // An op carrying NO if_rev — which is every op the human's pane sends — is
    // never refused. This is the negative control: without it, "refuse
    // everything" would pass the assertion above.
    apply_to(
        &path,
        TodoOp::Update(TodoUpdate {
            id: id.clone(),
            title: Some("the human wins".to_string()),
            ..TodoUpdate::default()
        }),
        &human(),
        T0 + 3,
        None,
    )
    .expect("an op with no if_rev is never refused");

    // And the CORRECT rev is accepted, so the refusal is about staleness and
    // not about `if_rev` being present at all.
    apply_to(
        &path,
        TodoOp::Update(TodoUpdate {
            id: id.clone(),
            if_rev: Some(3),
            important: Some(true),
            ..TodoUpdate::default()
        }),
        &agent(),
        T0 + 4,
        None,
    )
    .expect("a matching if_rev is accepted");
}

// ---------- caps: every one REFUSES, none truncates ----------

/// Table-driven so a cap added later without a row here is a visible omission
/// rather than a silent one. Each row builds an op that exceeds exactly one
/// cap; the assertion is that the op is REFUSED **and** that nothing was
/// written — a truncating implementation would pass a refusal-only check by
/// failing for some other reason, and would pass a "value is at the cap" check
/// while silently losing the human's text.
#[test]
fn every_cap_refuses_rather_than_truncating() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());

    let over_title = "t".repeat(TITLE_MAX + 1);
    let over_notes = "n".repeat(NOTES_MAX + 1);
    let over_tags: Vec<String> = (0..=TAGS_MAX).map(|n| format!("tag{n}")).collect();
    let over_steps: Vec<String> = (0..=STEPS_MAX).map(|n| format!("step {n}")).collect();

    let rows: Vec<(&str, TodoAdd, TodoError)> = vec![
        (
            "title",
            TodoAdd {
                title: over_title.clone(),
                ..TodoAdd::default()
            },
            TodoError::Cap("title", TITLE_MAX),
        ),
        (
            "notes",
            TodoAdd {
                title: "ok".to_string(),
                notes: Some(over_notes.clone()),
                ..TodoAdd::default()
            },
            TodoError::Cap("notes", NOTES_MAX),
        ),
        (
            "tags",
            TodoAdd {
                title: "ok".to_string(),
                tags: Some(over_tags.clone()),
                ..TodoAdd::default()
            },
            TodoError::Cap("tags", TAGS_MAX),
        ),
        (
            "steps",
            TodoAdd {
                title: "ok".to_string(),
                steps: Some(over_steps.clone()),
                ..TodoAdd::default()
            },
            TodoError::Cap("steps", STEPS_MAX),
        ),
    ];

    for (field, spec, expected) in rows {
        let err = unwrap_err_for(apply_to(&path, TodoOp::Add(spec), &agent(), T0, None), field);
        assert_eq!(err, expected, "{field} cap");
        assert!(
            err.to_string().starts_with("refused: "),
            "{field}: a cap message must read as a refusal, got {err}"
        );
        assert!(
            !path.exists() || load_store(&path).store.items.is_empty(),
            "{field}: a refused add must write nothing at all"
        );
    }

    // The same caps on the UPDATE path — a guard present on one call site and
    // absent from its sibling is a bypass exactly the width of the asymmetry.
    let id = apply_to(&path, add("real item"), &human(), T0, None).unwrap().ids[0].clone();
    let rev_before = load_store(&path)
        .store
        .items
        .iter()
        .find(|i| i.id == id)
        .unwrap()
        .rev;

    for (field, up, expected) in vec![
        (
            "title",
            TodoUpdate {
                id: id.clone(),
                title: Some(over_title.clone()),
                ..TodoUpdate::default()
            },
            TodoError::Cap("title", TITLE_MAX),
        ),
        (
            "notes",
            TodoUpdate {
                id: id.clone(),
                notes: Some(over_notes.clone()),
                ..TodoUpdate::default()
            },
            TodoError::Cap("notes", NOTES_MAX),
        ),
        (
            "tags",
            TodoUpdate {
                id: id.clone(),
                tags: Some(over_tags.clone()),
                ..TodoUpdate::default()
            },
            TodoError::Cap("tags", TAGS_MAX),
        ),
        (
            "steps",
            TodoUpdate {
                id: id.clone(),
                steps: Some(
                    over_steps
                        .iter()
                        .map(|t| StepPatch {
                            id: None,
                            title: t.clone(),
                            done: false,
                        })
                        .collect(),
                ),
                ..TodoUpdate::default()
            },
            TodoError::Cap("steps", STEPS_MAX),
        ),
    ] {
        let err = apply_to(&path, TodoOp::Update(up), &agent(), T0 + 1, None).unwrap_err();
        assert_eq!(err, expected, "{field} cap on update");
        let item_now = load_store(&path).store.items.iter().find(|i| i.id == id).cloned().unwrap();
        assert_eq!(item_now.title, "real item", "{field}: nothing was truncated in");
        assert_eq!(item_now.rev, rev_before, "{field}: a refused update must not bump rev");
    }
}

/// The one cap a table row cannot express, because reaching it means
/// [`ITEMS_MAX`] real items. Driven against the pure `apply` rather than the
/// file path: 5 000 atomic writes is a minute of disk for a bound that is a
/// property of the model.
#[test]
fn the_per_scope_item_cap_refuses_the_next_add() {
    let mut store = TodoStore::default();
    for n in 0..ITEMS_MAX {
        todo::apply(
            &mut store,
            TodoOp::Add(TodoAdd {
                scope: Scope::Global,
                title: format!("item {n}"),
                ..TodoAdd::default()
            }),
            &agent(),
            T0,
        )
        .unwrap_or_else(|e| panic!("item {n} should fit under the cap: {e}"));
    }
    assert_eq!(store.live(&Scope::Global).len(), ITEMS_MAX);

    let err = todo::apply(&mut store, add("one too many"), &agent(), T0).unwrap_err();
    assert_eq!(err, TodoError::Cap("items", ITEMS_MAX));
    assert_eq!(
        store.live(&Scope::Global).len(),
        ITEMS_MAX,
        "the refused add must not have landed"
    );

    // PER SCOPE, not global: a full global list must not block a workspace one.
    todo::apply(
        &mut store,
        TodoOp::Add(TodoAdd {
            scope: Scope::Workspace("c:/projects/loomux".to_string()),
            title: "different scope".to_string(),
            ..TodoAdd::default()
        }),
        &agent(),
        T0,
    )
    .expect("the cap is per scope");
}

#[test]
fn an_empty_title_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    let err = apply_to(&path, add("   "), &human(), T0, None).unwrap_err();
    assert!(matches!(err, TodoError::Invalid("title", _)), "got {err}");
}

// ---------- soft delete and purge ----------

#[test]
fn a_deleted_item_is_hidden_immediately_and_purged_after_thirty_days() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    let doomed = apply_to(&path, add("delete me"), &human(), T0, None).unwrap().ids[0].clone();
    let keeper = apply_to(&path, add("keep me"), &human(), T0, None).unwrap().ids[0].clone();

    apply_to(
        &path,
        TodoOp::Delete {
            id: doomed.clone(),
        },
        &human(),
        T0,
        None,
    )
    .unwrap();

    // Hidden from every reader...
    let snap = snapshot_at(&path, None);
    assert!(
        !snap.items.iter().any(|i| i.id == doomed),
        "a tombstone must not reach a reader"
    );
    // ...and unknown to a writer: a deleted id reads exactly like an id that
    // never existed, so a caller cannot probe for what it may not see.
    let err = apply_to(
        &path,
        TodoOp::Complete {
            id: doomed.clone(),
            done: true,
        },
        &human(),
        T0,
        None,
    )
    .unwrap_err();
    assert_eq!(err, TodoError::Unknown(doomed.clone()));

    // ...but still ON DISK, which is what makes the human's undo possible.
    let on_disk = load_store(&path).store;
    let tomb = on_disk.items.iter().find(|i| i.id == doomed).unwrap();
    assert_eq!(tomb.deleted_ms, Some(T0));

    // One day short of the purge window: still there.
    apply_to(&path, add("a later write"), &human(), T0 + PURGE_AFTER_MS - 1, None).unwrap();
    assert!(
        load_store(&path).store.items.iter().any(|i| i.id == doomed),
        "a tombstone inside the window must survive"
    );

    // At the window: the next write drops it, and only it.
    let applied = apply_to(&path, add("the purging write"), &human(), T0 + PURGE_AFTER_MS, None)
        .unwrap();
    assert_eq!(applied.purged, 1, "the write should report what it dropped");
    let after = load_store(&path).store;
    assert!(
        !after.items.iter().any(|i| i.id == doomed),
        "an expired tombstone must be purged"
    );
    assert!(
        after.items.iter().any(|i| i.id == keeper),
        "the purge must not touch live items"
    );
}

// ---------- ordering ----------

#[test]
fn order_after_moves_an_item_between_its_new_neighbours() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    let a = apply_to(&path, add("a"), &human(), T0, None).unwrap().ids[0].clone();
    let b = apply_to(&path, add("b"), &human(), T0, None).unwrap().ids[0].clone();
    let c = apply_to(&path, add("c"), &human(), T0, None).unwrap().ids[0].clone();

    // Move `c` to the front.
    apply_to(
        &path,
        TodoOp::Update(TodoUpdate {
            id: c.clone(),
            order_after: Some(OrderAfter::Start),
            ..TodoUpdate::default()
        }),
        &human(),
        T0 + 1,
        None,
    )
    .unwrap();
    let store = load_store(&path).store;
    let order: Vec<&str> = store
        .live(&Scope::Global)
        .iter()
        .map(|i| i.id.as_str())
        .collect();
    assert_eq!(order, vec![c.as_str(), a.as_str(), b.as_str()]);

    // And back, after `a`.
    apply_to(
        &path,
        TodoOp::Update(TodoUpdate {
            id: c.clone(),
            order_after: Some(OrderAfter::Item(a.clone())),
            ..TodoUpdate::default()
        }),
        &human(),
        T0 + 2,
        None,
    )
    .unwrap();
    let store = load_store(&path).store;
    let order: Vec<&str> = store
        .live(&Scope::Global)
        .iter()
        .map(|i| i.id.as_str())
        .collect();
    assert_eq!(order, vec![a.as_str(), c.as_str(), b.as_str()]);

    // The `order` VALUE, not just the resulting sequence. The sequence alone is
    // masked by the re-spacing safety net: an implementation that lands the
    // moved item ON its predecessor's order produces a collision, gets
    // renumbered, and comes out in the right sequence anyway — a mutation that
    // deleted the midpoint arithmetic entirely reddened nothing until this
    // assertion existed (#3263 S1 scratch, M13).
    let at = |id: &str| -> i64 {
        store.items.iter().find(|i| i.id == id).unwrap().order
    };
    assert!(
        at(&a) < at(&c) && at(&c) < at(&b),
        "the moved item must land strictly between its neighbours, not on one of them: a={} c={} b={}",
        at(&a),
        at(&c),
        at(&b)
    );
    assert_eq!(
        at(&a),
        ORDER_GAP,
        "a move with room to spare must not have re-spaced the scope"
    );
}

// ---------- unknown keys ----------

#[test]
fn keys_this_build_does_not_know_survive_a_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    // A file a NEWER build wrote — but at version 1, so this build may write
    // it. Every level carries a key this build has never heard of.
    std::fs::write(
        &path,
        r#"{
          "version": 1,
          "recurrence_defaults": {"weekly": true},
          "workspaces": {"c:/projects/loomux": {"label":"loomux","root":"C:/Projects/loomux","first_seen_ms":1,"colour":"gold"}},
          "items": [{
            "id": "td-000000000000beef",
            "title": "has unknown fields",
            "rev": 4,
            "recur": {"every": "week"},
            "steps": [{"id":"st-1","title":"a step","done":false,"assignee":"a-7"}]
          }]
        }"#,
    )
    .unwrap();

    // A write to a DIFFERENT item is what re-serialises the whole file, which
    // is exactly the moment an unpreserved key would be lost.
    apply_to(&path, add("an unrelated item"), &human(), T0, None).unwrap();

    let raw = read_raw(&path);
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        value["recurrence_defaults"]["weekly"],
        serde_json::json!(true),
        "an envelope-level unknown key was dropped"
    );
    assert_eq!(
        value["workspaces"]["c:/projects/loomux"]["colour"],
        serde_json::json!("gold"),
        "a workspace-level unknown key was dropped"
    );
    let old = value["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == serde_json::json!("td-000000000000beef"))
        .expect("the pre-existing item should still be there");
    assert_eq!(
        old["recur"]["every"],
        serde_json::json!("week"),
        "an item-level unknown key was dropped"
    );
    assert_eq!(
        old["steps"][0]["assignee"],
        serde_json::json!("a-7"),
        "a step-level unknown key was dropped"
    );
    // The negative control: without it, a decoder that wrote the input file
    // back verbatim would pass everything above.
    assert_eq!(
        value["items"].as_array().unwrap().len(),
        2,
        "the new item should have been written too"
    );
}

// ---------- workspace identity ----------

/// The three spellings of one directory the issue names, under BOTH rule sets.
///
/// `normalize_key` rather than `workspace_key` on purpose: `workspace_key`
/// canonicalises, which needs the directory to exist, and `C:\Projects\Loomux`
/// exists on nobody's CI runner. Driving the pure half with the
/// case-insensitivity rule as an argument is what lets the WINDOWS rules be
/// witnessed on the two CI platforms that are not Windows — a `#[cfg(windows)]`
/// test would leave them unwitnessed there, and the lower-casing rule is
/// precisely the one whose absence is invisible until a human has two lists for
/// one project.
#[test]
fn three_spellings_of_one_windows_directory_produce_one_key() {
    let spellings = [
        r"C:\Projects\Loomux\",
        "c:/projects/loomux",
        r"\\?\C:\Projects\Loomux",
        r"C:\Projects\Loomux",
    ];
    let keys: Vec<String> = spellings
        .iter()
        .map(|s| todo::normalize_key(s, true))
        .collect();
    assert!(
        keys.iter().all(|k| k == "c:/projects/loomux"),
        "every Windows spelling must key the same: {keys:?}"
    );

    // Idempotent: normalising a key again is a no-op, which is what makes it
    // safe to key a map that was written by an older build.
    for k in &keys {
        assert_eq!(&todo::normalize_key(k, true), k);
    }

    // The case-SENSITIVE rule set is the discriminating control: without it,
    // an implementation that lower-cased unconditionally would pass the block
    // above and silently merge two genuinely different directories on Linux.
    assert_eq!(
        todo::normalize_key("/home/w/Loomux", false),
        "/home/w/Loomux"
    );
    assert_ne!(
        todo::normalize_key("/home/w/Loomux", false),
        todo::normalize_key("/home/w/loomux", false),
        "case is significant off Windows"
    );
}

#[test]
fn a_root_keeps_its_trailing_slash_and_a_unc_path_keeps_its_share() {
    // The trailing-slash rule must not eat the only slash a drive root has.
    assert_eq!(todo::normalize_key(r"C:\", true), "c:/");
    assert_eq!(todo::normalize_key("/", false), "/");
    // `canonicalize` spells a UNC path `\\?\UNC\server\share`; a caller spells
    // it `\\server\share`, and the two must key the same.
    assert_eq!(
        todo::normalize_key(r"\\?\UNC\server\share\proj", true),
        todo::normalize_key(r"\\server\share\proj", true)
    );
    assert_eq!(
        todo::normalize_key(r"\\?\UNC\server\share\proj", true),
        "//server/share/proj"
    );
}

#[test]
fn workspace_key_canonicalises_a_directory_that_exists() {
    // The half `normalize_key` cannot witness: that `workspace_key` really does
    // canonicalise before normalising. A tempdir is the one directory a test
    // can be sure exists.
    let tmp = tempfile::tempdir().unwrap();
    let nested = tmp.path().join("proj");
    std::fs::create_dir(&nested).unwrap();

    let direct = todo::workspace_key(&nested);
    // The same directory reached through a `.` component — canonicalisation is
    // what makes these one key; a purely lexical rule would leave the `/./`.
    let indirect = todo::workspace_key(&tmp.path().join(".").join("proj"));
    assert_eq!(direct, indirect, "two spellings of one real directory");
    assert!(
        !direct.contains(r"\") && !direct.contains("/./"),
        "the key should be normalised: {direct}"
    );
    assert!(direct.ends_with("proj"), "got {direct}");
}

#[test]
fn the_workspace_label_is_the_folder_name() {
    assert_eq!(todo::workspace_label("c:/projects/loomux"), "loomux");
    assert_eq!(todo::workspace_label("c:/"), "c:");
    assert_eq!(todo::workspace_label("/"), "/");
}

// ---------- scope isolation ----------

#[test]
fn a_snapshot_filtered_to_one_scope_shows_only_that_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    let ws = Scope::Workspace("c:/projects/loomux".to_string());
    let other = Scope::Workspace("c:/projects/other".to_string());

    for (scope, title) in [
        (Scope::Global, "global one"),
        (ws.clone(), "loomux one"),
        (other.clone(), "other one"),
    ] {
        apply_to(
            &path,
            TodoOp::Add(TodoAdd {
                scope,
                title: title.to_string(),
                ..TodoAdd::default()
            }),
            &human(),
            T0,
            None,
        )
        .unwrap();
    }

    let titles = |scope: Option<&Scope>| -> Vec<String> {
        let mut t: Vec<String> = snapshot_at(&path, scope)
            .items
            .iter()
            .map(|i| i.title.clone())
            .collect();
        t.sort();
        t
    };
    assert_eq!(titles(Some(&ws)), vec!["loomux one".to_string()]);
    assert_eq!(titles(Some(&Scope::Global)), vec!["global one".to_string()]);
    assert_eq!(
        titles(None),
        vec![
            "global one".to_string(),
            "loomux one".to_string(),
            "other one".to_string()
        ],
        "an unfiltered snapshot is what the pane's scope switch reads"
    );
}

// ---------- placement ----------

#[test]
fn the_store_is_a_sibling_of_the_other_data_root_singletons() {
    // `tabs.json` / `boardprefs.json` live directly under the data root; so
    // does this. Asserted through the pure half so the process-global
    // `data_root()` is not read (and cannot be redirected out from under every
    // other test in this binary).
    let root = Path::new("/somewhere/data");
    assert_eq!(todo_path_in(root), root.join("todo.json"));
}

