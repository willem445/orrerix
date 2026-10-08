//! Starting a run and moving it: the group it is minted in, the first pane,
//! the `report` interception, and the reviewer's workspace.

use super::*;

/// **A quick run is a group with no root.** `quick_start` mints the group and
/// records the run; it opens no pane, because the launcher has to bind its tab
/// to the group before a pane can be placed. The first step opens exactly one
/// pane — the worker — in a worktree of its own.
#[test]
fn a_run_starts_with_no_pane_and_its_first_step_opens_one_worker_in_a_group_with_no_root() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let group = start(&reg, &repo);

    assert!(reg.is_quick_group(&group), "the group carries the quick marker");
    assert_eq!(state(&reg, &group), "work-wait");
    assert_eq!(status(&reg, &group)["brief_pending"], json!(true));
    assert!(live_agents(&reg, &group).is_empty(), "starting a run opens no pane");

    let out = step(&reg, &group, T0 + 1);
    let (side, worker, how) = out.handed_to.clone().expect("the first step hands the turn over");
    assert_eq!((side.as_str(), how.as_str()), ("worker", "opened"));
    assert_eq!(live_agents(&reg, &group), vec![worker.clone()], "exactly one pane is open");

    let a = reg.agent(&worker).expect("the worker is on the roster");
    assert_eq!(a.role, Role::Worker);
    assert!(!a.role.is_root(), "no pane in a quick group is a root");
    assert_ne!(
        a.cwd.replace('\\', "/"),
        repo.path(),
        "the worker is in a worktree of its own, never the human's checkout"
    );
    assert_eq!(status(&reg, &group)["cwd"], json!(a.cwd), "and the run recorded where");
    assert_eq!(status(&reg, &group)["brief_pending"], json!(false));
    // The brief it was opened with is the task and the way out of the turn.
    assert!(a.task.contains("add a --json flag to the list command"), "{}", a.task);
    assert!(a.task.contains("report(outcome=done"), "{}", a.task);
    assert_eq!(pane(&reg, &group, QuickSide::Worker), worker);
}

/// **A step that finds nothing to do does nothing**: the worker is still
/// working, so a second step neither opens a pane nor moves the run.
#[test]
fn a_step_with_no_report_opens_nothing_and_moves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker) = working(&reg, &repo);
    let spawns = action_count(&reg, &group, "agent-spawn");
    assert_eq!(spawns, 1, "the control: the first step really did spawn the worker");

    let out = step(&reg, &group, T0 + MIN);
    assert_eq!(out.advanced, None);
    assert_eq!(out.handed_to, None);
    assert!(!out.notice);
    assert_eq!(action_count(&reg, &group, "agent-spawn"), spawns);
    assert_eq!(live_agents(&reg, &group), vec![worker]);
    assert_eq!(state(&reg, &group), "work-wait");
}

/// **The worker's `report(done)` is consumed by the run, not delivered** — and
/// the next step opens a reviewer in the WORKER's own worktree.
///
/// The cwd is the property the whole no-PR design rests on: the reviewer reads
/// the worker's uncommitted work where it is. It is opened with no worktree of
/// its own (`use_worktree: false` + `cwd_override`), which is why its roster
/// row carries no branch — and that absence is what keeps the reviewer-scratch
/// reclaim from ever treating the worker's worktree as scratch when the
/// reviewer's pane dies.
#[test]
fn the_workers_done_is_consumed_and_opens_a_reviewer_in_the_workers_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker) = working(&reg, &repo);

    let answer =
        report(&reg, &worker, json!({ "outcome": "done", "note": "added the flag; tests pass" }));
    assert!(answer.contains("consumed by the quick run"), "the tool says what happened: {answer}");
    // Consumed — the positive control for the absence below.
    let consumed = audit_details(&reg, &group, quickdrive::audit_action::CONSUMED);
    assert_eq!(consumed.len(), 1, "{consumed:?}");
    assert_eq!(consumed[0]["holds_turn"], json!(true));
    // …and delivered to nobody: the worker's words were typed into no pane.
    assert!(
        !delivered_texts(&reg, &group).iter().any(|t| t.contains("added the flag")),
        "a consumed report reaches no pane: {:?}",
        delivered_texts(&reg, &group)
    );
    assert_eq!(state(&reg, &group), "work-wait", "the report is acted on by the STEP, not the tool");

    let out = step(&reg, &group, T0 + 2);
    assert_eq!(out.advanced, Some(("work-wait".to_string(), "review-wait".to_string())));
    let (side, reviewer, how) = out.handed_to.clone().expect("the step opens the reviewer");
    assert_eq!((side.as_str(), how.as_str()), ("reviewer", "opened"));

    let w = reg.agent(&worker).unwrap();
    let r = reg.agent(&reviewer).unwrap();
    assert_eq!(r.role, Role::Reviewer);
    assert_eq!(r.cwd, w.cwd, "the reviewer is opened in the worker's own worktree");
    assert_eq!(r.branch, None, "and cut no worktree or branch of its own");
    assert_eq!(live_agents(&reg, &group).len(), 2, "the worker's pane is left open");
    // The brief it was opened with carries the worker's own words and where to look.
    assert!(r.task.contains("added the flag; tests pass"), "{}", r.task);
    assert!(r.task.contains("git diff main...HEAD"), "{}", r.task);
    assert!(r.task.contains(&w.cwd), "{}", r.task);
}

/// **A report from the pane that does NOT hold the turn moves nothing**, and is
/// told so. The worker has handed the work over; its second `done` is about a
/// turn it no longer has.
#[test]
fn a_report_out_of_turn_is_recorded_and_moves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker, reviewer) = reviewing(&reg, &repo);

    let answer = report(&reg, &worker, json!({ "outcome": "done", "note": "also tidied a comment" }));
    assert!(answer.contains("not this pane's turn"), "{answer}");
    let out = step(&reg, &group, T0 + 3);
    assert_eq!(out.advanced, None, "the reviewer still holds the turn: {out:?}");
    assert_eq!(state(&reg, &group), "review-wait");
    // The control: the pane that DOES hold the turn moves it with the same call.
    report(&reg, &reviewer, json!({ "outcome": "approved", "note": "looks right" }));
    assert_eq!(
        step(&reg, &group, T0 + 4).advanced,
        Some(("review-wait".to_string(), "satisfied".to_string()))
    );
}

/// **The interception is keyed on the group AND the agent, and an ordinary
/// group is untouched** — the hook class's negative control
/// (`.claude/skills/add-orch-tool`), with operands that collide as far as this
/// codebase allows: one registry, one repository, two groups, and the
/// identical tool call from a worker in each.
///
/// In the quick group the worker's `done` is consumed and typed nowhere. In
/// the ordinary group the same call reaches the orchestrator's pane, exactly
/// as it always did.
#[test]
fn an_ordinary_groups_report_still_reaches_its_orchestrator() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (quick, quick_worker) = working(&reg, &repo);

    // A second group on the same repo: an ordinary orchestration.
    let plain = reg
        .create_group(&repo.path(), Guardrails { agent_cli: "claude".into(), ..Guardrails::default() })
        .unwrap()
        .id;
    assert_ne!(plain, quick, "the two launches must not share a group");
    let orch = reg.spawn_agent(&plain, Role::Orchestrator, "orch", "", false, None).unwrap();
    let plain_worker = reg.spawn_agent(&plain, Role::Worker, "w", "", false, None).unwrap();
    make_deliverable(&reg, &plain, &orch.id, 9001);

    assert_eq!(reg.qd_owner(&plain, &plain_worker.id), None, "not a quick group: nobody owns it");
    assert!(
        matches!(
            reg.qd_owner(&quick, &quick_worker),
            Some(QdOwner::Pane { side: QuickSide::Worker, current: true, holds_turn: true })
        ),
        "the quick run owns its own worker: {:?}",
        reg.qd_owner(&quick, &quick_worker)
    );

    let args = json!({ "outcome": "done", "note": "the same words from both" });
    let plain_answer = report(&reg, &plain_worker.id, args.clone());
    let quick_answer = report(&reg, &quick_worker, args);

    assert_eq!(plain_answer, "reported to orchestrator");
    let to_orch = texts_to(&reg, &plain, &orch.id);
    assert_eq!(to_orch.len(), 1, "the ordinary report was typed into its orchestrator: {to_orch:?}");
    assert!(to_orch[0].contains("the same words from both"), "{to_orch:?}");
    assert_eq!(action_count(&reg, &plain, quickdrive::audit_action::CONSUMED), 0);

    assert!(quick_answer.contains("consumed by the quick run"), "{quick_answer}");
    assert_eq!(action_count(&reg, &quick, quickdrive::audit_action::CONSUMED), 1);
    assert!(
        !delivered_texts(&reg, &quick).iter().any(|t| t.contains("the same words from both")),
        "and the quick one reached no pane: {:?}",
        delivered_texts(&reg, &quick)
    );
}

/// **The tick looks only at runs that are working**, so a finished run costs
/// no wake at all, and nothing keeps polling once the task has ended.
#[test]
fn the_tick_services_a_working_run_and_ignores_it_once_it_has_ended() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let group = start_with(&reg, &repo, |r| r.review_step = false);

    // The tick is what opens the first pane when nobody asked for a step.
    assert_eq!(reg.qd_driver_tick(T0 + 1), Some(group.clone()));
    let worker = pane(&reg, &group, QuickSide::Worker);
    assert!(!worker.is_empty(), "the tick delivered the pending brief");

    report(&reg, &worker, json!({ "outcome": "done", "note": "finished" }));
    assert_eq!(reg.qd_driver_tick(T0 + 2), Some(group.clone()));
    assert_eq!(state(&reg, &group), "satisfied", "a run with no review step ends on the worker's done");

    // The run is over: the tick has no candidate, wake after wake.
    assert_eq!(reg.qd_driver_tick(T0 + 3), None);
    assert_eq!(reg.qd_driver_tick(T0 + 1_000 * MIN), None);
    assert_eq!(state(&reg, &group), "satisfied");
}

/// **A launch on the same repo is never handed a group whose run has not
/// ended** — neither one whose first pane has not opened yet, nor one that is
/// parked with every pane closed.
///
/// A group id is chosen by liveness, and both of those groups have no live
/// agent. Before `next_group_id` read the run, the second launch here was
/// handed the first one's group and overwrote its record.
#[test]
fn a_second_launch_never_takes_the_group_of_a_run_that_has_not_ended() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();

    // Started, and no pane yet: the group has no live agent.
    let first = start(&reg, &repo);
    assert!(live_agents(&reg, &first).is_empty(), "the fixture's premise");
    let second = start(&reg, &repo);
    assert_ne!(second, first, "the second start must mint its own group");
    assert_eq!(state(&reg, &first), "work-wait", "and the first run's record is untouched");

    // Parked, with its only pane gone: still no live agent, still not free.
    let out = step(&reg, &first, T0 + 1);
    let (_, worker, _) = out.handed_to.clone().expect("the worker opens");
    reg.mark_dead(&worker, Some(1));
    step(&reg, &first, T0 + 2);
    assert_eq!(held_reason(&reg, &first), "worker-gone");
    assert!(live_agents(&reg, &first).is_empty(), "the fixture's premise, again");
    let third = reg
        .create_group(&repo.path(), Guardrails { agent_cli: "claude".into(), ..Guardrails::default() })
        .unwrap()
        .id;
    assert_ne!(third, first, "an ordinary launch does not take a parked run's group either");
    assert!(reg.is_quick_group(&first), "so its marker and its run are still there to resume");

    // The control: once the run has ENDED, its group is free again.
    reg.quick_cancel_at(&first, T0 + 3).expect("the parked run stops");
    let fourth = reg
        .create_group(&repo.path(), Guardrails { agent_cli: "claude".into(), ..Guardrails::default() })
        .unwrap()
        .id;
    assert_eq!(fourth, first, "an ended run with no live pane frees its group id");
    assert!(!reg.is_quick_group(&fourth), "and the reused group no longer carries the marker");
}

/// **Every refusal `quick_start` makes happens before anything is created.**
#[test]
fn a_start_that_cannot_run_is_refused_before_a_group_exists() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let groups_before = std::fs::read_dir(dir.path()).map(|d| d.count()).unwrap_or(0);

    let mut empty = request(&repo);
    empty.task = "   \n ".into();
    let err = reg.quick_start_at(empty, T0).expect_err("an empty task is refused");
    assert!(err.contains("needs a description"), "{err}");

    let mut bad_cli = request(&repo);
    bad_cli.work.cli = "emacs".into();
    let err = reg.quick_start_at(bad_cli, T0).expect_err("an unknown CLI is refused");
    assert!(err.contains("unsupported CLI") && err.contains("work step"), "{err}");

    // codex cannot be held read-only, so it cannot host the review step — the
    // same containment check a spawn makes, asked while the form is still open.
    let mut codex_review = request(&repo);
    codex_review.review.cli = "codex".into();
    let err = reg.quick_start_at(codex_review, T0).expect_err("a reviewer codex cannot host");
    assert!(err.contains("the review step cannot run on codex"), "{err}");

    assert_eq!(
        std::fs::read_dir(dir.path()).map(|d| d.count()).unwrap_or(0),
        groups_before,
        "none of the three refusals created a group directory"
    );
    // The control: the same repo, an acceptable request — and a step that is
    // OFF is not checked at all, so codex on a review that will not run is fine.
    let mut ok = request(&repo);
    ok.review_step = false;
    ok.review.cli = "codex".into();
    reg.quick_start_at(ok, T0).expect("a review step that is off is not a reason to refuse");
}
