//! The role-contract templates and the fragments rendered into them: every
//! `include_str!("templates/*.md")` constant, the orchestrator playbook's
//! section index, the Rust-side fragments (`MERGE_QUEUE_NOTE`,
//! `REVIEW_DRIVER_NOTE`, `PLAN_DRIVER_NOTE`, `LOCKS_NOTE`, `LOCKS_ORCH_NOTE`)
//! and `role_template`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P2. It stays in
//! `src-tauri` beside `templates/`, so every `include_str!` path is unchanged,
//! and `mod.rs` re-exports it whole (`pub use templates::*`), so every
//! `orchestration::` spelling still resolves. The bytes are pinned by
//! `tests/fixtures/pre222/`; see docs/design/engine-extraction.md §6 for why
//! the template mapping lives here rather than in the engine.

use super::*;

// doc-hidden `pub` so the integration tests can reconstruct the pre-#222
// rendering of a template and assert loomux still writes exactly that when no
// workflow is active (`the_toggle_off_leaves_every_instruction_file_byte_for_byte_what_it_was`).
#[doc(hidden)]
pub const ORCHESTRATOR_TPL: &str = include_str!("templates/orchestrator.md");

/// The orchestrator **playbook** (#1683) — the on-demand half of the
/// orchestrator's contract. The resident core (`ORCHESTRATOR_TPL`) keeps the
/// INVARIANTS, the tool surface and every rule; the playbook carries the
/// situational **procedure**, served one `## ` section at a time by the
/// orchestrator-only MCP tool `read_playbook(section)`. The split exists
/// because the resident template is paid on every model call while a playbook
/// section is paid only when its trigger fires — and because the failure mode
/// of an on-demand document is not an unreadable section, it is an
/// orchestrator that never knows to ask: so every moved section leaves a
/// resident stub naming its trigger
/// (`every_playbook_section_has_a_resident_stub_naming_it` pins that pairing).
///
/// Rendered into `<group dir>/orchestrator-playbook.md` by
/// `write_instruction_files` with the same var list as every role file, so it
/// is what a default group reads and is manifest-tracked like them.
#[doc(hidden)]
pub const ORCHESTRATOR_PLAYBOOK_TPL: &str = include_str!("templates/orchestrator-playbook.md");

/// The rendered playbook's file name in the group dir (#1683). One constant
/// for the same reason `generated_instructions_manifest_path` owns its own
/// name: the writer in `write_instruction_files`, the reader in
/// `read_playbook`, and the manifest row must spell it identically or the
/// tool serves a file the render never wrote.
pub const ORCHESTRATOR_PLAYBOOK_FILE: &str = "orchestrator-playbook.md";

/// The closed enum of playbook section ids, in template order — the tool
/// description's index and `read_playbook`'s vocabulary.
///
/// It is a hand-maintained mirror of what the template's `## ` headings
/// derive to, and it is not allowed to drift:
/// `every_playbook_heading_yields_a_unique_id_and_the_tool_enum_lists_exactly_
/// them` derives the ids from `ORCHESTRATOR_PLAYBOOK_TPL` (the lessons
/// splitter's boundary, fenced code excluded) and asserts this slice equals
/// them exactly — a section added to the template without a row here, or a
/// row here whose section is gone, is a red at the scan, not a tool that
/// offers a section no template carries.
#[doc(hidden)]
pub const PLAYBOOK_SECTION_IDS: &[&str] = &[
    "about-this-playbook",
    "asking-the-human",
    "cost-guardrails",
    "autonomous-mode",
    "full-autonomy",
    "prototype-proceed",
    "label-signals",
    "planning-and-scheduling",
    "engineering-standards",
    // #3040 P2. The ONE copy of the definition of done
    // (`templates/dod.md`, substituted as `{{DOD}}`), served here rather than
    // inlined into the resident core: `orchestrator.md` was 44,990 B against
    // `RESIDENT_CORE_BUDGET`'s 45,000, so the 5.7 KB of DoD text could not go
    // there without the orchestrator paying it on every model call. The
    // playbook is on demand and has no such budget — the same argument
    // #2565 already made for a step it moved here.
    "definition-of-done",
    "delivery-notices",
    "merge-gate",
    "squash-closes-issues",
    "red-main",
    "mergeability",
    "ci-gate",
    "monitoring-open-prs",
    "learning-loop",
    "queue-orphans-and-refused",
    // #3441. The writing standard (`templates/writing.md`, substituted as
    // `{{WRITING}}`) — the same single copy every role file renders, served to
    // the orchestrator on demand rather than resident, for the budget reason
    // `definition-of-done` gives above.
    "writing-for-humans",
    // #1683 slice 2b (#3367 item 4). The long form of the resident
    // `## Your orrerix MCP tools` bullets and the `## The task board`
    // procedure: the core kept every rule in a shortened bullet and a stub
    // naming each of these, which is what took it under 35 KB.
    "tool-reference",
    "task-board",
];

/// The section id a playbook heading yields: lowercased, every run of
/// non-ASCII-alphanumeric characters collapsed to a single `-`, leading and
/// trailing `-` trimmed. `"About this playbook"` → `about-this-playbook`;
/// `"Prototype → Proceed"` → `prototype-proceed` (the arrow is one non-alnum
/// run like the spaces around it). Total and deterministic — the same heading
/// always yields the same id, which is the property both the resident stubs
/// and the tool's vocabulary are keyed on.
///
/// Non-ASCII *letters* (é, ü) are dropped rather than transliterated: the
/// playbook is orrerix-authored ASCII prose (constraint 8 — no repo or machine
/// vocabulary in product code), so a heading that would lose its whole id to
/// this rule is a template bug the uniqueness check at the scan catches, not
/// a case to handle here.
pub fn playbook_section_id(title: &str) -> String {
    let mut id = String::with_capacity(title.len());
    let mut in_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            id.push(c.to_ascii_lowercase());
            in_dash = false;
        } else if !in_dash && !id.is_empty() {
            id.push('-');
            in_dash = true;
        }
    }
    while id.ends_with('-') {
        id.pop();
    }
    id
}

/// Every playbook section id in the rendered playbook, in file order.
///
/// `rendered`, not the template: the tool serves the group's written copy, so
/// the ids that matter are the ones its headings yield. Pure — see
/// [`playbook_section`] for the read half.
pub fn playbook_section_ids(rendered: &str) -> Vec<String> {
    loomux_engine::lessons::split_sections(rendered)
        .into_iter()
        .map(|(title, _)| playbook_section_id(title))
        .collect()
}

/// One playbook section by id: the verbatim slice from its `## ` heading to
/// the next (or end of file). Pure; the audit line and the group-dir read
/// live on [`OrchRegistry::read_playbook`].
pub fn playbook_section(rendered: &str, id: &str) -> Option<String> {
    loomux_engine::lessons::split_sections(rendered)
        .into_iter()
        .find(|(title, _)| playbook_section_id(title) == id)
        .map(|(_, text)| text.to_string())
}

/// The `{{MERGE_QUEUE}}` fragment (#581 §11.1) — substituted into
/// `orchestrator.md` **only** when the repo declares `merge_queue: enabled:
/// true`, and empty otherwise.
///
/// It brings its own leading newlines because the placeholder sits at the end of
/// the preceding sentence: an empty substitution has to leave that line exactly
/// as it was, which is the invariant `a_workflow_placeholder_must_sit_at_the_
/// end_of_a_line_it_shares` pins.
///
/// Everything here names machinery a queue-less group does not have — the three
/// tools, the refusal vocabulary, the kick-back routing rule — which is why it
/// is a fragment and not template prose.
pub(super) const MERGE_QUEUE_NOTE: &str = r#"

**This repo runs a merge queue, and that changes how approved sub-PRs land.** Instead of merging
an approved sub-PR onto the integration branch yourself, hand it to the queue. It exists because a
green sub-PR is evidence about a **PR**, not about a **branch**: several individually-green PRs can
still produce a red integration branch — a semantic conflict, a test that only fails when two
changes coexist, a lockfile that resolves differently once both are present — and when that happens
nobody can say which one did it.

- `queue_merge(pr, target?)` — hand an **approved** sub-PR to the queue, once per PR, after its
  review has passed. loomux batches the queued PRs onto a scratch ref, opens a **draft PR** so this
  repo's own CI judges that exact object, fast-forwards it onto the target on green, and on red
  bisects and kicks back the one PR that broke the combination. **The commit that was tested is the
  commit that lands** — nothing is rebuilt after CI. `target` is an assertion, not a choice: it
  checks that the PR's base resolves to that branch, and a mismatch refuses.
- `merge_queue_status()` — where the queue stands. Read-only.
- `cancel_queued_merge(pr)` — take a PR back out, including one inside an in-flight batch (that
  batch is abandoned and rebuilt without it; nothing lands).

**Nothing here loosens.** The queue never touches the default branch — structurally, not by policy
— never calls `gh pr merge`, and **never grants what the review gate would not**: it re-enforces
that same gate itself, at batch build *and* again at the moment of submit, so a reviewer's `fail`
or a rebase in between still stops the landing. INVARIANT 1 is untouched.

**Refusals are a closed set, and each says what to do** — read the reason rather than retrying:
`base-is-default` (that PR targets the default branch; the queue only lands on integration
branches) · `base-unverifiable` (loomux could not resolve the base or the repo default, and unknown
is never treated as safe) · `base-not-target` (this queue is already landing elsewhere — drain it
first; the entries already queued were approved against that other branch) · `gate-not-configured`
(no gate covers this target, and the backend will not push approved-by-nobody PRs under its own
authority) · `gate-not-met` · `already-queued` · `queue-full`.

**When a batch goes red you get ONE notice naming the culprit**, and a comment lands on that PR
with the failing check, the batch id and the sibling set. **Routing is yours** — loomux
deliberately does not brief the owning worker, because worker liveness, resume-versus-fresh-spawn
and folding this in with whatever else is pending are your calls, and the board mapping that equips
them is yours. Two things to carry into that call: bisect isolates **a** culprit, not necessarily
**the** culprit (a genuine pairwise interaction attributes to whichever entry the split isolated),
and the survivors were already re-queued at the front — do not re-queue them yourself.

**A batch can also come back `unverifiable`.** That is not a red batch and **no PR is implicated**:
the repo's checks never reached a terminal state within the bound. Nothing landed, the entries were
re-queued, and the thing to look at is the repo's CI.

**One thing this changes about merging.** For PRs that are **in the queue**, the speculative
merge **is** the mergeability probe, so a sibling that would conflict is kicked back at
construction time with no CI spent and nothing landed — wait to be told. This does **not** cover
open PRs that are not queued: they still get the open-PR sweep, which asks whether each PR still
merges, never whether it is fresh — a branch merely behind its base is left alone (INVARIANT 7)."#;
/// The `{{REVIEW_DRIVER}}` fragment (#1778 §5.5) — substituted into
/// `orchestrator.md` **only** when the repo declares `driver: enabled: true`,
/// and empty otherwise.
///
/// It brings its own leading newlines for `MERGE_QUEUE_NOTE`'s reason: the
/// placeholder sits at the end of the preceding sentence, so an empty
/// substitution has to leave that line exactly as it was — the invariant
/// `a_workflow_placeholder_must_sit_at_the_end_of_a_line_it_shares` pins.
///
/// **A fragment rather than template prose**, on the same test the queue's note
/// passes: everything here names machinery a driverless group does not have —
/// three tools, a closed refusal vocabulary, and a narrowing of where a
/// delegate's report arrives. Prose about a mechanism the reader does not have
/// is an invitation to go looking for it.
///
/// **The one thing it must say that nothing else can.** §7 makes the
/// orchestrator's view of its own group *narrower* while a drive is live: a
/// driven delegate's `report` and `review_verdict` stop arriving as prompts. An
/// orchestrator that did not know that would read the silence as a stalled
/// delegate and go looking for it — so the narrowing, the two surfaces that
/// compensate for it (`review_drive_status()` and the audit log), and the one
/// channel that is never intercepted are all named here rather than left to be
/// inferred from a notice that does not arrive.
pub(super) const REVIEW_DRIVER_NOTE: &str = r#"

**This repo runs an engine review driver, and it can perform the worker-reviewer rounds you do by
hand.** Hand it ONE PR and orrerix does, on its own poll loop: wait for CI; spawn or resume each
reviewer lane the merge gate requires, in gate order, briefing each with the head and what moved;
hand a `fail`, a red run or a conflict back to the worker session you named; and stop with **one**
notice here at gate-satisfied, at an `escalate`, or at a bound.

- `drive_review(pr, worker_session, reset_counters?, rounds_already_spent?)` — start a drive, or
  resume a held one. Give the **full** worker session id: orrerix resolves it once, here, and
  stores what came back, because a prefix that is unique today can become ambiguous as the roster
  grows. It does **not** read the board for it — the board is agent-writable, so a check that
  trusted it would be a check the thing being checked gets to answer.
- `review_drive_status()` — where your drives stand. Read-only, and **read it after a compaction**:
  a drive you have forgotten is still running.
- `cancel_review_drive(pr)` — stop one.

**Consent is per PR and it is yours.** A drive never starts on its own, and by default not on a
worker's `report(done)`: the PRs where a drive is wrong are ordinary ones — a scratch or
red-evidence PR, a release bump, a PR the human said they would read themselves. INVARIANT 8 makes
*what starts* your call. **The one opt-in is the repo's, not a delegate's:** where `driver:` sets
`auto_drive_on_done: true`, a worker YOU spawned that reports `done` with `ref: #N` for its
OWN open PR (its recorded branch is the PR's head) starts the drive on its own session, and its
report reaches you inside that drive's first notice rather than on its own. It is refused — and
the report delivered as always — for a `[scratch]` PR, a ref that is not a PR, another branch's
PR, a PR already driven or parked, and a PR that already carries any verdict (a drive starts at
zero rounds, which is only true of a PR nobody has reviewed); each refusal is on the audit log as
`rd-auto-start-declined` with its reason.

**What the driver may never do**, so you never have to wonder: merge, or use any landing verb;
write a merge grant; relabel or edit an issue or a PR, bodies included; widen or author a brief
(every variable in one is a fact orrerix READ); decide a disposition; or open the gate
— only a reviewer's own `review_verdict` does that.

**It kills a pane in exactly two cases**, and it is worth knowing which so you do not go looking
for them: a reviewer lane whose verdict is recorded at the head it is driving, and a worker that
has reported and gone idle once the drive has read that report. Both are *released* — the pane
closes, the delegate slot frees at once, and the session is kept, so the next round comes back to
the same conversation in a fresh pane. It never kills anything else, and never kills a pane
because slots are short; each release is on the audit log as `rd-lane-released` or
`rd-worker-released`, and costs you no line in this pane. It is strictly **additive** to the merge gate:
it never grants what the gate would not, and a completed drive is never a substitute for a
reviewer's `pass`.

**Counters are INVARIANT 9's, and they clamp toward it and never away.** Three review rounds, three
CI attempts, one rebase. **One bounded exception, and it is worth knowing because it saves you a
wake:** when a round's blocking fail comes back on a lane the driver had re-briefed about the PR
BODY alone — the head has not moved since a full round was already spent on it, and every required
lane had already answered about that code — the drive hands the worker back once more rather than
parking at the bound. Once per drive, never for a fail about the code, and the next blocking fail
after it parks the drive whatever it is about. It is on the audit log as `rd-round-grace` and on
`review_drive_status` as `grace_used`, which is the field to read when a live drive shows three
rounds of three. "Yours count too" is a property of the budget rather than of who spends
it: if you already reviewed this PR by hand and got a `fail`, pass `rounds_already_spent`, or the
drive starts at zero and spends three more.

**Where `driver:` sets `fix_nonblocking_rounds: N`, the driver runs the non-blocking loop
itself.** At a satisfied gate where every required lane PASSED and stated `0 blocking` with some
lane stating a non-blocking count above zero, it hands the PR back to the worker ("review:
request-changes, findings on PR #N, address all, report when green"), waits for the report and
green, re-briefs the lanes, and repeats — up to `N` times, and **every one of those rounds is a
review round**, so the three-round bound above is shared and never exceeded. It wakes you on an
`escalate`, a stated blocking finding, a lane that did not state its counts (unknown is never
zero), a worker that handed back an unchanged PR, `N` spent, or nothing left open; a `fail` after
one of these rounds takes the ordinary fail hand-back. Its `GATE SATISFIED` line then says
`Non-blocking rounds run by the driver: k/N; residual: …` — the disposition of that residual is
still yours (INVARIANT 3). Each round is `rd-auto-handback` on the audit log and `nit_rounds` on
`review_drive_status`.

**One thing genuinely narrows while a drive is live, and it is the reason this paragraph exists.**
A driven delegate's `report` and `review_verdict` are consumed by the driver instead of arriving
here as prompts. Do not read that silence as a stalled delegate. **The worker's half starts at the
first hand-back**, not at `drive_review`: interception is keyed on a pane orrerix has recorded, and
it records the worker's when it first hands the PR back, so a worker `report` that arrives while
the drive is still on CI or on a reviewer reaches you exactly as it always did. Nothing in the
drive reads it, so nothing is lost — it is simply not silent yet. Two surfaces compensate:
`review_drive_status()` and the audit log, where every consumed event is recorded as `rd-consumed`
with its kind, its agent and its PR — consumed is a different word from dropped.
**`message_orchestrator` is never intercepted**: a delegate's own words always reach you
unchanged, and the drive then holds so you know why they came.

**A held drive is PARKED, not finished.** It keeps its spent counters, `review_drive_status()`
lists it, and `drive_review` on it resumes the same budget — `reset_counters: true` spends another
three, which is a decision rather than a side effect of typing the call twice. The kick-back names
the one fact that decides what you do next and the tool that acts on it.

**Sequence with the merge queue is serial and has a direction.** A driven PR may not be queued and
a queued PR may not be driven — `drive_review` refuses `in-merge-queue`, `queue_merge` refuses
`in-review-drive`. Let the drive reach gate-satisfied, disposition its findings (INVARIANT 3 —
that is still yours), and *then* queue. **The one exception is the CLEAN case** (#3367): every
required lane passed at the head declaring `open_findings: 0`, CI green. Its GATE SATISFIED line
says `clean: true` — nothing to disposition — and where `merge_queue` is on the driver has already
submitted it to the queue, whose answer (`queued at position N` or its refusal) ends the line."#;


/// The `{{PLAN_DRIVER}}` fragment (#3040 P4) — substituted into the
/// **playbook's** `Planning and scheduling` section, and only when the repo
/// declares `driver: enabled: true` AND `plan_enabled: true`.
///
/// **Why the playbook and not the resident core.** `orchestrator.md` sat 45
/// bytes under `RESIDENT_CORE_BUDGET` at #3040 P2, and the core is paid on
/// every model call while a playbook section is paid when its trigger fires.
/// The trigger here is exact and already resident: the core's Planning &
/// scheduling section says to read `planning-and-scheduling` "when planning any
/// work item — and when deciding whether to spawn a planner", which is the one
/// moment `drive_plan` is the alternative being weighed. So no new section id
/// and no new stub: the pairing
/// `every_playbook_section_has_a_resident_stub_naming_it` polices is already
/// satisfied by the pointer that was there, and the core does not grow.
///
/// **A fragment rather than playbook prose**, on `REVIEW_DRIVER_NOTE`'s test:
/// everything here names machinery a group without `plan_enabled` does not
/// have — four tools, a fence schema, a closed hold vocabulary, and a second
/// narrowing of where a delegate's report arrives. Prose about a mechanism the
/// reader does not have is an invitation to go looking for it.
///
/// It brings its own leading newline for `MERGE_QUEUE_NOTE`'s reason: the
/// placeholder sits at the end of the preceding paragraph's last line, so an
/// empty substitution has to leave that line exactly as it was
/// (`a_workflow_placeholder_must_sit_at_the_end_of_a_line_it_shares`).
pub(super) const PLAN_DRIVER_NOTE: &str = r#"
**This repo also runs an engine PLAN driver, and it can carry a labelled issue from planning to
merged slices without a turn from you.** Where the review driver takes one PR through review, this
takes one ISSUE through planning: `drive_plan(issue)` spawns a planner from your roster, validates
the plan it writes, boards it, and spawns each slice's worker as its dependencies clear — handing
every PR to the review driver itself, so the first thing you usually hear about a driven issue is
that driver's own `GATE SATISFIED`.

**Call it instead of hand-briefing when the issue is one you would have spawned a planner for
anyway.** The ladder above is unchanged and still decides: contained work you could already write
the brief for is still a worker you spawn yourself, and `drive_plan` on it would buy a planner
nobody needed. What it replaces is the sequence AFTER a plan — reading it, boarding it, writing k
briefs, spawning k workers, and calling `drive_review` k times — which is where the turns actually
went.

- `drive_plan(issue, planner_block?, review_minutes?, base?)` — start one. Refused unless the issue
  is open and carries `agent-ready` or `agent-investigation`; the label is consent and it is
  **re-read before every spawn**, so taking it off mid-drive parks the drive. An
  `agent-investigation` drive stops at the posted plan and never boards or spawns anything.
- `plan_drive_status()` — where your drives stand. **Read it after a compaction**: a drive you have
  forgotten is still running.
- `cancel_plan_drive(issue)` — stop one. It kills nothing: panes keep running under you, and their
  reports start reaching this one again. Ending a pane is still `kill_agent`.
- `resume_plan_drive(issue)` — restart a parked drive, releasing its parked slices with it.

**The plan is a fenced `orrerix-plan` block, and it is validated before anything is posted.** An
invalid one comes back to the planner as a tool error with line numbers, nothing reaches the issue,
and it fixes it inside its own turn — which costs you nothing. Three refusals park the drive with
the reasons. Nothing is ever repaired: an id, a branch or a dependency orrerix refuses is refused,
never rewritten. A worker's brief is a header orrerix writes, the planner's own words **verbatim**,
then the same definition of done every brief here quotes.

**What the driver may never do**, so you never have to wonder: merge, or mark a slice done on
anything but a merge it positively read; write or widen a brief; spawn past the delegate cap or the
board's WIP limits, which refuse it exactly as they refuse you; touch an issue, a PR or a label;
decide a disposition. Every one of those is still yours, and INVARIANT 3 in particular is untouched
— a drive reaching gate-satisfied is a gate for you to disposition, not a merge.

**Your veto is the BOARD, and it works while the drive runs.** Readiness is re-read from the rows
every tick and never cached: mark a row `done` and its dependents start; set one `blocked` or
`cancelled` and it never spawns; edit its deps and they are respected; delete it and the drive
parks with a notice naming it. A slice the planner flagged `hold: true` is boarded and never
spawned — that is the planner telling you it carries a decision that is yours to make, and briefing
it by hand is what you do with it. If you want a pause between the plan and the first spawn,
`drive_plan(issue, review_minutes: N)` posts exactly one notice and waits.

**One slice parks without parking the plan.** A worker that reports `blocked`, one whose pane dies
without reporting, one that says done and never opens a PR, a PR closed unmerged, or a delegate cap
that stays full — each parks that slice, with one notice, and the independent slices keep going.
Withdrawn consent, a struck row and the whole-drive stall backstop park the drive.

**The same narrowing the review driver has, for the same reason.** A driven planner's and a driven
slice worker's `report` are consumed by the driver instead of arriving here as prompts; the audit
log (`pd-*`) and `plan_drive_status()` are what compensate, and `message_orchestrator` is never
intercepted, so a delegate's own words always reach you."#;
/// The `{{LOCKS}}` / `{{LOCKS_ORCH}}` fragments (#858) — substituted into
/// `worker.md`/`reviewer.md` and `orchestrator.md` respectively, **only** when
/// the repo declares a non-empty `resources:` block, and empty otherwise.
///
/// Two fragments rather than one because the orchestrator's job with a lock is
/// a different job: an agent takes and releases one, an orchestrator decides
/// which task needs one and reads the queue to understand its fleet. One shared
/// paragraph would have had to say both things to both readers.
///
/// Conditional at all, rather than template prose, for the reason
/// `the_default_rendering_never_names_the_gate_machinery` states: a group whose
/// repo declares no resources has no lock tools, and prose naming a mechanism
/// the reader does not have is an invitation to go looking for it. Gated on the
/// declaration and not merely on the advanced-orchestrator toggle, for the same
/// reason `MERGE_QUEUE_NOTE` is gated on the queue being enabled — a repo can
/// run a workflow and declare no resources, and that group has the workflow's
/// machinery but not this.
///
/// Each brings its own leading newline because the placeholder sits at the end
/// of the preceding bullet's last line: an empty substitution has to leave that
/// line exactly as it was, which is the invariant
/// `a_workflow_placeholder_must_sit_at_the_end_of_a_line_it_shares` pins.
pub(super) const LOCKS_NOTE: &str = r#"
- `acquire_lock(name, note?, wait_minutes?)` / `release_lock(name)` / `list_locks()` — this repo
  declares scarce resources (a build slot, a GPU, a device, a port) that agents must take turns on.
  Take the lock **before** the work that needs it, and release it the moment that work is done.
  `acquire_lock` never blocks: it answers "it is yours" or "you are queued at position N".
  **Queued means END YOUR TURN** — never sleep, poll, or re-call in a loop. An `[orrerix]` notice is
  typed into this pane when the lock becomes yours, and a pane sitting mid-turn cannot take that
  delivery, so waiting for it is the one thing guaranteed not to work. Re-calling when you already
  hold or already wait is a harmless no-op that reports where you stand. A hold you forget is
  reclaimed automatically at the declared max-hold and audited as a reclaim — releasing it yourself
  is faster for everyone behind you and a better record. `list_locks()` shows who holds what."#;

/// The orchestrator's half — see [`LOCKS_NOTE`].
pub(super) const LOCKS_ORCH_NOTE: &str = r#"
- `acquire_lock(name, note?, wait_minutes?)` / `release_lock(name)` / `list_locks()` — this repo
  declares scarce resources (a build slot, a GPU, a device, a port); your agents get the same three
  tools. **Two things are yours here.** First, name the lock in the brief whenever a task will touch
  one — a worker that was never told will not think to take it, and nothing enforces it for you.
  Second, `list_locks()` is the answer to "why is that worker quiet": a queued agent is waiting its
  turn, not stalled, and the queue tells you in what order the work will actually happen — schedule
  around it rather than spawning another worker to contend for the same slot. Holds are bounded (the
  repo declares a max) and every acquire, release, reclaim and timeout is in the audit log."#;

/// The writing standard every role reads (#3441): human-first bodies, short
/// comments, terse board text, issues only for tracked work, and the one-line
/// AI tail. ONE copy, substituted as `{{WRITING}}` under a `## Writing for
/// humans` heading into `worker.md`, `reviewer.md`, `planner.md`, `lead.md`
/// and the orchestrator playbook — the `{{DOD}}` pattern (`brief::DOD_TPL`),
/// for the same reason: a rule an agent executes literally, held in five
/// files, drifts. `manager.md` does not render it: a manager never posts to
/// GitHub or writes the board.
///
/// `pub` for the golden fixture in `tests/workflow/goldens.rs`, which pins these
/// bytes against a human-blessed copy; use [`writing_body`] for the value.
#[doc(hidden)]
pub const WRITING_TPL: &str = include_str!("templates/writing.md");

/// The `{{WRITING}}` substitution value: [`WRITING_TPL`] without its trailing
/// newline, because the placeholder sits on a line of its own that already
/// supplies one (`brief::dod_body`'s reasoning, byte for byte).
pub fn writing_body() -> &'static str {
    WRITING_TPL.trim_end_matches('\n')
}

#[doc(hidden)]
pub const WORKER_TPL: &str = include_str!("templates/worker.md");
#[doc(hidden)]
pub const REVIEWER_TPL: &str = include_str!("templates/reviewer.md");
#[doc(hidden)]
pub const PLANNER_TPL: &str = include_str!("templates/planner.md");
/// The manager's role contract (#1161).
///
/// Unlike the four above it is **not** part of any default group's rendering:
/// `write_instruction_files`'s class-fallback loop deliberately does not write
/// `manager.md`, because no built-in roster has a manager and a file appearing
/// in every group dir would be a default-path change for a feature nobody
/// declared. It is written by the block loop, for a roster that declares one.
///
/// It is still golden-pinned the same way (`tests/fixtures/pre222/manager.md`
/// and the `LIVE` pairing in `tests/workflow/goldens.rs`) — what it is NOT part of is
/// the two "what a default group reads" pins, which have nothing to compare it
/// against.
#[doc(hidden)]
pub const MANAGER_TPL: &str = include_str!("templates/manager.md");
/// The lead pane's role contract (#2519).
///
/// Like [`MANAGER_TPL`] it is outside `write_instruction_files`'s
/// class-fallback loop — no built-in roster has a lead, and a `lead.md`
/// appearing in every group dir would be a default-path change for a feature
/// nobody turned on. It is written by the block loop, for the roster the
/// launcher's toggle mints.
///
/// **Golden-pinned since slice B**, and the timing was the decision. The pin
/// (`tests/fixtures/pre222/` + the `LIVE` pairing in `tests/workflow/goldens.rs`)
/// exists to make an accidental edit to bytes a shipped pane already reads
/// fail loudly. Slice A shipped the CLASS and delivered nothing, so there was
/// no shipped reading to regress and pinning then would have meant a re-bless
/// in slice B — one chance per slice to bless a mistake. Slice B ships the
/// launch path that pastes this file's kickoff, so it is pinned here, in
/// `GOLDENS` and `LIVE` but NOT in `PRE222`: a default group has no lead block,
/// so no `lead.md` is written into its dir. `block.md` and `workflow.md` stay
/// unpinned, on the terms this paragraph used to state for all three.
///
/// It carries `{{GROUP_ID}}` and `{{REPO}}` and no other placeholder: a lead
/// may never carry a repo persona (`workflow::persona_allowed` is false for
/// every `Role::is_fixture` class), it holds no locks, and there is no workflow
/// file in a lead group for a `{{WORKFLOW}}` fragment to describe.
#[doc(hidden)]
pub const LEAD_TPL: &str = include_str!("templates/lead.md");
/// Workflow-aware fragments (#222), substituted into the role templates above as
/// `{{WORKFLOW}}` (orchestrator) and `{{BLOCK_NOTE}}` (worker/reviewer/planner).
///
/// They are *fragments*, not templates, for one reason: `render_template` is a
/// dumb `{{KEY}}` replace with no conditionals, and it should stay that way. So
/// the conditional lives in Rust (`workflow::roster_is_custom`) and the prose
/// lives in markdown, where the rest of the prose lives and where it can be read
/// and reviewed as prose. **Both placeholders resolve to the empty string for the
/// default roster**, and both sit line-final in their templates, so a group with
/// no workflow gets instruction files that are byte-for-byte the pre-#222 ones.
pub(super) const WORKFLOW_TPL: &str = include_str!("templates/workflow.md");
pub(super) const BLOCK_TPL: &str = include_str!("templates/block.md");

/// The built-in role contract template for a capability class. A block's persona
/// *layers on* this (append) or replaces its body (replace) — but never its
/// [`mechanics_core`].
///
/// A free function taking a [`Role`], not a method on one, and that is the shape
/// #888 slice A2 batch 4 deliberately chose. `Role` itself is data and lives in
/// `loomux_engine::model`; an inherent impl has to live in the crate that defines
/// its type, so a `Role::template()` would have pulled `templates/*.md` — and
/// with them the byte-golden fixture root `tests/fixtures/pre222/` that pins
/// those four files — across the crate boundary as a side effect of moving an
/// enum. The template mapping belongs next to the content it names and next to
/// the fixture that blesses it, which is here.
///
/// Its former partner did NOT stay: `role_instructions_file` moved to
/// `loomux_engine::model` in batch 5 (re-exported below) because it loads no
/// bytes — it names a file in the *group dir*, and `workflow::Block` calls it
/// from inside the engine. Content stays; a name that happens to resemble one
/// travels. See `docs/design/engine-extraction.md` §6.
pub(crate) fn role_template(role: Role) -> &'static str {
    match role {
        Role::Orchestrator => ORCHESTRATOR_TPL,
        Role::Worker => WORKER_TPL,
        Role::Reviewer => REVIEWER_TPL,
        Role::Planner => PLANNER_TPL,
        Role::Manager => MANAGER_TPL,
        // Solo panes never receive a kickoff prompt and have no role
        // template — an arbitrary human-launched CLI, not a loomux
        // persona (#271 W3 addendum). Never reached: `kickoff_body`
        // (the only caller of a role's template machinery) is never
        // invoked for `Role::Solo`.
        Role::Solo => unreachable!("solo panes have no role template"),
        // #2519. A real template, not an `unreachable!`, and the reason is that
        // this arm IS reachable from a public path: `write_instruction_files`'s
        // block loop renders every block in a roster, so it runs the moment a
        // lead block exists — which is before any lead pane is opened, and in a
        // function whose failure mode would be a process abort rather than an
        // error. See [`LEAD_TPL`] for when it joined the golden pin, and why.
        Role::Lead => LEAD_TPL,
    }
}
