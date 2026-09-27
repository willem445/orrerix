//! One clock per digest move (#2194), time-in-state bounds (#2110), and the cap-starvation stamp (#2135).
//!
//! One module of the `reviewdrive` integration-test target (`main.rs`),
//! split out of the former single-file `tests/reviewdrive.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ── #2194: one event — a digest move at an unchanged head — one clock ───────

/// **#2194.** A BODY-ONLY fix re-briefs a lane through two different arms: a
/// LIVE pane is re-briefed where it sits (§8's body-changed row), a DEAD pane's
/// lane is re-opened on its own session (#2163). One event, two paths — and
/// both must write the SAME `lane-stalled` anchor: the re-brief time. If the
/// re-open path inherited the anchor the ORIGINAL brief set, the same body-only
/// fix would hand a reviewer that has read nothing a truncated stall window
/// while the identical fix under a live pane got the full one — two clocks for
/// one event, decided by whether a pane happened to die.
///
/// The pure rule is pinned in the engine crate
/// (`a_dead_panes_replacement_inherits_the_stall_anchor_and_a_new_round_does_not`),
/// and it is BLIND to the wiring this test exists for: `lane_stall_anchor`
/// re-arms on a moved digest only because `rd_open_lane` threads the LIVE
/// digest into it. A call site that passed `None` — "we could not check", which
/// `lane_open_for` reads as still-open — or the lane's own RECORDED digest
/// would silently reinstate the inheritance, and every engine-unit row would
/// stay green, because those rows call the function directly. Reading the
/// anchor off the persisted lane record through the real tick is the only
/// instrument that sees that seam.
///
/// **The live arm's pane has ENDED ITS TURN, and that is the fixture's
/// premise, not decoration.** A reviewer still writing the review it was
/// briefed for is refused, not re-briefed (#2109's duplicate guard) — that is
/// a different, separately bounded path. §8's body-changed re-brief is
/// delivered into a pane that is idle and ready, which is what a `report`
/// stamps (`idle_since_ms`), plus a pty so the reuse arm has somewhere to
/// type it.
#[test]
fn a_moved_digest_re_arms_the_stall_clock_on_both_the_dead_pane_and_live_pane_paths() {
    type Row = (&'static str, u64);
    let mut observed: Vec<Row> = Vec::new();

    for arm in ["live", "dead"] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, _orch, lane) = lane_round_one(&reg, &repo, &gh);
        if arm == "live" {
            // End the reviewer's turn so the pane is idle and ready — the
            // state §8's body-changed re-brief is delivered into. No verdict
            // file is written: the lane must still be OUTSTANDING for the
            // digest move to re-brief it. Three fixture enablers ride along:
            // a pty, a CONFIRMED last delivery on it (readiness, #2089 — a
            // pty with no delivery record reads `no-record`, which the reuse
            // arm declines), and a paused group — a headless registry has no
            // AppHandle, so an unpaused delivery landing alone at the front
            // of an idle queue withdraws itself ("no app handle"); the pause
            // branch admits without one, which is `make_delivery_land`'s own
            // rationale.
            make_delivery_land(&reg, &group, &lane, 7401);
            make_pane_ready(&reg, 7401, true);
            report_as(&reg, &group, &lane, Role::Reviewer, "approved");
        }

        // The premise, read off the record rather than assumed: round one's
        // brief anchored the clock at the tick that sent it, and recorded the
        // digest of the body as it stood THEN — so the rows below measure a
        // re-arm against a real prior anchor at a real prior revision.
        let before = live_lanes(&reg, &group);
        assert_eq!(before.len(), 1, "{arm}: one lane on the record");
        assert_eq!(
            before[0]["spawned_ms"],
            json!(20_000),
            "{arm}: round one anchored the clock at its own tick"
        );
        assert_eq!(
            before[0]["briefed_digest"],
            json!(body_digest("b")),
            "{arm}: round one briefed at the body as it stood then"
        );

        // The body-only fix: the digest moves, the head does not. The ONE axis
        // both arms share; the only difference between them is the pane.
        gh.set_body("b2");
        if arm == "dead" {
            assert!(
                reg.mark_agent_dead_for_test(&lane),
                "{arm}: the fixture's own premise"
            );
        }

        let again = reg.rd_drive_group_with(&group, &gh, 40_000);
        assert_eq!(
            again.lanes_opened.len(),
            1,
            "{arm}: a moved digest re-briefs the lane whether the pane is live or dead"
        );
        let lanes = live_lanes(&reg, &group);
        let rec = lanes
            .iter()
            .find(|l| l["block"] == json!("rev-std"))
            .expect("the re-briefed lane is on the record");
        if arm == "live" {
            assert_eq!(
                again.lanes_opened[0].2,
                lane,
                "{arm}: the re-brief went INTO the live pane (#1960's reuse), not a second one. \
                 Declined rows: {:?}; readiness: {:?}; lane agent: {:?}",
                rows_for(&reg, &group, "rd-reuse-declined"),
                reg.pane_readiness(7401),
                reg.agent(&lane).map(|a| (
                    a.idle_since_ms,
                    a.pty_id,
                    a.session_id.clone(),
                    a.block.clone(),
                    a.status
                ))
            );
        } else {
            assert_ne!(
                again.lanes_opened[0].2, lane,
                "{arm}: the re-open cannot reuse a dead pane — the replacement is its own"
            );
        }
        assert_eq!(
            rec["briefed_head"],
            json!(HEAD_A),
            "{arm}: the head did not move, so this is one round, not a new one"
        );
        assert_eq!(
            rec["briefed_digest"],
            json!(body_digest("b2")),
            "{arm}: the re-brief binds to the body as it stands NOW"
        );
        observed.push((arm, rec["spawned_ms"].as_u64().unwrap()));
    }

    let expected: Vec<Row> = vec![("live", 40_000), ("dead", 40_000)];
    assert_eq!(
        observed, expected,
        "each row is (arm, the `lane-stalled` anchor the re-brief wrote). A `20_000` on the \
         dead arm is the replacement inheriting the clock the ORIGINAL brief set — a \
         reviewer that has read nothing gets only the minutes the dead pane left, while \
         the same body-only fix under a live pane re-arms to full. An anchor other than \
         the re-brief tick on either arm is two clocks for one event."
    );
}

/// The roster with a delegate cap of one, so the worker `drive_review` needs is
/// the whole of it and every lane spawn is refused.
pub(crate) fn rails_capped() -> Guardrails {
    Guardrails { max_agents: 1, ..rails() }
}

/// A capped group with the drive already in `review-wait` and its first lane
/// spawn refused — the fixture both #2109 ask-3 tests start from.
///
/// Answers the group, the orchestrator's pane and the clock of the tick that
/// took the refusal, so a caller can measure the window from where the
/// starvation actually began rather than from a number it remembered.
fn cap_starved(reg: &OrchRegistry, repo: &Repo, gh: &FakeGh) -> (GroupId, String, u64) {
    let group = reg.create_group(&repo.path(), rails_capped()).unwrap().id;
    let w = reg.spawn_agent(&group, Role::Worker, "w", "", false, None).expect("the one slot");
    let session = w.session_id.clone().expect("claude mints a session id at spawn");
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(reg, &orch.id, 7001);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    let out = reg.drive_review_with(&group, gh, 1758, &session, false, 0, "orch-1", 0);
    assert_eq!(out["driving"], json!(true), "drive_review refused: {out}");

    reg.rd_drive_group_with(&group, gh, 10_000); // ci-wait -> review-wait
    let first = reg.rd_drive_group_with(&group, gh, 20_000); // lane spawn, refused
    assert!(first.lanes_opened.is_empty(), "the control: the cap is full, so nothing spawned");
    (group, orch.id, 20_000)
}

/// **#2109 ask 3, the record.** A cap refusal says how long the cap has been
/// refusing *this drive*, not merely that it refused on this tick.
///
/// `cap: true` (#1960) already answered "was a slot the problem here", and that
/// was the whole of what the log carried while PR #2105's drive sat starved for
/// three hours: thirty-seven identical rows, each true, none of them saying the
/// drive had been stuck since the first one. `starved_ms` is the run, and it is
/// the number `held(cap-full)` is decided from — so a row with `cap: true` and
/// no run is a log that can report the condition but never its duration.
///
/// Split from the hold below rather than asserted before it, because a test
/// that fails here tells you nothing about whether the hold works: the first
/// assertion to move is the only one a red evidences.
#[test]
fn a_cap_refusal_records_how_long_the_cap_has_been_refusing_this_drive() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _orch, at) = cap_starved(&reg, &repo, &gh);

    let refused = rows_for(&reg, &group, "rd-refused");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0]["cap"], json!(true), "the cap is what refused it: {refused:?}");
    assert_eq!(
        refused[0]["starved_ms"],
        json!(0),
        "and the run is zero long, because this is its first tick: {refused:?}"
    );

    // A second refusal on a later tick is the SAME run, and the row says so by
    // the number growing. Without this the field could be a constant zero.
    reg.rd_drive_group_with(&group, &gh, at + 90_000);
    let again = rows_for(&reg, &group, "rd-refused");
    assert_eq!(again.len(), 2, "the cap refused again: {again:?}");
    assert_eq!(
        again[1]["starved_ms"],
        json!(90_000),
        "…measured from the FIRST refusal, so a reader sees the run and not the tick: {again:?}"
    );
}

/// **#2109 ask 3, the exit.** A drive the cap will not let spawn becomes one of
/// §2.2's exits instead of sitting invisible.
///
/// The measured incident: PR #2105's drive sat in `review-wait` with
/// `lanes: []` for about three hours — `since_ms` 11,083,045 at the read —
/// emitting one `rd-refused` row per tick and no notice at all, while released
/// lanes from a finished drive held the cap. Nothing was wrong with the drive,
/// the PR or the session; a slot was missing, and the only surface that said so
/// was a log a human read by hand.
///
/// **The driver still never kills a pane TO MAKE ROOM** (§3.1 item 5, as #2501
/// narrowed it), which is why this is a hold rather than a reap: what the driver
/// owes is the sentence naming who can free a slot, and the notice carries both
/// that and the drive's own panes. The narrowing is about panes the drive is
/// FINISHED with, and a starved drive by construction has none — every lane spawn
/// it wanted was refused — so nothing here is reachable by it.
///
/// The one-tick-short assertion is the discriminator. Holding on the FIRST
/// refusal would be the opposite defect — a capped lane usually clears itself
/// within a back-off — so an implementation that parks immediately fails here,
/// and one that never parks fails below.
#[test]
fn a_cap_that_starves_a_drive_parks_it_as_cap_full_rather_than_leaving_it_silent() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, orch, at) = cap_starved(&reg, &repo, &gh);

    // One tick short of the window: still trying, still no orchestrator turn.
    let short = reg.rd_drive_group_with(&group, &gh, at + reviewdrive::CAP_HOLD_MS - 1);
    assert_eq!(
        status_state(&reg, &group),
        "review-wait",
        "a cap refusal that has not lasted the window is a back-off, not a hold"
    );
    assert!(
        short.notices.iter().all(|n| !n.contains("HELD")),
        "and it costs no orchestrator turn: {:?}",
        short.notices
    );

    let held = reg.rd_drive_group_with(&group, &gh, at + reviewdrive::CAP_HOLD_MS);
    assert_eq!(status_state(&reg, &group), "held");
    let status = reg.review_drive_status(&group);
    assert_eq!(
        status["drives"][0]["held_reason"],
        json!("cap-full"),
        "the hold names the CAP, not the drive's age: {status}"
    );
    let notice = held
        .notices
        .iter()
        .find(|n| n.contains("HELD"))
        .expect("a hold delivers exactly one notice");
    assert!(
        notice.contains("live-delegate cap"),
        "the notice must say a SLOT is what is missing: {notice}"
    );
    assert!(
        notice.contains("kill_agent") && notice.contains("drive_review"),
        "and name what frees one and what resumes the drive: {notice}"
    );
    assert!(
        notice.contains("never kills a pane to make room"),
        "and say why the driver did not free the slot itself (3.1 item 5): {notice}"
    );
    // The retracted half of that claim, pinned so it cannot come back (#2501).
    // Before the narrowing this notice said the driver never kills a pane at
    // all, which is now false — and an assertion quoting a claim does not merely
    // carry it, it ENFORCES it, so the correction has to be repinned rather than
    // left to the substring that happens to still match.
    assert!(
        !notice.contains("never kills a pane: "),
        "the pre-#2501 blanket claim must not survive in a line an orchestrator \
         acts on: {notice}"
    );
    assert!(
        texts_to(&reg, &group, &orch).iter().any(|t| t.contains("HELD")),
        "and it must land in the ORCHESTRATOR pane, which is the whole point of the hold"
    );
}

/// The two-lane roster with a cap that admits the worker and **one** lane.
fn rails_capped_two() -> Guardrails {
    Guardrails { max_agents: 2, ..rails() }
}

/// The state review 4's W1 needs, which no fixture in this file could reach: a
/// live cap stamp **beside a live lane pane**, with the tick's per-refusal kind
/// no longer the cap's.
///
/// `rails_capped`'s cap of one cannot produce it — it refuses every lane, so the
/// stamp's owner is always also the selected lane, which is the one case the
/// defect does not show up in. This needs a cap that admits the worker and
/// exactly one lane.
///
/// 1. Lane 0 opens and records `pass` at `(head-a, d1)`; its pane stays live and
///    busy, because recording a verdict is not going idle.
/// 2. `first_stale_lane` moves to lane 1, whose spawn the cap refuses — both
///    slots are held by the worker and lane 0's pane. The entry is stamped.
/// 3. The PR body is edited. The digest moves, lane 0's `pass` no longer stands,
///    and `first_stale_lane` comes back to **lane 0** — whose re-brief the
///    duplicate refusal declines, because its pane is live at this head. That
///    refusal is not the cap's.
///
/// Answers the group, the orchestrator's pane, and the clock of the tick that
/// took the non-cap refusal.
///
/// **The digest has two sources here and both are set.** `review_verdict`
/// records `body_digest` of what `pr_body` answers, while the drive digests the
/// body `observe_pr` read out of `FakeGh`. Left unset the first fails, the
/// verdict records an EMPTY digest, and `body_changed` then answers `None` —
/// "we could not tell" — which `lane_verdict_is_current` reads as still current.
/// The body edit would then stale nothing and step 3 would silently re-select
/// lane 1, which is how this fixture first went green for the wrong reason.
fn two_lane_stamp_then_duplicate(
    reg: &OrchRegistry,
    repo: &Repo,
    gh: &FakeGh,
) -> (GroupId, String, u64) {
    let group = reg.create_group(&repo.path(), rails_capped_two()).unwrap().id;
    let w = reg.spawn_agent(&group, Role::Worker, "w", "", false, None).expect("slot 1 of 2");
    let session = w.session_id.clone().expect("claude mints a session id at spawn");
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(reg, &orch.id, 7001);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    reg.set_pr_body_override(Some("b".to_string()));
    let out = reg.drive_review_with(&group, gh, 1758, &session, false, 0, "orch-1", 0);
    assert_eq!(out["driving"], json!(true), "drive_review refused: {out}");

    // 1. Lane 0 opens — slot 2 of 2 — and answers.
    reg.rd_drive_group_with(&group, gh, 10_000);
    let first = reg.rd_drive_group_with(&group, gh, 20_000);
    let (_pr, block0, lane0) = first.lanes_opened.first().cloned().expect("lane 0 opens");
    assert_eq!(block0, "rev-std");
    dispatch(
        reg,
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

    // 2. Lane 1 is what the gate wants next, and the cap is full.
    let capped = reg.rd_drive_group_with(&group, gh, 30_000);
    assert!(capped.lanes_opened.is_empty(), "both slots are held, so lane 1 cannot open");
    let refused = rows_for(reg, &group, "rd-refused");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0]["block"], json!("rev-final"), "…and it is LANE 1 that was refused");
    assert_eq!(refused[0]["cap"], json!(true), "…by the cap, which is what stamps: {refused:?}");

    // 3. The body moves, so lane 0's pass no longer stands and the gate comes
    //    back to it — where the duplicate refusal, not the cap, is what answers.
    gh.set_body("b2");
    reg.set_pr_body_override(Some("b2".to_string()));
    let dup = reg.rd_drive_group_with(&group, gh, 40_000);
    assert!(dup.lanes_opened.is_empty(), "lane 0's pane holds the round");
    let dups = rows_for(reg, &group, "rd-lane-duplicate-refused");
    assert_eq!(dups.len(), 1, "the tick's refusal is the DUPLICATE one: {dups:?}");
    assert_eq!(dups[0]["block"], json!("rev-std"), "…and its subject is lane 0 now: {dups:?}");
    (group, orch.id, 40_000)
}

/// **#2109 review 4, W1 — the record.** A refusal that is not the cap's must
/// stop publishing a cap-run duration beside itself.
///
/// `cap: false` and a growing `starved_ms` on one row is a second surface saying
/// the run is a cap run when it is not, and it is the surface a reader chasing a
/// starved drive looks at first.
///
/// Split from the hold below rather than asserted before it, for the reason that
/// bit this PR twice already: a red evidences only the assertion it reaches, so
/// two claims in one test means the second is never seen to fail.
#[test]
fn a_refusal_that_is_not_the_caps_stops_publishing_a_cap_run_duration() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::with(WORKFLOW_TWO_LANES);
    let gh = FakeGh::green(HEAD_A);
    let (group, _orch, _at) = two_lane_stamp_then_duplicate(&reg, &repo, &gh);

    let all = rows_for(&reg, &group, "rd-refused");
    assert_eq!(all.len(), 2, "{all:?}");
    assert_eq!(all[0]["starved_ms"], json!(0), "the cap refusal opened the run: {all:?}");
    assert_eq!(all[1]["cap"], json!(false), "the second refusal is not the cap's: {all:?}");
    assert_eq!(
        all[1]["starved_ms"],
        json!(null),
        "…so the row must not go on publishing a cap-run duration beside it: {all:?}"
    );
}

/// **#2109 review 4, W1 — the exit.** The cap-starvation stamp is a claim about
/// the run happening NOW, so a refusal that is not the cap's ends it.
///
/// The stamp used to be guarded on the write edge alone — written only when
/// `cap`, cleared only by a lane opening or a state arc — which made it a
/// **latch**: a single early cap refusal aged into `held(cap-full)` behind a run
/// of refusals that were nothing of the kind, which is exactly what the comment
/// above the write says must not happen.
///
/// This drives the composition rather than the mechanism, because the mechanism
/// was already green in isolation and that is the point: the stamp belongs to
/// the ENTRY while `first_stale_lane` re-picks the lane every tick, so the two
/// can come apart. At the moment of the hold a lane IS open, and the action
/// actually owed — a delta into lane 0's own pane once it frees up — costs no
/// slot at all, so the notice's remedy would send an orchestrator to kill a pane
/// for a condition killing a pane does not fix.
///
/// The second half is the non-vacuity control, and it is why this is a pin on
/// the *clear* rather than on `cap-full` being hard to reach: with the cap
/// genuinely refusing throughout, the same drive at the same clock does park.
#[test]
fn a_cap_stamp_does_not_outlive_the_cap_and_park_a_drive_on_a_refusal_of_another_kind() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::with(WORKFLOW_TWO_LANES);
    let gh = FakeGh::green(HEAD_A);
    let (group, _orch, at) = two_lane_stamp_then_duplicate(&reg, &repo, &gh);

    let later = reg.rd_drive_group_with(&group, &gh, at + reviewdrive::CAP_HOLD_MS);
    assert_eq!(
        status_state(&reg, &group),
        "review-wait",
        "a stamp left by lane 1's cap refusal must not park the drive on lane 0's duplicate \
         refusal — the cap is not what is refusing, a lane IS open, and the delta the drive \
         owes costs no slot"
    );
    assert!(
        later.notices.iter().all(|n| !n.contains("cap")),
        "…and nothing may tell the orchestrator to free a slot: {:?}",
        later.notices
    );

    // The control: the same clock, with the cap genuinely refusing throughout,
    // DOES park. Without it the assertions above pass under an implementation
    // that simply never holds.
    let dir2 = tempfile::tempdir().unwrap();
    let reg2 = relaunch_registry(dir2.path());
    let repo2 = Repo::new();
    let gh2 = FakeGh::green(HEAD_A);
    let (group2, _orch2, at2) = cap_starved(&reg2, &repo2, &gh2);
    reg2.rd_drive_group_with(&group2, &gh2, at2 + reviewdrive::CAP_HOLD_MS);
    assert_eq!(
        status_state(&reg2, &group2),
        "held",
        "the control: an unbroken cap run still reaches the hold"
    );
}

// ── #2110: the bound measures time IN a state, and forgives what the cap took ──

/// Drive a PR that keeps MOVING, for `cycles` rounds spaced `step_ms` apart.
///
/// Each cycle is two arcs and no counter: `ci-wait -> review-wait` on a green
/// tick, then `review-wait -> ci-wait` on the tick after the head has moved
/// under the lane (arc 6, which `decide_review_wait` checks before it reads a
/// verdict or opens a lane — so no lane is ever spawned and the fixture stays a
/// statement about the clocks). The head alternates between the two constants,
/// which is enough: what arc 6 reads is that the live head DIFFERS from the
/// recorded one, not which sha it is.
///
/// Answers the clock of the last tick it took.
fn progressing(
    reg: &OrchRegistry,
    gh: &FakeGh,
    group: &GroupId,
    cycles: u64,
    step_ms: u64,
) -> u64 {
    let mut last = 0;
    for i in 0..cycles {
        let t = i * step_ms;
        reg.rd_drive_group_with(group, gh, t);
        let head = if i % 2 == 0 { HEAD_B } else { HEAD_A };
        gh.set_facts("OPEN", head);
        reg.set_pr_head_override(Some(head.to_string()));
        last = t + 60_000;
        reg.rd_drive_group_with(group, gh, last);
    }
    last
}

/// **#2110's first ask.** A drive that is making progress must not be parked
/// for having existed a while.
///
/// The measured incident: PR #2104's drive was parked `held(drive-stalled)` —
/// "the drive passed its total age bound" — at about four hours, with round 2
/// live, a blocking finding just fixed and CI green at the new head. Nothing
/// was stalled. An age cannot tell progress from paralysis, because every
/// drive's age grows at the same rate whatever it is doing, and four hours of
/// real review rounds is what the driver exists to spend.
///
/// So the fixture is a drive doing nothing but advancing, taken past the bound
/// that used to park it. It is the ONE assertion a fixture like this can carry
/// honestly — "still not held" — so the two things that would make it vacuous
/// are pinned beside it: that the drive really did reach that age, and that it
/// really is still working rather than sitting in some state a bound forgot.
#[test]
fn a_drive_that_keeps_advancing_is_not_parked_for_its_age_alone() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    let (group, _session) = driven(&reg, &repo, &gh);

    // Eleven cycles half an hour apart: five hours of wall clock, every state
    // left well inside its own bound (29 minutes in `ci-wait` against ninety,
    // one minute in `review-wait` against four hours — its constant plus this
    // one-lane gate's own sixty-minute timeout).
    let last = progressing(&reg, &gh, &group, 11, 30 * 60_000);

    // **Read on the clock the ticks ran on.** `review_drive_status` derives every
    // figure from the `now` it is handed, so the wall-clock reading answers in wall
    // units against anchors stamped in this test's units — which is how the first
    // draft of this test passed: `since_ms` was an epoch-sized number that cleared
    // the bound below without the fixture having advanced at all.
    let status = reg.review_drive_status_with(&group, last);
    let drive = &status["drives"][0];
    assert!(
        drive["since_ms"].as_u64().unwrap_or(0) > 240 * 60_000,
        "the fixture must actually pass the bound that used to park it, or this pins \
         nothing: {status}"
    );
    assert_ne!(
        drive["state"],
        json!("held"),
        "a drive advancing every half hour was parked for its age: {status}"
    );
    assert_eq!(
        drive["held_reason"],
        json!(null),
        "…and with no reason, which is what a working drive has: {status}"
    );
    // Not vacuous by the drive having quietly stopped: it is in a working state
    // and its own per-state clock is short, so the reason nothing fired is that
    // nothing was stuck — not that some state has no bound.
    assert_eq!(drive["state"], json!("ci-wait"), "{status}");
    assert!(
        drive["state_ms"].as_u64().unwrap_or(u64::MAX) <= 30 * 60_000,
        "the drive must be freshly in this state, or 'not held' says nothing: {status}"
    );
    assert_eq!(
        drive["starved_ms"],
        json!(0),
        "…and nothing here was excluded, so the age above is the whole five hours \
         rather than a figure the exclusion flattered: {status}"
    );
    assert_eq!(
        status_head(&reg, &group),
        HEAD_B,
        "the drive must have followed the last head move, or it stopped advancing \
         somewhere in the loop and this is a test about a drive that stalled quietly"
    );
}

/// **#2110's second ask.** A drive that really is stuck is parked on the state
/// it is stuck IN, and the notice says which, for how long, and against what.
///
/// `ci-wait` is the state chosen because before this it had no bound of its own
/// at all: `CiObservation::Pending` returns `Wait` for ever, and the first thing
/// to notice used to be the total age hours later, on a notice that named
/// neither the state nor a number. An orchestrator reading "the drive passed its
/// total age bound" has exactly one move available — resume and see — which is
/// the reflex #2110 asks to turn back into a decision.
///
/// The one-tick-short half is the discriminator. An implementation that parks a
/// drive the moment it stops advancing fails there, and one that never parks
/// fails below it.
#[test]
fn a_drive_stuck_in_one_state_parks_on_that_states_bound_and_the_notice_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    // CI that never resolves — the wait `ci-wait` is for, and the one no other
    // hold can see.
    gh.set_checks(r#"[{"name":"build","state":"IN_PROGRESS","link":"x"}]"#);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    let (group, _session) = driven(&reg, &repo, &gh);

    // **The bound is derived, not restated.** `CI_WAIT_BOUND_MS` was the whole
    // of `ci-wait`'s bound when this was written; since #2168 E1 it is the SLACK
    // over `fix_timeout_minutes`, which the fixture leaves at its 60-minute
    // default. Written as the constant, the "one tick short" half would tick at
    // 89m59s against a 150-minute bound and stop discriminating anything.
    let bound = reviewdrive::state_bound_ms(reviewdrive::DriveState::CiWait, &DriveLimits::default(), 0)
        .expect("a working state has a bound");
    let short = reg.rd_drive_group_with(&group, &gh, bound - 1);
    assert_eq!(
        status_state(&reg, &group),
        "ci-wait",
        "a check run that is merely slow is a wait, not a hold"
    );
    assert!(
        short.notices.iter().all(|n| !n.contains("HELD")),
        "and it costs no orchestrator turn: {:?}",
        short.notices
    );

    let held = reg.rd_drive_group_with(&group, &gh, bound);
    assert_eq!(status_state(&reg, &group), "held");
    let status = reg.review_drive_status(&group);
    let drive = &status["drives"][0];
    assert_eq!(
        drive["held_reason"],
        json!("state-stalled"),
        "the hold names the state clock, not the drive's age: {status}"
    );
    assert_eq!(
        drive["held_state"],
        json!("ci-wait"),
        "…and the status says what the drive was doing, so a resume is a decision: {status}"
    );
    assert_eq!(
        drive["held_state_ms"],
        json!(bound),
        "…and for how long, measured on the clock that fired: {status}"
    );

    let notice = held
        .notices
        .iter()
        .find(|n| n.contains("HELD"))
        .expect("a hold delivers exactly one notice");
    assert!(
        notice.contains("in ci-wait for 2h 30m"),
        "the notice must name the state and the time in it: {notice}"
    );
    assert!(
        notice.contains("bound for that state is 2h 30m"),
        "…and the bound that decided, so a near miss reads differently from a long \
         stall: {notice}"
    );
    assert!(
        notice.contains("drive_review") && notice.contains("cancel_review_drive"),
        "…and the two things an orchestrator can do about it: {notice}"
    );
}

/// **#2110's third ask, through the seam that publishes it.** Time the cap
/// refused this drive a lane is excluded from both clocks, and the numbers say
/// so where an orchestrator can read them.
///
/// The measured incident: PR #2105's drive spent about three of its four hours
/// in `review-wait` with `lanes: []` because another drive's released lanes held
/// every slot, and that starvation was charged to the age budget of the drive
/// that was starved. A hold is not progress and it is not a stall.
///
/// **The figures asserted here ARE the bound's inputs**, which is what makes
/// this more than a status-view test: `decide` reads `state_elapsed_ms` — the
/// same call `state_ms` is rendered from — and `age_ms` minus `starved_ms`,
/// both published here. The decision-level half, where the exclusion changes
/// which hold fires, is `time_the_cap_refused_a_lane_advances_neither_age_bound`
/// in the engine crate.
///
/// Measured one tick short of `CAP_HOLD_MS` deliberately: the drive is still
/// working there, so these are the clocks of a live drive rather than of a
/// parked one.
#[test]
fn time_the_cap_refused_this_drive_is_excluded_from_the_clocks_it_publishes() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    // `cap_starved` enters `review-wait` at 10_000 and takes the first refusal
    // at `at` (20_000). Everything after `at` is time this drive could not act.
    let (group, _orch, at) = cap_starved(&reg, &repo, &gh);

    let now = at + reviewdrive::CAP_HOLD_MS - 1;
    reg.rd_drive_group_with(&group, &gh, now);
    let status = reg.review_drive_status_with(&group, now);
    let drive = &status["drives"][0];
    assert_eq!(
        drive["state"],
        json!("review-wait"),
        "the drive must still be working, or these are a parked drive's clocks: {status}"
    );
    assert_eq!(
        drive["since_ms"],
        json!(now),
        "`since_ms` stays the WALL age — an age that shrank when a cap cleared would be a \
         worse answer than the one it replaced: {status}"
    );
    assert_eq!(
        drive["starved_ms"],
        json!(now - at),
        "…and the excluded total is published beside it, so the difference is checkable \
         rather than inferred: {status}"
    );
    assert_eq!(
        drive["state_ms"],
        json!(10_000),
        "the state clock must hold at the ten seconds this drive actually spent able to \
         act; charging it the cap's fifteen minutes is what reported PR #2105 as \
         stalled: {status}"
    );
}

/// **#2110's fourth ask.** The backstop is still a backstop: a drive that
/// advances for ever and never finishes is still parked, and its notice now
/// says what it was doing.
///
/// This is §8's `also: [base-green]` row in miniature — a drive with an advance
/// available on every wake resets every per-state clock, so the per-state bounds
/// can never see it and the total age is the only thing that can. Which is why
/// the age was kept rather than replaced, and why it is checked BEFORE the state
/// bounds in `decide`.
///
/// Same fixture as the first test in this section, run past twelve hours instead
/// of stopped at five — so the pair is one drive under two clocks, and the
/// difference between them is the whole design.
#[test]
fn the_total_age_backstop_still_parks_a_drive_that_advances_for_ever() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    let (group, _session) = driven(&reg, &repo, &gh);

    // Twenty-four cycles at half an hour: eleven and a half hours of advancing,
    // one tick short of the backstop.
    let last = progressing(&reg, &gh, &group, 24, 30 * 60_000);
    assert!(last < 720 * 60_000, "the fixture must stop SHORT of the bound: {last}");
    assert_ne!(
        status_state(&reg, &group),
        "held",
        "eleven and a half hours of advancing is inside the backstop"
    );

    let held = reg.rd_drive_group_with(&group, &gh, 720 * 60_000);
    assert_eq!(status_state(&reg, &group), "held");
    let status = reg.review_drive_status_with(&group, 720 * 60_000);
    assert_eq!(
        status["drives"][0]["held_reason"],
        json!("drive-stalled"),
        "the backstop is what fires on a drive no per-state clock can catch: {status}"
    );
    assert_eq!(
        status["drives"][0]["held_state"],
        json!("ci-wait"),
        "…and it still records what the drive was doing: {status}"
    );

    let notice = held
        .notices
        .iter()
        .find(|n| n.contains("HELD"))
        .expect("a hold delivers exactly one notice");
    assert!(
        notice.contains("total age bound of 12h"),
        "the notice must name the bound's value, which is #2110's second bullet: {notice}"
    );
    assert!(
        notice.contains("in ci-wait for"),
        "…and what the drive was doing when it fired, which is the third: {notice}"
    );
    assert!(
        notice.contains("BACKSTOP"),
        "…and that this is the backstop rather than a claim the drive sat still: {notice}"
    );
}

// ── #2135: the cap-starvation stamp across a process boundary, and under ─────
// ── alternating refusal kinds                                            ─────

/// The whole of `<group-dir>/review_drives.json`, parsed.
///
/// Read as JSON rather than through `reviewdrive::load_state`, because what
/// these tests are about is the FILE: an optional field is invisible to a typed
/// load (it deserializes to the same `None` whether it was absent or written
/// `null`), and "the stamp reached disk" is exactly the claim a typed read
/// cannot make.
pub(crate) fn drives_json(reg: &OrchRegistry, group: &GroupId) -> serde_json::Value {
    let p = reg.state_root().join(group.as_str()).join(reviewdrive::REVIEW_DRIVES_FILE);
    let body = std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("review_drives.json at {}: {e}", p.display()));
    serde_json::from_str(&body).expect("review_drives.json must be JSON")
}

/// The worker session the drive record holds, so a test can resume a drive
/// without every fixture in this file having to hand one back.
/// The LIVE drive's lane records, as JSON. Selected by state rather than by
/// index: a cancelled entry can still be on file beside the fresh one, and
/// `entries[0]` would then be a claim about whichever `prune_terminal` happened
/// to leave first.
pub(crate) fn live_lanes(reg: &OrchRegistry, group: &GroupId) -> Vec<serde_json::Value> {
    drives_json(reg, group)["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|e| {
            !matches!(e["state"].as_str(), Some("satisfied") | Some("cancelled") | None)
        })
        .and_then(|e| e["lanes"].as_array().cloned())
        .unwrap_or_default()
}

pub(crate) fn driven_worker_session(reg: &OrchRegistry, group: &GroupId) -> String {
    drives_json(reg, group)["entries"][0]["worker_session"]
        .as_str()
        .expect("a live drive record names the worker session it is driving")
        .to_string()
}

/// **#2135(a), the record.** The `held(cap-full)` anchor is written to
/// `review_drives.json`, so it is a fact the next process inherits rather than
/// one the shutdown discards.
///
/// This is the premise the two behaviour tests below rest on, and it is not
/// obvious either way: the field is `skip_serializing_if = "Option::is_none"`,
/// and a field that is sometimes omitted is one edit from a field that is
/// always omitted. Split from those tests rather than asserted inside them
/// because a red here and a red there mean different things — one is a
/// persistence defect, the other a decision defect — and a red evidences only
/// the assertion it reached.
///
/// The second half is the control: the key really is optional, so its presence
/// in the first half is caused by the cap refusal and not by a serializer that
/// writes it unconditionally.
#[test]
fn the_cap_starvation_stamp_is_written_to_the_drive_record() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _orch, at) = cap_starved(&reg, &repo, &gh);

    let stored = drives_json(&reg, &group);
    assert_eq!(
        stored["entries"][0]["cap_starved_since_ms"],
        json!(at),
        "the anchor `held(cap-full)` is decided from must reach the file, or a resumed drive \
         decides from a different entry than the one that was stored: {stored}"
    );

    let dir2 = tempfile::tempdir().unwrap();
    let reg2 = relaunch_registry(dir2.path());
    let repo2 = Repo::new();
    let gh2 = FakeGh::green(HEAD_A);
    reg2.set_pr_head_override(Some(HEAD_A.to_string()));
    let (group2, _s) = driven(&reg2, &repo2, &gh2);
    reg2.rd_drive_group_with(&group2, &gh2, 10_000);
    let clean = drives_json(&reg2, &group2);
    assert_eq!(
        clean["entries"][0]["cap_starved_since_ms"],
        json!(null),
        "a drive the cap never refused must carry no stamp at all — otherwise the assertion \
         above is about a serializer and not about a starvation: {clean}"
    );
}

/// **#2135(a), the exit — and the defect it found.** A cap-starvation run
/// cannot straddle a process boundary, so §2.4's restart reconcile drops it.
///
/// The shipped behaviour before this test: the stamp is persisted (above),
/// nothing on the restart path touched it, and `decide` reads
/// `cap_starved_for >= CAP_HOLD_MS` **above** the arm that proposes a spawn. So
/// the first tick of the new process parked the drive `held(cap-full)` before
/// one spawn was attempted — on a notice telling an orchestrator to free a
/// slot, in a group where every pane died with the previous process and the cap
/// is empty. The stamp's own field doc is what makes that wrong: what it
/// measures is a run of refusals ticks OBSERVED, and across the gap no tick
/// ran.
///
/// **The relaunched registry's empty `agents` map is the fixture, not a
/// shortcut.** That is what a real restart looks like — panes do not survive
/// the process — and it is what makes the two outcomes distinguishable here at
/// all: with the cap still full, "parked on a stale stamp" and "parked on a
/// live one" produce the same state.
#[test]
fn a_cap_stamp_from_a_previous_process_does_not_park_a_drive_whose_cap_the_restart_freed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, at) = {
        let reg = relaunch_registry(dir.path());
        let (group, _orch, at) = cap_starved(&reg, &repo, &gh);
        (group, at)
    };

    // The restart: a second registry over the same state root. Its `agents` map
    // is empty, so this group's live-delegate cap is empty too.
    let reg = relaunch_registry(dir.path());
    reg.create_group(&repo.path(), rails_capped()).unwrap();
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7001);
    assert_eq!(
        drives_json(&reg, &group)["entries"][0]["cap_starved_since_ms"],
        json!(at),
        "the control: the new process really did inherit a live stamp, so what follows is \
         about the decision and not about a stamp that was never there"
    );

    let first = reg.rd_drive_group_with(&group, &gh, at + reviewdrive::CAP_HOLD_MS);
    assert_ne!(
        status_state(&reg, &group),
        "held",
        "a stamp from a process that is gone must not park the resumed drive on its first \
         tick: no tick observed the cap across the gap, and after the restart the cap this \
         drive was starved by is empty"
    );
    assert!(
        !first.lanes_opened.is_empty(),
        "…and the drive must actually try the slot the restart freed: {:?}",
        rows_for(&reg, &group, "rd-refused")
    );
    assert!(
        first.notices.iter().all(|n| !n.contains("cap")),
        "…and nothing may tell the orchestrator to free a slot in a group whose slots are \
         all free: {:?}",
        first.notices
    );

    // The clear is on the record, because its only other visible effect is a
    // drive that did NOT park — indistinguishable on this log from there having
    // been no stamp at all. The first row is the first process's own reconcile,
    // which had nothing to forget, and is the non-vacuity control for the flag.
    let recovered = rows_for(&reg, &group, "rd-recovered");
    assert_eq!(
        recovered.len(),
        2,
        "one reconcile per REGISTRY INSTANCE — which this fixture cannot tell apart from \
         per process, because `relaunch_registry` builds its second registry inside this \
         one (#2135 review 2, premortem 1): {recovered:?}"
    );
    assert_eq!(
        recovered[0]["cap_run_forgotten"],
        json!(false),
        "the first process's reconcile ran before any refusal, so it forgot nothing: \
         {recovered:?}"
    );
    assert_eq!(
        recovered[1]["cap_run_forgotten"],
        json!(true),
        "…and the restart's says it dropped the run the shutdown left standing: {recovered:?}"
    );
}

/// **#2135's residual, pinned rather than merely admitted.** The restart clear
/// is scoped to the PROCESS boundary, and an in-process tick gap longer than
/// `CAP_HOLD_MS` still parks on a single observed refusal.
///
/// Distinct from `a_cap_that_starves_a_drive_parks_it_as_cap_full_rather_than_leaving_it_silent`
/// in the one way that matters: that test ticks at `CAP_HOLD_MS - 1` first, so
/// the cap is re-observed a millisecond before the hold. Here nothing is
/// observed between the single refusal and the park, so what parks the drive is
/// a stamp whose run no tick re-confirmed — the same shape the restart case is
/// about, on the one side of the boundary the fix deliberately does not cross.
///
/// It is the right direction (a drive really starved for fifteen minutes is
/// parked whether or not orrerix was busy), and closing it wants a
/// `last_tick_ms` and a gap rule, which is #2117's own disclosed non-decision.
///
/// **What this fixture structurally cannot witness**: the latch is a field of
/// the REGISTRY, so the guarantee is once per group per registry instance, and
/// `relaunch_registry` builds its second registry inside this same process.
/// "Restart" here therefore means "a second registry", and no test in this file
/// can separate the two (#2135 review 2, premortem 1).
/// Pinning it is what stops the disclosure in `discard_cap_starvation_run` from
/// going false quietly in either direction.
#[test]
fn an_in_process_tick_gap_still_parks_on_a_single_observed_cap_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _orch, at) = cap_starved(&reg, &repo, &gh);

    // One tick, a whole window later, in the SAME process — nothing between.
    reg.rd_drive_group_with(&group, &gh, at + reviewdrive::CAP_HOLD_MS);
    assert_eq!(
        status_state(&reg, &group),
        "held",
        "the clear is the RESTART reconcile's and nothing wider: in one process the stamp \
         still ages across a gap no tick observed"
    );
    let status = reg.review_drive_status_with(&group, at + reviewdrive::CAP_HOLD_MS);
    assert_eq!(
        status["drives"][0]["held_reason"],
        json!("cap-full"),
        "…and it is still the cap that is named: {status}"
    );
    assert_eq!(
        rows_for(&reg, &group, "rd-refused").len(),
        1,
        "the point of the fixture: exactly ONE refusal was ever observed"
    );
}

/// **#2135(a), the half that makes the resume safe — and it is not the
/// resume's doing.** Taking `held(cap-full)` disarms the very clock it fired
/// on, because the hold is an ARC and `advance` zeroes the stamp on every one.
///
/// Split out and asserted here, one whole tick before anyone resumes anything,
/// because that is the only place it can be FALSE. A draft asserted the field
/// was `None` **after** the resume and credited arc 11 with it; that assertion
/// holds under every implementation of arc 11 there is — including one that
/// carries the run faithfully — because by then there is no run left to carry.
/// Two scratch rounds cut to redden it passed before the claim was corrected
/// rather than shipped, and the second of those is what showed the reason is
/// structural: arc 11 lands the drive in `ci-wait`, so it must take a SECOND
/// arc to reach `review-wait` where the cap is even consulted, and that arc
/// clears anything the first one left.
///
/// Registry-level on purpose: the engine's own
/// `the_starvation_clock_is_stamped_once_per_run_and_no_arc_carries_one_across`
/// pins the arc, and nothing pinned it through the tick that takes the hold.
#[test]
fn the_cap_full_hold_disarms_the_clock_it_fired_on() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _orch, at) = cap_starved(&reg, &repo, &gh);
    assert_eq!(
        drives_json(&reg, &group)["entries"][0]["cap_starved_since_ms"],
        json!(at),
        "the control: a run really is open going into the park, or the read below says nothing"
    );
    reg.rd_drive_group_with(&group, &gh, at + reviewdrive::CAP_HOLD_MS);
    assert_eq!(status_state(&reg, &group), "held", "the fixture must park, or nothing fired");
    assert_eq!(
        drives_json(&reg, &group)["entries"][0]["cap_starved_since_ms"],
        json!(null),
        "the arc INTO `held(cap-full)` clears the stamp: the hold is the report, and a drive \
         that has reported is no longer one accumulating a run"
    );
}

/// **#2135(a), the resume half.** A resumed drive gets a WHOLE new
/// `CAP_HOLD_MS`, measured from its own first refusal and not from anything the
/// run that parked it left behind.
///
/// Split from the disarm above rather than asserted after it, for the reason
/// that shape keeps earning here: a red evidences only the assertion it
/// REACHED, and the two are different claims — one about the hold, one about
/// the resume. Together in one test the disarm reddens first and the window is
/// never measured.
///
/// The last two ticks are the discriminator, and they are why this is a pin on
/// the WINDOW rather than on "it did not re-park immediately": an
/// implementation that merely suppressed the first re-park passes the middle
/// assertion and fails the last.
#[test]
fn a_drive_review_resume_starts_the_cap_window_over_rather_than_re_parking() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _orch, at) = cap_starved(&reg, &repo, &gh);
    reg.rd_drive_group_with(&group, &gh, at + reviewdrive::CAP_HOLD_MS);
    assert_eq!(
        status_state(&reg, &group),
        "held",
        "the fixture parks first, or there is nothing to resume"
    );

    let resumed_at = at + reviewdrive::CAP_HOLD_MS + 1_000;
    let session = driven_worker_session(&reg, &group);
    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", resumed_at);
    assert_eq!(out["driving"], json!(true), "the resume must be accepted: {out}");

    // `ci-wait` -> `review-wait`, then the first refusal of the NEW run.
    reg.rd_drive_group_with(&group, &gh, resumed_at + 1_000);
    let refused_at = resumed_at + 2_000;
    reg.rd_drive_group_with(&group, &gh, refused_at);
    assert_ne!(
        status_state(&reg, &group),
        "held",
        "the cap is still full, but this run is seconds old: {:?}",
        rows_for(&reg, &group, "rd-refused")
    );

    reg.rd_drive_group_with(&group, &gh, refused_at + reviewdrive::CAP_HOLD_MS - 1);
    assert_ne!(
        status_state(&reg, &group),
        "held",
        "one tick short of a whole NEW window, measured from the first refusal after the \
         resume rather than from anything the old run left behind"
    );
    reg.rd_drive_group_with(&group, &gh, refused_at + reviewdrive::CAP_HOLD_MS);
    assert_eq!(
        status_state(&reg, &group),
        "held",
        "…and a genuinely new full run parks it again, so the resume is a fresh window and \
         not an exemption"
    );
}

/// One cycle of the alternation #2135(b) is about: a **cap** refusal, then a
/// refusal of another kind — one of each, in that order, `step_ms` apart.
///
/// Continues the state `two_lane_stamp_then_duplicate` leaves, whose last tick
/// was the duplicate. Lane 0 answers about the body in front of it, so
/// `first_stale_lane` moves on to lane 1, whose spawn the cap refuses and which
/// STAMPS the clock; then the body moves, lane 0's pass no longer stands, the
/// gate comes back to lane 0, and the duplicate refusal — not the cap's —
/// CLEARS it.
///
/// **`step_ms` must be under `CAP_HOLD_MS` or the fixture stops being about
/// alternation**: a single cap run longer than the window parks the drive
/// `cap-full` on the very next tick, and the non-cap refusal that would have
/// cleared it never happens, because `decide` holds above the arm that proposes
/// the spawn. That is the real tick cadence's own property (`RD_BACKOFF_MS` is
/// minutes, the window is fifteen), and it is asserted rather than assumed.
///
/// Answers the clock of the tick that closed the cycle.
fn alternate_refusal_kinds(
    reg: &OrchRegistry,
    gh: &FakeGh,
    group: &GroupId,
    lane0: &str,
    at: u64,
    step_ms: u64,
    nth: u64,
) -> u64 {
    assert!(step_ms < reviewdrive::CAP_HOLD_MS, "see this function's doc: {step_ms}");
    dispatch(
        reg,
        &Caller {
            agent_id: lane0.to_string(),
            group: group.clone(),
            role: Role::Reviewer,
            role_hint: None,
        },
        "tools/call",
        &json!({ "name": "review_verdict", "arguments": {
            "pr": "1758", "verdict": "pass", "summary": "pass - lane one is still happy" } }),
    )
    .expect("lane 0 records");
    // Lane 1 is what the gate wants next, and the cap is full: the stamp opens.
    reg.rd_drive_group_with(group, gh, at + step_ms);
    // The body moves, so the gate comes back to lane 0 and the duplicate
    // refusal — which is not the cap's — closes the run.
    let body = format!("b-alt-{nth}");
    gh.set_body(&body);
    reg.set_pr_body_override(Some(body));
    reg.rd_drive_group_with(group, gh, at + 2 * step_ms);
    at + 2 * step_ms
}

/// **#2135(b).** Alternating cap and non-cap refusals postpone the park, and
/// the postponement is BOUNDED — rev-std's premortem on #2112 named the
/// opposite ("silent-loop aging of a standing stamp").
///
/// It is coverage rather than a fix, and what it covers is a cost #2109 review
/// 4 chose deliberately: clearing the stamp on every non-cap refusal is what
/// makes `held(cap-full)`'s word "continuously" true, and its stated price is
/// that a mixed run restarts the window at each cap refusal. Under a strict
/// alternation that price is total — `cap-full` never fires at all — so what
/// answers the premortem is not that hold but the one below it, and the two
/// numbers this pins are the ones nobody had measured.
///
/// **What comes out, on stock knobs and a two-lane gate**: the drive parks
/// `held(state-stalled)` at **2.00x** the `review-wait` bound of five hours —
/// 36,040,000 ms against a nominal 18,000,000 — because #2110's accumulators
/// forgive every ended cap run and a strict alternation makes those runs
/// exactly half the timeline. The floor, the ceiling and the factor are all
/// assertions: an implementation that forgave nothing would park at the bound
/// itself and fail the floor, and one that let the exclusion run away — a
/// per-block clock, say, which #2109 review 4 rejected for exactly this reason
/// — would fail the ceiling. Every clock is injected, so the factor is
/// deterministic and is pinned rather than bracketed.
///
/// The refusal counts are the population control, and they are taken off the
/// `cap` boolean rather than off row totals: `rd-refused` is written for EVERY
/// refused lane spawn and only that flag says which kind it was. Counting rows
/// instead reads a one-for-one alternation as a run of caps twice as long,
/// which is what this control caught on its own first CI round.
#[test]
fn alternating_cap_and_non_cap_refusals_postpone_the_park_by_a_bounded_factor() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::with(WORKFLOW_TWO_LANES);
    let gh = FakeGh::green(HEAD_A);
    let (group, _orch, at) = two_lane_stamp_then_duplicate(&reg, &repo, &gh);
    let lane0 = drives_json(&reg, &group)["entries"][0]["lanes"][0]["agent"]
        .as_str()
        .expect("lane 0 opened, so the record names its pane")
        .to_string();

    // `review-wait`'s own bound at this gate: the three-hour constant plus one
    // `lane_timeout_minutes` per required lane (#2117 review 2), both at stock.
    let nominal = 180 * 60_000 + 2 * 60 * 60_000;
    let step = 5 * 60_000;
    let mut t = at;
    let mut parked_at = None;
    for nth in 0..120 {
        t = alternate_refusal_kinds(&reg, &gh, &group, &lane0, t, step, nth);
        if status_state(&reg, &group) == "held" {
            parked_at = Some(t);
            break;
        }
    }
    let parked_at = parked_at.unwrap_or_else(|| {
        panic!(
            "the alternation postponed the park past {t} ms — it must be bounded, not silent \
             (refusals: {} cap, {} duplicate)",
            rows_for(&reg, &group, "rd-refused").len(),
            rows_for(&reg, &group, "rd-lane-duplicate-refused").len()
        )
    });

    let status = reg.review_drive_status_with(&group, parked_at);
    assert_eq!(
        status["drives"][0]["held_reason"],
        json!("state-stalled"),
        "`cap-full` cannot fire under an alternation — every non-cap refusal restarts its \
         window — so what answers the premortem is the per-state bound: {status}"
    );
    assert_eq!(
        status["drives"][0]["held_state"],
        json!("review-wait"),
        "…and the notice still names the wait the drive was actually in: {status}"
    );
    // **The figure itself, pinned rather than bracketed — and asserted FIRST,
    // above the band it sits inside.** Under an even alternation the ended cap
    // runs are exactly half the timeline, so the forgiveness is half and the
    // bound is reached at twice the wall time it nominally names. Every clock
    // here is injected, so this is deterministic and not a tolerance; it is
    // pinned exactly because publishing the number is the point, and a band
    // would hide a change behind a range nobody re-derives.
    //
    // Ordered above the floor and ceiling deliberately: a red evidences only
    // the assertion it REACHED, and with the loose pair first every mutation
    // large enough to move the park trips one of those and leaves this one
    // unreached — which is exactly what the first cut of the counterfactual
    // round did. The tightest pin goes first so it is the one that speaks.
    //
    // A deliberate move of `REVIEW_WAIT_BOUND_MS` or of this fixture's step is
    // meant to redden this; re-measure it here rather than widening the band.
    assert_eq!(
        parked_at, 36_040_000,
        "the forgiven half is what sets this: 2.00x the nominal {nominal} ms, measured — \
         got {parked_at}"
    );
    assert!(
        parked_at > nominal * 3 / 2,
        "the postponement is REAL and is the cost #2109 review 4 chose: the ended cap runs \
         are forgiven, so the bound is reached at wall time well past the {nominal} ms it \
         nominally names (parked at {parked_at})"
    );
    assert!(
        parked_at < nominal * 3,
        "…but it is bounded by a small factor, not indefinite: {parked_at} against a nominal \
         {nominal}"
    );

    // The population control: the fixture really did alternate, one refusal of
    // each kind per cycle, rather than drifting into a run of one.
    //
    // **Counted off the `cap` boolean, not off the row count.** `rd-refused` is
    // written for EVERY refused lane spawn and distinguishes the kinds with
    // that flag (#1960); `rd-lane-duplicate-refused` is an additional row the
    // duplicate arm writes from inside `rd_open_lane`. So a bare
    // `rows_for("rd-refused").len()` counts both kinds and reads as a run of
    // caps twice as long as the alternation — which is exactly what this
    // assertion caught on its first CI round (120 / 60 against a real 60 / 60).
    let refused = rows_for(&reg, &group, "rd-refused");
    let caps = refused.iter().filter(|r| r["cap"] == json!(true)).count() as i64;
    let non_caps = refused.iter().filter(|r| r["cap"] == json!(false)).count() as i64;
    let dups = rows_for(&reg, &group, "rd-lane-duplicate-refused").len() as i64;
    assert!(caps >= 10 && non_caps >= 10, "too few cycles to be about alternation: {caps}/{non_caps}");
    assert!(
        (caps - non_caps).abs() <= 1,
        "the kinds must alternate one for one — a RUN of either is a different subject: \
         {caps} cap, {non_caps} non-cap"
    );
    assert_eq!(
        non_caps, dups,
        "…and every non-cap refusal here really was the DUPLICATE one, which is the fixture's \
         whole mechanism: {non_caps} non-cap rows against {dups} duplicate rows"
    );
    assert_eq!(
        caps + non_caps,
        refused.len() as i64,
        "…and the flag partitions the rows, so neither count is reading past a row it \
         cannot classify: {} rows",
        refused.len()
    );
    // And no single run ever reached the window, which is WHY `cap-full` never
    // fired: the mechanism, not merely its outcome.
    let longest = refused
        .iter()
        .filter_map(|r| r["starved_ms"].as_u64())
        .max()
        .expect("every cap refusal publishes the run it belongs to");
    assert!(
        longest < reviewdrive::CAP_HOLD_MS,
        "no run may reach {} ms, or the drive would have parked `cap-full` and this test \
         would be about a different hold: longest {longest}",
        reviewdrive::CAP_HOLD_MS
    );
}
