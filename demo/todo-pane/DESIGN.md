# The To-Do pane — visual language

The decisions behind `demo/todo-pane`, written for S4 to inherit rather than
re-derive. Where a decision was a choice between two defensible shapes, the
rejected one is named: a design note that only records what was done cannot
stop the next slice quietly undoing it.

The human's brief for the look was "a Microsoft To Do replacement with better
features". The two things that makes concrete are below, and everything else in
this note follows from them:

- **The list has to feel like a list**, not like a database view. One column,
  one line of meta, generous hit targets, and detail that opens where you are.
- **The better features are agent-awareness and the keyboard.** MS To Do has
  neither. This pane has to say which agent touched a row, and it has to be
  fully drivable without the mouse.

## 1. What the surface is for

A grid-cell pane that answers "what am I doing next" for the human, in a tool
where **agents write to the same list**. That second half is what makes it a
different product from a to-do app: a row can change while you are looking at
it, and it can change because something you launched decided to.

Two consequences run through the whole design:

- **Attribution is first-class, not a detail view.** Which agent last touched a
  row is on the row, at rest, in the identity channel (§2).
- **No un-submitted value lives in an element** (§7). An agent's write causes a
  re-render, and a re-render must not be able to eat a half-typed note.

The pane is a **grid cell**, not an overlay and not a dock tab. It never
resizes a PTY — nothing in here can: it is a DOM view in a cell a terminal
would otherwise occupy, which is `CLAUDE.md` constraint 1 obeyed structurally
rather than by discipline.

## 2. Colour: the app's tokens, copied, and one channel per question

`todo.css`'s `:root` is a **verbatim copy** of `src/styles.css`'s token layer —
64 declarations, value for value — so the demo is a standalone page with no
build step. It is a copy and it can drift; `src/styles.css` is the source of
truth and `test/theme.test.ts` pins that file, not this one. Below `:root`
there is **no raw colour**, which is the app's own rule held here by hand
because no test watches this directory.

Dark only. There is no light block, and inventing one would mean inventing
palette values #3263 never sanctioned.

The three channels answer three different questions, and this pane uses each in
exactly one kind of position:

| channel | question | where it appears here |
|---|---|---|
| **state** `--state-*` | what is this thing doing | `--state-attention` on an **overdue** due date, and on the Overdue bucket heading. Nothing else. |
| **interaction** `--accent` | what can I act on / what did I pick | the focus ring, the checkbox ring and tick, the star when set, the active view chip's underline, the quick-add's left edge when the line is submittable, the `!!!` left edge |
| **identity** `--id-*` | which thing is this | the 6 px attribution dot, and only that |

Three rules fall out of the table, and each one is a thing a later slice will
be tempted to break:

**A to-do has no agent state, so five of the six state dyes never appear.** A
task is not working, held, idle or ok — it is due, or it is not. Giving
`--state-working` to an in-progress task would put a second meaning on a
pigment the fleet already reads as "an agent is running", and the supervisor
looking at a grid of panes is the person who pays for that.

**The 2 px warp thread is NOT reused.** `ui-redesign.md` gives every pane a 2 px
left thread whose colour is its live agent state, and the structured-pane mock
runs the same device down its transcript gutter as a second scale. This pane
deliberately does not: there is no agent state here to carry, and a thread in
the same position carrying something *else* would break the one thing the warp
is for — that its colour always answers the same question. Where this pane does
use a 2 px left edge (`!!!` priority, the submittable quick-add), it is in the
**accent**, which is visibly not a state dye, and it is an emphasis mark rather
than a status.

**Gold marks, it never grounds.** The checkbox is an accent *ring* with an
accent *tick* — never a gold fill. A list of thirty unchecked rows is a column
of hairline circles; a list of thirty gold-filled ones is a gold pane. The
selected row's ground is `--selection`, which `src/styles.css` made achromatic
for exactly this reason.

**Overdue is the only dye, and only when it is genuinely late.** A due date
that is merely soon is ink. If everything upcoming is amber, nothing is.

## 3. The attribution mark

A 6 px dot before the title, in the touching agent's `--id-*` hue, with the
agent's name and CLI on hover. Under the title, when an agent touched the row,
a faint `worker-3 · 12m` in `--ink-faint`.

**A human-authored row has no dot.** Absence is the human. The alternative —
a "human" hue — would spend an identity pigment on the majority case and make
the marks mean "everyone" instead of "which agent".

The dot is in the **identity** channel and may never be read as a status. It
says *who*, never *how it is going*. A later slice wanting to show that an
agent is mid-edit on a row needs a different position, not this one's colour.

The hue comes from the item's actor record, not from a hash of the name: an
agent's hue is assigned once, in one place, so the same agent is the same
colour in the pane header, the session list and here.

## 4. The quick-add bar, and why the chips exist

The bar is pinned under the view strip and is the pane's centre of gravity. As
you type, `quickadd.js` parses the line and the consumed tokens come back as
**chips to the right of the field**, before you press Enter.

The chips are not decoration; they are the parser's evidence. The failure that
makes a natural-language quick-add untrustworthy is the silent one: you type a
title, a word of it is read as a date, and the title you get is not the title
you typed. So the parser has two rules that the chips make checkable:

- **It never consumes a token it did not understand.** Unparseable text stays
  in the title, whole.
- **A line that OPENS with a weekday is a title.** `Friday retro notes` keeps
  every word; `call the vendor fri` does not. A bare weekday is a date only
  when something precedes it, which is where a date actually appears in a
  sentence someone types.

Two more parser decisions worth inheriting: `fri` on a Friday means **next**
Friday (if you meant today you would have typed `today`, and a task that lands
silently in the past hour is worse than one a week out), and a bare date takes
**09:00** while `at 4pm` takes what you said.

The clock is **injected** (`parseQuickAdd(text, nowMs)`), never read from the
host. A parser that reads the wall clock cannot be tested for "tomorrow"
without the test being a different test every day — and the demo's 09:00 /
14:00 / 23:00 buttons work precisely because nothing underneath them calls
`Date.now()`.

**In an empty view, the quick-add IS the empty state.** There is nothing else
to reach for, so the empty message points at it rather than drawing a picture.

## 5. Rows, and why detail expands inline

A row is: the checkbox, the attribution dot, the title, one meta line (due,
`2/5` steps, tags, the My Day sun, a `note` mark), and — on hover, selection or
focus — the star and the chevron.

Detail opens **inline, in place**: steps, a next-step field, notes, and the
due / My Day / Delete controls. It is not a side panel, and that is a
constraint rather than a preference: the pane can be a 320 px grid cell, and a
detail panel at that width is a modal with extra steps. The demo shows both a
320 px cell and a full tile side by side, driven from one state object, so the
claim is checkable rather than asserted.

Priority is **weight**, not a third colour: `!!` and `!!!` bold the title and
`!!!` additionally takes the accent left edge. Inventing a priority hue would
mean a fourth channel in a pane that already has three.

## 6. The keyboard map

The issue's requirement is "without the mouse", so every action on screen has a
key, and the footer carries the five most-used as a reminder.

| key | what |
|---|---|
| `n` | focus the quick-add |
| `/` | focus (and expand) search |
| `j` / `k`, `Down` / `Up` | move the selection |
| `space` | complete / restore the selected row |
| `e` | expand or collapse the selected row |
| `i` | important |
| `t` | My Day |
| `d` | expand the selected row, where the due control is |
| `Del` | delete (soft — `u` brings it back) |
| `u` | undo |
| `g` | toggle scope, Global ⇄ workspace |
| `1`–`5` | My Day, Planned, Important, All, Completed |
| `Alt+Up` / `Alt+Down` | reorder within the view |
| `Esc` | collapse, clear the tag filter, drop the selection |

Single letters, unmodified, because the pane has no text field focused when
they fire — typing into the quick-add or a note swallows them, which is the
behaviour you want and is why `n` and `/` are the two ways in.

**The chord that opens the pane is NOT decided here.** The plan leaves it open
between `Alt+H` and `Alt+J`, both to be checked against the
`agent-cli-reference` discipline before one is committed to (`Alt+D` and
`Alt+L`/`N`/`U` are readline-bound; `Alt+T` is taken). The PR asks the human.

## 7. State lives in the view, never in an element

`main.js` holds one state object and rebuilds the DOM from it wholesale on
every change. That is `CLAUDE.md`'s in-list-editor rule, and it is here at S0
so S4 inherits the shape instead of discovering it: the board's own lesson is
that a list which re-renders on every agent write will eat an un-submitted
value the moment that value lives in an `<input>` rather than in the view.

The costs are paid in one place each: the caret is restored once, centrally,
after each render; a note is written to the model **on input**, not read at
submit.

Undo is an **inverse-op stack**, 50 deep, not a pile of snapshots. Every
mutation pushes the op that undoes it, and soft delete is what makes deletion
invertible at all. The inverse carries the **attribution** too — undoing an
agent's completed row must put the agent's dot back, not leave the row looking
human-authored.

## 8. Load: state the elision, never hide it

The list builds at most `ROW_BUDGET` (200) rows and prints what it is not
showing: `52 more not shown — narrow with / or a #tag`. The `busy` fixture's
500 items exist to make that visible rather than describe it, and the count
chip in the header always carries the real total.

This is a **budget, not a virtual scroller**. A scroller is an argument for a
later slice to make with measurements in hand; a budget is the honest thing to
put in front of a human at S0, and the elision line is what keeps it honest —
a pane that silently stops at 200 rows has lied about how much work there is.

Measured in the demo at the `busy` fixture, both cells rendering at once:
narrowing 252 rows to 16 with a search term took 8.2 ms; widening back to the
full 200-row window took 32.6 ms.

## 9. Motion

State changes only. Three animations, each with a job:

- **The completion strike** — the row strikes, dims and slides out over 600 ms.
  The duration is the window in which the undo toast is still the obvious thing
  to reach for, which is what it is for.
- **The chip-in** — a parsed chip fades in from 4 px left, at `--dur-base`. It
  is what makes the parse feel like it is reading you.
- **The reminder pulse** — the one continuous cue in the pane, two cycles of
  the attention dye on a row's left edge.

`prefers-reduced-motion` collapses every duration to 1 ms, and the demo's
Motion toggle does the same thing through a data attribute so the reduced
reading can be seen without touching an OS setting. There is **one** code path
either way: the completion commit runs on a timer in both cases, so there is no
second path to keep honest.

## 10. Type

From the app's `FONT` roles, unchanged. Sans (`--font-ui`) for titles, labels
and prose. Mono (`--font-mono`) for **machine identifiers and quantities** —
due timestamps, `2/5` step counts, the view-strip counts, the count chip, the
workspace path. The rule the app already holds is that mono means "a literal
string the machine gave you", and a due date is one.

Sizes: `--text-m` for titles, `--text-s` for steps and the search field,
`--text-xs` for meta and chips, `--text-eyebrow` with tracking for the bucket
headings. Nothing new was introduced.

## 11. Scope: Global ⇄ workspace

The header's segmented control selects **which list you are looking at** —
`◐ Global` or `◆ <folder name>` — and it also selects which list a quick-add
lands in. One switch, one meaning, and the `⋯` reveals the workspace's root
path for the case where two checkouts share a folder name.

It is a switch, not a merge. A combined view would need per-row scope marks and
a rule for what a quick-add does, and neither is worth spending on before the
human has used the two-list version. That is a deliberate deferral, not an
oversight — if the human wants a merged "All lists" position, it is a third
segment and the counts already support it.

## 12. What each later slice inherits

| slice | takes |
|---|---|
| **S3** | `quickadd.js` almost verbatim as `src/todoquickadd.ts`, and `render.js`'s `project()` as the core of `src/todomodel.ts` — the view predicates, the Planned buckets, the ordering and the search |
| **S4** | `todo.css` below `:root` as the basis of the pane's styles in `src/styles.css` (tokens only, so `test/theme.test.ts` stays green), the DOM shapes in `renderInto`, the keyboard map in §6, and the state discipline in §7 |
| **S5** | §9's motion, the undo stack's shape, the archive control, and the `ROW_BUDGET` decision in §8 — which is the point at which a virtual scroller either earns its place or does not |

Nothing downstream inherits `main.js`, `index.html`, or the fixtures.
