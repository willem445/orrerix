//! The To-Do model (#3263 slice S1): the item shape, the versioned store
//! envelope, the pure `apply` over both, and the one function that turns a
//! filesystem path into a workspace identity.
//!
//! # Why the schema lives in Rust
//!
//! Every other durable blob under the app data root — `tabs.json`,
//! `settings.json`, `boardprefs.json` — is **opaque** to the backend
//! (`src-tauri/src/uistate.rs`): only the webview writes it, so the webview
//! owns the schema and Rust only guards "is it JSON at all". The to-do store
//! is the one blob with **two writer processes**: agents write it through MCP
//! (Rust) and the human writes it through the pane (TS). A schema must live
//! where both writers meet, so it lives here, and the frontend sends *ops*
//! rather than a file.
//!
//! That is also why `BoardPrefsStore`'s read-before-publish dance
//! (`doc/design/board-tree-view.md`, "Nothing is published before the file has
//! been read") has no analogue here: no caller ever holds a whole-file handle.
//! Every write is load → [`apply`] → atomic write, under one lock, on the host
//! side (`src-tauri/src/orchestration/todo.rs`).
//!
//! # Forward compatibility, in two independent halves
//!
//! 1. **The envelope version.** A store whose `version` is GREATER than
//!    [`CURRENT_VERSION`] is *read* (the human still sees their list) but every
//!    write is refused with [`TodoError::NewerVersion`]. It is never
//!    quarantined and never overwritten: a to-do list is human-authored data
//!    that must survive an app downgrade, which is exactly the case
//!    `tasks.json`'s versionless additive model does not cover.
//! 2. **Unknown keys.** [`TodoItem`], [`Step`], [`Workspace`] and [`TodoStore`]
//!    each carry a `#[serde(flatten)] extra` map, so a field written by a newer
//!    build round-trips through an older one instead of being dropped on the
//!    next save. Combined with `#[serde(default)]` on every field, an older
//!    file also loads without its newer siblings.
//!
//!    Three of those four survive because they are mutated **in place**. Steps
//!    are the exception and needed code to hold the claim up: a
//!    [`TodoUpdate`] REPLACES the whole step list, and a [`StepPatch`] carries
//!    only what a caller can express, so `apply_update` re-attaches each
//!    surviving step's `extra` by id. Without that, a newer build's step field
//!    would be dropped the first time the human edited the checklist — the one
//!    level at which this promise would otherwise be false.
//!
//! # Purity
//!
//! [`apply`] takes the clock as an argument and performs no I/O. Its one
//! non-deterministic step is [`new_id`] for an `Add`, which draws from std's
//! OS-seeded `RandomState` (CLAUDE.md constraint 2 — no getrandom-based crate
//! may enter the shipped Windows binary). Ids are therefore asserted by SHAPE
//! in tests, never by value.
//!
//! See `doc/design/todo-pane.md`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use crate::pathseg::{self, SegmentError};

/// The envelope version this build writes. A store carrying a greater value is
/// read-only (see the module header).
pub const CURRENT_VERSION: u32 = 1;

/// How long a soft-deleted item's tombstone survives before the next write
/// drops it. 30 days: long enough that "I deleted the wrong thing last month"
/// is still recoverable from the file, short enough that the store does not
/// grow without bound.
pub const PURGE_AFTER_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Gap between adjacent `order` values, so a reorder is one field write rather
/// than a rewrite of the whole list.
pub const ORDER_GAP: i64 = 1024;

// ---------- caps ----------
//
// Every one of these REFUSES rather than truncates. A runaway agent loop is the
// shape being bounded, and a truncated title is a silent data loss the human
// discovers weeks later; a refusal is a message the agent can act on now and an
// audit row the human can find.

/// Longest `title`, in chars.
pub const TITLE_MAX: usize = 500;
/// Longest `notes`, in bytes.
pub const NOTES_MAX: usize = 20_000;
/// Most `tags` on one item.
pub const TAGS_MAX: usize = 20;
/// Longest single `tag`, in bytes.
///
/// Beside [`TAGS_MAX`], not instead of it: a count cap alone bounds how MANY
/// tags an item carries and nothing about how big each one is, so twenty tags
/// of a megabyte each passed every check this store had (#3285 item 1). Bytes
/// rather than chars because the bound being defended is the file's size, and
/// a `String`'s cost is its bytes. A tag is a LABEL — prose belongs in
/// `notes`, which has its own far larger cap — so this is deliberately tight
/// enough that the tag list cannot become free-form storage for a runaway
/// agent loop.
pub const TAG_BYTES_MAX: usize = 100;
/// Most `steps` on one item.
pub const STEPS_MAX: usize = 100;
/// Most LIVE (not tombstoned) items in one scope.
pub const ITEMS_MAX: usize = 5_000;

/// Highest accepted `priority`.
pub const PRIORITY_MAX: u8 = 3;

// ---------- scope ----------

/// Which list an item belongs to.
///
/// Serialises as `"global"` or `{"workspace": "<key>"}` — a shape that stays
/// readable in the file and leaves room for a third kind without a migration.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// The one list that follows the human everywhere.
    Global,
    /// One project's list, keyed by [`workspace_key`].
    Workspace(String),
}

impl Default for Scope {
    fn default() -> Self {
        Scope::Global
    }
}

impl Scope {
    /// The workspace key, for a workspace scope.
    pub fn workspace(&self) -> Option<&str> {
        match self {
            Scope::Global => None,
            Scope::Workspace(k) => Some(k),
        }
    }
}

// ---------- actor ----------

/// Who performed a write. Recorded on every item as `created_by` / `updated_by`
/// so the pane can attribute a row to the agent that touched it — the issue's
/// agent-first requirement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Actor {
    /// The human, through the pane.
    Human,
    /// An agent, through MCP.
    Agent {
        /// Agent id (`a-7`).
        id: String,
        /// Pane/agent name as the human sees it.
        name: String,
        /// The agent's group id.
        group: String,
        /// The agent's role.
        role: String,
    },
}

impl Default for Actor {
    fn default() -> Self {
        Actor::Human
    }
}

impl Actor {
    /// A short label for an audit row's `actor` column.
    pub fn label(&self) -> String {
        match self {
            Actor::Human => "human".to_string(),
            Actor::Agent { id, .. } => id.clone(),
        }
    }
}

// ---------- items ----------

/// One sub-step of an item. `2/5` in the pane.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// Stable id, so a reorder or a rename is not a delete-plus-add.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub done: bool,
    /// Unknown keys written by a newer build (module header, half 2).
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// One to-do.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub scope: Scope,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub notes: String,
    /// `"open"` or `"done"`. A string rather than a bool because the issue's
    /// later slices want room for a third state without a migration.
    #[serde(default = "status_open")]
    pub status: String,
    #[serde(default)]
    pub done_ms: Option<u64>,
    #[serde(default)]
    pub due_ms: Option<u64>,
    #[serde(default)]
    pub remind_ms: Option<u64>,
    /// The day-stamp (unix ms at local midnight) this item was put in My Day
    /// for, or `None`.
    #[serde(default)]
    pub my_day: Option<u64>,
    #[serde(default)]
    pub priority: u8,
    #[serde(default)]
    pub important: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub steps: Vec<Step>,
    /// Sort position within the scope; gaps of [`ORDER_GAP`].
    #[serde(default)]
    pub order: i64,
    #[serde(default)]
    pub created_ms: u64,
    #[serde(default)]
    pub created_by: Actor,
    #[serde(default)]
    pub updated_ms: u64,
    #[serde(default)]
    pub updated_by: Actor,
    /// Bumped on every write. An agent may send `if_rev` and be refused
    /// ([`TodoError::Conflict`]); the human's ops never carry one.
    #[serde(default)]
    pub rev: u64,
    /// Set by the archive op (#3263 S5). Archived items are live data — they
    /// are hidden from the default views, not tombstoned.
    #[serde(default)]
    pub archived_ms: Option<u64>,
    /// Soft-delete tombstone. Hidden everywhere; dropped by the first write
    /// [`PURGE_AFTER_MS`] later.
    #[serde(default)]
    pub deleted_ms: Option<u64>,
    /// Unknown keys written by a newer build (module header, half 2).
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

fn status_open() -> String {
    "open".to_string()
}

impl TodoItem {
    /// Whether this item is a tombstone.
    pub fn is_deleted(&self) -> bool {
        self.deleted_ms.is_some()
    }
    /// Whether this item is completed.
    pub fn is_done(&self) -> bool {
        self.status == "done"
    }
}

/// A project the store has seen, for the pane's scope switch label.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// Display label — the folder name, not the key.
    #[serde(default)]
    pub label: String,
    /// The root as the caller spelled it, for the reveal in the pane. **Never
    /// joined onto anything** (CLAUDE.md constraint 6): the key is a JSON map
    /// key and this is display text.
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub first_seen_ms: u64,
    /// Unknown keys written by a newer build (module header, half 2).
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

/// The whole file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TodoStore {
    #[serde(default = "default_version")]
    pub version: u32,
    /// Keyed by [`workspace_key`]. `BTreeMap` so the file's key order is
    /// stable across writes — a JSON diff of two saves shows what changed
    /// rather than a reshuffle.
    #[serde(default)]
    pub workspaces: BTreeMap<String, Workspace>,
    #[serde(default)]
    pub items: Vec<TodoItem>,
    /// Unknown keys written by a newer build (module header, half 2).
    #[serde(flatten, default)]
    pub extra: Map<String, Value>,
}

fn default_version() -> u32 {
    CURRENT_VERSION
}

impl Default for TodoStore {
    fn default() -> Self {
        TodoStore {
            version: CURRENT_VERSION,
            workspaces: BTreeMap::new(),
            items: Vec::new(),
            extra: Map::new(),
        }
    }
}

impl TodoStore {
    /// Live (not tombstoned) items in `scope`, in `order`.
    pub fn live(&self, scope: &Scope) -> Vec<&TodoItem> {
        let mut v: Vec<&TodoItem> = self
            .items
            .iter()
            .filter(|i| !i.is_deleted() && &i.scope == scope)
            .collect();
        v.sort_by_key(|i| (i.order, i.created_ms));
        v
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|i| i.id == id)
    }
}

// ---------- errors ----------

/// Why a write was refused. The `Display` text is the wire message an MCP tool
/// hands back (#3263 plan §2), so it is part of the tool contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TodoError {
    /// The store on disk was written by a build with a greater envelope
    /// version. Read-only, never quarantined, never overwritten.
    NewerVersion(u32),
    /// No such live item in the scopes the caller can see.
    Unknown(String),
    /// `if_rev` did not match. Carries `(id, actual, sent)`.
    Conflict(String, u64, u64),
    /// A cap was exceeded. Carries `(field, cap)`.
    Cap(&'static str, usize),
    /// A field was outside its allowed range. Carries `(field, detail)`.
    Invalid(&'static str, String),
    /// A minted id failed [`pathseg::check_segment`] — unreachable with the
    /// [`new_id`] alphabet, and a loud failure rather than a silent one if that
    /// recipe is ever changed.
    BadId(SegmentError),
}

impl fmt::Display for TodoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TodoError::NewerVersion(v) => write!(
                f,
                "todo store was written by a newer build (version {v}); this build writes version {CURRENT_VERSION} and will not overwrite it"
            ),
            TodoError::Unknown(id) => write!(f, "unknown todo: {id}"),
            TodoError::Conflict(id, actual, sent) => {
                write!(f, "conflict: {id} is at rev {actual} (you sent {sent})")
            }
            TodoError::Cap(field, cap) => write!(f, "refused: {field} exceeds {cap}"),
            TodoError::Invalid(field, detail) => write!(f, "refused: {field} {detail}"),
            TodoError::BadId(e) => write!(f, "refused: generated id rejected ({e})"),
        }
    }
}

impl std::error::Error for TodoError {}

// ---------- ops ----------

/// Fields an `Add` carries. Everything optional defaults to the empty/absent
/// value; caps are checked before anything is mutated.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TodoAdd {
    pub scope: Scope,
    pub title: String,
    pub notes: Option<String>,
    pub due_ms: Option<u64>,
    pub remind_ms: Option<u64>,
    pub priority: Option<u8>,
    pub important: Option<bool>,
    pub tags: Option<Vec<String>>,
    /// Step titles, in order.
    pub steps: Option<Vec<String>>,
    pub my_day: Option<u64>,
}

/// One step as an `Update` sends it. An absent `id` mints a new one.
#[derive(Clone, Debug, Default, Serialize)]
pub struct StepPatch {
    pub id: Option<String>,
    pub title: String,
    pub done: bool,
}

/// Where an item should land in its scope's order.
#[derive(Clone, Debug, Serialize)]
pub enum OrderAfter {
    /// First in the scope.
    Start,
    /// Directly after this item.
    Item(String),
}

/// Fields an `Update` carries. `None` means "leave alone"; a nested
/// `Some(None)` clears a nullable field.
///
/// Deliberately **not** `Deserialize`: the MCP layer (#3263 S2) and the Tauri
/// command layer (S3) each parse their own JSON arguments with their own strict
/// validators, and a derived decoder here would be a third, laxer way in.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TodoUpdate {
    pub id: String,
    /// When `Some`, the rev the caller believes the item is at. A mismatch is
    /// [`TodoError::Conflict`]. The human's ops never set this.
    pub if_rev: Option<u64>,
    pub title: Option<String>,
    pub notes: Option<String>,
    pub due_ms: Option<Option<u64>>,
    pub remind_ms: Option<Option<u64>>,
    pub my_day: Option<Option<u64>>,
    pub priority: Option<u8>,
    pub important: Option<bool>,
    pub tags: Option<Vec<String>>,
    /// Replaces the whole step list when present.
    pub steps: Option<Vec<StepPatch>>,
    pub order_after: Option<OrderAfter>,
}

/// Every mutation the store accepts.
#[derive(Clone, Debug, Serialize)]
pub enum TodoOp {
    Add(TodoAdd),
    Update(TodoUpdate),
    /// Set or clear completion. Steps are untouched.
    Complete { id: String, done: bool },
    /// Soft delete: a tombstone, purged [`PURGE_AFTER_MS`] later.
    Delete { id: String },
    /// Un-delete a tombstone that has not yet been purged. The inverse a
    /// `Delete` had no way to express (#3285).
    Restore { id: String },
}

impl TodoOp {
    /// The audit action name for this op.
    pub fn action(&self) -> &'static str {
        match self {
            TodoOp::Add(_) => "todo-add",
            TodoOp::Update(_) => "todo-update",
            TodoOp::Complete { .. } => "todo-complete",
            TodoOp::Delete { .. } => "todo-delete",
            TodoOp::Restore { .. } => "todo-restore",
        }
    }
}

/// What a successful [`apply`] did.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Applied {
    /// The scope the write landed in.
    pub scope: Scope,
    /// Ids the write touched — one for every op today, a list so an archive-all
    /// (#3263 S5) does not need a second return shape.
    pub ids: Vec<String>,
    /// The item as it now stands, for a single-item op.
    pub item: Option<TodoItem>,
    /// How many expired tombstones this write dropped.
    pub purged: usize,
}

// ---------- ids ----------

/// `td-` + 16 hex, from std's OS-seeded `RandomState`.
///
/// Copied from `orchestration::new_token`'s recipe rather than reaching for
/// `uuid`: CLAUDE.md constraint 2 bans every getrandom-based crate from the
/// shipped Windows binary (`ProcessPrng` is not exported on the Windows 10
/// baseline, and the binary then fails to load with `0xc0000139`).
pub fn new_id() -> String {
    format!("td-{:016x}", entropy_u64())
}

fn new_step_id() -> String {
    format!("st-{:016x}", entropy_u64())
}

/// One draw from std's OS-seeded `RandomState`, mixed with the wall clock the
/// way `new_token` does — a fresh `RandomState` per draw is what makes two
/// calls in the same millisecond differ.
fn entropy_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::hash::RandomState::new().build_hasher();
    h.write_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0),
    );
    h.finish()
}

// ---------- workspace identity ----------

/// The workspace identity for a filesystem path: canonicalise where the path
/// exists, then normalise to one spelling.
///
/// **One function, by design.** Callers hand Rust a *path*; nobody else
/// computes a key. The result is a JSON map key inside `todo.json` and is
/// **never joined onto a path** (CLAUDE.md constraint 6) — which is why it is a
/// `String` and not a `PathSegment`: its alphabet is a path's, not an
/// identifier's, and typing it as a segment would be a promise the value cannot
/// keep. If a later slice ever splits the store into per-workspace files, hash
/// the key with `std::hash::DefaultHasher::new()` (fixed keys, deterministic,
/// no getrandom) and put the hex through `PathSegment::parse` — never the raw
/// key.
///
/// `canonicalize` is attempted first because it resolves symlinks, junctions
/// and 8.3 short names, so two genuinely different spellings of one directory
/// key the same. It fails for a path that does not exist, and the lexical
/// fallback then applies the same normalisation to the caller's own spelling.
pub fn workspace_key(path: &Path) -> String {
    let raw = match std::fs::canonicalize(path) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    };
    normalize_key(&raw, cfg!(windows))
}

/// [`workspace_key`]'s pure half: the spelling rules, with the
/// case-insensitivity rule as an argument.
///
/// Split out so BOTH rule sets are testable on EVERY platform. A
/// `#[cfg(windows)]` test would leave the Windows rules unwitnessed on the two
/// CI platforms that are not Windows, and the lower-casing rule is precisely
/// the one whose absence is invisible until a human on Windows has two lists
/// for one project.
///
/// Rules, in order: strip the `\\?\` (and `\\?\UNC\`) extended-length prefix
/// `canonicalize` adds on Windows; back-slashes to forward slashes; when
/// `case_insensitive`, ASCII-lowercase; drop a trailing `/` unless the key
/// would otherwise lose its root (`/`, or `c:/`).
pub fn normalize_key(raw: &str, case_insensitive: bool) -> String {
    let mut s = raw.to_string();
    // `\\?\UNC\server\share` is a UNC path; re-spell it as `\\server\share` so
    // it matches the way a caller would have written it.
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        s = format!(r"\\{rest}");
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        s = rest.to_string();
    }
    s = s.replace('\\', "/");
    if case_insensitive {
        s = s.to_ascii_lowercase();
    }
    while s.len() > 1 && s.ends_with('/') && !s.ends_with(":/") {
        s.pop();
    }
    s
}

/// The display label for a workspace root — the last non-empty path component,
/// falling back to the whole key when there is none (a drive root).
pub fn workspace_label(key: &str) -> String {
    key.rsplit('/')
        .find(|seg| !seg.is_empty())
        .unwrap_or(key)
        .to_string()
}

// ---------- apply ----------

/// Apply one op to `store`, in place.
///
/// Pure but for [`new_id`] (module header): no I/O, no clock of its own. The
/// host module loads, calls this, atomically writes and emits — so a caller
/// never holds a whole-file handle and the multi-tenant whole-file hazard
/// (CLAUDE.md, `BoardPrefsStore`) cannot arise.
///
/// Refuses BEFORE mutating anything: every cap and range check runs first, so a
/// refused write leaves the store byte-identical.
pub fn apply(
    store: &mut TodoStore,
    op: TodoOp,
    actor: &Actor,
    now_ms: u64,
) -> Result<Applied, TodoError> {
    if store.version > CURRENT_VERSION {
        return Err(TodoError::NewerVersion(store.version));
    }

    let applied = match op {
        TodoOp::Add(add) => apply_add(store, add, actor, now_ms)?,
        TodoOp::Update(up) => apply_update(store, up, actor, now_ms)?,
        TodoOp::Complete { id, done } => apply_complete(store, &id, done, actor, now_ms)?,
        TodoOp::Delete { id } => apply_delete(store, &id, actor, now_ms)?,
        TodoOp::Restore { id } => apply_restore(store, &id, actor, now_ms)?,
    };

    // Purge AFTER the write, so the tombstone this op may just have created is
    // measured against the same clock as every other one and the reported count
    // is of tombstones that had genuinely expired.
    let before = store.items.len();
    store
        .items
        .retain(|i| !matches!(i.deleted_ms, Some(d) if now_ms.saturating_sub(d) >= PURGE_AFTER_MS));
    let purged = before - store.items.len();

    Ok(Applied { purged, ..applied })
}

fn check_title(title: &str) -> Result<(), TodoError> {
    if title.trim().is_empty() {
        return Err(TodoError::Invalid("title", "is empty".to_string()));
    }
    if title.chars().count() > TITLE_MAX {
        return Err(TodoError::Cap("title", TITLE_MAX));
    }
    Ok(())
}

fn check_notes(notes: &str) -> Result<(), TodoError> {
    if notes.len() > NOTES_MAX {
        return Err(TodoError::Cap("notes", NOTES_MAX));
    }
    Ok(())
}

/// Two independent bounds on one field: how many tags, and how big each one
/// is. They were one before #3285 — the list was counted and never measured.
fn check_tags(tags: &[String]) -> Result<(), TodoError> {
    if tags.len() > TAGS_MAX {
        return Err(TodoError::Cap("tags", TAGS_MAX));
    }
    for t in tags {
        if t.len() > TAG_BYTES_MAX {
            return Err(TodoError::Cap("tag", TAG_BYTES_MAX));
        }
    }
    Ok(())
}

fn check_priority(p: u8) -> Result<(), TodoError> {
    if p > PRIORITY_MAX {
        return Err(TodoError::Invalid(
            "priority",
            format!("must be 0..={PRIORITY_MAX}"),
        ));
    }
    Ok(())
}

fn check_steps_len(n: usize) -> Result<(), TodoError> {
    if n > STEPS_MAX {
        return Err(TodoError::Cap("steps", STEPS_MAX));
    }
    Ok(())
}

fn apply_add(
    store: &mut TodoStore,
    add: TodoAdd,
    actor: &Actor,
    now_ms: u64,
) -> Result<Applied, TodoError> {
    check_title(&add.title)?;
    let notes = add.notes.unwrap_or_default();
    check_notes(&notes)?;
    let tags = add.tags.unwrap_or_default();
    check_tags(&tags)?;
    let priority = add.priority.unwrap_or(0);
    check_priority(priority)?;
    let step_titles = add.steps.unwrap_or_default();
    check_steps_len(step_titles.len())?;
    for t in &step_titles {
        check_title(t)?;
    }
    if store.live(&add.scope).len() >= ITEMS_MAX {
        return Err(TodoError::Cap("items", ITEMS_MAX));
    }

    let id = new_id();
    pathseg::check_segment(&id).map_err(TodoError::BadId)?;

    let order = store
        .live(&add.scope)
        .last()
        .map(|i| i.order + ORDER_GAP)
        .unwrap_or(ORDER_GAP);

    let item = TodoItem {
        id: id.clone(),
        scope: add.scope.clone(),
        title: add.title,
        notes,
        status: "open".to_string(),
        done_ms: None,
        due_ms: add.due_ms,
        remind_ms: add.remind_ms,
        my_day: add.my_day,
        priority,
        important: add.important.unwrap_or(false),
        tags,
        steps: step_titles
            .into_iter()
            .map(|title| Step {
                id: new_step_id(),
                title,
                done: false,
                extra: Map::new(),
            })
            .collect(),
        order,
        created_ms: now_ms,
        created_by: actor.clone(),
        updated_ms: now_ms,
        updated_by: actor.clone(),
        rev: 1,
        archived_ms: None,
        deleted_ms: None,
        extra: Map::new(),
    };
    let scope = item.scope.clone();
    store.items.push(item.clone());
    Ok(Applied {
        scope,
        ids: vec![id],
        item: Some(item),
        purged: 0,
    })
}

/// Resolve a live item's index, or [`TodoError::Unknown`]. A tombstoned item is
/// unknown — a deleted id must read the same as an id that never existed, so a
/// caller cannot probe for what it may not see.
fn live_index(store: &TodoStore, id: &str) -> Result<usize, TodoError> {
    match store.index_of(id) {
        Some(ix) if !store.items[ix].is_deleted() => Ok(ix),
        _ => Err(TodoError::Unknown(id.to_string())),
    }
}

fn check_if_rev(item: &TodoItem, if_rev: Option<u64>) -> Result<(), TodoError> {
    match if_rev {
        Some(sent) if sent != item.rev => Err(TodoError::Conflict(item.id.clone(), item.rev, sent)),
        _ => Ok(()),
    }
}

/// An `Update`, in three steps: check everything, write the fields, re-space
/// the scope if the move closed a gap.
///
/// Split out of one 88-line body (#3285 item 4). The three steps answer three
/// different questions and only the FIRST of them may refuse, which is the
/// property `apply`'s doc promises ("refuses BEFORE mutating anything"). As one
/// function that promise was a reading order a later edit could break silently
/// by putting a `?` below the first assignment; it cannot now, because the
/// writer takes `&mut TodoItem`, returns nothing, and so has no `?` to add.
fn apply_update(
    store: &mut TodoStore,
    up: TodoUpdate,
    actor: &Actor,
    now_ms: u64,
) -> Result<Applied, TodoError> {
    let ix = live_index(store, &up.id)?;
    check_if_rev(&store.items[ix], up.if_rev)?;
    check_update(&up)?;
    // The one check that needs the STORE rather than the patch: a destination
    // is resolved against the item's live neighbours.
    let new_order = match &up.order_after {
        None => None,
        Some(after) => Some(order_for(store, &store.items[ix], after)?),
    };

    let scope = store.items[ix].scope.clone();
    write_update(&mut store.items[ix], up, new_order, actor, now_ms);
    if new_order.is_some() {
        renumber_if_crowded(store, &scope);
    }
    let item = store.items[ix].clone();
    Ok(Applied {
        scope,
        ids: vec![item.id.clone()],
        item: Some(item),
        purged: 0,
    })
}

/// Every cap and range check an `Update` owes, over the PATCH alone.
///
/// Nothing here reads the store, which is what lets it run before anything has
/// been mutated (`apply`'s doc).
fn check_update(up: &TodoUpdate) -> Result<(), TodoError> {
    if let Some(t) = &up.title {
        check_title(t)?;
    }
    if let Some(n) = &up.notes {
        check_notes(n)?;
    }
    if let Some(t) = &up.tags {
        check_tags(t)?;
    }
    if let Some(p) = up.priority {
        check_priority(p)?;
    }
    if let Some(s) = &up.steps {
        check_steps_len(s.len())?;
        for st in s {
            check_title(&st.title)?;
        }
    }
    Ok(())
}

/// Write a checked `Update` onto its item. Infallible BY SIGNATURE: every
/// refusal has already happened, in [`check_update`] and [`order_for`].
fn write_update(
    item: &mut TodoItem,
    up: TodoUpdate,
    new_order: Option<i64>,
    actor: &Actor,
    now_ms: u64,
) {
    if let Some(t) = up.title {
        item.title = t;
    }
    if let Some(n) = up.notes {
        item.notes = n;
    }
    if let Some(d) = up.due_ms {
        item.due_ms = d;
    }
    if let Some(r) = up.remind_ms {
        item.remind_ms = r;
    }
    if let Some(m) = up.my_day {
        item.my_day = m;
    }
    if let Some(p) = up.priority {
        item.priority = p;
    }
    if let Some(i) = up.important {
        item.important = i;
    }
    if let Some(t) = up.tags {
        item.tags = t;
    }
    if let Some(s) = up.steps {
        item.steps = merge_steps(&item.steps, s);
    }
    if let Some(o) = new_order {
        item.order = o;
    }
    item.updated_ms = now_ms;
    item.updated_by = actor.clone();
    item.rev += 1;
}

/// Rebuild the step list from the caller's patches, carrying each surviving
/// step's unknown keys across the replace.
///
/// A `StepPatch` is what a CALLER can express, so rebuilding the list from
/// patches alone would drop a newer build's step-level field (the suite's
/// example: `assignee`) the first time the human edited the checklist —
/// silently, and only for steps, which is exactly the asymmetry the module
/// header's round-trip claim must not have. Item, workspace and envelope keys
/// survive because those are mutated in place rather than rebuilt.
fn merge_steps(prior: &[Step], patches: Vec<StepPatch>) -> Vec<Step> {
    patches
        .into_iter()
        .map(|p| {
            let id = p.id.unwrap_or_else(new_step_id);
            let extra = prior
                .iter()
                .find(|old| old.id == id)
                .map(|old| old.extra.clone())
                .unwrap_or_default();
            Step {
                id,
                title: p.title,
                done: p.done,
                extra,
            }
        })
        .collect()
}

/// The `order` value that puts `moving` where `after` says — midway between its
/// new neighbours. When the gap has closed to nothing the caller renumbers.
fn order_for(store: &TodoStore, moving: &TodoItem, after: &OrderAfter) -> Result<i64, TodoError> {
    let live: Vec<&TodoItem> = store
        .live(&moving.scope)
        .into_iter()
        .filter(|i| i.id != moving.id)
        .collect();
    let (prev, next) = match after {
        OrderAfter::Start => (None, live.first().copied()),
        OrderAfter::Item(id) => {
            let pos = live
                .iter()
                .position(|i| i.id == *id)
                .ok_or_else(|| TodoError::Unknown(id.clone()))?;
            (Some(live[pos]), live.get(pos + 1).copied())
        }
    };
    Ok(match (prev, next) {
        (None, None) => ORDER_GAP,
        (None, Some(n)) => n.order - ORDER_GAP,
        (Some(p), None) => p.order + ORDER_GAP,
        (Some(p), Some(n)) => p.order + (n.order - p.order) / 2,
    })
}

/// Re-space a scope on [`ORDER_GAP`] when two live items have collided on one
/// `order` — the case a long run of midpoint inserts eventually reaches.
fn renumber_if_crowded(store: &mut TodoStore, scope: &Scope) {
    let ordered: Vec<String> = store.live(scope).iter().map(|i| i.id.clone()).collect();
    let orders: Vec<i64> = store.live(scope).iter().map(|i| i.order).collect();
    if !orders.windows(2).any(|w| w[1] <= w[0]) {
        return;
    }
    for (n, id) in ordered.iter().enumerate() {
        if let Some(ix) = store.index_of(id) {
            store.items[ix].order = (n as i64 + 1) * ORDER_GAP;
        }
    }
}

fn apply_complete(
    store: &mut TodoStore,
    id: &str,
    done: bool,
    actor: &Actor,
    now_ms: u64,
) -> Result<Applied, TodoError> {
    let ix = live_index(store, id)?;
    {
        let item = &mut store.items[ix];
        item.status = if done { "done" } else { "open" }.to_string();
        item.done_ms = if done { Some(now_ms) } else { None };
        item.updated_ms = now_ms;
        item.updated_by = actor.clone();
        item.rev += 1;
    }
    let item = store.items[ix].clone();
    Ok(Applied {
        scope: item.scope.clone(),
        ids: vec![item.id.clone()],
        item: Some(item),
        purged: 0,
    })
}

fn apply_delete(
    store: &mut TodoStore,
    id: &str,
    actor: &Actor,
    now_ms: u64,
) -> Result<Applied, TodoError> {
    let ix = live_index(store, id)?;
    {
        let item = &mut store.items[ix];
        item.deleted_ms = Some(now_ms);
        item.updated_ms = now_ms;
        item.updated_by = actor.clone();
        item.rev += 1;
    }
    let item = store.items[ix].clone();
    Ok(Applied {
        scope: item.scope.clone(),
        ids: vec![item.id.clone()],
        item: Some(item),
        purged: 0,
    })
}

/// Un-delete a soft-deleted item (#3285): the inverse a `Delete` had no way to
/// express. `apply` treats a tombstone as unknown, so before this op an undo
/// of a delete could only refuse — `doc/design/todo-pane.md`, "Undo refuses
/// rather than guesses", which said so and named the missing op.
///
/// Three refusals, each on its own ground:
///
///  * **no such id, or a tombstone the purge window has already passed** —
///    both [`TodoError::Unknown`]. An expired tombstone is a row this build
///    has already promised to drop (the very next write drops it), so a
///    restore that revived it would hand back data the store no longer keeps.
///    It reads as an unknown id because from the caller's side it is one, and
///    because that is what every other op answers for a tombstone.
///  * **the item is live** — [`TodoError::Invalid`], never a silent success:
///    a no-op restore is indistinguishable from one that worked, and the
///    caller asked precisely because it did not know.
///  * **the scope is full** — the same [`ITEMS_MAX`] cap an `Add` hits. A
///    restore puts a LIVE item into a scope exactly as an add does, and a cap
///    only one of the two doors respects is not a cap.
fn apply_restore(
    store: &mut TodoStore,
    id: &str,
    actor: &Actor,
    now_ms: u64,
) -> Result<Applied, TodoError> {
    let ix = store
        .index_of(id)
        .ok_or_else(|| TodoError::Unknown(id.to_string()))?;
    let deleted_ms = match store.items[ix].deleted_ms {
        None => return Err(TodoError::Invalid("restore", format!("{id} is not deleted"))),
        Some(d) => d,
    };
    if now_ms.saturating_sub(deleted_ms) >= PURGE_AFTER_MS {
        return Err(TodoError::Unknown(id.to_string()));
    }
    let scope = store.items[ix].scope.clone();
    if store.live(&scope).len() >= ITEMS_MAX {
        return Err(TodoError::Cap("items", ITEMS_MAX));
    }
    {
        let item = &mut store.items[ix];
        item.deleted_ms = None;
        item.updated_ms = now_ms;
        item.updated_by = actor.clone();
        item.rev += 1;
    }
    let item = store.items[ix].clone();
    Ok(Applied {
        scope,
        ids: vec![item.id.clone()],
        item: Some(item),
        purged: 0,
    })
}

/// Record a workspace the store has now seen, so the pane's scope switch has a
/// label and a root to reveal. Idempotent: an existing record keeps its
/// `first_seen_ms` and its unknown keys.
pub fn touch_workspace(store: &mut TodoStore, key: &str, root: &str, now_ms: u64) {
    let entry = store
        .workspaces
        .entry(key.to_string())
        .or_insert_with(|| Workspace {
            label: workspace_label(key),
            root: root.to_string(),
            first_seen_ms: now_ms,
            extra: Map::new(),
        });
    // The root is refreshed because a project can move; the label follows it.
    entry.root = root.to_string();
    entry.label = workspace_label(key);
}
