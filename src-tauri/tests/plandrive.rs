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

use loomux_lib::orchestration::mcp::dispatch;
use loomux_lib::orchestration::mqdriver::CmdOut;
use loomux_lib::orchestration::plandrive::{self, Consent, PdHeldReason, PlanDriveState};
use loomux_lib::orchestration::rddrive::RdRunner;
use loomux_lib::orchestration::{
    Caller, GroupId, Guardrails, OrchRegistry, PdPlanCheck, Role, PD_MAX_GH_PER_TICK,
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
        Repo { _root: root, repo }
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
            calls: std::sync::Mutex::new(Vec::new()),
            barrier: std::sync::Mutex::new(None),
            held: std::sync::atomic::AtomicUsize::new(0),
        }
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
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).push(
            args.iter().map(|s| s.to_string()).collect(),
        );
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
        file["entries"][0]["slice_tasks"],
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
    // `boarding` and `boarding` → `held(awaiting-p3b)` are two of them. The
    // intermediate state is real and a status read can catch it.
    reg2.pd_drive_group_with(&g2, &gh2, 1_200);
    assert_eq!(
        drive_state(&reg2, &g2),
        "boarding",
        "the first tick reaches boarding: {}",
        status(&reg2, &g2)
    );
    reg2.pd_drive_group_with(&g2, &gh2, 1_300);
    assert_eq!(
        held_reason(&reg2, &g2),
        PdHeldReason::AwaitingP3b.as_str(),
        "an agent-ready drive parks on the named hold, not on `complete`: {}",
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
    // (a) the static half.
    for r in PdHeldReason::ALL {
        assert_one_paragraph(&format!("PdHeldReason::{}'s notice_line", r.as_str()), r.notice_line());
    }

    // (b) the composed half. Each of these drives a real arc to the point where
    // it owes a notice, and the notice is read off the delivery the
    // orchestrator's pane really received.
    let mut seen = 0usize;

    // held(awaiting-p3b) — the end of every `agent-ready` drive in this build.
    {
        let repo = Repo::new();
        let (reg, _d) = test_registry();
        let gh = FakeGh::open(&["agent-ready"]);
        let (group, _orch, _planner) = driven(&reg, &repo, &gh);
        let doc = plandrive::validate_for_drive(&in_comment(PLAN), 3040, &roster()).unwrap();
        reg.pd_store_posted_plan_at(&group, 3040, doc, "https://example/c/1", 1_100);
        reg.pd_drive_group_with(&group, &gh, 1_200);
        reg.pd_drive_group_with(&group, &gh, 1_300);
        for t in pane_texts(&reg, &group) {
            assert_one_paragraph("an awaiting-p3b notice", &t);
            seen += 1;
        }
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
        for t in pane_texts(&reg, &group) {
            assert_one_paragraph("a plan-posted-complete notice", &t);
            seen += 1;
        }
    }
    // cancelled — the reconcile's closed-issue notice.
    {
        let repo = Repo::new();
        let (reg, _d) = test_registry();
        let gh = FakeGh::open(&["agent-ready"]);
        let (group, _orch, _planner) = driven(&reg, &repo, &gh);
        gh.set_state("CLOSED");
        reg.pd_drive_group_with(&group, &gh, 1_200);
        for t in pane_texts(&reg, &group) {
            assert_one_paragraph("an issue-closed notice", &t);
            seen += 1;
        }
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
        for t in pane_texts(&reg, &group) {
            assert_one_paragraph("a plan-invalid notice", &t);
            seen += 1;
        }
    }
    // held(plan-missing) via the RESERVATION arm — the literal that shipped
    // broken, reached the only way it can be: through the reconcile.
    {
        let (reg, group, _repo, _d) = reserved_drive();
        let gh = FakeGh::open(&["agent-ready"]);
        reg.pd_drive_group_with(&group, &gh, 9_000);
        for t in pane_texts(&reg, &group) {
            assert_one_paragraph("a reservation-unspawned notice", &t);
            seen += 1;
        }
    }

    // THE POSITIVE CONTROL, and this test needs it more than most: every
    // assertion above is inside a loop over a list that may be empty, so a
    // fixture that delivered nothing would satisfy all of them.
    assert!(
        seen >= 5,
        "the fixtures must really have delivered notices — an empty list passes every assertion \
         in this test: seen={seen}"
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
