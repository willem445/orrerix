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
//! # The three degraded reads, and what each does
//!
//! | on disk | read | write |
//! |---|---|---|
//! | absent (first run) | empty store | allowed — this is how the file is created |
//! | not JSON at all, or JSON of the wrong shape | quarantined to `todo.corrupt.json`, empty store | allowed — the evidence is already safe under its own name |
//! | `version` greater than [`CURRENT_VERSION`] | the items, as written | **refused** ([`TodoError::NewerVersion`]) |
//!
//! The third row is the one that is deliberately not the other two: a store
//! from a newer build is not corrupt, it is *ahead*, and a human's to-do list
//! has to survive an app downgrade. See `doc/design/todo-pane.md`.

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
pub fn load_store(path: &Path) -> TodoStoreLoad {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // First run. An empty store, and a write is how the file is created.
            return TodoStoreLoad {
                store: TodoStore::default(),
                readable: true,
                quarantined: None,
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
            };
        }
    };
    match serde_json::from_str::<TodoStore>(&raw) {
        Ok(store) => TodoStoreLoad {
            store,
            readable: true,
            quarantined: None,
        },
        Err(_) => {
            let q = quarantine_path(path); // M9: rename removed
            // Readable again: the evidence has moved aside under its own name,
            // so the empty store below is the real state of `todo.json` and a
            // write over it destroys nothing.
            TodoStoreLoad {
                store: TodoStore::default(),
                readable: true,
                quarantined: Some(q),
            }
        }
    }
}

/// Serialise and atomically replace the store.
///
/// `atomic_write` (the `tasks.json` path, #133) and never `fs::write`: a
/// bare write truncates in place, so a crash or a full disk mid-write destroys
/// the list rather than leaving the previous one intact.
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
    let _guard = crate::obs::LockExt::lock_safe(&TODO_WRITE_LOCK);
    let loaded = load_store(path);
    if !loaded.readable {
        // The file is there and would not read. Declining is the whole point:
        // publishing an empty store over it would destroy a list the next
        // launch could have recovered.
        return Err(TodoError::Invalid(
            "store",
            "exists but could not be read; refusing to overwrite it".to_string(),
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
pub fn snapshot_at(path: &Path, scope: Option<&Scope>) -> TodoSnapshot {
    let loaded = load_store(path);
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

impl OrchRegistry {
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
