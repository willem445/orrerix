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
| `notice` | `Note` (#2850), `Compacted`, `Exited`, `Observed(..)`, an unknown kind | — | the harness talking about itself; `noteKind` says whether it really was the harness |
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

**The one case where it does replace is READ, never inferred.** pi's
subtraction has a precondition — each accumulation extends the last — and where
that fails the adapter emits the whole value instead of a wrong suffix. A
consumer cannot tell that from a legitimate delta that happens to repeat
earlier bytes, and any heuristic for it ("does this restate what I hold?")
silently eats genuinely repeating output, which is a worse failure than the
duplication it fixes. So `ToolOutput` carries `replaces: bool` (#2850 S1b):
`false` on every ordinary delta and on a call's first output, `true` when
`delta` is the whole current value and supersedes everything held for that
`ToolUseId`. It is the same argument that put the subtraction in the adapter
rather than in every renderer, applied one step further.

> This shipped as a stated residual first — unmarked on the wire, duplicated on
> the card — and S1b added the field after S2 argued the fact could not be
> recovered downstream. Recorded because the reasoning, not the field, is what
> a later harness adapter needs: a fact a consumer cannot infer is one the
> producer has to carry.

## 4. The one input that is not a `HarnessEvent`

A structured pane's transcript has to show something no harness reports: what
orrerix **delivered** into the pane — `harness::Turn`'s four variants, the one
thing in the stream the agent did not produce. That is a `LocalEvent`, tagged
`delivery`, riding the same batch but **not** spelled as a `HarnessEvent`
variant. Giving a harness a way to emit a `Delivery` would let it forge one —
the same conflation §1.3 rule 2 refuses between a scraped fact and a reported
one.

The harness's own asides are **not** local. This slice first shipped them as a
second `LocalEvent` because §1.2 had no variant for them; #2850 S1b then added
one, so a retry, a failure and a fire-and-forget extension display are reported
facts and arrive as `Note`.

> **Tense:** the variant below and the 11 -> 17 count *will be* true when #2986
> (S1b) and #2942 (S1a) land. On `main` today `HarnessEvent` has eleven variants
> and `is_decision_grade` lists five; the seven counted here include S1a's two,
> and are countable on the TypeScript side of this slice alone. This module
> reads the shape either way — an unrecognised kind becomes a notice rather than
> an error — so nothing here breaks while those sit open.

```rust
pub enum NoteKind { Retry, Error, Ui }
HarnessEvent::Note { turn: Option<TurnId>, note: NoteKind, text: String }
```

Three things about it the projection is built around:

- **The inner field is `note`, not `kind`.** The outer enum is
  `#[serde(tag = "kind")]`, so a variant field of that name emits a duplicate
  key, which `serde_derive` REFUSES outright — ``variant field name `kind` conflicts with internal tag``, so the colliding shape never compiled (#2850
  S1b, run 34046263686). What matters downstream is the other fix: silencing
  the derive with a `rename` keeps the field and ships the collision, and a JS
  consumer then sees `JSON.parse` keep the LAST duplicate key — every note
  arrives spelled `"retry"`, matches no arm, and is filed as an unrecognised
  event with nothing red on either side. The compiler stops the shape; nothing
  stops the rename, which is why the argument is written down rather than left
  to the build.
- **`turn` is a real `Option`.** A retry begins before a turn reopens and an
  extension can throw at boot. `null` is carried through as `null`; the open
  turn is never substituted, because that would invent an attribution in the
  field a renderer groups by. `TurnId` is a transparent newtype, so on the wire
  this is a bare number or `null`.
- **`NoteKind` is carried beside `level`, not collapsed into it.** The three
  kinds are each meant to be drawn differently, and `level` cannot separate a
  harness `ui` note from orrerix's own compaction row — both are informational
  and they are not the same thing. `noteKind` is `null` exactly when orrerix
  generated the row.

Population is **11 → 17** and seven variants are decision-grade (`ToolCall`,
`PermissionRequest`, `PermissionSettled`, `UiRequest`, `UiSettled`, `TurnEnded`,
`Exited`). That split is an audit-log concern, not a projection one: this module
draws all seventeen. Protocol bookkeeping — message boundaries, settle events,
command acks — never reaches `events()` at all, so it never reaches here.

## 4a. One thing #2891 asks for that the contract cannot carry: an exit code

#2891 wants a shell command rendered "command line + exit code + duration". Two
of those three exist here: the command line is `ToolBlock.input`, the duration is
`durationMs`. **The exit code has no field to occupy.** The contract's
`ToolResult` is `{turn, id, ok}` — a boolean verdict, not a status — and
`ToolOutput` carries bytes plus `is_error`. So a card can say a command failed
and cannot say it exited 2.

This is recorded rather than silently dropped, because the alternative is a
renderer slice discovering it at draw time and either inventing a field or
scraping the number out of the output text — which is the machinery the
structured path exists to retire. Closing it is a contract change (`exit_code:
Option<i32>` on `ToolResult`, or a distinct event), so it belongs to whoever owns
§1.2, not here. Until then the requirement is **declined at the contract**, and
a renderer that wants it should read this section rather than improvise.

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
