# The To-Do pane

The pane is #3263's deliverable; this note is the argument behind it. It is
written slice by slice — S1 (this text) covers the **data model**, the **store**
and **workspace identity**. S2 appends the MCP tool surface, S3 the Tauri
commands and the change event, S4 the pane itself, and S5 reminders, undo, the
archive and `todo_restore`.

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
| …and the quarantine rename itself failed | empty store, `readable: false`, `quarantine_failed: true` | **refused** — nothing was preserved, so nothing may be overwritten |
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

The last two rows both answer `readable: false` and they are **not the same
event**, so they do not share a message. "exists but could not be read" is the
third row; the fourth says "is corrupt and could not be quarantined", because
those bytes read perfectly well and what failed was moving them aside — and the
human reading the decline is deciding which file to go and look at.

**The quarantine RENAME is under the write lock**, which is why `load_store`
takes a `TodoWriteGuard` rather than trusting a comment. A snapshot that
renamed without it could move a store a concurrent write had just published:
a reader finds corrupt bytes and decides to quarantine; a writer, under the
lock, quarantines first and writes a fresh list; the reader's rename then
lands on THAT file. Every syscall succeeds and nothing reports a thing. The
token is the compiler's proof, not a scan's: there is no way to obtain one
without the lock, so the unserialised call cannot be written (CLAUDE.md's
preference for the type system over a source-scanning guard).

**Within one process, and that qualifier is a residual rather than a caveat.**
`TODO_WRITE_LOCK` is a process-local `Mutex`, so two app instances — or the
`loomux-server` daemon (`doc/design/remote-engine-daemon.md`) — against one
data root interleave exactly as the paragraph above describes, and no test in
this repo can see it. Pre-existing rather than introduced, and written down
because a note claiming the race is closed *full stop* would be the next false
claim on this surface. Closing it needs a lock the filesystem holds, which
belongs with `fsatomic`'s primitive and not with this one caller — the same
place the torn-file window above is left.

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

**Caps refuse; they never truncate.** Title 500 chars, notes 20 KB, 20 tags of
100 bytes each, 100 steps, 5 000 live items per scope. A runaway agent loop is
the shape being bounded, and a truncated title is a silent data loss the human
finds weeks later, where a refusal is a message the agent can act on now and an
audit row the human can find. Every check runs *before* anything is mutated, so
a refused write leaves the store byte-identical — including `rev`.

The tag list carries **two** bounds because one of them is not a bound on
anything a count can see: `TAGS_MAX` says how many tags, `TAG_BYTES_MAX` how
big each one may be. With only the first, twenty tags of a megabyte apiece
passed every check the store had. Bytes rather than chars, because what is
being defended is the file.

**Ordering** is an integer `order` with gaps of 1024 per scope, re-spaced when
two live items collide. A drag or an `Alt+↑` is then one field write rather
than a rewrite of the list.

**Ids** are `td-` + 16 hex drawn from std's OS-seeded `RandomState`, the
`new_token` recipe. Not `uuid` v4: CLAUDE.md constraint 2 bans every
getrandom-based crate from the shipped binary. Each minted id is put through
`pathseg::check_segment` — it never becomes a path today, and validating it
costs one line and keeps per-item attachments possible later.

## The ops

Every mutation the store accepts, and what each one refuses. The table is the
contract both writers share — the pane's `todo_apply` decoder (below) and the
MCP tools (S2) each parse their own JSON onto these, and neither may invent an
op the engine does not have.

| op | does | refuses |
|---|---|---|
| `add` | mints an item in the **command's** scope | empty or over-cap title; over-cap notes, tag count, tag, or step count; priority out of range; the scope already holding `ITEMS_MAX` live items |
| `update` | writes the fields it names, and only those | unknown or tombstoned id; an `if_rev` that does not match; any cap or range above |
| `complete` | sets or clears `status` and `done_ms`; steps untouched | unknown or tombstoned id |
| `delete` | writes a `deleted_ms` tombstone | unknown or already-tombstoned id |
| `restore` | clears the tombstone, putting the row back where it was | unknown id; a tombstone the purge window has passed (as **unknown**, not as its own error); an item that is already live; a scope already at `ITEMS_MAX` |
| `archive` (S5) | sets or clears `archived_ms` on every id it names | an empty id list; an id named twice; any unknown or tombstoned id; ids from **more than one scope**; more than `ARCHIVE_IDS_MAX` ids. Refuses **before** it writes anything, so a refused archive leaves the store byte-identical |

**`restore` is what makes a soft delete an inverse** rather than a
one-way door. Without it `apply` treats a tombstone as unknown, so nothing —
not the pane's undo, not an agent correcting its own `todo_delete` — could put
a row back; the file kept the data for 30 days and no op could reach it. The
three refusals each answer a different question:

* **past the purge window** answers `unknown todo: <id>`, the same as an id
  that never existed. That is deliberate and not laziness: the row is one this
  build has already undertaken to drop — the very next write drops it — so
  reviving it would hand back data the store no longer guarantees is intact.
  It also keeps the "a deleted id reads exactly as one that never existed"
  rule true for every id a caller can still address.
* **an item that is live** answers `refused: restore <id> is not deleted`. A
  restore that quietly did nothing is indistinguishable from one that worked,
  and the caller asked precisely because it did not know.
* **a full scope** answers the same `ITEMS_MAX` cap an `add` hits. A restore
  puts a LIVE item into a scope exactly as an add does, and a cap only one of
  the two doors respects is not a cap — an agent refused at `add` could
  otherwise delete-and-restore its way past it.

**S5 wired both halves of that.** `inverseOp` in `src/todomodel.ts` emits
`{restore: {id}}` for a delete instead of refusing, and `todo_restore` is the
seventh MCP tool — so the pane's `u` and an agent correcting its own
`todo_delete` both reach this op. Two things about the undo of a delete are
worth keeping in view because they are what make it honest rather than merely
available: it can still FAIL, loudly, with the backend's own message in a toast
(a tombstone past its window, a scope at `ITEMS_MAX`), and it can never half
work. See "The seventh tool" below for the capability argument, which is the
part that needed making rather than writing.

### Archive carries its ids, and that is what gives it an inverse

`archive` is the pane's one BULK gesture: the Completed view's "Archive 7".
The obvious shape for that is `ArchiveDone { scope }` — sweep everything
finished — and it was rejected for one reason: **it has no inverse.**
"Everything that was completed at the time" is not a set the store can
reconstruct afterwards, so an undo would have to guess, and `inverseOp`
refuses to guess. That would have left the pane's only bulk write as the one
write a human could not take back, which is precisely backwards.

Carrying the ids also makes what moved exactly what the human was SHOWN: a row
an agent completed between the render and the click is not swept into a sweep
nobody asked for. The pane re-derives the ids from the current projection at
click time rather than from the button's own label, so the window between them
is closed too.

**One batch, one list.** `Applied` carries a single `scope`, so a batch
spanning two has no honest value for that field. An earlier revision took it
from `ids[0]` and said in its own comment that the scope was "derived rather
than assumed" — which was true of every batch except the one that mattered:
a mixed batch would have reported one list for a write that moved rows in two.
It now refuses, naming both lists so the caller need not bisect its own id list
to find the mix (#3301 review round 1). This costs the pane nothing:
`archiveShown` builds its ids from ONE projection of ONE scope.

Two further consequences are deliberate and read as inconsistencies unless stated.
**Archiving an already-archived row is accepted**, which is the opposite of
`restore`'s "a no-op is indistinguishable from a success": a bulk op names a
state it wants a set to end in, not a question about one row, and refusing the
batch because one row was already away would make the button fail exactly when
a human retries it. **There is no `todo_archive` MCP tool**: archiving is the
human's housekeeping gesture on their own finished list, not a delegate
capability, and a sweep tool is the single easiest way for a runaway agent loop
to make a list unreadable. An agent that thinks an item is finished has
`todo_complete`; putting the finished ones away is the human's.

**Archived is not deleted**, and the pane has to keep a way back or the
distinction is a lie. `inView` puts an archived item in NO view, so the
Completed view carries a "show archived" toggle beside the Archive button —
without it an archive would be a one-way door whose only exit is an undo stack
that dies with the pane. The toggle applies to Completed and nowhere else:
honouring it on `all` would put back the very rows the human archived to get
rid of. It also never moves the view STRIP's counts, which say how much work
exists, and archived work is over.

**The 30-day tombstone purge, surfaced.** Every write answers `Applied.purged`
— how many expired tombstones that write dropped — and the number has had no
reader. It still has none in the UI, and that is now a decision rather than an
omission: a toast saying "3 old deleted items were cleaned up" is a notice
about housekeeping nobody asked for, arriving at the moment the human was doing
something else. The count is in the audit log on every `todo-*` row
(`OrchRegistry::todo_apply` writes it), which is where a human looking for
"what happened to that thing I deleted last month" actually goes.

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
`todo-delete` / `todo-restore`, and a refusal is audited as `todo-refused` with
the reason — a cap that bounces a runaway loop is exactly the event the human
needs to find afterwards. A pane with no group writes no row: there is no group
audit log to write to, and the item's own `updated_by` still records who did
it.

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
* the frontend cannot NAME a scope it is not in, because it never sends a key
  at all. An absent or blank root is the global list.

**That is a claim about SCOPE, and only about scope** (#3286 review round 1).
`add` takes its scope from the root and `parse_op` refuses an op that carries
one — but `update`, `complete` and `delete` are addressed by **id alone**,
and `apply_update` → `live_index` does no scope comparison, so a call made
with workspace A's root can still mutate an item in workspace B or in the
global list. That is S1's op shape rather than anything this slice chose, and
it is harmless on THIS path because the caller is the trusted webview
(constraint 5's whole premise). It stops being harmless where the caller is an
AGENT: confining an agent to the ids it may name is #3263 S2's problem, not a
property this command layer provides.

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
{"restore":  {"id": "td-..."}}
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
**archived** item is in none of them — that is what archiving is for — and the
Completed view's own toggle is the one way to see one (see "Archive carries its
ids" above). Completed is ordered by most recent finish rather than by `order`,
because it is a log and that is the row a human opens it to find.

`plannedBucket` decides by whole-day distance, never by a calendar comparison:
a `getMonth()`-based bucket would put 1 June in a different bucket from 31 May
for no reason a human would recognise.

### My Day empties itself at local midnight, and writes nothing

The plan's §8 open question, answered by the human on the S0 mock: an item
leaves My Day at local midnight, as Microsoft To Do does. `myDayIsStale` is
the predicate that decides it and `inView(item, "myday", nowMs)` is its one
reader — which is why that function's clock parameter stopped being `_nowMs`
in the same commit.

**Nothing is written.** The item leaves the VIEW; its `my_day` stamp stays
exactly where it was put. That is the whole reason a per-viewer clock is safe
to decide this on a store two processes write to:

* a machine asleep at midnight, and a pane open across it, agree — neither has
  to have RUN at midnight, because the answer is recomputed from the stamp on
  every render;
* two windows in two timezones each empty at their OWN midnight, which is what
  a human in either one means by "today";
* nothing is destroyed, so `t` puts the item back with one keystroke, and the
  record of when it was last pulled in survives.

A midnight SWEEP would have all three the other way round, and would also be a
write from a viewer — a third kind of write to a store that has exactly two
writers, saying nothing about anybody's intent.

**And it is what keeps the list legible to an AGENT**, which is the question
the S0 mock's §"My Day auto-clears" raised and handed on: after a sweep, an
agent reading at 02:00 finds an emptied list and cannot tell "the human cleared
it" from "nothing was planned". Under a view rule it can: `my_day` is a
TIMESTAMP rather than a boolean, it is never cleared by the midnight rule, and
`todo_get` returns it. (`todo_list`'s compact row deliberately does not carry
it — that row is a title-and-status summary — so an agent that needs the
distinction opens the item, which is the split `todo_list`/`todo_get` exists
for.)

**Whole local days, never 24-hour spans.** `dayDelta` divides two local
midnights and rounds, so a DST day — 23 or 25 hours long — still rounds to the
one day it is. It is a DIFFERENCE rather than an offset, which is why it does
not use `addDays` (#3298/#3299's helper, the repo's one calendar-day
arithmetic): `addDays` answers "what is n days after this", and the question
here is "how many days apart are these two". Pinned in a forced
`TZ=America/Chicago` child process, so the assertion means something on a CI
runner with no DST of its own.

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

Two cases have no honest inverse and each says so instead of shipping a button
that silently does nothing:

* **no `before` snapshot.** Without it the pane cannot know what to restore,
  and a best-effort guess is how an undo quietly writes the wrong value.
* **an update that named no field.** There is nothing to put back.

**A delete used to be a third, and S5 is where it stopped being one.** It now
inverts to `{restore: {id}}` — see "The ops". It needs no `before` snapshot at
all, because the tombstone still carries every field.

**The stack** is `UndoStack` in `todomodel.ts`: fifty deep, inverses computed
at PUSH time, dropped from the oldest end. Three things about it are decisions
rather than defaults.

*Inverses, not forward ops replayed backwards.* The difference is the `before`
snapshot, which is a fact only the frame that made the write has; deriving it
later from a store two writers have moved since is exactly the guess
`inverseOp` refuses to make.

*Fifty is a bound on MEANING, not on memory.* Each entry is a few hundred
bytes. What is being bounded is an undo forty writes ago, in a list two agents
have also been writing to, restoring a value to a row whose context the human
no longer remembers.

*A popped entry is gone whether or not the write lands.* An undo can genuinely
fail — a tombstone past its window, a scope at `ITEMS_MAX` — and re-queuing a
gesture now known not to work is worse than saying so once with the backend's
own message. The undo of an undo is not pushed either: `u` is a history walk,
not a toggle. And the stack is CLEARED on a scope switch, because every entry
names an id in the list the pane just left and the engine resolves an id with
no scope check.

A reorder is a fourth, and it is handled differently rather than refused:
`order_after` is a destination, not a value, and the item that was above this
one may itself have moved since — so the honest inverse of a reorder is a fresh
one the pane computes from the CURRENT list, and `inverseOp` omits it.

## The tools (#3263 S2)

Six MCP tools, on the **shared** tier of `tool_defs`: `todo_list`, `todo_get`,
`todo_add`, `todo_update`, `todo_complete`, `todo_delete`.

Shared rather than orchestrator-gated because the list is the human's, not the
fleet's. A worker that spots a follow-up with the code in front of it is
exactly who should be able to write it down, and routing every such note
through the orchestrator would make the feature cost a turn nobody has. The
same argument reaches the two classes with a *positive* surface — a manager and
a lead are the panes the human actually talks to, so "put that on my list" is
typed into them more often than into anything else — and it does **not** reach
`Role::Solo`, whose contract is that a standalone token confers zero
group-scoped power. That rule is flat and this feature does not bend it.

Three gates, and each is enumerated separately on purpose:

| class | listing | dispatch |
|---|---|---|
| orchestrator, worker, reviewer, planner | the shared tier | the shared arms, no role check |
| manager | `MANAGER_SHARED` names all seven | the manager gate names all seven |
| lead | `LEAD_SHARED` names all seven | the lead gate names all seven |
| solo | the channel pair, before the shared tier is built | refused above the match |

`MANAGER_SHARED`/`LEAD_SHARED` and their dispatch gates are **default-deny**, so
a tool added to the shared tier by a later slice reaches neither class until
someone names it and argues for it — the direction a capability list should
fail in. The two halves of each pair are spelled twice rather than shared, and
`manager_tool_surface_is_exactly_the_enumerated_set` /
`lead_tool_surface_is_exactly_the_enumerated_set` assert the produced list by
name: a single shared constant would make one edit move both halves, which is
precisely what a double gate exists to catch.

### Why a lead gets them, when it gets no board, gate or queue

The withheld surface is withheld because there is nothing behind it: a lead
group has no orchestrator, so `get_state` would answer `"{}"` forever, and
listing a board tool would advertise a route with nothing at the end of it. The
to-do store is the opposite case. It is one file at the data root — not
group-scoped state — so a lead's `todo_list` returns the human's real list, and
a lead group carries a repo, so `workspace` scope resolves exactly as it does
anywhere else. Nothing in the seven is orchestration authority: a to-do reaches
no agent, no board, no branch and no gate.
(`mcp_a_lead_sees_and_may_dispatch_all_seven_todo_tools` drives the *workspace*
scope specifically, because "a lead group has a repo" is the half of this
argument that could stop being true.)

### The workspace is derived, never passed

`scope` is `"global"` or `"workspace"`, defaulting to `workspace`. The
workspace KEY is computed from the caller's own `GroupInfo.repo` through
`todo::workspace_key` — the one function that turns a path into a workspace
identity (see **Workspace identity** above) — and is **not** an argument on any
of the seven. So no group can name another project's list, and an agent cannot
widen its own reach by spelling a key. A group with no repo is told
`workspace scope unavailable` rather than silently falling back to the global
list, which would put a project note on the human's everywhere list with
nothing to say it happened.

`workspace` is the default because it is right for almost everything an agent
notices, and because the other default would quietly pile every group's project
notes onto the one list the human carries everywhere.

### `unknown todo`, and why it is the same words

The set an id may name is {the global list} ∪ {the caller's own workspace
list}. Anything else is refused with `unknown todo: <id>` — **byte for byte
what an id that never existed gets**. A distinct "not yours" would let a caller
probe another project's list for which ids are real, so the two are
deliberately indistinguishable; this is `require_in_group`'s "unknown agent"
posture applied to items, and the engine already gives a tombstoned id the same
treatment for the same reason.

That check (`todo_visible`) runs in the MCP layer and BEFORE every mutating
arm, because the engine has no notion of who is asking: `apply` refuses an id
that is absent or deleted, and this is what refuses an id that is present but
not the caller's. Its positive control is
`mcp_the_global_list_is_the_same_list_from_every_group` — without it, the
refusal test would pass equally against a build where a group could see nothing
it had not written, which would be a different and wrong feature.

### Audit, including the refusals this layer makes

`reg.todo_apply` writes the `todo-add`/`update`/`complete`/`delete` row and the
`todo-refused` row for anything the ENGINE refused (see **Audit** above). A
cross-workspace id and a malformed argument never reach it, so the MCP layer
audits those itself, with the same action and the same two detail keys — one
`todo-refused` filter over a group's audit log therefore answers "what did this
group try and get told no for" regardless of which layer said no. A caller
repeatedly probing ids it may not see is exactly the event that must not be the
one refusal in the feature leaving no trace.

### Where the role prose lives, and the one deviation

Each of `worker.md`, `reviewer.md`, `planner.md`, `manager.md` and `lead.md`
carries one bullet naming the seven and the rule (groom, never sweep; `if_rev` on
anything you did not create; delete one at a time and only when asked).
`orchestrator.md` does **not**: it measures 44,955 B at blob `816a9c22` against
`RESIDENT_CORE_BUDGET`'s 45,000, so a paragraph there would redden
`the_resident_core_is_under_the_byte_budget`. The orchestrator-facing half is
a paragraph folded into `orchestrator-playbook.md`'s EXISTING
`## Planning and scheduling` section, which is on-demand and unbudgeted.

**A NEW playbook section would not have worked, and that is the part worth
recording.** `every_playbook_section_has_a_resident_stub_naming_it` is default-deny over the
playbook's own headings with no allowlist: every section must be named by a
`read_playbook("<id>")` stub in the resident core, because the failure mode of an
on-demand playbook is not an unreadable section but an orchestrator that never
knows to ask. A stub costs more than the 45 bytes available, so a standalone
section is structurally unavailable here — and folding into a section whose stub
already exists is what #2815 actually did, rather than what its log entry reads
like at a glance. The rule lands where an orchestrator already reads about what
belongs on the board, which is the right place for "the to-do list is not your
queue" anyway.

### One residual, stated so it is falsifiable

`todo_list` filters out items carrying `archived_ms`, and **that line is covered
by a test as of S5**, which is the slice that gave `archived_ms` a writer.
Until then no fixture in the tree could build an archived item, so deleting the
filter reddened nothing — recorded here as a residual rather than left as an
unremarked green line (review round 1, finding 6), with the test named in
advance. That test is
`mcp_todo_list_omits_an_archived_row_while_todo_get_still_returns_it`, and it
is the case this paragraph asked for: the listing loses the row, `todo_get`
keeps answering for it, and the item it answers with carries the `archived_ms`
that explains the absence. The control is the same listing before anything is
archived.

### The seventh tool, and the ones there still are not

Grooming — re-titling, re-prioritising, due dates, notes, tags, splitting work
into steps — is `todo_update` plus `todo_add`. A "split" tool would be a second
way to add an item, and two ways to create one row is two shapes to keep in
step. That is still the rule, and it is why there is no `todo_split`, no
`todo_tag` and no `todo_archive`.

**`todo_restore` is the seventh, and it is the exception the rule allows for.**
It is not a second way to do anything: it is the only way to reach an op
nothing else reaches, and without it an agent cannot undo its own
`todo_delete` — precisely what a soft tombstone was for. #3285 left it
unbuilt because a new shared-tier tool must be named in `MANAGER_SHARED`,
`LEAD_SHARED` and both dispatch gates, which is a capability argument a
store-hardening slice should not make on its own. S5 made it, and it is this:

* **It widens no reach.** The tool takes one id and reaches exactly the scopes
  its caller could already see. Everything a restored row contains, its own
  group could already read through `todo_get`.
* **The one thing it could have leaked, it does not.** `todo_visible` reads
  `todo_snapshot`, which filters tombstones — so this arm needed a
  tombstone-VISIBLE read, and that is exactly the shape that can tell a caller
  "that id exists, you just cannot have it". It is a SIBLING function rather
  than a flag on the old one (`todo_visible_including_deleted`), it applies the
  same scope rule to the same field, and it answers `unknown todo: <id>` —
  the words an id that never existed answers. Pinned, with a positive control,
  by `mcp_a_tombstone_in_another_workspace_reads_exactly_as_an_id_that_never_existed`.
* **Whether an agent may revive a row a HUMAN deleted** — the question #3285
  handed on — is answered in the tool's DESCRIPTION and in every role
  template, not in Rust. The engine records who deleted a row; "they meant it"
  is not derivable from that, and a Rust check would also refuse the
  legitimate case where the human asks for it back. So it is instruction,
  which is this repo's line between a guardrail and a judgement
  (`CLAUDE.md`: "caps, pinned models, and isolation are enforced in Rust;
  agent judgment stays in the instruction templates").

## The pane (S4)

The sixth `ContentPaneKind`. A grid cell whose content is the list, hosted on
the same machinery the file explorer, the editor, the git view, the workflow
builder and the structured transcript use.

**Why a pane and not an overlay.** `doc/design/content-panes.md`'s "Why a pane
and not a bigger overlay" is the general argument, and a to-do list is its
clearest case: it is a *station* you keep open beside the work, not a *look* you
take and dismiss. The overlays this app has — git, issues, the board — float
OVER a terminal and are sized from it, which is also why none of them could
have hosted this: there is no terminal here to float over. Constraint 1 then
holds by construction rather than by discipline: there is no ConPTY behind this
pane, so nothing in it can resize one.

**`Alt+J`, and it is open-or-focus rather than a toggle.** The chord was chosen
by the `agent-cli-reference` discipline with the references fetched, not
recalled; `src/shortcuts.ts` carries the per-CLI check beside the binding, and
the residual is stated there too — Codex's reference documents no Alt binding at
all, so that one is UNVERIFIED rather than confirmed free. Alt+H came out
equally free and J took it because `h` is the conventional help letter and the
likelier of the two to be claimed later.

The gesture is deliberately not a toggle. A second press of a toggle CLOSES the
thing, and this pane holds a half-typed quick-add line and an expanded row —
state a stray keypress must not be able to throw away. Dismissing an overlay
costs nothing; closing a pane costs what is in it.

### The root is optional, and that is the one rule this kind breaks

Every other content kind opens ON a directory, so `planPaneSetup` refuses a
blank path for all four and both the launcher and the restore path probe it,
failing soft to the welcome form when the folder has gone.

A to-do pane opens on a **list**. The root only says which *workspace* list the
`◆` half of the scope switch offers, and a pane with no root is not broken — it
is the **global** list, which is a first-class scope rather than a degraded one.
So:

* `contentKindNeedsRoot` is a named predicate rather than an extra clause inside
  `planPaneSetup`, so the divergence is findable from either side and the shared
  branch can keep saying "the path is mandatory" and mean it — **and
  `planPaneSetup` branches on that predicate**, rather than on a `kind === "todo"`
  literal. The distinction is the whole value of naming the rule: as first
  written, the predicate was exported, documented and tested while the setup
  path tested the kind directly, so the rule was stated on four surfaces and
  enforced on none. A later slice adding a second rootless kind would have put
  it in the predicate, watched the guard go green, and still been told "the path
  is mandatory" by the branch. One condition now, and
  `the rootless branch is decided by the predicate, for every content kind`
  derives its expectation FROM the predicate rather than from a list of kinds it
  remembers (#3293 review round 2, finding 1);
* neither the launcher nor the restore arm probes the root. A project that has
  been deleted or unmounted costs the human the workspace half of a switch and
  nothing else, and failing soft would discard a working pane to recover from a
  folder it does not need;
* a `todo` leaf with a null `cwd` restores as a todo pane, on Global.

`test/panesetup.test.ts` and `test/tabstore.test.ts` pin both halves — the
rootless todo plan, and the four rooted kinds still refusing a blank path, which
is the control that keeps lifting `todo` out of the shared branch from having
loosened it for the kinds it still covers.

### Persistence, and the downgrade

`"todo"` joins `PersistedPaneKind` and `CONTENT_KINDS`. Additive and
shape-driven like the four before it — the workspace root rides in the existing
`cwd` — so `SCHEMA_VERSION` stays at 2 and a file written before this slice
simply never carries a `todo` leaf.

The **downgrade** direction costs exactly what the `ssh` kind's note already
describes and no more: an older build's `decodePane` does not recognise the
kind, answers null, and `decodeLayout`'s whole-tree fail-safe collapses that
tab's layout to one welcome pane. That behaviour is **unchanged** by this slice,
and it is asserted rather than assumed — one test decodes a file holding both a
`todo` leaf and a leaf from an imagined newer build, and requires the first to
be understood and the second to still collapse its tab. A decode loosened to
accept `todo` by accepting *anything* reddens there.

### Three modules, and the line between them

`todomodel.ts` (S3) is the **store's** model: what an item is, which smart view
it is in, which Planned bucket, the op shape, undo's inverse and (S5) the undo
stack itself. Its readers are this pane and `todoreminders.ts`.

`todoview.ts` (S4) is the **pane's** projection: the strip's counts, the
rendered groups, the row budget and its elision, the tag rail, the per-viewer
preferences, the un-submitted row draft and the selection walk. DOM-free and
clock-injected, so `test/todoview.test.ts` pins a month-boundary bucket and a
full 200-row elision without a browser.

`todopane.ts` owns **elements** and nothing else. The split is the S0 mock's own
(`render.js` §"two halves"), and its point is that a renderer which also decides
what My Day contains can only be tested by mounting it.

Two decisions in the projection are worth stating because the obvious
implementation gets each backwards:

* **the strip's counts are computed BEFORE the search and tag filters.** A chip
  whose number moves as you type is telling you about your query; the strip is
  there to say how much work exists. `test/todoview.test.ts`'s fixture collides
  on purpose — the query matches exactly one of three rows — so a
  counts-after-filter implementation cannot pass it;
* **the tag rail is built from the scope's open items, not from the filtered
  rows.** A rail that shrank to the tags of what you can already see could not
  be used to widen the filter, which is the only thing it is for.

### Nothing un-submitted lives in an element

`todo-changed` fires on **every** successful write from **either** writer, so an
agent editing an unrelated row through MCP re-renders this pane — and a
re-render rebuilds every control from its seed. That is the board's own lesson
(`CLAUDE.md`'s in-list-editor rule) arriving in a pane where the interfering
writer is a different *process*.

So the quick-add draft, each expanded row's notes and next-step field, the
search term, the tag filter, the selection and the expanded set are **fields on
the view**. Every control is seeded from them and writes back on `input`, never
read at submit. The one cost of a wholesale render — the caret — is paid **once,
centrally**, after each render, rather than per control as it is built: spread
over N sites, the one that gets forgotten is the one an agent's write
interrupts.

**"Untouched" is measured against the draft's own SEED, never against the item's
current value**, which is why `RowDraft` carries `seededNotes`. The two differ
exactly when a second writer has been at the row — this pane's normal condition
rather than an edge case — and measuring against the live item has two
consequences, one visible and one not. The visible one: an untouched draft flips
to "edited" the moment an agent writes. The invisible one is the defect both
reviewers' premortems found (#3293 review round 2): the draft keeps the
pre-agent notes, `commitDraft` sees them differ from the item's new value, and
**Save silently reverts a write the human never saw** — legal under
last-writer-wins and undetectable until something surfaces versions. So a
PRISTINE draft follows the store (`reseedPristineDrafts`, run beside the prune on
every render) and a draft the human has typed into is never touched. The
residual is honest rather than hidden: a human mid-sentence when an agent writes
still overwrites that write on Save. Surfacing a genuine conflict needs the
item's `rev` and a decision about what to show, and the store already carries
`rev` and `if_rev` for whoever builds it.

`rowDraftIsPristine` reads **every** typable field of `RowDraft`, because the
renderer's seed and "is this untouched" are one question asked twice; a field added to one
and not the other is #1348 N1/N4's defect, and the test drives the check off the
object's own keys so a forgotten field reddens rather than passing. The draft is
seeded from the **item**, not from a literal, so a row that already has notes
does not read as edited the moment it is opened. It is cleared on **success
only**, and on **both** routes — the Save button and the Enter key reach one
function — because clearing on the Enter route alone leaves the draft on the
route most people use.

### The bound on the event

`TodoPaneView` is the only listener on `todo-changed` and it refreshes through a
`CoalescingRefresh`, exactly as the board does with `orch-tasks-changed`:
single-flight with a trailing-edge merge, so an agent's burst costs the refetch
already in flight plus exactly one more, and the trailing run reads the final
store. It is also visibility-gated the way #1318 gates the board — a **hidden**
pane drops the wake outright rather than coalescing it, and `show()` re-reads
unconditionally, which is the half that makes the drop safe rather than merely
cheap. `test/perfpolicy.test.ts`'s row moved from `argued-none` to `throttled`
in the same commit; it had been a declared gap naming this slice.

A refused write is the one case that re-renders without an event: the store is
byte-identical and nothing fires, so the pane re-renders from what it already
has and a control the human just flipped snaps back instead of lying.

**The gate is only half of it, and the other half is a stale-response guard.**
`refreshgate.ts`'s own header says both are needed — "the gate alone would still
let a slow old-mode fetch paint stale data, and the mode check alone would leave
the new mode with nothing to render" — and this pane shipped with only the gate
(#3293 review round 2, finding 2). A `TodoSnapshot` carries no scope of its own,
so a response cannot identify itself; `refreshNow` therefore captures the root
**before** the await and drops the response if the scope has changed under it.
Without that, a refresh in flight against the workspace root when the human
presses `g` paints the **workspace** list into a pane whose header, switch and
`scopeRoot()` all say Global — and the rows in that window are live, so
completing one sends the op with `scopeRoot() === null` and it lands on the
global store against an id that is not there. Dropping the response costs
nothing: `setScope` has already asked for a fresh run and the coalescer
guarantees the trailing one.

**And the snapshot is dropped ON the scope change**, which is the half that
needs no race at all (#3293 review round 3). `setScope` renders synchronously,
before the new read has even been requested, so without this it paints the OLD
scope's rows under the NEW header and switch — rows that are live, against an
engine that resolves `update`/`complete`/`delete` by id with no scope check, so
acting on one in that window writes to whichever store holds it while the header
says otherwise. Dropping it is right rather than merely safe, and the asymmetry
with a FAILED read is the point: there the list we hold is still the truth for
the scope we are on, so publishing an empty one would destroy it; here the list
we hold is definitively the wrong scope's. The cost is one frame of the empty
state.

**What the bound does NOT cover**, written down rather than left for the row in
`test/perfpolicy.test.ts` to imply:

* it bounds **frequency, not size**. Every refresh reads the whole store over
  IPC and decodes it, and `ROW_BUDGET` bounds what is *built*, not what is
  *read*. At the engine's own caps a maximal store is a large parse on the
  webview thread, once per burst per open pane. Real stores are nowhere near it,
  nothing here measures the per-refresh cost, and a delta-read is a later
  slice's argument to make with figures in hand;
* the event's `scope` is **discarded**, so a write to the global list wakes a
  workspace-scoped pane and vice versa. Filtering on it is not a one-liner: the
  payload names a workspace KEY and the frontend may never name one (§"The
  caller names a ROOT, never a key"), so it would have to resolve through
  `TodoSnapshot.workspaces`, the key-to-root map that exists for exactly this.

### Colour: the mock's channel discipline, held by a test

`demo/todo-pane/DESIGN.md` §2 carries the argument (PR #3271 — **unmerged** at
this slice, so that path is not on `main` yet) and the stylesheet's own
header repeats it. Three claims are now pinned in `test/theme.test.ts` rather
than left to discipline:

* **the two coloured positions each stay in their own channel** — the overdue
  due date and the Overdue bucket heading in `--state-*`, the attribution dot in
  `--id-*` — pinned by NAME rather than by internal agreement, because the
  general channel guard compares a position's variants against each other and
  would pass a position whose every variant reached for the same wrong channel
  (#1344);
* **the pane spends only the state dyes it argues for.** A to-do has no agent
  state — it is not working, held, idle or ok, it is due or it is not — so
  giving `--state-working` to an in-progress task would put a second meaning on
  a pigment the fleet already reads as "an agent is running". Two dyes are
  argued for and the test asserts the SET: `--state-attention` (overdue) and
  `--state-danger` (the Delete control on hover, spent on the action rather than
  on the task);
* **gold marks, it never grounds, and the warp thread is not reused.** A list of
  thirty unchecked rows is a column of hairline rings, not a gold pane; and
  `ui-redesign.md`'s 2px left thread carries a pane's live agent state, so this
  pane's two 2px left edges are the **accent** — visibly not a state dye — and
  the test refuses a state dye in that position.

**The attribution dot takes its hue from the app's ONE role table.** DESIGN.md
§3 asks for a hue "assigned once, in one place", and this repo already has that:
`theme.ts`'s identity channel mapped per role, with `test/theme.test.ts`'s "one
role table" guard holding every surface to it. A hue hashed from the agent id
here would have been a second answer to a settled question, which is the drift
that guard exists to catch — so the dot is classed `role-<role>` and the pane
became that guard's **fifth surface**, scanned `complete: true` for the reason
the roster and the workflow node are.

That leaves a third state, and it is deliberate: a **dot** means an agent, **no
dot** means the human (absence is the human — colouring the majority case would
make the marks mean "someone" rather than "which agent"), and a **colourless
dot** means an agent whose role this build does not know, which is what a newer
orrerix writing a new role looks like from here. It is not the uncoloured-badge
bug the `complete: true` scan exists for, which is a role this build *does* know
and forgot to paint.

### Where this pane departs from the mock

Two places, both because the app around it had already answered the question:

* **reorder is `Shift+↑`/`↓`, not the mock's `Alt+↑`/`↓`.** Those are already
  `focus-up`/`focus-down` in `shortcuts.ts`, matched on `document` in the
  capture phase and withheld from every pane — so a handler in this view would
  never see them and "reorder" would silently be "move focus to the pane above";
* **the attribution hue is the role table's**, per the section above, rather
  than a local hash of the agent id.

Both are recorded here so the mock's tables and the shipped ones do not quietly
disagree.

### Reminders are per-viewer, and they never write

`src/todoreminders.ts` is a pure scan — items and a clock in, notices out —
fired by a one-minute `setInterval` that `show()` arms and `hide()`/`dispose()`
clear. `remind_ms` beats `due_ms` (an explicit "tell me at" is the human
saying when they want to hear about it; honouring the due date instead would
override that with a default), and a COMPLETED item is skipped — a notice about
finished work is the fastest way to make a channel worthless.

**"Has this fired?" is answered from a `Set` the PANE owns**, and that is the
decision the module is shaped around. A "reminded" flag on disk was rejected
three times over: the store has two writer processes and a viewer's flag says
nothing about anybody's intent; orrerix can be open in more than one window on
one file, so whichever pane scanned first would be the only one that ever
noticed; and a machine asleep at 09:00 would either burn the reminder unseen or
need a "did anyone see this" field the schema does not have.

The cost is real and is stated rather than hidden: a reminder fires again in a
pane opened after it was dismissed in another, and nothing survives a reload.
For a nudge that is the right trade.

Two smaller rules follow from "once". The key is `id@atMs`, not the bare id, so
a RESCHEDULED reminder fires at its new time while a re-render, a re-read and
an unrelated agent write to the same row all stay quiet. And a notice older
than `REMINDER_WINDOW_MS` (4 h) is CONSUMED without being shown — a pane opened
at 17:00 must not stack up everything the day already passed, and marking it
fired is what stops it arriving whenever the window next slides over it.

**One toast per TICK, however many came due.** The app has a single toast
element, so a loop calling `showToast` per notice has each call overwrite the
last: three reminders at 09:00 showed the third and the human never learned the
other two existed — a silent loss of exactly the thing the feature is for
(#3301 review round 1). `reminderSummary` coalesces the tick into one
sentence: two titles named, the rest counted, and the count is always the true
total so nothing is hidden without being accounted for. Coalescing rather than
a queue, because five queued toasts hold the human's attention for
twenty-five seconds over events that happened at the same moment — they are one
event, and a toast that listed eight titles would be a dialog.

The toast carries one action ("Show", or "Show first" when the tick coalesced),
which selects the row, expands it, and moves the view if the row is not
currently rendered. A toast whose button does nothing visible is the
silently-dead control this note argues against everywhere else.

**And "nothing" is a real answer, so it is said out loud.** The scan only ever
creates a notice for an OPEN item — but the human clicks LATER, and an agent's
`todo_complete` or `todo_archive` in that window moves the row out of every
view. Show then had nothing to reveal and revealed it silently: filters
cleared, All selected, `scrollIntoView` called on a selector matching nothing
(#3301 review round 2). `planReveal` in `todoview.ts` is the decision, pure so
it is testable without a DOM, and it answers one of three things — reveal
(optionally naming a view to move to), `gone` (a delete; the only case the
pane used to explain), or `left`, which distinguishes **done** from
**archived** because the two have different answers for the human: a finished
row is in Completed, an archived one needs the toggle there as well.

It takes **no clock**, unlike everything else in these two modules. Its three
questions are timeless, and a parameter kept for symmetry would be a claim that
the decision can move at midnight.

**That move does NOT become a preference.** The stored view is "what a fresh
pane opens on", which the human sets by clicking the strip; jumping to All
because a reminder fired is navigation the pane did on its own. Persisting it
would let a notification silently redefine a setting — every pane opened
afterwards, in every window, starting on All because something came due once
while this one happened to be on Important. `setView` therefore takes a
`persist` flag, false on exactly the call sites the pane initiates
(#3301 review round 1).

### What a maximal list costs, measured

`ITEMS_MAX` is 5,000 LIVE items **per scope**, and the pane reads one scope at
a time, so 5,000 is the largest snapshot a pane can be handed. #3293's review
recorded that nobody had measured it; this is the measurement, taken once
rather than pinned as a test, because a timing assertion on shared CI is a
flake and what the number is FOR is the scroller question `ROW_BUDGET`'s doc
defers.

Node 24.16.0, one developer machine, 20 reps after two warm-ups, against a
deliberately unkind fixture — every optional field populated, ~200-character
notes, three steps and two tags per row, so nothing takes a cheap absent
branch:

| stage | 500 items | 5,000 items |
|---|---|---|
| `JSON.parse` alone | 1.6 ms | 16.6 ms |
| `decodeSnapshot` (**including** that parse) | 1.9 ms | 19.5 ms |
| `projectPane`, All, no filter | 0.16 ms | 1.1 ms |
| `projectPane`, Planned + a two-term query | 0.54 ms | 5.0 ms |
| `scanReminders`, cold set | 0.03 ms | 0.09 ms |

Three readings, and the third is the one that matters.

**The decode is not the cost; `JSON.parse` is.** At the cap the defensive
per-field decode adds ~3 ms on top of a 16.6 ms parse — about 15% — so the
"drop what you cannot understand" discipline is close to free, and a faster
decoder would be optimising the wrong 15%.

**The reminder scan is free**, which is what makes a one-minute tick on a
per-viewer timer an easy call: 0.09 ms at the cap, no IPC, no allocation worth
naming.

**The per-KEYSTROKE projection is the one with a ceiling in sight.** 5 ms for a
filtered Planned view is inside a 16 ms frame, but not by much, and it is paid
on every character typed into search. That is the datum the scroller argument
was waiting for, and it points somewhere slightly different from where
`ROW_BUDGET` was aimed: the budget bounds how many rows are BUILT, and this
cost is the filter-and-sort over the whole scope, which a scroller would not
change. Whoever takes it should debounce the query or memoise the filtered set
first, and reach for virtual scrolling only if the DOM build turns out to
dominate. Not this slice's to spend.

### What S5 shipped, and the one thing it did not

S4 left three hooks that **said so rather than shipping a control that silently
does nothing**, and all three are now built: `u` runs the undo stack, the
in-row due control is a text field parsed by `parseQuickAdd` (one date grammar
per pane — a calendar widget with its own idea of what "next Monday" means
would be a second), and the Completed view has Archive.

The one that remains is the reorder gap: a move that has run out of integers
between two neighbours still says the list needs re-spacing instead of not
moving the row. A renumber op is a store-wide rewrite with its own argument to
make, and "it says so" is the correct behaviour until someone makes it.
