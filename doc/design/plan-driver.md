# The plan driver (#3040)

**Status: the drive runs as far as a plan.** Contracts 1–5 and 7 are in the
present tense because they describe code in the tree; 6 and 8 are WILL-tense,
and so is every paragraph below marked WILL. Do not act on a WILL-tense section
as though it described the build you are looking at.

**Where the drive stops in this build**, stated once here because every section
below assumes it: P3a spawns the planner, validates and stores the plan, and
then stops. An `agent-investigation` issue reaches `complete` — the plan IS its
deliverable, so that is the drive finishing, not a shortfall. An `agent-ready`
issue reaches `boarding` and parks on `held(awaiting-p3b)`, with one notice in
the orchestrator's pane: the board rows, the worker spawns and the hand-off to
the review driver land in P3b. A named, audited, notice-bearing park is the
whole point of that hold — a drive that quietly did nothing would be worse than
one that never started.

The full design is the plan comment on #3040. This note exists so each contract
gets a durable home as its slice lands, rather than living only in an issue
comment.

## 1. The `orrerix-plan` block (P1 — shipped)

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
| `slices[].branch` | yes | A git ref name, checked and **refused, never sanitized**. Unique within the plan, **case-insensitively**. |
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

## 2. `<group-dir>/plan_drives.json` v1 (P3a — shipped)

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
      "slice_tasks": {},
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

**`slice_tasks` is empty in this build** and is persisted anyway: P3b fills it
with the slice-id → board-row map, and shipping the field now means the record's
shape does not change under a running fleet.

### The states

| state | meaning | leaves for |
| --- | --- | --- |
| `planning` | a planner pane is open; waiting for a plan block | `plan-posted`, `held`, `cancelled` |
| `plan-posted` | a valid block is stored, with the comment it was posted as | `boarding`, `complete`, `held`, `cancelled` |
| `boarding` | the plan is being turned into rows — **P3a parks here** | `held`, `cancelled` |
| `complete` | terminal: the drive did everything it was going to | — |
| `cancelled` | terminal: cancelled by tool, or the issue is positively closed | — |
| `held` | **parked**, carrying a reason | back to the state it came from, or `cancelled` |

One parked state carrying a closed reason, rather than seven states: a reader
asking "is this drive parked" asks one question, and the reason travels in the
notice and the audit row instead of being inferred from which field is set.

**A resume returns to the state the hold came FROM**, which is why `held` has
three outgoing working arcs. A drive parked on `planner-stalled` and one parked
on `awaiting-p3b` resume into different work, and a single `held → planning`
arc would silently re-open a planner for a plan that is already posted.

### The hold reasons

| reason | what happened |
| --- | --- |
| `plan-invalid` | three plan blocks refused; the last reasons are on the record |
| `plan-missing` | the planner finished, or its pane went, without posting |
| `planner-stalled` | neither a post nor a report inside `planner_timeout_minutes` |
| `planner-blocked` | the planner reported `blocked` |
| `consent-withdrawn` | the issue's label was withdrawn while the drive was live |
| `awaiting-p3b` | the plan is posted and this build has no executor for it |
| `drive-stalled` | the whole drive outran `drive_timeout_minutes` |

P3b extends both vocabularies. An older build reading a newer file refuses it
through the `version` check above rather than acting on a word it cannot read,
which is why adding a state word is a schema question and not a free one.

## 3. `driver:` block keys (P3a — shipped)

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

## 4. The four MCP tools (P3a — shipped)

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

## 5. `post_issue_comment` for a driven planner (P3a — shipped)

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

## 6. `templates/dod.md` and `{{DOD}}` — WILL (P2)

The Definition of Done will exist in exactly one file, quoted into both the
orchestrator template and every driver-composed brief. See #3040.

## 7. The `pd-*` audit vocabulary (P3a — shipped for the plan phase)

Thirteen actions beside the review driver's `rd-*`, every row written with
`brand::AUDIT_ACTOR` as the actor and `on_behalf_of` as a detail key — so it is
the KEY, not the actor, that distinguishes a driver action, and an audit reader
filters on it. The prefix is what separates the two drivers when a reader wants
one rather than both.

`pd-started`, `pd-refused`, `pd-planner-spawned`, `pd-plan-invalid`,
`pd-plan-posted`, `pd-planner-consumed`, `pd-held`, `pd-resumed`,
`pd-cancelled`, `pd-complete`, `pd-notice`, `pd-recovered`,
`pd-state-unreadable`.

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

WILL (P3b): `pd-boarded`, `pd-slice-spawned`, `pd-slice-cap-refused`,
`pd-slice-blocked`, `pd-slice-pr`, `pd-review-driven`, `pd-slice-merged`,
`pd-slice-closed`.

## 8. The planner's output contract — WILL (P2/P4)

`planner.md` will state that the `orrerix-plan` block is mandatory when the
planner was spawned by a drive, and recommended otherwise. See #3040.

## What P3a deliberately does not do

Named here rather than left to be discovered, because each is a thing a reader
will look for in the code and not find.

- **No board rows and no worker spawns.** That is P3b, and `held(awaiting-p3b)`
  is where the drive says so.
- **No review window.** `plan_review_minutes` is recorded and spent on nothing;
  the `plan-review` state and its one notice are P3b's.
- **No consent re-check before a spawn**, because there is no spawn. Consent IS
  re-read every tick, and a withdrawn label parks the drive — what P3b adds is
  the check immediately before each slice spawn.
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
