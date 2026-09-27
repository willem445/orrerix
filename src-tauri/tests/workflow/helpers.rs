//! Shared fixtures: the registry and rails, and the throwaway `Repo`.
//!
//! One module of the `workflow` integration-test target (`main.rs`),
//! split out of the former single-file `tests/workflow.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

/// Build a registry against `dir` with every test-only directory override
/// applied — see `orchestration.rs`'s `relaunch_registry` (same rationale,
/// duplicated because these are separate integration-test binaries): a
/// second `OrchRegistry::new` built directly, without reapplying these
/// overrides, falls through to the REAL `~/.claude/agents`/`~/.copilot/agents`
/// on the next spawn (#464).
pub(crate) fn relaunch_registry(dir: &Path) -> OrchRegistry {
    let reg = OrchRegistry::new(dir.to_path_buf());
    reg.set_port(45999); // fake port so config writing works
    reg.set_claude_agents_dir_override(dir.join("claude-agents"));
    reg.set_copilot_agents_dir_override(dir.join("copilot-agents"));
    reg.set_compact_hook_dir_override(dir.join("compacthook"));
    reg.set_copilot_hooks_dir_override(dir.join("copilot-hooks"));
    reg
}

pub(crate) fn test_registry() -> (OrchRegistry, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    (reg, dir)
}

// #464 B2: the raw-`OrchRegistry::new` guard used to live here too (one
// `include_str!("workflow.rs")` copy per file). Replaced by a SINGLE
// dynamic test in `tests/orchestration/` that reads every `tests/*.rs`
// file from disk at runtime — a per-file `include_str!` copy could only
// ever see its own file, so `tests/lessonsfile.rs` and `tests/prompts.rs`
// (each with their own registry-construction helper) were never covered.
// See that test's doc for the full rationale.

/// Guardrails for a group that RUNS the repo's workflow file — i.e. the human
/// turned the advanced orchestrator on (#222). Every test below that is about the
/// schema, personas or the block model wants this; the toggle-off behavior (the
/// default, and the one the whole feature promises leaves loomux unchanged) has
/// its own tests in the *advanced-orchestrator toggle* section at the bottom.
pub(crate) fn rails() -> Guardrails {
    Guardrails {
        max_agents: 6,
        agent_cli: "claude".into(),
        auto_ops: false,
        advanced_orchestrator: true,
        ..Guardrails::default()
    }
}

/// A throwaway repo directory, optionally carrying a `.loomux/workflow.yml` and
/// `.github/agents/*.md` persona files.
///
/// The repo lives one level BELOW its own private temp root (`<root>/repo`),
/// not AT the root itself — see `orchestration.rs`'s `RealRepo` for why:
/// `git_worktree_add` cuts a worktree to a directory SIBLING to the repo
/// (`<repo's-parent>/<repo-name>-worktrees/<name>`), and a `git_init()`'d
/// `Repo` here feeds `spawn_agent` MCP calls whose worktree defaults ON
/// (#338/#359). Nested one level down, that sibling — and the `.git/
/// worktrees/<name>` admin registration `git worktree add` writes inside the
/// repo's own `.git` — both stay inside `_root`, so `_root`'s `Drop` (success,
/// assertion failure, or panic unwind alike) reclaims the whole tree; a bare
/// `tempfile::tempdir()` used directly as the repo root left the sibling
/// outside that scope and leaked it on every passing test (#464).
pub(crate) struct Repo {
    _root: tempfile::TempDir,
    pub(crate) repo: PathBuf,
}

impl Repo {
    pub(crate) fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        Repo { _root: root, repo }
    }
    pub(crate) fn path(&self) -> String {
        self.repo.to_string_lossy().replace('\\', "/")
    }
    pub(crate) fn workflow(self, yaml: &str) -> Self {
        let dir = self.repo.join(".loomux");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("workflow.yml"), yaml).unwrap();
        self
    }
    pub(crate) fn agent_file(self, name: &str, body: &str) -> Self {
        let dir = self.repo.join(".github").join("agents");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(name), body).unwrap();
        self
    }
    /// A minimal real git repo (one commit on the default branch). Needed by
    /// #338 tests: a worker spawn's worktree now defaults on at the MCP
    /// surface, so a test that spawns a worker through `spawn_agent` (rather
    /// than the direct Rust API) needs real git under this repo to succeed.
    pub(crate) fn git_init(self) -> Self {
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .current_dir(&self.repo)
                .args(args)
                .output()
                .expect("git must be installed for this test");
            assert!(ok.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&ok.stderr));
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        fs::write(self.repo.join("f.txt"), "hi").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        self
    }
}
