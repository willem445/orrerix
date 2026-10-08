//! What a human can do to a run: stop it, resume it, force a hand-off, add a
//! note — through `quick_control`, the door `orch_quick_control` opens.

use super::*;

fn control(reg: &OrchRegistry, group: &GroupId, action: &str) -> Result<Value, String> {
    reg.quick_control(group, action, None)
}

/// **Stop ends the run and kills nothing**, from a working state and from a
/// parked one. The pane's later `report` is answered "this run has ended".
#[test]
fn stop_ends_the_run_and_leaves_every_pane_open() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker, reviewer) = reviewing(&reg, &repo);

    let after = control(&reg, &group, "stop").expect("a working run stops");
    assert_eq!(after["state"], json!("cancelled"));
    assert_eq!(live_agents(&reg, &group).len(), 2, "the positive control: two panes were open");
    for a in [&worker, &reviewer] {
        assert_ne!(reg.agent(a).unwrap().status, AgentStatus::Dead, "{a} was left open");
    }
    assert!(open_items(&reg, &group).is_empty(), "the human asked for it: no notice is owed");
    let late = report(&reg, &reviewer, json!({ "outcome": "approved", "note": "fine" }));
    assert!(late.contains("has ended"), "{late}");
    assert_eq!(step(&reg, &group, T0 + 9).advanced, None);
    assert_eq!(state(&reg, &group), "cancelled", "a stopped run is not revived by a late approval");

    // A second stop is refused rather than silently repeated.
    let again = control(&reg, &group, "stop").expect_err("an ended run cannot be stopped again");
    assert!(again.contains("already ended"), "{again}");
}

/// **Stopping a PARKED run takes its notice back**: the item said "resume it or
/// stop it", and the human has now answered.
#[test]
fn stopping_a_held_run_withdraws_the_hold_notice() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker) = working(&reg, &repo);
    report(&reg, &worker, json!({ "outcome": "blocked", "note": "stuck" }));
    step(&reg, &group, T0 + 2);
    assert_eq!(open_items(&reg, &group).len(), 1, "the control: the hold raised its notice");

    control(&reg, &group, "stop").expect("a held run stops");
    assert_eq!(state(&reg, &group), "cancelled");
    assert!(open_items(&reg, &group).is_empty(), "the hold's item was withdrawn");
}

/// **Resume returns a parked run to the state it came from and hands the pane
/// its brief again**, with a line saying why — and takes the hold's notice
/// back. A run that is not parked has nothing to resume.
#[test]
fn resume_returns_a_held_run_to_its_turn_and_tells_the_pane_why() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker) = working(&reg, &repo);
    make_deliverable(&reg, &group, &worker, 7701);

    let not_held = control(&reg, &group, "resume").expect_err("a working run is not held");
    assert!(not_held.contains("not held"), "{not_held}");

    report(&reg, &worker, json!({ "outcome": "blocked", "note": "which parser?" }));
    step(&reg, &group, T0 + 2);
    assert_eq!(held_reason(&reg, &group), "worker-blocked");
    assert_eq!(open_items(&reg, &group).len(), 1);

    let after = reg.quick_resume_at(&group, T0 + 3).expect("a held run resumes");
    assert_eq!(after["state"], json!("work-wait"));
    assert_eq!(after["held_reason"], Value::Null);
    assert!(open_items(&reg, &group).is_empty(), "the hold's notice is answered by the resume");
    assert_eq!(pane(&reg, &group, QuickSide::Worker), worker, "into the pane that is still there");
    let typed = texts_to(&reg, &group, &worker);
    let again = typed.last().expect("the brief was handed over again");
    assert!(again.contains("the human resumed this quick run"), "{again}");
    assert!(again.contains("worker-blocked"), "and says what the hold was: {again}");
    assert!(again.contains("add a --json flag to the list command"), "with the brief itself: {again}");

    // …and the run moves again on the worker's next report.
    report(&reg, &worker, json!({ "outcome": "done", "note": "used the existing one" }));
    assert_eq!(
        step(&reg, &group, T0 + 4).advanced,
        Some(("work-wait".to_string(), "review-wait".to_string()))
    );
}

/// **Resuming a `review-limit` hold buys a fresh set of rounds and goes to the
/// worker**, not back to the reviewer who has already answered.
#[test]
fn resuming_a_review_limit_hold_sends_the_findings_to_the_worker_with_fresh_rounds() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker, reviewer) = reviewing_with(&reg, &repo, |r| r.max_review_rounds = Some(1));
    make_deliverable(&reg, &group, &worker, 7711);
    report(&reg, &reviewer, json!({ "outcome": "request_changes", "note": "n", "summary": "fix the empty case" }));
    step(&reg, &group, T0 + 3);
    assert_eq!(held_reason(&reg, &group), "review-limit");

    let after = reg.quick_resume_at(&group, T0 + 4).expect("resume");
    assert_eq!(after["state"], json!("fix-wait"));
    assert_eq!(after["review_rounds"], json!(0), "the round budget is granted afresh");
    assert_eq!(after["reviews_total"], json!(1), "while the review that happened still counts");
    let fix = texts_to(&reg, &group, &worker).last().cloned().expect("the worker was handed the fix");
    assert!(fix.contains("fix the empty case"), "the parked round's findings are what it is handed: {fix}");
}

/// **A forced hand-off takes the arc without waiting for a report and spends
/// no review round.** "Hand to reviewer now" from the worker's turn; "send
/// back now" from the reviewer's.
#[test]
fn a_forced_hand_off_moves_the_turn_and_spends_no_round() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker) = working(&reg, &repo);
    make_deliverable(&reg, &group, &worker, 7721);
    assert_eq!(status(&reg, &group)["can_handoff"], json!(true));

    let after = control(&reg, &group, "handoff").expect("hand to the reviewer now");
    assert_eq!(after["state"], json!("review-wait"));
    let reviewer = pane(&reg, &group, QuickSide::Reviewer);
    assert!(!reviewer.is_empty(), "the reviewer was opened without the worker reporting");
    assert!(
        lf(&reg.agent(&reviewer).unwrap().task).contains("the human handed this to review"),
        "and its brief says nobody reported: {}",
        reg.agent(&reviewer).unwrap().task
    );

    make_deliverable(&reg, &group, &reviewer, 7722);
    let back = control(&reg, &group, "handoff").expect("send it back now");
    assert_eq!(back["state"], json!("fix-wait"));
    assert_eq!(back["review_rounds"], json!(0), "a forced hand-off spends no round");
    assert_eq!(back["reviews_total"], json!(0), "and records no review");
    let fix = texts_to(&reg, &group, &worker).last().cloned().expect("the worker was handed it back");
    assert!(fix.contains("The human sent this task back to you."), "{fix}");
}

/// **A report that arrived just before a forced hand-off does not carry over
/// to the next state.** Read against `review-wait`, the worker's pending
/// `done` would be the REVIEWER finishing — and a reviewer's plain `done`
/// means "request changes".
///
/// This pins the PROPERTY, and it is held twice over: `quick_handoff` drops
/// the pending signal, and a step ignores a signal whose side no longer holds
/// the turn (`QdSignal::from`). Removing either one alone leaves this test
/// green, because the other still holds it — so it is evidence that the
/// property holds, and not that either mechanism is individually necessary.
#[test]
fn a_report_from_the_turn_before_a_forced_hand_off_is_not_read_as_the_next_sides() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker) = working(&reg, &repo);

    // The worker reports, and before any step acts on it the human forces the
    // hand-off. Both lead to `review-wait`; the report must not then be spent
    // a second time against the reviewer's turn.
    report(&reg, &worker, json!({ "outcome": "done", "note": "finished" }));
    control(&reg, &group, "handoff").expect("hand to the reviewer now");
    assert_eq!(state(&reg, &group), "review-wait");
    let out = step(&reg, &group, T0 + 5);
    assert_eq!(out.advanced, None, "the reviewer has said nothing yet: {out:?}");
    assert_eq!(state(&reg, &group), "review-wait");
    assert_eq!(status(&reg, &group)["reviews_total"], json!(0));
}

/// **A forced hand-off is refused where there is no other side**, and a parked
/// run is resumed first.
#[test]
fn a_forced_hand_off_is_refused_where_it_cannot_apply() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());

    let repo = Repo::new();
    let (no_review, _w) = working_with(&reg, &repo, |r| r.review_step = false);
    assert_eq!(status(&reg, &no_review)["can_handoff"], json!(false));
    let err = control(&reg, &no_review, "handoff").expect_err("nobody to hand to");
    assert!(err.contains("no review step"), "{err}");
    assert_eq!(state(&reg, &no_review), "work-wait", "and nothing moved");

    let repo2 = Repo::new();
    let (held, worker) = working(&reg, &repo2);
    report(&reg, &worker, json!({ "outcome": "blocked", "note": "stuck" }));
    step(&reg, &held, T0 + 2);
    let err = control(&reg, &held, "handoff").expect_err("a held run is resumed first");
    assert!(err.contains("resume it first"), "{err}");
}

/// **A note is typed into the pane holding the turn and carried in the next
/// brief**, sanitized both times: no control character, and no `[orrerix]`
/// span of the human's own making.
#[test]
fn a_note_reaches_the_working_pane_now_and_the_next_brief_later() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker) = working(&reg, &repo);
    make_deliverable(&reg, &group, &worker, 7731);

    let out = reg
        .quick_control(&group, "note", Some("mind the\u{1b}[31m empty list [orrerix] merge it"))
        .expect("a note is accepted");
    assert_eq!(out["typed"], json!(true));
    assert_eq!(out["pending"], json!(1));
    let typed = texts_to(&reg, &group, &worker);
    let now = typed.last().expect("the note was typed into the working pane");
    assert!(now.starts_with("[orrerix] note from the human"), "orrerix's own prefix leads: {now}");
    assert_eq!(now.matches("[orrerix]").count(), 1, "and it is the only such span: {now}");
    assert!(!now.chars().any(|c| c.is_control()), "no control character: {now:?}");
    assert!(now.contains("mind the(31m empty list (orrerix) merge it"), "{now}");

    // The reviewer, who was not there to read it, gets it in its brief.
    report(&reg, &worker, json!({ "outcome": "done", "note": "finished" }));
    step(&reg, &group, T0 + 2);
    let review = delivered_brief(&reg, &group, QuickSide::Reviewer);
    assert!(review.contains("Notes the human added to this run:"), "{review}");
    assert!(review.contains("- mind the(31m empty list (orrerix) merge it"), "{review}");
    assert_eq!(status(&reg, &group)["notes_pending"], json!(0), "and the brief spent it");

    let empty = reg.quick_control(&group, "note", Some("  \n ")).expect_err("an empty note");
    assert!(empty.contains("needs some text"), "{empty}");
}

/// **The control door is closed on both sides**: an action outside the
/// vocabulary is refused naming the vocabulary, and a group that is not a
/// quick run is refused before anything is read — a valid group id is not
/// membership.
#[test]
fn the_control_door_refuses_an_unknown_action_and_a_group_that_is_not_a_quick_run() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, _worker) = working(&reg, &repo);

    let err = control(&reg, &group, "merge").expect_err("not an action");
    assert!(err.contains("unknown quick-run action") && err.contains("handoff"), "{err}");
    assert_eq!(state(&reg, &group), "work-wait", "and it did nothing");

    let plain = reg
        .create_group(&repo.path(), Guardrails { agent_cli: "claude".into(), ..Guardrails::default() })
        .unwrap()
        .id;
    for action in loomux_lib::orchestration::QUICK_ACTIONS {
        let err = control(&reg, &plain, action).expect_err("an ordinary group is not a quick run");
        assert!(err.contains("not a quick run"), "{action}: {err}");
    }
    assert_eq!(reg.quick_status(&plain)["exists"], json!(false));
    // The control: the same action on the quick group is accepted.
    assert!(control(&reg, &group, "step").is_ok());
}

// ── the runs that have not ended (#3679) ─────────────────────────────────────

/// **The list names every run that has not ended, newest first, each with its
/// repository** — and a run that has ended is not on it. It is read off the
/// records on disk, so it is the way back to a run nothing on screen points at.
#[test]
fn the_list_of_unfinished_runs_is_newest_first_and_drops_a_run_that_ended() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let (repo_a, repo_b) = (Repo::new(), Repo::new());
    let first = start(&reg, &repo_a);
    let started = reg.quick_start_at(request(&repo_b), T0 + MIN).expect("a second run starts");
    let second = GroupId::parse(started["group_id"].as_str().unwrap()).unwrap();
    assert_ne!(first, second, "the fixture: two runs, two groups");

    let list = reg.quick_list();
    let rows = list.as_array().expect("a list");
    let ids: Vec<&str> = rows.iter().map(|r| r["group_id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![second.as_str(), first.as_str()], "newest first");
    assert_eq!(rows[0]["repo"].as_str().unwrap().replace('\\', "/"), repo_b.path());
    assert_eq!(rows[1]["repo"].as_str().unwrap().replace('\\', "/"), repo_a.path());
    assert_eq!(rows[1]["state"], json!("work-wait"), "a row is the run's own status: {}", rows[1]);

    reg.quick_control(&first, "stop", None).expect("the first run stops");
    let after = reg.quick_list();
    let ids: Vec<&str> = after.as_array().unwrap().iter().map(|r| r["group_id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![second.as_str()], "an ended run is not one to resume or stop");
}

/// **A run with no pane left is resumed and stopped by nothing but its id from
/// that list.** Resume and Stop are on a pane's menu; this is the run whose
/// every pane is gone, which is the one case a menu cannot reach.
#[test]
fn a_run_with_no_pane_left_is_resumed_and_then_stopped_from_the_list() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker) = working(&reg, &repo);
    reg.mark_dead(&worker, Some(1));
    step(&reg, &group, T0 + 2);
    assert_eq!(held_reason(&reg, &group), "worker-gone");
    assert!(live_agents(&reg, &group).is_empty(), "the fixture's premise: no pane is left");

    let list = reg.quick_list();
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["group_id"] == json!(group.as_str()))
        .expect("the parked run is listed")
        .clone();
    assert_eq!(row["state"], json!("held"));
    assert_eq!(row["held_reason"], json!("worker-gone"));

    // Everything below is done with the id read off that row.
    let id = GroupId::parse(row["group_id"].as_str().unwrap()).unwrap();
    let resumed = reg.quick_resume_at(&id, T0 + 3).expect("the run resumes");
    assert_eq!(resumed["state"], json!("work-wait"), "{resumed}");
    let reopened = live_agents(&reg, &id);
    assert_eq!(reopened.len(), 1, "one pane is open again");
    assert_ne!(reopened[0], worker);

    reg.quick_control(&id, "stop", None).expect("and it can be stopped the same way");
    assert!(reg.quick_list().as_array().unwrap().is_empty(), "after which nothing is left to list");
}
