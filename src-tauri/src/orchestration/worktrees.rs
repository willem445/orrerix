//! Worktree cleanup targets and the reviewer scratch-worktree verdict.
//! Design note: `docs/design/orchestration.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. Sibling files it
//! calls: `pathkey.rs`.

use super::*;

/// Distinct agent working directories to remove when a group is torn down
/// with worktree cleanup: dedup (case/separator-insensitively), and never the
/// repo root itself — the orchestrator and any repo-mode workers run there, so
/// removing it would delete the user's own checkout. Pure so the path
/// filtering is testable without a real git tree; the actual removal is
/// `git::git_worktree_remove`, which git refuses on a non-worktree anyway.
pub fn worktree_cleanup_targets(repo: &str, cwds: &[String]) -> Vec<String> {
    let norm = |s: &str| s.replace('\\', "/").trim_end_matches('/').to_lowercase();
    let repo_n = norm(repo);
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for c in cwds {
        if c.trim().is_empty() {
            continue;
        }
        let cn = norm(c);
        if cn == repo_n {
            continue; // repo root — the orchestrator's cwd, never a worktree
        }
        if seen.insert(cn) {
            out.push(c.clone());
        }
    }
    out
}

/// One pane's claim on a workspace, as the reviewer-scratch reclaim reads the
/// roster (#3443). Built from the live registry and the durable roster alike
/// (`OrchRegistry::workspace_claims`), because the pane that CUT a worktree and
/// the pane that dies in it are not always the same one: a resumed reviewer
/// runs in its session's original worktree and carries no branch of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceClaim {
    pub id: String,
    /// The pane's capability class is `Role::Reviewer`.
    pub reviewer: bool,
    pub cwd: String,
    /// The branch the spawn recorded for it — for a reviewer, `Some` exactly
    /// when that spawn cut a worktree (`spawn_agent_ex` persists a branch for a
    /// reviewer on no other path).
    pub branch: Option<String>,
    /// Not `Dead` in this process's registry.
    pub live: bool,
}

/// What [`reviewer_scratch_verdict`] decided about one workspace (#3443).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScratchVerdict {
    /// A reviewer's scratch worktree, cut on `branch`, that nothing else
    /// claims: the reclaim may remove it and the branch, and a resume may cut
    /// it again at the same path.
    Scratch { branch: String },
    /// Not a cut reviewer worktree at all — the group's main clone, or a path
    /// no reviewer record carries a branch for. Nothing to do and nothing worth
    /// an audit row: this is every worker, planner and no-worktree reviewer.
    NotScratch,
    /// A reviewer's cut worktree that something else ALSO claims. Kept, and the
    /// reason is audited, because this is the case where removing it would
    /// destroy someone's workspace.
    Kept(&'static str),
}

/// **Is `cwd` a reviewer's scratch worktree that nothing else holds?** (#3443)
///
/// A reviewer's worktree is scratch by contract (#359): cut fresh from the
/// default branch, used to `gh pr checkout --detach` the PR under review, never
/// pushed. So removing it when its pane dies loses nothing — but only if the
/// path really is one. A worker's worktree holds the branch under review, and
/// this function is the whole of what stands between the reclaim and it, so it
/// decides on the roster's own records and fails toward KEEPING:
///
/// 1. never the group's main clone;
/// 2. some reviewer record must carry a branch for exactly this path — the
///    record of the spawn that cut it, which for a fresh reviewer is the dying
///    pane itself and for a resumed one is the session's original pane. Two
///    such records naming different branches is ambiguous and kept;
/// 3. no NON-reviewer record may name this path, or that branch — a worker
///    resumed into it, or one whose own branch happens to share the name, owns
///    it as much as the reviewer does;
/// 4. no live pane other than `except` (the one that just died) may be running
///    in it — a resumed reviewer still using the directory.
///
/// Rule 2 is what makes a worker's worktree unreachable: nothing records a
/// reviewer at a worker's path. Rule 3 is the backstop for the one way the
/// roster could — a resume with an explicit `cwd` naming someone else's
/// workspace. Pure, so every rule is pinned without git.
pub fn reviewer_scratch_verdict(
    repo: &str,
    cwd: &str,
    claims: &[WorkspaceClaim],
    except: Option<&str>,
) -> ScratchVerdict {
    if cwd.trim().is_empty() || same_path_key(cwd, repo) {
        return ScratchVerdict::NotScratch;
    }
    let here = |c: &WorkspaceClaim| !c.cwd.trim().is_empty() && same_path_key(&c.cwd, cwd);
    let mut cut: Option<&str> = None;
    for c in claims.iter().filter(|c| c.reviewer && here(c)) {
        let Some(b) = c.branch.as_deref().map(str::trim).filter(|b| !b.is_empty()) else { continue };
        match cut {
            None => cut = Some(b),
            Some(prev) if prev == b => {}
            Some(_) => return ScratchVerdict::Kept("ambiguous-branch"),
        }
    }
    let Some(branch) = cut else { return ScratchVerdict::NotScratch };
    if claims
        .iter()
        .any(|c| !c.reviewer && (here(c) || c.branch.as_deref().map(str::trim) == Some(branch)))
    {
        return ScratchVerdict::Kept("claimed-by-a-non-reviewer");
    }
    if claims.iter().any(|c| c.live && Some(c.id.as_str()) != except && here(c)) {
        return ScratchVerdict::Kept("in-use-by-a-live-pane");
    }
    ScratchVerdict::Scratch { branch: branch.to_string() }
}

/// The waits before each reviewer-scratch removal attempt (#3443) — five
/// attempts over about fifteen seconds.
///
/// More than one because the pane's process may still be alive when its death
/// is recorded: a driver release marks the pane dead BEFORE it kills the pty
/// (`release_driven_pane`'s ordering), and on Windows a directory that is a
/// live process's cwd cannot be deleted. The first wait gives the kill that
/// follows a chance to land; the rest cover a child process (a `gh` or `git`
/// the agent started there) taking a moment longer to go.
pub(in crate::orchestration) const SCRATCH_RECLAIM_BACKOFF: [Duration; 5] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
];
