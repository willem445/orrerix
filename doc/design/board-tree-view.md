# Filtering the board as a tree, and where view state lives (#1270)

The board that prompted #1152 carried 400+ rows. Sinking and the cleared archive
answered *"most of this is finished"*; they do not answer *"where is the auth
thing"* or *"show me the blocked stories"*. #1270 adds the tree-view controls
that do — collapse-all/expand-all, four filter families (a fifth, `sprint`,
arrived with #1272's board UI), and a search box — and
makes the collapse state and the filters **durable**, which is the part with a
design decision in it.

Read alongside `board-order-and-archive.md` (#1152), whose argument about what
is board data and what is not this note continues and partly amends, and
`task-hierarchy.md` (#958/#1156) for the containment model everything here
projects.

## Filtering a tree is not `board.filter(pred)`

Three rules, all in `taskboard.ts`'s filter section, all pinned in
`test/taskboard.test.ts`.

### 1. A match keeps its ancestor chain

A matching row renders, and so does every container above it, flagged
`BoardRow.context` so the view can draw it as scaffolding rather than as a hit.

Without this, `kind=story` returns a flat list and the containment the whole
#1156 hierarchy model exists to show is gone from the one view that shows it —
the human is handed six stories with no way to tell which feature each belongs
to. Dimming them, rather than styling them like hits, is what stops a filtered
board reading as *"every epic matched too"*.

Descendants of a match are deliberately **not** pulled in. An epic matching
`kind=epic` renders alone; what is inside it is named by its `done/total` chip,
not by dragging forty rows onto a screen the human just asked to narrow.

### 2. An active filter overrides collapse, and never mutates it

While any family is armed, `visibleRows` ignores the collapsed set entirely.

This is not merely the conventional tree-view behaviour (though it is that — a
search that finds a row and then hides it inside a folded container is worse
than no search). Under rule 1 it is the only coherent option: a kept container
either has a kept descendant, in which case it MUST expand to show it, or is a
match whose whole subtree was filtered out, in which case folding it changes
nothing. **Collapse has no observable effect while a filter is active.**

So the per-row chevron and ⊟/⊞ render *inert* rather than as dead clicks, and
the stored set is untouched — clearing the filter restores the exact shape the
human left. The same test asserts both halves, because "we did not mutate it" is
only checkable by re-rendering with the same set afterwards.

### 3. AND across families, OR within one

`kind ∈ {epic, feature}` AND `status ∈ {blocked}` AND the title or id contains
`auth`. An empty family constrains nothing; it never means "match nothing".

Two smaller decisions inside that:

- **`unlabelled` is a first-class chip.** A row with no `kind` is legal and
  permanently exempt from the ladder (#1156), so without a chip it would be the
  one class of row the level filter cannot name. An empty-string `kind` reads as
  unlabelled too (`||`, not `??`) rather than becoming an invisible fifth class.
- **`kindFilterChoices` derives its tail from the board.** `ladderRule` exempts
  an out-of-vocabulary kind on purpose (CLAUDE.md constraint 8 — Orrerix must not
  require a methodology), so a hand-edited `tasks.json` may legitimately carry
  `saga`. A fixed chip row would leave such a row matching neither a ladder level
  nor `unlabelled` — it *is* labelled — and reachable only by clearing the level
  filter entirely.

### Where the archive and the filter meet

An archived row cannot match while the archive is off screen. Clearing is board
data (#1152) and filtering is a view; the two compose rather than overriding each
other, so a hit inside the archive does not drag the archive back on screen
behind the human's back — 👁 is what does that, and the same needle finds it once
they click it.

There is no hole under that: `clearedIds` only archives a row whose whole subtree
is cleared too, so an archived container can never sit above an un-archived
match.

### One rule per input

`buildSieve` takes `archived` as a parameter and `visibleRows` computes it once,
which is why there is **no exported `filterSieve(board, filter, attention,
showCleared)`** for the view to call. Two callers each passing `showCleared` to a
different place is exactly the asymmetry CLAUDE.md's one-rule-per-input
convention is about: the two would disagree precisely where they differ, and the
bug would live in the gap.

The same reasoning puts the `attention` id set on the *view* side. The pure
module never learns what a question or a demo gate is — it receives an opaque
`ReadonlySet<string>` — and the view derives it from `boardMarker`, the same rule
the ❓/👀 marker chips are drawn from, so the toggle and the chips cannot
disagree about which rows are waiting on the human.

## Where the durable view state lives

**`boardprefs.json`**, an app-global sibling of `tabs.json` / `settings.json` /
`sshprofiles.json` under the app data dir, holding one record per group;
`src/boardprefs.ts` owns the schema and `uistate.rs` stores the blob opaquely.

### Not on the task

#1152 put `cleared_ms` on the task and argued the line this sits on the far side
of: *"I have acknowledged this item and want it out of my working set"* is a
human-authored decision about the work item, so it is board data by the same test
`status` is. Collapse and filters are not that. Putting them on the task would
make every chevron click an audited board write handed to the orchestrator, for
a fact about one human's screen.

### Why the drift objection does not carry over

#1152 rejected a task-id-keyed sidecar partly because it **can drift** — delete a
task and its id lives on in the set. That objection is real and it does not bite
here, for a structural reason rather than a promise:

- **A stale id in a collapsed set is inert.** It names no container, so it
  collapses nothing. A stale `cleared_ms` sidecar entry would have *hidden a live
  row* — a wrong answer, not a no-op.
- **It is already self-healing.** `retainExisting` prunes the set to live rows on
  every board refresh, so the next save writes the dead ids out.

#1152's other objection — that a sidecar splits the audit story — does not apply
either, because there is no audit story to split: nothing here is auditable, by
design.

### Why a sibling file and not a key in `settings.json`

The reason #887 gave for `sshprofiles.json`: a multi-entry keyed structure with
its own lifecycle does not belong inside a flat bag of app-wide scalars, and
keeping them apart keeps both schemas simple.

### Why not the group dir

It would have tied the record's lifetime to the group's, which is genuinely
nicer. It was not worth what it costs: a per-group file means a group id
reaching a path, i.e. a new `#[tauri::command]` taking a group id, parsing it at
the boundary and joining it through `group_dir_at` — new surface on the one
constraint in this codebase with a source-scanning guard behind it (CLAUDE.md
constraint 6), spent on a view preference.

**As an app-global blob the group id is a JSON map key and never a path**, so
constraint 6's surface is untouched. The lifetime problem it trades for is
bounded by an LRU instead (below), and the worst case is that a board opens at
its defaults.

### Bounded by construction

Keyed by group, the file would otherwise grow forever — one record per group ever
opened, long after the group is gone. Nothing on the frontend can know that it is
gone (asking the orchestration registry would make a view preference depend on a
live backend read at save time), so `encodeBoardPrefs` keeps the **50 most
recently touched** groups and drops the rest.

Eviction at the *write*, not the read: a build that only ever loaded would let
the file grow on disk however small the in-memory map was, and the encoder is the
one function every write goes through. Falling off the end costs one board its
folds and filters — the pre-#1270 behaviour, not the loss of anything a human
authored.

### A new filter family is a key, not a migration

The persisted `filters` object is keyed by family. Adding one — a sprint filter
over #1272's `sprint`, a filter over #1273's `links` — is a new key plus one
clause in `matchesFilter`; no version bump, no migration.

**Both of those landed on `main` while this change was in review, and the seam
held as designed**: they added `sprint` and `links` to the board MODEL (on the
task, which is where assignment belongs — the same line #1152 drew) and neither
opened a second per-group view-state store, which is the collision this change's
plan comment flagged in advance. That is only true in *both* directions if a
build that does not know a key hands it back unchanged, so `decodeBoardPrefs`
keeps unknown families verbatim and `encodeBoardPrefs` writes them back
**before** the validated ones, where they cannot shadow a family this build owns.

**The extension point has since been spent once, and it cost what it claimed.**
#1272's board UI added `sprint` as the fifth family: one key on `BoardFilter`,
one clause in `matchesFilter`, the four persistence sites the `EncodedGroups`
type forces together, and nothing else — no version bump, no migration, no
second store. The one thing that did NOT follow the compiler was the tests that
had used `sprint` as their *specimen* for an unknown family; shipping the family
moved that specimen out of the class it was witnessing, and it was relocated
onto a name no `BoardFilter` key can take (`test/boardprefs.test.ts`).

The corollary for whoever builds sprint grouping: **sprint *assignment* is board
data and belongs on the task, like `status`; sprint *view state* belongs in this
record.** A second per-group UI-prefs store keyed the same way would be the drift
#1152 warned about, arriving through a different door.

### Nothing is published before the file has been read

The blob is ONE file for every group, so a save built from a store that was never
read publishes an empty map as the whole truth. That is not hypothetical: it is
what the first version of this change did, and it would have destroyed up to
`MAX_GROUPS` other groups' collapse sets and filters, silently, on nothing worse
than a cold start plus a fast click (#1270 review B1).

`BoardPrefsStore` holds the ordering, in `boardprefs.ts` with injected IO rather
than in the view — the invariant IS an ordering between two async calls, so
there is no single value to assert about, and a race parked in DOM wiring is a
race nobody can test. Precedent for the shape: `CoalescingRefresh`
(`refreshgate.ts`).

- Every `write` awaits the read.
- A read that **failed** declines the write outright rather than treating "I
  could not look" as "there was nothing there".
- That failure is **not latched** — the next gesture retries, so one transient
  rejection does not disable persistence for the life of the view.
- `read` answers `null` for an unreadable file, which a caller must not collapse
  into `defaultGroupView()`. Adopting defaults there would show an expanded,
  unfiltered board and then let the next gesture save that over what the human
  actually left.

### A live gesture beats the file

`loadPrefs` runs once and adopts nothing if the human has already changed the
view in this window. The disk copy is what they left last session; a chevron
clicked in this one is newer, and adopting the file over it would look like the
click was ignored.

Saves are debounced (400ms) and fire-and-forget — the `persistTabs` contract: a
failed write just means the last gesture is not durable until the next one, and
the store keeps the newer value so the next gesture re-offers it. The one
exception is `dispose`, which flushes a pending save, for the reason `flushTabs`
awaits the quit path: closing the board is the commonest way a session ends, and
there is no next gesture to retry on.

## What the count chip says now

The board has carried a `done/total` chip on every container since #958. What it
could not say is whether any of those children are **on screen** — `3/7` reads
identically whether all seven rows are underneath it or none are, which is
exactly what makes collapse-all unpleasant to use.

`BoardRow.shownKids` (how many of a row's direct children the projection actually
rendered) closes that: the chip picks up a dashed outline and an extended tooltip
whenever `shownKids < total`, naming the cause — folded up, or hidden by the
filter. The numbers themselves are unchanged and still the orchestrator's own
`children`/`children_done`, because the human's board and `list_tasks`
disagreeing about a count they both display would be a defect. An outline rather
than a colour, for the same reason: nothing about the *work* changed, and this is
a note about the view.

`shownKids` is counted off the rendered set after the walk, not re-derived from
the three rules that decide a row's fate (collapse, the archive, the filter), so
it cannot disagree with what is actually on the screen.

## The per-row priority ladder (#2937)

The board is read in a normal-width pane with the UI docked to the left, and in
that pane the one field a human needs from a row — its **name** — was the field
that lost. `.task-top` was a single non-wrapping flex line; every chip, badge
and button on it is `flex: none`; `.task-title` was the only flexible item on
it. So the name was the only thing that *could* give ground under pressure, and
it gave all of it. Reviewing what work was left meant hovering each row.

The fix is not a narrower chip. It is deciding, once and in one place, that most
of a row is **detail**, and putting that decision where it can be tested:
`src/boardrow.ts`.

### The ladder

Four rungs, in the human's own words on #2937:

| Rung | Fields | Where |
| --- | --- | --- |
| 1 | the task name, the task id | the compact line, always |
| 2 | the issue and PR chips | the compact line, always |
| 3 | the status control, a container's `children_done/children` | the compact line, always |
| 4 | everything else | behind the row's `⌄` |

Rung 4 is the long one: the assignee and session chips, the kind and sprint
badges, the *ready*, *all inside done*, *in `<missing parent>`* and *cleared*
markers, the ACTIVE badge and the *needs a decision* / *needs a look* deep link,
▶ Start, ✓ Approve, ✎ Changes, ▶ Proceed, the 🔗 ⤵ 🏷 🎯 📎 🗨 controls, ↩
restore and ✕ delete — plus the deps / see-also line and whichever picker is
open on it.

Two of those are the judgment calls, because #2937's own "everything else" list
does not name them: the **ACTIVE badge** and the **needs-a-decision marker**.
Both are rung 4, and the argument is that neither is the only carrier of its
signal — the ROW says active with a left accent, a glow and a pulse
(`.task-row-active`) and awaiting-human with its own left accent
(`.awaiting-human`), and those cost the name no horizontal room at all. The chip
names *who* and offers a deep link; the row already says *that*, which is what an
eye scanning for "what is left" is reading.

### The ladder is the render order, not a description of it

`renderTask` does not append fields to the line. It **files** each one under its
ladder slot — `place("title", node)` — and two loops at the end of the method
put the slots on screen in `rowLayout`'s order. That is what keeps `boardrow.ts`
load-bearing rather than a second, separately-tested description of an order a
3000-line render method really decides; it also means the name can never be
pushed right by something ranked below it, whatever order the code above happens
to construct things in.

`RowField` is an exhaustive union and the ladder is a `Record<RowField, RowTier>`
over it, so a field added without a rung is a `tsc` error rather than a field
that silently renders nowhere. What the compiler cannot see — a field given a
rung and then left out of both render orders — is what
`every field is placed exactly once, on exactly one rung` pins.

### `.task-top` wraps, and the wrap point is the ladder's

The line is built id → name → issue/PR → status → progress → `⌄`, and
`flex-wrap: wrap` means the first thing a narrow pane pushes onto a second line
is precisely what the ladder ranks below the name. `align-items: baseline`
rather than `center`, because the name now wraps to two or three lines and the
id has to sit on its **first** one. The title takes `overflow-wrap: anywhere` so
a long unbroken token (a path, a branch name) breaks rather than forcing the row
wider than the pane.

### Expanded rows are per-session view state

`TasksView.expandedRows` is the third `Set<string>` of its kind on this view,
and deliberately not a new meaning for either of the other two: `expanded`
(notes) and `expandedLinks` (groundings) answer different questions, and a human
who opened one is routinely not done with it when they shut another.

It is **view state, never DOM state.** The board re-renders on every
`write_tasks`, so reading "is this row open" back off an element would lose it
the first time an agent wrote to the board — the same rule the in-list editors
follow, for the same reason. It is pruned to live rows on every refresh beside
`selected`/`collapsed`/`expanded`, so a deleted row's id cannot accumulate for
the rest of the session.

Per-session rather than persisted in `boardprefs.json` like `collapsed`: an open
row says how you are reading the board *right now*, not how you want it set up.
The distinction the section above draws between board data and view state is
unaffected either way — this never touches the task.

### What is deliberately not gated on it

The 🗨 **notes** and 📎 **grounding** sections keep rendering on their own sets,
whichever way the row is folded. They are full-width blocks *below* the line, so
they cost the name no horizontal room — the complaint this issue is about is
horizontal — and gating them here would make a row's presence in `withNotes`
disagree with what is on screen, which is the wire invariant #1317 established.

The deps / see-also line **is** rung 4, with one carve-out: an open picker keeps
the line whichever way the row is folded, so a picker the human has just opened
can never become unreachable. Nothing can open one from a collapsed row — every
trigger is inside the detail block — so that covers the transient case only.

### Keyboard, and what the expand control is not

The control is a native `<button>`, which already synthesizes a `click` for
Enter and Space. So the key handler on it deliberately **does not toggle**:
doing so as well would fire twice and leave the row where it started. What it
does with an Enter or a Space is `stopPropagation()` and nothing more, so an
app-level shortcut cannot swallow the keystroke before the button acts on it.
It never calls `preventDefault()` — a button activates on Space at *keyup*, and
cancelling the keydown cancels the activation this is here to protect.

**Nothing here resizes a PTY.** The detail block is a child of `.task-main`,
inside the row, inside the overlay the board already occupied; an expanded row
grows downward and scrolls with the list. No sibling is added to `#grid-area`
and nothing in the layout moves. Hard constraint 1.

## What is not here

**Keyboard navigation of the tree** was on #1270's candidate list and is tracked
separately as **#1314**. Its cost is not in the pure module: a roving-tabindex
focus model has to compose with the multi-select tickboxes, inline title editing,
the four pickers, the request-changes modal, the two-click delete confirms and
`shortcuts.ts`'s global keybindings — all DOM wiring, which this repo validates by
hand. That is a large, low-coverage surface, and bolting it onto a change whose
value is a testable pure projection would have made both harder to review.

What this change leaves in place for it, so the split costs nothing:
`visibleRows` already returns the rendered rows **in display order** with
`depth`/`hasChildren`/`collapsed`/`shownKids` — the sequence arrow-key movement
has to walk, derived and tested; `containerIds` names every foldable row; each
row carries the `data-item-id` anchor `drainFocus` already scrolls to; and
collapse is durable per group, so a fold made from the keyboard persists like a
clicked one with no new persistence work. The open question #1314 records is what
left/right should mean while a filter is armed, since folding is inert then.

**Nothing here resizes a PTY.** The control strip is a flex child of
`.tasks-view`, inside the overlay (or the embed slot) the board already occupied;
it adds no sibling to `#grid-area` and moves nothing in the layout. Hard
constraint 1.
