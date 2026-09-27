//! Agent exits: the exit diagnostic and cause, who initiated the exit, and
//! where the exit notice is routed.
//! Design note: `docs/design/orchestration.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. It calls no
//! sibling file.

use super::*;

/// Cap for the `task` field in a `list_agents` roster row (#851): a spawn
/// brief can run multiple hundred words, and a roster with a dozen dead
/// agents each carrying its full brief verbatim pushed one group's
/// `list_agents` response to ~4k tokens. The full brief stays retrievable
/// where it already lives (the audit log, the task board) — this is only
/// the roster excerpt, so the orchestrator can tell what a row is about
/// without paying for the whole text on every call.
pub(in crate::orchestration) const TASK_EXCERPT_CHARS: usize = 140;

/// First `n` **chars** (not bytes) of `s`, with `…` appended when something
/// was dropped. Counting chars rather than bytes keeps the cap UTF-8-safe
/// by construction — `s.chars().take(n)` can never land mid-codepoint, so
/// there is no boundary search to get wrong, unlike a byte-offset cut.
pub(in crate::orchestration) fn task_excerpt(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push('…');
    out
}

/// The diagnostic clause `on_pty_exit` puts in its orchestrator notice (#281):
/// a bare exit code can't distinguish "produced real output, then failed" from
/// "exited before printing a single byte" (the resume-CLI-boots-and-
/// immediately-exits signature this issue is about) — `total_bytes == 0` names
/// that case explicitly instead of leaving the orchestrator to guess. Only
/// ever called for an exit loomux did NOT itself cause (see `on_pty_exit`'s
/// `expected` guard), but still covers a clean, voluntary exit_code 0 that
/// happened to produce nothing — so the wording says "exited", never "died"
/// or "crashed", which would misname a graceful (if unhelpfully silent) stop
/// as a failure. Factored out as a pure function (no pty/app handle needed)
/// so it's directly unit-testable.
pub fn exit_diagnostic(tail: &str, total_bytes: u64) -> String {
    if total_bytes == 0 {
        "produced no output before exiting — it likely exited before the CLI printed \
         anything at all (a missing/corrupt session file, a rejected resume flag, or \
         a gone cwd are the usual causes)"
            .to_string()
    } else {
        format!("last output before it exited: {:?}", tail_snippet(tail, 400))
    }
}

/// The `cause` clause `on_pty_exit` puts in its notice — `exit_diagnostic`,
/// gated behind whether loomux itself caused this exit.
///
/// This gate exists because `PtyManager::kill` (pty.rs) removes the pty
/// handle from its live map BEFORE the waiter thread ever gets to snapshot
/// its output, so an `expected` exit (kill_agent, idle-kill, a pane close)
/// always arrives here with `tail == ""` and `total_bytes == 0` — REGARDLESS
/// of how much real work the agent actually did. Feeding that straight into
/// `exit_diagnostic` would misdiagnose a productive delegate the orchestrator
/// deliberately stopped as having silently died before printing anything,
/// pointing it at a corrupt session file or a bad resume flag that was never
/// the issue. The diagnostic is only meaningful for an exit loomux did NOT
/// cause; an expected one gets a plain, honest "loomux stopped it" instead.
/// Pure so this gate — the actual fix — is directly unit-testable without the
/// live pty/bind machinery `on_pty_exit` itself needs.
pub fn exit_cause(expected: bool, tail: &str, total_bytes: u64) -> String {
    if expected {
        "loomux stopped it (kill or idle-timeout) — not a crash".to_string()
    } else {
        exit_diagnostic(tail, total_bytes)
    }
}

/// Who initiated an agent's termination (#533-B) — recorded on the agent's
/// own record by the code that issues the kill, BEFORE the pty is touched.
///
/// This exists because the pre-#533 signal for "loomux stopped it" was
/// `on_pty_exit`'s `expected` flag, which is a property of the PTY
/// (`PtyManager::kill` inserts the id into `expected_exits`) and therefore
/// answers a different question: it says loomux closed the pane, not WHO
/// decided to. A human closing a pane, `end_group`'s teardown, and the
/// orchestrator's own `kill_agent` all arrive as `expected: true` and are
/// indistinguishable there. Routing a notice on that would demote exits
/// the orchestrator never initiated, which is precisely what #533-B says
/// must keep prompting — so the initiator is recorded as a FACT at the call
/// site instead of inferred at the exit site.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExitInitiator {
    /// The orchestrator's own `kill_agent` MCP call — it asked for this
    /// exit, so being told about it is a turn spent learning what it
    /// already knows.
    Orchestrator,
    /// The idle-kill guardrail's reaper (`reap_idle_agents`). By
    /// construction it only ever kills workers/reviewers that are IDLE —
    /// no task in flight, nothing to lose — which is what makes it safe to
    /// demote alongside an orchestrator-initiated kill.
    IdleTimeout,
    /// The review driver releasing a pane it no longer needs (#2501, #2811 S1) — a
    /// reviewer lane whose verdict is recorded at the drive's current head, a
    /// worker that reported and went idle once the drive had consumed the
    /// report, or either of them at the step that ENDS the drive.
    /// `reviewdrive::releasable` is the closed rule and
    /// `OrchRegistry::release_driven_pane` is the only thing that stamps this.
    DriverRelease,
    /// The LEAD pane of this delegate's group, by dying (#2519). A lead group
    /// has no root once its lead is gone, so its helpers are ended with it —
    /// `OrchRegistry::end_lead_children` is the only thing that stamps this.
    ///
    /// Demoted for a reason the other three do not have: there is nobody left
    /// to prompt. The pane an exit notice would be typed into is the one whose
    /// death caused the exit, so a `Prompt` here is a delivery whose only
    /// possible outcome is a dropped notice.
    LeadExit,
    /// A PLANNER closing itself after its `report(done)` (#203, demoted by
    /// #3040 N2). `OrchRegistry::close_completed_planner` is the only thing
    /// that stamps this.
    ///
    /// Its own variant rather than a reuse of `Orchestrator`, on the
    /// instruction in [`exit_notice_route`]'s doc: nothing in this process
    /// asked for it in the sense that variant means — the planner's own final
    /// report is what triggers it — so recording it as an orchestrator kill
    /// would put a false initiator on the audit row that exists to make the
    /// demotion readable.
    ///
    /// Demoted for the reason that doc applies to `IdleTimeout`, plus one it
    /// does not have: nothing is in flight (the planner's contract is one plan
    /// → one report → exit, and the report has landed), the output is durable
    /// (the plan is on GitHub and the report is the previous prompt in the
    /// recipient's own pane), and the roster carries the liveness half.
    PlannerCompleted,
}

impl ExitInitiator {
    pub fn as_str(self) -> &'static str {
        match self {
            ExitInitiator::Orchestrator => "orchestrator",
            ExitInitiator::IdleTimeout => "idle-timeout",
            ExitInitiator::DriverRelease => "driver-release",
            ExitInitiator::LeadExit => "lead-exit",
            ExitInitiator::PlannerCompleted => "planner-completed",
        }
    }
}

/// Where an agent-exit notice goes (#533-B).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitNoticeRoute {
    /// Deliver to the orchestrator's pane — costs it a turn, and is worth
    /// one: it did not cause this and cannot know about it otherwise.
    Prompt,
    /// Write the notice to the audit log and stop. The roster
    /// (`list_agents`) already reflects liveness, so the orchestrator can
    /// read this on demand rather than being interrupted by it.
    AuditOnly,
}

/// Route an agent-exit notice from the RECORDED initiator (#533-B).
///
/// Pure, and deliberately takes only the recorded initiator — not
/// `expected`, not the exit code, not the tail. Anything else would be the
/// inference this replaces.
///
/// **If the idle reaper ever grows a mode that kills agents with in-flight
/// work, that mode must PROMPT.** `IdleTimeout` is demoted here only
/// because `reap_idle_agents` is, by construction, restricted to agents
/// that are idle — no task, nothing lost. A future "kill the stalled
/// worker" reaper is a materially different event the orchestrator did not
/// ask for and cannot reconstruct from the roster: give it its own
/// `ExitInitiator` variant and route it to `Prompt`, rather than reusing
/// this one.
///
/// **`DriverRelease` is demoted, and it is worth saying why it is not the
/// "kill to reclaim a slot" case that sentence turns away** (#2501). That
/// case is a reaper choosing a victim to make room; this is a drive
/// disposing of a pane whose work it has already taken delivery of, in one
/// of the states `reviewdrive::releasable` closes over: a lane whose
/// verdict is recorded at the drive's current head, a worker whose
/// `report` the drive has consumed, or — since #2811 S1 — either of them at
/// the step that ends the drive, where there is nothing left to wait for.
/// All carry `IdleTimeout`'s own property — the pane is idle, so nothing
/// is in flight — and add the one it does not: the output is already
/// durable (a verdict file, a consumed report) and the conversation is
/// resumable by session, so nothing is lost rather than merely nothing in
/// progress.
///
/// The orchestrator can reconstruct it, which is the actual test this
/// function applies: `rd-lane-released` / `rd-worker-released` name the
/// pane, the session and the reason on the audit log, the roster reflects
/// the liveness, and the drive's own exit notice lists the panes that are
/// still there. And prompting would be self-defeating — an orchestrator
/// turn per released pane is precisely the cost #2501 measured and exists
/// to remove, so a `Prompt` here would spend the saving on announcing it.
pub fn exit_notice_route(initiator: Option<ExitInitiator>) -> ExitNoticeRoute {
    match initiator {
        Some(ExitInitiator::Orchestrator)
        | Some(ExitInitiator::IdleTimeout)
        | Some(ExitInitiator::DriverRelease)
        | Some(ExitInitiator::LeadExit)
        | Some(ExitInitiator::PlannerCompleted) => ExitNoticeRoute::AuditOnly,
        // Crash, watchdog-driven death, an agent quitting unexpectedly, a
        // human closing the pane — nobody in this process asked for it.
        None => ExitNoticeRoute::Prompt,
    }
}
