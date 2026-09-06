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

**That flag means TRANSIENT, and the projection must not oversell it.** Slice
B's own `fp_partial` section above is the contract: only a failure that can
clear on the next bucket sets it, because only such a failure produces the
false-mark *pair* the flag exists to explain. A **stable** unreadable answer —
a directory where a file belongs — hashes identically every bucket, cannot
flip, and is deliberately never flagged. So an UNSET flag is not proof that
every surface was read either, and the view's wording says "could not be read
this round" rather than the narrower "hit a cap" it carried before slice B
settled this (`c3ae9819`, which landed after slice C forked and reached it
only at the rebase onto main — the re-read that caught it is the one every
sibling slice owes its own prose).

`beforeAfter` is `null` below `k` buckets on a side — **never `0`**. A mark
two buckets after the series began has no "before", and printing `0` there
says the fleet spent nothing for an hour, which is the opposite of "we cannot
say". `k` travels on every row, because `k` is the scope of the claim.

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

A hue follows the **block**, from a caller-supplied stable order (the group's
roster), never from the windowed data's own rank — a filter that changes which
series are on screen must not repaint the survivors. A **ninth** block takes
the neutral ramp rather than recycling slot 0: a repeated hue is a false claim
that two blocks are one. Nothing is merged away to avoid that, because this is
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

`scorecardColumns` ships here as the tested projection; slice D renders those
columns beside the bars. Its fail-verdict vocabulary is ENUMERATED rather than
"anything that is not a pass", so a verdict this build has not heard of lands
in neither column instead of silently inflating the fail rate.
