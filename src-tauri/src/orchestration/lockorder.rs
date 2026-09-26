// The registry's declared lock ranks. Its documentation is the `///` block on
// `pub mod lockorder;` in `orchestration/mod.rs`, which this file's contents
// sat under until #3498 P2 moved them here verbatim; the design note is
// docs/design/lock-order.md §4.

use loomux_engine::lockwatch::LockRank;

/// `marker_io` — consent-marker IO, outermost.
///
/// Its own claim: "`marker_io` is taken FIRST and outermost — the set locks
/// and `AUDIT_LOCK` are taken under it, never the reverse."
pub const MARKER_IO: LockRank = LockRank::new(100);

/// `group_file_io` — the `group.json` read-modify-write, outermost.
///
/// Its own claim: "taken FIRST and outermost — the `groups` lock and
/// `AUDIT_LOCK` may be taken under it, never the reverse."
///
/// Ranked BELOW `marker_io` rather than beside it. Nothing takes both
/// today; the two are different durable stores with different callers, and
/// giving them one rank each (rather than one shared rank) is what keeps
/// "same rank" meaning "the same field from two registries" — see
/// [`loomux_engine::lockwatch::LockRank`].
pub const GROUP_FILE_IO: LockRank = LockRank::new(200);

/// `queue_persist` — serializes writers of a group's `queue.json`.
///
/// Its own claim: "this BEFORE `queues`, never the reverse", and it is
/// taken with no other lock held.
pub const QUEUE_PERSIST: LockRank = LockRank::new(300);

/// `queues` — the live per-pane delivery queues (the `QueueMap`'s lock).
///
/// Under `queue_persist` (above), and above the two staging maps:
/// "Lock order is `queues` -> `recovered_queue` -> `recovered_markers`."
pub const QUEUES: LockRank = LockRank::new(400);

/// `recovered_queue` — entries read back from disk that have not yet found
/// a pane. Under `queues`, over `recovered_markers`.
pub const RECOVERED_QUEUE: LockRank = LockRank::new(410);

/// `recovered_markers` — the unreplayable staging half. Innermost of the
/// three: `archive_staged_overflow` takes `recovered_queue` then this one,
/// "matching `group_queue_entries`".
pub const RECOVERED_MARKERS: LockRank = LockRank::new(420);

/// `by_pty` — the pane -> agent reverse index.
///
/// `session_for_pty` takes this and then `agents`, and RELEASES this one
/// first — a `let`-statement temporary — so the two do not in fact nest
/// today. The rank records that order, which is the order any future site
/// that DOES nest them has to follow.
///
/// Stated as what the code does rather than as a quotation from that
/// function's doc (#1702): the doc used to promise exactly this and the
/// promise was worthless to its CALLER, which was holding `agents` when it
/// called in. See `docs/design/lock-liveness.md` §6.
pub const BY_PTY: LockRank = LockRank::new(500);

/// `agents` — the agent table. Under `by_pty`, over `groups`.
pub const AGENTS: LockRank = LockRank::new(510);

/// `groups` — the group table.
///
/// Innermost of the core three, and the one `group_file_io` names as
/// takeable under itself. #1611 (Phase 3b) collapses `groups`/`agents`/
/// `by_token`/`by_pty` into one `Core` lock, at which point these three
/// ranks become one; they are separate here because 3a lands first and
/// alone.
pub const GROUPS: LockRank = LockRank::new(520);

/// `delivered_prompts` — prompt bodies keyed by CLI session.
///
/// Ranks under `agents` because a caller that has to RESOLVE a pty to its
/// session takes `by_pty` and then `agents` first, releasing both before
/// this map is taken. Stated from the code rather than quoted from that
/// field's doc: the doc used to make exactly this claim, #1702 moved the
/// resolution out of the record read to the callers, and a rank that cites
/// a sentence which may be rewritten is a rank nobody can re-check. A
/// caller holding an agent snapshot resolves nothing and reaches this map
/// having taken no registry lock at all.
pub const DELIVERED_PROMPTS: LockRank = LockRank::new(600);

/// `delivered_notices` — what loomux wrote into each pane.
///
/// Its own claim: "takes no other registry lock while held", and its writer
/// runs inside `deliver_now`'s paste loop. A leaf, ranked near the bottom
/// so anything it were to take would be reported.
pub const DELIVERED_NOTICES: LockRank = LockRank::new(610);

/// `agent_seq_persist` — the agent-id mint (seed, bump, persist).
///
/// Its own claim: "takes no other registry lock while held, and no caller
/// holds one when it calls in". Ranked as a leaf, which enforces the first
/// half. The second half — that no caller holds one — is not a rank
/// question and is not enforced here.
pub const AGENT_SEQ_PERSIST: LockRank = LockRank::new(700);

/// `tasks_lock` — the task board's read-modify-write.
///
/// A leaf except for `AUDIT_LOCK` and the app handle — plus the one nesting
/// `needs_you_lock` names: "`tasks_lock` -> `needs_you_lock`, never the
/// reverse", because `upsert_task` keeps the board lock across
/// `sync_demo_item`.
pub const TASKS: LockRank = LockRank::new(800);

/// `needs_you_lock` — the demo-item store. Under `tasks_lock`, by that
/// field's own claim.
pub const NEEDS_YOU: LockRank = LockRank::new(810);

/// `questions_lock` — the question registry's read-modify-write.
///
/// "A leaf of its own, not `tasks_lock`", taking only `AUDIT_LOCK` and the
/// app handle. Nothing nests it with the siblings below; the relative order
/// among the four file leaves is therefore arbitrary, and an inversion
/// report on any pair of them would be a genuinely new fact.
pub const QUESTIONS: LockRank = LockRank::new(820);

/// `mailbox_lock` — the mailbox's read-modify-write.
///
/// "Lock order: nothing. It is taken alone" — `post_to_manager` resolves
/// the manager block through `self.group(..)` BEFORE taking this guard,
/// deliberately, so `groups` is never taken under it. That is exactly what
/// this rank enforces: `groups` is 520 and this is 830.
pub const MAILBOX: LockRank = LockRank::new(830);

/// `usage_lock` — the usage store's read-modify-write.
///
/// "Takes no other registry lock while held" except `AUDIT_LOCK`.
pub const USAGE: LockRank = LockRank::new(840);

/// `triage_defer_lock` — delivery triage's read-modify-write of one
/// group's `deferred.json` (#3304 S1).
///
/// Inner of every registry map, because the load-decide-store it spans is
/// pure file I/O and takes none of them: `triage_delivery` resolves the
/// policy (`groups`) and attempts the merge-queue enqueue
/// (`mq_state_lock`) ABOVE it, deliberately, so neither is ever held
/// under this one. Outer of `AUDIT` alone, which every write under it
/// reaches.
///
/// **Never held across a delivery**, the #467/#468 rule every state lock
/// here follows: `take_deferred` returns the framed notice and releases,
/// and the caller delivers it afterwards.
pub const TRIAGE_DEFER: LockRank = LockRank::new(850);

/// `AUDIT_LOCK` — the audit append. The innermost leaf.
///
/// Four of the file leaves above name it explicitly as the one lock they
/// take while held, and `marker_io`/`group_file_io` name it as takeable
/// under themselves. Nothing anywhere takes a registry lock while holding
/// it, which is what "innermost" means and what this rank enforces.
pub const AUDIT: LockRank = LockRank::new(900);

/// Every const above, with the registry field it ranks.
///
/// Not decoration: `src-tauri/tests/liveness.rs` reads this to assert that
/// the ranks are DISTINCT (the property that lets re-entrancy be decided by
/// lock identity rather than by rank equality) and that every name here is
/// still a live lock in a fresh registry — so a field renamed out from
/// under a const fails a test instead of silently ranking nothing.
pub const ALL: &[(&str, LockRank)] = &[
    ("marker_io", MARKER_IO),
    ("group_file_io", GROUP_FILE_IO),
    ("queue_persist", QUEUE_PERSIST),
    ("queues", QUEUES),
    ("recovered_queue", RECOVERED_QUEUE),
    ("recovered_markers", RECOVERED_MARKERS),
    ("by_pty", BY_PTY),
    ("agents", AGENTS),
    ("groups", GROUPS),
    ("delivered_prompts", DELIVERED_PROMPTS),
    ("delivered_notices", DELIVERED_NOTICES),
    ("agent_seq_persist", AGENT_SEQ_PERSIST),
    ("tasks_lock", TASKS),
    ("needs_you_lock", NEEDS_YOU),
    ("questions_lock", QUESTIONS),
    ("mailbox_lock", MAILBOX),
    ("usage_lock", USAGE),
    ("triage_defer_lock", TRIAGE_DEFER),
    ("audit", AUDIT),
];
