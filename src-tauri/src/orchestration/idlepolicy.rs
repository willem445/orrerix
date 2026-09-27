//! The idle reaper's, watchdog's and idle tick's pure decisions, and the
//! low-disk transition.
//! Design note: `docs/design/orchestration.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. Sibling files it
//! calls: `exits.rs`, `spawnpolicy.rs`, `tuning.rs`.

use super::*;

/// Whether an idle agent has sat long enough to auto-kill. Pure so the
/// threshold logic is testable without threads or wall-clock; the reaper
/// loop lives in `start_idle_reaper`. `idle_since_ms` is `None` for an agent
/// that currently has work (never idle-killed); a `threshold_min` of 0
/// disables the guardrail entirely.
///
/// **This is the threshold, not the policy.** Who is eligible at all — the
/// orchestrator is not, and since #891 S4 neither is a liaison block — is
/// decided by `idle_reap_candidates` before it ever asks this function, so a
/// `true` here does not mean "will be killed".
pub fn idle_should_kill(idle_since_ms: Option<u64>, now_ms: u64, threshold_min: u32) -> bool {
    match (threshold_min, idle_since_ms) {
        (0, _) | (_, None) => false,
        (m, Some(t)) => now_ms.saturating_sub(t) >= (m as u64) * 60_000,
    }
}

/// Whether a working agent has been silent (no terminal output, no report)
/// long enough to warrant one watchdog nudge to the orchestrator. Pure so the
/// stall arithmetic and the anti-nag rule are testable without threads or a
/// real pty; the scan loop lives in `start_watchdog`. `threshold_min` 0
/// disables the guardrail; `already_notified` enforces at-most-one-notice per
/// stall (the caller clears it when the agent produces output/reports again).
pub fn watchdog_should_notify(
    silent_since_ms: u64,
    now_ms: u64,
    threshold_min: u32,
    already_notified: bool,
) -> bool {
    if threshold_min == 0 || already_notified {
        return false;
    }
    now_ms.saturating_sub(silent_since_ms) >= (threshold_min as u64) * 60_000
}

/// The `why` on a `watchdog-suppressed` audit row: the agent holds a live
/// `notify_when` watch, so it is plausibly waiting on its own CI check (#852).
pub const WATCHDOG_SUPPRESS_LIVE_WATCH: &str = "live-watch";
/// The `why` on a `watchdog-suppressed` audit row: a LIVE review drive owns
/// this pane (#3040 N2).
pub const WATCHDOG_SUPPRESS_DRIVEN_LANE: &str = "driven-lane";
/// The `why` on a `watchdog-suppressed` audit row: this pane's termination was
/// already asked for by something in this process (#3040 N2).
pub const WATCHDOG_SUPPRESS_EXIT_INITIATED: &str = "exit-initiated";

/// **Is a stall that has passed the threshold still NEWS?** (#3040 N2.)
///
/// A watchdog nudge is a wake-up signal, and #3040's census found two shapes
/// where it wakes the orchestrator with something it already knows or can do
/// nothing about — 16 of the 25 stall notices in that census were the first of
/// them. `Some(why)` demotes the stall to a `watchdog-suppressed` audit row
/// carrying that word; `None` announces it exactly as before.
///
/// - **`exit-initiated`.** Something in this process already asked for this
///   pane to end and the pty has not caught up yet. Routed through
///   [`exit_notice_route`] rather than by listing initiators here, so the two
///   answers cannot drift: an exit whose notice #533-B judged not worth a turn
///   cannot have a stall notice about the same pane that is. A `None`
///   initiator — a crash, a human closing the pane, nobody in this process
///   asking — routes to `Prompt` there and announces here.
/// - **`driven-lane`.** A LIVE review drive owns the pane, so the driver is
///   already watching it on its own tick and answers a stuck lane with a
///   `lane-stalled` HOLD, which is the decision-grade signal; the watchdog
///   nudge arrives beside it saying the same thing with no remedy. `is_driven`
///   is a closure because answering it costs a file read under another lock —
///   only asked once the cheaper reason has not already decided.
///
/// Deliberately NOT a reason: a paused group, an idle pane, and a fixture role
/// are all excluded upstream in `watchdog_tick`, before the stall clock is even
/// consulted. Folding them in here would give one rule two homes.
pub fn watchdog_suppress_reason(
    killed_by: Option<ExitInitiator>,
    is_driven: impl FnOnce() -> bool,
) -> Option<&'static str> {
    if exit_notice_route(killed_by) == ExitNoticeRoute::AuditOnly {
        return Some(WATCHDOG_SUPPRESS_EXIT_INITIATED);
    }
    is_driven().then_some(WATCHDOG_SUPPRESS_DRIVEN_LANE)
}

/// Compose the watchdog stall notice delivered to the orchestrator. Split out
/// of `watchdog_tick` (the `watch_fired_notice`-style idiom from notify.rs) so
/// it's unit-testable with no registry/app needed.
///
/// #248 originally gave this a `has_live_watch` annotation ("may be
/// deliberately waiting, not stuck") for a stalled agent holding a live CI
/// watch; #852 replaced that half-measure with full suppression of the
/// notice in that case (`watchdog_tick`), so by construction a notice this
/// function composes can never describe an agent holding a live watch —
/// the parameter (and the branch) was dropped as dead code (review finding
/// on #852).
pub fn watchdog_stall_notice(name: &str, id: &str, minutes: u32) -> String {
    format!(
        "[orrerix] watchdog: agent {name} ({id}) has produced no terminal output and sent no report for {minutes}+ min — it may be stalled or waiting on input. Inspect it with get_output(\"{id}\"); if its kickoff was lost or it is stuck, re-send the task with send_prompt. You will get this notice at most once per stall."
    )
}

/// Autonomous mode (#83): whether an idle tick should fire for an orchestrator
/// that has been output-quiet since `quiet_since_ms`. Pure so the threshold /
/// latch / per-hour-cap / clock-skew rules are testable without threads or a real
/// pty; the scan loop lives in `idle_tick_tick` / `start_idle_tick`.
///
/// - `threshold_min` 0 disables the tick entirely.
/// - `already_notified` is the one-notice-per-idle-window latch (mirrors
///   `watchdog_should_notify`): once a tick fires, no re-fire until the
///   orchestrator produces output (it acted), which clears the latch and resets
///   the quiet clock. This is the primary self-regulation.
/// - `tick_times` + `per_hour_cap` are the hard runaway backstop, reusing the
///   same sliding-window rule as the spawn-rate guardrail (`spawn_rate_exceeded`);
///   `per_hour_cap` 0 = uncapped.
/// - `saturating_sub` tolerates a `now` before `quiet_since_ms` (clock skew /
///   a freshly-stamped clock) as "no elapsed silence", never a giant interval.
pub fn idle_tick_should_fire(
    quiet_since_ms: u64,
    now_ms: u64,
    threshold_min: u32,
    already_notified: bool,
    tick_times: &[u64],
    per_hour_cap: u32,
) -> bool {
    if threshold_min == 0 || already_notified {
        return false;
    }
    if now_ms.saturating_sub(quiet_since_ms) < (threshold_min as u64) * 60_000 {
        return false;
    }
    // Under the per-hour backstop → fire. Reuses the spawn-rate window rule so the
    // "N events per rolling hour" arithmetic lives in exactly one place.
    !spawn_rate_exceeded(tick_times, now_ms, per_hour_cap, SPAWN_RATE_WINDOW_MS)
}

/// Autonomous mode (#83): whether pty-output growth between two idle-tick
/// observations counts as the orchestrator *actively working* (so it resets the
/// quiet clock and the one-notice latch) rather than idle **repaint noise**.
/// Pure so the burst-floor rule is fixture-testable.
///
/// `output_total` counts every byte the pane emits, including statusline/spinner
/// repaints that keep creeping while the CLI is parked at its prompt — and there
/// is no output-frame classifier to strip them (the #112 work classifies human
/// *input*, not output). Treating *any* growth as activity (as the watchdog does)
/// means a single stray repaint byte resets the whole quiet window, so an
/// orchestrator that repaints even occasionally could never accumulate a full
/// window and would never tick. So we discriminate by size: a real turn dumps
/// `floor`+ bytes at once, an idle repaint far fewer. Growth `>= floor` is
/// activity; sub-floor growth is noise and leaves the quiet clock running.
pub fn idle_output_is_activity(prev_total: u64, cur_total: u64, floor: u64) -> bool {
    cur_total.saturating_sub(prev_total) >= floor
}

/// Pure latch transition for the low-disk backstop (#134). Given current free
/// bytes on the workspace drive, the arming threshold `low`, the higher clear
/// threshold `clear` (hysteresis), and whether a notice is already latched,
/// return `(new_latched, fire_now)`. `fire_now` is the one-per-episode edge:
/// true only on the tick that first crosses below `low`. Split out so the
/// arm/clear hysteresis is unit-testable without a real disk.
pub fn low_disk_transition(free: u64, low: u64, clear: u64, latched: bool) -> (bool, bool) {
    if !latched && free < low {
        (true, true) // crossed below → arm and fire this tick
    } else if latched && free >= clear {
        (false, false) // recovered past the hysteresis mark → reset the latch
    } else {
        (latched, false) // no edge — hold state, stay quiet
    }
}

/// The one-per-episode low-disk notice delivered to a group's orchestrator.
pub fn low_disk_notice(free_bytes: u64) -> String {
    let free_gb = free_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    format!(
        "[orrerix] disk space low: only {free_gb:.1} GB free on the workspace drive. \
         At 0 bytes, backend builds (cargo) fail machine-wide and durable writes fail — \
         a full disk previously destroyed a live task board. Reclaim space now: end merged \
         worktrees (end_group with cleanup), `cargo clean` in idle worktrees, or clear temp \
         files. You will get this notice at most once per low-disk episode."
    )
}

/// #248: the watchdog stall notice must say so when the stalled agent holds a
/// live CI watch, so it doesn't read identically to a genuinely hung agent.
#[cfg(test)]
mod watchdog_stall_notice_tests {
    use super::*;

    #[test]
    fn carries_the_core_stall_fields() {
        // #852 review finding 2: this module used to pin the #248
        // has_live_watch annotation ("may be deliberately waiting, not
        // stuck"); #852 removed that branch entirely (a notified agent can
        // no longer hold a live watch — watchdog_tick suppresses that case
        // instead), so there's nothing left to annotate. What's left to pin
        // is the name/id/minutes/get_output instructions the orchestrator
        // relies on.
        let n = watchdog_stall_notice("w-9", "w-9", 45);
        assert!(n.starts_with("[orrerix] watchdog:"), "got: {n}");
        assert!(n.contains("w-9"), "got: {n}");
        assert!(n.contains("45+ min"), "got: {n}");
        assert!(n.contains("get_output(\"w-9\")"), "got: {n}");
    }
}
