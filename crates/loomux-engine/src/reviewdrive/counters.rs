//! §2.3 the counters and the bounds they run against: [`DriveLimits`],
//! [`Counters`], [`Counter`], and #2509's one-shot grace for a body-only fail.
//!
//! Part of the review driver's pure core, split out of the former single-file
//! `reviewdrive.rs` by #3498 P7 as a pure move. The module map is in `mod.rs`;
//! the design note is `docs/design/review-driver.md`.

use super::*;

// ── §2.3 the counters, and the bounds they run against ──────────────────────
//
// INVARIANT 9 in `templates/orchestrator.md` reads: *three CI attempts, three
// rounds of review findings (yours count too), one rebase attempt, one
// architectural bounce.* The driver takes three of those four and deliberately
// leaves the fourth alone — an architectural bounce is INVARIANT 4 judgment,
// and §3 says the driver never makes one.

/// The ceiling INVARIANT 9 sets on `max_review_rounds` and `max_ci_attempts`,
/// and the top of §5.3's `1..=3` range.
pub const MAX_ROUNDS_CEILING: u32 = 3;
/// The ceiling on `max_rebase_attempts`, and the top of §5.3's `0..=1` range.
pub const MAX_REBASE_CEILING: u32 = 1;

/// The bounds one drive runs against — the value type [`decide`] consumes.
///
/// **This is not a second parser of the `driver:` block.** §5.3's block is
/// S2's, in `workflow/parse.rs`, and that is where a malformed block goes loudly down
/// the `workflow-invalid` path. What lives here is the *value* the pure core is
/// handed.
///
/// **The clamp only ever tightens, and it is a capability boundary rather than
/// input hygiene.** §2.3: a repo may run a *tighter* loop than the orchestrator
/// template promises; it may not run a looser one, because the driver acts on
/// the orchestrator's authority and a repo file that raised the bound would be
/// loosening the orchestrator's own invariant from a configuration file. That
/// is `docs/design/workflows.md`'s closure exactly — **a workflow file may
/// select from what loomux permits and may never widen it** — so a
/// `driver.max_review_rounds: 9` that reached a decision would be a repo file
/// granting a capability, not a validation slip.
///
/// **Two independent layers hold it, and neither is allowed to rely on the
/// other.** S2 refuses or clamps out-of-range values as it parses `driver:`;
/// [`decide`] clamps again on the values it actually reads. The second is not
/// redundant — [`decide`] is a `pub fn` over a plain value type, so any caller
/// in any crate can reach it without passing through S2's parser at all, and a
/// boundary that holds only when the expected caller is upstream is not a
/// boundary. Round 21 in the PR is the counterfactual for this arm.
///
/// The type carries a private field so the struct cannot be built by *literal*
/// outside this module (E0451): every construction path a caller can reach
/// ([`DriveLimits::new`], [`Default`], [`DriveLimits::clamped`]) clamps.
///
/// **That seal does not make an out-of-range value unspellable, and claiming it
/// did was weaker as well as false.** The bounds fields are `pub` — they are
/// meant to be read — so a caller outside this module can take a clamped value
/// from any of those constructors and then assign to one:
/// `let mut l = DriveLimits::default(); l.max_review_rounds = 9;` compiles
/// anywhere. What the seal actually buys is that such a value can only arise
/// by a *deliberate* post-construction write, never by someone filling in a
/// struct literal without noticing there was a range.
///
/// The load-bearing statement is the stronger one, and it is about reach rather
/// than spelling: **an out-of-range bound cannot reach a decision.** [`decide`]
/// clamps unconditionally — no `if`, no caller opt-out — and *shadows* its own
/// binding with the clamped value, so every read below that line is of a
/// clamped bound and there is no path through the function that consults the
/// argument as passed. That is a property of one function anyone can re-read,
/// which is why it is the claim worth making; "cannot be spelled" was a claim
/// about the whole type system and was not true.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DriveLimits {
    pub max_review_rounds: u32,
    pub max_ci_attempts: u32,
    pub max_rebase_attempts: u32,
    pub lane_timeout_minutes: u64,
    pub fix_timeout_minutes: u64,
    pub drive_timeout_minutes: u64,
    /// **How many non-blocking rounds the driver may run on its own** (#3367
    /// item 1) — `driver.fix_nonblocking_rounds`, `0..=3`, default `0`, which
    /// is today's behaviour: a gate satisfied with findings open wakes the
    /// orchestrator at once.
    ///
    /// **It is not a second budget.** Every such round ALSO spends
    /// `review_rounds` (see [`Counter::NonblockingRound`]), so this can only
    /// ever SHORTEN what INVARIANT 9's bound already allows; it can never let a
    /// drive run past `max_review_rounds`. Clamped by [`clamped`](Self::clamped)
    /// to the same ceiling, for the reach argument the type's own doc makes.
    pub fix_nonblocking_rounds: u32,
    /// Private to `reviewdrive`, and load-bearing: it makes `DriveLimits { … }`
    /// a compile error outside that module (E0451), so the clamping constructors
    /// are the only way in. The fields stay `pub` so a caller can still *read*
    /// the bounds — what is closed is authoring one, not inspecting it.
    /// `pub(super)` since #3498 P7 split the module into files: a sibling
    /// file's test spells `..DriveLimits::default()`, which needs every field
    /// visible, and `pub(super)` from here is exactly the old private scope.
    pub(super) _seal: (),
}

impl Default for DriveLimits {
    /// §5.3's defaults, which are already inside every range.
    fn default() -> Self {
        DriveLimits {
            max_review_rounds: 3,
            max_ci_attempts: 3,
            max_rebase_attempts: 1,
            lane_timeout_minutes: 60,
            fix_timeout_minutes: 60,
            drive_timeout_minutes: DRIVER_DRIVE_TIMEOUT_DEFAULT_MIN as u64,
            fix_nonblocking_rounds: 0,
            _seal: (),
        }
    }
}

impl DriveLimits {
    /// Every counter bound brought inside §5.3's range. `1..=3` for the two
    /// round counters, `0..=1` for rebases — note the asymmetric floor: zero
    /// review rounds would be a drive that parks on the first `fail` having
    /// handed nothing back, while zero rebase attempts is a coherent policy
    /// (park on the first conflict, §2.2's `rebase-limit`).
    pub fn clamped(self) -> DriveLimits {
        DriveLimits {
            max_review_rounds: self.max_review_rounds.clamp(1, MAX_ROUNDS_CEILING),
            max_ci_attempts: self.max_ci_attempts.clamp(1, MAX_ROUNDS_CEILING),
            max_rebase_attempts: self.max_rebase_attempts.min(MAX_REBASE_CEILING),
            // #3367: `0` is legal here (it is the default, and the feature
            // off), so this is a ceiling and not a range — the asymmetry
            // `max_rebase_attempts` has, for the same reason.
            fix_nonblocking_rounds: self.fix_nonblocking_rounds.min(MAX_ROUNDS_CEILING),
            ..self
        }
    }

    /// These limits with `driver.fix_nonblocking_rounds` set (#3367 item 1),
    /// clamped like every other bound.
    ///
    /// A builder rather than a seventh argument to [`new`](Self::new): every
    /// existing caller keeps meaning exactly what it meant — the feature off —
    /// without an edit, which is the default the note promises for a repo that
    /// does not write the key.
    pub fn with_fix_nonblocking_rounds(self, rounds: u32) -> DriveLimits {
        DriveLimits { fix_nonblocking_rounds: rounds, ..self }.clamped()
    }

    /// The only way to build a `DriveLimits` from outside this module, and it
    /// clamps. S3 maps S2's parsed `driver:` block through here; the timeouts
    /// pass through unclamped because §5.3 does not bound them against
    /// INVARIANT 9 — they are pacing, not budget.
    pub fn new(
        max_review_rounds: u32,
        max_ci_attempts: u32,
        max_rebase_attempts: u32,
        lane_timeout_minutes: u64,
        fix_timeout_minutes: u64,
        drive_timeout_minutes: u64,
    ) -> DriveLimits {
        DriveLimits {
            max_review_rounds,
            max_ci_attempts,
            max_rebase_attempts,
            lane_timeout_minutes,
            fix_timeout_minutes,
            drive_timeout_minutes,
            fix_nonblocking_rounds: 0,
            _seal: (),
        }
        .clamped()
    }
}

/// What a drive has spent (§5.2's `counters`).
///
/// **The comparison against a bound is check-before-bump**, and that is a
/// decision with evidence rather than a coin flip, because the two orderings
/// differ by a whole round. [`counter_exhausted`] is where it is spelled;
/// §2.2's `rebase-limit` row is what decides it.
/// **Every COUNT is required, not just the block.** `DriveEntry::counters`
/// carries no `serde(default)` for the reason on that field — zeros silently
/// grant a full fresh budget — and a per-field default on a count would have
/// reopened the same hole one level down, where `"counters": {}` parses to three
/// zeros and `"counters": {"review_rounds": 2}` quietly forgives the CI attempts.
/// The block being mandatory is worth nothing if its contents are optional.
///
/// **The one `serde(default)` below is `body_only_grace`, and it is an argued
/// exception rather than a hole in that rule** (#2509): it is a bool, not a
/// count, a defaulted `false` grants one round once rather than a whole fresh
/// budget, and it is the TRUE reading of a file written before the field
/// existed. The full argument is on the field. Read the rule above as being
/// about the three counts — a NEW count still takes no default.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counters {
    pub review_rounds: u32,
    pub ci_attempts: u32,
    pub rebase_attempts: u32,
    /// **#2509's one-shot grace — the only field here that is a BOOL rather
    /// than a count.** Whether this drive has already spent its single extra
    /// review round for a body-only blocking fail (§2.3).
    ///
    /// It is a SEPARATE budget on purpose, and that is the whole of what
    /// "grace is inside the ceiling" means. `review_rounds` still stops dead at
    /// `max_review_rounds` and is never bumped past it, so [`MAX_ROUNDS_CEILING`]
    /// bounds exactly what it bounded before; the extra round is funded from
    /// here instead, once, which caps a drive at `max_review_rounds + 1` review
    /// rounds and no more. Funding it out of `review_rounds` was the other
    /// reading and is vacuous at stock knobs — the default `max_review_rounds`
    /// IS the ceiling — so the feature would have helped only a repo that had
    /// lowered its own bound, and never the case it was filed for.
    ///
    /// **`serde(default)` here, and NOT on the three counts above.** The
    /// argument on those is that a defaulted zero silently re-grants a whole
    /// fresh budget, so the conservative direction is to refuse the file. A
    /// defaulted `false` grants one round once, and it is also the TRUE reading
    /// of a file written before this field existed: that drive cannot have
    /// spent a grace the build that wrote it had never shipped. Refusing to
    /// parse every in-flight drive at upgrade is the worse failure, and §5.2's
    /// posture for machine-authored state is that it degrades rather than
    /// fails loud.
    #[serde(default)]
    pub body_only_grace: bool,
    /// Preserved unknown fields — see [`ReviewDrivesState`].
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Counters {
    /// Counters seeded by `drive_review`'s `rounds_already_spent` (§2.3).
    ///
    /// "Yours count too" is a property of the **budget**, not of who spends it.
    /// An orchestrator that reviews by hand once, gets a `fail`, and *then*
    /// calls `drive_review` would otherwise start every counter at zero and
    /// spend three more, for five against an invariant of three. Clamped
    /// `0..=3` exactly as §2.3 specifies, so the seed cannot exceed the budget
    /// it is spending from.
    pub fn seeded(rounds_already_spent: u32) -> Counters {
        Counters {
            review_rounds: rounds_already_spent.min(MAX_ROUNDS_CEILING),
            ..Counters::default()
        }
    }
}

/// Which counter a step spends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counter {
    /// Spent on a lane's `fail` (§2.1's `review-wait` row).
    ReviewRounds,
    /// Spent on a red CI observation (§2.1's `ci-wait` row).
    CiAttempts,
    /// Spent on a CONFLICTING mergeability (§2.1's `ci-wait` row).
    RebaseAttempts,
    /// Spent on a body-only `fail` recorded AT the review bound (#2509) — the
    /// one-shot grace, and the only variant here that spends a bool rather
    /// than a count. See [`Counters::body_only_grace`].
    BodyOnlyGrace,
    /// Spent on the arc a SATISFIED gate takes back to the worker because every
    /// required lane passed with only non-blocking findings open (#3367 item 1).
    ///
    /// **It spends `review_rounds` as well as its own count**, and that is the
    /// whole of "the existing bound is SHARED": a hand-back the driver makes on
    /// its own initiative is a review round the orchestrator did not make, and
    /// INVARIANT 9's "yours count too" is a property of the budget, not of who
    /// spends it. `decide` checks both bounds before proposing it, and
    /// [`DriveEntry::advance`] bumps both in the one place the arc and its cost
    /// cannot come apart.
    NonblockingRound,
}

/// Whether `spent` has reached `bound` — **checked before the bump, never
/// after**, which is the whole of the ordering decision and is worth its own
/// function so both callers and both tests name the same thing.
///
/// The two orderings are not equivalent, they differ by one hand-back, and
/// §2.2's `rebase-limit` row settles it: *"a second conflict after the one
/// rebase hand-back"*. At `max_rebase_attempts = 1`, check-before-bump gives
/// exactly that — the first conflict finds `0 < 1`, bumps to 1 and hands back
/// (the one rebase hand-back); the second finds `1 >= 1` and parks. Bump-then-
/// check would park on the *first* conflict, having handed nothing back, which
/// is not what that row describes.
///
/// It is also the only ordering under which §2.3's `rounds_already_spent`
/// honours "yours count too" rather than overshooting the other way: seeded at
/// 2 of 3 it leaves exactly one driven round, where bump-then-check leaves
/// none at all and makes the parameter a way to disable the drive.
pub fn counter_exhausted(spent: u32, bound: u32) -> bool {
    spent >= bound
}

/// **Does #2509's one-shot grace apply to the `fail` this lane has just
/// recorded at the review bound?**
///
/// A `held(review-limit)` costs the orchestrator three wakes and a hand-written
/// brief: cancel, resume the worker by hand, re-drive with
/// `rounds_already_spent`. INVARIANT 9 bounds review rounds so that a reviewer
/// surfacing one new nit per round cannot run for ever — and a blocking fail on
/// the PR **body**, at a head that has not moved since a full round was already
/// spent on it, is not that shape. It is a text edit the worker can make in one
/// turn with nothing to build and nothing to re-check. PR #2397 reached the
/// bound on two sentences.
///
/// # What it reads, and what it deliberately does not
///
/// It reads the **brief this drive sent** — [`LaneRecord::briefed_body_only`],
/// stamped by [`DriveEntry::open_lane`] from the step that chose the lane — and
/// never the reviewer's prose, never a word the reviewer could type. #2509
/// considered a `body_only` parameter on `review_verdict` and rejected it:
/// `workflow/verdict.rs`'s line-5 marker is placed where it is precisely because "a
/// marker a reviewer could type would be a marker a reviewer could forge", and
/// a reviewer that can mark its own fail body-only can buy itself a round.
///
/// The pair is matched **exactly**, not through [`lane_open_for`]'s
/// unknown-tolerant comparison, and an empty digest never matches: this is a
/// grant, and a brief whose revision cannot be pinned grants nothing. That is
/// [`LaneRecord::briefed_verify`]'s posture one grant over.
///
/// # The residual, disclosed and bounded
///
/// The mark says the driver **asked** about the body alone, not that the
/// findings that came back are about the body. A reviewer answering a body-only
/// re-brief with a code nit it missed a round earlier still earns the grace.
/// That is the honest cost of deriving the bit rather than trusting a
/// reviewer's word for it, and it is contained by construction rather than
/// argued away: at most ONE extra round, once per drive, and only ever on code
/// that a full review round has already been spent on.
pub fn body_only_grace_applies(
    entry: &DriveEntry,
    block: &str,
    head: &str,
    digest: Option<&str>,
) -> bool {
    // Spent once and never again — §2.3's "never stacking". Asked FIRST so the
    // rest reads as the grant's preconditions rather than as its budget.
    if entry.counters.body_only_grace {
        return false;
    }
    let Some(digest) = digest.filter(|d| !d.is_empty()) else { return false };
    entry.lane(block).is_some_and(|r| {
        r.briefed_body_only && r.briefed_head == head && r.briefed_digest == digest
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── §2.3 the counters ───────────────────────────────────────────────────

    #[test]
    fn the_bound_is_checked_before_the_bump_which_is_the_rebase_rows_arithmetic() {
        // §2.2's `rebase-limit` row is the decisive wording: "a second conflict
        // after the one rebase hand-back". At max_rebase_attempts = 1 that is
        // hand back once, park on the second — which only check-before-bump
        // produces. Bump-then-check parks on the FIRST conflict, having handed
        // nothing back, and no reading of that row describes it.
        let limits = DriveLimits::default();
        assert_eq!(limits.max_rebase_attempts, 1);
        assert!(!counter_exhausted(0, limits.max_rebase_attempts));
        assert!(counter_exhausted(1, limits.max_rebase_attempts));

        let mut e = entry_at(DriveState::CiWait);
        let facts = DriveFacts {
            ci: CiObservation::Conflicting,
            ..facts_at("head-a")
        };
        // First conflict: the one rebase hand-back.
        assert_eq!(
            decide(&e, &facts, &limits),
            DriveStep::Advance {
                to: DriveState::FixWait,
                held_reason: None,
                bump: Some(Counter::RebaseAttempts),
            }
        );
        e.counters.rebase_attempts += 1;
        // Second conflict: parked.
        assert_eq!(
            decide(&e, &facts, &limits),
            DriveStep::Advance {
                to: DriveState::Held,
                held_reason: Some(HeldReason::RebaseLimit),
                bump: None,
            }
        );
    }

    #[test]
    fn rounds_already_spent_leaves_the_rest_of_the_budget_not_none_of_it() {
        // §2.3: "yours count too" is a property of the budget, not of who
        // spends it. Seeded at 2 of 3, exactly one driven round remains — the
        // reading under which the parameter bounds the loop rather than
        // disabling it.
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        e.counters = Counters::seeded(2);
        assert_eq!(e.counters.review_rounds, 2);
        let facts = DriveFacts {
            required_lanes: Some(vec![lane_fact("rev-std", Some(Verdict::Fail), "head-a", "d1")]),
            ..facts_at("head-a")
        };
        assert_eq!(
            decide(&e, &facts, &limits),
            DriveStep::Advance {
                to: DriveState::FixWait,
                held_reason: None,
                bump: Some(Counter::ReviewRounds),
            },
            "one round of the three is still unspent"
        );
        e.counters.review_rounds += 1;
        assert_eq!(
            decide(&e, &facts, &limits),
            DriveStep::Advance {
                to: DriveState::Held,
                held_reason: Some(HeldReason::ReviewLimit),
                bump: None,
            },
            "and the next fail parks at 3/3"
        );
    }

    #[test]
    fn a_repo_cannot_raise_invariant_9_by_handing_decide_a_wider_bound() {
        // THE capability test, and it is deliberately built from RAW limits.
        // Every other `decide` fixture uses `DriveLimits::default()`, which is
        // already inside every range — so the axis §2.3 makes load-bearing was
        // constant across the whole suite, and the property read green under an
        // implementation that did not have it (`clamped()` was called only from
        // tests, and `decide` used the raw values). A fixture that cannot vary
        // the axis cannot witness it.
        //
        // `docs/design/workflows.md`: a workflow file selects from what loomux
        // permits and never widens it. A `driver:` block asking for nine review
        // rounds must get three at the decision, not nine.
        let wide = DriveLimits {
            max_review_rounds: 9,
            max_ci_attempts: 9,
            max_rebase_attempts: 9,
            ..DriveLimits::default()
        };
        assert_eq!(wide.max_review_rounds, 9, "the fixture really is over-bound");

        // Review rounds: at 3 spent, a further `fail` must PARK, not hand back.
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        e.counters.review_rounds = MAX_ROUNDS_CEILING;
        let failing = DriveFacts {
            required_lanes: Some(vec![lane_fact("rev-std", Some(Verdict::Fail), "head-a", "d1")]),
            ..facts_at("head-a")
        };
        assert_eq!(
            decide(&e, &failing, &wide),
            DriveStep::held(HeldReason::ReviewLimit),
            "a repo file must not buy a fourth review round"
        );

        // CI attempts, same shape.
        let mut c = entry_at(DriveState::CiWait);
        c.counters.ci_attempts = MAX_ROUNDS_CEILING;
        let red = DriveFacts { ci: CiObservation::Red, ..facts_at("head-a") };
        assert_eq!(
            decide(&c, &red, &wide),
            DriveStep::held(HeldReason::CiLimit)
        );

        // Rebases: the ceiling is 1, so a second conflict parks however wide
        // the file asked to be.
        let mut r = entry_at(DriveState::CiWait);
        r.counters.rebase_attempts = MAX_REBASE_CEILING;
        let conflict = DriveFacts { ci: CiObservation::Conflicting, ..facts_at("head-a") };
        assert_eq!(
            decide(&r, &conflict, &wide),
            DriveStep::held(HeldReason::RebaseLimit)
        );

        // The negative control: the same raw fixture one under each ceiling
        // still hands back, so the assertions above are the clamp biting and
        // not `decide` refusing everything.
        let mut ok = entry_at(DriveState::ReviewWait);
        ok.head = "head-a".into();
        ok.counters.review_rounds = MAX_ROUNDS_CEILING - 1;
        assert_eq!(
            decide(&ok, &failing, &wide),
            DriveStep::spend(DriveState::FixWait, Counter::ReviewRounds)
        );
    }

    #[test]
    fn an_unresolved_head_stops_the_tick_instead_of_spawning_a_reviewer() {
        // The failed head read. Without the guard in `decide`, arc 6's own
        // `!facts.head.is_empty()` check skips itself, `first_stale_lane` finds
        // every pass stale against "" (`reviewed("")` is false for any real
        // verdict head), and `lane_open_for` refuses every record briefed at a
        // real head — so the tick returns OpenLane{0} and S3 spawns a reviewer.
        // EVERY tick. And because each brief re-arms `spawned_ms`,
        // `lane-stalled` never fires: the loop disables the bound meant to
        // catch it.
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        e.open_lane("rev-std", "s1", "rev-1", "head-a", Some("d1"), 1_000, false, false);
        let blind = DriveFacts {
            required_lanes: Some(vec![lane_fact("rev-std", Some(Verdict::Pass), "head-a", "d1")]),
            head: String::new(),
            ..facts_at("head-a")
        };
        assert_eq!(
            decide(&e, &blind, &limits),
            DriveStep::Wait,
            "an unresolved head must not brief a lane"
        );

        // Not a hold either — a `gh` blip is not a stall, and §8 backs off.
        // The same entry with the head readable still makes progress, which is
        // the control that stops this passing by refusing everything.
        assert_eq!(
            decide(&e, &facts_at("head-a"), &limits),
            DriveStep::to(DriveState::GateCheck)
        );

        // ...and declining costs no boundedness: the age bound needs no head.
        let aged = DriveFacts {
            now_ms: 1_000 + minutes_ms(limits.drive_timeout_minutes),
            head: String::new(),
            ..facts_at("head-a")
        };
        assert_eq!(
            decide(&e, &aged, &limits),
            DriveStep::held(HeldReason::DriveStalled)
        );
    }

    #[test]
    fn a_transition_and_its_cost_cannot_come_apart() {
        // `advance` took a reason but not a bump, so a caller could take arc 5
        // and forget the increment — INVARIANT 9 defeated by an omission.
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        e.advance(DriveState::FixWait, None, Some(Counter::ReviewRounds), 2_000)
            .unwrap();
        assert_eq!(e.counters.review_rounds, 1);
        assert_eq!(e.counters.ci_attempts, 0, "only the named counter moves");

        // A REFUSED arc spends nothing — the bump lands after the transition is
        // accepted, never before.
        let mut t = entry_at(DriveState::Satisfied);
        let before = t.counters.clone();
        assert!(t
            .advance(DriveState::CiWait, None, Some(Counter::CiAttempts), 3_000)
            .is_err());
        assert_eq!(t.counters, before, "a refused transition costs nothing");

        // `take` applies a whole step, so the tick cannot apply one half.
        let mut s = entry_at(DriveState::CiWait);
        let step = DriveStep::spend(DriveState::FixWait, Counter::CiAttempts);
        s.take(&step, 4_000).unwrap();
        assert_eq!(s.state(), DriveState::FixWait);
        assert_eq!(s.counters.ci_attempts, 1);
        // OpenLane needs a spawned session id, so `take` leaves it to S3.
        let mut o = entry_at(DriveState::ReviewWait);
        o.take(&DriveStep::OpenLane { index: 0, verify: false, body_only: false }, 5_000).unwrap();
        assert_eq!(o.state(), DriveState::ReviewWait);
        assert!(o.lanes.is_empty());
    }

    #[test]
    fn a_partial_counter_block_refuses_the_file() {
        // The block being mandatory is worth nothing if its contents are
        // optional: `"counters": {}` would parse to three zeros, which is the
        // full fresh budget the required-block rule exists to deny.
        for partial in [
            r#""counters": {},"#,
            r#""counters": { "review_rounds": 2 },"#,
            r#""counters": { "ci_attempts": 1, "rebase_attempts": 0 },"#,
        ] {
            let bad = NOTE_EXAMPLE.replace(
                r#""counters": { "review_rounds": 1, "ci_attempts": 0, "rebase_attempts": 0 },"#,
                partial,
            );
            assert!(bad.contains(partial), "the mutation must actually land");
            assert!(
                matches!(parse_state(&bad), Err(StateError::Malformed(_))),
                "{partial} must refuse"
            );
        }
        // The complete block still parses, so the loop above is not refusing
        // everything.
        assert!(parse_state(NOTE_EXAMPLE).is_ok());
    }

    #[test]
    fn interception_is_keyed_on_the_agent_and_an_unrecorded_pane_fails_closed() {
        // §7's first bounding property: "It is keyed on the agent, never on a
        // `ref` string a delegate typed, because a delegate that could choose
        // whether its report reaches the orchestrator by naming a PR number is
        // a delegate that can route around the orchestrator."
        let mut e = entry_at(DriveState::ReviewWait);
        e.open_lane("rev-std", "s1", "rev-4", "head-a", Some("d1"), 1_000, false, false);
        e.worker_agent = "w-7".into();

        assert_eq!(e.driven_role("rev-4"), Some(lane_pane("rev-std", true)));
        assert_eq!(e.driven_role("w-7"), Some(worker_pane(true)));
        // A delegate this drive did not spawn is not this drive's, however
        // plausible its id: nothing is inferred from a prefix or a role.
        assert_eq!(e.driven_role("rev-5"), None);
        assert_eq!(e.driven_role("w-70"), None);
        assert_eq!(e.driven_role("orch-1"), None);

        // The fail-closed half. A drive that has never handed back carries an
        // empty `worker_agent`, and a lane recorded before the field existed
        // carries an empty `agent`. Neither may own a caller — without the
        // guard, an empty id would match and the drive would consume the
        // traffic of a delegate it cannot prove it spawned.
        let mut fresh = entry_at(DriveState::CiWait);
        assert_eq!(fresh.worker_agent, "", "a fresh drive has resumed nobody");
        assert_eq!(fresh.driven_role(""), None);
        fresh.open_lane("rev-std", "s1", "", "head-a", Some("d1"), 1_000, false, false);
        assert_eq!(fresh.driven_role(""), None, "an unrecorded pane owns no caller");
        // ...and the positive control, so the four `None`s above are the guard
        // and not a method that answers `None` to everything.
        fresh.worker_agent = "w-1".into();
        assert_eq!(fresh.driven_role("w-1"), Some(worker_pane(true)));
    }

    fn worker_pane(current: bool) -> DrivenPane {
        DrivenPane { role: DrivenRole::Worker, current }
    }

    fn lane_pane(block: &str, current: bool) -> DrivenPane {
        DrivenPane { role: DrivenRole::Lane(block.into()), current }
    }

    /// **#1871 B2, at the layer that owns the key.** A drive that hands back or
    /// re-briefs twice must still recognise the pane it opened first: that pane
    /// is live, on the same session, working the same PR, and its `report` is
    /// exactly the traffic §7 exists to absorb. Before this the assignment was a
    /// single slot, so the second spawn evicted the first pane's id outright and
    /// its reports reached the orchestrator as if nobody owned it.
    ///
    /// The second half is the one the fix could get wrong in the other
    /// direction: owning a superseded pane must not mean TAKING ITS WORD. Each
    /// assertion therefore pins `current` as well as the role — a version that
    /// answered `current: true` for every owned pane would satisfy a role-only
    /// test and let a two-heads-stale `done` advance the drive.
    #[test]
    fn a_superseded_pane_is_still_owned_and_is_never_current() {
        let mut e = entry_at(DriveState::ReviewWait);
        e.record_worker_pane("w-1");
        e.record_worker_pane("w-2");
        assert_eq!(e.driven_role("w-2"), Some(worker_pane(true)), "the latest pane is current");
        assert_eq!(
            e.driven_role("w-1"),
            Some(worker_pane(false)),
            "the pane the second hand-back superseded is still this drive's — #1871 B2"
        );

        // The lane's sibling arc: `open_lane` replaces the record wholesale.
        e.open_lane("rev-std", "s1", "rev-1", "head-a", Some("d1"), 1_000, false, false);
        e.open_lane("rev-std", "s1", "rev-2", "head-b", Some("d1"), 2_000, false, false);
        assert_eq!(e.driven_role("rev-2"), Some(lane_pane("rev-std", true)));
        assert_eq!(
            e.driven_role("rev-1"),
            Some(lane_pane("rev-std", false)),
            "a re-brief supersedes a reviewer pane; it does not un-own it"
        );

        // The exit list (#1871 B3) is that same population, and it must not name
        // a pane twice — a resumed-twice pane is one pane, and the notice sends a
        // human looking for each id it prints.
        e.record_worker_pane("w-1");
        assert_eq!(e.driven_role("w-2"), Some(worker_pane(false)), "w-2 is superseded in turn");
        let panes: Vec<String> = e.owned_panes().into_iter().map(|(a, _)| a).collect();
        assert_eq!(
            panes,
            vec!["w-2", "w-1", "rev-1", "rev-2"],
            "every pane once, worker first, oldest first within each"
        );

        // Re-pointing the drive at a different worker session forgets ALL of
        // them, not just the current one — they belong to a worker this drive no
        // longer owns.
        e.forget_worker_panes();
        assert_eq!(e.driven_role("w-1"), None);
        assert_eq!(e.driven_role("w-2"), None);
        assert_eq!(
            e.driven_role("rev-2"),
            Some(lane_pane("rev-std", true)),
            "...and the lanes are untouched, so this is not a method that forgets everything"
        );
    }

    /// **The superseded lists are bounded by LIVENESS, never by size** — and a
    /// size cap is what this replaces, because a cap reproduces #1871 B2.
    ///
    /// A cap must choose a victim, and age is the only ordering available to it.
    /// The oldest superseded pane is still running, still on this session and
    /// still able to `report`, so evicting it un-owns it exactly as the single
    /// slot did — B2 again, reachable by the very usage that produced B2. This
    /// test is the pin on that: pane 1 stays owned across an eviction pressure
    /// that a size rule would have acted on.
    ///
    /// Liveness is the one rule that cannot re-open it, and provably rather than
    /// plausibly: `resolve_token` refuses a `Dead` agent and has no entry for a
    /// gone one, so a dead pane cannot reach the MCP seam and there is no
    /// traffic left to fail to own.
    #[test]
    fn a_dead_superseded_pane_is_forgotten_and_a_live_one_is_never_evicted() {
        let mut e = entry_at(DriveState::FixWait);
        for i in 0..40 {
            e.record_worker_pane(&format!("w-{i}"));
        }
        e.open_lane("rev-std", "s1", "rev-1", "head-a", Some("d1"), 0, false, false);
        e.open_lane("rev-std", "s1", "rev-2", "head-a", Some("d1"), 1, false, false);

        // Forty hand-backs and nothing is dropped while every pane is alive:
        // the property a size cap cannot have.
        assert_eq!(e.prior_worker_agents.len(), 39, "no pane is evicted for being old");
        assert_eq!(
            e.driven_role("w-0"),
            Some(worker_pane(false)),
            "the OLDEST superseded pane is exactly the one a size cap drops, and it is still \
             live, still on this session, and still able to report — dropping it is #1871 B2"
        );

        // Now kill two of them. Only those two are forgotten.
        let dead = ["w-0", "rev-1"];
        let is_live = |a: &str| !dead.contains(&a);
        assert!(e.forget_dead_panes(&is_live), "it reports having dropped something");
        assert_eq!(e.driven_role("w-0"), None, "a dead pane cannot call, so forgetting is safe");
        assert_eq!(e.driven_role("rev-1"), None, "…on the lane's list too");
        assert_eq!(
            e.driven_role("w-1"),
            Some(worker_pane(false)),
            "…and every LIVE superseded pane survives the prune"
        );
        assert_eq!(e.prior_worker_agents.len(), 38);

        // Idempotent, and it says so: a second pass drops nothing, so a tick that
        // pruned nothing does not rewrite the file for it.
        assert!(!e.forget_dead_panes(&is_live), "nothing left to drop");

        // The current panes are never candidates — they are the drive's live
        // reference, and `rd_handback`/`open_lane` are what replace them.
        let all_dead = |_: &str| false;
        e.forget_dead_panes(&all_dead);
        assert_eq!(e.driven_role(&e.worker_agent.clone()), Some(worker_pane(true)));
        assert_eq!(e.driven_role("rev-2"), Some(lane_pane("rev-std", true)));

        // "We could not check" is not "it is dead": a predicate that answers
        // true keeps everything, which is the fail-closed direction here —
        // keeping a pane costs a string, dropping a live one costs the leak.
        let mut f = entry_at(DriveState::FixWait);
        f.record_worker_pane("w-1");
        f.record_worker_pane("w-2");
        assert!(!f.forget_dead_panes(&|_| true));
        assert_eq!(f.driven_role("w-1"), Some(worker_pane(false)));
    }

    #[test]
    fn the_two_layers_agree_on_invariant_9() {
        // **The cross-pin, and the reason it exists is the one thing it does NOT
        // change.** §2.3 puts INVARIANT 9's numbers behind two independent
        // enforcers: `workflow/parse.rs` refuses an out-of-range `driver:` value as it
        // parses, and `decide` clamps again on the values it actually reads.
        // Both must keep enforcing — that is the consent boundary, and
        // `a_repo_cannot_raise_invariant_9_by_handing_decide_a_wider_bound` is
        // why the second is not redundant: `decide` is a `pub fn` over a plain
        // value type any caller in any crate can reach without passing through
        // the parser at all.
        //
        // Independence of ENFORCEMENT is not duplication of the VALUE. Two
        // layers encoding "three" separately can drift to three and four with
        // nothing red to say so, and the direction that drift takes is a WIDENED
        // ceiling on an invariant the orchestrator template promises a human.
        // Nothing else in either crate compares them.
        //
        // Sited here because this is the one place that can see both: the
        // ceilings are this module's and the range constants are
        // `crate::workflow`'s, and they are one `use` apart in the same crate.
        use crate::workflow;
        assert_eq!(
            MAX_ROUNDS_CEILING, workflow::DRIVER_MAX_REVIEW_ROUNDS_MAX,
            "the review-round ceiling `decide` clamps to and the one the `driver:` block \
             refuses past have drifted apart"
        );
        assert_eq!(
            MAX_ROUNDS_CEILING, workflow::DRIVER_MAX_CI_ATTEMPTS_MAX,
            "the CI-attempt ceiling has drifted from the review-round one; INVARIANT 9 gives \
             both the same number"
        );
        assert_eq!(
            MAX_REBASE_CEILING, workflow::DRIVER_MAX_REBASE_ATTEMPTS_MAX,
            "the rebase ceiling `decide` clamps to and the one the `driver:` block refuses \
             past have drifted apart"
        );

        // The FLOORS are the other half of the same agreement, and they are not
        // symmetric — `clamped()` floors the two round counters at 1 and lets
        // rebases reach 0, because zero review rounds is a drive that parks on
        // the first `fail` having handed nothing back, while zero rebases is a
        // coherent policy a repo may choose. The parser has to permit exactly
        // what the clamp would produce, or one layer accepts a value the other
        // silently rewrites.
        assert_eq!(workflow::DRIVER_MAX_REVIEW_ROUNDS_MIN, 1);
        assert_eq!(workflow::DRIVER_MAX_CI_ATTEMPTS_MIN, 1);
        assert_eq!(workflow::DRIVER_MAX_REBASE_ATTEMPTS_MIN, 0);
        let floored = DriveLimits::new(0, 0, 0, 60, 60, 240);
        assert_eq!(floored.max_review_rounds, workflow::DRIVER_MAX_REVIEW_ROUNDS_MIN);
        assert_eq!(floored.max_ci_attempts, workflow::DRIVER_MAX_CI_ATTEMPTS_MIN);
        assert_eq!(floored.max_rebase_attempts, workflow::DRIVER_MAX_REBASE_ATTEMPTS_MIN);

        // And the non-vacuity control: the constants are not all the same
        // number, so the three equalities above are three facts rather than one
        // tautology over a single value.
        assert_ne!(MAX_ROUNDS_CEILING, MAX_REBASE_CEILING);
    }

    #[test]
    fn the_seed_and_the_clamps_only_ever_tighten() {
        // §2.3: a repo may run a tighter loop than the orchestrator template
        // promises; it may not run a looser one.
        assert_eq!(Counters::seeded(9).review_rounds, 3);
        assert_eq!(Counters::seeded(0).review_rounds, 0);
        let loose = DriveLimits {
            max_review_rounds: 5,
            max_ci_attempts: 99,
            max_rebase_attempts: 4,
            ..DriveLimits::default()
        }
        .clamped();
        assert_eq!(loose.max_review_rounds, 3);
        assert_eq!(loose.max_ci_attempts, 3);
        assert_eq!(loose.max_rebase_attempts, 1);
        // A tighter repo policy survives untouched — the clamp is a ceiling,
        // not a substitution.
        let tight = DriveLimits {
            max_review_rounds: 1,
            max_ci_attempts: 2,
            max_rebase_attempts: 0,
            ..DriveLimits::default()
        }
        .clamped();
        assert_eq!(tight.max_review_rounds, 1);
        assert_eq!(tight.max_ci_attempts, 2);
        assert_eq!(tight.max_rebase_attempts, 0);
        // Zero review rounds would be a drive that parks having handed nothing
        // back, so that floor is 1 — the asymmetry with rebases is deliberate.
        let floored = DriveLimits {
            max_review_rounds: 0,
            ..DriveLimits::default()
        }
        .clamped();
        assert_eq!(floored.max_review_rounds, 1);
    }

    // ── §2.3 #2509's one-shot grace for a body-only fail ─────────────────────

    /// A drive sitting AT the review bound, with `rev-std`'s brief carrying (or
    /// not carrying) #2509's mark.
    ///
    /// `body_only` is the axis every test below varies, and it is the only one:
    /// the counters, the live revision, the lane's pane and the reviewer's word
    /// are identical on both sides, so a rule that decided on the bound alone —
    /// or on nothing — is red rather than green-by-luck.
    fn at_the_bound(body_only: bool) -> DriveEntry {
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        e.counters.review_rounds = DriveLimits::default().max_review_rounds;
        e.open_lane(
            "rev-std", "sess-lane", "rev-1", "head-a", Some("d1"), 1_000, false, body_only,
        );
        e
    }

    /// The facts a lane's blocking `fail`, recorded about the live revision,
    /// puts in front of `review-wait`.
    fn failing_at_the_bound() -> DriveFacts {
        DriveFacts {
            required_lanes: Some(vec![lane_fact("rev-std", Some(Verdict::Fail), "head-a", "d1")]),
            ..facts_at("head-a")
        }
    }

    /// **THE test.** Two rows differing in the BRIEF the driver sent and in
    /// nothing else.
    #[test]
    fn a_body_only_fail_at_the_bound_buys_one_more_round_and_a_code_fail_does_not() {
        let limits = DriveLimits::default();
        let facts = failing_at_the_bound();
        assert_eq!(
            decide(&at_the_bound(true), &facts, &limits),
            DriveStep::spend(DriveState::FixWait, Counter::BodyOnlyGrace),
            "a fail on a lane re-briefed about the body alone buys the one grace round"
        );
        assert_eq!(
            decide(&at_the_bound(false), &facts, &limits),
            DriveStep::held(HeldReason::ReviewLimit),
            "a fail on a lane briefed about the code parks the drive exactly as it always did"
        );
    }

    /// §2.3's "never stacking", performed rather than asserted: the arc is
    /// taken, the drive goes round again on the same body-only brief, and the
    /// second fail parks.
    #[test]
    fn the_grace_is_spent_once_and_the_next_body_only_fail_parks() {
        let limits = DriveLimits::default();
        let facts = failing_at_the_bound();
        let mut e = at_the_bound(true);
        let step = decide(&e, &facts, &limits);
        assert_eq!(step, DriveStep::spend(DriveState::FixWait, Counter::BodyOnlyGrace));
        e.take(&step, 3_000).unwrap();
        assert!(e.counters.body_only_grace, "the arc is what spends it");

        // Arc 7 then arc 2: the worker pushes, CI settles, and the drive is back
        // in `review-wait` with the same body-only brief out and the same fail.
        e.advance(DriveState::CiWait, None, None, 4_000).unwrap();
        e.advance(DriveState::ReviewWait, None, None, 5_000).unwrap();
        e.open_lane(
            "rev-std", "sess-lane", "rev-1", "head-a", Some("d1"), 6_000, false, true,
        );
        assert_eq!(
            decide(&e, &facts, &limits),
            DriveStep::held(HeldReason::ReviewLimit),
            "the grace is one per drive, not one per body-only fail"
        );
    }

    /// **"`MAX_ROUNDS_CEILING` is untouched" is a claim about a NUMBER**, so it
    /// is pinned as one: the grace round leaves `review_rounds` exactly where
    /// the bound left it, and is funded from the bool beside it.
    #[test]
    fn the_grace_round_never_moves_the_review_round_counter() {
        let limits = DriveLimits::default();
        let mut e = at_the_bound(true);
        let spent = e.counters.review_rounds;
        assert_eq!(spent, limits.max_review_rounds, "the fixture really is at the bound");
        let step = decide(&e, &failing_at_the_bound(), &limits);
        e.take(&step, 3_000).unwrap();
        assert_eq!(e.counters.review_rounds, spent, "a grace round is not a review round");
        assert!(
            e.counters.review_rounds <= MAX_ROUNDS_CEILING,
            "and INVARIANT 9's ceiling bounds exactly what it bounded before"
        );
        assert!(e.counters.body_only_grace, "it is funded from its own one-shot budget");
        assert_eq!(e.state(), DriveState::FixWait, "and it really does hand the worker back");
    }

    /// The grace is asked **only at the bound**. Below it the ordinary round is
    /// spent and the grace is untouched — otherwise it is a discount, and a
    /// drive would arrive at the bound with it already gone.
    #[test]
    fn below_the_bound_a_body_only_fail_spends_an_ordinary_round() {
        let limits = DriveLimits::default();
        let mut e = at_the_bound(true);
        e.counters.review_rounds = limits.max_review_rounds - 1;
        let step = decide(&e, &failing_at_the_bound(), &limits);
        assert_eq!(step, DriveStep::spend(DriveState::FixWait, Counter::ReviewRounds));
        e.take(&step, 3_000).unwrap();
        assert!(!e.counters.body_only_grace, "an ordinary round leaves the grace unspent");
        assert_eq!(e.counters.review_rounds, limits.max_review_rounds);
    }

    /// All four crossings of {this lane was already briefed at the live head} x
    /// {every required lane has ANSWERED at it}, because the mark reads two
    /// signals and a mark that read either one alone is green on two of them.
    #[test]
    fn the_body_only_mark_needs_a_re_brief_at_an_unchanged_head_and_a_fully_answered_one() {
        let limits = DriveLimits::default();
        // `briefed`: the head this lane's record was last briefed at, or "" for
        // a lane with no record at all. `other`: the SECOND required lane's
        // verdict, which is what decides "has everyone answered".
        let step = |briefed: &str, other: Option<Verdict>| {
            let mut e = entry_at(DriveState::ReviewWait);
            e.head = "head-a".into();
            if !briefed.is_empty() {
                e.open_lane("rev-std", "s1", "rev-1", briefed, Some("OLD"), 1_000, false, false);
            }
            let facts = DriveFacts {
                required_lanes: Some(vec![
                    lane_fact("rev-std", Some(Verdict::Fail), "head-a", "OLD"),
                    lane_fact("rev-final", other, "head-a", "OLD"),
                ]),
                body_digest: Some("NEW".into()),
                ..facts_at("head-a")
            };
            decide(&e, &facts, &limits)
        };
        let marked = |s: &DriveStep| match s {
            DriveStep::OpenLane { body_only, .. } => *body_only,
            other => panic!("expected a lane brief, got {other:?}"),
        };
        // The one row that is #2509's case: re-briefed at this head, everyone
        // has spoken about it, and only the body moved.
        assert!(marked(&step("head-a", Some(Verdict::Pass))));
        // A lane nobody has answered at this head: the CODE here is not
        // reviewed to completion, so a later fail is a first opinion, not a
        // body finding.
        assert!(!marked(&step("head-a", None)));
        // A brief that predates this head is not a re-brief AT it.
        assert!(!marked(&step("head-b", Some(Verdict::Pass))));
        // And a lane with no record at all has never been briefed here.
        assert!(!marked(&step("", Some(Verdict::Pass))));
    }

    /// The grant's posture, which is [`LaneRecord::briefed_verify`]'s one grant
    /// over: an **exact** `(briefed_head, briefed_digest)` match, never
    /// [`lane_open_for`]'s unknown-tolerant comparison. A brief whose revision
    /// cannot be pinned grants nothing.
    #[test]
    fn the_grace_refuses_a_brief_whose_revision_it_cannot_pin() {
        let e = at_the_bound(true);
        assert!(
            body_only_grace_applies(&e, "rev-std", "head-a", Some("d1")),
            "the positive control: this fixture DOES earn the grace"
        );
        assert!(!body_only_grace_applies(&e, "rev-std", "head-b", Some("d1")), "the head moved");
        assert!(!body_only_grace_applies(&e, "rev-std", "head-a", Some("d2")), "the body moved");
        assert!(
            !body_only_grace_applies(&e, "rev-std", "head-a", None),
            "the body could not be read: unknown is never 'unbound, therefore fine'"
        );
        assert!(
            !body_only_grace_applies(&e, "rev-std", "head-a", Some("")),
            "and an empty digest is not a digest"
        );
        assert!(
            !body_only_grace_applies(&e, "rev-final", "head-a", Some("d1")),
            "another lane's brief grants this one nothing"
        );
        assert!(
            !body_only_grace_applies(&at_the_bound(false), "rev-std", "head-a", Some("d1")),
            "a brief that was never marked body-only"
        );
        let mut spent = at_the_bound(true);
        spent.counters.body_only_grace = true;
        assert!(!body_only_grace_applies(&spent, "rev-std", "head-a", Some("d1")), "already spent");
    }

    /// A repo file cannot buy a SECOND grace any more than it can buy a fourth
    /// round — the sibling of
    /// `a_repo_cannot_raise_invariant_9_by_handing_decide_a_wider_bound`, on the
    /// axis that slice could not vary because the field did not exist.
    #[test]
    fn a_repo_that_widens_its_bound_still_gets_exactly_one_grace() {
        let wide = DriveLimits { max_review_rounds: 9, ..DriveLimits::default() };
        assert_eq!(wide.max_review_rounds, 9, "the fixture really is over-bound");
        let facts = failing_at_the_bound();
        let mut e = at_the_bound(true);
        e.counters.review_rounds = MAX_ROUNDS_CEILING;
        assert_eq!(
            decide(&e, &facts, &wide),
            DriveStep::spend(DriveState::FixWait, Counter::BodyOnlyGrace),
            "the clamp puts a wide repo at the same bound, and the grace answers there"
        );
        e.counters.body_only_grace = true;
        assert_eq!(
            decide(&e, &facts, &wide),
            DriveStep::held(HeldReason::ReviewLimit),
            "and nine rounds in a repo file still buys no second grace"
        );
    }

    /// An `escalate` at the bound is still §3's judgment call, not a grace: the
    /// currency ladder answers `escalate` above the `fail` arm, and #2509 must
    /// not have quietly moved a JUDGMENT hold onto the driver's own budget.
    #[test]
    fn an_escalate_at_the_bound_is_still_a_judgment_hold_and_never_a_grace() {
        let limits = DriveLimits::default();
        let facts = DriveFacts {
            required_lanes: Some(vec![lane_fact(
                "rev-std",
                Some(Verdict::Escalate),
                "head-a",
                "d1",
            )]),
            ..facts_at("head-a")
        };
        let e = at_the_bound(true);
        assert_eq!(decide(&e, &facts, &limits), DriveStep::held(HeldReason::Escalate));
        assert!(
            !e.counters.body_only_grace,
            "and it costs the grace nothing, so a disposition can still spend it"
        );
    }

    /// §5.2's graceful-degradation posture, and the asymmetry with the three
    /// COUNTS beside it: a missing count is REFUSED because a defaulted zero
    /// re-grants a whole budget, while a missing bool grants one round once and
    /// is the true reading of a file no build with this field ever wrote.
    #[test]
    fn a_counters_block_written_before_the_grace_existed_parses_with_it_unspent() {
        let old = NOTE_EXAMPLE;
        assert!(
            !old.contains("body_only_grace"),
            "the fixture really predates the field"
        );
        let st = parse_state(old).expect("a pre-#2509 counters block still parses");
        assert!(
            st.entries.iter().all(|d| !d.counters.body_only_grace),
            "and reads as a grace nobody has spent"
        );
        // The negative control: the field is not merely being ignored on the way
        // in. A file that says the grace IS spent round-trips as spent.
        let spent = old.replace(
            r#""rebase_attempts": 0 },"#,
            r#""rebase_attempts": 0, "body_only_grace": true },"#,
        );
        assert_ne!(spent, old, "the mutation must actually land");
        let st = parse_state(&spent).expect("and the new field parses");
        assert!(st.entries.iter().all(|d| d.counters.body_only_grace));
    }
}
