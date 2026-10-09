# Design: what the next prompt will cost

Status: implemented (issue #3831).

## Problem

A long-lived pane carries a large context. Sending it one more prompt costs very
different amounts depending on something the human cannot see: whether the
provider's prompt cache still holds that context. While it does, the context is
read at a small fraction of the input price. Once it has expired, the same
context is written to the cache again at more than the input price. And a third
option is always there: start a new agent, which pays for a much smaller context.

The cache-age chip ([cache-age.md](cache-age.md)) says which state a pane is
probably in. This note is about putting a figure on it, so that "send now, wait,
or start fresh" is a comparison of three numbers and not a guess.

## The number

For one pane, with:

- `C`: the context its newest turn was sent, in tokens;
- `F`: the context its session's **first** turn was sent;
- `P`: the typed prompt, in tokens;
- a price per million tokens for a cache read, a 5-minute cache write and a
  1-hour cache write;
- `T`: the cache TTL the chip uses;

the estimate prices the **input side of the next request** three ways:

| figure | tokens | priced as |
|---|---|---|
| **warm** | `C` read, `P` written | `C × read + P × write` |
| **cold** | `C + P` written | `(C + P) × write` |
| **fresh agent** | `F + P` written | `(F + P) × write` |

`write` is the 1-hour rate when `T` is 60 minutes or more, and the 5-minute rate
otherwise. "Now" is whichever of warm and cold the chip's state says applies:
warm while the chip reads `hot` or `cooling`, cold once it reads `cold`.

**Where no cache state is known, all three figures are still shown and none is
called "now".** There are two such cases, and the surface says which:

- **No request has been seen on the pane yet.** Its chip reads `cache —`. This
  is a pane whose session has nothing on record: one just identified, or one
  restored after a restart whose session this store has never watched. Its
  context and its price are known at once, and this is the moment the estimate
  is most wanted, by a human back at a pane that has probably gone cold. So the
  cold figure, the warm figure and the fresh-agent figure are shown, cold first,
  under a row that reads "Cache state unknown: no request seen on this pane yet".
- **No cache lifetime is known for the pane's CLI.** Its chip reads `idle 12m`.
  The row reads "Cache state unknown: no cache TTL is known for this pane".

A pane with no chip reading **and** no context reading shows nothing: there is
nothing to price, and nothing to say that the chip's own tooltip does not.

A restored pane is not always in the first case. A usage row is keyed by
session id and its activity is persisted with it, so a pane that resumes a
session with a request on record gets its chip back at once, with the age that
session had, usually `cold`. It then has a state like any other pane.

`F` is measured, not assumed. It is the first counted turn's own input, cache-read
and cache-written tokens together: the CLI's system prompt, its tools, the repo's
instruction files and that session's first prompt, on this machine, this repo and
this model.

## Where each input comes from

All of them are on the usage row, under one `prompt_cost` object, resolved by the
backend. The frontend keeps no price table, no model table and no TTL table. That
is the rule [cache-age.md](cache-age.md) set for the TTL, for the same reason: a
second copy is a second place for the answer to drift.

| input | source | where it is unknown |
|---|---|---|
| `C` | the context reading the usage tick already makes each pass for the usage series (`agent_context_signals_for_group`) | a CLI with no token record (copilot); a session not yet identified |
| `F` | each transcript fold records its first counted turn (`SessionUsage::first_context_tokens`), persisted on the usage row | opencode, whose session row is a running total with no per-turn record |
| price | `usage::price_quote` for the row's current model | any model the table does not list; any row whose CLI reports its own dollars |
| `T`, the cache state | the chip's own reading; the TTL beside it is the usage row's resolved `cache_ttl_minutes` | a CLI with no TTL and nothing detected; a pane no request has been seen on yet. Both still show all three figures, with none called "now" |
| characters per token | resolved beside the price | wherever the price is |
| `P` | the compose strip's text | every pane without a compose strip |

`prompt_cost` is an object on a **live** row and `null` on a historical one. A
row whose agent is gone has no next prompt. It is also the row the MCP
`group_usage` tool hands an agent ten at a time, mostly historical ones, so eight
flat keys on every row would be context each of those calls pays for and nothing
reads.

### Which rows are priced

A row is priced off the table only when its own dollars are: `estimated: true`,
which is the row saying its CLI bills by this table. pi and opencode report the
dollars they paid, so their rows carry no price, and a pi model id that happens
to name a Claude family (`anthropic/claude-opus-4-8`) does not change that. The
gate reads the row's provenance. It never branches on a CLI's name. Such a pane
still gets the token half of the estimate.

### The typed prompt

There is no tokenizer here. Claude's is not public, so any tokenizer crate would
count with another model's vocabulary. The vendor's `count_tokens` endpoint needs
an API key, which a subscription login does not have, and would be a network call
per keystroke. So the typed text is counted by its length:

- on models before Claude 4.7, a token is about **3.5** English characters;
- Claude 4.7 and later produce about **30 % more** tokens for the same text, so
  about **2.7** characters each.

Both figures are the vendor's (below), and the backend resolves which applies to
the row's model. The count is rounded up. It is rough: code and non-English text
tokenize differently, and the surface says so. Where no characters-per-token
figure is known, the typed prompt is **left out** of every figure and the surface
says that, instead of counting it with a constant nobody sourced.

The typed text is visible only in the compose strip, which only an orchestrator
pane has. Text typed into a CLI's own input box never reaches orrerix. Everywhere
else the estimate is for the history alone, and the menu says "typed prompt not
visible — add its length yourself".

**Images attached in the compose strip are not counted.** They are sent as
"Attached image: <path>" lines, which the agent then reads with a tool. Neither
those lines nor what reading the images costs is in any figure, and that cost is
not knowable here: it depends on the images and on how the model reads them. The
inputs row says "N images attached, not counted" and the compose line ends with
"(N images not counted)", so an images-only draft does not read as nothing to
send.

## The price table

`usage::PRICE_ROWS` is transcribed from Anthropic's pricing page, "Model pricing":

- source: <https://platform.claude.com/docs/en/about-claude/pricing>
- read: **2026-10-09** (`usage::PRICE_TABLE_DATED`, which travels on every row that
  carries a price)

One row of the table per row of the page, in the page's order, each row the
page's own five numbers. Three rules stated on that page shape the code, quoted
as fetched:

- **The cache multipliers.** "5-minute cache write: 1.25x base input price",
  "1-hour cache write: 2x base input price", "Cache read (hit): 0.1x base input
  price (0.025x on Claude Fable 5.1 and Claude Mythos 5.1; 0.05x on Claude Opus
  5.5 and Claude Sonnet 5.5)". A test asserts every row obeys them, which is what
  catches a mistyped digit in one of the three derived columns.
- **The Haiku 5.5 tier.** "Claude Haiku 5.5 is priced by prompt length: a request
  whose prompt is over 100,000 tokens pays higher prices. A request's prompt
  length counts all of its input tokens, including cache reads and cache writes.
  Each request is priced on its own".
- **Sonnet 5.** The "$2/$10 per million input/output token pricing for Claude
  Sonnet 5, announced at launch as introductory pricing through August 31, 2026,
  is now the standard price."

The tokenizer figures are from the same vendor's glossary, "Tokens", read the same
day: "Claude 4.7 and later models and Claude Mythos Preview use a newer tokenizer
that produces approximately 30 percent more tokens for the same text than earlier
models, on which a token represents approximately 3.5 English characters."

### The contract

- **A listed version gets its own row.** The table is keyed on family **and**
  version, read off the model id (`claude-sonnet-5-5`, `claude-sonnet-4-5-20250929`,
  `claude-3-5-haiku-20241022`, `claude-opus-4-8[1m]`). The family-only table this
  replaces could not tell Sonnet 4.6 at $3 from Sonnet 5.5 at $2.
- **An unlisted version of a listed family takes that family's highest current
  price**, column by column, and the row says so (`price_basis:
  "family-ceiling"`). The estimate's posture has always been that it never
  under-reports, and an id the table has not caught up with is far likelier a
  model released since than an old one.
- **"Current" leaves out the rows the page marks retired.** Opus 4.1 lists at
  three times any current Opus. Counting it would triple the figure for every new
  Opus until someone updated the table. A retired version an old transcript
  really does carry is still priced exactly, by its own row.
- **An unlisted family is not priced.** Tokens only, on every surface.
- **A session's cost is priced per request.** Two things a session-wide price
  cannot carry: the prompt-length tier each request's own prompt falls in, and
  which cache each write went to. Before this, every cache write was priced at
  the 5-minute rate, which under-reported an account on the 1-hour cache.

To update the table: re-read the page, change the rows and the date together,
and re-read the three rules above.

## Honesty of the number

Each way the figure is wrong, and where the surface says so. Every row of this
table is in the chip menu, on the row it applies to or in that row's tooltip.

| way it is wrong | what the surface says |
|---|---|
| output tokens are unknown | the header: "estimate, input side only", and that output is priced on top, at the output rate of the price tier the request falls in |
| a tool-heavy turn re-reads the context once per tool call | "It is one request: a turn with N tool calls reads the cache about N times" |
| 5-minute or 1-hour cache | which write rate was used, the TTL it follows, and where that TTL came from |
| the CLI compacts on its own | the context is "the last turn's"; with a compact in flight, "compact in flight" and "may have dropped since" |
| the chip errs toward hot | the warm figure is "if the cache is still warm (inferred, not observed)" |
| the warm model assumes the whole context is read | it points at the last wake, shown above it in the same menu: what a real request read and wrote |
| typed text is not tokens | `≈`, and that it is counted from its length, with the characters-per-token used; where it cannot be counted, "typed … not counted" in the menu and "(typed not counted)" on the compose line itself |
| `F` includes that session's first prompt | stated, with `F`'s size |
| a subscription pays no per-token price | "list price", with the model and the date; tokens come first on every row |
| Haiku 5.5's price flips at 100,000 tokens | the tier each figure was priced at is named |
| the version is not in the table | "its family's highest current price is used and the figure may be high" |
| the cache state is not known | no figure is called "now"; a row says "Cache state unknown" and why, and the compose line ends "(cache state unknown)" |
| images are attached to the draft | "N images attached, not counted" on the inputs row; "(N images not counted)" on the compose line |

### What was measured

The warm model was checked against real requests before it shipped. This is an
observation of one machine, not a contract: 14 local Claude Code sessions
(versions 2.1.284 to 2.1.295), at every boundary where a typed prompt followed a
finished turn. There were 39 such boundaries; at 35 the next request read most of
its prompt from the cache, and those 35 are the sample below. Costs are compared
at Opus 5.5's 1-hour rates.

- The next request read at least **100 %** of `C` from the cache at every one of
  the 35 (100.5 % at the median, 101.6 % at the 90th percentile). The whole
  context is read.
- `C × read` alone was 79 % of the request's real input-side cost at the 10th
  percentile, 97 % at the median and 98 % at the 90th, and between 71 % and 99 %
  across all 35. It under-estimates, slightly, and in this sample never
  over-estimates.
- What it leaves out is the previous reply. A reply is output, so it is not in
  the cache, and the next request writes it. Adding the previous turn's output
  tokens to the write side was tried and is worse: 98 % / 113 % / 160 % of the
  real cost at the same three percentiles, because output tokens include
  thinking, which is not sent back. So the formula stays `C × read + P × write`,
  and this is its stated residual.

### Residuals

- **The warm figure runs a few percent low**, by the previous reply's visible
  text written at the write rate (above). On a pane whose last reply was very
  long it runs lower.
- **Cold and fresh assume nothing is cached.** Of the four cold boundaries in the
  same sample, three still read 31,000 to 35,000 tokens from the cache after gaps
  of 1.2 to 8.6 hours: the front of the prompt is shared with other panes, which
  keep it warm. (The fourth, after ten days, read nothing.) So both figures run
  high by roughly that prefix whenever another pane on the same account is
  active.
- **`C` is the last turn's.** A compact the CLI ran on its own since then is
  visible only after the next turn.
- **`F` is this session's first turn**, on the model it started on. A session
  that switched models is priced at the current model's rate against a context
  measured on another tokenizer generation.
- **The typed prompt's count is rough**, and there is no measured error bound for
  it: with no tokenizer there is nothing to measure against. It is normally two
  orders of magnitude smaller than `C`.
- **Fast mode, data residency and batch pricing are not modelled.** The page
  lists each as a multiplier over these rates, and nothing in a transcript's
  usage record is read to detect them.

## Where it is computed and shown

`src/promptcost.ts`: DOM-free, with no imports from `src/`, and tested in
`test/promptcost.test.ts`. It is not in the engine: every input is already on the
strip payload in the frontend, and an estimate computed in Rust would be an IPC
call per keystroke.

- **The cache chip's menu**, on every pane whose chip has a reading: a header
  row, the figures, and an inputs row. All read-only; each row's tooltip carries
  what the figure assumes. That includes the panes #3837 gave a chip: a solo
  pane, a lead pane and a plain agent pane adopted at spawn are looked up under
  the same identity the chip uses (`cacheIdentityOfPane`), and their rows carry
  the same `prompt_cost` object.
- **On a pane whose chip reads `cache —`, the estimate alone, where the context
  is known.** Clicking that chip opens a menu of the estimate's rows and nothing
  else: there is no wake on record and nothing to compact from there. On an
  orchestrator pane the compose line shows too. The chip's tooltip ends "Click
  for what the next prompt would cost", since nothing else about a muted chip
  says it opens anything. Where the context is not known either, the click
  opens nothing, as it did before, and the tooltip does not offer it.
- **The compose strip**, on orchestrator panes: one line under the box, with the
  same rows as its tooltip. It shares the status line's fixed-height slot rather
  than taking a row, so the strip is exactly as tall as before and the terminal
  above it is never resized (CLAUDE.md constraint 1). A rejected send's message
  takes the slot while it shows. What the line leaves out it says on the line,
  not only in the tooltip: a draft that cannot be counted ends it with "(typed
  not counted)", because the line is what is read while typing.

There is no timer. The line is recomputed when the draft changes, coalesced to
one per animation frame, and on each strip delivery, which is the read the chip
already rides.

One function, `PaneBadges.promptCostView`, builds the estimate for both surfaces,
and every decision in it is `costContextFor`'s, in `promptcost.ts`, where it is
tested. The cache state is the chip's own. The TTL is the usage row's resolved
`cache_ttl_minutes`, the one field the chip reads too, so the estimate and the
chip cannot be on different TTLs, and a pane with no chip reading still has one. How that TTL is resolved, including the
lifetime read off a session's own cache writes, is in
[cache-age.md](cache-age.md#the-ttl-table-and-where-it-lives).

## What an older build sees

`usage.json` rows gain two optional fields, `first_context_tokens` and
`detected_cache_ttl_minutes`. Both are absent from a row with nothing to say, so
a file written before them loads unchanged and is not rewritten by being read,
and an older build reading a newer file meets at most two keys it does not know
and ignores them ([usage-store.md](usage-store.md)).

The usage row gains `prompt_cost` and `cache_ttl_source`. Both are optional to the
frontend: against a backend that sends neither, the menu says what is missing.

## Alternatives rejected

- **A tokenizer crate, or `count_tokens`.** Above.
- **A frontend price table.** Drift, and the TTL precedent.
- **Estimating in the engine.** An IPC call per keystroke.
- **An app setting for "my account bills per token".** A second place for the
  truth, which nothing in a transcript can check. Dollars are labelled list
  price and tokens lead instead.
- **A prompt-length field in the menu** for panes without a compose strip. One
  more control to explain, for a term that is normally under 1 % of the figure.
- **Adding the last reply to the write side.** Measured above; worse.

## Tests

- `src-tauri/src/usage.rs` (unit): each listed version's five numbers; every
  spelling an id's version is read off; the family ceiling, with the retired rows
  left out; the Haiku 5.5 tier one token either side of the threshold; every row
  against the vendor's stated multipliers; the tokenizer split at 4.7; a cache
  write priced by the cache it went to; Haiku 5.5 priced per request; the first
  turn surviving a re-emitted line; and the detected TTL's three rules.
- `src-tauri/tests/orchestration/promptcost.rs`: the row's `prompt_cost` off a real
  transcript; a tiered and an unpriced model; a row whose CLI reports its own
  dollars; a historical row carrying no object; and a `usage.json` from before
  the two fields loading unchanged and being read by an older row shape.
- `src-tauri/tests/piusage.rs`, `src-tauri/tests/codexusage.rs`: the first-turn
  context on those two folds.
- `test/promptcost.test.ts`: the three figures; their ordering on a fixture where
  it holds and one where it does not; the write rate either side of the hour;
  which figure is "now"; nothing typed; an unknown `F`, an unknown `C`, no price
  and an uncountable prompt, each staying unknown; the tier; the strip lookup;
  the pane with no chip reading, priced with no state claimed, with its exact
  rows, figures and reason; the pane with no chip reading and no context, which
  shows nothing; which unknown each reason belongs to; attached images; and the
  words on both surfaces.
- The menu rows and the compose-strip line are DOM wiring over that module,
  validated by hand (see the PR).
