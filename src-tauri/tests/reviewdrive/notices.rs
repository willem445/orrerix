//! Stalls on driven lanes (#3040 N2) and the review driver's notice diet (#3040 N1).
//!
//! One module of the `reviewdrive` integration-test target (`main.rs`),
//! split out of the former single-file `tests/reviewdrive.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ───────── #3040 N2: a stall on a lane the driver owns is not news ─────────

/// **A watchdog stall on a pane a LIVE drive owns is suppressed, with the reason
/// on the audit row** (#3040 N2).
///
/// The driver is already watching that pane on its own tick and answers a stuck
/// lane with a `lane-stalled` HOLD — which names the PR, the lane and what to do
/// about it. The watchdog nudge arrives beside that saying the same thing with
/// no remedy: 16 of the 25 stall notices in #3040's census were exactly this,
/// against `Standard review …` lanes, and none was acted on.
///
/// It lives in this file rather than beside its siblings in
/// `tests/orchestration/` for the reason this file's own header gives: the
/// fixture needs a live drive, and the drive helpers are here. Its control —
/// `a_stall_on_an_undriven_pane_still_announces` — is over there with the rest
/// of the watchdog suite, which is where the assertion it controls for lives.
#[test]
fn a_stall_on_a_driven_lane_is_suppressed_with_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);

    // `driven`'s setup, inlined for one reason: the group needs a non-zero
    // `watchdog_stall_minutes`, and `rails()` leaves the guardrail OFF. Built
    // from `rails()` with that one field changed, so nothing else diverges.
    let group = reg
        .create_group(&repo.path(), Guardrails { watchdog_stall_minutes: 5, ..rails() })
        .unwrap()
        .id;
    let w = reg
        .spawn_agent(&group, Role::Worker, "w", "", false, None)
        .expect("a worker to hand back to");
    let session = w.session_id.clone().expect("claude mints a session id at spawn");
    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 0);
    assert_eq!(out["driving"], serde_json::json!(true), "drive_review refused: {out}");

    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7301);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));

    reg.rd_drive_group_with(&group, &gh, 10_000);
    let report = reg.rd_drive_group_with(&group, &gh, 20_000);
    let (_pr, _block, lane) = report.lanes_opened.first().cloned().expect("lane 0 opens");
    assert!(
        reg.rd_owner(&group, &lane).is_some(),
        "fixture: the drive must really own this lane, or the test proves nothing"
    );

    // The lane has an assignment and has produced nothing since it opened, which
    // is exactly the shape the watchdog fires on.
    let no_output = std::collections::HashMap::new();
    let no_watch = std::collections::HashMap::new();
    let notified = reg.watchdog_tick(FAR, &no_output, &no_watch);
    assert!(
        !notified.contains(&lane),
        "a driven lane's stall reached the orchestrator: {notified:?}"
    );

    let rows = audit_details(&reg, &group, "watchdog-suppressed");
    let row = rows
        .iter()
        .find(|d| d["agent"] == serde_json::json!(lane))
        .unwrap_or_else(|| panic!("the suppression must be diagnosable, not silent: {rows:?}"));
    assert_eq!(row["why"], serde_json::json!("driven-lane"),
        "the row must say WHICH of the three reasons this was: {row}");
    assert_eq!(row["watch_ids"], serde_json::json!([]),
        "…and not borrow #852's field, which is about a live notify_when watch: {row}");

    // ── the control, and the RE-ARM it also pins (rev-std round 1, N2) ──
    //
    // Two things are being asserted by the same sequence, and the second is why
    // the first is written this way. The CONTROL is that this is about the DRIVE
    // and not about reviewer lanes in general: end the drive and the same pane
    // announces, so an implementation that suppressed every lane's stall fails
    // here. The RE-ARM is that ending the drive is ENOUGH — the pane needs no
    // activity, no new assignment and no human touch to become announceable
    // again.
    //
    // An earlier draft of this test fed the lane one tick of synthetic OUTPUT
    // before re-ticking, to clear the anti-nag latch the suppressed stall had
    // set. That made the control pass while hiding a real defect: nothing cleared
    // that latch when a drive ended, so a lane stalled under a drive that was
    // then cancelled — or whose driver died — would never be nudged again, where
    // base announced once. The synthetic tick WAS the bug's disguise, which is
    // exactly what rev-std named. It is gone: the only thing that happens between
    // the suppression and the announcement is the drive going away.
    assert_eq!(
        reg.cancel_review_drive(&group, 1758, "orch-1")["cancelled"],
        serde_json::json!(true),
        "the drive must actually stop, or the control is the same case again"
    );
    assert!(reg.rd_owner(&group, &lane).is_none(), "…and really stop owning the lane");

    // Tick 1 after the drive ends: the re-arm fires. It does NOT announce — the
    // pane is handed a fresh FULL window from this instant, exactly as #852's
    // watch arm does, rather than firing on the remains of the expired one.
    let notified = reg.watchdog_tick(FAR, &no_output, &no_watch);
    assert!(!notified.contains(&lane),
        "the re-arm gives a fresh window, it does not fire on the old one: {notified:?}");

    // Tick 2, a full window later, with NO activity of any kind in between: now
    // it announces. This is the assertion the synthetic output tick was standing
    // in for, and it is what fails against the shipped-without-a-re-arm
    // implementation.
    //
    // It is deliberately the FIRST of the two claims below. A red evidences only
    // the assertion it reached and MOVED, and the `watchdog-rearmed` pin used to
    // sit above this one — so the red instrument aborted on the audit row and
    // never reached the behavioural claim at all, which is the claim worth
    // proving fail-able.
    let notified = reg.watchdog_tick(FAR + 6 * 60_000, &no_output, &no_watch);
    assert!(
        notified.contains(&lane),
        "with no drive owning it, the same silent lane is the orchestrator's business \
         again — and getting there took no activity, only the drive ending: {notified:?}"
    );

    // …and the re-arm is diagnosable rather than silent. Second, for the reason
    // above; its own red is therefore not separately evidenced — the same
    // mutation removes both, and this one is reached only once the behaviour is
    // right.
    assert!(
        audit_details(&reg, &group, "watchdog-rearmed")
            .iter()
            .any(|d| d["agent"] == serde_json::json!(lane)),
        "the re-arm must be diagnosable, not silent"
    );
}

/// A time far past any real `now_ms()`, so a `watchdog_tick` at this instant is
/// unambiguously past the stall window for a pane whose clock was stamped with
/// the real wall clock. (`tests/orchestration/` has its own copy for its own
/// watchdog suite; the two files share no module.)
const FAR: u64 = 1_000_000_000_000_000;

// ── #3040 N1: the review driver's notices go on a diet ──────────────────────

/// **The gate notice carries a COUNT and a pointer, never the summaries.**
///
/// The `GATE SATISFIED` line was 25 % of every byte this driver has ever put in
/// an orchestrator's pane, and most of it was two capped 400-character reviewer
/// summaries that the orchestrator's very next turn read back out of the record
/// through `list_verdicts` anyway. What the line owes is the decision: which
/// lanes answered, what it cost, whether anything is left to disposition, and
/// where the words are.
///
/// **The `!contains` assertions sit beside a positive control that the notice
/// was DELIVERED at all** — an absence assertion passes just as well against a
/// pane that received nothing, which is the failure this test would otherwise
/// be blind to. And the summary is a SENTINEL rather than a phrase the notice
/// might legitimately contain, so a match is the reviewer's text and nothing
/// else.
#[test]
fn a_satisfied_notice_carries_no_lane_summary() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _session) = driven(&reg, &repo, &gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7001);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    reg.set_pr_body_override(Some("b".to_string()));

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
            "pr": "1758", "verdict": "pass",
            "summary": "pass - SENTINELSUMMARY, rename the helper when you get a chance" } }),
    )
    .expect("the lane records its verdict");

    let before = delivered_texts(&reg, &group).len();
    reg.rd_drive_group_with(&group, &gh, 30_000); // review-wait -> gate-check
    reg.rd_drive_group_with(&group, &gh, 40_000); // gate-check -> satisfied

    // The positive control: exactly one line reached the pane, so every
    // `!contains` below is about THAT line and not about silence.
    let after: Vec<String> = delivered_texts(&reg, &group)[before..].to_vec();
    let gate: Vec<&String> = after.iter().filter(|t| t.contains("GATE SATISFIED")).collect();
    assert_eq!(gate.len(), 1, "the gate exit announces exactly once: {after:?}");
    let n = gate[0];

    // What it must still carry — the decision, and where the words are.
    assert!(n.contains("review drive PR #1758: GATE SATISFIED at"), "{n}");
    assert!(n.contains("rev-std PASS"), "the lane verdicts are the gate's answer: {n}");
    assert!(n.contains("rounds") && n.contains("CI") && n.contains("rebases"), "{n}");
    assert!(
        n.contains("1 lane carries non-blocking findings"),
        "the COUNT is what says there is something to disposition: {n}"
    );
    assert!(n.contains("Disposition is yours (INVARIANT 3)"), "{n}");
    assert!(n.contains(r#"list_verdicts("1758")"#), "and the pointer at the words: {n}");

    // What it must NOT: the reviewer's own text, and the prose that was
    // retracted with it. Pinned so the fat form cannot come back silently.
    assert!(!n.contains("SENTINELSUMMARY"), "the summary is re-read, never re-sent: {n}");
    assert!(!n.contains("Non-blocking findings left open"), "{n}");
    assert!(!n.contains("full text:"), "{n}");
    assert!(
        !n.contains("disposing of them is yours") && !n.contains("none of them killed"),
        "the panes clause is a list, not the playbook paragraph: {n}"
    );
    assert!(n.len() < 400, "a gate notice is one decision-grade line ({} bytes): {n}", n.len());
}

/// **A `cancel_review_drive` cancel is audited, not announced** (#533-B's route,
/// applied to the one cancel whose caller is holding the answer already).
///
/// The orchestrator called the tool; the tool answered synchronously with the
/// panes it released and the cancellation itself. A prompt arriving afterwards
/// is a wake-up for a fact its own caller has in hand.
///
/// **The second half is the positive control and it is load-bearing**: the same
/// orchestrator pane, in the same group, DOES receive the cancel notice a tick
/// produces on its own (`CancelCause::PrGone`). Without it, "nothing was
/// delivered" passes equally well against a pane that can receive nothing —
/// which is a fixture defect and not a feature.
#[test]
fn a_tool_cancel_audits_and_delivers_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, session) = driven(&reg, &repo, &gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    make_delivery_land(&reg, &group, &orch.id, 7001);
    reg.rd_drive_group_with(&group, &gh, 10_000);

    let before = delivered_texts(&reg, &group).len();
    let out = reg.cancel_review_drive_with(&group, 1758, "orch-1", 20_000);
    assert_eq!(out["cancelled"], json!(true), "the tool must succeed: {out}");

    let after: Vec<String> = delivered_texts(&reg, &group)[before..].to_vec();
    assert!(
        after.iter().all(|t| !t.contains("CANCELLED")),
        "the caller is holding this answer; a prompt says nothing it lacks: {after:?}"
    );

    // ...and the text exists to read on demand, which is what makes the
    // demotion a route rather than a drop (#1857's argument, kept).
    let demoted = audit_details(&reg, &group, "rd-notice-demoted");
    assert_eq!(demoted.len(), 1, "the demotion is audited exactly once: {demoted:?}");
    assert_eq!(demoted[0]["pr"], json!(1758));
    assert_eq!(demoted[0]["reason"], json!("tool-cancel"));
    let text = demoted[0]["notice"].as_str().unwrap_or_default();
    assert!(
        text.contains("CANCELLED") && text.contains("cancel_review_drive"),
        "the row carries the NOTICE, not merely the fact that one was not sent: {text:?}"
    );
    assert_eq!(
        audit_details(&reg, &group, "rd-cancelled").len(),
        1,
        "and the cancel itself is still on the log, unchanged"
    );
    // Nothing is owed, so nothing retains the entry: the flush prunes it.
    assert!(
        reg.review_drive_status(&group)["drives"].as_array().is_some_and(|d| d.is_empty()),
        "a terminal entry owing no notice leaves at once: {}",
        reg.review_drive_status(&group)
    );

    // ── the positive control ────────────────────────────────────────────────
    // The same pane, the same group: a cancel nobody in this process asked for
    // still interrupts.
    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 30_000);
    assert_eq!(out["driving"], json!(true), "the re-drive must succeed: {out}");
    gh.set_facts("CLOSED", HEAD_A);
    let before = delivered_texts(&reg, &group).len();
    reg.rd_drive_group_with(&group, &gh, 40_000);
    let after: Vec<String> = delivered_texts(&reg, &group)[before..].to_vec();
    assert!(
        after.iter().any(|t| t.contains("CANCELLED")),
        "the control: this pane CAN receive one, so the silence above is the route: {after:?}"
    );
}

/// **A `pr-gone` cancel still announces, and names its own cause.**
///
/// The sibling of the demotion above, kept as its own test because it is the
/// half that must NOT change: nobody in this process asked for this cancel, so
/// it is the only way the orchestrator learns the drive ended.
#[test]
fn a_pr_gone_cancel_still_announces() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _session) = driven(&reg, &repo, &gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    make_delivery_land(&reg, &group, &orch.id, 7001);
    reg.rd_drive_group_with(&group, &gh, 10_000);

    gh.set_facts("MERGED", HEAD_A);
    let before = delivered_texts(&reg, &group).len();
    reg.rd_drive_group_with(&group, &gh, 20_000);
    let after: Vec<String> = delivered_texts(&reg, &group)[before..].to_vec();
    let n = after
        .iter()
        .find(|t| t.contains("CANCELLED"))
        .unwrap_or_else(|| panic!("a cancel nobody asked for must announce: {after:?}"));
    assert!(
        n.contains("the PR is closed or merged"),
        "and say which cause it was, not the tool's: {n}"
    );
    assert!(!n.contains("cancel_review_drive."), "{n}");
    assert!(
        audit_details(&reg, &group, "rd-notice-demoted").is_empty(),
        "this cause is never demoted"
    );
}

/// **A hold the drive has already announced is audited, not repeated.**
///
/// A hold can only ever recur after a resume — `transition` refuses a `held` ->
/// `held` self-arc — so the repeat is always a resume that changed nothing the
/// drive can observe. `cap-full` is the fixture because it is the reason that
/// spends no counter: the cap refused the lane, so the drive comes back to
/// exactly where it was, at exactly the head it was at.
///
/// **Not every reason repeats with nothing new to say**, and the sibling
/// `a_time_bound_hold_announces_every_time_because_its_line_carries_the_time`
/// is the other half: `state-stalled` and `drive-stalled` report a duration,
/// so an identical key there is a fresh stall. This test is what says the
/// exception stayed narrow.
///
/// Both halves are asserted: the pane got ONE line, and the log carries the
/// second with its text — a suppression with no record of what was suppressed
/// is a line an operator cannot get back.
///
/// **Why this fixture still dedups after rev-final round 3.** The key digests
/// the rendered line, and `cap-full`'s two lines here are identical: the cap
/// refuses with the same message both times, so the drive genuinely has
/// nothing new to say. Its sibling
/// `a_repeat_whose_refusal_changed_is_not_the_same_hold` is the other half —
/// vary the refusal and the same reason at the same head announces.
#[test]
fn a_re_hold_on_the_same_reason_and_head_is_announced_once() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, session) = cap_starved_session(&reg, &repo, &gh);

    let at = 20_000;
    let first = reg.rd_drive_group_with(&group, &gh, at + reviewdrive::CAP_HOLD_MS);
    assert_eq!(status_state(&reg, &group), "held", "the fixture's premise: it parked");
    assert_eq!(
        first.notices.iter().filter(|n| n.contains("HELD")).count(),
        1,
        "the FIRST hold is announced — the control for the silence below: {:?}",
        first.notices
    );

    // A plain resume: no reset, no push, nothing the drive can see. It starves
    // again and parks again, at the same head, on the same reason.
    let resumed_at = at + reviewdrive::CAP_HOLD_MS + 1_000;
    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", resumed_at);
    assert_eq!(out["driving"], json!(true), "the resume must succeed: {out}");
    let second = starve_again(&reg, &group, &gh, resumed_at);
    assert_eq!(status_state(&reg, &group), "held", "it really did park a second time");

    assert!(
        second.notices.iter().all(|n| !n.contains("HELD")),
        "the second hold says exactly what the first did: {:?}",
        second.notices
    );
    assert_eq!(
        audit_details(&reg, &group, "rd-held").len(),
        2,
        "the hold HAPPENED both times, and §5.4 records what happened"
    );
    let repeated = audit_details(&reg, &group, "rd-hold-repeated");
    assert_eq!(repeated.len(), 1, "the suppression is audited: {repeated:?}");
    assert_eq!(repeated[0]["reason"], json!("cap-full"));
    assert!(
        repeated[0]["notice"].as_str().unwrap_or_default().contains("HELD"),
        "with the line it did not send: {repeated:?}"
    );
}

/// **The negative control, in both directions a resume can re-arm.**
///
/// `head`: a resume after the worker pushed is a hold about a different
/// revision, and it is the case an orchestrator most needs to see. Drop `head`
/// from `reviewdrive::hold_key` and this arm reddens — the second hold is then
/// suppressed as a repeat of a hold about code that no longer exists.
///
/// `reset-counters`: the counter VALUES cannot see a spent round, because a
/// reset puts them back where the previous hold found them and the next hold
/// fires at the same bound. `DriveEntry::rearm_hold_notice` is what closes
/// that, and deleting its call reddens this arm.
#[test]
fn a_resume_re_arms_the_hold_notice() {
    for arm in ["head-moved", "reset-counters"] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, session) = cap_starved_session(&reg, &repo, &gh);

        let at = 20_000;
        let first = reg.rd_drive_group_with(&group, &gh, at + reviewdrive::CAP_HOLD_MS);
        assert_eq!(status_state(&reg, &group), "held", "{arm}: the fixture parked");
        assert_eq!(
            first.notices.iter().filter(|n| n.contains("HELD")).count(),
            1,
            "{arm}: the first hold is announced: {:?}",
            first.notices
        );

        let resumed_at = at + reviewdrive::CAP_HOLD_MS + 1_000;
        let reset = arm == "reset-counters";
        if arm == "head-moved" {
            // The worker pushed while the drive was parked.
            gh.set_facts("OPEN", HEAD_B);
            reg.set_pr_head_override(Some(HEAD_B.to_string()));
        }
        let out =
            reg.drive_review_with(&group, &gh, 1758, &session, reset, 0, "orch-1", resumed_at);
        assert_eq!(out["driving"], json!(true), "{arm}: the resume must succeed: {out}");
        let second = starve_again(&reg, &group, &gh, resumed_at);
        assert_eq!(status_state(&reg, &group), "held", "{arm}: it parked again");

        assert_eq!(
            second.notices.iter().filter(|n| n.contains("HELD")).count(),
            1,
            "{arm}: this hold is not the one already announced, so it announces: {:?}",
            second.notices
        );
        assert!(
            audit_details(&reg, &group, "rd-hold-repeated").is_empty(),
            "{arm}: and nothing was suppressed"
        );
    }
}

/// [`cap_starved`] plus the worker session, for the two dedup tests: they resume
/// the drive, which needs the session `drive_review` was pointed at.
///
/// A separate helper rather than a wider return on `cap_starved`, so the two
/// #2109 tests that use that one keep their signature and their meaning.
fn cap_starved_session(reg: &OrchRegistry, repo: &Repo, gh: &FakeGh) -> (GroupId, String) {
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
    assert!(first.lanes_opened.is_empty(), "the premise: the cap is full, so nothing spawned");
    (group, session)
}

/// Walk a RESUMED cap-starved drive back to its next `held(cap-full)`.
///
/// **Two ticks before the window, not one, and the refusal is asserted.** A
/// resume lands the drive in `ci-wait`, so the first tick is arc 2 and the
/// SECOND is the one that tries a lane and is refused — and the starvation run
/// `cap-full` fires on is measured from that tick, because `advance` clears the
/// stamp on the arc out of `held`. Counting from the resume instead left the
/// drive in `review-wait` one tick short of its own bound, which reads as "the
/// dedup swallowed the hold" and is nothing of the kind.
fn starve_again(
    reg: &OrchRegistry,
    group: &GroupId,
    gh: &FakeGh,
    resumed_at: u64,
) -> RdDriveReport {
    reg.rd_drive_group_with(group, gh, resumed_at + 1_000); // ci-wait -> review-wait
    let refused = reg.rd_drive_group_with(group, gh, resumed_at + 2_000); // spawn refused
    assert!(
        refused.lanes_opened.is_empty(),
        "the premise: the cap is still full, so the starvation run really restarts here"
    );
    reg.rd_drive_group_with(group, gh, resumed_at + 2_000 + reviewdrive::CAP_HOLD_MS)
}

/// **A time-bound hold announces every time, because its line says something
/// new every time** (rev-std round 1 on #3040 N1).
///
/// The dedup rests on a claim — a hold with the same reason at the same head
/// with the same counters spent says exactly what the last one said — and for
/// `state-stalled` and `drive-stalled` that claim is false. Their notices carry
/// a DURATION, and the duration is precisely what changed: `advance` re-stamps
/// `state_since_ms` on the arc out of `held`, so a second one means the drive
/// sat out its whole bound again. Suppressing it would hide a fresh stall
/// behind an old one.
///
/// **The key really is identical**, and that is asserted rather than assumed —
/// otherwise this test would pass under a dedup that simply never fired here,
/// which is the thing it is meant to discriminate.
#[test]
fn a_time_bound_hold_announces_every_time_because_its_line_carries_the_time() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    // CI that never resolves — the wait `ci-wait` is for.
    gh.set_checks(r#"[{"name":"build","state":"IN_PROGRESS","link":"x"}]"#);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    let (group, session) = driven(&reg, &repo, &gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7001);

    let bound =
        reviewdrive::state_bound_ms(reviewdrive::DriveState::CiWait, &DriveLimits::default(), 0)
            .expect("a working state has a bound");
    let first = reg.rd_drive_group_with(&group, &gh, bound);
    assert_eq!(status_state(&reg, &group), "held", "the fixture's premise: it parked");
    assert_eq!(
        reg.review_drive_status(&group)["drives"][0]["held_reason"],
        json!("state-stalled"),
        "…on the state clock"
    );
    assert_eq!(
        first.notices.iter().filter(|n| n.contains("HELD")).count(),
        1,
        "the first hold is announced: {:?}",
        first.notices
    );

    // A plain resume: no reset, no push. The drive goes back to waiting on the
    // same never-resolving checks, at the same head, and parks again.
    let resumed_at = bound + 1_000;
    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", resumed_at);
    assert_eq!(out["driving"], json!(true), "the resume must succeed: {out}");
    let second = reg.rd_drive_group_with(&group, &gh, resumed_at + bound);
    assert_eq!(status_state(&reg, &group), "held", "it parked a second time");

    let held = audit_details(&reg, &group, "rd-held");
    assert_eq!(held.len(), 2, "two holds: {held:?}");
    assert_eq!(held[0]["reason"], json!("state-stalled"));
    assert_eq!(held[1]["reason"], json!("state-stalled"));
    assert_eq!(
        held[0]["head"], held[1]["head"],
        "the discriminator: the KEY is identical across the two, so a dedup that saw only \
         the key would suppress the second — this test is about the exception, not about \
         a dedup that never fired: {held:?}"
    );

    assert_eq!(
        second.notices.iter().filter(|n| n.contains("HELD")).count(),
        1,
        "…and it is announced anyway, because the line carries a duration the key cannot \
         see: {:?}",
        second.notices
    );
    assert!(
        audit_details(&reg, &group, "rd-hold-repeated").is_empty(),
        "nothing was suppressed"
    );
    let n = second
        .notices
        .iter()
        .find(|n| n.contains("HELD"))
        .expect("the second hold's own line");
    assert!(
        n.contains("It was in ci-wait for"),
        "and that duration is what it says: {n}"
    );
}
