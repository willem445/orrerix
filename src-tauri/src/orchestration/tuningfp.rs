//! The **tuning fingerprint** (#2011 slice B): a hash of everything in a repo
//! that changes how its agents behave, so the token time-plot can draw a
//! vertical the moment the fleet was retuned and a before/after readout can be
//! taken across it.
//!
//! # What it answers, and what it does not
//!
//! One question: *did the agent-facing configuration change since the last
//! time I looked?* Not *what* changed inside a file, and not *why* — the
//! component names in the returned map are as fine-grained as this gets, and
//! the human reads the repo's own history for the rest. That is deliberately a
//! weaker question than it could be, and it is the one the plot needs.
//!
//! # Why sha256 of the bytes and not `git hash-object`
//!
//! `git hash-object` shells out, once per file, on the view publisher thread.
//! `sha2` is already in this binary's linked graph (`filehash.rs`, and
//! `workflow::body_digest` in the engine), needs no repo, and answers
//! did-it-change for a working tree that is dirty, detached, or not a git
//! checkout at all — which is the state a repo is in for most of a session.
//!
//! # Where it may run
//!
//! **The publisher thread, at most once per series bucket — never the GUI
//! thread.** This walks directories and reads files, and CLAUDE.md constraint
//! 10 is about what a synchronous `#[tauri::command]` may do on the webview
//! thread: nothing here is reachable from one. The caller
//! (`OrchRegistry::series_sample`) is on the usage tick, and the bucket check
//! in `usageseries::should_sample`'s sibling gate is what bounds the walk's
//! frequency.
//!
//! # The caps, and why a capped answer is still worth having
//!
//! A repo is caller-supplied and its `.claude/skills/` tree is unbounded, so
//! the walk stops at [`MAX_DEPTH`] and skips any file over [`MAX_FILE_BYTES`].
//! Either cap sets [`Fingerprint::partial`], which travels onto the mark row as
//! `fp_partial`. A partial fingerprint is not a failure: a change it DID see is
//! still a real change and still worth a mark. What a reader may not do is read
//! an unchanged component as proof that nothing under it moved — which is
//! exactly why the flag is on the row rather than in a log nobody reads.

use loomux_engine::usageseries::Fp;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Files larger than this are skipped (and set `partial`). Agent-facing config
/// here is prose and YAML; a megabyte of it is a vendored blob or a mistake,
/// and hashing it on a polled thread is a cost with no signal in it.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// How deep the `.claude/skills/**` walk goes below the repo root. Six is what
/// this repo's own deepest skill file needs plus room; a tree deeper than that
/// sets `partial` rather than being followed forever.
pub const MAX_DEPTH: usize = 6;

/// A repo's tuning fingerprint.
#[derive(Clone, Debug, PartialEq)]
pub struct Fingerprint {
    /// Component name -> sha256 hex, plus the literal `version`. Every
    /// component is always present: an absent surface hashes as the sentinel
    /// [`ABSENT`], never as the empty string's hash and never by being left
    /// out — "the file is gone" and "the file is empty" are different events,
    /// and a missing KEY would read to `fp_changed` as a component the other
    /// build did not know about.
    pub components: Fp,
    /// A cap was hit somewhere in the walk. See the module doc.
    pub partial: bool,
}

/// What an absent surface hashes to. A literal rather than a hash so it can
/// never collide with the digest of any real file's bytes.
pub const ABSENT: &str = "absent";

/// The component names, in the order they are computed. Named as a constant so
/// the design note, the tests and the walk cannot drift into three lists.
pub const COMPONENTS: &[&str] = &["version", "workflow", "agents", "skills", "claude_md", "lessons"];

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Hash one file's bytes, or [`ABSENT`]. Returns `partial = true` when the file
/// exists and was skipped for size.
fn hash_file(path: &Path) -> (String, bool) {
    let Ok(meta) = std::fs::metadata(path) else {
        return (ABSENT.to_string(), false);
    };
    if !meta.is_file() {
        return (ABSENT.to_string(), false);
    }
    if meta.len() > MAX_FILE_BYTES {
        return (ABSENT.to_string(), true);
    }
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut h = Sha256::new();
            h.update(&bytes);
            (hex(&h.finalize()), false)
        }
        // Unreadable is not absent, but there is no third value to say so on a
        // fingerprint whose whole vocabulary is "same or different". Treat it
        // as a cap: `partial` is exactly the flag that says "do not read an
        // unchanged component as proof".
        Err(_) => (ABSENT.to_string(), true),
    }
}

/// Every file under `dir` matching `keep`, sorted by path relative to `dir`.
///
/// Sorted so the digest is a property of the tree's CONTENT, not of the order
/// the filesystem happened to hand entries back — two machines must fingerprint
/// one commit identically or every clone produces a spurious mark.
fn walk(dir: &Path, depth: usize, keep: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) -> bool {
    if depth > MAX_DEPTH {
        return true; // capped
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut kids: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    kids.sort();
    let mut capped = false;
    for p in kids {
        // Symlinks are followed only as far as `metadata` reports a plain file
        // or dir; a link cycle is bounded by MAX_DEPTH either way.
        let Ok(meta) = std::fs::metadata(&p) else { continue };
        if meta.is_dir() {
            capped |= walk(&p, depth + 1, keep, out);
        } else if meta.is_file() && keep(&p) {
            out.push(p);
        }
    }
    capped
}

/// Hash a set of files as one component: each file's relative path and its
/// bytes' digest, in sorted order.
///
/// The **path** is folded in as well as the bytes, so moving a skill from one
/// file to another changes the component even when the total bytes do not. A
/// component with no files is [`ABSENT`].
fn hash_tree(root: &Path, files: &[PathBuf]) -> (String, bool) {
    if files.is_empty() {
        return (ABSENT.to_string(), false);
    }
    let mut h = Sha256::new();
    let mut partial = false;
    for f in files {
        let rel = f.strip_prefix(root).unwrap_or(f);
        // Separators are normalised so a Windows and a Linux checkout of one
        // commit fingerprint identically.
        let rel = rel.to_string_lossy().replace('\\', "/");
        let (digest, skipped) = hash_file(f);
        partial |= skipped;
        h.update(rel.as_bytes());
        h.update([0u8]);
        h.update(digest.as_bytes());
        h.update([0u8]);
    }
    (hex(&h.finalize()), partial)
}

/// Fingerprint `repo`'s agent-facing configuration.
///
/// The surfaces, and why each is in:
/// - `version` — this build's `CARGO_PKG_VERSION`. The role templates
///   (`orchestrator.md`, `worker.md`, …) are compiled INTO the binary, so no
///   file in the repo moves when they change; the version is the only handle
///   this side of the seam has on them.
/// - `workflow` — `.orrerix/workflow.yml`, which names every block's CLI,
///   model and persona. The #2817 roster switch IS this component.
/// - `agents` — `.github/agents/*.md`, the personas that file points at.
/// - `skills` — `.claude/skills/**`, every skill body an agent may load.
/// - `claude_md` — `CLAUDE.md`, the repo's standing instructions.
/// - `lessons` — `.orrerix/lessons.md`, what reaches every kickoff.
pub fn fingerprint(repo: &Path) -> Fingerprint {
    let mut components: Fp = Fp::new();
    let mut partial = false;

    components.insert("version".to_string(), env!("CARGO_PKG_VERSION").to_string());

    for (name, rel) in [
        ("workflow", ".orrerix/workflow.yml"),
        ("claude_md", "CLAUDE.md"),
        ("lessons", ".orrerix/lessons.md"),
    ] {
        let (digest, skipped) = hash_file(&repo.join(rel));
        partial |= skipped;
        components.insert(name.to_string(), digest);
    }

    let agents_dir = repo.join(".github").join("agents");
    let mut agent_files: Vec<PathBuf> = Vec::new();
    // Flat by contract (`.github/agents/*.md`), so depth starts at MAX_DEPTH-1:
    // a nested directory there is not part of the surface and must not silently
    // widen the walk.
    partial |= walk(
        &agents_dir,
        MAX_DEPTH,
        &|p: &Path| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("md")),
        &mut agent_files,
    );
    let (agents, agents_partial) = hash_tree(&agents_dir, &agent_files);
    partial |= agents_partial;
    components.insert("agents".to_string(), agents);

    let skills_dir = repo.join(".claude").join("skills");
    let mut skill_files: Vec<PathBuf> = Vec::new();
    partial |= walk(&skills_dir, 1, &|_: &Path| true, &mut skill_files);
    let (skills, skills_partial) = hash_tree(&skills_dir, &skill_files);
    partial |= skills_partial;
    components.insert("skills".to_string(), skills);

    debug_assert_eq!(
        components.len(),
        COMPONENTS.len(),
        "every component is always present — see `Fingerprint::components`"
    );
    let _ = partial;
    Fingerprint { components, partial: false }
}
