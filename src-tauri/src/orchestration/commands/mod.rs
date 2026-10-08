//! The orchestration module's Tauri boundary: every `#[tauri::command]` the
//! webview calls into the registry, one file per banner of what was the
//! command tail of `orchestration/mod.rs` (#3498 P2, a pure move).
//!
//! This file holds the boundary helpers every command shares — `run_blocking`,
//! `reg_of`, `COMMAND_REFUSED` and `command_group` (CLAUDE.md constraint 6:
//! parse the group id at the edge) — and re-exports each child whole, so
//! `orchestration/mod.rs`'s `pub use commands::*` keeps every command at its
//! old `orchestration::` path and `lib.rs`'s `generate_handler!` list is
//! unchanged. The `#[tauri::command]` macro's hidden `__cmd__*` re-exports
//! ride the same globs. Registry-taking SYNC commands still route through
//! `OrchRegistry::mutating_command`/`read_command` (constraint 10;
//! `tests/synccommands.rs` default-denies every file here). Layout rules:
//! docs/design/module-layout.md.

use super::*;

mod attention;
mod autonomy;
mod channels;
mod guardrails;
mod humanside;
mod launch;
mod mergegate;
mod panes;
mod quick;
mod tasks;

pub use attention::*;
pub use autonomy::*;
pub use channels::*;
pub use guardrails::*;
pub use humanside::*;
pub use launch::*;
pub use mergegate::*;
pub use panes::*;
pub use quick::*;
pub use tasks::*;

// ---------- tauri commands ----------

/// Run an orchestration command's whole body off the webview main thread
/// (#743 S4c for the polled reads, #762 for the mutation and lifecycle
/// commands — the shape #399 gave `git.rs`, #724 gave `gh.rs`, and #719 gave
/// `write_pty`).
///
/// **Every conversion here owes a reentrancy argument, in code, at the
/// command.** The synchronous dispatch this removes was an accidental mutual
/// exclusion: one thread ran every command body, so no two could interleave and
/// nothing had to say why that was safe. Moving them off it is therefore not a
/// pure latency change — it is a concurrency change — and #726's finding is
/// that the ordering a conversion gives up has to be restored deliberately
/// where anything depended on it, never assumed away. So each command below
/// carries a `**Reentrancy.**` paragraph naming the guard that actually makes
/// it safe (a lock, an atomic reserve, an idempotent write) or the interleaving
/// it accepts and why. A conversion with no such paragraph is not finished.
///
/// Tauri dispatches a *synchronous* `#[tauri::command]` by calling it directly
/// on the webview/GUI thread, and — #724's review finding — it polls an async
/// command's future on that thread too, so anything before the first `.await`
/// is still main-thread work. Each converted command below therefore resolves
/// its registry handle (an `Arc` clone: one pointer copy, no I/O, no blocking
/// lock) and hands the ENTIRE remaining body to `spawn_blocking`.
///
/// **Why these commands take `tauri::AppHandle` instead of
/// `tauri::State<'_, Arc<OrchRegistry>>`.** A borrowed argument gives an async
/// command a lifetime parameter, which Tauri only supports for a `Result`
/// return — and these payloads are frozen contracts the frontend is built
/// against (CLAUDE.md constraint 5), so widening a `Value` to `Result<Value,
/// _>` is not available. Both `AppHandle` and `State` are injected by Tauri and
/// neither appears in the argument object the frontend sends, so the wire
/// contract is byte-identical either way.
///
/// **A panicking body stays a panic.** It is re-raised here rather than
/// degraded to an invented empty payload: moving work off the main thread must
/// not change what a bug does, and no command here can honestly synthesise the
/// answer it failed to compute.
///
/// **The residual that leaves, stated** (#1702). An `async` command body run
/// through here is one of the three no-frame caller classes a re-entrant
/// `lock_safe` now panics on, so this is a path that refusal can reach — and
/// unlike a cadenced tick there is no supervisor, because there is nothing to
/// supervise: the command is one shot. The re-raise takes the panic to Tauri's
/// own boundary, so the webview's `invoke` promise for that one call never
/// settles and its panel stays on whatever it last had. That is a degraded
/// SURFACE rather than a wedge — the registry lock is released by the unwind,
/// every other command keeps answering, and the crash log names both sites —
/// and it is left as it is deliberately: inventing a value here is the "a guard
/// that does not hold is a lie every caller would act on" trade in a different
/// costume.
///
/// **Why that reasoning does NOT extend to a SYNCHRONOUS command** (#1713
/// review B1). It rests on the unwind costing one caller, which is true here
/// because this body runs on the blocking pool. A non-`async` command runs
/// inline in the WebView2 COM callback's frame, where an unwind reaches a plain
/// `extern "system"` thunk with no `catch_unwind` anywhere in between and
/// ABORTS the process. Those commands are given a frame instead — see
/// [`OrchRegistry::mutating_command`] and `docs/design/lock-order.md` §2.1 for
/// the measured call chain — and `src-tauri/tests/synccommands.rs` keeps that
/// true for the next one added.
pub(super) async fn run_blocking<T, F>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    match crate::blocking::spawn_counted(f).await {
        Ok(v) => v,
        Err(e) => panic!("orchestration command task failed: {e}"),
    }
}

/// The registry handle for a converted command — see [`run_blocking`]. Cheap
/// and non-blocking (a managed-state lookup plus an `Arc` clone), which is what
/// makes it safe to run before the first `.await`.
pub(super) fn reg_of(app: &AppHandle) -> Arc<OrchRegistry> {
    app.state::<Arc<OrchRegistry>>().inner().clone()
}

/// What a synchronous command's caller is told when its body was refused
/// rather than run (#1702) — see [`OrchRegistry::mutating_command`].
///
/// **One paragraph, and it promises only what this path delivers.** It does not
/// say "nothing was applied": the refusal fires at an acquisition, and the body
/// may have completed earlier work before reaching it, so the honest word is
/// "may". It does not point at a crash log either — this unwind goes through
/// `budget::unwind_to_frame`, which is a `resume_unwind` and runs no panic
/// hook, so no crash log exists. What does exist is the breadcrumb this helper
/// writes and the `lock-reentrant` finding the watchdog composes beside it.
pub const COMMAND_REFUSED: &str = "loomux refused that to avoid deadlocking itself — an internal lock \
                              was already held by the same operation. It may have partly applied, \
                              so check before retrying; logs/breadcrumbs.log under your orrerix \
                              data directory names the fault.";

/// **The trust boundary** (#904): where a group id stops being a string the
/// caller chose and becomes a [`GroupId`] the backend has checked.
///
/// Every `#[tauri::command]` taking a `group_id` starts here. Until #904 that
/// string went straight to `root.join(...)` on the strength of CLAUDE.md hard
/// constraint 6 — which was never a claim about the id, but about the caller:
/// only our own in-process webview can invoke a Tauri command. #888 replaces
/// that caller with a network peer, at which point "it connected" is all the
/// identity there is. This function is the difference between the two worlds,
/// and it is deliberately the *first* thing each command does, before any
/// registry state is touched.
///
/// The error carries the `GroupIdError` but not the offending string: the
/// message goes back over IPC to a caller that just supplied it, so echoing it
/// tells an attacker nothing it doesn't know while giving a confused-deputy
/// bug somewhere to hide.
///
/// # Commands with no error channel
///
/// Most commands return `Result` and simply propagate. Eleven do not — they
/// return `Value`, `Vec<_>` or `bool` — and each has to answer a refused id
/// with a *value*. **The rule is: return the shape that command's own caller
/// already handles as "absent", checked against the frontend rather than
/// assumed.** Not one uniform sentinel.
///
/// The first version of this change used `json!({})` everywhere and justified it
/// with "`{}.x` is `undefined` where `null.x` throws". rev-440 took that apart:
/// the reads are *nested*, so the one-level argument never held, and
/// `groupview.ts` calls `render()` **outside** `load()`'s `try/catch` — so the
/// throw was uncaught and left the panel half-drawn, which is precisely what the
/// comment claimed to prevent. Verified per site, the shapes are:
///
/// - `orch_group_watches` -> `json!([])`. Success is an array; the panel calls
///   `.filter` on it, and `{}` has no `.filter`.
/// - `orch_group_summary`, `orch_group_usage`, `orch_autonomy`,
///   `orch_lock_state`, `orch_workflow_status`, `orch_merge_queue`,
///   `orch_channel_for_pane` -> `Value::Null`. Every consumer already guards
///   with `?.`/`??`/`if (!x)`, and `null` takes those guards where `{}` walks
///   straight past them. `orch_channel_for_pane`'s own absent sentinel is
///   already `Value::Null`, and `{}` is truthy, so `{}` answered "yes, there is
///   a channel".
///
/// What `null` buys for `orch_autonomy` is narrower than an earlier version of
/// this comment claimed, and rev-450 (N10) traced it: with `{}` returned across
/// the board `render()` threw at `s.roles.orchestrator` first, so
/// `renderAutonomy` was never reached — the crash came before the wrong toggle.
/// And `null` does not distinguish unknown from off either, since `autoBtn` is
/// constructed reading "off". What it does do is stop a later failed poll from
/// clobbering a previously-correct render, which is a real improvement and the
/// honest size of it.
/// - `orch_tasks`, `orch_audit` -> `Vec::new()`; the three `bool` commands ->
///   `false` (see the note at those sites).
///
/// None of this is reachable from today's webview, which only ever sends ids the
/// backend minted. It is reachable from the caller this whole change exists for
/// — #888's remote client — which is why it is fixed now rather than filed.
fn command_group(raw: &str) -> Result<GroupId, String> {
    GroupId::parse(raw).map_err(|e| format!("invalid group id: {e}"))
}
