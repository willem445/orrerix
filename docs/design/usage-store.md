# The usage store: rows in memory, an overlay on disk

How a group's usage rows are kept and written (#3677). What the rows *mean* is
[group-cost-tracking.md](group-cost-tracking.md); this note is only about
where they live and when the disk is touched. The code is
`src-tauri/src/orchestration/usagestore.rs` (the store, pure) and
`OrchRegistry::merge_usage_snapshots` in `registry/usage.rs` (its one caller).

## The cost this removes

A group's `usage.json` holds one row per agent the group has ever run. Every
usage recompute — once a second for each live or strip-leased group
([polled-views.md](polled-views.md)) — read the whole file, parsed it, merged
the live agents' fresh readings into it, pretty-printed every row and replaced
the file, whether or not a figure had moved. #3677 measured it on v1.3.1-beta7
with one live agent: a 1.37 MB file of 2,324 rows rewritten 19 times in 20
seconds, 1.7 MB/s of disk writes at idle across two groups. The work grew with
the number of *dead* agents, which is the one population that can never change
the answer.

Two things were wrong, and they need different fixes:

1. **A tick wrote when nothing had changed.** Each row carries `updated_ms`,
   stamped by the tick, so every row a live agent owned differed from the one on
   disk on every tick.
2. **A tick that did have something to write wrote everything.** One agent
   spending rewrote two thousand rows that were not its own.

## The design

**The rows live in memory, behind `usage_lock`.** A group's store is read off
the disk once and kept. The lock that already serialized the read-modify-write
now guards the data too (`TrackedMutex<UsageStores>`), so there is no second
lock and nothing new to order: see *Locks* below.

**The disk is two files.**

| file | holds | written |
| --- | --- | --- |
| `usage.json` | every row, as one pretty-printed JSON array — the shape it has always had | whole, when a row *settles* |
| `usage-live.json` | the rows that changed since `usage.json` was last written, same row shape, compact | on a tick where a row changed |

**What a tick does** (`plan_usage_write`):

| the merge found | written |
| --- | --- |
| no row whose persisted content changed | nothing |
| a change, and every row waiting in the overlay belongs to an agent this tick carried | `usage-live.json` |
| a change, and the overlay holds a row no agent in this tick owns | `usage.json`, whole; the overlay is then removed |
| a kill snapshot (`upsert_usage_snapshot`), with anything waiting | `usage.json`, whole; the overlay is then removed |

So the steady state while one agent spends is one small file replaced per
tick, holding that agent's row and those of any other agent that has moved
since the last whole write. Its size is bounded by the live agents, not by the
group's history. The whole-file write happens once per agent that ends, which
is the cadence at which the historical rows actually change.

**Why a kill writes the whole file.** A dead agent's row has no later tick to
carry it anywhere, and `usage.json` is the file every build reads. The same
write folds in whatever else was waiting, so the overlay is empty again
afterwards.

**Why a tick sometimes does.** The overlay must not become a second
ever-growing file. Two things leave a row in it that no later tick will
refresh: a process that stops without killing its agents (their rows are in
the overlay when it comes back), and a pane whose key changes (an `agent:<id>`
row from before its session id was known). The rule above catches both: the
first tick that has anything to write, and finds such a row waiting, writes
`usage.json` instead. That is one whole write per such event.

## What "unchanged" means

A tick writes when some row's **persisted content** would differ, compared
with `updated_ms` left out (`usage_rows_persist_alike`). The comparison is made
on the two rows' serialized form rather than field by field, so a field added
to `UsageSnapshot` later is compared from the commit that adds it, with no
second list to forget.

`updated_ms` keeps moving in memory on every tick, so the list a caller is
handed is as fresh as before. It is persisted with the next real change. On
disk it now means *when this row's figures were last written*, which is never
earlier than when they last changed.

That is safe only if nothing needs the per-tick value on disk. Every reader:

| reader | what it reads | needs the per-tick stamp on disk? |
| --- | --- | --- |
| `merge_usage_entry` → `cacheage::fold_activity` | the *incoming* reading's `updated_ms`, as "now" | no — it never reads the stored row's |
| `compute_group_usage` (the usage rows the panel and the MCP tool see) | counters, cost, `activity.last_active_ms`, `activity.last_wake` | no — `updated_ms` is not in the payload |
| the cache-age chip (`src/cacheage.ts`) | `last_active_ms`, `last_wake` from those rows | no — both come from `activity` |
| `series_sample` | counters, cost, model, off the list the merge returns; its own clock for `ts_ms` | no |
| `mark_dead` → `upsert_usage_snapshot` | nothing stored; it writes a fresh reading | no |
| the idle-compact backstop | `AgentEntry::last_progress_ms` | no — it does not read usage rows |
| `scripts/orch-scorecard.cjs` | counters, cost, `source`, `key`, `agent_id` | no |
| the store's own loader | `updated_ms`, to order a row in both files | it needs a stamp that moves **with each change**, which is what is persisted |

`activity` needs no such argument. It changes only when a reading moves the
counters or sets a baseline, and each of those is a content change, so it is
written when it happens.

One consequence worth knowing when reading a test: a row's *second* reading is
a change even at equal counters, because that is when the fold records the
baseline it measures growth from. A row is quiet from its third tick on.

## Loading, and which file wins

`load_usage_store` reads `usage.json`, then `usage-live.json`, and folds the
second over the first by key (`fold_usage_overlay`). A row in both is decided
by `updated_ms`: **the newer wins, and the overlay wins a tie.** A row only the
overlay has is appended.

In the ordinary case the overlay is the newer of the two by construction,
because it only ever holds rows that changed after the whole write. The
comparison exists for the two cases where it is not:

- **A crash between the two halves of a whole write.** `write_whole` replaces
  `usage.json` and *then* removes the overlay. A crash in between leaves an
  overlay whose every row is already in `usage.json`, at an `updated_ms` no
  newer than the one there. The other order would open a window in which the
  waiting rows were in neither file.
- **An older build ran in between.** It rewrites `usage.json` and does not
  know the overlay exists. Applying the overlay unconditionally afterwards
  would walk every row that build refreshed backwards.

For the comparison to be sound, `updated_ms` must not go backwards for a row
and must move strictly when its content changes. `merge_usage_entry` enforces
both: a change takes `max(reading's stamp, previous + 1)`. Two readings in one
millisecond, or a wall clock that stepped back, therefore cannot leave an older
row looking like the newer one. The cache-age fold takes the reading's own
stamp before that adjustment, so the activity clock is not affected.

## What the store inherits from the read it replaced

The old path re-read the file every tick, and so answered a few things for
free. Each one, and what answers it now:

| the per-tick read gave | now |
| --- | --- |
| a change made by anything else — a second process on the same state root, a hand edit, a restore — seen within a tick | `UsageStore::matches_disk`: the store keeps each file's length and modification time and compares them with two `stat`s per tick. A mismatch drops the store and re-reads before anything is merged |
| a group directory that was removed and made again | the same check: "absent" is a stamp like any other |
| a corrupt file noticed on the next tick | noticed on load, and on any reload the check above triggers |
| both persisted-only groups and live ones | both go through the same store; a group with no live agents is a read that costs two `stat`s |

**The blind spot, stated.** A stamp is not a read. A rewrite from outside that
preserves both the length and the modification time of a file is not seen
until the process restarts or the group ends. No writer in this repo produces
one: this app's own writes replace the file by rename, and a second instance's
writes land at a different time. The exposure is a filesystem whose
modification time is coarser than the gap between two writers, combined with a
rewrite that happens to keep the byte length. What it would cost is the
foreign change, overwritten at this process's next whole write.

It is not bounded by a timer, deliberately. A periodic re-read would cost a
parse of the whole file at whatever interval was chosen, which is the cost
this store removes, at a lower rate. The promise is narrowed instead: *a
change that moves a file's length or modification time is picked up on the
next tick.* `the_store_does_not_reread_a_file_whose_stamp_has_not_moved` pins
the blind spot itself, so this section cannot go stale with nothing red to say
so, and `a_rewrite_from_outside_the_store_is_picked_up_when_the_stamp_moves`
pins the promise.

Two details keep the stamp honest. It is taken *before* the read it describes,
so a write landing in between leaves newer rows under an older stamp, which
the next tick sees as a mismatch; the other order would record a newer stamp
over older rows and nothing would ever notice. And after this process's own
write the stamp is kept only if the file's length is the one just written.
A stamp that cannot be taken at all never equals anything, itself included.

## When things fail

- **A write fails.** The merged rows are not what is on disk, so they are not
  reported and not kept: the store is dropped and re-read, and the caller is
  given the figures that persisted. The next tick's reading differs from those
  again, so the write is retried without any retry state. This is the rule the
  write site already carried (a cost meter that invents a number on a failing
  disk is worse than one that stops moving), kept exactly.
- **A file cannot be read** (as opposed to being absent). Nothing is known
  about the rows it holds, so nothing is written over them, no store is cached,
  and the next tick tries again. The tick reports its own readings merged over
  nothing, and one `poll-read-failed` audit row (reader `usage-store`) is
  written per episode. Before this store, any failed read was taken for "no
  usage yet" and the tick's rows were then written over the unread file.
  This is the one tick on which the usage series can sample a reading that was
  not saved.
  A kill snapshot taken during such an episode is not saved either, and unlike a
  tick it has no later reading to retry with: the agent's row stays at what its
  last successful tick wrote.
- **A file reads and does not parse.** Unchanged in substance: it is moved
  aside as `<name>.bad`, a `usage-corrupt` audit row records it, and the store
  starts without it. Two things are new: the overlay gets the same treatment
  (`usage-live.json.bad`), and bytes that are not UTF-8 now count as corrupt.
  They used to fail the *read*, which was taken for "absent", so the file was
  overwritten with no copy kept.
- **A crash mid-write.** Both files are replaced through
  `fsatomic::atomic_write` (temp file, `fsync`, rename), so each is always
  either its old contents or its new ones (#133).

## Older builds, in both directions

**An existing `usage.json` loads as it is.** Its shape did not change, so
there is no migration to run: a file written by v1.3.0 (no `current_model`, no
`activity`) or by v1.3.1-beta7 loads through the same `serde` defaults as
before. Reading it does not rewrite it. Both shapes are pinned on fixtures cut
from a real store (`src-tauri/tests/fixtures/usagestore/`).

**An older build can read what this build writes**, with one limit. It reads
`usage.json`, which is still a valid, complete row list, and ignores
`usage-live.json`. So it sees every row, with a live agent's row as of the
last whole write. After an orderly end of every agent that is nothing, because
each kill writes the whole file. After the app is closed or crashes with
agents running, an older build undercounts those agents by what they spent
since the last whole write, until their sessions are resumed (a session's
counters are cumulative, so its next reading restores the full figure). The
figure is not destroyed: it is still in the overlay, and this build reads it
again on return, where the newer-wins rule above keeps whichever of the two
files is ahead per row.

`scripts/orch-scorecard.cjs` reads `usage.json` directly and is taught the
same fold, so a scorecard taken while agents are live is not behind.

## Locks

No lock was added and no ordering edge with it. `usage_lock` (rank 840) is
still a leaf that nests only `AUDIT_LOCK`, on the corrupt-file branch, as
[lock-order.md](lock-order.md) records. What is under it changed in kind, not
in extent: two `stat`s, the merge, and at most one file write, instead of a
whole-file read, parse, serialize and write. The transcript reads that produce
a tick's readings stay outside it (#743 S4b). The `poll-read-failed` note for
an unreadable store is written after the guard is dropped, because that path
takes a latch of its own.

Nothing here runs on the GUI thread (CLAUDE.md constraint 10): the callers are
the ones `series_sample`'s doc enumerates.

## What was rejected

- **Write-on-change alone, one file.** Fixes the idle case and nothing else:
  one agent spending still rewrites every row once a second.
- **Compact JSON alone.** Cuts the bytes by a constant factor and leaves them
  proportional to history.
- **Live and dead rows in two disjoint files.** Avoids deciding which file
  wins, but an older build reading `usage.json` alone would then be missing
  every live agent's row outright, rather than holding a slightly stale one.
  With an overlay, `usage.json` is always a complete file.
- **An append-only journal of changed rows.** Smaller writes still, but it
  grows by a row per tick for as long as an agent spends, so it needs a second,
  size-based trigger to rewrite the base and a rule for a torn final line. An
  overlay replaced whole is bounded by construction and inherits #133's
  guarantee.
- **SQLite.** Per-row upserts with real durability, at the price of a new file
  format no older build can read at all, a migration, and a reader the
  scorecard does not have.
- **A timer that re-reads the store.** See *The blind spot*.

## What this does not do

- **The summary is still built from every row each tick.**
  `compute_group_usage` sums and renders one JSON row per agent the group has
  had, in memory. That was true before and is unchanged; it is CPU and
  allocation, not disk, and the polled view already drops the historical rows
  before they cross the IPC seam (`live_usage_view`, #1317).
- **Memory.** A store stays loaded for as long as its group is polled, and is
  dropped by `end_group`. A group that is only ever read (a strip-leased one
  from an earlier session) keeps its rows for the process's life: one parsed
  copy of a file that path used to parse every second.
- **A whole write is still a whole write.** Ending an agent in a group with
  thousands of rows writes them all, once.

## Tests

`src-tauri/tests/orchestration/usagestore.rs`. Writes are observed off the
filesystem (length, a digest of the bytes, modification time), not off a
counter in the code under test:

- an unchanged tick writes nothing, with the control that a changed row is
  written and is what a restarted process reads;
- one agent spending does not rewrite a file of 400 historical rows, and the
  overlay holds that agent's row alone;
- the store does not re-read a file whose stamp has not moved, and does when
  it has;
- a kill snapshot lands in `usage.json`, and lifetime totals are the same
  after a restart that killed nothing;
- a row in both files, in each direction, and an overlay row with no live
  owner;
- a failed write, an unreadable store, and a non-UTF-8 file;
- `usage.json` as v1.3.0 and v1.3.1-beta7 wrote it, and `usage.json` as an
  older build would read it back.
