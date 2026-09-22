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
    ARCHIVE_IDS_MAX, ITEMS_MAX, NOTES_MAX, ORDER_GAP, PRIORITY_MAX, PURGE_AFTER_MS, STEPS_MAX,
    TAGS_MAX, TAG_BYTES_MAX,
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
    // `docs/design/todo-pane.md`, "Undo refuses rather than guesses", which said
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
        // **THE SPECIMEN MOVED, AND THE ASSERTION DID NOT.** This row was
        // `archive` at S2, chosen because it was not an op; #3263 S5 made it
        // one, so the witness is relocated rather than the check relaxed
        // (CLAUDE.md, "A test's specimen must stay a member of the class it
        // witnesses"). The extra assertion below is what stops the next slice
        // repeating it silently: the refusal must say the op is UNKNOWN, so a
        // future `sweep` op reddens this row instead of leaving it vacuous.
        (json!({"sweep": {"id": "td-1"}}), "an unknown action"),
        (json!("delete"), "an op that is not an object"),
    ] {
        let msg = refusal(v, Scope::Global, what);
        assert!(
            msg.starts_with("refused: op "),
            "{what} should be refused against the `op` field, got: {msg}"
        );
        if what == "an unknown action" {
            assert!(
                msg.contains("unknown op"),
                "this row's specimen must still BE unknown — if it has become a real op, \
                 move the witness rather than relaxing this: {msg}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The MCP tool surface (#3263 slice S2) — `todo_list` / `todo_get` /
// `todo_add` / `todo_update` / `todo_complete` / `todo_delete`.
//
// Appended here rather than to `tests/orchestration.rs` for the reason this
// file's header already gives: that file is the end-of-file-append conflict
// class, and the To-Do slices land concurrently. Every test below drives the
// REAL `dispatch()` with a `Caller` resolved from a real spawned agent's
// token, so the role gates, the listing filter and the dispatch gate are all
// exercised as the product runs them.
//
// **Why these tests redirect the process-global data root, and how they stay
// honest about it.** Everything above drives `apply_to`/`snapshot_at` against
// an explicit path, deliberately. The tool arms cannot: they go through
// `OrchRegistry::todo_apply`, which reads `todo_path()` -> `obs::data_root()`,
// one value for the whole test binary. So each test below takes
// [`MCP_SERIAL`] and installs a fresh `$ORRERIX_DATA_DIR` through
// [`DataRoot`], whose `Drop` puts the previous value back (CLAUDE.md: restore
// any global the harness overrode from a `Drop` guard). The lock is taken with
// a poison-tolerant `lock_safe`, for the same file's other rule — one panicking
// test must not turn every later one into a `PoisonError` and make a mutation
// round's reds unattributable.
// ---------------------------------------------------------------------------

use loomux_engine::obs::LockExt;
use loomux_lib::orchestration::mcp::dispatch;
use loomux_lib::orchestration::workflow;
use loomux_lib::orchestration::{AgentEntry, Caller, GroupId, Guardrails, OrchRegistry, Role};
// `json` is already in scope from the S3 command-decoder block above (this
// is one module), so importing it again here would be E0252 — the name
// defined twice. Only `Value` is new to this block.
use serde_json::Value;
use std::sync::Mutex;

/// Serialises every test that redirects the process-global data root.
///
/// `lock_safe`, never `.lock().unwrap()`: one failing test panicking under the
/// guard would poison it and report N failures for one real one, which is
/// exactly what makes a mutation round's reds unattributable.
static MCP_SERIAL: Mutex<()> = Mutex::new(());

/// Point `obs::data_root()` at a fresh temp directory for the life of one
/// test, and put the previous value back afterwards.
///
/// The restore is in `Drop` so it happens on a panicking test too — a test
/// that failed while holding the override would otherwise leave every later
/// test in this binary pointed at a deleted directory, turning one red into a
/// cascade with no relationship to the change under test.
struct DataRoot {
    _dir: tempfile::TempDir,
    prior: Option<std::ffi::OsString>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl DataRoot {
    /// The redirected data root, so a test can reach the store FILE the tools
    /// are reading — needed to age a tombstone past the purge window, which no
    /// injectable clock can do on this path (`todo_apply` reads `now_ms()`).
    fn path(&self) -> &Path {
        self._dir.path()
    }

    fn install() -> DataRoot {
        let _guard = MCP_SERIAL.lock_safe();
        let dir = tempfile::tempdir().unwrap();
        let prior = std::env::var_os("ORRERIX_DATA_DIR");
        std::env::set_var("ORRERIX_DATA_DIR", dir.path());
        DataRoot { _dir: dir, prior, _guard }
    }
}

impl Drop for DataRoot {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(v) => std::env::set_var("ORRERIX_DATA_DIR", v),
            None => std::env::remove_var("ORRERIX_DATA_DIR"),
        }
    }
}

/// Build a registry with every test-only directory override applied.
///
/// The ONE raw `OrchRegistry::new` this file is sanctioned for in
/// `orchestration.rs`'s
/// `no_registry_construction_bypasses_the_test_agent_dir_overrides` allowlist.
/// It is a real requirement rather than ceremony: a registry built without
/// these writes a generated agent file into the developer's REAL `~/.claude` /
/// `~/.copilot` agents dir on its first spawn (#464). Helpers do not cross
/// integration-test binaries, which is why this cannot call
/// `orchestration.rs`'s.
fn relaunch_registry(dir: &Path) -> OrchRegistry {
    let reg = OrchRegistry::new(dir.to_path_buf());
    reg.set_port(45993);
    reg.set_claude_agents_dir_override(dir.join("claude-agents"));
    reg.set_copilot_agents_dir_override(dir.join("copilot-agents"));
    reg.set_compact_hook_dir_override(dir.join("compacthook"));
    reg.set_copilot_hooks_dir_override(dir.join("copilot-hooks"));
    reg
}

/// The four agent/hook dir overrides this file's allowlist row ASSUMES
/// [`relaunch_registry`] applies (#1778's row convention, in the shape
/// `plandrive.rs` and `piusage.rs` already use it).
///
/// Without it the row in `orchestration.rs` is a claim about this file that
/// nothing in this file checks — and the property it stands in for (#464: no
/// generated agent file reaches a developer's real `~/.claude`) is exactly the
/// kind that fails silently. The four setters are private to the registry's
/// own crate-internal accessors, so the check is a scan of the HELPER'S BODY,
/// bounded by the population control below so it cannot be satisfied by some
/// other function in this file.
#[test]
fn its_registry_helper_applies_every_override_this_allowlist_row_assumes() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/todo.rs"),
    )
    .expect("this file reads itself");

    // The helper's body: from its signature to the first line that closes it at
    // column 0.
    let start = src
        .find("fn relaunch_registry(dir: &Path) -> OrchRegistry {")
        .expect("the sanctioned helper must exist, under the name the row names");
    let body = &src[start..];
    let end = body.find("\n}").expect("the helper must terminate") + 2;
    let body = &body[..end];

    for needed in [
        "set_claude_agents_dir_override",
        "set_copilot_agents_dir_override",
        "set_compact_hook_dir_override",
        "set_copilot_hooks_dir_override",
    ] {
        assert!(
            body.contains(needed),
            "the #464 allowlist row for tests/todo.rs assumes this helper applies every \
             override; it no longer applies {needed}, so a registry built through it can reach \
             the real agent dirs and the row's premise is gone"
        );
    }

    // The population control: the extraction really did isolate the helper, so
    // the four assertions above are about ITS body and not about the whole file
    // — which contains those same names in prose.
    assert!(
        body.len() < 1_200,
        "the helper's body extraction ran away ({} chars); the assertions above would then be \
         satisfied by any other function in this file",
        body.len()
    );
    assert!(
        !body.contains("#[test]"),
        "the extraction swallowed a test, so it is no longer reading only the helper"
    );
}

/// Guardrails with room for one pane of every class these tests drive.
///
/// `max_agents: 8` rather than the 2 `orchestration.rs`'s fixture uses: the
/// per-role sweep below spawns one of each, and a cap refusal mid-fixture would
/// make a "this role can add a to-do" assertion fail for a reason that has
/// nothing to do with the to-do list.
fn mcp_rails() -> Guardrails {
    Guardrails {
        max_agents: 8,
        agent_cli: "claude".into(),
        blocks: workflow::default_roster(&[
            (Role::Orchestrator, "claude", "opus"),
            (Role::Worker, "claude", "sonnet"),
            (Role::Reviewer, "claude", "sonnet"),
            (Role::Planner, "claude", "opus"),
        ]),
        auto_ops: false,
        idle_kill_minutes: 0,
        max_spawns_per_hour: 0,
        watchdog_stall_minutes: 0,
        ..Guardrails::default()
    }
}

fn caller_for(reg: &OrchRegistry, a: &AgentEntry) -> Caller {
    reg.resolve_token(&a.token).expect("a spawned agent's token must resolve")
}

/// A group on `repo`, with one orchestrator and one worker pane.
fn mcp_group(reg: &OrchRegistry, repo: &Path) -> (GroupId, Caller, Caller) {
    let g = reg.create_group(&repo.to_string_lossy(), mcp_rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let worker = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    (g.id.clone(), caller_for(reg, &orch), caller_for(reg, &worker))
}

/// Call one tool through the REAL dispatch. Returns `(is_error, text)`.
fn call(reg: &OrchRegistry, c: &Caller, name: &str, args: Value) -> (bool, String) {
    let out = dispatch(reg, c, "tools/call", &json!({ "name": name, "arguments": args })).unwrap();
    let text = out["content"][0]["text"].as_str().unwrap_or("").to_string();
    (out["isError"] == json!(true), text)
}

/// Call a tool that must SUCCEED, and parse its JSON reply.
fn ok_json(reg: &OrchRegistry, c: &Caller, name: &str, args: Value) -> Value {
    let (err, text) = call(reg, c, name, args);
    assert!(!err, "{name} must succeed: {text}");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name} must return JSON ({e}): {text}"))
}

/// Call a tool that must be REFUSED, and return the refusal text.
fn mcp_refusal(reg: &OrchRegistry, c: &Caller, name: &str, args: Value) -> String {
    let (err, text) = call(reg, c, name, args);
    assert!(err, "{name} should have been refused, got: {text}");
    text
}

fn listed_tools(reg: &OrchRegistry, c: &Caller) -> Vec<String> {
    dispatch(reg, c, "tools/list", &json!({})).unwrap()["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap_or("").to_string())
        .collect()
}

fn audit_actions(reg: &OrchRegistry, group: &GroupId) -> Vec<String> {
    reg.audit_log(group).into_iter().map(|e| e.action).collect()
}

/// The seven, in one place, so a test cannot cover six of them by accident.
///
/// #3263 S5 added `todo_restore` HERE rather than in each test, which is the
/// point of the constant: the three default-deny surface tests below — every
/// delegate role, the manager, the lead — widen with it, so a seventh tool
/// cannot reach a role whose two gates were not both edited.
const TODO_TOOLS: [&str; 7] = [
    "todo_list",
    "todo_get",
    "todo_add",
    "todo_update",
    "todo_complete",
    "todo_delete",
    "todo_restore",
];

// ---------- the surface: who sees the tools, and who may dispatch them ----------

/// **Every non-Solo role sees all seven and may dispatch them.**
///
/// Both halves matter and they are different claims. The listing is cosmetic
/// (`tool_defs`); the dispatch check is the real gate, and this repo's own
/// `add-orch-tool` checklist says so. A tool that listed but did not dispatch
/// would be invisible in a way nothing else here would catch, and one that
/// dispatched without listing is the "works but is invisible to agents" failure
/// the same checklist names.
///
/// Manager and lead get their own tests below rather than joining this loop:
/// each has a POSITIVE, default-deny enumeration at BOTH gates, so for those
/// two classes "it is on the shared tier" implies nothing at all — and each
/// needs a differently-shaped group to exist in.
#[test]
fn mcp_every_delegate_role_sees_and_may_dispatch_all_seven_todo_tools() {
    let _root = DataRoot::install();
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = tempfile::tempdir().unwrap();
    let g = reg.create_group(&repo.path().to_string_lossy(), mcp_rails()).unwrap();

    for role in [Role::Orchestrator, Role::Worker, Role::Reviewer, Role::Planner] {
        let a = reg.spawn_agent(&g.id, role, "pane", "task", false, None).unwrap();
        let c = caller_for(&reg, &a);
        let names = listed_tools(&reg, &c);
        for tool in TODO_TOOLS {
            assert!(names.contains(&tool.to_string()), "{role:?} must be OFFERED {tool}: {names:?}");
        }
        // The real gate: the arm dispatches rather than being refused by role.
        let out = ok_json(&reg, &c, "todo_list", json!({ "scope": "global" }));
        assert!(out["items"].is_array(), "{role:?}'s todo_list returns rows: {out}");
    }
}

/// **A manager sees and may dispatch all seven.**
///
/// Its own test because a manager needs a workflow file to exist at all, and
/// because `MANAGER_SHARED` plus the manager dispatch gate are two independent
/// default-deny lists: a tool added to the shared tier reaches a manager only
/// if BOTH name it, and the halves are spelled separately on purpose so they
/// cannot drift together.
#[test]
fn mcp_a_manager_sees_and_may_dispatch_all_seven_todo_tools() {
    let _root = DataRoot::install();
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = tempfile::tempdir().unwrap();
    let wf = repo.path().join(".loomux");
    std::fs::create_dir_all(&wf).unwrap();
    std::fs::write(
        wf.join("workflow.yml"),
        "version: 1\nname: with-a-manager\n\
         blocks:\n\
         \x20 - id: manager\n    kind: manager\n\
         \x20 - id: worker\n    kind: worker\n\
         \x20 - id: reviewer\n    kind: reviewer\n",
    )
    .unwrap();
    let g = reg
        .create_group(
            &repo.path().to_string_lossy(),
            Guardrails { advanced_orchestrator: true, ..mcp_rails() },
        )
        .unwrap();
    // The fixture asserts its own validity: a workflow that never parsed would
    // leave the built-in roster in place and this would be testing a worker.
    assert!(
        reg.manager_block(&g.id).is_some(),
        "fixture must really declare a manager block, or nothing below tests what it says"
    );
    let mgr = reg.spawn_agent(&g.id, Role::Manager, "manager", "", false, None).unwrap();
    assert_eq!(mgr.role, Role::Manager, "precondition: the pane really is a manager");
    let cm = caller_for(&reg, &mgr);

    let names = listed_tools(&reg, &cm);
    for tool in TODO_TOOLS {
        assert!(names.contains(&tool.to_string()), "a manager must be OFFERED {tool}: {names:?}");
    }
    // Happy path through the real dispatch gate, not just the listing.
    let item = ok_json(&reg, &cm, "todo_add", json!({ "title": "ask about the roadmap" }));
    assert_eq!(item["title"], json!("ask about the roadmap"));
    assert_eq!(
        item["created_by"]["role"],
        json!("manager"),
        "the item records the manager's own class: {item}"
    );
}

/// **A lead sees and may dispatch all seven.**
///
/// Its own test for the manager's reason exactly: `LEAD_SHARED` and the lead
/// dispatch gate are two separate default-deny enumerations, so nothing about
/// the shared tier reaches a lead by itself. The `workspace` scope is driven
/// deliberately rather than `global`: "a lead group has a repo, so workspace
/// scope resolves" is the half of the grant's argument that could stop being
/// true, and only a workspace-scoped write witnesses it.
#[test]
fn mcp_a_lead_sees_and_may_dispatch_all_seven_todo_tools() {
    let _root = DataRoot::install();
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = tempfile::tempdir().unwrap();
    let rails = Guardrails {
        blocks: workflow::default_roster(&[(Role::Lead, "claude", ""), (Role::Worker, "claude", "")]),
        ..mcp_rails()
    };
    let g = reg.create_group(&repo.path().to_string_lossy(), rails).unwrap();
    assert!(
        reg.group(g.id.as_str()).unwrap().guardrails.block_for(Role::Lead).is_some(),
        "fixture must really carry a lead block, or nothing below tests what it says"
    );
    let lead = reg.spawn_agent(&g.id, Role::Lead, "lead", "", false, None).unwrap();
    assert_eq!(lead.role, Role::Lead, "precondition: the pane really is a lead");
    let cl = caller_for(&reg, &lead);

    let names = listed_tools(&reg, &cl);
    for tool in TODO_TOOLS {
        assert!(names.contains(&tool.to_string()), "a lead must be OFFERED {tool}: {names:?}");
    }
    let item = ok_json(&reg, &cl, "todo_add", json!({ "title": "chase the flaky test" }));
    assert!(item["scope"]["workspace"].is_string(), "workspace scope resolves for a lead: {item}");
    assert_eq!(item["created_by"]["role"], json!("lead"), "{item}");
}

/// **A solo pane gets none of the seven, at both gates.**
///
/// The negative control for the sweep above, and the one class whose exclusion
/// is structural rather than enumerated: `tool_defs` returns the channel pair
/// before the shared tier is built at all, and `call_tool` refuses anything but
/// those two before the match. A to-do is the human's personal data, but it is
/// still reached through a group-scoped registry, and "a solo token confers
/// zero group-scoped power" is a flat rule this feature does not get to bend.
///
/// The refusal TEXT is asserted, not just the refusal: a solo caller reaching a
/// `title required` would mean it had passed the solo gate and been stopped by
/// an argument check, which is a different and much weaker guarantee.
#[test]
fn mcp_a_solo_pane_gets_no_todo_tools_at_all() {
    let _root = DataRoot::install();
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let prepared = reg.solo_prepare("claude", "C:/tmp/solo", "solo pane").unwrap();
    let agent_id = prepared["agent_id"].as_str().unwrap().to_string();
    reg.solo_bind(&agent_id, 503).unwrap();
    let token = reg.agent(&agent_id).unwrap().token;
    let cs = reg.resolve_token(&token).expect("a solo pane's token must resolve");
    assert_eq!(cs.role, Role::Solo, "precondition: the pane really is solo");

    let names = listed_tools(&reg, &cs);
    for tool in TODO_TOOLS {
        assert!(
            !names.contains(&tool.to_string()),
            "a solo pane must not be offered {tool}: {names:?}"
        );
        // Arguments that would SATISFY every one of the seven, so the refusal
        // below cannot be an argument check wearing the gate's clothes.
        let text =
            mcp_refusal(&reg, &cs, tool, json!({ "id": "td-1", "title": "x", "done": true }));
        assert!(
            text.contains("no group-scoped power"),
            "{tool} must be refused by the SOLO gate, not by an argument check: {text}"
        );
    }
}

// ---------- the happy path ----------

/// **Add, list, get, update, complete, delete — one item through six of the
/// seven (restore has its own tests below), with the audit row each leaves.**
///
/// Asserted on the store's own read-back (`todo_list` / `todo_get`) rather than
/// on the reply alone: a reply is what the arm computed, and the point of the
/// feature is what it WROTE.
#[test]
fn mcp_a_worker_can_add_groom_complete_and_delete_a_workspace_todo() {
    let _root = DataRoot::install();
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = tempfile::tempdir().unwrap();
    let (gid, _co, cw) = mcp_group(&reg, repo.path());

    // ADD
    let added = ok_json(
        &reg,
        &cw,
        "todo_add",
        json!({
            "title": "fix the flaky resize test",
            "notes": "see #3263",
            "priority": 2,
            "tags": ["infra"],
            "steps": ["reproduce", "fix"],
        }),
    );
    let id = added["id"].as_str().expect("add returns the item").to_string();
    assert_eq!(added["created_by"]["kind"], json!("agent"), "{added}");
    assert_eq!(
        added["created_by"]["id"],
        json!(cw.agent_id),
        "the item records the CALLER, from the registry: {added}"
    );
    assert!(added["scope"]["workspace"].is_string(), "the default scope is workspace: {added}");

    // LIST — the compact row, and the item really is in the store.
    let listed = ok_json(&reg, &cw, "todo_list", json!({}));
    let rows = listed["items"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "exactly the item just added: {listed}");
    assert_eq!(rows[0]["id"], json!(id));
    assert_eq!(rows[0]["steps_total"], json!(2), "the row carries the step counts: {listed}");
    assert_eq!(rows[0]["steps_done"], json!(0));
    assert!(rows[0].get("notes").is_none(), "the compact row is NOT the whole item: {listed}");

    // GET — the full record, including what the row left out.
    let got = ok_json(&reg, &cw, "todo_get", json!({ "id": id }));
    assert_eq!(got["notes"], json!("see #3263"), "{got}");
    assert_eq!(got["priority"], json!(2));

    // UPDATE — grooming, with the rev guard the tool's own description tells a
    // caller to pass.
    let rev = got["rev"].as_u64().expect("the item carries a rev");
    let updated = ok_json(
        &reg,
        &cw,
        "todo_update",
        json!({ "id": id, "if_rev": rev, "title": "fix the resize flake", "priority": 3 }),
    );
    assert_eq!(updated["title"], json!("fix the resize flake"));
    assert_eq!(updated["priority"], json!(3));
    assert_eq!(updated["notes"], json!("see #3263"), "an omitted field is LEFT ALONE: {updated}");

    // COMPLETE — and the default listing stops showing it, which is the
    // behaviour a caller actually observes.
    ok_json(&reg, &cw, "todo_complete", json!({ "id": id, "done": true }));
    let after = ok_json(&reg, &cw, "todo_list", json!({}));
    assert_eq!(
        after["items"].as_array().unwrap().len(),
        0,
        "a done item is hidden by default: {after}"
    );
    let with_done = ok_json(&reg, &cw, "todo_list", json!({ "include_done": true }));
    assert_eq!(
        with_done["items"].as_array().unwrap()[0]["status"],
        json!("done"),
        "{with_done}"
    );

    // DELETE — a tombstone, so the id reads as unknown afterwards.
    ok_json(&reg, &cw, "todo_delete", json!({ "id": id }));
    let text = mcp_refusal(&reg, &cw, "todo_get", json!({ "id": id }));
    assert_eq!(
        text,
        format!("unknown todo: {id}"),
        "a deleted id must read exactly as one that never existed"
    );

    // Every write left its audit row on the CALLER's group.
    let actions = audit_actions(&reg, &gid);
    for expected in ["todo-add", "todo-update", "todo-complete", "todo-delete"] {
        assert!(
            actions.contains(&expected.to_string()),
            "the write must be auditable as {expected}: {actions:?}"
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

/// **The GLOBAL list is one list, and every group reaches it.**
///
/// The positive control for the cross-workspace refusal below. Without it, that
/// test passes just as well against an implementation where a group cannot see
/// anything it did not write — which would be a different, wrong feature, and
/// "the one list that follows the human everywhere" would be false.
#[test]
fn mcp_the_global_list_is_the_same_list_from_every_group() {
    let _root = DataRoot::install();
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo_a = tempfile::tempdir().unwrap();
    let repo_b = tempfile::tempdir().unwrap();
    let (_ga, _oa, wa) = mcp_group(&reg, repo_a.path());
    let (_gb, _ob, wb) = mcp_group(&reg, repo_b.path());

    let added =
        ok_json(&reg, &wa, "todo_add", json!({ "scope": "global", "title": "renew the domain" }));
    let id = added["id"].as_str().unwrap().to_string();

    // Group B reads it, gets it, and may groom it — it is the HUMAN's list.
    let listed = ok_json(&reg, &wb, "todo_list", json!({ "scope": "global" }));
    assert_eq!(listed["items"].as_array().unwrap()[0]["id"], json!(id), "{listed}");
    let got = ok_json(&reg, &wb, "todo_get", json!({ "id": id }));
    assert_eq!(got["title"], json!("renew the domain"), "{got}");
    let done = ok_json(&reg, &wb, "todo_complete", json!({ "id": id, "done": true }));
    assert_eq!(done["status"], json!("done"), "{done}");
}

// ---------- refusals ----------

/// **Another workspace's id is `unknown todo`, at every tool that takes one —
/// and the refusal is audited.**
///
/// The wording is the whole point: a distinct "not yours" would let a caller
/// probe another project's list for which ids exist, which is the leak the
/// plan's refusal table rules out. So this asserts the EXACT text rather than
/// a substring — the same posture `require_in_group`'s "unknown agent" takes.
///
/// Its positive control is the test above: group B reaching the same store
/// through the GLOBAL scope succeeds, so the refusal here is about the
/// workspace and not about a group being unable to see anything at all.
#[test]
fn mcp_an_id_in_another_groups_workspace_reads_as_unknown_and_is_audited() {
    let _root = DataRoot::install();
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo_a = tempfile::tempdir().unwrap();
    let repo_b = tempfile::tempdir().unwrap();
    let (_ga, _oa, wa) = mcp_group(&reg, repo_a.path());
    let (gb, _ob, wb) = mcp_group(&reg, repo_b.path());

    let added = ok_json(&reg, &wa, "todo_add", json!({ "title": "A's own note" }));
    let id = added["id"].as_str().unwrap().to_string();

    // Precondition: group A really can see it, so the refusals below are about
    // WHO is asking and not about the item having failed to land.
    assert_eq!(
        ok_json(&reg, &wa, "todo_get", json!({ "id": id }))["title"],
        json!("A's own note")
    );

    // Vacuity control: B's audit carries no refusal yet, so the rows counted
    // afterwards were written by THESE calls.
    assert!(
        !audit_actions(&reg, &gb).contains(&"todo-refused".to_string()),
        "precondition: group B has refused nothing yet"
    );

    for (tool, args) in [
        ("todo_get", json!({ "id": id })),
        ("todo_update", json!({ "id": id, "title": "taken over" })),
        ("todo_complete", json!({ "id": id, "done": true })),
        ("todo_delete", json!({ "id": id })),
    ] {
        let text = mcp_refusal(&reg, &wb, tool, args);
        assert_eq!(
            text,
            format!("unknown todo: {id}"),
            "{tool} must refuse with EXACTLY the wording an id that never existed gets — \
             anything more tells B that A's id is real"
        );
    }

    // …and A's item is untouched: the refusals refused, they did not half-apply.
    let still = ok_json(&reg, &wa, "todo_get", json!({ "id": id }));
    assert_eq!(still["title"], json!("A's own note"), "{still}");
    assert_eq!(still["status"], json!("open"), "{still}");
    assert_eq!(still["rev"], json!(1), "no write landed, so the rev did not move: {still}");

    // Every one of the four left a `todo-refused` row on B's OWN group.
    let refused = audit_actions(&reg, &gb).into_iter().filter(|a| a == "todo-refused").count();
    assert_eq!(refused, 4, "each refused call is auditable: {:?}", audit_actions(&reg, &gb));
}

/// **A stale `if_rev` is refused, names both revs, and changes nothing.**
#[test]
fn mcp_a_stale_if_rev_is_refused_and_names_both_revs() {
    let _root = DataRoot::install();
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = tempfile::tempdir().unwrap();
    let (gid, co, cw) = mcp_group(&reg, repo.path());

    let added = ok_json(&reg, &cw, "todo_add", json!({ "title": "one" }));
    let id = added["id"].as_str().unwrap().to_string();
    let stale = added["rev"].as_u64().unwrap();

    // Someone else writes first — the orchestrator, so the two callers really
    // are different panes and the conflict is the one the tool describes.
    ok_json(
        &reg,
        &co,
        "todo_update",
        json!({ "id": id, "notes": "the orchestrator got here first" }),
    );

    let text =
        mcp_refusal(&reg, &cw, "todo_update", json!({ "id": id, "if_rev": stale, "title": "two" }));
    assert_eq!(
        text,
        format!("conflict: {id} is at rev {} (you sent {stale})", stale + 1),
        "the refusal must name BOTH revs so the caller can re-read and re-apply"
    );

    // Nothing was written: the title is untouched and the other pane's note
    // survives — a conflict must not half-apply.
    let after = ok_json(&reg, &cw, "todo_get", json!({ "id": id }));
    assert_eq!(after["title"], json!("one"), "{after}");
    assert_eq!(after["notes"], json!("the orchestrator got here first"), "{after}");

    // Re-reading and re-applying to what is there NOW is what the tool tells
    // the caller to do, and it works.
    let fresh = after["rev"].as_u64().unwrap();
    let ok =
        ok_json(&reg, &cw, "todo_update", json!({ "id": id, "if_rev": fresh, "title": "two" }));
    assert_eq!(ok["title"], json!("two"), "{ok}");

    assert!(
        audit_actions(&reg, &gid).contains(&"todo-refused".to_string()),
        "the conflict is auditable: {:?}",
        audit_actions(&reg, &gid)
    );
}

/// **A cap refuses rather than truncating, and the refusal is audited.**
///
/// The title cap, driven through the TOOL rather than through the engine: the
/// engine's own caps are pinned above, and what this adds is that the MCP arm
/// surfaces the refusal as a tool error carrying the engine's wording instead
/// of swallowing it or writing a shortened title. A runaway agent loop is the
/// shape being bounded, so the audit row is the half the human needs.
#[test]
fn mcp_a_cap_refusal_comes_back_as_a_tool_error_and_is_audited() {
    let _root = DataRoot::install();
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = tempfile::tempdir().unwrap();
    let (gid, _co, cw) = mcp_group(&reg, repo.path());

    assert!(
        !audit_actions(&reg, &gid).contains(&"todo-refused".to_string()),
        "precondition: nothing refused yet, so the row below is THIS call's"
    );

    let over = "x".repeat(TITLE_MAX + 1);
    let text = mcp_refusal(&reg, &cw, "todo_add", json!({ "title": over }));
    assert_eq!(
        text,
        format!("refused: title exceeds {TITLE_MAX}"),
        "the arm surfaces the engine's own cap wording"
    );

    // NOTHING was written — a cap refuses, it does not truncate.
    let listed = ok_json(&reg, &cw, "todo_list", json!({}));
    assert_eq!(
        listed["items"].as_array().unwrap().len(),
        0,
        "no shortened item landed: {listed}"
    );

    assert!(
        audit_actions(&reg, &gid).contains(&"todo-refused".to_string()),
        "a cap that bounces a runaway loop is exactly the event the human must be able to find \
         afterwards: {:?}",
        audit_actions(&reg, &gid)
    );
}

/// **A malformed argument is refused by name, and nothing reaches the store.**
#[test]
fn mcp_a_malformed_argument_is_refused_before_anything_is_written() {
    let _root = DataRoot::install();
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = tempfile::tempdir().unwrap();
    let (_gid, _co, cw) = mcp_group(&reg, repo.path());

    let text = mcp_refusal(&reg, &cw, "todo_add", json!({ "title": "x", "scope": "everywhere" }));
    assert!(text.contains("scope must be"), "the refusal names the shape it wanted: {text}");
    let empty = ok_json(&reg, &cw, "todo_list", json!({}));
    assert_eq!(empty["items"].as_array().unwrap().len(), 0, "nothing landed: {empty}");

    // A string where a number belongs is refused rather than silently dropped —
    // a dropped `if_rev` is the concurrency guard going quiet while telling the
    // caller its guarded write landed.
    let added = ok_json(&reg, &cw, "todo_add", json!({ "title": "real" }));
    let id = added["id"].as_str().unwrap().to_string();
    let text = mcp_refusal(&reg, &cw, "todo_update", json!({ "id": id, "if_rev": "1", "title": "y" }));
    assert!(text.contains("if_rev must be a whole number"), "{text}");
    let after = ok_json(&reg, &cw, "todo_get", json!({ "id": id }));
    assert_eq!(after["title"], json!("real"), "the refused call wrote nothing: {after}");
}

/// **`query` requires EVERY term to match, case-insensitively, over the title
/// and the notes.**
#[test]
fn mcp_the_list_query_requires_every_term_to_match() {
    let _root = DataRoot::install();
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = tempfile::tempdir().unwrap();
    let (_gid, _co, cw) = mcp_group(&reg, repo.path());

    ok_json(
        &reg,
        &cw,
        "todo_add",
        json!({ "title": "Fix the RESIZE flake", "notes": "windows only" }),
    );
    ok_json(&reg, &cw, "todo_add", json!({ "title": "Write the design note" }));

    let hit = ok_json(&reg, &cw, "todo_list", json!({ "query": "resize flake" }));
    assert_eq!(hit["items"].as_array().unwrap().len(), 1, "case-insensitive, both terms: {hit}");

    // A term matching only the NOTES still counts — the field is part of what a
    // human searches, and a title-only filter would quietly miss it.
    let notes = ok_json(&reg, &cw, "todo_list", json!({ "query": "windows" }));
    assert_eq!(notes["items"].as_array().unwrap().len(), 1, "{notes}");

    // ALL terms must match: one that does not appear excludes the row.
    let miss = ok_json(&reg, &cw, "todo_list", json!({ "query": "resize macos" }));
    assert_eq!(miss["items"].as_array().unwrap().len(), 0, "every term must match: {miss}");

    // An empty query is "no filter", not "a filter nothing satisfies".
    let all = ok_json(&reg, &cw, "todo_list", json!({ "query": "   " }));
    assert_eq!(all["items"].as_array().unwrap().len(), 2, "{all}");
}

/// **The workspace a write lands in is the CALLER's own repo, derived and never
/// passed.**
///
/// Two groups on two repos write with the default scope and land in two
/// different lists — the containment claim behind the whole feature, stated as
/// a fact about the store rather than as a fact about the argument list. A
/// `scope` argument that could name a key would pass every other test here and
/// fail this one.
#[test]
fn mcp_the_workspace_a_write_lands_in_is_the_callers_own_repo() {
    let _root = DataRoot::install();
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo_a = tempfile::tempdir().unwrap();
    let repo_b = tempfile::tempdir().unwrap();
    let (_ga, _oa, wa) = mcp_group(&reg, repo_a.path());
    let (_gb, _ob, wb) = mcp_group(&reg, repo_b.path());

    let a = ok_json(&reg, &wa, "todo_add", json!({ "title": "A" }));
    let b = ok_json(&reg, &wb, "todo_add", json!({ "title": "B" }));
    assert_ne!(
        a["scope"]["workspace"], b["scope"]["workspace"],
        "two repos are two workspaces: {a} / {b}"
    );
    assert_eq!(
        a["scope"]["workspace"],
        json!(todo::workspace_key(repo_a.path())),
        "and the key is the one `workspace_key` derives from the group's own repo"
    );

    // Each group's default listing shows only its own.
    let la = ok_json(&reg, &wa, "todo_list", json!({}));
    let lb = ok_json(&reg, &wb, "todo_list", json!({}));
    assert_eq!(la["items"].as_array().unwrap().len(), 1, "{la}");
    assert_eq!(la["items"].as_array().unwrap()[0]["title"], json!("A"), "{la}");
    assert_eq!(lb["items"].as_array().unwrap()[0]["title"], json!("B"), "{lb}");
}

// ==================== the archive op (#3263 slice S5) ====================
//
// `archived_ms` had a reader and no writer until this slice: `inView` excluded
// an archived item from every view and `todo_list` filtered one out, and no
// fixture in the tree could build one. The design note recorded that as a
// residual ("One residual, stated so it is falsifiable") and said the slice
// that gave the field a writer owned the test. This is it.

/// The one thing this op is actually for: the pane's Completed view says
/// "Archive 7" and seven rows go away.
#[test]
fn an_archive_puts_the_named_items_away_and_leaves_the_rest_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    let a = apply_to(&path, add("done one"), &human(), T0, None).unwrap().ids[0].clone();
    let b = apply_to(&path, add("done two"), &human(), T0, None).unwrap().ids[0].clone();
    let keep = apply_to(&path, add("still open"), &human(), T0, None).unwrap().ids[0].clone();

    // Revs BEFORE the archive. Asserted as a DELTA below rather than against a
    // remembered absolute: what a fresh `add` leaves `rev` at is `apply_add`'s
    // business, and a test that hardcodes it measures that instead of this op.
    let rev_of = |id: &str| {
        snapshot_at(&path, None).items.iter().find(|i| i.id == id).unwrap().rev
    };
    let (rev_a, rev_keep) = (rev_of(&a), rev_of(&keep));

    let applied = apply_to(
        &path,
        TodoOp::Archive {
            ids: vec![a.clone(), b.clone()],
            archived: true,
        },
        &agent(),
        T0 + 5,
        None,
    )
    .expect("archiving two live items must succeed");

    assert_eq!(
        applied.ids,
        vec![a.clone(), b.clone()],
        "the op answers what it moved"
    );
    assert!(
        applied.item.is_none(),
        "a bulk op must not answer with ONE item — a caller would read it as the item that changed"
    );
    assert_eq!(
        TodoOp::Archive {
            ids: vec![a.clone()],
            archived: true
        }
        .action(),
        "todo-archive",
        "the audit row needs its own action name"
    );

    let items = snapshot_at(&path, None).items;
    let find = |id: &str| items.iter().find(|i| i.id == id).cloned().unwrap();
    assert_eq!(
        find(&a).archived_ms,
        Some(T0 + 5),
        "the stamp is the write's clock"
    );
    assert_eq!(find(&b).archived_ms, Some(T0 + 5));
    assert_eq!(
        find(&keep).archived_ms,
        None,
        "an id the op did not name must not move"
    );
    assert_eq!(find(&keep).rev, rev_keep, "and must not even be touched");
    assert_eq!(find(&a).rev, rev_a + 1, "an archive is exactly one write");
    assert_eq!(
        find(&a).updated_by,
        agent(),
        "attributed to whoever performed it"
    );
    assert_eq!(find(&a).updated_ms, T0 + 5);

    // ARCHIVED IS NOT DELETED. The snapshot still carries it, which is what
    // makes the Completed view's "show archived" toggle possible at all and
    // what makes the op's own inverse reachable.
    assert!(
        snapshot_at(&path, None).items.iter().any(|i| i.id == a),
        "an archived item must still reach a reader — it is live data put away, not a tombstone"
    );
    assert_eq!(
        find(&a).deleted_ms,
        None,
        "archiving must not tombstone anything"
    );
}

/// The inverse the pane's undo sends, and the reason the op carries a
/// direction rather than being one-way.
#[test]
fn un_archiving_the_same_ids_is_an_exact_inverse() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    let a = apply_to(&path, add("one"), &human(), T0, None).unwrap().ids[0].clone();
    let b = apply_to(&path, add("two"), &human(), T0, None).unwrap().ids[0].clone();

    // Looked up BY ID rather than by position: the snapshot's order is the
    // store's, and a test that indexed into it would be pinning that instead.
    let rev_of = |id: &str| {
        snapshot_at(&path, None).items.iter().find(|i| i.id == id).unwrap().rev
    };
    let rev_before = [(a.clone(), rev_of(&a)), (b.clone(), rev_of(&b))];

    apply_to(
        &path,
        TodoOp::Archive {
            ids: vec![a.clone(), b.clone()],
            archived: true,
        },
        &human(),
        T0 + 1,
        None,
    )
    .unwrap();
    apply_to(
        &path,
        TodoOp::Archive {
            ids: vec![a.clone(), b.clone()],
            archived: false,
        },
        &human(),
        T0 + 2,
        None,
    )
    .expect("un-archiving must be the same op with the flag flipped");

    let items = snapshot_at(&path, None).items;
    for (id, was) in &rev_before {
        let item = items.iter().find(|i| &i.id == id).unwrap();
        assert_eq!(
            item.archived_ms, None,
            "un-archive must clear the stamp, not re-stamp it"
        );
        assert_eq!(
            item.rev,
            was + 2,
            "two writes, so two revs — a delta, not what a fresh add happens to leave"
        );
    }
}

/// Every refusal, and the half that matters more than the message: the store
/// is BYTE-IDENTICAL afterwards. A partial archive would leave the human with
/// some rows away and some not, and an undo naming all of them would then
/// un-archive rows the forward op never touched.
#[test]
fn an_archive_refuses_before_it_mutates_anything() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    let good = apply_to(&path, add("keep me"), &human(), T0, None).unwrap().ids[0].clone();
    let gone = apply_to(&path, add("tombstoned"), &human(), T0, None).unwrap().ids[0].clone();
    apply_to(
        &path,
        TodoOp::Delete { id: gone.clone() },
        &human(),
        T0 + 1,
        None,
    )
    .unwrap();
    let before = read_raw(&path);

    // An id that never existed, WITH a good id in front of it: the refusal has
    // to come before the good one is written, not after.
    let e = unwrap_err_for(
        apply_to(
            &path,
            TodoOp::Archive {
                ids: vec![good.clone(), "td-nope".to_string()],
                archived: true,
            },
            &human(),
            T0 + 2,
            None,
        ),
        "an unknown id",
    );
    assert_eq!(e.to_string(), "unknown todo: td-nope");
    assert_eq!(
        read_raw(&path),
        before,
        "the good id in front of it was written anyway"
    );

    // A TOMBSTONE reads exactly as an id that never existed — the rule every
    // other op here follows.
    let e = unwrap_err_for(
        apply_to(
            &path,
            TodoOp::Archive {
                ids: vec![gone.clone()],
                archived: true,
            },
            &human(),
            T0 + 2,
            None,
        ),
        "a tombstoned id",
    );
    assert_eq!(e.to_string(), format!("unknown todo: {gone}"));
    assert_eq!(read_raw(&path), before);

    // No ids at all. An empty archive is indistinguishable from one that
    // worked, and it has no inverse either.
    let e = unwrap_err_for(
        apply_to(
            &path,
            TodoOp::Archive {
                ids: vec![],
                archived: true,
            },
            &human(),
            T0 + 2,
            None,
        ),
        "an empty id list",
    );
    assert_eq!(e.to_string(), "refused: archive names no items");
    assert_eq!(read_raw(&path), before);

    // The SAME id twice. Two spellings of one write make `ids` mean something
    // other than what it says.
    let e = unwrap_err_for(
        apply_to(
            &path,
            TodoOp::Archive {
                ids: vec![good.clone(), good.clone()],
                archived: true,
            },
            &human(),
            T0 + 2,
            None,
        ),
        "a repeated id",
    );
    assert_eq!(
        e.to_string(),
        format!("refused: archive names {good} more than once")
    );
    assert_eq!(read_raw(&path), before);

    // THE POSITIVE CONTROL. Without it every assertion above passes against an
    // op that refuses everything, and "the store did not change" would be the
    // loudest thing in this file saying nothing.
    apply_to(
        &path,
        TodoOp::Archive {
            ids: vec![good.clone()],
            archived: true,
        },
        &human(),
        T0 + 3,
        None,
    )
    .expect("the same shape, with a good id, must succeed");
    assert_ne!(
        read_raw(&path),
        before,
        "the control did not move the store either"
    );
}

/// Archiving a row that is already away is ACCEPTED, and deliberately — the
/// opposite of `restore`'s "a no-op is indistinguishable from a success".
#[test]
fn archiving_an_already_archived_row_is_accepted_because_this_op_is_bulk() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    let id = apply_to(&path, add("x"), &human(), T0, None).unwrap().ids[0].clone();
    apply_to(
        &path,
        TodoOp::Archive {
            ids: vec![id.clone()],
            archived: true,
        },
        &human(),
        T0 + 1,
        None,
    )
    .unwrap();
    apply_to(
        &path,
        TodoOp::Archive {
            ids: vec![id.clone()],
            archived: true,
        },
        &human(),
        T0 + 2,
        None,
    )
    .expect(
        "a bulk op names a state it wants, so an already-archived row in the batch is not an \
         error — refusing would make the pane's Archive button fail exactly when a human retries it",
    );
    let items = snapshot_at(&path, None).items;
    let item = items.iter().find(|i| i.id == id).unwrap();
    assert_eq!(
        item.archived_ms,
        Some(T0 + 2),
        "the second write re-stamps rather than being a no-op"
    );
}

/// The cap, because `ids` is caller JSON.
#[test]
fn an_archive_naming_more_than_the_cap_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    let id = apply_to(&path, add("x"), &human(), T0, None).unwrap().ids[0].clone();
    let before = read_raw(&path);
    // Distinct ids, so the duplicate check cannot be what refuses this — the
    // refusal has to be the CAP, and the message says which.
    let ids: Vec<String> = (0..ARCHIVE_IDS_MAX + 1)
        .map(|n| format!("td-{n:016x}"))
        .collect();
    let e = unwrap_err_for(
        apply_to(
            &path,
            TodoOp::Archive { ids, archived: true },
            &human(),
            T0 + 1,
            None,
        ),
        "more ids than the cap",
    );
    assert_eq!(
        e.to_string(),
        format!("refused: archive exceeds {ARCHIVE_IDS_MAX}")
    );
    assert_eq!(read_raw(&path), before);

    // And exactly AT the cap passes the count check — what stops it there is
    // the NEXT rule, the unknown ids. Without this the assertion above would
    // hold just as well against an off-by-one that refused at the cap itself.
    let mut ids: Vec<String> = (0..ARCHIVE_IDS_MAX - 1)
        .map(|n| format!("td-{n:016x}"))
        .collect();
    ids.push(id);
    let e = unwrap_err_for(
        apply_to(
            &path,
            TodoOp::Archive { ids, archived: true },
            &human(),
            T0 + 1,
            None,
        ),
        "exactly the cap",
    );
    assert!(
        e.to_string().starts_with("unknown todo:"),
        "at the cap the refusal must be about the ids, not the count: {e}"
    );
}

// ---------- the decoder's archive arm ----------

#[test]
fn the_archive_op_decodes_from_the_shape_the_pane_sends() {
    // Default-deny again: without an arm here `{"archive": …}` falls to
    // `parse_op`'s `other` arm and the pane's one bulk gesture is unreachable.
    match parse_op(&json!({"archive": {"ids": ["td-1", "td-2"]}}), Scope::Global) {
        Ok(TodoOp::Archive { ids, archived }) => {
            assert_eq!(ids, vec!["td-1".to_string(), "td-2".to_string()]);
            assert!(
                archived,
                "an omitted `archived` means archive, as `complete`'s `done` does"
            );
        }
        other => panic!("an archive must decode to TodoOp::Archive, got {other:?}"),
    }
    match parse_op(
        &json!({"archive": {"ids": ["td-1"], "archived": false}}),
        Scope::Global,
    ) {
        Ok(TodoOp::Archive { archived, .. }) => {
            assert!(!archived, "the inverse must be spellable")
        }
        other => panic!("an un-archive must decode, got {other:?}"),
    }

    // Default-deny on the body, and on the one required field.
    let msg = refusal(
        json!({"archive": {"ids": ["td-1"], "when": 1}}),
        Scope::Global,
        "an archive carrying an unknown key",
    );
    assert!(msg.contains("when"), "the refusal must name the key, got: {msg}");
    let msg = refusal(
        json!({"archive": {}}),
        Scope::Global,
        "an archive with no ids",
    );
    assert!(msg.contains("ids is required"), "got: {msg}");
    let msg = refusal(
        json!({"archive": {"ids": [7]}}),
        Scope::Global,
        "an archive whose ids are not strings",
    );
    assert!(msg.contains("ids"), "got: {msg}");
}

#[test]
fn the_unknown_op_message_names_every_op_the_decoder_takes() {
    // The message is a CONTRACT `src/todo.ts` is written against, and it is the
    // one place a caller learns what the wire accepts. An op added to the
    // decoder and not to this sentence is a capability nobody can discover.
    let msg = refusal(
        json!({"nope": {}}),
        Scope::Global,
        "an op the decoder has no arm for",
    );
    for op in ["add", "update", "complete", "delete", "restore", "archive"] {
        assert!(msg.contains(op), "the unknown-op message must name `{op}`: {msg}");
    }
    let msg = refusal(json!({}), Scope::Global, "an op naming nothing");
    assert!(
        msg.contains("archive"),
        "the zero-op message must name every op too: {msg}"
    );
}

// ==================== `todo_restore`, the seventh tool (#3263 slice S5) ====================
//
// S2 shipped six tools while the engine had no `restore` op; #3285 added the
// op with no tool; this slice closes the gap. The design note's "No seventh
// tool" section asked whoever took it to make the capability argument, and the
// tests below are the half of that argument a reader can check: it reaches the
// same surfaces the six do (the three default-deny gates above cover that,
// because `TODO_TOOLS` now has seven entries), and it widens NOTHING — a
// tombstone in another project's list is exactly as invisible as a live row
// there, which is the property a tombstone-visible read could have broken.

/// The happy path, end to end through the REAL dispatch: an agent deletes its
/// own row and puts it back.
#[test]
fn mcp_an_agent_can_restore_a_to_do_it_deleted() {
    let _root = DataRoot::install();
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = tempfile::tempdir().unwrap();
    let (g, _orch, w) = mcp_group(&reg, repo.path());

    let added = ok_json(
        &reg,
        &w,
        "todo_add",
        json!({ "title": "keep this", "notes": "with a note", "tags": ["x"] }),
    );
    let id = added["id"].as_str().unwrap().to_string();
    ok_json(&reg, &w, "todo_delete", json!({ "id": id }));
    let listed = ok_json(&reg, &w, "todo_list", json!({}));
    assert_eq!(
        listed["items"].as_array().unwrap().len(),
        0,
        "precondition: the tombstone is hidden from the listing: {listed}"
    );

    let back = ok_json(&reg, &w, "todo_restore", json!({ "id": id }));
    assert_eq!(back["id"], json!(id));
    assert_eq!(
        back["notes"],
        json!("with a note"),
        "a restore returns the WHOLE item — the answer to did I get the right one: {back}"
    );
    assert_eq!(back["tags"], json!(["x"]), "{back}");
    let listed = ok_json(&reg, &w, "todo_list", json!({}));
    assert_eq!(
        listed["items"].as_array().unwrap().len(),
        1,
        "the restored row must be back in the listing: {listed}"
    );

    // The audit row, under its own action name, on the CALLER's group.
    let actions = audit_actions(&reg, &g);
    assert!(
        actions.iter().any(|a| a == "todo-restore"),
        "a restore must leave its own audit row: {actions:?}"
    );
}

/// **The leak this tool could have opened, and did not.**
///
/// `todo_visible` reads `todo_snapshot`, which filters tombstones — so this arm
/// needed a tombstone-VISIBLE read, and a tombstone-visible read is exactly the
/// shape that tells a caller "that id exists, you just cannot have it". It does
/// not: the sibling applies the same scope rule to the same field, so a
/// tombstone in another project's list answers `unknown todo` — the words an id
/// that never existed answers.
///
/// The POSITIVE CONTROL is the second half and is what makes this a test about
/// the gate rather than about restore being broken: the very same id, restored
/// by the group that owns it, succeeds.
#[test]
fn mcp_a_tombstone_in_another_workspace_reads_exactly_as_an_id_that_never_existed() {
    let _root = DataRoot::install();
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo_a = tempfile::tempdir().unwrap();
    let repo_b = tempfile::tempdir().unwrap();
    let (_ga, _oa, wa) = mcp_group(&reg, repo_a.path());
    let (gb, _ob, wb) = mcp_group(&reg, repo_b.path());

    let id = ok_json(&reg, &wa, "todo_add", json!({ "title": "A's row" }))["id"]
        .as_str()
        .unwrap()
        .to_string();
    ok_json(&reg, &wa, "todo_delete", json!({ "id": id }));

    // The two messages cannot be byte-identical — each quotes the id the
    // CALLER sent, which is the one thing the caller already knows. What must
    // be identical is everything else, so the comparison is against a template
    // with the id substituted rather than against the other string.
    let never_id = "td-0000000000000000";
    let theirs = mcp_refusal(&reg, &wb, "todo_restore", json!({ "id": id }));
    let never = mcp_refusal(&reg, &wb, "todo_restore", json!({ "id": never_id }));
    assert_eq!(
        theirs,
        format!("unknown todo: {id}"),
        "a tombstone B may not see must read as an unknown id and say nothing else"
    );
    assert_eq!(never, format!("unknown todo: {never_id}"));
    assert_eq!(
        theirs.replace(&id, "<ID>"),
        never.replace(never_id, "<ID>"),
        "with the id blanked the two refusals must be the SAME sentence — anything that \
         differs is something B learned about A's list"
    );

    // B's refusal is audited on B's group, as every other refusal this layer
    // makes is — one `todo-refused` filter answers the question either way.
    let actions = audit_actions(&reg, &gb);
    assert!(
        actions.iter().any(|a| a == "todo-refused"),
        "a cross-workspace refusal must leave a row: {actions:?}"
    );

    // THE POSITIVE CONTROL. Same id, its own group, and it comes back — so the
    // assertion above is about the gate and not about a restore that never
    // works.
    let back = ok_json(&reg, &wa, "todo_restore", json!({ "id": id }));
    assert_eq!(back["id"], json!(id), "{back}");
}

/// The refusals that reach the caller with the ENGINE's own words, which is
/// what lets an agent act on one rather than guess.
///
/// Renamed at #3301 review round 1: this used to promise "an expired
/// tombstone" and run an id that had never existed — a different refusal
/// reaching the same words by a different route, so the name was a claim the
/// body did not support. The expired-tombstone arm now has its own test
/// (`mcp_an_expired_tombstone_is_refused_as_unknown_through_the_tool`), which
/// ages a real tombstone on disk.
#[test]
fn mcp_restoring_a_live_row_and_an_absent_id_each_say_which_it_is() {
    let _root = DataRoot::install();
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = tempfile::tempdir().unwrap();
    let (_g, _orch, w) = mcp_group(&reg, repo.path());

    let id = ok_json(&reg, &w, "todo_add", json!({ "title": "alive" }))["id"]
        .as_str()
        .unwrap()
        .to_string();
    let msg = mcp_refusal(&reg, &w, "todo_restore", json!({ "id": id }));
    assert!(
        msg.contains("is not deleted"),
        "a live row must say so rather than quietly succeeding: {msg}"
    );
    assert!(
        !msg.contains("unknown todo"),
        "and must NOT read as a missing id — the caller asked precisely because it did not know: {msg}"
    );

    // An id that was never here at all — NOT an expired tombstone, which is a
    // different route to the same words and has its own test below.
    let msg = mcp_refusal(&reg, &w, "todo_restore", json!({ "id": "td-ffffffffffffffff" }));
    assert_eq!(msg, "unknown todo: td-ffffffffffffffff", "got: {msg}");

    // A malformed call, so the argument check is not mistaken for the gate.
    let msg = mcp_refusal(&reg, &w, "todo_restore", json!({}));
    assert!(msg.contains("id required"), "got: {msg}");
}

/// **The residual the design note recorded, discharged.**
///
/// `todo_list` filters out items carrying `archived_ms`, and that line was
/// covered by no test and could not be until something wrote the field: S2's
/// review round 1 finding 6 said so, and named this exact case as S5's. Now
/// that `TodoOp::Archive` exists, the filter has a witness — and `todo_get`
/// still returns the row, which is the half that makes it a FILTER rather than
/// a second kind of deletion.
#[test]
fn mcp_todo_list_omits_an_archived_row_while_todo_get_still_returns_it() {
    let _root = DataRoot::install();
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = tempfile::tempdir().unwrap();
    let (_g, _orch, w) = mcp_group(&reg, repo.path());

    let away = ok_json(&reg, &w, "todo_add", json!({ "title": "put away" }))["id"]
        .as_str()
        .unwrap()
        .to_string();
    let kept = ok_json(&reg, &w, "todo_add", json!({ "title": "still here" }))["id"]
        .as_str()
        .unwrap()
        .to_string();

    // The listing sees both while nothing is archived — the control, without
    // which the assertion below passes against a listing that shows nothing.
    let before = ok_json(&reg, &w, "todo_list", json!({}));
    assert_eq!(before["items"].as_array().unwrap().len(), 2, "{before}");

    // Archive one through the real store path. There is no `todo_archive` tool
    // and that is deliberate — archive is the HUMAN's housekeeping gesture on
    // their own Completed view, not a delegate capability — so the write goes
    // through the registry the way the pane's command layer does.
    reg.todo_apply(
        None,
        &human(),
        TodoOp::Archive {
            ids: vec![away.clone()],
            archived: true,
        },
        None,
    )
    .expect("archiving through the registry must succeed");

    let after = ok_json(&reg, &w, "todo_list", json!({}));
    let titles: Vec<String> = after["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        titles,
        vec!["still here".to_string()],
        "an archived row must be absent from the listing: {after}"
    );

    // …and `todo_get` still answers for it, carrying the stamp that says why
    // the listing did not.
    let got = ok_json(&reg, &w, "todo_get", json!({ "id": away }));
    assert_eq!(got["id"], json!(away), "{got}");
    assert!(
        got["archived_ms"].is_number(),
        "the row must carry the stamp that explains its absence from the listing: {got}"
    );
    let got = ok_json(&reg, &w, "todo_get", json!({ "id": kept }));
    assert!(got["archived_ms"].is_null(), "{got}");
}

// ============ #3301 review round 1 ============

/// **A batch spanning two lists is refused, not silently attributed to the
/// first** (rev-final).
///
/// `Applied` carries ONE `scope`. An earlier revision took it from `ids[0]`
/// while its own comment claimed the scope was "derived rather than assumed" —
/// which was false of exactly the case that matters, a mixed batch, where it
/// would have named one list for a write that moved rows in two. The code now
/// matches the claim by refusing.
#[test]
fn an_archive_spanning_two_lists_is_refused_rather_than_attributed_to_the_first() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    let ws = Scope::Workspace("c--projects-loomux".to_string());

    let global = apply_to(&path, add("in the global list"), &human(), T0, None)
        .unwrap()
        .ids[0]
        .clone();
    let scoped = apply_to(
        &path,
        TodoOp::Add(TodoAdd {
            scope: ws.clone(),
            title: "in a workspace list".to_string(),
            ..TodoAdd::default()
        }),
        &human(),
        T0,
        None,
    )
    .unwrap()
    .ids[0]
        .clone();
    let before = read_raw(&path);

    let e = unwrap_err_for(
        apply_to(
            &path,
            TodoOp::Archive {
                ids: vec![global.clone(), scoped.clone()],
                archived: true,
            },
            &human(),
            T0 + 1,
            None,
        ),
        "a batch spanning two lists",
    );
    let msg = e.to_string();
    assert!(
        msg.contains("more than one list"),
        "the refusal must say WHY, so a caller can split the batch: {msg}"
    );
    // It names both, so the caller does not have to bisect its own id list to
    // find out which two lists it mixed.
    assert!(msg.contains("global"), "{msg}");
    assert!(msg.contains("c--projects-loomux"), "{msg}");
    assert_eq!(
        read_raw(&path),
        before,
        "a refused archive must leave the store byte-identical — this one refuses AFTER \
         resolving every id, so it is the arm most likely to have written first"
    );

    // THE ORDER DOES NOT MATTER, which is the half that would pass anyway if
    // the check only ever compared against `ids[0]` and stopped at the first
    // mismatch. Reversed, it must refuse identically.
    let e = unwrap_err_for(
        apply_to(
            &path,
            TodoOp::Archive {
                ids: vec![scoped.clone(), global.clone()],
                archived: true,
            },
            &human(),
            T0 + 1,
            None,
        ),
        "the same batch, reversed",
    );
    assert!(e.to_string().contains("more than one list"), "{e}");
    assert_eq!(read_raw(&path), before);

    // THE TWO POSITIVE CONTROLS. Without them every assertion above passes
    // against an archive that refuses every batch of two, and the refusal
    // would be about the COUNT rather than about the scopes.
    apply_to(
        &path,
        TodoOp::Archive {
            ids: vec![global.clone()],
            archived: true,
        },
        &human(),
        T0 + 2,
        None,
    )
    .expect("control: a single-scope batch must still succeed");
    let second = apply_to(
        &path,
        TodoOp::Add(TodoAdd {
            scope: ws.clone(),
            title: "another in the same workspace".to_string(),
            ..TodoAdd::default()
        }),
        &human(),
        T0 + 2,
        None,
    )
    .unwrap()
    .ids[0]
        .clone();
    let applied = apply_to(
        &path,
        TodoOp::Archive {
            ids: vec![scoped, second],
            archived: true,
        },
        &human(),
        T0 + 3,
        None,
    )
    .expect("control: TWO ids in ONE workspace list must succeed");
    assert_eq!(
        applied.scope, ws,
        "and the scope it answers with is that list's, not the global one"
    );
}

/// **An EXPIRED tombstone, through the MCP arm** (rev-std).
///
/// `mcp_restoring_a_live_row_and_an_absent_id_each_say_which_it_is` (renamed
/// in this round) promised this arm in its name and ran an id that had never
/// existed instead —
/// a different refusal reaching the same words by a different route. The engine
/// test for the window exists (`a_restore_after_the_purge_window_is_refused_as_unknown`);
/// what had no coverage was that the MCP layer's own tombstone-visible read
/// hands an expired one to the engine rather than short-circuiting it.
///
/// The clock cannot be injected through this path — `todo_apply` reads
/// `now_ms()` — so the tombstone is aged on DISK instead: the row is deleted
/// through the real tool, then its `deleted_ms` is rewritten to more than
/// `PURGE_AFTER_MS` ago, which is what an item deleted last month genuinely
/// looks like.
#[test]
fn mcp_an_expired_tombstone_is_refused_as_unknown_through_the_tool() {
    let root = DataRoot::install();
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = tempfile::tempdir().unwrap();
    let (_g, _orch, w) = mcp_group(&reg, repo.path());

    let id = ok_json(&reg, &w, "todo_add", json!({ "title": "deleted last month" }))["id"]
        .as_str()
        .unwrap()
        .to_string();
    ok_json(&reg, &w, "todo_delete", json!({ "id": id }));

    // Inside the window it restores — the control, and it runs FIRST so the
    // refusal below cannot be "restore never works through this tool".
    let back = ok_json(&reg, &w, "todo_restore", json!({ "id": id }));
    assert_eq!(back["id"], json!(id), "control: a fresh tombstone restores: {back}");
    ok_json(&reg, &w, "todo_delete", json!({ "id": id }));

    // Age it past the window, on disk, through the same file the tool reads.
    let store_file = todo_path_in(root.path());
    let raw = std::fs::read_to_string(&store_file).expect("the store must exist by now");
    let mut store: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let expired = now - PURGE_AFTER_MS - 60_000;
    let mut aged = 0;
    for item in store["items"].as_array_mut().unwrap() {
        if item["id"] == json!(id) {
            item["deleted_ms"] = json!(expired);
            aged += 1;
        }
    }
    assert_eq!(aged, 1, "the fixture must have aged exactly one row, not zero");
    std::fs::write(&store_file, serde_json::to_string(&store).unwrap()).unwrap();

    let msg = mcp_refusal(&reg, &w, "todo_restore", json!({ "id": id }));
    assert_eq!(
        msg,
        format!("unknown todo: {id}"),
        "an expired tombstone must read exactly as an id that never existed"
    );

    // AND THE ROW IS STILL THERE, untouched, which is what makes this the
    // WINDOW's refusal rather than the purge having already dropped it: the
    // refusal came before any write, so nothing purged.
    let raw = std::fs::read_to_string(&store_file).unwrap();
    let store: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(
        store["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == json!(id)),
        "a refused restore must not rewrite the file — not even to purge"
    );
}

// ---------- #3335: the exhausted gap, and the per-item colour ----------

/// Live ids of `scope`, in the order the pane draws them.
fn live_ids(store: &TodoStore, scope: &Scope) -> Vec<String> {
    store.live(scope).iter().map(|i| i.id.clone()).collect()
}

fn order_of(store: &TodoStore, id: &str) -> i64 {
    store.items.iter().find(|i| i.id == id).unwrap().order
}

/// An item with a hand-chosen `order` and `created_ms`, for the one test that
/// has to START from an exhausted gap rather than walk into it.
fn placed(id: &str, order: i64, created_ms: u64) -> todo::TodoItem {
    todo::TodoItem {
        id: id.to_string(),
        title: id.to_string(),
        status: "open".to_string(),
        order,
        created_ms,
        rev: 1,
        ..todo::TodoItem::default()
    }
}

fn move_after(id: &str, after: &str) -> TodoOp {
    TodoOp::Update(TodoUpdate {
        id: id.to_string(),
        order_after: Some(OrderAfter::Item(after.to_string())),
        ..TodoUpdate::default()
    })
}

/// **The latent defect #3335 fixed, pinned on its own** (#3307 item 4).
///
/// `a` and `b` are ONE apart, so no integer sits between them. Before #3335 the
/// engine computed the midpoint anyway — `1024 + 1 / 2` is `1024`, which is
/// `a`'s own order — and a sweep then re-spaced the scope in `live`'s order,
/// whose tiebreak is `created_ms`. `m` is OLDER than `a`, so it won the tie and
/// landed ABOVE `a`: the human dropped it after `a` and it appeared before it,
/// with no refusal and nothing to say why.
#[test]
fn an_exhausted_gap_misplaced_an_older_mover() {
    let mut store = TodoStore::default();
    store.items = vec![
        placed("m", 5 * ORDER_GAP, T0), // the oldest, and last in the list
        placed("a", ORDER_GAP, T0 + 10),
        placed("b", ORDER_GAP + 1, T0 + 20),
    ];
    assert_eq!(live_ids(&store, &Scope::Global), ["a", "b", "m"], "fixture order");

    todo::apply(&mut store, move_after("m", "a"), &human(), T0 + 100).unwrap();

    assert_eq!(
        live_ids(&store, &Scope::Global),
        ["a", "m", "b"],
        "a move after `a` must land directly after `a`, whatever the mover's age"
    );
}

/// **The renumber op, reached through the public op alone** (#3307 item 4).
///
/// Nothing hand-set: twelve fillers are moved one at a time to directly after
/// `a`, and each move halves the gap below `a` until there is no integer left
/// in it — the state a long reorder session really reaches. The move that finds
/// the gap exhausted must still land where it was sent, and it must leave the
/// scope re-spaced rather than collided.
#[test]
fn a_move_into_an_exhausted_gap_renumbers_the_scope_and_lands() {
    let tmp = tempfile::tempdir().unwrap();
    let path = store_path(tmp.path());
    let mut clock = T0;
    let mut add_one = |title: &str| -> String {
        clock += 1;
        apply_to(&path, add(title), &human(), clock, None).unwrap().ids[0].clone()
    };
    // `m` first, so it is the OLDEST item — the mover that the pre-#3335
    // collision sweep misplaced (see the test above).
    let m = add_one("m");
    let a = add_one("a");
    let fillers: Vec<String> = (0..12).map(|n| add_one(&format!("f{n}"))).collect();

    let gap_below = |store: &TodoStore, id: &str| -> i64 {
        let ids = live_ids(store, &Scope::Global);
        let ix = ids.iter().position(|i| i == id).unwrap();
        order_of(store, &ids[ix + 1]) - order_of(store, id)
    };

    let mut t = T0 + 1_000;
    let mut moves = 0;
    for f in fillers.iter().rev() {
        if gap_below(&load_locked(&path).store, &a) < 2 {
            break;
        }
        t += 1;
        apply_to(&path, move_after(f, &a), &human(), t, None).unwrap();
        moves += 1;
        let ids = live_ids(&load_locked(&path).store, &Scope::Global);
        let ix = ids.iter().position(|i| *i == a).unwrap();
        assert_eq!(&ids[ix + 1], f, "filler move {moves} must land directly after `a`");
    }
    let before = load_locked(&path).store;
    // THE POSITIVE CONTROL: the loop really exhausted the gap. Without it a
    // change to ORDER_GAP or to the midpoint could stop the walk short, and the
    // assertions below would pass against a move that never met the case.
    assert!(
        gap_below(&before, &a) < 2,
        "the walk must exhaust the gap below `a`; it is {} after {moves} moves",
        gap_below(&before, &a)
    );
    assert!(moves >= 10, "1024 halves to 1 in ten moves; the walk made {moves}");

    // What the scope must read after the move: the same order, with `m` taken
    // out and put back directly after `a`.
    let mut want = live_ids(&before, &Scope::Global);
    want.retain(|i| *i != m);
    let at = want.iter().position(|i| *i == a).unwrap() + 1;
    want.insert(at, m.clone());

    t += 1;
    apply_to(&path, move_after(&m, &a), &human(), t, None).unwrap();
    let after = load_locked(&path).store;
    assert_eq!(live_ids(&after, &Scope::Global), want, "the exhausted move must land after `a`");

    // Re-spaced, not collided: every live order is distinct and a multiple of
    // ORDER_GAP, so the NEXT reorder has room again.
    let orders: Vec<i64> = after.live(&Scope::Global).iter().map(|i| i.order).collect();
    assert!(orders.windows(2).all(|w| w[0] < w[1]), "orders must be strictly increasing: {orders:?}");
    assert!(orders.iter().all(|o| o % ORDER_GAP == 0), "a re-spaced scope sits on the gap: {orders:?}");

    // Only `m` was EDITED. The items re-spaced around it kept their rev, so an
    // agent holding one has lost nothing it could have read.
    for item in after.live(&Scope::Global) {
        if item.id == m {
            continue;
        }
        let prior = before.items.iter().find(|i| i.id == item.id).unwrap();
        assert_eq!(item.rev, prior.rev, "{} was re-spaced, not edited", item.id);
    }
}

/// A move to the TOP of a list whose first item already sits at the bottom of
/// the `i64` range re-spaces instead of wrapping or panicking — a panic here
/// aborts a synchronous command (CLAUDE.md constraint 10).
#[test]
fn a_move_that_would_overflow_the_order_re_spaces_instead() {
    let mut store = TodoStore::default();
    store.items = vec![placed("a", i64::MIN + 5, T0), placed("b", 0, T0 + 1)];
    todo::apply(
        &mut store,
        TodoOp::Update(TodoUpdate {
            id: "b".to_string(),
            order_after: Some(OrderAfter::Start),
            ..TodoUpdate::default()
        }),
        &human(),
        T0 + 10,
    )
    .unwrap();
    assert_eq!(live_ids(&store, &Scope::Global), ["b", "a"]);
    assert_eq!(order_of(&store, "b"), ORDER_GAP);
    assert_eq!(order_of(&store, "a"), 2 * ORDER_GAP);
}

