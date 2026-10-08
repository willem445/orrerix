//! How a run ends or parks, and what the human is told: approval, the review
//! bound, a blocked or dead pane, a message, a refused pane, the clocks.

use super::*;

/// **Approval ends the run with exactly ONE notice and kills nothing.** The
/// panes are left for the human, and from here every pane's report is answered
/// "this run has ended".
#[test]
fn approval_raises_exactly_one_item_and_kills_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker, reviewer) = reviewing(&reg, &repo);
    assert!(open_items(&reg, &group).is_empty(), "nothing is raised while the run is working");

    report(&reg, &reviewer, json!({ "outcome": "approved", "note": "reads well and the test covers it" }));
    let out = step(&reg, &group, T0 + 3);
    assert_eq!(out.advanced, Some(("review-wait".to_string(), "satisfied".to_string())));
    assert!(out.notice);

    let items = open_items(&reg, &group);
    assert_eq!(items.len(), 1, "exactly one item: {items:?}");
    assert_eq!(items[0].kind, needsyou::Kind::Feedback);
    assert_eq!(items[0].task, None, "a quick run has no board row to name");
    assert!(items[0].text.contains("approved after 1 review"), "{}", items[0].text);
    assert!(items[0].text.contains("add a --json flag"), "{}", items[0].text);
    assert!(items[0].text.contains("reads well and the test covers it"), "{}", items[0].text);
    assert!(items[0].text.contains("Nothing was merged"), "{}", items[0].text);
    assert!(!items[0].text.contains('\n'), "a notice is one paragraph: {:?}", items[0].text);

    // Nothing was killed: both panes are still open, by the roster's own word.
    assert_eq!(live_agents(&reg, &group).len(), 2, "the positive control: there are two panes");
    for a in [&worker, &reviewer] {
        assert_ne!(reg.agent(a).unwrap().status, AgentStatus::Dead, "{a} was left open");
    }

    // A later step raises nothing more, and a later report moves nothing.
    assert!(!step(&reg, &group, T0 + 4).notice);
    assert_eq!(open_items(&reg, &group).len(), 1);
    let late = report(&reg, &worker, json!({ "outcome": "done", "note": "one more tweak" }));
    assert!(late.contains("has ended"), "{late}");
    assert_eq!(state(&reg, &group), "satisfied");
}

/// **`request_changes` on the last round the run allows parks it on
/// `review-limit`** with one notice, and the findings of that round are still
/// written.
#[test]
fn request_changes_on_the_last_round_parks_on_review_limit_with_one_item() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, _worker, reviewer) = reviewing_with(&reg, &repo, |r| r.max_review_rounds = Some(1));

    report(
        &reg,
        &reviewer,
        json!({ "outcome": "request_changes", "note": "still wrong", "summary": "the empty case" }),
    );
    let out = step(&reg, &group, T0 + 3);
    assert_eq!(out.advanced, Some(("review-wait".to_string(), "held".to_string())));
    assert_eq!(held_reason(&reg, &group), "review-limit");
    let items = open_items(&reg, &group);
    assert_eq!(items.len(), 1, "{items:?}");
    assert!(items[0].text.contains("held (review-limit)"), "{}", items[0].text);
    assert!(items[0].text.contains("still wrong"), "the notice quotes the reviewer: {}", items[0].text);
    assert_eq!(items[0].urgency, needsyou::Urgency::High, "a hold waits on the human");
    let (_plan, findings, _messages) = reg.qd_document_paths_for_test(&group, 1);
    assert!(findings.is_file(), "the last round's findings are kept for whoever resumes");
    assert_eq!(live_agents(&reg, &group).len(), 2, "and a hold kills nothing either");
}

/// **`blocked` parks the run naming the side that said it**, and the notice
/// carries what the pane said — a hold whose notice does not say what the pane
/// said is a hold nobody can act on.
#[test]
fn a_blocked_worker_parks_the_run_and_the_notice_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker) = working(&reg, &repo);

    report(&reg, &worker, json!({ "outcome": "blocked", "note": "the build needs a token I do not have" }));
    let out = step(&reg, &group, T0 + 2);
    assert_eq!(out.advanced, Some(("work-wait".to_string(), "held".to_string())));
    assert_eq!(held_reason(&reg, &group), "worker-blocked");
    let items = open_items(&reg, &group);
    assert_eq!(items.len(), 1);
    assert!(items[0].text.contains("the build needs a token I do not have"), "{}", items[0].text);
    assert_ne!(reg.agent(&worker).unwrap().status, AgentStatus::Dead);
}

/// **The pane holding the turn closing before it reports parks the run.** The
/// positive control is in `handback.rs`: the same pane closing while it does
/// NOT hold the turn parks nothing.
#[test]
fn the_pane_holding_the_turn_closing_parks_the_run_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, _worker, reviewer) = reviewing(&reg, &repo);

    reg.mark_dead(&reviewer, Some(3));
    let out = step(&reg, &group, T0 + 3);
    assert_eq!(out.advanced, Some(("review-wait".to_string(), "held".to_string())));
    assert_eq!(held_reason(&reg, &group), "reviewer-gone");
    let items = open_items(&reg, &group);
    assert_eq!(items.len(), 1);
    assert!(items[0].text.contains(&reviewer), "the notice names the pane: {}", items[0].text);
}

/// **`message_orchestrator` in a quick group is recorded for the human and
/// parks the run** — there is no orchestrator to deliver it to. The tool
/// answers `Ok`, the message is in the run's `messages.md`, and the notice
/// shows it.
#[test]
fn a_message_from_a_pane_parks_the_run_and_is_kept_for_the_human() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker) = working(&reg, &repo);

    let (is_error, answer) = call(
        &reg,
        &worker,
        "message_orchestrator",
        json!({ "text": "should --json include hidden entries?" }),
    );
    assert!(!is_error, "the tool answers Ok rather than failing on a missing root: {answer}");
    assert!(answer.contains("held"), "and says the run is being held: {answer}");
    let (_plan, _findings, messages) = reg.qd_document_paths_for_test(&group, 1);
    let kept = std::fs::read_to_string(&messages).expect("the message was appended");
    assert!(kept.contains("should --json include hidden entries?"), "{kept}");
    assert!(kept.contains(&worker), "attributed to the pane that sent it: {kept}");
    assert!(
        !delivered_texts(&reg, &group).iter().any(|t| t.contains("hidden entries")),
        "and it was typed into no pane"
    );

    let out = step(&reg, &group, T0 + 2);
    assert_eq!(out.advanced, Some(("work-wait".to_string(), "held".to_string())));
    assert_eq!(held_reason(&reg, &group), "messaged");
    let items = open_items(&reg, &group);
    assert_eq!(items.len(), 1);
    assert!(items[0].text.contains("should --json include hidden entries?"), "{}", items[0].text);
}

/// **A message parks AFTER a report has been honoured.** A pane that messaged
/// and then finished hands the turn on, and the hold then covers the whole
/// window — one park, not one per thing that was said.
#[test]
fn a_pane_that_messaged_and_then_finished_hands_the_turn_on_first() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker) = working(&reg, &repo);

    call(&reg, &worker, "message_orchestrator", json!({ "text": "fyi: I renamed a helper" }));
    report(&reg, &worker, json!({ "outcome": "done", "note": "finished" }));
    let out = step(&reg, &group, T0 + 2);
    assert_eq!(
        out.advanced,
        Some(("work-wait".to_string(), "review-wait".to_string())),
        "the report is the stronger fact"
    );
    assert_eq!(out.handed_to.as_ref().map(|h| h.0.as_str()), Some("reviewer"));
    // The message was consumed by that arc: the run does not park on it a tick later.
    assert_eq!(step(&reg, &group, T0 + 3).advanced, None);
    assert_eq!(state(&reg, &group), "review-wait");
}

/// **A pane the group's live-agent cap refuses parks the run on
/// `cap-refused`**, quoting the refusal — the run cannot make room for itself.
#[test]
fn a_reviewer_the_live_cap_refuses_parks_the_run_on_cap_refused() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker) = working_with(&reg, &repo, |r| r.max_agents = Some(1));

    report(&reg, &worker, json!({ "outcome": "done", "note": "finished" }));
    let out = step(&reg, &group, T0 + 2);
    assert!(!out.refusal.is_empty(), "the spawn was refused: {out:?}");
    assert_eq!(out.handed_to, None);
    assert_eq!(held_reason(&reg, &group), "cap-refused");
    assert_eq!(
        status(&reg, &group)["held_note"].as_str().unwrap_or_default().is_empty(),
        false,
        "the refusal is on the record for the notice to quote"
    );
    assert_eq!(open_items(&reg, &group).len(), 1);
    assert_eq!(live_agents(&reg, &group), vec![worker], "and no reviewer was opened");
}

/// **Each state parks on its own clock**: a reviewer silent past the review
/// timeout parks `lane-stalled`, and a first pass still running at the run's
/// time bound parks `drive-stalled` — while the first pass has no tighter
/// clock of its own.
#[test]
fn the_clocks_park_a_silent_reviewer_and_an_overlong_run() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();

    // The reviewer's turn began at T0 + 2 (the step that opened it).
    let (group, _worker, _reviewer) = reviewing(&reg, &repo);
    assert_eq!(step(&reg, &group, T0 + 2 + 59 * MIN).advanced, None, "inside the review timeout");
    step(&reg, &group, T0 + 2 + 60 * MIN);
    assert_eq!(held_reason(&reg, &group), "lane-stalled");

    // A run whose bound is the minimum, five minutes.
    let repo2 = Repo::new();
    let (slow, _w) = working_with(&reg, &repo2, |r| r.drive_timeout_minutes = Some(5));
    assert_eq!(status(&reg, &slow)["drive_timeout_minutes"], json!(5));
    assert_eq!(step(&reg, &slow, T0 + 4 * MIN).advanced, None);
    step(&reg, &slow, T0 + 5 * MIN);
    assert_eq!(held_reason(&reg, &slow), "drive-stalled");

    // The default bound is 240 minutes, and three hours of first pass is fine.
    let repo3 = Repo::new();
    let (long, _w) = working(&reg, &repo3);
    assert_eq!(status(&reg, &long)["drive_timeout_minutes"], json!(240));
    assert_eq!(step(&reg, &long, T0 + 180 * MIN).advanced, None);
    assert_eq!(state(&reg, &long), "work-wait");
}

/// **A `report(progress)` from the pane holding the turn is answered once, in
/// that pane, and moves nothing** (#1959's rule).
#[test]
fn a_progress_report_is_answered_once_in_the_pane_that_sent_it() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker) = working(&reg, &repo);
    make_deliverable(&reg, &group, &worker, 7401);

    report(&reg, &worker, json!({ "outcome": "progress", "note": "halfway" }));
    let out = step(&reg, &group, T0 + 2);
    assert!(out.kicked_back, "{out:?}");
    assert_eq!(out.advanced, None, "progress advances nothing");
    let typed = texts_to(&reg, &group, &worker);
    assert_eq!(typed.len(), 1, "{typed:?}");
    assert!(typed[0].contains("report(progress) moves nothing"), "{typed:?}");

    // Once per turn: a second progress report is not answered again.
    report(&reg, &worker, json!({ "outcome": "progress", "note": "three quarters" }));
    assert!(!step(&reg, &group, T0 + 3).kicked_back);
    assert_eq!(texts_to(&reg, &group, &worker).len(), 1);
    assert_eq!(state(&reg, &group), "work-wait");
}

/// **A message is cut to the body cap, and `messages.md` stops growing at its
/// ceiling** (#3681 review N1). Any pane in the group may call the tool as
/// often as it likes with up to a megabyte each time, and nothing reads the
/// file back — so what it can cost is disk, and that is bounded.
#[test]
fn a_message_is_cut_and_the_messages_file_stops_growing() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, worker) = working(&reg, &repo);
    let (_plan, _findings, messages) = reg.qd_document_paths_for_test(&group, 1);
    let long = "x".repeat(QD_BODY_CAP * 3);
    let send = || call(&reg, &worker, "message_orchestrator", json!({ "text": long }));

    let (is_error, answer) = send();
    assert!(!is_error, "{answer}");
    let one = std::fs::metadata(&messages).expect("the control: the first message was saved").len();
    assert!(one > QD_BODY_CAP as u64 / 2, "most of a body cap was written: {one}");
    assert!(one < QD_BODY_CAP as u64 + 200, "one message is cut to the cap, not written whole: {one}");

    // Eighty more: 1.6 MB asked for against a 1 MiB ceiling.
    for _ in 0..80 {
        let (is_error, answer) = send();
        assert!(!is_error, "a full file is not a tool error: {answer}");
    }
    let total = std::fs::metadata(&messages).unwrap().len();
    assert!(total >= 1024 * 1024, "the control: the file did reach its ceiling: {total}");
    assert!(total < 1024 * 1024 + QD_BODY_CAP as u64 + 200, "and stopped one message past it at most: {total}");

    let rows = audit_details(&reg, &group, quickdrive::audit_action::MESSAGE);
    assert!(rows.iter().any(|d| d["saved"] == json!(true)), "the control: some were kept");
    let last = rows.last().expect("the calls were audited");
    assert_eq!(last["saved"], json!(false), "and the audit says which were not: {last}");
}
