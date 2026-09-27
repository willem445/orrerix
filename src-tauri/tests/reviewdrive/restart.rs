//! Drives across a restart: fix-wait re-briefs (#2811 S10) and the live-roster reconcile (#3225/#3226).
//!
//! One module of the `reviewdrive` integration-test target (`main.rs`),
//! split out of the former single-file `tests/reviewdrive.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ── #2811 S10: a drive parked in `fix-wait` across a restart ────────────────

/// Persist a drive at its first hand-back, then hand the same group dir to a
/// **new registry** — which is what a restart is, and the only fixture that can
/// produce the fact S10 rests on: every pane died with the previous process.
///
/// The two registries are deliberately not the same object. A test that reused
/// one and merely cleared a map would be exercising the mid-session reading
/// `LaneFact::pane_dead` declines to make, and would pass against a reconcile
/// that had never run.
pub(crate) fn fix_wait_across_a_restart(
    dir: &std::path::Path,
    repo: &Repo,
    gh: &FakeGh,
) -> (OrchRegistry, GroupId) {
    let group = {
        let reg = relaunch_registry(dir);
        let (group, _session) = driven(&reg, repo, gh);
        to_first_handback(&reg, &group, gh);
        let status = reg.review_drive_status_with(&group, 40_000);
        assert_eq!(
            status["drives"][0]["state"],
            json!("fix-wait"),
            "the fixture must actually persist a FIX-WAIT drive, or nothing below is about \
             S10: {status}"
        );
        group
    };
    let reg = relaunch_registry(dir);
    reattach(&reg, repo, &group);
    (reg, group)
}

/// Re-register a group with a restarted registry — the half of a restart that
/// happens in memory.
///
/// `create_group`'s id is repo-derived "so a relaunch resumes the same state
/// dir", which is how the app reattaches on startup. A fixture that skips it
/// leaves `self.groups` empty, and then `driver_policy` answers `None` and
/// `rd_drive_group_with` returns before the reconcile it is supposed to be
/// testing — a tick that does NOTHING, which reads on the audit log exactly
/// like a reconcile that decided nothing.
pub(crate) fn reattach(reg: &OrchRegistry, repo: &Repo, group: &GroupId) {
    let again = reg.create_group(&repo.path(), rails()).expect("the relaunch reattaches");
    assert_eq!(
        &again.id, group,
        "the relaunch must resume the SAME group dir, or the drive under test is not the \
         one this registry is now driving"
    );
}

/// **The red.** On `main` the first tick after a restart does not hand the
/// worker back at all: the drive sits in `fix-wait` waiting on a pane that died
/// with the previous process, and the only thing that ever moves it is
/// `fix_timeout_minutes` expiring into `held(fix-stalled)` — a row that says a
/// worker was silent when what happened is that orrerix was restarted under it.
#[test]
fn a_fix_wait_drive_left_by_a_restart_is_handed_back_again() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (reg, group) = fix_wait_across_a_restart(dir.path(), &repo, &gh);

    reg.rd_drive_group_with(&group, &gh, 50_000);

    // The control: this test is about what the RECONCILE marked, so a zero
    // below must not be able to mean "the reconcile never ran in this registry
    // at all".
    //
    // **Counted, not `any`.** The audit log lives on disk and both registries
    // append to it, so the FIRST one's own reconcile row satisfies an
    // existence check for ever — a control that cannot fail. Two rows is the
    // property: one reconcile per registry that drove this group.
    assert_eq!(
        action_count(&reg, &group, "rd-recovered"),
        2,
        "one reconcile per registry: the restarted one must reconcile too, or nothing \
         below is about S10"
    );

    // **The whole rd-* trail, not just the hand-backs.** A re-brief that was
    // DECIDED and then failed to reach the worker emits `rd-refused` and a hold
    // rather than `rd-handback`, and a message quoting only the hand-backs
    // reports that as "nothing happened" — which is the one reading that sends
    // the next person looking in the wrong place.
    let trail: Vec<(String, serde_json::Value)> = reg
        .audit_log(&group)
        .into_iter()
        .filter(|e| e.action.starts_with("rd-"))
        .map(|e| (e.action, e.detail))
        .collect();
    let handbacks = audit_details(&reg, &group, "rd-handback");
    let restart: Vec<_> =
        handbacks.iter().filter(|d| d["why"] == json!("restart")).collect();
    assert_eq!(
        restart.len(),
        1,
        "the first tick after a restart must re-brief the worker it was waiting on. \
         The whole driver trail for this group: {trail:#?}"
    );
    assert!(
        !restart[0]["agent"].as_str().unwrap_or_default().is_empty(),
        "and name the pane it reached, which is what says the resume worked: {restart:?}"
    );

    // Still in `fix-wait`: a re-brief is not an arc, and the drive is waiting on
    // the same worker for the same fix it was waiting on before the restart.
    let status = reg.review_drive_status_with(&group, 50_000);
    assert_eq!(status["drives"][0]["state"], json!("fix-wait"), "{status}");
}

/// **A restart is not a round** — the property that makes the new `why` value
/// worth having rather than reusing the original hand-back's.
///
/// Measured as a DIFFERENCE across the restart, not against literals: the
/// fixture's own counters are whatever `to_first_handback` spent, and pinning
/// those numbers here would make this test fail for a change to the fixture
/// rather than for the property it is about.
#[test]
fn the_restart_hand_back_charges_no_counter() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);

    let (reg, group) = fix_wait_across_a_restart(dir.path(), &repo, &gh);
    // Read from the RESTARTED registry before it ticks: reconcile does not touch
    // the counters, so this is the pre-restart figure, read where the comparison
    // is made rather than carried across two registries by hand.
    let before = reg.review_drive_status_with(&group, 45_000)["drives"][0]["counters"].clone();
    reg.rd_drive_group_with(&group, &gh, 50_000);

    let after = reg.review_drive_status_with(&group, 50_000)["drives"][0]["counters"].clone();
    assert_eq!(
        after, before,
        "a restart must spend none of INVARIANT 9's budget: {before} -> {after}"
    );
    // **Counted by `why`, not in total.** The audit log is on disk and both
    // registries append to it, so the total also carries the fixture's own
    // `ci-red` hand-back from before the restart; what this test is about is
    // that the restart produced exactly one re-brief and charged nothing for
    // it.
    let restarts = audit_details(&reg, &group, "rd-handback")
        .into_iter()
        .filter(|d| d["why"] == json!("restart"))
        .count();
    assert_eq!(restarts, 1, "exactly one restart re-brief, and it spent nothing");
}

/// One re-brief per restart, and the ticks after it are what say so.
///
/// Were the mark not discharged, it would stand for the life of the process and
/// re-brief the worker on every tick for as long as the drive stayed in
/// `fix-wait` — a hand-back loop, not a recovery.
///
/// **The discharge moved at review 2 and this test did not**, which is why the
/// name says "reads": it was written when the mark was TAKEN at facts-build
/// time, so reading it and spending it were the same event. They are no longer
/// — a tick that cannot read the PR reads the mark and leaves it standing
/// (`a_restart_tick_that_cannot_read_the_pr_still_re_briefs_on_the_next_tick`)
/// — and what this pins is the half that did not change: the tick that ACTS
/// discharges it, so no later tick re-briefs. Kept under its original name so
/// the mutation history in the PR body still resolves to it.
#[test]
fn the_restart_mark_is_spent_by_the_tick_that_reads_it() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (reg, group) = fix_wait_across_a_restart(dir.path(), &repo, &gh);

    reg.rd_drive_group_with(&group, &gh, 50_000);
    let after_one = action_count(&reg, &group, "rd-handback");
    reg.rd_drive_group_with(&group, &gh, 60_000);
    reg.rd_drive_group_with(&group, &gh, 70_000);

    assert_eq!(
        action_count(&reg, &group, "rd-handback"),
        after_one,
        "the mark is consumed by the tick that reads it; later ticks re-brief nobody"
    );
}

/// The `fix-stalled` clock is re-anchored on the re-brief, so the bound is
/// measured from a brief the worker can actually answer.
///
/// The fixture handed back at 40s and the drive is re-briefed at 50s; a tick
/// one `fix_timeout_minutes` past the ORIGINAL hand-back must therefore still
/// find the drive working. Without the re-stamp the same tick parks it
/// `held(fix-stalled)`, which is the whole failure S10 removes arriving through
/// the fix for it.
#[test]
fn the_restart_hand_back_re_anchors_the_fix_stalled_clock() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (reg, group) = fix_wait_across_a_restart(dir.path(), &repo, &gh);

    reg.rd_drive_group_with(&group, &gh, 50_000);
    // Just past the default 60-minute bound as measured from the fixture's own
    // hand-back at 40s, and short of it as measured from the re-brief at 50s.
    let past_original = 40_000 + 60 * 60_000 + 1_000;
    reg.rd_drive_group_with(&group, &gh, past_original);

    let status = reg.review_drive_status_with(&group, past_original);
    assert_eq!(
        status["drives"][0]["state"],
        json!("fix-wait"),
        "the bound must run from the brief the worker was actually given: {status}"
    );
}

/// The guarded bypass, both ways, at the level the decision is made.
///
/// The tick derives `WorkerSignal::Unresumable` from the worker PANE having
/// exited, and after a restart every pane has — so on that one tick the signal
/// is a fact about the process and holding on it would park every resumable
/// drive orrerix came back up under. The mark is what tells the two apart, and
/// the second half here is the control: with no mark, the same facts still hold.
#[test]
fn a_dead_pane_holds_the_drive_unless_the_restart_mark_says_why_it_is_dead() {
    // **The head must MATCH the entry's.** `DriveEntry::new` leaves `head`
    // empty and `decide` returns `Wait` for an empty observed head before any
    // state logic runs, so a fixture that leaves them unequal takes arc 7 (the
    // worker pushed) and never reaches `decide_fix_wait` at all.
    let mut e = entry_at(DriveState::FixWait);
    e.head = "head-a".to_string();
    let limits = DriveLimits::default();
    let dead = DriveFacts { worker: WorkerSignal::Unresumable, ..facts_at("head-a") };

    assert_eq!(
        reviewdrive::decide(&e, &DriveFacts { restart_handback: true, ..dead.clone() }, &limits),
        DriveStep::Rehandback,
        "a pane that died with the process is probed, not held: the hand-back is what \
         discovers whether the SESSION survived"
    );
    assert_eq!(
        reviewdrive::decide(&e, &dead, &limits),
        held(HeldReason::WorkerUnresumable),
        "control: with no restart under it, a dead worker pane is still the hold it was"
    );
}

/// Arc 7 outranks the restart re-brief.
///
/// A worker that pushed before the process went down has already answered; CI
/// is what has to speak next, and re-briefing it would ask again for work that
/// is done. The control is the same facts at the UNMOVED head, which is the
/// only difference between the two.
#[test]
fn a_push_that_landed_before_the_shutdown_outranks_the_restart_re_brief() {
    let mut e = entry_at(DriveState::FixWait);
    e.head = "head-a".to_string();
    let limits = DriveLimits::default();
    let moved = DriveFacts { restart_handback: true, ..facts_at("head-b") };
    assert_ne!(e.head, "head-b", "the fixture must actually move the head");

    assert_eq!(
        reviewdrive::decide(&e, &moved, &limits),
        DriveStep::Advance { to: DriveState::CiWait, held_reason: None, bump: None },
        "the push is the answer; the restart does not un-ask for it"
    );
    assert_eq!(
        reviewdrive::decide(&e, &DriveFacts { restart_handback: true, ..facts_at(&e.head) }, &limits),
        DriveStep::Rehandback,
        "control: at the unmoved head the same facts DO re-brief, so the arm above is \
         precedence and not an arc that never fires"
    );
}

/// **Coverage, not a claim.** A `review-wait` drive already recovers across a
/// restart by a path that predates S10, and this pins that it does — so nothing
/// here is read as having made it work.
///
/// It survives the restart on disk in the state it was parked in, its lane is
/// re-opened by the ordinary path (the record's pane is gone, so `open_lane`
/// resumes the recorded session), it never reaches `fix-stalled`, and it emits
/// no `why: restart` hand-back: the re-brief is `fix-wait`'s alone, which is
/// what makes the mark a fact about ONE state rather than about restarts.
///
/// **Scoped to `review-wait` deliberately.** Reaching `gate-check` through this
/// seam needs recorded pass verdicts for every required lane, which is fixture
/// machinery about the GATE rather than about the restart; `decide_gate_check`
/// reads only `facts.gate` and `facts.required_lanes`, neither of which the
/// mark touches, and it is covered where the gate is.
#[test]
fn a_review_wait_drive_recovers_across_a_restart_by_the_path_it_already_had() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let group = {
        let reg = relaunch_registry(dir.path());
        let (group, _s) = driven(&reg, &repo, &gh);
        reg.rd_drive_group_with(&group, &gh, 10_000);
        let status = reg.review_drive_status_with(&group, 10_000);
        assert_eq!(
            status["drives"][0]["state"],
            json!("review-wait"),
            "the fixture must actually park the drive in `review-wait`: {status}"
        );
        group
    };
    let reg = relaunch_registry(dir.path());
    reattach(&reg, &repo, &group);

    let status = reg.review_drive_status_with(&group, 30_000);
    assert_eq!(
        status["drives"].as_array().map(|a| a.len()),
        Some(1),
        "a live drive survives the restart on disk: {status}"
    );
    assert_eq!(
        status["drives"][0]["state"],
        json!("review-wait"),
        "and in the state it was parked in: {status}"
    );

    reg.rd_drive_group_with(&group, &gh, 40_000);
    let after = reg.review_drive_status_with(&group, 40_000);
    assert_ne!(
        after["drives"][0]["held_reason"],
        json!("fix-stalled"),
        "no state but `fix-wait` may reach the bound S10 is about: {after}"
    );
    let restarted = audit_details(&reg, &group, "rd-handback")
        .into_iter()
        .filter(|d| d["why"] == json!("restart"))
        .count();
    assert_eq!(
        restarted, 0,
        "the restart re-brief is `fix-wait`'s alone: `review-wait` emitted one"
    );
}

/// `rd-handback` rows this drive recorded for the RESTART, which is the only
/// `why` these tests are about — the shared on-disk audit log also carries the
/// fixture's own `ci-red` hand-back from before the restart.
fn restart_handbacks(reg: &OrchRegistry, group: &GroupId) -> usize {
    audit_details(reg, group, "rd-handback")
        .into_iter()
        .filter(|d| d["why"] == json!("restart"))
        .count()
}

/// Repoint the drive's recorded worker session by rewriting `review_drives.json`
/// — the only way to build a drive that reached `fix-wait` with a REAL worker
/// and then lost that session, which is what a restart across a roster edit
/// looks like. A drive pointed at a bad session from the start never reaches
/// `fix-wait` at all: it parks on its first hand-back.
fn repoint_worker_session(reg: &OrchRegistry, group: &GroupId, session: &str) {
    let p = reg.state_root().join(group.as_str()).join(reviewdrive::REVIEW_DRIVES_FILE);
    let mut v = drives_json(reg, group);
    let before = v["entries"][0]["worker_session"].as_str().unwrap_or_default().to_string();
    assert!(!before.is_empty(), "the fixture must start from a recorded session: {v}");
    assert_ne!(before, session, "the repoint must actually change the session");
    v["entries"][0]["worker_session"] = json!(session);
    std::fs::write(&p, serde_json::to_string_pretty(&v).unwrap()).expect("rewrite the record");
}

/// **Review 2, rev-final finding 1.** A tick that could not read the PR has
/// decided nothing about the restart, so the mark must survive it.
///
/// `observe_pr` yields an empty head on any `gh` failure — a runner error, a
/// rate limit, an unparseable response — and `decide` returns `Wait` at its
/// empty-head guard before `decide_fix_wait` runs. That is a routine first-tick
/// condition, because a restart sends a burst of `gh` calls at once. With the
/// mark taken at facts-build time it was spent by that tick and never re-issued
/// (the reconcile runs once per registry instance), and the drive waited out
/// `fix_timeout_minutes` into the very `held(fix-stalled)` this slice removes.
///
/// The first half is the negative control and is true either way; the second
/// half is the test.
#[test]
fn a_restart_tick_that_cannot_read_the_pr_still_re_briefs_on_the_next_tick() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (reg, group) = fix_wait_across_a_restart(dir.path(), &repo, &gh);

    // The seam itself is down: not a fact about the PR, just no answer.
    gh.seam_down();
    reg.rd_drive_group_with(&group, &gh, 50_000);
    assert_eq!(
        restart_handbacks(&reg, &group),
        0,
        "control: a tick that could not read the PR re-briefs nobody"
    );

    // `gh` comes back at the head the hand-back was made against, so nothing
    // but the restart has changed.
    gh.set_facts("OPEN", HEAD_B);
    reg.rd_drive_group_with(&group, &gh, 60_000);
    assert_eq!(
        restart_handbacks(&reg, &group),
        1,
        "the mark must survive a tick that decided nothing, or one transient `gh` failure \
         at startup costs the drive the whole fix timeout"
    );

    // And it is still discharged exactly once: the tick that re-briefed spent it.
    reg.rd_drive_group_with(&group, &gh, 70_000);
    reg.rd_drive_group_with(&group, &gh, 80_000);
    assert_eq!(restart_handbacks(&reg, &group), 1, "spent by the tick that acted");
}

/// **Review 2, rev-std finding 2.** The `Rehandback -> Err` edge, executed.
///
/// The body and the design note both say a session that will not resume lands
/// on `held(worker-unresumable)` "by the ordinary path, through the one shared
/// `rd_handback_failed`". The extraction makes the DECISION shared; this makes
/// the new call edge — and the `rd-refused` row and hold that ride it on a
/// restart tick — something a test executes rather than something the prose
/// asserts.
#[test]
fn a_restart_re_brief_that_cannot_resume_the_session_holds_worker_unresumable() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (reg, group) = fix_wait_across_a_restart(dir.path(), &repo, &gh);

    // A well-shaped uuid this roster never recorded — `driven`'s own doc names
    // it as the fixture that parks a drive on its first hand-back. Written to
    // the record before the first post-restart tick, so the reconcile still
    // marks a `fix-wait` entry and the failure happens at the re-brief.
    repoint_worker_session(&reg, &group, "11111111-2222-3333-4444-555555555555");

    reg.rd_drive_group_with(&group, &gh, 50_000);

    let status = reg.review_drive_status_with(&group, 50_000);
    assert_eq!(status["drives"][0]["state"], json!("held"), "{status}");
    assert_eq!(
        status["drives"][0]["held_reason"],
        json!("worker-unresumable"),
        "the restart re-brief must fail into the ordinary hold, not a new one: {status}"
    );
    assert_eq!(
        restart_handbacks(&reg, &group),
        0,
        "and no `rd-handback` row claims a worker that was never reached"
    );
    let refused = audit_details(&reg, &group, "rd-refused");
    assert!(
        refused.iter().any(|d| d["reason"] == json!("worker-unresumable")),
        "the refusal is recorded where §5.4 asks a reader to count it: {refused:?}"
    );
}

// ── #2811 S10, review 3: the mark must not outlive the entry it describes ───

/// A restarted registry holding a **stale** restart mark: the reconcile marked
/// a `fix-wait` entry, and the tick that followed could not read the PR, so the
/// mark is still standing (which is review 2's fix — see
/// `a_restart_tick_that_cannot_read_the_pr_still_re_briefs_on_the_next_tick`).
///
/// That is the only state from which the three paths below can be tested: the
/// mark has to survive its own tick before anything else can strand it.
fn restarted_with_a_standing_mark(
    dir: &std::path::Path,
    repo: &Repo,
    gh: &FakeGh,
) -> (OrchRegistry, GroupId) {
    let (reg, group) = fix_wait_across_a_restart(dir, repo, gh);
    gh.seam_down();
    reg.rd_drive_group_with(&group, gh, 50_000);
    assert_eq!(
        restart_handbacks(&reg, &group),
        0,
        "the fixture must leave the mark STANDING, not spent: a tick that re-briefed \
         would make every assertion below vacuous"
    );
    (reg, group)
}

/// Walk a fresh drive on the same PR to `fix-wait`, then tick ONCE MORE, and
/// answer how many `why: restart` re-briefs it collected — which must be none,
/// because no restart interrupted THIS drive.
///
/// **Two things here are load-bearing, and the first draft had neither.** That
/// draft walked the drive to `fix-wait` through `ci-wait` and stopped, and it
/// passed with all three clears reverted — vacuously, twice over:
///
/// 1. The re-drive's `ci-wait` ticks each end outside `fix-wait`, and the
///    review-2 spend rule discharges the mark on exactly that condition. The
///    stale mark was gone before `fix-wait` was ever reached, so no clear was
///    needed for the assertion to hold. The route here is therefore
///    `CONFLICTING` on the first tick: arc 3 takes `ci-wait -> fix-wait` in one
///    step, the entry ends IN `fix-wait`, and the mark survives to be asked
///    for.
/// 2. The mark is spent by the tick AFTER the one that enters `fix-wait`, so a
///    walk that stops on entry never asks for it at all. Hence the extra tick,
///    with mergeability back to `CLEAN` so that tick is not another conflict
///    hand-back.
fn redrive_to_fix_wait(reg: &OrchRegistry, group: &GroupId, gh: &FakeGh, session: &str) -> usize {
    gh.set_facts("OPEN", HEAD_A);
    gh.set_checks(r#"[{"name":"build","state":"SUCCESS","link":"x"}]"#);
    gh.set_merge_state("CONFLICTING");
    let out = reg.drive_review_with(group, gh, 1758, session, false, 0, "orch-1", 100_000);
    assert_eq!(out["driving"], json!(true), "the re-drive must succeed: {out}");
    let before = restart_handbacks(reg, group);

    // Arc 3, in ONE step: the entry ends in `fix-wait`, so the review-2 spend
    // rule does not discharge the mark on the way.
    reg.rd_drive_group_with(group, gh, 110_000);
    let entered = reg.review_drive_status_with(group, 110_000);
    assert_eq!(
        entered["drives"][0]["state"],
        json!("fix-wait"),
        "the re-drive must reach `fix-wait` in one step, or the mark is discharged before \
         anything asks for it and this pins nothing: {entered}"
    );

    // The tick that would SPEND a stale mark.
    gh.set_merge_state("CLEAN");
    reg.rd_drive_group_with(group, gh, 120_000);
    restart_handbacks(reg, group) - before
}
/// **Review 3, the ruling.** A mark left behind by a CANCELLED drive must not
/// re-brief the next drive on that PR.
///
/// `rd_signals` is cleared here for the same reason and always has been; the
/// mark was not, and it is keyed `(group, pr)` while the entry it describes is
/// not — so it survived the cancel and was spent by the next drive's first
/// `fix-wait` tick as one unearned `why: restart` hand-back, on a drive no
/// restart ever interrupted.
#[test]
fn a_cancelled_drive_does_not_leave_a_restart_mark_for_the_next_one() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (reg, group) = restarted_with_a_standing_mark(dir.path(), &repo, &gh);

    assert_eq!(
        reg.cancel_review_drive(&group, 1758, "orch-1")["cancelled"],
        json!(true),
        "the fixture must actually cancel"
    );

    let w = reg.spawn_agent(&group, Role::Worker, "w2", "", false, None).unwrap();
    let session = w.session_id.clone().expect("claude mints a session id at spawn");
    assert_eq!(
        redrive_to_fix_wait(&reg, &group, &gh, &session),
        0,
        "a cancel must forget the mark: the next drive on this PR was interrupted by no \
         restart and is owed no re-brief"
    );
}

// ── #3225 / #3226: the restart reconcile reads the LIVE ROSTER ───────────────
//
// The beta11 restart drill's two siblings. Every pane died with the previous
// process, and the drive's own file still named them: a `review-wait` drive
// waited on a reviewer that no longer existed (#3226), and a `ci-wait` drive
// waited for the `report(done)` of a worker that had already pushed and could
// no longer speak (#3225), out to `held(fix-stalled)` naming both dead panes as
// "still OWNED".
//
// The fixtures below are `fix_wait_across_a_restart`'s shape — two registries
// over one state dir, never one registry with a cleared map, because only the
// first produces the fact these recoveries rest on.

/// The lane record of the one live drive, or a panic naming what was there.
fn live_lane(reg: &OrchRegistry, group: &GroupId) -> serde_json::Value {
    let lanes = live_lanes(reg, group);
    assert_eq!(
        lanes.len(),
        1,
        "the fixture must have opened exactly one lane, or the reads below are about \
         whichever one came first: {lanes:?}"
    );
    lanes[0].clone()
}

fn lane_pane(reg: &OrchRegistry, group: &GroupId) -> String {
    live_lane(reg, group)["agent"].as_str().unwrap_or_default().to_string()
}

/// The roster row for `agent_id`, or `None` when the live roster has none.
///
/// [`roster_row`] panics for an id it cannot find, which is precisely the state
/// the restart tests are about: a pane that died with the previous process
/// leaves no row behind, and "there is no row" is the assertion rather than the
/// accident.
fn roster_row_opt(
    reg: &OrchRegistry,
    group: &GroupId,
    agent_id: &str,
) -> Option<serde_json::Value> {
    reg.list_agents(group)
        .as_array()
        .expect("list_agents answers an array")
        .iter()
        .find(|r| r["id"] == json!(agent_id))
        .cloned()
}

fn worker_pane(reg: &OrchRegistry, group: &GroupId) -> String {
    drives_json(reg, group)["entries"][0]["worker_agent"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Walk a drive to `review-wait` with a lane genuinely OPEN — a lane record
/// naming a pane and a session — and answer the registry it was built in, so a
/// caller can run the LIVE control before restarting.
fn review_wait_with_an_open_lane(
    dir: &std::path::Path,
    repo: &Repo,
    gh: &FakeGh,
) -> (OrchRegistry, GroupId) {
    let reg = relaunch_registry(dir);
    let (group, _session) = driven(&reg, repo, gh);
    reg.rd_drive_group_with(&group, gh, 10_000);
    let pane = tick_until_lane(&reg, gh, &group, 20_000)
        .expect("the fixture must actually brief a lane, or nothing below is about #3226");
    assert_eq!(
        lane_pane(&reg, &group),
        pane,
        "the lane RECORD must name the pane the tick briefed — a record with no pane is \
         the one shape #3226 cannot be about"
    );
    assert_eq!(status_state(&reg, &group), "review-wait");
    (reg, group)
}

/// **#3226, and its own positive control.**
///
/// Both halves run against the same drive, which is what makes the second one
/// evidence: a lane whose reviewer is alive and thinking must be left alone, and
/// a re-brief rule that could not tell the two apart would show up here as a
/// doubled brief on the live half rather than as a passing test.
///
/// **The red.** On `main` the restarted registry re-briefs nobody: the lane
/// record still names the dead pane, `rd_dead_lane_pane` answers `None` for an
/// agent the roster has no entry for ("we could not check", the mid-session
/// reading), so `LaneFact::pane_dead` is false, `lane_open_for` is true, and
/// `decide_review_wait` waits — for the whole `review-wait` state bound, on a
/// reviewer that no longer exists. Measured on PRs #3221 and #3222: forty-five
/// minutes, no `rd-lane-spawned`, no hold, no row of any kind.
#[test]
fn a_review_wait_lane_whose_pane_died_with_the_process_is_re_briefed_and_a_live_one_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let group = {
        // **The control: the same drive, still running.** Four more ticks with
        // the lane's pane alive and briefed at this head must brief nobody.
        let (reg, group) = review_wait_with_an_open_lane(dir.path(), &repo, &gh);
        let spawns = action_count(&reg, &group, "rd-lane-spawned");
        for i in 0..4 {
            reg.rd_drive_group_with(&group, &gh, 60_000 + i * 5_000);
        }
        assert_eq!(
            action_count(&reg, &group, "rd-lane-spawned"),
            spawns,
            "a LIVE lane must be waited for, not re-briefed — a rule that cannot tell the \
             two apart doubles every round's reviewer"
        );
        group
    };
    let reg = relaunch_registry(dir.path());
    reattach(&reg, &repo, &group);

    // The premise, asserted rather than assumed: the recorded pane is not in
    // this registry's roster, and the recorded SESSION is still on the record.
    let dead = lane_pane(&reg, &group);
    assert!(!dead.is_empty(), "the record must still name the pane that died");
    assert_eq!(
        roster_row_opt(&reg, &group, &dead),
        None,
        "the pane died with the previous process, so the live roster must not have it — \
         if it does, this test is not about a restart"
    );
    let session = live_lane(&reg, &group)["session"].as_str().unwrap_or_default().to_string();
    assert!(!session.is_empty(), "the lane's conversation is what survives the restart");

    let spawns_before = action_count(&reg, &group, "rd-lane-spawned");
    let rounds_before = review_rounds(&reg, &group);
    let brief_pane = tick_until_lane(&reg, &gh, &group, 100_000)
        .expect("the first tick after a restart must re-brief the lane whose pane is gone");

    let rows = audit_details(&reg, &group, "rd-lane-spawned");
    assert_eq!(
        rows.len(),
        spawns_before + 1,
        "exactly one re-brief, not one per tick: {rows:#?}"
    );
    let row = rows.last().expect("the row just asserted");
    assert_eq!(row["resumed"], json!(true), "the lane's own conversation is resumed: {row}");
    assert_eq!(row["session"], json!(session), "…and it is the recorded one: {row}");
    assert_eq!(
        row["why"],
        json!("restart"),
        "the row must say WHY, or a restart recovery reads on this log exactly like an \
         ordinary round: {row}"
    );
    assert_ne!(brief_pane, dead, "the record must be re-pointed at the pane that now holds it");
    assert_eq!(
        lane_pane(&reg, &group),
        brief_pane,
        "and §7's interception key must follow it"
    );
    // **A restart is not a round** — the same property S10's re-hand-back has.
    assert_eq!(
        review_rounds(&reg, &group),
        rounds_before,
        "re-briefing a lane the restart killed must spend none of INVARIANT 9's budget"
    );
}

/// **#3225's second half, which is the one an orchestrator READS.**
///
/// `held(fix-stalled)`'s notice enumerates `owned_panes()` as "still OWNED", and
/// on the drill every id it printed had died with the process — so the remedy it
/// printed sent a human to look at panes that were not there. The ownership is
/// dropped at the reconcile, before any notice can be built from it; the sessions
/// are what survive, and the second assertion is that they do.
///
/// **The fixture is the `ci-wait` one, and that is load-bearing rather than
/// incidental.** A `fix-wait` drive cannot witness this at all: its first tick
/// after the restart re-briefs the worker, which mints a NEW pane and pushes the
/// old one onto `prior_worker_agents`, where `forget_dead_panes` — which
/// predates this change — drops it. The property would hold there with the
/// reconcile doing nothing, which is a test that passes for someone else's
/// reason. Here CI has not settled on the pushed head, so the tick reconciles
/// and decides nothing else.
///
/// The read BEFORE the tick is the control: the record really does still name
/// that pane, so the drop afterwards is caused by the reconcile rather than by a
/// field nothing ever wrote.
#[test]
fn the_reconcile_drops_ownership_of_panes_the_roster_does_not_have_and_keeps_the_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (reg, group) = ci_wait_after_a_push_across_a_restart(dir.path(), &repo, &gh);

    let dead = worker_pane(&reg, &group);
    assert!(
        !dead.is_empty(),
        "control: the record still names the pane it handed the fix to, so the drop below          is the reconcile's doing"
    );
    assert!(
        roster_row_opt(&reg, &group, &dead).is_none(),
        "…and that pane died with the previous process"
    );
    let session_before = driven_worker_session(&reg, &group);

    reg.rd_drive_group_with(&group, &gh, 60_000);

    let record = drives_json(&reg, &group)["entries"][0].clone();
    let owned_now: Vec<String> = record["prior_worker_agents"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(std::iter::once(&record["worker_agent"]))
        .filter_map(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    assert!(
        !owned_now.contains(&dead),
        "a pane that died with the process is not this drive's to name — and a notice          cannot enumerate what the record no longer holds: {record}"
    );
    assert_eq!(
        driven_worker_session(&reg, &group),
        session_before,
        "the CONVERSATION is what survives a restart — dropping it would cost the fix, not          a slot: {record}"
    );
    assert_eq!(
        status_state(&reg, &group),
        "ci-wait",
        "the tick that dropped it decided nothing else, which is what makes the drop          attributable to the reconcile: {record}"
    );
}

/// A drive in `ci-wait` for a head its worker PUSHED, handed to a new registry
/// — #3225's state exactly: the push is on record (`fix_pushed`), the receipts
/// are not, and the pane that would have reported them is gone.
fn ci_wait_after_a_push_across_a_restart(
    dir: &std::path::Path,
    repo: &Repo,
    gh: &FakeGh,
) -> (OrchRegistry, GroupId) {
    let group = {
        let reg = relaunch_registry(dir);
        let (group, _session) = driven(&reg, repo, gh);
        to_first_handback(&reg, &group, gh);
        // The worker pushes its fix: a head the drive has not seen. CI has not
        // settled on it yet, which is what keeps the drive in `ci-wait` rather
        // than advancing on the same tick.
        gh.set_checks(r#"[{"name":"build","state":"IN_PROGRESS","link":"x"}]"#);
        gh.set_facts("OPEN", HEAD_C);
        reg.rd_drive_group_with(&group, gh, 50_000);
        let status = reg.review_drive_status_with(&group, 50_000);
        assert_eq!(
            status["drives"][0]["state"],
            json!("ci-wait"),
            "the fixture must actually park the drive in `ci-wait` on the pushed head: \
             {status}"
        );
        assert_eq!(status["drives"][0]["head"], json!(HEAD_C), "…at the pushed head: {status}");
        group
    };
    let reg = relaunch_registry(dir);
    reattach(&reg, repo, &group);
    (reg, group)
}

/// **#3225's red.** On `main` the restarted drive goes green and then waits for
/// a `report(done)` from a pane that died with the previous process:
/// `decide_fix_receipts` reads `WorkerSignal::Silent`, the per-process signal map
/// is empty and can never fill, and one `fix_timeout_minutes` later the drive
/// parks `held(fix-stalled)` — a claim that a worker went silent, about a worker
/// orrerix was restarted under. Measured on PR #3220.
///
/// The push is the fix: it is durable, it is the head CI just went green on, and
/// the only thing missing is a message no pane can send.
#[test]
fn a_ci_wait_drive_whose_pusher_died_with_the_process_briefs_the_lane_on_the_green_head() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (reg, group) = ci_wait_after_a_push_across_a_restart(dir.path(), &repo, &gh);

    // The premise: the drive's worker pane is not in this registry's roster.
    let dead = worker_pane(&reg, &group);
    assert!(!dead.is_empty(), "the record must still name the pane that pushed");
    assert_eq!(
        roster_row_opt(&reg, &group, &dead),
        None,
        "the pusher died with the previous process — if the roster has it, this test is \
         not about a restart"
    );

    // CI settles green on the pushed head.
    gh.set_checks(r#"[{"name":"build","state":"SUCCESS","link":"x"}]"#);
    reg.rd_drive_group_with(&group, &gh, 60_000);
    assert_eq!(
        status_state(&reg, &group),
        "review-wait",
        "green on a head this drive's own worker pushed, with nobody left to report it, is \
         the fix delivered"
    );

    let pane = tick_until_lane(&reg, &gh, &group, 70_000)
        .expect("and the lane must actually be briefed at that head");
    assert!(!pane.is_empty());
    assert_eq!(
        live_lane(&reg, &group)["briefed_head"],
        json!(HEAD_C),
        "…at the head the worker pushed, not the one it was handed back from"
    );

    // Well past `fix_timeout_minutes` measured from the push: the bound this
    // drive used to die on must not fire at all.
    let past_the_bound = 60_000 + 90 * 60_000;
    reg.rd_drive_group_with(&group, &gh, past_the_bound);
    let status = reg.review_drive_status_with(&group, past_the_bound);
    assert_ne!(
        status["drives"][0]["held_reason"],
        json!("fix-stalled"),
        "no wait for a report that cannot come, so no hold about a silence that never \
         happened: {status}"
    );
}

/// The control for the arm above, at the level the decision is made: a `ci-wait`
/// drive whose worker is ALIVE still waits for its receipts (#2168 E1), which is
/// the property the restart mark must not have deleted.
#[test]
fn a_live_pusher_is_still_waited_for_and_only_the_restart_mark_changes_that() {
    let mut e = entry_at(DriveState::FixWait);
    e.head = "head-a".to_string();
    // **Arc 7 is the only way into the receipts wait**, and the fixture asserts
    // it landed. note_fix_push re-stamps an anchor that already exists and does
    // nothing when there is none, so calling it on a fresh ci-wait entry leaves
    // fix_pushed() false — decide_ci_wait then takes the plain green arc, and
    // BOTH halves below pass for a reason that has nothing to do with the mark.
    e.advance(DriveState::CiWait, None, None, 1_500).unwrap();
    assert!(
        e.fix_pushed(),
        "the fixture must actually be waiting on a push's receipts, or neither half below is about #3225"
    );
    let limits = DriveLimits::default();
    let green = DriveFacts { ci: CiObservation::Green, ..facts_at("head-a") };

    assert_eq!(
        reviewdrive::decide(&e, &green, &limits),
        DriveStep::Wait,
        "control: green is not the end of the round — the receipts still have to reach the \
         body, and a live worker is the one that writes them"
    );
    assert_eq!(
        reviewdrive::decide(&e, &DriveFacts { restart_push_delivered: true, ..green }, &limits),
        DriveStep::Advance { to: DriveState::ReviewWait, held_reason: None, bump: None },
        "and with the pusher gone, the push IS the fix delivered"
    );
}

/// **#3226's recovery-detail comment.** `drive_review` on a stuck `review-wait`
/// drive was refused `already-driven`, so the only manual route out was
/// `cancel_review_drive` — which reports the dead panes as "released" — plus a
/// fresh `drive_review`, and the counters go with it.
///
/// The second half is the positive control and it is load-bearing: the refusal is
/// NARROWED, not removed. Once the repair has run there is nothing dead left, so
/// the very next identical call is the ordinary duplicate again.
#[test]
fn drive_review_on_a_live_drive_whose_panes_died_resumes_instead_of_refusing() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let group = review_wait_with_an_open_lane(dir.path(), &repo, &gh).1;
    let reg = relaunch_registry(dir.path());
    reattach(&reg, &repo, &group);
    let session = driven_worker_session(&reg, &group);
    let rounds_before = review_rounds(&reg, &group);

    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 45_000);
    assert_eq!(
        out["driving"],
        json!(true),
        "a drive whose panes all died is not a duplicate — refusing it leaves \
         `cancel_review_drive` as the only recovery, and that one loses the counters: {out}"
    );
    assert_eq!(
        out["state"],
        json!("review-wait"),
        "and it is a REPAIR, not arc 11: the drive stays in the state it was in: {out}"
    );
    assert_eq!(
        review_rounds(&reg, &group),
        rounds_before,
        "…carrying the counters the cancel-and-re-drive workaround threw away"
    );
    assert_eq!(
        lane_pane(&reg, &group),
        "",
        "the dead lane pane is un-owned, which is what makes the next tick re-brief it"
    );

    let again = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 46_000);
    assert_eq!(
        again["refused"],
        json!("already-driven"),
        "control: with nothing left dead, a second call is the ordinary duplicate and is \
         still refused: {again}"
    );

    // And the repair is what the drive needed: the lane comes back.
    let pane = tick_until_lane(&reg, &gh, &group, 50_000)
        .expect("the tick after the repair must re-brief the lane");
    assert!(!pane.is_empty());
}

/// **#3228 review 2, W1: a dead SUPERSEDED pane is not a loss to recover from.**
///
/// A drive between a pane replacement and the next tick's own `forget_dead_panes`
/// prune has a dead id on `prior_worker_agents` as its ORDINARY state — nothing
/// was lost and the next tick tidies it. Reading that as a lost pane took an
/// ordinary duplicate `drive_review` into the repair arm, and on a `fix-wait`
/// drive the repair marks a re-hand-back **on the state alone**: the next tick
/// then re-briefs a live worker mid-fix with `why: restart` — a paid turn and a
/// duplicated brief on a drive that lost nothing.
///
/// **The existing narrowed-refusal control cannot reach this**, which is why this
/// is its own test: in `drive_review_on_a_live_drive_whose_panes_died_resumes_instead_of_refusing`
/// the second call refuses because the repair already emptied the record, so it
/// would pass under the defect too.
///
/// The fixture builds the superseded pane the way the driver really produces one
/// — two hand-backs, the first pane made busy in between so #3203's take-over
/// arm cannot reuse it — and kills it WITHOUT ticking, because a tick would
/// prune the prior list and dissolve the very state under test.
#[test]
fn a_dead_superseded_pane_does_not_turn_an_ordinary_duplicate_drive_review_into_a_repair() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, session) = driven(&reg, &repo, &gh);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));

    // Hand-back one.
    gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
    let first = reg.rd_drive_group_with(&group, &gh, 10_000);
    let (_pr, w1) = first.handbacks.first().cloned().expect("the drive hands back");

    // Hand-back two supersedes it.
    gh.set_facts("OPEN", HEAD_B);
    reg.set_pr_head_override(Some(HEAD_B.to_string()));
    reg.rd_drive_group_with(&group, &gh, 20_000);
    let second = reg.rd_drive_group_with(&group, &gh, 30_000);
    let (_pr, w2) = second.handbacks.first().cloned().expect("a second red hands back");
    assert_ne!(w1, w2, "the fixture needs two panes, or there is no superseded one");
    assert_eq!(
        reg.rd_owner(&group, &w1).map(|(_pr, p)| p.current),
        Some(false),
        "the fixture's premise: w1 is SUPERSEDED and still owned"
    );

    // It dies, and nothing ticks afterwards — a tick would prune the prior list
    // and dissolve the state this test is about.
    assert!(reg.mark_agent_dead_for_test(&w1), "the superseded pane must exist to be killed");
    assert_eq!(
        reg.rd_owner(&group, &w2).map(|(_pr, p)| p.current),
        Some(true),
        "…while the CURRENT pane is untouched, which is what makes this an ordinary \
         duplicate rather than a drive that lost something"
    );

    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 40_000);
    assert_eq!(
        out["refused"],
        json!("already-driven"),
        "a drive whose CURRENT panes are alive is the ordinary duplicate, whatever a \
         superseded list still names: {out}"
    );
    assert!(
        reg.review_drive_status_with(&group, 40_000)["drives"][0]["state"] == json!("fix-wait"),
        "the fixture must still be the fix-wait drive the consequence is about"
    );
    // The consequence, pinned where it would actually be paid: no restart mark
    // was minted, so the next tick re-briefs nobody.
    let restarts_before = restart_handbacks(&reg, &group);
    reg.rd_drive_group_with(&group, &gh, 50_000);
    assert_eq!(
        restart_handbacks(&reg, &group),
        restarts_before,
        "and no `why: restart` re-brief of a worker that is alive and mid-fix"
    );
}

/// **#3318 F2 — a pane a live review drive owns cannot be forked.** The driver
/// routes its panes by agent id, and a fork is a new agent it never briefed, so
/// orrerix refuses rather than inventing a third owner. The bystander — a worker
/// in the same group this drive never touched — is the control: the same call
/// is admitted for it, so the refusal is keyed on ownership and not on the
/// group having a drive at all.
#[test]
fn a_fork_of_a_driven_worker_is_refused_and_an_undriven_one_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, orch, worker) = driven_worker(&reg, &repo, &gh);

    let refused = reg
        .fork_agent(&group, &orch, &worker, "", None, None, "")
        .expect_err("a driven worker is the driver's");
    assert!(refused.contains("review drive on PR #1758"), "{refused}");

    let bystander = reg
        .spawn_agent(&group, Role::Worker, "unrelated", "", false, None)
        .expect("a delegate this drive never touched");
    let fork = reg
        .fork_agent(&group, &orch, &bystander.id, "", None, None, "")
        .unwrap_or_else(|e| panic!("the undriven control must be admitted: {e}"));
    assert_eq!(fork.forked_from, bystander.session_id);
}
