//! The CONFLICTING arc through the seam, the rebase hand-back count, and #3176's lane release on a conflict.
//!
//! One module of the `reviewdrive` integration-test target (`main.rs`),
//! split out of the former single-file `tests/reviewdrive.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ── the CONFLICTING arc, through the seam (#1862) ────────────────────────────
//
// Everything below reaches `CiObservation::Conflicting` by giving `FakeGh` a
// non-clean `mergeStateStatus` and letting the real `observe_pr` classify it.
// None of it hands a `DriveFacts` to `decide`: that construction is what pinned
// this arc before, and it is the construction #1841's B1 — the driven
// reviewer's `report(approved)` read as a worker finishing — was green under
// through two clean review passes. The arc it leaves untested is the one that
// fires whenever the default branch moves under a driven PR, on a budget of one.

/// **`observe_pr` classifies the mergeability, and SKIPS the second call.**
///
/// The two halves share one fixture and differ in one field. The canned
/// `gh pr checks` payload stays **green for the whole test**, which is what makes
/// the operands collide: after the mergeability flips, everything asserted below
/// is decided by that field alone. An `observe_pr` that read checks first, or
/// that never classified the mergeability JSON at all, reads `SUCCESS` here and
/// lands the drive in `review-wait` — failing every assertion rather than
/// passing vacuously. The `CLEAN` half is the positive control for the skip: it
/// establishes that this fake DOES answer `gh pr checks` and that the driver
/// DOES ask, so the absence below is a skip rather than a fake that was never
/// wired.
///
/// The skip is not an optimisation with a fallback. GitHub creates no check
/// suite for a PR with no clean merge ref, so `gh pr checks` on a conflicting PR
/// sits at "no checks reported" — `Pending` — forever, which is why the
/// mergeability is read FIRST. Reading it second would make every conflict look
/// like a slow build until `drive_timeout_minutes` ended the drive with the
/// wrong reason.
#[test]
fn a_conflicting_pr_is_classified_through_the_seam_and_the_checks_call_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _session) = driven(&reg, &repo, &gh);

    // ── CLEAN, green checks: arc 2, and the control for the skip ────────────
    reg.rd_drive_group_with(&group, &gh, 10_000);
    assert_eq!(status_state(&reg, &group), "review-wait", "a CLEAN + green tick is arc 2");
    assert!(
        gh.checks_calls() > 0,
        "the positive control: a CLEAN tick DOES spend the second call, so its absence below \
         is a skip and not a fake that never answers `checks`"
    );

    // ── CONFLICTING, with the SAME green checks payload ─────────────────────
    gh.set_merge_state("CONFLICTING");
    gh.set_facts("OPEN", HEAD_B);

    // **One tick, not two, since #2311.** The head moved as well, so this used
    // to take arc 6 back to `ci-wait` and read the conflict on the tick after.
    // `decide` now reads mergeability above the per-state logic, so the arc-6
    // return is never taken and the hand-back happens on THIS tick — same
    // destination, same counter, one tick and one `gh` round-trip earlier.
    let checks_before = gh.checks_calls();
    let audits_before = audit_actions(&reg, &group).len();
    let report = reg.rd_drive_group_with(&group, &gh, 20_000);
    assert_eq!(
        gh.checks_calls(),
        checks_before,
        "`observe_pr` must SKIP `gh pr checks` when the first read says CONFLICTING — the \
         answer is already known and GitHub has no suite to report"
    );
    assert_eq!(
        status_state(&reg, &group),
        "fix-wait",
        "arc 3: a conflict is a hand-back for a rebase — taken here rather than after the \
         arc-6 return to `ci-wait` this moved head would otherwise have caused"
    );

    let mut all = audit_actions(&reg, &group);
    let after = all.split_off(audits_before);
    assert!(
        after.iter().any(|a| a == "rd-conflicting"),
        "the tick that classified the conflict must say so in the audit: {after:?}"
    );
    assert!(
        !after.iter().any(|a| a == "rd-ci-red" || a == "rd-ci-green"),
        "…and must not ALSO report a check result it never read: {after:?}"
    );

    // The budget spent is the REBASE one. A conflict misclassified as a red run
    // reaches `fix-wait` too, so the state alone does not discriminate between
    // the two arcs — the counter does.
    let s = reg.review_drive_status(&group);
    assert_eq!(s["drives"][0]["counters"]["rebase_attempts"], json!(1), "{s}");
    assert_eq!(
        s["drives"][0]["counters"]["ci_attempts"],
        json!(0),
        "a conflict must not spend a CI attempt: the two budgets are separate because a \
         rebase and a failing build are different work, and one of them is spendable once: {s}"
    );
    assert!(
        report.handbacks.first().is_some(),
        "the conflict hands the PR back to a worker, which is the whole point of the arc"
    );
}

/// **A mergeability that is neither `CLEAN` nor `CONFLICTING` is not a conflict**
/// — and is not treated as one.
///
/// `pr_mergeability_result` short-circuits on the literal `CONFLICTING` and on
/// nothing else, deliberately: `BEHIND`, `BLOCKED`, `UNSTABLE`, `DRAFT` and
/// `UNKNOWN` are all states in which GitHub still runs checks, so the second
/// call is the answer and taking the conflict arc on one of them would spend the
/// single rebase attempt on a PR with nothing to rebase.
///
/// This is the negative control for the test above. Without it, "classify every
/// non-CLEAN mergeability as a conflict" passes there, and the arc would fire on
/// the several ordinary states a driven PR passes through.
#[test]
fn a_mergeability_that_is_merely_not_clean_is_not_a_conflict() {
    for state in ["BEHIND", "BLOCKED", "UNSTABLE", "UNKNOWN"] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, _session) = driven(&reg, &repo, &gh);

        gh.set_merge_state(state);
        reg.rd_drive_group_with(&group, &gh, 10_000);

        assert_eq!(
            status_state(&reg, &group),
            "review-wait",
            "{state}: the checks are green and this state is not a conflict, so arc 2 is what \
             answers"
        );
        assert!(
            gh.checks_calls() > 0,
            "{state}: the second call must still be made — it is the answer here"
        );
        let s = reg.review_drive_status(&group);
        assert_eq!(
            s["drives"][0]["counters"]["rebase_attempts"],
            json!(0),
            "{state}: no rebase attempt may be spent on a state that is not a conflict: {s}"
        );
    }
}

/// **The conflict hand-back tells the worker to rebase, and says the budget is
/// one.**
///
/// `{{WHAT}}` is loomux-authored text chosen from a closed set of three, and the
/// conflict arm is the one no test rendered: `no_placeholder_survives_into_a_brief`
/// covers the first-call and ci-red arms, and this is the third.
///
/// The three arms are pinned as **mutually exclusive** rather than one at a
/// time. A `rd_fix_brief` that fell through to the review-findings arm renders a
/// brief that is well-formed, carries no placeholder, and tells the worker to go
/// read findings that do not exist — which is exactly the silent wrong-brief the
/// arms exist to prevent.
///
/// The attempt line matters on its own. A worker told "attempt 1 of 3" on a
/// budget of one would reasonably push a partial resolution and expect two more
/// tries; there are none, and the next conflict is a hold rather than a retry.
#[test]
fn a_conflict_hand_back_briefs_the_rebase_and_names_the_single_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    // CONFLICTING from the first tick, so `ci-wait` takes arc 3 before any lane
    // has opened and the brief under test is the only thing that happened.
    gh.set_merge_state("CONFLICTING");
    let (group, _session) = driven(&reg, &repo, &gh);

    let report = reg.rd_drive_group_with(&group, &gh, 10_000);
    assert_eq!(status_state(&reg, &group), "fix-wait", "arc 3 on the first tick");
    let (_pr, worker) = report
        .handbacks
        .first()
        .cloned()
        .expect("the conflict hand-back resumed a worker pane");
    let fix = lane_brief(&reg, &worker);

    assert!(!fix.contains("{{"), "an unregistered placeholder survived into a fix brief: {fix}");
    assert!(
        fix.contains("It is CONFLICTING against main."),
        "the brief must name the base it conflicts against, which is the fact the worker acts \
         on: {fix}"
    );
    assert!(
        fix.contains("Rebase onto origin/main, resolve, and push."),
        "…and the instruction itself, naming the remote ref rather than a local branch that \
         may be stale: {fix}"
    );
    assert!(
        fix.contains("This is attempt 1 of 1."),
        "the rebase budget is ONE, and the brief must say so — a worker told it has three \
         would reasonably push a partial resolution: {fix}"
    );

    // The other two arms are ABSENT. This is what makes the assertions above
    // about the conflict arm rather than about a template that renders every
    // sentence it has.
    assert!(
        !fix.contains("CI is red at that head"),
        "the ci-red arm must not render on a conflict: {fix}"
    );
    assert!(
        !fix.contains("Review requested changes"),
        "…nor the review-findings arm, which would send the worker to findings that do not \
         exist: {fix}"
    );

    // One paragraph, as the lane briefs are. The conflict sentence carries two
    // clauses across a `\` continuation in the source and would ship the indent
    // between them if one ever collapsed.
    let what = fix
        .lines()
        .find(|l| l.starts_with("It is CONFLICTING"))
        .unwrap_or_else(|| panic!("the conflict arm rendered on its own line: {fix}"));
    assert!(
        what.contains("Rebase onto origin/main") && !what.contains("          "),
        "the conflict sentence must arrive as one whole paragraph on one line: {what:?}"
    );
}

/// **The single rebase attempt is spent, and the second conflict is a HOLD.**
///
/// §2.2's `rebase-limit` row is *"a second conflict after the one rebase
/// hand-back"*, and the budget cannot be spent twice — so a mistake on this arc
/// is not a retry, it is a park that waits for a human.
///
/// **The first half is the positive control for the second.** The drive
/// demonstrably TAKES the hand-back and demonstrably spends the counter, so the
/// hold that follows is exhaustion rather than a refusal from the start: an
/// implementation that held on the FIRST conflict — never spending the attempt
/// at all — fails the first half, and one that never held fails the second.
/// Without the first half, `counter_exhausted`'s check-before-bump ordering
/// could be inverted and this test would not notice.
#[test]
fn a_second_conflict_after_the_one_rebase_hand_back_holds_on_rebase_limit() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    gh.set_merge_state("CONFLICTING");
    let (group, _session) = driven(&reg, &repo, &gh);

    // ── the first conflict: the attempt is SPENT ────────────────────────────
    reg.rd_drive_group_with(&group, &gh, 10_000);
    assert_eq!(status_state(&reg, &group), "fix-wait", "the first conflict hands back");
    let s = reg.review_drive_status(&group);
    assert_eq!(
        s["drives"][0]["counters"]["rebase_attempts"],
        json!(1),
        "the one attempt must be SPENT here, or the hold below is a refusal rather than an \
         exhaustion: {s}"
    );

    // The worker pushes its resolution — arc 7, on the head moving.
    gh.set_facts("OPEN", HEAD_B);
    reg.rd_drive_group_with(&group, &gh, 20_000);
    assert_eq!(status_state(&reg, &group), "ci-wait", "arc 7: the worker pushed");

    // ── and it still conflicts ──────────────────────────────────────────────
    let report = reg.rd_drive_group_with(&group, &gh, 30_000);
    assert_eq!(status_state(&reg, &group), "held", "the second conflict has no attempt to spend");
    let s = reg.review_drive_status(&group);
    assert_eq!(s["drives"][0]["held_reason"], json!("rebase-limit"), "{s}");
    assert_eq!(
        s["drives"][0]["counters"]["rebase_attempts"],
        json!(1),
        "the hold must not spend an attempt it does not have — the counter stays at its \
         bound rather than passing it: {s}"
    );

    let notice = report
        .notices
        .iter()
        .find(|n| n.contains("rebase"))
        .or_else(|| report.notices.first())
        .unwrap_or_else(|| panic!("a hold must deliver its notice: {:?}", report.notices));
    assert!(
        notice.contains("still CONFLICTING"),
        "the notice names the fact that decides what the orchestrator does next: {notice}"
    );
    assert!(
        notice.contains("cancel_review_drive"),
        "…and the tool that acts on it, since a compacted orchestrator reading this line must \
         not have to remember the API: {notice}"
    );
}

// ── #1863 D1: the count and its own closing sentence ─────────────────────────

/// **A closed refusal list and the sentence that closes it state the SAME
/// number.**
///
/// `queue_merge`'s description opens *"FIVE FURTHER REASONS MEAN LOOMUX ITSELF
/// FAILED"*, enumerates them, and used to close twenty words later, in the same
/// sentence-group, with *"None of the four should appear in a running build."*
/// The count went to five when the review driver's own `rd-state-unreadable`
/// joined the list; three instances were corrected and the fourth — the one
/// nearest a corrected one — was not (#1863 D1).
///
/// It lives in this file rather than in `tests/mergequeue.rs` because the fifth
/// reason is the driver's, and the miss was the driver's PR.
///
/// **The assertion is AGREEMENT, not a literal.** Pinning "five" would go stale
/// the moment a sixth reason is added, in the same direction as the defect: the
/// test would then be enforcing a wrong number rather than catching one. What
/// cannot go stale is that the opening word and the closing word are two
/// statements of one fact.
///
/// The third assertion cross-checks both against the list itself, and its
/// delimiter is stated rather than assumed: every one of these five reasons
/// carries a parenthesised gloss, so <code>` (</code> counts each exactly once.
/// A reason added WITHOUT a gloss makes this count wrong and the test red, which
/// is the direction to fail in — the alternative is a census that cannot see one
/// of its own subjects and reports a smaller number with no sign it did. The
/// sibling clause in `drive_review`'s description is deliberately NOT covered
/// here for exactly that reason: its `rd-unavailable` carries no gloss, so this
/// delimiter would silently under-count it.
#[test]
fn queue_merges_failure_list_agrees_with_both_numbers_that_describe_it() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = Caller {
        agent_id: orch.id.clone(),
        group: group.clone(),
        role: Role::Orchestrator,
        role_hint: None,
    };

    let listed = dispatch(&reg, &co, "tools/list", &json!({})).unwrap();
    let desc = listed["tools"]
        .as_array()
        .expect("tools/list answers an array")
        .iter()
        .find(|t| t["name"] == json!("queue_merge"))
        .and_then(|t| t["description"].as_str())
        .expect("the orchestrator is offered queue_merge")
        .to_string();

    let (before, clause) = desc
        .split_once(" FURTHER REASONS MEAN LOOMUX ITSELF FAILED")
        .expect("the description opens its failure list with a count");
    let opener = before.rsplit(' ').next().unwrap_or_default().to_ascii_lowercase();
    let (listed_text, _) = clause
        .split_once(" should appear in a running build")
        .expect("…and closes it with a second count");
    let closer = listed_text.rsplit(' ').next().unwrap_or_default().to_ascii_lowercase();

    // The positive control. Both words must have been READ — an empty string
    // equals an empty string, and a parse that found nothing would otherwise
    // satisfy the agreement below without having looked at anything.
    let number = |w: &str| -> Option<usize> {
        ["zero", "one", "two", "three", "four", "five", "six", "seven", "eight"]
            .iter()
            .position(|c| *c == w)
    };
    let n_open = number(&opener)
        .unwrap_or_else(|| panic!("the opening count is not a number word: {opener:?}"));
    let n_close = number(&closer)
        .unwrap_or_else(|| panic!("the closing count is not a number word: {closer:?}"));

    assert_eq!(
        n_open, n_close,
        "the two counts describing one list disagree — it opens {opener:?} and closes \
         {closer:?}, twenty words apart in the same sentence-group"
    );

    // …and both against the list they describe.
    let enumerated = listed_text.matches("` (").count();
    assert_eq!(
        enumerated, n_open,
        "the count and the enumeration disagree: {n_open} claimed, {enumerated} reasons \
         parsed. If a reason was just added WITHOUT a parenthesised gloss, this delimiter \
         cannot see it — fix the delimiter here rather than the number"
    );
}

// ── #3176: the CONFLICTING transition releases open reviewer lanes ──────────

/// **An open lane on a PR that has gone CONFLICTING is released, and the round
/// it was reviewing is not charged for** (#3176).
///
/// The defect this closes is a whole review round spent on nothing. #2311 hoisted
/// the conflict read above the per-state logic, so a drive whose base moved under
/// an open lane now hands the worker back for a rebase instead of holding
/// `routing-unaccountable` — but the lane the drive had already spawned was left
/// running, reviewing a head the rebase is about to replace. Whatever it
/// concluded bound to that head, `lane_verdict_is_current` read it as stale the
/// moment the worker pushed, and the drive re-briefed the same lane at the new
/// head. Measured on #3150, which paid a `rev-std` pass exactly that way.
///
/// **The two arms differ in ONE fact — the mergeability — and everything else in
/// the fixture is identical**, so the release is attributable to the conflict and
/// not to a walk that happened to end somewhere else. Without the CLEAN arm
/// "the lane was released" is satisfied by any implementation that releases an
/// idle lane whenever it likes, and "no review round was spent" is satisfied by a
/// drive that never left `review-wait`.
///
/// Six things are asserted together because each alone has a passing
/// implementation that is wrong: the RELEASE (the pane really went, through the
/// barrier), the REASON (`conflict`, not one of the three words that claim a
/// finished review — an audit reason is a claim), the COUNTERS (`rebase_attempts`
/// says the conflict arc was taken, `review_rounds` says nothing was billed for
/// it), the SESSION (kept, or the release costs the review rather than a slot),
/// and the RECORD (the pane forgotten, and the revision key with it, so the next
/// round is an ordinary re-brief rather than a wait on a lane with no pane).
#[test]
fn a_conflict_releases_the_open_lane_it_was_about_to_strand() {
    for (arm, conflicting) in [("CONFLICTING", true), ("CLEAN", false)] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, lane) = briefed(&reg, &repo, &gh);
        reg.set_pr_body_override(Some("b".to_string()));
        reg.set_pr_head_override(Some(HEAD_A.to_string()));

        // The lane ends its turn WITHOUT recording anything — the shape the issue
        // is about, and the one `releasable`'s other three reasons cannot reach:
        // there is no verdict, no consumed report and no terminal step.
        //
        // `blocked` and not `progress`, and the difference is the whole fixture:
        // `set_agent_idle` is called with `matches!(status, "done" | "blocked")`,
        // so a `progress` report leaves `idle_since_ms` unset and the release
        // barrier refuses the pane — which is a fact about the reviewer still
        // being mid-turn, not about the rule. A LANE's report carries no drive
        // signal either way (mcp.rs: what a lane says to the drive is its verdict
        // FILE), so this ends the turn and moves nothing.
        report_as(&reg, &group, &lane, Role::Reviewer, "blocked");
        let session_before = live_lanes(&reg, &group)
            .first()
            .and_then(|l| l["session"].as_str().map(str::to_string))
            .unwrap_or_default();
        assert!(
            !session_before.is_empty(),
            "{arm}: the fixture's premise: the lane has a session to keep"
        );
        assert_eq!(
            live_lanes(&reg, &group).first().and_then(|l| l["briefed_head"].as_str()),
            Some(HEAD_A),
            "{arm}: …and it was briefed at this head"
        );

        // The ONE thing that differs between the two runs.
        if conflicting {
            gh.set_merge_state("CONFLICTING");
        }
        let out = reg.rd_drive_group_with(&group, &gh, 30_000);

        let released: Vec<String> = out.released.iter().map(|(_, _, a)| a.clone()).collect();
        let rows = audit_details(&reg, &group, "rd-lane-released");
        let dead = reg.agent(&lane).map(|a| a.status == AgentStatus::Dead).unwrap_or(false);
        let rec = live_lanes(&reg, &group).first().cloned().unwrap_or_default();
        let s = reg.review_drive_status(&group);

        assert_eq!(
            released,
            if conflicting { vec![lane.clone()] } else { vec![] },
            "{arm}: the pane the driver freed"
        );
        assert_eq!(dead, conflicting, "{arm}: …and its liveness");
        assert_eq!(rows.len(), usize::from(conflicting), "{arm}: rd-lane-released rows {rows:?}");
        assert_eq!(
            s["drives"][0]["counters"]["review_rounds"],
            json!(0),
            "{arm}: no lane delivered any findings, so no review round is billed — the half of \
             #3176 that says the round is not spent: {s}"
        );

        if conflicting {
            let row = &rows[0];
            assert_eq!(row["reason"], json!("conflict"), "{arm}: {row}");
            assert_eq!(row["block"], json!("rev-std"), "{arm}: {row}");
            assert_eq!(row["agent"], json!(lane), "{arm}: {row}");
            assert_eq!(row["head"], json!(HEAD_A), "{arm}: the head it was released at: {row}");
            assert_eq!(
                row["session"].as_str().unwrap_or_default(),
                session_before,
                "{arm}: the conversation survives, so the re-brief at the rebased head resumes \
                 the reviewer that has already read this PR: {row}"
            );
            assert_eq!(
                status_state(&reg, &group),
                "fix-wait",
                "{arm}: …and the drive still takes #2311's rebase arc: {s}"
            );
            assert_eq!(
                s["drives"][0]["counters"]["rebase_attempts"],
                json!(1),
                "{arm}: the REBASE budget is what a conflict spends: {s}"
            );
            assert_eq!(
                rec["session"].as_str().unwrap_or_default(),
                session_before,
                "{arm}: the record keeps the session: {rec}"
            );
            assert!(
                rec["agent"].as_str().unwrap_or_default().is_empty(),
                "{arm}: …and stops naming a pane that is gone: {rec}"
            );
            assert_eq!(
                rec["briefed_head"].as_str().unwrap_or_default(),
                "",
                "{arm}: …and forgets the revision it was briefed at. A plain release leaves this \
                 standing, and `decide_review_wait`'s wait arm then reads the lane as open at \
                 this revision with a pane that is not dead — `pane_dead` is derived from the \
                 recorded pane, which the release just emptied — so the drive would wait out \
                 `state-stalled` for a verdict no pane can produce: {rec}"
            );
        } else {
            // The control, which is also the negative one for the reason: a CLEAN
            // PR at exactly this point releases nothing at all and stays in
            // `review-wait` waiting on the lane it has open.
            assert_eq!(
                status_state(&reg, &group),
                "review-wait",
                "{arm}: nothing moved this drive: {s}"
            );
            assert_eq!(s["drives"][0]["counters"]["rebase_attempts"], json!(0), "{arm}: {s}");
            assert_eq!(
                rec["agent"].as_str().unwrap_or_default(),
                lane,
                "{arm}: the record still names the live pane: {rec}"
            );
        }
    }
}

/// **A lane that has ALREADY answered at this head is not released as a
/// conflict** (#3176) — the carve-out, performed rather than described.
///
/// An audit reason is a claim, and `conflict` claims a review that was thrown
/// away. A lane whose verdict is on durable record at this head threw nothing
/// away, so neither arm may touch it: it is not told to stop (it is not
/// mid-review) and it is not released as a `conflict` (it is
/// `verdict-recorded`'s, or the ordinary stale-verdict handling's).
///
/// **The two arms differ in exactly one thing — whether a verdict was recorded
/// — and the unanswered one is the positive control.** Without it "no rows"
/// is satisfied by any fixture that never reached the conflict at all, and by an
/// implementation that does nothing whatsoever.
///
/// # The fixture, and the two ways an earlier draft of it proved nothing
///
/// The lane stays **mid-turn** throughout: `record_pass_for` writes the verdict
/// file and does not stamp `idle_since_ms`, and no `report` follows it. That is
/// load-bearing twice over. It keeps `release_driven_pane` refusing, so the
/// answered lane's pane survives the pre-conflict tick with its record intact —
/// a draft that let condition 2 release it there had no pane left for the
/// carve-out to decline, and the assertion passed against a lane that was simply
/// gone. And it routes the counterfactual through the STOP arm, which is where
/// a missing carve-out is actually visible: drop it and the answered lane is
/// told to stand down from a review it has already delivered.
///
/// The live body is moved out from under the recorded pass so condition 2 cannot
/// answer either — `gh.set_body`, not `set_pr_body_override`. The override is
/// what a verdict binds to when it is RECORDED, which is already past; the
/// fake's body is what the drive reads live. A draft that moved the override
/// moved neither, `lane_verdict_is_current` stayed true, and condition 2
/// released the lane before the carve-out was reached.
///
/// Rows are counted from a SNAPSHOT taken before the conflict tick. `audit_log`
/// is the whole group's history, so a bare `audit_details` here answers for
/// every tick the fixture walked through — which is how the first draft read a
/// release from the setup as if the conflict had produced it.
#[test]
fn a_lane_that_answered_at_this_head_is_never_released_as_a_conflict() {
    for (arm, answered) in [("answered", true), ("never answered", false)] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, lane) = briefed(&reg, &repo, &gh);
        reg.set_pr_body_override(Some("b".to_string()));
        reg.set_pr_head_override(Some(HEAD_A.to_string()));
        make_delivery_land(&reg, &group, &lane, 4322);

        if answered {
            record_pass_for(&reg, &group, &lane);
            // The drive must have READ that verdict before the conflict lands,
            // or the record carries nothing to distinguish this lane from an
            // unanswered one and the test would pass for the wrong reason.
            reg.rd_drive_group_with(&group, &gh, 25_000);
            assert_eq!(
                live_lanes(&reg, &group).first().and_then(|l| l["at_head"].as_str()),
                Some(HEAD_A),
                "{arm}: the fixture's premise: the drive has recorded this lane's answer"
            );
            // …and the live body moves, so `lane_verdict_is_current` is false
            // and condition 2 cannot be what declines this lane.
            gh.set_body("b — and one more sentence");
        }
        assert!(
            reg.agent(&lane).expect("the lane is on the roster").idle_since_ms.is_none(),
            "{arm}: the fixture's premise: the reviewer is mid-turn in BOTH arms"
        );
        assert_eq!(
            live_lanes(&reg, &group).first().and_then(|l| l["agent"].as_str()),
            Some(lane.as_str()),
            "{arm}: …so its pane is still on the record, whatever happened before"
        );

        // Everything above is setup; only what follows is this test's subject.
        let before = reg.audit_log(&group).len();
        gh.set_merge_state("CONFLICTING");
        reg.rd_drive_group_with(&group, &gh, 30_000);
        let mut log = reg.audit_log(&group);
        let after: Vec<_> = log.split_off(before);
        let rows = |action: &str| -> Vec<serde_json::Value> {
            after.iter().filter(|e| e.action == action).map(|e| e.detail.clone()).collect()
        };
        let stopped = rows("rd-lane-stopped");
        let released = rows("rd-lane-released");

        assert_eq!(
            status_state(&reg, &group),
            "fix-wait",
            "{arm}: the control on the tick itself — this drive really did act on the conflict"
        );
        for row in &released {
            assert_ne!(
                row["reason"],
                json!("conflict"),
                "{arm}: a lane whose verdict is on record threw no review away, so `conflict` \
                 would be a false row on the surface §5.4 asks a reader to count from: {row}"
            );
        }
        assert_eq!(
            stopped.len(),
            usize::from(!answered),
            "{arm}: rd-lane-stopped rows — a lane mid-review is told to stand down, and one \
             whose verdict is already delivered is told nothing: {stopped:?}"
        );
    }
}

/// **A conflict with no open lane releases nothing and still takes the rebase
/// arc** (#3176) — the positive control for the rule's own emptiness.
///
/// "No lane was released" is trivially true of a fixture that never reached a
/// conflict at all, so the arc is asserted beside it: this drive really did
/// observe the conflict, spend `rebase_attempts` and reach `fix-wait`. What it
/// did not do is invent a release for a lane it never opened.
#[test]
fn a_conflict_with_no_open_lane_releases_nothing_and_still_rebases() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _session) = driven(&reg, &repo, &gh);

    // Red CI hands the PR back before any lane has opened, so the drive has a
    // worker pane and no lane record at all.
    gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
    reg.rd_drive_group_with(&group, &gh, 10_000);
    assert_eq!(status_state(&reg, &group), "fix-wait", "the fixture's premise");
    assert!(live_lanes(&reg, &group).is_empty(), "…and no lane has ever been opened");

    // `fix-wait` is the one state #2311 excludes from the conflict read, so the
    // worker pushes: arc 7 takes the drive to `ci-wait`, where the conflict is
    // acted on.
    gh.set_merge_state("CONFLICTING");
    gh.set_facts("OPEN", HEAD_B);
    reg.set_pr_head_override(Some(HEAD_B.to_string()));
    reg.rd_drive_group_with(&group, &gh, 20_000);
    reg.rd_drive_group_with(&group, &gh, 30_000);

    let s = reg.review_drive_status(&group);
    assert_eq!(
        s["drives"][0]["counters"]["rebase_attempts"],
        json!(1),
        "the control: this drive really did act on the conflict: {s}"
    );
    assert!(
        audit_details(&reg, &group, "rd-lane-released").is_empty(),
        "…and released no lane, because it had none"
    );
}

/// **The rule is a standing property of the FACTS, not of the one tick that took
/// the arc** (#3176) — which is the difference between a fix and a coin flip.
///
/// #2311's hoist takes arc 3 on the first tick that observes the conflict, and on
/// that tick the lane's pane is whatever it happened to be doing. A reviewer
/// mid-turn is refused by `release_driven_pane`'s idle barrier — the judgment §3
/// forbids the driver making — so a rule keyed on the STEP would release the pane
/// only when the conflict happened to arrive between the reviewer's turns, and
/// leave it burning the round it was spawned for the rest of the time.
///
/// Both halves are asserted in one walk, and the first is the negative control
/// for the second: the SAME lane and the SAME conflict, released on a later tick
/// once it is idle and not killed on the tick that took the arc.
#[test]
fn a_lane_still_mid_turn_at_the_conflict_is_released_on_the_next_tick_it_is_idle() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, lane) = briefed(&reg, &repo, &gh);
    reg.set_pr_body_override(Some("b".to_string()));
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    assert!(
        reg.agent(&lane).expect("the lane is on the roster").idle_since_ms.is_none(),
        "the fixture's premise: the reviewer has not ended a turn"
    );

    gh.set_merge_state("CONFLICTING");
    reg.rd_drive_group_with(&group, &gh, 30_000);
    assert_eq!(
        status_state(&reg, &group),
        "fix-wait",
        "the arc is taken on this tick whatever the pane is doing"
    );
    assert!(
        audit_details(&reg, &group, "rd-lane-released").is_empty(),
        "…and the busy pane is NOT killed: the barrier refuses it, and a release row is written \
         on the kill succeeding rather than on the intent"
    );
    assert_eq!(
        reg.agent(&lane).map(|a| a.status == AgentStatus::Dead),
        Some(false),
        "…so the reviewer mid-turn is still alive"
    );

    // The reviewer ends its turn — `blocked`, because that and `done` are the
    // only words `set_agent_idle` treats as the end of one. Nothing else about
    // the world has changed: the PR is still CONFLICTING, the drive is still
    // waiting out the same rebase, and a lane's report carries no drive signal.
    report_as(&reg, &group, &lane, Role::Reviewer, "blocked");
    reg.rd_drive_group_with(&group, &gh, 40_000);

    let rows = audit_details(&reg, &group, "rd-lane-released");
    assert_eq!(rows.len(), 1, "the later tick releases it: {rows:?}");
    assert_eq!(rows[0]["reason"], json!("conflict"), "{:?}", rows[0]);
    assert_eq!(rows[0]["agent"], json!(lane), "{:?}", rows[0]);
}

/// **A busy open lane is TOLD to stop, exactly once, and a lane that has
/// already recorded is told nothing** (#3176) — the arm that saves the round
/// rather than the slot.
///
/// The release arm cannot reach a reviewer mid-review: `release_driven_pane`
/// refuses a pane that is not idle, which is §3 forbidding the driver to kill a
/// pane mid-turn, and a reviewer is idle only after it reports. So on the case
/// the issue is actually about — a lane spawned, working, and reading a head the
/// rebase is about to replace — a release-only fix does nothing at all. This is
/// the other half: one QUEUED delivery into that lane's own pane
/// (`Delivery::MidSession` — the mechanism `rd_reuse_pane` types a re-brief
/// with, never an interrupt), telling it to stand down and report.
///
/// Five things are asserted. **Exactly one delivery across two ticks**, on a
/// second tick where nothing about the world changed. **The `stopped_head` mark
/// on the record**, which is what the next tick and the next process read.
/// **None to a lane that has already recorded a verdict** — it is not
/// mid-review and has nothing to stand down from, and telling it would be the
/// same false claim the `conflict` release reason avoids. **The pane is still
/// alive**: this arm delivers, it does not kill. And **the line itself**, read
/// off the pane's own audit trail rather than off the renderer — a test that
/// called `rd_lane_stop_brief` directly would pass just as well if nothing were
/// ever delivered.
///
/// **What this test does NOT pin, stated because a measurement that surprised
/// me is worth more than one that did not.** Deleting the `lane_stopped_at`
/// check reddens nothing here: the second delivery does not duplicate even with
/// the guard gone, for a reason further down the delivery stack this test
/// cannot see. So the row count is the observable PROPERTY and not evidence for
/// the mechanism behind it, and the guard's own coverage is
/// `the_stop_mark_is_per_revision_and_a_reseed_clears_it` in the engine crate,
/// where the question is pure and every answer is reachable. An absence nobody
/// can explain is not evidence for the thing that was supposed to cause it.
#[test]
fn a_busy_lane_on_a_conflicted_pr_is_told_to_stop_once_and_an_answered_one_is_not() {
    for (arm, answered) in [("busy mid-review", false), ("already recorded", true)] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, lane) = briefed(&reg, &repo, &gh);
        reg.set_pr_body_override(Some("b".to_string()));
        reg.set_pr_head_override(Some(HEAD_A.to_string()));
        // A delivery needs a pane and a queue that holds — `pause_with_pane`,
        // the same probe every other delivery test in this file uses.
        make_delivery_land(&reg, &group, &lane, 4321);

        if answered {
            record_pass_for(&reg, &group, &lane);
            reg.rd_drive_group_with(&group, &gh, 25_000);
            assert_eq!(
                live_lanes(&reg, &group).first().and_then(|l| l["at_head"].as_str()),
                Some(HEAD_A),
                "{arm}: the fixture's premise: the drive has read this lane's verdict"
            );
        }
        // Both arms are MID-TURN — `record_pass_for` writes a verdict file and
        // does not stamp `idle_since_ms`, so the difference between the arms is
        // the verdict and nothing else.
        assert!(
            reg.agent(&lane).expect("the lane is on the roster").idle_since_ms.is_none(),
            "{arm}: the fixture's premise: the reviewer has not ended a turn"
        );

        gh.set_merge_state("CONFLICTING");
        reg.rd_drive_group_with(&group, &gh, 30_000);

        // **The mark is on the record**, which is what the next tick and the
        // next PROCESS read. This asserts the WRITE, not the check — see the
        // doc above for what does and does not discriminate here.
        if !answered {
            assert_eq!(
                live_lanes(&reg, &group).first().and_then(|l| l["stopped_head"].as_str()),
                Some(HEAD_A),
                "the stop is recorded against the revision it was about, and persisted, so the next process does not re-send a line this one already sent"
            );
        }

        // A second tick on which NOTHING about the world has changed: still
        // CONFLICTING, still the same busy pane, still the same head.
        reg.rd_drive_group_with(&group, &gh, 40_000);

        let rows = audit_details(&reg, &group, "rd-lane-stopped");
        assert_eq!(
            rows.len(),
            usize::from(!answered),
            "{arm}: rd-lane-stopped rows — exactly one for a lane mid-review across TWO ticks, \
             and none at all for one whose verdict is already on record: {rows:?}"
        );
        assert_eq!(
            reg.agent(&lane).map(|a| a.status == AgentStatus::Dead),
            Some(false),
            "{arm}: this arm DELIVERS; it never kills, and the pane is busy in both arms"
        );

        if !answered {
            let row = &rows[0];
            assert_eq!(row["pr"], json!(1758), "{arm}: {row}");
            assert_eq!(row["block"], json!("rev-std"), "{arm}: {row}");
            assert_eq!(row["agent"], json!(lane), "{arm}: {row}");
            assert_eq!(row["head"], json!(HEAD_A), "{arm}: {row}");
            assert_eq!(row["why"], json!("conflict"), "{arm}: {row}");

            // **The line the reviewer actually receives**, read off the pane's
            // own queue rather than off the renderer — a test that called
            // `rd_lane_stop_brief` itself would pass just as well if nothing
            // were ever delivered.
            let sent = texts_to(&reg, &group, &lane).join("\n---\n");
            assert!(
                sent.contains("STOP this review"),
                "{arm}: the stop line reaches the pane: {sent}"
            );
            assert!(
                sent.contains("call report with outcome done"),
                "{arm}: …and asks for the report that is what makes the pane releasable: {sent}"
            );
            assert!(
                !sent.contains("Review the change on its merits"),
                "{arm}: …and NOT `rd_lane_brief`'s carry-on arm, which would contradict it in \
                 the same paragraph: {sent}"
            );
            assert!(
                !sent.contains('\r'),
                "{arm}: one paragraph, no stray control characters: {sent}"
            );
        }
    }
}

/// **A stop line the pane's queue refuses says so, and the next tick tries
/// again** (#3176) — the mirror of `a_takeover_refused_by_a_full_queue_says_so_
/// and_falls_through`, for the arm this PR adds.
///
/// The refusal is reachable with nothing wrong at all: `deliver_prompt` answers
/// `Err` at `QUEUE_MAX_PER_PANE`, and that is precisely the case where a
/// reviewer goes on burning a round nobody can see it burning. On a silent skip
/// it would be indistinguishable from "there was no busy lane to tell" — the
/// indistinguishability `rd-reuse-declined` and `rd-takeover-declined` were both
/// added to remove.
///
/// **Two depths, and the shallower one is the control.** At 7 the delivery is
/// admitted, so a row is written and the mark is set; at 8 it is refused, so
/// there is no `rd-lane-stopped` row, no mark, and a `rd-lane-stop-declined` row
/// naming the pane instead. Without the depth-7 arm, "no row at 8" is satisfied
/// by a fixture that never reached the conflict.
///
/// **The mark is what makes the retry true**, so it is asserted rather than
/// described: it is written on the delivery SUCCEEDING, so a refused tick leaves
/// the lane tellable and the next tick — with the queue drained — really does
/// tell it.
#[test]
fn a_stop_line_the_queue_refuses_is_audited_and_retried() {
    // (depth, rd-lane-stopped rows, rd-lane-stop-declined rows, mark set)
    type Row = (usize, usize, usize, bool);
    let mut observed: Vec<Row> = Vec::new();

    for depth in [7usize, 8] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, lane) = briefed(&reg, &repo, &gh);
        reg.set_pr_body_override(Some("b".to_string()));
        reg.set_pr_head_override(Some(HEAD_A.to_string()));
        make_delivery_land(&reg, &group, &lane, 7701);

        // Back the LANE's pane up to this arm's depth, through the real
        // admission path — the same way the take-over test does it.
        for k in 0..depth {
            reg.deliver_prompt(&lane, &format!("[test] backlog {k}"), "orch-1", Delivery::MidSession)
                .unwrap_or_else(|e| panic!("depth={depth}: entry {k} must be admitted: {e}"));
        }
        assert_eq!(
            reg.queue_depth(7701),
            depth,
            "depth={depth}: the fixture's premise — the pane is backed up to exactly this depth"
        );

        gh.set_merge_state("CONFLICTING");
        let before = reg.audit_log(&group).len();
        reg.rd_drive_group_with(&group, &gh, 30_000);
        let mut log = reg.audit_log(&group);
        let after: Vec<_> = log.split_off(before);
        let rows = |action: &str| after.iter().filter(|e| e.action == action).count();

        observed.push((
            depth,
            rows("rd-lane-stopped"),
            rows("rd-lane-stop-declined"),
            live_lanes(&reg, &group)
                .first()
                .and_then(|l| l["stopped_head"].as_str())
                .is_some_and(|h| h == HEAD_A),
        ));
    }

    assert_eq!(
        observed,
        vec![(7, 1, 0, true), (8, 0, 1, false)],
        "at 7 the stop line lands, is audited and marks the revision; at 8 the queue refuses it, \
         which is AUDITED rather than swallowed, and the mark is left unset so the next tick \
         tells the lane again: {observed:?}"
    );
}

/// **A lane whose pane is GONE is told nothing, and no row is written for the
/// telling that did not happen** (#3176, rev-std round 1 finding 1).
///
/// The declined row earns its retry from the queue-full case, where the next
/// tick can succeed. A pane that died mid-review while the PR is CONFLICTING is
/// the opposite: `deliver_prompt` answers `Err` on every tick of the whole
/// conflict window, the mark is never written, and `release_driven_pane` refuses
/// the same pane — so a futile retry would put one `rd-lane-stop-declined` row
/// per tick on the surface §5.4 asks a reader to count from. Truthful and
/// bounded, and still noise.
///
/// **Two arms differing in ONE fact — whether the pane is alive — and TWO ticks
/// each**, because the defect is per-tick repetition and a single tick cannot
/// show it. The live arm is the positive control: without it, "no rows" is
/// satisfied by a fixture that never reached the conflict, and by an
/// implementation that stopped telling anyone anything.
#[test]
fn a_lane_whose_pane_died_is_not_told_to_stop_and_writes_no_declined_row() {
    // (pane alive, rd-lane-stopped rows, rd-lane-stop-declined rows)
    type Row = (bool, usize, usize);
    let mut observed: Vec<Row> = Vec::new();

    for alive in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, lane) = briefed(&reg, &repo, &gh);
        reg.set_pr_body_override(Some("b".to_string()));
        reg.set_pr_head_override(Some(HEAD_A.to_string()));
        make_delivery_land(&reg, &group, &lane, 7801);

        if !alive {
            // The pane dies mid-review, the way a human kill or the idle reaper
            // ends one — the lane record still names it, which is the whole
            // point: the drive has an agent id to try and it is futile.
            // `mark_dead` and not `kill_agent`: the latter needs an app handle to
            // kill the pty and a headless test has none ("no app handle"). This is
            // the same primitive `release_driven_pane` claims a pane with, and the
            // §3.1 scan that forbids it covers the three DRIVER source files, not
            // this one.
            reg.mark_dead(&lane, Some(0)).expect("the lane's pane is claimable");
            assert_eq!(
                reg.agent(&lane).map(|a| a.status == AgentStatus::Dead),
                Some(true),
                "the fixture's premise: the pane is gone"
            );
            assert_eq!(
                live_lanes(&reg, &group).first().and_then(|l| l["agent"].as_str()),
                Some(lane.as_str()),
                "…and the lane record still names it, so the arm has something to try"
            );
        }

        gh.set_merge_state("CONFLICTING");
        let before = reg.audit_log(&group).len();
        reg.rd_drive_group_with(&group, &gh, 30_000);
        // A second tick, because the defect is one row PER TICK.
        reg.rd_drive_group_with(&group, &gh, 40_000);
        let mut log = reg.audit_log(&group);
        let after: Vec<_> = log.split_off(before);
        let rows = |a: &str| after.iter().filter(|e| e.action == a).count();

        observed.push((alive, rows("rd-lane-stopped"), rows("rd-lane-stop-declined")));
    }

    assert_eq!(
        observed,
        vec![(true, 1, 0), (false, 0, 0)],
        "a live busy lane is told once across two ticks; a lane whose pane is gone is told \
         nothing and writes no declined row for either tick — retrying a dead pane is provably \
         futile, so the row would be noise rather than a record: {observed:?}"
    );
}
