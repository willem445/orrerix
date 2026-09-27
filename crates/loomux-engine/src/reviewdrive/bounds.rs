//! §2.2's per-state bounds (#2110): the per-state constants and
//! [`state_bound_ms`], which combines each with the repo's knobs.
//!
//! Part of the review driver's pure core, split out of the former single-file
//! `reviewdrive.rs` by #3498 P7 as a pure move. The module map is in `mod.rs`;
//! the design note is `docs/design/review-driver.md`.

use super::*;

// ── §2.2's per-state bounds (#2110) ────────────────────────────────────────
//
// **Why these exist at all, and what they replaced.** Until #2110 the only
// clock over a working drive was its total age, and the two measured failures
// were the two that measure cannot tell apart: a drive making steady progress
// across four hours of transitions was parked as "stalled", and a drive that
// spent three of its four hours unable to spawn a lane at all had that
// starvation counted against its own budget. An age answers "how long has this
// drive existed"; nobody wanted that bounded. What a bound is *for* is "how
// long has this drive sat in one place", and that is what these measure.
//
// **They are constants and not `driver:` policy**, on [`CAP_HOLD_MS`]'s
// argument: how long orrerix waits on its OWN machinery — a check run, a
// reviewer lane, a resumed worker — before telling an orchestrator is not a
// repo's call.
//
// **How each combines with the repo's knobs differs per arm, and there is no
// one sentence for all four.** This block used to carry one — "a per-state
// bound is the LARGER of its constant and whatever the repo configured" — and
// it was the fifth surface of a claim rounds 3 and 4 had already corrected
// elsewhere. It was true of one arm, never true of two, and stopped being true
// of the fourth when #2117's W3 made that arm an add. [`state_bound_ms`] is
// where the per-arm rule is argued; the short form:
//
// - `gate-check` has **no knob at all** — [`DriveLimits`] carries no `gate_`
//   timeout — so there is nothing to combine with and nothing to shadow.
// - `fix-wait` is the LARGER of its constant and `fix_timeout_minutes`. It is
//   the only arm that is a max at all.
// - `review-wait` is its constant PLUS `lane_timeout_minutes` per required
//   lane, because that state holds several waits in sequence and the product
//   alone funds their silences and none of the gaps between them.
// - `ci-wait` is its constant PLUS `fix_timeout_minutes` (#2168 E1), on the
//   same argument: since E1 that state holds the check matrix and then the
//   worker's report on a pushed head, one after the other. It was a bare
//   constant before E1 and belonged on the `gate-check` line.

/// How long a drive may sit in `ci-wait` (§2.1) before [`HeldReason::StateStalled`].
///
/// **Ninety minutes, and `ci-wait` is the state that most needed one**: it is
/// the only working state with no bound of its own at all —
/// `CiObservation::Pending` and `Unknown` both return `Wait`, forever, and
/// before #2110 the first thing to notice was the total age hours later. This
/// project's own matrix is three platforms at twenty to thirty minutes; ninety
/// covers a full re-run behind a queue and is still an hour clear of anything
/// that has ever been legitimate here.
///
/// **The SLACK since #2168 E1, not the bound outright.** [`state_bound_ms`]
/// ADDS `fix_timeout_minutes` to this, because that state now holds a second
/// wait measured on that knob — [`decide_fix_receipts`] — after the check wait
/// this number is sized for. Compared rather than added, the margin at any
/// `fix_timeout_minutes` above ninety minutes is exactly zero and
/// `held(state-stalled)` preempts the `held(fix-stalled)` that names the pane
/// to read; that is `REVIEW_WAIT_BOUND_MS`'s own argument, one state over.
pub const CI_WAIT_BOUND_MS: u64 = 90 * 60_000;

/// How long a drive may sit in `review-wait` (§2.1) before
/// [`HeldReason::StateStalled`].
///
/// **Three hours, because `review-wait` is the one state that legitimately
/// holds several waits in a row.** The gate's lanes are reviewed in SEQUENCE
/// and `lane_index` advancing is deliberately not a transition (§2.1), so a
/// three-lane gate whose reviewers each take their full `lane_timeout_minutes`
/// spends three hours here with nothing wrong.
///
/// **It is the SLACK over that product, not a rival to it** (#2117 review 2,
/// W3): [`state_bound_ms`] ADDS this to `lane_timeout_minutes * lanes` rather
/// than taking the larger of the two. The product covers the lanes' silences;
/// this covers everything inside `review-wait` that is not one of them — the
/// stretch before the first brief, and the tick-detection gap after each
/// verdict. Compared rather than added, the margin is exactly zero at the
/// configurations the floor exists to protect, and a drive whose reviewers all
/// answered in time parks anyway. See [`state_bound_ms`] for the worked case.
pub const REVIEW_WAIT_BOUND_MS: u64 = 180 * 60_000;

/// How long a drive may sit in `fix-wait` (§2.1) before
/// [`HeldReason::StateStalled`].
///
/// **Ninety minutes, a floor over `fix_timeout_minutes` rather than a second
/// opinion about it.** `held(fix-stalled)` already bounds the wait this state
/// is for — a worker that neither pushes nor reports — and it is the better
/// notice whenever it applies, so this is sized *above* that knob's own
/// sixty-minute default and named in [`state_bound_ms`] so a repo that raises
/// the knob raises this with it. What is left for this to catch is the case
/// `fix-stalled` cannot see: a drive in `fix-wait` whose worker is neither
/// silent nor finished.
pub const FIX_WAIT_BOUND_MS: u64 = 90 * 60_000;

/// How long a drive may sit in `gate-check` (§2.1) before
/// [`HeldReason::StateStalled`].
///
/// **Fifteen minutes, and it is the tightest because `gate-check` is a
/// decision rather than a wait.** Every outcome but one leaves the state on the
/// tick that entered it; the exception is `GateOutcome::NotEvaluated`, a tick
/// that reached `gate-check` without evaluating the gate, which is a fault in
/// the read and not a thing to wait out. Three back-off intervals — the same
/// number [`CAP_HOLD_MS`] allows a transient cap — is generous for a condition
/// that ought to clear on the next tick.
pub const GATE_CHECK_BOUND_MS: u64 = 15 * 60_000;

/// The bound on time spent in one working state (#2110), or `None` for a state
/// that is not a wait — `held` is parked and the two terminals are over, so
/// none of the three has a clock at all.
///
/// **What each state's bound is made of, per state** — there is no single rule,
/// and an earlier version of this paragraph claimed one ("`max` of the constant
/// and the state's own configured bound"). That was true of every arm when it
/// was written, stopped being true of `review-wait` the moment #2117's W3 made
/// that arm an ADD and of `ci-wait` when #2168 E1 made that one an ADD too, and
/// was never true of the arm that has no knob to take a max against. Three
/// shapes over four arms:
///
/// - `gate-check` is a **bare constant**. [`DriveLimits`] has no `gate_`
///   timeout, so there is nothing to shadow and nothing to take a maximum
///   with.
/// - `fix-wait` is `max(constant, fix_timeout_minutes)` — a floor over the
///   knob, so a repo that raises `fix_timeout_minutes` to four hours raises
///   this with it rather than being parked at ninety minutes by a number in
///   loomux.
/// - `ci-wait` is the constant **PLUS** `fix_timeout_minutes` since #2168 E1,
///   on `review-wait`'s argument rather than `fix-wait`'s, and the choice of
///   ADD over `max` is the whole of the point. That state now holds two waits
///   in SEQUENCE — the check matrix, and then the worker's read-and-report on
///   the head it pushed ([`decide_fix_receipts`]) — so a bound that is merely
///   the LARGER of the two funds only one of them. Under `max`, a repo with
///   `fix_timeout_minutes` above the constant gets a state bound EQUAL to the
///   knob, and [`decide`] reads the state bound above the state's own logic:
///   the first tick past it answers `state-stalled` and the `fix-stalled` that
///   names the pane to read never fires at all. That is the preemption
///   [`HeldReason::StateStalled`] says cannot happen, and it is the same
///   zero-margin defect #2117 review 2 found in `review-wait`, one state over.
///   Added, the constant is the slack over the second wait exactly as it is
///   there.
///
///   **The add applies to every `ci-wait` drive, including one that never
///   handed anything back**, and that is chosen rather than overlooked. A bound
///   scoped to the waiting drives would have to read [`DriveEntry::fix_pushed_ms`],
///   and `rdtick` computes `held_bound_ms` off the entry AFTER the arc into
///   `held` — the arc that clears that flag — so the notice would quote a
///   different bound from the one that fired. What a first drive pays is that a
///   check run which never resolves is caught at 150 minutes rather than 90 on
///   stock knobs. It fails toward a later park and never toward an earlier
///   false one, every wait-specific hold still fires inside it, and the
///   twelve-hour backstop is unmoved.
/// - `review-wait` is the constant **PLUS** `lane_timeout_minutes * lanes`, for
///   the reason argued below. It is the arm that is not a max, and the sentence
///   above exists because saying "max" of all four read as covering it.
///
/// # `review-wait`'s floor is the sum of the SILENCES, plus slack
///
/// `required_lanes` is how many lanes the gate requires at this head, and the
/// gate's lanes are reviewed in SEQUENCE — `first_stale_lane` picks one at a
/// time and a lane brief is not an arc, so nothing re-stamps
/// [`DriveEntry::state_since_ms`] between them. So a legitimate `review-wait`
/// can hold `required_lanes` full `lane_timeout_minutes` waits end to end, and
/// the floor has to cover their sum or the bound fires on a drive whose every
/// reviewer answered in time.
///
/// **That product alone is not enough, and the gap is what review 2 on #2117
/// found.** `lane_timeout_minutes` bounds a lane's SILENCE, measured from that
/// lane's own `spawned_ms`; this bound measures ELAPSED time in the state, from
/// `state_since_ms`. Every interval inside `review-wait` that is not one
/// lane's silence is unfunded by the product: the stretch from entering the
/// state to the first brief, and the tick-detection gap after each verdict
/// lands. With three lanes at the sixty-minute default, three reviewers each
/// answering at fifty-nine minutes and three such intervals costing ninety
/// seconds apiece crosses a bare 180-minute floor — and the margin is exactly
/// zero wherever `lane_timeout_minutes * required_lanes` reaches
/// [`REVIEW_WAIT_BOUND_MS`], which is precisely the configuration the floor
/// exists to protect. A drive whose reviewers all answered promptly would park
/// `state-stalled` naming no lane, which is the same class of false park
/// #2110 exists to remove.
///
/// So the slack is [`REVIEW_WAIT_BOUND_MS`] itself, ADDED to the product rather
/// than compared against it. It is not a tuned allowance-per-lane: an
/// allowance sized to the detection gap would be a guess about tick timing that
/// goes stale with `RD_BACKOFF_MS`, while a whole extra copy of the constant is
/// a bound whose looseness is stated rather than estimated. The cost of being
/// generous here is small and one-directional — this is the catch-all, and
/// every wait-specific hold (`lane-stalled`, `cap-full`) still fires inside it.
///
/// **The residual is that the sum can exceed the twelve-hour backstop**, and
/// then the backstop fires first because [`decide`] checks the age above the
/// state bound: `lane_timeout_minutes: 240` on a three-lane gate floors this at
/// 900 minutes, so such a drive parks `drive-stalled`. **On STOCK knobs the
/// crossover is nine lanes** — `180 + 60n >= 720` from `n = 9` — which is the
/// number an operator declaring a wide gate wants and which the worked example
/// above does not give; pinned by
/// `the_review_wait_floor_overtakes_the_backstop_at_nine_lanes_on_stock_knobs`.
/// That is degraded but not
/// the pre-#2110 notice — `held_from` is stamped on every hold arc, so the
/// `drive-stalled` notice still names the state and the time in it. Pinned by
/// `a_review_wait_floor_that_outruns_the_backstop_still_names_the_state`.
///
/// Zero required lanes (a routing answer this tick could not produce) falls
/// back to the constant plus its slack; that drive holds
/// `routing-unaccountable` on the same tick anyway.
///
/// # What these clocks charge that nobody wants them to
///
/// Two properties are disclosed rather than closed, both inherited from
/// [`DriveEntry::state_elapsed_ms`] being wall time minus cap starvation and
/// nothing else (#2117 review 3):
///
/// - **orrerix's own downtime is charged to the state.** The clocks are
///   absolute stamps, not tick counts, so a group paused or an app closed for
///   two hours while a drive sat in `ci-wait` parks it `state-stalled` on the
///   first tick after the restart. The age bound had this property before
///   #2110 and nobody hit it at four hours; these bounds are tighter, so it is
///   now reachable in an ordinary lunch break. It is not silent (the notice
///   names the state and the elapsed figure) and it is recoverable by the
///   remedy that notice prints — arc 11 re-stamps every clock. Closing it wants
///   a `last_tick_ms` and a gap-detection rule, which is a second clock with
///   its own failure mode (a drive that never ticks would never bound), and
///   that is a decision this issue did not ask for. Pinned by
///   `orrerix_downtime_is_charged_to_the_state_it_spanned`.
/// - **The `review-wait` bound moves when the GATE does**, because
///   `required_lanes` is read fresh from `facts` on every tick rather than
///   stamped when the state was entered (#2117 review 6, premortem 1). A
///   workflow edit or a path-scoped routing rule that takes a three-lane gate
///   down to one shrinks this bound from six hours to four **retroactively**,
///   so a drive five hours into a legitimate three-lane sequence parks on the
///   next tick for time it spent inside the wider bound it was actually
///   running under. Reading the count fresh is what makes the bound track the
///   gate at all, and stamping it at state entry would freeze a six-hour
///   bound onto a drive whose gate had since narrowed — the opposite error,
///   and the one that fails toward NOT parking. Disclosed rather than
///   chosen between: every test here holds `required_lanes` constant, so
///   nothing pins bound stability against a moving lane list.
/// - **A backward wall-clock step suspends every bound** rather than firing
///   one: the subtraction saturates, so `state_elapsed_ms` reads zero until the
///   clock catches up. That is the fail-safe direction — no false park — and it
///   is the same behaviour `age_ms` has had since #1778. Pinned by
///   `a_clock_that_steps_backward_suspends_the_bound_rather_than_firing_it`.
pub fn state_bound_ms(
    state: DriveState,
    limits: &DriveLimits,
    required_lanes: usize,
) -> Option<u64> {
    let lane = minutes_ms(limits.lane_timeout_minutes);
    match state {
        DriveState::CiWait => {
            Some(CI_WAIT_BOUND_MS.saturating_add(minutes_ms(limits.fix_timeout_minutes)))
        }
        DriveState::ReviewWait => Some(
            REVIEW_WAIT_BOUND_MS
                .saturating_add(lane.saturating_mul(required_lanes.max(1) as u64)),
        ),
        DriveState::FixWait => {
            Some(FIX_WAIT_BOUND_MS.max(minutes_ms(limits.fix_timeout_minutes)))
        }
        DriveState::GateCheck => Some(GATE_CHECK_BOUND_MS),
        DriveState::Held | DriveState::Satisfied | DriveState::Cancelled => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The `review-wait` floor must EXCEED the sum of the silences it covers,
    /// not equal it** (#2117 review 2, W3).
    ///
    /// `lane_timeout_minutes` bounds one lane's SILENCE, from that lane's own
    /// `spawned_ms`; `state-stalled` measures ELAPSED time, from
    /// `state_since_ms`, which nothing re-stamps between sequential lane briefs.
    /// A floor equal to the product funds the silences and nothing else, so the
    /// gaps between them — before the first brief, and after each verdict lands
    /// — come out of a margin of exactly zero, at precisely the configurations
    /// the floor exists to protect.
    ///
    /// The strict inequality is the property and the literals are the pin: the
    /// first would pass under any generous formula, and the second alone would
    /// go stale silently if the slack were ever folded back into a `max`.
    #[test]
    fn the_review_wait_floor_exceeds_the_sequential_gate_it_covers() {
        let limits = DriveLimits::default();
        let lane = minutes_ms(limits.lane_timeout_minutes);
        let bound = |lanes: usize| state_bound_ms(DriveState::ReviewWait, &limits, lanes).unwrap();

        for lanes in 1..=4 {
            assert!(
                bound(lanes) > lane * lanes as u64,
                "a {lanes}-lane gate whose reviewers each answer just inside their own \
                 timeout must not park: the floor has to fund the gaps between them too"
            );
        }

        // The literals, so folding the slack back into a `max` is not silent.
        assert_eq!(bound(1), minutes_ms(240));
        assert_eq!(bound(3), minutes_ms(360));
        // Zero required lanes reads as one — a routing answer this tick could
        // not produce, which holds `routing-unaccountable` on the same tick.
        assert_eq!(bound(0), bound(1));

        // The worked case from the finding: three reviewers each answering at
        // fifty-nine minutes, plus the THREE unfunded intervals a three-lane
        // gate has — the stretch before the first brief, and one detection gap
        // after each of the first two verdicts. Under the bare product this sum
        // parks a drive nothing was wrong with.
        //
        // The first draft of this fixture used two gaps and came to 178.5
        // minutes, which is UNDER the 180-minute product — so it asserted the
        // finding was not reachable, and CI said so. Three intervals is what
        // the mechanism actually has.
        let real = minutes_ms(59) * 3 + 90_000 * 3;
        assert!(real > lane * 3, "the fixture must exceed the bare product, or it pins nothing");
        assert!(real < bound(3), "…and must sit inside the floor as shipped");
    }

    /// **A state clock may not outrun the drive's own age** (#2117 review 2,
    /// premortem 1).
    ///
    /// `state_since_ms` is `serde(default)`, so an entry written before it
    /// existed reads zero and the raw subtraction answers `now` — epoch-scaled.
    /// The HOLD that produces is correct and argued at the field; the NUMBER was
    /// not, and `advance` stamps `held_after_ms` from it, so the notice told an
    /// operator their drive had been in `ci-wait` for some twenty thousand days.
    ///
    /// The two halves are the cap and its non-vacuity control: without the
    /// second, an implementation returning a constant zero would pass.
    #[test]
    fn a_state_clock_never_outruns_the_drives_own_age() {
        // The pre-#2110 entry, reconstructed the way serde would: a real
        // `started_ms`, and a `state_since_ms` the older build never wrote.
        let limits = DriveLimits::default();
        let mut old = entry_at(DriveState::CiWait);
        old.started_ms = 1_000;
        old.state_since_ms = 0;
        let now = 1_000 + minutes_ms(200);

        assert_eq!(
            old.state_elapsed_ms(now),
            minutes_ms(200),
            "a drive that has existed for 200 minutes cannot have been in one state longer"
        );
        // Asked of `state_bound_ms` rather than of the constant: since #2168 E1
        // `CI_WAIT_BOUND_MS` is the slack over `fix_timeout_minutes`, so a
        // comparison against it would understate the bound this half is about.
        assert!(
            old.state_elapsed_ms(now) >= state_bound_ms(DriveState::CiWait, &limits, 0).unwrap(),
            "…and it still reaches the bound, so the argued hold-then-resume is unchanged"
        );

        // The control: an ordinary entry, where the cap must not be what decides.
        let mut fresh = entry_at(DriveState::CiWait);
        fresh.started_ms = 1_000;
        fresh.state_since_ms = 1_000 + minutes_ms(150);
        assert_eq!(
            fresh.state_elapsed_ms(now),
            minutes_ms(50),
            "an entry whose state clock is younger than the drive reports the state clock"
        );
    }

    /// **A `review-wait` floor that outruns the backstop still names the
    /// state** (#2117 review 2, premortem 2).
    ///
    /// `lane_timeout_minutes: 240` on a three-lane gate floors `review-wait`
    /// above the twelve-hour age, and [`decide`] checks the age first — so that
    /// drive parks `drive-stalled` and the state bound is unreachable for it.
    /// That is a real residual and it is disclosed at `state_bound_ms`.
    ///
    /// What it is NOT is the pre-#2110 notice, and this pins the difference:
    /// `held_from` is stamped on every arc into `held`, so the hold still
    /// records which state the drive was in and for how long. The reviewer's
    /// premise — "parks `drive-stalled` naming no state" — is the half that does
    /// not hold, and an assertion is worth more here than a correction in prose.
    #[test]
    fn a_review_wait_floor_that_outruns_the_backstop_still_names_the_state() {
        let limits = DriveLimits::new(3, 3, 1, 240, 60, 720);
        assert!(
            state_bound_ms(DriveState::ReviewWait, &limits, 3).unwrap()
                > minutes_ms(limits.drive_timeout_minutes),
            "the fixture must actually be the configuration the premortem names"
        );

        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        let past = 1_000 + minutes_ms(limits.drive_timeout_minutes);
        let facts = DriveFacts {
            now_ms: past,
            required_lanes: Some(vec![
                lane_fact("rev-std", None, "", ""),
                lane_fact("rev-two", None, "", ""),
                lane_fact("rev-final", None, "", ""),
            ]),
            ..facts_at("head-a")
        };
        assert_eq!(
            decide(&e, &facts, &limits),
            DriveStep::held(HeldReason::DriveStalled),
            "the age outranks a state bound this config put out of reach"
        );

        // …and the hold still carries what the drive was doing, which is the
        // whole of #2110's third ask and the half the premortem got wrong.
        e.advance(DriveState::Held, Some(HeldReason::DriveStalled), None, past).unwrap();
        assert_eq!(e.held_from, Some(DriveState::ReviewWait));
        assert_eq!(
            e.held_after_ms,
            minutes_ms(limits.drive_timeout_minutes),
            "…and for how long, which is what the notice interpolates"
        );
    }

    /// **orrerix's own downtime is charged to the state it spanned** (#2117
    /// review 3, premortem 1) — disclosed at [`state_bound_ms`], and pinned here
    /// so the disclosure cannot go false with nothing red to say so.
    ///
    /// The clocks are absolute stamps rather than tick counts, so a group paused
    /// or an app closed across a gap is indistinguishable from a drive that sat
    /// in `ci-wait` for that long. The age bound had this property before #2110
    /// and nobody reached it at four hours; at the per-state bounds it is an
    /// unattended afternoon, and on the three arms shorter than `ci-wait`'s it
    /// is less than that.
    ///
    /// **The gap this test uses is derived from the bound rather than written**,
    /// because the figure moved once already: it was a literal two hours against
    /// a bare ninety-minute `ci-wait` bound, and #2168 E1 made that arm the
    /// constant plus `fix_timeout_minutes`.
    ///
    /// **This pins the residual itself, not a fix**, and the second half is what
    /// makes that honest: the drive is recoverable by the remedy its own notice
    /// prints, because arc 11 re-stamps every clock. A pin on the first half
    /// alone would read as a defect nobody had thought about.
    #[test]
    fn orrerix_downtime_is_charged_to_the_state_it_spanned() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::CiWait);
        e.head = "head-a".into();
        // One tick at 1_000, then nothing at all — orrerix was not running. No
        // starvation was recorded, because no tick was there to record one.
        //
        // **The gap is derived from the bound and not written as a number.** It
        // was a literal 120 minutes, chosen when `ci-wait`'s bound was the bare
        // `CI_WAIT_BOUND_MS`; #2168 E1 made that arm the constant PLUS
        // `fix_timeout_minutes`, and a literal that no longer crosses the bound
        // stops witnessing the residual while still reading like a lunch break.
        // Asked of `state_bound_ms` instead, so the next change to that arm
        // moves the specimen with it (CLAUDE.md: a test's specimen must stay a
        // member of the class it witnesses).
        let bound = state_bound_ms(DriveState::CiWait, &limits, 0).unwrap();
        let after_gap = 1_000 + bound;
        assert_eq!(
            decide(&e, &DriveFacts { now_ms: after_gap, ..facts_at("head-a") }, &limits),
            DriveStep::held(HeldReason::StateStalled),
            "the gap is charged to `ci-wait`, which is the disclosed residual — if this ever \
             stops being true, `state_bound_ms`'s downtime paragraph is what needs rewriting"
        );

        // …and the remedy the notice prints really does clear it: arc 11
        // re-stamps `state_since_ms`, so the drive goes back to work rather than
        // re-holding on its first tick.
        e.advance(DriveState::Held, Some(HeldReason::StateStalled), None, after_gap).unwrap();
        e.advance(DriveState::CiWait, None, None, after_gap).unwrap();
        assert_eq!(
            e.state_elapsed_ms(after_gap),
            0,
            "a resume must start the state clock over, or the hold is unrecoverable"
        );
        assert_eq!(
            decide(&e, &DriveFacts { now_ms: after_gap + 1_000, ..facts_at("head-a") }, &limits),
            DriveStep::Wait,
            "…and the very next tick must not re-hold on the gap it already reported"
        );
    }

    /// **A backward wall-clock step suspends the bound rather than firing it**
    /// (#2117 review 3, premortem 2) — the fail-safe direction, disclosed at
    /// [`state_bound_ms`] and pinned here.
    ///
    /// Every clock here is a `saturating_sub` against an absolute stamp, so a
    /// `now` behind `state_since_ms` reads zero rather than wrapping to
    /// `u64::MAX` and parking the drive instantly. The three halves are the
    /// suspension, its non-vacuity control (the same entry at a forward clock
    /// DOES hold, so the first assertion is not passing because nothing ever
    /// holds), and the recovery once the clock is ahead again.
    #[test]
    fn a_clock_that_steps_backward_suspends_the_bound_rather_than_firing_it() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::CiWait);
        e.head = "head-a".into();
        e.state_since_ms = minutes_ms(500);
        e.started_ms = minutes_ms(500);

        // The clock steps back behind the stamp.
        let behind = minutes_ms(400);
        assert_eq!(e.state_elapsed_ms(behind), 0, "saturating, not wrapping");
        assert_eq!(e.bounded_age_ms(behind), 0, "…and the age with it");
        assert_eq!(
            decide(&e, &DriveFacts { now_ms: behind, ..facts_at("head-a") }, &limits),
            DriveStep::Wait,
            "a clock that went backwards must not park a drive; failing toward WAIT is the \
             only safe direction here"
        );

        // The control: the same entry at a forward clock past the bound holds,
        // so the assertion above is about the step and not about a drive that
        // could never hold at all. **The bound is asked of `state_bound_ms`**,
        // not restated as `CI_WAIT_BOUND_MS` — since #2168 E1 that constant is
        // the SLACK over `fix_timeout_minutes` rather than the whole bound, and
        // a control that stops crossing it is a control that stops controlling.
        let ahead = minutes_ms(500) + state_bound_ms(DriveState::CiWait, &limits, 0).unwrap();
        assert_eq!(
            decide(&e, &DriveFacts { now_ms: ahead, ..facts_at("head-a") }, &limits),
            DriveStep::held(HeldReason::StateStalled),
            "the same entry past its bound on a forward clock must still park"
        );
    }

    /// **Where the `review-wait` floor overtakes the backstop, on STOCK
    /// knobs** (#2117 review 4's premortem) — the crossover asserted rather
    /// than left to arithmetic in a doc comment.
    ///
    /// The residual was already disclosed at [`state_bound_ms`], but only with
    /// a worked example on a NON-default `lane_timeout_minutes: 240`, and the
    /// floor test beside it covers one to four lanes. Neither says where the
    /// crossover actually is at defaults, which is the number an operator
    /// declaring a wide gate would want. It is **nine**: `180 + 60n >= 720`
    /// from `n = 9`, so a nine-lane gate on stock knobs already has an
    /// unreachable state bound, one lane earlier than the review's estimate of
    /// ten.
    ///
    /// The two halves are the last reachable width and the first unreachable
    /// one, so this fails if the crossover moves in either direction — which it
    /// does if any of the three numbers involved is retuned.
    #[test]
    fn the_review_wait_floor_overtakes_the_backstop_at_nine_lanes_on_stock_knobs() {
        let limits = DriveLimits::default();
        let backstop = minutes_ms(limits.drive_timeout_minutes);
        let bound = |lanes: usize| state_bound_ms(DriveState::ReviewWait, &limits, lanes).unwrap();

        assert_eq!(bound(8), minutes_ms(660));
        assert!(
            bound(8) < backstop,
            "an eight-lane gate on stock knobs must still be able to reach its state bound"
        );
        assert_eq!(bound(9), minutes_ms(720));
        assert!(
            bound(9) >= backstop,
            "…and at nine the floor has overtaken the twelve-hour backstop, so `decide`'s \
             age check — which runs first — is what such a drive parks on. Disclosed at \
             `state_bound_ms`; this is where it starts"
        );
    }
}
