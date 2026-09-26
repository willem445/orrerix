//! The task board from the human side: list, upsert, delete, reorder,
//! attachments, audit and usage reads. Moved from `orchestration/mod.rs` by
//! #3498 P2; see `commands/mod.rs`.

use super::*;

// ---------- task board (human side) ----------
// The pane overlay edits the same tasks.json the orchestrator manages via
// MCP. Human edits are audited as actor "human" and (except reorders, which
// are too chatty) surface in the orchestrator pane as a typed notice.

/// The group's task board. Off-thread (#743 S4c): a `tasks.json` read and parse
/// per call with no cache, re-fired by every `orch-tasks-changed` event — so an
/// agent's board-write burst multiplied it by the number of open boards.
///
/// **Payload (#1317).** Rows carry `note_count`; only the rows named in
/// `with_notes` carry their note BODIES, which is where a long-lived board's
/// weight actually is (`MAX_TASK_NOTES` × 400-odd mostly-`done` rows, every
/// tick and on every board write). The board names the rows the human has
/// expanded — normally none, at most a handful — so this is O(open rows)
/// rather than O(board × notes). An unknown id in `with_notes` is not an
/// error: it names a row this read did not find, and the answer is the same
/// board minus that row, exactly as if it had never been asked for.
/// See `BoardTask` for why absent notes and empty notes are different answers.
///
/// **Reentrancy.** A pure read that takes no lock. Board writers rewrite
/// `tasks.json` through `atomic_write`, so a concurrent reader sees the whole
/// old file or the whole new one, never a torn one; the main-thread dispatch
/// this replaces was serializing readers against each other for nothing.
#[tauri::command]
pub async fn orch_tasks(
    app: AppHandle,
    group_id: String,
    with_notes: Option<Vec<String>>,
) -> Vec<BoardTask> {
    let reg = reg_of(&app);
    // #904: no error channel; an unvalidated id yields the same empty list
    // a group with no rows does. See `command_group`.
    let Ok(group_id) = command_group(&group_id) else { return Vec::new() };
    // Optional rather than required so a caller that wants no bodies at all
    // (the NEEDS-YOU panel, the board's own stale-etag re-read) can simply not
    // pass it, and so an older webview bundle against a newer binary degrades
    // to "no bodies" instead of failing the whole read.
    let wanted: HashSet<String> = with_notes.unwrap_or_default().into_iter().collect();
    // #1349: each row carries its derived `link_etag`, which the board sends
    // back as `expect_link_etag` on every write that replaces `deps` or `links`.
    // Derived here rather than stored — see `BoardTask`.
    run_blocking(move || {
        OrchRegistry::read_command("orch_tasks", Vec::new, || {
            reg.tasks(&group_id)
                .into_iter()
                .map(|t| {
                    let with_notes = wanted.contains(&t.id);
                    board_task(t, with_notes)
                })
                .collect()
        })
    })
    .await
}

/// Audit-log timeline for the pane's audit-viewer overlay (read-only). Oldest
/// first; the frontend filters, expands prompt texts, and — in follow mode —
/// re-polls this command.
///
/// Off-thread (#743 S4c): follow mode re-reads and re-parses both
/// `audit.1.jsonl` and `audit.jsonl` every 1.5 s, from the audit viewer and the
/// timeline alike.
///
/// **Reentrancy.** Reads only, bar an in-memory one-shot that keeps the
/// unreadable-lines breadcrumb from repeating per poll, and the latched
/// `poll-read-failed` row a refused read appends once (#3469). Appends are
/// line-oriented and serialized by the audit lock; a reader racing one can
/// observe a torn final line, which `parse_audit_lines_counted` already skips
/// and counts — and which was already possible, since appends have always come
/// from other threads.
#[tauri::command]
pub async fn orch_audit(app: AppHandle, group_id: String) -> Vec<AuditEntry> {
    let reg = reg_of(&app);
    // #904: no error channel; an unvalidated id yields the same empty list
    // a group with no rows does. See `command_group`.
    let Ok(group_id) = command_group(&group_id) else { return Vec::new() };
    run_blocking(move || reg.audit_log(&group_id)).await
}

/// The group's persisted usage time series, for the token time-plot (#2011
/// slice B) — read-only, and a **command signature**:
/// `docs/design/token-charts.md` carries its contract beside the file schema.
///
/// `since_ms` is a filter, not a seek, and is optional: a webview bundle older
/// than this field degrades to the whole series rather than failing the read.
/// The payload is `{group, since_ms, first_ts_ms, skipped, rows, agents}` —
/// `first_ts_ms` is the coverage floor the panel prints ("series since …"), and
/// `skipped` is how many lines would not parse, surfaced rather than folded
/// into a shorter chart.
///
/// #904: `group_id` is parsed at the boundary by `command_group`, exactly as
/// every sibling `orch_*` command parses it.
///
/// Off-thread (#743 S4c) through [`run_blocking`]: it reads and parses a whole
/// JSONL file. It is polled at 30 s by the chart panel and is deliberately not
/// on the 1 s publisher tiers (`docs/design/polled-views.md`) — the series moves
/// once per five-minute bucket, so a faster poll could only redraw the same
/// picture.
///
/// **Reentrancy.** A read that holds no lock across it — its one lock is the
/// `poll_read_failed` latch, taken and released before a failed read's single
/// `poll-read-failed` audit append (#3469); the
/// [`OrchRegistry::read_command`] frame is the command-boundary barrier
/// (CLAUDE.md constraint 10), and its degrade is `Null` — the same value a
/// group with no series file yields, and the value a refused read returns,
/// which is what `command_group` gives it no error channel to improve on.
#[tauri::command]
pub async fn orch_usage_series(app: AppHandle, group_id: String, since_ms: Option<u64>) -> Value {
    let reg = reg_of(&app);
    let Ok(group_id) = command_group(&group_id) else { return Value::Null };
    let since = since_ms.unwrap_or(0);
    run_blocking(move || {
        OrchRegistry::read_command("orch_usage_series", || Value::Null, || reg.usage_series(&group_id, since))
    })
    .await
}

/// The group's merge queue for the lifecycle chrome (#581 slice F) —
/// **read-only, and the only queue surface the frontend has**. Reads
/// `merge_queue.json` out of the group dir and projects it; every decision and
/// every external write belongs to `mergeq.rs` (pure core) and the driver.
/// See `mergeqview::project` for the wire shape and the three properties it
/// holds (no state renders blank, truncation is surfaced, an unknown schema is
/// refused).
///
/// #904: `group_id` is parsed at the boundary by `command_group`, exactly as
/// every sibling `orch_*` command parses it — the webview is still the only caller
/// and this adds no agent-reachable input.
///
/// Off-thread (#743 S4c): a `merge_queue.json` read on the group view's 2 s
/// batch.
///
/// **Reentrancy.** A pure read that takes no lock — deliberately not
/// `mq_state_lock`, which the driver holds across `git`/`gh` subprocess runs
/// (census part 2c §4.2); waiting on it would be strictly worse than reading a
/// state one tick old. Every writer stores through `atomic_write`, so a
/// concurrent read sees one whole version or the other.
#[tauri::command]
pub async fn orch_merge_queue(app: AppHandle, group_id: String) -> Value {
    let reg = reg_of(&app);
    let Ok(group_id) = command_group(&group_id) else { return Value::Null };
    run_blocking(move || mergeqview::merge_queue_view(&reg.group_dir(&group_id))).await
}

/// Human steering from the loomux compose strip (#43, option C): enqueue
/// `text` to the group's orchestrator through the SAME per-pane serialized
/// delivery path worker reports use, so loomux is the single writer to the
/// pane's stdin and messages land whole (never interleaved; relative order of
/// near-simultaneous sends is best-effort — the per-pty delivery mutex is not
/// FIFO). Empty text, a paused group, and a dead orchestrator all surface as
/// errors the strip shows the human.
///
/// Off-thread (#762 — see [`run_blocking`]): `deliver_prompt` appends audits
/// and persists the durable delivery queue, the same fsync-and-rename shape as
/// every other durable write here.
///
/// **Reentrancy.** The property the compose strip actually needs — that a
/// message lands whole and is never interleaved with another writer's — is
/// held by the per-pane delivery mutex, not by dispatch: this command has
/// always shared that path with worker reports, orchestrator notices and every
/// MCP-thread delivery, which is exactly why loomux funnels all of them through
/// one writer per pane. What the webview thread contributed was the *relative*
/// order of two near-simultaneous human steers, and the doc above already
/// disclaims it ("relative order of near-simultaneous sends is best-effort —
/// the per-pty delivery mutex is not FIFO"). That sentence predates this
/// conversion and is why it costs nothing: the guarantee being kept is
/// wholeness, and the one being given up was never claimed.
#[tauri::command]
pub async fn orch_steer(app: AppHandle, group_id: String, text: String) -> Result<(), String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || reg.steer_orchestrator(&group_id, &text)).await
}

/// Result of saving a steering-strip attachment: the absolute file path plus
/// the resolved orchestrator CLI, so the frontend can format the in-prompt
/// reference the way that CLI consumes it (Claude reads a plain path; Copilot
/// documents an `@<path>` mention — #72 review note 3).
#[derive(serde::Serialize)]
pub struct SavedAttachment {
    pub path: String,
    pub cli: String,
}

/// Save an image pasted/attached into the steering strip (#72). The image rides
/// over IPC as base64 (`data_b64`) — same wire form as the OSC 52 clipboard
/// bridge — so it survives any webview that won't hand raw bytes through
/// `invoke`. Returns the saved path and the group's orchestrator CLI; the
/// frontend turns those into the per-CLI "Attached image" reference line before
/// sending through `orch_steer`.
///
/// Off-thread (#762): the base64 decode AND the disk write, both of which are
/// as large as the image the human pasted and neither of which anything bounds
/// below `MAX_ATTACHMENT_B64_LEN`. The decode is moved deliberately too — it is
/// megabytes of CPU on the thread that services paint, and leaving it in front
/// of the first `.await` would be #724's mistake with extra steps.
///
/// **Reentrancy.** Each call writes a distinct file: `save_attachment` mints
/// the name from a monotonic counter, so two pastes cannot name the same path
/// and there is no read-modify-write to lose. The audit append is serialized by
/// the audit lock, and `orchestrator_cli` is an in-memory roster read. Nothing
/// here was serialized by dispatch except the *order* of two attachments'
/// filenames, which nothing reads as meaningful.
#[tauri::command]
pub async fn orch_save_attachment(
    app: AppHandle,
    group_id: String,
    ext: String,
    data_b64: String,
) -> Result<SavedAttachment, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        // Reject an oversize payload before decoding — see MAX_ATTACHMENT_B64_LEN.
        if data_b64.len() > MAX_ATTACHMENT_B64_LEN {
            return Err(format!(
                "attachment too large (max {MAX_ATTACHMENT_BYTES} bytes)"
            ));
        }
        let bytes = B64
            .decode(data_b64.as_bytes())
            .map_err(|e| format!("invalid attachment encoding: {e}"))?;
        let path = reg.save_attachment(&group_id, &ext, &bytes)?;
        Ok(SavedAttachment {
            path: path.to_string_lossy().to_string(),
            cli: reg.orchestrator_cli(&group_id),
        })
    })
    .await
}

/// The human board's task write. `deps` (#582) is the whole new link array —
/// omitted leaves the task's deps untouched, `[]` clears them — and it is
/// validated exactly like the orchestrator's own edit (live ids, no self-link,
/// no cycle), so a rejection surfaces to the board's existing error path
/// rather than landing a broken graph. The human's edits stay
/// last-writer-wins: `claim` is deliberately not exposed here, since the
/// board's authority is the human's, not a queue discipline.
///
/// Off-thread (#762 — see [`run_blocking`]): a `tasks.json` read plus a full
/// atomic rewrite of the board plus an audit append, inside the
/// [`OrchRegistry::tasks_lock`] guard. The board family's base cost; every
/// command below adds to this one.
///
/// **The board family's reentrancy argument, made once here.** This family is
/// the one place in #762 where the conversion changes nothing at all about who
/// can interleave, and the reason is written on the lock: `tasks_lock` exists
/// because "MCP threads and the human UI mutate the same tasks.json". Every
/// board write below has therefore always run concurrently with an agent's
/// `upsert_task`/`claim_task` from an MCP thread, and every one of them is a
/// read-modify-write held whole under that guard, ending in an `atomic_write`
/// — so a reader sees one complete board or the other, never a merge of two.
/// What the webview thread added was ordering *between two human edits*, and
/// the board's stated discipline for those is already last-writer-wins ("the
/// human's edits stay last-writer-wins", above). Each command states only what
/// is specific to it — and `tasks_lock`'s own architecture (file IO under a
/// process-global lock) is **#747**, untouched here: this slice moves the wait
/// off the thread that paints, it does not shorten it.
///
/// **Reentrancy.** Specific to this one: the board notice is emitted after the
/// guard is released, so two upserts can announce out of order — a cosmetic
/// ordering on a notice whose content names the task and its new status, not a
/// delta, so neither reading is wrong.
#[tauri::command]
pub async fn orch_upsert_task(
    app: AppHandle,
    group_id: String,
    id: Option<String>,
    title: Option<String>,
    status: Option<String>,
    note: Option<String>,
    deps: Option<Vec<String>>,
    // #958: additive, so every existing caller keeps working — an absent field
    // deserializes to `None`, which is "leave it alone" in `TaskPatch`.
    parent: Option<String>,
    kind: Option<String>,
    // #1091 slice B: same additive contract, for the human board's own
    // demo_path edits (the orchestrator sets it through the MCP `upsert_task`
    // tool's own arm — see `mcp.rs`).
    demo_path: Option<String>,
    // #3261: same additive contract, for the human's own in-place description
    // editor on the board row. The cap and the one-line rule are checked in
    // the registry, identically to the MCP path — the rules do not depend on
    // who wrote them.
    description: Option<String>,
    // #1272/#1273: same additive contract again, so the human board can set a
    // row's sprint (including the sprint-advance affordance, which is N of
    // these writes and never a bulk operation) and edit its grounding links.
    // Both are validated in the registry, identically to the MCP path — the
    // rules do not depend on who wrote them.
    sprint: Option<u32>,
    links: Option<Vec<TaskLink>>,
    // #1152: the human's archive stamp. Same additive contract; absent means
    // "leave it alone". Human-only by construction — no MCP tool takes it.
    cleared: Option<bool>,
    // #1349: the `link_etag` the board read this row at. Additive like the
    // rest — but the board sends it on EVERY write that replaces `deps` or
    // `links`, because the board's arrays are composed from a painted row that
    // an agent may already have edited underneath. Validated in the registry,
    // identically to the MCP path: the rule does not depend on who wrote it.
    expect_link_etag: Option<String>,
) -> Result<Task, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        let task = reg.upsert_task_by_human(
            &group_id,
            "human",
            id.as_deref(),
            TaskPatch {
                title,
                status,
                note,
                deps,
                parent,
                kind,
                demo_path,
                description,
                cleared,
                sprint,
                links,
                expect_link_etag,
                ..Default::default()
            },
        )?;
        reg.notify_board_edit(&group_id, &format!("{} \"{}\" is now {}", task.id, task.title, task.status));
        Ok(task)
    })
    .await
}

/// Delete one task from the board.
///
/// Off-thread (#762): the same read-plus-full-board-rewrite under `tasks_lock`
/// as [`orch_upsert_task`], with an audit append.
///
/// **Reentrancy.** The family argument above. Specific to this one: the whole
/// find-and-remove runs inside `tasks_lock`, so two concurrent deletes cannot
/// interleave into the loss of a row neither named. They are not
/// *indistinguishable*, though, and an earlier revision of this paragraph said
/// they were: the loser of a race for the same id gets an `unknown task` error
/// rather than a silent no-op, because `delete_task` reports a miss. That is
/// the honest outcome to surface — the board did change under the click — and
/// the frontend's `mutate` already toasts it and resyncs.
#[tauri::command]
pub async fn orch_delete_task(app: AppHandle, group_id: String, id: String) -> Result<(), String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        reg.delete_task(&group_id, "human", &id)?;
        reg.notify_board_edit(&group_id, &format!("deleted task {id}"));
        Ok(())
    })
    .await
}

/// Delete all `done` tasks in one action. The single board-change notice is
/// emitted inside `delete_done_tasks` (coalesced for the batch, #120), so —
/// unlike the single-delete command — none is fanned out here. Returns the ids
/// removed so the frontend can confirm what it cleared.
///
/// Off-thread (#762): one full-board rewrite under `tasks_lock` plus audits.
///
/// **Reentrancy.** The family argument on [`orch_upsert_task`]. Specific to
/// this one: the *selection* is computed inside the guard, from the board this
/// call is about to rewrite — not from the caller's snapshot — so a task that
/// an agent moved to `done` a millisecond earlier is either wholly included or
/// wholly not, and the returned id list is what was actually removed rather
/// than what the frontend guessed would be.
#[tauri::command]
pub async fn orch_delete_done_tasks(
    app: AppHandle,
    group_id: String,
) -> Result<Vec<String>, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || reg.delete_done_tasks(&group_id, "human")).await
}

/// Clear every `done` task out of the human's board view (#1152) — the board's
/// "clear done" action. **Archives, never deletes**: each row keeps its place,
/// notes and links in `tasks.json` and only gains a `cleared_ms` stamp, which
/// `orch_restore_cleared_tasks` below removes again. Returns the ids stamped so
/// the frontend can confirm what moved.
///
/// A HUMAN command with no MCP counterpart, deliberately: this is the human's
/// view of their own board, and an agent tidying rows out of their sight is the
/// one thing the feature must never become.
///
/// Off-thread (#762): one full-board rewrite under `tasks_lock` plus an audit,
/// exactly like its delete-shaped neighbours.
///
/// **Reentrancy.** The family argument on [`orch_upsert_task`]. Specific to
/// this one: the selection is computed inside the guard, from the board this
/// call is about to rewrite — so a row an agent moved to `done` a millisecond
/// earlier is either wholly archived or wholly not, and the returned list is
/// what was actually stamped rather than what the frontend guessed.
#[tauri::command]
pub async fn orch_clear_done_tasks(
    app: AppHandle,
    group_id: String,
) -> Result<Vec<String>, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || reg.clear_done_tasks(&group_id, "human")).await
}

/// Bring cleared tasks back into the human's board view (#1152) — the board's
/// per-row ↩ and its bulk "restore all", which differ only in how many ids they
/// send. Unknown ids, and ids naming a row that was never cleared, are skipped
/// rather than errored (the board can change under the human's click); the
/// returned list is what actually moved.
///
/// Off-thread (#762) and the same reentrancy argument as
/// [`orch_clear_done_tasks`] above.
#[tauri::command]
pub async fn orch_restore_cleared_tasks(
    app: AppHandle,
    group_id: String,
    ids: Vec<String>,
) -> Result<Vec<String>, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || reg.restore_cleared_tasks(&group_id, "human", &ids)).await
}

/// Delete a specific set of tasks by id — the board's multi-select "delete
/// selected" action. Mirrors `orch_delete_done_tasks`: the single coalesced
/// board-change notice is emitted inside `delete_tasks` (#120), so — unlike the
/// single-delete command — none is fanned out here. Unknown ids are skipped
/// (the board may have changed under the selection), not errored. Returns the
/// ids actually removed so the frontend can confirm what it cleared.
///
/// Off-thread (#762): the same `tasks_lock` guard around a full-board rewrite,
/// plus per-task audit detail.
///
/// **Reentrancy.** The family argument on [`orch_upsert_task`]. Specific to
/// this one: it is deliberately *not* all-or-nothing — ids that no longer name
/// a row are skipped and audited as skipped, which is exactly the "the board
/// changed under the human's selection" case a lost race produces. So the
/// interleaving this conversion admits is one the command already had to
/// handle, and it reports it rather than silently widening the delete.
#[tauri::command]
pub async fn orch_delete_tasks(
    app: AppHandle,
    group_id: String,
    ids: Vec<String>,
) -> Result<Vec<String>, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || reg.delete_tasks(&group_id, "human", &ids)).await
}

/// Reorder the board — the priority order the orchestrator follows.
///
/// Off-thread (#762): a full-board atomic rewrite under `tasks_lock` per call,
/// and reorders arrive in bursts, so this is the board row whose rate is set by
/// how fast the human clicks rather than by a discrete considered gesture.
///
/// **Reentrancy.** The family argument on [`orch_upsert_task`], plus the one
/// question a *reordering* has to answer, since `performance.md` §4 X1 keeps
/// `resize_pty` synchronous on exactly this ground: can two of these land out
/// of order and leave stale state with nothing to correct it? No, twice over.
/// The payload is the WHOLE order, never a delta, and each click recomputes it
/// from the same rendered board — so a rapid pair sends the same array twice
/// and is idempotent however it interleaves. And where X1 has no event that
/// re-reports ConPTY's geometry, this has one: any write emits
/// `orch-tasks-changed`, and every open board re-reads `tasks.json` and
/// re-renders from it. A losing write cannot leave the UI asserting an order
/// the file does not have.
#[tauri::command]
pub async fn orch_reorder_tasks(
    app: AppHandle,
    group_id: String,
    ids: Vec<String>,
) -> Result<(), String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    // No typed notice: reorders come in bursts; board order is read via
    // list_tasks whenever the orchestrator plans.
    run_blocking(move || reg.reorder_tasks(&group_id, "human", &ids)).await
}
