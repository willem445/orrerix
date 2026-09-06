# Orchestration evals — the per-PR scorecard

**Scope.** This note is the specification for `scripts/orch-scorecard.cjs`: what
each counter counts, written as the audit/transcript **shape** it reads, so that two
readers derive the same number from the same rows. It covers the narrow slice #2011
B1 needs — benchmarking the engine-owned review driver (#1778) by measuring, per pull
request, how much orchestrator attention and how many tokens the review loop cost,
and the orchestrator's **share** of those tokens.

It is deliberately not the whole of #2011. The axes table, the three eval classes and
the regression gate live in the plan on that issue; this note carries only what the
script implements, plus the two things a specification owes a reader who did not
write it: the attribution rules **with their gaps named**, and a section on what the
number cannot say.

**Retirement.** `scripts/orch-scorecard.cjs` is **deleted** in #2011 S3, once an
engine module (`crates/loomux-engine`, surfaced as the `group_metrics` MCP tool)
reproduces this output byte-for-byte on the fixture corpus. Until then the script is
the only reader and this note is its spec: a counter changes here first. When the
script goes, this note stays and re-points at the engine module — the definitions
below are the durable half.

---

## 1. Inputs

Four files, all of which the app already writes. Nothing here is a new row, a new
field, or a new gate.

| Input | What it gives | Where |
| --- | --- | --- |
| `audit.jsonl` (+ rotated `audit.N.jsonl`) | every row below | `<group>/` |
| `usage.json` | tokens and cost per CLI session, with `agent_id` | `<group>/` |
| `agents.json` | `id → role, block, session, task` | `<group>/` |
| the orchestrator's CLI transcript | per-turn token usage with a timestamp | `~/.claude/projects/<slug>/<session>.jsonl` |
| a DELEGATE's CLI transcript | the tokens a zero `usage.json` row lost (#2167, §4.6) | the same tree, located by `--claude-projects` |
| `--pr-meta` (optional) | `merged_at`, `build`, `issue` per PR | supplied by the caller from `gh` |

**Why the transcript is required for the orchestrator's tokens and `usage.json` is
not enough.** `usage.json` is a durable *snapshot* store keyed by CLI session:
`group-cost-tracking.md` states the reason — "a resumed session updates one row
instead of double-counting, since the transcript is cumulative". A row therefore
carries one lifetime total per session and there is no time series in it, so no
window can be cut from it. The orchestrator's session spans the whole benchmark
(both the hand-routed and the driven rounds are one lineage), so its `usage.json`
row is a single number covering every PR at once. The transcript is the only
per-turn record.

**What the script reads from ANY transcript, and nothing else:** `timestamp` and
`message.usage`. Every transcript it opens — the orchestrator's here, and a
delegate's for the §4.6 backfill — goes through the one reader, which projects each
line to those two fields at parse time and drops the rest before anything is
retained, so no transcript prose is ever held in memory, printed, or written to a
fixture. What coverage publishes about a backfilled row is its PATH and its token
totals, never a byte of its content. The tests run against synthetic transcripts
written by hand — six lines for the orchestrator, three for the delegate.

---

## 2. Windows

Two windows per PR, because two different questions are being asked and they do not
have the same answer.

| Window | Start | End | Used for |
| --- | --- | --- | --- |
| **loop** | the first `rd-*` row with `detail.pr == N` | the last such row | `span_h`, loop notices |
| **pr** | the first audit row **naming** the PR (§3) | `merged_at`, else the last `rd-*` row, else the last naming row | wakes, orchestrator tokens |

The **driver counters and the review rounds sit in neither** — they are matched
structurally on `detail.pr` and counted wherever the row lands, which §4.3, §4.4 and
§5 all state and which `rows_counted_outside_pr_window` reports on. Only the four
counters named in the table above are window-gated.

Both take a **+10 minute tail** for membership tests (`--tail-min`, default 10) —
the rule #1778's S5 table used, and for the same reason: a merge or CI notice the
orchestrator reads after the driver has stopped writing still belongs to that loop.
`span_h` is the **untailed** span, so it is the honest duration of the thing measured.

A hand-routed PR has **no loop window**; its loop counters are zero and its
`span_h` is `null`. That asymmetry is the point — every other counter in this note
reads a row that predates the driver, which is what makes a before/after comparison
possible at all.

The `end_source` field says which arm produced the end, and it matters: without
`merged_at` the last-naming-row fallback is **days late**, because a merged PR keeps
being cited in the log. Measured on this group's own log, every one of the eleven
benchmark PRs was still being named 1.9 to 90 hours after it merged. Always pass
`--pr-meta` for a benchmark; the fallback exists so the script still answers on a
PR nobody has merge data for, and it declares itself when it fires.

---

## 3. "Names the PR"

A row names PR *N* when either:

1. `detail.pr === N` — structural, and what every `rd-*` and `review-verdict` row
   carries; or
2. the serialized `detail` matches `/#N(?![0-9])/` — the token join, with a
   trailing-digit guard so `#175` cannot match inside `#1751`.

Issues and pull requests share one number space on GitHub, so `#N` is unambiguous
once *N* is known to be a PR. The token join is heuristic **H1** (§6).

---

## 4. Counters

Every counter below is defined by the rows it reads. A counter with no matching row
reads zero, never absent — a table cell is never blank for a counter that simply did
not fire.

### 4.1 Orchestrator wakes

A **wake** is a `prompt` row whose `detail.to` is an agent whose `agents.json` role
is `orchestrator`. One paste is one wake. Its **kind** is decided by the leading
shape of `detail.text`, first match wins, in this order:

| Kind | Shape |
| --- | --- |
| `delegate-progress` | `^\[orrerix\] \S+ reports progress\b` |
| `delegate-done` | `^\[orrerix\] \S+ reports done\b` |
| `reviewer-report` | `^\[orrerix\] \S+ reports (approved\|request_changes)\b` |
| `delegate-blocked` | `^\[orrerix\] (\S+ reports blocked\b\|message from \S)` |
| `verdict-notice` | `^\[orrerix\] \S+ \([^)]+\) recorded verdict\b` |
| `system-notice` | anything else beginning `[orrerix]` |
| `other` | no `[orrerix]` prefix — in practice a human typing into the pane |

The order is the tie-break, and both ties are load-bearing: a reviewer's
`reports approved` is a **reviewer report**, not a generic delegate report, and a
verdict echo is a **verdict notice**, not a system notice. `system-notice` and `other` are defined
by exclusion on purpose — a new orrerix notice shape lands in `system-notice` without
silently joining a report class, and a new human phrasing lands in `other`.

A wake counts toward a PR when it names that PR (§3) **and** falls inside the PR
window.

The same population carries a second histogram, the **orchestrator prompt classes**
(S0, plan-2504 §3): the brief's classes instead of the wake ones, because the two
answer different questions — the wake classes say what WOKE the orchestrator, these
say what the review drive COST its pane. First match wins:

| Class | Shape |
| --- | --- |
| `driver-gate-satisfied` | `^\[orrerix\] review drive PR #\d+: GATE SATISFIED\b` |
| `driver-held` | `^\[orrerix\] review drive PR #\d+: HELD\b` |
| `driver-cancelled` | `^\[orrerix\] review drive PR #\d+: CANCELLED\b` |
| `run-completed` | `^\[orrerix\] run \d+: completed\b` |
| `delegate-report` | `^\[orrerix\] \S+ reports (progress\|done\|approved\|request_changes\|blocked)\b` — the wake classes' report shapes, pooled: for the S0 question a reviewer's pass and a worker's done are both one delegation turn |
| `other` | everything else — a human typing, a system notice. Reported, never dropped: an unrecognised shape surfacing here is how the next class gets added |

Per PR, a class counts the same population the wakes do (names the PR, inside the
PR window), on the card as `orchestrator.prompt_classes`. Per FILE, `group.files[]`
carries `orch_prompt_total` + `orch_prompt_classes` over EVERY prompt row to an
orchestrator pane — the whole-file figure the plan's §1 totals ("prompt rows into
the orchestrator pane | 126") are checked against, and a different population from
the per-PR one: a run-completed check names no PR and lands in the file census
only.

### 4.2 Loop notices

Two numbers, and the difference between them is a correction to #1778's S5 table.

- **`loop_notices`** — an `[orrerix]`-prefixed wake naming the PR inside the **loop**
  window. This is orchestrator attention the loop actually cost.
- **`loop_notices_any_pane_s5`** — the same filter with **two** of those tests
  dropped, not one: neither the "delivered to an orchestrator pane" test nor the PR
  window applies (`scorePr` counts it above the `isWake` branch, so only the loop
  window and the `[orrerix]` prefix gate it). This is what S5's "orch notices" column
  counted, reproduced so that table stays checkable. The second dropped test changes
  no figure on the eleven benchmark PRs — every such prompt already falls inside the
  PR window — but the two counters differ by more than the pane test in principle.

They differ by 1–2 per PR on all six driven PRs, and the difference is entirely the
driver's own resume prompts typed into a **worker** pane — which is precisely the
traffic the driver moved *off* the orchestrator. The S5 column therefore overstates
orchestrator involvement, and it overstates it in the direction that makes the driver
look worse. See §7.

### 4.3 Review rounds

`review-verdict` rows with `detail.pr == N`, bucketed by `detail.block` then
`detail.verdict` (lower-cased). `rounds` is the total. Matched **structurally and
counted wherever the row lands**, like the driver counters above — a verdict recorded
after the merge is still a round the PR cost. This counter is driver-blind: it reads
the same row for a hand-routed PR and a driven one.

### 4.4 Driver counters

`rd-*` rows with `detail.pr == N`, one bucket per action. These are matched
**structurally and counted wherever the row lands** — NOT gated on either window, so a
re-drive after the merge still counts; §5's `rows_counted_outside_pr_window` is what
reports the spill. (The loop window is derived FROM these rows, so in practice every
one of them is inside it; the `--pr-meta` merge time is what a late row can fall past.)

| Field | Row | Notes |
| --- | --- | --- |
| `drives` | `rd-started` | more than one means a re-drive after a cancel |
| `lane_spawns` | `rd-lane-spawned` | |
| `hand_backs` | `rd-handback` | |
| `refused` + `refused_by_reason` | `rd-refused` | keyed on `detail.reason` |
| `refused_cap` | `rd-refused` | the slice whose row says `cap: true` — the live-delegate-cap shape; other refusals (`already-driven`, `worker-unresumable`) stay out of it. `starved_ms_max` / `starved_ms_sum` ride the same rows; the max is `null` where no refusal measured a starvation, because "nothing measured" and "measured 0 ms" differ |
| `held` + `held_by_reason` | `rd-held` | keyed on `detail.reason` |
| `cancelled`, `consumed`, `satisfied`, `ci_green`, `resumed`, `pruned` | the matching `rd-*` | |
| `lane_scope` + `lane_scope_triples` | `rd-lane-spawned` | the whole-diff / delta / body-only histogram, DEDUPED on `(pr, block, round)` — a replaced lane pane (the same block and round spawned again on a new agent after a refusal) is ONE review round, not two, so the triple counts once, under the FIRST row's scope. `lane_spawns` keeps counting rows, so rows − triples is exactly the replaced panes. An unrecognised `detail.scope` is its own `other` bucket, never folded into a known one |
| `handback_ratio` | `rd-handback`, `rd-worker-released` | `hand_backs / workers_released`; `null`, never 0, where nothing was released — a zero would read as "no hand-backs" when the fact is "no worker pane was ever released", the exact figure plan-2504 §1's finding (b) turns on |
| `kills_by_initiator` | `agent-kill` | the row's own `detail.initiator` (`driver-release`, `orchestrator`), keyed as written. The row carries no `pr`, so a kill reaches a card only through the agent→PR attribution (§5), and only when that attribution is a SINGLE PR; every other kill is counted once in the group's `driver_totals`, which reconciles the two: `kills_total = kills_in_cards + kills_not_in_cards`, with the agents the cards could not take named in `kills_agents_not_in_cards` |

Any `rd-*` action not in that list is still counted, under its own action name, so a
new row in the driver's vocabulary (`review-driver.md` §5.4) cannot go missing —
it appears in the card rather than being dropped.

The per-PR values pool into a top-level `driver_totals` block (`--format table`
renders it as a total row under the per-PR one). It is the shape the S0 baseline
table quotes, and it carries the reconciliation the cards cannot: the kill totals
count rows, the cards count attributed single-PR kills, and both operands are
reported. It exists because the plan-2504 §1 hand tally (issue #2811, comment
5562317136) is quoted as session totals, and a table that cannot add up to the
figure it is checked against is not a table — it is a quiz.

#### The §1 control, reconciled (measured 2026-09-06, head 4638ae3f)

The baseline run (`--all --from 1788706648042 --cut 1788729593887` over both
audit generations, 5,024 rows; posted on #2812 as "beta9 baseline (S0)") against
the plan-2504 §1 hand tally, figure by figure:

| Figure | §1 (hand) | S0 (mechanical) | Verdict |
| --- | --- | --- | --- |
| drives | 26 (15 PRs, 9 re-drives) | 26 / 15 / 9 | reproduces |
| held | 3 | 3 | reproduces |
| refused (cap) + max starved_ms | 22 / 689,089 | 22 / 689,089 | reproduces, all three quoted per-PR maxima included |
| worker-released / round-grace | 5 / 0 | 5 / 0 | reproduces |
| orchestrator prompt rows / driver G+H+C | 126 / 29 | 126 / 29 | reproduces |
| satisfied | 20 | 23 rows | unit: rows vs drive outcomes — six PRs emitted >1 `rd-satisfied` because a satisfied drive was re-driven (the §1(d) re-drives); 26 = 20 satisfied + 3 held + 3 cancelled holds as drive outcomes |
| hand-backs | 20 | 21 rows | unit: one hand-back was written twice (#3038's w-2460, after the orchestrator kill and again after the recovery — §1(c)); 21 − 1 double-written event = 20 |
| lane-released | 67 | 66 at the stated cut, 67 uncut | §1's grep ran on a log ~15 min past its own stated last row — the window, not the count, moved |
| lane scope W/D/B | 30 / 19 / 7 (56) | 42 / 16 / 6 (64 triples; 68 raw rows) | unreconstructable: 56 is below the raw row count, and no dedup variant on these rows yields 30/19/7 — a finding about the hand tally's unit or population |
| agent-kill dr/orch | 65 / 33 | 71 / 45 | §1's stated instrument (ripgrep on `on_behalf_of`) cannot have produced it: 0 of the window's 116 `agent-kill` rows carry that field. The decomposition is per generation — 65/33 is exactly `audit.jsonl`'s census; `audit.1.jsonl` holds the other 6 dr / 12 orch |

Two residuals the reconciliation surfaced, stated rather than buried:

- **16 of the 116 kills belong to no review drive** (`plan-2386`/`2387`/`2446`
  panes and 13 workers the attribution cannot tie to one PR — the workers whose
  report the drive never consumed, §1(b)'s panes). A "session drive cost"
  reading must quote the reconciled pair (`kills_in_cards` 100 +
  `kills_not_in_cards` 16), never the raw 116.
- **The dedup key trusts `round`.** `(pr, block, round)` collapses a spawn row
  with no `round` field into its block's single triple, which would undercount
  historical rounds. Measured across BOTH audit generations (318 spawn rows):
  0 lack `round`; a future generation that omits it would undercount, and
  `lane_scope_rows − lane_scope_triples` is where such a collapse would show.

### 4.5 Orchestrator tokens

The sum of `input`, `cache_read`, `cache_creation` and `output` over every deduped
transcript **turn** whose `timestamp` falls inside the PR window.

**Dedup is not optional.** A Claude Code transcript writes an `assistant` line per
content block, so one API response appears two or more times carrying the same
`message.id` and the same usage object. The ratio on the orchestrator's own
transcript is close to 2:1 (30,842 assistant lines to 15,087 distinct message ids,
measured 2026-09-02) — summing the lines would report roughly double. That file is
live and grows, so re-derive the ratio rather than quoting this one. The rule: one turn per `message.id`, keeping the
occurrence with the **largest** `output_tokens`, because an earlier line was written
mid-stream and under-reports it. A line with no `message.id` is counted once and
declared in coverage as `usage_rows_without_id`.

Two figures are reported:

- **`tokens_window`** — the raw window sum. The orchestrator serves several PRs at
  once, so this is an **upper bound** for any one of them.
- **`tokens_attributed`** — the same sum scaled by this PR's share of the
  orchestrator wakes in the same window (`pr_wakes / window_wakes`). Heuristic **H5**.

Neither is "the tokens this PR cost". The raw figure over-counts by whatever else was
in flight; the apportioned figure assumes a wake costs the same whichever PR it is
about. Both are reported so a reader can see the spread, and `wake_share` carries
both operands so the apportionment is checkable rather than trusted.

### 4.6 Delegate tokens

A `usage.json` row is keyed by **CLI session**, not by agent. `group-cost-tracking.md`
says why ("keyed by CLI session id … a resumed session updates one row instead of
double-counting"), and the consequence has to be handled here: when a pane's session is
carried to a new agent id, that one row's `agent_id` names only the **last** occupant,
and every earlier agent on the session has no row of its own. Joining on `agent_id`
would report an agent that demonstrably spent tokens as having spent zero while
crediting its successor with the whole lineage — and on this group's store that is not
a corner case. On the `usage.json` / `agents.json` read recorded in #2114's coverage
block (measured 2026-09-03): **514 distinct sessions in `agents.json` are shared by
more than one agent id — the same 514 usage rows — carrying 31.2 G of the store's
44.1 G tokens**, and the same read counts 1210 distinct sessions in all. The store is
LIVE: `usage.json` held 1356 rows at the first measurement and 1362 an hour later,
with 514 shared both times, so no figure here is a standing ratio — quote the shared
**count** and re-derive the denominators from a fresh read. §10 refers here for these
figures instead of restating them.

So the row is looked up by the agent's **session** and split evenly across every agent
that occupied it (heuristic **H8**), then weighted by `1/k` where the agent is
attributed to *k* PRs (**H4**). The session split is self-correcting in the common
case — a pane reused by a worker's own successive agent ids has every occupant
attributed to the same PR, so the halves re-sum to the whole row — and gives a
fraction rather than all-or-nothing where only some occupants are attributed. An
`agent_id` lookup remains as a **fallback** for a row with no session key, and
`usage_key` per delegate says which index answered.

Each delegate reports `tokens` (the whole row its session carries) beside
`tokens_credited` (what this PR actually got after both weights), so the delegate
total is checkable by hand. `has_usage_row: false` means neither index had anything
for that agent: it contributes **0**, which makes the delegate side an
under-count and the orchestrator share correspondingly an over-estimate. The coverage
block is where a reader sees how often that happens.

An agent whose `agents.json` role is `orchestrator` is **excluded** — its spend is
already in §4.5 and counting it here would double it.

`usage.json` is cumulative per session and cannot be windowed (§1), so a delegate
reports its whole life against the PR it is attributed to. That is heuristic **H6**,
and it is a good approximation here only because delegates are spawned per task: a
worker or reviewer pane in this group serves one PR and is killed.

**A zero row is repaired from the transcript before any of this runs (#2167).**
Between 2026-08-30 and the fix, loomux resolved a delegate's CLI from its
capability class's DEFAULT block rather than from its own block, so on this
group's two-blocks-per-class roster every Claude delegate was read as an
OpenCode one: its transcript arm never ran and its row landed as
`statusline`/`none` with four zero counters. `group-cost-tracking.md` carries
the mechanism and the fix. The consequence HERE is that every delegate-token and
orchestrator-share figure this script has ever produced was an under-count on
the delegate side and correspondingly an over-estimate of the orchestrator's
share — the tables posted on #2011 included.

So a row whose four counters are all zero, and whose session has a transcript at
`<claude-projects>/*/<session>.jsonl`, is summed from that transcript before the
usage index is built. The fold is the SAME message-id dedup §4.5 puts the
orchestrator's transcript through, so the two sides of the share ratio cannot be
computed two different ways. `--no-backfill` reproduces the old, wrong reading
for comparison. The backfill is UNPRICED and does not honour `--cut` (heuristic
**H9**), and `coverage.usage_rows_backfilled_from_transcript` reports how many
rows were considered, how many were repaired and from which files, and which
zero rows were skipped — split three ways, because only ONE of them says the
store lost a session: no transcript on disk, a transcript that folds to nothing,
and a row keyed `agent:<id>` rather than by a CLI session (`UsageSnapshot::key`
produces that for a pane that never got one, so no transcript could ever match).
That last case was 86% of the skips on this group's own store. All four outcomes
reconcile against the count considered, or `reconcileBackfill` throws rather
than publish a coverage figure counted at the match site instead of the verified
one — a guard pinned directly, since no pipeline branch can produce a record
that fails it.

The candidate test is the four counters, never the `source` label: a zero row
can carry `none`, `statusline`, or even `transcript`, and a row with tokens is
never rewritten from disk however its source reads.

### 4.7 Orchestrator share

`orch / (orch + delegates)`, reported twice — once from `tokens_window`
(`orchestrator_pct_raw`) and once from `tokens_attributed`
(`orchestrator_pct_attributed`). Both are `null` when the denominator is zero, never
`0` — "no data" and "the orchestrator spent nothing" are different facts.

### 4.8 The CLI axis, and the per-lane columns

Which **CLI** produced a delegate's tokens is not a field loomux writes anywhere.
It is **read off** the `source` label of the `usage.json` row carrying those
tokens, because that label names the per-CLI record the collector folded — a
Claude Code transcript, an OpenCode session DB, a pi session file, a codex
rollout ([group-cost-tracking.md](group-cost-tracking.md), the per-CLI sections).
The map is therefore the inverse of the collector's own arm selection, not a
guess about which CLI a block runs:

| `source` | `cli` |
| --- | --- |
| `transcript` | `claude` |
| `transcript-backfill` | `claude` — this script's own label for a row it rebuilt by folding a Claude transcript off disk (§4.6) |
| `pi-transcript` | `pi` |
| `session-db` | `opencode` |
| `codex-transcript` | `codex` |
| `statusline`, `none`, absent, anything else | `unknown` |

**A ladder, and `unknown` is the last outcome, never a guess.**

0. `spawn-row-session-conflict` — first, and **only** where the session's own
   occupants did not all run one CLI. A `usage.json` row is keyed by CLI
   session, so its `source` answers for **every** agent that occupied that
   session — and this group's store has panes recycled onto a different block
   running a different CLI: sessions `358b100f…` and `e81c5d8a…` are each
   shared by a `worker-adv` (claude) and a `worker-std` (opencode) agent
   (audit generations 1+2, read 2026-09-06). One label cannot be right for two
   CLIs, so the **per-agent** `agent-spawn` record wins there. It is its own
   rung rather than a silent correction, and
   `coverage.cli_axis.sessions_with_conflicting_clis` names every session it
   fired on with the split — the defect is SURFACED here, not repaired: the
   structural fix is H10's own, a `cli` on `UsageSnapshot`.
1. `usage-source` — the map above, where it resolves.
2. `spawn-row` — where it does not (a `statusline` scrape says a CLI printed a
   dollar figure, never which one), the `cli` on that agent's `agent-spawn` row.
   Verified rather than assumed, and **the load-bearing fact is the two sites,
   not any count**: there are exactly two `agent-spawn` `json!` sites in
   `src-tauri/src/orchestration/mod.rs`, and the **delegate** one carries `cli`
   and `block` while the **orchestrator** one carries neither. Every row without
   `cli` is therefore an orchestrator spawn, and an orchestrator is excluded
   from the delegate side anyway (§4.6), so the rung covers every agent it is
   asked about. A ratio is only an illustration and decays by the hour on a live
   store, so it is dated rather than quoted as standing: 628 of 637 spawn rows
   carried `cli` when this group was read on 2026-09-06, and 629 of 638 on a
   re-read the same day. `coverage.cli_axis.spawn_rows_with_cli` /
   `_without_cli` re-derives it on every run — read that, do not cite these.
   A third site that omitted `cli` would land in `unknown` — reported, not
   silently filled in.
3. `unknown`, with `cli_via: null`. The block's **declared** CLI is deliberately
   not consulted: a pane can be recycled onto a block whose roster line has since
   changed, and the whole point of the axis is to measure what actually ran.

`cli_via` says which rung answered, per delegate. Two rows disagreeing under one
key resolve to `mixed` rather than to whichever was folded last — the session
index sees one row per key, but the `agent_id` fallback index can see several.

**A residual the ladder does not close**, stated because a reader would
otherwise assume it does: a cross-CLI session whose occupants have **no**
`agent-spawn` `cli` (a spawn site that never wrote one) still takes the row's
single `source` label for every occupant, and nothing distinguishes that from a
correct reading. It cannot happen on today's two spawn sites — the delegate one
always writes `cli` — which is what bounds it, not anything in this script.

This is heuristic **H10**, and `coverage.cli_axis` is its population control:
`delegate_slots`, `by_rung`, `by_cli` and `unknown`, counted once per delegate
**on a card** — the site where a cli is actually used — rather than once per
usage row scanned, which would certify coverage the table never received.

**Per PR, `delegates.by_block_cli`** buckets the credited tokens by
`<block>/<cli>`, keys read off the rows and never off a roster. Keying on the
block alone would fold a PR whose `worker-std` panes ran two different CLIs into
one bucket, which is exactly the case a switch produces.

**Per lane** (`review.lanes[<block>]`), beside §4.3's verdict buckets:

- `rounds_to_pass` — the 1-based position of the **first** `pass` in that
  block's `pass`/`fail` sequence. `null` where the block never passed — never 0
  and never the round count, because a lane still in review has no answer yet
  and the round count would read as a lane that passed on its last round. A
  later `fail` (a re-review on a new head) does not lower it, and a re-`pass` is
  not a second answer. This is a question about the **sequence**, which §4.3's
  bucket has thrown away.
- `fail_rate` — `fail / (pass + fail)` over the whole sequence, so those later
  rounds do count here. `null` when the lane recorded neither: `0` would claim a
  clean lane where there is no lane at all.
- `verdicts_other` — anything that is neither `pass` nor `fail` (the live
  vocabulary is exactly those two: 0 `escalate` rows across 451
  `review-verdict` rows in this group's two audit generations, read
  2026-09-06), excluded from both figures rather than allowed to shift an index
  silently. **The consequence, stated because it biases a compared column:** a
  lane that escalates and then passes reports `rounds_to_pass: 1` beside
  `rounds: 2`, so the first lane to use `escalate` — a first-class verdict in
  `review_verdict`'s own enum — makes this column read low. Pinned by
  `lanes: an escalate BEFORE a pass is not counted as a round to pass`, so the
  disclosure cannot go stale with nothing red to say so.

**Per PR, `wall_clock_h`** is the PR window's own untailed span (`windows.pr.span_h`),
lifted to the top of the card because it is one of the compared columns. `null`,
never 0, where no audit row names the PR.

### 4.9 The CLI comparison table (`--format cli-table`)

**Medians, never totals.** The two windows hold different PRs doing different
work, so a total is a statement about the task mix rather than about the CLI.
Every cell is a median with an inter-quartile range and an `n`, and a cell below
`MEDIAN_MIN_N` (3) reads **`null`** — `n` is still reported at every size,
including 0, so a null cell is legible as "not enough data" rather than as a
missing measurement. Quartiles use the exclusive-median (Tukey hinge)
convention: on an odd-length sample the halves exclude the middle element.

**Selection**, and why each bound is there:

| bound | rule |
| --- | --- |
| merged | the PR must have a `merged_at` in `--pr-meta` — an open PR has no wall clock and its lanes have not finished |
| one cli | every delegate of a compared block must resolve to the **same** cli, and it must not be `unknown` or `mixed`; a PR whose `worker-std` panes were half opencode and half pi measures the switch rather than either side of it |
| both lanes agree | `worker-std` and `rev-std` must resolve to the same cli |
| side | `merged_at` against `--split-at`, an instant the CALLER passes — for the #2817 roster switch, commit `93d51cc9`, 2026-09-06T10:36:55Z |

Every refusal is listed under `excluded` with the bound that refused it — the
selection is stated, never silent, and `per_pr` plus `excluded` is every PR the
run scored.

**The side and the cli are resolved independently, and cross-checked only
against an expectation the CALLER declares** — `--sides <before>:<after>`, e.g.
`opencode:pi`. A PR whose lanes resolve to the other side's cli then appears
under `side_cli_disagreements` rather than being reconciled away: it means the
split instant is wrong or a lane was hand-run, and that is the instrument
telling on itself.

**Without `--sides` there is no expectation and no check**, and
`side_cli_disagreements` is **`null`** — not `[]`. "Nobody declared what to
expect" and "an expectation was checked and nothing disagreed" are different
facts; rendering them the same would report a clean cross-check that never ran,
so the table says in words that no cross-check was run.

**Nothing in the script knows which cli ran on which side.** Rows are one per
`(cli, window)` pair any selected PR produced; the window LABELS come from
`--split-label` (defaulting to the split instant's own ISO form, so a label
cannot describe a different split from the instant beside it); the expected
pair, when there is one, comes from `--sides`. An earlier revision of this
section claimed that while `cliTable` hardcoded `pre-2817 ? 'opencode' : 'pi'`,
which made the cross-check fire on **every** selected PR under any other
`--split-at` — a stale roster wearing the costume of a finding. The claim now
holds of the whole function rather than of row construction alone.

**Columns**: `worker-std` credited tokens, `rev-std` credited tokens, `rev-std`
`rounds_to_pass`, `rev-std` `fail_rate`, `wall_clock_h`, and — the **control** —
`rev-final` `rounds_to_pass`. `rev-final` is Claude on both sides of the split,
so a column that moves there is measuring something other than the
worker/reviewer CLI, and a control that moves as much as the treatment columns
is the table refuting itself.

**The confounder block is printed by the script**, under the table, so a table
pasted into a comment cannot arrive without the reasons not to over-read it:
the effective thinking level (#2938 — reported as `unknown` until that lands,
never guessed at a value), the task mix, the driver-waste changes that land on
the pre-switch side (#2501, #2507, #2508, #2509) and their measurement (#2812),
and the cumulative-usage bounds H6/H8 carry equally on both sides.

---

## 5. Attribution: agent → PR

Two tiers, and which tier carried an agent is reported per agent, because that
difference is exactly #2011 B2's scope.

**Structural.** The row itself carries both the agent and the PR:

- `rd-lane-spawned` and `rd-handback` — `detail.agent` + `detail.pr`;
- `review-verdict` — `actor` + `detail.pr`.

**Text.** An `agent-spawn` row **inside the PR window** whose serialized `detail`
(brief, name, branch) names `#N`. Heuristics **H2** and **H3**.

Structural wins: once an agent has a structural attribution, no text row can add a
PR to it. The window bound on the text tier is what stops a later brief citing an old
PR from claiming its tokens — without it, this very slice's own worker brief (which
names all eleven benchmark PRs) would be attributed to all eleven.

**Known gaps, all reported in the coverage block rather than swallowed:**

- `agents_unattributed_spawned_in_window` — an agent spawned inside some PR's window
  that no row ties to any PR. Its tokens are counted nowhere.
- `agents_split_across_prs` — an agent attributed to more than one PR, with the list.
  Each gets `1/k` (**H4**), which is a guess about how the agent's time divided.
- `rows_unclassified_in_window` — rows naming the PR inside its window that no
  counter consumed, grouped by action. This is the "did the scorecard see everything"
  check; it is expected to be non-empty (an `agent-spawn` is named but not counted).
- `has_usage_row: false` on a delegate — neither the session index nor the `agent_id`
  fallback had a row for it, so it contributes 0 and the delegate side is an
  under-count. On the eleven benchmark PRs it is **0 of 94** delegate entries: every
  one resolves through the session index. Under the `agent_id` join this replaced it
  was **28 of 94**, which is how that defect was found — so read a non-zero here as a
  reason to distrust the delegate totals, not as noise.
- `usage_sessions_shared_by_more_than_one_agent` — how many sessions the H8 split had
  to divide, and `usage_rows_unusable` — rows with neither a session key nor an
  `agent_id`, which are skipped.
- `usage_rows_backfilled_from_transcript` — the #2167 repair (§4.6):
  `zero_rows_considered`, `rows` repaired with the file and token breakdown of
  each under `from`, and three skip buckets —
  `zero_rows_without_a_transcript`, `zero_rows_whose_transcript_summed_to_zero`
  and `zero_rows_with_no_session_id`. A non-zero `rows` says the store was
  written by a build carrying the collector defect. Of the skips, only the FIRST
  bucket says a figure is an under-count that cannot be recovered from disk: the
  second says the file is there and holds no billable turn, and the third says
  the row was never keyed by a session, so nothing was ever lost.
- `rows_counted_outside_pr_window` — the reverse direction. A `review-verdict` or
  `rd-*` row is matched **structurally** on `detail.pr`, so it counts wherever it
  occurs: a late verdict or a re-drive after the merge is still a round the PR cost,
  and excluding it would silently under-count. That makes those two counters wider
  than the PR window, so the number of rows that fell outside it is reported. It is
  **0 on all eleven benchmark PRs**, so the widening is latent there rather than live.

---

## 6. The heuristics list

The script emits this list in every run, under `coverage.heuristics`, and it **is**
#2011 B2's scope — one row per place where a structural field would replace a guess.

| id | The guess | The structural fix |
| --- | --- | --- |
| H1 | A PR is joined to a row by the `#N` token wherever `detail.pr` is absent. | A `pr` field on `prompt` / `delivery-queued` rows. |
| H2 | An agent is joined to a PR by the `#N` token in its `agent-spawn` detail. | A `pr` field on `agent-spawn`. |
| H3 | A text-tier join counts only inside the PR window. | Same as H2 — the bound exists only because the token is ambiguous. |
| H4 | An agent attributed to *k* PRs contributes `1/k` of its tokens to each, and *k* counts only the PRs in **this run's selection** — run one PR alone and its shared agents get full weight, so always run the whole comparison set together. | A `block` and a `pr` on `UsageSnapshot`. |
| H5 | Orchestrator tokens are apportioned by wake share. | Per-turn PR attribution — nothing structural exists (§7). |
| H6 | `usage.json` is cumulative, so a delegate's whole life counts against its PR. | A windowed usage series, or accepting the approximation. |
| H7 | The PR window ends at `merged_at` passed in by the caller. | A loomux row for a human merge (#388). |
| H8 | A `usage.json` row is keyed by CLI **session**; a session carried to a new agent id names only its last occupant, so the row is split evenly across every agent that occupied it. | An `agent_id` (or `block` + `pr`) on **every** `UsageSnapshot`, not just the latest — the same missing field as H4. |
| H9 | A zero row backfilled from its transcript (§4.6) is **unpriced** — it keeps whatever `cost_usd` the collector recorded, so its tokens are right and its dollars are not — and does not honour `--cut`, because `--cut` cannot rewind an ordinary `usage.json` row either. | The collector recording the row correctly, which #2167 does: after it, `rows` reads 0 on any store written by a fixed build. |
| H10 | A delegate's CLI is read off the `source` label of the `usage.json` row carrying its tokens (§4.8); a row whose source names no CLI (`statusline`, `none`) falls back to the `cli` on that agent's `agent-spawn` row, and to `unknown` — reported, never filled in from the block's declared CLI — when neither answers. That label is per **session**, so where one session's occupants ran different CLIs the per-agent spawn row is preferred instead and the session is named in coverage. | A `cli` field on `UsageSnapshot`, and one on the orchestrator `agent-spawn` site, which unlike the delegate site carries none — t-664's family, the same missing-field fix as H4/H8. |

A run that adds a heuristic adds a row here in the same commit. The test suite
asserts every emitted heuristic has an id, a statement and a named fix, so an
undocumented guess fails rather than shipping quietly.

---

## 7. What this cannot say

The counters above measure **how much machinery a PR consumed**. None of them
measures whether the machinery was right.

- **It does not say whether a finding was real.** A review round is a
  `review-verdict` row. A `fail` that caught a genuine defect and a `fail` on a
  mis-stated body figure are the same row, and this scorecard counts them
  identically. Nothing here distinguishes them; the row that would
  (`finding-dispositioned`) does not exist and waits on findings being structured
  first (#995). Reading a drop in rounds as a quality improvement is unsupported by
  anything in this file.
- **It does not attribute an orchestrator turn to a PR.** §4.5 gives an upper bound
  and a wake-share apportionment, and both are declared heuristics. An orchestrator
  turn that read three PRs' notices and answered one of them is one turn; nothing in
  the transcript or the audit log says which PR it was about.
- **It does not say whether a round was the worker's fault or the brief's.** Two
  rounds on a PR whose brief was ambiguous and two on a PR whose worker was careless
  read the same.
- **It cannot compare across groups or repos.** Wake counts scale with how chatty a
  delegate's `report` discipline is, and token counts scale with the model and the
  context. A before/after is only meaningful within one group over one repo, dated
  to the workflow and template blobs at each end.
- **A window is not an isolation.** Concurrent PRs share the orchestrator's window;
  `wake_share` is reported so the overlap is visible, not so it is corrected.

---

## 8. Reproducing a historical measurement

The audit log is **live** and grows while it is being read, so a number measured
today is not reproducible tomorrow without a bound. `--cut <ms|iso>` drops every
audit row and every transcript turn after an instant, which is what makes an earlier
figure checkable: the plan's part-1 census names both its cut (`ts_ms`
`1788315192783`) and its row count (6337), and `--cut 1788315192783` reproduces that
row set exactly. `--from <ms|iso>` is the other end — it drops every row BEFORE an
instant, symmetric with `--cut`, so a window with two ends is one command. The
plan-2504 §1 session (issue #2811, comment 5562317136) is
`--from 1788706648042 --cut 1788729593887`. Like `--cut`, `--from` cannot rewind a
cumulative `usage.json` row; both bounds are echoed on the run as
`inputs.from_ms` / `inputs.cut_ms`.

Group-wide totals (`group.files[]`) are reported per audit file rather than pooled,
because a generation boundary is where a rotation happened and pooling two
generations hides it.

**`--cut` bounds the log FORWARD, and that is all it can do.** It cannot recover
a row a **rotation** has discarded. This group keeps two audit generations and
rotates at 8 MB, so there is a **coverage floor** below which a PR is not scored
at all — and it does not appear in a selection's `excluded` list either, because
nothing in the log names it any more. A run is reproducible only while its rows
are still on disk.

Measured, so this is not a caution about a hypothetical: the first posting of
the §4.9 table read **32,943 rows across two generations and selected 20 PRs**;
a rotation at **2026-09-06T17:14Z** discarded the older generation, and the same
command with the same `--cut` then read **16,351 rows and selected 10**. Both
runs were correct about the log they could see; only the floor moved. `--format
cli-table` therefore prints its coverage floor under the table, and two runs of
this script are comparable only after their floors are.

**Which PRs the floor names, and why the obvious test is nearly useless.**
`windows.pr.start_ms` is `namedFirst` — the first row naming the PR that
*survived* the read — so a PR whose rows **straddle** the rotation has a
post-floor start **by construction**, and `start_ms <= ts_first` can only ever
catch the one PR whose first surviving row is the log's oldest. Testing that
alone would contradict the floor's own rationale. So the straddle is decided on
a signal that survives truncation: an `agent-spawn` row is written once, when a
delegate is created, and always before the `rd-*` / `review-verdict` rows that
attribute it to a PR — so a PR credited with a delegate whose `agent-spawn` row
is **not** in the surviving log has provably lost rows.

`coverage_floor.windows_touching_the_floor` reports both, each with its reason,
because a reader must be able to tell one from the other:

| `why` | means |
| --- | --- |
| `spawn-row-missing` | **proven** truncated — a credited delegate has no surviving spawn row, so the counters are a lower bound |
| `window-at-floor` | **possibly** truncated — the window begins at the oldest surviving row, so nothing rules out earlier rows |

A PR matching both is reported under the stronger reason, never twice.

**The residual, stated rather than implied:** a PR that lost rows but kept every
delegate's `agent-spawn` row, and whose window starts after the floor, is
detected by neither — and nothing in a truncated log could detect it, because
the evidence is the part that was deleted. The render says so on a clean run
rather than issuing a clean bill of health.

---

## 9. Output shape

One JSON object. `--format table` renders the per-PR GFM table instead;
`--format both` prints each. `--format cli-table` renders the §4.9 comparison
(and needs `--split-at`; `--split-label` and `--sides` are optional); its own
shape is `{ split_at_ms, split_label, sides_declared, min_n, columns[],
rows: [ { key, cli, side, prs[], cells: { <column>: { n, dropped, median, q1,
q3, iqr, min, max } } } ], per_pr[], excluded[],
side_cli_disagreements[] | null, confounders[] }`.

```
{
  generated_ms, inputs: { audit[], usage, agents, transcript[], pr_meta, tail_min, cut_ms, from_ms,
                          claude_projects, backfill },
  group:    { files: [ { path, rows, span_h, orchestrator_wakes, wakes_by_kind,
                        orch_prompt_total, orch_prompt_classes, agent_kills,
                        agent_kills_by_initiator, ... } ] },
  driver_totals: { drives, satisfied, held, cancelled, refused, refused_cap,
                   starved_ms_max, starved_ms_sum, lane_scope, lane_scope_rows,
                   lane_scope_triples, hand_backs, workers_released, handback_ratio,
                   kills_total, kills_by_initiator, kills_in_cards, kills_not_in_cards,
                   kills_agents_not_in_cards[], orch_prompt_total, orch_prompt_classes },
  prs:      [ {
    pr, build, issue, outcome, merged_at, wall_clock_h,
    windows:      { pr: { start_ms, end_ms, end_source, tail_ms, span_h } | null,
                    loop: { start_ms, end_ms, tail_ms, span_h } | null },
    orchestrator: { wakes_total, wakes_by_kind, prompt_classes, loop_notices, loop_notices_any_pane_s5,
                    wake_share: { pr_wakes, window_wakes, share },
                    tokens_window, tokens_attributed },
    review:       { rounds, by_block: { <block>: { <verdict>: n } },
                    lanes: { <block>: { rounds, pass, fail, verdicts_other,
                                       rounds_to_pass, fail_rate } } },
    driver:       { drives, lane_spawns, hand_backs, refused, refused_cap, held, ...,
                    starved_ms_max, starved_ms_sum, lane_scope, lane_scope_triples,
                    handback_ratio, kills_by_initiator,
                    refused_by_reason, held_by_reason },
    delegates:    { count, tokens,
                    by_block_cli: { "<block>/<cli>": { block, cli, tokens, tokens_credited, count } },
                    agents: [ { agent, block, role, cli, cli_via, tier, weight, pr_weight,
                    session_weight, shared_session_agents, tokens, tokens_credited,
                    usage_key, has_usage_row } ] },
    share:        { orchestrator_pct_raw, orchestrator_pct_attributed },
    rows_classified, rows_counted_outside_pr_window, rows_unclassified_in_window
  } ],
  coverage: { audit_rows_read, audit_parse_errors, rows_classified, transcripts[],
              agents_attributed, agents_unattributed_spawned_in_window,
              agents_split_across_prs, usage_rows_unusable, usage_sessions_indexed,
              usage_sessions_shared_by_more_than_one_agent,
              cli_axis: { delegate_slots, by_rung, by_cli, unknown,
                          spawn_rows_with_cli, spawn_rows_without_cli,
                          sessions_with_conflicting_clis[] },
              usage_rows_backfilled_from_transcript: {
                claude_projects_root, scanned, projects_scanned, transcripts_indexed,
                zero_rows_considered, rows, tokens,
                from: [ { session, agent_id, role, was_source, path, turns, tokens } ],
                zero_rows_without_a_transcript[],
                zero_rows_whose_transcript_summed_to_zero[],
                zero_rows_with_no_session_id[] },
              heuristics[] }
}
```

`rows_classified` is the positive control: it is the count of rows a counter actually
consumed, and a run reporting zero has not measured anything, however many rows it
read.

---

## 10. Testing

`test/orchscorecard.test.ts` over a synthetic corpus in
`test/fixtures/orchscorecard/`: one driven PR, one hand-routed PR with no merge time,
and one PR **named by nothing** as the negative control that makes every "the
mechanism ran" assertion fail-able. The corpus is built so that no two counters share
a value — a fixture whose axes are all one constant cannot tell a working counter
from a broken one.

The transcript fixture is six hand-written lines. Real transcript content is private
and never enters a fixture.

The corpus also carries one instance of each edge the text above promises to handle,
so none of them is a claim with no test behind it: an `rd-*` action this reader has no
bucket for, a transcript line with usage but **no** `message.id`, and a `usage.json` row
with neither a session key nor an `agent_id`.

The #2167 backfill (§4.6) has its own corpus, `test/fixtures/orchscorecard/backfill/`,
rather than an extra row in the shared one: the shared corpus's counters are pinned
to the digit and a new session would move several of them for a reason unrelated to
what those tests are about.

The S0 counters (§4.4, the prompt classes in §4.1) have their own corpus too,
`test/fixtures/orchscorecard/s0/` — and it is the one corpus here built from REAL
rows: audit rows lifted verbatim from this group's beta9 session, the same window
the plan-2504 §1 hand tally was taken on. The synthetic corpora prove the SHAPE of
a counter; the s0 corpus proves the counter reads the real row — `cap: true`,
`starved_ms`, `scope: delta since <sha>`, the driver notice texts — exactly as the
app writes them. Its README maps every counter to the rows that witness it, and
the replaced-pane rows pin the dedup rule the brief asks for: two
`rd-lane-spawned` rows on one `(pr, block, round)` triple, different scopes, one
histogram cell under the first row's scope.

The backfill corpus itself is the shared corpus with ONE change — `ses-13`
(`w-13`, attributed to #900) carries the row the broken collector wrote, four zero
counters under `source: "statusline"` — and its transcript sits under a
WORKTREE-cwd project folder (`C--Projects-loomux-worktrees-agent-rev-1919`). That
folder name is the shape #2167 first suspected of breaking the lookup, and it is
there as a **non-regression witness**: neither this script's scan nor
`usage::claude_transcript_path` derives a slug from a cwd at all, so the folder's
name never decided anything. Two controls sit beside it — `ses-14`, a NON-zero row
whose transcript is also on disk and which must be left exactly alone, and
`ses-19`, a zero row with no transcript, which must be REPORTED as skipped rather
than silently dropped; `ses-20`, a zero row whose transcript EXISTS and folds to
nothing; and `agent:w-21`, a zero row keyed by agent id rather than by a session.
Each must land in its own skip bucket — only the first says the store lost
anything — and each is what stops its bucket being a field nothing ever fills. The backfilled run's #900 card is asserted equal to the
shared corpus's own, field for field: the backfill's job is to reconstruct exactly
what a working collector would have written.

The H8 split gets **both** of its branches, and they are genuinely different — which
matters, because the branch this PR originally claimed to cover was not in the corpus
at all: `rev-11-prev` existed only in `agents.json`, named by no audit row, so both
shared sessions were the partial case and the self-correcting branch was pinned by
nothing:

- `ses-11` — **both** occupants attributed to #900, via an `rd-lane-spawned` row that
  names `rev-11-prev`. Each is credited half and the halves **re-sum to the whole row**
  (535 + 535 = 1070). This is the case the benchmark's delegate column rests on, and on
  this group's store it is the common case rather than a corner — §4.6 carries the
  live-store figures.
- `ses-14` — one occupant attributed, one not. Half the row is **dropped**, so the
  delegate side under-counts and the orchestrator share correspondingly over-estimates.

What the test pins, named as `test/orchscorecard.test.ts` asserts it: each per-occupant
credit **individually** (`rev-11.tokens_credited === 535`,
`rev-11-prev.tokens_credited === 535`), the row each occupant reads (`tokens === 1070`
for both — one session row, two occupants) beside the split's operands (`usage_key` is
`session`, `shared_session_agents` is 2, and the two weights that multiply into the
credit — `session_weight` is 0.5, `pr_weight` is 1) — and their **sum**, asserted as
`535 + 535 === 1070` with "a fully-attributed lineage must re-sum to its session row,
neither doubled nor halved". Beside them the unshared control `w-13`
(`shared_session_agents` is 1, credited whole — `tokens_credited` equals `tokens`)
pins that the split is not a blanket halving. Because the per-occupant credits are
pinned at 535 each, they redden on their own under either regression: crediting a
shared row *once* reads 1070 + 0, a *no-split* implementation reads 1070 + 1070, and
neither value passes an assert against 535. The sum is a cross-check, not the only
witness — it states the re-sum property outright instead of leaving it implied by the
two halves.

What no test can reach is whether an **even** split is the right one: it is an
assumption about how a shared pane's spend divided, and only the structural field named
in H8 would settle it.

The one instrument here is `npm test`. `tsconfig.json`'s `include` is `["src"]`, so
`tsc --noEmit` typechecks neither `scripts/` nor `test/` and has no opinion on any of
this — a mutation table for this file must not claim the compiler as a second reader.
