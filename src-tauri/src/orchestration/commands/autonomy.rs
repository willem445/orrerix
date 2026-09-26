//! Autonomous mode (#83): the autonomy toggles set from the human side.
//! Moved from `orchestration/mod.rs` by #3498 P2; see `commands/mod.rs`.

use super::*;

// ---------- autonomous mode (#83): toggles + budget + state read ----------
//
// FROZEN COMMAND CONTRACT (W2 builds the group-panel UI against this):
//   orch_set_autonomous(group_id, enabled: bool) -> Result<(), String>
//     Flip autonomous idle-tick mode. Enabling anchors the budget meter at the
//     group's current spend; disabling is the explicit consent needed to resume
//     after a budget suspension.
//   orch_set_auto_merge(group_id, enabled: bool) -> Result<(), String>
//     Flip the merge gate. Default OFF = human approval required (today's
//     behavior). ON lets the orchestrator merge adequately-tested PRs itself.
//   orch_set_full_autonomy(group_id, enabled: bool, goal: String) -> Result<(), String>
//     Flip full autonomy (#778): the orchestrator self-selects eligible work on its
//     idle tick instead of waiting for the opt-in label funnel. Dependent on
//     autonomous (rejects enable while off). `goal` is opaque, normalized to one
//     bounded line; empty/whitespace = no goal. Re-enabling with a DIFFERENT goal
//     re-aims the mode rather than no-opping — the goal is the consent's parameter.
//   orch_set_autonomy_budget(group_id, tokens: u64) -> Result<u64, String>
//     Per-group autonomous-era token budget; 0 = no cap. Returns the applied value.
//   orch_set_idle_tick_minutes(group_id, minutes: u32) -> Result<u32, String>
//     Per-group idle-tick quiet window; 0 → default (5), floored at 1, clamped to
//     1440. Returns the applied value. Lets the human set 1–2 min to verify fast.
//   orch_set_idle_activity_floor(group_id, bytes: u64) -> Result<u64, String>
//     Per-group per-tick byte floor separating a real turn from idle repaint noise;
//     0 → default (2048), floored at 1, clamped to 1 MiB. Returns the applied value.
//     The runtime remedy if a chatty CLI's idle repaints starve the tick.
//   orch_autonomy(group_id) -> Value
//     The whole panel state in one read:
//       { autonomous: bool, auto_merge: bool, auto_release: bool,
//         dangerous_mode: bool, full_autonomy: bool,
//         full_autonomy_goal: string | null, hold_label: string,
//         budget_tokens: u64,
//         budget_anchor_tokens: u64, spend_since_enable_tokens: u64 | null,
//         suspended: bool, idle_tick_minutes: u32, idle_activity_floor_bytes: u64,
//         quiet_secs: u64 | null, eligible_in_secs: u64 | null,
//         tick_status: "off"|"starting"|"paused"|"counting_down"|"eligible"|"waiting_for_activity"|"rate_capped" }
//     `hold_label` (#778) is the group's RESOLVED veto spelling — the panel's
//     full-autonomy help and mode chip name it, and both are instructions, so a
//     stale one tells the human to apply a label that holds nothing. Reported
//     whatever the mode's state, unlike `full_autonomy_goal`.
//     `spend_since_enable_tokens` is null when autonomous is off (no live meter).
//     `suspended` is true iff autonomous is off *because the budget enforcer
//     flipped it* (durable `autonomy_suspended` marker), vs a plain user toggle-off
//     — so the UI shows "budget spent, raise it or re-enable" without parsing the
//     audit log. Always false while autonomous is on.
//     `idle_tick_minutes`/`idle_activity_floor_bytes` are the active knobs.
//     `tick_status` is the honest idle-tick state; `eligible_in_secs` is a REAL
//     countdown ONLY for `counting_down`/`eligible`/`rate_capped` (it never shows a
//     lying 0 while the latch gates the tick — then it is null with status
//     `waiting_for_activity`). Both are null while off / no live orchestrator.

/// Enable/disable autonomous idle-tick mode for a group (durable, audited).
///
/// Off-thread (#762): a marker write carrying the budget anchor, audit appends,
/// and the force-disable cascade onto the dependent gates — several file
/// operations per gesture.
///
/// **Reentrancy.** The set's `insert` reserves the enable, and the disable
/// removes the marker before it touches memory — but neither makes the reserve
/// and the marker write ONE unit, and this command's marker carries consent.
/// [`OrchRegistry::marker_io`] now spans that whole region (rev-260 B1), the
/// budget-anchor computation included, because the anchor is the widest part of
/// the gap it has to close. Without it a disable landing mid-enable removes a
/// marker that is not written yet, clears the set, and lets the enable recreate
/// the file behind it — memory OFF, `autonomous` marker on disk, and the next
/// restart's re-seed resurrects autonomous mode from disk with no human in the
/// loop.
///
/// An earlier revision of this paragraph asserted that pairing was impossible
/// and cited the idle-tick thread as a pre-existing racer that made the
/// question moot. The racer is real (`run_idle_tick` →
/// `enforce_autonomy_budgets` → `suspend_autonomous`) but it does not make the
/// human-vs-human pairing safe, and that pairing is ordinary rather than
/// exotic: the group panel does not disable the control while the call is in
/// flight, so a double-click emits enable-then-disable. The idle-tick variant
/// stays outside the guard on purpose — a budget money-stop must never wait on
/// a human's toggle — and is the one variant restart genuinely reconciles, via
/// the `autonomy_suspended` marker it co-writes.
#[tauri::command]
pub async fn orch_set_autonomous(
    app: AppHandle,
    group_id: String,
    enabled: bool,
) -> Result<(), String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        let out = reg.set_autonomous(&group_id, enabled);
        // #1608: republish this group before the command returns, so the
        // group view's own post-action reload — which is immediate, not on
        // the next publish tick — cannot read the pre-write snapshot.
        reg.publish_group_now(&group_id);
        out
    })
    .await
}

/// Enable/disable the auto-merge gate for a group (durable, audited). Default OFF
/// = human merges; ON lets the orchestrator merge adequately-tested PRs itself.
///
/// Off-thread (#762): marker write, audit append, and a delivery into the
/// orchestrator's pane.
///
/// **Reentrancy.** [`OrchRegistry::marker_io`] spans the dependency check, the
/// reserve and the marker write (rev-260 B1) — the same unit, for the same
/// reason, as [`orch_set_autonomous`] above: without it a disable racing an
/// enable leaves memory OFF with an `auto_merge` marker on disk, and the next
/// restart re-seeds merge authority the human had explicitly withdrawn. The
/// disk-first fail-loud disable and the atomic reserve are still there; they
/// were never sufficient on their own, and an earlier revision of this
/// paragraph claimed they were.
///
/// Holding the `is_autonomous` check inside the same window also closes the
/// **human-vs-human** half of #788 as a side effect: an autonomous-off can no
/// longer land between this check and this reserve, because that path takes the
/// same lock for its own marker region.
///
/// **The residual that remains, stated rather than glossed** (#788): the
/// idle-tick half. `suspend_autonomous` force-disables this gate from the
/// budget enforcer without taking `marker_io` — deliberately, so a money-stop
/// never waits on a human's click — so an autonomous suspension can still land
/// between the check and the reserve. That window predates this PR (it has been
/// open since #83) and is bounded on both ends: `auto_merge` is an instruction
/// rendered into the orchestrator's config, not the merge enforcement itself (a
/// default-branch merge still needs a grant), and `create_group`'s re-seed
/// reconciles the pair on the next restart, clearing a stale marker and
/// auditing it.
#[tauri::command]
pub async fn orch_set_auto_merge(
    app: AppHandle,
    group_id: String,
    enabled: bool,
) -> Result<(), String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        let out = reg.set_auto_merge(&group_id, enabled);
        // #1608: republish this group before the command returns, so the
        // group view's own post-action reload — which is immediate, not on
        // the next publish tick — cannot read the pre-write snapshot.
        reg.publish_group_now(&group_id);
        out
    })
    .await
}

/// Enable/disable the auto-release gate for a group (durable, audited, independent
/// of auto-merge). Default OFF = releases/tags need a per-tag human grant; ON lets
/// the orchestrator publish releases itself while autonomous. Rejects enable unless
/// autonomous is on.
///
/// Off-thread (#762): the same marker write, audit and delivery as
/// [`orch_set_auto_merge`], from the same settings surface.
///
/// **Reentrancy.** [`OrchRegistry::set_auto_release`] mirrors
/// [`OrchRegistry::set_auto_merge`] exactly against an independent marker, so
/// the argument on [`orch_set_auto_merge`] applies verbatim: the same
/// `marker_io` unit over check-reserve-write (rev-260 B1), closing the same
/// resurrect-on-restart interleave — here for release and tag authority — and
/// the same remaining idle-tick residual under #788.
#[tauri::command]
pub async fn orch_set_auto_release(
    app: AppHandle,
    group_id: String,
    enabled: bool,
) -> Result<(), String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        let out = reg.set_auto_release(&group_id, enabled);
        // #1608: republish this group before the command returns, so the
        // group view's own post-action reload — which is immediate, not on
        // the next publish tick — cannot read the pre-write snapshot.
        reg.publish_group_now(&group_id);
        out
    })
    .await
}

/// Enable/disable full autonomy for a group (#778, durable, audited). A dependent
/// toggle of autonomous mode like the two above: enabling is rejected unless
/// autonomous is on, and autonomous-off or a budget suspension force-clears it.
/// `goal` is opaque to loomux — it is captured, normalized and echoed, never
/// parsed or scored (what work is valuable is the orchestrator's judgment, stated
/// in its contract, not policy in product code). Empty/whitespace = no goal.
///
/// **Async + `run_blocking`, unlike its `orch_set_auto_merge` siblings.** Those are
/// `SYNC_COMMANDS` rows in `tests/perf_dispatch.rs` — #743's census of *existing*
/// debt, which #762 owns draining; the roadmap is deleting rows, so a command
/// written today does the marker write, the audit append and the pane delivery off
/// the webview thread (INV-1's §2 P1 shape) rather than adding a row to a list
/// being emptied.
#[tauri::command]
pub async fn orch_set_full_autonomy(
    app: AppHandle,
    group_id: String,
    enabled: bool,
    goal: String,
) -> Result<(), String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        let out = reg.set_full_autonomy(&group_id, enabled, &goal);
        // #1608: republish this group before the command returns, so the
        // group view's own post-action reload — which is immediate, not on
        // the next publish tick — cannot read the pre-write snapshot.
        reg.publish_group_now(&group_id);
        out
    })
    .await
}

/// Enable/disable supervised dangerous mode for a group (#83, durable, audited).
/// Lets the human — present and supervising — authorize the orchestrator to merge
/// to the default branch and publish releases/tags itself, WITHOUT autonomous mode.
/// Mutually exclusive with autonomous: rejects enable while autonomous is on, and
/// enabling autonomous force-clears it.
///
/// Off-thread (#762): marker write, audit append and delivery. A deliberately
/// rare gesture, which is why it ranks below the polled set rather than being
/// harmless.
///
/// **Reentrancy.** Same `marker_io` unit as the two gates above (rev-260 B1),
/// and this is the member of the family where the conversion *created* the
/// exposure rather than widening it — worth stating plainly, because an earlier
/// revision of this paragraph claimed the opposite. It named a background
/// racer that does not exist: `force_disable_dangerous_mode` has exactly one
/// call site, `set_autonomous_as`'s enable arm, and no background path reaches
/// it (`suspend_autonomous` force-disables the two gates, never this). So until
/// this commit the webview thread genuinely was the only thing serializing two
/// dangerous-mode toggles, and the guard restores that rather than inheriting
/// it from somewhere else.
///
/// The mutual exclusion with autonomous is still a check-then-set and is still
/// #788's, in the direction the guard does not cover: an autonomous-on landing
/// between this call's check and its reserve clears nothing and leaves both
/// modes set. That grants the union of two authorities the human enabled with
/// two clicks, not a third neither asked for, and `create_group`'s re-seed
/// reconciles the pair at the next restart.
#[tauri::command]
pub async fn orch_set_dangerous_mode(
    app: AppHandle,
    group_id: String,
    enabled: bool,
) -> Result<(), String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        let out = reg.set_dangerous_mode(&group_id, enabled);
        // #1608: republish this group before the command returns, so the
        // group view's own post-action reload — which is immediate, not on
        // the next publish tick — cannot read the pre-write snapshot.
        reg.publish_group_now(&group_id);
        out
    })
    .await
}
