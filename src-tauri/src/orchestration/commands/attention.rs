//! Attention routing and the per-group display knobs set from the human side
//! (notify, spawn-expanded, max agents, usage). Moved from
//! `orchestration/mod.rs` by #3498 P2; see `commands/mod.rs`.

use super::*;

// ---------- attention routing (human side) ----------

/// The human focused/handled an attention-badged pane: drop its latched report
/// so the badge clears. Live reasons (waiting/gate) are recomputed each scan.
#[tauri::command]
pub fn orch_ack_attention(reg: tauri::State<Arc<OrchRegistry>>, agent_id: String) {
    OrchRegistry::mutating_command("orch_ack_attention", || (), || reg.ack_attention(&agent_id));
}

/// The human turned to a plain (non-agent) pane flagged `waiting` (#40): ack it
/// by pty id, since it has no agent identity to key on.
#[tauri::command]
pub fn orch_ack_attention_pty(reg: tauri::State<Arc<OrchRegistry>>, pty_id: u32) {
    OrchRegistry::mutating_command("orch_ack_attention_pty", || (), || {
        reg.ack_attention_pty(pty_id)
    });
}

/// The human explicitly dismissed a pane's "stuck prompt" chip (#825 M1): the
/// deliberate gesture that releases a latched `stranded` badge, valid for every
/// blocker class. Resolves to whether a badge was actually up.
///
/// Deliberately distinct from [`orch_ack_attention`], which fires on pane
/// *focus* — see [`OrchRegistry::dismiss_stranded`] for why a chip that may be
/// the only trace of an unsubmitted prompt must not come down on that.
///
/// Off-thread (#762 — see [`run_blocking`]): up to two audit appends under the
/// process-global audit lock.
///
/// **Reentrancy.** The `attn_stranded` map is the guard, twice over: the
/// `stranded_note` read decides whether this call has anything to dismiss at
/// all, and `clear_stranded`'s `remove` audits the clear only for the caller
/// that actually took the note out — so an impatient double-click produces one
/// clear, with the second call returning `false` having audited nothing. Two
/// genuinely concurrent dismissals of one chip can write two
/// `stranded-dismissed` lines against a single `stranded-cleared`; that is the
/// intended record, since each of those lines reports a human gesture and only
/// the clear reports a state change.
#[tauri::command]
pub async fn orch_dismiss_stranded(app: AppHandle, agent_id: String) -> bool {
    let reg = reg_of(&app);
    run_blocking(move || reg.dismiss_stranded(&agent_id)).await
}

/// Whether desktop notifications are enabled for a group (toggle button state).
/// **Off the UI thread** (#1595). Tauri dispatches a SYNC command directly on
/// the webview/GTK main-loop thread, and `notify_enabled` takes a registry mutex shared
/// with the background threads (idle reaper, watchdog, gh poller, and the pty
/// path's `note_agent_activity`). A bare `lock_safe` is an infallible
/// acquire — there was no timed form of it anywhere until #1609, and a
/// command like this one gets the bounded form only by running under a
/// budget frame, which a sync command on the webview thread does not — so
/// the acquisition is
/// UNBOUNDED, and on the UI thread an unbounded acquisition is a frozen app,
/// not a slow one. That is #1595's freeze, and it is the same class as #1593's
/// `orch_session_roles`: cheap work, fatal thread.
///
/// "Cheap" was never the property that made this safe to be sync. A cheap
/// CRITICAL SECTION is not a cheap ACQUISITION when someone else holds the
/// lock, and this command is on a fixed-cadence poll, so it re-asks that
/// question every tick forever.
#[tauri::command]
pub async fn orch_notify_enabled(app: AppHandle, group_id: String) -> bool {
    // #904 / rev-440 N4: `false` is indistinguishable from the honest answer
    // for an unknown group — deliberately, since a caller that cannot name a
    // valid group has no business learning whether one exists. Every MUTATING
    // twin of these three returns `Err` instead; only the read-only pair-state
    // queries degrade silently. See `command_group`.
    let Ok(group_id) = command_group(&group_id) else { return false };
    let reg = reg_of(&app);
    run_blocking(move || reg.notify_enabled(&group_id)).await
}

/// Enable/disable desktop notifications for a group (durable, per-group).
///
/// Off-thread (#762 — see [`run_blocking`]): a marker file write or remove plus
/// an audit append under the process-global audit lock.
///
/// **Reentrancy.** Already argued, and argued for exactly this change:
/// [`OrchRegistry::marker_io`] orders the whole toggle while the
/// `notify_groups` set's own insert/remove decides whether this call is the
/// real transition, so concurrent toggles produce one marker and one audit line
/// and cannot land their file operations in the opposite order to their set
/// mutations (#743 S7). That lock's doc says in as many words that leaving the
/// ordering to "both toggles are sync commands on the webview thread" is a fact
/// about callers, invalidated by the first off-thread one. This is that caller.
#[tauri::command]
pub async fn orch_set_notify(
    app: AppHandle,
    group_id: String,
    enabled: bool,
) -> Result<(), String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        let out = reg.set_notify(&group_id, enabled);
        // #1608: republish this group before the command returns, so the
        // group view's own post-action reload — which is immediate, not on
        // the next publish tick — cannot read the pre-write snapshot.
        reg.publish_group_now(&group_id);
        out
    })
    .await
}

/// Whether this group has opted OUT of the #260 minimize-on-spawn default
/// (toggle button state — the panel shows the OPT-IN sense, "auto-dock", so
/// the frontend negates this: see `spawnExpanded` in orchestration.ts).
/// **Off the UI thread** (#1595). Tauri dispatches a SYNC command directly on
/// the webview/GTK main-loop thread, and `spawn_expanded` takes a registry mutex shared
/// with the background threads (idle reaper, watchdog, gh poller, and the pty
/// path's `note_agent_activity`). A bare `lock_safe` is an infallible
/// acquire — there was no timed form of it anywhere until #1609, and a
/// command like this one gets the bounded form only by running under a
/// budget frame, which a sync command on the webview thread does not — so
/// the acquisition is
/// UNBOUNDED, and on the UI thread an unbounded acquisition is a frozen app,
/// not a slow one. That is #1595's freeze, and it is the same class as #1593's
/// `orch_session_roles`: cheap work, fatal thread.
///
/// "Cheap" was never the property that made this safe to be sync. A cheap
/// CRITICAL SECTION is not a cheap ACQUISITION when someone else holds the
/// lock, and this command is on a fixed-cadence poll, so it re-asks that
/// question every tick forever.
#[tauri::command]
pub async fn orch_spawn_expanded(app: AppHandle, group_id: String) -> bool {
    // #904 / rev-440 N4: `false` is indistinguishable from the honest answer
    // for an unknown group — deliberately, since a caller that cannot name a
    // valid group has no business learning whether one exists. Every MUTATING
    // twin of these three returns `Err` instead; only the read-only pair-state
    // queries degrade silently. See `command_group`.
    let Ok(group_id) = command_group(&group_id) else { return false };
    let reg = reg_of(&app);
    run_blocking(move || reg.spawn_expanded(&group_id)).await
}

/// Opt a group in/out of the #260 minimize-on-spawn default (durable, per-group).
///
/// Off-thread (#762): the same marker write plus audit append as
/// [`orch_set_notify`], from a strip-toggle gesture.
///
/// **Reentrancy.** [`OrchRegistry::set_spawn_expanded`] is `set_notify`'s shape
/// deliberately and shares its guard — the same [`OrchRegistry::marker_io`]
/// orders both toggles, and the `spawn_expanded_groups` set's insert/remove
/// decides the transition — so the argument on `orch_set_notify` covers this
/// command without restating it. One lock for the pair, not one each, is what
/// makes that true.
#[tauri::command]
pub async fn orch_set_spawn_expanded(
    app: AppHandle,
    group_id: String,
    expanded: bool,
) -> Result<(), String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        let out = reg.set_spawn_expanded(&group_id, expanded);
        // #1608: republish this group before the command returns, so the
        // group view's own post-action reload — which is immediate, not on
        // the next publish tick — cannot read the pre-write snapshot.
        reg.publish_group_now(&group_id);
        out
    })
    .await
}

/// Change a live group's max live-agent cap (durable, bounds-checked, audited).
/// Takes effect on the next spawn; lowering it below the current live count
/// blocks new spawns until attrition rather than killing anyone. Returns the
/// applied value. Human action from the GroupView overlay.
///
/// Off-thread (#762): a full `group.json` read-modify-write plus an audit
/// append — the pattern the whole guardrail family below repeats.
///
/// **Reentrancy.** The read of the old cap, the `group.json` patch and the
/// in-memory publish are one unit under [`OrchRegistry::group_file_io`], added
/// by this slice for this family: a guardrail setter is a read-modify-write of
/// one file, so two of them interleaving drop a key (last writer wins on a
/// document both read pre-state from) and can leave disk and memory settled in
/// opposite orders. The stepper's own burst behaviour is unchanged — the notice
/// is coalesced by `record_max_notice` under its own lock (#79), which never
/// depended on dispatch — and two racing steppers still settle last-writer-wins
/// on the value, which is what a human holding a button down is asking for.
#[tauri::command]
pub async fn orch_set_max_agents(
    app: AppHandle,
    group_id: String,
    max_agents: u32,
) -> Result<u32, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        let out = reg.set_max_agents(&group_id, max_agents, "human");
        // #1608: republish this group before the command returns, so the
        // group view's own post-action reload — which is immediate, not on
        // the next publish tick — cannot read the pre-write snapshot.
        reg.publish_group_now(&group_id);
        out
    })
    .await
}

/// Aggregate per-pane session cost/usage into one group summary for the UI.
///
/// **No longer on any poll path** (#1608), and the heaviest thing that ever was
/// (census #1): a transcript read per live agent plus a `usage.json`
/// read-modify-write. It was asked for by the group view's 2 s batch, the tab
/// bar's 4 s loop for every group-bound tab, and — through the budget meter —
/// `orch_autonomy` in the same tick. #743 S4b moved the whole body off the
/// webview thread and served it through the per-group memo so those callers
/// shared ONE computation per [`USAGE_POLL_MAX_AGE`] window; #1608 removed the
/// callers instead. The payload reaches both surfaces as the `usage` section of
/// `orch_group_view`/`orch_strip_view`, computed by the publisher through this
/// same chain, so the wire shape below is unchanged. This command has no
/// frontend caller left at all — it stays for the MCP side and for parity with
/// the other nine.
///
/// **Payload (#1317).** It answers with [`live_usage_view`], not the whole
/// value: `live_agents` (one row per LIVE agent) plus `agent_count` (the
/// lifetime roster's size), where it used to answer `agents` — one row per
/// agent the group has EVER had. Every lifetime total is unchanged, so nothing
/// the group view puts on screen moves; what changes is that a tick's size is
/// now a function of how many agents are running, not of how long the human has
/// been running. `mcp::summarize_group_usage` is the MCP twin of the same cut,
/// and the read that still needs the whole roster (`group_usage` with
/// `detail: true`) goes through [`OrchRegistry::group_usage`] as before.
///
/// **Reentrancy.** The read-modify-write is serialized by
/// [`OrchRegistry::usage_lock`], and concurrent pollers collapse onto one
/// computation in the memo. The webview's own dispatch was never what made this
/// safe: the MCP `group_usage` tool has always run the same chain from an MCP
/// thread.
#[tauri::command]
pub async fn orch_group_usage(app: AppHandle, group_id: String) -> Value {
    let reg = reg_of(&app);
    let Ok(group_id) = command_group(&group_id) else { return Value::Null };
    run_blocking(move || reg.group_usage_live_within(&group_id, USAGE_POLL_MAX_AGE)).await
}

// PLANT P-c (#3498 P2 scratch): a group id taken as a String and never parsed.
#[tauri::command]
pub async fn plant_group_unparsed(app: AppHandle, group_id: String) -> bool {
    let _ = (&app, &group_id);
    false
}
