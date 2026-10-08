//! The quick task's Tauri boundary (#3679): start a run, read where it
//! stands, and act on it.
//!
//! Three commands rather than one per verb. Every `#[tauri::command]` is a row
//! in the command manifest, a grant in an ACL set and a line in the handler
//! list, and the five things a human can do to a run — open its first pane,
//! stop it, resume it, force a hand-off, add a note — are one authority
//! exercised five ways: the same caller, the same group, the same ACL tier. So
//! they are one command with a CLOSED action vocabulary
//! ([`QUICK_ACTIONS`]), refused on anything else, and the registry method per
//! verb is where each one's contract is written.
//!
//! All three are `async` through [`run_blocking`], and for `orch_fork_agent`'s
//! reason rather than by habit: a step may open a pane, and a spawn blocks
//! until the FRONTEND has opened and bound it. A synchronous command runs on
//! the webview thread — the thread that has to service that bind — so a sync
//! step would wait on itself for the whole bind timeout and then fail.
//!
//! Design note: `docs/design/quick-orchestration.md`.

use super::*;

/// Start a quick run: mint its group and record the run. Returns
/// `{group_id, state}`.
///
/// It opens no pane. The launcher binds its tab to the returned group and then
/// asks for the first step (`orch_quick_control(group_id, "step")`), because a
/// new pane is placed by the group its tab is bound to — see
/// [`OrchRegistry::quick_start`].
///
/// **Reentrancy.** Group creation and the run's record are written under the
/// registry's `creation` mutex, which is what makes "pick a free group id" and
/// "this id now holds a run" one step: two starts racing on one repository
/// cannot be handed the same group, because the second one's id selection
/// reads the first one's record.
#[tauri::command]
pub async fn orch_quick_start(app: AppHandle, req: QuickStartRequest) -> Result<Value, String> {
    let reg = reg_of(&app);
    run_blocking(move || reg.quick_start(req)).await
}

/// Where a group's quick run stands — the status chip's poll.
///
/// **Reentrancy.** A pure read: one load of the run's record under its state
/// lock, released before the registry is asked which panes are alive. It
/// writes nothing, so any number of polls may overlap each other and any
/// control verb.
#[tauri::command]
pub async fn orch_quick_status(app: AppHandle, group_id: String) -> Result<Value, String> {
    let reg = reg_of(&app);
    let group = command_group(&group_id)?;
    run_blocking(move || Ok(reg.quick_status(&group))).await
}

/// Every quick run that has not ended, newest first — the launcher's list of
/// runs to resume or stop when no pane is left to do it from. See
/// [`OrchRegistry::quick_list`].
///
/// It takes no group: the point of the list is the runs nothing on screen
/// names. Each row carries its own `group_id`, which is what the control verb
/// is then given.
///
/// **Reentrancy.** A pure read — one directory listing and one record load per
/// quick group, each under that record's state lock and released before the
/// next. It writes nothing and holds nothing across groups.
#[tauri::command]
pub async fn orch_quick_list(app: AppHandle) -> Result<Value, String> {
    let reg = reg_of(&app);
    run_blocking(move || Ok(reg.quick_list())).await
}

/// Act on a quick run: `action` is one of [`QUICK_ACTIONS`] — `step`, `stop`,
/// `resume`, `handoff`, `note` (which takes `text`). Returns the run's status
/// afterwards, or for `note` whether it was typed into a pane.
///
/// **Reentrancy.** Every verb is a read-modify-write of the run's record under
/// its state lock, and each refuses a record that is not in the state it
/// needs — a second `resume` finds the run no longer held, a `stop` after a
/// `stop` finds it ended. The verbs that hand a turn over (`step`, `resume`,
/// `handoff`) then run one drive step, and a group is stepped by one caller at
/// a time: a step that finds another already running for the group does
/// nothing, so a double-click cannot open two panes for one turn.
#[tauri::command]
pub async fn orch_quick_control(
    app: AppHandle,
    group_id: String,
    action: String,
    text: Option<String>,
) -> Result<Value, String> {
    let reg = reg_of(&app);
    let group = command_group(&group_id)?;
    run_blocking(move || reg.quick_control(&group, &action, text.as_deref())).await
}
