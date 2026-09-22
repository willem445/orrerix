---
title: Watched panes
layout: default
parent: Features
nav_order: 14
---

# Watched panes
{: .no_toc }

<details open markdown="block">
  <summary>On this page</summary>
  {: .text-delta }
- TOC
{:toc}
</details>

---

Some panes you keep open just in case. Others you are actually waiting on. When
you step away and come back, they look the same.

**Watching** a pane marks it as one of the ones you came back for. Press
**`Alt+H`** on the pane you care about, or right-click its header and choose
**Watch this pane**. It grows a violet bar down its left edge, and the same mark
appears everywhere that pane is listed — so you can find it without opening
anything.

Press **`Ctrl+Shift+H`** to jump straight to the next watched pane, in any tab.

## Where the mark shows up

| Where | What you see |
| --- | --- |
| The pane itself | A violet bar down its left edge, and a `◉` in the header |
| The minimize dock | A violet `◉` on the chip, so minimizing one doesn't hide it |
| The tab strip | `◉3` on any tab holding watched panes |
| The Agents tab | A `◉` on the row, and a **watched** filter chip |

The mark is violet on purpose. The amber "needs attention" chip means *the
agent wants something*; violet means *you said to look at this*. They are
different colours because they are different facts, and a pane often carries
both at once.

## It stays until you clear it

Nothing clears a watch but you. Focusing the pane doesn't. The agent finishing
doesn't. Output going quiet doesn't. That is the point — a mark that cleared
itself while you were away would be gone by the time you got back.

It survives the pane restarting, and it survives closing and reopening orrerix:
the watch is saved with the pane's layout.

To clear one: `Alt+H` again, **Stop watching** on the right-click menu, or click
the `◉` in the pane's header.

## Finding them again

Two ways, and they answer different questions.

**`Ctrl+Shift+H`** — "take me there." It moves you to the next watched pane,
switching tabs and un-minimizing if it has to, and cycles round so pressing it
repeatedly visits all of them and comes back. If you are not watching anything,
it says so rather than moving you somewhere arbitrary.

**The watched chip in the Agents tab** — "show me the list." Open the left panel's
Agents tab and click **watched**; the list narrows to those panes, each still
showing what its agent is doing. The chip is only there once you are watching
something.

That list covers **agent panes**. The Agents tab is a list of agents, so a
watched shell or editor has no row there — if you have any, the list says so
underneath and points you at `Ctrl+Shift+H`. The chord and the tab-strip count
cover every pane; the Agents tab is the one place that does not.

## Which panes can be watched

All of them. A shell, an editor, a file explorer, an agent, a reconnect card —
if you can see it, you can mark it. The mark says something about *you*, not
about what the pane is running.

The chord reaches all of them and the tab strip counts all of them. Only the
Agents tab is narrower, for the reason above.
