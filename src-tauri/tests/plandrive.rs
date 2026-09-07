//! Integration tests for the engine-driven PLAN driver (#3040 P3a).
//!
//! Design note: `doc/design/plan-driver.md`. The pure core's own properties are
//! pinned inline in `crates/loomux-engine/src/plandrive.rs`; what lives here is
//! everything that needs a **crate boundary** or the registry — the tick's
//! wiring, the four tools, the planner interception, and the post-time plan
//! hook.
//!
//! A new integration-test *target*, for `tests/reviewdrive.rs`'s two reasons:
//! CLAUDE.md constraint 4 (a test executable linking the full lib needs the
//! comctl32-v6 manifest `build.rs` embeds through `-tests`-scoped link args, and
//! the reason is the target KIND, not the file name), and the end-of-file append
//! conflict that file catalogues on `tests/orchestration.rs`.
//!
//! No test here spawns a real agent CLI (constraint 3) or a real `gh` child.

use loomux_lib::orchestration::brand;
use loomux_lib::orchestration::mcp::dispatch;
use loomux_lib::orchestration::mqdriver::CmdOut;
use loomux_lib::orchestration::plandrive::{self, Consent, PdHeldReason, PlanDriveState};
use loomux_lib::orchestration::rddrive::RdRunner;
use loomux_lib::orchestration::plandrive::PR_WAIT_HOLD_MS;
use loomux_lib::orchestration::reviewdrive::CAP_HOLD_MS;
use loomux_lib::orchestration::{
    Caller, GroupId, Guardrails, OrchRegistry, PdPlanCheck, Role, PD_MAX_GH_PER_TICK,
    PD_MAX_PR_CHECKS_PER_TICK,
};
use serde_json::{json, Value};

// ── fixtures ────────────────────────────────────────────────────────────────

const WORKFLOW: &str = r#"version: 1
name: driven
blocks:
  - id: orch
    kind: orchestrator
  - id: plan-lead
    name: The planner
    kind: planner
  - id: worker-adv
    kind: worker
  - id: rev-std
    kind: reviewer
gates:
  merge:
    require: all-pass
    reviewers: [rev-std]
driver:
  enabled: true
  plan_enabled: true
  planner_timeout_minutes: 60
"#;

/// The same roster with the PLAN driver off — the product default, and the
/// control for every `plan-driver-disabled` assertion.
const WORKFLOW_NO_PLAN_DRIVER: &str = r#"version: 1
name: driven
blocks:
  - id: orch
    kind: orchestrator
  - id: plan-lead
    name: The planner
    kind: planner
  - id: worker-adv
    kind: worker
  - id: rev-std
    kind: reviewer
gates:
  merge:
    require: all-pass
    reviewers: [rev-std]
driver:
  enabled: true
"#;

/// The same roster with a SHORT whole-drive timeout — the only axis it varies.
///
/// A declared review window is clamped at 120 minutes and the default timeout
/// is 720, so a window can never outrun the backstop on its own; shortening the
/// TIMEOUT crosses the same two knobs from the side a repo can actually declare.
const WORKFLOW_SHORT_TIMEOUT: &str = r#"version: 1
name: driven
blocks:
  - id: orch
    kind: orchestrator
  - id: plan-lead
    name: The planner
    kind: planner
  - id: worker-adv
    kind: worker
  - id: rev-std
    kind: reviewer
gates:
  merge:
    require: all-pass
    reviewers: [rev-std]
driver:
  enabled: true
  plan_enabled: true
  planner_timeout_minutes: 60
  drive_timeout_minutes: 5
"#;

/// A roster with no `kind: planner` block at all.
const WORKFLOW_NO_PLANNER: &str = r#"version: 1
name: driven
blocks:
  - id: orch
    kind: orchestrator
  - id: worker-adv
    kind: worker
  - id: rev-std
    kind: reviewer
gates:
  merge:
    require: all-pass
    reviewers: [rev-std]
driver:
  enabled: true
  plan_enabled: true
"#;

/// A valid plan for issue 3040, in the shape a planner posts it.
const PLAN: &str = "\
version: 1
issue: 3040
slices:
  - id: P1
    title: the plan block parser
    branch: feat/3040-p1-plan-block
    block: worker-adv
    deps: []
    brief: |
      Extract, parse and validate one fenced orrerix-plan block, refusing with
      line numbers and repairing nothing at all.
  - id: P2
    title: the drive core
    branch: feat/3040-p2-drive-core
    block: worker-adv
    deps: [P1]
    brief: |
      The record, the state machine and the per-tick decision, mirroring the
      review driver's own core module.
";

fn in_comment(block: &str) -> String {
    format!("Plan for #3040.\n\nHere it is:\n\n```orrerix-plan\n{block}```\n\nEnd.\n")
}

struct Repo {
    _root: tempfile::TempDir,
    repo: std::path::PathBuf,
}

impl Repo {
    fn new() -> Repo {
        Repo::with(WORKFLOW)
    }
    fn with(yaml: &str) -> Repo {
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
    /// A minimal real git repo, and the repo sits one level BELOW its own temp
    /// root so that the worktrees `git worktree add` cuts as its SIBLING land
    /// inside the directory `Drop` reclaims (#464's leak check is what makes
    /// that placement load-bearing rather than tidy).
    ///
    /// Needed because a slice's worker is spawned WITH a worktree — that is
    /// what a worker gets — and `git_worktree_add_sync` needs real git under
    /// the repo to cut one.
    fn git_init(&self) {
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
        std::fs::write(self.repo.join("f.txt"), "hi").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
    }
    fn path(&self) -> String {
        self.repo.to_string_lossy().replace('\\', "/")
    }
}

/// **Every fixture workflow in this file must PARSE**, and this is not
/// housekeeping — it is the control for every "the driver is off" assertion
/// below.
///
/// `pd_policy` answers OFF for a file it cannot read, which is exactly the
/// answer those tests assert. So a fixture that stops parsing turns them all
/// vacuous with nothing red to say so, and that is not hypothetical: the two
/// reduced rosters here declared `require: all-pass` with no `kind: reviewer`
/// block, the whole file was refused, and
/// `a_repo_with_no_plan_driver_block_is_byte_for_byte_unchanged` and
/// `every_plan_tool_is_orchestrator_only_and_off_by_default` passed against a
/// file the engine never read. Each fixture now varies exactly ONE thing from
/// [`WORKFLOW`], and this test is what keeps that true.
#[test]
fn every_fixture_workflow_parses() {
    for (name, yaml) in [
        ("WORKFLOW", WORKFLOW),
        ("WORKFLOW_NO_PLAN_DRIVER", WORKFLOW_NO_PLAN_DRIVER),
        ("WORKFLOW_NO_PLANNER", WORKFLOW_NO_PLANNER),
        ("WORKFLOW_SHORT_TIMEOUT", WORKFLOW_SHORT_TIMEOUT),
    ] {
        let parsed = loomux_lib::orchestration::workflow::parse_workflow(yaml);
        assert!(
            parsed.is_ok(),
            "{name} does not parse, so every 'the driver is off' assertion measured against it \
             is vacuous: {:?}",
            parsed.err()
        );
    }

    // The three fixtures differ from each other in exactly the axes they are
    // named for, and nothing else — the population control for the sentence
    // above. A fixture that quietly grew a second difference would make its
    // test a statement about two things at once.
    let full = loomux_lib::orchestration::workflow::parse_workflow(WORKFLOW).unwrap();
    let no_plan = loomux_lib::orchestration::workflow::parse_workflow(WORKFLOW_NO_PLAN_DRIVER)
        .unwrap();
    let no_planner =
        loomux_lib::orchestration::workflow::parse_workflow(WORKFLOW_NO_PLANNER).unwrap();

    assert!(full.driver.plan_enabled, "WORKFLOW is the one with the plan driver ON");
    assert!(!no_plan.driver.plan_enabled, "WORKFLOW_NO_PLAN_DRIVER varies exactly that switch");
    assert!(
        no_plan.driver.enabled,
        "…and nothing else: the REVIEW driver is still on, so a test using it is about \
         plan_enabled rather than about the driver block being absent"
    );
    assert!(
        full.blocks.iter().any(|b| b.kind == Role::Planner),
        "WORKFLOW has a planner block"
    );
    assert!(
        !no_planner.blocks.iter().any(|b| b.kind == Role::Planner),
        "WORKFLOW_NO_PLANNER varies exactly that block"
    );
    assert!(
        no_planner.driver.plan_enabled,
        "…and nothing else: the plan driver is still ON, so `no-planner-block` is what that \
         test can possibly be measuring"
    );

    let short =
        loomux_lib::orchestration::workflow::parse_workflow(WORKFLOW_SHORT_TIMEOUT).unwrap();
    assert_eq!(
        short.driver.drive_timeout_minutes, 5,
        "WORKFLOW_SHORT_TIMEOUT varies exactly the whole-drive backstop"
    );
    assert!(
        short.driver.plan_enabled && short.blocks.iter().any(|b| b.kind == Role::Planner),
        "…and nothing else: the plan driver and its planner block are still there, so a test \
         using it is about the TIMEOUT rather than about the driver being off"
    );
}

fn rails() -> Guardrails {
    Guardrails {
        max_agents: 8,
        agent_cli: "claude".into(),
        auto_ops: false,
        advanced_orchestrator: true,
        ..Guardrails::default()
    }
}

fn test_registry() -> (OrchRegistry, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let reg = OrchRegistry::new(dir.path().to_path_buf());
    reg.set_port(45998);
    reg.set_claude_agents_dir_override(dir.path().join("claude-agents"));
    reg.set_copilot_agents_dir_override(dir.path().join("copilot-agents"));
    reg.set_compact_hook_dir_override(dir.path().join("compacthook"));
    reg.set_copilot_hooks_dir_override(dir.path().join("copilot-hooks"));
    (reg, dir)
}

/// **The premise of this file's #464 allowlist row**, checked here rather than
/// asserted there (`tests/orchestration.rs`,
/// `only_the_sanctioned_helpers_construct_a_registry`).
///
/// That row permits exactly one raw `OrchRegistry::new` in this file, on the
/// stated grounds that it is [`test_registry`] and that [`test_registry`]
/// redirects every generated-agent-file destination away from the real
/// `~/.claude` / `~/.copilot`. A helper that quietly stopped applying one of
/// those overrides would leave the row true about the COUNT and false about the
/// property, and nothing over there could tell.
#[test]
fn its_registry_helper_applies_every_override_this_allowlist_row_assumes() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/plandrive.rs"),
    )
    .expect("this file reads itself");

    // The helper's body: from its signature to the first line that closes it at
    // column 0 — narrow enough that an override applied by some OTHER function
    // in this file cannot satisfy the assertions below.
    let start = src
        .find("fn test_registry() -> (OrchRegistry, tempfile::TempDir) {")
        .expect("the sanctioned helper must exist, under the name the row names");
    let body = &src[start..];
    let end = body.find("\n}").expect("the helper must terminate") + 2;
    let body = &body[..end];

    for needed in [
        "set_claude_agents_dir_override",
        "set_copilot_agents_dir_override",
        "set_compact_hook_dir_override",
        "set_copilot_hooks_dir_override",
    ] {
        assert!(
            body.contains(needed),
            "the #464 allowlist row for tests/plandrive.rs assumes this helper applies every \
             override; it no longer applies {needed}, so a registry built through it can reach \
             the real agent dirs and the row's premise is gone"
        );
    }

    // The population control: the extraction really did isolate the helper, so
    // the four assertions above are about ITS body and not about the whole file
    // — which contains those same names in prose.
    assert!(
        body.len() < 1_200,
        "the helper's body extraction ran away ({} chars); the assertions above would then be \
         satisfied by any other function in this file",
        body.len()
    );
    assert!(
        !body.contains("#[test]"),
        "the extraction swallowed a test, so it is no longer reading only the helper"
    );
}

/// The canned `gh`, keyed on WHAT was asked rather than on call order, with the
/// call log a test asserts the "no round trip for the plan" claim against.
struct FakeGh {
    state: std::sync::Mutex<String>,
    labels: std::sync::Mutex<Vec<String>>,
    title: std::sync::Mutex<String>,
    /// The seam itself failing — `gh` missing, or a child killed at the command
    /// timeout. Not a `gh` refusal, and not a fact about the issue.
    down: std::sync::Mutex<bool>,
    /// PR number -> (state, mergedAt). What `gh pr view` answers, keyed on the
    /// number rather than on call order so a test states what the driver
    /// concluded and not the sequence it happened to read in.
    prs: std::sync::Mutex<std::collections::BTreeMap<u64, (String, Option<String>)>>,
    /// Branch -> PR number, for the `gh pr list --head` fallback.
    heads: std::sync::Mutex<std::collections::BTreeMap<String, u64>>,
    calls: std::sync::Mutex<Vec<Vec<String>>>,
    /// Set by `hold_first`: the first N callers of `gh` wait here for each
    /// other, so a test can FORCE an interleaving instead of hoping for one.
    barrier: std::sync::Mutex<Option<std::sync::Arc<std::sync::Barrier>>>,
    /// How many callers the barrier actually held — the positive control.
    held: std::sync::atomic::AtomicUsize,
}

impl FakeGh {
    fn open(labels: &[&str]) -> FakeGh {
        FakeGh {
            state: std::sync::Mutex::new("OPEN".into()),
            labels: std::sync::Mutex::new(labels.iter().map(|s| s.to_string()).collect()),
            title: std::sync::Mutex::new("plan me".into()),
            down: std::sync::Mutex::new(false),
            prs: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            heads: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            calls: std::sync::Mutex::new(Vec::new()),
            barrier: std::sync::Mutex::new(None),
            held: std::sync::atomic::AtomicUsize::new(0),
        }
    }
    fn set_pr(&self, pr: u64, state: &str, merged_at: Option<&str>) {
        self.prs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(pr, (state.to_string(), merged_at.map(str::to_string)));
    }
    fn set_head_pr(&self, branch: &str, pr: u64) {
        self.heads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(branch.to_string(), pr);
    }
    /// How many `gh pr view` calls this fake has answered — the population
    /// figure a "the budget bounded it" assertion is measured against.
    fn pr_views(&self) -> usize {
        self.calls()
            .iter()
            .filter(|a| a.first().map(String::as_str) == Some("pr")
                && a.get(1).map(String::as_str) == Some("view"))
            .count()
    }
    fn set_state(&self, s: &str) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = s.to_string();
    }
    fn set_labels(&self, labels: &[&str]) {
        *self.labels.lock().unwrap_or_else(|e| e.into_inner()) =
            labels.iter().map(|s| s.to_string()).collect();
    }
    fn set_down(&self, down: bool) {
        *self.down.lock().unwrap_or_else(|e| e.into_inner()) = down;
    }
    /// Hold the first `n` callers of [`gh`](RdRunner::gh) together on a
    /// barrier, so a test can force an interleaving rather than hope for one.
    ///
    /// The issue read is the point both orderings of `drive_plan_with` pass
    /// through, and it sits after the pre-fix unlocked `already-driven` check
    /// and before the reservation — which is what makes it the one place a
    /// concurrency pin can stand.
    ///
    /// **A `Barrier` is REUSABLE**, so a test that arms this and then makes a
    /// number of `gh` calls that is not a multiple of `n` parks the leftovers
    /// forever. The one caller today makes exactly two, one per
    /// `drive_plan_with`. Arm it per test, not per fixture, and count the calls
    /// the test will really make.
    fn hold_first(&self, n: usize) {
        *self.barrier.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(std::sync::Arc::new(std::sync::Barrier::new(n)));
    }

    /// How many callers the barrier actually held — the positive control for
    /// any test that uses it. A barrier that was never reached leaves this at
    /// zero, and the test is then measuring a sequential run.
    fn held(&self) -> usize {
        self.held.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    /// How many `gh issue view` calls this fake has answered — the population
    /// figure every "no gh round trip" assertion below is measured against, so
    /// that an absence is read off a log that is demonstrably non-empty.
    fn issue_views(&self) -> usize {
        self.calls().iter().filter(|a| a.iter().any(|s| s == "issue")).count()
    }
}

impl RdRunner for FakeGh {
    fn gh(&self, args: &[&str]) -> Result<CmdOut, String> {
        // Owned up front, because the subcommand match below reads them after
        // the call log has taken its copy.
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).push(args.clone());
        // Taken and RELEASED before waiting: holding the barrier's own lock
        // across the wait would deadlock every caller on the second one.
        let barrier = self.barrier.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(b) = barrier {
            self.held.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            b.wait();
        }
        if *self.down.lock().unwrap_or_else(|e| e.into_inner()) {
            return Err("gh-not-found".into());
        }
        // Keyed on the SUBCOMMAND rather than on call order, so a test asserts
        // what the driver concluded and not the sequence it happened to read in.
        // Everything below the match is the `issue view` answer, which is the
        // shape P3a modelled and still the only other one this driver makes.
        match (args.first().map(String::as_str), args.get(1).map(String::as_str)) {
            (Some("pr"), Some("view")) => {
                let n: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
                let prs = self.prs.lock().unwrap_or_else(|e| e.into_inner());
                let Some((state, merged)) = prs.get(&n) else {
                    return Ok(CmdOut {
                        code: Some(1),
                        stdout: String::new(),
                        stderr: format!("no pull request found for {n}"),
                    });
                };
                let body = json!({ "state": state, "mergedAt": merged });
                return Ok(CmdOut { code: Some(0), stdout: body.to_string(), stderr: String::new() });
            }
            (Some("pr"), Some("list")) => {
                let head = args
                    .iter()
                    .position(|a| a == "--head")
                    .and_then(|i| args.get(i + 1))
                    .cloned()
                    .unwrap_or_default();
                let rows: Vec<Value> = self
                    .heads
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&head)
                    .map(|n| vec![json!({ "number": n })])
                    .unwrap_or_default();
                let body = Value::Array(rows);
                return Ok(CmdOut { code: Some(0), stdout: body.to_string(), stderr: String::new() });
            }
            _ => {}
        }
        let labels: Vec<Value> = self
            .labels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|l| json!({ "name": l }))
            .collect();
        let body = json!({
            "state": *self.state.lock().unwrap_or_else(|e| e.into_inner()),
            "labels": labels,
            "title": *self.title.lock().unwrap_or_else(|e| e.into_inner()),
        });
        Ok(CmdOut { code: Some(0), stdout: body.to_string(), stderr: String::new() })
    }
}

/// A registry with a group and an orchestrator pane, ready to drive.
fn grouped(reg: &OrchRegistry, repo: &Repo) -> (GroupId, String) {
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;
    let orch = reg
        .spawn_agent(&group, Role::Orchestrator, "orch", "", false, None)
        .expect("an orchestrator to deliver notices to");
    // A delivery needs a pane; in test mode nothing binds one, so an
    // orchestrator spawned and left alone silently receives nothing and a test
    // asserting a notice reads an empty audit log rather than a missing feature.
    reg.set_pty_for_test(&orch.id, 7);
    (group, orch.id)
}

/// A live drive on #3040, started through the real `drive_plan`.
fn driven(reg: &OrchRegistry, repo: &Repo, gh: &FakeGh) -> (GroupId, String, String) {
    let (group, orch) = grouped(reg, repo);
    let out = reg.drive_plan_with(&group, gh, 3040, None, None, None, &orch, 1_000);
    assert_eq!(out["driving"], json!(true), "drive_plan refused: {out}");
    let planner = out["planner"].as_str().expect("a planner pane").to_string();
    (group, orch, planner)
}

fn status(reg: &OrchRegistry, group: &GroupId) -> Value {
    reg.plan_drive_status_with(group, 2_000)
}

fn drive_state(reg: &OrchRegistry, group: &GroupId) -> String {
    status(reg, group)["drives"][0]["state"].as_str().unwrap_or_default().to_string()
}

fn held_reason(reg: &OrchRegistry, group: &GroupId) -> String {
    status(reg, group)["drives"][0]["held_reason"].as_str().unwrap_or_default().to_string()
}

fn audit_actions(reg: &OrchRegistry, group: &GroupId) -> Vec<String> {
    reg.audit_log(group).into_iter().map(|e| e.action).collect()
}

fn caller(group: &GroupId, agent: &str, role: Role) -> Caller {
    Caller { agent_id: agent.to_string(), group: group.clone(), role, role_hint: None }
}

// ── §2(d): consent is the label ─────────────────────────────────────────────

/// **A drive is refused on an issue nobody labelled**, and the label is the
/// whole of the human's consent.
///
/// The positive control is the same call one line later with the label ON: the
/// refusal is a statement about the label rather than about this fixture being
/// undriveable, which is the difference between a pin and a test that would
/// pass against a `drive_plan` that refused everything.
#[test]
fn a_drive_on_an_unlabelled_issue_is_refused() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["bug", "windows"]);
    let (group, orch) = grouped(&reg, &repo);

    let out = reg.drive_plan_with(&group, &gh, 3040, None, None, None, &orch, 1_000);
    assert_eq!(
        out["refused"],
        json!(plandrive::refusal::NOT_LABELLED),
        "an unlabelled issue must be refused: {out}"
    );
    assert!(
        status(&reg, &group)["drives"].as_array().is_none_or(|d| d.is_empty()),
        "a refused drive leaves no entry: {}",
        status(&reg, &group)
    );

    // The control.
    gh.set_labels(&["bug", "agent-ready"]);
    let out = reg.drive_plan_with(&group, &gh, 3040, None, None, None, &orch, 1_000);
    assert_eq!(out["driving"], json!(true), "the SAME call with the label must drive: {out}");
    assert_eq!(out["consent"], json!("agent-ready"));
}

/// A closed issue, and an issue orrerix could not READ, are different answers —
/// unknown is never treated as safe.
#[test]
fn a_closed_issue_and_an_unreadable_one_are_different_refusals() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, orch) = grouped(&reg, &repo);

    gh.set_state("CLOSED");
    let out = reg.drive_plan_with(&group, &gh, 3040, None, None, None, &orch, 1_000);
    assert_eq!(out["refused"], json!(plandrive::refusal::ISSUE_NOT_OPEN), "{out}");

    gh.set_state("OPEN");
    gh.set_down(true);
    let out = reg.drive_plan_with(&group, &gh, 3040, None, None, None, &orch, 1_000);
    assert_eq!(
        out["refused"],
        json!(plandrive::refusal::ISSUE_UNVERIFIABLE),
        "a seam failure is NOT `issue-not-open`: {out}"
    );

    // The control: with the seam back, the same call drives.
    gh.set_down(false);
    let out = reg.drive_plan_with(&group, &gh, 3040, None, None, None, &orch, 1_000);
    assert_eq!(out["driving"], json!(true), "{out}");
}

/// **`agent-investigation` never boards.** The plan is the whole deliverable,
/// and the drive completes at `plan-posted` rather than walking on into
/// `boarding`.
///
/// The positive control is the `agent-ready` half of the same test: the two
/// drives are identical but for the label, and they end in different states, so
/// "never boards" is a statement about the label rather than about a build that
/// cannot board at all.
#[test]
fn an_investigation_issue_never_boards() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-investigation"]);
    let (group, orch) = grouped(&reg, &repo);

    let out = reg.drive_plan_with(&group, &gh, 3040, None, None, None, &orch, 1_000);
    assert_eq!(out["consent"], json!("agent-investigation"), "{out}");
    let planner = out["planner"].as_str().unwrap().to_string();

    let doc = plandrive::validate_for_drive(&in_comment(PLAN), 3040, &roster())
        .expect("the specimen plan must validate");
    reg.pd_store_posted_plan_at(&group, 3040, doc, "https://example/c/1", 1_100);
    assert_eq!(drive_state(&reg, &group), "plan-posted");

    let report = reg.pd_drive_group_with(&group, &gh, 1_200);
    assert_eq!(report.advanced, vec![3040], "the tick must move an investigation drive");
    // Terminal, so `plan_drive_status` no longer lists it — the absence is read
    // beside the record itself, which still holds the entry.
    let file = read_record(&reg, &group);
    assert_eq!(file["entries"][0]["state"], json!("complete"));
    assert_eq!(
        file["entries"][0]["slices"],
        json!({}),
        "an investigation drive boards no row at all"
    );
    assert!(!planner.is_empty());

    // The control: the same plan under `agent-ready` reaches `boarding`.
    let repo2 = Repo::new();
    let (reg2, _d2) = test_registry();
    let gh2 = FakeGh::open(&["agent-ready"]);
    let (g2, orch2) = grouped(&reg2, &repo2);
    let out = reg2.drive_plan_with(&g2, &gh2, 3040, None, None, None, &orch2, 1_000);
    assert_eq!(out["driving"], json!(true), "{out}");
    let doc = plandrive::validate_for_drive(&in_comment(PLAN), 3040, &roster()).unwrap();
    reg2.pd_store_posted_plan_at(&g2, 3040, doc, "https://example/c/1", 1_100);
    // TWO ticks, and the second one is the point: `decide` answers ONE step per
    // entry per tick — the review driver's own shape — so `plan-posted` →
    // `boarding` and `boarding` → `running` are two of them. The intermediate
    // state is real and a status read can catch it.
    reg2.pd_drive_group_with(&g2, &gh2, 1_200);
    assert_eq!(
        drive_state(&reg2, &g2),
        "boarding",
        "the first tick reaches boarding: {}",
        status(&reg2, &g2)
    );
    reg2.pd_drive_group_with(&g2, &gh2, 1_300);
    assert_eq!(
        drive_state(&reg2, &g2),
        "running",
        "an agent-ready drive boards and RUNS, where the investigation one completed: {}",
        status(&reg2, &g2)
    );
    let _ = orch2;
}

// ── §2(b) step 3: the post-time plan hook ───────────────────────────────────

/// This group's roster, as `validate_for_drive` reads it.
fn roster() -> Vec<(String, bool)> {
    vec![
        ("orch".to_string(), false),
        ("plan-lead".to_string(), false),
        ("worker-adv".to_string(), true),
        ("rev-std".to_string(), false),
    ]
}

/// **A valid block is stored at post time with no `gh` round trip for the
/// plan** — the whole reason the hook exists rather than a `gh issue view`
/// after the fact.
///
/// The absence is measured against a POSITIVE CONTROL: the call log is
/// demonstrably non-empty (the drive's own consent read is in it) and its
/// `issue view` count does not move across the store. An `is_empty()` on the
/// log would pass just as well against a fake nobody ever called.
#[test]
fn a_valid_block_is_stored_at_post_time_without_a_gh_call() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, planner) = driven(&reg, &repo, &gh);

    // The control: the drive HAS spent `gh` calls, so the log is not empty and
    // the figure below is a real before/after rather than 0 == 0.
    let before = gh.issue_views();
    assert!(before > 0, "the drive's own consent read must be in the log: {:?}", gh.calls());

    let check = reg.pd_plan_check_at(&group, &planner, 3040, &in_comment(PLAN), 1_100);
    let doc = match check {
        PdPlanCheck::Valid(d) => *d,
        PdPlanCheck::Invalid(reasons) => panic!("the specimen plan was refused: {reasons:?}"),
        PdPlanCheck::NotDriven => panic!("the drive's own planner must be recognised"),
    };
    assert_eq!(doc.slices.len(), 2);
    reg.pd_store_posted_plan_at(&group, 3040, doc, "https://example/c/9", 1_100);

    assert_eq!(
        gh.issue_views(),
        before,
        "the plan reached orrerix in the tool payload; nothing may re-read the issue for it: {:?}",
        gh.calls()
    );
    let s = status(&reg, &group);
    assert_eq!(s["drives"][0]["state"], json!("plan-posted"), "{s}");
    assert_eq!(s["drives"][0]["comment_url"], json!("https://example/c/9"));
    assert_eq!(s["drives"][0]["slices"][0]["id"], json!("P1"));
    assert_eq!(s["drives"][0]["slices"][1]["deps"][0], json!("P1"));
    assert_eq!(
        s["drives"][0]["slices"][0]["task_id"],
        Value::Null,
        "P3a boards nothing, so no slice carries a row id"
    );
    assert!(audit_actions(&reg, &group)
        .contains(&plandrive::audit_action::PLAN_POSTED.to_string()));
}

/// **An invalid block is refused with reasons and nothing is stored.**
///
/// Three separate faults, each of a different class, because "invalid" is not
/// one thing: a comment with no block at all, a block for another issue, and a
/// block naming a roster block that is not a worker (§2(d)'s refusal).
#[test]
fn an_invalid_block_is_refused_and_nothing_is_stored() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, planner) = driven(&reg, &repo, &gh);

    let refused = |body: &str, at: u64| -> Vec<String> {
        match reg.pd_plan_check_at(&group, &planner, 3040, body, at) {
            PdPlanCheck::Invalid(r) => r,
            PdPlanCheck::Valid(_) => panic!("this body must be refused: {body}"),
            PdPlanCheck::NotDriven => panic!("the drive's own planner must be recognised"),
        }
    };

    let r = refused("Just some prose about the issue.\n", 1_100);
    assert!(
        r.iter().any(|m| m.contains("orrerix-plan")),
        "a comment with no block names the fence it needed: {r:?}"
    );

    let r = refused(&in_comment(&PLAN.replace("issue: 3040", "issue: 2850")), 1_200);
    assert!(
        r.iter().any(|m| m.contains("#3040")),
        "a plan for another issue is refused, never retargeted: {r:?}"
    );

    // The state is unchanged by every refusal: nothing stored, still `planning`.
    assert_eq!(drive_state(&reg, &group), "planning");
    assert_eq!(
        status(&reg, &group)["drives"][0]["comment_url"],
        json!(""),
        "a refused post stores no comment url"
    );
    assert!(audit_actions(&reg, &group)
        .contains(&plandrive::audit_action::PLAN_INVALID.to_string()));
    // The control, and it is the one that makes every assertion above a
    // statement about the BODIES rather than about a hook that refuses
    // everything: a well-formed block on the same drive is accepted.
    assert!(matches!(
        reg.pd_plan_check_at(&group, &planner, 3040, &in_comment(PLAN), 1_400),
        PdPlanCheck::Valid(_)
    ));
}

/// **A slice naming a non-worker block is refused at post time** (§2(d)) — a
/// plan cannot have the drive spawn a capability class it invented for itself.
#[test]
fn a_slice_naming_a_reviewer_block_is_refused() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, planner) = driven(&reg, &repo, &gh);

    let body = in_comment(&PLAN.replacen("block: worker-adv", "block: rev-std", 1));
    match reg.pd_plan_check_at(&group, &planner, 3040, &body, 1_100) {
        PdPlanCheck::Invalid(r) => assert!(
            r.iter().any(|m| m.contains("rev-std") && m.contains("not a worker")),
            "the refusal names the block and why: {r:?}"
        ),
        other => panic!(
            "a reviewer block must be refused: {}",
            match other {
                PdPlanCheck::Valid(_) => "accepted",
                _ => "not driven",
            }
        ),
    }

    // A block that is not in the roster at all is its own refusal, and the
    // roster it is judged against is the one PINNED AT LAUNCH.
    let body = in_comment(&PLAN.replacen("block: worker-adv", "block: no-such-block", 1));
    match reg.pd_plan_check_at(&group, &planner, 3040, &body, 1_200) {
        PdPlanCheck::Invalid(r) => assert!(
            r.iter().any(|m| m.contains("no-such-block") && m.contains("roster")),
            "{r:?}"
        ),
        _ => panic!("a block outside the roster must be refused"),
    }
}

/// **The third refused block parks the drive**, carrying the last reasons.
///
/// The first two refusals leave it `planning` — the bound is a bound, not a
/// hair trigger — which is the control that makes the third assertion a
/// statement about the COUNT.
#[test]
fn the_third_invalid_block_holds_the_drive() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, planner) = driven(&reg, &repo, &gh);

    for (n, at) in [(1u32, 1_100u64), (2, 1_200)] {
        let _ = reg.pd_plan_check_at(&group, &planner, 3040, "no block here\n", at);
        assert_eq!(
            drive_state(&reg, &group),
            "planning",
            "refusal {n} must not park the drive: {}",
            status(&reg, &group)
        );
        assert_eq!(status(&reg, &group)["drives"][0]["invalid_count"], json!(n));
    }

    let _ = reg.pd_plan_check_at(&group, &planner, 3040, "still no block\n", 1_300);
    assert_eq!(drive_state(&reg, &group), "held");
    assert_eq!(held_reason(&reg, &group), PdHeldReason::PlanInvalid.as_str());
    let last = status(&reg, &group)["drives"][0]["last_invalid"].clone();
    assert!(
        last.as_array().is_some_and(|a| !a.is_empty()),
        "the hold carries the last reasons, not merely the fact: {last}"
    );

    // A parked drive owns nobody: the planner's next post goes through as an
    // ordinary comment, which is what makes a hold a hand-back rather than a
    // quieter drive.
    assert!(matches!(
        reg.pd_plan_check_at(&group, &planner, 3040, &in_comment(PLAN), 1_400),
        PdPlanCheck::NotDriven
    ));

    // And a resume gives it a fresh three rather than resuming onto the bound.
    let out = reg.resume_plan_drive_with(&group, 3040, "orch-1", 1_500);
    assert_eq!(out["resumed"], json!(true), "{out}");
    assert_eq!(drive_state(&reg, &group), "planning", "a resume returns to where the hold came from");
    assert_eq!(status(&reg, &group)["drives"][0]["invalid_count"], json!(0));
}

/// **The hook, through the real tool: an invalid block is refused and NOTHING
/// is posted** (§2(b) step 3).
///
/// The two halves are one test so neither can pass vacuously, and the control is
/// the sharp one: "nothing was posted" is an absence, and what makes it a
/// statement rather than a coincidence is that the SAME tool call from a planner
/// this drive does not own DOES reach `gh`. No `gh` fake is needed to see that —
/// the registry audits an `issue-comment` row on the FAILURE path too, so that
/// row's presence is the positive control for having reached the child, and its
/// absence is the guarantee.
#[test]
fn an_invalid_block_is_refused_in_the_tool_and_nothing_is_posted() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, planner) = driven(&reg, &repo, &gh);

    let post = |agent: &str, body: &str| -> Result<String, String> {
        let c = caller(&group, agent, Role::Planner);
        let r = dispatch(
            &reg,
            &c,
            "tools/call",
            &json!({ "name": "post_issue_comment",
                     "arguments": { "issue": 3040, "body": body } }),
        )
        .expect("dispatch itself must not fail");
        let text = r["content"][0]["text"].as_str().unwrap_or_default().to_string();
        if r["isError"] == json!(true) {
            Err(text)
        } else {
            Ok(text)
        }
    };
    let comment_rows = || -> usize {
        reg.audit_log(&group).into_iter().filter(|e| e.action == "issue-comment").count()
    };

    // (a) the drive's own planner, with no block in the body.
    let err = post(&planner, "Some prose, and no plan block at all.\n")
        .expect_err("a driven planner's invalid post must be refused");
    assert!(
        err.contains("NOT posted") && err.contains("orrerix-plan"),
        "the refusal says nothing was posted and names what was wrong: {err}"
    );
    assert_eq!(
        comment_rows(),
        0,
        "a refused post must not reach `gh` at all — there is no path from that arm to a child"
    );
    assert!(
        audit_actions(&reg, &group).contains(&plandrive::audit_action::PLAN_INVALID.to_string()),
        "…and the refusal is on the record: {:?}",
        audit_actions(&reg, &group)
    );

    // (b) THE CONTROL. A planner in this group that this drive does not own
    // takes the ordinary path, reaches `gh`, and leaves the row that proves it.
    // Without this, (a)'s zero would be indistinguishable from a tool that never
    // posts for anybody.
    let other = reg
        .spawn_agent(&group, Role::Planner, "p2", "", false, None)
        .expect("a second planner");
    assert_eq!(reg.pd_owner(&group, &other.id), None, "…and it really is unowned");
    let _ = post(&other.id, "An ordinary comment with no plan block in it.\n");
    assert_eq!(
        comment_rows(),
        1,
        "an unowned planner's post takes the ordinary path — the hook is invisible to it, which \
         is what keeps #2815 unregressed"
    );
}

/// **Two CONCURRENT `drive_plan` calls on one issue open exactly ONE planner**
/// (rev-std round 1, finding 5).
///
/// # Why this test is threaded, and why the obvious version was worthless
///
/// The first draft called `drive_plan_with` twice in a row and asserted one
/// pane. It passed against the defect. Sequentially there is no race at all:
/// call 1 completes its store before call 2 starts, so call 2's unlocked
/// `already-driven` read sees the entry and refuses before spawning anything.
/// A red-before-green run on the pre-fix head proved exactly that — the pin
/// was green on the code it was written to catch, which is a test that pins
/// nothing while looking like it pins the fix.
///
/// The defect only exists when both calls pass the unlocked check before either
/// writes. So the interleaving is FORCED rather than hoped for: the fake `gh`
/// holds the first two callers on a barrier inside the issue read, which is a
/// point both orderings pass through and — crucially — sits on the far side of
/// the pre-fix `already-driven` check and on the NEAR side of the reservation.
/// Both threads are therefore inside `drive_plan_with`, past the unlocked read,
/// when they are released.
///
/// - **Pre-fix**: both are past the check, both spawn, and the loser's pane is
///   left running, unowned and unaudited — free to publish a second plan
///   comment on the issue.
/// - **Fixed**: both reach the reservation, one wins under the lock, the loser
///   refuses `already-driven` having opened nothing.
///
/// The pane COUNT is the assertion. The refusal is not: a refusal was already
/// returned before the fix, so asserting it alone is what made the first draft
/// pass.
#[test]
fn two_concurrent_drives_on_one_issue_open_no_second_planner() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    // Hold the first two `gh` callers together, so both are inside
    // `drive_plan_with` and past its unlocked read when either proceeds.
    gh.hold_first(2);
    let (group, orch) = grouped(&reg, &repo);

    let planners = || -> Vec<String> {
        reg.list_agents(&group)
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter(|a| a["role"] == json!("planner"))
                    .filter_map(|a| a["id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    assert_eq!(planners().len(), 0, "no planner exists before either call");

    let outs: Vec<Value> = std::thread::scope(|s| {
        let a = s.spawn(|| reg.drive_plan_with(&group, &gh, 3040, None, None, None, &orch, 1_000));
        let b = s.spawn(|| reg.drive_plan_with(&group, &gh, 3040, None, None, None, &orch, 1_000));
        vec![a.join().unwrap(), b.join().unwrap()]
    });

    // The barrier really did hold both — the positive control for the whole
    // test. Without it the two calls could have run end to end and this would
    // be the sequential test again, which passes against the defect.
    assert!(
        gh.held() >= 2,
        "both calls must have been inside the issue read together, or this is not the race: \
         held={}",
        gh.held()
    );

    // Exactly one drove and exactly one refused — whichever won.
    let driving = outs.iter().filter(|o| o["driving"] == json!(true)).count();
    let refused = outs.iter().filter(|o| !o["refused"].is_null()).count();
    assert_eq!((driving, refused), (1, 1), "one winner, one refusal: {outs:?}");
    assert!(
        outs.iter().any(|o| o["refused"] == json!(plandrive::refusal::ALREADY_DRIVEN)),
        "and the loser's refusal names the reason: {outs:?}"
    );

    // THE assertion.
    let panes = planners();
    assert_eq!(
        panes.len(),
        1,
        "the losing call must open no pane at all — an unowned planner is free to publish a \
         second plan comment on the issue: {panes:?}"
    );
    // The one pane that exists is the winner's, and it is OWNED.
    let winner = outs
        .iter()
        .find(|o| o["driving"] == json!(true))
        .and_then(|o| o["planner"].as_str())
        .expect("the winner names its planner");
    assert_eq!(panes[0], winner);
    assert_eq!(reg.pd_owner(&group, &panes[0]), Some(3040));

    // One entry, so the reservation left no duplicate behind.
    let file = read_record(&reg, &group);
    assert_eq!(file["entries"].as_array().map(Vec::len), Some(1), "{file}");
}
/// A spawn this group's cap refuses **rolls the reservation back**, so the issue
/// is driveable again rather than parked on a drive whose planner never opened.
///
/// The control is the retry: the same call succeeds once the cap has room, which
/// is what makes the first half a statement about the rollback rather than about
/// a `drive_plan` that refuses twice for its own reasons.
#[test]
fn a_refused_spawn_leaves_no_reservation_behind() {
    let repo = Repo::new();
    // ONE delegate slot. The orchestrator does not consume one — it is a
    // FIXTURE (`Role::is_fixture`), which is why an earlier draft of this
    // fixture used `max_agents: 2` and watched the planner spawn happily as the
    // second DELEGATE rather than being refused as the third agent.
    let (reg, _d) = test_registry();
    let group = reg
        .create_group(&repo.path(), Guardrails { max_agents: 1, ..rails() })
        .unwrap()
        .id;
    let orch = reg
        .spawn_agent(&group, Role::Orchestrator, "orch", "", false, None)
        .expect("the orchestrator fits");
    reg.set_pty_for_test(&orch.id, 7);
    // The one delegate slot, taken — so the planner spawn below is refused by
    // the group's own live-delegate cap rather than by anything this test
    // arranged specially.
    let filler = reg
        .spawn_agent(&group, Role::Worker, "filler", "", false, None)
        .expect("the filler fits");
    let gh = FakeGh::open(&["agent-ready"]);

    let out = reg.drive_plan_with(&group, &gh, 3040, None, None, None, &orch.id, 1_000);
    assert_eq!(
        out["refused"],
        json!(plandrive::refusal::PLANNER_UNSPAWNABLE),
        "the cap must refuse the planner: {out}"
    );
    // …and refused BY THE CAP. `planner-unspawnable` is ONE name for every way
    // a spawn can fail, so the assertion above says nothing about WHY — which
    // is the hole the first draft of this fixture fell through: it set
    // `max_agents: 2`, the orchestrator turned out to be exempt from the count
    // (`counts_against_max_agents`), and the planner spawned. That draft failed
    // loudly only because it failed on this line's predecessor; a fixture that
    // broke the spawn some OTHER way would have passed here with nothing about
    // the cap exercised at all.
    assert!(
        out["detail"]
            .as_str()
            .is_some_and(loomux_lib::orchestration::is_live_cap_refusal),
        "the refusal must be the live-delegate cap's own, not merely some spawn failure: {out}"
    );
    // The rollback: no entry survives, so nothing is parked and nothing refuses
    // a later attempt with `already-driven`.
    let s = reg.plan_drive_status_with(&group, 1_100);
    assert!(
        s["drives"].as_array().is_none_or(|d| d.is_empty()),
        "a refused spawn must leave no reservation: {s}"
    );
    assert!(!record_path(&reg, &group).exists() || read_record(&reg, &group)["entries"]
        .as_array()
        .is_none_or(|e| e.is_empty()));

    // THE CONTROL: with room, the identical call drives.
    //
    // The slot is freed by marking the filler DEAD rather than through
    // `kill_agent`, which cannot run here at all: it wants a terminal bound
    // (every agent in test mode is "still binding") and then a Tauri app handle
    // to end the process, and there is none. `mark_agent_dead_for_test` is
    // `tests/reviewdrive.rs`'s own seam for exactly this, and it moves the one
    // thing the cap reads — `AgentStatus::Dead` is what
    // `counts_against_max_agents` stops counting.
    assert!(
        reg.mark_agent_dead_for_test(&filler.id),
        "the filler must exist to be freed"
    );
    let out = reg.drive_plan_with(&group, &gh, 3040, None, None, None, &orch.id, 1_200);
    assert_eq!(out["driving"], json!(true), "the same call must drive once there is room: {out}");
}

/// **The restart reconcile reads NON-TERMINAL entries, not live ones** — the
/// figure `PD_MAX_GH_PER_TICK`'s doc and the design note both state.
///
/// A fixture where every entry is live cannot tell the two readings apart, which
/// is the non-discriminating shape a corrected claim is most likely to ship
/// with. Here one of the five is HELD, so `live` is 4 and `non_terminal` is 5
/// and the two predictions differ by exactly one round trip.
#[test]
fn the_reconcile_reads_held_entries_too() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, orch) = grouped(&reg, &repo);

    for issue in [1u64, 2, 3, 4, 5] {
        let out = reg.drive_plan_with(&group, &gh, issue, None, None, None, &orch, 1_000);
        assert_eq!(out["driving"], json!(true), "#{issue}: {out}");
    }
    // Park #5 — through the real refusal bound, so the hold is one the machine
    // produces rather than one a fixture wrote into the file.
    let planner = reg
        .plan_drive_status_with(&group, 1_010)["drives"]
        .as_array()
        .and_then(|d| d.iter().find(|e| e["issue"] == json!(5)))
        .and_then(|e| e["planner"].as_str())
        .expect("#5 has a planner")
        .to_string();
    for at in [1_010u64, 1_020, 1_030] {
        let _ = reg.pd_plan_check_at(&group, &planner, 5, "no block here\n", at);
    }
    let held: Vec<u64> = reg.plan_drive_status_with(&group, 1_040)["drives"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["state"] == json!("held"))
        .filter_map(|e| e["issue"].as_u64())
        .collect();
    assert_eq!(held, vec![5], "exactly one entry is parked, and it is #5");

    let before = gh.issue_views();
    reg.pd_drive_group_with(&group, &gh, 1_100);
    assert_eq!(
        gh.issue_views() - before,
        5 + 4,
        "the reconcile reads all FIVE non-terminal entries (the held one included, because a \
         restart must be able to learn its issue was closed), and the tick then services the \
         four live ones — a `live` reading would predict 4 + 4: {:?}",
        gh.calls()
    );
}

/// **A store that fails after the post succeeded leaves no false record**, and
/// the drive is recoverable rather than stuck (rev-std round 1, premortem).
///
/// The corner: a driven planner's block validates, `gh` posts it, and
/// `plan_drives.json` is torn at that instant. The plan is live on GitHub and
/// the record cannot be updated. What must NOT happen is a record claiming a
/// plan it does not have, or one claiming a state the plan is not in.
#[test]
fn a_store_that_fails_after_the_post_records_nothing_false() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, planner) = driven(&reg, &repo, &gh);

    let doc = plandrive::validate_for_drive(&in_comment(PLAN), 3040, &roster()).unwrap();
    // The record goes unreadable between the validate and the store — which is
    // exactly where `post_issue_comment` has already reached `gh`.
    std::fs::write(record_path(&reg, &group), "{ not json").unwrap();
    reg.pd_store_posted_plan_at(&group, 3040, doc, "https://example/c/9", 1_100);

    // Unrepaired, and no invented entry.
    assert_eq!(
        std::fs::read_to_string(record_path(&reg, &group)).unwrap(),
        "{ not json",
        "a store that could not read the record must not rewrite it"
    );
    // The tool answered success and the plan IS on the issue, so the honest
    // state is "orrerix cannot read its record" — never "no plan was posted".
    let s = reg.plan_drive_status_with(&group, 1_200);
    assert_eq!(s["refused"], json!(plandrive::refusal::STATE_UNREADABLE), "{s}");

    // And it is recoverable: with the record restored by hand, the drive is
    // still `planning` — the plan is on GitHub and the record never claimed
    // otherwise, so a human re-runs the planner or boards it themselves. The
    // control is that this same status call reads normally again, so the
    // assertion above was about the torn file rather than about a drive that
    // had gone missing.
    let mut fixed = plandrive::PlanDrivesState::default();
    let mut entry = plandrive::PdEntry::new(
        3040,
        "orch-1",
        "plan-lead",
        Consent::Ready,
        None,
        0,
        1_000,
    );
    entry.planner_agent = planner.clone();
    fixed.entries.push(entry);
    std::fs::write(
        record_path(&reg, &group),
        serde_json::to_string(&fixed).unwrap(),
    )
    .unwrap();
    assert_eq!(drive_state(&reg, &group), "planning");
}

// ── §2(b) step 4 / §2(e): the planner's own end ─────────────────────────────


/// **A planner that reports `done` without posting parks on `plan-missing`.**
///
/// Its own reason rather than waiting out `planner-stalled`: the pane is
/// finished, so nothing is going to produce a plan now, and a name that said
/// "stalled" would misdescribe it.
#[test]
fn a_planner_done_without_a_post_holds_plan_missing() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, planner) = driven(&reg, &repo, &gh);

    // Through the real MCP arm, so the interception is what is pinned rather
    // than a direct call to `pd_ingest`.
    let c = caller(&group, &planner, Role::Planner);
    let out = dispatch(
        &reg,
        &c,
        "tools/call",
        &json!({ "name": "report", "arguments": { "outcome": "done", "note": "posted" } }),
    );
    assert!(out.is_ok(), "the planner's report must be accepted: {out:?}");
    assert!(
        audit_actions(&reg, &group)
            .contains(&plandrive::audit_action::PLANNER_CONSUMED.to_string()),
        "a driven planner's report is CONSUMED and audited, never silently dropped: {:?}",
        audit_actions(&reg, &group)
    );

    reg.pd_drive_group_with(&group, &gh, 1_200);
    assert_eq!(held_reason(&reg, &group), PdHeldReason::PlanMissing.as_str());
}

/// **A planner that neither posts nor reports parks on `planner-stalled`**, and
/// not one tick before its timeout.
///
/// The before/after pair is the control: the same drive, the same facts, one
/// tick inside the bound and one past it.
#[test]
fn a_planner_stall_holds() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, _planner) = driven(&reg, &repo, &gh);

    // The planner was spawned at 1_000 and the timeout is 60 minutes.
    let inside = 1_000 + 59 * 60_000;
    reg.pd_drive_group_with(&group, &gh, inside);
    assert_eq!(
        drive_state(&reg, &group),
        "planning",
        "a drive inside its planner timeout is still planning"
    );

    let past = 1_000 + 61 * 60_000;
    reg.pd_drive_group_with(&group, &gh, past);
    assert_eq!(drive_state(&reg, &group), "held");
    assert_eq!(held_reason(&reg, &group), PdHeldReason::PlannerStalled.as_str());
}

/// **Withdrawing the label stops the drive**, because consent is re-read on
/// every tick rather than remembered from `drive_plan`.
#[test]
fn a_withdrawn_label_holds_the_drive() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, _planner) = driven(&reg, &repo, &gh);

    // The control: a tick with the label still on changes nothing.
    reg.pd_drive_group_with(&group, &gh, 1_100);
    assert_eq!(drive_state(&reg, &group), "planning");

    gh.set_labels(&["bug"]);
    reg.pd_drive_group_with(&group, &gh, 1_200);
    assert_eq!(held_reason(&reg, &group), PdHeldReason::ConsentWithdrawn.as_str());
}

/// **An unreadable label list is not a withdrawal.** A `gh` outage must not
/// park every live drive in the fleet.
#[test]
fn a_gh_outage_is_not_a_withdrawn_label() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, _planner) = driven(&reg, &repo, &gh);

    gh.set_down(true);
    reg.pd_drive_group_with(&group, &gh, 1_200);
    assert_eq!(
        drive_state(&reg, &group),
        "planning",
        "unknown is not a withdrawal: {}",
        status(&reg, &group)
    );

    // The control: with the labels really gone, the same tick holds.
    gh.set_down(false);
    gh.set_labels(&[]);
    reg.pd_drive_group_with(&group, &gh, 1_300);
    assert_eq!(held_reason(&reg, &group), PdHeldReason::ConsentWithdrawn.as_str());
}

// ── §2(c): cancel, and the reconcile ────────────────────────────────────────

/// **`cancel_plan_drive` kills nothing.** It releases ownership; the planner
/// pane keeps running under the orchestrator, and its traffic reaches that pane
/// again the moment the entry stops being live.
#[test]
fn cancel_kills_nothing() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, orch, planner) = driven(&reg, &repo, &gh);

    let out = reg.cancel_plan_drive_with(&group, 3040, &orch, 1_100);
    assert_eq!(out["cancelled"], json!(true), "{out}");
    assert_eq!(out["planner_left_running"], json!(planner));

    let alive = reg.agent(&planner).expect("the planner pane still exists");
    assert_ne!(
        alive.status,
        loomux_lib::orchestration::AgentStatus::Dead,
        "cancel releases ownership; it does not end a pane"
    );
    // And ownership really is released: the planner is nobody's delegate now.
    assert_eq!(reg.pd_owner(&group, &planner), None);
    assert!(matches!(
        reg.pd_plan_check_at(&group, &planner, 3040, &in_comment(PLAN), 1_200),
        PdPlanCheck::NotDriven
    ));
}

/// **A restart over a closed issue reconciles to `cancelled`**, with the notice
/// the drive owes for stopping.
#[test]
fn a_closed_issue_reconciles_to_cancelled() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, _planner) = driven(&reg, &repo, &gh);

    gh.set_state("CLOSED");
    reg.pd_drive_group_with(&group, &gh, 1_200);
    let file = read_record(&reg, &group);
    assert_eq!(file["entries"][0]["state"], json!("cancelled"), "{file}");
    // The notice is asserted on the AUDIT ROW rather than on the tick's return
    // value, and the difference is a real one: the once-per-process reconcile
    // runs FIRST, cancels the entry itself, and flushes its own notice — so
    // `report.notices` (which counts only the tick's own later flush) is
    // legitimately 0 on exactly the wake this test is about.
    assert!(
        audit_actions(&reg, &group).contains(&plandrive::audit_action::NOTICE.to_string()),
        "a drive that stops owes the orchestrator one notice: {:?}",
        audit_actions(&reg, &group)
    );
    assert!(
        audit_actions(&reg, &group).contains(&plandrive::audit_action::RECOVERED.to_string()),
        "and the reconcile is what recorded it: {:?}",
        audit_actions(&reg, &group)
    );
}

/// **A torn record refuses the tick loudly and repairs nothing.**
#[test]
fn an_unreadable_record_refuses_the_tick_and_is_never_repaired() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, _planner) = driven(&reg, &repo, &gh);

    let path = record_path(&reg, &group);
    std::fs::write(&path, "{ not json").unwrap();
    let report = reg.pd_drive_group_with(&group, &gh, 1_200);
    assert!(report.state_unreadable, "the tick must say it could not read the record");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{ not json",
        "the record is left exactly as it was — never repaired, never deleted"
    );
    assert!(audit_actions(&reg, &group)
        .contains(&plandrive::audit_action::STATE_UNREADABLE.to_string()));

    let s = status(&reg, &group);
    assert_eq!(
        s["refused"],
        json!(plandrive::refusal::STATE_UNREADABLE),
        "'orrerix cannot read the record' is NOT 'nothing is driven': {s}"
    );
}

// ── §5.1: the tools ─────────────────────────────────────────────────────────

/// **All four tools are orchestrator-only, and all four are invisible and
/// refused in a group whose repo has not turned the plan driver on.**
///
/// One test over the four names rather than four tests, because the property is
/// the same one four times and a per-tool test is a list someone has to
/// remember to extend.
#[test]
fn every_plan_tool_is_orchestrator_only_and_off_by_default() {
    const TOOLS: [(&str, Value); 4] = [
        ("drive_plan", Value::Null),
        ("plan_drive_status", Value::Null),
        ("cancel_plan_drive", Value::Null),
        ("resume_plan_drive", Value::Null),
    ];
    let args = |name: &str| -> Value {
        if name == "plan_drive_status" {
            json!({})
        } else {
            json!({ "issue": 3040 })
        }
    };

    // (a) the role gate.
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let (group, orch) = grouped(&reg, &repo);
    let worker = reg.spawn_agent(&group, Role::Worker, "w", "", false, None).unwrap();

    // A role denial is an MCP-level tool ERROR, not a transport one: dispatch
    // answers `Ok` with `isError: true` and the message as content. Reading it
    // as `Err` was wrong in the direction that matters — every one of these
    // four would have "passed" on a tool that answered normally.
    let denied = |v: &Value| -> bool {
        v["isError"] == json!(true)
            && v["content"][0]["text"]
                .as_str()
                .is_some_and(|t| t.contains("orchestrator-only"))
    };
    let mut refused_for_a_worker = 0;
    for (name, _) in TOOLS {
        let c = caller(&group, &worker.id, Role::Worker);
        let out = dispatch(&reg, &c, "tools/call", &json!({ "name": name, "arguments": args(name) }))
            .expect("dispatch itself must not fail");
        assert!(denied(&out), "{name} must refuse a worker: {out}");
        refused_for_a_worker += 1;
        // The control, and it is what makes the assertion above a statement
        // about the ROLE rather than about the arguments: the SAME call from the
        // orchestrator is not denied.
        let c = caller(&group, &orch, Role::Orchestrator);
        let out = dispatch(&reg, &c, "tools/call", &json!({ "name": name, "arguments": args(name) }))
            .expect("dispatch itself must not fail");
        assert!(!denied(&out), "{name} must reach the registry for an orchestrator: {out}");
    }
    assert_eq!(refused_for_a_worker, 4, "every one of the four was exercised");

    // (b) the repo gate: a group whose `driver:` block omits `plan_enabled`.
    let repo = Repo::with(WORKFLOW_NO_PLAN_DRIVER);
    let (reg, _d2) = test_registry();
    let (group, orch) = grouped(&reg, &repo);
    let gh = FakeGh::open(&["agent-ready"]);
    let out = reg.drive_plan_with(&group, &gh, 3040, None, None, None, &orch, 1_000);
    assert_eq!(out["refused"], json!(plandrive::refusal::DRIVER_DISABLED), "{out}");
    assert_eq!(
        gh.calls().len(),
        0,
        "a disabled driver spends no `gh` round trip at all: {:?}",
        gh.calls()
    );
    for name in ["drive_plan", "cancel_plan_drive", "resume_plan_drive"] {
        let out = match name {
            "cancel_plan_drive" => reg.cancel_plan_drive_with(&group, 3040, &orch, 1_000),
            "resume_plan_drive" => reg.resume_plan_drive_with(&group, 3040, &orch, 1_000),
            _ => reg.drive_plan_with(&group, &gh, 3040, None, None, None, &orch, 1_000),
        };
        assert_eq!(
            out["refused"],
            json!(plandrive::refusal::DRIVER_DISABLED),
            "{name}: {out}"
        );
    }
    assert_eq!(reg.plan_drive_status(&group)["enabled"], json!(false));
}

/// **A tool call cannot reach another group's drive.** The group is resolved
/// from the caller's own token and is never an argument, so the cross-group
/// attempt is what a second group's orchestrator naming the first's issue
/// looks like: it addresses ITS OWN group and finds nothing.
#[test]
fn a_plan_drive_is_invisible_to_another_groups_orchestrator() {
    let repo_a = Repo::new();
    let repo_b = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group_a, _orch_a, planner_a) = driven(&reg, &repo_a, &gh);
    let (group_b, orch_b) = grouped(&reg, &repo_b);

    // The control: A really is driving #3040.
    assert_eq!(drive_state(&reg, &group_a), "planning");
    assert_eq!(reg.pd_owner(&group_a, &planner_a), Some(3040));

    assert!(
        status(&reg, &group_b)["drives"].as_array().is_none_or(|d| d.is_empty()),
        "B's status shows A's drive: {}",
        status(&reg, &group_b)
    );
    assert_eq!(reg.pd_owner(&group_b, &planner_a), None, "A's planner is not B's delegate");
    let out = reg.cancel_plan_drive_with(&group_b, 3040, &orch_b, 1_100);
    assert_eq!(
        out["refused"],
        json!(plandrive::refusal::NOT_DRIVEN),
        "B cancelling #3040 must find nothing: {out}"
    );
    let out = reg.resume_plan_drive_with(&group_b, 3040, &orch_b, 1_100);
    assert_eq!(out["refused"], json!(plandrive::refusal::NOT_DRIVEN), "{out}");
    // And A's drive is untouched by all of it.
    assert_eq!(drive_state(&reg, &group_a), "planning");
}

/// A roster with no planner block refuses before it spends a `gh` call.
#[test]
fn a_roster_with_no_planner_block_is_refused_before_the_gh_call() {
    let repo = Repo::with(WORKFLOW_NO_PLANNER);
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, orch) = grouped(&reg, &repo);

    let out = reg.drive_plan_with(&group, &gh, 3040, None, None, None, &orch, 1_000);
    assert_eq!(out["refused"], json!(plandrive::refusal::NO_PLANNER_BLOCK), "{out}");
    assert_eq!(gh.calls().len(), 0, "no round trip is spent to learn nothing: {:?}", gh.calls());

    // Naming a block that exists but is not a planner is the same refusal, and
    // the control is that naming the real one drives.
    let repo = Repo::new();
    let (reg, _d2) = test_registry();
    let (group, orch) = grouped(&reg, &repo);
    let out =
        reg.drive_plan_with(&group, &gh, 3040, Some("worker-adv"), None, None, &orch, 1_000);
    assert_eq!(out["refused"], json!(plandrive::refusal::NO_PLANNER_BLOCK), "{out}");
    let out = reg.drive_plan_with(&group, &gh, 3040, Some("plan-lead"), None, None, &orch, 1_000);
    assert_eq!(out["driving"], json!(true), "{out}");
    assert_eq!(out["planner_block"], json!("plan-lead"));
}

/// `resume_plan_drive` on a LIVE drive is `not-held`, not `not-driven` — the
/// two want different things from the orchestrator.
#[test]
fn resuming_a_live_drive_is_not_held_and_not_not_driven() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, orch, _planner) = driven(&reg, &repo, &gh);

    let out = reg.resume_plan_drive_with(&group, 3040, &orch, 1_100);
    assert_eq!(out["refused"], json!(plandrive::refusal::NOT_HELD), "{out}");

    // The control: an issue with no entry at all answers the OTHER refusal.
    let out = reg.resume_plan_drive_with(&group, 9999, &orch, 1_100);
    assert_eq!(out["refused"], json!(plandrive::refusal::NOT_DRIVEN), "{out}");
}


// ── the tick's own bounds ───────────────────────────────────────────────────

/// **A repo with no plan driver is byte-for-byte unchanged**: the tick services
/// no group, spends no `gh` call, and writes no record.
#[test]
fn a_repo_with_no_plan_driver_block_is_byte_for_byte_unchanged() {
    let repo = Repo::with(WORKFLOW_NO_PLAN_DRIVER);
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch) = grouped(&reg, &repo);

    let report = reg.pd_drive_group_with(&group, &gh, 1_000);
    assert!(report.disabled, "the tick must decline a repo that did not ask for the driver");
    assert_eq!(gh.calls().len(), 0);
    assert!(!record_path(&reg, &group).exists(), "no record is created");

    // The control: the same tick on a repo that DID ask does read the issue —
    // so the zeros above are about the policy, not about a tick that never runs.
    let repo2 = Repo::new();
    let (reg2, _d2) = test_registry();
    let gh2 = FakeGh::open(&["agent-ready"]);
    let (g2, _o2, _p2) = driven(&reg2, &repo2, &gh2);
    let before = gh2.issue_views();
    reg2.pd_drive_group_with(&g2, &gh2, 1_100);
    assert!(gh2.issue_views() > before, "an enabled driver reads the issue every tick");
}

/// **One wake services at most [`PD_MAX_GH_PER_TICK`] drives**, and the ones it
/// could not reach are named rather than silently skipped.
#[test]
fn a_tick_services_at_most_four_drives() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, orch) = grouped(&reg, &repo);

    // Five live drives — one more than the bound.
    for issue in [1u64, 2, 3, 4, 5] {
        let out = reg.drive_plan_with(&group, &gh, issue, None, None, None, &orch, 1_000);
        assert_eq!(out["driving"], json!(true), "#{issue}: {out}");
    }
    // The FIRST tick also runs the once-per-process reconcile, which reads one
    // issue per live entry and is NOT under this bound — see
    // `PD_MAX_GH_PER_TICK`'s own doc, which says so. Drain it here, and pin its
    // shape, so the steady-state figure below is measured on a steady-state
    // wake rather than on the one wake that is different.
    let reconcile_from = gh.issue_views();
    reg.pd_drive_group_with(&group, &gh, 1_050);
    assert_eq!(
        gh.issue_views() - reconcile_from,
        5 + PD_MAX_GH_PER_TICK,
        "the reconcile reads every live entry once, and the tick then spends its own bound: {:?}",
        gh.calls()
    );

    let before = gh.issue_views();
    let report = reg.pd_drive_group_with(&group, &gh, 1_100);
    assert_eq!(
        gh.issue_views() - before,
        PD_MAX_GH_PER_TICK,
        "a steady-state wake spends exactly the bound, not one call more: {:?}",
        gh.calls()
    );
    assert_eq!(report.deferred.len(), 1, "the drive it could not reach is NAMED, not dropped");
}

/// One paragraph, both shapes — the assertion every pd notice goes through.
///
/// CLAUDE.md's rule, and it has now caught this branch twice: a `\n` plus
/// indentation ships the source's own leading spaces to the reader, and a `\`
/// continuation that collapsed leaves the same run with no `\n` to notice.
fn assert_one_paragraph(what: &str, text: &str) {
    assert!(!text.contains('\n'), "{what} must not carry a newline: {text:?}");
    assert!(
        !text.contains("          "),
        "{what} must not carry a ten-space run — a `\\` continuation that collapsed leaves one \
         with no newline to notice: {text:?}"
    );
}

/// Every notice one fixture delivered: one paragraph each, and **at least one**.
///
/// The per-block floor, and it is the whole of what the aggregate `seen >= 5`
/// could not say. Every assertion in this test sits inside a loop over a list
/// that may be empty, so a fixture delivering nothing satisfies all of them —
/// and an aggregate floor is satisfied by ONE block delivering five while four
/// deliver none. That is not hypothetical: this test's first block used to
/// drive to `held(awaiting-p3b)`, and #3040 P3b replaced that arc with
/// `boarding -> running`, which owes no notice at all. The block went silently
/// vacuous and the aggregate stayed green.
fn assert_block_notices(what: &str, reg: &OrchRegistry, group: &GroupId) -> usize {
    let texts = pane_texts(reg, group);
    assert!(
        !texts.is_empty(),
        "{what}: this fixture delivered NO notice, so every one-paragraph assertion over it is \
         vacuous — the arc it is meant to drive no longer owes one"
    );
    for t in &texts {
        assert_one_paragraph(what, t);
    }
    texts.len()
}

/// **Every notice this drive can put in the orchestrator's pane is ONE
/// PARAGRAPH** (rev-std round 3, finding 1).
///
/// The population nothing covered, and the gap is not theoretical: finding 1 of
/// round 1 fixed this class in `parse_workflow`'s refusals and added
/// `every_parse_error_is_one_paragraph` for THAT population — and the same
/// commit shipped a fresh 26-space instance in `pdtick.rs`'s
/// `reservation-unspawned` notice, because pd notices were a third population
/// with no pin at all. `every_listing_finding_is_one_paragraph` covers a fourth.
/// Three pinned populations and one unpinned is how a class survives being
/// fixed.
///
/// Two halves, because the texts come from two places. The STATIC half is
/// `PdHeldReason::notice_line`, which the engine owns. The COMPOSED half is
/// what `pdtick` actually hands to `owe_notice`, and it is collected by driving
/// the real arcs and reading the delivered prompts — a literal that only the
/// wiring builds is exactly where the last one hid.
#[test]
fn every_pd_notice_is_one_paragraph() {
    // (a) the static half — BOTH enums. A slice hold parks one slice where a
    // drive hold parks the drive, but its text lands in the same pane under the
    // same rule; iterating only the drive reasons left every `PdSliceHold`
    // notice line unpinned, including the `worker-gone` one this round adds.
    for r in PdHeldReason::ALL {
        assert_one_paragraph(&format!("PdHeldReason::{}'s notice_line", r.as_str()), r.notice_line());
    }
    for r in loomux_lib::orchestration::plandrive::PdSliceHold::ALL {
        assert_one_paragraph(&format!("PdSliceHold::{}'s notice_line", r.as_str()), r.notice_line());
    }

    // (b) the composed half. Each of these drives a real arc to the point where
    // it owes a notice, and the notice is read off the delivery the
    // orchestrator's pane really received.
    // Counted per BLOCK, not per notice: the floor this test needs is "every
    // fixture still drives an arc that owes a notice", and a notice total is
    // satisfied by one fixture delivering several while another delivers none.
    let mut blocks = 0usize;

    // plan-review — the ONE notice a declared window buys, which interpolates a
    // slice count and the plan's URL.
    {
        let repo = Repo::new();
        let (reg, _d) = test_registry();
        let gh = FakeGh::open(&["agent-ready"]);
        let (group, orch) = grouped(&reg, &repo);
        let out = reg.drive_plan_with(&group, &gh, 3040, None, Some(30), None, &orch, 1_000);
        assert_eq!(out["driving"], json!(true), "{out}");
        let doc = plandrive::validate_for_drive(&in_comment(PLAN3), 3040, &roster()).unwrap();
        reg.pd_store_posted_plan_at(&group, 3040, doc, "https://example/c/1", 1_100);
        reg.pd_drive_group_with(&group, &gh, 1_200);
        blocks += 1;
        assert_block_notices("a plan-review notice", &reg, &group);
    }
    // held(row-removed) — the human strikes a boarded row.
    {
        let repo = Repo::new();
        let (reg, _d) = test_registry();
        let gh = FakeGh::open(&["agent-ready"]);
        let (group, _orch, rows) = running(&reg, &repo, &gh, PLAN3);
        reg.delete_task(&group, "the human", &rows["P2"]).expect("the human strikes a row");
        reg.pd_drive_group_with(&group, &gh, 1_500);
        blocks += 1;
        assert_block_notices("a row-removed notice", &reg, &group);
    }
    // A SLICE hold — composed in `pd_execute` rather than by `owe_notice`, and
    // it interpolates the worker's own note, which is the one value here that a
    // delegate chooses. A third composition site, and the one this slice added.
    {
        let repo = Repo::new();
        let (reg, _d) = test_registry();
        let gh = FakeGh::open(&["agent-ready"]);
        let (group, _orch, _rows) = running(&reg, &repo, &gh, PLAN3);
        reg.pd_drive_group_with(&group, &gh, 1_400);
        let agent = slice_agent(&reg, &group, "P1");
        reg.set_pty_for_test(&agent, 7_101);
        report(&reg, &group, &agent, "blocked", json!({ "note": "the contract is ambiguous" }));
        reg.pd_drive_group_with(&group, &gh, 1_500);
        blocks += 1;
        assert_block_notices("a worker-blocked slice notice", &reg, &group);
    }
    // complete — the `agent-investigation` end, whose notice interpolates a URL.
    {
        let repo = Repo::new();
        let (reg, _d) = test_registry();
        let gh = FakeGh::open(&["agent-investigation"]);
        let (group, _orch, _planner) = driven(&reg, &repo, &gh);
        let doc = plandrive::validate_for_drive(&in_comment(PLAN), 3040, &roster()).unwrap();
        reg.pd_store_posted_plan_at(&group, 3040, doc, "https://example/c/1", 1_100);
        reg.pd_drive_group_with(&group, &gh, 1_200);
        blocks += 1;
        assert_block_notices("a plan-posted-complete notice", &reg, &group);
    }
    // cancelled — the reconcile's closed-issue notice.
    {
        let repo = Repo::new();
        let (reg, _d) = test_registry();
        let gh = FakeGh::open(&["agent-ready"]);
        let (group, _orch, _planner) = driven(&reg, &repo, &gh);
        gh.set_state("CLOSED");
        reg.pd_drive_group_with(&group, &gh, 1_200);
        blocks += 1;
        assert_block_notices("an issue-closed notice", &reg, &group);
    }
    // held(plan-invalid) — the notice the hook itself owes, which is composed in
    // a different function again and interpolates the planner's own reasons.
    {
        let repo = Repo::new();
        let (reg, _d) = test_registry();
        let gh = FakeGh::open(&["agent-ready"]);
        let (group, _orch, planner) = driven(&reg, &repo, &gh);
        for at in [1_100u64, 1_200, 1_300] {
            let _ = reg.pd_plan_check_at(&group, &planner, 3040, "no block here\n", at);
        }
        blocks += 1;
        assert_block_notices("a plan-invalid notice", &reg, &group);
    }
    // held(plan-missing) via the RESERVATION arm — the literal that shipped
    // broken, reached the only way it can be: through the reconcile.
    {
        let (reg, group, _repo, _d) = reserved_drive();
        let gh = FakeGh::open(&["agent-ready"]);
        reg.pd_drive_group_with(&group, &gh, 9_000);
        blocks += 1;
        assert_block_notices("a reservation-unspawned notice", &reg, &group);
    }

    // The control that MATTERS is now per block, inside
    // `assert_block_notices` — an aggregate floor is satisfied by one fixture
    // delivering five while four deliver none, which is exactly how this test's
    // first block went vacuous when #3040 P3b replaced the arc it drove.
    //
    // What is left here is a floor on the number of BLOCKS, so that deleting a
    // fixture is visible rather than merely lowering a total.
    assert!(
        blocks == 7,
        "seven fixtures, each of which must still deliver at least one notice: blocks={blocks}"
    );
}

/// Every notice-delivery ATTEMPT this group made, in order.
///
/// **Attempts, not notices, and the distinction is this harness's.**
/// `deliver_prompt` writes its `prompt` audit row and THEN fails with
/// "no app handle" — there is no Tauri app in a test — so every flush audits a
/// row and reports failure, the drive KEEPS the undelivered notice by design
/// ("a hold nobody was told about is a drive that stopped in silence"), and the
/// next flush retries it. One notice therefore shows up once per flush.
///
/// So a COUNT of these says how many times orrerix tried, which is a fact about
/// the harness. What a test wants is the set of DISTINCT texts —
/// [`distinct_pane_texts`].
fn pane_texts(reg: &OrchRegistry, group: &GroupId) -> Vec<String> {
    reg.audit_log(group)
        .into_iter()
        .filter(|e| e.action == "prompt")
        .filter_map(|e| e.detail["text"].as_str().map(str::to_string))
        .filter(|t| t.contains("plan drive #"))
        .collect()
}

/// The distinct notices this group produced, order-preserving.
///
/// The set a test means when it says "one notice is owed" — see
/// [`pane_texts`] for why the raw row count is not that.
fn distinct_pane_texts(reg: &OrchRegistry, group: &GroupId) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in pane_texts(reg, group) {
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// A group whose record carries a BARE RESERVATION: an entry `drive_plan` wrote
/// and never completed, with no planner recorded on it.
///
/// Built by writing the record rather than by driving, because the state is one
/// only a process that ENDED mid-call can leave behind — `drive_plan_with`
/// either attaches the pane or rolls the reservation back, so no sequence of
/// calls produces it. That is exactly why the arm exists, and why nothing but a
/// hand-built record can reach it.
fn reserved_drive() -> (OrchRegistry, GroupId, Repo, tempfile::TempDir) {
    let repo = Repo::new();
    let (reg, dir) = test_registry();
    let (group, _orch) = grouped(&reg, &repo);

    let mut state = plandrive::PlanDrivesState::default();
    let entry = plandrive::PdEntry::new(3040, "orch-1", "plan-lead", Consent::Ready, None, 0, 1_000);
    // The shape the arm keys on, asserted here so the fixture cannot drift into
    // some other state and take the test's meaning with it.
    assert!(entry.planner_agent.is_empty() && entry.spawned_ms == 0, "a BARE reservation");
    state.entries.push(entry);
    std::fs::create_dir_all(record_path(&reg, &group).parent().unwrap()).unwrap();
    std::fs::write(record_path(&reg, &group), serde_json::to_string(&state).unwrap()).unwrap();

    (reg, group, repo, dir)
}

/// **A reservation nobody completed is held, named and explained on restart**
/// (rev-std round 3, finding 2).
///
/// Without this arm the entry sits in `planning` with no clock to charge it —
/// `spawned_ms` is 0, which `planner_age_ms` reads as "no pane yet", so the
/// planner-stall bound never fires — until the whole-drive backstop parks it
/// hours later with a reason that describes something else.
#[test]
fn a_reservation_nobody_completed_is_held_on_restart() {
    let (reg, group, _repo, _d) = reserved_drive();
    let gh = FakeGh::open(&["agent-ready"]);

    // The control: before the reconcile it really is a live `planning` drive,
    // so the hold below is this arm's doing and not the fixture's.
    assert_eq!(drive_state(&reg, &group), "planning");

    reg.pd_drive_group_with(&group, &gh, 9_000);

    assert_eq!(drive_state(&reg, &group), "held");
    assert_eq!(held_reason(&reg, &group), PdHeldReason::PlanMissing.as_str());
    let rows: Vec<String> = reg
        .audit_log(&group)
        .into_iter()
        .filter(|e| e.action == plandrive::audit_action::RECOVERED)
        .filter_map(|e| e.detail["why"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        rows,
        vec!["reservation-unspawned".to_string()],
        "the reconcile names WHY it recovered this entry, not merely that it did"
    );

    // The notice says what happened and is one paragraph — the shape that
    // shipped broken here.
    //
    // DISTINCT texts, because the raw `prompt` rows are delivery ATTEMPTS: this
    // harness has no app handle, so every attempt fails, the drive keeps the
    // notice (deliberately — see `pd_flush_notices`) and retries it on the next
    // flush. An earlier draft asserted the row count was 1 and read the retry as
    // a duplicated notice.
    let texts = distinct_pane_texts(&reg, &group);
    assert_eq!(texts.len(), 1, "exactly one notice is owed: {texts:?}");
    // …and the retry really is a retry of THAT notice rather than a second one.
    assert!(
        pane_texts(&reg, &group).iter().all(|t| t == &texts[0]),
        "every attempt must be the same notice"
    );
    assert_one_paragraph("the reservation-unspawned notice", &texts[0]);
    assert!(
        texts[0].contains("no planner was ever recorded"),
        "the notice says what is actually wrong: {:?}",
        texts[0]
    );
    // And it does NOT claim a pane cannot exist: this arm also fires when the
    // process ended between the spawn and the attach, where one does.
    assert!(
        !texts[0].contains("nothing to hear from"),
        "the notice must not overclaim — a pane may have been opened and lost: {:?}",
        texts[0]
    );
}

// ── helpers that touch the record itself ────────────────────────────────────

fn record_path(reg: &OrchRegistry, group: &GroupId) -> std::path::PathBuf {
    reg.pd_record_path_for_test(group)
}

fn read_record(reg: &OrchRegistry, group: &GroupId) -> Value {
    let text = std::fs::read_to_string(record_path(reg, group))
        .expect("the drive record must exist by now");
    serde_json::from_str(&text).expect("the record must be valid JSON")
}

/// The record's schema is v1 and its unknown fields survive a read/write cycle
/// — a newer build's key must not be destroyed by an older one.
#[test]
fn the_record_is_v1_and_preserves_a_newer_builds_fields() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, _planner) = driven(&reg, &repo, &gh);

    let file = read_record(&reg, &group);
    assert_eq!(file["version"], json!(1));

    let mut with_extra = file.clone();
    with_extra["from_a_newer_build"] = json!({ "keep": "me" });
    std::fs::write(record_path(&reg, &group), with_extra.to_string()).unwrap();

    // A tick reads, decides, and writes it back.
    reg.pd_drive_group_with(&group, &gh, 1_100);
    let after = read_record(&reg, &group);
    assert_eq!(
        after["from_a_newer_build"],
        json!({ "keep": "me" }),
        "a key this build does not know must survive: {after}"
    );

    // A schema this build does not understand is refused, never acted on.
    let mut bumped = file;
    bumped["version"] = json!(99);
    std::fs::write(record_path(&reg, &group), bumped.to_string()).unwrap();
    let s = reg.plan_drive_status(&group);
    assert_eq!(s["refused"], json!(plandrive::refusal::STATE_UNREADABLE), "{s}");
}

/// The state and consent vocabularies are closed and round-trip through their
/// own wire spelling — the property every audit filter and every stored record
/// depends on.
#[test]
fn every_state_and_reason_word_round_trips() {
    for s in PlanDriveState::ALL {
        assert_eq!(PlanDriveState::parse(s.as_str()), Some(s));
    }
    for r in PdHeldReason::ALL {
        assert_eq!(PdHeldReason::parse(r.as_str()), Some(r));
        assert!(!r.notice_line().is_empty(), "{} has no notice line", r.as_str());
    }
    assert_eq!(PlanDriveState::parse("boarding-ish"), None);
    assert_eq!(PdHeldReason::parse("plan-invalid "), Some(PdHeldReason::PlanInvalid));
    assert_eq!(PdHeldReason::parse("what"), None);
    // `Ready` wins a tie, and the tie is real: an issue can carry both labels.
    assert_eq!(
        Consent::from_labels(&["agent-investigation", "agent-ready"]),
        Some(Consent::Ready)
    );
    assert_eq!(Consent::from_labels(&["agent-investigation"]), Some(Consent::Investigation));
    assert_eq!(Consent::from_labels(&["bug"]), None);
}

// ════════════════════════════════════════════════════════════════════════════
// #3040 P3b — the executor: boarding, spawning, and the hand-off to the review
// driver. Everything below runs against the same seam P3a built: a canned `gh`,
// real `spawn_agent_bound` panes, and `mcp::dispatch` for the delegates' own
// reports. No test here spawns a real agent CLI (CLAUDE.md constraint 3).
// ════════════════════════════════════════════════════════════════════════════

/// A three-slice plan: two independent, one dependent, and one of the
/// independents flagged `hold: true`.
///
/// The `hold: true` slice is on an INDEPENDENT slice on purpose. Flagging a
/// dependent one would make "it never spawned" true for two reasons at once —
/// the flag, and its unmet dep — and a test cannot tell those apart.
const PLAN3: &str = "\
version: 1
issue: 3040
slices:
  - id: P1
    title: the parser
    branch: feat/3040-p1
    block: worker-adv
    deps: []
    avoid_files: [src-tauri/src/orchestration/mod.rs]
    red_before_green: cargo test --locked -p loomux-engine plandoc
    brief: |
      Write the parser, refuse everything you cannot read, and repair nothing.
      The second paragraph is here so the verbatim pin has a blank line in it.
  - id: P2
    title: the core
    branch: feat/3040-p2
    block: worker-adv
    deps: [P1]
    brief: |
      The record and the state machine, mirroring the review driver's own core.
  - id: P3
    title: the judgement call
    branch: feat/3040-p3
    block: worker-adv
    deps: []
    hold: true
    brief: |
      This one carries a design decision the planner deliberately left open for
      a human, so orrerix must never spawn it by itself.
";

/// The planner's `brief:` for P1, exactly as YAML will have decoded it. Written
/// out rather than re-derived from [`PLAN3`], so the verbatim pin compares
/// against a literal a human can read instead of against the parser's own
/// output — which would pass however the parser mangled it.
const P1_BRIEF: &str = "Write the parser, refuse everything you cannot read, and repair nothing.\n\
The second paragraph is here so the verbatim pin has a blank line in it.\n";

fn roster3() -> Vec<(String, bool)> {
    roster()
}

/// A pane for an agent. In test mode nothing binds one, and an agent without a
/// pane silently receives nothing — so a test asserting a delivery would read
/// an empty log rather than a missing feature.
fn with_pane(reg: &OrchRegistry, agent_id: &str, pty: u32) {
    reg.set_pty_for_test(agent_id, pty);
}

/// A drive taken all the way to `running`, with its rows on the board.
///
/// Two ticks, and both are real: `plan-posted` -> `boarding` writes the rows,
/// `boarding` -> `running` is `decide` seeing them. Returns the group, the
/// orchestrator and the slice-id -> task-id map read back off the record.
fn running(
    reg: &OrchRegistry,
    repo: &Repo,
    gh: &FakeGh,
    plan: &str,
) -> (GroupId, String, std::collections::BTreeMap<String, String>) {
    let (group, orch) = grouped(reg, repo);
    let out = reg.drive_plan_with(&group, gh, 3040, None, None, None, &orch, 1_000);
    assert_eq!(out["driving"], json!(true), "drive_plan refused: {out}");
    let doc = plandrive::validate_for_drive(&in_comment(plan), 3040, &roster3())
        .expect("the specimen plan must validate");
    reg.pd_store_posted_plan_at(&group, 3040, doc, "https://example/c/1", 1_100);
    reg.pd_drive_group_with(&group, gh, 1_200);
    reg.pd_drive_group_with(&group, gh, 1_300);
    assert_eq!(
        drive_state(reg, &group),
        "running",
        "the drive must reach `running` before a test about running it means anything: {}",
        status(reg, &group)
    );
    let map = slice_rows(reg, &group);
    assert!(!map.is_empty(), "boarding wrote no rows at all");
    (group, orch, map)
}

/// Slice id -> board task id, read off the record.
fn slice_rows(
    reg: &OrchRegistry,
    group: &GroupId,
) -> std::collections::BTreeMap<String, String> {
    read_record(reg, group)["entries"][0]["slices"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    v["task_id"].as_str().map(|t| (k.clone(), t.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One slice's run state, off the record.
fn slice_state(reg: &OrchRegistry, group: &GroupId, slice: &str) -> String {
    read_record(reg, group)["entries"][0]["slices"][slice]["state"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn slice_hold(reg: &OrchRegistry, group: &GroupId, slice: &str) -> String {
    read_record(reg, group)["entries"][0]["slices"][slice]["hold"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn slice_agent(reg: &OrchRegistry, group: &GroupId, slice: &str) -> String {
    read_record(reg, group)["entries"][0]["slices"][slice]["agent"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn row(reg: &OrchRegistry, group: &GroupId, id: &str) -> Option<loomux_lib::orchestration::Task> {
    reg.tasks(group).into_iter().find(|t| t.id == id)
}

/// The `report` one of this group's delegates makes, through the real MCP arm.
fn report(reg: &OrchRegistry, group: &GroupId, agent: &str, status: &str, args: Value) -> Value {
    let mut a = args;
    a["status"] = json!(status);
    let out = dispatch(
        reg,
        &caller(group, agent, Role::Worker),
        "tools/call",
        &json!({ "name": "report", "arguments": a }),
    )
    .unwrap_or_else(|e| panic!("report({status}) failed: {e:?}"));
    // A tool that ERRORS answers `Ok` with `isError`, so a bare `unwrap` above
    // would let a refused report pass for a delivered one — which is exactly
    // the shape every test here is about.
    assert_eq!(
        out["isError"],
        json!(false),
        "report({status}) was refused: {out}"
    );
    out
}

// ── §3: the brief ───────────────────────────────────────────────────────────

/// **The planner's text is delivered VERBATIM**, between an orrerix-written
/// header and the one copy of the definition of done.
///
/// The three assertions are deliberately different in kind. `contains` on the
/// brief would pass against a build that quoted it twice or wrapped it; what is
/// pinned instead is the DECOMPOSITION — the brief splits the kickoff into
/// exactly a header, the planner's bytes and the DoD, in that order and with
/// nothing between them — which is the property §3 actually states.
///
/// The positive control is the header itself: a spawn that carried no brief at
/// all would satisfy "the planner's text is not mangled" trivially, so the
/// header's own facts (the branch, the base, the `Do not touch:` line) are
/// asserted present in the same breath.
#[test]
fn the_brief_carries_the_planner_text_verbatim_between_header_and_dod() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, _rows) = running(&reg, &repo, &gh, PLAN3);

    reg.pd_drive_group_with(&group, &gh, 1_400);
    let agent = slice_agent(&reg, &group, "P1");
    assert!(!agent.is_empty(), "P1 must have spawned: {}", status(&reg, &group));
    let kickoff = reg.agent(&agent).expect("the spawned pane is on the roster").task;

    // The header, first, and it is the population control for everything below:
    // an empty kickoff would pass a "not mangled" test and fails this one.
    assert!(kickoff.starts_with("You are slice `P1` of the plan for issue #3040: the parser."),
            "the header leads: {kickoff:?}");
    assert!(kickoff.contains("Branch: `feat/3040-p1`"), "{kickoff:?}");
    assert!(
        kickoff.contains("Do not touch: src-tauri/src/orchestration/mod.rs"),
        "the plan's avoid_files reach the worker: {kickoff:?}"
    );
    assert!(
        kickoff.contains("Red before green: cargo test --locked -p loomux-engine plandoc"),
        "{kickoff:?}"
    );

    // The decomposition. `\n\n` is the joint §3 names, so the kickoff must be
    // exactly header + planner + DoD with those two joints and no third.
    let dod = loomux_lib::orchestration::brief::dod_trailer();
    let body = kickoff
        .strip_suffix(&format!("\n\n{dod}"))
        .unwrap_or_else(|| panic!("the DoD must be the trailer, byte for byte: {kickoff:?}"));
    let planner = body
        .strip_suffix(&format!("\n\n{P1_BRIEF}"))
        .map(|_| &body[body.len() - P1_BRIEF.len()..])
        .unwrap_or_else(|| {
            panic!("the planner's brief must sit VERBATIM between the two: {body:?}")
        });
    assert_eq!(planner, P1_BRIEF, "verbatim means byte for byte");
    assert!(
        !body[..body.len() - P1_BRIEF.len()].contains(P1_BRIEF),
        "the planner's text appears ONCE, not quoted into the header as well"
    );
}

// ── §2(b) step 6: readiness, and one spawn a tick ───────────────────────────

/// **A dep-free slice spawns; its dependent does not.**
///
/// The two halves are the control for each other: both slices are boarded, both
/// are `queued` in the record, and only one of them has an unmet dep — so a
/// build that spawned nothing, or one that spawned everything, fails a
/// different half of this test.
#[test]
fn a_dep_free_slice_spawns_and_a_dependent_does_not() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, rows) = running(&reg, &repo, &gh, PLAN3);

    // The board really does carry the dep edge — the premise of the half below.
    let p2 = row(&reg, &group, &rows["P2"]).expect("P2 has a row");
    assert_eq!(p2.deps, vec![rows["P1"].clone()], "P2's row depends on P1's");

    reg.pd_drive_group_with(&group, &gh, 1_400);
    assert_eq!(slice_state(&reg, &group, "P1"), "running", "{}", status(&reg, &group));
    assert_eq!(
        slice_state(&reg, &group, "P2"),
        "queued",
        "a dependent must not spawn while its dep is open: {}",
        status(&reg, &group)
    );
    assert!(slice_agent(&reg, &group, "P2").is_empty(), "and no pane was opened for it");
}

/// **At most one spawn per tick within ONE drive**, whatever the board says is
/// ready — enforced by the spawn loop stopping after its first ATTEMPT, refused
/// or not.
///
/// The GROUP-wide half of the same bound is a different property with a
/// different enforcement point, and a mutation run showed this test does not
/// reach it: see `one_spawn_per_group_per_tick_across_two_drives`.
///
/// The plan's third slice is `hold: true`, so the ready set here is one — which
/// would make this test vacuous. It therefore un-holds P3 by hand first (the
/// plan is re-posted with the flag cleared), giving TWO ready slices, and the
/// assertion is that the tick spawned exactly one of them and the next tick
/// spawned the other. A build that spawned both fails on the count; one that
/// spawned neither fails on the second tick.
#[test]
fn only_one_spawn_per_tick_even_with_three_ready_slices() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let plan = PLAN3.replace("    hold: true\n", "");
    assert!(!plan.contains("hold: true"), "the un-hold really happened");
    let (group, _orch, _rows) = running(&reg, &repo, &gh, &plan);

    let ready = ["P1", "P3"];
    reg.pd_drive_group_with(&group, &gh, 1_400);
    let spawned: Vec<&str> =
        ready.iter().copied().filter(|s| slice_state(&reg, &group, s) == "running").collect();
    assert_eq!(
        spawned.len(),
        1,
        "exactly one of the two ready slices spawned this tick, not {spawned:?}: {}",
        status(&reg, &group)
    );

    reg.pd_drive_group_with(&group, &gh, 1_500);
    let spawned: Vec<&str> =
        ready.iter().copied().filter(|s| slice_state(&reg, &group, s) == "running").collect();
    assert_eq!(
        spawned.len(),
        2,
        "and the second tick spawns the other — the bound is per tick, not a cap on the plan: {}",
        status(&reg, &group)
    );
}

/// **A `hold: true` slice is never spawned** (§2(c)(i)) — the planner's own
/// veto, exercised on a surface that costs no orchestrator turn.
///
/// The control is P1 in the same drive: it is dep-free exactly as P3 is, and it
/// does spawn. So "P3 never spawned" is a statement about the flag rather than
/// about a build that spawns nothing.
#[test]
fn a_hold_true_slice_is_never_spawned() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, rows) = running(&reg, &repo, &gh, PLAN3);

    // The row exists and is ready by every measure the BOARD has: what stops it
    // is the plan, not the board, which is the whole point of the flag.
    let p3 = row(&reg, &group, &rows["P3"]).expect("P3 is boarded like any other slice");
    assert_eq!(p3.status, "queued");
    assert!(p3.deps.is_empty(), "P3 has no dep to be waiting on");

    for (i, now) in [1_400u64, 1_500, 1_600, 1_700].into_iter().enumerate() {
        reg.pd_drive_group_with(&group, &gh, now);
        assert_eq!(
            slice_state(&reg, &group, "P3"),
            "queued",
            "tick {i}: a hold: true slice must never spawn: {}",
            status(&reg, &group)
        );
    }
    // The control.
    assert_eq!(
        slice_state(&reg, &group, "P1"),
        "running",
        "the dep-free slice WITHOUT the flag did spawn, so the flag is what stopped P3"
    );
}

// ── §2(b) step 6: the hand-off, and what merging releases ───────────────────

/// **A worker's `report(done)` hands its PR to the REVIEW driver**, on behalf of
/// the orchestrator this plan drive acts for.
///
/// Asserted on the `rd-started` audit row and its `on_behalf_of`, because that
/// is the fact the design turns on: the review drive exists, and it is doing the
/// work for the same orchestrator — not for the plan driver, which is not a
/// principal.
#[test]
fn a_worker_done_hands_the_pr_to_drive_review_on_behalf_of_the_orchestrator() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, orch, _rows) = running(&reg, &repo, &gh, PLAN3);
    reg.pd_drive_group_with(&group, &gh, 1_400);

    let agent = slice_agent(&reg, &group, "P1");
    with_pane(&reg, &agent, 7_101);
    gh.set_pr(4_100, "OPEN", None);
    report(&reg, &group, &agent, "done", json!({ "ref": "#4100", "note": "green" }));

    // The report was CONSUMED by the plan driver, not delivered — the
    // interception this slice adds. Read before the tick, because the tick is
    // what turns it into a hand-off.
    assert!(
        audit_actions(&reg, &group).iter().any(|a| a == "pd-slice-consumed"),
        "a driven slice's worker reports to its drive: {:?}",
        audit_actions(&reg, &group)
    );

    reg.pd_drive_group_with(&group, &gh, 1_500);
    assert_eq!(slice_state(&reg, &group, "P1"), "in-review", "{}", status(&reg, &group));

    let rd = reg
        .audit_log(&group)
        .into_iter()
        .find(|e| e.action == "rd-started")
        .expect("the review driver was started for this PR");
    assert_eq!(
        rd.detail.get("on_behalf_of").and_then(Value::as_str),
        Some(orch.as_str()),
        "the review drive acts for the ORCHESTRATOR, not for the plan driver: {:?}",
        rd.detail
    );
    assert!(
        audit_actions(&reg, &group).iter().any(|a| a == "pd-review-driven"),
        "{:?}",
        audit_actions(&reg, &group)
    );
}

/// **A MERGED PR marks the row `done`, and that is what releases the dependent.**
///
/// The negative control is in the same test and is the whole reason it is worth
/// writing: the tick BEFORE the merge is read, with the PR open, and P2 is still
/// queued there. So "the dependent spawned" is caused by the merge rather than
/// by time passing.
#[test]
fn a_merged_pr_marks_done_and_spawns_the_dependent() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, rows) = running(&reg, &repo, &gh, PLAN3);
    reg.pd_drive_group_with(&group, &gh, 1_400);

    let agent = slice_agent(&reg, &group, "P1");
    with_pane(&reg, &agent, 7_101);
    gh.set_pr(4_100, "OPEN", None);
    report(&reg, &group, &agent, "done", json!({ "ref": "#4100", "note": "green" }));
    reg.pd_drive_group_with(&group, &gh, 1_500);
    assert_eq!(slice_state(&reg, &group, "P1"), "in-review");

    // The control: an OPEN PR moves nothing.
    reg.pd_drive_group_with(&group, &gh, 1_600);
    assert_eq!(slice_state(&reg, &group, "P1"), "in-review", "an open PR is not done");
    assert_eq!(slice_state(&reg, &group, "P2"), "queued", "so its dependent stays queued");

    gh.set_pr(4_100, "MERGED", Some("2026-01-01T00:00:00Z"));
    reg.pd_drive_group_with(&group, &gh, 1_700);
    assert_eq!(slice_state(&reg, &group, "P1"), "done", "{}", status(&reg, &group));
    assert_eq!(
        row(&reg, &group, &rows["P1"]).map(|t| t.status),
        Some("done".into()),
        "the BOARD row is what the dependent's readiness is read from"
    );
    assert!(
        audit_actions(&reg, &group).iter().any(|a| a == "pd-slice-merged"),
        "{:?}",
        audit_actions(&reg, &group)
    );

    reg.pd_drive_group_with(&group, &gh, 1_800);
    assert_eq!(
        slice_state(&reg, &group, "P2"),
        "running",
        "the dependent spawns once its dep's row is done: {}",
        status(&reg, &group)
    );
}

/// **A worker's `report(blocked)` parks ITS slice and nothing else** (§2(e)).
///
/// The control is P3 — un-held for this test so there is a second slice that
/// could have been affected — which keeps running through the same ticks. A
/// build that parked the whole drive fails on the drive state; one that parked
/// every slice fails on P3.
#[test]
fn a_worker_blocked_holds_only_its_slice() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let plan = PLAN3.replace("    hold: true\n", "");
    let (group, _orch, _rows) = running(&reg, &repo, &gh, &plan);
    reg.pd_drive_group_with(&group, &gh, 1_400);
    reg.pd_drive_group_with(&group, &gh, 1_500);
    for s in ["P1", "P3"] {
        assert_eq!(slice_state(&reg, &group, s), "running", "both independents are up first");
    }

    let agent = slice_agent(&reg, &group, "P1");
    with_pane(&reg, &agent, 7_101);
    report(&reg, &group, &agent, "blocked", json!({ "note": "the API contract is ambiguous" }));
    reg.pd_drive_group_with(&group, &gh, 1_600);

    assert_eq!(slice_state(&reg, &group, "P1"), "held");
    assert_eq!(slice_hold(&reg, &group, "P1"), "worker-blocked", "{}", status(&reg, &group));
    assert_eq!(
        slice_state(&reg, &group, "P3"),
        "running",
        "the independent slice keeps going: {}",
        status(&reg, &group)
    );
    assert_eq!(
        drive_state(&reg, &group),
        "running",
        "and so does the DRIVE — a worker's block is not a drive-level hold"
    );
    let notice = reg
        .audit_log(&group)
        .into_iter()
        .filter(|e| e.action == "pd-notice")
        .count();
    assert!(notice >= 1, "the hold is told to the orchestrator: {:?}", audit_actions(&reg, &group));
}

// ── §2(c): the human's board is the veto ────────────────────────────────────

/// **A struck row holds the drive on `row-removed`.**
///
/// Deleting a slice's row leaves its dependents naming a task that no longer
/// exists — an unmet dep forever — so the drive says so rather than ticking on
/// against work that can never become ready.
#[test]
fn a_struck_row_holds_row_removed() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, rows) = running(&reg, &repo, &gh, PLAN3);

    // The control: with every row present, the same tick does NOT hold.
    reg.pd_drive_group_with(&group, &gh, 1_400);
    assert_eq!(drive_state(&reg, &group), "running", "{}", status(&reg, &group));

    reg.delete_task(&group, "the human", &rows["P2"]).expect("the human strikes a row");
    reg.pd_drive_group_with(&group, &gh, 1_500);
    assert_eq!(drive_state(&reg, &group), "held", "{}", status(&reg, &group));
    assert_eq!(held_reason(&reg, &group), PdHeldReason::RowRemoved.as_str());
}

/// **A row a human marks `done` by hand releases its dependents**, with no PR
/// and no merge anywhere in sight.
///
/// This is §2(c)'s claim that the driver keeps no private copy of readiness: it
/// re-reads `tasks.json` every tick and asks `task_ready`, so an edit made on
/// the board is simply what the next tick sees.
#[test]
fn a_hand_marked_done_row_releases_its_dependents() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, rows) = running(&reg, &repo, &gh, PLAN3);

    // The control: P2 is not ready while P1's row is open, and the drive has
    // been ticked, so "P2 did not spawn" is not merely "nothing has run yet".
    reg.pd_drive_group_with(&group, &gh, 1_400);
    assert_eq!(slice_state(&reg, &group, "P2"), "queued");

    reg.upsert_task_by_human(
        &group,
        "the human",
        Some(&rows["P1"]),
        loomux_lib::orchestration::TaskPatch {
            status: Some("done".into()),
            ..loomux_lib::orchestration::TaskPatch::default()
        },
    )
    .expect("a human may mark a row done");

    reg.pd_drive_group_with(&group, &gh, 1_500);
    assert_eq!(
        slice_state(&reg, &group, "P2"),
        "running",
        "a hand-marked row releases its dependents — readiness is the BOARD's: {}",
        status(&reg, &group)
    );
}

// ── §2(d): consent is re-read, never remembered ─────────────────────────────

/// **A label withdrawn mid-drive holds the drive BEFORE the next spawn.**
///
/// The control is the tick immediately before: the same drive, the same ready
/// slice, and the label still on — it spawns. So the refusal is about the label
/// rather than about the drive having run out of work.
#[test]
fn a_withdrawn_label_holds_before_the_next_spawn() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let plan = PLAN3.replace("    hold: true\n", "");
    let (group, _orch, _rows) = running(&reg, &repo, &gh, &plan);

    // The control.
    reg.pd_drive_group_with(&group, &gh, 1_400);
    assert_eq!(slice_state(&reg, &group, "P1"), "running", "{}", status(&reg, &group));
    assert_eq!(slice_state(&reg, &group, "P3"), "queued", "P3 is next in line");

    gh.set_labels(&["bug"]);
    reg.pd_drive_group_with(&group, &gh, 1_500);
    assert_eq!(drive_state(&reg, &group), "held", "{}", status(&reg, &group));
    assert_eq!(held_reason(&reg, &group), PdHeldReason::ConsentWithdrawn.as_str());
    assert_eq!(
        slice_state(&reg, &group, "P3"),
        "queued",
        "and the slice that was next was NOT spawned: {}",
        status(&reg, &group)
    );
    assert!(slice_agent(&reg, &group, "P3").is_empty(), "no pane was opened for it");
}

// ── §2(c): the declared review window ───────────────────────────────────────

/// **A declared review window posts EXACTLY ONE notice**, and spawns nothing
/// until it has run.
///
/// The count is the assertion, not merely "a notice was sent": the window's
/// whole cost is one orchestrator interruption, and a driver that re-announced
/// itself every tick would be spending the turns this design exists to remove.
#[test]
fn a_review_window_posts_exactly_one_notice() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, orch) = grouped(&reg, &repo);
    let out = reg.drive_plan_with(&group, &gh, 3040, None, Some(30), None, &orch, 1_000);
    assert_eq!(out["driving"], json!(true), "{out}");
    let doc = plandrive::validate_for_drive(&in_comment(PLAN3), 3040, &roster3()).unwrap();
    reg.pd_store_posted_plan_at(&group, 3040, doc, "https://example/c/1", 1_100);

    // Into the window, and then three more ticks well inside it.
    let mut now = 1_200u64;
    for _ in 0..4 {
        reg.pd_drive_group_with(&group, &gh, now);
        now += 60_000;
    }
    assert_eq!(drive_state(&reg, &group), "plan-review", "{}", status(&reg, &group));
    assert_eq!(
        slice_rows(&reg, &group).len(),
        0,
        "nothing is boarded and nothing is spawned inside the window"
    );
    // **One notice OWED, however many ticks pass** — which is the property, and
    // it is read off the record rather than off a count of delivery attempts.
    // A notice that fails to deliver is KEPT and RETRIED by design (a hold
    // nobody was told about is the one outcome this design exists to avoid), so
    // `pd-notice` rows count attempts, not notices: four ticks against a pane
    // that cannot take a delivery produce four rows for the one notice. What
    // `failures` distinguishes is exactly the thing under test — a retry of the
    // same notice from a second notice, which would reset it to zero.
    let owed = read_record(&reg, &group)["entries"][0]["owed"].clone();
    assert!(
        owed["text"]
            .as_str()
            .is_some_and(|t| t.contains("PLAN POSTED") && t.contains("spawning in 30 min")),
        "the window's own notice is the one owed: {owed}"
    );
    let attempts = reg.audit_log(&group).into_iter().filter(|e| e.action == "pd-notice").count();
    assert!(attempts >= 2, "the population control: delivery really was attempted more than once");
    assert!(
        owed["failures"].as_u64().is_some_and(|f| f >= 1),
        "those attempts are RETRIES of this notice, counted on it: {owed}"
    );
    // The exact pin, and the one an extra notice could not survive: `owed_ms`
    // is stamped when a notice is OWED and never touched by a retry, so it is
    // still the tick that entered `plan-review`. A second notice — one per
    // tick, say — would have replaced this one and re-stamped it.
    assert_eq!(
        owed["owed_ms"],
        json!(1_200),
        "still the notice owed on the tick that entered the window, not a later one: {owed}"
    );

    // Past the window: the drive boards and runs, so the wait was a wait and not
    // a stop.
    reg.pd_drive_group_with(&group, &gh, 1_100 + 31 * 60_000);
    reg.pd_drive_group_with(&group, &gh, 1_100 + 32 * 60_000);
    assert_eq!(drive_state(&reg, &group), "running", "{}", status(&reg, &group));
    assert_eq!(slice_rows(&reg, &group).len(), 3, "and the rows are written");
}


// ── §2(b) step 6: a cap refusal is not an error, and is bounded ─────────────

/// **A live-cap refusal leaves the row `queued`, and only becomes a hold after
/// `CAP_HOLD_MS`.**
///
/// Two properties in one test because the second is meaningless without the
/// first: a build that held on the FIRST refusal would satisfy "it eventually
/// holds" and defeat the whole retry, and one that never held would satisfy
/// "the row stays queued" by starving forever.
///
/// The cap is made to bite by filling the group with live delegates rather than
/// by faking a refusal, and the control that it really is the CAP — and not an
/// unknown block or the spawn-rate backstop — is `is_live_cap_refusal` being
/// what the audit row says: `pd-slice-cap-refused` is written on that
/// classification alone.
#[test]
fn a_cap_refusal_leaves_the_row_queued_then_holds_after_cap_hold_ms() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, rows) = running(&reg, &repo, &gh, PLAN3);

    // Fill the delegate cap. `rails()` allows 8 agents; the orchestrator is one,
    // the planner is another, so six more make the next spawn the one over.
    let mut filler = Vec::new();
    while let Ok(a) =
        reg.spawn_agent(&group, Role::Worker, "filler", "", false, None)
    {
        filler.push(a.id);
        if filler.len() > 16 {
            panic!("the delegate cap never bit — this test cannot measure what it is for");
        }
    }
    assert!(!filler.is_empty(), "at least one filler had to open for the cap to be full");

    reg.pd_drive_group_with(&group, &gh, 1_400);
    assert_eq!(
        slice_state(&reg, &group, "P1"),
        "queued",
        "a cap refusal is not an error — the row stays queued and is retried: {}",
        status(&reg, &group)
    );
    assert_eq!(
        row(&reg, &group, &rows["P1"]).map(|t| t.status),
        Some("queued".into()),
        "and the BOARD row is rolled back too, not left in-progress with nobody on it"
    );
    assert!(
        row(&reg, &group, &rows["P1"]).and_then(|t| t.assignee).is_none(),
        "nor left assigned to the driver"
    );
    assert!(
        audit_actions(&reg, &group).iter().any(|a| a == "pd-slice-cap-refused"),
        "the refusal is classified as the CAP's, not as a spawn failure: {:?}",
        audit_actions(&reg, &group)
    );
    assert_eq!(slice_hold(&reg, &group, "P1"), "", "and it is NOT held on the first refusal");

    // Still inside the window: still queued, still not held.
    reg.pd_drive_group_with(&group, &gh, 1_400 + CAP_HOLD_MS - 1);
    assert_eq!(slice_hold(&reg, &group, "P1"), "", "{}", status(&reg, &group));

    // Past it.
    reg.pd_drive_group_with(&group, &gh, 1_400 + CAP_HOLD_MS);
    assert_eq!(slice_state(&reg, &group, "P1"), "held", "{}", status(&reg, &group));
    assert_eq!(slice_hold(&reg, &group, "P1"), "cap-full");
    assert_eq!(
        drive_state(&reg, &group),
        "running",
        "one starved slice does not park the whole drive"
    );
}

/// **The PR poll is bounded and ROUND-ROBIN**, so a drive with more in-review
/// slices than the budget still sees the later ones.
///
/// The bound is asserted as a count of `gh pr view` calls — the population
/// figure the fake keeps — and the round robin is asserted by the cursor having
/// moved, which is what stops the same first slices being polled forever.
#[test]
fn the_pr_poll_is_bounded_per_tick() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, _rows) = running(&reg, &repo, &gh, PLAN3);
    reg.pd_drive_group_with(&group, &gh, 1_400);

    let agent = slice_agent(&reg, &group, "P1");
    with_pane(&reg, &agent, 7_101);
    gh.set_pr(4_100, "OPEN", None);
    report(&reg, &group, &agent, "done", json!({ "ref": "#4100", "note": "green" }));
    reg.pd_drive_group_with(&group, &gh, 1_500);
    assert_eq!(slice_state(&reg, &group, "P1"), "in-review", "{}", status(&reg, &group));

    let before = gh.pr_views();
    reg.pd_drive_group_with(&group, &gh, 1_600);
    let spent = gh.pr_views() - before;
    assert!(spent >= 1, "the poll really ran — the control for the bound below");
    assert!(
        spent <= PD_MAX_PR_CHECKS_PER_TICK,
        "one tick spent {spent} pr views, over the budget of {PD_MAX_PR_CHECKS_PER_TICK}"
    );
}

/// **The `ref` is a hint the driver may miss, and there is a fallback.**
///
/// A worker that reports `done` with no usable `ref` must still reach the review
/// driver: §2(b) step 6 allows exactly one `gh pr list --head <branch>` to stand
/// in for it. The control is the call log — the fallback really was the thing
/// that resolved the number, rather than a number that came from somewhere else.
#[test]
fn a_done_with_no_usable_ref_resolves_the_pr_from_the_branch() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, _rows) = running(&reg, &repo, &gh, PLAN3);
    reg.pd_drive_group_with(&group, &gh, 1_400);

    let agent = slice_agent(&reg, &group, "P1");
    with_pane(&reg, &agent, 7_101);
    gh.set_head_pr("feat/3040-p1", 4_242);
    gh.set_pr(4_242, "OPEN", None);
    report(&reg, &group, &agent, "done", json!({ "note": "opened it, forgot the ref" }));
    reg.pd_drive_group_with(&group, &gh, 1_500);

    assert!(
        gh.calls().iter().any(|a| a.first().map(String::as_str) == Some("pr")
            && a.get(1).map(String::as_str) == Some("list")
            && a.contains(&"feat/3040-p1".to_string())),
        "the fallback ran, keyed on the slice's own branch: {:?}",
        gh.calls()
    );
    assert_eq!(
        read_record(&reg, &group)["entries"][0]["slices"]["P1"]["pr"],
        json!(4_242),
        "and it is what resolved the PR: {}",
        status(&reg, &group)
    );
    assert_eq!(slice_state(&reg, &group, "P1"), "in-review");
}

/// **A resume releases the SLICE holds, not just the drive's.**
///
/// Without this there is no second way out of a slice hold at all:
/// `resume_plan_drive` moves the DRIVE, and nothing else in the build ever
/// un-parks a slice — so a `worker-blocked` slice would be a deletion wearing a
/// hold's name.
///
/// Released to `queued` rather than straight back to `running`, which is the
/// half worth pinning: readiness is re-derived from the BOARD, so a row the
/// human marked `blocked` in the meantime still does not spawn. That second
/// assertion is what stops a resume from being a way to spawn over a human's
/// own decision.
#[test]
fn a_resume_releases_a_held_slice_and_still_obeys_the_board() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let plan = PLAN3.replace("    hold: true\n", "");
    let (group, orch, rows) = running(&reg, &repo, &gh, &plan);
    reg.pd_drive_group_with(&group, &gh, 1_400);
    reg.pd_drive_group_with(&group, &gh, 1_500);

    // Park both independents: one by its worker, one by the human's board.
    for s in ["P1", "P3"] {
        let agent = slice_agent(&reg, &group, s);
        with_pane(&reg, &agent, 7_100 + s.len() as u32);
        report(&reg, &group, &agent, "blocked", json!({ "note": "stuck" }));
    }
    reg.pd_drive_group_with(&group, &gh, 1_600);
    assert_eq!(slice_hold(&reg, &group, "P1"), "worker-blocked", "{}", status(&reg, &group));
    assert_eq!(slice_hold(&reg, &group, "P3"), "worker-blocked");

    // The human decides P3 is not happening, and parks the whole drive by
    // taking the label off so there is something to resume.
    reg.upsert_task_by_human(
        &group,
        "the human",
        Some(&rows["P3"]),
        loomux_lib::orchestration::TaskPatch {
            status: Some("blocked".into()),
            ..loomux_lib::orchestration::TaskPatch::default()
        },
    )
    .expect("a human may park a row");
    gh.set_labels(&["bug"]);
    reg.pd_drive_group_with(&group, &gh, 1_700);
    assert_eq!(drive_state(&reg, &group), "held");

    gh.set_labels(&["agent-ready"]);
    let out = reg.resume_plan_drive_with(&group, 3040, &orch, 1_800);
    assert_eq!(out["resumed"], json!(true), "{out}");
    assert_eq!(slice_hold(&reg, &group, "P1"), "", "the slice hold is released: {}", status(&reg, &group));
    assert_eq!(slice_state(&reg, &group, "P1"), "queued");
    // **And the BOARD row with it**, which is the half that makes the release
    // mean anything: readiness is the board's, so a record set back to `queued`
    // over a row still `in-progress` and assigned to the pane that held it is a
    // release that buys nothing at all — the slice never becomes ready again and
    // the resume is silently a no-op. This assertion is what caught that.
    let p1 = row(&reg, &group, &rows["P1"]).expect("P1 still has a row");
    assert_eq!(p1.status, "queued", "the row is released too: {p1:?}");
    assert_eq!(p1.assignee, None, "and unassigned, so a claim can succeed: {p1:?}");
    // The row the human parked is NOT rolled back — a resume must not spawn
    // over their decision. Read here rather than only via the spawn below, so
    // the property is pinned on the row itself.
    let p3 = row(&reg, &group, &rows["P3"]).expect("P3 still has a row");
    assert_eq!(p3.status, "blocked", "the human's own row status stands: {p3:?}");

    let cwd_before = read_record(&reg, &group)["entries"][0]["slices"]["P1"]["cwd"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!cwd_before.is_empty(), "the first spawn recorded a workspace");

    reg.pd_drive_group_with(&group, &gh, 1_900);
    assert_eq!(
        slice_state(&reg, &group, "P1"),
        "running",
        "and it really does run again: {}",
        status(&reg, &group)
    );
    // **In the workspace it already had.** Cutting a second worktree for a
    // branch that is already checked out is not slower, it is REFUSED by git —
    // so a released slice that tried to would fail to spawn forever, which is
    // how this defect actually presented. The pin is the path, not the fact
    // that a pane opened.
    assert_eq!(
        read_record(&reg, &group)["entries"][0]["slices"]["P1"]["cwd"].as_str(),
        Some(cwd_before.as_str()),
        "the re-spawn resumed into the same worktree: {}",
        status(&reg, &group)
    );
    let respawned = slice_agent(&reg, &group, "P1");
    assert_eq!(
        reg.agent(&respawned).expect("the re-spawned pane is on the roster").cwd,
        cwd_before,
        "and the PANE really is there, not merely the record saying so"
    );
    assert_eq!(
        slice_state(&reg, &group, "P3"),
        "queued",
        "but a row the HUMAN parked does not spawn, resume or no resume: {}",
        status(&reg, &group)
    );
    assert!(slice_agent(&reg, &group, "P3").is_empty(), "no pane was opened for it");
}

/// **A `done` whose PR cannot be resolved yet is RETRIED, not lost.**
///
/// A `report` is an event, and the tick that consumes it clears it whether or
/// not it could act — so a `done` with no usable `ref`, on a tick where
/// `gh pr list --head` has not seen the branch yet, would otherwise leave the
/// slice `running` until the whole-drive stall backstop hours later. The record
/// carries the report instead.
///
/// The first tick is the control and is the whole point: it must genuinely FAIL
/// to resolve the PR — asserted on the slice still being `running` with no PR —
/// or the second tick proves nothing.
#[test]
fn a_done_whose_pr_is_not_resolvable_yet_is_retried_not_lost() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, _rows) = running(&reg, &repo, &gh, PLAN3);
    reg.pd_drive_group_with(&group, &gh, 1_400);

    let agent = slice_agent(&reg, &group, "P1");
    with_pane(&reg, &agent, 7_101);
    // No `ref`, and `gh` knows about no PR on that branch yet.
    report(&reg, &group, &agent, "done", json!({ "note": "pushed, PR opening" }));

    // The control: this tick really cannot resolve it.
    reg.pd_drive_group_with(&group, &gh, 1_500);
    assert_eq!(
        slice_state(&reg, &group, "P1"),
        "running",
        "the PR was not resolvable, so nothing was handed off: {}",
        status(&reg, &group)
    );
    assert_eq!(
        read_record(&reg, &group)["entries"][0]["slices"]["P1"]["reported_done"],
        json!(true),
        "but the report is on the RECORD, not only in the tick's signal map"
    );

    // The PR appears. No second `report` — the worker said `done` once.
    gh.set_head_pr("feat/3040-p1", 4_777);
    gh.set_pr(4_777, "OPEN", None);
    reg.pd_drive_group_with(&group, &gh, 1_600);
    assert_eq!(
        slice_state(&reg, &group, "P1"),
        "in-review",
        "the retry found it and handed it off: {}",
        status(&reg, &group)
    );
    assert_eq!(
        read_record(&reg, &group)["entries"][0]["slices"]["P1"]["reported_done"],
        json!(false),
        "and the flag is cleared by the thing it was waiting for"
    );
}

/// **The one-spawn-per-tick bound is per GROUP, not per drive** (§2(b) step 6),
/// and it is what makes "the plan driver holds, never starves the review
/// driver" a fact rather than a hope: the review driver has already spent its
/// own budget by the time this runs, so a plan driver that spawned once per
/// DRIVE would scale its appetite with how many issues an orchestrator handed
/// over.
///
/// **This test exists because a mutation run said the property was unpinned.**
/// Setting `spawn_budget` to 2 reddened nothing:
/// `only_one_spawn_per_tick_even_with_three_ready_slices` passes under it,
/// because within ONE drive the bound is enforced by the loop's `break` and not
/// by the budget at all. Two drives is the only shape that can tell them apart.
#[test]
fn one_spawn_per_group_per_tick_across_two_drives() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, orch) = grouped(&reg, &repo);

    // Two drives in one group, each with a dep-free slice ready to go.
    for issue in [3040u64, 3041] {
        let plan = PLAN3.replace("issue: 3040", &format!("issue: {issue}")).replace("    hold: true\n", "");
        // Distinct branches per drive: two slices on one branch is refused, and
        // it would also be a second reason for the second not to spawn.
        let plan = plan.replace("feat/3040-", &format!("feat/{issue}-"));
        let out = reg.drive_plan_with(&group, &gh, issue, None, None, None, &orch, 1_000);
        assert_eq!(out["driving"], json!(true), "#{issue}: {out}");
        let doc = plandrive::validate_for_drive(&in_comment(&plan), issue, &roster3())
            .unwrap_or_else(|e| panic!("#{issue}: {e:#?}"));
        reg.pd_store_posted_plan_at(&group, issue, doc, "https://example/c/1", 1_100);
    }
    reg.pd_drive_group_with(&group, &gh, 1_200);
    reg.pd_drive_group_with(&group, &gh, 1_300);

    // The premise: BOTH drives are running with a ready slice each, so the tick
    // below genuinely has two candidates. Without this the assertion would hold
    // for a build that had nothing to spawn at all.
    let record = read_record(&reg, &group);
    let entries = record["entries"].as_array().expect("two entries");
    assert_eq!(entries.len(), 2, "{record}");
    for e in entries {
        assert_eq!(e["state"], json!("running"), "both drives must be running: {e}");
        assert!(
            e["slices"].as_object().is_some_and(|m| m.len() == 3),
            "and boarded: {e}"
        );
    }

    reg.pd_drive_group_with(&group, &gh, 1_400);
    let spawned: usize = read_record(&reg, &group)["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            e["slices"]
                .as_object()
                .map(|m| m.values().filter(|s| s["state"] == json!("running")).count())
                .unwrap_or(0)
        })
        .sum();
    assert_eq!(
        spawned, 1,
        "one spawn for the GROUP, not one per drive: {}",
        read_record(&reg, &group)
    );

    // And the other drive is not starved — it takes the next tick's slot.
    reg.pd_drive_group_with(&group, &gh, 1_500);
    let spawned: usize = read_record(&reg, &group)["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            e["slices"]
                .as_object()
                .map(|m| m.values().filter(|s| s["state"] == json!("running")).count())
                .unwrap_or(0)
        })
        .sum();
    assert_eq!(spawned, 2, "the deferred drive gets the next tick: {}", read_record(&reg, &group));
}

// ── rev-std round 1 ─────────────────────────────────────────────────────────

/// **A worker pane that DIES without reporting parks its slice** (finding 1).
///
/// Nothing synthesizes a report for a dead pane, so before this arc the slice
/// stayed `running` forever — and a slice in `running` is exactly what makes
/// `running_idle` false, so the stall backstop could not see it either. The
/// drive ran on in silence, which is the one outcome the design exists to
/// avoid, and `resume_plan_drive` refused `not-held` because the drive was
/// never parked.
///
/// The control is the tick BEFORE the kill: same drive, same slice, pane alive
/// — still `running`. So the hold is the death's doing and not the tick's.
#[test]
fn a_dead_worker_pane_parks_its_slice() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let plan = PLAN3.replace("    hold: true\n", "");
    let (group, _orch, _rows) = running(&reg, &repo, &gh, &plan);
    reg.pd_drive_group_with(&group, &gh, 1_400);
    reg.pd_drive_group_with(&group, &gh, 1_500);

    let agent = slice_agent(&reg, &group, "P1");
    assert!(!agent.is_empty(), "P1 must have spawned: {}", status(&reg, &group));

    // The control.
    reg.pd_drive_group_with(&group, &gh, 1_600);
    assert_eq!(
        slice_state(&reg, &group, "P1"),
        "running",
        "a LIVE worker's slice keeps running: {}",
        status(&reg, &group)
    );

    assert!(reg.mark_agent_dead_for_test(&agent), "the worker pane must exist to be ended");
    reg.pd_drive_group_with(&group, &gh, 1_700);

    assert_eq!(slice_state(&reg, &group, "P1"), "held", "{}", status(&reg, &group));
    assert_eq!(slice_hold(&reg, &group, "P1"), "worker-gone");
    assert_eq!(
        slice_state(&reg, &group, "P3"),
        "running",
        "and only ITS slice — the independent one is untouched: {}",
        status(&reg, &group)
    );
    assert_eq!(drive_state(&reg, &group), "running", "the drive is not parked by one dead worker");
    assert!(
        reg.audit_log(&group).into_iter().any(|e| {
            e.action == "pd-slice-held"
                && e.detail["reason"] == json!("worker-gone")
        }),
        "the hold is on the record with its reason: {:?}",
        audit_actions(&reg, &group)
    );
}

/// **A worker that reported `done` and then exited is NOT parked** (finding 1,
/// the ordering half).
///
/// A pane exits after its work; parking on `Dead` alone would throw away the
/// hand-off the worker had just earned, on the very tick the PR is resolved.
/// `reported_done` is read before liveness, and this is what pins that order —
/// the mutation that swaps them reddens here and nowhere else.
#[test]
fn a_worker_that_reported_done_and_exited_is_handed_off_not_parked() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, _rows) = running(&reg, &repo, &gh, PLAN3);
    reg.pd_drive_group_with(&group, &gh, 1_400);

    let agent = slice_agent(&reg, &group, "P1");
    with_pane(&reg, &agent, 7_101);
    gh.set_pr(4_100, "OPEN", None);
    report(&reg, &group, &agent, "done", json!({ "ref": "#4100", "note": "green" }));
    // The pane exits, as a finished worker's does, BEFORE the tick runs.
    assert!(reg.mark_agent_dead_for_test(&agent), "the worker pane must exist to be ended");

    reg.pd_drive_group_with(&group, &gh, 1_500);
    assert_eq!(
        slice_state(&reg, &group, "P1"),
        "in-review",
        "a finished worker's death must not cost it the hand-off: {}",
        status(&reg, &group)
    );
    assert_eq!(slice_hold(&reg, &group, "P1"), "");
}

/// **A declared window longer than the stall timeout is not a stall**
/// (finding 4).
///
/// The backstop outside `running` measures whole-drive AGE, which includes a
/// window the caller deliberately asked for — so a window past the timeout
/// parked the drive on `drive-stalled` for doing exactly what it was told, with
/// a notice naming the wrong cause.
///
/// The control is the second half: the suppression ENDS. Past the window the
/// drive boards, so this is a bounded wait rather than an exemption from the
/// backstop.
#[test]
fn a_review_window_longer_than_the_drive_timeout_is_not_a_stall() {
    // The window is clamped to `PLAN_REVIEW_MINUTES_MAX` (120) and the default
    // drive timeout is 720 minutes, so a window alone can never outrun it. The
    // fixture shortens the TIMEOUT instead — the same crossing from the other
    // side, and the one a repo can actually declare.
    let repo = Repo::with(WORKFLOW_SHORT_TIMEOUT);
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, orch) = grouped(&reg, &repo);


    let out = reg.drive_plan_with(&group, &gh, 3040, None, Some(60), None, &orch, 1_000);
    assert_eq!(out["driving"], json!(true), "{out}");
    let doc = plandrive::validate_for_drive(&in_comment(PLAN3), 3040, &roster()).unwrap();
    reg.pd_store_posted_plan_at(&group, 3040, doc, "https://example/c/1", 1_100);
    reg.pd_drive_group_with(&group, &gh, 1_200);
    assert_eq!(drive_state(&reg, &group), "plan-review", "{}", status(&reg, &group));

    // Well past the 5-minute drive timeout, still inside the 60-minute window.
    reg.pd_drive_group_with(&group, &gh, 1_200 + 30 * 60_000);
    assert_eq!(
        drive_state(&reg, &group),
        "plan-review",
        "a drive doing what it was told is not stalled: {}",
        status(&reg, &group)
    );
    assert_eq!(held_reason(&reg, &group), "", "and carries no hold reason at all");

    // The control: the suppression ends with the window.
    reg.pd_drive_group_with(&group, &gh, 1_200 + 61 * 60_000);
    assert_eq!(
        drive_state(&reg, &group),
        "boarding",
        "past the window the drive boards — a bounded wait, not an exemption: {}",
        status(&reg, &group)
    );
}

// ── rev-std round 2 ─────────────────────────────────────────────────────────

/// **A worker that reports `done` and produces NO PR is bounded** (round 2,
/// finding 6).
///
/// The `worker-gone` arm deliberately skips a slice whose worker has reported,
/// so this input fell through every arc at once: the resolution loop retried a
/// `gh pr list` that will never find anything, the hand-off needs `pr > 0`, and
/// a slice in `running` makes `running_idle` false so the whole-drive backstop
/// could not see it either. Silent forever — the outcome round 1's finding was
/// filed for, on the round-1 fix's own blind side.
///
/// Three assertions, and the middle one is the point: BEFORE the bound the
/// slice is still `running`, so the hold is the bound's doing and not merely
/// "any tick after a report parks it" — which would break the ordinary case of
/// a worker reporting a moment before its PR is visible.
#[test]
fn a_done_that_never_produces_a_pr_is_parked_once_the_wait_bound_trips() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, _rows) = running(&reg, &repo, &gh, PLAN3);
    reg.pd_drive_group_with(&group, &gh, 1_400);

    let agent = slice_agent(&reg, &group, "P1");
    with_pane(&reg, &agent, 7_101);
    // `done` with a ref that resolves to nothing, and no PR on the branch — the
    // worker violated its own definition of done. Then its pane exits.
    report(&reg, &group, &agent, "done", json!({ "ref": "not-a-pr", "note": "pushed I think" }));
    assert!(reg.mark_agent_dead_for_test(&agent), "the worker pane must exist to be ended");

    // Inside the bound: still running, still retrying, NOT parked.
    reg.pd_drive_group_with(&group, &gh, 1_500);
    assert_eq!(
        slice_state(&reg, &group, "P1"),
        "running",
        "a PR that is merely slow must not be parked: {}",
        status(&reg, &group)
    );
    let waiting = read_record(&reg, &group)["entries"][0]["slices"]["P1"]["pr_wait_since_ms"]
        .as_u64()
        .unwrap_or(0);
    assert!(waiting > 0, "the wait is stamped on the first tick that observes it");

    reg.pd_drive_group_with(&group, &gh, waiting + PR_WAIT_HOLD_MS - 1);
    assert_eq!(slice_state(&reg, &group, "P1"), "running", "still inside the bound");

    // Past it.
    reg.pd_drive_group_with(&group, &gh, waiting + PR_WAIT_HOLD_MS);
    assert_eq!(slice_state(&reg, &group, "P1"), "held", "{}", status(&reg, &group));
    assert_eq!(slice_hold(&reg, &group, "P1"), "pr-missing");
    assert_eq!(
        drive_state(&reg, &group),
        "running",
        "one slice, not the drive — the other slices are unaffected"
    );
    assert!(
        reg.audit_log(&group)
            .into_iter()
            .any(|e| e.action == "pd-slice-held" && e.detail["reason"] == json!("pr-missing")),
        "the hold is on the record with its reason: {:?}",
        audit_actions(&reg, &group)
    );
}

/// **A PR that is merely LATE still gets its hand-off, and the wait is
/// forgotten** (round 2, finding 6 — the other side).
///
/// The bound above must not cost the ordinary case: a worker reports `done` the
/// moment it pushes, and the PR is visible a tick later. This is that case
/// carried past the point where a bound keyed on the report rather than on the
/// wait would have parked it.
#[test]
fn a_late_pr_still_hands_off_and_clears_the_wait() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let (group, _orch, _rows) = running(&reg, &repo, &gh, PLAN3);
    reg.pd_drive_group_with(&group, &gh, 1_400);

    let agent = slice_agent(&reg, &group, "P1");
    with_pane(&reg, &agent, 7_101);
    report(&reg, &group, &agent, "done", json!({ "note": "pushed, PR opening" }));
    reg.pd_drive_group_with(&group, &gh, 1_500);
    assert!(
        read_record(&reg, &group)["entries"][0]["slices"]["P1"]["pr_wait_since_ms"]
            .as_u64()
            .unwrap_or(0)
            > 0,
        "the control: this slice really is waiting"
    );

    // The PR appears, late but inside the bound.
    gh.set_head_pr("feat/3040-p1", 4_777);
    gh.set_pr(4_777, "OPEN", None);
    reg.pd_drive_group_with(&group, &gh, 1_500 + PR_WAIT_HOLD_MS / 2);
    assert_eq!(slice_state(&reg, &group, "P1"), "in-review", "{}", status(&reg, &group));
    assert_eq!(
        read_record(&reg, &group)["entries"][0]["slices"]["P1"]["pr_wait_since_ms"],
        json!(0),
        "and the wait is forgotten, so a later slow tick cannot park a slice already handed off"
    );

    // Well past the bound, with the slice in review: still not parked.
    reg.pd_drive_group_with(&group, &gh, 1_500 + 3 * PR_WAIT_HOLD_MS);
    assert_eq!(slice_state(&reg, &group, "P1"), "in-review", "{}", status(&reg, &group));
    assert_eq!(slice_hold(&reg, &group, "P1"), "");
}

/// **A release rolls back a row the drive's own CLAIM is still holding** (#3160).
///
/// `pd_spawn_slice` writes the board twice: it claims the row as
/// `brand::AUDIT_ACTOR`, and then — once the pane exists — rewrites the assignee
/// to that pane's id. If the second write fails, the record names the agent
/// while the row still names the claimant, and the release's rollback used to
/// check the AGENT alone: it found a stranger on the row, skipped it, and left
/// the row `in-progress` with nobody on it. Readiness is the board's, so that
/// slice never becomes ready again and the resume silently buys nothing — the
/// exact defect `a_resume_releases_a_held_slice_and_still_obeys_the_board`
/// closed, re-entered through the gap between the two writes.
///
/// The failing write is not stageable from here (it is one `upsert_task` call
/// inside a spawn), so this stages its RESULT, which is the whole of what the
/// rollback sees: the row left carrying the claimant. The widened guard
/// (`rollback_is_ours_of`) is what makes it releasable again.
///
/// The last half is the control the widening must not cost: a row a HUMAN holds
/// matches neither of the drive's two names and is still left alone.
#[test]
fn a_release_rolls_back_a_row_still_carrying_the_drives_own_claimant() {
    let repo = Repo::new();
    let (reg, _d) = test_registry();
    let gh = FakeGh::open(&["agent-ready"]);
    let plan = PLAN3.replace("    hold: true\n", "");
    let (group, orch, rows) = running(&reg, &repo, &gh, &plan);
    reg.pd_drive_group_with(&group, &gh, 1_400);
    reg.pd_drive_group_with(&group, &gh, 1_500);

    // Stage the failed post-spawn write: the row goes back to naming the
    // CLAIMANT, which is what it carries between the claim and the rewrite.
    let agent = slice_agent(&reg, &group, "P1");
    assert!(!agent.is_empty(), "the slice really did spawn: {}", status(&reg, &group));
    reg.upsert_task(
        &group,
        brand::AUDIT_ACTOR,
        Some(&rows["P1"]),
        loomux_lib::orchestration::TaskPatch {
            assignee: Some(brand::AUDIT_ACTOR.to_string()),
            ..loomux_lib::orchestration::TaskPatch::default()
        },
    )
    .expect("staging the row the way a failed assignee write leaves it");
    let staged = row(&reg, &group, &rows["P1"]).expect("P1 has a row");
    assert_eq!(staged.status, "in-progress", "the claim still stands: {staged:?}");
    assert_eq!(
        staged.assignee.as_deref(),
        Some(brand::AUDIT_ACTOR),
        "and the row names the claimant, not the pane — the state under test: {staged:?}"
    );
    assert_ne!(
        staged.assignee.as_deref(),
        Some(agent.as_str()),
        "the record and the row really do disagree, which is this test's premise"
    );

    // Park the slice, then the drive, so there is something to resume.
    with_pane(&reg, &agent, 7_310);
    report(&reg, &group, &agent, "blocked", json!({ "note": "stuck" }));
    reg.pd_drive_group_with(&group, &gh, 1_600);
    assert_eq!(slice_hold(&reg, &group, "P1"), "worker-blocked", "{}", status(&reg, &group));
    gh.set_labels(&["bug"]);
    reg.pd_drive_group_with(&group, &gh, 1_700);
    assert_eq!(drive_state(&reg, &group), "held");

    gh.set_labels(&["agent-ready"]);
    let out = reg.resume_plan_drive_with(&group, 3040, &orch, 1_800);
    assert_eq!(out["resumed"], json!(true), "{out}");

    let p1 = row(&reg, &group, &rows["P1"]).expect("P1 still has a row");
    assert_eq!(
        p1.status, "queued",
        "the row is released even though it names the claimant rather than the pane: {p1:?}"
    );
    assert_eq!(p1.assignee, None, "and unassigned, so the re-spawn's claim can succeed: {p1:?}");

    // The control: a row a HUMAN holds is neither of the drive's names, and a
    // release must not touch it. Staged on P3, parked and resumed the same way.
    let p3_agent = slice_agent(&reg, &group, "P3");
    assert!(!p3_agent.is_empty(), "P3 spawned too: {}", status(&reg, &group));
    reg.upsert_task_by_human(
        &group,
        "the human",
        Some(&rows["P3"]),
        loomux_lib::orchestration::TaskPatch {
            assignee: Some("a-human".into()),
            ..loomux_lib::orchestration::TaskPatch::default()
        },
    )
    .expect("a human may take a row");
    with_pane(&reg, &p3_agent, 7_311);
    report(&reg, &group, &p3_agent, "blocked", json!({ "note": "stuck too" }));
    reg.pd_drive_group_with(&group, &gh, 1_900);
    assert_eq!(slice_hold(&reg, &group, "P3"), "worker-blocked");
    gh.set_labels(&["bug"]);
    reg.pd_drive_group_with(&group, &gh, 2_000);
    gh.set_labels(&["agent-ready"]);
    reg.resume_plan_drive_with(&group, 3040, &orch, 2_100);
    let p3 = row(&reg, &group, &rows["P3"]).expect("P3 still has a row");
    assert_eq!(
        p3.assignee.as_deref(),
        Some("a-human"),
        "a human's claim matches neither of the drive's names and is left alone: {p3:?}"
    );
}

/// **The orchestrator is TOLD it has a plan drive, and only where it has one**
/// (#3040 P4).
///
/// The teaching is a conditional fragment in the playbook's
/// `Planning and scheduling` section rather than resident prose, for
/// `REVIEW_DRIVER_NOTE`'s reason: everything it says names machinery a group
/// without `plan_enabled` does not have — four tools, a fence schema, a hold
/// vocabulary — and prose about a mechanism the reader does not have is an
/// invitation to go looking for it. The resident core is unchanged, and pays
/// nothing.
///
/// **The control is the second half, and it is the one that can fail.** The
/// gate is the SECOND switch, so the group that must not see this is not a
/// driverless one — it is a group whose review driver is on and whose plan
/// driver is not, which is the only reading of `plan_enabled` that is a
/// separate consent at all. Asserting the review driver's own note is still
/// there in that same file is what stops "not present" from meaning "the
/// playbook did not render".
#[test]
fn the_playbook_names_the_plan_drive_only_where_the_second_switch_is_on() {
    // A distinctive line of each fragment. Neither appears in the template.
    let plan_marker = "`drive_plan(issue, planner_block?, review_minutes?, base?)`";
    let review_marker = "`drive_review(pr, worker_session, reset_counters?, rounds_already_spent?)`";

    let playbook = |workflow: &str| -> String {
        let repo = Repo::with(workflow);
        let (reg, _dir) = test_registry();
        let (group, _orch) = grouped(&reg, &repo);
        let path = reg
            .state_root()
            .join(group.as_str())
            .join(loomux_lib::orchestration::ORCHESTRATOR_PLAYBOOK_FILE);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} is not readable: {e}", path.display()));
        text
    };

    let on = playbook(WORKFLOW);
    assert!(
        on.contains(plan_marker),
        "a group with `plan_enabled: true` is told what `drive_plan` is: {on}"
    );
    assert!(
        !on.contains("{{"),
        "and the fragment is SUBSTITUTED, not left as template bytes: {on}"
    );

    let off = playbook(WORKFLOW_NO_PLAN_DRIVER);
    assert!(
        !off.contains(plan_marker),
        "a group whose second switch is off is told nothing about a plan drive: {off}"
    );
    assert!(
        off.contains(review_marker),
        "but the REVIEW driver's own note is still there — which is what makes the \
         assertion above about the gate rather than about a playbook that did not render"
    );
    assert!(
        on.contains(review_marker),
        "and both fragments coexist where both switches are on"
    );
}
