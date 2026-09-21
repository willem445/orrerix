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

**What that does not promise.** `atomic_write` writes a temp file and renames,
and when the *rename* fails — a momentarily locked destination, which a
concurrent reader on Windows produces — it falls back to exactly that bare
`fs::write`, keeping the temp so the new contents stay recoverable. The
torn-file window is therefore narrowed, not closed, and this store carries the
same exposure `tasks.json` has carried since #133. It is written down rather
than papered over because the recovery for a torn file is the quarantine above,
which is why that path is tested rather than assumed. A per-file lock that
would close it belongs with the primitive, not with this one caller.

## The envelope, and the degraded reads

```json
{ "version": 1, "workspaces": { "<key>": { … } }, "items": [ … ] }
```

| on disk | read | write |
|---|---|---|
| absent (first run) | empty store | allowed — this is how the file is created |
| present, unreadable (permissions, a directory in its place) | empty store, `readable: false` | **refused** |
| not JSON, or JSON of the wrong shape | quarantined to `todo.corrupt.json`, empty store | allowed — the evidence is already safe under its own name |
| …and the quarantine rename itself failed | empty store, `readable: false` | **refused** — nothing was preserved, so nothing may be overwritten |
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

Three of those four hold for free, because an update mutates them **in place**.
Steps are the exception and needed code: an update REPLACES the step list, and
a `StepPatch` carries only what a caller can express, so `apply_update`
re-attaches each surviving step's `extra` by id. Without that, a newer build's
step-level field would be dropped the first time the human edited a checklist —
silently, and at the one level where nothing else would have noticed.

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

## The two commands, and the `todo-changed` event

The pane reaches the store through exactly two `#[tauri::command]`s, both
`async` and both off-thread through `run_blocking`:

| command | takes | answers | frame |
|---|---|---|---|
| `todo_snapshot` | `workspace_root: Option<String>` | `TodoSnapshot` | `read_command`, degrading to an empty snapshot |
| `todo_apply` | `op: Value`, `workspace_root: Option<String>` | `Result<Applied, String>` | `mutating_command`, degrading to `COMMAND_REFUSED` |

Both frames are #1702's containment barrier. They are not strictly *required*
here — `tests/synccommands.rs` scans the **synchronous** commands in `mod.rs`,
and an `async` body runs on the async runtime rather than inside the WebView2
COM callback — but they are what every converted sibling does, and the
degraded values are the honest ones: an empty list the pane re-reads on the
next event, and a refusal the human sees.

### The caller names a ROOT, never a key

`workspace_root` is the active pane's project root as the caller spells it.
`scope_for` is the only thing that turns it into a `Scope`, and it does so
through `todo::workspace_key` — the same door the MCP path uses. Two
consequences, both deliberate:

* two spellings of one project cannot become two lists, because the
  canonicalisation happens once, on the backend, for both writers;
* the frontend cannot address a workspace it is not in, because it never sends
  a key at all. An absent or blank root is the global list.

### Why the op decoder is hand-written

The engine's op types are `Serialize`-only on purpose (their own doc says so),
and this is the half of that decision that lives in `src-tauri`. A
`#[derive(Deserialize)]` on `TodoUpdate` would be a third, laxer way into the
store:

* `TodoUpdate`'s nullable fields are `Option<Option<u64>>`, where the outer
  `None` means *leave alone* and `Some(None)` means *clear it*. Serde's derived
  decoder reads a JSON `null` into an `Option` as `None`, so the derive cannot
  express "clear this due date" at all without a `deserialize_with` helper per
  field — more code than the explicit reader, and it hides the rule instead of
  stating it.
* A derive accepts what it knows and silently ignores the rest, so `due` for
  `due_ms` would be a write that succeeds and does nothing. `parse_op` is
  **default-deny**: an unknown key is a refusal naming itself.
* `scope` is not a field a caller may send at all, per the rule above.

The wire shape is one single-key object per op, so the tag can never be
omitted:

```json
{"add":      {"title": "pay rent", "due_ms": 1750000000000, "tags": ["home"]}}
{"update":   {"id": "td-...", "if_rev": 3, "due_ms": null}}
{"complete": {"id": "td-...", "done": true}}
{"delete":   {"id": "td-..."}}
```

`order_after` takes `"start"` or `{"item": "<id>"}` rather than a nullable id,
because `null` already means *leave alone* on this wire and "first in the list"
is a destination, not the absence of one.

### The event

Every successful write emits `todo-changed` with `{scope, ids, actor}` — from
**either** writer, the pane's own `todo_apply` and an agent's through the MCP
tools. It is a NOTIFICATION, not a delta: the pane re-reads the snapshot rather
than patching its list from the payload, so a missed event costs one stale
render and never a divergent one. It is declared in `test/perfpolicy.test.ts`'s
stream manifest, today as a `debt` row against S4 — S3 ships the subscription
helper and nothing calls it, so the `CoalescingRefresh` that bounds it arrives
with the pane that needs it.

## The frontend split

`src/todo.ts` is the only module that talks to the backend, and it does so
through `./transport.ts` (constraint 5). It names the two commands, decodes
through `decodeSnapshot`, and subscribes to the event — nothing else.

Every DECISION lives in `src/todomodel.ts`, which is DOM-free and takes its
clock as a parameter. That is what makes a month-boundary Planned bucket
testable without a DOM or a fixed system date, and it is the repo's convention
for frontend logic worth testing (`layout.ts`, `steer.ts`, `spawnexpiry.ts`).

### Decode drops, it does not throw

The backend owns this schema and may be a newer build than the loaded bundle,
so `decodeSnapshot` reads defensively: an item without a string `id` and a
string `title` is dropped and the rest are kept. Those two are the bar because
they are what make a row renderable and addressable — a row you cannot click,
complete or delete would be a lie to draw. Every other field has a defined
absent value. A hostile or absent payload decodes to an empty snapshot rather
than throwing, because a pane that renders an error where the list should be,
every time a read degrades, is worse than one that renders nothing and re-reads.

### The smart views, and the one open question

`inView` is total over `SMART_VIEWS`. Four of the five are the open list sliced
differently and exclude a finished item; `completed` collects them. An
**archived** item is in none of them — that is what archiving is for (S5 adds
the op). Completed is ordered by most recent finish rather than by `order`,
because it is a log and that is the row a human opens it to find.

`plannedBucket` decides by whole-day distance, never by a calendar comparison:
a `getMonth()`-based bucket would put 1 June in a different bucket from 31 May
for no reason a human would recognise.

**My Day does not auto-clear.** Microsoft To Do empties it at midnight; whether
this one should is still open (#3263 plan §8, for the human to answer on the S0
mock). Until it is answered the predicate takes the non-destructive reading — a
carried-over item stays — and `myDayIsStale` reports the carry-over as a
separate signal the pane can surface. Nothing clears anything.

### Reorder, and the gap that runs out

The backend places a moved item at the **midpoint** of its new neighbours
(`order_for`). After enough halvings there is no integer strictly between them
and the move becomes a silent no-op: the item does not budge and nothing says
why. `needsRenumber` is what lets the pane notice. `moveTarget` computes the
`order_after` for a one-step move relative to what the human can SEE — the list
`visibleItems` returned, not the whole store.

### Undo refuses rather than guesses

`inverseOp` derives the op that undoes a write from the item as it stood
before. It carries **exactly** the fields the forward op named — an undo that
rewrote untouched fields would clobber a concurrent agent edit to something the
human never touched — and never an `if_rev`, which is stale by construction by
the time an undo runs.

Three cases have no honest inverse today and each says so instead of shipping a
button that silently does nothing:

* **a delete.** The store's delete is a soft tombstone, but the op set S1
  shipped has no RESTORE: `apply` treats a tombstoned item as unknown, so an
  update aimed at it is refused. The plan's "soft delete makes every op
  invertible" is therefore not yet true — undoing a delete needs a new engine
  op, and that is S5's to add.
* **no `before` snapshot.** Without it the pane cannot know what to restore,
  and a best-effort guess is how an undo quietly writes the wrong value.
* **an update that named no field.** There is nothing to put back.

A reorder is a fourth, and it is handled differently rather than refused:
`order_after` is a destination, not a value, and the item that was above this
one may itself have moved since — so the honest inverse of a reorder is a fresh
one the pane computes from the CURRENT list, and `inverseOp` omits it.
