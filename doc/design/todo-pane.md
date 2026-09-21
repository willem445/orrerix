# The To-Do pane

The pane is #3263's deliverable; this note is the argument behind it. It is
written slice by slice — S1 (this text) covers the **data model**, the **store**
and **workspace identity**. S2 appends the MCP tool surface, S3 the Tauri
commands and the change event, S4 the pane itself, S5 reminders and undo.

## Why the backend owns the schema

Every other durable blob under the app data root is **opaque to Rust**.
`tabs.json`, `settings.json`, `sshprofiles.json`, `boardprefs.json` and
`sessionlog.json` are strings that `src-tauri/src/uistate.rs` stores and hands
back; the webview owns each schema, and the backend's whole job is an atomic
write and a corrupt-file quarantine. That split is right *because there is one
writer*.

The to-do store is the first blob with **two writer processes**. Agents write it
through MCP (Rust); the human writes it through the pane (TypeScript). A schema
has to live where both writers meet, so it lives in Rust —
`crates/loomux-engine/src/todo.rs` for the model and the pure `apply`,
`src-tauri/src/orchestration/todo.rs` for everything that touches the machine.

The consequence is the important part: **the frontend sends ops, never a file.**
`BoardPrefsStore` (`src/boardprefs.ts`, `doc/design/board-tree-view.md`
§"Nothing is published before the file has been read") has to perform a careful
read-before-publish dance because the frontend owns that whole blob and one
gesture beating the initial load would erase every other group's record. Here
that entire class of race is unreachable: no caller ever holds a whole-file
handle. `apply_to` loads, applies and atomically writes under one lock, and a
read that *fails* declines the write rather than defaulting to an empty store.

## Where the file lives

`<data root>/todo.json` — a sibling of `tabs.json`, under
`loomux_engine::obs::data_root()`.

| candidate | verdict |
|---|---|
| `<data root>/todo.json` (**chosen**) | one file, outliving every group and every repo checkout; the same root all durable singletons already use |
| inside a group directory | rejected: a group is a *launch*. One repo gets a new group id each time the human starts an orchestration, so the list would reset |
| inside the repo (`.orrerix/todo.json`) | rejected: the issue's explicit non-location, and a personal list must not become a committable file |
| one file per workspace (`<data>/todo/<hash>.json`) | rejected for v1: a second path-assembly point (constraint 6), more atomic-write surfaces, and the global/workspace switch reads both anyway. Revisit if one file ever exceeds ~10 MB |

Writes go through `loomux_engine::fsatomic::atomic_write` — the `tasks.json`
path, and the reason is #133: a bare `fs::write` truncates in place, so a
crash or a full disk mid-write destroys the list instead of leaving the previous
one intact.

## The envelope, and the three degraded reads

```json
{ "version": 1, "workspaces": { "<key>": { … } }, "items": [ … ] }
```

| on disk | read | write |
|---|---|---|
| absent (first run) | empty store | allowed — this is how the file is created |
| present, unreadable (permissions, a directory in its place) | empty store, `readable: false` | **refused** |
| not JSON, or JSON of the wrong shape | quarantined to `todo.corrupt.json`, empty store | allowed — the evidence is already safe under its own name |
| `version` greater than `CURRENT_VERSION` | the items, as written | **refused** |

The first and third rows are what `uistate` already does. Two things about the
rest are the whole reason `load_store` is its own function rather than a call
to `uistate::load_or_quarantine`:

* **Present-but-unreadable is not absent.** `uistate`'s helper answers an
  `Option`, which folds "nothing there" and "I could not look" into one `None`.
  Every blob it holds is best-effort, so that fold is correct there. A to-do
  list is human-authored data, and publishing an empty store over a file that
  merely would not read this time destroys a list the next launch could have
  recovered.
* **JSON-valid but the wrong shape is still corrupt.** `uistate` checks only
  "is it JSON at all", because the webview owns those schemas and validates
  them itself. This is the one blob whose schema the backend owns, so a valid
  document that is not a `TodoStore` earns the same rename a torn file does.

**A newer store is read-only, not quarantined.** A store written by a future
build is not damaged, it is *ahead*. It parses (every field is
`#[serde(default)]`), its items render, and every write is refused with the
version named. Quarantining it, or overwriting it, would destroy a list to
"recover" from a downgrade. `tasks.json`'s versionless additive-only model is
fine for a per-launch board and wrong for a store meant to survive app updates
for years, which is why this one carries a version at all.

**Unknown keys survive.** `TodoStore`, `TodoItem`, `Step` and `Workspace` each
carry a `#[serde(flatten)] extra` map, so a field a newer build wrote
round-trips through an older one instead of being dropped by the next save.
This is the second, independent half of forward compatibility: the version
guards the *shape*, `extra` guards the *fields*.

## The item

Per-item `rev`, bumped on every write. An **agent** may send `if_rev` and is
refused with `conflict: <id> is at rev N (you sent M)`; the **human's** ops
never carry one and are never refused on this ground. That asymmetry is the
issue's own rule, and it is the right one: the human is the owner of the list,
and a queue discipline declared for agents must not bounce the person who
declared it — the same argument `upsert_task_by_human` makes for WIP limits.

**Delete is soft.** A `deleted_ms` tombstone; hidden from every reader, and
*unknown* to every writer — a deleted id answers exactly as an id that never
existed does, so a caller cannot probe for what it may not see. The next write
30 days later drops it. That is what makes the human's undo possible and an
agent's `todo_delete` recoverable.

**Caps refuse; they never truncate.** Title 500 chars, notes 20 KB, 20 tags,
100 steps, 5 000 live items per scope. A runaway agent loop is the shape being
bounded, and a truncated title is a silent data loss the human finds weeks
later, where a refusal is a message the agent can act on now and an audit row
the human can find. Every check runs *before* anything is mutated, so a refused
write leaves the store byte-identical — including `rev`.

**Ordering** is an integer `order` with gaps of 1024 per scope, re-spaced when
two live items collide. A drag or an `Alt+↑` is then one field write rather
than a rewrite of the list.

**Ids** are `td-` + 16 hex drawn from std's OS-seeded `RandomState`, the
`new_token` recipe. Not `uuid` v4: CLAUDE.md constraint 2 bans every
getrandom-based crate from the shipped binary. Each minted id is put through
`pathseg::check_segment` — it never becomes a path today, and validating it
costs one line and keeps per-item attachments possible later.

## Workspace identity

**The workspace is the normalised repo root**, computed in exactly one function,
`todo::workspace_key(path)`. Callers hand Rust a *path*; nobody else computes a
key.

| candidate | verdict |
|---|---|
| repo root path (**chosen**) | stable across restarts and across every group ever launched on the repo; works for a non-git folder; matches the human's "this project". Machine-local, which matches v1's no-sync non-goal |
| group id | rejected: a launch, not a project — the list would reset per launch |
| git remote URL | rejected: two clones or worktrees of one repo would share a list unexpectedly, and a non-git folder has none. A later *alias* field, perhaps |
| project tab | rejected: a UI object, not a persisted identity |

The rules, in order: `canonicalize` where the path exists (which resolves
symlinks, junctions and 8.3 short names, so two genuinely different spellings of
one directory key the same); strip the `\\?\` / `\\?\UNC\` extended-length
prefix Windows canonicalisation adds; back-slashes to forward; ASCII-lowercase
**on Windows only**, whose filesystem is case-insensitive; drop a trailing
slash unless it is the root's own.

Those rules are split into a pure `normalize_key(raw, case_insensitive)` so both
rule sets are testable on **every** CI platform. A `#[cfg(windows)]` test would
leave the Windows rules unwitnessed on two of the three, and the lower-casing
rule is precisely the one whose absence is invisible until a human on Windows
has two lists for one project.

**Constraint 6.** The key is a JSON map key and is **never joined onto a path**.
That is why it is a `String` and not a `PathSegment`: its alphabet is a path's,
not an identifier's, and typing it as a segment would be a promise the value
cannot keep. If a later slice ever splits the store into per-workspace files,
hash the key with `std::hash::DefaultHasher::new()` (fixed keys, deterministic,
no getrandom) and put the *hex* through `PathSegment::parse` — never the raw
key.

## The lock

`TODO_WRITE_LOCK` serialises the whole load-apply-write. It is a leaf: nothing
else is acquired while it is held except the filesystem, and it is never taken
under a registry lock. A plain `std::sync::Mutex` for the same reason
`fileedit::WRITE_GATE` is one — it guards one file's durability, not a shared
in-memory table with an order to respect — acquired through
`obs::LockExt::lock_safe` so one panicking test cannot poison every later write.

## Audit

`todo_apply` writes the audit row on the **caller's** group, so an agent's edit
to the *global* list is still findable in the audit of the group whose agent
made it. Actions are `todo-add` / `todo-update` / `todo-complete` /
`todo-delete`, and a refusal is audited as `todo-refused` with the reason — a
cap that bounces a runaway loop is exactly the event the human needs to find
afterwards. A pane with no group writes no row: there is no group audit log to
write to, and the item's own `updated_by` still records who did it.
