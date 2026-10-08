---
title: Quick tasks
layout: default
parent: Features
nav_order: 15
---

# Quick tasks
{: .no_toc }

<details open markdown="block">
  <summary>On this page</summary>
  {: .text-delta }
- TOC
{:toc}
</details>

*Behind the scenes:* the design note [`docs/design/quick-orchestration.md`](https://github.com/willem445/orrerix/blob/main/docs/design/quick-orchestration.md) argues the *why* — it is a contributor document and is not part of this site.

---

A **quick task** sits between a single agent pane and a full orchestration. You
hand orrerix one task, and it runs up to three steps for you — **plan**, **work**,
**review** — passing the work between them until the reviewer approves or a limit
is reached. There is no orchestrator pane, no task board and no issue queue, and
you do not need a `.orrerix/workflow.yml`.

It never merges, tags, closes or labels anything, and it needs no GitHub issue or
pull request. When the task ends, its panes stay open for you to read, and
nothing keeps running in the background.

## Starting one

Open a new pane and pick **Quick task** under **Kind**.

| Field | What it does |
| --- | --- |
| **Repository** | Where the work happens. Required. |
| **Task** | What you want done, in your own words. Required. |
| **Plan first** | Off by default. A read-only planner writes a plan, and the worker follows it. |
| **Review the work** | On by default. A reviewer reads the work and approves it or asks for changes. |
| **Plan / Work / Review step** | The CLI and model each step runs on, and a box for your own instructions to that step. |
| **Instruction preset** | Fill the three instruction boxes from a saved preset, or save what is in them. |
| **Review rounds** | How many reviews the run may have, 1 to 3. Default 3. |
| **Time bound** | How long the whole run may take, in minutes, 5 to 1440. Default 240. |
| **Branch from** | The branch the worker's branch is cut from. Empty means the repository's default branch. |
| **Max live agents / Idle-kill / Max spawns per hour** | The same limits an orchestration has. |
| **Permissions** | Whether the panes pre-approve git, `gh` and their own tools. |

Leave an instructions box empty and that step runs on its role's own
instructions. What you type is *added* to them; it does not replace them.

Press **Create**. The first pane opens in the tab you are in — the planner if
you asked for a plan, otherwise the worker.

### Which CLI can run which step

A planner is held read-only and a reviewer is not allowed to edit files, exactly
as in a full orchestration. Not every CLI can be held that way, so each step
offers only the CLIs that can run it.

| Step | CLIs |
| --- | --- |
| Work | claude, copilot, opencode, pi, codex |
| Review | claude, copilot, opencode, pi |
| Plan | claude, copilot, opencode |

## What happens

1. **Plan** (if on). The planner reads the repository and reports a plan.
   orrerix saves it and closes the planner's pane.
2. **Work.** The worker gets its own git worktree and branch, and your task —
   with the plan, if there is one. It reports when the work is ready.
3. **Review** (if on). The reviewer opens *in the worker's worktree*, so it reads
   the work exactly as it is, committed or not. It either approves, or asks for
   changes and lists its findings.
4. **Fix.** The findings go back to the worker, in the pane it already has. When
   it reports, the work goes to the reviewer again.

Steps 3 and 4 repeat until the reviewer approves or the review rounds run out.
Only one pane is working at any moment.

A pull request is optional. If the worker opens one and names it when it
reports, the reviewer is told about it and asked to post its review there too.

## What you see

Each pane of the run carries a chip in its header:

| Chip | Meaning |
| --- | --- |
| `quick · planning` | The planner is writing the plan. |
| `quick 1/3 · working` | The worker is on the task. `1/3` is the review round out of the rounds allowed. |
| `quick 2/3 · reviewing` | The reviewer has the work. |
| `quick 2/3 · fixing` | The worker is addressing the reviewer's findings. |
| `quick · held: …` | The run has stopped and is waiting for you. The word after the colon is why. |
| `quick · approved` | The reviewer approved. The run is over. |
| `quick · done` | The worker finished a run that had no review step. |
| `quick · stopped` | You stopped the run. |

The chip is filled in on the pane that is working and outlined on the others.
Hover it for the task and, when the run is held, for the reason.

When a run finishes or is held, orrerix adds one item to your **needs-you** list
and shows a desktop notification. The item says what happened, which branch the
work is on, and what the reviewer said.

## Controlling a run

Right-click the header of any pane in the run:

| Menu item | What it does |
| --- | --- |
| **Hand to the reviewer now** | Sends the work for review without waiting for the worker to report. |
| **Send back to the worker now** | Gives the turn back to the worker without waiting for a verdict. |
| **Add note to run…** | Types your note into the pane that is working, and includes it in the next step's instructions. |
| **Resume quick run** | Continues a held run from where it stopped. |
| **Stop quick run** | Ends the run. |

Stopping a run closes nothing. Its panes stay open.

## When a run is held

A held run is waiting for you. It holds when:

- the reviewer still wants changes after the last review round;
- a pane reports that it is blocked;
- a pane closes before it reports;
- the planner, the reviewer or a worker fixing findings goes quiet for an hour;
- the run reaches its time bound;
- a pane sends a message — there is no orchestrator to answer it, so orrerix
  shows it to you instead;
- the next pane could not be opened, for example because the group is at its
  live-agent limit;
- the pane's provider reports a usage limit.

**Resume** continues the run and gives it its time bound again. Resuming after
the last review round sends the findings to the worker and allows a fresh set of
rounds.

## Files a run leaves

A run keeps its documents in its own folder under orrerix's data directory. The
instructions each pane is given name the exact paths.

| File | Contents |
| --- | --- |
| `plan.md` | The planner's plan. |
| `round-1.md`, `round-2.md`, … | The reviewer's findings, one file per review that asked for changes. |
| `messages.md` | Any messages the panes sent. |

The work itself is on the worker's branch, in its worktree.

## Instruction presets

A preset is a name and the three instruction texts. **Save as…** stores what is
in the boxes; picking a preset fills them; **Delete** removes the selected one.

Presets are yours, not a repository's. They are kept with orrerix's own settings
and offered in every repository. Nothing in a repository can add or change one.

## After a restart

If you close orrerix while a run is working, its panes go with it. The next time
orrerix starts, the run is held with the reason `restart`, and nothing is
re-opened until you say so. **Resume** re-opens the session that was working and
gives it its instructions again.

One case cannot be resumed: a pane that was closed before its CLI had reported a
session to orrerix. There is no session to re-open, and the run says so when you
press Resume.

## Limits

- **One task per run.** A quick task has no queue. Start another for the next
  task.
- **Resume and Stop are on a pane's menu.** If you close every pane of a run,
  it is held and there is no pane left to resume or stop it from.
- **No token budget.** A run is bounded by its review rounds and its time bound,
  not by spend.
- **A pull request is found only if the worker names it.** orrerix does not
  look one up on GitHub.
- **The plan step's pane closes** once the plan is reported. The plan is in
  `plan.md`.
- **Pausing the tab's group** holds back what orrerix would type into the panes,
  while the run's time limits keep counting.
