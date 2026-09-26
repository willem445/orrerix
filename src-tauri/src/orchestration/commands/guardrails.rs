//! The persisted guardrail knobs, the group/strip/lock views, and the
//! workflow switch/apply/status commands. Moved from `orchestration/mod.rs`
//! by #3498 P2; see `commands/mod.rs`.

use super::*;

// ---------- the persisted guardrail knobs ----------
//
// Seven commands, one shape: read the current value, patch that one key in
// `group.json`, publish the new value into the in-memory guardrails the running
// loops read, audit. All off-thread as of #762.
//
// **Their reentrancy argument is one argument, made once here and cited by
// each.** A read-modify-write of a shared document is the textbook lost update:
// two of these running at once on the same group both read the pre-state, and
// the second write drops the first one's key — on the file that carries the
// group's consent-bearing guardrails. The webview thread used to make that
// impossible for free. [`OrchRegistry::group_file_io`] now makes it impossible
// on purpose, holding the read, the patch and the in-memory publish as one unit
// (so disk and memory cannot settle in opposite orders either), and its own doc
// carries the full argument including the lock order. Each command below states
// only what is specific to it.

/// Set a group's autonomous-era token budget (0 = no cap; durable, audited).
/// Returns the applied value.
///
/// Off-thread (#762): a `group.json` read-modify-write plus an audit append.
///
/// **Reentrancy.** The family argument above, under
/// [`OrchRegistry::group_file_io`]. Specific to this knob: it deliberately does
/// NOT move the enable-time anchor, so a budget raise racing the idle-tick
/// thread's `enforce_autonomy_budgets` cannot change what that pass has already
/// counted — the worst interleaving is a suspension that the new budget would
/// have avoided, which the human resolves by re-enabling, and which was already
/// possible before this conversion.
#[tauri::command]
pub async fn orch_set_autonomy_budget(
    app: AppHandle,
    group_id: String,
    tokens: u64,
) -> Result<u64, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        let out = reg.set_autonomy_budget(&group_id, tokens);
        // #1608: republish this group before the command returns, so the
        // group view's own post-action reload — which is immediate, not on
        // the next publish tick — cannot read the pre-write snapshot.
        reg.publish_group_now(&group_id);
        out
    })
    .await
}

/// Set a group's idle-tick quiet window in minutes (0 → default; floored at 1,
/// clamped to the max; durable, audited). Returns the applied value. Lets the
/// human drop it to 1–2 min to verify autonomous mode fires quickly.
///
/// Off-thread (#762): a `group.json` read-modify-write plus an audit append.
///
/// **Reentrancy.** The family argument above, under
/// [`OrchRegistry::group_file_io`]. Specific to this knob: the idle-tick loop
/// reads the in-memory value fresh each pass and never caches it, so a tick
/// that overlaps this write uses either the old cadence or the new one — both
/// are cadences the human chose, and neither is a torn value, because the
/// publish happens under the same guard as the write it followed.
#[tauri::command]
pub async fn orch_set_idle_tick_minutes(
    app: AppHandle,
    group_id: String,
    minutes: u32,
) -> Result<u32, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        let out = reg.set_idle_tick_minutes(&group_id, minutes);
        // #1608: republish this group before the command returns, so the
        // group view's own post-action reload — which is immediate, not on
        // the next publish tick — cannot read the pre-write snapshot.
        reg.publish_group_now(&group_id);
        out
    })
    .await
}

/// Set a group's idle-tick activity floor in bytes (0 → default; floored at 1,
/// clamped to 1 MiB; durable, audited). Returns the applied value. The runtime
/// remedy if a chatty CLI's idle repaints exceed the default and starve the tick.
///
/// Off-thread (#762): a `group.json` read-modify-write plus an audit append.
///
/// **Reentrancy.** The family argument above, under
/// [`OrchRegistry::group_file_io`]. Specific to this knob: like the tick
/// cadence beside it, the floor is re-read by the idle-tick and compact-nudge
/// passes every time rather than latched, so an overlapping pass sees one whole
/// value or the other and no pass can straddle the change.
#[tauri::command]
pub async fn orch_set_idle_activity_floor(
    app: AppHandle,
    group_id: String,
    bytes: u64,
) -> Result<u64, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        let out = reg.set_idle_activity_floor(&group_id, bytes);
        // #1608: republish this group before the command returns, so the
        // group view's own post-action reload — which is immediate, not on
        // the next publish tick — cannot read the pre-write snapshot.
        reg.publish_group_now(&group_id);
        out
    })
    .await
}

/// Set a group's compact-nudge quiet window in minutes (#287; 0 = off,
/// clamped to the max; durable, audited). Returns the applied value.
///
/// Off-thread (#762): a `group.json` read-modify-write plus an audit append.
///
/// **Reentrancy.** The family argument above, under
/// [`OrchRegistry::group_file_io`]. Specific to this knob: `0` is the feature's
/// off switch rather than a "use the default" sentinel, so the write that turns
/// the nudge off is the same single-key patch as any other value — there is no
/// second store to keep in step with it, and nothing to reconcile if a
/// compact-nudge pass overlaps.
#[tauri::command]
pub async fn orch_set_compact_nudge_minutes(
    app: AppHandle,
    group_id: String,
    minutes: u32,
) -> Result<u32, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        let applied = reg.set_compact_nudge_minutes(&group_id, minutes)?;
        reg.publish_group_now(&group_id);
        Ok(applied)
    })
    .await
}

/// The cache-age chip's "Compact now" (#3407) — see
/// [`OrchRegistry::human_request_compact`]. Off-thread like its siblings: it
/// takes the `agents` lock and appends an audit line.
#[tauri::command]
pub async fn orch_request_compact(
    app: AppHandle,
    group_id: String,
    agent_id: String,
) -> Result<String, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || reg.human_request_compact(&group_id, &agent_id)).await
}

/// Set a group's compact-nudge eligible roles (#287; unrecognized names
/// dropped, empty falls back to `["orchestrator"]`; durable, audited).
/// Returns the applied set.
///
/// Off-thread (#762): a `group.json` read-modify-write plus an audit append.
///
/// **Reentrancy.** The family argument above, under
/// [`OrchRegistry::group_file_io`]. Specific to this knob: the value is a whole
/// canonicalized *set*, replaced outright rather than added to, so two writers
/// cannot merge into a roster neither of them chose — the loser's set is
/// overwritten entire, which is the same last-writer-wins the control's own
/// semantics already give (a checkbox group submits its whole state).
#[tauri::command]
pub async fn orch_set_compact_nudge_roles(
    app: AppHandle,
    group_id: String,
    roles: Vec<String>,
) -> Result<Vec<String>, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || reg.set_compact_nudge_roles(&group_id, roles)).await
}

/// Set a group's compact-nudge context-usage escalation threshold (#328; `0`
/// = off, clamped to 100; durable, audited). Returns the applied value.
///
/// Off-thread (#762): a `group.json` read-modify-write plus an audit append.
///
/// **Reentrancy.** The family argument above, under
/// [`OrchRegistry::group_file_io`]. Specific to this knob: it is read only by
/// the compact-nudge escalation decision, which takes no lock and holds no
/// snapshot across passes, so an overlapping pass reads one whole percentage or
/// the other.
#[tauri::command]
pub async fn orch_set_compact_context_threshold(
    app: AppHandle,
    group_id: String,
    percent: u32,
) -> Result<u32, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        let applied = reg.set_compact_context_threshold(&group_id, percent)?;
        reg.publish_group_now(&group_id);
        Ok(applied)
    })
    .await
}

/// Set a group's compact-nudge min-context floor (benchtest finding; `0` =
/// off — the heuristic lull-timer fires on the lull alone, today's behavior;
/// clamped to 100; durable, audited). Never gates an agent's own
/// `request_compact`. Returns the applied value.
///
/// Off-thread (#762): a `group.json` read-modify-write plus an audit append.
///
/// **Reentrancy.** The family argument above, under
/// [`OrchRegistry::group_file_io`]. Specific to this knob: it is the one
/// tri-state in the family, and this setter only ever writes `Some` — it can
/// never restore the unset/smart-default arm — so no interleaving of two writes
/// can produce a value the tri-state does not already admit, and the guard is
/// what stops one of them being dropped on the way to disk.
#[tauri::command]
pub async fn orch_set_compact_nudge_min_context_percent(
    app: AppHandle,
    group_id: String,
    percent: u32,
) -> Result<u32, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        let applied = reg.set_compact_nudge_min_context_percent(&group_id, percent)?;
        reg.publish_group_now(&group_id);
        Ok(applied)
    })
    .await
}

/// The group's autonomous-mode state for the panel: toggles, budget, anchor, and
/// spend-since-enable. Single read the UI renders all three controls from.
///
/// Off-thread (#743 S4c), and — while autonomous is ON — sharing the group
/// view's usage computation rather than re-running the whole chain a second
/// time in the same 2 s tick (#743 S4b, census part 2a row 27).
///
/// **Reentrancy.** The markers, guardrails, and agent state it reads are
/// read-only here. The token total is not: it runs the same usage computation
/// `orch_group_usage` does, whose `usage.json` read-modify-write is serialized
/// by [`OrchRegistry::usage_lock`] — and inside one window the two commands
/// share a single computation rather than racing to do it twice.
#[tauri::command]
pub async fn orch_autonomy(app: AppHandle, group_id: String) -> Value {
    let reg = reg_of(&app);
    let Ok(group_id) = command_group(&group_id) else { return Value::Null };
    run_blocking(move || reg.autonomy_state_within(&group_id, USAGE_POLL_MAX_AGE)).await
}

/// **The group view's whole 2 s poll, in one read** (#1608, plan #1600 §3
/// Phase 1). Replaces a `Promise.all` batch of ten `orch_*` commands, every
/// one of which acquired a registry mutex on an unbounded `lock_safe`.
///
/// Being ASYNC was never enough, and this command is why the distinction is
/// worth stating: #1595 moved five polled commands off the webview thread and
/// #1600 §1.2 is the release where the same unbounded wait exhausted the
/// shared 512-thread blocking pool instead — after which `write_pty` could not
/// be scheduled and no pane accepted input. The body here takes **no registry
/// lock at all**: it stamps a view lease and reads a published cell, so it
/// enters the pool and cannot park in it. `perf_dispatch.rs`'s poll-path guard
/// (test L6) asserts exactly that, by requiring `views.load(` in this body.
///
/// The lease is what keeps the expensive half honest: the publisher computes
/// the eight view-tier sections only while a caller keeps asking, so a
/// `merge_queue.json` read, a `workflow.yml` parse and a `git` default-branch
/// resolution stay at one open view's rate rather than every group's.
///
/// #904: a refused id answers `Value::Null` — and so does a group created
/// since the last publish pass, deliberately, because the caller's response to
/// both is the same: keep the previous render and ask again. See
/// `command_group` and `docs/design/polled-views.md`.
///
/// **Reentrancy.** A read of an immutable snapshot; the only mutation is the
/// lease stamp, which is last-writer-wins on a monotonic instant and cannot
/// disagree with itself.
#[tauri::command]
pub async fn orch_group_view(app: AppHandle, group_id: String) -> Value {
    let Ok(group_id) = command_group(&group_id) else { return Value::Null };
    let reg = reg_of(&app);
    run_blocking(move || {
        reg.views.note_view_lease(&group_id);
        views::group_view_payload(&reg.views.load(), &group_id, Instant::now())
    })
    .await
}

/// **The whole tab strip's 4 s poll, in one read** (#1608). Replaces
/// `orch_group_summary` + `orch_group_usage` issued once per group-bound tab,
/// so the per-tick IPC fan-out collapses from 2xN to 1 and stops growing with
/// the human's tab count.
///
/// Served from the same published snapshot as [`orch_group_view`], with the
/// same no-registry-lock property and the same L6 enforcement. Its `meta`
/// reports the OLDEST group's age, so `stale` means "nothing on this strip is
/// older than this" — the strip's job is to be right about the tab that is in
/// trouble, and a payload-wide average would report it fresh because one tab
/// moved.
///
/// **Takes the caller's BOUND group ids, and that is not a per-tab read.** One
/// snapshot still serves every group and one IPC still serves the whole strip;
/// what `bound` does is tell the publisher which groups to cover.
///
/// It has to, and #1625 review round 2 is why. The publisher's strip tier was
/// `reg.groups` — the groups this session created or resumed. A tab can be bound
/// to a RESTORED orchestration, which lives on disk and never enters that map
/// (`list_recorded` reads the root directory directly), so those tabs got no
/// entry at all and lost the accrued-cost badge #194 P4 LOW-8 put on them
/// deliberately. The per-tab reads this replaced had no such gap: they answered
/// for any id the caller named.
///
/// So the strip stamps a lease per bound id, exactly as the group view stamps
/// one, and the publisher covers `reg.groups` plus every fresh strip lease. A
/// leased id the registry does not know is computed through the SAME functions
/// as any other — `group_summary` answers zero live agents, `group_usage_live_within`
/// reads that group's `usage.json` — so a restored group's entry is
/// wire-identical to what the commands it replaced returned for it, by
/// construction rather than by a second disk path.
///
/// Ids that fail `command_group` are skipped rather than refused: the whole
/// call must not fail because one tab carries a stale id.
#[tauri::command]
pub async fn orch_strip_view(app: AppHandle, bound: Vec<String>) -> Value {
    let reg = reg_of(&app);
    run_blocking(move || {
        let now = Instant::now();
        for raw in &bound {
            if let Ok(g) = command_group(raw) {
                reg.views.note_strip_lease_at(&g, now);
            }
        }
        views::strip_view_payload(&reg.views.load(), now)
    })
    .await
}

/// Live-agent count, role breakdown, and uptime for the lifecycle panel.
///
/// **Off the UI thread** (#1595) — the command whose sync dispatch froze
/// v1.2.0-beta5. `group_summary` takes the `agents` mutex (and then, in a
/// separate statement, `groups`), both shared with the background threads:
/// the idle reaper, the watchdog, the gh poller, and `note_agent_activity` on
/// the pty output path. A bare `lock_safe` is an infallible acquire, so the
/// acquisition was UNBOUNDED. (#1609 added a bounded form — `lock_within`, and
/// `budget::read_budget` for a whole read path — but a caller only gets it by
/// running under a budget frame, which this command, being sync on the
/// webview thread, did not.) On the GTK main
/// loop an unbounded acquisition is a frozen window that never repaints and
/// never processes input, which is a force-quit rather than a slow panel.
///
/// **It WAS polled from TWO loops, not one**, which is why the freeze arrived a
/// minute or two in rather than at startup: `GroupView.load()`'s 2 s batch
/// (`groupview.ts`), and `TabBar.pollStatus()`'s 4 s loop (`tabbar.ts`),
/// which iterated EVERY group-bound tab and awaited each in turn. So the number
/// of unbounded main-thread acquisitions per tick scaled with open group tabs.
///
/// **Since #1608 it is polled from neither.** Both loops make one
/// `orch_group_view`/`orch_strip_view` call served from a published snapshot,
/// and this payload reaches them as that read's `summary` section — computed by
/// the publisher through this same function, so the shape is unchanged. The only
/// frontend caller left is `tasksview.ts`, once per open. The paragraphs above
/// are kept because they are the canonical statement of the #1595 class, not
/// because they still describe this command's callers.
///
/// **The "cheap in-memory" classification was not wrong about the work — it was
/// wrong about what makes a sync command safe.** A cheap CRITICAL SECTION is
/// not a cheap ACQUISITION while another thread holds the lock, and being on a
/// fixed cadence means the question is re-asked every tick forever. #1593's
/// `orch_session_roles` was the same class with expensive work; this one shows
/// the work never had to be expensive.
#[tauri::command]
pub async fn orch_group_summary(app: AppHandle, group_id: String) -> Value {
    let Ok(group_id) = command_group(&group_id) else { return Value::Null };
    let reg = reg_of(&app);
    run_blocking(move || reg.group_summary(&group_id)).await
}

/// Live watches for a group's agents — the group view's "⏳ waiting on …"
/// per-agent indicator (#248), fed from the same registry state the
/// `notify_when`/`list_notifications` MCP tools use.
/// **Off the UI thread** (#1595), for the reason spelled out on
/// [`orch_group_summary`] above: it reads the same registry state under the
/// same unbounded `lock_safe` acquisition, on the same 2 s poll batch. Since
/// #1608 neither is polled: both are sections of `orch_group_view`, computed by
/// the publisher through these same functions.
#[tauri::command]
pub async fn orch_group_watches(app: AppHandle, group_id: String) -> Value {
    // #904 / rev-440 B4: an ARRAY, because that is what success returns here and
    // the panel calls `.filter` on it (`groupview.ts` `this.watches.filter`).
    // The rule for these no-error-channel degrades is written out once, above
    // `command_group` — the short version is: return the shape the caller's own
    // absent-case handling already copes with, verified per site, never a
    // uniform `{}`.
    let Ok(group_id) = command_group(&group_id) else { return json!([]) };
    let reg = reg_of(&app);
    run_blocking(move || reg.group_watches(&group_id)).await
}

/// Live lock-resource state for a group's chrome (#858) — who holds what, and
/// how deep each queue is. The SAME shape the `list_locks` MCP tool returns,
/// deliberately: what the human sees beside the panes and what the agents read
/// are one payload, so they can never disagree.
///
/// #904: `group_id` is parsed at the boundary by `command_group` like every
/// other group-taking command, so this no longer rests on the webview being the
/// only caller. It adds no agent-reachable input either way.
///
/// **Off-thread** (performance.md §2 P1): this reconciles against the repo's
/// declared `resources:`, which reads and parses `.loomux/workflow.yml`. That
/// was on the group view's 2 s batch until #1608; the publisher's view tier now
/// makes the read, once per second and only for a leased group, and the panel
/// gets it as `orch_group_view`'s `locks` section. It is the same read
/// `orch_workflow_status` makes on that same pass, and the same reason
/// `orch_merge_queue` is async.
///
/// This used to add "unlike its neighbours `orch_group_summary`/
/// `orch_group_watches`: those are pure in-memory registry reads". #1595
/// deleted the contrast by converting both of them — being in-memory turned
/// out not to make a POLLED sync command safe, because what it bounds is the
/// critical section and not the acquisition. The file IO named above is still
/// why THIS one could never have been sync; it is no longer what tells it
/// apart from its neighbours.
#[tauri::command]
pub async fn orch_lock_state(app: AppHandle, group_id: String) -> Value {
    let reg = reg_of(&app);
    let Ok(group_id) = command_group(&group_id) else { return Value::Null };
    run_blocking(move || reg.lock_state(&group_id)).await
}

/// LIVE advanced-orchestrator toggle (#316), reached from the groupview button
/// (Slice C) — human action, not agent-triggered. Arms/clears the merge gate and
/// swaps the roster for FUTURE spawns; see `OrchRegistry::set_advanced_orchestrator`
/// for the full contract (refusals, satisfiability, the consent boundary on live
/// delegates). Returns the resulting workflow status — the same shape
/// `orch_workflow_status` reads.
///
/// Off-thread (#762): it reads the repo's workflow file, patches `group.json`,
/// syncs the merge-gate spec file and returns `workflow_status`.
///
/// **Reentrancy.** Two guards, for two different racers. The `group.json`
/// read-modify-write and the in-memory publish are one unit under
/// [`OrchRegistry::group_file_io`], like the guardrail knobs it sits beside —
/// that is what this conversion adds. The other racer needs nothing added,
/// because it was already there: `run_workflow_gate_reload`'s background pass
/// re-arms the merge gate from the repo's current file, and #385 already made
/// it re-read the group's guardrails immediately before writing precisely
/// because "`set_advanced_orchestrator` runs on a different thread" — a
/// sentence written about the webview thread that stays true, word for word,
/// about a pool thread. The guard is released before the gate sync and the
/// notice, so nothing waits on it across a pane write.
#[tauri::command]
pub async fn orch_set_advanced_orchestrator(
    app: AppHandle,
    group_id: String,
    on: bool,
) -> Result<Value, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || {
        let out = reg.set_advanced_orchestrator(&group_id, on, "human");
        // #1608: republish this group before the command returns, so the
        // group view's own post-action reload — which is immediate, not on
        // the next publish tick — cannot read the pre-write snapshot.
        reg.publish_group_now(&group_id);
        out
    })
    .await
}

/// Parse a raw workflow name at the COMMAND BOUNDARY (#1689), the way
/// `command_group` parses a raw group id — so a `WorkflowName` is the only
/// thing that travels inward and the compiler, not a scan, is what keeps a
/// hand-typed `../../etc/x` out of a path join (CLAUDE.md constraint 6).
///
/// The raw string is **not** echoed back in the refusal: this reaches a toast,
/// which is a display surface, and `orch_workflow_preview` already declines to
/// echo one into `path` for the same reason.
fn command_workflow_name(raw: &str) -> Result<workflow::WorkflowName, String> {
    workflow::WorkflowName::parse(raw).map_err(|e| format!("that is not a usable workflow name: {e}"))
}

/// What applying a named workflow to this group would change (#1689 slice B) —
/// the payload the group header's **Review & apply** modal is built from, and
/// the same resolution [`orch_apply_workflow`] runs, so the confirmation and
/// the action cannot describe different things.
///
/// Read-only. `Err` is a refusal no confirmation can clear (workflow mode off,
/// the name is unusable, the file is absent or will not parse); an `Ok` whose
/// `refusal` is set is a refusal the human can act on by editing the file, and
/// carries the diff beside it so the modal can explain rather than just stop.
///
/// Off-thread (#762): it reads and parses a workflow file and the group's
/// armed gate spec.
///
/// **Reentrancy.** Reads only — in-memory guardrails, the workflow file and the
/// `merge_gate` spec file; it mutates nothing, so nothing here can be observed
/// half-done.
#[tauri::command]
pub async fn orch_workflow_switch_preview(
    app: AppHandle,
    group_id: String,
    name: String,
) -> Result<Value, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    let name = command_workflow_name(&name)?;
    run_blocking(move || reg.workflow_switch_preview(&group_id, &name)).await
}

/// Apply a named workflow to a LIVE group (#1689 slice B) — human action from
/// the group header, never agent-triggered, and gated behind the confirmation
/// [`orch_workflow_switch_preview`] fills in.
///
/// Swaps the roster for FUTURE spawns, rewrites `group.json`, reconciles the
/// group dir's instruction files and re-arms the merge gate; a pane already
/// running keeps the block it was spawned under. See
/// `OrchRegistry::apply_workflow` for the full contract. Returns the resulting
/// workflow status — the same shape `orch_workflow_status` reads.
///
/// `expect_digest` is the `digest` the preview returned. Pass it: it is what
/// binds this apply to the bytes the human actually read, and without it the
/// apply installs whatever the file says at click time — which may not be the
/// diff that was confirmed.
///
/// Off-thread (#762): it reads the repo's workflow file, patches `group.json`,
/// writes the group dir's instruction files, syncs the merge-gate spec file and
/// returns `workflow_status`.
///
/// **Reentrancy.** The `group.json` read-modify-write, the in-memory publish and
/// the gate sync are one unit under `OrchRegistry::group_file_io`, exactly as
/// `orch_set_advanced_orchestrator`'s are and for the same reason — and the
/// other racer, `run_workflow_gate_reload`'s background pass, already re-reads
/// the group's guardrails immediately before writing (#385).
#[tauri::command]
pub async fn orch_apply_workflow(
    app: AppHandle,
    group_id: String,
    name: String,
    expect_digest: Option<String>,
) -> Result<Value, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    let name = command_workflow_name(&name)?;
    run_blocking(move || {
        let out = reg.apply_workflow(&group_id, &name, expect_digest.as_deref(), "human");
        // #1608: republish this group before the command returns, so the group
        // view's own post-action reload — which is immediate, not on the next
        // publish tick — cannot read the pre-write snapshot.
        reg.publish_group_now(&group_id);
        out
    })
    .await
}

/// The group's current workflow-mode status for the lifecycle UI (Slice C):
/// whether advanced-orchestrator is on, the resolved roster, and the armed merge
/// gate (with a freshly-recomputed `satisfiable`/`missing_blocks`, #316's
/// satisfiability guarantee) — `{ advanced, name, default_branch: string|null,
/// blocks: [{id,kind,cli,model,persona}],
/// gate: {require,reviewers,also,satisfiable,missing_blocks} | null }`.
/// **No longer on the group view's poll path** (#1608). It was a member of
/// `GroupView.load()`'s ten-invoke `Promise.all`; that batch is now a single
/// `orch_group_view`, served from the published snapshot, and this command's
/// payload reaches the panel as that read's `workflow` section — computed by
/// the publisher thread through the same `workflow_status` call below, so the
/// wire shape is unchanged. What remains here are the non-poll callers
/// (`tasksview.ts` reads it once on open). See `docs/design/polled-views.md`.
///
/// The off-thread argument below still stands and is the reason the PUBLISHER
/// pays it on a 1 s cadence rather than a poll paying it per tick.
///
/// Off-thread (#743 S4c), and its `default_branch` is memoised per repo (#743
/// S4a) — resolving that name costs 2-4 blocking `git` spawns, which this was
/// paying every 2 s per open group view.
///
/// **Reentrancy.** Reads only — the workflow file, in-memory guardrails, and
/// the branch name; the sole mutation is filling the in-memory branch memo,
/// which is last-writer-wins on a value every racer resolves identically.
/// Settle an extension-UI dialog on a structured pane (#2850 S3b).
///
/// **The one trusted entry point, and the gate is that it IS one.** Section
/// 3.5: every agent may be ASKED, no agent may ever answer. That is enforced
/// by there being no MCP tool for it — a `#[tauri::command]` is reachable only
/// from the app own webview, so the answerer identity is a property of the
/// door rather than an argument anyone could set. `DecisionSource::Human` is
/// therefore recorded because of WHERE this ran, not because a caller said so.
///
/// An agent that could settle its own dialog would have a gate that is
/// theatre, which is the `questions.json` boundary and its reason.
///
/// `value` carries a `select`/`input`/`editor` answer, `confirmed` a
/// `confirm` one, and neither means a cancel. Exactly one of the two may be
/// set; both together is a refusal rather than a precedence rule, because a
/// caller that supplied both did not know which dialog it was answering.
///
/// `async`, so constraint 10 does not apply the way it does to a sync command:
/// the work runs on the blocking pool through `run_blocking`, not inline on
/// the webview thread.
#[tauri::command]
pub async fn orch_answer_pane_ui(
    app: AppHandle,
    group_id: String,
    agent_id: String,
    request: String,
    value: Option<String>,
    confirmed: Option<bool>,
) -> Result<(), String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    // The three shapes pi accepts, decided here rather than in the engine:
    // which one a dialog wants is a property of its METHOD, and the caller
    // that rendered the control is the one that knows.
    let answer = match (value, confirmed) {
        (Some(v), None) => loomux_engine::harness::UiAnswer::Value(v),
        (None, Some(c)) => loomux_engine::harness::UiAnswer::Confirmed(c),
        (None, None) => loomux_engine::harness::UiAnswer::Cancelled,
        (Some(_), Some(_)) => {
            return Err(
                "a dialog answer is a value OR a confirmation, never both".to_string()
            )
        }
    };
    run_blocking(move || {
        let row = reg.answer_pane_ui(&group_id, &agent_id, &request, answer)?;
        // The human answered, so their own queue row is discharged — a row
        // that outlived the dialog it describes is a queue that only grows.
        //
        // Done HERE rather than in `structured.rs` because `ResolveSource` is
        // pinned by a source scan to `needsyou.rs` and this file, and a third
        // file naming it is a new resolving surface rather than a list to
        // extend. This is the same trusted-command layer
        // `orch_needs_you_resolve` settles from.
        //
        // `Webview` is the party and the surface: the human, in loomux's own
        // webview, through a command no agent can reach. WHICH gesture it was
        // goes in the resolution note rather than into a new provenance tag —
        // see the PR for why a fifth `resolved_by` spelling was considered and
        // left to a reviewer's call rather than taken unilaterally on a trust
        // boundary.
        //
        // Best-effort: a row already cleared by hand, or pruned, must not turn
        // a dialog that really was answered into an error the human sees.
        if let Some(row) = row.as_deref() {
            if let Err(e) = reg.resolve_needs_you(
                &group_id,
                row,
                Some("answered in the pane dialog"),
                needsyou::ResolveSource::Webview,
            ) {
                crate::obs::breadcrumb(
                    "structured-dialog-row-unresolved",
                    &format!("row={row} err={e}"),
                );
            }
        }
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn orch_workflow_status(app: AppHandle, group_id: String) -> Value {
    let reg = reg_of(&app);
    let Ok(group_id) = command_group(&group_id) else { return Value::Null };
    run_blocking(move || reg.workflow_status(&group_id)).await
}
