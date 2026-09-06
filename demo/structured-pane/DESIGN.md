# The structured pane — visual language

The decisions behind `demo/structured-pane`, written for S2 and S4 to inherit.
Where this file and `doc/design/harness-adapters.md` disagree, that note wins:
it is the contract and this is a mock.

## 1. What the surface is for

A supervisor watches several of these tiled, all day, and asks one question of
each: **what is this agent doing, and does it need me?** Everything below is
downstream of that. It is an *Operate* surface — the tool disappears into the
task — so familiarity is a feature and expression lives in precise details, not
in invention.

Dark is not picked by category. It is picked from the use scene: long sessions,
tiled beside terminals, on the near-black ground the rest of the app already
paints.

## 2. Colour: the app's tokens, copied, dark-only

Every colour is a `var(--token)` off one `:root` block in `pane.css`, copied
verbatim from `src/styles.css`. It is a **copy and it can drift** — `src/` is
the source of truth and `test/theme.test.ts` pins that file, not this one. The
copy exists only so the demo is a standalone page with no build step. Nothing
below `:root` introduces a raw colour, which is the same rule the app's token
layer enforces, held here by hand because no test watches this directory.

**There is one theme.** orrerix is dark-only — a single `:root`,
`color-scheme: dark`, no `prefers-color-scheme` block anywhere in `src/`. This
demo ships no light palette: inventing one would mean inventing values #2891
never sanctioned, and `doc/design/ui-redesign.md`'s rule that a hue is not free
to move between channels is exactly what an invented ramp would break. If a
light theme ever lands in the app, this page inherits it by replacing the
copied block with an import.

The three channels keep their jobs, and the transcript adds no fourth:

- **state** (six dyes) — what the agent is doing. The only tokens the warp
  gutter, the status chip and the state dot may use.
- **interaction** (`--accent`, the brand gold) — marks, edges, rings, carets.
  **Never a ground.** The primary button is gold *ink on a gold hairline*, not
  a gold fill, for exactly the reason `src/theme.ts` gives about `selectionFill`.
- **identity** (`--id-*`, `--cli-*`) — which *family* a tool belongs to, and
  which CLI runs in this pane. Never what state something is in.

## 3. The signature: the warp, at a third scale

`ui-redesign.md` §The signature gives every pane a 2px thread down its left
edge whose colour is the pane's live agent state, and argues that the rail
would carry the same device as an aggregate — "the same warp at two scales".

A structured transcript is a **timeline**, so this design runs the thread down
the transcript's own left gutter as well. **The pane edge says what the agent
is doing now; the gutter says what it has been doing, segment by segment.** You
scan the gutter to see the shape of a turn without reading a word of it, and
collapsing a block leaves its segment in place — so you can still see that
thinking happened there.

Three rules carry over unchanged, and one is new:

- The thread is **always 2px**. A state change moves a colour, never a size.
- It is a positioned pseudo-element, never a border in a box, so it cannot cost
  a reflow.
- Two weights in the whole surface — the hairline (`--line-w`) and the thread
  (`--thread`). No third.
- **New: the gutter's vocabulary is the six state dyes and nothing else.** An
  event that is not an agent state — a delivery arriving, a turn boundary, a
  compaction — is marked by **form**: a drawn glyph, a full-width hairline, a
  dashed rule. That is the same move the design note makes for `held` and
  `idle`, which "carry no dye and are marked by form, not by hue". It is what
  keeps the gutter readable as one question rather than as decoration, and it
  is why a delivery is a *seam* in the transcript rather than another colour.

A settled row's segment drops to 0.72 opacity so the eye lands on the live end
of the thread without the history going invisible.

**Open for S4:** the pane-edge warp is a documented *state position*. The
transcript gutter is a derived position at a third scale, and this mock keeps
it inside the state channel's rule rather than widening it. If a later slice
wants a seventh meaning in the gutter, that is an edit to `ui-redesign.md`
first, not a new token here.

## 4. Type

Two faces, and the second is earned rather than worn:

- `--font-ui` carries labels, prose and the model's own answer.
- `--font-mono` carries **machine identifiers only** — paths, commands, ids,
  counts, durations, token figures, diffs. The token layer's own rule is that
  the mono face always means "a literal string the machine gave you", which is
  what keeps it from being a costume for "technical".

The consequence is a rule with a real edge: a **permission's** payload is a
command line and takes mono; a **dialog's** message is prose written for the
human and takes the UI face. `render.js` marks the second `data-prose="1"`.

Every figure a reader compares against the last time they looked is tabular
(`.num`) — a ticker whose digits shift position as they change is the cheapest
thing to get wrong.

## 5. Motion

`ui-redesign.md`: "One animation exists in this app" — an attention thread
pulsing slowly, only while an agent is actually waiting on the human. This
surface holds that budget and adds exactly one continuous cue:

| cue | loops? | what it reports |
|---|---|---|
| attention pulse (pane edge + gutter) | yes | a request is waiting on you |
| streaming caret | yes, while bytes arrive | this text is still coming |
| running spinner on a tool chip | yes, while the call runs | this call has not returned |
| card pending → running → ok/error | no | the outcome landed |
| fold open/close (`grid-template-rows`) | no | you opened it |
| row arrival (120ms, 3px rise) | no | something new |
| ticker digit lift | no | this figure just moved |

The caret is the live-text analogue of the attention pulse: it reports a state
and stops the instant that state ends. Nothing here is decoration, and there is
no page-load choreography — a pane loads into a task.

**Reduced motion is measured, not asserted.** Under
`prefers-reduced-motion: reduce` the page has **0** animations with infinite
iterations (checked with `getAnimations()` in a Playwright context, not by
reading the stylesheet). The caret stays **solid** rather than vanishing — it
still reports that bytes are arriving, which is the state it exists to carry.
The demo's Motion toggle applies the same rules so the reading can be seen
without changing an OS setting.

## 6. The block catalogue

| event | renders as | why that shape |
|---|---|---|
| `Text` | bare prose, ≤74ch, no container | the model's answer is the content; everything else is apparatus around it |
| `Thinking` | dim, hairline-ruled, own mark, auto-collapses when done | the one block the eye should be able to skip |
| `ToolCall`/`ToolResult` | a card: mark, name, **identifying** argument, duration, status chip | it has a lifecycle and an outcome — that earns a container |
| `Bash` | its own block: `$`, command line, exit code | a command is read command-first, output-only-if-the-status-says-so |
| `ToolOutput` | mono, capped height, own scroll | a 4000-line result must not push the live end off screen |
| `Edit` | a real diff of the real arguments | "a tool ran" vs "here is what changed" |
| `UiRequest`/`PermissionRequest` | an **inline** amber card with the actions | §3.3 refuses a dialog outright: a dialog strands every agent reporting to that pane (#946) |
| `UiSettled` | the same card, quiet, recording the answer and **who decided** | a settled request is the audit trail; it collapses, it does not vanish |
| `Delivery` | a seam: full-width block, inbound glyph, sender, timestamp | the one thing in the stream the agent did not produce |
| `QueueChanged` | a footer chip, hidden at zero | an empty queue is not news |
| `TurnStarted`/`TurnEnded` | a labelled rule carrying the receipt | a turn is the unit a human scrolls by |
| `Compacted` | a **dashed** rule with before/after tokens | the transcript's own history being rewritten — `held`'s form, for a different kind of seam |
| `Note` (retry, `[orrerix]`, extension fault) | faint mono, leading rule, no container | the harness talking about itself, not the agent's voice |
| `Booted` | fills the header; unknown facts render `—` | "unknown is not a value" — never the string `unknown` |

Two behaviours are deliberate and worth keeping:

- **A failure opens itself.** Nobody should have to click to find out why
  something broke.
- **A permission card shows the arguments.** §3.2's whole gain over the argv
  path is that the prompt sees the *actual* call — allow `Bash(git status)`,
  refuse `Bash(git push)`. A card showing only the tool name throws that away
  and trains the human to click Allow without reading. The first cut of this
  mock got it wrong; that is why it is written down.

## 7. Load: state the elision, never hide it

Two ceilings, both visible in the storm fixture:

- **400 rows** in the pane, oldest dropped, with a `[ring]` row stating how
  many rolled out and that the full transcript is on disk in the event log.
- **64 KiB per card output**, head dropped, with the byte count stated.

An elision the reader cannot see is a transcript that lies. Both numbers are
rendered, not logged.

**What these two do NOT cap, and S4 must.** The row ring bounds the number of
rows and `OUT_CAP` bounds one *tool card's* output, but an assistant `Text` or
`Thinking` block is a single uncapped text node — `onText`/`onThinking` append
every delta forever, with no ceiling and no elision notice. The storm fixture
stresses tool output only, so nothing here would catch it. One long turn
streaming tens of MB of deltas holds the whole string in the DOM and says
nothing about it, which is the same silent-lie failure §7 exists to prevent, one
block type over. Left unbuilt in S0 deliberately — a third ceiling is renderer
work, not mock work — and named here so S4 inherits it as a known gap rather
than discovering it in production.

The storm fixture found a real defect that reading the code did not: following
the live end read `scrollHeight` on **every appended row**, which forces a
synchronous layout per row and is O(n²) in the size of a burst — it froze the
tab outright on 461 calls. The follow is now coalesced to one scroll per frame.
That is the same argument §5.3's coalescer makes one level down: **a producer
may not make the consumer pay per event.** S4 inherits the constraint, not just
the fix.

## 8. Two projections, one log — settled

R1's `harness-adapters.md` §5.1 rendered a structured pane as **VT bytes into
the existing `OutputBuf` ring**, so `get_output`, termgrid replay, thumbnails,
`last_exit_tail` and C5 replay-on-attach kept working with no API change — and
it explicitly rejected "a DOM transcript view beside the terminal" because that
breaks all five. #2891 then raised the bar above that floor: "a designed surface
… not an xterm emulation of a chat log", and **a VT renderer cannot draw a
collapsible card, a fold animation, or a button.**

This mock was built while that tension was open, and **S1a settles it** — in
**PR #2942, which is not merged yet**, so until it lands `main`'s §5.1 still
carries R1's text and everything in this section is dated to that PR rather than
to the contract. Re-check it when #2942 merges; if the section changed in
review, this one changes with it.

As #2942 writes §5.1, it becomes *"Two projections, one log"*: the
`transcript::Renderer` VT projection keeps feeding the ring, so the ring
consumers are untouched, and the human gets a DOM renderer in the same grid cell
fed by `orch-pane-event` — which replaces the never-emitted
`orch-pane-transcript`. Its own argument, quoted from the design-note text the
PR adds rather than from the PR's summary of itself:

> The rejection was right about the CONSEQUENCE and wrong about the CHOICE it
> was forced into: keeping the ring and putting the human on a DOM surface are
> not alternatives, because the ring is fed from the log rather than from the
> screen.

**What that means for this design:** it is the shape the mock was already built
as, so nothing here changes. The consequences worth carrying forward:

- **The DOM is a projection, never the only copy.** Every block in §6 is derived
  from an event; nothing is scraped, and nothing renders from state the log does
  not carry.
- **`projectText()` is the other projection**, and the two must be kept honest by
  being generated from one source rather than by discipline. That is why it is a
  pure, DOM-free function and why **S2 owns it as the tested module** — the risk
  this shape creates is two projections drifting, and a shared log plus a tested
  projection is where it is closed.
- **S4 renders DOM in the pane's own grid cell** — not beside the terminal, not
  over it. Constraint 1 still holds by construction: there is no PTY behind a
  structured pane, and nothing in `render.js` measures or resizes one.

## 8b. One theme — settled

Also decided rather than open: orrerix stays **dark-only** for this surface. The
human's call, and §2 records what it rules out — no light ramp is invented here,
because inventing palette values would break `ui-redesign.md`'s rule that a hue
is not free to move between channels. Every colour is a `var(--token)` off one
copied block, so if a light theme ever lands app-wide this page inherits it by
swapping that block for an import.

## 9. What each later slice inherits

- **S2 (projection)** owns `projectText()` — the DOM-free event-log → render
  model, and the tested module. The block catalogue in §6 is its spec.
- **S4 (renderer)** owns `render.js` and `pane.css`: the warp gutter, the block
  shapes, the motion budget, the two ceilings in §7, and the coalescing
  constraint that §7's defect established.
- **S1b (`pi.rs`)** replaces `decode.js`. The mapping there is written against
  pi's real wire vocabulary, so the two should agree field for field; where
  they do not, the Rust is right and this file is a mock.
