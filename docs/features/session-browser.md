---
title: Session browser & editor
layout: default
parent: Features
nav_order: 5
---

# Session browser & editor launch
{: .no_toc }

<details open markdown="block">
  <summary>On this page</summary>
  {: .text-delta }
- TOC
{:toc}
</details>

---

## Session browser

Press **`Ctrl+Shift+P`** (or the *sessions* button) to open the session browser.

The panel it opens in has **two tabs**. Sessions is this page; the second is the
[Agents tab](agents-tab.html), which lists the panes already open in this window
and what each one is doing. `Ctrl+Shift+P` and the *sessions* button always open
the Sessions tab; the *agents* button opens the other one. Switching between them
does not resize your panes — only opening and closing the panel does, exactly as
it always has.

A **Mine ⇄ Orchestration** control at the top picks which world you are looking
at, and orrerix remembers your choice:

- **Mine** — the sessions you started yourself: every pane you launched by hand,
  and nothing an orchestration minted.
- **Orchestration** — everything an orchestration group minted, with the
  **Orchestrations** list above it (the primary route back into a recorded
  group, described below).

The split is on whether orrerix recorded an orchestration identity for the
session, not on a list of role names — so a workflow that invents a new role
puts its sessions in **Orchestration**, where they belong, rather than quietly
mixing them in with your own.

### Orchestrations

Shown in **Orchestration** mode, above the session list. Every orchestration
group orrerix has a record of, on every agent CLI, newest
activity first with running groups at the top. **Resume** brings the whole group
back — same group id, state, task board and audit history, with fresh MCP
identity wired into the resumed orchestrator conversation.

This list is built from orrerix's own record of each group (`group.json` plus the
orchestrator row of `agents.json`), not from any CLI's session store. That is why
it is the reliable restart route: an OpenCode group's orchestrator session is
never in the session list below (see below), and Copilot's is there only once
orrerix has learned its session id.

A row without a **Resume** button says why it has none:

| What the row says | What happened | What to do |
| --- | --- | --- |
| *Running now* | The group has live agents in this window | Click **Focus** on the row — it brings that group's orchestrator pane back into view, out of the dock or out from behind a fullscreen pane, in whichever project tab holds it |
| *Session not yet identified* | Copilot, OpenCode and Codex mint their session ids after boot, and orrerix has not learned this one yet (or its watcher timed out). Claude Code and pi never show this row — orrerix assigns their ids before the pane starts | Wait for it. If the watcher timed out there is nothing to resume by hand — start a fresh orchestrator, which reattaches to this group's existing board and roster |
| *Recorded session is no longer in the … store* | The CLI's own history no longer holds that conversation | Start a fresh orchestrator — it reattaches to this group's existing board and roster |
| *This group's record could not be read* | The group's `group.json` is missing or damaged | Repair or remove that file; until then orrerix cannot tell which CLI ran the group |

### Sessions

Below that, the individual agent sessions orrerix found on this machine:

- **Claude Code** — `~/.claude/projects/*/*.jsonl` (titled by the first real
  prompt, resumed with `claude --resume <id>`).
- **Copilot CLI** — `~/.copilot/session-state/*/workspace.yaml` (resumed with
  `copilot --resume=<id>`) (#458).
- **OpenCode** — its own SQLite store, `~/.local/share/opencode/opencode.db`
  (`$XDG_DATA_HOME` and `$OPENCODE_DB` are honoured, exactly as opencode itself
  resolves them), resumed with `opencode --session <id>`.
- **pi** — `~/.pi/agent/sessions/--<your-folder>--/<timestamp>_<id>.jsonl`
  (`$PI_CODING_AGENT_SESSION_DIR` and `$PI_CODING_AGENT_DIR` are honoured, in
  that order, exactly as pi itself resolves them), titled by the first real
  prompt and resumed with `pi --session <id>`. If you point
  `$PI_CODING_AGENT_SESSION_DIR` somewhere, pi writes session files straight
  into it with no per-folder subdirectory — both shapes are read, so a store
  you have used both ways lists everything in it.
- **Codex CLI** — `~/.codex/sessions/<year>/<month>/<day>/rollout-<time>-<id>.jsonl`
  (`$CODEX_HOME` is honoured, and it names Codex's *home*, so the sessions live
  one level inside it — exactly as Codex itself resolves it), titled by the first
  real prompt and resumed with `codex resume <id>`.

If you have moved your pi store using the `sessionDir` key in
`~/.pi/agent/settings.json` rather than either environment variable, orrerix
does not read that file and will list no pi sessions. It will not list *wrong*
ones.

Two things about **Codex**'s store are worth knowing, because you will see both.

Codex compresses its own older transcripts in place, so a session older than
about a week lists **without a title or a folder**. Orrerix still finds it and
can still resume it — the conversation is intact, and Codex reads it back
itself — but orrerix does not unpack it just to read a title, so the row says
*(no prompt)* and shows no working directory. Listing those sessions without
the details is the deliberate choice: the alternative was not listing a
week-old session at all.

Sessions you have **archived** are not listed. `codex archive` moves a session
into a separate store and orrerix reads only the live one — so archiving
something in Codex takes it out of this list too, which is what archiving it
was for. `codex unarchive <id>` brings it back.

Only *your own* opencode and pi sessions are listed — the ones a solo pane or
your own terminal created. Sessions belonging to an orchestration group live in
that group's own store and are reopened by restoring the group from the
**Orchestrations** list above, not as standalone panes: a bare
`opencode --session <id>` or `pi --session <id>` pane would come back with no
MCP tools and no task board.

Codex is listed differently, and it is worth saying why: it keeps every session
in *one* store, so a group's Codex sessions sit in the same place as your own
and **do** appear here. Resuming one from this list gives you the conversation
and nothing else — no MCP tools, no task board, no roster — so if the session
belonged to a group, restore the group from **Orchestrations** instead. The row
is a way back into the transcript, not a way back into the group.

Clicking a session opens a new pane in the session's original working directory
and resumes it there. The pane is auto-named from the session.

Clicking a **running** group's orchestrator session does not try to resume it —
there is nothing to resume, the conversation is open. It reveals that pane
instead, exactly as the **Focus** button above does. If the group is running
somewhere other than this window, the row says so rather than failing with the
backend's refusal. Worker and reviewer rows are unaffected: a running group
still rejoins them.

#### The name you gave the pane, and your notes

If you renamed the pane you ran a session in, that name is shown on the row
under the session's own title. It appears only when it adds something: a
session you never renamed, or one whose pane still carries the name orrerix
minted for it, shows just its title — the title *is* the fallback, never a
placeholder.

Every row also carries a small **notes** button on the right, with a count when
that session has notes. Click it for the same overlay the pane's own
[Notes button](../core-concepts.html#notes-on-an-agent-pane) opens, where you
can read, add and delete notes about that session. It works on a **dead**
session too: a note is your record *about* a session, and whether the session
can still be resumed is the CLI's business, not the note's.

The count is the number of notes orrerix has read for that session. If it
cannot read its notes file the button shows no number rather than a zero, and
says so when you hover it — a zero there would claim a session has no notes
when orrerix simply does not know.

**Orchestration sessions** in this list are marked with `ORCH` / `W` / `REV`
chips. Clicking a dead group's orchestrator session restores the *whole*
orchestration, exactly as the **Orchestrations** list does; worker/reviewer
sessions rejoin their group once it is running. Which route a click takes is
decided by the recorded membership the chip reflects, never by which CLI wrote
the session. See
[Restart after orrerix closes](../orchestration.html#persistence--restart).

#### Delegate sessions are hidden by default

A group mints a session per delegate and a fresh one on every rejoin, so a
machine that has run a few fleets accumulates hundreds of worker and reviewer
rows against the handful you would ever click. In **Orchestration** mode the
list therefore shows only **orchestrator** sessions by default; everything else
sits behind a **Show N hidden agent sessions** button under the list, which
toggles them all back on.

The two controls answer different questions and compose rather than replace each
other: the mode picks *whose* sessions, and this button then decides *how much*
of an orchestration you see. In **Mine** there are no delegates to hide, so the
button is not shown at all.

Nothing is filtered out of the *scan* — every session is still found, still
badged, and one click away. Restoring a group still brings its workers and
reviewers back: the orchestrator respawns them from the group's own roster,
which is why their individual rows are not a route you need.

The button counts what your current search left hidden, so it changes as you
type. It disappears entirely when nothing is hidden.

## Fork a session

Right-click an agent pane's header and choose **Fork session…** to open a new
pane that starts as a copy of that conversation. The original pane keeps
running, untouched — the fork is where you take the side quest.

**Name it as you fork.** Before anything opens, a small box over the pane asks
what to call the fork, already filled in with `<pane name> (fork)` (and
`(fork 2)`, … for a fork of a fork). Type a name and press **Enter**, or press
**Esc** to fork under the suggested name; only the box's **✕** — or a click
somewhere else — backs out without forking. The name is simply the new pane's
name: the same one a double-click on the title renames, and the one the
session list shows for that session from then on.

The new pane opens beside the one you forked, in the same folder, running the
same CLI with the same model and the same permissions you launched with. It
gets its own session and its own identity for
[connecting panes](../orchestration.html) — so anything you do in it is
invisible to the pane you forked.

The fork is the CLI's own: orrerix asks the CLI to fork the session rather
than copying any files around — Claude Code's `--fork-session`, codex's
`codex fork`, pi's `--fork` and opencode's `--fork`. That means the fork carries
the conversation up to this moment. Expect to re-approve tools you had allowed
"for this session" — the fork is a new process, so those approvals are not
expected to carry.

**Claude Code, codex, pi and opencode.** On a standalone agent pane the item is
always there: greyed out with the reason on copilot and gemini (neither has a
command-line fork), and greyed out until the agent has been prompted at least
once (there is no conversation to fork before that). A codex or opencode fork
learns its own session id a little after its first prompt, the way any fresh
pane of those CLIs does.

If the pane restarted between opening the menu and clicking **Fork session…**,
orrerix refuses and asks you to open the menu again, rather than forking the
conversation the pane was having before.

**Orchestration panes fork too.** On a worker, reviewer or planner the fork is
a new agent of the same kind in its group — same persona, CLI and model — with
its own worktree cut from the original's branch; it counts against the group's
agent limit and reports to the orchestrator like any other. A pane a review or
plan drive is using can't be forked while the drive runs. On a **lead** pane,
the fork is a standalone pane for you (never a second lead), and the lead can
ask for the same thing itself. An orchestrator or manager pane has no Fork item.
See [Orchestration](../orchestration.html#forking-an-agents-session).

**There is no rejoin.** Nothing merges two conversations back together — no
agent CLI offers it — so when a side quest is worth keeping, you copy what you
want back into the original pane yourself.

Forked panes are restored like any other: when you reopen orrerix, a forked
pane comes back on **its own** session, not by forking again.

### Finding your way back: fork lineage

Every fork remembers which session it was forked from, and orrerix uses that to
show you the family tree:

- **In the session list**, a fork sits indented under the session it was forked
  from. Its row reads `↳ fork of <parent name> · forked 3h ago`. A session that
  has been forked carries a **▸ 2** button that shows (and **▾** hides) its
  forks — collapsed until you ask, so a session forked five times is still one
  row. Typing in the filter box opens every branch, so a fork you search for is
  never hidden behind its parent. Forks of forks nest the same way.
- **Return to the parent**: the **↰** button on a fork's row takes you to the
  session it was forked from — to its pane if it is open, or, if that pane is
  gone, it resumes the parent from the list exactly as clicking its row would.
- **On the pane itself**, a forked pane's header carries a small
  `↰ <parent name>` crumb. Click it for the same thing: the parent's pane, or
  the parent resumed.

This works for every CLI that can fork — Claude Code, codex, pi and opencode —
and for orchestration forks as well as your own, and it survives closing the
fork's pane: the closed fork's row still sits under its parent.

A fork whose parent orrerix no longer has any record of (its transcript
deleted, say) still says so — `fork of session 1a2b3c4d — no longer on
record` — and sits at the top of the list rather than pretending to be a
conversation of its own. An orchestration fork's crumb appears once the session
list has refreshed (opening the **Sessions** tab does it).

## Open in editor

Orrerix is a terminal, not an editor — so when you need to open files in a real
editor, the **`</>`** button in a pane header (or **`Alt+E`**) launches your
editor on that pane's current folder. The first time, you're asked for the editor
command; it's remembered after that.

- Set it to `code` (VS Code), `zed`, `subl`, or any command on your `PATH`, or a
  full path to the editor executable.
- The workspace folder is passed as the editor's sole argument, spawned detached
  — the editor keeps running independently of orrerix.
- Right-click the `</>` button any time to change the editor command.

If nothing is configured, or the editor can't be found/launched, orrerix shows a
short toast explaining what went wrong.
