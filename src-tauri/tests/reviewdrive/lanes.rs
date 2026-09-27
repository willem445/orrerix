//! Lane re-briefs, round scope, resume, stalled lanes, the live report path, and the CI-arm brief sentences.
//!
//! One module of the `reviewdrive` integration-test target (`main.rs`),
//! split out of the former single-file `tests/reviewdrive.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ── the three functional defects that had no witness ────────────────────────

/// **The delta brief is reachable at all** — the pin for the defect that would
/// have shipped looking correct while delivering close to none of the saving.
///
/// `LaneRecord::at_head` is what distinguishes a lane that has **answered** from
/// one that has only been **asked** (§5.2 keeps it apart from `briefed_head` for
/// exactly this). Nothing wrote it: the tick read every lane's verdict file into
/// its notice inputs and threw the reading away. So a re-briefed lane looked
/// like a first-time lane forever, `driver-delta.md` was unreachable, and every
/// round would have got the first-call template — while the delta brief is the
/// line an orchestrator typed by hand nine times on one PR, and is most of what
/// §1 measures this feature's value as.
///
/// The fixture makes the lane answer and *then* moves the head, which is what
/// stales the pass and forces the re-brief. Both halves are asserted: the delta
/// template rendered, and it names the revision the lane previously answered at
/// — a brief that said "DELTA" while naming the current head would be the same
/// defect wearing the right word.
#[test]
fn a_lane_that_has_answered_is_re_briefed_with_the_delta_template() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _s) = driven(&reg, &repo, &gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7001);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));

    reg.rd_drive_group_with(&group, &gh, 10_000);
    let first = reg.rd_drive_group_with(&group, &gh, 20_000);
    let (_pr, _b, lane) = first.lanes_opened.first().cloned().expect("lane 0 opens");
    assert!(
        lane_brief(&reg, &lane).starts_with("Review PR #1758"),
        "the FIRST call is the first-call template, which is the control for the delta below"
    );

    // The lane answers at HEAD_A, through the real recording path.
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
            "pr": "1758", "verdict": "pass", "summary": "pass - nothing blocking" } }),
    )
    .expect("the lane records");

    // …and then the head moves, which stales that pass (§2.1's first carried-over
    // property) and sends the drive back round to re-brief the same lane.
    gh.set_facts("OPEN", HEAD_B);
    reg.set_pr_head_override(Some(HEAD_B.to_string()));
    reg.rd_drive_group_with(&group, &gh, 30_000); // review-wait -> ci-wait (arc 6)
    reg.rd_drive_group_with(&group, &gh, 40_000); // ci-wait -> review-wait (arc 2)
    let again = reg.rd_drive_group_with(&group, &gh, 50_000); // re-brief
    let (_pr, _b, lane2) = again
        .lanes_opened
        .first()
        .cloned()
        .expect("the stale lane must be re-briefed at the new head");

    let delta = lane_brief(&reg, &lane2);
    assert!(
        delta.starts_with("DELTA on PR #1758"),
        "a lane that has ANSWERED must get the delta template, not the first-call one — \
         `at_head` is what tells the two apart: {delta}"
    );
    assert!(
        delta.contains(&format!("at head {HEAD_A}")),
        "…and it must name the revision that lane previously answered at: {delta}"
    );
    assert!(delta.contains(HEAD_B), "…and the one it is being asked about now: {delta}");
}

/// **#2508: the brief states the round's scope on one machine-readable line** —
/// `scope: whole-diff` | `scope: delta since <sha>` | `scope: body-only` — and
/// the rev-std persona keys how wide the round measures off that line.
///
/// One drive per mode, each asserting its own scope line AND the absence of the
/// other two: a scope that silently widened is the exact defect the issue
/// measures — late blockers in round-1 code a delta-scoped round never saw.
/// The negative control is the first drive and it is structural, not asserted
/// into existence: round 1 has no previous revision to name, so a delta line
/// there means the derivation read a lane record that did not exist.
///
/// **Four blocks, not three** (#2508 review, W1): `body-only` has TWO paths in
/// `rd_lane_scope` — the `verify` early return and the unchanged-head arm — and
/// a one-lane gate can only ever reach the first (a sole answered lane makes
/// the grant true whenever the digest is readable). The third block pins the
/// grant path; the fourth pins the arm, on a two-lane gate whose second lane
/// has recorded nothing, and asserts the brief does NOT announce the
/// verification grant — the negative control that keeps the two paths distinct.
///
/// **The mode is pinned against the brief's own arms, not only against the
/// golden** (rev-std finding 2): each block asserts scope ⟺ template arm ⟺
/// WHAT_MOVED text, so an edit to one classification that leaves the other
/// behind goes red here rather than shipping a scope line that contradicts the
/// prose it rides on.
///
/// The `rd-lane-spawned` audit row carries the same string, asserted on every
/// block (#2508 review round 2) — the row is what a beta's before/after count
/// reads, and a row that disagreed with the brief would make that count measure
/// nothing.
#[test]
fn the_lane_brief_names_the_round_scope_on_every_round_mode() {
    // ── whole-diff: a first round, nothing has ever been briefed or answered ──
    {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, agent) = briefed(&reg, &repo, &gh);
        let brief = lane_brief(&reg, &agent);
        assert!(
            brief.contains("scope: whole-diff"),
            "a first-round brief must be scoped to the whole diff: {brief}"
        );
        assert!(
            !brief.contains("scope: delta"),
            "round 1 never says delta — there is no previous revision to name: {brief}"
        );
        assert!(
            !brief.contains("scope: body-only"),
            "a first-round brief is not a verification round: {brief}"
        );
        // Cross-pin (rev-std finding 2): whole-diff ⟺ the first-call template —
        // the scope's `None` branch and the brief's template-arm `None` branch
        // read the same lane record, and each block here pins the two reads
        // against each other.
        assert!(
            brief.starts_with("Review PR #1758"),
            "a whole-diff round renders the first-call template: {brief}"
        );
        assert!(
            !brief.contains("head moved from") && !brief.contains("Re-read the body"),
            "a first-round brief renders no WHAT_MOVED arm at all: {brief}"
        );
        let row = lane_spawn_rows(&reg, &group)
            .last()
            .cloned()
            .expect("the first round spawned a lane, which emitted rd-lane-spawned");
        assert_eq!(
            row["scope"], json!("scope: whole-diff"),
            "rd-lane-spawned must carry the scope the brief rendered: {row}"
        );
    }
    // ── delta since: the lane answered at HEAD_A, the head moved to HEAD_B ──
    {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, _s) = driven(&reg, &repo, &gh);
        let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
        with_pane(&reg, &orch.id, 7001);
        reg.set_pr_head_override(Some(HEAD_A.to_string()));
        reg.rd_drive_group_with(&group, &gh, 10_000);
        let first = reg.rd_drive_group_with(&group, &gh, 20_000);
        let (_pr, _b, lane) = first.lanes_opened.first().cloned().expect("lane 0 opens");
        // The lane answers at HEAD_A through the real recording path (the same
        // fixture `a_lane_that_has_answered_is_re_briefed_with_the_delta_template`
        // uses), then the head moves and the drive re-briefs it.
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
                "pr": "1758", "verdict": "pass", "summary": "pass - nothing blocking" } }),
        )
        .expect("the lane records");
        gh.set_facts("OPEN", HEAD_B);
        reg.set_pr_head_override(Some(HEAD_B.to_string()));
        reg.rd_drive_group_with(&group, &gh, 30_000); // review-wait -> ci-wait
        reg.rd_drive_group_with(&group, &gh, 40_000); // ci-wait -> review-wait
        let again = reg.rd_drive_group_with(&group, &gh, 50_000); // re-brief
        let (_pr, _b, lane2) = again
            .lanes_opened
            .first()
            .cloned()
            .expect("the stale lane must be re-briefed at the new head");
        let delta = lane_brief(&reg, &lane2);
        assert!(
            delta.contains(&format!("scope: delta since {HEAD_A}")),
            "a re-brief at a moved head must name the revision it deltas from: {delta}"
        );
        assert!(
            !delta.contains("scope: whole-diff"),
            "a delta brief must not claim the whole diff: {delta}"
        );
        assert!(
            !delta.contains("scope: body-only"),
            "the head moved, so this is not a body-only round: {delta}"
        );
        // Cross-pin (rev-std finding 2): delta ⟺ the moved-head WHAT_MOVED arm,
        // both naming the same previous revision.
        assert!(
            delta.starts_with("DELTA on PR #1758"),
            "a delta round renders the delta template: {delta}"
        );
        assert!(
            delta.contains(&format!("head moved from {HEAD_A} to {HEAD_B}")),
            "the delta scope line and the moved-head prose must agree on the revision it \
             deltas from: {delta}"
        );
        assert!(
            !delta.contains("VERIFICATION-ONLY"),
            "verify was not granted on a moved head — the brief must not announce the grant: \
             {delta}"
        );
        let row = lane_spawn_rows(&reg, &group)
            .last()
            .cloned()
            .expect("the re-brief spawned a lane, which emitted rd-lane-spawned");
        assert_eq!(
            row["scope"], json!(format!("scope: delta since {HEAD_A}")),
            "rd-lane-spawned must carry the scope the brief rendered: {row}"
        );
    }
    // ── body-only: the verification round (#2308 E2) — head unchanged, body moved ──
    {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        // **The gate must DECLARE `body-unchanged`** (#2168 E2): on the stock
        // roster a body edit leaves the gate satisfied, the drive reaches
        // `gate-check` and terminates, and no verification round ever opens —
        // the exact premise `a_verification_brief_announces_itself_on_every_
        // path_that_grants_it` documents for the same reason.
        let repo = Repo::with(WORKFLOW_BODY_UNCHANGED);
        const REVIEWED: &str = "the body every lane passed";
        const EDITED: &str = "the body somebody edited afterwards";
        let gh = FakeGh::green(HEAD_A);
        // **The body override is set BEFORE the pass is recorded** — the same
        // premise case C of
        // `a_verification_brief_announces_itself_on_every_path_that_grants_it`
        // builds on. `review_verdict` reads the body it passes against through
        // this seam, so a pass recorded without it carries no digest at all;
        // `body_changed` reads a missing digest as "cannot tell", never as
        // drift, and a pass that never went stale is never re-briefed — the
        // drive would sit at `gate-check` refusing a pass it cannot compare.
        gh.set_body(REVIEWED);
        reg.set_pr_head_override(Some(HEAD_A.to_string()));
        reg.set_pr_body_override(Some(REVIEWED.to_string()));
        let (group, _s) = driven(&reg, &repo, &gh);
        let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
        with_pane(&reg, &orch.id, 7001);
        reg.rd_drive_group_with(&group, &gh, 10_000);
        let first = reg.rd_drive_group_with(&group, &gh, 20_000);
        let (_pr, _b, lane) = first.lanes_opened.first().cloned().expect("lane 0 opens");
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
                "pr": "1758", "verdict": "pass", "summary": "pass - nothing blocking" } }),
        )
        .expect("the lane records");
        // End the lane's turn the way a reviewer does, so the re-brief is not
        // refused as a duplicate (#2109/#2162) — the premise
        // `a_verification_brief_announces_itself_on_every_path_that_grants_it`
        // documents for the same fixture.
        dispatch(
            &reg,
            &Caller {
                agent_id: lane.clone(),
                group: group.clone(),
                role: Role::Reviewer,
                role_hint: None,
            },
            "tools/call",
            &json!({ "name": "report", "arguments": {
                "outcome": "approved", "note": "nothing blocking", "ref": "#1758" } }),
        )
        .expect("the lane reports, which ends its turn");
        assert!(
            reg.agent(&lane).expect("the lane is on the roster").idle_since_ms.is_some(),
            "the fixture's premise — that pane has finished its turn, so a re-brief is not \
             refused as a duplicate"
        );
        // A body-only edit at the unchanged head: every lane has passed the
        // code, so `decide_review_wait` grants `verify` — the body-only round.
        gh.set_body(EDITED);
        reg.set_pr_body_override(Some(EDITED.to_string()));
        let opened = tick_until_lane(&reg, &gh, &group, 30_000);
        let agent = opened.unwrap_or_else(|| panic!("the verification round must re-brief"));
        let text = lane_brief(&reg, &agent);
        assert!(
            text.contains("scope: body-only"),
            "a verification round must be scoped to the body: {text}"
        );
        assert!(
            !text.contains("scope: whole-diff"),
            "a verification round must not re-measure the whole diff: {text}"
        );
        assert!(
            !text.contains("scope: delta since"),
            "the head did not move, so there is no revision to delta from: {text}"
        );
        // Cross-pins (rev-std finding 2): the mode the scope line names and the
        // arm the brief's own prose took must be ONE decision. This block
        // reached `body-only` through the `verify` early return, so the brief
        // must carry the verification paragraph — the same grant, said twice.
        assert!(
            text.starts_with("DELTA on PR #1758"),
            "a verification round over an answered lane renders the delta template: {text}"
        );
        assert!(
            text.contains("VERIFICATION-ONLY"),
            "the verification grant and the scope line must agree — one grant, said twice: \
             {text}"
        );
        assert!(
            !text.contains("head moved from"),
            "the head did not move, so the delta-text arm must not have rendered: {text}"
        );
        // The audit row on the verify-granted path (#2508 review round 2): the
        // re-brief emits `rd-lane-spawned`, and the row's `scope` must equal the
        // brief's line here too — this block reached `body-only` through the
        // `verify` early return, which is the one path the other three blocks
        // cannot witness.
        let row = lane_spawn_rows(&reg, &group)
            .last()
            .cloned()
            .expect("the verification round spawned a lane, which emitted rd-lane-spawned");
        assert_eq!(
            row["scope"], json!("scope: body-only"),
            "rd-lane-spawned must carry the scope the brief rendered: {row}"
        );
    }
    // ── body-only, the OTHER path: answered at this head, verify not granted ──
    // (#2508 review, W1 — the unchanged-head arm of `rd_lane_scope`, previously
    // pinned by nothing: the block above reaches `body-only` through the
    // `verify` early return, and on a ONE-lane gate the arm is unreachable,
    // because a sole answered lane makes the grant true whenever the digest is
    // readable. `WORKFLOW_TWO_LANE`'s second lane has recorded nothing, so the
    // grant fails while `first_stale_lane` still stales the lane that answered
    // — the only fixture that reaches the arm.)
    {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::with(WORKFLOW_TWO_LANE);
        const REVIEWED: &str = "the body lane zero passed";
        const EDITED: &str = "the body somebody edited afterwards";
        let gh = FakeGh::green(HEAD_A);
        // The body override is set before the pass is recorded, for the same
        // reason the block above documents: a pass with no digest never goes
        // stale, and this block needs lane 0's pass to go stale on the edit.
        gh.set_body(REVIEWED);
        reg.set_pr_head_override(Some(HEAD_A.to_string()));
        reg.set_pr_body_override(Some(REVIEWED.to_string()));
        let (group, _s) = driven(&reg, &repo, &gh);
        let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
        with_pane(&reg, &orch.id, 7001);
        reg.rd_drive_group_with(&group, &gh, 10_000);
        let first = reg.rd_drive_group_with(&group, &gh, 20_000);
        let (_pr, opened_block, lane) =
            first.lanes_opened.first().cloned().expect("lane 0 opens");
        assert_eq!(opened_block, "rev-std", "lane 0 is the gate's first lane");
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
                "pr": "1758", "verdict": "pass", "summary": "pass - nothing blocking" } }),
        )
        .expect("the lane records");
        dispatch(
            &reg,
            &Caller {
                agent_id: lane.clone(),
                group: group.clone(),
                role: Role::Reviewer,
                role_hint: None,
            },
            "tools/call",
            &json!({ "name": "report", "arguments": {
                "outcome": "approved", "note": "nothing blocking", "ref": "#1758" } }),
        )
        .expect("the lane reports, which ends its turn");
        assert!(
            reg.agent(&lane).expect("the lane is on the roster").idle_since_ms.is_some(),
            "the fixture's premise — that pane has finished its turn, so a re-brief is not \
             refused as a duplicate"
        );
        // The body-only edit at the UNCHANGED head, with lane 1 unanswered:
        // `first_stale_lane` stales lane 0 (its pass no longer settles — the
        // digest moved) and `decide_review_wait`'s grant fails (lane 1 has no
        // pass), so lane 0 is re-briefed with `verify: false` — the arm.
        gh.set_body(EDITED);
        reg.set_pr_body_override(Some(EDITED.to_string()));
        let opened = tick_until_lane(&reg, &gh, &group, 30_000);
        let agent = opened
            .unwrap_or_else(|| panic!("the unchanged-head non-verify re-brief must happen"));
        let text = lane_brief(&reg, &agent);
        assert!(
            text.contains("scope: body-only"),
            "a re-brief at the head the lane answered must be scoped to the body: {text}"
        );
        assert!(
            !text.contains("scope: whole-diff"),
            "the lane has measured this PR; the round is not a whole-diff one: {text}"
        );
        assert!(
            !text.contains("scope: delta since"),
            "the head did not move, so there is no revision to delta from: {text}"
        );
        // Cross-pins, and the arm's own negative control: `verify` was NOT
        // granted here, so the brief must NOT announce the verification grant —
        // the same scope value as the block above reached through the OTHER
        // branch, which is what makes the two branches distinct subjects.
        assert!(
            text.starts_with("DELTA on PR #1758"),
            "an answered lane re-briefed at an unchanged head renders the delta template: {text}"
        );
        assert!(
            !text.contains("VERIFICATION-ONLY"),
            "verify was not granted on this path — the brief must not announce the grant: {text}"
        );
        assert!(
            text.contains("Re-read the body, not the diff"),
            "the unchanged-head WHAT_MOVED arm must have rendered — the scope line and this \
             prose are one decision: {text}"
        );
        let row = lane_spawn_rows(&reg, &group)
            .last()
            .cloned()
            .expect("the re-brief spawned a lane, which emitted rd-lane-spawned");
        assert_eq!(
            row["scope"], json!("scope: body-only"),
            "rd-lane-spawned must carry the scope the brief rendered: {row}"
        );
    }
}

/// **A resume with a NEW session drops the old pane**, because that pane is an
/// interception key.
///
/// `worker_agent` is what `driven_role` matches an incoming `report` against. A
/// resume that re-pointed the drive at a different session while leaving the old
/// pane recorded would have the drive consume the traffic of a worker it no
/// longer owns — and the worker it *does* own report to the orchestrator as if
/// undriven. Both halves are wrong and neither is visible in a notice.
///
/// The control is the second half: resuming with the SAME session keeps the
/// pane, because that pane is still the right one. Without it this test would
/// pass under an implementation that cleared the field unconditionally, which
/// would break the ordinary resume — the common case.
#[test]
fn a_resume_with_a_new_session_forgets_the_pane_and_one_with_the_same_session_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, session) = driven(&reg, &repo, &gh);

    // Drive to a hand-back so a worker pane is actually recorded.
    reg.rd_drive_group_with(&group, &gh, 10_000);
    reg.rd_drive_group_with(&group, &gh, 20_000);
    gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
    gh.set_facts("OPEN", HEAD_B);
    reg.rd_drive_group_with(&group, &gh, 30_000);
    let handed = reg.rd_drive_group_with(&group, &gh, 40_000);
    let (_pr, worker) = handed.handbacks.first().cloned().expect("the drive hands back");
    assert_eq!(
        reg.rd_owner(&group, &worker).map(|(pr, _)| pr),
        Some(1758),
        "the recorded pane is the interception key, so it must own that agent to begin with"
    );

    // Park it, then resume pointing at a DIFFERENT session.
    let day = 24 * 60 * 60 * 1000;
    reg.rd_drive_group_with(&group, &gh, day);
    assert_eq!(status_state(&reg, &group), "held");
    let other = "dead1234-9999-8888-7777-666666666666";
    assert_ne!(other, session, "the two sessions must actually differ");
    let out = reg.drive_review_with(&group, &gh, 1758, other, false, 0, "orch-1", 0);
    assert_eq!(out["driving"], json!(true), "{out}");
    assert_eq!(
        reg.rd_owner(&group, &worker),
        None,
        "a drive re-pointed at another session still owned the OLD pane — it would consume \
         that worker's traffic while the worker it now owns reported as undriven"
    );

    // The control: the same session keeps its pane. Otherwise the assertion
    // above would hold under an implementation that always cleared it, which
    // breaks the ordinary resume.
    let dir2 = tempfile::tempdir().unwrap();
    let reg2 = relaunch_registry(dir2.path());
    let repo2 = Repo::new();
    let gh2 = FakeGh::green(HEAD_A);
    let (g2, s2) = driven(&reg2, &repo2, &gh2);
    reg2.rd_drive_group_with(&g2, &gh2, 10_000);
    reg2.rd_drive_group_with(&g2, &gh2, 20_000);
    gh2.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
    gh2.set_facts("OPEN", HEAD_B);
    reg2.rd_drive_group_with(&g2, &gh2, 30_000);
    let h2 = reg2.rd_drive_group_with(&g2, &gh2, 40_000);
    let (_p, w2) = h2.handbacks.first().cloned().expect("the drive hands back");
    reg2.rd_drive_group_with(&g2, &gh2, day);
    assert_eq!(status_state(&reg2, &g2), "held");
    reg2.drive_review_with(&g2, &gh2, 1758, &s2, false, 0, "orch-1", 0);
    assert_eq!(
        reg2.rd_owner(&g2, &w2).map(|(pr, _)| pr),
        Some(1758),
        "a resume with the SAME session must keep its pane — that pane is still the right one"
    );
}


/// The same roster with **two** reviewer lanes — the only fixture in which
/// `held(lane-stalled)` can be got wrong at all.
///
/// With one lane, "the stalled lane" and "the last lane with a verdict" are the
/// same lane and every selection rule passes. The defect this pins needs a lane
/// that has ANSWERED and a different lane that has not.
pub(crate) const WORKFLOW_TWO_LANES: &str = r#"version: 1
blocks:
  - id: worker
    kind: worker
  - id: rev-std
    name: Standard review
    kind: reviewer
  - id: rev-final
    name: Final validation
    kind: reviewer
gates:
  merge:
    require: all-pass
    reviewers: [rev-std, rev-final]
driver:
  enabled: true
"#;

/// **`held(lane-stalled)` names the lane that stalled**, which §2.2 says is that
/// notice's whole job — it names the pane a human or the orchestrator has to go
/// and look at.
///
/// The defect: the hold's facts fell back to "the last lane with a verdict" when
/// no lane had spoken. A stalled lane has by definition recorded nothing, so it
/// is absent from that list entirely, and the fallback named a **different,
/// passing** lane and *its* pane. The notice fired and read as healthy either
/// way, which is what made it worth a two-lane fixture.
///
/// The two assertions are a pair on purpose: naming the stalled lane is only
/// half of it, because a rule that named the FIRST lane unconditionally would
/// satisfy the first assertion here and be just as wrong. The second says the
/// passed lane must not be the subject.
#[test]
fn a_stalled_lane_hold_names_the_stalled_lane_and_not_the_one_that_passed() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::with(WORKFLOW_TWO_LANES);
    let gh = FakeGh::green(HEAD_A);
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7001);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    let out = reg.drive_review_with(
        &group,
        &gh,
        1758,
        "cafb930d-1111-2222-3333-444444444444",
        false,
        0,
        "orch-1",
        0,
    );
    assert_eq!(out["driving"], json!(true), "{out}");

    // Lane 0 opens and passes.
    reg.rd_drive_group_with(&group, &gh, 10_000);
    let first = reg.rd_drive_group_with(&group, &gh, 20_000);
    let (_pr, block0, lane0) = first.lanes_opened.first().cloned().expect("lane 0 opens");
    assert_eq!(block0, "rev-std", "gate order puts the static reviewers first");
    dispatch(
        &reg,
        &Caller {
            agent_id: lane0,
            group: group.clone(),
            role: Role::Reviewer,
            role_hint: None,
        },
        "tools/call",
        &json!({ "name": "review_verdict", "arguments": {
            "pr": "1758", "verdict": "pass", "summary": "pass - lane one is happy" } }),
    )
    .expect("lane 0 records");

    // Lane 1 opens and then says nothing at all.
    let second = reg.rd_drive_group_with(&group, &gh, 30_000);
    let (_pr, block1, lane1) = second
        .lanes_opened
        .first()
        .cloned()
        .expect("lane 1 opens once lane 0's pass stands");
    assert_eq!(block1, "rev-final", "…and the routed/declared order is the gate's");

    // Past the lane timeout, with the drive's own age bound still far away so
    // this is `lane-stalled` and not `drive-stalled`.
    let past_lane_timeout = 30_000 + 61 * 60 * 1000;
    let report = reg.rd_drive_group_with(&group, &gh, past_lane_timeout);
    assert_eq!(status_state(&reg, &group), "held");
    let notice = report
        .notices
        .iter()
        .find(|n| n.contains("HELD"))
        .expect("a hold delivers exactly one notice");

    // **Split at the pane clause, and assert on each half for what that half
    // claims.** #1871 B3 appended a disclosure that names EVERY pane the drive
    // owns, `rev-std`'s included, and it names it for a correct reason — so a
    // whole-notice `!contains("rev-std")` stopped being a discriminator the
    // moment that clause landed. Relaxing the assertion to fit would delete the
    // witness; scoping it to the SUBJECT clause keeps it exactly as strong,
    // because a rule that always named the first lane would still put `rev-std`
    // there. The second half then pins what the disclosure is actually for,
    // which is why this is a repin rather than a narrowing.
    let (subject, panes) = notice
        .split_once(" Panes still OWNED:")
        .expect("a held drive discloses the panes it still owns (#1871 B3)");
    assert!(
        subject.contains("lane rev-final"),
        "the hold must name the lane that STALLED: {subject}"
    );
    assert!(
        !subject.contains("rev-std"),
        "…and must not name the lane that PASSED — a rule that always named the first lane \
         would satisfy the assertion above and be just as wrong: {subject}"
    );
    assert!(
        panes.contains("(rev-std)") && panes.contains("(rev-final)"),
        "…while the disclosure names BOTH, because it answers a different question: which \
         panes are still running and still this drive's: {panes}"
    );
    assert!(
        notice.contains(&lane1),
        "…and §2.2 says this notice names the PANE, which is what a human goes and reads: \
         {notice}"
    );
}

// ── the live path: what a real delegate actually calls ──────────────────────

/// **B1's witness, and the reason it had none.** Every earlier test drove the
/// driver's own machinery; not one dispatched `report` from a driven delegate —
/// the call `reviewer.md` and `worker.md` both instruct. So a defect that lived
/// entirely in that arm was invisible to a green suite.
///
/// `rd_owner` computes which side of the drive the caller is, and the arm
/// discarded it. A driven REVIEWER's `report(approved)` — `approved` resolves to
/// the `done` status word — was ingested as `WorkerSignal::Done`, which is arc 8
/// out of `fix-wait`: a review round spent on a hand-back that never happened,
/// with no worker turn at all.
///
/// The three assertions are one property from three directions: a lane's report
/// is CONSUMED (so §7's narrowing still holds and nothing leaks to the
/// orchestrator) and carries NO worker signal (so the drive does not move), for
/// every outcome word a reviewer can send.
#[test]
fn a_driven_lanes_report_is_consumed_and_never_read_as_a_worker_signal() {
    for outcome in ["approved", "request_changes", "blocked"] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, _s) = driven(&reg, &repo, &gh);
        let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
        with_pane(&reg, &orch.id, 7001);

        // Open lane 0 and keep its pane — that agent is the driven LANE, and
        // capturing it from the tick that opened it is how every other test in
        // this file gets one.
        reg.rd_drive_group_with(&group, &gh, 10_000);
        let opened = reg.rd_drive_group_with(&group, &gh, 20_000);
        let (_pr, _block, lane_agent) =
            opened.lanes_opened.first().cloned().expect("lane 0 opens");

        // Then drive to a hand-back, so the entry is in `fix-wait` — the one
        // state a misread worker signal would move, and therefore the only state
        // in which this defect is observable at all.
        gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
        gh.set_facts("OPEN", HEAD_B);
        reg.rd_drive_group_with(&group, &gh, 30_000);
        let handed = reg.rd_drive_group_with(&group, &gh, 40_000);
        assert!(!handed.handbacks.is_empty(), "the drive must hand back for {outcome} to matter");
        assert_eq!(status_state(&reg, &group), "fix-wait");

        // The LANE reports — through `dispatch`, exactly as a real reviewer does.
        let caller = Caller {
            agent_id: lane_agent.clone(),
            group: group.clone(),
            role: Role::Reviewer,
            role_hint: None,
        };
        dispatch(
            &reg,
            &caller,
            "tools/call",
            &json!({ "name": "report", "arguments": {
                "outcome": outcome, "note": "the lane speaking", "ref": "#1758" } }),
        )
        .unwrap_or_else(|e| panic!("a driven lane must still be able to report ({outcome}): {e:?}"));

        // §7 still holds: consumed, not delivered.
        assert!(
            !delivered_texts(&reg, &group).iter().any(|t| t.contains("reports")),
            "a driven lane's report reached the orchestrator's pane ({outcome})"
        );
        assert!(
            reg.audit_log(&group).iter().any(|e| e.action == "rd-consumed"),
            "…and it must be on the record as consumed ({outcome})"
        );

        // And the drive has NOT moved: a lane's report is not a worker signal.
        reg.rd_drive_group_with(&group, &gh, 50_000);
        assert_eq!(
            status_state(&reg, &group),
            "fix-wait",
            "a driven LANE's report({outcome}) moved the drive out of fix-wait — that is arc 8, \
             which is a WORKER's report(done) with the head unchanged, and no worker spoke"
        );
    }
}

/// The other half, and the control for the test above: a driven WORKER's report
/// still is a worker signal. Without this, the assertions above would hold under
/// an implementation that ignored every report from anyone.
#[test]
fn a_driven_workers_report_is_still_a_worker_signal() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _s) = driven(&reg, &repo, &gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7001);

    reg.rd_drive_group_with(&group, &gh, 10_000);
    reg.rd_drive_group_with(&group, &gh, 20_000);
    gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
    gh.set_facts("OPEN", HEAD_B);
    reg.rd_drive_group_with(&group, &gh, 30_000);
    let handed = reg.rd_drive_group_with(&group, &gh, 40_000);
    let (_pr, worker) = handed.handbacks.first().cloned().expect("the drive hands back");
    assert_eq!(status_state(&reg, &group), "fix-wait");

    dispatch(
        &reg,
        &Caller {
            agent_id: worker,
            group: group.clone(),
            role: Role::Worker,
            role_hint: None,
        },
        "tools/call",
        &json!({ "name": "report", "arguments": {
            "outcome": "blocked", "note": "cannot proceed", "ref": "#1758" } }),
    )
    .expect("a driven worker reports");

    reg.rd_drive_group_with(&group, &gh, 50_000);
    assert_eq!(status_state(&reg, &group), "held", "a WORKER's blocked must park the drive");
    let s = reg.review_drive_status(&group);
    assert_eq!(
        s["drives"][0]["held_reason"],
        json!("worker-blocked"),
        "…and name the worker, which is the side that actually spoke: {s}"
    );
}

/// **B2's witness.** `held -> ci-wait` is arc 11, and §2.3 calls resuming a
/// parked drive the default — four shipped surfaces tell the orchestrator so.
///
/// `decide` checks the drive's AGE before any per-state logic, and `started_ms`
/// was never reset on the resume. So a drive parked longer than
/// `drive_timeout_minutes` re-held `drive-stalled` on its very first tick after
/// being resumed — and a hold a human takes their time over is exactly that old.
/// Arc 11 was a no-op for precisely the holds it exists to recover.
///
/// The lane clock is the same shape at a quarter the threshold, so both are
/// asserted: the drive is resumed at a `now` past BOTH timeouts and must reach a
/// working state and stay there.
#[test]
fn a_resume_recovers_a_drive_older_than_its_own_timeouts() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, session) = driven(&reg, &repo, &gh);

    // Park it on the drive's own age bound — the hold this test is about.
    let past_drive_timeout = 721 * 60 * 1000;
    reg.rd_drive_group_with(&group, &gh, past_drive_timeout);
    assert_eq!(status_state(&reg, &group), "held", "the drive must actually be parked");
    assert_eq!(
        reg.review_drive_status(&group)["drives"][0]["held_reason"],
        json!("drive-stalled")
    );

    // The remedy every one of those surfaces names.
    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", past_drive_timeout);
    assert_eq!(out["driving"], json!(true), "{out}");
    assert_eq!(status_state(&reg, &group), "ci-wait", "arc 11 puts it back to work");

    // …and it STAYS at work. This is the assertion the defect moves: before the
    // fix the very next tick re-held, because the age was still measured from an
    // entry created four hours ago.
    let after_resume = past_drive_timeout + 60_000;
    reg.rd_drive_group_with(&group, &gh, after_resume);
    assert_ne!(
        status_state(&reg, &group),
        "held",
        "the drive re-held on the first tick after a resume — arc 11 is a no-op for exactly the \
         holds it exists to recover, and four shipped surfaces promise otherwise"
    );
    assert_eq!(status_head(&reg, &group), HEAD_A, "and it really ticked");

    // The lane clock too: a resumed drive must not immediately `lane-stalled` on
    // a lane the orchestrator has just looked at and chosen to resume.
    reg.rd_drive_group_with(&group, &gh, after_resume + 60_000);
    assert_ne!(
        status_state(&reg, &group),
        "held",
        "a resumed drive re-held on the lane clock, which is the same defect at 60 minutes"
    );
}

/// **`held(lane-stalled)` must be recoverable by the thing its own notice tells
/// you to do**, which is the half of B2 that `drive-stalled` got and this did
/// not.
///
/// The notice says "read that pane, then drive_review to resume". Arc 11 does
/// put the drive back to `ci-wait` — but `decide_review_wait` re-opens a lane
/// only when `lane_open_for` is false, and at a stable head it stays true, so
/// the lane that stalled was never spoken to again. Re-arming `spawned_ms` made
/// that *quieter* rather than better: before it the drive re-held on the first
/// tick, after it the drive sat silent for a full `lane_timeout_minutes` and
/// re-held then.
///
/// So the assertion is deliberately **not** "it did not re-hold" — that passes
/// on a drive doing nothing for an hour, which is the bug. It is that the
/// stalled lane is briefed **again**, in the pane it already had.
#[test]
fn a_resume_re_briefs_the_lane_that_stalled_rather_than_waiting_on_it_again() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::with(WORKFLOW_TWO_LANES);
    let gh = FakeGh::green(HEAD_A);
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7101);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    let session = "cafb930d-1111-2222-3333-444444444444";
    let out = reg.drive_review_with(&group, &gh, 1758, session, false, 0, "orch-1", 0);
    assert_eq!(out["driving"], json!(true), "{out}");

    reg.rd_drive_group_with(&group, &gh, 10_000);
    let first = reg.rd_drive_group_with(&group, &gh, 20_000);
    let (_pr, _b0, lane0) = first.lanes_opened.first().cloned().expect("lane 0 opens");
    dispatch(
        &reg,
        &Caller {
            agent_id: lane0,
            group: group.clone(),
            role: Role::Reviewer,
            role_hint: None,
        },
        "tools/call",
        &json!({ "name": "review_verdict", "arguments": {
            "pr": "1758", "verdict": "pass", "summary": "pass - lane one is happy" } }),
    )
    .expect("lane 0 records");

    let second = reg.rd_drive_group_with(&group, &gh, 30_000);
    let (_pr, block1, _lane1) =
        second.lanes_opened.first().cloned().expect("lane 1 opens after lane 0's pass");
    assert_eq!(block1, "rev-final");

    // Lane 1 then says nothing at all, past its timeout — with the drive's own
    // age bound still far away, so this parks on `lane-stalled` and not on
    // `drive-stalled`.
    let stalled_at = 30_000 + 61 * 60 * 1000;
    reg.rd_drive_group_with(&group, &gh, stalled_at);
    assert_eq!(
        reg.review_drive_status(&group)["drives"][0]["held_reason"],
        json!("lane-stalled"),
        "the fixture must park on the hold this test is about, not on another one"
    );

    // The remedy the notice prints, at the clock the hold happened on.
    let out = reg.drive_review_with(&group, &gh, 1758, session, false, 0, "orch-1", stalled_at);
    assert_eq!(out["driving"], json!(true), "{out}");

    // The assertion the defect moves.
    //
    // **Two ticks, because §2.4 allows at most one advance per tick.** The first
    // moves `ci-wait -> review-wait` and stops; the lane can only be opened by
    // the one after it. A single tick here asserts nothing about the re-open —
    // it is empty under every implementation, this one included, which is a
    // vacuous pin rather than a failing one.
    let after = stalled_at + 60_000;
    let first = reg.rd_drive_group_with(&group, &gh, after);
    assert_eq!(
        status_state(&reg, &group),
        "review-wait",
        "the tick after a resume spends its one advance getting back to review-wait"
    );
    let second = reg.rd_drive_group_with(&group, &gh, after + 60_000);
    let resumed = second;
    let reopened: Vec<String> = first
        .lanes_opened
        .iter()
        .chain(resumed.lanes_opened.iter())
        .map(|(_, b, _)| b.clone())
        .collect();
    assert!(
        reopened.iter().any(|b| b == "rev-final"),
        "a resumed lane-stalled drive must re-brief the lane that stalled — otherwise the resume \
         its own notice instructs buys a silent lane_timeout and then re-holds: {reopened:?}"
    );

    // …and the pane that now holds the lane really received a brief.
    //
    // **Not asserted: that it is the SAME pane.** `rd_open_lane` resumes the
    // session recorded for a lane and spawns a fresh reviewer when there is
    // none — and a spawn in this harness records no session id, so the fixture
    // takes the fallback and a fresh pane is the correct outcome here. Pinning
    // identity would pin the fixture rather than the rule. What matters either
    // way is what this does assert: the lane record now points at the pane that
    // was actually briefed, so §7's interception stays keyed on a live pane
    // rather than on the abandoned one.
    let (_pr, _b, agent_after) = resumed
        .lanes_opened
        .iter()
        .find(|(_, b, _)| b == "rev-final")
        .cloned()
        .expect("checked immediately above");
    let re_brief = lane_brief(&reg, &agent_after);
    assert!(
        re_brief.contains("1758") && re_brief.contains(HEAD_A),
        "the re-opened lane's pane must actually hold a brief for this PR at this head: \
         {re_brief}"
    );
    // **Deliberately NOT asserted here: that the lane whose pass still stands
    // was left alone.** That assertion was written and removed as vacuous —
    // `first_stale_lane` skips a standing pass before any lane record is read,
    // so `rev-std` is unreachable from the re-open under EVERY implementation,
    // this one and a broken one alike. Its operands cannot be made to collide
    // either: making `rev-std` a re-open candidate means staling its pass, and
    // a stale `rev-std` becomes the deciding lane, so `rev-final` is never
    // reached and the test stops being about the stall.
    //
    // The property it looked like it covered — that clearing EVERY lane's
    // briefed head is safe — is really an invariant of `first_stale_lane`, not
    // of this arc, and pinning it belongs with that function rather than here.
    // Tracked as a follow-up rather than asserted vacuously.
}

/// The control for the test above. The re-open is scoped to `lane-stalled`, so
/// a resume out of a **different** hold must not re-brief a lane that is
/// legitimately mid-review.
///
/// Without this, "re-open every lane on every resume" satisfies the assertion
/// above and is wrong: it would re-deliver a brief to a reviewer who is reading
/// the diff, on every resume of any hold.
#[test]
fn a_resume_out_of_a_different_hold_does_not_re_brief_a_working_lane() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, session) = driven(&reg, &repo, &gh);

    reg.rd_drive_group_with(&group, &gh, 10_000);
    let opened = reg.rd_drive_group_with(&group, &gh, 20_000);
    assert!(
        !opened.lanes_opened.is_empty(),
        "a lane must be open for this control to mean anything"
    );

    // Park on the drive's AGE, not the lane's: this lane is inside its own
    // timeout and has simply not answered yet.
    let past_drive_timeout = 721 * 60 * 1000;
    reg.rd_drive_group_with(&group, &gh, past_drive_timeout);
    assert_eq!(
        reg.review_drive_status(&group)["drives"][0]["held_reason"],
        json!("drive-stalled"),
        "this is only a control if the hold is a different one"
    );

    let out =
        reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", past_drive_timeout);
    assert_eq!(out["driving"], json!(true), "{out}");

    // The SAME two ticks the positive test spends, for the same §2.4 reason. A
    // single tick would find nothing opened whatever the code does, so this
    // control has to reach the tick that could open one before its emptiness
    // means anything.
    let a = reg.rd_drive_group_with(&group, &gh, past_drive_timeout + 60_000);
    assert_eq!(
        status_state(&reg, &group),
        "review-wait",
        "this control must reach the state where a lane COULD be re-opened"
    );
    let b = reg.rd_drive_group_with(&group, &gh, past_drive_timeout + 120_000);
    let opened_after: Vec<String> =
        a.lanes_opened.iter().chain(b.lanes_opened.iter()).map(|(_, x, _)| x.clone()).collect();
    assert!(
        opened_after.is_empty(),
        "a lane inside its own timeout was re-briefed because some OTHER hold on the same drive \
         was resumed: {opened_after:?}"
    );
}

/// **A lane brief states the CI this tick OBSERVED, and never an unconditional
/// green.**
///
/// Both lane templates asserted "this PR's checks are green" as a fact, and
/// `rd_lane_brief` never read `brief.ci` — though the driver had the
/// observation in hand and the fix path already reads it.
///
/// The reachable path is arc 8: `fix-wait -> review-wait` on a worker's
/// `report(done)` at an unchanged head, which by design does **not** consult
/// `facts.ci`. A drive that entered `fix-wait` on a red CI and whose worker
/// reports done without pushing — the "that failure was unrelated" turn — then
/// briefs its reviewers with a green the same tick had just read as red.
#[test]
fn a_lane_brief_reports_the_ci_it_saw_and_never_asserts_a_green_it_did_not() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _session) = driven(&reg, &repo, &gh);

    // CI is red from the first tick, so `ci-wait` hands back on arc 3 before any
    // lane has opened or recorded anything.
    gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
    let handed = reg.rd_drive_group_with(&group, &gh, 10_000);
    let (_pr, worker) =
        handed.handbacks.first().cloned().expect("a red CI hands the PR back to its worker");
    assert_eq!(status_state(&reg, &group), "fix-wait");

    // The worker reports done WITHOUT pushing: the head does not move, so arc 7
    // cannot fire and arc 8 is what answers.
    dispatch(
        &reg,
        &Caller {
            agent_id: worker,
            group: group.clone(),
            role: Role::Worker,
            role_hint: None,
        },
        "tools/call",
        &json!({ "name": "report", "arguments": {
            "status": "done", "summary": "that failure was unrelated" } }),
    )
    .expect("the driven worker reports");

    reg.rd_drive_group_with(&group, &gh, 20_000);
    assert_eq!(
        status_state(&reg, &group),
        "review-wait",
        "arc 8 must put the drive in review-wait at an unchanged head with CI still red"
    );

    let opened = reg.rd_drive_group_with(&group, &gh, 30_000);
    let (_pr, _b, lane) =
        opened.lanes_opened.first().cloned().expect("a lane opens once review-wait is reached");
    let brief = lane_brief(&reg, &lane);
    assert!(
        !brief.contains("checks are green"),
        "the brief told a reviewer the checks were green at a head this very tick read as RED: \
         {brief}"
    );
    assert!(
        brief.contains("RED"),
        "…and it must say what it actually saw rather than merely omitting the false claim, or a \
         template with the sentence deleted would pass this: {brief}"
    );
}

/// The CI observations a lane brief can be rendered under, named so a shape pin
/// can be run against each rather than against whichever one the fixture
/// happened to produce (#1863 D2).
///
/// **`Conflicting` left this list in #2311 and is not an omission.** `decide`
/// now reads mergeability above the per-state logic, so a conflicting PR takes
/// arc 3 out of `review-wait` instead of opening a lane there — the one route
/// that used to brief a reviewer about a conflict. `rd_lane_brief` keeps the
/// sentence (its match over a closed enum must stay exhaustive), and
/// `a_conflicting_pr_briefs_no_lane_at_all` is what pins the unreachability,
/// so the arm cannot come back to life unnoticed. Relocating the witness rather
/// than relaxing this list is `CLAUDE.md`'s rule about a specimen that has left
/// the class it witnessed.
#[derive(Clone, Copy, Debug)]
enum CiArm {
    Green,
    Red,
    Pending,
}

impl CiArm {
    /// **The population, named once** (#1863 D2, second half). The arms used to
    /// be an array literal written inline at the one call site, which is a
    /// population nothing states: trimming it back to `[CiArm::Green]` — the
    /// very shape D2 was raised about — leaves every assertion inside the loop
    /// true and the test green, because the loop simply runs once. A `for` body
    /// that passes is evidence about the arms it RAN over, never about the arms
    /// that exist.
    ///
    /// A fifth `CiObservation` is a compile error in two exhaustive matches
    /// ([`CiArm::sentence`] and `lane_brief_under`), so what this list can go
    /// wrong by is omission or padding, not by silently absorbing a new arm —
    /// and `every_arm_states_a_different_sentence` is what refuses the padding.
    const ALL: [CiArm; 3] = [CiArm::Green, CiArm::Red, CiArm::Pending];

    /// The sentence this arm must render, verbatim. It is the CONTENT pin that
    /// makes each fixture discriminating: a `Conflicting` fixture that quietly
    /// produced the `Pending` sentence would satisfy every shape assertion, and
    /// this is what refuses it.
    fn sentence(self) -> &'static str {
        match self {
            CiArm::Green => "This PR's checks are green at that head.",
            CiArm::Red => "This PR's checks are RED at that head.",
            CiArm::Pending => {
                "This PR's checks are not green at that head (orrerix could not read a settled result)."
            }
        }
    }
}

/// Drive to an opened lane whose brief was rendered under `arm`, and return that
/// brief.
///
/// **`Green` is the only arm with a direct route**, because `ci-wait` leaves for
/// `review-wait` on green and on nothing else. The other two reach a lane
/// through **arc 8**: a red CI hands the PR back, the worker reports `done`
/// WITHOUT pushing, and `fix-wait -> review-wait` is taken without consulting
/// `facts.ci` at all — the "that failure was unrelated" turn. That is the one
/// route on which a lane is briefed at a head whose CI is not green, which is
/// the entire reason `rd_lane_brief` reads `brief.ci`, and
/// `a_lane_brief_reports_the_ci_it_saw_and_never_asserts_a_green_it_did_not` is
/// the test written against it. The arm under test is then set on the tick that
/// OPENS the lane, so what the brief renders is what that tick observed.
fn lane_brief_under(arm: CiArm) -> String {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _session) = driven(&reg, &repo, &gh);

    if matches!(arm, CiArm::Green) {
        reg.rd_drive_group_with(&group, &gh, 10_000);
        let opened = reg.rd_drive_group_with(&group, &gh, 20_000);
        let (_pr, _b, lane) =
            opened.lanes_opened.first().cloned().expect("a lane opens on a green drive");
        return lane_brief(&reg, &lane);
    }

    gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
    let handed = reg.rd_drive_group_with(&group, &gh, 10_000);
    let (_pr, worker) =
        handed.handbacks.first().cloned().expect("a red CI hands the PR back to its worker");
    assert_eq!(status_state(&reg, &group), "fix-wait", "{arm:?}: the hand-back must have happened");

    dispatch(
        &reg,
        &Caller {
            agent_id: worker,
            group: group.clone(),
            role: Role::Worker,
            role_hint: None,
        },
        "tools/call",
        &json!({ "name": "report", "arguments": {
            "status": "done", "summary": "that failure was unrelated" } }),
    )
    .expect("the driven worker reports");
    reg.rd_drive_group_with(&group, &gh, 20_000);
    assert_eq!(
        status_state(&reg, &group),
        "review-wait",
        "{arm:?}: arc 8 must reach review-wait at an unchanged head"
    );

    match arm {
        // Returned above; the arm is listed rather than absorbed by a `_` so a
        // fifth `CiObservation` is a compile error here.
        CiArm::Green => {}
        // The red payload set for the hand-back is already what this arm wants.
        CiArm::Red => {}
        CiArm::Pending => gh.set_checks(r#"[{"name":"build","state":"IN_PROGRESS","link":"x"}]"#),
    }
    let opened = reg.rd_drive_group_with(&group, &gh, 30_000);
    let (_pr, _b, lane) = opened
        .lanes_opened
        .first()
        .cloned()
        .unwrap_or_else(|| panic!("{arm:?}: a lane must open once review-wait is reached"));
    lane_brief(&reg, &lane)
}

/// **A brief's sentences are each one paragraph**, pinned as a SHAPE beside the
/// content the two tests around this one assert — **once per CI arm**.
///
/// This is `manager_lifecycle.rs`'s `is_one_paragraph` idiom, and it is here
/// because the CI literals shipped exactly the failure it exists to catch: a
/// `\n` plus seventeen spaces of source indent, delivered into a reviewer's
/// pane. The suite was green over it, because both content assertions are
/// `.contains` of one fragment's interior and no asserted substring straddles
/// the break — which is the whole reason a shape pin has to sit beside a content
/// pin rather than being implied by it.
///
/// **It ran on one arm, and it was the wrong one** (#1863 D2). The fixture was
/// `FakeGh::green(HEAD_A)`, so `brief.ci` was `Green` — the one short literal
/// that never had the defect. The `\n` plus seventeen spaces lived in the `Red`,
/// `Conflicting` and `Pending`/`Unknown` arms exclusively, so a regression on
/// the arms that carry the risk would have shipped under a green test whose own
/// doc said it existed to catch it. That is #1344's rule pointed at a test
/// rather than at a guard: a green is evidence about the POPULATION it ran
/// over, never about the property.
///
/// **The population is three since #2311**, not because the risk went away but
/// because the `Conflicting` arm is no longer reachable: a conflicting PR takes
/// arc 3 out of `review-wait` and never opens a lane. Its sentence still exists
/// in `rd_lane_brief` and is pinned unreachable by
/// `a_conflicting_pr_briefs_no_lane_at_all`, so nothing about the defect class
/// is un-witnessed — what moved is which test witnesses it.
///
/// Each arm also asserts its own sentence verbatim, which is what stops the
/// widened population from being three runs of one fixture: a route that
/// silently produced the `Green` sentence under `CiArm::Pending` passes every
/// shape assertion and fails the content one.
///
/// Both shape halves are checked. A hard break is the obvious form; a run of ten
/// spaces is the one a collapsed `\` continuation leaves behind, with no newline
/// at all to notice.
#[test]
fn a_lane_brief_is_one_paragraph_per_sentence() {
    let mut arms_checked = 0usize;
    for arm in CiArm::ALL {
        let brief = lane_brief_under(arm);

        // The template itself is deliberately multi-paragraph; what must not
        // carry a break is any single interpolated sentence. So this reads the
        // lines rather than the whole, and asserts none of them leaks source
        // indentation.
        let mut checked = 0usize;
        for line in brief.lines() {
            checked += 1;
            assert!(
                !line.contains("          "),
                "{arm:?}: a brief line leaks source indentation, which is what a collapsed \
                 `\\` continuation leaves behind: {line:?}"
            );
        }
        assert!(
            checked > 3,
            "{arm:?}: the per-arm positive control — this must have read real lines"
        );

        // And the CI sentence specifically, which is the one that shipped
        // broken. Found by the prefix every arm shares, so the finder itself
        // does not decide which arm it is looking at.
        let ci_line = brief
            .lines()
            .find(|l| l.trim_start().starts_with("This PR"))
            .unwrap_or_else(|| {
                panic!("{arm:?}: every lane brief states the CI it observed: {brief}")
            });
        assert!(
            ci_line.contains(arm.sentence()),
            "{arm:?}: the brief must render THIS arm's sentence — a fixture that reached a \
             different arm would satisfy every shape assertion below: {ci_line:?}"
        );
        assert!(
            !ci_line.contains("          ") && ci_line.trim_end().ends_with('.'),
            "{arm:?}: the CI sentence must be one whole paragraph on one line: {ci_line:?}"
        );
        // **Counted at the VERIFIED site, not the match site.** Incremented
        // after this arm's assertions have all run, so the population control
        // below certifies coverage that was actually delivered rather than
        // arms the loop merely started (CLAUDE.md's `test/theme.test.ts` rule).
        arms_checked += 1;
    }
    // **The population control, and it is what #1863 D2 was really about.** The
    // per-arm floor above counts LINES; nothing counted ARMS, so the fixture
    // this test exists to have widened — one arm, and the wrong one — was still
    // reachable by deleting entries from the loop's array. The line floor holds
    // under it, every content pin holds under it, and the test stays green while
    // covering exactly the arm that never carried the defect.
    assert_eq!(
        arms_checked,
        CiArm::ALL.len(),
        "every CI arm must have been rendered AND checked, not merely enumerated"
    );
    assert_eq!(arms_checked, 3, "…and the population is the three still reachable (#2311)");
}

/// The guard on [`CiArm::ALL`] itself: it can go wrong by omission, and padding
/// a short list with a repeat would hide that from the count above.
///
/// Distinctness is the checkable form — each arm exists precisely because it
/// renders a different sentence, so two entries answering the same one means the
/// list is naming three arms while claiming four.
#[test]
fn every_arm_states_a_different_sentence() {
    let mut seen: Vec<&str> = CiArm::ALL.iter().map(|a| a.sentence()).collect();
    let listed = seen.len();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen.len(),
        listed,
        "CiArm::ALL names {listed} arms and only {} distinct sentences — a repeat pads the \
         population control in `a_lane_brief_is_one_paragraph_per_sentence` back to green \
         while an arm goes unrendered",
        seen.len()
    );
}

/// The control for the test above: on a genuinely green drive the brief still
/// says so. Without it, "never mention CI at all" satisfies the red assertion.
#[test]
fn a_lane_brief_on_a_green_drive_still_says_the_checks_are_green() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _session) = driven(&reg, &repo, &gh);
    reg.rd_drive_group_with(&group, &gh, 10_000);
    let opened = reg.rd_drive_group_with(&group, &gh, 20_000);
    let (_pr, _b, lane) = opened.lanes_opened.first().cloned().expect("a lane opens on green");
    let brief = lane_brief(&reg, &lane);
    assert!(brief.contains("checks are green"), "{brief}");
}

/// **A drive record orrerix cannot read refuses the enqueue rather than reading
/// as "not driven".**
///
/// `load_state(..).map(is_driven).unwrap_or(false)` answered a question it had
/// not been able to ask, and in the one direction that is unsafe: the queue
/// would enqueue a PR that may be under a live drive, which is precisely the
/// overlap §8.1 forbids. Every other unreadable-state site in this codebase
/// refuses — `queue-state-unreadable` sits a few lines above this one.
#[test]
fn a_torn_drive_record_refuses_the_enqueue_instead_of_reading_as_undriven() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _session) = driven(&reg, &repo, &gh);

    // A control first: while the record IS readable, this PR is refused for
    // being driven — so the refusal below is about the record being torn and not
    // about `queue_merge` refusing everything in a repo with no real remote.
    let before = reg.queue_merge(&group, 1758, None);
    assert_eq!(before["refused"], json!("in-review-drive"), "{before}");

    assert!(reg.corrupt_drive_record_for_test(&group), "the record must exist to be torn");

    let after = reg.queue_merge(&group, 1758, None);
    assert_eq!(
        after["refused"],
        json!("rd-state-unreadable"),
        "a drive record orrerix cannot read is a FAULT, not evidence that the PR is undriven: \
         {after}"
    );
}
