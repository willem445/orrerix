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

The trigger is **transience, not ignorance**. Only a failure that can clear on
the next bucket flips a component back and makes the false mark a *pair*; a
stable answer, however unhelpful — a path that is a directory where a file
belongs — hashes the same way every bucket, cannot flip, and is deliberately not
flagged. Widening `fp_partial` to cover it would flag such a repo forever and
teach a reader to ignore the flag.

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

## The projection, and the panel (slice C)

Everything above is what the app WRITES. This is what reads it.

`src/tokencharts.ts` is DOM-free and has **no intra-src imports at all**, the
same rule `timelinelayout.ts` follows (TS5097). It re-declares the wire shapes
above structurally rather than importing them, which is not merely import
hygiene: it makes the module answer *given rows of this shape, what is the
picture*, and it puts the check that the two descriptions agree at
`tokenchartsview.ts`'s call site — so a field renamed here fails the VIEW's
compile, which is where it should fail. `src/tokenchartsview.ts` holds no
arithmetic: every number it paints came from the tested module.

### Attribution: the ladder, and the two bars that are not features

First rung that decides wins, and the rung is reported as `via` on every
answer so a reader can tell a strong attribution from a weak one:

| rung | test | `via` |
| --- | --- | --- |
| 0 | `role == "orchestrator"` | `orchestrator` |
| 1 | a board row whose `assignee` is the agent's id | `assignee` |
| 2 | a board row whose `session` is the agent's session | `session` |
| 3 | the agent's brief names `#N`, and a row carries that `#N` | `brief` |
| 4 | — | `none` |

Rungs 1–3 then walk `parent` to the nearest `feature`, else the nearest
`epic`, else the chain's root, and record which of the three in `level`. The
**root** fallback is deliberate and is not the same as unattributed: a board
running no agile levels at all is legal and is the pre-#958 shape, and sending
every one of its agents to `(unattributed)` would be false — the ladder DID
find the row being worked.

Rung 3 takes the first `#N` **a board row actually carries**, not the first in
the text. A brief routinely cites issues it merely references ("per #2011's
plan") beside the one it is working, and only the board can tell those apart.

**Rung 0 sits ABOVE the ladder rather than inside it**, and that placement is
the whole rule. An orchestrator NAMED as a board row's assignee would
otherwise fire rung 1 and have its entire session lifetime billed to whichever
row it happened to be holding. There is no per-turn PR attribution for a
long-lived orchestrator (§7 of the plan), so it gets `(orchestrator,
group-wide)` — its own bar, and its own series in the plot, where its real
spend IS visible. That is how the chart avoids lying about #2502 in either
direction: not a fabricated per-feature split, and not a silent zero.

`(unattributed)` renders **first and unconditionally**, present even with no
agents on it. A bar whose job is to say "this chart is not the whole story"
must not be able to disappear by being empty. Beside it, `unknownAgentTokens`
counts spend by an agent the ROSTER does not list — a different fact from "the
board does not", since the roster is group-wide and includes exited agents, so
a miss there is a real hole rather than an unlabelled one.

The legend prints the identity the chart is checkable by:
`features + orchestrator + unattributed = total`. Its test asserts the sum
against the DELTAS themselves and not against the same three numbers re-added,
because a bar the loop failed to reach would satisfy the weaker form.

**That total is scoped to the caller's range, and the field is named `totals`
rather than `lifetime` for exactly that reason.** `featureBars` filters on
`opts.startMs`/`endMs` and the panel passes its selected window (24 h by
default), so only an unwindowed call — or the panel's `all` preset — produces
a group lifetime. The earlier name invited a comparison with the group panel's
`lifetime_tokens`, which is the whole series: following it on any group older
than the default window declared a CORRECT chart wrong. The legend now carries
its scope in the label, and `featureBars totals are SCOPED to the caller's
window` pins the divergence so the claim cannot go quietly false again
(review round 1, B2).

### Differencing, and why the grid is dense

Deltas are taken per **usage key** — the cumulative counter's own identity —
and each one is attributed to the LATER row's agent, block, cli and timestamp.
That is the forward fix for H8: `usage.json` keeps a key's last occupant, the
series keeps every one, and spend lands on whoever was there when it was
counted.

The read side re-buckets onto an **epoch-aligned** grid (the writer's spacing
is measured from its own last row, per §The bucket is a SPACING). Alignment is
what makes bucketing identical in every timezone and across a DST boundary,
the property `timelinelayout.ts` already leans on.

The grid is **dense**: every key gets a point in every bucket of the window,
zero where nothing was written. The sparse alternative joins two real samples
with one straight segment and draws an hour of steady spend that never
happened — and it looks *more* plausible than the truth, which is what makes
it the dangerous default rather than merely the wrong one.

Nothing is dropped silently. A delta outside the window is COUNTED
(`dropped`), never clamped onto an edge where it would read as spend at a
time it did not happen; a key with only its baseline row is counted
(`baselineOnlyKeys`) rather than drawn as a spike; a clamped interval is
counted (`resets`). Each is a sentence under the chart.

Cost is **null-poisoned** at every level — interval, bucket, segment, bar,
readout. One unknown makes the sum `null` rather than a partial total,
because a partial total prints a SMALLER bill, which is a wrong number rather
than a missing one.

### The mark labels are measured, not read

A `mark` row carries sha256 hashes and component NAMES — never content — so
*what changed* cannot be read out of it. What CAN be read is what the fleet
then ran: the CLI each block was last sampled with before the mark, against
the first it was sampled with after. `worker-std: opencode → pi` is therefore
a measurement of the #2817 switch rather than a guess at it.

The cost of measuring it this way is stated rather than hidden: **a block
observed on only one side of a mark contributes no row**, because "not
observed" is not "unchanged". A mark no block's CLI moved across falls back to
its component list, and `fp_partial` rides through to the view so a reader is
told when an unchanged component is not proof that nothing under it moved.

**The projection must not oversell that flag, in either direction.**
`tuningfp.rs` sets `partial` from four conditions, and it is worth naming
them because two are **stable properties of the repo**, not passing failures:

| condition | site | stable? |
| --- | --- | --- |
| file over `MAX_FILE_BYTES` | `hash_file` | **yes** — flags every bucket while the file is oversized |
| `std::fs::read` fails | `hash_file` | maybe — a lock or a scan clears; a permission does not |
| tree deeper than `MAX_DEPTH` | `walk` | **yes** — flags every bucket while the tree is deep |
| `read_dir` fails and the directory is not provably absent | `walk` | maybe |

What is deliberately NOT flagged is narrower than "stable": a surface that
genuinely does not exist, and a path that is not a file (`!meta.is_file()`),
both hash as `absent` with the flag clear — a real answer rather than a cap.

So neither direction is proof. An UNSET flag does not mean every surface was
read; a SET flag does not promise the condition is temporary, and under a size
or depth cap it will appear on every mark until the repo changes. The view's
tooltip says exactly that.

(This paragraph claimed "transient failures only" until rev-final's B1: slice
B's own §`fp_partial` argues the transient case at length because that is the
case the flag was *designed* around, and I generalised its argument into a
rule the code never had. The lesson is the one this repo already writes down —
a design note's rationale is not a substitute for reading the function, and a
claim about a sibling slice's code has to be checked against that code.)

`beforeAfter` is `null` below `k` buckets on a side — **never `0`**. A mark
two buckets after the series began has no "before", and printing `0` there
says the fleet spent nothing for an hour, which is the opposite of "we cannot
say". `k` travels on every row, because `k` is the scope of the claim.

### Model regrouping and in-session switches

Each sample's `model` field is the source for the chart's optional **split by
model** regrouping. With it enabled, a line key is `block/cli/model`; when the
sample has no model, its key says `unknown model`. The regrouping changes only
the line keys and bucket destinations. It consumes the same deltas, so the
sum of line totals is invariant, and the feature bars keep their existing
`block/cli` segments and totals. The existing **merge CLIs** control composes
with model splitting: enabling both groups by block and model. One function,
`lineKeyOf`, spells the key for both the legend (`seriesKeys`) and the routing
(`bucketSeries`), because a delta routed to a key no legend entry carries is
dropped without a sound.

The projection also compares consecutive samples under each usage `key`. When
their `model` values differ, it emits a labelled chart mark at the later
sample's timestamp, naming the key, block, CLI and old/new model. This does not
write a durable mark or change the series schema; it is derived on read just
like the measured CLI roster marks. A missing model is labelled `unknown
model`, since the sample still records that the value changed — so a key whose
samples start carrying a model mid-window gets one mark at that boundary. The
per-key grouping is taken off the same stable time sort the roster logic reads,
never a second sort, so adding model marks cannot move which same-tick sample
the roster's "last before / first after" picks land on.

**Why a sample's `model` is the pane's CURRENT model.** "At the later sample's
timestamp" is the switch time only if `model` answers *which model is this pane
on now*, and one usage source used to answer a different question. The sampler
writes `UsageSnapshot::current_model` — `SessionUsage::current_model` from the
source — per CLI:

| CLI | Where `current_model` comes from | Current? |
| --- | --- | --- |
| claude | the model of the latest counted assistant message (`TranscriptFold.last_model`) | yes |
| pi | the latest assistant entry's `provider/model` | yes |
| codex | the latest `turn_context` line's model | yes |
| opencode | the root `session.model` column | yes, per prompt — see below |

On claude, `SessionUsage::model` is a **pricing pick** — the priced model of the
single message with the most output tokens — and it is kept, unchanged, for the
usage panel's "priced against" label. It is not current: a session switched
after a long first answer keeps naming the old model until a message on the new
one out-writes that record, possibly never, so sampling it put the switch mark
late or nowhere and credited the new model's spend to the old line (#3457 B1).
Every other source already filled `model` with the latest turn's, so there the
two fields are equal. A persisted `usage.json` row written before
`current_model` existed samples its `model` instead, which keeps it from
reading as a switch to `unknown model` and back. The one visible seam: a claude
key whose earlier samples carried the pricing pick and whose later ones carry
the current model gets a single mark across the upgrade where the two differed.

opencode's column was checked against the source at the `v1.18.11` pin rather
than assumed: `SessionPrompt` calls `Session.setAgentModel` whenever a prompt's
model differs from the stored one, so the column follows the latest prompt and
is not the model the session was created with. A known gap remains on that
column's SHAPE: upstream declares it `text({ mode: "json" })` — an
`{id, providerID, variant}` object — while `opencodedb::session_usage` reads it
as a plain string. A switch still reads as a change, since the text changes,
but its label is that JSON text rather than a model id. That is a display
defect of the opencode reader, outside this chart.

Two models of one block and CLI are drawn in the same hue and the same line
style; only the legend separates them. Whether that reads well enough is the
human's visual check, not something a DOM-free test can settle.

Effort is not present in the usage sample, and it cannot currently be read as a
per-CLI value by this projection. The chart therefore does not mark effort
switches. If effort should be charted, a future wire-shape change must record
it per sample (or write an explicit switch mark); no Rust-side effort mark is
part of this change.

### Colour: why the order is measured

Colour carries the **block**; the CLI is carried by line style and bar hatch.
Two channels, not one, and the second is load-bearing rather than decorative:
the identity octet's `azure`/`violet` pair separates by ΔE **0.4** under a
protan simulation and **5.7** in normal vision (OKLab×100), so colour alone is
never allowed to be the only difference between two series.

A categorical palette's separation check is over **adjacent** pairs, which
makes the ORDER the one lever a chart owns over a fixed design system.
`HUE_ORDER` is therefore the exhaustive-search best of all 8! permutations of
`theme.ts`'s `IDENTITY` octet — worst adjacent pair ΔE 12.0 (deutan) / 11.5
(tritan) / 20.4 normal, clearing the ≥8 and ≥15 floors that
`IDENTITY`'s own declaration order fails. **Changing that order is a
measurement, not a preference:** re-run the search.

The residual is real and is why the other channels exist: `azure` and
`violet` remain an ALL-pairs collision, so two blocks four slots apart can
still collide. Hence a legend that is always present, a CLI carried by dash
pattern, and direct labels while there are few enough series to carry them.

A hue follows the **block**, from a caller-supplied stable order, never from
the windowed data's own rank — a filter that changes which series are on
screen must not repaint the survivors. That order (`hueBlockOrder`) is the
blocks that **draw a line**, by the time of their first delta in the whole
series file, then the rest of the roster. It is not the roster itself: the
roster is every block the group has ever spawned, in spawn order, and a
long-lived group's roster opens with blocks retired before the series file
existed. Those drew nothing and still took the first slots, so the blocks
the chart actually showed went grey (#3449 — five of eight slots spent on
blocks with zero samples). First-delta order also means a block that starts
spending later is appended, never inserted ahead of one already coloured;
ordering the roster and skipping the blocks without data would not give that.
The residual is compaction: nothing rewrites the file today, but a future
compaction that drops a block's early rows moves its first delta and can
shift hues. Rows with a blank block (labelled `unknown`, the file's oldest)
are not a block and are never ordered, so they cannot take a named block's
slot. The price of hues that never move is that slots go to spend that
**ever** happened: a retired spender keeps its slot for the life of the file,
so the ninth block *ever* to spend draws grey even in a window where the first
eight are silent. A **ninth** block takes the neutral ramp rather than
recycling slot 0: a repeated hue is a false claim that two blocks are one. Nothing is merged away to avoid that, because this is
a cost chart and folding two blocks' spend together to save a colour is the
worse trade.

### Where the panel lives, and what it costs

An embeddable view on the orchestrator pane, registered as embed kind
`tokens` exactly as `timeline` is — it floats as an overlay and docks through
the shared #361 engine. **No PTY resize** (constraint 1): the chart's width
comes from its own container's `ResizeObserver`, and the pure layout takes it
as a parameter. Rejected: a new `PaneKind` (content kinds are cwd-rooted
surfaces with no group) and a side-dock section (the dock follows the active
pane's *directory*; this is group-scoped like audit and timeline).

Poll cadence is **30 s** — twenty times the two audit-backed follows, and
deliberately so. The series advances once per `SERIES_BUCKET_MS` by
construction, so a faster tick could only redraw the same picture, and
`orch_usage_series` reads a whole append-only file that grows without bound.
The publisher's 1 s tiers are untouched (`polled-views.md`). The `AuditStore`
read a tick also takes is the pane's SHARED one (#1317), never a second
`orch_audit`.

Rendering is **SVG**, following `timelineview.ts`/`timelinelayout.ts` and
reusing `makeScale`/`xForTs`/`niceTicks`. The plan's S7 said Canvas; the
repo's chart convention is SVG (theme via CSS classes — `theme.ts` notes that
a canvas cannot read custom properties — and hit-testing for free). At
five-minute buckets a month is ~8.6k points per key; Canvas is the fallback
past ~50k, and the projection is renderer-agnostic so that swap would touch
the view alone.

### The coverage floor

Two floors, and they are different facts. The **series** floor is
`first_ts_ms` (§The coverage note): history starts at deploy, and the panel
prints the instant rather than drawing a flat line where there is no data. The
**audit** floor is the oldest row of the pane's `AuditStore` read, which is
capped at `AUDIT_VIEW_LIMIT` across two generations of a rotating log — so
`scorecardColumns` flags any bar with spend older than that `belowFloor`
rather than presenting "2 review rounds" for a feature that had eleven. A
`null` floor means *no rows were read*, which is "we have not looked" and
never "there is no history".

`scorecardColumns` ships here as the tested projection; its floor renders in
the pane's notes (§The coverage note), per bar, and its fail-verdict
vocabulary is ENUMERATED rather than "anything that is not a pass", so a
verdict this build has not heard of lands in neither column instead of
silently inflating the fail rate. Slice D's table beside the bars is a
DIFFERENT cut of the same log: `src/tokenscorecard.ts`, a DOM-free port of
`scripts/orch-scorecard.cjs` — one row per `block/cli` lane with per-PR
`rounds_to_pass` / `fail_rate` / wall-clock medians (`laneStats`,
`computeWindows`, `statCell`, `laneCliOf` ported; the script stays the spec,
and its `laneStats` decides pass|fail only, so an unheard-of verdict is
counted as undecided, not folded into a column) — and, as its caption,
`coverageFloor`'s spawn-row-missing half: a window credited with a delegate
whose `agent-spawn` row did not survive the read has PROVEN truncation,
stated even when empty so the caption is not vacuous. The port's stated
residuals: the text-tier attribution (H2/H3) and the `merged_at` window arm
are not ported — the pane has no `--pr-meta`, so the population is every PR
the surviving log names structurally and `end_source` always says which
fallback answered.

## Averages: what is a sample

`src/tokenaverages.ts` (#3475 slice B) answers "what does a pane / block /
model / work item cost *on average*", and the whole design is the choice of
sample — the population a mean and a median are taken over. Two answers,
because the four groupings ask two different questions.

**Agent, block, model: one sample per interval.** A sample is one `Delta` —
the spend between two consecutive rows of one usage key, exactly what the
plot's buckets are summed from. So *n* counts deltas, never rows: a key with
four rows has three samples, and a key with only its baseline row has none
(its lifetime-to-first-row is deliberately not drawn, §The coverage note).
With that sample a pane that ran away for one interval is ONE huge value among
modest ones — 100 / 100 / 10 000 has a mean of 3 400 and a median of 100. The
median is the headline (it is what a typical interval of that key costs), and
the mean is printed beside it because the gap between them IS the runaway
signal; showing either alone hides the other half of that. A `null` or blank
model keys as `unknown model` and keeps its own row — "the sampler recorded no
model" is a different fact from any model's spend.

**Item: one sample per work item.** A sample is one feature bar's TOTAL spend
inside the window (`attributeAgents`' bucket, reused as-is), so the mean is
literally tokens ÷ items and *n* is the number of items that spent in the
window — an item that spent nothing there is not a sample of it. Only
`feature` bars are items. The orchestrator's spend is group-wide by
construction (rung 0) and `(unattributed)` is spend no item could be named
for; dividing either across the items would inflate every item by work none of
them did, and dropping them would make the chart look cheaper than the group
was. So both stay OUT of the items denominator and are shown as rows of their
own, each with its interval sample like the other groupings.

**The window, the mark, and the floor.** The window is `[startMs, endMs]`,
inclusive at both ends as `bucketSeries` counts it; a delta outside it is
excluded and *counted* (`outside`), never clamped onto an edge. With a mark,
each row carries `before` / `after` halves partitioned on the delta's `tsMs`
(`< mark` before, `>= mark` after — the split `beforeAfter` makes on
bucket starts). On
an interval row the halves partition *n*; on the items row they cannot, since
an item that spent on both sides is one item in `all` and one on each side —
its *sum* partitions instead, and that is the identity pinned. The five-number
cell is `statcell.ts`' `statCell`, injected by the caller (pure modules are
import-free), so a cell below `MEDIAN_MIN_N` (3) is `null`; the mean obeys the
same floor, because a table that nulled a median of two and printed a mean of
two would invite reading the printed one. The sum is never floored. Cost is
null-poisoned as everywhere in this panel: one interval with no cost figure
makes that cell's sum, mean and median `null`, with `unknown` saying how many.

**Over time.** `averagesOverTime` cuts the same population into time buckets
and groups each bucket exactly as the totals are grouped, under the same key
list — so a key's per-bucket sums add back to its totals row, and the grid is
dense: a bucket where nothing spent is `n: 0`, not a missing point (the
plot's reason, §Differencing, and why the grid is dense). The default bucket
is one LOCAL CALENDAR DAY: a five-minute bucket holds one delta per key, where
a mean and a median are the same number, and "per day" is how the trend is
read. A local day is 23 or 25 hours across DST, so the day grid is never
`n × 86 400 000`: each step is `setDate` followed by `setHours(0, 0, 0, 0)`
(the `addDays` idiom). The second call is needed where DST starts AT midnight
(America/Santiago). That day begins at 01:00, and without the re-anchor every
later bucket would keep starting at 01:00, filing each 00:00–01:00 delta
under the day before. Both transition shapes are pinned under a forced `TZ`.
A fixed-width `bucketMs` is available and aligns to multiples of itself as
the plot's grid does. The grid is capped at `MAX_BUCKETS` (100 000), which
is a guard against a nonsense window, not a sizing: the caller picks a bucket
width that keeps grid × keys small.

`statcell.ts` is a verbatim copy of `tokenscorecard.ts`' cell, not a move:
neither pure module may import the other (TS5097), so `test/statcell.test.ts`
runs both over hand-known fixtures and a generated spread and asserts they
agree — the duplication is pinned rather than trusted.

## Lifecycle rates — audit rows, and what the window cannot see

`src/tokenlifecycle.ts` (#3475 slice D) derives the work-item and PR rates the
pane plots beside the tokens: time in each board status, time to completion,
items done per day, review rounds and CI attempts per PR. It reads the pane's
shared `AuditStore` read and nothing else, and returns raw samples — the view
applies `statCell`, so there is one definition of a median in the pane.

**Why audit rows and not the board.** A board row holds its current status and
`updated_ms`, the instant of its LAST write, which a note appended a week after
the task finished moves. The board keeps no history. Every status change,
though, passes through the one backend call that audits the whole task snapshot
as `task-upsert` (or `task-claim`, the guarded grab), so two consecutive rows for
one `detail.id` whose `status` differs are a transition, dated at the later
row. There is no `prev_status` on the row; the reader keeps the last status it
saw. An unchanged-status write (a title edit, a note) is not a transition and
does not split a span.

**What each figure is.**

- *Time in a status* is a span from the row that entered it to the row that
  left it, and only when BOTH rows are in the read. A span is attributed to the
  window by its leaving instant.
- *Time to completion* runs from the task's first row in the read to its first
  dated `done`. A first row that is `queued` is read as the task's creation — the
  same rule that starts its queued span, so the two figures cannot disagree
  about when the task began (`fromQueued: true`). A first row in any other
  status means the queued row aged out, so the figure is a lower bound, and the
  sample says so (`fromQueued: false`).
- *Done* is counted once per task, at its FIRST dated `done` — a reopen and a
  second `done` are transitions, not a second completion. `doneAtMs`/`doneIds`
  are slice C's input for "tokens per completed item", and hold only tasks whose
  FIRST dated `done` is inside the window: C divides a whole-window token total
  by their count, so a task finished before the window (and reopened and
  finished again inside it) is not in the denominator.
- *Done per day* is by local calendar day (`setDate`), because a DST day is 23 or
  25 hours and a 24-hour stride files an item done just after midnight under the
  day before. The test forces `TZ=America/Chicago` in a child `node` for the
  same reason `todomodel.test.ts` does: CI runs in UTC, where the wrong
  arithmetic passes. The *rate* divides by the window's length in calendar days
  (`windowDays`), where a partial first or last day counts as the fraction of
  that day it covers — a rolling seven-day window starting mid-afternoon touches
  eight days, and dividing by eight would under-read it by an eighth.
- *Review rounds* per PR are the number of distinct heads the PR's busiest
  block gave a verdict on: a round is one pass of every lane, so summing across
  blocks counts a three-lane round three times, and a pass is a head, so a lane
  re-recording its verdict at the same head (a body edit re-asks it) is not a
  new round. A verdict row with no head cannot be matched to another and counts
  as its own round. The review driver keeps its own counter on
  `rd-lane-spawned.detail.round`, and the two are compared and a disagreement
  is flagged, never reconciled — one source lost rows and the pane cannot tell
  which. `rd-handback` carries no `round`, so it is not read for one. A PR
  belongs to the window its last verdict falls in.
- *CI attempts* count `rd-ci-green` and `rd-ci-red` per PR. A PR the driver never
  drove has no such rows and reads `null`, not zero.

**What the window cannot see.** The read is capped at `AUDIT_VIEW_LIMIT` rows over
two rotating generations, and every figure is over the rows that survived it.
A task is born `queued`, so a first row that is `queued` is read as its
creation, and both its queued span and its time to completion start there.
That reading is wrong in one case the pane cannot detect: the creation row aged
out and a later write to the still-queued task (a note, a title edit) survived.
The Task snapshot carries no creation instant, so that row looks exactly like a
creation, and both figures come out short by the same amount — never one short
and the other exact. The alternative — distrusting every first row — would
leave `queued` unmeasured for every task created inside the read, which is
nearly all of them, to protect the few created before the floor. A first row in
any OTHER status entered it at an unknown instant — the row may be the
transition or any later write — so that span is never reported; it is counted
`openedBeforeWindow` instead. For the same reason a task whose first row is
already `done` is `doneUndated` and is never dated at that row, even if it is
reopened and finished again later. Unlike `openedBeforeWindow`, which counts
only first rows the window can see, `doneUndated` counts across the WHOLE read,
the window's edges ignored: it answers "how many done tasks could not be
dated", not "how many in this window", and the two are not the same kind of
count to set side by side. Rounds and CI attempts older than the read are
missing, which is why `floorMs` travels on the result; the wire carries no
truncation flag, so a read at the cap reports `mayBeTruncated` and the pane says
"may be". Nothing is backfilled, and there is deliberately no fallback to the
board's `updated_ms` — the wrong instant, silently.

The window's END has the mirror-image blind spot, and it matters for any window
ending before now. A span is attributed by its leaving instant, so a span that
enters inside the window and leaves after `endMs` is counted nowhere in that
window — not in `values`, and not in `stillOpen`, which is only for a status the
read ended on; a first-seen status that leaves after `endMs` is not in
`openedBeforeWindow` either. And a PR belongs to the window its LAST verdict
falls in, so a PR whose earlier verdicts sit inside the window but whose last
one falls after `endMs` is absent from that window's `reviewRoundsPerPr`. Each sample is
counted in exactly one window by construction; in a historical window, the
samples still in flight at its end are counted in a later one instead.

**Over time.** Each rate is also a series over the chart window — calendar days
by default, fixed-width buckets when `bucketMs` is given — holding the raw
samples per bucket (items done, time to completion, time in each status, rounds
per PR), so slice E plots a median per bucket with the same `statCell`. Each
sample lands in exactly one bucket, by the instant of its own event, so a
series sums to its total. A tuning mark splits every sample the same way into
before and after.

## Tokens per completed item — what divides what

`src/tokenperitem.ts` (#3475 slice C) answers *how many tokens does a finished
work item cost, and which roles spent them*. It is DOM-free and import-free
(TS5097, as above); the shapes it reads — a `Delta`, an `Attribution`, a board
row — are declared as the structural subset it needs, so slice E passes
`diffRows`'s deltas and `attributeAgents`'s result straight in.

**The numerator is every token the group spent in the window.** The
orchestrator's included, in-flight work included, unattributed spend
included. It is a *throughput* ratio — spend over a period divided by what
the period finished — and that choice is forced by the two other readings the
panel needs. A before/after pair around a tuning mark, and a per-day trend,
must each add back up to the figure beside them, and only a numerator
partitioned by the DELTA's instant does: an item's own spend is spread over
days it was not finished on, so "the tokens of the items finished today" is a
quantity no day owns. The alternative — sum only the spend attributed to the
done items — was rejected for that reason; what it would have said is kept as
`byClass`, which splits the numerator into spend on a done item (the matched
row or any container above it, walked cycle-safe), the orchestrator's, spend
on rows not done (`inFlight`) and `unattributed`. The ratio is never shown
without its composition.

**The denominator is the caller's `doneIds`.** The orchestrator IS a cost of
the items and is in the numerator; its bar is never an item, and its role row
carries a note saying so. `perItem` is `null` when there are no items — never
`0`, never `Infinity`.

**An item is placed in time only by its done instant.** `doneIds` comes from
slice D's lifecycle projection (the done transitions dated inside the window,
with `doneAtMs`). Until slice E wires that in, the view passes the board's currently-`done`
rows and labels the figure *board state, not dated*: the whole-window ratio
still stands, but an undated item cannot be put on a side of a mark or in a
day, so those item counts and ratios are `null` — never guessed from a board
row's `updated_ms`, which is the row's last write, not its done moment. A
done instant OUTSIDE the window counts as undated too: that item was not done
in this window, so neither a half nor a bucket may claim it. Both entry points
read that one rule (`placeItems`), so they cannot disagree on which items are
placed.

**Role share reads `Delta.role`** — the role the series row carried, not a
list. A role this build has never heard of keeps its own row, and a blank one
reads `unknown`, apart (the `"claude" ?` rule applied to roles). The class
split also reads the delta's role for the orchestrator, so an orchestrator
whose agent has left the roster is still counted as the orchestrator's spend
rather than as unattributed.

**The window is inclusive at both ends**, `featureBars`' rule, so this
numerator equals the bars' total over the same window. Deltas outside it are
counted (`excluded`), never silently dropped. `markTsMs` partitions on
`tsMs < mark`; a delta on the mark is after it.

**Over time** (`perCompletedItemOverTime`), the default bucket is a CALENDAR
day: local midnights, advanced with `setDate` — a DST day is 23 or 25 hours,
and `n * 86_400_000` would move every later bucket boundary by an hour. A
fixed `bucketMs` aligns to its multiples, as `bucketSeries` does. Buckets are
half-open; the grid always includes the bucket holding `endMs`. Over the
same window the buckets' tokens and `byClass` sum to the totals'. When a mark
is supplied, bucket item counts sum to `before.items + after.items`; without a
mark, the trend's placed-item count is compared with its own `items` total (the
totals have no before/after item counts). When `doneAtMs` is supplied, the
series' `unplacedItems` equals the totals' `undatedItems`, so placed plus
unplaced is `items`. Undated as a whole, every bucket's items are `null` and
`unplacedItems` is all of `doneIds`, while the totals report `undatedItems` 0
and `null` half counts: the two say "cannot place" in their own shapes. That is the
property slice E's trend line and its table share; it holds unless the grid hit
its bucket cap (`truncated`, whose spend is then counted `excluded`). A
degenerate or inverted window yields no buckets.

## Interaction: the window is a value, not a preset

The chart window is an explicit `[startMs, endMs]` value. `zoomAbout` scales
around the pointer's time while preserving its fractional position; `panBy`
translates without changing the span. Both use `clampWindow` to stay inside
`[first_ts, max(now, last_ts)]`, with a minimum of two buckets and a maximum
of the available series extent. Invalid or inverted windows are preserved
rather than repaired into a different request. A selected mark's comparison
window snaps to the first bucket start at or after the mark, matching
`beforeAfter`'s split, then spans `k` whole buckets on each side. The half-open
result is `[split - k·bucketMs, split + k·bucketMs)`; `markSpan` accepts the
grid origin (epoch-aligned by default). It uses
`tokencharts.ts`'s `DEFAULT_BEFORE_AFTER_K` for the same half-width as the
readout. On a log axis, zero and one share the floor mapping; ticks include
zero and powers of ten from ten upward, avoiding overlapping floor labels.

The y-domain is computed from finite points within the current window, so
zooming autoscale follows the visible data rather than an off-screen peak. The
extrema are gathered in one pass with constant auxiliary memory; the scan does
not allocate a filtered copy or spread the series into function arguments. The
linear axis uses that domain directly. For log mapping we use
`log10(max(1, value))`: zero and negative values map to the floor at one,
never to an undefined logarithm; log ticks include an explicit zero-floor tick
and powers of ten. This intentionally compresses values below one into the
same floor rather than implying meaningful negative log values.

The time grid chooses from a fixed ladder beginning at one minute and
increasing through one week. The bucket-by-key cell budget is 20,000; the
chooser selects a coarse enough bucket to stay within that cap and reports
`coarsened` so the view can disclose lost time resolution. It does not silently
allocate a window-sized dense grid at the smallest interval.

## Interaction: the view

`tokenchartsview.ts` owns SVG and input wiring; window, y-domain, log mapping,
ticks and bucket selection remain in `chartwindow.ts`. A custom window is stored
as start/end instants, not as a preset name, so polls cannot reset a zoom. Wheel
zoom maps the pointer through `tsForX`, then `zoomAbout`; pointer capture keeps
a drag continuous outside the plot, and `panBy` clamps it to the series extent.
Marks select the lifecycle comparison and fit their snapped split window.

The view's default token counter is `total`; cache reads are a selectable
measure but are not the default. Each chart/table labels its counter because
per-item's numerator follows that selection and no longer necessarily matches
the all-token feature bars. A shared bucket chooser caps bucket-by-key work;
coarsening, truncation and excluded samples are surfaced next to trends. The
four trend series share the chart window: per-completed-item tokens, completions
per day, median completion latency and average tokens per agent pane. The
lifecycle denominator is taken directly from done-in-window audit transitions,
never from the board's current done status.
