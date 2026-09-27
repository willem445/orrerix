//! The staging directory for `post_issue_comment` bodies and its sweep.
//! Design note: `docs/design/shim-path-integrity.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. IO: fs. It calls
//! no sibling file.

use super::*;

/// Per-call sequence for `post_issue_comment`'s staging file — see that method's
/// doc for why the agent id alone is not a unique enough name. Process-wide
/// rather than per-group: it only has to separate calls that are in flight at the
/// same moment, and one counter does that for every group at once.
pub(in crate::orchestration) static COMMENT_BODY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The subdirectory of the group dir that `post_issue_comment` stages comment
/// bodies in (#3061 residual 1).
///
/// A directory of its own because the group dir is ALSO where each roster
/// block's instruction file lives as `<block id>.md`, and a block id is
/// operator-authored — see `post_issue_comment`'s doc for the collision this
/// separates. It is also what makes [`sweep_staged_comment_bodies`] safe to
/// write: it enumerates a directory nothing else writes to.
pub const COMMENT_BODY_DIR: &str = "comment-bodies";

/// What `post_issue_comment` answers when the comment WAS posted and `gh` did
/// not print a URL this build can read (#3061 residual 4).
///
/// A sentence rather than an empty string, and `Ok` rather than `Err`, because
/// both of the obvious alternatives state something false. `Err` says the post
/// did not happen — it did, and an agent that retried on it would double its
/// plan onto the issue. An empty string is the unpinned shape this residual is
/// about: it reads as an address and is not one. This reads as neither.
pub const POSTED_URL_UNREADABLE: &str =
    "(posted — orrerix could not read the comment's URL from gh's output; the audit row carries what gh printed)";

/// How old a staged comment body must be before the sweep will delete it.
///
/// **Derived, not picked.** A staging file is live exactly as long as the `gh`
/// child reading it can run, which [`GH_CAPTURE_TIMEOUT`] bounds; this is that
/// timeout with a wide margin, so a file belonging to an in-flight post is never
/// a sweep candidate. A sweep that raced a concurrent post would be a worse
/// defect than the litter it cleans.
pub const STAGING_ORPHAN_AGE: std::time::Duration =
    std::time::Duration::from_secs(GH_CAPTURE_TIMEOUT.as_secs() * 10 + 600);

/// Delete comment-body staging files old enough that no in-flight post can own
/// them (#3061 residual 3).
///
/// **Best-effort throughout, and every failure is silence rather than an
/// error**: this runs on the way into a post, and a directory orrerix cannot
/// read is not a reason to refuse to publish a plan. An entry whose mtime
/// cannot be read is treated as YOUNG and left alone — unknown is not a licence
/// to delete.
///
/// `now` is the CALLER's clock, so the bound is one a test can actually
/// perform — the same reason `cancel_review_drive_with` takes one. A bound
/// measured against a clock a test cannot set is a bound no test can reach.
pub fn sweep_staged_comment_bodies(staging: &Path, now: std::time::SystemTime) {
    let Ok(entries) = fs::read_dir(staging) else { return };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else { continue };
        // A file stamped in the FUTURE (a clock step, a copied mtime) yields
        // `Err` here and is left alone, which is the young side — unknown is
        // never a licence to delete.
        let Ok(age) = now.duration_since(modified) else { continue };
        if age >= STAGING_ORPHAN_AGE {
            let _ = fs::remove_file(entry.path());
        }
    }
}
