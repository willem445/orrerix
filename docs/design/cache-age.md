# Design: prompt-cache age — the pane chip and compacting before idle

Status: implemented (issue #3407).

## Problem

A provider's prompt cache is keyed on the conversation prefix and expires a fixed
time after the last request that read or wrote it. A wake after it has expired
re-reads the whole context at the uncached price. For a long-lived orchestrator
that is the whole session: ≈1.3 M cache-read tokens a wake while the cache is warm,
and the same prefix written back at the cache-WRITE rate when it is not. Nobody
could see how long a pane had been quiet compared with that TTL. The human could
not tell whether a compact would be cheap now or wasted later, and the orchestrator
had no reason to compact before going idle.

## What the chip claims, and what it cannot

**The cache is not observable.** No CLI, transcript or API response tells orrerix
that a prefix is still cached. Two things are observable:

1. **When the pane last made a model request** — the moment its usage counters
   moved (below).
2. **The provider's documented lifetime rule.** Anthropic: "By default, the cache
   has a 5-minute lifetime", measured "from the start of the request that writes
   or reads the cache entry, not from the end of its response", refreshed on each
   use, with a paid 1-hour option. OpenAI: a prefix "remains eligible for reuse for
   30 minutes after its most recent write or reuse" on GPT-5.6 and later, and
   "typically remain[s] active for around 5 to 10 minutes of inactivity" on earlier
   models. Both were checked 2026-09-23.

`hot` / `cooling` / `cold` is an **inference** from those two facts, and every
surface says "inferred, not observed".

**The inference errs toward hot, and the error has a known size.** The provider
starts its clock when a request STARTS. orrerix sees the request when its counters
land, which is at the END of the response, rounded up to the next usage tick. So
the observed age is younger than the real one by the length of the last response
plus at most one tick. The cooling band below is sized to cover ordinary
responses. It does not cover the extreme case the Anthropic docs name: a
four-minute stream against a five-minute TTL. That case is a stated residual.

## Why it is derived from the usage sampler, never a new poll

`compute_group_usage` already refreshes every live agent's cumulative counters on
the polled publisher's strip tier (`docs/design/polled-views.md`), about once a
second. "The counters moved since the last reading" is the same thing as "a request
landed since the last reading". So `cacheage::fold_activity` runs inside the merge
the collector already performs (`merge_usage_entry`), on the reading the tick
already made. There is no second transcript read, no new clock and no new thread.

It reads the **four counters every usage source already fills**, never a per-vendor
timestamp field. That keeps it CLI-agnostic (CLAUDE.md constraint 8): a claude
transcript, a codex rollout, pi's session file and a structured pane's stream all
produce it the same way. The price is that a source carrying no tokens produces no
activity. Today that is a statusline-only row (copilot), and that CLI has no TTL
anyway.

The fold's rules, each pinned in `crates/loomux-engine/src/cacheage.rs`:

- **Growth is judged on the total.** A reading that moves tokens between buckets at
  an equal total, or a lower reading from a different source replacing the row, is
  not a request.
- **A wake is growth after at least `WAKE_GAP_MS` (1 min) of quiet**, and it records
  the delta. Growth inside the gap is the same turn continuing: the clock advances
  and the recorded wake stands.
- **A first sighting is never folded.** A row that arrives already carrying tokens
  has a cumulative total, not a request. Charging that total to one wake would
  report a session's whole history as its last wake, so its activity stays unknown
  until its counters next move.

The result persists in `usage.json` as the row's `activity` object. The field is
additive: an older row reads as unknown. So a chip survives an app restart with
the right age.

### The last wake's cost

The chip's menu shows what the first request(s) after the last quiet stretch cost,
split three ways: tokens **read** from the cache, tokens **written** to it, and
**uncached** input, plus the dollar delta when both readings carried one. That
split shows what cold costs. A warm wake is mostly cache-read (0.1x input on
Anthropic's table). A cold wake is mostly cache-written (1.25x) plus input. At the
tick's one-second cadence the delta is usually one request. When two requests land
inside one tick it covers both. That only ever makes the figure larger: it never
shows a cold wake as a warm read.

## The TTL table, and where it lives

- **The per-CLI default is `CliCaps::cache_ttl_minutes`.** The TTL is a fact about
  the vendor, like every other `CliCaps` field, so it is written down once and
  looked up rather than re-derived as `if cli == …` at a call site. The value is
  **the conservative one on purpose**: guessing a TTL longer than the account really
  gets makes the chip say `hot` over a cache that is gone, which is the one wrong
  answer this feature exists to prevent. claude takes 5 (not the paid 1 h). codex
  takes 5 (the earlier-models rule, because the model is a block setting and the
  30-minute rule belongs to newer models only). copilot, opencode and pi route to
  several providers, so their honest default is `None`: the chip shows the age and
  claims no state. gemini documents no fixed lifetime, so it is also `None`.
- **The override is the block's `cache_ttl_minutes:`** in `.orrerix/workflow.yml`.
  That is the "documented setting" that switches the Anthropic 5-minute and 1-hour
  cases: an account on the 1-hour cache writes `cache_ttl_minutes: 60` on the blocks
  it runs. `0` means "unknown, infer nothing". A value above a day is refused, not
  clamped, because no provider documents a longer cache.

  **Why the block, and not a group guardrail or an app setting.** Both inputs are
  per block. A block pins the `cli:` and the `model:` (the codex TTL depends on the
  model), and a roster can mix an Opus block on a 1-hour account with a codex block
  on a 30-minute model. A single group knob would be wrong for one of them, and an
  app setting could not tell them apart at all. The value grants nothing, reaches no
  command line and names no path, so the capability-closure rule has nothing to
  guard.
  `group.json` persists it with the rest of the block, and a hand-edited value above
  the ceiling is dropped on read, the same defense `driver` has.
- **The backend resolves it, and the frontend keeps no table.** Each usage row
  carries `cache_ttl_minutes` and `cache_cooling_after_ms` already resolved
  (`effective_ttl_minutes`, `cooling_after_ms`). A second copy of the table in
  TypeScript would be a second place for the answer to drift.

**Not done: automatic TTL detection.** Claude transcripts split cache writes into
`ephemeral_5m_input_tokens` / `ephemeral_1h_input_tokens`, which is evidence of
which TTL a session actually uses. Reading it means a claude-specific branch in the
fold and a mixed-TTL rule. The override covers the need today, so detection is left
as a follow-up rather than built speculatively.

## The chip

`hot 3m` → `cooling 48m/60m` → `cold`, or `idle 12m` where no TTL is known.

- **Cooling starts at `cooling_after_ms(ttl)`**: the TTL minus a band of a fifth of
  it, never narrower than two minutes (48 min on a 60-minute TTL, 3 min on a
  5-minute one). The two-minute floor exists for the backstop (next section). It
  is also wider than the response-duration error above for an ordinary response.
- **Cold starts at the TTL.** The chip never says `hot` or `cooling` over an
  expired cache.
- **No timer of its own.** The reading rides the tab strip's existing snapshot read
  (`orch_strip_view`, every few seconds, gated on visibility). `main.ts` hands each
  pane its reading on the same subscription the Agents tab's idle rung uses, and
  the label is re-derived against the clock on each delivery. Minute-granularity
  text needs no faster tick, and a hidden window does not need one at all.
- **Chrome only** (constraint 1). The chip is a header button with the queue and
  mail chips' shrink weight. Its menu is `showContextMenu`'s floating overlay. No
  PTY is resized.
- **Compact now** calls `orch_request_compact(group, agent)`. That sets the same
  `compact_requested` flag an agent's own `request_compact()` sets, so
  `compact_nudge_tick` is still the one place that types `/compact`: at the pane's
  next quiet observation, never mid-turn, arming the post-compact re-grounding like
  any trusted fire. The command refuses an agent outside the named group (holding a
  valid group id is not membership), a dead agent, and a CLI with no `/compact`. It
  is audited as `compact-requested` with `by: human`. The menu item is disabled,
  with the reason, where it could not act.
- **The Agents tab** shows the same label per row, from the same reading
  (`PaneFacts.cache`). It is a second axis beside the state ladder, never an input
  to it: how long ago a pane last spoke says nothing about whether it is working now.

## The orchestrator compacts before going idle

Two halves, because a rule an agent can forget needs a backstop that cannot.

**The resident rule.** It is one line in `orchestrator.md`'s *Compact at lulls*
bullet: always compact before ending a turn with nothing in flight, because a wake
past the TTL re-reads the whole context uncached.

That line has to agree with the rule further down the same bullet, which is the
human's: every compact costs a full re-grounding cycle, so do not compact below 50%
context "unless you have a specific reason". Ending a turn with nothing in flight is
now **named as one of those reasons**, so the two lines are one rule, not two that
contradict each other. Below 50% the resident rule still says compact, while the
backstop does not nudge. That asymmetry is deliberate: the orchestrator's own call
may take a small compact, but orrerix's unprompted nudge keeps the floor it has
always kept. The resident core is under a byte budget, and both lines were
tightened to fit it.

**The backstop, `cache_idle_nudge_tick`.** It runs on the compact-nudge loop, after
`compact_nudge_tick`, so the quiet clock it reads has already folded this tick's
output and a compact that pass just armed reads as busy. It types
`[orrerix] going idle with no work — compact now` into an orchestrator pane when
**all** of these hold:

- **It is inside the cooling band.** Idle at least `cooling_after_ms(ttl)` and still
  under the TTL. Earlier, the orchestrator may yet act on its own. At or past the
  TTL the cache is already (inferred) cold, and compacting then pays the cold
  re-read it was meant to save. The point is to compact while the compaction
  request itself still reads a warm cache. The two-minute floor on the band gives
  the 60-second loop two ticks inside it on a five-minute TTL.
- **Nothing is in flight.** Nothing is running that could wake the pane on its own:
  - a live delegate (a worker, reviewer or planner that is not dead; a manager or
    lead is the human's own pane);
  - a pending `notify_when` watch in the group;
  - pending intake;
  - a queued delivery for the pane;
  - a live review or plan drive. The drive files are read last, and only for a pane
    that would otherwise fire. An unreadable file counts as in flight.
- **The context is at or above the compact floor.** This is the heuristic nudge's
  own floor setting, with the 50 % smart default. It **fails closed** on a missing
  reading, unlike the heuristic floor: the nudge is itself a model request, and
  with no reading there is no evidence it would pay for itself.
- **No compact is already pending or requested**, the group is not paused, and the
  CLI can take `/compact`.

**Once per idle stretch, released on evidence, never on output.** A fire latches
`cache_idle_nudge_latched`. The latch releases only when something goes in flight
or the context falls under the floor (a compact landed). The pane's own output does
not release it. Most of the output after a nudge is the orchestrator answering
the nudge, so releasing on output would re-nudge an orchestrator that read the
notice and chose not to compact, once per TTL, forever. The fire is audited as
`cache-idle-nudge` with the idle time, the TTL and the context percent. Delivery
goes through `deliver_prompt`, which holds for a human's occupied input box, so
"never mid-decision" is the existing quiet-window rule plus the existing input
guard, not a new one.

**Deviation from the issue's wording, stated.** The issue asks that pending watches
not block the backstop when they are "terminal or > TTL away". orrerix knows when
a watch EXPIRES, never when its target will resolve. A CI watch can fire at any
second of its life. So every live watch counts as in flight. An orchestrator
waiting on CI will be woken by that watch anyway, and a compact while it waits
would be the wasted one.

## Residuals

- **The hot-side error:** as large as the last response's duration plus one tick
  (above). The cooling band absorbs the ordinary case, not a multi-minute stream.
- **No activity from a token-less source** (a statusline row). Today that is only
  copilot, whose TTL is unknown anyway.
- **A first sighting is unknown.** A row first seen with tokens already on it shows
  no chip until its next request.
- **The backstop's idle clock is the output-quiet clock, not the usage clock.** It
  is the signal every sibling tick uses, and it errs toward "not idle": any
  meaningful output resets it. It is not the chip's clock, so the two can disagree
  by a response's length.
- **An orchestrator that answers the nudge without compacting and keeps chatting
  with the human** is not re-nudged until work goes in flight or the context drops.
  That is the latch doing its job, stated so it is not mistaken for a miss.

## Tests

- `crates/loomux-engine/src/cacheage.rs` (unit): TTL resolution, including the
  override, `0` and an unknown CLI; the cooling band; every arm of the fold (no
  growth, bucket shuffle, wake after the gap, same turn inside it, the inclusive
  boundary, first-ever movement, a lower dollar figure); the backstop's band edges
  and each disqualifier on its own; and the notice as one paragraph.
- `src-tauri/tests/orchestration.rs`: the fold through the real usage merge and
  `group_usage`, persisted to `usage.json`; the resolved TTL on a row from the CLI,
  a block override and `0`; Compact now refusing another group and firing through
  `compact_nudge_tick`; the backstop firing once in the band, not re-arming on
  output, re-arming on a context drop, refusing past the TTL and without a reading,
  holding for a delegate and a watch; and a block override moving the band and `0`
  turning it off.
- `src-tauri/tests/workflow.rs`: the block key's range, absence, and the refusal
  above a day.
- `test/cacheage.test.ts`: the state boundaries against the row's own threshold,
  the "cannot say" rungs, the labels, flooring, the wake line, the tooltip's
  inferred-not-observed wording, and every way the strip lookup answers null.
- The header chip, its menu and the Agents-tab cell are DOM wiring over those
  modules, validated by hand (see the PR).
