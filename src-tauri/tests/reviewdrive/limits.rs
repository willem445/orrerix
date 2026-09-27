//! `HeldReason::ProviderLimit` (#2811 S5b) and the aggregate's fail-safe re-read (#3195).
//!
//! One module of the `reviewdrive` integration-test target (`main.rs`),
//! split out of the former single-file `tests/reviewdrive.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ── #2811 S5b: HeldReason::ProviderLimit ────────────────────────────────────
//
// The seam is the attention scan's published map, not pane text: the scan owns
// the one pane-text classifier (S5a), and these tests write the map through
// `set_provider_limit_for_test` for the reason `with_pane` exists — a fake
// runner has no pty tails for a real scan to read.

// The lane pane a provider limit stops is the one `briefed()` returns: a drive
// starts in `ci-wait` and only reaches `review-wait` once CI is green, so the
// lane opens on the SECOND tick. An earlier revision of these tests read the
// first tick's report and every one of them failed on its own fixture guard —
// which is the guard working, and the reason it is an assertion.

/// A drive whose lane pane is stopped on a provider's refusal holds
/// `provider-limit` on the NEXT TICK — not sixty minutes later on
/// `lane-stalled`, which is the whole of #2811.
#[test]
fn a_limited_lane_pane_parks_the_drive_on_the_next_tick() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, lane) = briefed(&reg, &repo, &gh);
    assert_eq!(status_state(&reg, &group), "review-wait", "precondition");

    // The provider stops that pane.
    reg.set_provider_limit_for_test(&lane, "openrouter");

    let out = reg.rd_drive_group_with(&group, &gh, 30_000);
    assert_eq!(status_state(&reg, &group), "held", "the drive must park");
    let s = reg.review_drive_status(&group);
    assert_eq!(
        s["drives"][0]["held_reason"],
        json!("provider-limit"),
        "…naming the provider outage, not a timeout: {s}"
    );

    // **Nothing was spent.** The hold exists because the panes are stopped, not
    // slow: charging a counter would make a drive that survived an outage look
    // like one that had burned its budget.
    assert_eq!(
        s["drives"][0]["counters"]["review_rounds"], json!(0),
        "a provider limit must spend no review round: {s}"
    );
    assert_eq!(
        s["drives"][0]["counters"]["ci_attempts"], json!(0),
        "…and no CI attempt: {s}"
    );

    // The orchestrator's line names the provider and the remedy.
    let n = out
        .notices
        .iter()
        .find(|n| n.contains("provider limit"))
        .unwrap_or_else(|| panic!("no provider-limit notice: {:?}", out.notices));
    assert!(n.contains("OpenRouter"), "the notice must NAME the provider: {n}");
    assert!(n.contains("#1758"), "…and the drive it held: {n}");
    assert!(
        n.contains("add credits") || n.contains("total limit"),
        "…and the remedy that actually clears it: {n}"
    );
    assert!(
        n.contains("no round or CI attempt was spent"),
        "…and that it cost the drive nothing: {n}"
    );
}

/// The negative control, and the one that would fail if the fact were wired to
/// "any pane in the group" rather than to the panes THIS drive owns: a limit on
/// an unrelated pane must not park anything.
#[test]
fn a_limit_on_a_pane_this_drive_does_not_own_parks_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, lane) = briefed(&reg, &repo, &gh);
    assert_eq!(status_state(&reg, &group), "review-wait", "precondition");

    // A pane in the same group that this drive never opened.
    let bystander = reg
        .spawn_agent(&group, Role::Worker, "w-other", "", false, None)
        .expect("a bystander pane");
    reg.set_provider_limit_for_test(&bystander.id, "openrouter");

    let out = reg.rd_drive_group_with(&group, &gh, 30_000);
    assert_eq!(
        status_state(&reg, &group),
        "review-wait",
        "a limit on a pane the drive does not own must not park it"
    );
    assert!(
        out.notices.iter().all(|n| !n.contains("provider limit")),
        "…and must raise no notice: {:?}",
        out.notices
    );

    // Non-vacuity: the SAME registry parks the drive once the limit lands on a
    // pane it DOES own, so the silence above is the ownership test working
    // rather than the fact never being read.
    reg.set_provider_limit_for_test(&lane, "openrouter");
    reg.rd_drive_group_with(&group, &gh, 40_000);
    assert_eq!(
        status_state(&reg, &group),
        "held",
        "control: the drive's OWN pane on the same provider does park it"
    );
}

/// A provider limit outranks the time bounds, and this is what "no lane
/// timeout" means: at a `now` past `lane_timeout_minutes` the drive must report
/// the outage, not `lane-stalled`, because the pane is not slow — it is stopped,
/// and "read that pane" is a remedy that does not work.
#[test]
fn a_provider_limit_outranks_the_lane_stall_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, lane) = briefed(&reg, &repo, &gh);

    // Past the lane bound — where the pre-#2811 behaviour lived.
    let past_lane_timeout = 61 * 60 * 1000;
    reg.set_provider_limit_for_test(&lane, "openrouter");
    reg.rd_drive_group_with(&group, &gh, past_lane_timeout);

    let s = reg.review_drive_status(&group);
    assert_eq!(
        s["drives"][0]["held_reason"],
        json!("provider-limit"),
        "the outage must outrank every bound below it: {s}"
    );

    // The control that makes the row above mean something: WITHOUT the limit,
    // the same clock really does produce a stall hold — so this test is about
    // precedence and not about a timeout that never fires.
    let dir2 = tempfile::tempdir().unwrap();
    let reg2 = relaunch_registry(dir2.path());
    let repo2 = Repo::new();
    let gh2 = FakeGh::green(HEAD_A);
    let (group2, _lane2) = briefed(&reg2, &repo2, &gh2);
    reg2.rd_drive_group_with(&group2, &gh2, past_lane_timeout);
    let s2 = reg2.review_drive_status(&group2);
    assert_ne!(
        s2["drives"][0]["held_reason"],
        json!("provider-limit"),
        "control: with no limit published, the same clock holds for a time reason: {s2}"
    );
    assert_eq!(s2["drives"][0]["state"], json!("held"), "control: it does hold: {s2}");
}

/// `drive_review` resumes a provider-limited drive, and the hold does not
/// come back while the limit is gone.
///
/// **The resume is explicit, deliberately.** plan-2504 also floats "a pane on
/// that provider completing a turn resumes all"; that is not shipped, and the
/// reason is in `docs/design/review-driver.md`: a pane completing a turn does
/// not prove the account was topped up. The refusal can simply have scrolled
/// out of the tail window — which is exactly how S5a's own chip clears — so
/// self-resuming on it would restart N drives against an account that is still
/// empty, and re-hold them one tick later.
#[test]
fn drive_review_resumes_a_provider_limited_drive() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, session) = driven(&reg, &repo, &gh);
    reg.rd_drive_group_with(&group, &gh, 10_000);
    let report = reg.rd_drive_group_with(&group, &gh, 20_000);
    let (_pr, _block, lane) = report
        .lanes_opened
        .first()
        .cloned()
        .expect("the second tick opens lane 0");
    reg.set_provider_limit_for_test(&lane, "openrouter");
    reg.rd_drive_group_with(&group, &gh, 30_000);
    assert_eq!(status_state(&reg, &group), "held", "precondition");

    // The human raised the limit; the scan stops publishing it.
    reg.clear_provider_limit_for_test(&lane);
    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 30_000);
    assert_eq!(out["driving"], json!(true), "the resume must take: {out}");
    assert_ne!(
        status_state(&reg, &group),
        "held",
        "…and put the drive back to work"
    );

    // And it stays at work: the hold is not re-armed by a stale map.
    reg.rd_drive_group_with(&group, &gh, 40_000);
    assert_ne!(
        reg.review_drive_status(&group)["drives"][0]["held_reason"],
        json!("provider-limit"),
        "a cleared limit must not re-hold on the next tick"
    );
}

/// **The half of the union the design argument rests on**, and the one no other
/// test here reaches: a drive in `fix-wait` owns a WORKER pane and no open lane
/// at all, so a per-`LaneFact` field — which is how plan-2504 words this —
/// structurally could not see it. That is why `provider_limited` is a
/// drive-level fact, and this is what makes the claim falsifiable rather than
/// merely argued in a design note.
#[test]
fn a_limited_worker_pane_parks_a_drive_in_fix_wait() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _session) = driven(&reg, &repo, &gh);

    let handed = to_first_handback(&reg, &group, &gh);
    let (_pr, worker) = handed
        .handbacks
        .first()
        .cloned()
        .expect("the drive hands the fix back to a worker");
    assert_eq!(
        status_state(&reg, &group),
        "fix-wait",
        "fixture: the drive must be in the state whose only owned pane is the worker"
    );
    // ...and it must genuinely have NO open lane, or this test would pass
    // through the lane half of the union and prove nothing about the worker.
    let s = reg.review_drive_status(&group);
    let lane_agents: Vec<String> = s["drives"][0]["lanes"]
        .as_array()
        .map(|ls| {
            ls.iter()
                .filter_map(|l| l["agent"].as_str())
                .filter(|a| !a.is_empty())
                .map(|a| a.to_string())
                .collect()
        })
        .unwrap_or_default();
    for a in &lane_agents {
        assert_ne!(
            *a, worker,
            "fixture: the worker must not also be a lane agent, or the union is not being split"
        );
        assert!(
            reg.provider_limit_for_agent(a).is_none(),
            "fixture: no lane pane may be limited here — only the worker is"
        );
    }

    reg.set_provider_limit_for_test(&worker, "anthropic");
    reg.rd_drive_group_with(&group, &gh, 50_000);

    let s = reg.review_drive_status(&group);
    assert_eq!(s["drives"][0]["state"], json!("held"), "the drive must park: {s}");
    assert_eq!(
        s["drives"][0]["held_reason"],
        json!("provider-limit"),
        "…on the worker pane's provider, which no per-lane fact could see: {s}"
    );
    // The other provider, named — so this is not passing on a hard-coded one.
    let notices = reg.rd_drive_group_with(&group, &gh, 60_000);
    let _ = notices;
    assert!(
        s["drives"][0]["counters"]["review_rounds"].as_u64().unwrap_or(99) <= 1,
        "a provider limit spends no round of its own: {s}"
    );
}

/// **The placement the `decide` comment claims, pinned.** The arm sits above
/// the drive-age backstop and the per-state bound, and until #3191's own
/// mutation round nothing tested that: the sibling test above runs at
/// sixty-one minutes, where neither of those bounds has fired, so what it
/// really pins is precedence over `lane-stalled`. Moving the arm below the
/// backstops reddened nothing — which is how this gap was found.
///
/// At thirteen hours the age backstop is past and `drive-stalled` is what a
/// drive reports. A provider-limited one must still report the outage: the age
/// is measuring a wait that is not this drive's fault, and "the drive has been
/// running twelve hours" is a remedy an orchestrator cannot act on while every
/// pane it owns is stopped on a vendor's billing.
#[test]
fn a_provider_limit_outranks_the_drive_age_backstop() {
    let past_drive_timeout = 721 * 60 * 1000;

    // With the limit: the outage wins.
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, lane) = briefed(&reg, &repo, &gh);
    reg.set_provider_limit_for_test(&lane, "openrouter");
    reg.rd_drive_group_with(&group, &gh, past_drive_timeout);
    let s = reg.review_drive_status(&group);
    assert_eq!(
        s["drives"][0]["held_reason"],
        json!("provider-limit"),
        "the outage must outrank the twelve-hour age backstop: {s}"
    );

    // Without it: the SAME clock reports the age. This is what makes the row
    // above a precedence claim rather than a statement about a bound that
    // never fires — the mutation that demoted the arm passed the sixty-one
    // minute test for exactly that reason.
    let dir2 = tempfile::tempdir().unwrap();
    let reg2 = relaunch_registry(dir2.path());
    let repo2 = Repo::new();
    let gh2 = FakeGh::green(HEAD_A);
    let (group2, _lane2) = briefed(&reg2, &repo2, &gh2);
    reg2.rd_drive_group_with(&group2, &gh2, past_drive_timeout);
    let s2 = reg2.review_drive_status(&group2);
    assert_eq!(
        s2["drives"][0]["held_reason"],
        json!("drive-stalled"),
        "control: with no limit published the same clock reports the age: {s2}"
    );
}

/// **The headline property, at the only N where it means anything** (#3191
/// review finding 2). "One hold per drive, ONE notice for all of them" was
/// pinned by nothing: every other provider-limit test drives a single PR, so
/// the aggregation ran with one entry in its list and a mutation that built one
/// notice PER newly-held drive left the whole suite green — which is exactly
/// the outcome this slice exists to prevent.
#[test]
fn two_drives_on_one_provider_produce_exactly_one_notice() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);

    // Drive 1758 (the helper's PR) and a second PR in the same group.
    let (group, lane_a) = briefed(&reg, &repo, &gh);
    let w2 = reg
        .spawn_agent(&group, Role::Worker, "w2", "", false, None)
        .expect("a second worker to drive");
    let s2 = w2.session_id.clone().expect("claude mints a session id");
    let out = reg.drive_review_with(&group, &gh, 1759, &s2, false, 0, "orch-1", 20_000);
    assert_eq!(out["driving"], json!(true), "the second drive must start: {out}");
    reg.rd_drive_group_with(&group, &gh, 30_000);
    let r = reg.rd_drive_group_with(&group, &gh, 40_000);
    let lane_b = r
        .lanes_opened
        .iter()
        .find(|(pr, _, _)| *pr == 1759)
        .map(|(_, _, a)| a.clone())
        .expect("the second drive opens a lane");
    assert_ne!(lane_a, lane_b, "fixture: the two drives must own different panes");

    // One provider stops both.
    reg.set_provider_limit_for_test(&lane_a, "openrouter");
    reg.set_provider_limit_for_test(&lane_b, "openrouter");
    let out = reg.rd_drive_group_with(&group, &gh, 50_000);

    // Both parked, on the same reason.
    let s = reg.review_drive_status(&group);
    let held: Vec<&serde_json::Value> = s["drives"]
        .as_array()
        .expect("drives")
        .iter()
        .filter(|d| d["held_reason"] == json!("provider-limit"))
        .collect();
    assert_eq!(held.len(), 2, "both drives must park on the outage: {s}");

    // ...and the orchestrator hears about it ONCE.
    let lines: Vec<&String> =
        out.notices.iter().filter(|n| n.contains("provider limit")).collect();
    assert_eq!(
        lines.len(),
        1,
        "one provider limit is ONE line however many drives it held: {:?}",
        out.notices
    );
    let n = lines[0];
    assert!(n.contains("2 drives held"), "…and it must count them: {n}");
    assert!(n.contains("#1758") && n.contains("#1759"), "…and name them: {n}");

    // The per-drive wording must NOT also reach the pane — it is the board
    // note. Without this the aggregate could be correct while N held lines went
    // out beside it, which is the same cost the one-line rule exists to avoid.
    assert!(
        out.notices.iter().all(|n| !n.contains("account behind this drive's panes")),
        "the per-drive hold wording must stay on the board: {:?}",
        out.notices
    );

    // The group-level audit row, which nothing asserted before this test.
    let audit = reg.audit_log(&group);
    let row = audit
        .iter()
        .find(|e| e.action == "rd-provider-limit")
        .expect("the aggregated notice records its own row");
    assert_eq!(row.detail["provider"], json!("openrouter"), "{:?}", row.detail);
    assert_eq!(row.detail["drives"], json!(2), "{:?}", row.detail);
}

/// **A staggered outage counts every drive still held, not just this tick's**
/// (#3191 review finding 4). Two drives on one provider that park on DIFFERENT
/// ticks used to produce two lines, the second reading "1 drive held" while two
/// were — each true about what had just happened and false about the thing an
/// orchestrator reads it for.
#[test]
fn a_staggered_provider_outage_counts_every_drive_still_held() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);

    let (group, lane_a) = briefed(&reg, &repo, &gh);
    let w2 = reg.spawn_agent(&group, Role::Worker, "w2", "", false, None).unwrap();
    let s2 = w2.session_id.clone().unwrap();
    reg.drive_review_with(&group, &gh, 1759, &s2, false, 0, "orch-1", 20_000);
    reg.rd_drive_group_with(&group, &gh, 30_000);
    let r = reg.rd_drive_group_with(&group, &gh, 40_000);
    let lane_b = r
        .lanes_opened
        .iter()
        .find(|(pr, _, _)| *pr == 1759)
        .map(|(_, _, a)| a.clone())
        .expect("the second drive opens a lane");

    // The first drive parks alone.
    reg.set_provider_limit_for_test(&lane_a, "openrouter");
    let first = reg.rd_drive_group_with(&group, &gh, 50_000);
    let l1 = first.notices.iter().find(|n| n.contains("provider limit")).expect("first line");
    assert!(l1.contains("1 drive held"), "the first line is honest about one: {l1}");

    // The second parks a tick later — and the new line must count BOTH, because
    // both are held on this provider right now.
    reg.set_provider_limit_for_test(&lane_b, "openrouter");
    let second = reg.rd_drive_group_with(&group, &gh, 60_000);
    let l2 = second
        .notices
        .iter()
        .find(|n| n.contains("provider limit"))
        .expect("the second arrival announces");
    assert!(
        l2.contains("2 drives held"),
        "a staggered outage must count every drive still held, not just this tick's: {l2}"
    );
    assert!(l2.contains("#1758") && l2.contains("#1759"), "…and name both: {l2}");
}

/// **Precedence over the per-state bound** (#3191 review finding 3). The arm is
/// pinned above `lane-stalled` (61 min) and the age backstop (721 min); the
/// per-state bound sits between them and was unpinned, so a mutation demoting
/// the arm below `state_bound_ms` but above the age check reddened nothing.
///
/// `fix-wait`'s bound is `max(90 minutes, fix_timeout_minutes)`, so 95 minutes
/// is past it at stock knobs.
#[test]
fn a_provider_limit_outranks_the_per_state_bound() {
    let past_state_bound = 95 * 60 * 1000;

    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _session) = driven(&reg, &repo, &gh);
    let handed = to_first_handback(&reg, &group, &gh);
    let (_pr, worker) = handed.handbacks.first().cloned().expect("a hand-back");
    reg.set_provider_limit_for_test(&worker, "anthropic");
    reg.rd_drive_group_with(&group, &gh, past_state_bound);
    let s = reg.review_drive_status(&group);
    assert_eq!(
        s["drives"][0]["held_reason"],
        json!("provider-limit"),
        "the outage must outrank the per-state bound: {s}"
    );

    // The control: the same clock, no limit published, reports a time reason —
    // so this is precedence and not a bound that never fires.
    let dir2 = tempfile::tempdir().unwrap();
    let reg2 = relaunch_registry(dir2.path());
    let repo2 = Repo::new();
    let gh2 = FakeGh::green(HEAD_A);
    let (group2, _s2) = driven(&reg2, &repo2, &gh2);
    to_first_handback(&reg2, &group2, &gh2);
    reg2.rd_drive_group_with(&group2, &gh2, past_state_bound);
    let s2 = reg2.review_drive_status(&group2);
    assert_eq!(s2["drives"][0]["state"], json!("held"), "control: it does hold: {s2}");
    assert_ne!(
        s2["drives"][0]["held_reason"],
        json!("provider-limit"),
        "control: with no limit the same clock holds for a time reason: {s2}"
    );
}

// ── #3195 item 2: the aggregate's fail-safe arm ─────────────────────────────

/// A runner that makes the group's `review_drives.json` unreadable on its first
/// `gh` call, then answers exactly as the `FakeGh` it wraps.
///
/// The first call is the tick's own first step read: the tick's opening
/// `load_state` has already succeeded by then (a tick that cannot read the
/// record refuses outright, so a sabotage any earlier tests the refusal, not
/// the arm), and reconcile is already latched by the fixture's earlier ticks.
/// Swapping the FILE for a DIRECTORY is the one sabotage that survives to the
/// re-read: the store before the re-read is the last writer, so the re-read
/// can only fail on a store that failed too, and a directory defeats both —
/// the store's rename falls through `fsatomic`'s fallback into a write that
/// cannot open a directory, and `load_state` answers `Err` (not `NotFound`,
/// which would read as an empty record and succeed).
pub(crate) struct RereadKiller {
    pub(crate) inner: FakeGh,
    pub(crate) state_file: std::path::PathBuf,
    pub(crate) fired: std::sync::atomic::AtomicBool,
}

impl RdRunner for RereadKiller {
    fn gh(&self, args: &[&str]) -> Result<CmdOut, String> {
        if !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
            std::fs::remove_file(&self.state_file)
                .expect("fixture: the old state file must be removable");
            std::fs::create_dir(&self.state_file)
                .expect("fixture: the state path must accept a directory");
        }
        self.inner.gh(args)
    }
}

/// **The fail-safe arm: a held-set re-read that fails still says what this
/// tick saw** (#3195 item 2). The aggregate is built from the live held set
/// re-read from state at notice time, and that read can fail — so the branch
/// under it falls back to the tick's own outs, on the argument that an
/// under-count beats silence and the per-drive `rd-held` rows are on the
/// record either way. Pinned by nothing before #3195: the two round-2 tests
/// both run the read that succeeds, so deleting the arm left the suite green.
///
/// The control beside it, `the_happy_reread_still_aggregates`, runs the
/// identical fixture without the sabotage, so the only delta between the two
/// is the failed re-read — and not some other breakage the sabotage dragged
/// in.
#[test]
fn a_failed_held_set_reread_still_says_what_this_tick_saw() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, lane) = briefed(&reg, &repo, &gh);
    assert_eq!(status_state(&reg, &group), "review-wait", "precondition");

    // The path the test reaches the state file by is the one `drives_json`
    // already uses — the registry hands out no directory (constraint 6), and
    // the sabotage needs the FILE, not the group dir, for the same reason
    // `corrupt_drive_record_for_test` writes a fixed payload rather than
    // taking one.
    let state_file =
        reg.state_root().join(group.as_str()).join(reviewdrive::REVIEW_DRIVES_FILE);
    assert!(state_file.is_file(), "fixture: the record must exist before the tick");

    reg.set_provider_limit_for_test(&lane, "openrouter");
    let killer =
        RereadKiller { inner: gh, state_file: state_file.clone(), fired: Default::default() };
    let out = reg.rd_drive_group_with(&group, &killer, 30_000);

    // The sabotage really ran, and really left the re-read nothing to read —
    // without this the test below could pass on a tick where the failure never
    // happened and the happy path produced the line.
    assert!(state_file.is_dir(), "fixture: the state path must now be a directory");
    assert_eq!(out.refused, None, "the tick must not refuse on the opening read: {:?}", out);
    let store_failed = reg
        .audit_log(&group)
        .iter()
        .any(|e| e.action == "rd-state-unreadable"
            && e.detail["reason"] == json!("review_drives.json could not be written"));
    assert!(store_failed, "fixture: the store must be the writer that failed");

    // The promise: the line still goes out, built from what THIS tick saw.
    let lines: Vec<&String> =
        out.notices.iter().filter(|n| n.contains("provider limit")).collect();
    assert_eq!(
        lines.len(),
        1,
        "the failed re-read must not silence the notice: {:?}",
        out.notices
    );
    let n = lines[0];
    assert!(n.contains("OpenRouter"), "…naming the provider: {n}");
    assert!(n.contains("#1758"), "…and the drive it saw park: {n}");

    // And the aggregated notice keeps its own audit row, as on the happy path.
    let audit = reg.audit_log(&group);
    let row = audit
        .iter()
        .find(|e| e.action == "rd-provider-limit")
        .expect("the fail-safe notice records its own row");
    assert_eq!(row.detail["provider"], json!("openrouter"), "{:?}", row.detail);
    assert_eq!(row.detail["drives"], json!(1), "{:?}", row.detail);
}

/// **The positive control for the fail-safe arm**: the identical fixture, no
/// sabotage. This is what separates "the arm carried the notice through the
/// failed re-read" from "aggregation is broken and the sabotage test passed on
/// some other path" — the happy re-read still aggregates, from state, the
/// same one line.
#[test]
fn the_happy_reread_still_aggregates() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, lane) = briefed(&reg, &repo, &gh);
    assert_eq!(status_state(&reg, &group), "review-wait", "precondition");

    let state_file =
        reg.state_root().join(group.as_str()).join(reviewdrive::REVIEW_DRIVES_FILE);
    reg.set_provider_limit_for_test(&lane, "openrouter");
    let out = reg.rd_drive_group_with(&group, &gh, 30_000);

    // The record survived the tick, and nothing audited a failed store — the
    // exact inverse of the sabotage test's fixture guards.
    assert!(state_file.is_file(), "control: the record must still be a file");
    assert!(
        !reg.audit_log(&group).iter().any(|e| e.action == "rd-state-unreadable"),
        "control: no store failure, so the line below came off the re-read"
    );

    let lines: Vec<&String> =
        out.notices.iter().filter(|n| n.contains("provider limit")).collect();
    assert_eq!(lines.len(), 1, "control: the happy path still aggregates: {:?}", out.notices);
    let n = lines[0];
    assert!(n.contains("OpenRouter"), "…naming the provider: {n}");
    assert!(n.contains("#1758") && n.contains("1 drive held"), "…from the re-read: {n}");
}
