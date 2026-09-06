# Design: the token charts, and the series they read

Two contracts live here, and both are load-bearing on surfaces outside this
repo's control:

- **`<group>/usage-series.jsonl` is a persisted schema.** It is written by one
  build and read by every later one.
- **`orch_usage_series(group_id, since_ms)` is a command signature.** The
  webview is not its only future caller — #888's remote client speaks the same
  wire.

Slice B (#2011) ships both plus the sampler that fills the file. The projection
and the panel are slice C; nothing here draws anything.

## Why a new file at all

`usage.json` answers *what has this group spent* and cannot answer *when*: it is
cumulative-to-now per CLI session, with no history
(`group-cost-tracking.md` §Durable accumulation). The audit log has no usage
rows at all — the only usage-touching action is `usage-corrupt` — and it rotates
at 8 MB, so it is the wrong file to keep history in. The CLIs' own transcripts
are timestamped, but reading them per panel open is the cost the scorecard
already pays once per run (a long-lived orchestrator transcript reaches hundreds
of MB), and opencode keeps no timestamped per-message record loomux reads at
all.

So loomux **samples** what it already computes. `compute_group_usage` refreshes
every live agent's snapshot on its own tick; turning that into history is one
compare per key and, at most, one small append per key per five minutes.

## The file

Append-only JSONL in the group directory. **One writer at a time** — the usage
tick, whichever thread is running it (see *Which thread writes it* below) —
**never rotated, and never rebuilt** from `audit.jsonl` or a transcript.

```
{"ts_ms":…, "kind":"sample", "key":"<usage key>", "agent":"w-2389",
 "block":"worker-std", "cli":"pi", "role":"worker",
 "in":…, "out":…, "cache_w":…, "cache_r":…,
 "cost_usd":…|null, "estimated":…, "source":"pi-transcript", "model":"…"}

{"ts_ms":…, "kind":"mark", "changed":["workflow"],
 "fp":{"version":"1.3.0-beta9","workflow":"<sha256>","agents":"…","skills":"…",
       "claude_md":"…","lessons":"…"},
 "prev":{…}, "fp_partial":false}
```

### Cumulative, not deltas

A sample carries the snapshot's counters **as of `ts_ms`**. A reader differences
consecutive rows per key (`usageseries::diff_series`). That is what makes the
one-writer, no-lock, no-rotation shape safe: a row that never landed — a failed
append, a torn line, an app that was closed — costs *resolution* and never
*spend*. A delta-encoded series would lose that interval forever, silently.

It is also why rotation is forbidden. Rotating away the oldest rows deletes the
baseline every later row is differenced against.

### Why no `AUDIT_LOCK`

`append_series_line` delegates to `append_ledger_line`, whose doc carries the
argument: a single `write_all` of a whole line is atomic enough when there is
one writer at a time and no rotation to race the append. The audit log has both
a second writer and a rotate step, which is what its lock is for.

### Which thread writes it, and what actually serializes them

**Not "the view publisher thread".** That was this note's original claim and it
was wrong in a way worth recording, because the property it was reaching for is
real and is better stated another way.

`compute_group_usage` has exactly one caller, `group_usage_memoed`, and three
ways in:

| caller | thread |
| --- | --- |
| the polled view publisher's tick | the publisher loop |
| `orch_group_usage`, `orch_autonomy` | a `run_blocking` pool thread |
| the MCP `group_usage` tool | a fresh per-request thread |

The third is the one that surprises: it asks with `Duration::ZERO`, so it never
serves the memo and always recomputes — and therefore always samples. **An agent
calling `group_usage` is a writer to this file, and walks its own repo for the
fingerprint.**

The two properties that do hold, and that everything above depends on:

- **Never the GUI thread.** All three are off it, so CLAUDE.md constraint 10 —
  about what a synchronous `#[tauri::command]` may do inside WebView2's COM
  frame — is not in play.
- **One writer at a time, per group.** `group_usage_memoed` holds that group's
  memo cell *across* `compute_group_usage`, so two ticks for one group
  serialize. That is mutual exclusion, not thread identity — and unlike a claim
  about which thread runs, it is checkable at the one place that enforces it.

### The bucket is a SPACING, not a grid

`SERIES_BUCKET_MS` is five minutes, and it is measured **from the last row
written for that key**, not from a wall-clock grid. Bucketing by
`ts_ms / BUCKET` would write two rows ten seconds apart whenever a tick straddles
a boundary, which is the one thing a budget on this file cares about. The
projection re-buckets on read, so the writer's only job is to bound how often it
writes.

### A row is written only when a counter MOVED

Both conditions must hold (`usageseries::should_sample`):

1. one of the four token counters differs from the last row written for that key;
2. the spacing has elapsed.

An idle agent therefore writes nothing: its line in the plot is flat because the
row before it says so. With no previous row, "moved" means the snapshot has
counted something at all — a freshly spawned pane whose transcript is empty is
not a data point, and writing it would put a `source: "none"` zero row at the
head of every key.

A `ts_ms` that goes **backwards** (a wall-clock correction between two ticks)
counts as elapsed. The alternative is a key that stops sampling until the clock
catches up, which is a silent hole exactly when the timestamps are already
untrustworthy.

### `block` and `cli` are the row's own fields

`role` is the capability CLASS — four values — so it cannot tell `worker-std`
from `worker-adv`, and that is the split every cost question here is actually
about. Both fields also land on `UsageSnapshot` itself (`#[serde(default)]`,
additive), which closes the `block` half of t-664.

**`cli` is never derived from `source`.** They answer different questions, and
`source` takes `statusline` and `none` values off which no CLI can be read at
all. A block with no readable usage yet reports `source: "none"` and its real
CLI.

Empty is a real answer: a row or a `usage.json` entry written before these
fields existed reads as `""`, which the projection labels *unknown*. A guessed
default would file real spend under a block that never spent it.

### `agent` is the occupant at WRITE time

`usage.json` keeps the last occupant of a key; the series keeps every one, in
order. That is what fixes the scorecard's H8 attribution *forward*, without
touching the store `usage.json` is.

## The cursor-reset clamp

A cumulative counter can go **down**. The causes are ordinary: the transcript
cursor revalidates every `CURSOR_REVALIDATE_AFTER`
(`group-cost-tracking.md` §Reading the transcript incrementally), and a rotated
or truncated transcript then re-reads shorter.

`diff_series` clamps every component to `0` and sets `reset: true` on that
interval. Never negative, never wrapped, and never silent — a labelled data
point the projection can render as a gap rather than as a cliff.

The dollar delta has its own rule: `null` when either endpoint carried no
figure, because *no figure* and *cost nothing* are different answers.

## The coverage note — history starts at deploy

**The first row for a key is a baseline and yields no delta.** Its counters are
that session's lifetime up to the moment sampling began, which may be an hour of
spend the series never saw; charging that to one five-minute bucket would draw a
spike that did not happen.

The other half of that statement is the panel's: `orch_usage_series` returns
`first_ts_ms`, the oldest row in the whole file *before* `since_ms` filtering,
and the panel prints it ("series since …"). A chart that does not say where its
history starts draws a flat line where there is simply no data. Backfilling
pre-deploy history from transcripts is slice E and is deliberately optional —
the human decides after seeing the empty state.

## Marks — when the fleet was retuned

`orchestration::tuningfp::fingerprint(repo)` hashes the repo's agent-facing
configuration into named components:

| component | surface | why |
| --- | --- | --- |
| `version` | this build's `CARGO_PKG_VERSION` | the role templates are compiled INTO the binary, so no repo file moves when they change |
| `workflow` | `.orrerix/workflow.yml` | names every block's CLI, model and persona — the #2817 roster switch IS this component |
| `agents` | `.github/agents/*.md` | the personas that file points at |
| `skills` | `.claude/skills/**` | every skill body an agent may load |
| `claude_md` | `CLAUDE.md` | the repo's standing instructions |
| `lessons` | `.orrerix/lessons.md` | what reaches every kickoff |

A `mark` row is written when the fingerprint differs from the last one this
process wrote. The rules that are easy to get wrong:

- **The first look SEEDS, it does not mark.** Otherwise every app restart stamps
  an "everything changed" vertical onto a plot where nothing had.
- **An absent surface hashes to the literal `absent`**, not to the digest of
  zero bytes, and its KEY is always present. "The file is gone" and "the file is
  empty" are different events, and a missing key would read to `fp_changed` as a
  component the other build did not know about.
- **A component present on one side only counts as changed.** That is the shape
  a build which hashes a NEW surface produces, and the first mark after such an
  upgrade is exactly the one a reader needs to see.
- **The walk is capped and says so.** A repo is caller-supplied: files over
  1 MB are skipped and the `.claude/skills/**` walk stops at depth 6. Either cap
  sets `fp_partial` on the row. A partial fingerprint is not a failure — a
  change it DID see is still real — but a reader may not treat an unchanged
  component as proof that nothing under it moved.
- **sha256 of the bytes, not `git hash-object`.** The latter shells out, once
  per file, on a polled thread. `sha2` is already in this binary's graph, needs
  no repo, and answers did-it-change for a working tree that is dirty, detached,
  or not a checkout at all.

### Where it may run

At most once per bucket, on whichever of the three threads above is running the
tick — including an MCP `group_usage` request, so an agent asking for its
group's cost walks that group's repo. **Never the GUI thread**: this reads
directories and files, and CLAUDE.md constraint 10 is about what a synchronous
`#[tauri::command]` may do inside WebView2's COM frame. Nothing reachable from
one calls it.

### Unreadable is a cap, and the cost is a false MARK

A surface that exists but cannot be read this instant — a Windows exclusive
lock, an antivirus scan — is treated as capped: `absent` for its digest, and
`fp_partial` set. Both the file arm and the directory arm do this; the directory
arm distinguishes "does not exist" (a real answer, not capped) from "exists and
could not be read" (capped).

The `fp_partial` half is the load-bearing one, and the reason is not a
slightly-wrong hash. `absent` compares equal to a genuinely missing surface, so
one locked read flips a component to absent for one bucket and back on the next:
**two mark rows, each asserting a tuning change that never happened**, on a chart
whose whole purpose is lining spend up against real changes. `fp_partial` is the
only signal a reader has that a `changed` list may be a failed read rather than
an edit.

This ships the disclosure, not a repair. Holding the previous digest through a
failed read would mean carrying per-component read errors onto the mark row and
teaching the projection to ignore them — a schema change slice C would have to
read. What is guaranteed here is that a spurious pair of marks is always flagged,
never silent.

## `orch_usage_series(group_id, since_ms)`

Async, off-thread through `run_blocking`, inside a `read_command` frame
(constraint 10), with `group_id` parsed at the boundary by `command_group`
(constraint 6). Its degrade — on an unvalidated id and on a refused frame alike
— is `null`, the same value `command_group` gives every sibling no error channel
to improve on.

```
{ group, since_ms, first_ts_ms, skipped, bytes, oversize, rows: SeriesRow[], agents: [...] }
```

- `since_ms` **filters, it does not seek**: the file is read whole. That is the
  honest shape while the writer is append-only and unrotated, and the panel
  polls at 30 s — the series moves once per five-minute bucket, so a faster poll
  could only redraw the same picture. This does **not** widen the publisher's
  1 s tiers (`polled-views.md`).
- `bytes` and `oversize` are the **revisit trigger**, and they are what keeps
  the line above from being an open-ended residual. Nothing rotates or compacts
  this file, so "cheap" has a size at which it stops being true, and a residual
  with no number in it is one nobody can tell has been reached. That number is
  `SERIES_REVISIT_BYTES` (32 MB, four times what the audit log rotates at, and
  well short of stalling a 30 s poll). Past it the payload says so and still
  returns every row: a chart that silently truncates its own history is worse
  than a slow one. When it fires, the fix is one of the two things this slice
  deliberately did not do: seek to `since_ms` instead of filtering, or compact
  the file. Sizing: ~288 rows/day per moving key at ~300 B is single-digit MB a
  month for a busy group.
- `skipped` is how many lines would not parse. Surfaced, never folded into a
  shorter chart: skipping silently is how #240 stayed invisible, as a corrupt
  log that read as a slightly shorter timeline.
- `agents` is `{id, block, cli, role, session, task}` roster-wide, so a row
  whose agent has exited still labels. A **dead** agent's CLI is read off the
  rows it wrote, never reversed out of its persisted role string:
  `workflow::kind_from_str` is a capability vocabulary with no arm for two of
  the classes, so that reversal is both lossy and a widening of a grant. Where
  nothing recorded one, `cli` is `""` — unknown, not guessed.

An absent series file is a normal **empty** payload, not a null: a group that
has not spent yet is an ordinary state.

## Where the code is

| piece | where | why there |
| --- | --- | --- |
| row schema, `should_sample`, parser, `diff_series` | `crates/loomux-engine/src/usageseries.rs` | Tauri-free, and it is the arithmetic that has to be right; `crates/loomux-server` needs it when `group_metrics` reads this file |
| the sampler, `append_series_line`, the read | `src-tauri/src/orchestration/mod.rs` | beside the usage collector it samples |
| the fingerprint | `src-tauri/src/orchestration/tuningfp.rs` | a filesystem walk over a repo path the host owns |
| the typed wrapper | `src/orchestration.ts` | the only IPC site (constraint 5) |

## What the sampler does NOT do

- It never reads a transcript. It is handed the snapshots the tick already
  computed, after the merge, so a row is written only for spend that persisted.
- It never fails a tick. A write error is one missing row in a cumulative
  series; a usage panel that stopped painting because a chart file could not be
  appended to would be strictly worse. The sampler's in-memory state is updated
  only on a **successful** append, so a failed write is retried next tick rather
  than starting a silent five-minute hole.
- It never re-samples a dead agent. `merge_usage_snapshots` returns live AND
  historical rows, and a frozen counter is not a new data point — without the
  live-key filter every restart would append one duplicate row per historical
  key, forever.
