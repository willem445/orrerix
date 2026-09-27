//! #1871's loop defects, #1861's all-lanes clear, superseded panes, and §5.2's owed notices.
//!
//! One module of the `reviewdrive` integration-test target (`main.rs`),
//! split out of the former single-file `tests/reviewdrive.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ── #1871: the loop defects the dogfood run found ───────────────────────────

/// Rounds spent, read off the surface an orchestrator reads.
pub(crate) fn review_rounds(reg: &OrchRegistry, group: &GroupId) -> u64 {
    reg.review_drive_status(group)["drives"][0]["counters"]["review_rounds"]
        .as_u64()
        .unwrap_or(u64::MAX)
}

/// The `rd-consumed` kinds this group has recorded, in order.
fn consumed_kinds(reg: &OrchRegistry, group: &GroupId) -> Vec<String> {
    reg.audit_log(group)
        .into_iter()
        .filter(|e| e.action == "rd-consumed")
        .filter_map(|e| e.detail["kind"].as_str().map(str::to_string))
        .collect()
}

/// **#1871 B1, through the seam.** A worker fix at a new head must RE-OPEN the
/// lane, not re-route the verdict recorded before the fix.
///
/// Observed on PR #1870, and this is that sequence: `rev-std` recorded `fail` at
/// `df76047f`; the worker fixed it and pushed `45d74286` with CI green; the
/// drive saw the new head, came back through `review-wait` — and read the SAME
/// `fail` again, spent a second `review_rounds` on it, and handed the worker
/// back its own already-addressed findings as "attempt 2". Nothing ever reached
/// `lane_open_for`, because the `Fail` arm answered first, from a commit that no
/// longer described the PR. Three passes reach INVARIANT 9's bound with no
/// re-review having happened at all.
///
/// **The operands collide, which is what lets this test fail.** ONE recorded
/// `fail`, from ONE lane, read at TWO heads: at the head it was recorded against
/// it must route (the first block, which is this test's own positive control),
/// and at the head the worker moved to it must not. A fixture that recorded the
/// verdict at a head the drive never returned to would pass under the defect,
/// and one that never routed it at all would pass under an implementation that
/// simply ignored every `fail`.
///
/// **The arc is asserted to have RUN before anything is asserted about what it
/// produced**: the drive is checked into `review-wait` at `HEAD_B` — so arcs 7
/// and 2 really did carry it there — before the re-brief is looked for. Without
/// that, a drive parked somewhere else entirely would satisfy "no round spent"
/// and "no hand-back" trivially.
#[test]
fn a_fix_at_a_new_head_re_opens_the_lane_instead_of_re_routing_the_stale_verdict() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _s) = driven(&reg, &repo, &gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7001);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));

    reg.rd_drive_group_with(&group, &gh, 10_000); // ci-wait -> review-wait
    let opened = reg.rd_drive_group_with(&group, &gh, 20_000); // -> lane spawned
    let (_pr, _b, lane) = opened.lanes_opened.first().cloned().expect("lane 0 opens");

    // The lane records `fail` AT HEAD_A, through the real recording path.
    dispatch(
        &reg,
        &Caller {
            agent_id: lane.clone(),
            group: group.clone(),
            role: Role::Reviewer,
            role_hint: None,
        },
        "tools/call",
        &json!({ "name": "review_verdict", "arguments": {
            "pr": "1758", "verdict": "fail", "summary": "fail - one blocking" } }),
    )
    .expect("the lane records a blocking verdict");

    // **The positive control, and it is the same verdict this test later
    // refuses.** At the head it was recorded against, a `fail` routes: arc 5,
    // one review round spent, one hand-back.
    let routed = reg.rd_drive_group_with(&group, &gh, 30_000);
    assert_eq!(
        status_state(&reg, &group),
        "fix-wait",
        "a fail AT THIS HEAD must route — otherwise the refusal below is vacuous"
    );
    assert_eq!(review_rounds(&reg, &group), 1, "arc 5 spends exactly one round");
    assert_eq!(routed.handbacks.len(), 1, "…and hands the findings back once");
    let (_pr, worker) = routed.handbacks.first().cloned().expect("the hand-back names a pane");
    with_pane(&reg, &worker, 7002);

    // The worker fixes and pushes. CI stays green, as it was on #1870.
    gh.set_facts("OPEN", HEAD_B);
    reg.set_pr_head_override(Some(HEAD_B.to_string()));
    reg.rd_drive_group_with(&group, &gh, 40_000); // arc 7: the head moved

    // …and then it finishes the round it was handed, which since #2168 E1 is
    // what arc 2 waits for on a head that arrived by arc 7. The report is part
    // of the fixture rather than a concession to it: `driver-fix.md` tells the
    // worker to push AND report, so a fixture that pushed and went silent was
    // modelling half a round — and under E1 that half is `held(fix-stalled)`,
    // which is a different subject from the one this test is about.
    dispatch(
        &reg,
        &Caller {
            agent_id: worker.clone(),
            group: group.clone(),
            role: Role::Worker,
            role_hint: None,
        },
        "tools/call",
        &json!({ "name": "report", "arguments": {
            "outcome": "done", "note": "fixed and pushed", "ref": "#1758" } }),
    )
    .expect("the driven worker reports its fix finished");
    reg.rd_drive_group_with(&group, &gh, 50_000); // arc 2: green, and reported

    // THE ARC RAN. Everything below is a statement about a drive that really is
    // back in `review-wait` looking at the new head.
    assert_eq!(status_state(&reg, &group), "review-wait", "arcs 7 and 2 carried the drive back");
    assert_eq!(status_head(&reg, &group), HEAD_B, "…and it is looking at the head that moved");

    let after = reg.rd_drive_group_with(&group, &gh, 60_000);

    let (_pr, _b, lane2) = after.lanes_opened.first().cloned().expect(
        "the lane owes a fresh verdict at the new head and must be RE-BRIEFED; instead the \
         drive re-routed the verdict recorded before the fix (#1871 B1)",
    );
    assert!(
        after.handbacks.is_empty(),
        "a hand-back carrying no fresh verdict is the defect itself: {:?}",
        after.handbacks
    );
    assert_eq!(
        review_rounds(&reg, &group),
        1,
        "a re-brief spends no round — a round counts findings DELIVERED, and this delivers \
         none. Spending one is what reached the bound in three passes with no re-review"
    );
    assert_eq!(
        status_state(&reg, &group),
        "review-wait",
        "…and the drive waits for that verdict rather than dropping back to fix-wait"
    );

    let brief = lane_brief(&reg, &lane2);
    assert!(
        brief.contains(HEAD_B),
        "the re-brief must ask about the revision in front of the drive: {brief}"
    );
    assert!(
        brief.starts_with("DELTA on PR #1758"),
        "…and this lane has ANSWERED before, so it is a delta rather than a first call: {brief}"
    );
}

/// **The same rule, seen from `escalate`.** `lane_verdict_is_current` is asked
/// word-blind, and this is the assertion that keeps it that way: an `escalate`
/// bound to a head the worker has moved past is not a judgment anyone is being
/// asked for, so the drive must re-brief rather than re-park on `held(escalate)`
/// for ever.
///
/// Worth its own test rather than a second arm in the one above because the two
/// words take different arcs — `fail` spends a counter, `escalate` parks — so a
/// fix that special-cased `Fail` alone passes that test and fails this one. The
/// first block is again the control: at ITS OWN head, the escalate still holds.
#[test]
fn a_stale_escalate_re_opens_the_lane_and_a_current_one_still_holds() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, session) = driven(&reg, &repo, &gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7001);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));

    reg.rd_drive_group_with(&group, &gh, 10_000);
    let opened = reg.rd_drive_group_with(&group, &gh, 20_000);
    let (_pr, _b, lane) = opened.lanes_opened.first().cloned().expect("lane 0 opens");
    dispatch(
        &reg,
        &Caller {
            agent_id: lane,
            group: group.clone(),
            role: Role::Reviewer,
            role_hint: None,
        },
        "tools/call",
        &json!({ "name": "review_verdict", "arguments": {
            "pr": "1758", "verdict": "escalate", "summary": "needs a human call" } }),
    )
    .expect("the lane escalates");

    reg.rd_drive_group_with(&group, &gh, 30_000);
    assert_eq!(
        status_state(&reg, &group),
        "held",
        "an escalate AT THIS HEAD parks the drive — the control for the re-open below"
    );
    assert_eq!(
        reg.review_drive_status(&group)["drives"][0]["held_reason"],
        json!("escalate")
    );

    // The orchestrator dispositions it and resumes; the worker has pushed since.
    gh.set_facts("OPEN", HEAD_B);
    reg.set_pr_head_override(Some(HEAD_B.to_string()));
    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 60_000);
    assert_eq!(out["driving"], json!(true), "{out}");

    reg.rd_drive_group_with(&group, &gh, 70_000); // ci-wait -> review-wait
    assert_eq!(status_state(&reg, &group), "review-wait", "the resume reached a working state");
    let after = reg.rd_drive_group_with(&group, &gh, 80_000);
    assert!(
        !after.lanes_opened.is_empty(),
        "an escalate bound to a head the worker moved past must re-open the lane, not re-park \
         the drive on a judgment about a revision that no longer exists"
    );
    assert_ne!(status_state(&reg, &group), "held", "…and the drive must not re-hold");
}

/// **#1958, and the arm it deliberately does NOT change.** A driven delegate's
/// `progress` report still reaches the DRIVER — consumed and audited under
/// `report:worker`, exactly as before.
///
/// #1958 takes a `progress` report off the orchestrator's pane and puts it on
/// the board instead. That decision belongs to the UNDRIVEN arm only: under a
/// live drive the recipient is not the orchestrator at all (#1778 §7), the
/// driver already writes its own board notes (`rd_task_note`), and a second,
/// unfiltered note stream from the delegate onto the same row would duplicate
/// the drive's own record. So this pins BOTH halves of "the driven arm is
/// untouched": the consumption still happens, and no board note is written.
///
/// The control is the consumption count moving 0 → 1 across the one call. An
/// assertion that no note appeared, and no line reached the pane, passes just as
/// well when the report never dispatched.
#[test]
fn a_driven_workers_progress_report_is_still_consumed_by_the_driver() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _s) = driven(&reg, &repo, &gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7001);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));

    // One hand-back, which is what gives this drive a worker pane it owns.
    gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
    let first = reg.rd_drive_group_with(&group, &gh, 10_000);
    let (_pr, w1) = first.handbacks.first().cloned().expect("the drive hands back");
    assert_eq!(
        reg.rd_owner(&group, &w1).map(|(pr, p)| (pr, p.current)),
        Some((1758, true)),
        "the handed-back pane must be this drive's current delegate, or the arm under test \
         is not the arm being exercised"
    );

    // A board row bound to that pane's session and PR — so "no note" below is a
    // real absence rather than an unresolvable row.
    let session = reg
        .agent(&w1)
        .and_then(|a| a.session_id.clone())
        .expect("the handed-back pane has a session");
    let t = reg
        .upsert_task(
            &group,
            "orch-1",
            None,
            TaskPatch { title: Some("Fix PR 1758".into()), ..Default::default() },
        )
        .unwrap();
    reg.upsert_task(
        &group,
        "orch-1",
        Some(&t.id),
        TaskPatch { session: Some(session), pr: Some("#1758".into()), ..Default::default() },
    )
    .unwrap();
    let notes_before = reg
        .tasks(&group)
        .into_iter()
        .find(|x| x.id == t.id)
        .map(|x| x.notes.len())
        .unwrap_or(usize::MAX);

    let consumed_before =
        consumed_kinds(&reg, &group).iter().filter(|k| *k == "report:worker").count();
    let before = delivered_texts(&reg, &group).len();
    dispatch(
        &reg,
        &Caller {
            agent_id: w1.clone(),
            group: group.clone(),
            role: Role::Worker,
            role_hint: None,
        },
        "tools/call",
        &json!({ "name": "report", "arguments": {
            "outcome": "progress", "note": "rebasing", "ref": "#1758" } }),
    )
    .expect("a driven pane may still report progress");

    assert_eq!(
        consumed_kinds(&reg, &group).iter().filter(|k| *k == "report:worker").count(),
        consumed_before + 1,
        "the driven arm must still consume a progress report: {:?}",
        consumed_kinds(&reg, &group)
    );
    assert!(
        !delivered_texts(&reg, &group)[before..].iter().any(|t| t.contains("reports progress")),
        "and it still reaches no pane"
    );
    assert_eq!(
        reg.tasks(&group).into_iter().find(|x| x.id == t.id).map(|x| x.notes.len()),
        Some(notes_before),
        "#1958's board note is the UNDRIVEN arm's behaviour — a driven report is the \
         driver's to record, and a second note stream would duplicate it"
    );
}

/// **#1871 B2, through the seam.** A drive that hands back twice still owns the
/// pane it opened first — and does not take its word.
///
/// Measured on PR #1870: `rd-handback agent=w-1715`, then `w-1715`'s
/// `report(progress)` correctly `rd-consumed`; then `rd-handback agent=w-1716`,
/// which overwrote the single-slot `worker_agent`; then BOTH of `w-1715`'s
/// `report(done)` calls delivered to the orchestrator's pane as if nobody owned
/// it. `w-1715` was still running, on the same session and the same PR.
///
/// **Both halves are asserted, because a fix can be wrong in either
/// direction.** Not owning it is the leak. Owning it and BELIEVING it is worse:
/// arc 8 would take a superseded pane's `done` as the current worker having
/// finished work that worker is still in the middle of. So the superseded pane's
/// report must be consumed, audited under its own kind, and change nothing — and
/// the current pane's identical report must still move the drive, which is what
/// stops "believe nobody" from passing this test.
#[test]
fn a_superseded_worker_pane_is_still_intercepted_and_never_moves_the_drive() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _s) = driven(&reg, &repo, &gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7001);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));

    // Hand-back one: CI is red at HEAD_A.
    gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
    let first = reg.rd_drive_group_with(&group, &gh, 10_000);
    let (_pr, w1) = first.handbacks.first().cloned().expect("the drive hands back");

    // Hand-back two: the worker pushed, and CI is red again at the new head.
    gh.set_facts("OPEN", HEAD_B);
    reg.set_pr_head_override(Some(HEAD_B.to_string()));
    reg.rd_drive_group_with(&group, &gh, 20_000); // arc 7
    let second = reg.rd_drive_group_with(&group, &gh, 30_000); // red again -> fix-wait
    let (_pr, w2) = second
        .handbacks
        .first()
        .cloned()
        .expect("a second red hands back again, into a new pane");
    assert_ne!(
        w1, w2,
        "with no pane to reuse the driver opens one, and the two hand-backs must land in \
         different panes for the ownership assertions below to have two subjects"
    );

    // Both panes are the drive's; only the second is current.
    assert_eq!(
        reg.rd_owner(&group, &w1).map(|(pr, p)| (pr, p.current)),
        Some((1758, false)),
        "the pane the second hand-back superseded is still this drive's delegate — its \
         report must not reach the orchestrator as if undriven (#1871 B2)"
    );
    assert_eq!(reg.rd_owner(&group, &w2).map(|(pr, p)| (pr, p.current)), Some((1758, true)));

    // The superseded pane reports done. It is consumed, and it changes nothing.
    let before = delivered_texts(&reg, &group).len();
    let state_before = status_state(&reg, &group);
    dispatch(
        &reg,
        &Caller {
            agent_id: w1.clone(),
            group: group.clone(),
            role: Role::Worker,
            role_hint: None,
        },
        "tools/call",
        &json!({ "name": "report", "arguments": {
            "outcome": "done", "note": "fixed it", "ref": "#1758" } }),
    )
    .expect("a superseded pane may still report");
    assert!(
        !delivered_texts(&reg, &group)[before..].iter().any(|t| t.contains("reports done")),
        "a pane this drive opened must not report to the orchestrator, superseded or not"
    );
    assert!(
        consumed_kinds(&reg, &group).contains(&"report:superseded-worker".to_string()),
        "…and the audit must say WHICH, so a reader can tell consumed from \
         consumed-and-acted-on: {:?}",
        consumed_kinds(&reg, &group)
    );
    reg.rd_drive_group_with(&group, &gh, 40_000);
    assert_eq!(
        status_state(&reg, &group),
        state_before,
        "a superseded pane's `done` must not take arc 8 — it is a claim about a revision the \
         drive has already moved past"
    );

    // The CURRENT pane's identical report still moves it. Without this the test
    // passes under an implementation that believes nobody.
    gh.set_checks(r#"[{"name":"build","state":"SUCCESS","link":"x"}]"#);
    dispatch(
        &reg,
        &Caller {
            agent_id: w2.clone(),
            group: group.clone(),
            role: Role::Worker,
            role_hint: None,
        },
        "tools/call",
        &json!({ "name": "report", "arguments": {
            "outcome": "done", "note": "fixed it", "ref": "#1758" } }),
    )
    .expect("the current pane reports");
    assert!(
        consumed_kinds(&reg, &group).contains(&"report:worker".to_string()),
        "{:?}",
        consumed_kinds(&reg, &group)
    );
    reg.rd_drive_group_with(&group, &gh, 50_000);
    assert_ne!(
        status_state(&reg, &group),
        "fix-wait",
        "the CURRENT worker's `done` still advances the drive"
    );
}

/// **#1871 B3, through the seam.** Every exit names the panes it leaves running,
/// and `cancel_review_drive` returns them.
///
/// After the cancel on #1870 the driver's three panes stayed alive and idle with
/// nothing said about them — two worker panes on ONE worktree and ONE session,
/// which is the #338/#359 hazard, produced by the mechanism the orchestrator
/// uses to avoid it. The human found them through the idle watchdog.
///
/// Three assertions, because two of them alone pass under a wrong fix: naming
/// the panes without saying they are RELEASED reads as "the drive still has this
/// in hand"; and killing them would satisfy "no orphans" while breaking §3.1
/// item 5 and a worker mid-edit. So the panes must be named, said to be
/// released, and still be ALIVE.
#[test]
fn a_cancel_names_the_panes_it_leaves_running_and_kills_none_of_them() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _s) = driven(&reg, &repo, &gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7001);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));

    reg.rd_drive_group_with(&group, &gh, 10_000);
    let opened = reg.rd_drive_group_with(&group, &gh, 20_000);
    let (_pr, block, lane) = opened.lanes_opened.first().cloned().expect("lane 0 opens");
    // …and a worker pane too, so the clause has both roles to distinguish.
    gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
    gh.set_facts("OPEN", HEAD_B);
    reg.set_pr_head_override(Some(HEAD_B.to_string()));
    reg.rd_drive_group_with(&group, &gh, 30_000); // arc 6 -> ci-wait
    let handed = reg.rd_drive_group_with(&group, &gh, 40_000); // red -> fix-wait
    let (_pr, worker) = handed.handbacks.first().cloned().expect("the drive hands back");

    let before = delivered_texts(&reg, &group).len();
    let out = reg.cancel_review_drive(&group, 1758, "orch-1");
    assert_eq!(out["cancelled"], json!(true), "{out}");

    // 1. The RESULT names them, with the role that decides how to dispose.
    let named: Vec<String> = out["panes"]
        .as_array()
        .expect("cancel_review_drive returns the panes it released")
        .iter()
        .map(|p| {
            format!(
                "{}:{}",
                p["agent"].as_str().unwrap_or_default(),
                p["role"].as_str().unwrap_or_default()
            )
        })
        .collect();
    assert!(named.contains(&format!("{worker}:worker")), "{named:?}");
    assert!(named.contains(&format!("{lane}:{block}")), "{named:?}");

    // 2. The AUDIT ROW names them and says they are released, not still in hand.
    //
    // **Read off the audit log rather than the pane since #3040 N1.** A
    // tool-initiated cancel is not announced: its caller is holding the result
    // this test read in step 1, so the notice is written to
    // `rd-notice-demoted` instead of delivered. What that row carries is the
    // notice VERBATIM, which is what makes this a change of surface and not of
    // coverage — the clause, the pane ids and the standing are asserted exactly
    // as they were. That the pane really receives nothing is
    // `a_tool_cancel_audits_and_delivers_nothing`, with its own control.
    assert!(
        delivered_texts(&reg, &group)[before..].iter().all(|t| !t.contains("CANCELLED")),
        "the tool's own caller has this answer already (#3040 N1)"
    );
    let demoted = audit_details(&reg, &group, "rd-notice-demoted");
    assert_eq!(demoted.len(), 1, "the demoted notice is on the log exactly once: {demoted:?}");
    let notice = demoted[0]["notice"].as_str().unwrap_or_default().to_string();
    assert!(notice.contains(&format!("{worker} (worker)")), "{notice}");
    assert!(notice.contains(&format!("{lane} ({block})")), "{notice}");
    assert!(notice.contains("RELEASED"), "a terminal exit hands its panes back: {notice}");

    // 3. And nothing was killed. §3.1 item 5, and a worker mid-edit.
    for a in [&worker, &lane] {
        let entry = reg.agent(a).expect("the pane is still on the roster");
        assert_ne!(
            entry.status,
            AgentStatus::Dead,
            "the driver kills no pane — disposal is the orchestrator's deliberate call: {a}"
        );
    }
}

// ── #1861: the invariant behind the all-lanes `briefed_head` clear ──────────

/// **#1861.** The resume out of `held(lane-stalled)` clears `briefed_head` for
/// EVERY lane, not just the stalled one. That is safe only because **the
/// deciding lane is verdict-selected** — and nothing pinned it.
///
/// If selection ever consulted the lane RECORD instead of the verdict — a
/// routing change, a new lane kind, a gate that counts differently — the
/// all-lanes clear silently becomes "re-brief every lane on every resume": a
/// delegate spawned per lane per resume, review rounds burned with no worker
/// turn, and nothing red.
///
/// **Why this is pinned here and not on the resume arc.** #1861 proposes a
/// two-lane seam fixture where only one lane is stalled. That fixture cannot be
/// made to fail, and the previous author had already worked out why and left the
/// analysis on `a_resume_re_briefs_the_lane_that_stalled_rather_than_waiting_on_it_again`:
/// `first_stale_lane` skips a standing pass before any lane record is read, so
/// the passed lane is unreachable from the re-open under every implementation —
/// and making it reachable means staling its pass, at which point IT becomes the
/// deciding lane and the test stops being about the stall. The operands cannot
/// collide on that arc. They collide here.
///
/// **The collision.** Both lane records are put in the SAME state — identical on
/// the `briefed_head` axis — so the only thing left that can distinguish them is
/// the VERDICT. A selection rule that read the record would pick lane 0: it is
/// first in the gate's order and its record is exactly as stale as lane 1's. The
/// assertion is that lane **1** opens.
///
/// **Asserting the property rather than the fixture** is what removes the
/// dependence on the resume happening to blank every lane. All four crossings of
/// {record blank, record stale} x {pass current, pass stale} are pinned, so "a
/// lane whose pass stands is never re-opened, whatever its record says" holds
/// however many lanes a future resume decides to clear.
///
/// **`Blank` IS the post-resume state**, which is what makes a `decide`-level
/// pin cover the arc #1841's B4 lives on: clearing `briefed_head` for every lane
/// is exactly the `Rec::Blank` row, and the row says that clearing it changes
/// nothing about WHICH lane is chosen. `Rec::Stale` is the ordinary post-push
/// state, and it is there so the property is not stated only about the resume.
/// The stale-pass column is the control that stops "always answer 1" passing.
///
/// **A third record state is deliberately not a row here.** A record briefed at
/// the LIVE head answers `Wait`, which names no lane and therefore witnesses
/// nothing about selection — it holds identically whichever lane the drive is
/// waiting on. The first draft of this test asserted `OpenLane { index: 1 }` for
/// it and went red on CI; it is now pinned below as its own strictly weaker,
/// explicitly labelled assertion rather than as a crossing that cannot fail.
#[test]
fn the_deciding_lane_is_verdict_selected_which_is_what_makes_the_all_lanes_clear_safe() {
    let limits = DriveLimits::default();

    // **Both record states make `lane_open_for` FALSE**, and that is what keeps
    // the observable an INDEX. `Blank` is what #1841's B4 all-lanes clear leaves
    // behind; `Stale` is the ordinary state after a push. A third state —
    // briefed at the live head — is deliberately not one of the crossings: it
    // answers `Wait`, which names no lane, so it cannot witness WHICH lane was
    // selected. It is pinned below as its own strictly weaker assertion rather
    // than folded in here as a row that holds under every implementation.
    #[derive(Clone, Copy, Debug)]
    enum Rec {
        Blank,
        Stale,
    }

    let entry_with = |rec: Rec| {
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = HEAD_A.into();
        e.open_lane("rev-std", "s0", "rev-1", HEAD_A, Some("d1"), 0, false, false);
        e.open_lane("rev-final", "s1", "rev-2", HEAD_A, Some("d1"), 0, false, false);
        for l in e.lanes.iter_mut() {
            match rec {
                Rec::Blank => l.briefed_head.clear(),
                Rec::Stale => l.briefed_head = HEAD_B.into(),
            }
        }
        e
    };

    // Lane 0 has a verdict, lane 1 never answered. `pass_head` decides whether
    // lane 0's pass still stands at the live head.
    let facts_with = |pass_head: &str| DriveFacts {
        required_lanes: Some(vec![
            lane_fact("rev-std", Some(Verdict::Pass), pass_head, "d1"),
            lane_fact("rev-final", None, "", ""),
        ]),
        ..facts_at(HEAD_A)
    };

    for rec in [Rec::Blank, Rec::Stale] {
        let e = entry_with(rec);

        // Lane 0's pass STANDS. Lane 1 is the deciding lane, and it is the one
        // that opens — even though lane 0 comes first in the gate's order and
        // its record is in exactly the same state as lane 1's.
        assert_eq!(
            reviewdrive::decide(&e, &facts_with(HEAD_A), &limits),
            DriveStep::OpenLane { index: 1, verify: false, body_only: false },
            "{rec:?} records: a lane whose pass stands must never be re-opened. Both records \
             are identical here, so a selection rule reading the RECORD rather than the \
             verdict picks lane 0 — which is what turns #1841's all-lanes clear into a \
             re-brief per lane per resume"
        );

        // The control, on the one axis that may move the answer: stale lane 0's
        // pass and it becomes the deciding lane. Without this, an implementation
        // that always answered `index: 1` would satisfy every assertion above.
        assert_eq!(
            reviewdrive::decide(&e, &facts_with(HEAD_B), &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: false },
            "{rec:?} records: a lane whose pass no longer stands IS the deciding lane — so \
             the assertion above is the verdict deciding, not a constant"
        );
    }

    // **The strictly weaker row, labelled.** A record briefed at the LIVE head
    // is open for this revision, so the drive waits for it rather than re-asking
    // — `Wait`, correctly, and the first draft of this test asserted
    // `OpenLane { index: 1 }` here and went red on CI. It is kept because it
    // pins something real (a standing pass is not re-opened at any record
    // state), and kept SEPARATE because `Wait` names no lane: it holds
    // identically whether the drive is waiting on lane 0 or lane 1, so folding
    // it into the crossings above would have put a row there that cannot fail.
    let mut open = entry_at(DriveState::ReviewWait);
    open.head = HEAD_A.into();
    open.open_lane("rev-std", "s0", "rev-1", HEAD_A, Some("d1"), 0, false, false);
    open.open_lane("rev-final", "s1", "rev-2", HEAD_A, Some("d1"), 0, false, false);
    assert_eq!(
        reviewdrive::decide(&open, &facts_with(HEAD_A), &limits),
        DriveStep::Wait,
        "a lane already briefed at this revision is waited for, not re-asked"
    );

    // …and the function that makes it true, named directly, so a change to
    // selection reddens at the site rather than only through `decide`. It takes
    // no lane record at all — the structural half of this invariant — so its
    // answer cannot depend on `briefed_head` however that field moves.
    assert_eq!(
        reviewdrive::first_stale_lane(
            facts_with(HEAD_A).required_lanes.as_deref().unwrap(),
            HEAD_A,
            Some("d1")
        ),
        1,
        "first_stale_lane selects by verdict currency alone"
    );
    assert_eq!(
        reviewdrive::first_stale_lane(
            facts_with(HEAD_B).required_lanes.as_deref().unwrap(),
            HEAD_A,
            Some("d1")
        ),
        0
    );
}

// ── #1871 B2, as rev-final narrowed it ──────────────────────────────────────

/// **A superseded pane parks nothing, and a current one still does.**
///
/// The first version of this rule exempted `message_orchestrator` and argued the
/// exception from safety: `held(messaged)` only ever PARKS a drive, and parking
/// hands it to a human. That argument holds and is not the whole question — a
/// superseded pane can call the tool again after every resume, so the exception
/// allowed one pane nobody is talking to any more to park the drive without
/// bound, an orchestrator turn per park, with no remedy short of killing the
/// pane. The rule is now uniform: only a current pane's word moves a drive, and
/// parking moves it.
///
/// **Both halves, because either alone passes under a wrong fix.** Dropping the
/// park for everyone would satisfy the first assertion and break the hold that
/// `held(messaged)` exists to be. The second assertion is the control that
/// refuses it.
///
/// The unbounded-parking property is pinned directly rather than described: the
/// superseded pane messages TWICE across a resume, which under the exception is
/// two parks and two orchestrator turns.
#[test]
fn a_superseded_panes_message_parks_nothing_and_a_current_panes_still_parks() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _s) = driven(&reg, &repo, &gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7001);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));

    // Two hand-backs, so there is a superseded worker pane and a current one.
    gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
    let first = reg.rd_drive_group_with(&group, &gh, 10_000);
    let (_pr, w1) = first.handbacks.first().cloned().expect("the drive hands back");
    gh.set_facts("OPEN", HEAD_B);
    reg.set_pr_head_override(Some(HEAD_B.to_string()));
    reg.rd_drive_group_with(&group, &gh, 20_000);
    let second = reg.rd_drive_group_with(&group, &gh, 30_000);
    let (_pr, w2) = second.handbacks.first().cloned().expect("a second red hands back again");
    assert_ne!(w1, w2);

    let msg = |agent: &str| {
        dispatch(
            &reg,
            &Caller {
                agent_id: agent.to_string(),
                group: group.clone(),
                role: Role::Worker,
                role_hint: None,
            },
            "tools/call",
            &json!({ "name": "message_orchestrator", "arguments": {
                "text": "the brief's premise looks wrong to me" } }),
        )
        .expect("a delegate may always message the orchestrator");
    };

    // The superseded pane speaks. Its words reach the orchestrator — this tool
    // is never intercepted — and the drive does not move.
    let before = delivered_texts(&reg, &group).len();
    msg(&w1);
    assert!(
        delivered_texts(&reg, &group)[before..].iter().any(|t| t.contains("premise looks wrong")),
        "message_orchestrator is never intercepted, superseded or not"
    );
    assert!(
        consumed_kinds(&reg, &group).contains(&"message:superseded".to_string()),
        "…and the audit records that the drive owned the speaker and did nothing: {:?}",
        consumed_kinds(&reg, &group)
    );
    reg.rd_drive_group_with(&group, &gh, 40_000);
    assert_ne!(
        status_state(&reg, &group),
        "held",
        "a superseded pane must not park the drive — under the exception this was one \
         orchestrator turn, repeatable after every resume, with no bound"
    );

    // …and again after a resume, which is the shape that made it unbounded.
    msg(&w1);
    reg.rd_drive_group_with(&group, &gh, 50_000);
    assert_ne!(status_state(&reg, &group), "held", "…still not, however many times it speaks");

    // The CONTROL: the current pane's identical call still parks the drive.
    // Without this, "never park" passes everything above and deletes the hold.
    msg(&w2);
    reg.rd_drive_group_with(&group, &gh, 60_000);
    assert_eq!(
        status_state(&reg, &group),
        "held",
        "a CURRENT delegate's message still parks the drive — that is the case the hold was \
         written for"
    );
    assert_eq!(
        reg.review_drive_status(&group)["drives"][0]["held_reason"],
        json!("messaged"),
        "…on the reason that names it"
    );
}

/// **The superseded lists are bounded by LIVENESS, not by size** — through the
/// seam, so the tick is what has to do the pruning.
///
/// rev-final promoted this from a risk to a defect and was right: the size cap
/// this replaces evicted the OLDEST pane, and the oldest superseded pane is one
/// that is still running, still on this session and still able to `report`. A
/// cap therefore reproduced #1871 B2 at scale, under exactly the usage that
/// produced B2.
///
/// The two assertions are a pair: a live superseded pane survives any number of
/// later hand-backs (what a cap could not promise), and a DEAD one is forgotten
/// (which is what keeps the list bounded at all). A rule that kept everything
/// for ever would satisfy the first and not the second.
#[test]
fn a_live_superseded_pane_is_never_pruned_and_a_dead_one_is() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _s) = driven(&reg, &repo, &gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7001);

    gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
    let first = reg.rd_drive_group_with(&group, &gh, 10_000);
    let (_pr, w1) = first.handbacks.first().cloned().expect("the drive hands back");
    gh.set_facts("OPEN", HEAD_B);
    reg.set_pr_head_override(Some(HEAD_B.to_string()));
    reg.rd_drive_group_with(&group, &gh, 20_000);
    let second = reg.rd_drive_group_with(&group, &gh, 30_000);
    let (_pr, w2) = second.handbacks.first().cloned().expect("a second red hands back");
    assert_ne!(w1, w2);

    // A tick with w1 alive changes nothing about it.
    reg.rd_drive_group_with(&group, &gh, 40_000);
    assert_eq!(
        reg.rd_owner(&group, &w1).map(|(pr, p)| (pr, p.current)),
        Some((1758, false)),
        "a LIVE superseded pane survives the tick's prune — a size cap is what would have \
         dropped it, and dropping it is #1871 B2 again"
    );

    // Now it dies. The next tick forgets it, because a dead pane cannot reach
    // the MCP seam at all and so has no traffic left for the drive to fail to own.
    assert!(reg.mark_agent_dead_for_test(&w1), "the pane must exist to be marked");
    reg.rd_drive_group_with(&group, &gh, 50_000);
    assert_eq!(
        reg.rd_owner(&group, &w1),
        None,
        "a DEAD superseded pane is forgotten, which is what bounds the list"
    );
    assert_eq!(
        reg.rd_owner(&group, &w2).map(|(pr, p)| (pr, p.current)),
        Some((1758, true)),
        "…and the current pane is untouched, so this is not a prune that forgets everything"
    );
}
// ── §5.2's ordering rule: a notice that reached no pane (#1857) ─────────────

/// Every prompt this group delivered that is a review-drive notice for `pr`.
///
/// Filtered on the notice's own opening rather than on a substring of the body,
/// so an `[orrerix]` line about something else in the same group cannot pad the
/// count the assertions below are about.
pub(crate) fn drive_notices(reg: &OrchRegistry, group: &GroupId, pr: u64) -> Vec<String> {
    let key = format!("review drive PR #{pr}:");
    delivered_texts(reg, group).into_iter().filter(|t| t.contains(&key)).collect()
}

pub(crate) fn action_count(reg: &OrchRegistry, group: &GroupId, action: &str) -> usize {
    audit_actions(reg, group).iter().filter(|a| a.as_str() == action).count()
}

pub(crate) fn audit_details(reg: &OrchRegistry, group: &GroupId, action: &str) -> Vec<serde_json::Value> {
    reg.audit_log(group)
        .into_iter()
        .filter(|e| e.action == action)
        .map(|e| e.detail)
        .collect()
}

/// **Make `deliver_to_orchestrator` answer `Ok` for `agent_id`** — a pane plus a
/// paused group, which is `orchestration/helpers.rs`'s own `pause_with_pane` (#569) and
/// the only way a headless test reaches that branch at all.
///
/// The obstacle is real and is not this feature's: a delivery that lands alone
/// at the front of an idle queue has to spawn the drainer that will paste it,
/// which needs a Tauri `AppHandle`; a test process has none, so
/// `deliver_prompt_as` WITHDRAWS the admission it just made and answers `Err`.
/// A paused group takes the branch above that — admit, audit the full `prompt`
/// line, return `Ok` — so the queue holds the payload exactly as it would in
/// production, and it is a real production state rather than a mock.
///
/// The pause is deliberately **not** in `driven`: the tests below vary whether a
/// delivery succeeds, so the state that makes one succeed has to be the thing
/// they turn on. Pausing touches nothing that happens before a delivery, which
/// is what makes it usable as a probe (#569).
pub(crate) fn make_delivery_land(reg: &OrchRegistry, group: &GroupId, agent_id: &str, pty: u32) {
    with_pane(reg, agent_id, pty);
    reg.pause_group(group).expect("a live group pauses");
}

/// **Make `pty` pass the driver's reuse-readiness predicate** (#2089): a
/// CONFIRMED last delivery on record, and nothing queued behind it.
///
/// A second, orthogonal obstacle to `make_delivery_land`'s, and it is why that
/// helper is not simply extended: that one is about whether `deliver_prompt`
/// answers `Ok` at all, this one about whether the driver will call the pane
/// ready in the first place. The production path that writes this record is
/// `deliver_now`'s confirm window, which needs a live pty and an `AppHandle`
/// — see `OrchRegistry::set_last_delivery_for_test`.
///
/// Called with `confirmed: false` it produces the OTHER half of the axis: a
/// pane whose last delivery is on record as not having landed.
pub(crate) fn make_pane_ready(reg: &OrchRegistry, pty: u32, confirmed: bool) {
    reg.set_last_delivery_for_test(pty, confirmed);
}

/// A drive walked to a terminal exit with the orchestrator's pane **down**, so
/// the exit notice's delivery genuinely fails.
///
/// The orchestrator exists but has no pane: `deliver_prompt` resolves the
/// target's `pty_id` before it audits and answers `Err` for an agent that has
/// none (#569). That is a real transient — a pane restarting is exactly this
/// state — rather than a fault injected below the seam, which matters because
/// what is under test is what the tick does with an `Err` from the production
/// delivery path.
///
/// The first tick is taken with the PR still OPEN, deliberately: it is what
/// latches the once-per-process reconcile, so the cancellation that follows is
/// the TICK's own `cancelled` arc and not reconcile's. Reconcile's producer gets
/// its own coverage in `reconciles_cancellation_is_owed_too_and_not_delivered_and_forgotten`.
fn cancelled_into_a_dead_pane(
    reg: &OrchRegistry,
    repo: &Repo,
    gh: &FakeGh,
    at: u64,
) -> (GroupId, String) {
    let (group, _session) = driven(reg, repo, gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    reg.rd_drive_group_with(&group, gh, at);
    gh.set_facts("CLOSED", HEAD_A);
    (group, orch.id)
}

/// **#1857.** A terminal entry whose notice reached no pane is NOT pruned, and
/// its notice is re-sent from the persisted record on a later tick.
///
/// This is §5.2's own sentence — "terminal entries are pruned once their notice
/// has been delivered" — which before this had no implementation anywhere: the
/// tick delivered before it pruned, which is necessary and not sufficient, and
/// `prune_terminal` dropped the entry whatever the delivery answered. A drive
/// whose final notice failed therefore ended with no line in the pane and no
/// record that could produce one.
///
/// **All four properties are asserted together because each alone passes under
/// an implementation that is wrong in one of the others' directions.** Retaining
/// without re-emitting is #1841's hold-back, which was inert; re-emitting
/// without retaining has nothing to re-emit from; delivering without pruning
/// leaks the entry forever; and pruning on the first tick is the bug.
///
/// The pane coming UP mid-test is what makes the re-emission discriminating: the
/// second tick does not step this entry at all (`rd_step_entry` returns `None`
/// for anything terminal, before any read), so the notice it delivers can only
/// have come off the entry — nothing on that path can rebuild it.
#[test]
fn a_terminal_notice_that_reached_no_pane_keeps_its_entry_and_is_re_sent_until_it_lands() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, orch) = cancelled_into_a_dead_pane(&reg, &repo, &gh, 10_000);

    // Tick 2: the drive cancels, and the notice cannot be delivered.
    let first = reg.rd_drive_group_with(&group, &gh, 20_000);
    assert_eq!(
        first.notice_undelivered,
        vec![1758],
        "the exit notice's delivery failed and the tick did not notice: {first:?}"
    );
    assert_eq!(
        first.notices.len(),
        1,
        "the notice was BUILT and attempted — `notices` is the attempt, and its failure is \
         what `notice_undelivered` reports: {first:?}"
    );
    assert!(
        first.pruned.is_empty(),
        "§5.2: a terminal entry is pruned ONCE ITS NOTICE HAS BEEN DELIVERED. This one \
         reached no pane and the entry is the only record that could produce it: {first:?}"
    );
    assert!(
        drive_notices(&reg, &group, 1758).is_empty(),
        "the control: no line reached the pane, so the assertions below are about a \
         re-emission and not about the first attempt having quietly worked"
    );
    assert_eq!(action_count(&reg, &group, "rd-pruned"), 0);

    // The pane comes up, so the delivery can now answer `Ok`. Nothing else
    // changes — no new `gh` facts, no new arc, no second notice built.
    make_delivery_land(&reg, &group, &orch, 7101);
    let second = reg.rd_drive_group_with(&group, &gh, 30_000);
    let landed = drive_notices(&reg, &group, 1758);
    assert_eq!(
        landed.len(),
        1,
        "the persisted notice must be re-sent once the pane is back — a terminal entry is \
         never STEPPED, so this line can only have come off the entry: {landed:?}"
    );
    assert!(landed[0].contains("CANCELLED"), "and it is the drive's real exit: {}", landed[0]);
    assert_eq!(second.notices, landed, "...and the tick reports having attempted it");
    assert!(
        second.notice_undelivered.is_empty(),
        "nothing is still owed once it landed: {second:?}"
    );

    // Delivered, so now it prunes — exactly once, and as a delivery rather than
    // as something given up on.
    assert_eq!(second.pruned, vec![1758], "a delivered notice releases the entry: {second:?}");
    assert_eq!(action_count(&reg, &group, "rd-pruned"), 1);
    assert_eq!(
        action_count(&reg, &group, "rd-notice-dropped"),
        0,
        "nothing was given up on here — `rd-notice-dropped` is the CEILING's word and a \
         reader filtering for a lost notice must not find this one"
    );

    // And it is over: a third tick re-sends nothing and re-prunes nothing.
    let third = reg.rd_drive_group_with(&group, &gh, 40_000);
    assert!(third.pruned.is_empty() && third.notices.is_empty(), "{third:?}");
    assert_eq!(
        drive_notices(&reg, &group, 1758).len(),
        1,
        "a delivered notice is delivered ONCE, not on every tick after"
    );
}

/// **The bound** (#1857's third requirement). A notice that can never be
/// delivered must not retain its entry forever — at `NOTICE_RETENTION_MS` past
/// the moment it was first owed the entry is dropped anyway.
///
/// **And the drop is audited WITH the notice text**, which is the half that
/// keeps the bound honest rather than merely bounded. #1857 is "no line in the
/// pane AND no record that could produce one"; a ceiling with no audit line
/// would close the first and reopen the second, which is the defect again with a
/// timer in front of it.
///
/// The tick just under the ceiling is the discriminating half: without it, an
/// implementation that dropped the entry on the first failure would pass every
/// assertion about the expired one.
#[test]
fn a_notice_that_can_never_be_delivered_is_bounded_and_its_text_survives_on_the_audit_log() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    // The pane never comes up, so no attempt can ever succeed.
    let (group, _orch) = cancelled_into_a_dead_pane(&reg, &repo, &gh, 10_000);
    let owed_at = 20_000;
    reg.rd_drive_group_with(&group, &gh, owed_at);

    // One tick short of the ceiling: still retained, still re-attempted.
    let inside = reg.rd_drive_group_with(&group, &gh, owed_at + NOTICE_RETENTION_MS - 1);
    assert_eq!(
        inside.notice_undelivered,
        vec![1758],
        "inside the ceiling the entry is kept and the notice re-attempted: {inside:?}"
    );
    assert!(inside.pruned.is_empty(), "...and nothing is dropped yet: {inside:?}");
    assert_eq!(action_count(&reg, &group, "rd-notice-dropped"), 0);

    // At the ceiling.
    let expired = reg.rd_drive_group_with(&group, &gh, owed_at + NOTICE_RETENTION_MS);
    assert_eq!(
        expired.pruned,
        vec![1758],
        "an undeliverable notice must not retain its entry forever — an unbounded retry is \
         a leak: {expired:?}"
    );
    assert!(drive_notices(&reg, &group, 1758).is_empty(), "it never did reach a pane");

    let dropped = audit_details(&reg, &group, "rd-notice-dropped");
    assert_eq!(dropped.len(), 1, "the ceiling audits exactly once: {dropped:?}");
    assert_eq!(dropped[0]["pr"], json!(1758));
    assert_eq!(dropped[0]["reason"], json!("retention-ceiling"));
    let text = dropped[0]["notice"].as_str().unwrap_or_default();
    assert!(
        text.contains("review drive PR #1758:") && text.contains("CANCELLED"),
        "the audit line must carry the NOTICE, not merely the fact that one was lost — it \
         is now the only record that could produce it: {text:?}"
    );
    // The bound really is the bound: nothing is retained past it.
    let after = reg.rd_drive_group_with(&group, &gh, owed_at + NOTICE_RETENTION_MS + 60_000);
    assert!(after.pruned.is_empty() && after.notice_undelivered.is_empty(), "{after:?}");
}

/// **The tool cancel now owes NOTHING, so there is nothing to lose** (#3040 N1
/// replacing #1857's guarantee on this one producer).
///
/// #1857's problem here was that `cancel_review_drive` built its notice after
/// its own write and handed it to a `let _ =`, so a cancel issued while the
/// orchestrator's pane was down was a drive that vanished with no line and
/// nothing to reproduce one from. It answered by OWING the notice on the entry.
/// N1 answers the same problem the other way for this producer only: the
/// notice is never delivered at all, and the full text goes to the audit log —
/// which is durable at once, with no delivery to fail and no retry to bound.
///
/// **The pane is down for exactly the reason it was in #1857**: that is the
/// condition under which "nothing was delivered" is uninformative, and the
/// assertions have to be about the RECORD. The retention ceiling is pinned on
/// the producers that still owe one:
/// `reconciles_cancellation_is_owed_too_and_not_delivered_and_forgotten`
/// and
/// `a_notice_that_can_never_be_delivered_is_bounded_and_its_text_survives_on_the_audit_log`.
#[test]
fn a_tool_cancel_into_a_dead_pane_records_its_notice_rather_than_owing_it() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _session) = driven(&reg, &repo, &gh);
    // An orchestrator with no pane: every delivery attempt would genuinely
    // fail, which is what makes the audit row the only possible record.
    let _orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    reg.rd_drive_group_with(&group, &gh, 10_000);

    let out = reg.cancel_review_drive_with(&group, 1758, "orch-1", 20_000);
    assert_eq!(out["cancelled"], json!(true), "the tool must succeed: {out}");

    let demoted = audit_details(&reg, &group, "rd-notice-demoted");
    assert_eq!(demoted.len(), 1, "the text survives a pane that is not there: {demoted:?}");
    let text = demoted[0]["notice"].as_str().unwrap_or_default();
    assert!(
        text.contains("review drive PR #1758:") && text.contains("cancel_review_drive"),
        "and it is this cancel's own line, cause word included: {text:?}"
    );
    // Nothing is owed, so nothing is retained and nothing is ever retried.
    assert!(
        reg.review_drive_status(&group)["drives"].as_array().is_some_and(|d| d.is_empty()),
        "the entry leaves at once: {}",
        reg.review_drive_status(&group)
    );
    let tick = reg.rd_drive_group_with(&group, &gh, 30_000);
    assert!(
        tick.notice_undelivered.is_empty() && drive_notices(&reg, &group, 1758).is_empty(),
        "a later tick has nothing to re-send and nothing to give up on: {tick:?}"
    );
    assert!(
        audit_details(&reg, &group, "rd-notice-dropped").is_empty(),
        "and nothing is ever given up on, because nothing was owed"
    );
}

/// Reconcile is the producer whose notice was most likely to be lost, because it
/// runs at startup — the exact moment an orchestrator pane is most likely to be
/// absent or still coming up. It owes onto the entry like every other terminal
/// exit (#1857).
///
/// The control is that reconcile really is the producer here: nothing calls a
/// tick with the PR open first, and `rd-cancelled` carrying `at: reconcile` is
/// asserted rather than assumed.
#[test]
fn reconciles_cancellation_is_owed_too_and_not_delivered_and_forgotten() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _session) = driven(&reg, &repo, &gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    gh.set_facts("CLOSED", HEAD_A);

    let first = reg.rd_drive_group_with(&group, &gh, 10_000);
    let reconciled = reg
        .audit_log(&group)
        .into_iter()
        .any(|e| e.action == "rd-cancelled" && e.detail["at"] == json!("reconcile"));
    assert!(reconciled, "this test is about RECONCILE's producer and it did not run");
    assert_eq!(first.notice_undelivered, vec![1758], "{first:?}");
    assert!(first.pruned.is_empty(), "a reconcile cancellation is retained too: {first:?}");

    make_delivery_land(&reg, &group, &orch.id, 7303);
    let second = reg.rd_drive_group_with(&group, &gh, 20_000);
    let landed = drive_notices(&reg, &group, 1758);
    assert_eq!(landed.len(), 1, "reconcile's notice is re-sent from the entry: {landed:?}");
    assert!(landed[0].contains("the PR is closed or merged"), "{}", landed[0]);
    assert_eq!(second.pruned, vec![1758]);
}

/// A fresh `drive_review` on a PR whose terminal entry has not been pruned drops
/// that entry (`state.entries.retain(|e| e.pr != pr)`, the queue's own "comes
/// back as a NEW entry" behaviour). Retention now HOLDS such an entry for an
/// undelivered notice, which makes that path reachable far more often — so it is
/// audited with the text rather than discarding the previous drive's ending
/// silently, which would be #1857 again with a different cause (#1857).
///
/// The notice must not be carried onto the new entry: it describes a drive that
/// is over, and delivering it beside a fresh drive's own traffic would read as
/// THIS drive ending.
#[test]
fn a_re_drive_that_displaces_a_still_owing_entry_audits_the_notice_it_gives_up_on() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, orch) = cancelled_into_a_dead_pane(&reg, &repo, &gh, 10_000);
    reg.rd_drive_group_with(&group, &gh, 20_000);
    assert_eq!(action_count(&reg, &group, "rd-notice-dropped"), 0, "the pre-state");

    // The PR is open again and the orchestrator starts a fresh drive on it. The
    // pane is up this time, so a notice carried onto the new entry WOULD be
    // delivered — which is what makes "it must not be" a real assertion.
    make_delivery_land(&reg, &group, &orch, 7404);
    gh.set_facts("OPEN", HEAD_A);
    let w = reg.spawn_agent(&group, Role::Worker, "w2", "", false, None).unwrap();
    let session = w.session_id.clone().expect("claude mints a session id at spawn");
    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 30_000);
    assert_eq!(out["driving"], json!(true), "the re-drive must succeed: {out}");

    let dropped = audit_details(&reg, &group, "rd-notice-dropped");
    assert_eq!(dropped.len(), 1, "displacing an owing entry is audited: {dropped:?}");
    assert_eq!(dropped[0]["reason"], json!("superseded"));
    assert!(
        dropped[0]["notice"].as_str().unwrap_or_default().contains("CANCELLED"),
        "with the text, so the record survives the entry: {dropped:?}"
    );

    // The new drive does not inherit the old one's ending.
    reg.rd_drive_group_with(&group, &gh, 40_000);
    let after = drive_notices(&reg, &group, 1758);
    assert!(
        after.is_empty(),
        "the previous drive's exit must not be announced beside a fresh drive's traffic: {after:?}"
    );
}
