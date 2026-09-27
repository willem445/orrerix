//! Spawn decisions: the delegate roster, live-cap and block refusals, spawn
//! expiry, minimised opening, what counts against max agents, the spawn rate,
//! and the Windows command-line length guard.
//! Design note: `docs/design/orchestration.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. Sibling files it
//! calls: `agentmodel.rs`.

use super::*;

/// Format the live-delegate roster line for the cap-rejection guardrail message
/// (#203) from `(id, role, idle, driven)` rows, sorted by id for a stable
/// message: `id (role, idle|working[, driven #<pr>]), …`. `idle`
/// (`idle_since_ms.is_some()`) is the
/// same signal the idle-reaper kills on, so it genuinely means "safe to
/// reclaim". Pure and free-standing so both `spawn_agent` cap checks can format
/// an identical message — the fast path via [`OrchRegistry::live_delegate_roster`],
/// the race-safe path directly against its already-held `agents` guard (no
/// re-lock). Empty string for no rows (the cap can't be hit then, but stay total).
///
/// # `driven` (#2811 S2)
///
/// The refusal's remedy is "reuse an idle agent or kill one first", and an idle
/// pane a live review drive is holding for its next round is the one row on this
/// list for which that advice is WRONG — following it strands the drive, which
/// is #3038 measured. So a driven pane says so, next to the `idle` that would
/// otherwise recommend it. `None` renders exactly the bytes this function
/// produced before, which is what keeps the existing pins on undriven rows
/// (`w-… (worker, working)`) their own negative control.
///
/// It is a plain `Option<u64>` per row rather than a lookup this function does,
/// because the race-safe caller formats under the `agents` guard and the
/// ownership read must not be called there. Both roster callers resolve it
/// beforehand through [`OrchRegistry::rd_driven_panes_now`] — the non-blocking
/// form, since the DRIVER's own spawn reaches this function from inside a tick
/// that already holds `rd_state_lock`; an unmarked row is the documented answer
/// for that caller, and [`OrchRegistry::rd_driven_panes`]'s locking note carries
/// why.
pub(in crate::orchestration) fn format_delegate_roster(mut rows: Vec<(String, &'static str, bool, Option<u64>)>) -> String {
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.into_iter()
        .map(|(id, role, idle, driven)| {
            let driven = driven.map(|pr| format!(", driven #{pr}")).unwrap_or_default();
            format!("{id} ({role}, {}{driven})", if idle { "idle" } else { "working" })
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The distinctive span of the live-delegate cap's refusal — the ONE literal
/// [`live_cap_refusal`] writes and [`is_live_cap_refusal`] reads (#1960).
///
/// Built from what the message CONTAINS rather than from what follows it: the
/// two numbers around it vary per group, so the constant sits between them.
const LIVE_CAP_MARKER: &str = "live agents already (max";

/// The live-delegate cap's refusal, written in one place.
///
/// It had two producers — `spawn_agent_bound`'s fast check and the race-safe
/// re-check inside the `agents` lock — and now has a CONSUMER: the review
/// driver classifies a refused hand-back, because a cap refusal and a session
/// it cannot resume are different holds naming different remedies, and
/// reporting the first as the second sends an orchestrator looking for a
/// replacement session for a session that is fine (#1960). A classifier reading
/// a literal one of its producers could edit away is a classifier that silently
/// stops classifying, so the literal is shared rather than duplicated a third
/// time.
pub(in crate::orchestration) fn live_cap_refusal(live: u32, max: u32, roster: &str) -> String {
    format!(
        "guardrail: {live} {LIVE_CAP_MARKER} {max}). Reuse an idle agent or kill one \
         first. Live delegates: {roster}."
    )
}

/// Whether a spawn refusal is the live-delegate cap's — see
/// [`live_cap_refusal`], whose literal this reads.
///
/// `pub` for the integration tests: #2501's cap-accounting test needs a CONTROL
/// that the spawn it expects to be refused is refused by the cap and not by the
/// spawn-rate backstop or a bad block, and a test that re-spelled the marker
/// would be a second copy of the literal this function exists to keep single.
#[doc(hidden)] // pub for integration tests
pub fn is_live_cap_refusal(err: &str) -> bool {
    err.contains(LIVE_CAP_MARKER)
}

/// The sentence `spawn_agent_bound` refuses a NAMED orchestrator block with —
/// a group has exactly one orchestrator, opened at launch — written in one
/// place for [`live_cap_refusal`]'s reason: the review driver quotes the SAME
/// sentence at the `drive_review` call (#2819 (g), S7), where it refuses a
/// worker session whose block is one of these, so one wording cannot drift
/// between the spawn it refuses and the hold that would have carried it.
pub(crate) fn orchestrator_block_refusal(id: &str) -> String {
    format!(
        "block {id:?} is an orchestrator block — a group has exactly one orchestrator, opened at launch"
    )
}

/// [`orchestrator_block_refusal`]'s manager twin (#1161 M3, S7).
pub(crate) fn manager_block_refusal(id: &str) -> String {
    format!(
        "block {id:?} is this group's manager — the human's own interface, declared in the \
         repo's workflow file and opened for them at launch, never spawned by an agent. \
         That includes resuming its session: a manager pane comes back through the \
         session browser, not through spawn_agent. To put something to the human, use \
         ask_human; to send them status, use message_manager.",
    )
}

/// The sentence `spawn_agent_bound` refuses an unknown block id with, shared
/// with the review driver's call-time check (#2819 (g), S7) for the same
/// one-wording reason.
pub(crate) fn unknown_block_refusal(id: &str, known: &[&str]) -> String {
    format!("unknown block {id:?}. Blocks in this group: {}", known.join(", "))
}

/// Whether a queued `orch-spawn-request` has expired and must be dropped
/// unserviced (#106). The backend stamps each request with the wall-clock
/// deadline of its own `bind` wait (`now + BIND_TIMEOUT`); a frontend that was
/// stalled past that point would otherwise open a zombie pane against
/// already-torn-down backend state (the config is cleaned and the pending bind
/// is gone). A `deadline_ms` of 0 means "unstamped" (legacy payloads) and never
/// expires. Pure and `pub` so both the stamping backend and the frontend can be
/// tested against one agreed rule, and so this rule lives in exactly one place.
pub fn spawn_request_expired(deadline_ms: u64, now_ms: u64) -> bool {
    deadline_ms != 0 && now_ms > deadline_ms
}

/// Whether a spawned pane should open docked/minimized instead of expanded
/// into the visible split tree (#260): true for every delegate role
/// (worker/reviewer/planner), UNLESS the group opted back into the pre-#260
/// "always expand" behavior (`group_opted_expanded`, backed by the durable
/// `spawn_expanded` marker — see `OrchRegistry::set_spawn_expanded`). The
/// orchestrator's own pane is NEVER minimized — it's the human's anchor into
/// the group, and a hidden orchestrator pane would defeat the whole point of
/// keeping it in focus — so this reads `false` for it regardless of the
/// group setting. Pure so the decision is unit-testable without a registry;
/// both `SpawnRequest` construction sites (`spawn_agent_ex` for delegates,
/// `create_orchestration_group` for the orchestrator) call this SAME
/// function, so the "orchestrator is exempt" rule can't drift between them.
///
/// **The manager (#1161) is exempt on #260's own argument, not by analogy.**
/// That argument is "the pane the human works through is never hidden", and
/// the manager is that pane more literally than the orchestrator: a docked
/// manager is a conversation the human cannot see, in a class whose entire
/// purpose is that they are in it. A minimized manager would also be the one
/// pane a human could not tell apart from an absent one.
///
/// **The lead (#2519) is exempt on that same argument** — it is a pane the
/// human types into, and a docked one is a conversation they cannot see. It is
/// spelled through [`Role::is_fixture`] rather than as a third name here, so
/// this rule and the six others that shared the old
/// `Orchestrator | Manager` spelling cannot drift apart per class.
pub fn spawn_opens_minimized(role: Role, group_opted_expanded: bool) -> bool {
    !role.is_fixture() && !group_opted_expanded
}

/// **Whether a live pane of this class consumes a `max_agents` slot** — the
/// cap's exemption rule, as one pure predicate (#1161 M3, decision D3).
///
/// `false` for every [`Role::is_fixture`] class — [`Role::Orchestrator`] (as it
/// always was), [`Role::Manager`], and [`Role::Lead`] (#2519). The cap contains
/// DELEGATE fan-out, which is the one axis a root agent controls; no exempt
/// class is a delegate anyone opened. A manager is declared in the repo's
/// `.orrerix/workflow.yml` and opened by loomux at launch for the human, so
/// making it competable with a worker slot would let a cap the orchestrator
/// itself can lower (`set_max_agents`) decide whether the human has an
/// interface. A lead is the pane the human is *sitting in*, and a cap that
/// counted it would spend one of their own helper slots on the seat.
///
/// **The lead's CHILDREN count, and that is the whole guardrail** (#2519): they
/// are ordinary [`Role::Worker`] panes, so they take this predicate's `true`
/// branch unchanged, and the launcher's "Max live agents" field is what bounds
/// a lead's fan-out. Nothing about this class widens the cap; it only keeps the
/// seat out of it.
///
/// [`Role::Solo`] counts: a solo pane is not in an orchestration group at all
/// (`__solo__`), so no group's cap is ever evaluated against one — reading
/// `true` here keeps the predicate a statement about the EXEMPT classes
/// rather than a hand-list of the counted ones that a new class would join
/// silently.
///
/// Pure, and the one expression. **Every site that decides this question calls
/// it rather than re-spelling the rule** — four functions, five decision
/// points: [`OrchRegistry::live_delegate_count`] (the value enforcement reads),
/// [`OrchRegistry::live_delegate_roster`] (the names in the refusal message),
/// `spawn_agent_bound` twice (its fast-path cap check and its race-safe
/// re-check under the agents lock), and [`OrchRegistry::group_summary`]'s
/// `live_delegates` (the number the lifecycle panel shows).
///
/// A `grep` for the call shows **seven**, not five, and the difference is not a
/// miscount: the race-safe re-check calls it three times — once to gate the
/// block, then inside each of the two filters that count the slot-holders and
/// name them — which is one decision made in three places that must agree.
///
/// Four of those sites had independently spelled `role != Role::Orchestrator`.
/// `group_summary` had spelled the rule a THIRD way again — a hand-sum of
/// per-class tallies — and was converted in the same edit even though that sum
/// already produced the right number, because a reader that re-spells the rule
/// is the drift this predicate exists to remove whether or not its spelling
/// happens to agree today (#1161 M3 review B1).
///
/// So a class cannot be exempt from the count, named in the refusal message,
/// and omitted from the panel's total independently of each other.
pub fn counts_against_max_agents(role: Role) -> bool {
    !role.is_fixture()
}

/// Whether this agent is a LIVE manager of `group` — the singleton rule, as one
/// expression (#1161 M3, review N2).
///
/// Pure over a single entry so the two places that ask are one rule rather than
/// two spellings: `spawn_agent_bound`'s check under the already-held agents
/// guard (which cannot call a lock-taking method), and
/// [`OrchRegistry::has_live_manager`], which takes the lock for callers that do
/// not hold it. Two hand-written copies of "same group, `Role::Manager`, not
/// dead" is precisely the divergence `counts_against_max_agents` exists to
/// prevent for the cap, and this rule deserves the same treatment.
pub(in crate::orchestration) fn is_live_manager_of(a: &AgentEntry, group: &GroupId) -> bool {
    a.group == group && a.role == Role::Manager && a.status != AgentStatus::Dead
}

/// Whether the spawn-rate guardrail should reject the next spawn: true when
/// at least `limit` spawns already fall inside the trailing `window_ms`.
/// Pure so the sliding-window arithmetic is testable; `limit` 0 = unlimited.
pub fn spawn_rate_exceeded(times: &[u64], now: u64, limit: u32, window_ms: u64) -> bool {
    if limit == 0 {
        return false;
    }
    let recent = times.iter().filter(|&&t| now.saturating_sub(t) < window_ms).count();
    recent as u32 >= limit
}

/// Round #417 correction 6, the belt-and-braces half of the fix: Windows
/// `CreateProcessW` has a HARD 32,767-character command-line limit — exceed
/// it and the OS itself refuses to start the process with an unreadable
/// `CreateProcessW error=87` (or similar), no matter which spawn path runs
/// it (direct `CreateProcessW`, or the `pwsh.exe -Command` shell fallback,
/// whose extra quoting/escaping layer can only make an oversized line
/// WORSE, never better). This is exactly the bug the file-based Claude
/// system-prompt fix above closes at the SOURCE (the multi-KB role
/// contract no longer rides argv at all, on either CLI) — this guard is
/// the backstop for the failure mode itself, so a FUTURE regression (some
/// other large value landing on argv again) fails loudly and diagnosably,
/// pre-spawn, instead of reproducing the exact unreadable wall the user's
/// live demo hit.
///
/// Checked against the STRUCTURED argv form (`build_agent_argv`'s output),
/// which both spawn paths are built from (`build_agent_command`'s shell
/// string is the same atoms, just shell-quoted — quoting can only ADD
/// length, never remove it, so a command line that's already too long
/// unquoted is too long quoted too; this one check covers both paths by
/// construction). `WINDOWS_COMMAND_LINE_SAFETY_LIMIT` sits well under the
/// documented 32,767 to leave headroom for the shell fallback's own
/// escaping inflation and any UTF-16 surrogate-pair expansion, without
/// hair-splitting the exact OS boundary.
const WINDOWS_COMMAND_LINE_SAFETY_LIMIT: usize = 28_000;

pub fn command_line_length_guard(argv: &[String]) -> Result<(), String> {
    let total: usize = argv.iter().map(|a| a.chars().count() + 1).sum(); // +1 per token: a rough separator estimate
    let Some((idx, oversized)) = argv.iter().enumerate().max_by_key(|(_, a)| a.chars().count()) else {
        return Ok(());
    };
    let oversized_len = oversized.chars().count();
    if oversized_len > WINDOWS_COMMAND_LINE_SAFETY_LIMIT || total > WINDOWS_COMMAND_LINE_SAFETY_LIMIT {
        let preview: String = oversized.chars().take(80).collect();
        return Err(format!(
            "agent launch command line is too long to spawn safely — Windows CreateProcessW's \
             hard limit is 32,767 characters; loomux refuses at {WINDOWS_COMMAND_LINE_SAFETY_LIMIT} \
             to leave a safety margin. argument #{idx} alone is {oversized_len} characters (starts: \
             {preview:?}...); the full command line is approximately {total} characters across {} \
             arguments. This should never happen after the #417 correction-6 file-based system-prompt \
             fix — if it does, something is putting large content directly on argv again.",
            argv.len()
        ));
    }
    Ok(())
}
