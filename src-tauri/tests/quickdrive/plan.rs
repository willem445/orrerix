//! The plan step: a planner opened first, its plan written to a file, and the
//! worker opened with it.

use super::*;

const PLAN: &str = "1. add `--json` to `list` in src/cli.rs\n2. serialise with the existing Row type\n3. cover the empty list in tests/list.rs";

fn planning(reg: &OrchRegistry, repo: &Repo) -> (GroupId, String) {
    let group = start_with(reg, repo, |r| r.plan_step = true);
    assert_eq!(state(reg, &group), "plan-wait");
    let out = step(reg, &group, T0 + 1);
    let (side, planner, how) = out.handed_to.clone().expect("the planner is opened first");
    assert_eq!((side.as_str(), how.as_str()), ("planner", "opened"));
    (group, planner)
}

/// **The planner is opened read-only in the repository, with no worktree**, and
/// nothing else is opened until it reports.
#[test]
fn a_run_with_a_plan_step_opens_the_planner_first_and_only_the_planner() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, planner) = planning(&reg, &repo);

    let p = reg.agent(&planner).unwrap();
    assert_eq!(p.role, Role::Planner);
    assert_eq!(p.cwd.replace('\\', "/"), repo.path(), "a planner reads the repo itself");
    assert_eq!(p.branch, None, "and cuts no branch");
    assert_eq!(live_agents(&reg, &group), vec![planner], "no worker is opened before the plan exists");
    assert_eq!(pane(&reg, &group, QuickSide::Worker), "");
}

/// **The planner's `report(done)` writes its summary to `plan.md`, and the
/// worker is opened with the plan in its brief and the file named.**
#[test]
fn the_planners_done_writes_the_plan_and_opens_the_worker_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, planner) = planning(&reg, &repo);
    let (plan_path, _findings, _messages) = reg.qd_document_paths_for_test(&group, 1);
    assert!(!plan_path.exists(), "the control: there is no plan before the planner reports");

    let answer = report(&reg, &planner, json!({ "outcome": "done", "note": "three steps", "summary": PLAN }));
    assert!(plan_path.is_file(), "the plan is on disk before the tool answers");
    assert_eq!(lf(&std::fs::read_to_string(&plan_path).unwrap()), PLAN);
    assert!(answer.contains(&plan_path.to_string_lossy().to_string()), "the planner is told where: {answer}");

    let out = step(&reg, &group, T0 + 2);
    assert_eq!(out.advanced, Some(("plan-wait".to_string(), "work-wait".to_string())));
    let (side, worker, how) = out.handed_to.clone().expect("the worker is opened");
    assert_eq!((side.as_str(), how.as_str()), ("worker", "opened"));
    let task = lf(&reg.agent(&worker).unwrap().task);
    assert!(task.contains("serialise with the existing Row type"), "the plan is in the brief: {task}");
    assert!(task.contains(&plan_path.to_string_lossy().to_string()), "and the file is named: {task}");
    assert!(task.contains("work to it"), "{task}");
    assert_eq!(status(&reg, &group)["plan_path"], json!(plan_path.to_string_lossy()));
}

/// **A planner that reports `blocked` parks the run**, and one whose pane
/// closes without a report parks it too — in both cases with no worker opened.
#[test]
fn a_blocked_or_vanished_planner_parks_the_run_and_opens_no_worker() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, planner) = planning(&reg, &repo);
    report(&reg, &planner, json!({ "outcome": "blocked", "note": "the task names a file that does not exist" }));
    step(&reg, &group, T0 + 2);
    assert_eq!(held_reason(&reg, &group), "planner-blocked");
    assert_eq!(pane(&reg, &group, QuickSide::Worker), "", "no worker was opened");
    assert_eq!(open_items(&reg, &group).len(), 1);

    let repo2 = Repo::new();
    let (gone, planner2) = planning(&reg, &repo2);
    reg.mark_dead(&planner2, Some(1));
    step(&reg, &gone, T0 + 2);
    assert_eq!(held_reason(&reg, &gone), "planner-gone");
    assert_eq!(pane(&reg, &gone, QuickSide::Worker), "");
}

/// **A planner on a CLI that cannot be held read-only is refused at the
/// start**, with the containment sentence — the same check a spawn makes,
/// asked while the launcher form is still open. pi can deny edits (so it may
/// review) and cannot deny its shell (so it may not plan).
#[test]
fn a_planner_its_cli_cannot_contain_is_refused_with_the_containment_note() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let mut req = request(&repo);
    req.plan_step = true;
    req.plan.cli = "pi".into();
    let err = reg.quick_start_at(req, T0).expect_err("pi cannot host a planner");
    assert!(err.contains("the plan step cannot run on pi"), "{err}");

    // The control, on the other side of the same line: pi may REVIEW.
    let mut ok = request(&repo);
    ok.review.cli = "pi".into();
    reg.quick_start_at(ok, T0).expect("pi can be held to no-edits, so it can review");
}

/// **A run with the plan step off never opens a planner** — and the positive
/// control is that it did open something.
#[test]
fn a_run_without_a_plan_step_never_opens_a_planner() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker, reviewer) = reviewing(&reg, &repo);
    report(&reg, &reviewer, json!({ "outcome": "approved", "note": "fine" }));
    step(&reg, &group, T0 + 3);
    assert_eq!(state(&reg, &group), "satisfied");

    assert_eq!(action_count(&reg, &group, "agent-spawn"), 2, "a worker and a reviewer, and nothing else");
    assert_eq!(pane(&reg, &group, QuickSide::Planner), "");
    for a in [&worker, &reviewer] {
        assert_ne!(reg.agent(a).unwrap().role, Role::Planner);
    }
    let (plan_path, _f, _m) = reg.qd_document_paths_for_test(&group, 1);
    assert!(!plan_path.exists(), "and no plan was written");
    assert!(!lf(&reg.agent(&worker).unwrap().task).contains("The planner wrote a plan"));
}
