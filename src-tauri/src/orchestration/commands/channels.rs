//! Cross-workspace channels (#271): the human-only connect/disconnect side.
//! Moved from `orchestration/mod.rs` by #3498 P2; see `commands/mod.rs`.

use super::*;

// ---------- cross-workspace channels (#271): human-only connect/disconnect ----------
//
// The connect gesture (pane header context menu) and every membership
// mutation live ONLY here — Tauri commands reached exclusively by the
// trusted webview (CLAUDE.md constraint 5). There is deliberately no MCP
// tool that opens/closes/joins a channel; `channel_send`/`channel_status`
// (mcp.rs) are the only agent-facing surface, and both are read/broadcast
// against a membership graph an agent can never edit.

/// Connect two agent panes (possibly in different groups) into a channel.
/// Human-only. See `OrchRegistry::connect_agents` for the join/mint/reject
/// rules.
///
/// Off-thread (#762 — see [`run_blocking`]): an audit append PER MEMBER GROUP
/// and two pane deliveries, so its cost scales with the channel rather than
/// being constant.
///
/// **The channel family's reentrancy argument, made once here.** The membership
/// graph has had concurrent writers since #271 and does not depend on dispatch
/// for any of its invariants. Two of them, both live today: `mark_dead` calls
/// `cleanup_agent_channel` → `disconnect_agent` from the reaper and watchdog
/// threads whenever a pane dies, and the agent-facing `channel_send` MCP tool
/// mutates reply credits inside `channels` from an MCP thread. What holds the
/// graph together is that every mutation takes `channels` and `agent_channel`
/// **together** and does the whole decision under both — mint, join, reject,
/// teardown — so the one-channel-per-pane invariant and the star topology's
/// single hub are properties of a critical section, not of who called it. The
/// audits and deliveries deliberately run after both guards drop (they can
/// block on a pane), which is why this command's cost is off-thread work worth
/// moving rather than lock hold time.
///
/// **Reentrancy.** Specific to this one: `sender_agent` is validated against
/// the channel state *inside* that critical section — a join confirms the
/// existing sender, a mint designates one — so two connects racing onto one
/// channel cannot both install a hub. The loser is refused with the existing
/// sender named, exactly as a sequential second connect would be.
#[tauri::command]
pub async fn orch_channel_connect(
    app: AppHandle,
    from_group: String,
    from_agent: String,
    to_group: String,
    to_agent: String,
    sender_agent: String,
) -> Result<Value, String> {
    let reg = reg_of(&app);
    // #904: both ids parse at the boundary like every other group-taking
    // command. This one is not a traversal hole even unparsed — the raw values
    // reach exactly one equality check against the roster's own validated ids —
    // but "safe because of how the callee happens to use it" is the property
    // this whole change exists to retire, and it also turns an invalid id into
    // an honest error instead of a misleading "stale pane reference".
    let from_group = command_group(&from_group)?;
    let to_group = command_group(&to_group)?;
    run_blocking(move || {
        reg.connect_agents(&from_group, &from_agent, &to_group, &to_agent, &sender_agent)
    })
    .await
}

/// Disconnect one agent pane from its channel. Human-only; tears the
/// channel down if this drops it below 2 members, or if the disconnected
/// pane was the channel's sender (#271 W3 addendum, part B — a star
/// topology has exactly one hub).
/// Off-thread (#762): the teardown half of [`orch_channel_connect`] — the same
/// per-group audit appends and per-member deliveries.
///
/// **Reentrancy.** The family argument on [`orch_channel_connect`], and this is
/// the command that proves it rather than asserting it: `cleanup_agent_channel`
/// has always called `OrchRegistry::disconnect_agent` from whichever thread
/// noticed a pane die, so this exact function has been reentered off the
/// webview thread since #271. The `agent_channel` remove is what decides
/// whether a call is a real disconnect — a second one finds no membership and
/// errors rather than tearing a channel down twice — and the teardown decision
/// (below 2 members, or the sender left) is taken with both maps held.
#[tauri::command]
pub async fn orch_channel_disconnect(
    app: AppHandle,
    group: String,
    agent: String,
) -> Result<Value, String> {
    let reg = reg_of(&app);
    let group = command_group(&group)?;
    run_blocking(move || reg.disconnect_agent(&group, &agent)).await
}

/// Every live channel, for the frontend's cross-tab indicators.
#[tauri::command]
pub fn orch_channel_list(reg: tauri::State<Arc<OrchRegistry>>) -> Value {
    OrchRegistry::read_command("orch_channel_list", || json!([]), || reg.channel_list())
}

/// The channel one pane belongs to, or `null` — for a single pane's header
/// chip on tab switch / reconnect.
#[tauri::command]
pub fn orch_channel_for_pane(reg: tauri::State<Arc<OrchRegistry>>, group: String, agent: String) -> Value {
    let Ok(group) = command_group(&group) else { return Value::Null };
    OrchRegistry::read_command("orch_channel_for_pane", || Value::Null, || {
        reg.channel_for_pane(&group, &agent)
    })
}

/// Human-only: reassign a channel's sender without reconnecting (#271 W3
/// addendum, part B5). See `OrchRegistry::set_sender` for the validation
/// (member + token) and side effects (credits cleared, both panes notified,
/// `channel-direction` audited).
/// Off-thread (#762): an audit append per member group and a delivery of the
/// direction change — the same per-member fan-out as connect, for a smaller
/// edit.
///
/// **Reentrancy.** The family argument on [`orch_channel_connect`]. Specific to
/// this one: membership and token validation and the sender swap happen under
/// the `channels` guard, so two reassignments serialize into one final hub
/// rather than a channel that names one sender and credits another. Reply
/// credits are cleared as part of the same mutation, which is what stops a
/// receiver keeping a credit minted under the previous direction.
#[tauri::command]
pub async fn orch_channel_set_sender(
    app: AppHandle,
    channel_id: String,
    new_sender_agent: String,
) -> Result<Value, String> {
    let reg = reg_of(&app);
    run_blocking(move || reg.set_sender(&channel_id, &new_sender_agent)).await
}
