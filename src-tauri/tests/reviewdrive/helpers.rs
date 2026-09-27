//! Shared fixtures: the registry and rails, the WORKFLOW* files, `Repo`, `FakeGh`, and the drive/status helpers.
//!
//! One module of the `reviewdrive` integration-test target (`main.rs`),
//! split out of the former single-file `tests/reviewdrive.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ── the tick, through the registry ──────────────────────────────────────────

/// Build a registry against `dir` with every test-only directory override
/// applied — see `orchestration/helpers.rs`'s `relaunch_registry` (same rationale,
/// duplicated because these are separate integration-test binaries): a second
/// `OrchRegistry::new` built directly, without reapplying these overrides,
/// falls through to the REAL `~/.claude/agents`/`~/.copilot/agents` on the next
/// spawn (#464).
pub(crate) fn relaunch_registry(dir: &std::path::Path) -> OrchRegistry {
    let reg = OrchRegistry::new(dir.to_path_buf());
    reg.set_port(45999);
    reg.set_claude_agents_dir_override(dir.join("claude-agents"));
    reg.set_copilot_agents_dir_override(dir.join("copilot-agents"));
    reg.set_compact_hook_dir_override(dir.join("compacthook"));
    reg.set_copilot_hooks_dir_override(dir.join("copilot-hooks"));
    reg
}

pub(crate) fn rails() -> Guardrails {
    Guardrails {
        max_agents: 6,
        agent_cli: "claude".into(),
        auto_ops: false,
        advanced_orchestrator: true,
        ..Guardrails::default()
    }
}

pub(crate) const WORKFLOW: &str = r#"version: 1
blocks:
  - id: worker
    kind: worker
  - id: rev-std
    name: Standard review
    kind: reviewer
gates:
  merge:
    require: all-pass
    reviewers: [rev-std]
    routing:
      - paths: [src/**]
        reviewers: [rev-std]
merge_queue:
  enabled: true
driver:
  enabled: true
"#;

/// The same roster whose merge gate DECLARES `body-unchanged` (#2168 E2).
///
/// The stock fixture's gate has no `also:`, so a body edit leaves it satisfied
/// and a drive that reaches `gate-check` terminates — which is correct, and
/// makes it impossible to observe what `review-wait` does on the round AFTER a
/// lane has answered. Declaring the condition is what returns an unsatisfied
/// gate to `ci-wait` (arc 10) and from there to `review-wait`.
pub(crate) const WORKFLOW_BODY_UNCHANGED: &str = r#"version: 1
blocks:
  - id: worker
    kind: worker
  - id: rev-std
    name: Standard review
    kind: reviewer
gates:
  merge:
    require: all-pass
    reviewers: [rev-std]
    also: [body-unchanged]
merge_queue:
  enabled: true
driver:
  enabled: true
"#;

/// The same roster with the `driver:` block **absent** — §5.3's product default,
/// and the fixture the opt-in test needs. An absent block is a different subject
/// from `enabled: false`, and only one of the two is what almost every repo has.
pub(crate) const WORKFLOW_NO_DRIVER: &str = r#"version: 1
blocks:
  - id: worker
    kind: worker
  - id: rev-std
    name: Standard review
    kind: reviewer
gates:
  merge:
    require: all-pass
    reviewers: [rev-std]
"#;

/// A TWO-lane gate — the fixture the scope test's fourth block needs (#2508
/// review, W1).
///
/// The unchanged-head **non-verify** arm of `rd_lane_scope` is reachable only
/// when a lane must be re-briefed at the head it answered while `verify` cannot
/// be granted, and on a one-lane gate those two never co-occur: a sole lane
/// whose pass is bound to the unchanged head makes the `all` in
/// `decide_review_wait`'s grant true whenever the digest is readable — so the
/// round is verification and reaches `body-only` through the early return, not
/// through the arm. With a second lane that has recorded nothing, the grant
/// fails while `first_stale_lane` still stales the lane that answered (its
/// pass no longer settles: the digest moved) — so the drive re-briefs lane 0 at
/// an unchanged head with `verify: false`, which is that arm's only witness.
pub(crate) const WORKFLOW_TWO_LANE: &str = r#"version: 1
blocks:
  - id: worker
    kind: worker
  - id: rev-std
    name: Standard review
    kind: reviewer
  - id: rev-final
    name: Final validation
    kind: reviewer
gates:
  merge:
    require: all-pass
    reviewers: [rev-std, rev-final]
merge_queue:
  enabled: true
driver:
  enabled: true
"#;

/// The same roster with a **manager** block declared — the fixture the
/// manager-session refusal test needs (#1161 M3's class only exists when a
/// workflow declares it, and only then can a manager session be in the roster
/// for `drive_review` to resolve).
pub(crate) const WORKFLOW_WITH_MANAGER: &str = r#"version: 1
blocks:
  - id: manager
    kind: manager
  - id: worker
    kind: worker
  - id: rev-std
    name: Standard review
    kind: reviewer
gates:
  merge:
    require: all-pass
    reviewers: [rev-std]
merge_queue:
  enabled: true
driver:
  enabled: true
"#;

/// The same roster with **no worker block at all** — the fixture the
/// both-empty arm needs (N3): a recorded session with no block identity has
/// only the class default to fall back to, and this roster does not declare
/// one.
pub(crate) const WORKFLOW_NO_WORKER: &str = r#"version: 1
blocks:
  - id: rev-std
    name: Standard review
    kind: reviewer
gates:
  merge:
    require: all-pass
    reviewers: [rev-std]
merge_queue:
  enabled: true
driver:
  enabled: true
"#;

/// A throwaway repo one level below its own temp root — `orchestration/helpers.rs`'s
/// `RealRepo` rationale: a worktree is cut SIBLING to the repo, so nesting keeps
/// it inside the root that `Drop` reclaims.
pub(crate) struct Repo {
    _root: tempfile::TempDir,
    pub(crate) repo: std::path::PathBuf,
}

impl Repo {
    pub(crate) fn new() -> Repo {
        Repo::with(WORKFLOW)
    }
    /// A repo whose workflow file is `yaml` — so a test can vary the one thing
    /// it is about (the `driver:` block) and nothing else.
    pub(crate) fn with(yaml: &str) -> Repo {
        Repo::build(yaml)
    }
    fn build(yaml: &str) -> Repo {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let path = repo.to_string_lossy().replace('\\', "/");
        // Written through the reader's own path resolution rather than a
        // hard-coded directory name, so a rename of the config directory cannot
        // leave this fixture writing where nothing reads.
        let wf = loomux_lib::orchestration::workflow::workflow_file(&path);
        std::fs::create_dir_all(wf.parent().unwrap()).unwrap();
        std::fs::write(&wf, yaml).unwrap();
        let r = Repo { _root: root, repo };
        r.git_init();
        r
    }
    /// A minimal real git repo. Needed because a fresh reviewer lane is spawned
    /// WITH a worktree — `#338/#359`: a reviewer that landed in the group's main
    /// clone would be contending on the human's own checkout — and
    /// `git_worktree_add_sync` needs real git under the repo to cut one.
    fn git_init(&self) {
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .current_dir(&self.repo)
                .args(args)
                .output()
                .expect("git must be installed for this test");
            assert!(
                ok.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&ok.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(self.repo.join("f.txt"), "hi").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
    }
    pub(crate) fn path(&self) -> String {
        self.repo.to_string_lossy().replace('\\', "/")
    }
    /// Replace this repo's workflow file — the human editing it between one
    /// launch and the next, which §222's consent rule makes the ONE moment a
    /// group's declared roster changes under a session already recorded
    /// against it (#1961).
    pub(crate) fn rewrite_workflow(&self, yaml: &str) {
        let wf = loomux_lib::orchestration::workflow::workflow_file(&self.path());
        std::fs::write(&wf, yaml).unwrap();
    }
}

pub(crate) const HEAD_A: &str = "aa11bb22cc33dd44ee55ff6677889900aabbccdd";
/// The branch a worker that AUTHORED the fake PR was spawned on (#3367 item 2).
pub(crate) const AUTHOR_BRANCH: &str = "feat/3367-fixture";
pub(crate) const HEAD_B: &str = "bb22cc33dd44ee55ff6677889900aabbccddeeff";
/// A third head, for the one sequence that needs three: brief at A, red at B,
/// and then the WORKER's fix push, which has to be distinguishable from B or
/// arc 7 has nothing to fire on (#2168 E1).
pub(crate) const HEAD_C: &str = "cc33dd44ee55ff6677889900aabbccddeeff1122";

/// A canned `gh`, keyed on the SUBCOMMAND rather than on call order, so a test
/// asserts what the driver concluded and not the sequence it happened to read
/// in — the order is the tick's business and is pinned in `rddrive`'s own tests.
///
/// # Why `merge` is its own field
///
/// `mergeStateStatus` used to be a `CLEAN` literal inside `facts_json`, which
/// made it the one axis of a driven PR **no fixture in this file could vary**:
/// 25 green fixtures and seven sites setting a non-green *check*, against zero
/// setting a non-clean *mergeability*. That is CLAUDE.md's unpinned-axis rule —
/// a value every fixture happens to share — and what it left untested was the
/// whole live `CONFLICTING` arc: `observe_pr`'s classification of the
/// mergeability JSON, the second call it skips, `rd-conflicting`, the
/// `rebase_attempts` spend against a budget of one, the fix brief's rebase text,
/// and `held(rebase-limit)`. `CiObservation::Conflicting` appeared once in this
/// file, as a hand-built `DriveFacts` handed straight to `decide` — below the
/// seam, which is the construction #1841's B1 got through two clean reviews in
/// (#1862).
///
/// It is a **separate mutex from `facts`** rather than a fourth `set_facts`
/// argument for two reasons. A test varies ONE axis per call, so the existing
/// `set_facts("OPEN", HEAD_B)` sites keep their meaning and their bytes; and a
/// mergeability set once STAYS set across a head move, which is what a real
/// conflicting PR does — the branch does not stop conflicting because the worker
/// pushed to it.
pub(crate) struct FakeGh {
    /// `Ok((state, head))`, or `Err` for the seam itself failing.
    facts: std::sync::Mutex<Result<(String, String), String>>,
    /// The PR **body**, which `observe_pr` digests. Its own field for
    /// `merge`'s reason: §8's body-changed row is a re-brief at an UNCHANGED
    /// head, and before this the body was a `"b"` literal inside
    /// [`facts_json`], so no test in this file could reach that row at all.
    pub(crate) body: std::sync::Mutex<String>,
    /// The `mergeStateStatus` the PR-facts read reports.
    merge: std::sync::Mutex<String>,
    checks: std::sync::Mutex<String>,
    /// `(headRefName, title)` for the auto-start's identity read (#3367
    /// item 2) — its own field because the PR-facts read does not carry either,
    /// and answered by its own arm below, keyed on the field list it asks for.
    identity: std::sync::Mutex<(String, String)>,
    /// The repo's default branch, for the merge queue's `gh repo view --json
    /// defaultBranchRef` read (#3367 item 5: the clean case's enqueue). `main`
    /// by default — the same branch `facts_json` reports as every PR's base, so
    /// the realistic answer to an enqueue is `base-is-default`.
    default_branch: std::sync::Mutex<String>,
    pub(crate) calls: std::sync::Mutex<Vec<Vec<String>>>,
}

impl FakeGh {
    pub(crate) fn green(head: &str) -> FakeGh {
        FakeGh {
            facts: std::sync::Mutex::new(Ok(("OPEN".to_string(), head.to_string()))),
            body: std::sync::Mutex::new("b".to_string()),
            merge: std::sync::Mutex::new("CLEAN".to_string()),
            checks: std::sync::Mutex::new(
                r#"[{"name":"build","state":"SUCCESS","link":"x"}]"#.to_string(),
            ),
            identity: std::sync::Mutex::new((
                AUTHOR_BRANCH.to_string(),
                "feat: a change".to_string(),
            )),
            default_branch: std::sync::Mutex::new("main".to_string()),
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }
    /// The repo's default branch as the merge queue reads it (#3367 item 5).
    pub(crate) fn set_default_branch(&self, name: &str) {
        *self.default_branch.lock().unwrap_or_else(|e| e.into_inner()) = name.to_string();
    }
    /// The PR's head branch and title, as the auto-start reads them (#3367).
    pub(crate) fn set_identity(&self, head_ref: &str, title: &str) {
        *self.identity.lock().unwrap_or_else(|e| e.into_inner()) =
            (head_ref.to_string(), title.to_string());
    }
    /// The seam itself failing — `gh` missing, or a child killed at the command
    /// timeout. Not a `gh` refusal, and not a fact about the PR.
    pub(crate) fn seam_down(&self) {
        *self.facts.lock().unwrap_or_else(|e| e.into_inner()) = Err("gh-not-found".into());
    }
    /// Replace the canned `gh pr checks` payload — how a test turns CI red, or
    /// gives a check a name a PR author could have written.
    pub(crate) fn set_checks(&self, json: &str) {
        *self.checks.lock().unwrap_or_else(|e| e.into_inner()) = json.to_string();
    }
    pub(crate) fn set_facts(&self, state: &str, head: &str) {
        *self.facts.lock().unwrap_or_else(|e| e.into_inner()) =
            Ok((state.to_string(), head.to_string()));
    }
    /// The PR body the next observation reads — how a test moves the DIGEST
    /// while leaving the head where it is (§8's body-changed row).
    pub(crate) fn set_body(&self, body: &str) {
        *self.body.lock().unwrap_or_else(|e| e.into_inner()) = body.to_string();
    }
    /// The mergeability GitHub reports — `CLEAN`, `CONFLICTING`, or any of the
    /// several words that are neither (`BEHIND`, `BLOCKED`, …), which
    /// `pr_mergeability_result` deliberately does not short-circuit on.
    pub(crate) fn set_merge_state(&self, merge: &str) {
        *self.merge.lock().unwrap_or_else(|e| e.into_inner()) = merge.to_string();
    }
    pub(crate) fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    /// How many `gh pr checks` calls this fake has answered — the positive
    /// control for the second call `observe_pr` SKIPS on a conflict.
    pub(crate) fn checks_calls(&self) -> usize {
        self.calls().iter().filter(|a| a.iter().any(|s| s == "checks")).count()
    }
}

fn facts_json(state: &str, head: &str, merge: &str, body: &str) -> String {
    format!(
        r#"{{"state":"{state}","headRefOid":"{head}","baseRefName":"main","body":"{body}",
             "mergeStateStatus":"{merge}","additions":1,"deletions":1}}"#
    )
}

impl RdRunner for FakeGh {
    fn gh(&self, args: &[&str]) -> Result<CmdOut, String> {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(args.iter().map(|s| s.to_string()).collect());
        let out = |s: &str| Ok(CmdOut { code: Some(0), stdout: s.to_string(), stderr: String::new() });
        if args.iter().any(|a| *a == "checks") {
            return out(&self.checks.lock().unwrap_or_else(|e| e.into_inner()).clone());
        }
        // The routed-file list, in `ROUTING_FILES_JQ`'s own reduced shape: the
        // word `ok`, then one `p <path>` line per changed file. Distinguished by
        // the `--jq` argument rather than by call order, so this stays keyed on
        // WHAT was asked.
        //
        // Answering it at all is the fix for a fixture gap that made the driver
        // look broken: a gate declaring `routing:` makes `review-wait` resolve
        // the changed-file list, and a fake that replied with the PR-facts JSON
        // produced a list `parse_routed_files` refuses — so `route_reviewers`
        // answered `None` and every drive parked on
        // `held(routing-unaccountable)`. That is the driver being RIGHT (an
        // unknown reviewer requirement is refused, never assumed empty) and the
        // fake being incomplete.
        // The default-branch read (#3367 item 5), keyed on the field it asks
        // for and answered BEFORE the `--jq` arm below, which it would
        // otherwise fall into — `default_branch_argv` carries a `--jq` too.
        if args.iter().any(|a| *a == "defaultBranchRef") {
            return out(&format!(
                "{}\n",
                self.default_branch.lock().unwrap_or_else(|e| e.into_inner())
            ));
        }
        if args.iter().any(|a| *a == "--jq") {
            return out("ok\np src/lib.rs\n");
        }
        // #3367 item 2's identity read, keyed on the field list it asks for.
        if args.iter().any(|a| *a == "state,headRefName,title") {
            let (head_ref, title) = self.identity.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let state = match &*self.facts.lock().unwrap_or_else(|e| e.into_inner()) {
                Ok((state, _)) => state.clone(),
                Err(e) => return Err(e.clone()),
            };
            return out(&format!(
                r#"{{"state":"{state}","headRefName":"{head_ref}","title":"{title}"}}"#
            ));
        }
        match &*self.facts.lock().unwrap_or_else(|e| e.into_inner()) {
            // Composed at read time from the two axes a test sets separately, so
            // a `set_facts` and a `set_merge_state` can be issued in either
            // order and neither silently reverts the other.
            Ok((state, head)) => out(&facts_json(
                state,
                head,
                &self.merge.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                &self.body.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            )),
            Err(e) => Err(e.clone()),
        }
    }
}

/// A registry with a group, the driver enabled, and one drive started on PR 1758
/// through the real `drive_review`.
pub(crate) fn driven(
    reg: &OrchRegistry,
    repo: &Repo,
    gh: &FakeGh,
) -> (GroupId, String) {
    // `create_group` answers a `GroupInfo`; every driver surface takes the
    // validated `GroupId` off it, which is CLAUDE.md constraint 6 — the proof
    // travels with the value rather than with the call site.
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;
    // **A REAL worker, whose session is genuinely resumable.**
    //
    // The obvious fixture — a well-shaped uuid this roster never recorded — is
    // accepted by `drive_review` (§5.1: `resolve_session_ref`'s passthrough arm,
    // and the note is explicit that resolving is not proving resumable), and
    // then parks the drive at `held(worker-unresumable)` on the FIRST hand-back.
    // That is the driver behaving exactly as §5.1 describes, and it is useless
    // as a fixture for anything downstream of a hand-back: three tests were
    // asserting `fix-wait` and reading `held`.
    //
    // So the worker is spawned for real and its own session id is what the drive
    // is pointed at — which is also what an orchestrator actually passes.
    let w = reg
        .spawn_agent(&group, Role::Worker, "w", "", false, None)
        .expect("a worker to hand back to");
    let session = w.session_id.clone().expect("claude mints a session id at spawn");
    let out = reg.drive_review_with(&group, gh, 1758, &session, false, 0, "orch-1", 0);
    assert_eq!(out["driving"], serde_json::json!(true), "drive_review refused: {out}");
    (group, session)
}

/// Give an agent a pane, which a delivery requires.
///
/// `deliver_prompt` resolves the target's `pty_id` BEFORE it audits, and answers
/// `Err` for an agent that has none — "a target with no terminal has nowhere to
/// hold anything" (#569). In test mode nothing binds a pane, so an orchestrator
/// spawned and left alone silently receives nothing, and a test asserting a
/// delivery reads an empty audit log rather than a missing feature.
pub(crate) fn with_pane(reg: &OrchRegistry, agent_id: &str, pty: u32) {
    reg.set_pty_for_test(agent_id, pty);
}

pub(crate) fn status_head(reg: &OrchRegistry, group: &GroupId) -> String {
    let s = reg.review_drive_status(group);
    s["drives"][0]["head"].as_str().unwrap_or_default().to_string()
}

pub(crate) fn status_state(reg: &OrchRegistry, group: &GroupId) -> String {
    let s = reg.review_drive_status(group);
    s["drives"][0]["state"].as_str().unwrap_or_default().to_string()
}
