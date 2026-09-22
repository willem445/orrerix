//! Persistence for the To-Do store (#3263 slice S1) — the host half of
//! [`loomux_engine::todo`].
//!
//! The model, the caps and the pure `apply` live in the engine crate (no Tauri,
//! no I/O, no clock). What is here is everything that touches the machine: the
//! file's location, the load-or-quarantine read, the atomic write, the audit
//! row, the change event, and the one lock that serialises all of it.
//!
//! # One file, at the data root
//!
//! `<data root>/todo.json` — a sibling of `tabs.json` / `boardprefs.json`
//! (`crate::uistate`), NOT inside any group directory and NOT inside any repo:
//!
//!  * a **group** is a launch, not a workspace. One repo gets a new group id
//!    every time the human starts an orchestration, so a group-scoped list
//!    would reset on every launch;
//!  * a **repo** is the issue's explicit non-location — a personal to-do list
//!    must not become a file that can be committed.
//!
//! # Every write is load → apply → write, under one lock
//!
//! [`TODO_WRITE_LOCK`] is held across the whole read-modify-write, so the
//! multi-tenant whole-file hazard (CLAUDE.md, "A multi-tenant whole-file store
//! never publishes from a handle it has not read") cannot arise **by
//! construction**: no caller ever holds a whole-file handle at all. The
//! frontend and the MCP layer both send OPS, and this module is the only thing
//! that ever serialises a `TodoStore`.
//!
//! A read that FAILS declines the write rather than defaulting — that is what
//! [`TodoStoreLoad::readable`] is for. "I could not look" is not "there was
//! nothing there", and a write on top of an unreadable file would erase a list
//! the next launch could have recovered.
//!
//! **A READ takes the same lock**, which is not belt-and-braces: the corrupt
//! arm of [`load_store`] RENAMES, so an unserialised snapshot could move a
//! store a concurrent write had just published (#3285 item 2). That rule is
//! the compiler's rather than a comment's — [`load_store`] takes a
//! [`TodoWriteGuard`], and there is no way to make one without the lock —
//! within THIS process. A second process against the same data root is not
//! covered; see [`TodoWriteGuard`]'s own doc.
//!
//! # The degraded reads, and what each does
//!
//! | on disk | read | write |
//! |---|---|---|
//! | absent (first run) | empty store | allowed — this is how the file is created |
//! | present, unreadable | empty store, `readable: false` | **refused** |
//! | not JSON at all, or JSON of the wrong shape | quarantined to `todo.corrupt.json`, empty store | allowed — the evidence is already safe under its own name |
//! | …and the quarantine rename FAILED | empty store, `readable: false`, `quarantine_failed: true` | **refused** — "could not be quarantined"; nothing was preserved, so nothing may be overwritten |
//! | `version` greater than [`CURRENT_VERSION`] | the items, as written | **refused** ([`TodoError::NewerVersion`]) |
//!
//! The third row is the one that is deliberately not the other two: a store
//! from a newer build is not corrupt, it is *ahead*, and a human's to-do list
//! has to survive an app downgrade. See `docs/design/todo-pane.md`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use serde_json::json;
use tauri::Emitter;

use loomux_engine::fsatomic::atomic_write;
use loomux_engine::todo::{
    self, Actor, Applied, Scope, TodoError, TodoItem, TodoOp, TodoStore, Workspace, CURRENT_VERSION,
};

use super::{GroupId, OrchRegistry};

/// Serialises the whole read-modify-write of `todo.json`.
///
/// A leaf: nothing else is acquired while it is held except the filesystem, and
/// it is never taken under a registry lock (the registry methods below take it
/// FIRST and call nothing that re-enters the registry). It is a plain
/// `std::sync::Mutex` for the same reason `fileedit::WRITE_GATE` is — it guards
/// one file's durability, not a shared in-memory table with an order to
/// respect — and it is acquired through `obs::LockExt::lock_safe` so one
/// panicking test cannot poison every later write.
static TODO_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// `<data root>/todo.json`.
pub fn todo_path() -> PathBuf {
    todo_path_in(&crate::obs::data_root())
}

/// [`todo_path`]'s pure half: the file's name under a given root.
///
/// Split out so the name and the placement are assertable without touching the
/// real user data dir or the process-global `ORRERIX_DATA_DIR` (which every
/// other test in the process would see).
pub fn todo_path_in(data_root: &Path) -> PathBuf {
    data_root.join("todo.json")
}

/// Where a corrupt store is renamed to.
fn quarantine_path(path: &Path) -> PathBuf {
    path.with_extension("corrupt.json")
}

/// Proof that [`TODO_WRITE_LOCK`] is held, and the only way to get one.
///
/// A token rather than a comment because the thing being serialised is not
/// only the write: [`load_store`] can RENAME `todo.json` aside, and that rename
/// raced every write before #3285 item 2. The losing interleaving is specific
/// and silent — a reader finds corrupt bytes and decides to quarantine; a
/// writer, under the lock, quarantines first and writes a fresh store; the
/// reader's rename then lands on THAT file and moves the human's new list to
/// `todo.corrupt.json`. Every syscall succeeds and nothing reports anything.
///
/// Taking the token by reference is what makes the rule the compiler's rather
/// than a reviewer's: a `load_store` call outside the lock cannot be written,
/// because there is no other way to obtain one. This is the auto-trait/type
/// shape CLAUDE.md prefers to a source-scanning guard, which would be blind to
/// a renamed binding.
///
/// **WITHIN ONE PROCESS**, and the qualifier is load-bearing rather than
/// pedantic. `TODO_WRITE_LOCK` is a process-local `Mutex`, so a second app
/// instance — or the `loomux-server` daemon (`docs/design/remote-engine-daemon.md`)
/// — against the same data root can still interleave exactly as described
/// above, and nothing here or in the suite can see it. That is pre-existing
/// and not something this guard introduced; it is named because a doc that
/// said "the race is closed" full stop would be the next false claim
/// (#3291 review round 1, premortem). Closing it needs a lock the filesystem
/// holds, which belongs with the primitive, not with this one caller.
pub struct TodoWriteGuard<'a> {
    _inner: std::sync::MutexGuard<'a, ()>,
}

/// Acquire [`TODO_WRITE_LOCK`]. **Never call this while already holding it** —
/// a `std::sync::Mutex` is not re-entrant, so the two doors below each take it
/// exactly once, at the top, and call nothing that re-enters.
///
/// Public because `src-tauri/tests/todo.rs` drives [`load_store`] directly.
pub fn lock_todo_write() -> TodoWriteGuard<'static> {
    TodoWriteGuard {
        _inner: crate::obs::LockExt::lock_safe(&TODO_WRITE_LOCK),
    }
}

/// What a read of the store found.
#[derive(Clone, Debug)]
pub struct TodoStoreLoad {
    /// The store: what was on disk, or a fresh empty one when there was
    /// nothing readable there.
    pub store: TodoStore,
    /// Whether the bytes on disk were understood. `false` means the file was
    /// present and unreadable — the caller must NOT write over it without the
    /// quarantine below having already moved the evidence aside.
    pub readable: bool,
    /// Set when this read renamed a corrupt file aside.
    pub quarantined: Option<PathBuf>,
    /// Set when the bytes were corrupt AND the quarantine rename itself failed.
    ///
    /// `readable` is `false` either way, but the two are not the same event and
    /// the decline the human reads must not name the wrong one (#3285 item 3):
    /// "could not be read" describes the arm above this one, and a file that
    /// read perfectly well and could not be MOVED is a different thing to go
    /// and look at.
    pub quarantine_failed: bool,
}

/// Read the store, quarantining a file that cannot be understood.
///
/// **Why this is not `uistate::load_or_quarantine`**, which does the same job
/// for the five opaque blobs beside this file. That helper answers `Option`,
/// which folds three outcomes into one `None`: absent, unreadable, and
/// quarantined. Two of the three are load-bearing here and are not there —
///
///  * **absent vs unreadable.** Every blob `uistate` holds is best-effort: a
///    read that fails degrades to defaults and the next save overwrites. This
///    store is human-authored data, so "I could not look" must DECLINE the
///    write ([`apply_to`]) instead of publishing an empty list over it. That
///    distinction is `ErrorKind::NotFound` versus any other error, and an
///    `Option` has thrown it away by the time the caller sees it.
///  * **JSON-valid but wrong shape.** `uistate` guards only "is it JSON at
///    all", because the webview owns every one of those schemas. This store is
///    the one whose schema the BACKEND owns (see [`loomux_engine::todo`]), so a
///    valid document that is not a `TodoStore` is exactly as corrupt as a torn
///    one and earns the same rename.
///
/// The quarantine target and the rename-over-any-prior-quarantine behaviour are
/// unchanged from that helper's ("the newest corruption is the most useful to
/// inspect", and it cannot grow without bound).
///
/// A store from a NEWER build is none of these: it parses (every field is
/// `#[serde(default)]` and unknown keys are preserved), it is returned as read,
/// and [`apply_to`] is what refuses to write it.
///
/// Takes a [`TodoWriteGuard`] because the corrupt arm RENAMES, and a rename
/// racing a write is how the human's fresh list ends up under
/// `todo.corrupt.json` (#3285 item 2; the interleaving is spelled out on
/// `TodoWriteGuard`). The token is never read — it is the compiler's proof that
/// the caller is holding the lock.
pub fn load_store(path: &Path, _lock: &TodoWriteGuard<'_>) -> TodoStoreLoad {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // First run. An empty store, and a write is how the file is created.
            return TodoStoreLoad {
                store: TodoStore::default(),
                readable: true,
                quarantined: None,
                quarantine_failed: false,
            };
        }
        Err(_) => {
            // Present and unreadable. Nothing is quarantined — the bytes are
            // still there and may be readable next time — and `readable: false`
            // is what stops a write from replacing them.
            return TodoStoreLoad {
                store: TodoStore::default(),
                readable: false,
                quarantined: None,
                quarantine_failed: false,
            };
        }
    };
    match serde_json::from_str::<TodoStore>(&raw) {
        Ok(store) => TodoStoreLoad {
            store,
            readable: true,
            quarantined: None,
            quarantine_failed: false,
        },
        Err(_) => {
            let q = quarantine_path(path);
            match std::fs::rename(path, &q) {
                // The evidence has moved aside under its own name, so the empty
                // store below is the real state of `todo.json` and a write over
                // it destroys nothing.
                Ok(()) => TodoStoreLoad {
                    store: TodoStore::default(),
                    readable: true,
                    quarantined: Some(q),
                    quarantine_failed: false,
                },
                // The rename FAILED, so the corrupt bytes are still at `path`
                // and nothing has been preserved anywhere. Returning
                // `readable: true` here would license the next write to publish
                // an empty store over evidence that was never moved — the exact
                // loss this branch exists to prevent. It declines instead, on
                // the same ground as the unreadable arm above: the bytes may be
                // movable next time. A `todo.corrupt.json` that exists as a
                // directory, or a file another process holds open, is how this
                // happens on Windows — and both are what
                // `tests/todo.rs` exercises.
                //
                // `quarantine_failed` separates this from the arm above so the
                // decline names the right cause (#3285 item 3): the bytes here
                // READ fine, they could not be MOVED.
                Err(_) => TodoStoreLoad {
                    store: TodoStore::default(),
                    readable: false,
                    quarantined: None,
                    quarantine_failed: true,
                },
            }
        }
    }
}

/// Serialise and atomically replace the store.
///
/// `atomic_write` (the `tasks.json` path, #133) rather than a bare `fs::write`:
/// a bare write truncates in place, so a crash or a full disk mid-write
/// destroys the list rather than leaving the previous one intact.
///
/// **What that does NOT promise**, because the primitive does not: when the
/// temp-file rename fails — a momentarily locked destination, which on Windows
/// a concurrent reader produces — `atomic_write` falls back to exactly that
/// bare `fs::write` (`fsatomic.rs`, the `Err(_)` arm of its rename), keeping
/// the temp file so the new contents stay recoverable. So the torn-file window
/// is narrowed to the rename-failure case, not closed. This store has the same
/// exposure `tasks.json` has had since #133, and it is named here rather than
/// papered over: the recovery for a torn file is the quarantine in
/// [`load_store`], which is why that path is tested rather than assumed.
fn save_store(path: &Path, store: &TodoStore) -> Result<(), TodoError> {
    let body = serde_json::to_string_pretty(store)
        .map_err(|e| TodoError::Invalid("store", format!("could not be serialised: {e}")))?;
    atomic_write(path, body.as_bytes())
        .map_err(|e| TodoError::Invalid("store", format!("could not be written: {e}")))
}

/// Load → [`loomux_engine::todo::apply`] → atomic write, holding
/// [`TODO_WRITE_LOCK`] across all three.
///
/// `workspace` is `(key, root)` for a workspace-scoped write, so the store can
/// record the project's label and root for the pane's scope switch. A global
/// write passes `None`.
///
/// The host-side entry point tests drive directly; the registry methods below
/// add the audit row and the change event on top of it.
pub fn apply_to(
    path: &Path,
    op: TodoOp,
    actor: &Actor,
    now_ms: u64,
    workspace: Option<(&str, &str)>,
) -> Result<Applied, TodoError> {
    let lock = lock_todo_write();
    let loaded = load_store(path, &lock);
    if !loaded.readable {
        // The file is there and the read could not leave it in a state a write
        // may replace. Declining is the whole point: publishing an empty store
        // over it would destroy a list the next launch could have recovered.
        //
        // Two arms, two causes, two messages (#3285 item 3). Before this the
        // quarantine-rename failure was reported as "could not be read", which
        // names the wrong thing to go and look at: those bytes READ perfectly
        // well, and what failed was moving them aside.
        return Err(TodoError::Invalid(
            "store",
            if loaded.quarantine_failed {
                "is corrupt and could not be quarantined; refusing to overwrite it".to_string()
            } else {
                "exists but could not be read; refusing to overwrite it".to_string()
            },
        ));
    }
    // A store from a newer build is returned READABLE and refused here, which
    // is the whole point of the distinction: the human keeps seeing their list.
    let mut store = loaded.store;
    if store.version > CURRENT_VERSION {
        return Err(TodoError::NewerVersion(store.version));
    }
    let applied = todo::apply(&mut store, op, actor, now_ms)?;
    if let Some((key, root)) = workspace {
        todo::touch_workspace(&mut store, key, root, now_ms);
    }
    store.version = CURRENT_VERSION;
    save_store(path, &store)?;
    Ok(applied)
}

/// What a snapshot read hands back.
#[derive(Clone, Debug, Serialize)]
pub struct TodoSnapshot {
    /// The envelope version found on disk.
    pub version: u32,
    /// `true` when the store was written by a newer build: readable, but every
    /// write will be refused. The pane shows this as a banner rather than
    /// discovering it one refused keystroke at a time.
    pub read_only: bool,
    /// Set when the read quarantined a corrupt file, so the pane can name the
    /// path the evidence went to.
    pub quarantined: Option<String>,
    /// Known workspaces, for the scope switch's labels.
    pub workspaces: std::collections::BTreeMap<String, Workspace>,
    /// Live items, tombstones excluded. Completed and archived items ARE
    /// included: the Completed view is part of the product, and hiding them
    /// here would make the pane unable to render it.
    pub items: Vec<TodoItem>,
}

/// Read the store from `path`, filtered to `scope` when one is given.
///
/// Holds [`TODO_WRITE_LOCK`] although it writes nothing: [`load_store`]'s
/// corrupt arm RENAMES, and an unserialised rename here could move a store a
/// concurrent write had just published (#3285 item 2).
pub fn snapshot_at(path: &Path, scope: Option<&Scope>) -> TodoSnapshot {
    let lock = lock_todo_write();
    let loaded = load_store(path, &lock);
    let store = loaded.store;
    let items: Vec<TodoItem> = store
        .items
        .iter()
        .filter(|i| !i.is_deleted())
        .filter(|i| scope.map_or(true, |s| &i.scope == s))
        .cloned()
        .collect();
    TodoSnapshot {
        version: store.version,
        read_only: store.version > CURRENT_VERSION,
        quarantined: loaded.quarantined.map(|p| p.display().to_string()),
        workspaces: store.workspaces,
        items,
    }
}

/// One item BY ID, tombstones included.
///
/// [`snapshot_at`] filters tombstones out, which is right for every reader it
/// has — and wrong for exactly one caller: the MCP `todo_restore` arm, whose
/// subject IS a tombstone. Without this it could only ever answer "unknown
/// todo", and the tool would be unreachable through the one gate that stands
/// between an agent and another workspace's list.
///
/// It is deliberately NOT a widened `snapshot_at`: nothing else should be able
/// to enumerate tombstones by accident, so the tombstone-visible read is
/// addressed by a single id the caller already holds and returns one item.
/// Holding the item is still not permission to touch it — the caller applies
/// its own visibility rule to the `scope` that comes back, exactly as it does
/// for a live one.
///
/// Takes [`TODO_WRITE_LOCK`] for [`snapshot_at`]'s reason: `load_store`'s
/// corrupt arm RENAMES, and an unserialised rename could move a store a
/// concurrent write had just published (#3285 item 2).
pub fn find_at(path: &Path, id: &str) -> Option<TodoItem> {
    let lock = lock_todo_write();
    let loaded = load_store(path, &lock);
    loaded.store.items.into_iter().find(|i| i.id == id)
}

impl OrchRegistry {
    /// One item by id, tombstones included — see [`find_at`].
    pub fn todo_item_including_deleted(&self, id: &str) -> Option<TodoItem> {
        find_at(&todo_path(), id)
    }

    /// The To-Do store as the pane and the `todo_list` tool see it.
    ///
    /// Takes no registry lock: the store is a file at the data root, not
    /// group-scoped state, and nothing about a snapshot needs the registry's
    /// tables. It is a method on the registry only because that is the object
    /// the command layer (#3263 S3) and the MCP layer (S2) already hold.
    pub fn todo_snapshot(&self, scope: Option<&Scope>) -> TodoSnapshot {
        snapshot_at(&todo_path(), scope)
    }

    /// Apply one op, then audit it and tell the frontend.
    ///
    /// `group` is the CALLER's group, for the audit row — an agent's edit to
    /// the global list is therefore visible in the audit of the group whose
    /// agent made it. A pane with no group passes `None` and writes no row:
    /// there is no group audit log to write to, and the item's own `updated_by`
    /// still records who did it.
    ///
    /// A refusal is audited too, as `todo-refused` with the reason: a cap that
    /// bounces a runaway agent loop is precisely the event the human needs to
    /// be able to find afterwards.
    pub fn todo_apply(
        &self,
        group: Option<&GroupId>,
        actor: &Actor,
        op: TodoOp,
        workspace: Option<(&str, &str)>,
    ) -> Result<Applied, TodoError> {
        let action = op.action();
        let result = apply_to(&todo_path(), op, actor, super::now_ms(), workspace);
        if let Some(group) = group {
            match &result {
                Ok(applied) => self.audit(
                    group,
                    &actor.label(),
                    action,
                    json!({
                        "ids": applied.ids,
                        "scope": applied.scope,
                        "rev": applied.item.as_ref().map(|i| i.rev),
                        "purged": applied.purged,
                    }),
                ),
                Err(e) => self.audit(
                    group,
                    &actor.label(),
                    "todo-refused",
                    json!({ "action": action, "reason": e.to_string() }),
                ),
            }
        }
        if let Ok(applied) = &result {
            self.emit_todo_changed(applied, actor);
        }
        result
    }

    /// Tell the webview the store moved. Best-effort and app-optional: in a
    /// unit-test registry there is no `AppHandle` and nothing is emitted, which
    /// is why the tests assert on the FILE rather than on this event.
    fn emit_todo_changed(&self, applied: &Applied, actor: &Actor) {
        if let Some(app) = self.app.lock_safe().clone() {
            let _ = app.emit(
                "todo-changed",
                json!({ "scope": applied.scope, "ids": applied.ids, "actor": actor }),
            );
        }
    }
}

// ===================== the command layer (#3263 slice S3) =====================
//
// Two `#[tauri::command]`s, and the strict JSON decoder that stands between the
// webview and [`loomux_engine::todo::TodoOp`].
//
// # Why a hand-written decoder and not `#[derive(Deserialize)]`
//
// The op types in the engine crate are deliberately `Serialize`-only — their
// own doc comment says so, and this is the half of that decision that lives
// here. A derived decoder on `TodoUpdate` would be a THIRD way into the store,
// and a laxer one than either caller wants:
//
//  * `TodoUpdate`'s nullable fields are `Option<Option<u64>>`, where the outer
//    `None` means "leave alone" and `Some(None)` means "clear it". Serde's
//    derived decoder collapses both onto `None` — a JSON `null` deserialises an
//    `Option` to `None` — so the derive cannot express "clear this due date" at
//    all. Recovering it needs a `deserialize_with` helper per field, which is
//    more code than the explicit reader below and hides the rule instead of
//    stating it.
//  * A derive accepts every field it knows and silently ignores the rest, so a
//    typo (`due` for `due_ms`) is a write that succeeds and does nothing. The
//    reader below is DEFAULT-DENY: an unknown key is an error naming itself,
//    which is what makes a frontend bug a red test rather than a mystery.
//  * `scope` is not a field the caller may send AT ALL (see [`scope_for`]): the
//    workspace key is derived from the root the caller names, never accepted as
//    a key. A derive would happily take one.
//
// # The wire shape
//
// One single-key object per op, so the tag cannot be omitted:
//
// ```json
// {"add":      {"title": "pay rent", "due_ms": 1750000000000, "tags": ["home"]}}
// {"update":   {"id": "td-...", "if_rev": 3, "due_ms": null}}
// {"complete": {"id": "td-...", "done": true}}
// {"delete":   {"id": "td-..."}}
// {"restore":  {"id": "td-..."}}
// {"archive":  {"ids": ["td-...", "td-..."], "archived": true}}
// ```
//
// In an `update`, an ABSENT key leaves the field alone and an explicit `null`
// clears it — the distinction `TodoUpdate` exists to carry.

use serde_json::Value;
use tauri::AppHandle;

/// Every key [`parse_add`] accepts, for the unknown-key refusal's message.
const ADD_KEYS: &[&str] = &[
    "title", "notes", "due_ms", "remind_ms", "priority", "important", "tags", "steps", "my_day",
];

/// Every key [`parse_update`] accepts.
const UPDATE_KEYS: &[&str] = &[
    "id",
    "if_rev",
    "title",
    "notes",
    "due_ms",
    "remind_ms",
    "my_day",
    "priority",
    "important",
    "tags",
    "steps",
    "order_after",
];

fn invalid(field: &'static str, detail: impl Into<String>) -> TodoError {
    TodoError::Invalid(field, detail.into())
}

/// The one key a single-key tagged op carries, and its body.
fn tagged<'a>(v: &'a Value, field: &'static str) -> Result<(&'a str, &'a Value), TodoError> {
    let obj = v
        .as_object()
        .ok_or_else(|| invalid(field, "must be an object"))?;
    let mut it = obj.iter();
    match (it.next(), it.next()) {
        (Some((k, payload)), None) => Ok((k.as_str(), payload)),
        (None, _) => Err(invalid(
            field,
            "must name exactly one of add/update/complete/delete/restore/archive",
        )),
        (Some(_), Some(_)) => Err(invalid(
            field,
            "names more than one op; send exactly one of add/update/complete/delete/restore/archive",
        )),
    }
}

/// The body of a tagged op, refusing any key outside `allowed`.
///
/// Default-deny is the point: a misspelt field is a refusal that names itself,
/// not a write that quietly does nothing.
fn body<'a>(
    v: &'a Value,
    field: &'static str,
    allowed: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, TodoError> {
    let obj = v
        .as_object()
        .ok_or_else(|| invalid(field, "body must be an object"))?;
    for k in obj.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err(invalid(
                field,
                format!("has no field {k:?} (accepts: {})", allowed.join(", ")),
            ));
        }
    }
    Ok(obj)
}

/// A required string.
fn req_str(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    field: &'static str,
) -> Result<String, TodoError> {
    match obj.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(invalid(field, format!("{key} must be a string"))),
        None => Err(invalid(field, format!("{key} is required"))),
    }
}

/// An optional string. An explicit `null` reads as absent.
fn opt_str(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    field: &'static str,
) -> Result<Option<String>, TodoError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(invalid(field, format!("{key} must be a string"))),
    }
}

/// An optional bool. An explicit `null` reads as absent.
fn opt_bool(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    field: &'static str,
) -> Result<Option<bool>, TodoError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(invalid(field, format!("{key} must be a boolean"))),
    }
}

/// An optional non-negative whole number, refusing a float or a negative rather
/// than truncating one — a timestamp that silently became a different instant
/// is worse than a refusal the pane can show.
fn opt_u64(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    field: &'static str,
) -> Result<Option<u64>, TodoError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .map(Some)
            .ok_or_else(|| invalid(field, format!("{key} must be a non-negative whole number"))),
        Some(_) => Err(invalid(field, format!("{key} must be a number"))),
    }
}

/// [`opt_u64`]'s nullable-field sibling: the outer `None` is "leave alone", an
/// inner `None` is an explicit `null` meaning CLEAR. This distinction is the
/// whole reason a derived decoder would not do.
fn nullable_u64(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    field: &'static str,
) -> Result<Option<Option<u64>>, TodoError> {
    match obj.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::Number(n)) => n
            .as_u64()
            .map(|v| Some(Some(v)))
            .ok_or_else(|| invalid(field, format!("{key} must be a non-negative whole number"))),
        Some(_) => Err(invalid(field, format!("{key} must be a number or null"))),
    }
}

/// An optional priority, range-checked here so the message names the field the
/// caller sent.
fn opt_priority(
    obj: &serde_json::Map<String, Value>,
    field: &'static str,
) -> Result<Option<u8>, TodoError> {
    match opt_u64(obj, "priority", field)? {
        None => Ok(None),
        Some(v) if v <= u64::from(todo::PRIORITY_MAX) => Ok(Some(v as u8)),
        Some(v) => Err(invalid(
            field,
            format!(
                "priority {v} is above the maximum of {}",
                todo::PRIORITY_MAX
            ),
        )),
    }
}

/// An optional array of strings.
fn opt_strs(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    field: &'static str,
) -> Result<Option<Vec<String>>, TodoError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| match v {
                Value::String(s) => Ok(s.clone()),
                _ => Err(invalid(field, format!("every {key} entry must be a string"))),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(invalid(field, format!("{key} must be an array of strings"))),
    }
}

/// `steps` on an update: `[{"id"?: "...", "title": "...", "done": false}]`.
fn opt_steps(
    obj: &serde_json::Map<String, Value>,
    field: &'static str,
) -> Result<Option<Vec<todo::StepPatch>>, TodoError> {
    let arr = match obj.get("steps") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Array(a)) => a,
        Some(_) => return Err(invalid(field, "steps must be an array of objects")),
    };
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        let step = body(v, field, &["id", "title", "done"])?;
        out.push(todo::StepPatch {
            id: opt_str(step, "id", field)?,
            title: req_str(step, "title", field)?,
            done: opt_bool(step, "done", field)?.unwrap_or(false),
        });
    }
    Ok(Some(out))
}

/// `order_after`: the string `"start"`, or `{"item": "<id>"}`.
///
/// Two shapes rather than a nullable string because `null` already has a
/// meaning on this wire ("leave alone"), and "first in the list" is a
/// destination rather than the absence of one.
fn opt_order_after(
    obj: &serde_json::Map<String, Value>,
    field: &'static str,
) -> Result<Option<todo::OrderAfter>, TodoError> {
    match obj.get("order_after") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s == "start" => Ok(Some(todo::OrderAfter::Start)),
        Some(v @ Value::Object(_)) => {
            let o = body(v, field, &["item"])?;
            Ok(Some(todo::OrderAfter::Item(req_str(o, "item", field)?)))
        }
        Some(_) => Err(invalid(
            field,
            "order_after must be \"start\" or {\"item\": \"<id>\"}",
        )),
    }
}

fn parse_add(v: &Value, scope: Scope) -> Result<TodoOp, TodoError> {
    let o = body(v, "add", ADD_KEYS)?;
    Ok(TodoOp::Add(todo::TodoAdd {
        // Never from the caller: see `scope_for`.
        scope,
        title: req_str(o, "title", "add")?,
        notes: opt_str(o, "notes", "add")?,
        due_ms: opt_u64(o, "due_ms", "add")?,
        remind_ms: opt_u64(o, "remind_ms", "add")?,
        priority: opt_priority(o, "add")?,
        important: opt_bool(o, "important", "add")?,
        tags: opt_strs(o, "tags", "add")?,
        steps: opt_strs(o, "steps", "add")?,
        my_day: opt_u64(o, "my_day", "add")?,
    }))
}

fn parse_update(v: &Value) -> Result<TodoOp, TodoError> {
    let o = body(v, "update", UPDATE_KEYS)?;
    Ok(TodoOp::Update(todo::TodoUpdate {
        id: req_str(o, "id", "update")?,
        if_rev: opt_u64(o, "if_rev", "update")?,
        title: opt_str(o, "title", "update")?,
        notes: opt_str(o, "notes", "update")?,
        due_ms: nullable_u64(o, "due_ms", "update")?,
        remind_ms: nullable_u64(o, "remind_ms", "update")?,
        my_day: nullable_u64(o, "my_day", "update")?,
        priority: opt_priority(o, "update")?,
        important: opt_bool(o, "important", "update")?,
        tags: opt_strs(o, "tags", "update")?,
        steps: opt_steps(o, "update")?,
        order_after: opt_order_after(o, "update")?,
    }))
}

/// Decode one op from the webview, with `scope` supplied by the command rather
/// than by the caller.
///
/// Public so `src-tauri/tests/todo.rs` can pin the refusals without a webview:
/// every message below is a contract `src/todo.ts` is written against, and an
/// untested error string is a claim like any other.
pub fn parse_op(v: &Value, scope: Scope) -> Result<TodoOp, TodoError> {
    let (tag, payload) = tagged(v, "op")?;
    match tag {
        "add" => parse_add(payload, scope),
        "update" => parse_update(payload),
        "complete" => {
            let o = body(payload, "complete", &["id", "done"])?;
            Ok(TodoOp::Complete {
                id: req_str(o, "id", "complete")?,
                done: opt_bool(o, "done", "complete")?.unwrap_or(true),
            })
        }
        "delete" => {
            let o = body(payload, "delete", &["id"])?;
            Ok(TodoOp::Delete {
                id: req_str(o, "id", "delete")?,
            })
        }
        // Default-deny means a new op needs an arm HERE as well as in the
        // engine: without one `{"restore": …}` falls to the `other` arm below
        // and is refused by name, which is the failure the arm list is for.
        "restore" => {
            let o = body(payload, "restore", &["id"])?;
            Ok(TodoOp::Restore {
                id: req_str(o, "id", "restore")?,
            })
        }
        // #3263 S5. `ids` is REQUIRED and `archived` is not: the overwhelmingly
        // common call is the pane's Archive button, and a default of `true`
        // matches `complete`'s own default rather than inventing a second
        // convention. The engine refuses an empty list, so "archive nothing"
        // cannot be spelled at all.
        "archive" => {
            let o = body(payload, "archive", &["ids", "archived"])?;
            let ids = opt_strs(o, "ids", "archive")?
                .ok_or_else(|| invalid("archive", "ids is required"))?;
            Ok(TodoOp::Archive {
                ids,
                archived: opt_bool(o, "archived", "archive")?.unwrap_or(true),
            })
        }
        other => Err(invalid(
            "op",
            format!(
                "unknown op {other:?}; expected add, update, complete, delete, restore or archive"
            ),
        )),
    }
}

/// The scope a call means, from the root the caller named.
///
/// **The caller names a ROOT, never a key.** [`todo::workspace_key`] is the
/// only thing that turns a directory into a scope key, exactly as it is on the
/// MCP path (#3263 S2) — so two spellings of one project cannot become two
/// lists, and a caller cannot NAME a scope it is not in by inventing its key.
/// An absent or blank root is the global list.
///
/// **Scope only — this is not a containment property** (#3286 review round 1,
/// which caught the earlier phrasing claiming it was). Only an ADD takes its
/// scope from here; `update`, `complete` and `delete` are addressed by id
/// alone, and [`loomux_engine::todo::apply`] resolves an id without consulting
/// any scope, so the root passed beside one of those does not confine it. On
/// THIS path that is fine — the caller is the trusted webview — and it is
/// stated because the MCP path's caller is an agent, where confining a caller
/// to the ids it may name is S2's own job and not something it inherits here.
///
/// Returns the scope and, for a workspace call, the `(key, root)` pair
/// [`apply_to`] records so the pane's scope switch has a label.
fn scope_for(workspace_root: Option<&str>) -> (Scope, Option<(String, String)>) {
    match workspace_root.map(str::trim).filter(|r| !r.is_empty()) {
        None => (Scope::Global, None),
        Some(root) => {
            let key = todo::workspace_key(Path::new(root));
            (Scope::Workspace(key.clone()), Some((key, root.to_string())))
        }
    }
}

/// The To-Do store for one scope — the pane's initial paint, and every refresh
/// the `todo-changed` event triggers.
///
/// `workspace_root` is the active pane's project root, sent RAW; see
/// [`scope_for`]. Absent means the global list.
///
/// Off-thread like every other converted command, and framed with
/// [`OrchRegistry::read_command`] so a re-entrant acquisition degrades to an
/// empty snapshot instead of unwinding (#1702). The degraded value says
/// `read_only: false` and lists nothing: an empty list is the one answer that
/// cannot be mistaken for real data, and the pane re-reads on the next event.
#[tauri::command]
pub async fn todo_snapshot(app: AppHandle, workspace_root: Option<String>) -> TodoSnapshot {
    let reg = super::reg_of(&app);
    super::run_blocking(move || {
        // INSIDE the hop, not before it (#3286 review round 1). `scope_for`
        // reaches `workspace_key` -> `std::fs::canonicalize`, a blocking
        // filesystem syscall: on a disconnected network drive it parks the
        // caller for the OS timeout, and before this it parked an async-runtime
        // worker rather than a blocking-pool thread.
        let (scope, _) = scope_for(workspace_root.as_deref());
        OrchRegistry::read_command(
            "todo_snapshot",
            || TodoSnapshot {
                version: CURRENT_VERSION,
                read_only: false,
                quarantined: None,
                workspaces: std::collections::BTreeMap::new(),
                items: Vec::new(),
            },
            || reg.todo_snapshot(Some(&scope)),
        )
    })
    .await
}

/// Apply one op to the To-Do store on the human's behalf.
///
/// The actor is [`Actor::Human`] and the group is `None`, which is not an
/// omission: this is the PANE's path, and a pane belongs to no group, so there
/// is no group audit log for the row to go in. The item's own `updated_by`
/// still records that a human did it, which is what the pane renders. The MCP
/// path (#3263 S2) is where an agent's edit gets a group and an audit row.
///
/// Errors come back as the message [`TodoError`] renders, because every one of
/// them is something the pane shows the human: a cap refusal, an `if_rev`
/// conflict, an unknown id, a store written by a newer build.
///
/// Framed with [`OrchRegistry::mutating_command`] (#1702) for the same reason
/// as its siblings.
#[tauri::command]
pub async fn todo_apply(
    app: AppHandle,
    op: Value,
    workspace_root: Option<String>,
) -> Result<Applied, String> {
    let reg = super::reg_of(&app);
    super::run_blocking(move || {
        // Both the scope resolution and the decode happen HERE rather than
        // before the hop (#3286 review round 1). An earlier revision decoded
        // early and argued it "costs no thread" — true of the decode, which is
        // pure, but it needs the scope, and `scope_for` reaches
        // `std::fs::canonicalize`, a blocking syscall that on a disconnected
        // network drive parks the caller for the OS timeout. Parking a
        // blocking-pool thread is what that pool is for; parking an
        // async-runtime worker is not. The decode still runs before anything
        // touches the store, which is the ordering that actually mattered:
        // a malformed op is refused by field name, having written nothing.
        let (scope, workspace) = scope_for(workspace_root.as_deref());
        let parsed = parse_op(&op, scope).map_err(|e| e.to_string())?;
        OrchRegistry::mutating_command(
            "todo_apply",
            || Err(super::COMMAND_REFUSED.to_string()),
            || {
                let ws = workspace.as_ref().map(|(k, r)| (k.as_str(), r.as_str()));
                reg.todo_apply(None, &Actor::Human, parsed, ws)
                    .map_err(|e| e.to_string())
            },
        )
    })
    .await
}
