//! What a restart does to a run: a second registry over the same state
//! directory is the process coming back up with every pane gone.

use super::*;

/// **A run left WORKING by an earlier process is parked on the first tick,
/// with exactly one notice — and nothing is re-opened.**
///
/// The tick's candidate list is in memory and starts empty, so this is also
/// the proof that the start-up scan finds a run nothing in this process has
/// heard of.
#[test]
fn a_working_run_is_parked_once_on_the_first_tick_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let group = {
        let reg = relaunch_registry(dir.path());
        let (group, _worker, _reviewer) = reviewing(&reg, &repo);
        assert_eq!(state(&reg, &group), "review-wait");
        group
    };

    let reg = relaunch_registry(dir.path());
    assert_eq!(state(&reg, &group), "review-wait", "reading the status parks nothing");
    assert!(open_items(&reg, &group).is_empty());
    let spawns = action_count(&reg, &group, "agent-spawn");
    assert_eq!(spawns, 2, "the control: the earlier process opened a worker and a reviewer");

    reg.qd_driver_tick(T0 + 10 * MIN);
    assert_eq!(state(&reg, &group), "held");
    assert_eq!(held_reason(&reg, &group), "restart");
    let items = open_items(&reg, &group);
    assert_eq!(items.len(), 1, "{items:?}");
    assert!(items[0].text.contains("held (restart)"), "{}", items[0].text);
    assert_eq!(action_count(&reg, &group, "agent-spawn"), spawns, "nothing was re-opened unasked");

    // Once: a later tick neither re-parks nor raises a second item.
    reg.qd_driver_tick(T0 + 11 * MIN);
    reg.qd_driver_tick(T0 + 12 * MIN);
    assert_eq!(open_items(&reg, &group).len(), 1);
    assert_eq!(action_count(&reg, &group, quickdrive::audit_action::RESTART_PARKED), 1);
}

/// **A run that was already parked is left exactly as it was** — its reason,
/// and the one notice it already had.
#[test]
fn a_run_that_was_already_held_keeps_its_reason_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let group = {
        let reg = relaunch_registry(dir.path());
        let (group, worker) = working(&reg, &repo);
        report(&reg, &worker, json!({ "outcome": "blocked", "note": "which parser?" }));
        step(&reg, &group, T0 + 2);
        assert_eq!(held_reason(&reg, &group), "worker-blocked");
        group
    };

    let reg = relaunch_registry(dir.path());
    reg.qd_driver_tick(T0 + 10 * MIN);
    assert_eq!(held_reason(&reg, &group), "worker-blocked", "the hold it had is the hold it has");
    assert_eq!(open_items(&reg, &group).len(), 1, "and its one notice is still the only one");
    assert_eq!(action_count(&reg, &group, quickdrive::audit_action::RESTART_PARKED), 0);
}

/// **Resume after a restart re-opens the session that held the turn, with the
/// brief it was owed** — in a registry that had never heard of the group until
/// the human pressed Resume.
#[test]
fn resume_after_a_restart_reopens_the_recorded_session_with_its_brief() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let (group, worker, session, cwd) = {
        let reg = relaunch_registry(dir.path());
        let (group, worker) = working(&reg, &repo);
        let a = reg.agent(&worker).unwrap();
        (group, worker, a.session_id.clone().expect("claude mints a session id"), a.cwd)
    };

    let reg = relaunch_registry(dir.path());
    assert!(reg.group(&group).is_none(), "the fixture's premise: this process has no such group");
    // Resume without waiting for the tick: it parks the run itself first, so
    // the button means one thing whichever got there first.
    let after = reg.quick_resume_at(&group, T0 + 10 * MIN).expect("the run resumes");
    assert_eq!(after["state"], json!("work-wait"));
    assert!(reg.group(&group).is_some(), "the group was put back from its own record");
    assert_eq!(action_count(&reg, &group, quickdrive::audit_action::RESTART_PARKED), 1);
    assert!(open_items(&reg, &group).is_empty(), "the restart notice is answered by the resume");

    let reopened = pane(&reg, &group, QuickSide::Worker);
    assert_ne!(reopened, worker, "the old pane died with the old process");
    let a = reg.agent(&reopened).expect("a new pane is on the roster");
    assert_eq!(a.role, Role::Worker);
    assert_eq!(a.session_id.as_deref(), Some(session.as_str()), "running the recorded session");
    assert_eq!(a.cwd, cwd, "in the worktree the work is in");
    let brief = lf(&a.task);
    assert!(brief.contains("the human resumed this quick run"), "{brief}");
    assert!(brief.contains("held (restart)"), "{brief}");
    assert!(brief.contains("add a --json flag to the list command"), "{brief}");

    // The run is live again: the tick services it, and the worker's report moves it.
    report(&reg, &reopened, json!({ "outcome": "done", "note": "finished after the restart" }));
    assert_eq!(reg.qd_driver_tick(T0 + 11 * MIN), Some(group.clone()));
    assert_eq!(state(&reg, &group), "review-wait");
}

/// **Reattaching a quick group reads ITS OWN record, by id** — never
/// `next_group_id`, which would resolve the first free group for the repo and
/// could be a different one.
///
/// Two runs on one repository; the second is resumed first. Its roster — the
/// instructions typed for it — must come back as its own, and the first run's
/// group must not be touched.
#[test]
fn resuming_one_of_two_runs_on_a_repo_reattaches_that_run_and_not_the_other() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let (first, second) = {
        let reg = relaunch_registry(dir.path());
        let first = start_with(&reg, &repo, |r| r.work.instructions = "instructions for the FIRST run".into());
        step(&reg, &first, T0 + 1);
        let second = start_with(&reg, &repo, |r| r.work.instructions = "instructions for the SECOND run".into());
        step(&reg, &second, T0 + 1);
        assert_ne!(first, second);
        (first, second)
    };

    let reg = relaunch_registry(dir.path());
    reg.quick_resume_at(&second, T0 + 10 * MIN).expect("the second run resumes");
    let prompt = |g: &GroupId| {
        reg.group(g).and_then(|info| {
            info.guardrails.block_for(Role::Worker).and_then(|b| b.prompt.clone())
        })
    };
    assert_eq!(prompt(&second).as_deref(), Some("instructions for the SECOND run"));
    assert!(reg.group(&first).is_none(), "the other run's group was not reattached in passing");
    assert_eq!(state(&reg, &first), "work-wait", "and its record is as the old process left it");
}
