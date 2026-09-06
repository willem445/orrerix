# The structured pane

The human-facing surface for a pane driven by a structured harness (#2891),
and the projection that feeds it. `doc/design/harness-adapters.md` is the
contract this note builds on and never restates: §1.2 is the event vocabulary,
§5.1 is "two projections, one log", §5.6 is the wire event. Where the two
disagree, that note wins.

This note covers **S2**, the DOM-free projection (`src/structuredview.ts`). S4
extends it with the renderer's own sections.

---

## 1. What S2 is, and what it is not

`project(state, batch) -> state` folds a batch of harness events into the
**block list** a renderer draws. It is a pure module in the
`timelinelayout.ts` / `embedsplit.ts` shape: no DOM, no timers, no
`Date.now()`, and no intra-`src` imports at all (TS5097), which is what lets
`node --test` run it directly.

It is **not** the VT projection. `harness-adapters.md` §5.1 puts that half in
the engine — `transcript::Renderer` writing bytes into the `OutputBuf` ring, so
`get_output`, replay, thumbnails and `last_exit_tail` keep working. Both halves
are fed from the same `events()` stream by the same drainer.

> `demo/structured-pane/DESIGN.md` §9 assigned the VT projection to a
> `projectText()` in this module, and §8 of that file left the choice open for
> whoever owned the contract. S1a closed it, in the engine. That mock predates
> the resolution and says so itself: "where this file and that note disagree,
> the note wins".

## 2. The block catalogue

`DESIGN.md` §6 is the visual spec; this is what the model gives the renderer to
draw it from. Eight block kinds:

| block | opened by | joined by | notes |
|---|---|---|---|
| `text` | `Text` | consecutive `Text` deltas | one run, not one block per delta |
| `thinking` | `Thinking` | consecutive `Thinking` deltas | **never** merged into `text` |
| `tool` | `ToolCall`, or a `ToolOutput`/`ToolResult` with no call | later `ToolOutput`/`ToolResult` **on the same `ToolUseId`** | one card with a lifecycle |
| `delivery` | a local `delivery` input | — | what orrerix sent in |
| `request` | `PermissionRequest` / `UiRequest` | its `*Settled`, on the same id | `channel` records which of the two |
| `turn` | `TurnStarted` | its `TurnEnded` receipt | one rule per turn, not one per boundary event |
| `notice` | `Compacted`, `Exited`, `Observed(..)`, a local `note`, an unknown kind | — | the harness talking about itself |
| `evicted` | the `MAX_BLOCKS` ceiling | further evictions | see §5 |

`QueueChanged` is **state, not a block**: an empty queue is not news, and a row
per update would fill the transcript with the queue's own churn.

## 3. Four rules the projection is written around

**Unknown is not a value.** A fact the pane does not have is `null`. An orphan
`ToolResult` — a result whose call this projection never saw, which a client
attaching mid-session gets on its first batch — produces a card whose `name` is
`null` and whose `orphan` flag is set, never a card named `"unknown"`.

**Thinking is not text.** They are separate blocks because a renderer that
quiets thinking (#2891's dim switch) cannot do so if they share one. Anything
that is not another delta of the same kind closes the open run, so a tool card
arriving mid-paragraph does not get the paragraph's second half rendered above
it.

**An unknown event kind is recorded, not fatal.** `HarnessEvent` is an additive
enum, and §1.2's promise is that "a consumer that does not match them keeps
compiling and keeps working, minus what it does not read". A kind this build
does not know becomes a `notice` block carrying the raw kind, and bumps
`state.unknownEvents`. Nothing in `project` throws on input.

**`ToolOutput.delta` is a DELTA, so the projection APPENDS.** §1.2 chose the
delta over pi's accumulated `partialResult` because the conversion only goes
one way cheaply: the adapter holds the previous value and subtracts, a consumer
does not. If this module ever starts replacing, pi's suffix subtraction has
moved into every renderer instead of living once in `pi.rs`.

## 4. The two inputs that are not `HarnessEvent`s

A structured pane's transcript has to show two things no harness reports: what
orrerix **delivered** into the pane (`harness::Turn`'s four variants — the one
thing in the stream the agent did not produce) and orrerix's own `[orrerix]`
notices or an adapter-level note (a retry, an extension fault).

They are `LocalEvent`s, tagged `delivery` and `note`, riding the same batch but
**not** spelled as `HarnessEvent` variants. Giving a harness a way to emit a
`Delivery` would let it forge one — the same conflation §1.3 rule 2 refuses
between a scraped fact and a reported one.

> Open, for S1b/S3b: the plan's pi decoder maps `auto_retry_*` and
> `extension_error` to a "Note", and §1.2's enum has no `Note` variant. Either
> the enum grows one or those events are dropped at the adapter. This module
> reads both a `note` local input and an unrecognised kind, so it is correct
> under either resolution; the decision is not S2's.

## 5. Two ceilings, both visible

An elision the reader cannot see is a transcript that lies (`DESIGN.md` §7), so
each ceiling produces a rendered artifact rather than a log line.

- **`MAX_BLOCKS` (2 000).** The oldest blocks roll into a single `evicted`
  sentinel block, always at the head, never itself evicted, carrying the count.
  Its slot counts against the ceiling, so showing the elision cannot push the
  pane over its own bound. `state.evicted` is the positive control a test
  asserts before claiming eviction fired. An evicted block's join keys are
  dropped, so a late event for it opens a fresh card rather than appending to
  something nobody can see.
- **`MAX_TEXT_BYTES_PER_BLOCK` (256 KiB), per block and per tool card.** The
  **head** is dropped, because the live end is the part being read, and the
  block records `droppedBytes`. The trim never splits a surrogate pair: a cut
  between the halves leaves a lone surrogate, which is not a character.

Byte figures are UTF-8, tracked incrementally per delta rather than re-measured
over the whole block — the storm fixture's rule that a producer may not make
the consumer pay per event, one level down.

## 6. `ViewState`, and why folds are keyed by id

`ViewState` — `{collapsed: Set<string>, dimThinking: boolean}` — is the view's,
and the reducer never writes to it. That is `CLAUDE.md`'s in-list-editor rule:
the renderer rebuilds its elements from the model on every batch, so a fold
held on a DOM element is lost on the next delta.

Block ids are a **counter over the event sequence**, not an array index, and
that is load-bearing twice over. Re-projecting the same events from
`emptyState()` yields the same ids, so a client that reconnects and replays
keeps the human's folds; and an eviction shifts every surviving block's
position, which under an index key would silently move a fold onto a different
card. `pruneViewState` drops folds for blocks that are gone, so the set cannot
grow without bound.

## 7. `textTail`

The thumbnail read model: the last *n* characters of the projection as plain
text, live end first. Thinking is included only when the view is not dimming
it, so a thumbnail shows what the human chose to look at. A tool card
contributes its name and status, **not** its output — a thumbnail of a
4 000-line grep result says nothing about what the agent is doing, which is the
one question a tiled pane has to answer.

## 8. Fixture, and the swap owed

`test/fixtures/structuredview/session.harness.jsonl` is synthesized, written
directly in the consumed vocabulary. S1b's
`crates/loomux-engine/tests/fixtures/harness/pi/*.jsonl` are pi's own **wire**
shapes, which need `pi.rs`'s decoder to become these events — a decoder this
module must not grow a second copy of. When S1b lands, the honest replacement
is a capture of what `pi.rs` really emits over those wire fixtures; the swap is
a fixture edit with no change to `structuredview.ts`. The fixtures README
carries the same statement next to the file.
