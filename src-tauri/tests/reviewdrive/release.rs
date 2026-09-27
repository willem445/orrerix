//! The driver releases what it no longer needs (#2501), and #2509's one-shot grace through the real tick.
//!
//! One module of the `reviewdrive` integration-test target (`main.rs`),
//! split out of the former single-file `tests/reviewdrive.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ── #2501: the driver releases what it no longer needs ──────────────────────

/// Record a `pass` for `agent` on PR 1758, through the real MCP arm.
pub(crate) fn record_pass_for(reg: &OrchRegistry, group: &GroupId, agent: &str) {
    let caller = Caller {
        agent_id: agent.to_string(),
        group: group.clone(),
        role: Role::Reviewer,
        role_hint: None,
    };
    dispatch(
        reg,
        &caller,
        "tools/call",
        &json!({ "name": "review_verdict", "arguments": {
            "pr": "1758", "verdict": "pass", "summary": "read the diff" } }),
    )
    .expect("the pass is recorded");
}

/// A delegate's `report`, through the real MCP arm — which is what ENDS its
/// turn: `idle_since_ms` is stamped by the report, never by a verdict file.
pub(crate) fn report_as(reg: &OrchRegistry, group: &GroupId, agent: &str, role: Role, outcome: &str) {
    let caller = Caller {
        agent_id: agent.to_string(),
        group: group.clone(),
        role,
        role_hint: None,
    };
    dispatch(
        reg,
        &caller,
        "tools/call",
        &json!({ "name": "report", "arguments": {
            "outcome": outcome, "note": "n", "ref": "#1758" } }),
    )
    .unwrap_or_else(|e| panic!("{agent} could not report {outcome}: {e:?}"));
}

/// **The lane rule, through the real tick: answered AND finished its turn.**
///
/// Two arms differing in ONE fact — whether the reviewer `report`ed — because
/// `idle_since_ms` is what the release barrier reads and a verdict file does not
/// stamp it. The `still writing` arm is the negative control and it is the one
/// that matters: an implementation that released on the verdict alone would kill
/// a reviewer mid-turn, which is the judgment §3 forbids the driver making.
///
/// Both arms assert the same five things, so a run says what each arm did rather
/// than stopping at the first difference: whether a release was reported, whether
/// the pane is dead, who ended it, whether the record still names it, and whether
/// the session survived. The session is the whole promise — a released lane is
/// resumable, so the drive loses a pane and never a conversation.
#[test]
fn a_lane_that_answered_is_released_only_once_its_own_turn_has_ended() {
    for (arm, ends_turn) in [("reported", true), ("still writing", false)] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, lane) = briefed(&reg, &repo, &gh);
        // The digest a verdict binds to is computed from the body override, so it
        // must agree with the one FakeGh serves or every pass reads as stale.
        reg.set_pr_body_override(Some("b".to_string()));
        reg.set_pr_head_override(Some(HEAD_A.to_string()));
        let session_before = live_lanes(&reg, &group)
            .first()
            .and_then(|l| l["session"].as_str().map(str::to_string))
            .unwrap_or_default();

        record_pass_for(&reg, &group, &lane);
        if ends_turn {
            report_as(&reg, &group, &lane, Role::Reviewer, "approved");
        }
        assert_eq!(
            reg.agent(&lane).expect("the lane is on the roster").idle_since_ms.is_some(),
            ends_turn,
            "{arm}: the fixture's premise — a report is what ends a reviewer's turn"
        );

        let report = reg.rd_drive_group_with(&group, &gh, 30_000);
        let released: Vec<String> =
            report.released.iter().map(|(_, _, a)| a.clone()).collect();
        let rows = audit_details(&reg, &group, "rd-lane-released");
        let dead = reg.agent(&lane).map(|a| a.status == AgentStatus::Dead).unwrap_or(false);
        let ended_by = reg.exit_initiator(&lane);
        let rec = live_lanes(&reg, &group).first().cloned().unwrap_or_default();

        assert_eq!(released, if ends_turn { vec![lane.clone()] } else { vec![] }, "{arm}");
        assert_eq!(rows.len(), usize::from(ends_turn), "{arm}: rd-lane-released rows {rows:?}");
        assert_eq!(dead, ends_turn, "{arm}: the pane's liveness");
        assert_eq!(
            ended_by,
            ends_turn.then_some(ExitInitiator::DriverRelease),
            "{arm}: a released pane is stamped as the DRIVER's doing, so the exit notice is \
             routed to the audit log rather than costing the orchestrator a turn"
        );
        assert_eq!(
            rec["agent"].as_str().unwrap_or_default().is_empty(),
            ends_turn,
            "{arm}: the record must stop naming a pane that is gone, and keep naming one \
             that is not: {rec}"
        );
        assert_eq!(
            rec["session"].as_str().unwrap_or_default(),
            session_before,
            "{arm}: the conversation survives either way — that is the whole promise, and a \
             release that dropped it would cost the review rather than a slot"
        );
        if ends_turn {
            let row = &rows[0];
            assert_eq!(row["pr"], json!(1758), "{arm}: {row}");
            assert_eq!(row["block"], json!("rev-std"), "{arm}: {row}");
            assert_eq!(row["agent"], json!(lane), "{arm}: {row}");
            assert_eq!(row["reason"], json!("verdict-recorded"), "{arm}: {row}");
            assert_eq!(
                row["session"].as_str().unwrap_or_default(),
                session_before,
                "{arm}: the row carries the session the next round resumes, so a scorecard \
                 counting released slots can also say what was kept: {row}"
            );
        }
    }
}

/// **A released pane frees its slot IMMEDIATELY** — the cap-accounting half of
/// #2501, and the one the issue is actually about.
///
/// "Immediately" is the claim under test rather than a flourish: `mark_dead` is
/// what drops the agent out of `live_delegate_count`, and it runs inside the
/// release rather than being left to the pty waiter thread. So the assertion is
/// a SPAWN — refused at the cap before the release, accepted after it, with
/// nothing in between but the tick that released the lane.
///
/// The refusal before is the control. Without it, "the spawn succeeded" is
/// equally true of a group that was never at its cap.
#[test]
fn a_released_lane_frees_its_delegate_slot_on_the_tick_that_releases_it() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    // Two slots: the worker takes one and the driver's lane takes the other.
    let group = reg
        .create_group(&repo.path(), Guardrails { max_agents: 2, ..rails() })
        .unwrap()
        .id;
    let w = reg.spawn_agent(&group, Role::Worker, "w", "", false, None).unwrap();
    let session = w.session_id.clone().expect("claude mints a session id at spawn");
    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 0);
    assert_eq!(out["driving"], json!(true), "drive_review refused: {out}");
    reg.rd_drive_group_with(&group, &gh, 10_000);
    let opened = reg.rd_drive_group_with(&group, &gh, 20_000);
    let (_pr, _block, lane) =
        opened.lanes_opened.first().cloned().expect("the second tick opens the lane");
    reg.set_pr_body_override(Some("b".to_string()));
    reg.set_pr_head_override(Some(HEAD_A.to_string()));

    // The control: the group is genuinely full.
    let refused = reg.spawn_agent(&group, Role::Worker, "w2", "", false, None);
    let refusal = refused.err().expect("the cap must refuse a third delegate");
    assert!(
        loomux_lib::orchestration::is_live_cap_refusal(&refusal),
        "the control must be the LIVE-DELEGATE CAP refusing, not a rate limit or a bad \
         block: {refusal}"
    );

    record_pass_for(&reg, &group, &lane);
    report_as(&reg, &group, &lane, Role::Reviewer, "approved");
    let report = reg.rd_drive_group_with(&group, &gh, 30_000);
    assert_eq!(report.released.len(), 1, "the tick under test must release the lane");

    // …and the slot is free on that same tick, with no pty waiter having run.
    reg.spawn_agent(&group, Role::Worker, "w2", "", false, None)
        .expect("the released pane's slot must be free immediately, not once its process exits");
}

/// **The worker rule, through the real tick**, and its negative control on the
/// one axis that decides: the word the worker reported.
///
/// `done` is a report the drive consumes and acts on — arc 8 — after which the
/// pane holds nothing but a slot, and the next hand-back resumes the SESSION.
/// `blocked` is INVARIANT 3 territory: it parks the drive for the orchestrator,
/// which is about to talk to that very pane, so killing it would take away the
/// thing the hold exists to hand over.
#[test]
fn the_worker_pane_is_released_on_a_done_report_and_kept_on_a_blocked_one() {
    for (arm, outcome, released) in [("done", "done", true), ("blocked", "blocked", false)] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, _lane) = briefed(&reg, &repo, &gh);
        let session_before = driven_worker_session(&reg, &group);

        // Red checks at a new head take the drive to `fix-wait`, which is the
        // only state a hand-back happens in.
        gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
        gh.set_facts("OPEN", HEAD_B);
        reg.rd_drive_group_with(&group, &gh, 30_000);
        let handed = reg.rd_drive_group_with(&group, &gh, 40_000);
        assert_eq!(status_state(&reg, &group), "fix-wait", "{arm}");
        let (_pr, worker) =
            handed.handbacks.first().cloned().expect("the hand-back resumed a worker");

        report_as(&reg, &group, &worker, Role::Worker, outcome);
        let report = reg.rd_drive_group_with(&group, &gh, 50_000);

        let got: Vec<String> = report.released.iter().map(|(_, _, a)| a.clone()).collect();
        let rows = audit_details(&reg, &group, "rd-worker-released");
        let dead = reg.agent(&worker).map(|a| a.status == AgentStatus::Dead).unwrap_or(false);

        assert_eq!(got, if released { vec![worker.clone()] } else { vec![] }, "{arm}");
        assert_eq!(rows.len(), usize::from(released), "{arm}: rows {rows:?}");
        assert_eq!(dead, released, "{arm}: the worker pane's liveness");
        assert_eq!(
            driven_worker_session(&reg, &group),
            session_before,
            "{arm}: the drive keeps the session either way — a hand-back resumes the \
             conversation, never the pane"
        );
        if released {
            let row = &rows[0];
            assert_eq!(row["pr"], json!(1758), "{arm}: {row}");
            assert_eq!(row["agent"], json!(worker), "{arm}: {row}");
            assert_eq!(row["reason"], json!("report-consumed"), "{arm}: {row}");
            assert_eq!(row["session"], json!(session_before), "{arm}: {row}");
            assert_eq!(
                reg.exit_initiator(&worker),
                Some(ExitInitiator::DriverRelease),
                "{arm}"
            );
        } else {
            assert_eq!(
                status_state(&reg, &group),
                "held",
                "{arm}: the fixture's premise — a blocked worker parks the drive, and the \
                 pane the orchestrator is about to speak to must still be there"
            );
        }
    }
}

/// **The worker pane is released on the tick that consumes its report even when
/// that tick is in `ci-wait`** (#2811 S1) — the ordinary push-then-report round,
/// which is 15 of the 20 hand-backs the measured session produced and every one
/// of the ones that held a slot for a whole review round.
///
/// The test above is the same rule on the OTHER route: a body-only fix, where
/// there is nothing to push, so the report lands while the drive is still in
/// `fix-wait`. That route was the only one #2501 covered, and it is exactly the
/// 5 releases the audit recorded — which is how a rule that never fired for
/// three quarters of its subjects stayed green for a month.
///
/// Both arms here PUSH; they differ in the word the worker then reports, which
/// is the axis that decides. `blocked` in `ci-wait` is INVARIANT 3 territory
/// just as it is in `fix-wait`, so the pane the hold hands to the orchestrator
/// must still be there.
#[test]
fn the_worker_pane_is_released_when_its_report_lands_in_ci_wait_after_a_push() {
    // The third head this file needs: the hand-back is at HEAD_B and the fix is
    // pushed on top of it, so arc 7 has a head move to see.
    const HEAD_C: &str = "cc33dd44ee55ff6677889900aabbccddeeff0011";
    for (arm, outcome, released) in [("done", "done", true), ("blocked", "blocked", false)] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, _lane) = briefed(&reg, &repo, &gh);
        let session_before = driven_worker_session(&reg, &group);

        // Red checks at a new head take the drive to `fix-wait` and hand back.
        gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
        gh.set_facts("OPEN", HEAD_B);
        reg.rd_drive_group_with(&group, &gh, 30_000);
        let handed = reg.rd_drive_group_with(&group, &gh, 40_000);
        assert_eq!(status_state(&reg, &group), "fix-wait", "{arm}");
        let (_pr, worker) =
            handed.handbacks.first().cloned().expect("the hand-back resumed a worker");

        // **The worker PUSHES.** The head moves under `fix-wait`, which is arc 7,
        // and the drive goes back to `ci-wait` to watch the new matrix. The
        // report has not arrived yet, so this tick must release nothing — the
        // negative control that keeps the assertion below about the REPORT.
        //
        // The new matrix is GREEN, which is what lets arc 2 be taken once the
        // report lands. An empty check list is not green — it is "no checks
        // reported", which `ci-wait` waits on — so the payload is the one
        // `FakeGh::green` uses.
        gh.set_checks(r#"[{"name":"build","state":"SUCCESS","link":"x"}]"#);
        gh.set_facts("OPEN", HEAD_C);
        let pushed = reg.rd_drive_group_with(&group, &gh, 50_000);
        assert_eq!(status_state(&reg, &group), "ci-wait", "{arm}: arc 7 puts it back in ci-wait");
        assert!(
            pushed.released.is_empty(),
            "{arm}: a push is not a report — released {:?}",
            pushed.released
        );

        report_as(&reg, &group, &worker, Role::Worker, outcome);
        let report = reg.rd_drive_group_with(&group, &gh, 60_000);

        let got: Vec<String> = report.released.iter().map(|(_, _, a)| a.clone()).collect();
        let rows = audit_details(&reg, &group, "rd-worker-released");
        let dead = reg.agent(&worker).map(|a| a.status == AgentStatus::Dead).unwrap_or(false);

        assert_eq!(got, if released { vec![worker.clone()] } else { vec![] }, "{arm}");
        assert_eq!(rows.len(), usize::from(released), "{arm}: rows {rows:?}");
        assert_eq!(dead, released, "{arm}: the worker pane's liveness");
        if released {
            let row = &rows[0];
            assert_eq!(row["agent"], json!(worker), "{arm}: {row}");
            assert_eq!(row["reason"], json!("report-consumed"), "{arm}: {row}");
            assert_eq!(row["session"], json!(session_before), "{arm}: {row}");
            // **At the PUSHED head**, which is what says the release belongs to
            // this round rather than to the hand-back that preceded it — the
            // audit shape §1(b) used to tell the two apart, and the one that
            // showed all five pre-#2811 S1 releases were body-only fixes.
            assert_eq!(row["head"], json!(HEAD_C), "{arm}: {row}");
            assert_eq!(
                status_state(&reg, &group),
                "review-wait",
                "{arm}: …and the same tick took arc 2, so the release rides the arc that \
                 consumed the report rather than a tick of its own"
            );
        } else {
            assert_eq!(
                status_state(&reg, &group),
                "held",
                "{arm}: a blocked worker parks the drive, and the pane the orchestrator is \
                 about to speak to must still be there"
            );
        }
    }
}

/// Drive to a `gate-check` tick that is **still holding both panes** — the one
/// route that reaches #2811 S1's terminal rule with a worker to release.
///
/// A satisfied drive normally has no worker pane left: the `ci-wait` rule
/// releases it on the tick that consumes its report. So the worker here reported
/// `blocked`, which parks the drive and KEEPS the pane (INVARIANT 3); the
/// orchestrator dispositioned it and resumed; and the drive then finished
/// without ever asking that worker for anything again. Arc 11 clears the arc-7
/// anchor, so no hand-back is outstanding at the exit.
///
/// The lane is kept the other way: `review_verdict` does not end a turn
/// (`idle_since_ms` is stamped by `report`), so the release barrier refuses it
/// on the `review-wait` tick and the pane survives into `gate-check`.
///
/// Returns `(group, worker pane, lane pane, worker session)` with the drive in
/// `gate-check`, one tick short of `satisfied`. Whether the lane then ENDS its
/// turn is the caller's to decide, and it is the axis the two tests below
/// differ on.
pub(crate) fn at_gate_check_holding_both_panes(
    reg: &OrchRegistry,
    repo: &Repo,
    gh: &FakeGh,
) -> (GroupId, String, String, String) {
    let (group, _lane0) = briefed(reg, repo, gh);
    let session = driven_worker_session(reg, &group);
    reg.set_pr_body_override(Some("b".to_string()));

    // Red at a new head: hand-back, and the worker answers `blocked`.
    gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
    gh.set_facts("OPEN", HEAD_B);
    reg.set_pr_head_override(Some(HEAD_B.to_string()));
    reg.rd_drive_group_with(&group, gh, 30_000);
    let handed = reg.rd_drive_group_with(&group, gh, 40_000);
    let (_pr, worker) = handed.handbacks.first().cloned().expect("the hand-back resumed a worker");
    report_as(reg, &group, &worker, Role::Worker, "blocked");
    reg.rd_drive_group_with(&group, gh, 50_000);
    assert_eq!(
        status_state(reg, &group),
        "held",
        "the fixture's premise: a blocked worker parks the drive"
    );
    assert!(
        reg.agent(&worker).is_some_and(|a| a.status != AgentStatus::Dead),
        "…and its pane is KEPT, which is what makes it available at the exit"
    );

    // The orchestrator dispositions and resumes; CI is green at the same head.
    gh.set_checks(r#"[{"name":"build","state":"SUCCESS","link":"x"}]"#);
    let out = reg.drive_review_with(&group, gh, 1758, &session, false, 0, "orch-1", 60_000);
    assert_eq!(out["driving"], json!(true), "the resume was refused: {out}");
    // Two ticks, as `briefed` takes: arc 11 re-enters `ci-wait`, the first tick
    // reads green and advances to `review-wait`, the second opens the lane.
    reg.rd_drive_group_with(&group, gh, 65_000);
    let reopened = reg.rd_drive_group_with(&group, gh, 70_000);
    let lane = reopened
        .lanes_opened
        .first()
        .cloned()
        .map(|(_, _, a)| a)
        .unwrap_or_else(|| panic!("the resumed drive briefs its lane: {reopened:?}"));

    // The lane answers but does not end its turn, so the `review-wait` tick's
    // release is refused and the pane survives into `gate-check`.
    record_pass_for(reg, &group, &lane);
    let to_gate = reg.rd_drive_group_with(&group, gh, 80_000);
    assert_eq!(status_state(reg, &group), "gate-check");
    assert!(
        to_gate.released.is_empty(),
        "the control: a lane mid-turn is not released, so what a caller observes at the \
         terminal step is the TERMINAL rule and not condition 2 firing early: {:?}",
        to_gate.released
    );
    (group, worker, lane, session)
}

/// **A TERMINAL step releases the panes it is finished with BEFORE the exit
/// notice is built** (#2811 S1) — so the notice names what is really left, and
/// the orchestrator is not handed a list of panes to kill by hand.
///
/// §1(f) measured what that hand-off cost: the orchestrator killed the reporting
/// worker in the same second it started 4 of one session's 16 drives, and each
/// following hand-back then resumed the session into a FRESH pane. The pane was
/// resumable the whole time; nobody was going to speak to it again.
#[test]
fn a_satisfied_tick_releases_its_panes_before_it_writes_the_satisfied_row() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, worker, lane, session) = at_gate_check_holding_both_panes(&reg, &repo, &gh);

    // The lane ENDS its turn, so both panes are idle at the tick that satisfies.
    report_as(&reg, &group, &lane, Role::Reviewer, "done");

    let before = audit_actions(&reg, &group).len();
    let end = reg.rd_drive_group_with(&group, &gh, 90_000);
    let actions: Vec<String> = audit_actions(&reg, &group).split_off(before);

    let pos = |a: &str| actions.iter().position(|x| x == a);
    let sat = pos("rd-satisfied").unwrap_or_else(|| panic!("the drive must satisfy: {actions:?}"));
    let lane_row =
        pos("rd-lane-released").unwrap_or_else(|| panic!("the lane must be released: {actions:?}"));
    let worker_row = pos("rd-worker-released")
        .unwrap_or_else(|| panic!("the worker must be released: {actions:?}"));
    assert!(lane_row < sat, "the lane's release precedes the satisfied row: {actions:?}");
    assert!(worker_row < sat, "…and so does the worker's: {actions:?}");

    let released: Vec<String> = end.released.iter().map(|(_, _, a)| a.clone()).collect();
    assert!(released.contains(&lane), "the lane pane went: {released:?}");
    assert!(released.contains(&worker), "the worker pane went: {released:?}");
    assert_eq!(
        audit_details(&reg, &group, "rd-worker-released")[0]["reason"],
        json!("drive-ended"),
        "the reason is not `report-consumed`: no report was consumed on this path, and an \
         audit reason is a claim"
    );

    // **The notice, which is the whole point of doing this before it is built.**
    let notice = end
        .notices
        .iter()
        .find(|n| n.contains("GATE SATISFIED"))
        .unwrap_or_else(|| panic!("a satisfied drive owes a notice: {:?}", end.notices));
    assert!(!notice.contains(&lane), "a released pane is not named as still running: {notice}");
    assert!(!notice.contains(&worker), "…nor is the worker: {notice}");
    assert!(
        notice.contains(&format!("worker session {session} resumes with spawn_agent(resume:)")),
        "…and what replaces it is the handle that still works: {notice}"
    );
}

/// Tick until the drive reaches `gate-check`, and answer the clock the last
/// tick ran at — so a caller can put its own tick strictly after it.
///
/// Bounded and asserting, never a silent give-up: how many ticks a one-lane gate
/// takes is an implementation detail of the states between `review-wait` and the
/// gate, and a test whose subject is the EXIT should not redden when that count
/// moves. A drive that never gets there panics here rather than letting the
/// caller measure a release at a step that is not the one it named.
fn tick_to_gate_check(reg: &OrchRegistry, group: &GroupId, gh: &FakeGh, from_ms: u64) -> u64 {
    let mut at = from_ms;
    for _ in 0..8 {
        reg.rd_drive_group_with(group, gh, at);
        if status_state(reg, group) == "gate-check" {
            return at;
        }
        at += 10_000;
    }
    panic!("the drive never reached gate-check; it is at {}", status_state(reg, group));
}

/// **A drive that never handed back still releases the worker pane at the
/// satisfied exit** (#3250) — the drive shape the release rule could not see.
///
/// The fixture is the ordinary one and that is the finding: an orchestrator
/// starts a drive on a worker that has already pushed and reported, CI is green
/// at that head, every required lane passes, and no hand-back is ever taken. So
/// `worker_agent` is empty for the whole drive — it is only ever written by a
/// hand-back — and `DriveEntry::owned_panes` names nothing on the worker side.
/// The terminal rule was keyed on that field, so it proposed no candidate at
/// all and the pane the orchestrator named at `start_review_drive` was still
/// alive and idle when the drive wrote `rd-satisfied`.
///
/// Measured twice on the beta12 build: PR #3243 (`w-2739`) and PR #3248
/// (`w-2747`), both `rd-started` -> `rd-satisfied` with no `rd-handback` and no
/// `rd-worker-released` row, both ending with an orchestrator killing the pane
/// by hand before `git worktree remove` would run.
///
/// **What this pins is the pane's FATE, not the candidate's existence** — the
/// row, the death, the caller's own list, and the session surviving so the
/// promise the exit notice makes still holds. The negative control is one test
/// down: a pane the barrier refuses (busy) stays exactly where it is, and the
/// widening here cannot reach past that barrier because it only widens the
/// population the barrier is asked about.
#[test]
fn a_satisfied_drive_releases_the_worker_pane_it_never_handed_back_to() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;
    let w = reg
        .spawn_agent(&group, Role::Worker, "w", "", false, None)
        .expect("the worker whose session the orchestrator is about to name");
    let worker = w.id.clone();
    with_pane(&reg, &worker, 41);
    let session = w.session_id.clone().expect("claude mints a session id at spawn");
    // It has finished and said so: idle, alive, on that session — which is the
    // only reason the release barrier could take it at all.
    report_as(&reg, &group, &worker, Role::Worker, "done");

    // A stable body digest, as the other gate fixtures take: a verdict binds to
    // a revision AND a body, and a drive whose body digest moves under it never
    // reaches the gate at all.
    reg.set_pr_body_override(Some("b".to_string()));
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 0);
    assert_eq!(out["driving"], json!(true), "drive_review refused: {out}");
    reg.rd_drive_group_with(&group, &gh, 10_000);
    let opened = reg.rd_drive_group_with(&group, &gh, 20_000);
    let lane = opened
        .lanes_opened
        .first()
        .cloned()
        .map(|(_, _, a)| a)
        .unwrap_or_else(|| panic!("the second tick opens the gate's lane: {opened:?}"));
    record_pass_for(&reg, &group, &lane);
    report_as(&reg, &group, &lane, Role::Reviewer, "done");
    // Ticked to the gate rather than counted to it: how many ticks a one-lane
    // gate takes is not the axis under test, and pinning it here would make this
    // test fail for a reason that has nothing to do with the release.
    let at = tick_to_gate_check(&reg, &group, &gh, 30_000);
    assert!(
        reg.agent(&worker).is_some_and(|a| a.status != AgentStatus::Dead),
        "…and nothing before the exit has touched the worker pane"
    );
    assert!(
        drives_json(&reg, &group)["entries"][0]["worker_agent"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "…and the drive never handed back, which is the whole shape under test"
    );

    let end = reg.rd_drive_group_with(&group, &gh, at + 10_000);
    // Read off the AUDIT, not the status: a satisfied drive is pruned on the
    // same tick, so `status_state` answers "" for it rather than "satisfied".
    assert!(
        audit_actions(&reg, &group).contains(&"rd-satisfied".to_string()),
        "the fixture's premise: this tick is the satisfied exit"
    );

    let released: Vec<String> = end.released.iter().map(|(_, _, a)| a.clone()).collect();
    assert_eq!(released, vec![worker.clone()], "the worker pane goes at the exit");
    let rows = audit_details(&reg, &group, "rd-worker-released");
    assert_eq!(rows.len(), 1, "one row per pane that actually went: {rows:?}");
    assert_eq!(rows[0]["agent"], json!(worker), "…naming the pane: {:?}", rows[0]);
    assert_eq!(rows[0]["reason"], json!("drive-ended"), "…and why: {:?}", rows[0]);
    assert_eq!(
        rows[0]["session"],
        json!(session),
        "…and the session it is released ONTO, which is what makes it resumable: {:?}",
        rows[0]
    );
    assert!(
        reg.agent(&worker).is_some_and(|a| a.status == AgentStatus::Dead),
        "the pane is really gone — a row without a death is the claim #2501 forbids"
    );
    let notice = end
        .notices
        .iter()
        .find(|n| n.contains("GATE SATISFIED"))
        .unwrap_or_else(|| panic!("a satisfied drive owes a notice: {:?}", end.notices));
    assert!(!notice.contains(&worker), "a released pane is not named as still running: {notice}");
    assert!(
        notice.contains(&format!("worker session {session} resumes with spawn_agent(resume:)")),
        "…and what replaces it is the handle that still works: {notice}"
    );
}

/// **The barrier still decides, and a BUSY pane on the drive's session stays**
/// (#3250) — the negative control for the widening above, and the one that says
/// it widened a POPULATION rather than granting the driver a new kill.
///
/// Same fixture, one fact changed: the worker never reported, so it is mid-turn
/// at the exit. `release_driven_pane` refuses a pane that is not idle, and the
/// drive must then leave it alone and say nothing — a release row for a pane
/// that is still running is exactly the false claim §5.4 asks a reader to be
/// able to count on.
#[test]
fn a_busy_pane_on_the_drives_session_is_not_released_at_the_satisfied_exit() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;
    let w = reg.spawn_agent(&group, Role::Worker, "w", "", false, None).expect("a worker");
    let worker = w.id.clone();
    with_pane(&reg, &worker, 41);
    let session = w.session_id.clone().expect("claude mints a session id at spawn");
    // Mid-turn through the product's own signal: `report` stamps
    // `idle_since_ms` for `done`/`blocked` and CLEARS it for `progress`, which
    // is the one word that says "still working". A fresh pane reads as idle, so
    // without this the arm would be the positive case wearing the label of the
    // negative one.
    report_as(&reg, &group, &worker, Role::Worker, "progress");

    // A stable body digest, as the other gate fixtures take: a verdict binds to
    // a revision AND a body, and a drive whose body digest moves under it never
    // reaches the gate at all.
    reg.set_pr_body_override(Some("b".to_string()));
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 0);
    assert_eq!(out["driving"], json!(true), "drive_review refused: {out}");
    reg.rd_drive_group_with(&group, &gh, 10_000);
    let opened = reg.rd_drive_group_with(&group, &gh, 20_000);
    let lane = opened
        .lanes_opened
        .first()
        .cloned()
        .map(|(_, _, a)| a)
        .unwrap_or_else(|| panic!("the second tick opens the gate's lane: {opened:?}"));
    record_pass_for(&reg, &group, &lane);
    report_as(&reg, &group, &lane, Role::Reviewer, "done");
    let at = tick_to_gate_check(&reg, &group, &gh, 30_000);
    assert!(
        reg.agent(&worker).is_some_and(|a| a.idle_since_ms.is_none()),
        "the fixture's premise: this pane is mid-turn, so the barrier must refuse it"
    );

    let end = reg.rd_drive_group_with(&group, &gh, at + 10_000);
    // Read off the AUDIT, not the status: a satisfied drive is pruned on the
    // same tick, so `status_state` answers "" for it rather than "satisfied".
    assert!(
        audit_actions(&reg, &group).contains(&"rd-satisfied".to_string()),
        "the fixture's premise: this tick is the satisfied exit"
    );
    assert!(
        end.released.iter().all(|(_, _, a)| *a != worker),
        "a busy pane is not released: {:?}",
        end.released
    );
    assert!(
        audit_details(&reg, &group, "rd-worker-released").is_empty(),
        "…and no row claims it was"
    );
    assert!(
        reg.agent(&worker).is_some_and(|a| a.status != AgentStatus::Dead),
        "…and it is still there for the orchestrator to speak to"
    );

    // **And the exit NAMES it** (review round 3, B2). A pane that survives the
    // exit is one the orchestrator has to dispose of, and this is the last line
    // the drive ever writes — the entry is terminal and is pruned, so there is
    // no later tick and no later notice. Before this the founding pane was in
    // no clause at all: `owned_panes` is empty for a drive that never handed
    // back, so the notice said nothing about the worker side and the
    // conversation that just ended was recoverable by nothing the reader could
    // see.
    let notice = end
        .notices
        .iter()
        .find(|n| n.contains("GATE SATISFIED"))
        .unwrap_or_else(|| panic!("a satisfied drive owes a notice: {:?}", end.notices));
    assert!(
        notice.contains(&worker),
        "the pane that survived the exit is named for the orchestrator to dispose of: {notice}"
    );
}

/// A drive started on a session that already carries `panes` worker panes, all
/// idle, walked to `gate-check` — the #3250 shape with the pane count as its
/// one axis.
///
/// Answers `(group, panes oldest-first, session, the clock the last tick ran
/// at)`. The panes are spawned in order and every one of them reports `done`
/// before the drive starts, which is what an orchestrator is looking at when it
/// hands a finished PR to the driver.
fn drive_started_on_session(
    reg: &OrchRegistry,
    repo: &Repo,
    gh: &FakeGh,
    panes: usize,
) -> (GroupId, Vec<String>, String, u64) {
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;
    let first = reg
        .spawn_agent(&group, Role::Worker, "w", "", false, None)
        .expect("the worker whose session the orchestrator is about to name");
    let session = first.session_id.clone().expect("claude mints a session id at spawn");
    let mut ids = vec![first.id.clone()];
    for k in 1..panes {
        // A SECOND pane on one conversation, which is what an orchestrator
        // produces by resuming a session whose pane is still alive — the shape
        // #3203 exists to stop the driver producing, and one the driver still
        // has to be able to clean up after.
        let more = reg
            .spawn_agent_ex(
                &group,
                Role::Worker,
                Some("worker".to_string()),
                &format!("w{k}"),
                "",
                false,
                None,
                None,
                Some(session.clone()),
                None,
                None,
            )
            .expect("a second pane on the same session");
        ids.push(more.id.clone());
    }
    for (k, id) in ids.iter().enumerate() {
        with_pane(reg, id, 41 + k as u32);
        report_as(reg, &group, id, Role::Worker, "done");
    }
    reg.set_pr_body_override(Some("b".to_string()));
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    let out = reg.drive_review_with(&group, gh, 1758, &session, false, 0, "orch-1", 0);
    assert_eq!(out["driving"], json!(true), "drive_review refused: {out}");
    // What the drive recorded is deliberately NOT asserted here. It is the
    // mechanism under test, not a premise of the fixture: pinning it in the
    // shared setup would make every test below fail on a premise rather than on
    // the pane's fate when the mechanism is absent, which is the weaker red.
    // `founding_panes_are_recorded_total_deduped_and_never_alongside_the_current_pane`
    // pins the recording itself.
    reg.rd_drive_group_with(&group, gh, 10_000);
    let opened = reg.rd_drive_group_with(&group, gh, 20_000);
    let lane = opened
        .lanes_opened
        .first()
        .cloned()
        .map(|(_, _, a)| a)
        .unwrap_or_else(|| panic!("the second tick opens the gate's lane: {opened:?}"));
    record_pass_for(reg, &group, &lane);
    report_as(reg, &group, &lane, Role::Reviewer, "done");
    let at = tick_to_gate_check(reg, &group, gh, 30_000);
    (group, ids, session, at)
}

/// **Two panes on one session are both released, each once, oldest first**
/// (#3250, review round 1 finding 2) — the plural path the first round of this
/// change never exercised.
///
/// Three claims in one fixture, because they fail differently: BOTH panes go
/// (a population that stopped at the first would pass a one-pane test), each
/// gets exactly ONE row (the dedup against `owned_panes`, which a merged list
/// can duplicate), and the rows are in the order the panes were opened — the
/// claim the release loop makes and which only the MERGED list can make true.
#[test]
fn two_panes_on_the_drives_session_are_both_released_once_each_oldest_first() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, panes, session, at) = drive_started_on_session(&reg, &repo, &gh, 2);

    let end = reg.rd_drive_group_with(&group, &gh, at + 10_000);
    assert!(
        audit_actions(&reg, &group).contains(&"rd-satisfied".to_string()),
        "the fixture's premise: this tick is the satisfied exit"
    );

    let released: Vec<String> = end.released.iter().map(|(_, _, a)| a.clone()).collect();
    assert_eq!(released, panes, "both panes go, oldest first: {released:?}");
    let rows = audit_details(&reg, &group, "rd-worker-released");
    let named: Vec<String> =
        rows.iter().map(|r| r["agent"].as_str().unwrap_or_default().to_string()).collect();
    assert_eq!(named, panes, "one row per pane, in the same order: {rows:?}");
    for row in &rows {
        assert_eq!(row["session"], json!(session), "each row names the session kept: {row}");
    }
    for p in &panes {
        assert!(reg.agent(p).is_some_and(|a| a.status == AgentStatus::Dead), "{p} is really gone");
    }
}

/// **A pane opened on the session AFTER the drive started is not released**
/// (#3250, review round 1 premortem) — in the window the premortem named.
///
/// The orchestrator resumes the conversation between the `gate-check` tick and
/// the exit. A freshly spawned pane reads as idle, so the barrier would take
/// it; the release is refused here by the POPULATION instead. `founding_panes`
/// is a list recorded when `drive_review` read the session, and this pane was
/// not on it — which is the whole reason the population is a recorded list
/// rather than a live re-read of the session.
///
/// The pane the drive WAS started on is asserted to go in the same run, so this
/// cannot pass by the release having stopped working altogether.
#[test]
fn a_pane_opened_on_the_session_after_the_drive_started_survives_the_exit() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, panes, session, at) = drive_started_on_session(&reg, &repo, &gh, 1);
    let founding = panes[0].clone();

    let late = reg
        .spawn_agent_ex(
            &group,
            Role::Worker,
            Some("worker".to_string()),
            "w-late",
            "",
            false,
            None,
            None,
            Some(session.clone()),
            None,
            None,
        )
        .expect("the orchestrator resumes the session inside the window")
        .id;
    with_pane(&reg, &late, 61);
    report_as(&reg, &group, &late, Role::Worker, "done");
    assert!(
        reg.agent(&late).is_some_and(|a| a.idle_since_ms.is_some()),
        "the fixture's premise: this pane reads as idle, so only the population can save it"
    );

    let end = reg.rd_drive_group_with(&group, &gh, at + 10_000);
    let released: Vec<String> = end.released.iter().map(|(_, _, a)| a.clone()).collect();
    assert_eq!(
        released,
        vec![founding.clone()],
        "the founding pane goes and the late one does not: {released:?}"
    );
    assert!(
        reg.agent(&late).is_some_and(|a| a.status != AgentStatus::Dead),
        "…and the pane someone has just started speaking to is still there"
    );
    assert_eq!(
        audit_details(&reg, &group, "rd-worker-released").len(),
        1,
        "…and no row claims otherwise"
    );
}

/// **The CANCELLED exit releases the founding pane too** (#3250, review round 1
/// finding 1) — the same defect one terminal state over.
///
/// The first round of this change bounded the widening to `satisfied`, on the
/// argument that its notice is what makes the release safe. `cancelled` is a
/// terminal step too: the drive is over, the terminal rule already ends the
/// panes it OWNS there, and a PR that closed under a drive that never handed
/// back left the orchestrator's own worker pane alive exactly as #3243 and
/// #3248 did. The two exits differing was an accident of the guard, not a
/// decision.
#[test]
fn a_cancelled_exit_releases_the_pane_the_drive_was_started_on() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;
    let w = reg.spawn_agent(&group, Role::Worker, "w", "", false, None).expect("a worker");
    let worker = w.id.clone();
    with_pane(&reg, &worker, 41);
    let session = w.session_id.clone().expect("claude mints a session id at spawn");
    report_as(&reg, &group, &worker, Role::Worker, "done");
    reg.set_pr_body_override(Some("b".to_string()));
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 0);
    assert_eq!(out["driving"], json!(true), "drive_review refused: {out}");

    // **One tick with the PR still open, and it is load-bearing.** The startup
    // RECONCILE also cancels a drive whose PR reads closed, and it does so
    // without taking a tick — `releasable` is never asked there and nothing is
    // killed, which is its own documented behaviour and not what this test is
    // about. Spending the reconcile here leaves the arc under test: the tick's
    // own `decide`, which answers `cancelled` for `pr_open == Some(false)`.
    // The residual is disclosed in `docs/design/review-driver.md` §3.
    reg.rd_drive_group_with(&group, &gh, 10_000);

    // The PR closes under the drive — a human merged or closed it.
    gh.set_facts("CLOSED", HEAD_A);
    let end = reg.rd_drive_group_with(&group, &gh, 20_000);
    let actions = audit_actions(&reg, &group);
    assert!(
        actions.iter().any(|a| a.contains("cancel")) || status_state(&reg, &group) == "cancelled",
        "the fixture's premise: a closed PR cancels the drive: {actions:?}"
    );

    let released: Vec<String> = end.released.iter().map(|(_, _, a)| a.clone()).collect();
    assert_eq!(released, vec![worker.clone()], "the founding pane goes at this exit too");
    let rows = audit_details(&reg, &group, "rd-worker-released");
    assert_eq!(rows.len(), 1, "one row: {rows:?}");
    assert_eq!(rows[0]["reason"], json!("drive-ended"), "…with the terminal reason: {:?}", rows[0]);
    assert_eq!(rows[0]["session"], json!(session), "…naming the session kept: {:?}", rows[0]);
    assert!(
        reg.agent(&worker).is_some_and(|a| a.status == AgentStatus::Dead),
        "the pane is really gone"
    );
}

/// **The RECONCILE's cancel names the founding pane too** (#3250, review round
/// 3, B1) — the one exit that releases nothing at all.
///
/// A PR that closed while orrerix was not running is cancelled by reconcile
/// rather than by a tick, and reconcile asks `releasable` nothing: no pane is
/// killed, owned or founding. That is argued and left alone in
/// `docs/design/review-driver.md` §3 — but the argument rests on the orchestrator
/// being able to see what survived, and `owned_panes` is EMPTY on the worker
/// side for a drive that never handed back, so the pane the orchestrator handed
/// the drive appeared in no clause of that notice at all.
///
/// The lane pane is asserted in the same run as the control: the clause was
/// never empty, so a test reading only "the notice names a pane" would have
/// passed before the fix.
#[test]
fn the_reconcile_cancel_names_the_pane_the_drive_was_started_on() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;
    let w = reg.spawn_agent(&group, Role::Worker, "w", "", false, None).expect("a worker");
    let worker = w.id.clone();
    with_pane(&reg, &worker, 41);
    let session = w.session_id.clone().expect("claude mints a session id at spawn");
    report_as(&reg, &group, &worker, Role::Worker, "done");
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    reg.set_pr_body_override(Some("b".to_string()));
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 0);
    assert_eq!(out["driving"], json!(true), "drive_review refused: {out}");

    // The PR closes while nothing is ticking, so the once-per-process reconcile
    // is the producer — no tick runs with the PR open first.
    gh.set_facts("CLOSED", HEAD_A);
    make_delivery_land(&reg, &group, &orch.id, 7305);
    reg.rd_drive_group_with(&group, &gh, 10_000);
    assert!(
        reg.audit_log(&group)
            .into_iter()
            .any(|e| e.action == "rd-cancelled" && e.detail["at"] == json!("reconcile")),
        "this test is about RECONCILE's producer and it did not run"
    );
    reg.rd_drive_group_with(&group, &gh, 20_000);

    let landed = drive_notices(&reg, &group, 1758);
    assert_eq!(landed.len(), 1, "reconcile owes exactly one notice: {landed:?}");
    assert!(
        landed[0].contains("the PR is closed or merged"),
        "the fixture's premise: this is reconcile's own cancel: {}",
        landed[0]
    );
    assert!(
        landed[0].contains(&worker),
        "the pane the orchestrator handed the drive is named — nothing killed it, and this \
         notice is the only thing that says it is still there: {}",
        landed[0]
    );
    assert!(
        reg.agent(&worker).is_some_and(|a| a.status != AgentStatus::Dead),
        "…and it really is still there, which is what makes naming it true"
    );
}

/// **A terminal release the BARRIER refuses leaves that pane exactly where it
/// was — named in the notice, alive, and the orchestrator's** (#2811 S1) — the
/// residual `releasable`'s doc discloses, pinned rather than described.
///
/// A disclosed residual is a counterfactual, and only a test that performs the
/// edit pins one: without this, the suite covers the arms that work and the
/// disclosure could go false with nothing red to say so. The one difference from
/// the test above is that the lane never `report`s, so `idle_since_ms` is never
/// stamped and `release_driven_pane` refuses it — the same refusal that protects
/// a reviewer mid-review, arriving at an exit.
///
/// **The worker is still released in the same tick**, and that is the load-
/// bearing half rather than a bonus: it says the two candidates are decided and
/// applied per pane, so one refusal does not abandon the other release. An
/// implementation that gave up on the whole terminal list at the first `Err`
/// would pass every assertion in the test above and fail here.
#[test]
fn a_terminal_release_the_barrier_refuses_leaves_that_pane_named_and_alive() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, worker, lane, session) = at_gate_check_holding_both_panes(&reg, &repo, &gh);

    // The lane does NOT end its turn. Everything else is the test above.
    let end = reg.rd_drive_group_with(&group, &gh, 90_000);

    let released: Vec<String> = end.released.iter().map(|(_, _, a)| a.clone()).collect();
    assert_eq!(released, vec![worker.clone()], "the worker goes, the busy lane does not");
    assert!(
        audit_details(&reg, &group, "rd-lane-released").is_empty(),
        "a release row is written on the kill SUCCEEDING, never on the intent"
    );
    assert_eq!(
        reg.agent(&lane).map(|a| a.status == AgentStatus::Dead),
        Some(false),
        "a lane mid-turn is not killed by its drive ending — the judgment §3 forbids"
    );

    let notice = end
        .notices
        .iter()
        .find(|n| n.contains("GATE SATISFIED"))
        .unwrap_or_else(|| panic!("a satisfied drive owes a notice: {:?}", end.notices));
    assert!(
        notice.contains(&lane),
        "the refused pane is named, so the orchestrator can still dispose of it: {notice}"
    );
    assert!(
        notice.contains(&format!("worker session {session} resumes with spawn_agent(resume:)")),
        "…and the released worker is named by session in the same notice: {notice}"
    );
    assert!(!notice.contains(&worker), "…but not by a pane id that is gone: {notice}");
}

/// **A terminal tick whose WORKER release is refused still releases the lane**
/// (#2811 S1) — the other refusal cell, and the one that makes the pair
/// discriminate on more than one mutation operator.
///
/// The test above refuses the LANE, and `releasable` pushes the worker candidate
/// FIRST ("Condition 3, first, so the list reads worker-first exactly as
/// `owned_panes` does"), so there the worker's release is already done before
/// the refusal is reached. That fixture therefore cannot see a release loop that
/// `break`s at the first `Err` instead of `continue`ing — it produces identical
/// output. rev-std round 2 caught the overclaim; this is the fixture that closes
/// it rather than a reworded sentence.
///
/// Here the refusal comes FIRST. A `break` releases nothing and fails
/// `released == vec![lane]`; an all-or-nothing pre-check fails it the same way;
/// only the shipped per-candidate `continue` passes. Between the two tests, both
/// orderings of {refused, released} are covered.
///
/// The worker is made busy the way the product makes any delegate busy — the
/// orchestrator sends it a prompt, which stamps `idle_since_ms = None` before
/// the delivery (`mcp.rs`'s `send_prompt` arm) — rather than by reaching into
/// the registry, so the fixture is a state the running app really produces.
#[test]
fn a_terminal_tick_whose_worker_release_is_refused_still_releases_the_lane() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, worker, lane, session) = at_gate_check_holding_both_panes(&reg, &repo, &gh);

    // The lane ends its turn; the worker is put back to work by its
    // orchestrator, so the barrier will refuse it and take the lane instead.
    report_as(&reg, &group, &lane, Role::Reviewer, "done");
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7001);
    // The delivery itself may fail in a headless test; `idle_since_ms` is
    // cleared BEFORE it either way, which is the fact this fixture needs and is
    // the product's own ordering ("the intent to assign counts regardless of
    // delivery timing").
    let _ = dispatch(
        &reg,
        &Caller {
            agent_id: orch.id.clone(),
            group: group.clone(),
            role: Role::Orchestrator,
            role_hint: None,
        },
        "tools/call",
        &json!({ "name": "send_prompt", "arguments": {
            "agent_id": worker.clone(), "text": "one more thing while I have you" } }),
    );
    assert!(
        reg.agent(&worker).is_some_and(|a| a.idle_since_ms.is_none()),
        "the fixture's premise: the worker is working again, so the barrier must refuse it"
    );

    let end = reg.rd_drive_group_with(&group, &gh, 90_000);

    let released: Vec<String> = end.released.iter().map(|(_, _, a)| a.clone()).collect();
    assert_eq!(
        released,
        vec![lane.clone()],
        "the lane goes even though the candidate BEFORE it was refused"
    );
    assert!(
        audit_details(&reg, &group, "rd-worker-released").is_empty(),
        "and no row claims a worker release that did not happen"
    );
    assert_eq!(
        reg.agent(&worker).map(|a| a.status == AgentStatus::Dead),
        Some(false),
        "a worker mid-turn is not killed by its drive ending, at a terminal step either"
    );

    let notice = end
        .notices
        .iter()
        .find(|n| n.contains("GATE SATISFIED"))
        .unwrap_or_else(|| panic!("a satisfied drive owes a notice: {:?}", end.notices));
    assert!(notice.contains(&worker), "the refused worker pane is named: {notice}");
    assert!(!notice.contains(&lane), "…and the released lane is not: {notice}");
    assert!(
        !notice.contains(&format!("worker session {session} resumes")),
        "…and no resume clause is offered for a pane that is still running: {notice}"
    );
}

/// **A released lane comes back on its own session** — the claim the whole
/// narrowing rests on, performed rather than asserted.
///
/// The lane passes at one head, its pane is released, the head then moves, and
/// the next round briefs that same lane. What must happen is a RESUME: the
/// reviewer is asked again in the conversation it already had, in a fresh pane,
/// rather than replaced by a stranger.
///
/// The `resumed` flag on `rd-lane-spawned` is the discriminator (#2109 added it
/// for exactly this reason: a resumed lane and a fresh one otherwise produce the
/// same row, the same kind of pane id and the same brief).
#[test]
fn a_released_lane_is_resumed_on_its_own_session_for_the_next_round() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, lane) = briefed(&reg, &repo, &gh);
    reg.set_pr_body_override(Some("b".to_string()));
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    let session = live_lanes(&reg, &group)
        .first()
        .and_then(|l| l["session"].as_str().map(str::to_string))
        .expect("a claude lane records the session it was spawned on");

    record_pass_for(&reg, &group, &lane);
    report_as(&reg, &group, &lane, Role::Reviewer, "approved");
    let released = reg.rd_drive_group_with(&group, &gh, 30_000);
    assert_eq!(released.released.len(), 1, "the premise: the pane really was released");

    // A new head stales the pass, and the drive comes back round to the lane.
    gh.set_facts("OPEN", HEAD_B);
    let reopened = tick_until_lane(&reg, &gh, &group, 40_000)
        .expect("the next round must brief the lane again");
    assert_ne!(reopened, lane, "a released pane cannot be the one that takes the new brief");

    let spawns = audit_details(&reg, &group, "rd-lane-spawned");
    let last = spawns.last().expect("the re-brief is on the log");
    assert_eq!(last["agent"], json!(reopened), "{last}");
    assert_eq!(
        last["session"],
        json!(session),
        "the new pane must be the OLD conversation: {last}"
    );
    assert_eq!(
        last["resumed"],
        json!(true),
        "a released lane is resumed, never respawned cold — that is what makes the release \
         cost a slot and not a review: {last}"
    );
    // …and the release is not misreported as a lane this drive LOST. That row
    // means "the pane died and we replaced it", which is what an orchestrator
    // reads after killing an idle delegate to find out whether it caused this.
    assert!(
        audit_details(&reg, &group, "rd-lane-reopened").is_empty(),
        "a deliberate release must not be logged as a lost pane"
    );
}

/// **The driver kills nothing outside the narrowed states**, over the shapes
/// that are closest to being releasable and are not.
///
/// Each arm is a lane or a worker the drive owns, idle or not, whose ONE
/// difference from a releasable pane is named in its label. The positive control
/// is the other tests above; what this adds is that a tick over each of these
/// leaves every pane in the group alive.
#[test]
fn the_driver_releases_nothing_outside_the_narrowed_states() {
    for arm in ["silent lane", "stale verdict", "parking step"] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, lane) = briefed(&reg, &repo, &gh);
        // The digest a verdict binds to is computed from the body override, so it
        // must agree with the one FakeGh serves or every pass reads as stale.
        reg.set_pr_body_override(Some("b".to_string()));
        reg.set_pr_head_override(Some(HEAD_A.to_string()));

        match arm {
            // Briefed, and has said nothing. The commonest pane in a drive, and
            // the one a "release idle lanes" rule would have taken.
            "silent lane" => {
                report_as(&reg, &group, &lane, Role::Reviewer, "progress");
            }
            // It answered — about a revision the PR has moved past. A verdict is
            // bound to what it reviewed, so this lane owes another one.
            "stale verdict" => {
                record_pass_for(&reg, &group, &lane);
                report_as(&reg, &group, &lane, Role::Reviewer, "approved");
                gh.set_facts("OPEN", HEAD_B);
            }
            // It answered `escalate` at this head, which makes the STEP a park:
            // §6's notice hands these panes to a human and says they are still
            // running.
            //
            // **A parking STEP is what this arm spans, and it is not the same as
            // a parking DRIVE** (rev-final R1). `releasable`'s condition 1 reads
            // the step `decide` proposed; a tick whose step is live can still
            // end parked when the arm itself refuses, and there the release has
            // already happened. That case is the opposite claim and has its own
            // test — `a_fail_route_hand_back_at_the_cap_is_fed_by_the_lane_it_
            // releases`, where the drive must NOT park and the lane MUST be
            // released. Naming this arm for the step is what keeps the two
            // distinguishable instead of one label covering a property it
            // cannot see.
            _ => {
                let caller = Caller {
                    agent_id: lane.clone(),
                    group: group.clone(),
                    role: Role::Reviewer,
                    role_hint: None,
                };
                dispatch(
                    &reg,
                    &caller,
                    "tools/call",
                    &json!({ "name": "review_verdict", "arguments": {
                        "pr": "1758", "verdict": "escalate", "summary": "a call for a human" } }),
                )
                .expect("the escalation is recorded");
                report_as(&reg, &group, &lane, Role::Reviewer, "approved");
            }
        }

        let report = reg.rd_drive_group_with(&group, &gh, 30_000);
        assert!(
            report.released.is_empty(),
            "{arm}: the driver released {:?}",
            report.released
        );
        assert!(
            audit_details(&reg, &group, "rd-lane-released").is_empty()
                && audit_details(&reg, &group, "rd-worker-released").is_empty(),
            "{arm}: a release row with nothing released"
        );
        assert_eq!(
            reg.agent(&lane).map(|a| a.status == AgentStatus::Dead),
            Some(false),
            "{arm}: the lane's pane must still be alive"
        );
        if arm == "parking step" {
            assert_eq!(
                status_state(&reg, &group),
                "held",
                "{arm}: the fixture's premise — an escalation makes the step a park"
            );
        }
    }
}

/// **A release costs the orchestrator no turn**, which is the whole point of
/// #2501 and is a property of `exit_notice_route` rather than of the driver.
///
/// The pure half is asserted first, over the full initiator set, so a variant
/// added later cannot silently default into either route. Then the live half:
/// the orchestrator's pane receives nothing about the released lane.
#[test]
fn a_released_pane_reaches_the_audit_log_and_never_the_orchestrators_pane() {
    assert_eq!(
        exit_notice_route(Some(ExitInitiator::DriverRelease)),
        ExitNoticeRoute::AuditOnly,
        "prompting per released pane would spend the saving on announcing it"
    );
    assert_eq!(exit_notice_route(None), ExitNoticeRoute::Prompt, "the control: nobody asked");

    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, lane) = briefed(&reg, &repo, &gh);
    reg.set_pr_body_override(Some("b".to_string()));
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7101);

    record_pass_for(&reg, &group, &lane);
    report_as(&reg, &group, &lane, Role::Reviewer, "approved");
    let before = delivered_texts(&reg, &group).len();
    let report = reg.rd_drive_group_with(&group, &gh, 30_000);
    assert_eq!(report.released.len(), 1, "the premise: a pane really was released");

    let new_lines: Vec<String> =
        delivered_texts(&reg, &group).into_iter().skip(before).collect();
    assert!(
        !new_lines.iter().any(|t| t.contains(&lane) && t.contains("exited")),
        "an exit notice for a pane the driver itself released: {new_lines:?}"
    );
    // The positive control for the assertion above: the release IS on the record,
    // so "no line in the pane" is not "nothing happened".
    assert_eq!(
        audit_actions(&reg, &group).iter().filter(|a| *a == "rd-lane-released").count(),
        1,
        "the release must be reconstructible from the audit log, which is what makes \
         demoting its exit notice honest"
    );
}

/// **rev-final W1: the release must land BEFORE the step's own arm**, or the
/// feature reintroduces the orchestrator wake it exists to remove.
///
/// The path is the ordinary `fail` round, and #2501's own worker rule is what
/// makes it common: the previous round released the worker's pane, so the
/// hand-back that follows a `fail` has no live pane to reuse and must SPAWN —
/// under the live-delegate cap, while the lane pane this same tick is about to
/// release is still counted. Release after the arm and the spawn is refused, the
/// drive parks `held(cap-refused)` on a notice asking an orchestrator to free a
/// slot, and only then is one freed. A parked drive does not self-advance, so
/// that is a full orchestrator turn — the exact cost #2501 measured, in the
/// scenario it measured it in.
///
/// **The fixture's cap is real and the reuse decline is the mechanism**, not a
/// shortcut. Two slots: the worker holds one and the driver's lane the other. The
/// worker's pane is alive and idle but its last delivery is on record as not
/// having landed, so `rd_reuse_pane` declines it (#2089's `unconfirmed`) and the
/// hand-back falls through to a spawn — which is what a released previous pane
/// produces in production, reached here without needing a second round. The pane
/// stays ALIVE, so it still counts against the cap; that is the whole point.
///
/// Three assertions, and each fails under the pre-fix order: the drive reaches
/// `fix-wait` rather than `held`, a hand-back actually happened, and the lane was
/// released on that same tick. The `rd-held` check is the discriminator — under
/// the old order the drive parks with `cap-refused` and the first assertion is
/// what catches it.
#[test]
fn a_fail_route_hand_back_at_the_cap_is_fed_by_the_lane_it_releases() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let group = reg
        .create_group(&repo.path(), Guardrails { max_agents: 2, ..rails() })
        .unwrap()
        .id;
    let w = reg.spawn_agent(&group, Role::Worker, "w", "", false, None).unwrap();
    let session = w.session_id.clone().expect("claude mints a session id at spawn");
    // Alive, idle, and NOT delivery-ready: the reuse arm declines it, so the
    // hand-back has to spawn — while this pane still holds its slot.
    with_pane(&reg, &w.id, 7301);
    make_pane_ready(&reg, 7301, false);

    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 0);
    assert_eq!(out["driving"], json!(true), "drive_review refused: {out}");
    reg.set_pr_body_override(Some("b".to_string()));
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    reg.rd_drive_group_with(&group, &gh, 10_000);
    let opened = reg.rd_drive_group_with(&group, &gh, 20_000);
    let (_pr, _block, lane) =
        opened.lanes_opened.first().cloned().expect("the second tick opens the lane");

    // The premise: the group is genuinely full, so the hand-back's spawn can
    // only succeed if the release has already freed a slot.
    let refusal = reg
        .spawn_agent(&group, Role::Worker, "probe", "", false, None)
        .err()
        .expect("the cap must refuse a third delegate");
    assert!(
        loomux_lib::orchestration::is_live_cap_refusal(&refusal),
        "the premise must be the LIVE-DELEGATE CAP, not a rate limit: {refusal}"
    );

    // The lane answers `fail` at this head and ends its turn — the two facts
    // that make it releasable on the very tick the fail routes.
    let reviewer = Caller {
        agent_id: lane.clone(),
        group: group.clone(),
        role: Role::Reviewer,
        role_hint: None,
    };
    dispatch(
        &reg,
        &reviewer,
        "tools/call",
        &json!({ "name": "review_verdict", "arguments": {
            "pr": "1758", "verdict": "fail", "summary": "one finding" } }),
    )
    .expect("the lane records its verdict");
    report_as(&reg, &group, &lane, Role::Reviewer, "request_changes");

    let report = reg.rd_drive_group_with(&group, &gh, 30_000);

    assert_eq!(
        status_state(&reg, &group),
        "fix-wait",
        "the drive must take the fix route, not park at the cap on a slot this same tick \
         freed: held rows {:?}",
        audit_details(&reg, &group, "rd-held")
    );
    assert_eq!(
        report.handbacks.len(),
        1,
        "…because the hand-back's spawn found the slot the release had just made: {:?}",
        report.handbacks
    );
    assert_eq!(
        report.released.iter().map(|(_, _, a)| a.clone()).collect::<Vec<_>>(),
        vec![lane.clone()],
        "…and it is THIS lane that was released, on the same tick"
    );
    assert!(
        audit_details(&reg, &group, "rd-held").is_empty(),
        "no hold at all: a `cap-refused` here would be the driver asking for what it was \
         about to free"
    );
}

// ── §2.3 #2509's one-shot grace, through the real tick ──────────────────────

/// [`driven`], with the drive seeded one round short of the bound.
///
/// `rounds_already_spent` is §2.3's own parameter and means exactly this — the
/// budget is the drive's, not the driver's — so the fixture reaches the bound in
/// ONE real round instead of three, without any test writing a counter by hand.
fn driven_one_short(
    reg: &OrchRegistry,
    repo: &Repo,
    gh: &FakeGh,
) -> (GroupId, String) {
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;
    let w = reg
        .spawn_agent(&group, Role::Worker, "w", "", false, None)
        .expect("a worker to hand back to");
    let session = w.session_id.clone().expect("claude mints a session id at spawn");
    let seed = DriveLimits::default().max_review_rounds - 1;
    let out = reg.drive_review_with(&group, gh, 1758, &session, false, seed, "orch-1", 0);
    assert_eq!(out["driving"], serde_json::json!(true), "drive_review refused: {out}");
    (group, session)
}

/// A lane's blocking `fail` **and the report that ends its turn**, through the
/// real MCP arm — the verdict file is what the next tick reads, and the report
/// is what stamps `idle_since_ms`.
///
/// **The report is not decoration, and leaving it out is what a body-only round
/// cannot survive.** A re-brief at an UNCHANGED head is refused as a duplicate
/// while that lane's pane is still live (#2109), and the pane stops being live
/// only when the lane has finished its turn — at which point the tick that
/// routes its current `fail` RELEASES it (#2501) and the next round resumes the
/// same session in a fresh pane. A code round never meets that refusal, because
/// a moved head makes the brief a different revision; so a fixture that reported
/// on one path and not the other would differ from its control in two ways
/// instead of one.
fn record_fail_for(reg: &OrchRegistry, group: &GroupId, agent: &str, summary: &str) {
    let caller = Caller {
        agent_id: agent.to_string(),
        group: group.clone(),
        role: Role::Reviewer,
        role_hint: None,
    };
    dispatch(
        reg,
        &caller,
        "tools/call",
        &json!({ "name": "review_verdict", "arguments": {
            "pr": "1758", "verdict": "fail", "summary": summary } }),
    )
    .expect("the lane records its verdict");
    dispatch(
        reg,
        &caller,
        "tools/call",
        &json!({ "name": "report", "arguments": {
            "outcome": "request_changes", "note": "see the PR", "ref": "#1758" } }),
    )
    .expect("the lane reports, which ends its turn");
}

/// The worker's `report(done)`, which is what takes arc 8 out of `fix-wait`.
fn worker_done(reg: &OrchRegistry, group: &GroupId, worker: &str) {
    dispatch(
        reg,
        &Caller {
            agent_id: worker.to_string(),
            group: group.clone(),
            role: Role::Worker,
            role_hint: None,
        },
        "tools/call",
        &json!({ "name": "report", "arguments": {
            "outcome": "done", "note": "fixed", "ref": "#1758" } }),
    )
    .expect("a driven worker reports");
}

fn status_grace(reg: &OrchRegistry, group: &GroupId) -> bool {
    reg.review_drive_status(group)["drives"][0]["grace_used"]
        .as_bool()
        .unwrap_or(false)
}

/// **The acceptance test, and it is one fixture read twice.**
///
/// The drive reaches the bound on a real `fail`; the worker then makes the fix
/// #2509 is about — the PR **body** moves and the head does not — and the lane
/// fails again. That second fail is the one the bound would have parked on, and
/// the grace hands the worker back instead. Then a THIRD body-only fail parks,
/// because the grace is one per drive.
///
/// Every step is the real tick: `decide` chooses, `rd_drive_group_with` performs,
/// and the verdict files are written by `review_verdict` through the MCP arm.
/// What the engine unit tests pin as a rule, this pins as a sequence — including
/// the two things only the seam can carry, the `rd-round-grace` row and
/// `review_drive_status`'s `grace_used`.
#[test]
fn a_body_only_fail_at_the_bound_buys_one_more_round_then_the_next_one_parks() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _s) = driven_one_short(&reg, &repo, &gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7001);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    gh.set_body("b1");
    reg.set_pr_body_override(Some("b1".to_string()));

    // Round one: the lane opens and fails on the CODE. This is the round that
    // reaches the bound, and it must NOT be graced — nothing has been briefed
    // about a body yet.
    reg.rd_drive_group_with(&group, &gh, 10_000);
    let first = reg.rd_drive_group_with(&group, &gh, 20_000);
    let (_pr, _b, lane) = first.lanes_opened.first().cloned().expect("lane 0 opens");
    record_fail_for(&reg, &group, &lane, "fail - the retry loop is unbounded");
    let handed = reg.rd_drive_group_with(&group, &gh, 30_000);
    let (_pr, worker) = handed.handbacks.first().cloned().expect("arc 5 hands the PR back");
    // Round one's brief, captured NOW — it is the control for the grace clause
    // asserted two rounds below, and the pane it lives in may be reused.
    let ordinary = lane_brief(&reg, &worker);
    assert_eq!(status_state(&reg, &group), "fix-wait");
    assert_eq!(
        review_rounds(&reg, &group),
        DriveLimits::default().max_review_rounds as u64,
        "the fixture's premise: that round put the drive AT the bound"
    );
    assert!(!status_grace(&reg, &group), "and it spent no grace getting there");

    // The fix is BODY-ONLY: the body moves, the head does not.
    gh.set_body("b2");
    reg.set_pr_body_override(Some("b2".to_string()));
    worker_done(&reg, &group, &worker);
    reg.rd_drive_group_with(&group, &gh, 40_000);
    assert_eq!(
        status_state(&reg, &group),
        "review-wait",
        "arc 8 returns at an UNCHANGED head, which is the only shape this feature has"
    );

    // The lane is re-briefed about the body, and fails on it.
    let reopened = reg.rd_drive_group_with(&group, &gh, 50_000);
    let (_pr, _b, lane2) = reopened
        .lanes_opened
        .first()
        .cloned()
        .expect("a moved digest at an unchanged head re-briefs the lane");
    record_fail_for(&reg, &group, &lane2, "fail - the receipt still cites the old run");

    // **THE assertion.** Under the old rule this tick parks `held(review-limit)`.
    let graced = reg.rd_drive_group_with(&group, &gh, 60_000);
    assert_eq!(
        status_state(&reg, &group),
        "fix-wait",
        "a body-only fail at the bound buys one more round instead of parking; \
         held rows: {:?}",
        rows_for(&reg, &group, "rd-held")
    );
    let (_pr, worker2) = graced
        .handbacks
        .first()
        .cloned()
        .expect("and the grace really does reach the worker");

    // **The sentence the worker is handed, read back.** Without this the whole
    // `grace_clause` paragraph is reachable, rendered, and deletable with the
    // suite green — and it is the one thing that stops "attempt 3 of 3",
    // appearing for the second round running, reading as a broken counter.
    let graced_brief = lane_brief(&reg, &worker2);
    assert!(
        !graced_brief.contains("{{"),
        "an unregistered placeholder survived into the grace brief: {graced_brief}"
    );
    assert!(
        graced_brief.contains("This is a GRACE round PAST the review bound"),
        "the grace round must announce itself: {graced_brief}"
    );
    assert!(
        graced_brief.contains("The attempt count below still reads at the bound"),
        "…and pre-empt the number it contradicts, which is the whole reason the \
         clause exists: {graced_brief}"
    );
    assert!(
        graced_brief.contains("This is attempt 3 of 3."),
        "…the number really is the one being explained: {graced_brief}"
    );
    // One paragraph. The clause crosses five `\` continuations in the source and
    // would ship the source indent between them if one ever collapsed (#1457).
    let what = graced_brief
        .lines()
        .find(|l| l.contains("This is a GRACE round PAST"))
        .unwrap_or_else(|| panic!("the grace clause rendered on its own line: {graced_brief}"));
    assert!(
        !what.contains("          "),
        "the grace clause must arrive as one paragraph on one line: {what:?}"
    );
    // **The control, on the SAME drive**: round one was an ordinary round, and
    // its brief must carry none of this. Without it the assertions above pass
    // against a template that renders the sentence unconditionally.
    //
    // Captured at the time rather than re-read here, because the drive may have
    // resumed the worker into the SAME pane — in which case re-reading it now
    // would hand back the GRACE brief and the control would be about nothing.
    assert!(
        !ordinary.contains("GRACE round PAST"),
        "an ordinary review round must not claim to be a grace: {ordinary}"
    );
    let rows = rows_for(&reg, &group, "rd-round-grace");
    assert_eq!(rows.len(), 1, "exactly one grace row: {rows:?}");
    assert_eq!(rows[0]["pr"], json!(1758), "{rows:?}");
    assert_eq!(rows[0]["block"], json!("rev-std"), "…naming the lane whose fail earned it");
    assert_eq!(rows[0]["reason"], json!("body-only"), "{rows:?}");
    assert_eq!(rows[0]["head"], json!(HEAD_A), "{rows:?}");
    assert!(status_grace(&reg, &group), "review_drive_status shows it spent");
    assert_eq!(
        review_rounds(&reg, &group),
        DriveLimits::default().max_review_rounds as u64,
        "and the review-round counter has NOT moved — MAX_ROUNDS_CEILING bounds \
         exactly what it bounded before"
    );

    // A second body-only round: same shape, and this time it parks. The grace is
    // one per drive, not one per body-only fail.
    gh.set_body("b3");
    reg.set_pr_body_override(Some("b3".to_string()));
    worker_done(&reg, &group, &worker2);
    reg.rd_drive_group_with(&group, &gh, 70_000);
    let again = reg.rd_drive_group_with(&group, &gh, 80_000);
    let (_pr, _b, lane3) = again.lanes_opened.first().cloned().expect("re-briefed once more");
    record_fail_for(&reg, &group, &lane3, "fail - and now the diffstat is stale too");
    reg.rd_drive_group_with(&group, &gh, 90_000);
    assert_eq!(status_state(&reg, &group), "held");
    let held = rows_for(&reg, &group, "rd-held");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["reason"], json!("review-limit"), "{held:?}");
    assert_eq!(
        rows_for(&reg, &group, "rd-round-grace").len(),
        1,
        "and no second grace was written"
    );
}

/// **The negative control, and the only difference is what the worker moved.**
///
/// Same drive, same bound, same reviewer, same word — but the fix moves the
/// HEAD. The lane is then re-briefed about a revision nobody has reviewed, so no
/// body-only mark is stamped and the bound parks the drive on the next fail
/// exactly as it always did.
///
/// Without this, the acceptance test above is equally green under a rule that
/// graced every fail at the bound — which would be #2509 defeating INVARIANT 9
/// rather than narrowing it.
#[test]
fn a_code_fail_at_the_bound_still_parks_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _s) = driven_one_short(&reg, &repo, &gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7001);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    gh.set_body("b1");
    reg.set_pr_body_override(Some("b1".to_string()));

    reg.rd_drive_group_with(&group, &gh, 10_000);
    let first = reg.rd_drive_group_with(&group, &gh, 20_000);
    let (_pr, _b, lane) = first.lanes_opened.first().cloned().expect("lane 0 opens");
    record_fail_for(&reg, &group, &lane, "fail - the retry loop is unbounded");
    let handed = reg.rd_drive_group_with(&group, &gh, 30_000);
    let (_pr, worker) = handed.handbacks.first().cloned().expect("arc 5 hands the PR back");
    assert_eq!(
        review_rounds(&reg, &group),
        DriveLimits::default().max_review_rounds as u64,
        "the same premise as the acceptance test: AT the bound"
    );

    // The fix moves the CODE. The body moves too — a worker fixing code
    // re-writes its receipts — so the difference between the two tests is the
    // head alone, not "one of them edited the body".
    gh.set_facts("OPEN", HEAD_B);
    reg.set_pr_head_override(Some(HEAD_B.to_string()));
    gh.set_body("b2");
    reg.set_pr_body_override(Some("b2".to_string()));
    // Arc 7 FIRST, then the report. `decide_fix_receipts`' own residual is that
    // a worker which pushes and reports inside one tick window has its report
    // spent on the arc; the real sequence is push, checks settle, read the
    // matrix, THEN report, and the fixture follows it.
    reg.rd_drive_group_with(&group, &gh, 40_000); // fix-wait -> ci-wait (arc 7)
    worker_done(&reg, &group, &worker);
    reg.rd_drive_group_with(&group, &gh, 50_000); // ci-wait -> review-wait (arc 2)
    assert_eq!(status_head(&reg, &group), HEAD_B, "the fixture's premise: the code moved");
    assert_eq!(status_state(&reg, &group), "review-wait");

    let reopened = reg.rd_drive_group_with(&group, &gh, 60_000);
    let (_pr, _b, lane2) = reopened
        .lanes_opened
        .first()
        .cloned()
        .expect("a moved head re-briefs the lane");
    record_fail_for(&reg, &group, &lane2, "fail - the new guard is inverted");
    reg.rd_drive_group_with(&group, &gh, 70_000);

    assert_eq!(status_state(&reg, &group), "held", "a code fail at the bound parks");
    let held = rows_for(&reg, &group, "rd-held");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["reason"], json!("review-limit"), "{held:?}");
    assert!(
        rows_for(&reg, &group, "rd-round-grace").is_empty(),
        "and no grace was granted: {:?}",
        rows_for(&reg, &group, "rd-round-grace")
    );
    assert!(!status_grace(&reg, &group), "…so it is still there for a body-only round");
    // …and neither the hold nor the hand-back before it claims a grace.
    let notices = drive_notices(&reg, &group, 1758).join("\n");
    assert!(
        !notices.contains("grace"),
        "a drive that never earned a grace must not mention one: {notices}"
    );
}
