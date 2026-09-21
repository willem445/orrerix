# Watched panes: the human's own mark on a pane (#3319)

Status: implemented (#3320). Sibling notes: `attention-provider-limit.md` and
the rest of the attention family — this is the mark that sits *beside* those and
is deliberately not one of them.

The ask, verbatim, because the whole design follows from the second sentence:

> *"I want to easily right click and highlight panes that are 'active'. The
> problem I have now is I have some agents that are just idle but I keep them
> open in case I need them in the near future and then other panes I'm actively
> working with and waiting for results. But when the results come back after I
> step away and come back, it's mixed in with all of the other idle panes and
> it's hard for me to remember which panes I need to actively look at."*

The thing being remembered is not a property of the pane. It is a decision the
human made about the pane, before the results arrived, and the reason it has to
be stored is that a human's memory of it does not survive stepping away. Every
choice below is downstream of that.

## Why it is not another `AttentionReason`

The obvious cheap implementation is a tenth entry in `attention.ts`'s `LABELS`.
It is wrong, and the way it is wrong is instructive.

`attention.ts` answers **what does the agent need**. Every reason in it is
*derived*: `provider-limit` and `stranded` are read out of pane text by
`attention_tick`, `report` and `question` arrive because an agent called an MCP
tool, `held-dialog` comes off the delivery record. Each is cleared by whatever
caused it — the agent gets its turn, the prompt lands, the hold resolves. The
whole subsystem is a *reading*, continuously re-taken.

`watched` answers **which panes did the human name**, is set by exactly one
human gesture, and is cleared by exactly one human gesture. Putting it in the
attention table would give the attention pass a value it must never overwrite
and a clear path it must never take, which is a special case in the one place
the design has kept free of them. It would also make the two inexpressible
together, and a watched pane that goes `blocked` showing both marks at once is
the case this feature exists for: *that* is the pane you came back for, and it
has news.

So they are two fields, on two axes, rendered as two marks. `PaneFacts` carries
both side by side and `AgentRow` keeps `watched` out of the `state` ladder for
the same reason — the ladder has one slot, and "watched" competing with
"blocked" for it would lose exactly the information the human wanted.

## The word

`watched`. Three were considered.

- **"active"** is the human's own word in the ask, and it is the one word that
  cannot be used. Every pane with a live process is active; the ask is about
  telling *active agents* apart from *idle* ones, so the word is already spoken
  for by the thing being distinguished from.
- **"pinned"** is taken twice over — the task board pins, and a pin in a tabbed
  app conventionally means "do not close this", which this does not mean.
- **"starred" / "flagged"** are generic bookmark words with no opinion. They
  would be fine.

`watched` wins on one thing the others lack: it names an ongoing relationship
rather than a stored bit. You watch a pane *because you are waiting on it*,
which is the human's actual sentence, and it reads correctly in the negative
("stop watching") without inventing a word.

## The colour, which is a measurement

`--mark-watched` is violet (`#9a8fc4`). The requirement is AC2's — the mark
must read *beside* the attention chip, not fight it — and that is not a taste
question, because this repo's theme tests already own the instrument. Measured
at `ed4375a5`, worst-case ΔE from every state dye **and** the accent, across
normal vision and all three CVD simulations:

| candidate | hex | worst ΔE | worst case |
| --- | --- | --- | --- |
| **violet** | `#9a8fc4` | **20.5** | held / tritan |
| lime | `#b5bf62` | 11.0 | attention / protan |
| cyan | `#5aa8b5` | 6.3 | ok / tritan |
| azure | `#6f93c4` | 4.7 | ok / tritan |
| orchid | `#c47f9e` | 4.3 | ok / tritan |

The state channel's own floor is 9. Violet is the only candidate with room to
spare; cyan, azure and orchid would have been a mark a colour-blind supervisor
could not tell from "done" or "broken".

**Why not the accent.** Gold is the interaction channel's one pigment, and
"what the human can act on" is a defensible reading of a human's own bookmark.
It is refused on the same instrument: gold sits **12.8 ΔE from amber**, which
clears the floor but puts the human's mark and the agent's attention on
adjacent hues — and gold already means *focus*, on every focused pane. One
pigment cannot say both "you are here" and "come back here" while staying
useful for either.

**The cost, which is real.** This is violet's second meaning: it is also
`--id-violet` (the fleet icon, the reviewer and PR badges, the review status,
the group timeline lane). The rule the three-channel design enforces is that no
identity-only hue may fill a **state** role, and watched is not a state — which
is why the token takes a `--mark-` prefix rather than `--state-`, keeping it out
of `STATE_DYES` by construction rather than by naming discipline. None of
`--id-violet`'s positions is a pane frame, a dock chip, a tab or an agents row,
so no surface shows both meanings at once. Minting a ninth palette hue was the
alternative and is worse: `theme.ts` §PALETTE already records that eight hues on
this ground cannot all survive CVD.

**Form, not just hue.** Attention is amber and *pulses*; watched is violet and
is *still*. A watch can stay set for an afternoon by design, and a mark that
flashed for an afternoon would be noise. The frame treatment is an inset bar
down the pane's own left edge — not the header edge, which is already spoken
for twice (the group colour on its left inset, the attention dye on its bottom),
and not a ring, which is the focus affordance.

## The chords

`Alt+H` toggles, `Ctrl+Shift+H` walks. Both carry their clearance evidence in
`src/shortcuts.ts` as comments, per the `agent-cli-reference` discipline, with
the references fetched for this slice rather than carried from #3263.

`Alt+H` is **H for highlight**, the human's own verb. It is free of documented
defaults in Claude Code, Copilot CLI, opencode and pi, and readline leaves `\eh`
unbound. Its one collision is pi's published *Vim Example* config, which a user
opts into by hand — the same standing `Alt+J` already ships with.

`Ctrl+Shift+H` sits in the Ctrl+Shift block rather than the Alt one because
that is where this app's cross-tab navigation already lives, and this chord
leaves the tab you are on. **Plain `Ctrl+H` is the hazard**, not this chord:
Claude Code lists it under "Reserved shortcuts" (the ASCII backspace byte) and
Copilot CLI binds it to delete-previous-character. The `e.shiftKey` requirement
on the block is what keeps loomux out of it, and `test/shortcuts.test.ts` pins
that plain `Ctrl+H` reaches the pane — because the guard is invisible at the one
line that adds the chord.

Codex documents no Alt or Ctrl+Shift binding at all, and its shortcut list is
prose rather than a declared-complete table, so both chords are **UNVERIFIED**
against Codex rather than confirmed free. A reference that lists no rows is not
evidence of no conflict.

## Where the state lives

On `PersistedPane`, the pane's own restore record.

The flag's lifetime has to match the pane's: it must survive the pane's respawn
(the record is what a dormant placeholder re-emits) and an app restart (the
record rides `tabs.json`), and it must die with the pane, because a mark on a
pane that no longer exists is not a mark on anything. The record is the one
object with exactly that lifetime.

The alternative was a keyed store on the `boardprefs.ts` pattern. It would have
needed a key that outlives a pane (there isn't one — `Pane.key` is a per-window
counter and `ptyId` changes on every respawn), a new Rust command pair in
`uistate.rs`, and its own answer to "the pane this names is gone". None of that
buys anything the ask contains.

Consequences worth naming:

- **No Rust change and no schema bump.** `uistate.rs` treats the blob as an
  opaque string, and the decoder's forward-compat rule is that an absent field
  reads as its pre-feature default. A pre-#3319 snapshot decodes to `false`.
- **Default-OFF on malformed input**, the polarity `lead` takes. A snapshot that
  invented a watch would put a violet bar on a pane nobody marked, and since
  nothing auto-clears, the human would have to hunt it down.
- **Not gated on `paneKind`**, unlike every launch field beside it. Those
  describe how to bring a pane back and are meaningless on another kind; this
  describes what the human decided, and they can decide it about any pane they
  can see.
- **A dormant placeholder merges rather than re-emits.** `Pane.capture()`
  returns `{ ...dormantRecord }` verbatim for a dormant pane; the watch is
  spliced over it, because a Reconnect card is exactly the kind of pane someone
  marks and the verbatim path would drop a fresh toggle silently.

## The overview gesture, and why it is two things

AC4 asked for the cheapest honest form and offered three. The answer is two of
them, because they answer different questions.

`Ctrl+Shift+H` answers **"take me there"** — one keypress, no chrome, no panel,
no persisted filter state, and it crosses tabs and un-minimizes. This is the
gesture for the moment described in the ask: you have just sat back down.

The **watched filter chip** in the Agents tab answers **"show me the list"** —
which of them, together, with what each agent is doing. It is a widening of an
existing control (`AgentFilter` gains one member) rather than new surface, and
it is offered only once something is watched.

A third panel was never a candidate: constraint 1's two-panel rule forbids it,
and nothing here needs one.

The chip is the one place `AgentFilter` stops being the state ladder, so
`matchesFilter` matches it on `row.watched` rather than smuggling a pseudo-state
into `row.state`, and `emptyMessage` needs its own sentence — "You are not
watching any panes", because watching is something you *do* to a pane and does
not fit "No panes are X". The compiler found that one: widening the union made
the index into `Record<AgentState, string>` an error rather than a sentence that
would have rendered "No panes are undefined."

## Clearing is the human's, and how that is enforced

AC5. Nothing clears a watch but a human gesture — not focus, not a report
arriving, not output going quiet, not a respawn.

**No opt-in "clear when I focus it" preference is offered.** It was considered
and declined: it is a new persisted preference, a new settings surface, and a
second rule for when a mark disappears, bought for a behaviour the human framed
as theirs. If it turns out to be wanted, it is a small follow-up; shipping it
speculatively would have made the feature's one promise conditional on a setting
nobody asked for.

The enforcement is the part worth recording, because the promise is about code
that does not exist. Every unit test here passes just as well on the day someone
adds `pane.setWatched(false)` to the focus handler — the model is correct either
way, and the DOM wiring has no test at all, because this repo validates DOM
wiring by hand. A promise like that goes false silently, one slice later, under
a green suite.

So `test/watchedwiring.test.ts` scans the source: `setWatched` is the one writer
(`isWatched` is `private`, which the compiler enforces), its callers are
default-denied against an allowlist with a reason per file, the passive paths
(`attention.ts`, `panefocus.ts`, `paneactivity.ts`, `grid.ts`, …) are asserted
clean, and the two restore calls are pinned to the literal `true` — because a
restore that passed `record.watched` unconditionally would be a silent
auto-clear the moment anything can set a watch before restore finishes.

A mutation round confirms it bites: writing an auto-clear into `src/panefocus.ts`
reddens that guard and **nothing else**, with `tsc` clean. The compiler has no
opinion on an auto-clear, which is exactly why the scan is there.

The residual is stated in that file: the scan is textual, so an aliased or
computed call is invisible to it; it bounds where the flag is written, never how
long a write lasts. What makes that enough is `private isWatched` — a second
writer outside `pane.ts` cannot compile, and one inside it fails the scan.

## What is not marked, and why

The issue asked for "the side dock / sessions list where a pane appears". Both
turn out not to be pane lists:

- **The side dock** (`sidedock.ts`) is the git/files/editor docked panel. It has
  no list of panes at all — it follows the active one.
- **The session browser** (`sessions.ts`) lists persisted CLI *sessions on disk*,
  not live panes. A watch is on a pane, and a session row may correspond to no
  open pane, or to one that has not been opened yet.

The surface that really lists panes is the left panel's **Agents tab**
(`agentsview.ts` over `agentrows.ts`), and that is where the row mark and the
filter chip went. The tab strip gets a per-tab **count** rather than a mark,
because a tab is not a pane and "one of these" and "four of these" are different
amounts of reason to switch.
