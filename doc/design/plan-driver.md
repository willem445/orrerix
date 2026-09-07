# The plan driver (#3040)

**Status: the drive runs end to end.** Contracts 1–7 are in the present tense
because they describe code in the tree; 8 is WILL-tense, and so is every
paragraph below marked WILL. Do not act on a WILL-tense section as though it
described the build you are looking at.

**What one drive does**, stated once here because every section below assumes
it: it spawns a planner, validates and stores the plan block that planner
posts, and then — for an `agent-ready` issue — boards that plan and executes
it. An `agent-investigation` issue reaches `complete` at the plan instead: the
plan IS its deliverable, and no worker is ever spawned off it.

**Two things the drive never does, in any state.** It never merges anything —
it reaches GitHub only through the review driver's `gh`-only `RdRunner`, which
has no `git` method at all — and it never marks a slice `done` on anything but
a **positively established MERGED** PR. A PR orrerix could not read leaves its
slice exactly where it was.

The full design is the plan comment on #3040. This note exists so each contract
gets a durable home as its slice lands, rather than living only in an issue
comment.

## 1. The `orrerix-plan` block

A planner's issue comment carries **exactly one** fenced block whose info string
is `orrerix-plan`. Its content is YAML, schema v1:

```yaml
version: 1
issue: 3040
slices:
  - id: P1
    title: plan block parser
    branch: feat/3040-p1-plan-block
    block: worker-adv
    deps: []
    brief: |
      Delivered verbatim into the worker's kickoff. Multi-paragraph prose is
      why this is YAML rather than JSON — a block scalar needs no escaping.
    avoid_files: [src-tauri/src/orchestration/mod.rs]
    red_before_green: "cargo test --locked -p loomux-engine plandoc"
    hold: false
risks:
  - "Free-form notes, carried through untouched."
```

| field | required | meaning |
| --- | --- | --- |
| `version` | yes | Must be `1`. A different version is refused, not read optimistically. |
| `issue` | yes | The issue this plan is for. |
| `slices` | yes | At least one. Order is the planner's; it carries no scheduling meaning — `deps` does. |
| `slices[].id` | yes | Unique within the plan, **case-insensitively**. Validated through `pathseg::check_segment` (CLAUDE.md constraint 6). |
| `slices[].title` | yes | Non-empty. Becomes the board row title beside the id. |
| `slices[].branch` | yes | A git ref name that is ALSO a legal worktree directory (`pathseg::worktree_name_ok` — ASCII letters and digits with `. _ - /`), checked and **refused, never sanitized**. Unique within the plan, **case-insensitively**. |
| `slices[].block` | yes | A roster block id. **Not** resolved at parse time. |
| `slices[].deps` | no (`[]`) | Slice ids in this same plan. Unknown ids and cycles are refused. |
| `slices[].brief` | yes | At least 40 characters. Delivered verbatim. |
| `slices[].avoid_files` | no (`[]`) | Informational; rendered into the brief header, enforced by nothing. |
| `slices[].red_before_green` | no (`null`) | How the slice's change is to be shown failing first. |
| `slices[].hold` | no (`false`) | `true` = never auto-spawn; the orchestrator briefs this slice by hand. |
| `risks` | no (`[]`) | Free-form notes, carried through untouched. |

Unknown keys at either level are **refused**, not ignored: a misspelled `depends`
that parsed silently would ship a slice with no dependencies at all.

**Three properties this contract rests on.**

*Exactly one block.* Two `orrerix-plan` fences in one comment are refused naming
both fence lines. Which one the author meant is precisely the thing that cannot
be guessed, and a reviewer quoting a plan back is a realistic way for a second
one to appear.

*Nothing is repaired.* An `id` of `../x` is refused, not rewritten to `x`; a
branch containing `..` is refused, not cleaned. Repair is how two distinct
strings come to name one thing, and a slice id reaches a branch name, a pane
name and a persisted row key.

*Uniqueness is case-insensitive, and that follows from the same argument.* Ids
`P1` and `p1`, or branches `feat/A` and `feat/a`, are two strings that name one
worktree directory on a case-insensitive filesystem — the hazard the previous
paragraph refuses to *create*, arriving instead from the planner's own text.
Both are refused, naming both spellings; neither is folded into the other. Two
slices sharing one branch outright are refused for a plainer reason: they cannot
each open their own PR.

*`block` is a string here.* Whether `worker-adv` exists is a question about the
roster the group was launched with, which the parser cannot see. The drive asks
it (P3a below).

Every refusal reads `plan block line N: …`, with N absolute **within the
comment**, so the planner can scroll to it. `serde_norway` 0.9.42 exposes no
per-value spans on parsed values, but it does attach a mark to any error raised
from inside a `Deserialize` impl. `plandoc` uses that twice: per-value checks
(`id`, `branch`, unknown keys, type mismatches) run during deserialization and
get their line for free, and cross-document checks (duplicate id, unknown dep,
cycle) are located by re-deserializing with a shadow type that deliberately
fails at the addressed value. Where that probe cannot reach the value, the
reason degrades to `plan block: slices[i].deps[j]: …` — the index addressing
`workflow::parse_workflow` uses — rather than inventing a line. The module doc
in `plandoc.rs` is the reference for this.

## 2. `<group-dir>/plan_drives.json` v1

One file per group, beside `review_drives.json`, holding one entry per driven
issue. `crates/loomux-engine/src/plandrive.rs` owns its shape; the group
directory is built by `group_dir_at`, the only place a group id becomes a path.

```json
{
  "version": 1,
  "entries": [
    {
      "issue": 3040,
      "state": "plan-posted",
      "held_reason": null,
      "held_from": null,
      "on_behalf_of": "orch-1",
      "planner_block": "plan-lead",
      "planner_agent": "a-7",
      "planner_session": "…",
      "consent": "agent-ready",
      "base": null,
      "review_minutes": 0,
      "started_ms": 0,
      "state_since_ms": 0,
      "spawned_ms": 0,
      "invalid_count": 0,
      "last_invalid": [],
      "plan": { "version": 1, "issue": 3040, "slices": [], "risks": [] },
      "comment_url": "https://github.com/…#issuecomment-…",
      "posted_ms": 0,
      "slices": {
        "P1": {
          "task_id": "t-12",
          "state": "in-review",
          "hold": null,
          "agent": "a-9",
          "session": "…",
          "pr": 3133,
          "cap_starved_since_ms": 0,
          "spawned_ms": 0
        }
      },
      "pr_poll_cursor": 0,
      "last_progress_ms": 0,
      "owed": null
    }
  ]
}
```

**The four properties this record is written for**, each the review driver's own
and each carried rather than re-argued:

*Atomic, through `fsatomic::atomic_write`.* A disk-full `fs::write` is what
truncated `tasks.json` and destroyed a live board in #133, and this file has the
same "losing it loses in-flight work" property — the planner's whole output is
in it.

*Unknown fields are preserved; an unknown SCHEMA is refused.* A key a newer
build wrote survives a read/write cycle by an older one. A `version` this build
does not understand is not acted on at all: the fields it recognises may no
longer mean what it thinks.

*Unparseable is loud, and nothing is repaired.* The tick audits
`pd-state-unreadable`, backs off, and leaves the file exactly as it found it.
Every tool answers `pd-state-unreadable` rather than `not-driven`, because
"orrerix cannot read the record" is not "there is nothing in it".

*Times are ABSOLUTE.* `started_ms`, `state_since_ms`, `spawned_ms` and
`posted_ms` are wall-clock stamps; every age in `plan_drive_status` is derived
from them at read time. A stored elapsed figure is stale the instant it is
written and meaningless across a restart.

**`slices` is what HAPPENED, never what the plan SAYS.** A slice's title,
branch, block, deps, brief and `hold` flag are read off the stored `plan` every
tick and are not copied here, so there is exactly one copy of the plan and a
plan cannot drift from its run state. What this map carries is the row the slice
was boarded as, how far it got, and the pane and PR it produced. It is empty
until `boarding` has run, and empty forever on an `agent-investigation` drive.

**`pr_poll_cursor` is persisted rather than restarted each tick**, which is
what makes the in-review poll fair: a drive with more in-review slices than
`PD_MAX_PR_CHECKS_PER_TICK` would otherwise look at the same first few forever
and never notice a later one merging.

**`last_progress_ms` is a SECOND stall meter, and the two measure different
things.** Outside `running`, a drive is bounded by its whole AGE — a drive that
has not started work yet and is old is stalled. Inside `running` that would park
a perfectly healthy multi-day plan for the crime of being big, so the meter is
idleness instead: nothing spawnable, nothing in review, and nothing moved for
`drive_timeout_minutes`.

### The states

| state | meaning | leaves for |
| --- | --- | --- |
| `planning` | a planner pane is open; waiting for a plan block | `plan-posted`, `held`, `cancelled` |
| `plan-posted` | a valid block is stored, with the comment it was posted as | `plan-review`, `boarding`, `complete`, `held`, `cancelled` |
| `plan-review` | the declared review window is running; ONE notice has been sent | `boarding`, `held`, `cancelled` |
| `boarding` | the plan is being turned into board rows | `running`, `held`, `cancelled` |
| `running` | the rows are being executed: claim, spawn, hand off, mark done | `complete`, `held`, `cancelled` |
| `complete` | terminal: the drive did everything it was going to | — |
| `cancelled` | terminal: cancelled by tool, or the issue is positively closed | — |
| `held` | **parked**, carrying a reason | back to the state it came from, or `cancelled` |

One parked state carrying a closed reason, rather than seven states: a reader
asking "is this drive parked" asks one question, and the reason travels in the
notice and the audit row instead of being inferred from which field is set.

**A resume returns to the state the hold came FROM**, which is why `held` has
one outgoing arc per working state. A drive parked on `planner-stalled` and one
parked on `cap-full` resume into different work, and a single `held → planning`
arc would silently re-open a planner for a plan that is already posted — or
re-board a plan whose rows are already on the board.

### The hold reasons

| reason | what happened |
| --- | --- |
| `plan-invalid` | three plan blocks refused; the last reasons are on the record |
| `plan-missing` | the planner finished, or its pane went, without posting |
| `planner-stalled` | neither a post nor a report inside `planner_timeout_minutes` |
| `planner-blocked` | the planner reported `blocked` |
| `consent-withdrawn` | the issue's label was withdrawn while the drive was live |
| `row-removed` | a slice's board row was struck, so its dependents can never become ready |
| `awaiting-p3b` | **legacy**: written by no build, read so a record from the build that shipped P3a still parses. Resuming one re-enters `boarding`, which is what that hold said was missing |
| `drive-stalled` | the drive outran `drive_timeout_minutes` — by AGE outside `running`, by IDLENESS inside it |

An older build reading a newer file refuses it through the `version` check
above rather than acting on a word it cannot read, which is why adding a state
word is a schema question and not a free one.

### The SLICE hold reasons

A slice hold parks **one slice** and leaves the rest of the plan running; a
drive hold parks the whole drive. Keeping them separate is what makes a blocked
worker cost one slice rather than a plan.

| reason | what happened |
| --- | --- |
| `cap-full` | the live-delegate cap refused this slice's spawn for longer than `CAP_HOLD_MS` |
| `worker-blocked` | this slice's worker reported `blocked`; the notice carries its own note |
| `pr-closed` | this slice's PR was CLOSED without merging |
| `worker-gone` | this slice's worker pane died without ever reporting |

**A cap refusal is not an error.** The row stays `queued` and is retried every
tick — with the board row rolled back to `queued` and unassigned, so a full
board never strands a row `in-progress` with nobody on it. `cap-full` is the
bound on retrying forever, and it is the review driver's own constant reused
rather than a second number to keep in step.

## 3. `driver:` block keys

Three keys join the workflow `driver:` block. `docs/orchestration.md` carries
the user-facing table; what belongs here is why each is shaped the way it is.

| key | range | default | outside the range |
| --- | --- | --- | --- |
| `plan_enabled` | — | `false` | — |
| `plan_review_minutes` | 0–120 | `0` | **refuse** |
| `planner_timeout_minutes` | 15–180 | `60` | **refuse** |

**`plan_enabled` is a SECOND switch, not a widening of `enabled`**, and it is
read UNDER it: the plan driver is off wherever the review driver is. The
separation is the consent. A repo that turned the review driver on consented to
orrerix running a review loop it already had an orchestrator for; it did not
consent to orrerix spawning a **planner** and turning that planner's output into
work. Nothing about the review driver's own gate expressed that difference, so a
widening would have granted the second on the strength of the first.

**Both minute keys are REFUSED outside their range, not clamped**, which puts
them with the counters rather than with the two lane/fix backstops. The
backstops are the notify-TTL family — one bounded wait on one fallible signal —
and these are not: a repo asking for a five-minute planner timeout has
misunderstood what a planner does, and quietly handing it fifteen would leave
the misunderstanding in place while the behaviour changed underneath it. That is
`merge_queue.max_batch`'s own argument.

**`plan_review_minutes` is recorded and spent on nothing in this build.** The
review window is P3b's, and the key lands in P3a so that the repo key, the
record field, the tool argument and the status view are one contract rather than
four separate landings. `drive_timeout_minutes` is the review driver's own knob,
**reused rather than duplicated**: it bounds the same quantity, a whole drive's
age.

## 4. The four MCP tools

All four are `require_orchestrator`-only, listed only for an orchestrator, and
re-checked in `call_tool` — the #243 double gate, where the listing is cosmetic
and the dispatch check is the gate. All four refuse `plan-driver-disabled`
unless the repo declares both switches.

- **`drive_plan(issue, planner_block?, review_minutes?, base?)`** — reads the
  issue once through `gh`, opens the planner, writes the entry. The refusal
  vocabulary is closed: `plan-driver-disabled`, `issue-not-open`,
  `issue-unverifiable`, `issue-not-labelled`, `already-driven`,
  `no-planner-block`, `planner-unspawnable`, plus the three that mean orrerix
  itself failed (`pd-state-unreadable`, `pd-state-unwritable`,
  `pd-unavailable`).
- **`plan_drive_status()`** — read-only; the state, the hold, the consent, the
  planner, the comment URL, the refusal count and the plan's own slices.
- **`cancel_plan_drive(issue)`** — works in any non-terminal state, **and it
  kills nothing**: the planner pane keeps running under the orchestrator, and
  its traffic reaches that pane again the moment the entry stops being live.
  Cancel releases ownership; ending a pane is the orchestrator's own
  `kill_agent`, unchanged.
- **`resume_plan_drive(issue)`** — moves a parked drive back to the state the
  hold came from, and clears the refused-block counter so a re-briefed planner
  gets a fresh three rather than resuming onto the bound. A LIVE drive answers
  `not-held`, which is deliberately a different word from `not-driven`: the two
  want different things from the orchestrator.

**The issue is a bare integer**, matching `post_issue_comment` rather than
`drive_review`'s three-spelling `pr_number`. What an agent typed as `#12` is a
string it built, and the one place this group resolves a number from is the tool
argument itself.

**Ordering in the tick.** `pd_driver_tick` is the sixth step of `gh_poll_tick`,
after `rd_driver_tick`, one group per wake, at most `PD_MAX_GH_PER_TICK` (4)
`gh` round trips on a **steady-state** wake.

**One wake per process is not steady-state**, and the bound does not cover it:
the once-per-group restart reconcile runs before that loop and reads one issue
per **non-terminal** entry, so the first wake after a restart spends
`non_terminal + min(live, 4)` round trips.

Non-terminal rather than live, deliberately: a `held` entry is not live and is
still reconciled, because the one thing a restart must be able to learn about a
parked drive is that its issue was closed while orrerix was down. A group
carrying holds therefore spends more here than its live count. That is bounded
by how many issues an orchestrator chose to drive, and it is a startup cost paid
once; it is stated rather than fixed because a reconcile that serviced only four
entries would leave the rest unreconciled with nothing scheduled to finish the
job. `a_tick_services_at_most_four_drives` measures both figures, so this
paragraph cannot go quietly false. Running second is the bound rather than a preference: the plan
driver can only ever take the budget the review driver left, which makes "the
plan driver holds, never starves the review driver" structural instead of a
counter nobody can check.

## 5. `post_issue_comment` for a driven planner

When the caller is the planner of a **live** drive on that issue and the drive
is in `planning`, the body is extracted, parsed and drive-validated **before
`gh` is run at all**. An invalid or missing block is the tool answering `Err`
with the line-numbered reasons, and **nothing is posted**. A valid block is
posted, and the document plus the new comment's URL are stored into the record
in the same call.

**Why a hook rather than a `gh issue view` after the fact.** The plan reaches
orrerix in the tool call's own payload, so a refusal costs one tool call inside
the planner's own turn: no round trip, no orchestrator turn, and nothing
published that a human then has to read and discount. A second comment carrying
a fence — a reviewer quoting the plan back — would also make a read-back
ambiguous, and the payload is not.

**The bound.** Three refusals park the drive on `held(plan-invalid)` carrying
the last reasons. The planner is still inside its own turn for each of them, so
a fix is cheap; three is the point past which it is not going to converge on its
own, and a human gets one notice instead of an unbounded loop.

**Four things this hook does NOT do**, each stated because a reader will look
for it. It does not fire for a caller that is nobody's planner — the product
default, and indistinguishable from the hook not existing, which is what keeps
#2815 unregressed. It does not fire once a plan is stored: a planner adding a
note to the issue afterwards is doing what any agent with this tool may do. It
does not fire for a **parked** drive, because a held entry owns nobody. And it
does not store the plan before `gh` has answered — a record saying a plan lives
at a URL that does not exist is worse than one saying nothing.

**The drive-level checks the parser cannot make.** `plandoc` judges the
document; `plandrive::validate_for_drive` adds the two questions that need the
drive's context: the block's `issue:` must be the issue being driven (refused,
never retargeted), and every slice's `block:` must name a `kind: worker` block
**in the roster the group was launched with**. A plan naming a reviewer is
refused at post time — a slice is work, and work is a worker's; anything else
would have the drive spawn a capability class the plan invented for itself. The
roster is the launched one, never `.orrerix/workflow.yml` re-read, because
consent to a roster is given at launch.

## 6. `templates/dod.md`, `{{DOD}}`, and the brief a slice's worker gets

The definition of done exists in exactly one file,
`src-tauri/src/orchestration/templates/dod.md`, substituted as `{{DOD}}` into
the templates that carry it and served by `brief::dod_trailer()` to every brief
this driver composes. One copy is what makes "the orchestrator's hand-written
briefs and the driver's briefs quote the same bytes" checkable rather than
asserted.

**The brief is three parts, joined by one blank line each, in this order:**

1. a **header** orrerix writes — the issue, the slice id and title, the branch,
   the base, a `Do not touch:` line built from the slice's `avoid_files`, and
   the `red_before_green` line the planner named;
2. the planner's own `brief:`, **VERBATIM** — not trimmed, not re-wrapped, not
   re-indented, not truncated. The only thing that touches it is
   `notify::sanitize_pane_text`, applied at the live call site rather than
   inside a test harness, because a test that sanitizes in its own harness
   asserts that two functions compose and passes identically while the real site
   hands raw LLM output to a pane;
3. the definition of done.

The composition itself (`plandrive::slice_brief`) is pure and lives in the
Tauri-free engine, which is what lets
`the_brief_carries_the_planner_text_verbatim_between_header_and_dod` pin the
DECOMPOSITION rather than merely asserting the text appears somewhere.

## 6a. Boarding, spawning, and the hand-off

**Boarding goes through `upsert_task` as an AGENT**, never a direct board
write, and that is the design of the step rather than an implementation detail:
an agent-origin write is what makes `find_dep_cycle` run on the dep edges, what
makes a WIP cap a REFUSAL rather than a warning, and what makes the link and
ladder validation apply. The driver gets exactly the board authority the
orchestrator's own boarding has, and not one check less. One parent row per
issue — **reused** if the issue already has one, so an orchestrator that boarded
it before handing over does not end up with two — and one child row per slice,
dep-linked in a second pass once every row exists.

**At most ONE spawn per group per tick**, and the review driver has already
spent its own budget by the time this runs: `pd_driver_tick` is the sixth step
of `gh_poll_tick`. A plan drive can therefore only ever take the slot the
review driver left, which is what makes "the plan driver holds, never starves
the review driver" structural rather than a counter nobody can check.

**A slice is spawned only when the BOARD says it is ready.** Readiness is
re-derived from `tasks.json` every tick and never cached, which is exactly what
makes a human's board edit work while a drive runs: strike a dep, mark a row
`done` by hand, set one `blocked`, and the next tick simply reads what the board
now says. `hold: true` on the slice itself is the planner's own veto and is read
off the plan, not the board — the driver never spawns such a slice, and the
orchestrator briefs it by hand.

**The spawn is claim-then-spawn, and the order is the WIP gate.** `claim` is a
guarded write (queued, unclaimed, deps met) that a WIP cap refuses outright;
spawning first would open a pane the board then declined to account for. A
refused spawn rolls the row back to `queued` and unassigned.

**A `report(done)` from a slice's worker hands its PR to the review driver.**
The PR number comes from the `ref` the worker named, or — when that yields
nothing usable — from one `gh pr list --head <branch>`. The `ref` is a HINT: it
never decides whether the report was intercepted (the agent id orrerix minted at
spawn does that), and the number it yields is handed to `drive_review_with`,
which re-reads the PR itself. From the hand-off on, the worker is `rd_owner`'s
and this driver never sees it again — `mcp.rs`'s `report` arm asks `rd_owner`
first, so the two ownership sets are disjoint by construction.

**A row is marked `done` only on a positively MERGED PR**, which is a read of
GitHub rather than a decision, and it is confined to rows this drive created.
That is what releases the dependents. A positively CLOSED-unmerged PR parks its
slice on `pr-closed`; anything else — including a PR orrerix could not read —
moves nothing.

**`complete`** is every slice settled: the record says `done`, or the human's
own row status does (`done`, `cancelled` or `blocked`). A slice a human parked
is a decision that has already been made, and waiting on it forever would be the
drive refusing to hear it.

## 6b. The declared review window

`drive_plan(issue, review_minutes:)`, defaulting to `driver.plan_review_minutes`
and resolved ONCE at `drive_plan` into the record — so a tick reads one number
rather than reconciling a repo key against a per-call argument. Above zero, the
drive enters `plan-review`, posts **exactly one** notice, boards nothing and
spawns nothing until the window has run.

**The default is zero, and that is the argument.** Under `agent-ready` the human
has already pressed go; a window nobody is told about is unused, and telling the
orchestrator costs the turn this whole design exists to remove. The
orchestrator keeps three vetoes that cost no turn at all: `hold: true` per
slice, board edits at any time, and disposition at every review gate. The notice
is the price of the window, paid only when somebody asked for one.

## 7. The `pd-*` audit vocabulary

Twenty-one actions beside the review driver's `rd-*`, every row written with
`brand::AUDIT_ACTOR` as the actor and `on_behalf_of` as a detail key — so it is
the KEY, not the actor, that distinguishes a driver action, and an audit reader
filters on it. The prefix is what separates the two drivers when a reader wants
one rather than both.

The plan phase: `pd-started`, `pd-refused`, `pd-planner-spawned`,
`pd-plan-invalid`, `pd-plan-posted`, `pd-planner-consumed`.

The executor: `pd-boarded`, `pd-slice-spawned`, `pd-slice-cap-refused`,
`pd-slice-held`, `pd-slice-pr`, `pd-review-driven`, `pd-slice-merged`,
`pd-slice-consumed`.

The drive's own life: `pd-held`, `pd-resumed`, `pd-cancelled`, `pd-complete`,
`pd-notice`, `pd-recovered`, `pd-state-unreadable`.

**`pd-plan-invalid` is its own action rather than a detail on
`pd-plan-posted`**, and `pd-held` is its own rather than a state detail, for
`rd-ci-red`'s reason: a reader counting the thing that happened must not have to
match the rows where it did not.

**`pd-planner-consumed` is the one that makes the interception accountable.** A
driven planner's `report` goes to the drive instead of the orchestrator's pane —
the same narrowing the review driver's §7 makes, keyed on the agent id orrerix
minted at spawn rather than on anything the caller can type. "Consumed" is a
different word from "dropped", and every consumed event is on the record with
its kind, its agent and its issue, so traffic that stopped arriving as a prompt
is still attributable. `message_orchestrator` is never intercepted.

**`pd-slice-held` is one action carrying a closed reason**, rather than the
`pd-slice-blocked`/`pd-slice-closed` pair the plan comment sketched: a reader
asking "which slices are parked" asks one question, and the reason travels in
the row exactly as `pd-held`'s does. **`pd-slice-consumed` is
`pd-planner-consumed`'s twin** and is separate for the same reason those two
are: "the planner spoke" and "a worker spoke" are different facts.

## 8. The planner's output contract — WILL (P2/P4)

`planner.md` will state that the `orrerix-plan` block is mandatory when the
planner was spawned by a drive, and recommended otherwise. See #3040.

## What the plan driver deliberately does not do

Named here rather than left to be discovered, because each is a thing a reader
will look for in the code and not find.

- **It never merges, and never marks a slice done on anything but a merge it
  positively read.** See the status note at the top.
- **It spends no review, CI or rebase counter of its own.** Every PR's counters
  are the review driver's; handing a PR over does not reset them.
- **It runs no second cap check.** The delegate cap is `spawn_agent_bound`'s and
  the WIP caps are `upsert_task_from`'s, and the driver neither duplicates nor
  can bypass either.
- **No `git` of any kind.** The plan driver reads through the review driver's
  `RdRunner`, a `gh`-only view with no `git` method at all, so "never merges,
  never pushes" is structural rather than a promise.
- **No `#[tauri::command]`.** The four tools are MCP tools dispatched off the
  MCP thread, so CLAUDE.md constraint 10's GUI-thread unwind hazard is not
  engaged; a later frontend surface for plan drives would owe that argument.

## Related

- `doc/design/review-driver.md` — the review driver this one reuses, and whose
  record/reconcile/notice shape the plan driver copies rather than abstracts.
- `doc/design/groupid-and-path-roots.md` — why the slice id goes through
  `pathseg::check_segment` rather than through a private predicate.
