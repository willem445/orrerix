//! `request_changes` and the hand-back to the worker: the findings file, and
//! the three tiers of the review driver's ladder the quick drive calls —
//! reuse the idle pane, take over the busy one, resume the session into a new
//! one. One test per tier, each differing in ONE fact about the worker's pane.

use super::*;

/// The findings a fixture reviewer reports. Multi-line, because the line
/// structure of a set of findings is the payload.
const FINDINGS: &str = "1. the --json output is not documented in the README\n2. no test covers an empty list";

fn request_changes(reg: &OrchRegistry, reviewer: &str) -> String {
    report(
        reg,
        reviewer,
        json!({ "outcome": "request_changes", "note": "two gaps", "summary": FINDINGS }),
    )
}

/// **Tier 1 — the worker's pane is alive and idle, so the fix is typed into
/// it.** `request_changes` writes the round's findings to a file before the
/// tool answers, the run goes back to the worker having spent a round, and no
/// second pane is opened on the worker's session.
#[test]
fn request_changes_writes_the_findings_and_hands_back_into_the_workers_idle_pane() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker, reviewer) = reviewing(&reg, &repo);
    // The worker reported `done`, so it is idle; give it a pane a delivery can land in.
    make_deliverable(&reg, &group, &worker, 7301);

    let answer = request_changes(&reg, &reviewer);
    let (_plan, findings, _messages) = reg.qd_document_paths_for_test(&group, 1);
    assert!(findings.is_file(), "the findings are on disk before the tool answers");
    assert_eq!(lf(&std::fs::read_to_string(&findings).unwrap()), FINDINGS);
    assert!(answer.contains("saved to"), "and the reviewer is told where: {answer}");

    let spawns = action_count(&reg, &group, "agent-spawn");
    assert_eq!(spawns, 2, "the control: a worker and a reviewer have been opened so far");
    let out = step(&reg, &group, T0 + 3);
    assert_eq!(out.advanced, Some(("review-wait".to_string(), "fix-wait".to_string())));
    assert_eq!(
        out.handed_to,
        Some(("worker".to_string(), worker.clone(), "reused".to_string())),
        "the fix goes into the pane that is already there"
    );
    assert_eq!(action_count(&reg, &group, "agent-spawn"), spawns, "and no pane was opened for it");

    let typed = texts_to(&reg, &group, &worker);
    let fix = typed.last().expect("the fix brief was typed into the worker's pane");
    assert!(fix.contains("The reviewer asked for changes"), "{fix}");
    assert!(fix.contains("no test covers an empty list"), "the findings are inlined: {fix}");
    assert!(fix.contains("report(outcome=done"), "{fix}");

    let s = status(&reg, &group);
    assert_eq!(s["review_rounds"], json!(1), "one round spent");
    assert_eq!(s["round"], json!(2), "and the run is on its second review");
    assert_eq!(s["reviews_total"], json!(1));

    // …and the worker's `done` sends it back to the SAME reviewer pane.
    make_deliverable(&reg, &group, &reviewer, 7302);
    report(&reg, &worker, json!({ "outcome": "done", "note": "documented it and added the test" }));
    let back = step(&reg, &group, T0 + 4);
    assert_eq!(back.advanced, Some(("fix-wait".to_string(), "review-wait".to_string())));
    assert_eq!(
        back.handed_to,
        Some(("reviewer".to_string(), reviewer.clone(), "reused".to_string()))
    );
    let again = texts_to(&reg, &group, &reviewer);
    let brief = again.last().expect("the second review brief was typed into the reviewer's pane");
    assert!(brief.contains("review round 2 of at most 3"), "{brief}");
    assert!(brief.contains("documented it and added the test"), "{brief}");
}

/// **Tier 2 — the worker's pane is alive but BUSY, so it is taken over rather
/// than a second pane being opened beside it** (#3203's rule, inherited).
///
/// The pane is made busy the way the product makes one busy: the worker says
/// it is still going. That `report(progress)` is out of turn and moves
/// nothing, but it clears the pane's idle stamp, which is the one fact the
/// reuse tier reads.
#[test]
fn a_busy_worker_pane_is_taken_over_rather_than_a_second_one_opened() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker, reviewer) = reviewing(&reg, &repo);
    make_deliverable(&reg, &group, &worker, 7311);
    report(&reg, &worker, json!({ "outcome": "progress", "note": "tidying while I wait" }));
    assert!(
        reg.agent(&worker).unwrap().idle_since_ms.is_none(),
        "the fixture's premise: the worker's pane is no longer idle"
    );

    request_changes(&reg, &reviewer);
    let spawns = action_count(&reg, &group, "agent-spawn");
    let out = step(&reg, &group, T0 + 3);
    assert_eq!(
        out.handed_to,
        Some(("worker".to_string(), worker.clone(), "taken-over".to_string())),
        "{out:?}"
    );
    assert_eq!(action_count(&reg, &group, "agent-spawn"), spawns, "no second pane on the session");
    assert!(
        texts_to(&reg, &group, &worker).last().is_some_and(|t| t.contains("The reviewer asked for changes")),
        "the brief is queued in the pane that is working"
    );
}

/// **Tier 3 — the worker's pane is gone, so its session is resumed into a new
/// one**, in the worktree the work is in. The pane it replaces is still the
/// run's, and no longer holds a turn.
#[test]
fn a_dead_worker_pane_is_resumed_from_its_session_in_the_same_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker, reviewer) = reviewing(&reg, &repo);
    let before = reg.agent(&worker).unwrap();
    let session = before.session_id.clone().expect("claude mints a session id at spawn");

    // The worker's pane closes while the REVIEWER holds the turn. That is not a
    // hold — the run is not waiting on the worker — which is the control for
    // the dead-pane hold `exits.rs` pins on the pane that does hold it.
    reg.mark_dead(&worker, Some(0));
    assert_eq!(step(&reg, &group, T0 + 3).advanced, None, "a pane the run is not waiting on may close");
    assert_eq!(state(&reg, &group), "review-wait");

    request_changes(&reg, &reviewer);
    let spawns = action_count(&reg, &group, "agent-spawn");
    let out = step(&reg, &group, T0 + 4);
    let (side, resumed, how) = out.handed_to.clone().expect("the fix is handed back");
    assert_eq!((side.as_str(), how.as_str()), ("worker", "resumed"), "{out:?}");
    assert_ne!(resumed, worker, "into a new pane");
    assert_eq!(action_count(&reg, &group, "agent-spawn"), spawns + 1, "exactly one pane was opened");

    let after = reg.agent(&resumed).unwrap();
    assert_eq!(after.session_id.as_deref(), Some(session.as_str()), "running the SAME session");
    assert_eq!(after.cwd, before.cwd, "in the worktree the work is in");
    assert!(after.task.contains("The reviewer asked for changes"), "{}", after.task);

    assert_eq!(pane(&reg, &group, QuickSide::Worker), resumed);
    assert_eq!(
        reg.qd_owner(&group, &worker),
        Some(QdOwner::Pane { side: QuickSide::Worker, current: false, holds_turn: false }),
        "the pane it replaced is still the run's, and holds no turn"
    );
    assert_eq!(
        reg.qd_owner(&group, &resumed),
        Some(QdOwner::Pane { side: QuickSide::Worker, current: true, holds_turn: true })
    );
}

/// **A reviewer's plain `done` is a request for changes, never an approval** —
/// at the tool boundary, through the real `report` arm. The run goes back to
/// the worker with the note as the findings.
#[test]
fn a_reviewers_plain_done_sends_the_work_back_and_never_ends_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker, reviewer) = reviewing(&reg, &repo);
    make_deliverable(&reg, &group, &worker, 7321);

    report(&reg, &reviewer, json!({ "outcome": "done", "note": "the empty case is wrong" }));
    let out = step(&reg, &group, T0 + 3);
    assert_eq!(
        out.advanced,
        Some(("review-wait".to_string(), "fix-wait".to_string())),
        "a reviewer that forgot to say `approved` has not approved"
    );
    assert!(open_items(&reg, &group).is_empty(), "so nothing told the human it was approved");
    let (_plan, findings, _messages) = reg.qd_document_paths_for_test(&group, 1);
    assert_eq!(
        lf(&std::fs::read_to_string(findings).unwrap()),
        "the empty case is wrong",
        "the note stands in for the findings it did not send"
    );
}
