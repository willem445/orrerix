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
hand orrerix one task and it is planned, done and reviewed for you, until the
work is approved or a limit is reached. There is no orchestrator pane, no task
board and no issue queue, and you do not need a `.orrerix/workflow.yml`.

There are two ways to run one, and you pick under **How**:

- **Steps.** You say which steps you want — **plan**, **work**, **review** — and
  orrerix passes the work between them itself. Most of this page describes it.
- **Describe it.** One agent opens and waits. You tell it the task in its pane,
  and it decides whether to plan and review and opens its own helpers. See
  [Describe it](#describe-it).

It never merges, tags, closes or labels anything, and it needs no GitHub issue or
pull request. When the task ends, its panes stay open for you to read, and
nothing keeps running in the background.

## Starting one

Open a new pane and pick **Quick task** under **Kind**.

| Field | What it does |
| --- | --- |
| **Repository** | Where the work happens. Required. |
| **How** | **Steps** or **Describe it**. The rest of this table is the Steps form. |
| **Task** | What you want done, in your own words. Required in Steps; Describe it has no task field. |
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
you asked for a plan, otherwise the worker. If it cannot be opened, the form
comes back with the reason and the run is stopped.

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

## In a folder that is not a git repository

A quick task works in a plain folder too, the way a single agent pane does.
There is nothing to cut a worktree or a branch from, so:

- **The worker and the reviewer open in the folder itself.** No worktree is
  made beside it and no branch is created. The planner opens there as well, as
  it always has.
- **They share that one folder.** In Steps only one pane works at a time, so
  nothing collides. In Describe it the agent is told to have one worker
  changing the folder at a time.
- **There is no branch, no commit and no pull request.** The worker changes the
  files in place and says which ones in its report. The reviewer reads those
  files where they are; it has no diff to ask for.
- **Branch from** is not used. In Describe it, a `branch` or `base` the agent
  passes when it opens a helper is ignored, and it is told so.

Each pane is told this when it opens, and in Describe it the agent is told in
the answer to every helper it opens.

This applies to quick tasks only. A full orchestration group still needs a git
repository: its workers and reviewers each get their own worktree, and one
started in a plain folder is refused its first worker.

If git is installed but cannot read the folder — a bare repository, or one whose
ownership git refuses — no helper opens and the run says why. orrerix does not
treat a folder git refused as a plain one.

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

## Describe it

Pick **Describe it** under **How** when you would rather say what you want than
decide the steps yourself. The form does not ask for the task. One agent opens
and waits, and you tell it in its pane, as you would any agent.

| Field | What it does |
| --- | --- |
| **Runs on** | The CLI and model of the agent that opens. Any CLI. |
| **Plan / Work / Review helper** | What each kind of helper runs on, if the agent opens one. |
| **Review rounds** | How many times the agent may send work back after a review, for each task. |
| **Time bound per task** | How long one task may take, counted from when the agent starts work on it. |
| **Branch from** | The branch helpers' branches are cut from. Empty means the repository's default branch. |

There is no task field, no steps to switch on and no instruction boxes.

### Giving it a task

Press **Create**. One pane opens — the agent's — with a `quick · idle` chip.
Nothing is typed into it. It already has its instructions: what a quick task
is, which tools it has, and what it may open.

Type what you want done. Your first message is the task. The agent can ask you
questions before it starts, and you can answer them in the pane; none of that
time counts against the time bound.

The agent then decides how much the task needs. A small change gets a worker
and nothing else; a larger one may get a planner first and a reviewer after. It
opens helper panes as it needs them, and they report back to it, not to you. It
does not do the work in its own pane: it is in your checkout, and the work
happens in a worker's worktree.

**The time bound starts when the agent opens or prompts its first helper.** The
chip changes to `quick · running` at that moment.

| Chip | Meaning |
| --- | --- |
| `quick · idle` | The agent is waiting for you. Nothing is running against a limit. |
| `quick · running` | A task is in progress. |
| `quick · held: …` | The task has stopped and is waiting for you. The word after the colon is why. |
| `quick · stopped` | You stopped the run. |

On gemini, which cannot be given its instructions any other way, one line is
typed into the pane when it opens. It says where the instructions are and to
wait for you. It is not a task.

### When a task ends

The task ends when the agent reports. You get one needs-you item with what it
said: where the work is, and what it left open. The chip goes back to
`quick · idle`.

The pane is still yours to use. Type the next task into it and the agent starts
again, with a fresh time bound and a fresh set of review rounds. Each task you
give it ends with its own needs-you item. Helpers from an earlier task stay
open until you or the agent close them.

If the agent cannot go on it says why, the run is held, and you can **Resume**
it once you have answered in its pane.

### Closing it

Close the pane when you are done with it. If no task is in progress, that is
all there is to do: nothing is left running, nothing needs stopping, and the
run does not appear under **Unfinished runs**. A pane you opened and never gave
a task to costs nothing to leave open and nothing to close.

If you close the pane while a task is in progress, its helpers are closed with
it and the run is held. You can resume it from **Unfinished runs**.

If the agent's program exits by itself — it crashed, or it could not start
because of a wrong model or a missing sign-in — the run is held instead of
ended, even with no task in progress. You get a needs-you item quoting the
last thing the pane printed, and **Resume here** under **Unfinished runs**
opens a fresh agent. Quitting the CLI from inside the pane counts as this too;
close the pane instead when you are done with it. Quitting orrerix does not: an
idle run just ends, with no needs-you item (see **After a restart**).

What the agent can and cannot do:

- It can open a worker, a reviewer or a planner, and nothing else.
- It cannot merge, tag, close or label anything, and it has no task board.
- It cannot be closed by one of its helpers.
- **Stop quick run** tells it to stop and closes nothing. It ends the run, not
  just the task: the agent cannot open helpers again afterwards. To change
  what it is doing without ending the run, tell it in its pane.
- When the run is held — at its time bound, for example — the agent is told,
  and it cannot open any more helpers until you resume the run.

## Runs that have not ended

Open the Quick task form and, if any run is working or held, they are listed
at the top under **Unfinished runs**: the task, the repository, and where the
run stands. A Describe it run shows "A task given in its pane" for the task,
and is listed only while a task is in progress or held — an idle one has
nothing to resume or stop.

- **Resume here** re-opens a held run's pane in the tab you are in.
- **Stop** ends the run. Nothing is closed or deleted.

This is how you reach a run whose panes are all gone — closed, or lost when you
quit orrerix. Closing a run's tab does not end the run: it is held, and it is on
this list.

## After a restart

If you close orrerix while a run is working, its panes go with it. The next time
orrerix starts, the run is held with the reason `restart`, and nothing is
re-opened until you say so. A Describe it run that was idle is simply over:
there was no task to pick up again. **Resume** re-opens the session that was working and
gives it its instructions again. With its panes gone, you resume it from
**Unfinished runs** in the Quick task form.

One case cannot be resumed: a pane that was closed before its CLI had reported a
session to orrerix. There is no session to re-open, and the run says so when you
press Resume.

## Limits

- **One task at a time.** A quick task has no queue. A Steps run is one task:
  start another for the next. A Describe it pane takes the next task when the
  last one is done.
- **A Describe it time bound starts with the first helper.** An agent that
  never opens one is not on a clock.
- **Opencode records its session at the first message.** If you give a
  Describe it agent on opencode its first task more than ten minutes after the
  pane opened, a task interrupted by a restart or a closed pane cannot be
  resumed.
- **On codex, the agent's session and usage are recorded from the first
  orrerix tool it uses**, which is normally opening a helper. A Describe it
  agent on codex that only talks to you is not counted in the run's usage.
- **On codex, do not start your own codex in the same repository while a
  Describe it agent is waiting for its first task**, if you want that task to
  be resumable. orrerix cannot tell the two new sessions apart, so it records
  neither: your own session is never mistaken for the agent's, but a task
  interrupted by a restart or a closed pane cannot then be resumed.
- **On copilot, its own autopilot prompt is yours to answer** when it appears
  on your first message to a Describe it agent.
- **A described run counts no review rounds itself.** The agent is told the
  limit and keeps to it; the time bound is the one orrerix enforces.
- **No token budget.** A run is bounded by its review rounds and its time bound,
  not by spend.
- **A pull request is found only if the worker names it.** orrerix does not
  look one up on GitHub.
- **In a folder that is not a git repository, nothing is isolated and nothing
  is recorded.** The panes share the folder, there is no branch to go back to,
  and a change is only as reversible as your own backups make it.
- **A repository with no commits is still a repository.** A quick task there is
  refused its worker, as before: git has nothing to cut a branch from. Make a
  first commit, or use a plain folder.
- **The plan step's pane closes** once the plan is reported. The plan is in
  `plan.md`.
- **Pausing the tab's group** holds back what orrerix would type into the panes,
  while the run's time limits keep counting.
