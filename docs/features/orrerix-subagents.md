---
title: orrerix subagents
layout: default
parent: Features
nav_order: 12
---

# orrerix subagents
{: .no_toc }

<details open markdown="block">
  <summary>On this page</summary>
  {: .text-delta }
- TOC
{:toc}
</details>

---

## What it is

Agent CLIs run helpers of their own. Claude Code delegates to subagents, Copilot
runs custom agents in a separate context window — and you never see any of it.
The work happens inside one process, in a context you cannot read, steer or
interrupt, and when it goes wrong your only signal is the summary that comes
back.

**orrerix subagents** is a checkbox on the launcher's Agent kind. Tick it and the
pane you are launching becomes a **lead**: it gets a small orchestration group of
its own, and its helpers open as ordinary orrerix panes — each in its own git
worktree, each with a terminal you can read, steer, and kill.

The pane you type into is still yours. Nothing drives it, nothing reaps it, and
it never gets a task brief: you are the loop. What changes is where its helpers
go.

## Turning it on

In the pane setup form, pick **Agent**, pick your CLI, and tick **orrerix
subagents**. Four guardrail numbers appear beside it — the same ones an
orchestrator launch has always had, because they now govern the same thing:

| Guardrail | What it does to a lead's helpers |
|---|---|
| **Max live agents** | how many helpers can be alive at once; the lead is refused a spawn past it |
| **Idle-kill (min)** | a helper with nothing to do for this long is reaped. `0` = off |
| **Max spawns/hour** | a backstop against a loop that keeps opening panes. `0` = unlimited |
| **Watchdog stall (min)** | a helper that goes silent mid-task is nudged once. `0` = off |

None of them apply to the lead pane itself. A human pane is silent when the
human is.

The toggle is off by default and stays where you left it.

## Where it is not offered

The checkbox is **hidden** for anything that cannot be a lead: another pane kind,
a custom command line (that line is yours — orrerix will not append flags to it),
and a CLI whose lead launch orrerix has no flags for. Today it is offered for
**claude**, **copilot** and **pi**.

It is **shown but disabled**, with the reason on the control, when the tab you
are launching into already runs an orchestration group. One tab owns one group,
and a lead mints one — so open a new tab (`Ctrl+Shift+T`) and launch it there.

## What your CLI is told

A lead is told to use orrerix's own tools instead of its harness's, and *how* it
is told differs per CLI. Where the CLI documents a way to switch its own
subagents off, orrerix uses it; where it does not, the instruction is the only
lever there is.

| CLI | Its own subagents | What orrerix does |
|---|---|---|
| **claude** | the `Agent` tool | denies that tool on the command line, so delegation is not available at all |
| **copilot** | custom agents in a separate context | **instruction only** — Copilot documents no tool name to deny, so a determined model can still use its own |
| **pi** | none — it ships without sub-agents | nothing to disable |
| **opencode**, **gemini** | yes | not offered: their MCP config arrives through a file or the environment, which the launcher's spawn seam does not set |
| **codex** | behind a config flag | not offered yet |

The instruction itself is a short briefing typed into the pane when it opens. If
you type your first message in the same instant the pane boots, the briefing can
land after it — scroll up, it is there.

## Reading the fleet

The lead pane wears a **LEAD** badge in its group's colour, and its helpers wear
the same colour, so a family reads as one at a glance.

In the [Agents tab](agents-tab.html), a helper is shown **indented under its
lead**, with a `↳`. The nesting is exact: a pane is shown under a lead only when
it is in that lead's group *and* in the same tab, so two leads in two tabs never
claim each other's helpers.

## Closing a lead

Closing a lead pane ends its whole group — every helper closes with it and their
agents are killed. Because that is not what closing a pane means anywhere else,
the ✕ **asks first**: the first click arms it (the button turns red and says how
many helpers it is about to end), a second click within four seconds does it, and
anything else disarms it.

The helpers' **worktrees are left on disk**. Ending the group kills the agents,
not the work: the branches and their changes are still there in the Git view and
in `git worktree list`.

## What does not come back

Two things about a restart are worth knowing before you rely on them.

**A lead pane comes back; its helpers do not.** A lead group cannot be resumed —
its helpers' sessions and worktrees have moved on — so on restart the lead pane
reopens on its own command line with a **fresh, empty group**. It has no memory
of the helpers it had. They are not reopened, and their worktrees are still on
disk for you to pick up by hand.

**A restored lead uses the default guardrails,** not the numbers you set when you
launched it. What is written to disk is that the pane *was* a lead, not how it
was configured. If you set an unusual cap, set it again after a restart.

## What a lead can and cannot do

A lead holds the fleet tools an orchestrator holds — open a helper, prompt it,
read its output, kill it, focus it, list them — and none of the project-running
surface. It has no task board, no review gate, no merge queue, and it is nobody's
delegate: it never reports to anyone, because you are sitting in front of it.

Its helpers are **workers**. Asking for a reviewer or a planner is refused, with
the reason: a lead group has no review gate and no board.

When a helper reports `done` or `blocked`, that report is typed into your pane —
the same delivery an orchestrator gets. A `progress` report has nowhere to go in
a group with no board, so it is recorded and types nothing.

## See also

- [Agents tab](agents-tab.html) — where the nesting is rendered.
- [The manager pane](manager.html) — the other pane you talk to, and the opposite
  pole: a manager holds no fleet tools at all.
