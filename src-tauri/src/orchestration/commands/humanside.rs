//! The human's answering surfaces: human questions (#946), the manager
//! mailbox (#1161 M2) and needs-you items (#1151). Each supplies its closed
//! `*Source` enum at this entry point rather than taking one from the caller.
//! Moved from `orchestration/mod.rs` by #3498 P2; see `commands/mod.rs`.

use super::*;

// ---------- human questions (human side) ----------
// The two TRUSTED surfaces onto the question registry (#946). Trusted in the
// same sense as the merge-grant commands: the caller is loomux's own webview
// and the gesture is a human's. No agent-reachable path exists to either — see
// `humanq`'s module doc and `OrchRegistry::answer_question`.

/// The group's questions, for the inbox panel — **the whole file, uncapped**.
///
/// Deliberately NOT `question_list`, which is the MCP tool's projection: that
/// one caps settled rows for an agent's context budget and returns the omitted
/// count alongside so the agent can say so. Through this command there would be
/// nowhere to put that count — the return type is a list — and a cap whose size
/// the caller cannot see is exactly the silent truncation the rest of this
/// feature refuses. The file needs no cap of its own here anyway: retention
/// already bounds it (`SETTLED_RETAINED` settled rows, `PENDING_MAX` pending),
/// so "everything" is a bounded answer by construction.
///
/// Off-thread (#743 S4c), like every other fs-touching command. Read-only and
/// takes no lock — writers replace `questions.json` through `atomic_write`, so
/// a concurrent reader sees the whole old file or the whole new one.
///
/// **A read failure reads as empty here, and only here.** The registry method
/// is deliberately loud about a malformed file (see its doc: a read-modify-write
/// that treats unparseable as empty destroys pending questions), but this
/// command has no error channel and its caller renders a list — so the panel
/// shows nothing rather than throwing, exactly as `orch_tasks` does for an
/// unparseable board. Nothing WRITES through this path, so the loud read that
/// protects the file is untouched.
#[tauri::command]
pub async fn orch_questions_list(app: AppHandle, group_id: String) -> Vec<humanq::Question> {
    let reg = reg_of(&app);
    // #904: no error channel; an unvalidated id yields the same empty list a
    // group with no questions does. See `command_group`.
    let Ok(group_id) = command_group(&group_id) else { return Vec::new() };
    run_blocking(move || {
        OrchRegistry::read_command("orch_questions_list", Vec::new, || {
            reg.questions(&group_id).unwrap_or_default()
        })
    })
    .await
}

/// The human answers a pending question, from the app's own webview.
///
/// **There is deliberately no `source` parameter.** The source is a property of
/// this entry point — `AnswerSource::Webview`, hard-coded below — not something
/// a caller states about itself. That is what makes "who answered" a fact
/// loomux establishes rather than one it is told, and it is why no agent can
/// impersonate a human even if some future path reached this command.
#[tauri::command]
pub async fn orch_question_answer(
    app: AppHandle,
    group_id: String,
    id: String,
    answer: String,
) -> Result<(), String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        reg.answer_question(&group_id, &id, &answer, humanq::AnswerSource::Webview).map(|_| ())
    })
    .await
}

/// The human dismisses a pending question, from the app's own webview (#2137).
///
/// **There is deliberately no `source` parameter**, on `orch_question_answer`'s
/// reasoning: the source is a property of this entry point —
/// `DismissSource::Webview`, hard-coded below — not something a caller states
/// about itself, and there is no MCP tool that reaches the dismiss entry point
/// at all.
///
/// `reason` is optional, and an absent or blank one is the ordinary case — a
/// dismissal is complete without an explanation. Either way the question is
/// settled durably before anything is delivered, and the orchestrator always
/// gets a notice, because a pending question is holding work.
#[tauri::command]
pub async fn orch_question_dismiss(
    app: AppHandle,
    group_id: String,
    id: String,
    reason: Option<String>,
) -> Result<(), String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        reg.dismiss_question(&group_id, &id, reason.as_deref(), humanq::DismissSource::Webview)
            .map(|_| ())
    })
    .await
}

// ---------- the manager mailbox (human side, #1161 M2) ----------
// One read, for the pane's unread chip (M5). The WRITE side is an agent
// surface only (`message_manager`), so there is no trusted-webview twin of it
// here: a human does not post into the mailbox, they talk to the manager.

/// How many mailbox messages this group's manager has not read (#1161 M2) —
/// what the pane's unread chip renders (M5).
///
/// `0` for every group that declares no manager, which is nearly all of them:
/// no manager means no mailbox file, and an absent file reads as empty.
///
/// **A read failure reads as 0**, on `orch_questions_list`'s reasoning applied
/// to chrome: this command has no error channel and its caller renders a badge,
/// so an unreadable file hides the chip rather than throwing. The registry's
/// own loud read (`mailbox`) is untouched, and every path that WRITES the file
/// still goes through it — see `OrchRegistry::mailbox_unread`.
///
/// Off-thread (#743 S4c), like every other fs-touching command. Read-only and
/// takes no lock: writers replace `mailbox.json` through `atomic_write`, so a
/// concurrent reader sees the whole old file or the whole new one.
#[tauri::command]
pub async fn orch_mailbox_status(app: AppHandle, group_id: String) -> usize {
    let reg = reg_of(&app);
    // #904: no error channel, so an unvalidated id yields the same 0 a group
    // with no mail does. See `command_group`.
    let Ok(group_id) = command_group(&group_id) else { return 0 };
    run_blocking(move || {
        OrchRegistry::read_command("orch_mailbox_status", || 0, || reg.mailbox_unread(&group_id))
    })
    .await
}

// ---------- needs-you items (human side, #1151) ----------

/// Everything the NEEDS-YOU panel renders from the item registry: the rows and
/// the clear-completed watermark, in ONE round trip.
///
/// One call rather than two, deliberately: the panel hides settled rows stamped
/// at or before the watermark, so fetching the two separately would let it
/// render this second's rows against last second's watermark and flash back a
/// row the human had just cleared.
///
/// Off-thread (#743 S4c) like every other fs-touching command, and — like
/// `orch_questions_list` — **a pure read that writes nothing and takes no
/// lock**. That is a deliberate property rather than an accident of the
/// implementation: this command is classified `viewer` in the remote-engine
/// roster (`remote-engine-protocol.md` §5.4), and that tier is defined as
/// "cannot write a file". An earlier revision ran the upgrade migration inline
/// here, which made a viewer-tier poll drive a file write, an event emission and
/// audit growth — and re-raised rows the human had just resolved. The migration
/// now runs once ever at group load; see `OrchRegistry::migrate_demo_items`.
/// **Do not put work back on this path.**
///
/// **A read failure reads as empty here, and only here.** The registry method is
/// deliberately loud about a malformed file — a read-modify-write that treats
/// unparseable as empty destroys open items — but this command has no error
/// channel and its caller renders a list, so the panel shows nothing rather than
/// throwing, exactly as `orch_questions_list` and `orch_tasks` do. Nothing
/// WRITES an item through this path, so the loud read that protects the file is
/// untouched.
/// **It also carries the board rows its open items name (#1317)**, which is
/// what lets the panel stop fetching the WHOLE board every tick to answer a
/// handful of point lookups. That adds one `tasks.json` read to this path and
/// no lock, no write and no event — the `viewer`-tier property above is about
/// what a command WRITES, and this still writes nothing. See
/// [`OrchRegistry::needs_you_read`].
#[tauri::command]
pub async fn orch_needs_you_list(app: AppHandle, group_id: String) -> NeedsYouRead {
    let reg = reg_of(&app);
    // #904: no error channel; an unvalidated id yields the same empty view a
    // group with no items does. See `command_group`.
    let Ok(group_id) = command_group(&group_id) else { return NeedsYouRead::default() };
    run_blocking(move || {
        OrchRegistry::read_command("orch_needs_you_list", NeedsYouRead::default, || {
            reg.needs_you_read(&group_id).unwrap_or_default()
        })
    })
    .await
}

/// The human closes out a needs-you item, from the app's own webview.
///
/// **There is deliberately no `source` parameter.** The source is a property of
/// this entry point — `ResolveSource::Webview`, hard-coded below — not something
/// a caller states about itself, which is what makes "who resolved this" a fact
/// loomux establishes rather than one it is told. `orch_question_answer`'s shape,
/// for the same reason and with the same consequence: there is no MCP tool that
/// reaches the resolve entry point at all.
///
/// `note` is optional. With one, the orchestrator gets a sanitized best-effort
/// pane notice; without one, event and audit only, because a delivery per tidy
/// is noise. Either way the item is settled durably before anything is
/// delivered, and a delivery failure never fails the resolve.
#[tauri::command]
pub async fn orch_needs_you_resolve(
    app: AppHandle,
    group_id: String,
    id: String,
    note: Option<String>,
) -> Result<(), String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        reg.resolve_needs_you(&group_id, &id, note.as_deref(), needsyou::ResolveSource::Webview)
            .map(|_| ())
    })
    .await
}

/// The human dismisses a needs-you item, from the app's own webview (#2137).
///
/// `orch_needs_you_resolve`'s shape with one difference the caller can see:
/// this settles the row as `dismissed:webview` rather than `webview`, and it
/// ALWAYS delivers a pane notice — a resolve without a note deliberately
/// delivers none. See `OrchRegistry::dismiss_needs_you` for why that is a
/// separate command rather than a boolean on the one beside it.
#[tauri::command]
pub async fn orch_needs_you_dismiss(
    app: AppHandle,
    group_id: String,
    id: String,
    reason: Option<String>,
) -> Result<(), String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        reg.dismiss_needs_you(
            &group_id,
            &id,
            reason.as_deref(),
            needsyou::ResolveSource::WebviewDismiss,
        )
        .map(|_| ())
    })
    .await
}

/// "Clear completed": stamp this group's watermark and return it, so the panel
/// can apply the new one without a second read.
///
/// **Deletes nothing and mutates no row** — see `clear_needs_you`. An OPEN item
/// is untouchable through this command by construction, which is what makes the
/// header button safe to click without a confirm.
#[tauri::command]
pub async fn orch_needs_you_clear(app: AppHandle, group_id: String) -> Result<u64, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || reg.clear_needs_you(&group_id)).await
}
