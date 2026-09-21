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
    ITEMS_MAX, NOTES_MAX, ORDER_GAP, PRIORITY_MAX, PURGE_AFTER_MS, STEPS_MAX, TAGS_MAX,
    TAG_BYTES_MAX,
    TITLE_MAX,
};
use loomux_lib::orchestration::todo::{
    apply_to, load_store, lock_todo_write, snapshot_at, todo_path_in, TodoStoreLoad,
};
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

/// [`load_store`] under the write lock, which is the only way to call it
/// (#3285 item 2): its corrupt arm RENAMES, so the token it takes is the
/// compiler's proof that no unserialised rename can be written.
///
/// The guard is scoped to this call. Holding one across an `apply_to` would
/// deadlock — `TODO_WRITE_LOCK` is a plain `std::sync::Mutex`, not a
/// re-entrant one — and the one test that deliberately holds it says so.
fn load_locked(path: &Path) -> TodoStoreLoad {
    let lock = lock_todo_write();
    load_store(path, &lock)
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
    let reread = load_locked(&path);
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

    let store = load_locked(&path).store;
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

    let loaded = load_locked(&path);
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
    assert_eq!(load_locked(&path).store.items.len(), 1);
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

    let loaded = load_locked(&path);
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

    let loaded = load_locked(&path);
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
    // The wording names THIS cause and not its neighbour (#3285 item 3). The
    // two arms below `readable: false` are different events — bytes that would
    // not read, and bytes that read fine and could not be moved — and the
    // human is being told which file to go and look at.
    assert!(
        err.to_string().contains("could not be read"),
        "this arm is the unreadable one: {err}"
    );
    assert!(
        !err.to_string().contains("quarantine"),
        "nothing was quarantined here, so nothing may say so: {err}"
    );
    assert!(path.is_dir(), "the refused write must have touched nothing");
}

#[test]
fn a_quarantine_rename_that_fails_declines_the_write_too() {
    // The half the quarantine tests above do NOT cover: `fs::rename` can fail,
    // and then nothing has been preserved anywhere. Reporting the read as
    // successful would license the next write to publish an empty store over
    // corrupt bytes that were never moved aside — the exact loss the quarantine
    // exists to prevent (rev-std round 1, finding 2).
    //
    // A DIRECTORY at the quarantine target is the portable way to make the
    // rename fail: POSIX and Windows both refuse to rename a file onto one.
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    std::fs::write(&path, "{ truncated").unwrap();
    std::fs::create_dir(tmp.path().join("todo.corrupt.json")).unwrap();

    let loaded = load_locked(&path);
    assert!(
        !loaded.readable,
        "a corrupt store whose quarantine FAILED must not read as safely emptied"
    );
    assert!(
        loaded.quarantined.is_none(),
        "nothing may be reported as quarantined when the rename did not happen"
    );

    let err = apply_to(&path, add("would erase the evidence"), &human(), T0, None).unwrap_err();
    assert!(
        err.to_string().contains("refusing to overwrite"),
        "the write must be declined: {err}"
    );
    assert_eq!(
        read_raw(&path),
        "{ truncated",
        "the corrupt bytes must still be exactly where they were"
    );
    // The other half of the wording pin (#3285 item 3): this arm READ
    // perfectly well, and what failed was moving the bytes aside. It collides
    // with the pin on the unreadable test by construction — no one message
    // can satisfy both, which is what makes the pair fail-able.
    assert!(
        err.to_string().contains("could not be quarantined"),
        "the decline must name the rename, not the read: {err}"
    );
    assert!(
        !err.to_string().contains("could not be read"),
        "these bytes read fine; saying otherwise sends the human to the wrong file: {err}"
    );
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
    let store = load_locked(&path).store;
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
    // ONE oversized tag in a list of THREE — well under `TAGS_MAX`, so this
    // row can be refused only by the per-tag BYTE cap and never by the count
    // cap it sits beside (#3285 item 1). The two operands COLLIDE by
    // construction: a build with only the count cap accepts this list, and a
    // build that answered `Cap("tags", ..)` for it would fail the assertion
    // below on the error's own field name.
    let over_one_tag: Vec<String> = vec![
        "fine".to_string(),
        "x".repeat(TAG_BYTES_MAX + 1),
        "also-fine".to_string(),
    ];

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
            "tag",
            TodoAdd {
                title: "ok".to_string(),
                tags: Some(over_one_tag.clone()),
                ..TodoAdd::default()
            },
            TodoError::Cap("tag", TAG_BYTES_MAX),
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
            !path.exists() || load_locked(&path).store.items.is_empty(),
            "{field}: a refused add must write nothing at all"
        );
    }

    // The same caps on the UPDATE path — a guard present on one call site and
    // absent from its sibling is a bypass exactly the width of the asymmetry.
    let id = apply_to(&path, add("real item"), &human(), T0, None).unwrap().ids[0].clone();
    let rev_before = load_locked(&path)
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
            "tag",
            TodoUpdate {
                id: id.clone(),
                tags: Some(over_one_tag.clone()),
                ..TodoUpdate::default()
            },
            TodoError::Cap("tag", TAG_BYTES_MAX),
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
        let item_now = load_locked(&path).store.items.iter().find(|i| i.id == id).cloned().unwrap();
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

    // The cap guards BOTH doors into a scope (#3285). A restore puts a LIVE
    // item back exactly as an add puts one in, and a cap only one of the two
    // respects is not a cap — an agent refused at `Add` could otherwise
    // delete-and-restore its way past it.
    let doomed = store.live(&Scope::Global)[0].id.clone();
    todo::apply(
        &mut store,
        TodoOp::Delete {
            id: doomed.clone(),
        },
        &agent(),
        T0,
    )
    .unwrap();
    assert_eq!(store.live(&Scope::Global).len(), ITEMS_MAX - 1);
    todo::apply(&mut store, add("back to the cap"), &agent(), T0)
        .expect("the slot the delete opened must be fillable");

    let err = todo::apply(
        &mut store,
        TodoOp::Restore {
            id: doomed.clone(),
        },
        &agent(),
        T0,
    )
    .unwrap_err();
    assert_eq!(err, TodoError::Cap("items", ITEMS_MAX));
    assert!(
        store
            .items
            .iter()
            .find(|i| i.id == doomed)
            .unwrap()
            .is_deleted(),
        "a refused restore must leave the tombstone exactly as it was"
    );
}

#[test]
fn a_priority_outside_its_range_is_refused_on_both_paths() {
    // The range check had no witness at all: mutating PRIORITY_MAX reddened
    // nothing (rev-std round 1, completeness table). Both paths, because a
    // guard on one call site and absent from its sibling is a bypass exactly
    // the width of the asymmetry.
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());

    let err = apply_to(
        &path,
        TodoOp::Add(TodoAdd {
            title: "too urgent".to_string(),
            priority: Some(PRIORITY_MAX + 1),
            ..TodoAdd::default()
        }),
        &agent(),
        T0,
        None,
    )
    .unwrap_err();
    assert!(matches!(err, TodoError::Invalid("priority", _)), "got {err}");
    assert!(
        err.to_string().contains(&format!("0..={PRIORITY_MAX}")),
        "the refusal must name the range: {err}"
    );

    // The boundary itself is accepted — without this, a guard that refused
    // every priority would pass the assertion above.
    let id = apply_to(
        &path,
        TodoOp::Add(TodoAdd {
            title: "urgent".to_string(),
            priority: Some(PRIORITY_MAX),
            ..TodoAdd::default()
        }),
        &agent(),
        T0,
        None,
    )
    .expect("the cap value itself is in range")
    .ids[0]
        .clone();

    let err = apply_to(
        &path,
        TodoOp::Update(TodoUpdate {
            id: id.clone(),
            priority: Some(PRIORITY_MAX + 1),
            ..TodoUpdate::default()
        }),
        &agent(),
        T0 + 1,
        None,
    )
    .unwrap_err();
    assert!(matches!(err, TodoError::Invalid("priority", _)), "got {err}");
    let item = load_locked(&path)
        .store
        .items
        .iter()
        .find(|i| i.id == id)
        .cloned()
        .unwrap();
    assert_eq!(item.priority, PRIORITY_MAX, "nothing was clamped in");
    assert_eq!(item.rev, 1, "a refused update must not bump rev");
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
    let on_disk = load_locked(&path).store;
    let tomb = on_disk.items.iter().find(|i| i.id == doomed).unwrap();
    assert_eq!(tomb.deleted_ms, Some(T0));

    // One day short of the purge window: still there.
    apply_to(&path, add("a later write"), &human(), T0 + PURGE_AFTER_MS - 1, None).unwrap();
    assert!(
        load_locked(&path).store.items.iter().any(|i| i.id == doomed),
        "a tombstone inside the window must survive"
    );

    // At the window: the next write drops it, and only it.
    let applied = apply_to(&path, add("the purging write"), &human(), T0 + PURGE_AFTER_MS, None)
        .unwrap();
    assert_eq!(applied.purged, 1, "the write should report what it dropped");
    let after = load_locked(&path).store;
    assert!(
        !after.items.iter().any(|i| i.id == doomed),
        "an expired tombstone must be purged"
    );
    assert!(
        after.items.iter().any(|i| i.id == keeper),
        "the purge must not touch live items"
    );
}

// ---------- restore (#3285) ----------

#[test]
fn a_deleted_item_can_be_restored_inside_the_purge_window() {
    // The op a soft delete had no inverse without. `apply` treats a tombstone
    // as unknown, so before this every undo of a delete could do was refuse —
    // `doc/design/todo-pane.md`, "Undo refuses rather than guesses", which said
    // so and named the missing op.
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    let id = apply_to(&path, add("deleted by mistake"), &human(), T0, None).unwrap().ids[0].clone();
    let before = load_locked(&path)
        .store
        .items
        .iter()
        .find(|i| i.id == id)
        .cloned()
        .unwrap();
    apply_to(&path, TodoOp::Delete { id: id.clone() }, &human(), T0 + 1, None).unwrap();
    assert!(
        snapshot_at(&path, None).items.iter().all(|i| i.id != id),
        "precondition: a tombstone is hidden from every reader"
    );

    // One millisecond short of the purge window — the far edge of what the
    // store promises is still recoverable.
    let applied = apply_to(
        &path,
        TodoOp::Restore { id: id.clone() },
        &agent(),
        T0 + PURGE_AFTER_MS - 1,
        None,
    )
    .expect("a tombstone inside the window must be restorable");
    let item = applied.item.expect("a restore returns the item it revived");
    assert_eq!(item.deleted_ms, None, "the tombstone must be gone");
    assert_eq!(
        item.title, before.title,
        "the row comes back as it was — a restore is not a re-add"
    );
    assert_eq!(item.order, before.order, "including its place in the list");
    assert_eq!(item.created_ms, before.created_ms);
    assert_eq!(item.created_by, before.created_by);
    assert_eq!(
        item.rev,
        before.rev + 2,
        "the delete and the restore are one write each"
    );
    assert_eq!(
        item.updated_by,
        agent(),
        "a restore is attributed to whoever performed it"
    );
    assert_eq!(applied.purged, 0, "nothing had expired");
    assert_eq!(
        TodoOp::Restore { id: id.clone() }.action(),
        "todo-restore",
        "the audit row needs its own action name, not the delete's"
    );

    // Visible to a reader again AND addressable by a writer, which is what
    // makes this a restore rather than a flag flip on a row nothing can reach.
    assert!(
        snapshot_at(&path, None).items.iter().any(|i| i.id == id),
        "a restored item must reach a reader"
    );
    apply_to(
        &path,
        TodoOp::Complete {
            id: id.clone(),
            done: true,
        },
        &human(),
        T0 + PURGE_AFTER_MS,
        None,
    )
    .expect("a restored item is a live item");
}

#[test]
fn a_restore_after_the_purge_window_is_refused_as_unknown() {
    // The bound on the undo: 30 days is what the store promises to keep, so a
    // tombstone past it is a row this build has already undertaken to drop —
    // the very next write drops it — and reviving one would hand back data the
    // store no longer guarantees is intact.
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    let id = apply_to(&path, add("long gone"), &human(), T0, None).unwrap().ids[0].clone();
    apply_to(&path, TodoOp::Delete { id: id.clone() }, &human(), T0, None).unwrap();

    // The tombstone is still ON DISK at this point — the purge runs on the next
    // write — so the refusal below is genuinely the expired-window one and not
    // "there is nothing there".
    let before = read_raw(&path);
    assert!(
        load_locked(&path).store.items.iter().any(|i| i.id == id),
        "fixture: the tombstone must still be on disk for this to test the window"
    );

    let err = apply_to(
        &path,
        TodoOp::Restore { id: id.clone() },
        &human(),
        T0 + PURGE_AFTER_MS,
        None,
    )
    .unwrap_err();
    assert_eq!(
        err,
        TodoError::Unknown(id.clone()),
        "an expired tombstone reads exactly as an id that never existed"
    );
    assert_eq!(
        read_raw(&path),
        before,
        "a refused restore must not rewrite the file — not even to purge"
    );

    // The control, one millisecond inside the window on an identical fixture:
    // without it this test would pass against a `Restore` that never works at
    // all.
    let tmp2 = tempfile::tempdir().unwrap();
    let path2 = store_path(tmp2.path());
    let id2 = apply_to(&path2, add("just in time"), &human(), T0, None).unwrap().ids[0].clone();
    apply_to(&path2, TodoOp::Delete { id: id2.clone() }, &human(), T0, None).unwrap();
    apply_to(
        &path2,
        TodoOp::Restore { id: id2 },
        &human(),
        T0 + PURGE_AFTER_MS - 1,
        None,
    )
    .expect("control: one ms inside the window the same restore succeeds");
}

#[test]
fn restoring_a_live_item_is_refused_rather_than_silently_doing_nothing() {
    // A no-op restore is indistinguishable from one that worked, and the caller
    // asked precisely because it did not know. It is also a DIFFERENT refusal
    // from an unknown id: one is a wrong-state error the caller can act on, the
    // other is "no such row".
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    let id = apply_to(&path, add("perfectly alive"), &human(), T0, None).unwrap().ids[0].clone();
    let before = read_raw(&path);

    let err = apply_to(
        &path,
        TodoOp::Restore { id: id.clone() },
        &human(),
        T0 + 1,
        None,
    )
    .unwrap_err();
    assert_eq!(
        err,
        TodoError::Invalid("restore", format!("{id} is not deleted"))
    );
    assert!(
        err.to_string().contains("is not deleted"),
        "the message must say what was wrong with it: {err}"
    );
    assert_eq!(
        read_raw(&path),
        before,
        "a refused restore must leave the store byte-identical"
    );

    let unknown = "td-0000000000000001".to_string();
    let err2 = apply_to(
        &path,
        TodoOp::Restore {
            id: unknown.clone(),
        },
        &human(),
        T0 + 1,
        None,
    )
    .unwrap_err();
    assert_eq!(
        err2,
        TodoError::Unknown(unknown),
        "an id that never existed is not the same refusal as a live one"
    );
}

// ---------- the write lock covers the quarantine RENAME ----------

#[test]
fn a_snapshot_cannot_quarantine_while_the_write_lock_is_held() {
    // #3285 item 2. `load_store`'s corrupt arm RENAMES, and it used to do so
    // from `snapshot_at` with no lock at all. The losing interleaving is silent
    // and every syscall in it succeeds: a reader sees corrupt bytes and decides
    // to quarantine; a writer, under the lock, quarantines first and publishes
    // a fresh store; the reader's rename then lands on THAT file and moves the
    // human's new list to `todo.corrupt.json`.
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    std::fs::write(&path, "{ truncated").unwrap();
    let q = tmp.path().join("todo.corrupt.json");

    // Held deliberately across a spawn. `load_locked()` above never does this —
    // the lock is a plain `std::sync::Mutex`, so a second acquisition on THIS
    // thread would deadlock rather than fail.
    let lock = lock_todo_write();
    let reading = path.clone();
    let reader = std::thread::spawn(move || snapshot_at(&reading, None));

    // The negative half, bounded: the rename is the FIRST thing a snapshot of
    // corrupt bytes does, so an unlocked one has finished it long before this
    // wait is over — 500 ms is some three orders of magnitude more than the
    // syscall needs.
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        !q.exists(),
        "a snapshot renamed the corrupt file out from under a writer holding the lock"
    );
    assert_eq!(
        read_raw(&path),
        "{ truncated",
        "and the bytes must still be where the lock holder last saw them"
    );

    drop(lock);
    let snap = reader.join().expect("the snapshot thread must not panic");

    // The positive control. Without it the assertions above pass just as well
    // against a snapshot that never ran, or one that cannot quarantine at all.
    assert!(
        q.exists(),
        "control: the same snapshot must quarantine once the lock is free"
    );
    assert!(
        snap.quarantined.is_some(),
        "control: and must report the path it moved the evidence to"
    );
}

// ---------- the two arms Windows alone can exercise ----------
//
// Both need a rename to FAIL against a file that is otherwise perfectly
// healthy, and the only portable way to arrange that — a directory in the way —
// is already used by the two tests above. What is left is the shape the module
// doc actually names: another process holding the file open. POSIX renames
// happily over an open file, so there is nothing on Unix or macOS to exercise;
// on Windows `MoveFileExW`'s REPLACE_EXISTING needs FILE_SHARE_DELETE on the
// destination, and a reader that grants read and write sharing but not delete
// is exactly the "concurrent reader" `fsatomic`'s fallback comment describes.
//
// Stated rather than silently no-op'd on the other two platforms, the way
// `tests/orchestration.rs`'s `#[cfg(unix)]` permission tests are.

/// `FILE_SHARE_READ | FILE_SHARE_WRITE`, and deliberately **not**
/// `FILE_SHARE_DELETE`: reads and writes are permitted, a rename over the file
/// is not.
#[cfg(windows)]
const SHARE_READ_WRITE_NOT_DELETE: u32 = 0x0000_0001 | 0x0000_0002;

#[cfg(windows)]
#[test]
fn a_write_whose_rename_a_concurrent_reader_blocks_still_lands() {
    // `atomic_write`'s bare-write fallback, exercised by the one thing the
    // module doc names as producing it. The fallback is what keeps the update
    // from being lost when the rename cannot happen; it is also what narrows
    // rather than closes the torn-file window, which is why the doc says so and
    // why this arm is now tested rather than assumed.
    use std::os::windows::fs::OpenOptionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    let first = apply_to(&path, add("already here"), &human(), T0, None).unwrap().ids[0].clone();

    let reader = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(SHARE_READ_WRITE_NOT_DELETE)
        .open(&path)
        .expect("the store must be openable for reading");

    // The fixture control. Without it this test passes on any build where the
    // share mode does not bite — certifying the happy rename rather than the
    // fallback it is named for.
    let probe = tmp.path().join("probe.bin");
    std::fs::write(&probe, b"x").unwrap();
    assert!(
        std::fs::rename(&probe, &path).is_err(),
        "fixture: a held-open reader must make a rename onto the store fail"
    );

    let second = apply_to(&path, add("through the fallback"), &human(), T0 + 1, None)
        .expect("a blocked rename must not lose the write")
        .ids[0]
        .clone();
    drop(reader);

    let store = load_locked(&path).store;
    let ids: Vec<&str> = store.items.iter().map(|i| i.id.as_str()).collect();
    assert!(
        ids.contains(&first.as_str()) && ids.contains(&second.as_str()),
        "both items must be in the file the fallback wrote: {ids:?}"
    );
    let orphans: Vec<String> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(
        orphans.is_empty(),
        "a fallback that SUCCEEDED removes its temp sibling: {orphans:?}"
    );
}

#[cfg(windows)]
#[test]
fn a_quarantine_target_another_process_holds_open_declines_the_write() {
    // The second untested arm: the directory-shaped fixture two tests above
    // covers the same branch, and the module doc names two ways in. This is the
    // other one, and the one a human actually hits — an editor or an antivirus
    // scanner with `todo.corrupt.json` open from a previous corruption.
    use std::os::windows::fs::OpenOptionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    std::fs::write(&path, "{ truncated").unwrap();
    let q = tmp.path().join("todo.corrupt.json");
    std::fs::write(&q, "an older corruption").unwrap();

    let holder = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(SHARE_READ_WRITE_NOT_DELETE)
        .open(&q)
        .expect("the prior quarantine must be openable");

    let l = load_locked(&path);
    assert!(
        !l.readable,
        "a corrupt store whose quarantine FAILED must not read as safely emptied"
    );
    assert!(
        l.quarantine_failed,
        "this is the rename-failure arm, not the could-not-read one"
    );
    assert!(l.quarantined.is_none(), "nothing was moved, so nothing is reported");

    let err = apply_to(&path, add("would erase the evidence"), &human(), T0, None).unwrap_err();
    assert!(
        err.to_string().contains("could not be quarantined"),
        "the decline must name the cause a human can go and look at: {err}"
    );
    assert_eq!(
        read_raw(&path),
        "{ truncated",
        "the corrupt bytes must still be exactly where they were"
    );
    assert_eq!(
        std::fs::read_to_string(&q).unwrap(),
        "an older corruption",
        "and the file that blocked the rename must be untouched"
    );
    drop(holder);
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
    let store = load_locked(&path).store;
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
    let store = load_locked(&path).store;
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
    // `b`, and not `a`, is what says the re-spacing never ran. `a` is a FIXED
    // POINT of it — first in the scope, so it is 1 * ORDER_GAP before and
    // after — which is why asserting on `a` left M13 green a second time: the
    // collision it causes is re-spaced away and every value the test read came
    // back the same. `b` moves 2048 -> 3072 when the scope is re-spaced.
    assert_eq!(
        at(&b),
        2 * ORDER_GAP,
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

#[test]
fn a_steps_replace_keeps_each_surviving_steps_unknown_keys() {
    // The one level at which the round-trip promise needed CODE rather than
    // `#[serde(flatten)]`: item, workspace and envelope keys survive because
    // they are mutated in place, but an update REPLACES the step list from
    // `StepPatch`es, which carry only what a caller can express. Without the
    // re-attach, a newer build's step field is dropped the first time the human
    // edits the checklist (rev-std round 1, finding 3).
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    std::fs::write(
        &path,
        r#"{
          "version": 1,
          "items": [{
            "id": "td-000000000000cafe",
            "title": "has steps with unknown keys",
            "rev": 1,
            "steps": [
              {"id":"st-keep","title":"kept","done":false,"assignee":"a-7"},
              {"id":"st-drop","title":"removed by the edit","done":false,"assignee":"a-9"}
            ]
          }]
        }"#,
    )
    .unwrap();

    // The human edits the checklist: renames the surviving step, ticks it, and
    // drops the other — the ordinary gesture, not a contrived one.
    apply_to(
        &path,
        TodoOp::Update(TodoUpdate {
            id: "td-000000000000cafe".to_string(),
            steps: Some(vec![
                StepPatch {
                    id: Some("st-keep".to_string()),
                    title: "kept, and renamed".to_string(),
                    done: true,
                },
                StepPatch {
                    id: None,
                    title: "brand new".to_string(),
                    done: false,
                },
            ]),
            ..TodoUpdate::default()
        }),
        &human(),
        T0,
        None,
    )
    .unwrap();

    let value: serde_json::Value = serde_json::from_str(&read_raw(&path)).unwrap();
    let steps = value["items"][0]["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0]["id"], serde_json::json!("st-keep"));
    assert_eq!(steps[0]["title"], serde_json::json!("kept, and renamed"));
    assert_eq!(steps[0]["done"], serde_json::json!(true));
    assert_eq!(
        steps[0]["assignee"],
        serde_json::json!("a-7"),
        "a surviving step's unknown key must cross the replace"
    );
    // The negative control: a step the caller MINTED has no prior keys to
    // inherit, so a blanket copy would show up here.
    assert!(
        steps[1].get("assignee").is_none(),
        "a new step must not inherit another step's unknown keys: {}",
        steps[1]
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


// ==================== the command decoder (#3263 slice S3) ====================
//
// `parse_op` is the strict reader that stands between the webview and
// `TodoOp`. Every message it produces is a contract `src/todo.ts` is written
// against, so each refusal below is pinned by the text a human would see —
// not merely by "it was an error".
//
// The two commands themselves (`todo_snapshot` / `todo_apply`) are `async
// #[tauri::command]`s, so exercising them end to end needs an `AppHandle` and
// a live registry. What is testable without one — and what actually carries
// the risk — is the decoder plus the host path it hands its op to, which is
// exactly what these drive. Their REGISTRATION is covered structurally
// elsewhere: `tests/acl_manifest.rs` fails if either name is missing from
// `generate_handler!`, from `command_manifest::APP_COMMANDS`, or from the
// grants `capabilities/default.json` resolves through `permissions/sets/`.

use loomux_lib::orchestration::todo::parse_op;
use serde_json::json;

/// The refusal message, or a panic naming what was expected to be refused.
fn refusal(v: serde_json::Value, scope: Scope, what: &str) -> String {
    match parse_op(&v, scope) {
        Ok(_) => panic!("{what}: the decoder should have refused this op"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn a_quick_add_shaped_op_decodes_and_lands_in_the_store() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    let ws = Scope::Workspace("c:/projects/loomux".to_string());

    // The exact JSON `src/todo.ts`'s `addTodo` sends for the quick-add line
    // `pay rent fri 4pm #home !!`.
    let op = parse_op(
        &json!({"add": {
            "title": "pay rent",
            "due_ms": T0 + 86_400_000u64,
            "tags": ["home"],
            "priority": 2,
            "important": true,
            "my_day": T0,
            "steps": ["find the landlord's email"],
        }}),
        ws.clone(),
    )
    .expect("a well-formed add should decode");

    let applied = apply_to(&path, op, &human(), T0, Some(("c:/projects/loomux", "C:/Projects/loomux")))
        .expect("the decoded op should apply");
    let item = applied.item.expect("an add returns the item it made");

    assert_eq!(item.title, "pay rent");
    assert_eq!(item.due_ms, Some(T0 + 86_400_000));
    assert_eq!(item.tags, vec!["home".to_string()]);
    assert_eq!(item.priority, 2);
    assert!(item.important);
    assert_eq!(item.my_day, Some(T0));
    assert_eq!(item.steps.len(), 1);
    assert_eq!(
        item.scope, ws,
        "the scope comes from the command's workspace root, not from the op"
    );

    // And it is really on disk under that scope, which is what the pane reads.
    let snap = snapshot_at(&path, Some(&ws));
    assert_eq!(snap.items.len(), 1);
    assert_eq!(snap.items[0].id, item.id);
}

#[test]
fn a_misspelt_field_is_refused_rather_than_silently_dropped() {
    // The whole reason this decoder is hand-written instead of derived: a
    // derive accepts what it knows and ignores the rest, so `due` for `due_ms`
    // would be a write that succeeds and does nothing.
    let msg = refusal(
        json!({"add": {"title": "pay rent", "due": 17}}),
        Scope::Global,
        "an add with a misspelt due field",
    );
    assert!(
        msg.contains("\"due\"") && msg.contains("due_ms"),
        "the refusal must name the offending key AND the accepted ones, got: {msg}"
    );
}

#[test]
fn a_caller_may_not_name_a_scope_at_all() {
    // The workspace key is derived from the root the caller names; accepting a
    // key from the caller would let a pane address a list it is not in.
    let msg = refusal(
        json!({"add": {"title": "x", "scope": {"workspace": "c:/somewhere/else"}}}),
        Scope::Global,
        "an add carrying its own scope",
    );
    assert!(
        msg.contains("\"scope\""),
        "the refusal must name `scope`, got: {msg}"
    );
}

#[test]
fn an_explicit_null_clears_a_due_date_and_an_absent_key_leaves_it() {
    // The `Option<Option<u64>>` distinction, both arms, on ONE item — a
    // non-interference pin whose two operands collide (CLAUDE.md): the update
    // that must LEAVE the due date alone is the same field on the same item
    // the other update CLEARS, so a decoder that folded the two onto "leave
    // alone" fails the first assertion and one that folded them onto "clear"
    // fails the second.
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());

    let op = parse_op(
        &json!({"add": {"title": "renew the domain", "due_ms": T0 + 86_400_000u64}}),
        Scope::Global,
    )
    .unwrap();
    let id = apply_to(&path, op, &human(), T0, None)
        .unwrap()
        .item
        .unwrap()
        .id;

    // Absent `due_ms`: leave it alone.
    let op = parse_op(
        &json!({"update": {"id": id, "title": "renew the domain (annual)"}}),
        Scope::Global,
    )
    .unwrap();
    let item = apply_to(&path, op, &human(), T0 + 1, None)
        .unwrap()
        .item
        .unwrap();
    assert_eq!(item.title, "renew the domain (annual)");
    assert_eq!(
        item.due_ms,
        Some(T0 + 86_400_000),
        "an absent key must leave the field alone"
    );

    // Explicit `null`: clear it.
    let op = parse_op(&json!({"update": {"id": id, "due_ms": null}}), Scope::Global).unwrap();
    let item = apply_to(&path, op, &human(), T0 + 2, None)
        .unwrap()
        .item
        .unwrap();
    assert_eq!(
        item.due_ms, None,
        "an explicit null must clear the field — the whole reason a derive would not do"
    );
    assert_eq!(
        item.title, "renew the domain (annual)",
        "and must leave every field it did not name alone"
    );
}

#[test]
fn an_op_naming_two_actions_or_none_is_refused() {
    for (v, what) in [
        (json!({}), "an empty op"),
        (
            json!({"add": {"title": "a"}, "delete": {"id": "td-1"}}),
            "an op naming two actions",
        ),
        (json!({"archive": {"id": "td-1"}}), "an unknown action"),
        (json!("delete"), "an op that is not an object"),
    ] {
        let msg = refusal(v, Scope::Global, what);
        assert!(
            msg.starts_with("refused: op "),
            "{what} should be refused against the `op` field, got: {msg}"
        );
    }
}

#[test]
fn a_timestamp_that_is_not_a_whole_non_negative_number_is_refused() {
    // Truncating a float or wrapping a negative would put the item at a
    // different instant than the caller meant, silently.
    for (v, what) in [
        (json!({"add": {"title": "a", "due_ms": 1.5}}), "a fractional due_ms"),
        (json!({"add": {"title": "a", "due_ms": -1}}), "a negative due_ms"),
        (
            json!({"update": {"id": "td-1", "remind_ms": 2.5}}),
            "a fractional remind_ms",
        ),
    ] {
        let msg = refusal(v, Scope::Global, what);
        assert!(
            msg.contains("non-negative whole number"),
            "{what} should say why, got: {msg}"
        );
    }
}

#[test]
fn the_decoders_own_priority_range_matches_the_stores() {
    // The decoder range-checks so the message names the field the CALLER sent.
    // If this drifts above `PRIORITY_MAX` the store refuses anyway; if it
    // drifts below, a legal priority becomes unreachable from the pane.
    assert!(parse_op(
        &json!({"add": {"title": "a", "priority": PRIORITY_MAX}}),
        Scope::Global
    )
    .is_ok());
    let msg = refusal(
        json!({"add": {"title": "a", "priority": u64::from(PRIORITY_MAX) + 1}}),
        Scope::Global,
        "a priority one above the maximum",
    );
    assert!(
        msg.contains(&format!("maximum of {PRIORITY_MAX}")),
        "the refusal must name the maximum, got: {msg}"
    );
}

#[test]
fn order_after_takes_start_or_an_item_but_not_a_bare_id() {
    // `null` already means "leave alone" on this wire, so "first in the list"
    // needs a spelling of its own rather than being the absence of one.
    match parse_op(
        &json!({"update": {"id": "td-1", "order_after": "start"}}),
        Scope::Global,
    ) {
        Ok(TodoOp::Update(u)) => assert!(matches!(u.order_after, Some(OrderAfter::Start))),
        other => panic!("`\"start\"` should decode to OrderAfter::Start, got {other:?}"),
    }
    match parse_op(
        &json!({"update": {"id": "td-1", "order_after": {"item": "td-2"}}}),
        Scope::Global,
    ) {
        Ok(TodoOp::Update(u)) => {
            assert!(matches!(u.order_after, Some(OrderAfter::Item(ref i)) if i == "td-2"))
        }
        other => panic!("an item object should decode to OrderAfter::Item, got {other:?}"),
    }
    let msg = refusal(
        json!({"update": {"id": "td-1", "order_after": "td-2"}}),
        Scope::Global,
        "a bare id as order_after",
    );
    assert!(msg.contains("order_after"), "got: {msg}");
}

#[test]
fn the_restore_op_decodes_and_revives_the_item_the_pane_named() {
    // The decoder is DEFAULT-DENY, so a new engine op is unreachable from the
    // pane until it has an arm here too: without one `{"restore": …}` falls to
    // `parse_op`'s `other` arm and is refused by name (#3285). That is the
    // failure this test exists to make loud rather than a mystery in S5.
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    let id = apply_to(&path, add("undo me"), &human(), T0, None).unwrap().ids[0].clone();
    apply_to(&path, TodoOp::Delete { id: id.clone() }, &human(), T0 + 1, None).unwrap();

    let op = parse_op(&json!({"restore": {"id": id.clone()}}), Scope::Global)
        .expect("a well-formed restore should decode");
    match &op {
        TodoOp::Restore { id: got } => assert_eq!(got, &id),
        other => panic!("a restore must decode to TodoOp::Restore, got {other:?}"),
    }
    apply_to(&path, op, &human(), T0 + 2, None).expect("the decoded op should apply");
    assert!(
        snapshot_at(&path, None).items.iter().any(|i| i.id == id),
        "the item the pane named must be back in the list"
    );

    // Default-deny holds for the new arm too, both ways.
    let msg = refusal(
        json!({"restore": {"id": id.clone(), "when": 1}}),
        Scope::Global,
        "a restore carrying an unknown key",
    );
    assert!(msg.contains("\"when\""), "the refusal must name the key: {msg}");
    let msg2 = refusal(
        json!({"restore": {}}),
        Scope::Global,
        "a restore with no id",
    );
    assert!(msg2.contains("id is required"), "got: {msg2}");
}

#[test]
fn complete_defaults_to_done_and_delete_needs_its_id() {
    match parse_op(&json!({"complete": {"id": "td-1"}}), Scope::Global) {
        Ok(TodoOp::Complete { id, done }) => {
            assert_eq!(id, "td-1");
            assert!(done, "a bare complete means DONE — the space-bar path");
        }
        other => panic!("got {other:?}"),
    }
    match parse_op(&json!({"complete": {"id": "td-1", "done": false}}), Scope::Global) {
        Ok(TodoOp::Complete { done, .. }) => assert!(!done, "un-completing must be expressible"),
        other => panic!("got {other:?}"),
    }
    let msg = refusal(
        json!({"delete": {}}),
        Scope::Global,
        "a delete with no id",
    );
    assert!(msg.contains("id is required"), "got: {msg}");
}
