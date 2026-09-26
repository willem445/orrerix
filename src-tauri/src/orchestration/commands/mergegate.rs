//! The merge gate from the human side: link resolution for a gate item's
//! refs, then the approve/grant/request-changes/start/proceed actions.
//! Moved from `orchestration/mod.rs` by #3498 P2 (two banners, in their
//! original order); see `commands/mod.rs`.

use super::*;

// ---------- merge-gate link resolution ----------
// The board stores issue/PR references as the orchestrator typed them
// (`#12`, a bare number, or a full URL). To make the chips clickable we
// resolve those to a web URL against the repo's `origin` remote.

/// Normalize a git remote URL (`git@`, `ssh://`, `https://`, with or without
/// a trailing `.git`) into its browsable web base, e.g.
/// `https://github.com/owner/repo`. None for anything that doesn't look like
/// a host/path we can turn into a link.
#[doc(hidden)] // pub for integration tests
pub fn normalize_remote_web_base(url: &str) -> Option<String> {
    let u = url.trim();
    if u.is_empty() {
        return None;
    }
    // Split into host and path, covering the three shapes git emits.
    let (host, path) = if let Some(rest) = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
        .or_else(|| u.strip_prefix("ssh://"))
    {
        // scheme://[user@]host[:port]/owner/repo
        let rest = rest.split_once('@').map(|(_, r)| r).unwrap_or(rest);
        let (host, path) = rest.split_once('/')?;
        // Drop any :port from the host part (ssh URLs may carry one).
        let host = host.split(':').next().unwrap_or(host);
        (host.to_string(), path.to_string())
    } else if let Some(rest) = u.strip_prefix("git@") {
        // scp-like: git@host:owner/repo.git
        let (host, path) = rest.split_once(':')?;
        (host.to_string(), path.to_string())
    } else {
        return None;
    };
    let host = host.trim().trim_end_matches('/');
    let path = path.trim().trim_start_matches('/').trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if host.is_empty() || path.is_empty() || !host.contains('.') {
        return None;
    }
    Some(format!("https://{host}/{path}"))
}

/// Web base for a repo's `origin` remote (falling back to any remote), or
/// None when the repo has no usable remote.
fn git_remote_web_base(repo: &str) -> Option<String> {
    if !Path::new(repo).is_dir() {
        return None;
    }
    let run = |args: &[&str]| -> Option<String> {
        let mut cmd = std::process::Command::new("git");
        cmd.current_dir(repo).args(args).env("GIT_TERMINAL_PROMPT", "0");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let out = cmd.output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let url = run(&["remote", "get-url", "origin"]).or_else(|| {
        // No `origin` — take the first remote git lists, if any.
        let name = run(&["remote"])?.lines().next()?.trim().to_string();
        (!name.is_empty()).then_some(name).and_then(|n| run(&["remote", "get-url", &n]))
    })?;
    normalize_remote_web_base(&url)
}

/// Resolve a stored issue/PR reference to a URL. `value` may already be a
/// full URL (used verbatim); otherwise it's a `#N`/`N` reference resolved
/// against `base`. `kind` is `"issue"` or `"pr"`. None when there's nothing
/// clickable (no number, or a bare number with no known remote).
#[doc(hidden)] // pub for integration tests
pub fn resolve_ref_url(base: Option<&str>, kind: &str, value: &str) -> Option<String> {
    let v = value.trim();
    if v.starts_with("https://") || v.starts_with("http://") {
        return Some(v.to_string());
    }
    // Pull the first run of digits out of `#12`, `12`, `GH-12`, etc.
    let num: String = v
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    if num.is_empty() {
        return None;
    }
    // GitHub redirects /issues/N <-> /pull/N, so a kind mismatch still lands.
    let seg = if kind == "issue" { "issues" } else { "pull" };
    Some(format!("{}/{seg}/{num}", base?.trim_end_matches('/')))
}

/// Open an http(s) URL in the user's default browser. The URL is passed to
/// the OS handler as a single process argument (never a shell line), and is
/// validated first so a crafted board reference can't smuggle anything.
fn open_external_url(url: &str) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("refusing to open a non-http(s) URL".into());
    }
    if url.len() > 2048 || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("unsafe URL".into());
    }
    #[cfg(target_os = "windows")]
    let mut cmd = {
        // rundll32 takes the URL as one argument, sidestepping cmd.exe's
        // `start` metacharacter handling.
        let mut c = std::process::Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.spawn().map(|_| ()).map_err(|e| format!("could not open browser: {e}"))
}

// ---------- merge-gate actions (human side) ----------
// The human's gatekeeping touchpoints on `pr` / `human-testing` items. Each
// records on the board (audited, actor "human") and delivers a purpose-built
// typed notice into the orchestrator's CLI so it can act on the decision.

/// Open a task's issue or PR reference in the default browser. `kind` is
/// `"issue"` or `"pr"`; `value` is the stored reference (`#12`, `12`, or a
/// full URL).
///
/// Off-thread (#762 — see [`run_blocking`]): up to two BLOCKING `git` spawns to
/// resolve the remote URL, then an audit append. This is an INV-2 violation
/// while it is synchronous — a process spawn on the thread that services paint
/// — and the reason it was `debt` rather than `cheap` even though its body
/// looks like three statements: the spawns are in `git_remote_web_base`, past
/// where E1's scan can see. The browser open itself is detached, so the spawns
/// were the whole cost.
///
/// **Reentrancy.** Nothing here mutates orchestration state: it reads the
/// group's repo (memory, falling back to `group.json`), shells out read-only,
/// appends one audit line through the audit lock, and hands a URL to the OS.
/// Two of these racing open two browser tabs, which is what two clicks mean.
#[tauri::command]
pub async fn orch_open_ref(
    app: AppHandle,
    group_id: String,
    kind: String,
    value: String,
) -> Result<(), String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || orch_open_ref_sync(&reg, &group_id, &kind, &value)).await
}

/// The body of [`orch_open_ref`], as a plain function so the command stays a
/// thin delegation (`performance.md` §2 P1) and the resolution logic stays
/// callable without a Tauri runtime.
fn orch_open_ref_sync(
    reg: &OrchRegistry,
    group_id: &GroupId,
    kind: &str,
    value: &str,
) -> Result<(), String> {
    let repo = reg
        .group(group_id)
        .map(|g| g.repo)
        .or_else(|| reg.load_group_file(group_id).map(|(repo, _)| repo))
        .ok_or("unknown group")?;
    let base = git_remote_web_base(&repo);
    let url = resolve_ref_url(base.as_deref(), kind, value)
        .ok_or("no URL for this reference — the repo may have no GitHub remote")?;
    reg.audit(group_id, "human", "open-ref", json!({ "kind": kind, "url": url }));
    open_external_url(&url)
}

/// Approve a merge-gate item: mark it done and notify the orchestrator to
/// merge. The human's direct sign-off, so the status change is applied here.
/// Off-thread (#762): an [`orch_upsert_task`]-sized board write under
/// `tasks_lock`, plus minting the grant file, plus the delivery.
///
/// **Reentrancy.** The board write itself is the family argument on
/// [`orch_upsert_task`]. Specific to this one, and worth stating rather than
/// waving at: the merge-gate check is a check-then-act (`ensure_at_merge_gate`,
/// then the upsert), so a second Approve that used to arrive *after* the first
/// — and be refused, the item no longer being at the gate — can now overlap it
/// and pass. What that produces is bounded by the grant's own shape: the grant
/// is keyed `merge_grants/pr-<N>` and written with `atomic_write`, so two mints
/// for one PR leave ONE single-use file, not two authorizations. The duplicate
/// is a duplicate *notice* to the orchestrator, never a second merge — and
/// `pr_number` resolves the key from the task's own ref, so the two racers
/// cannot even grant different PRs.
///
/// **The board write is not idempotent, though** (rev-260): the approval note
/// is pushed onto the task's note list, not set, and that list is capped at
/// `MAX_TASK_NOTES`. So a duplicate approve leaves two identical notes and
/// pushes the oldest ones one step closer to `cap_task_notes`' collapse — which
/// preserves them as a counted placeholder rather than dropping them silently,
/// so nothing is lost outright, but the task's readable history is shortened by
/// a click that changed nothing. The authority is bounded; the note trail is
/// not. The same unguarded gate check is shared with [`orch_request_changes`]
/// and [`orch_proceed_task`], so the widening is the trio's, not this command's
/// alone.
#[tauri::command]
pub async fn orch_approve_task(
    app: AppHandle,
    group_id: String,
    id: String,
    // Optional approve-with-comment note (#83): delivered to the orchestrator with
    // the one-time merge grant, e.g. "approved — also bump the changelog first".
    comment: Option<String>,
) -> Result<Task, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || reg.approve_task(&group_id, &id, comment.as_deref())).await
}

/// Approve a whole board selection at the merge gate (#507): one per-PR
/// one-time grant per item — the same authority a single Approve issues,
/// issued N times — and ONE consolidated notice to the orchestrator instead
/// of N prompts. All-or-nothing: if any item is not at the merge gate the
/// call fails having minted nothing.
/// Off-thread (#762): one board write plus a grant file minted PER ITEM, under
/// the same guard — so the file count scales with the selection.
///
/// **Reentrancy.** As [`orch_approve_task`]. The all-or-nothing pre-flight is
/// narrower than it sounds and is worth stating exactly, since this is the
/// command that mints merge grants in bulk (rev-260 B4). It is all-or-nothing
/// **against the batch's own items** — every id must exist, be at the gate, and
/// name a PR no sibling names, before anything is written — and it reads the
/// board through `tasks()`, which takes NO lock. The write loop that follows
/// takes `tasks_lock` once per item and does not re-check the gate. So a racer
/// that flips an item out of the merge gate after the snapshot is invisible to
/// this call: the batch approves that item and mints its grant, and returns
/// `Ok`. An earlier revision of this paragraph claimed the opposite — that such
/// a racer makes the call fail having minted nothing — which describes neither
/// the code nor any guard it has.
///
/// That interleaving predates the conversion (the MCP board tools have always
/// run on their own threads), and #762 does not widen it: the pre-flight and
/// the writes were never one critical section on any thread. It is recorded
/// here rather than fixed because making it true means re-checking each item's
/// gate state under the write lock, which changes #507's designed batch
/// semantics — see the PR discussion.
#[tauri::command]
pub async fn orch_approve_tasks(
    app: AppHandle,
    group_id: String,
    // Board selection in board order, each with its own optional note.
    items: Vec<ApproveItem>,
) -> Result<Vec<Task>, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || reg.approve_tasks(&group_id, &items)).await
}

/// Issue a one-time human merge grant for a PR (#83), independent of the board —
/// a human-pane path to authorize exactly one default-branch merge. Optional
/// comment is delivered to the orchestrator with the grant. HUMAN-ONLY (Tauri
/// command; no MCP tool reaches grant-writing).
/// Off-thread (#762): the grant file write, an audit append, and the delivery
/// of the grant to the orchestrator's pane.
///
/// **Reentrancy.** The grant is a single `atomic_write` to a path keyed by the
/// PR number, carrying an expiry and a nonce from a process-global counter.
/// Two mints for one PR therefore produce one file with one nonce — the shim
/// consumes one merge either way — and two mints for different PRs touch
/// different paths. There is no read-modify-write to lose and no shared
/// directory state to corrupt, so nothing here depended on the webview thread;
/// what it contributed was the order of two notices.
#[tauri::command]
pub async fn orch_grant_merge(
    app: AppHandle,
    group_id: String,
    pr: String,
    comment: Option<String>,
) -> Result<u64, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || reg.grant_merge(&group_id, &pr, comment.as_deref(), "human")).await
}

/// Issue a human release grant for `tag` (#83/#438): authorizes the whole
/// release pipeline for that ONE tag — tag push, `gh release …`, release notes —
/// for `RELEASE_GRANT_TTL_SECS`, not a single command. Releases are never
/// blanket-allowed by autonomous mode, so this explicit grant is the only path.
/// Optional comment delivered to the orchestrator. HUMAN-ONLY.
/// Off-thread (#762): grant file, audit append and delivery.
///
/// **Reentrancy.** [`orch_grant_merge`]'s argument against an independent
/// directory, keyed by tag rather than PR number: one atomic write per tag, no
/// read-modify-write, and a TTL the file carries itself rather than inferring
/// from arrival order.
#[tauri::command]
pub async fn orch_grant_release(
    app: AppHandle,
    group_id: String,
    tag: String,
    comment: Option<String>,
) -> Result<(), String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || reg.grant_release(&group_id, &tag, comment.as_deref(), "human")).await
}

/// Request changes on a merge-gate item: record the findings and deliver them
/// to the orchestrator to route back to a worker.
/// Off-thread (#762): extra `tasks.json` reads on top of an upsert-sized write
/// under `tasks_lock`, plus a delivery to the worker.
///
/// **Reentrancy.** The family argument on [`orch_upsert_task`]. Specific to the
/// start/proceed/request-changes trio: each is a status transition the
/// orchestrator is *told about* rather than one it infers, so two of them
/// racing deliver two notices about a board whose final state both notices name
/// explicitly — the orchestrator reads the board, not the sequence of prompts.
#[tauri::command]
pub async fn orch_request_changes(
    app: AppHandle,
    group_id: String,
    id: String,
    findings: String,
) -> Result<Task, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || reg.request_changes(&group_id, &id, &findings)).await
}

/// Start a queued item: record a human-attributed note and tell the
/// orchestrator to begin work. Does not flip the status — the orchestrator
/// moves it to `in-progress` when it actually assigns a worker.
/// Off-thread (#762): the same shape as [`orch_request_changes`] — extra board
/// reads, an upsert-sized write under `tasks_lock`, and a delivery.
///
/// **Reentrancy.** As [`orch_request_changes`]. Specific to this one: it
/// deliberately does NOT flip the status (the orchestrator does that when it
/// actually assigns a worker), so it has no transition to lose a race over — it
/// records a human-attributed note and asks.
#[tauri::command]
pub async fn orch_start_task(app: AppHandle, group_id: String, id: String) -> Result<Task, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || reg.start_task(&group_id, &id)).await
}

/// Proceed on a prototype item (#147): flip it to `in-progress`, record the
/// human's sign-off, and tell the orchestrator to promote the prototype to a
/// full production build. The human's demo-gate verdict, so the status change
/// is applied here (mirrors `orch_approve_task`).
/// Off-thread (#762): extra board reads, an upsert-sized write under
/// `tasks_lock` and a delivery; the third member of the
/// start/proceed/request-changes trio.
///
/// **Reentrancy.** As [`orch_request_changes`]. Specific to this one: like
/// approve, it applies the status change itself (the human's demo-gate
/// verdict), so a duplicate lands the same `in-progress` twice and delivers two
/// promote notices — idempotent on the board, and the second notice names the
/// same item and the same verdict.
#[tauri::command]
pub async fn orch_proceed_task(app: AppHandle, group_id: String, id: String) -> Result<Task, String> {
    let reg = reg_of(&app);
    let group_id = command_group(&group_id)?;
    run_blocking(move || reg.proceed_task(&group_id, &id)).await
}
