//! Where a group lives on disk: `group_dir_at` (CLAUDE.md constraint 6), the
//! group id for a repo, and the group-repo check.
//! Design note: `docs/design/groupid-and-path-roots.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. It calls no
//! sibling file.

use super::*;

/// The two checks every group-minting path runs against the repository path it
/// was handed, before anything is created.
///
/// Extracted from `create_orchestration_group` (#2519) rather than restated in
/// `lead_prepare`: the quote guard in particular is not obvious, and a second
/// copy that forgot it would be a pane whose command line the path escapes.
pub(in crate::orchestration) fn validate_group_repo(repo: &str) -> Result<(), String> {
    // Paths are interpolated into a quoted shell line; a quote inside one
    // would escape it. (Windows filesystems forbid `"` in names; this
    // guards the Unix builds and hand-typed paths.)
    if repo.contains('"') {
        return Err("repository path must not contain a quote character".into());
    }
    if !Path::new(repo).is_dir() {
        return Err(format!("repository path does not exist: {repo}"));
    }
    Ok(())
}

/// **The single place a group-scoped path is assembled** (#904) — the
/// `group_dir(root, &GroupId)` the issue asked for, as a free function so that
/// the callers holding a bare `root` (`append_audit` and
/// `promptsubmit_marker_path`, both reached from `deliver_now`, which has no
/// registry) share it rather than each writing their own join.
/// [`OrchRegistry::group_dir`] is the `&self` convenience over this.
///
/// Because [`GroupId`] does not implement `AsRef<Path>`, a validated id cannot
/// be joined onto anything except through this function — the shortcut is not
/// merely discouraged, it is unspellable. `the_orchestration_root_is_joined_with_a_group_in_exactly_one_place`
/// pins that no second join grows back.
pub(crate) fn group_dir_at(root: &Path, group: &GroupId) -> PathBuf {
    root.join(group.as_str())
}

/// Stable, filesystem-safe group id for a repo path, so relaunching an
/// orchestrator on the same repo reattaches to the same state directory.
///
/// #904: "filesystem-safe" is now a checkable claim rather than an assertion —
/// the output of this function must always satisfy [`GroupId::parse`], which is
/// what `the_minter_can_never_produce_an_id_its_own_validator_rejects` pins.
/// The one shape that used to violate it was a repo *directory* whose name
/// begins with `-`: the slug kept the dash and the id led with it, which
/// `GroupId::parse` refuses (a bare `-foo` is an option to any command line the
/// id reaches). Leading dashes are stripped here so the minter cannot mint
/// something the validator would reject. The delta is confined to repo
/// directories named `-…`, which get a different state directory than they
/// would have before.
#[doc(hidden)] // pub for integration tests
pub fn group_id_for_repo(repo: &str) -> String {
    let norm = repo.replace('\\', "/").to_lowercase();
    let norm = norm.trim_end_matches('/');
    // FNV-1a 64
    let mut h: u64 = 0xcbf29ce484222325;
    for b in norm.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let slug: String = norm
        .rsplit('/')
        .next()
        .unwrap_or("repo")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(24)
        .collect();
    let slug = slug.trim_start_matches('-');
    let slug = if slug.is_empty() { "repo" } else { slug };
    format!("{slug}-{:08x}", (h >> 32) as u32 ^ h as u32)
}
