---
title: To-do pane
layout: default
parent: Features
nav_order: 13
---

# To-do pane
{: .no_toc }

<details open markdown="block">
  <summary>On this page</summary>
  {: .text-delta }
- TOC
{:toc}
</details>

---

Press **`Alt+J`** for your task list, in a pane beside your work. It is a
to-do app with one thing no to-do app has: **the agents you launch write to the
same list**, and every row says which one touched it.

`Alt+J` opens a to-do pane in the current tab, or jumps to the one already
open. It is a pane, not a pop-up panel — it stays put while you work in the
panes beside it, and it never resizes your terminals.

## Two lists: Global and this project

The switch in the header picks which list you are looking at, and which list a
new task lands in.

- **`◐ Global`** — tasks that belong to you, not to a repository. It is there
  whatever the pane is pointed at.
- **`◆ <project>`** — the list for the project this pane sits in. The pane takes
  its project from the pane you pressed `Alt+J` from: the repository it is
  working in, or its folder. Click the `⋯` to see the full path, for when two
  checkouts share a folder name.

Press `g` to flip between them.

A pane with no folder — one you opened from a terminal sitting in your home
directory — shows Global only, and says so. That is not a broken pane: the
global list is the whole feature for anyone who does not want a per-project
one.

Two spellings of the same project are the same list. orrerix resolves the path
once, in one place, so a pane opened at `C:\Projects\loomux` and one opened at
`c:/projects/loomux/` do not quietly become two lists.

## Quick-add: type it the way you would say it

The bar under the view strip is where tasks come from. Type a line and the
parts orrerix understood light up as **chips to the right, before you press
Enter** — so you can see it read `fri 4pm` as a date and not as part of your
title.

```
ship release notes fri 4pm #release !!
```

| you type | it means |
|---|---|
| `today`, `tomorrow`, `tonight` | a due date |
| `mon` … `sun`, `friday` | the *next* one of those days |
| `next week`, `next fri`, `this fri` | further out, or this week's |
| `in 3 days`, `in 2 weeks` | a distance |
| `at 4pm`, `at 16:00`, `4:30pm` | a time — otherwise a bare date means 09:00 |
| `#tag` | a tag |
| `!`, `!!`, `!!!` | priority — `!!` and `!!!` embolden the row |
| `*` | important (the star) |
| `@myday` | put it in My Day |

Two rules are worth knowing because they are what make the parse trustworthy:

- **Anything it does not understand stays in your title, whole.** It never eats
  a word it could not read.
- **A line that starts with a weekday is a title.** `Friday retro notes` keeps
  every word; `call the vendor fri` gets the date. A bare weekday is a date
  only when something comes before it — which is where a date actually turns up
  in a sentence someone types.

And a weekday means the *next* one: `fri` on a Friday is a week away. If you
meant today you would have typed `today`, and a task that lands silently in the
past hour is worse than one a week out.

## The five views

The strip across the top, with the live count on each. Press `1`–`5`.

| view | what is in it |
|---|---|
| **My Day** | what you picked for today (`t` on a row, or `@myday`) |
| **Planned** | everything with a due date, grouped **Overdue · Today · Tomorrow · This week · Later** |
| **Important** | everything starred |
| **All** | every open task |
| **Completed** | finished, most recent first |

The counts on the chips are the size of each **view**, not of what your search
matched — they are there to tell you how much work exists, so they do not move
as you type.

**A due date is only coloured when it is genuinely late.** Everything upcoming
is plain: if every date on screen were amber, none of them would mean anything.

## Rows, and opening one up

A row is a checkbox, the title, and one line of detail — the due date, `2/5`
step progress, tags, a sun for My Day, a `note` mark.

Click the chevron (or press `e`) and the row opens **in place**: its steps, a
field for the next one, its notes, and the My Day / Delete controls. It opens
inline rather than in a side panel on purpose — this pane can be a narrow grid
cell, and a detail panel at that width is a pop-up with extra steps.

Notes and a new step are saved by the **Save** button that appears once you
have typed something, or by pressing Enter in the step field. What you have
typed and not saved is never lost to an agent writing to an unrelated row.

## Who did what

**A coloured dot before the title means an agent touched that row**, in that
agent's role colour — the same colour that role has in the session list and the
group roster. Hover it for the agent's name; under the title, a faint
`worker-3 · 12m` says who and how long ago.

**A row with no dot is yours.** Absence is the human: colouring the common case
would make the marks mean "someone" instead of "which agent".

The dot says *who*, never *how it is going*. It is not a status light.

## Keyboard

The pane is drivable without the mouse. These fire when the list has focus —
typing into the quick-add or a note swallows them, which is why `n` and `/` are
the two ways in.

| key | what |
|---|---|
| `n` | quick-add |
| `/` | search |
| `j` / `k`, `↓` / `↑` | move the selection |
| `space` | complete / restore |
| `e` | open or close the row |
| `i` | important |
| `t` | My Day |
| `Del` | delete |
| `u` | undo the last change you made in this pane |
| `g` | switch Global ⇄ project |
| `1`–`5` | the five views |
| `Shift+↑` / `Shift+↓` | move the task up or down |
| `Esc` | close the row, then clear the tag filter, then drop the selection |

`Shift+↑`/`↓` rather than `Alt+↑`/`↓` because orrerix already uses `Alt+`arrows
to move focus between panes.

## Searching and tags

`/` filters on whole words across titles, notes and tags — `rel note` matches a
row containing both, and it is substring matching, not fuzzy: searching `omai`
will not find `domain`. Click a `#tag` on a row, or one in the footer, to see
only that tag; click it again, or press `Esc`, to clear it.

The tag rail in the footer shows the tags on your open tasks in this list — not
just the ones you can currently see, so you can always use it to widen a
filter.

## Long lists

The pane draws at most 200 rows at a time and **tells you when there are more**
(`52 more not shown — narrow with / or a #tag`). The count chip in the header
always carries the real total. A pane that quietly stopped at 200 would be
lying about how much work there is.

## What agents can do to your list

Agents you launch get seven tools — list, get, add, update, complete, delete
and restore — and they reach **the global list and their own project's list**,
never another project's. Restore is there so an agent can put back something it
deleted by mistake; their instructions say that a row **you** deleted stays
deleted unless you ask for it back. Every write shows up in your pane immediately, with the agent's dot
on it, and every one is in that group's audit log.

The list lives outside any repository, so nothing here can end up in a commit.

## Reminders

**A task with a reminder time nudges you when it arrives**, as a small toast
with a **Show** button that takes you straight to the row. A task with only a
due date nudges you when the date arrives. Set either from the quick-add
(`fri 4pm`) or from the due field inside an open row.

Three things are worth knowing, because they are choices rather than gaps:

- **A task you have already ticked off never nudges you.** If you finish
  something at 3pm its 4pm reminder does not arrive.
- **Reminders live in this window, not in your list.** Nothing is written when
  one fires, so if you have orrerix open twice you will see it twice, and
  reloading starts the day's reminders over. The upside is that nothing about
  reminders can change your list.
- **You will not get a wall of them.** Anything more than four hours late is
  dropped rather than shown when you open the pane, and several tasks coming
  due at the same moment arrive as **one** notice naming the first two and
  counting the rest — **Show first** opens the soonest.

## Undo

**`u` undoes the last change you made in this pane** — completing something,
deleting it, starring it, moving it to My Day, archiving, editing a note.

**Three of those also offer you a button:** completing, deleting and archiving
each show a toast with an **Undo** on it, because those are the three that make
something disappear. The rest are still undoable with `u`; they just do not
interrupt to say so, which is what keeps the toast worth reading when it does
appear.

It remembers your last 50 changes, and only yours: changes an agent made are
not on your undo stack, and switching between Global and your project starts a
fresh one. If an undo cannot be done — you deleted something more than 30 days
ago, or the list is full — it tells you why rather than quietly doing nothing.

## Finishing with a list: archive

Open **Completed** and there is an **Archive** button with a count on it. It
puts those finished tasks away: they leave every view, including Completed, and
the list is yours again.

They are **not deleted**. **Show archived** beside the button brings them back
on screen, and unarchiving one is an ordinary undo. Deleted tasks are the other
thing — those become a 30-day tombstone and then really are gone.

## My Day empties overnight

**A task you put in My Day today leaves it at midnight**, the way Microsoft To
Do works. The task itself is untouched — it is still in All, still has its due
date, still has everything you wrote on it — it has simply stopped being
*today's*. Press `t` to put it back.

Midnight means midnight where you are, on the day you are actually having: a
25-hour or 23-hour clock-change day still ends at its own midnight.

## Not here yet

Being honest about the edges, since a button that silently does nothing is
worse than no button:

- **Reordering** works until a list runs out of room between two tasks, at
  which point it says so instead of silently not moving the row.
- **Reminders are a toast in orrerix**, not a desktop notification — if the
  window is not open you will not see one.
- **Redo** is not there. Undo walks backwards only.
