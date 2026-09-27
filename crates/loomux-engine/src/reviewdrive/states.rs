//! §2.1 the states: `DriveState`, `HeldReason`, the enumerated
//! [`transition`] table, and the hold key a repeated hold is compared by.
//!
//! Part of the review driver's pure core, split out of the former single-file
//! `reviewdrive.rs` by #3498 P7 as a pure move. The module map is in `mod.rs`;
//! the design note is `docs/design/review-driver.md`.

use super::*;

// ── §2.1 the states ─────────────────────────────────────────────────────────

/// Where one driven PR is (§2.1). **Four working states, one parked state, two
/// terminals** — seven, and the enum is closed.
///
/// **There is no `Unknown` variant and no catch-all arm**, exactly as
/// [`crate::mergeq::EntryState`] has none, and §2.1 makes that a prescription
/// rather than a style note. The reason is that §5.2 persists this as a
/// *string* while promising unknown *fields* are tolerated and preserved, and
/// the two promises pull opposite ways unless one governs. The refusal governs:
/// an unknown field is data some newer build added that this one can carry
/// across a read/write cycle without understanding, and preserving it loses
/// nothing; an unknown state is the entry's entire meaning, and every available
/// default is a guess that either resumes a drive somebody stopped or abandons
/// one still running. So a file naming a state this build does not know fails
/// to parse — §2.4's `rd-state-unreadable`, refuse the tick, back off, never
/// repair and never delete.
///
/// **`Held` is parked, not terminal, and the distinction is load-bearing.** The
/// queue's `KickedBack` *is* terminal, and a kicked-back PR comes back through
/// a fresh `queue_merge` as a NEW entry. A drive cannot copy that, because §2.3
/// carries the spent counters across a resume and a fresh entry would reset
/// them — the one thing INVARIANT 9's "yours count too" forbids. So `Held`
/// keeps its counters, has exactly two outgoing arcs (`drive_review` resumes
/// it, `cancel_review_drive` cancels it), and is never pruned (§5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DriveState {
    /// Waiting on the PR's checks and mergeability at the current head.
    CiWait,
    /// A reviewer lane is open; waiting for its verdict at (head, digest).
    ReviewWait,
    /// Handed back to the worker; waiting for a push or a report.
    FixWait,
    /// Re-reading the gate at the live head, the last thing before `satisfied`.
    GateCheck,
    /// **Parked**, carrying a [`HeldReason`]. The tick does not advance it; the
    /// two tools do.
    Held,
    /// Terminal: the gate is satisfied at the live head.
    Satisfied,
    /// Terminal: cancelled, or reconcile positively established the PR is
    /// closed or merged.
    Cancelled,
}

impl DriveState {
    /// Every state, in §2.1's table order. Exists so a caller — or the test
    /// that walks all forty-nine `(from, to)` pairs — can enumerate the machine
    /// without matching on the enum, which is what lets that test fail when an
    /// eighth state is added rather than silently keep checking seven.
    pub const ALL: [DriveState; 7] = [
        DriveState::CiWait,
        DriveState::ReviewWait,
        DriveState::FixWait,
        DriveState::GateCheck,
        DriveState::Held,
        DriveState::Satisfied,
        DriveState::Cancelled,
    ];

    /// The wire/audit spelling — the same string serde writes.
    pub fn as_str(self) -> &'static str {
        match self {
            DriveState::CiWait => "ci-wait",
            DriveState::ReviewWait => "review-wait",
            DriveState::FixWait => "fix-wait",
            DriveState::GateCheck => "gate-check",
            DriveState::Held => "held",
            DriveState::Satisfied => "satisfied",
            DriveState::Cancelled => "cancelled",
        }
    }

    /// Parse a state word. `None` for anything unrecognized — never coerced,
    /// the same "reject, never guess" posture [`Verdict::parse`] and
    /// [`crate::mergeq::EntryState::parse`] take; the reason is on the enum.
    pub fn parse(s: &str) -> Option<DriveState> {
        match s.trim() {
            "ci-wait" => Some(DriveState::CiWait),
            "review-wait" => Some(DriveState::ReviewWait),
            "fix-wait" => Some(DriveState::FixWait),
            "gate-check" => Some(DriveState::GateCheck),
            "held" => Some(DriveState::Held),
            "satisfied" => Some(DriveState::Satisfied),
            "cancelled" => Some(DriveState::Cancelled),
            _ => None,
        }
    }

    /// `satisfied` / `cancelled`, and **only** those two (§2.1). A terminal
    /// state has no outgoing transition at all, and terminal entries are what
    /// §5.2's retention prunes.
    pub fn is_terminal(self) -> bool {
        matches!(self, DriveState::Satisfied | DriveState::Cancelled)
    }

    /// Parked (§2.1). Not terminal, not live: the tick leaves it alone, its
    /// counters are preserved, and it is **never pruned** — pruning one would
    /// silently grant three fresh review rounds (§5.2).
    pub fn is_parked(self) -> bool {
        matches!(self, DriveState::Held)
    }

    /// The working and gate states — the scope of §5.1's `already-driven`
    /// decline, spelled once here because that is the whole of its definition:
    /// "`already-driven` covers the working and `gate-check` states only". A
    /// `held` entry is parked and §2.3 calls resuming it the default, so a flat
    /// `already-driven` would make that path unreachable and `reset_counters` a
    /// parameter nothing can pass.
    pub fn is_live(self) -> bool {
        !self.is_terminal() && !self.is_parked()
    }
}

/// Why a drive is parked (§2.2). **One state carrying a closed reason enum, not
/// fifteen states**, so a reader asking "is this drive parked" asks one
/// question, and the reason travels in the notice and the audit line rather
/// than being inferred from which counter happens to sit at its bound.
///
/// Sixteen reasons. With `satisfied` and `cancelled` that is §2.2's eighteen
/// exits back to the LLM orchestrator, and [`HeldReason::ALL`] is what makes
/// that count checkable rather than asserted.
///
/// Closed for [`DriveState`]'s reason: a hold whose reason this build cannot
/// read is a hold it cannot explain, and the notice is the entire product of a
/// hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HeldReason {
    /// A lane recorded `escalate`.
    Escalate,
    /// `review_rounds` reached its bound.
    ReviewLimit,
    /// `ci_attempts` reached its bound.
    CiLimit,
    /// A second conflict after the one rebase hand-back.
    RebaseLimit,
    /// A spawned or resumed **reviewer** lane recorded no verdict inside
    /// `lane_timeout_minutes`.
    LaneStalled,
    /// A resumed **worker** went quiet on a hand-back for
    /// `fix_timeout_minutes`. **Two shapes since #2168 E1, and the notice says
    /// which** (`rddrive::held_notice` branches on `HeldFacts::held_state`):
    /// from `fix-wait`, the worker neither pushed nor reported; from `ci-wait`,
    /// it pushed and then did not report on the pushed head, which is the wait
    /// [`decide_fix_receipts`] adds. One reason rather than two because it is
    /// one wait for one worker on one hand-back — and one remedy, the pane the
    /// notice already names.
    FixStalled,
    /// The drive's **age** passed `drive_timeout_minutes` — the backstop, and
    /// since #2110 only the backstop. Its notice names the state the drive was
    /// in and how long it had been there, because at twelve hours the age alone
    /// says nothing about what to do next.
    DriveStalled,
    /// **The drive sat in ONE working state past that state's own bound**
    /// (#2110) — [`state_bound_ms`], reset by every transition.
    ///
    /// Its own reason rather than [`DriveStalled`](HeldReason::DriveStalled),
    /// and the argument is what the orchestrator LEARNS. The two measured
    /// incidents that produced #2110 both reported `drive-stalled`, and in
    /// neither was anything stalled: one drive was mid-round with CI green at a
    /// new head, the other had spent three of its four hours unable to spawn a
    /// lane because another drive held every slot. An age is the one quantity
    /// that cannot distinguish those from a drive that is genuinely stuck,
    /// because every drive's age grows at the same rate whatever it is doing.
    /// This one grows only while nothing moves, so a drive that reaches it
    /// really is sitting still — and the notice says where.
    ///
    /// **Not a replacement for the wait-specific holds, and never their
    /// preemption.** `lane-stalled`, `fix-stalled` and `cap-full` each name a
    /// remedy this cannot ("read that pane", "free a slot"), and each fires
    /// well inside its state's bound; [`state_bound_ms`] is floored against the
    /// repo's own knobs so that stays true however they are configured — and
    /// #2168 E1 is why that clause is load-bearing rather than decorative: it
    /// put a second, `fix_timeout_minutes`-long wait into `ci-wait` behind the
    /// check wait, so that arm became its constant PLUS the knob. What is left
    /// for this to catch is a
    /// `ci-wait` on a check run that never resolves, the state that should
    /// never be a wait at all (`gate-check`), and any future path that sits in
    /// a state none of the others can see.
    StateStalled,
    /// `route_reviewers` returned `None` — the changed-file list could not be
    /// shown complete, so *which reviewers are required* is unknown. Never a
    /// guess: guessing "no rule fired" is guessing in favour of merging.
    RoutingUnaccountable,
    /// The gate file is present and orrerix could not turn it into a routing
    /// answer — an I/O error, or a file `parse_gate_file` refuses. **Not**
    /// `gate-not-configured`, which means the file is genuinely absent.
    ///
    /// Both are the same fact to a drive: the gate exists and cannot be read,
    /// so what it requires is unknown — and §8 never treats unknown as safe.
    GateUnreadable,
    /// The worker reported `blocked`.
    WorkerBlocked,
    /// **The fix could not be handed back to its worker** — the class, not one
    /// cause. A session that no longer resolves is the original one; a block
    /// that has left the roster and a pane that opened and exited before saying
    /// anything are the two #1961 added, and each names itself in
    /// [`crate::rddrive::HeldFacts::refusal`] rather than being reported as the
    /// first. Narrowing this doc to "the session no longer resolves" is how the
    /// notice came to send an orchestrator after a replacement session for a
    /// session that was fine.
    WorkerUnresumable,
    /// **The group's live-delegate cap refused the pane the drive needed**
    /// (#1960) — its own reason, and the reason it is not
    /// [`WorkerUnresumable`](HeldReason::WorkerUnresumable).
    ///
    /// It was reported as that one, which named a remedy that does not work: it
    /// tells the orchestrator the recorded session no longer resolves and to
    /// re-point the drive at another one. The session resolves fine. What is
    /// exhausted is a slot, the remedy is to free one (or wait), and those are
    /// different actions — measured on the dogfood, an orchestrator went
    /// looking for a replacement session for a session whose `.jsonl` was on
    /// disk and which re-pointed successfully the moment panes were killed.
    ///
    /// A **lane** spawn refused by the cap does not reach this: `review-wait`
    /// backs off and retries (§8's live-delegate-cap row). The asymmetry is the
    /// states': a lane can be opened on any later tick, while `fix-wait` has
    /// already taken its arc and spent its round. A lane refusal that does not
    /// clear becomes [`CapFull`](HeldReason::CapFull) after [`CAP_HOLD_MS`]
    /// (#2109) — a different reason, on that variant's argument, and never this
    /// one.
    CapRefused,
    /// **The cap has refused this drive's LANE, continuously, for
    /// [`CAP_HOLD_MS`]** (#2109) — the starvation made visible.
    ///
    /// "Continuously" is a promise the TICK keeps, not this enum: a refusal
    /// that is not the cap's clears the stamp rather than letting it age, so
    /// this reason cannot be reached behind a run of refusals that were
    /// something else. Guarding only the write made the stamp a latch, which is
    /// review 4's W1 on #2112.
    ///
    /// Its own reason rather than [`CapRefused`](HeldReason::CapRefused), and
    /// the argument is a DURATION rather than a remedy. The two share a remedy
    /// (free a slot), so a reader deciding what to *do* could be served by one
    /// spelling. What one spelling cannot say is how long: `cap-refused` is a
    /// single hand-back refusal, held on the spot, with a round already spent;
    /// this is "every lane spawn for this drive has been refused since `t`".
    /// The measured incident is exactly that difference — a drive sat in
    /// `review-wait` with `lanes: []` for three hours emitting 37 identical
    /// `rd-refused` rows and no notice at all, and an orchestrator reading
    /// `cap-refused` there would have learned that the cap refused *a* spawn,
    /// which had been true and harmless thirty-seven ticks earlier.
    ///
    /// **The driver still never kills a pane to make room** (§3.1 item 5, as
    /// #2501 narrowed it: it releases a lane whose verdict is recorded at this
    /// head and a worker whose report it consumed, on facts about those panes
    /// and never on how full the group is), and this hold is what makes the
    /// remaining starvation survivable rather than silent: the notice names who
    /// can free a slot. What actually releases the cap is a human or the
    /// orchestrator killing an idle delegate, the idle reaper where one is
    /// configured, or another drive ending — and #2109's other two fixes are
    /// what make waiting terminate at all, because a drive now costs ONE pane
    /// per lane block for its whole life instead of one or two per round.
    CapFull,
    /// A driven delegate called `message_orchestrator` (§7 — that call is never
    /// intercepted; the delegate's own line arrives by its own path and this
    /// hold is the routing fact beside it).
    Messaged,
    /// **The account behind a pane this drive owns is out of budget** (#2811
    /// S5b) — the provider printed its refusal and the pane stopped.
    ///
    /// Its own reason rather than [`LaneStalled`](HeldReason::LaneStalled),
    /// which is what used to catch it, and the argument is what the
    /// orchestrator LEARNS and what it costs to learn it. `lane-stalled` is a
    /// sixty-minute timeout measured from the brief, so a provider outage was
    /// reported an hour late, once per affected drive, with a hold apiece and
    /// a remedy — "read that pane" — that does not work: nothing in the pane
    /// clears an exhausted account. This fires on the NEXT TICK, names the
    /// provider, and carries the remedy that does work (raise the key's limit,
    /// add credits, or point the block at a different `model:`).
    ///
    /// **It spends nothing.** No round, no CI attempt, no lane timeout — the
    /// panes are not slow, they are stopped, and charging a counter for a
    /// vendor's billing state would make a drive that survived an outage look
    /// like one that had burned its budget. That is why the arm sits above the
    /// age and per-state backstops in [`decide`] rather than beside the other
    /// waits: every bound below it is measuring a wait that is not this drive's
    /// fault and cannot be shortened by anything the driver does.
    ///
    /// **One hold per affected drive, but ONE notice for all of them.** A
    /// provider limit stops every pane on that provider at once, so N drives
    /// hold on one cause with one remedy; N identical lines would be N times
    /// the orchestrator's attention for one action. The per-drive `rd-held`
    /// row is still written for each — §5.4 is a record of what happened —
    /// and the aggregation is the TICK's, in `rdtick`, not this enum's.
    ///
    /// The detection is not the driver's either: it is the attention scan's
    /// [`crate::providerlimit`] table, read off the pane's own text, which is
    /// the one pane-text classifier (#2811 S5a).
    ProviderLimit,
}

impl HeldReason {
    /// Every reason, so a caller — or a test counting §2.2's exits — can
    /// enumerate them without matching on the enum. Order is §2.2's table.
    pub const ALL: [HeldReason; 16] = [
        HeldReason::Escalate,
        HeldReason::ReviewLimit,
        HeldReason::CiLimit,
        HeldReason::RebaseLimit,
        HeldReason::LaneStalled,
        HeldReason::FixStalled,
        HeldReason::DriveStalled,
        HeldReason::StateStalled,
        HeldReason::RoutingUnaccountable,
        HeldReason::GateUnreadable,
        HeldReason::WorkerBlocked,
        HeldReason::WorkerUnresumable,
        HeldReason::CapRefused,
        HeldReason::CapFull,
        HeldReason::Messaged,
        HeldReason::ProviderLimit,
    ];

    /// The wire/audit spelling — the same string serde writes, and the detail
    /// `rd-held` carries (§5.4).
    pub fn as_str(self) -> &'static str {
        match self {
            HeldReason::Escalate => "escalate",
            HeldReason::ReviewLimit => "review-limit",
            HeldReason::CiLimit => "ci-limit",
            HeldReason::RebaseLimit => "rebase-limit",
            HeldReason::LaneStalled => "lane-stalled",
            HeldReason::FixStalled => "fix-stalled",
            HeldReason::DriveStalled => "drive-stalled",
            HeldReason::StateStalled => "state-stalled",
            HeldReason::RoutingUnaccountable => "routing-unaccountable",
            HeldReason::GateUnreadable => "gate-unreadable",
            HeldReason::WorkerBlocked => "worker-blocked",
            HeldReason::WorkerUnresumable => "worker-unresumable",
            HeldReason::CapRefused => "cap-refused",
            HeldReason::CapFull => "cap-full",
            HeldReason::Messaged => "messaged",
            HeldReason::ProviderLimit => "provider-limit",
        }
    }

    /// Parse a hold reason. `None` for anything unrecognized.
    pub fn parse(s: &str) -> Option<HeldReason> {
        match s.trim() {
            "escalate" => Some(HeldReason::Escalate),
            "review-limit" => Some(HeldReason::ReviewLimit),
            "ci-limit" => Some(HeldReason::CiLimit),
            "rebase-limit" => Some(HeldReason::RebaseLimit),
            "lane-stalled" => Some(HeldReason::LaneStalled),
            "fix-stalled" => Some(HeldReason::FixStalled),
            "drive-stalled" => Some(HeldReason::DriveStalled),
            "state-stalled" => Some(HeldReason::StateStalled),
            "routing-unaccountable" => Some(HeldReason::RoutingUnaccountable),
            "gate-unreadable" => Some(HeldReason::GateUnreadable),
            "worker-blocked" => Some(HeldReason::WorkerBlocked),
            "worker-unresumable" => Some(HeldReason::WorkerUnresumable),
            "cap-refused" => Some(HeldReason::CapRefused),
            "cap-full" => Some(HeldReason::CapFull),
            "messaged" => Some(HeldReason::Messaged),
            "provider-limit" => Some(HeldReason::ProviderLimit),
            _ => None,
        }
    }
}

/// A transition the state machine refuses. Carries both ends so the audit event
/// and the notice can say what was actually attempted (§5.4 — an audit action
/// must name what happened).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidTransition {
    pub from: DriveState,
    pub to: DriveState,
}

impl fmt::Display for InvalidTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} -> {} is not a legal review-drive transition",
            self.from.as_str(),
            self.to.as_str()
        )
    }
}

/// **The state machine.** §2.1's thirteen arcs, each with the section that asks
/// for it. Anything else is refused — including a self-transition, which is not
/// a transition: advancing to lane *k+1* leaves the entry in `review-wait` and
/// writes the lane index, exactly as refreshing a `blocked_reason` leaves a
/// queue entry `queued`.
///
/// **Enumerated, and a pair this does not name is a refusal**, copying
/// [`crate::mergeq::transition`], which matches explicit pairs and falls
/// through to `Err`. §2.1 gives the reason: a state machine whose §8 needs an
/// arc §2 never named does not fail as a documentation gap, it fails at
/// runtime, on the degradation path, where nothing is watching.
///
/// Thirteen arc *rows* are twenty legal `(from, to)` pairs, because three rows
/// are written over a set of froms — arc 3 (`-> fix-wait` on a conflict) over
/// every working or gate state but `fix-wait`: `ci-wait` and `gate-check`
/// directly, plus `review-wait` through the pair it shares with arc 5 below
/// (§2.1: CONFLICTING is read from EVERY such state since #2311), arc 12
/// (`-> held`) over the four working and gate states, arc 13 (`-> cancelled`)
/// over all five non-terminals. All
/// are spelled here as explicit variant lists rather than as a predicate like
/// `!from.is_terminal()`: the two spellings coincide today, and the enumerated
/// one is what makes an eighth state a compile-time decision instead of
/// something a predicate quietly absorbs.
///
/// **Arc 1 is not here.** `(none) -> ci-wait` is a *creation* — `drive_review`
/// on a PR with no live entry (§5.1) — and it has no `from`. It is
/// [`DriveEntry::new`], the way the queue's own arc-from-nothing is
/// `QueueEntry::new`.
///
/// **Nor is a per-reason restriction on arc 12,** and that is a decision rather
/// than an omission. §2.1's per-state "Leaves for" column names which
/// [`HeldReason`]s each state reaches, but it omits `messaged` from every cell
/// while §2.2 and arc 12 require it from any working state (§7: a driven
/// delegate can call `message_orchestrator` at any point in the loop). Read as
/// a second, finer table it would therefore refuse a hold the note mandates. So
/// the arc list governs — §2.1 says it is "a table of its own", listed in full
/// precisely so nothing is inferred from the prose — and the "Leaves for"
/// column is read as the summary it is. [`decide`] is where each reason is
/// actually produced, and it produces them per state.
pub fn transition(from: DriveState, to: DriveState) -> Result<DriveState, InvalidTransition> {
    use DriveState::*;
    let ok = match (from, to) {
        // 2. Checks green (§2.1).
        (CiWait, ReviewWait) => true,
        // 3. Checks red, or CONFLICTING (§2.1) — and CONFLICTING from
        //    `gate-check` as well since #2311, which is the whole of what makes
        //    `satisfied` mean "gated AND mergeable". Red is a `ci-wait`
        //    observation only: this state does not read the check matrix.
        (CiWait | GateCheck, FixWait) => true,
        // 4. The last required lane passed at (head, digest).
        (ReviewWait, GateCheck) => true,
        // 5. A lane recorded `fail` (§2.1) — and, since #2311, the pair arc 3
        //    also uses when this state observes a CONFLICTING PR. One pair, two
        //    arcs: what tells them apart is the counter each spends, which is
        //    what `a_conflicting_pr_takes_the_rebase_arc_from_every_state_that_can`
        //    asserts.
        (ReviewWait, FixWait) => true,
        // 6. The head moved under a lane mid-review (§8 row 4). The verdict
        //    that lands binds to the old head; a `fail` there still routes, a
        //    `pass` there is stale and the lane is re-briefed after CI.
        (ReviewWait, CiWait) => true,
        // 7. The worker pushed (§2.1).
        (FixWait, CiWait) => true,
        // 8. `report(done)` with the head unchanged — a body-only fix; it
        //    re-enters at the first stale lane (§8 row 5, [`first_stale_lane`]).
        (FixWait, ReviewWait) => true,
        // 9. `evaluate_merge_gate` satisfied at the live head.
        (GateCheck, Satisfied) => true,
        // 10. NOT satisfied, for ANY reason — a stale pass, an unsatisfied
        //     `also:` condition, a push that landed under the check (§8, the
        //     body-changed and `also: [base-green]` rows). Deliberately wider
        //     than "stale": a drive parked on a red default branch is not
        //     staleness, and an arc named only for staleness would refuse it.
        (GateCheck, CiWait) => true,
        // 11. `drive_review` resumes a parked drive (§2.3).
        (Held, CiWait) => true,
        // 12. A counter bound, a lane/fix/drive timeout, an unaccountable
        //     route, an unreadable gate, a blocked or unresumable worker, an
        //     escalate, or a delegate's `message_orchestrator` (§2.2). From the
        //     four working and gate states — NOT from `held` itself, whose two
        //     outgoing arcs §2.1 enumerates, and not from a terminal.
        (CiWait | ReviewWait | FixWait | GateCheck, Held) => true,
        // 13. `cancel_review_drive`, or reconcile positively established the PR
        //     is closed or merged (§8). From any non-terminal, `held` included:
        //     cancelling is a parked drive's second way out.
        (CiWait | ReviewWait | FixWait | GateCheck | Held, Cancelled) => true,
        _ => false,
    };
    if ok {
        Ok(to)
    } else {
        Err(InvalidTransition { from, to })
    }
}

/// **The two reasons whose line can repeat WORD FOR WORD and still be news, so
/// they are never suppressed** (#3040 N1; narrowed by rev-final round 3).
///
/// Since the key digests the rendered line, an interpolated fact that changed —
/// a different refusal, a moved roster — already makes a different key. What
/// this covers is the residue that survives that: `state-stalled` and
/// `drive-stalled` report a duration ROUNDED to minutes, so two consecutive
/// holds really can render identical text ("It was in ci-wait for 2h 30m")
/// while meaning that the drive sat out its whole bound a second time. There
/// the identical line is not a repeat; it is the same sentence about a new
/// fact, and no digest can tell those apart.
///
/// **It is a backstop, and it is deliberately not the mechanism.** #3040 N1's
/// first attempt made this list the whole answer — exempt the reasons whose
/// line carries a duration — and rev-final found the same class one field over,
/// on the three reasons whose line carries a refusal. Enumerating
/// interpolations is that bug with a longer list; the digest in [`hold_key`] is
/// what generalises, and this stays for the one case a digest structurally
/// cannot see.
///
/// Named as a closed match over the enum rather than as a `matches!` on the
/// two, so a seventeenth reason has to decide (the sixteenth, #2811 S5b's
/// `provider-limit`, did — below). The question it must answer is
/// narrow now: *can this line render identically while meaning something new?*
/// Anything a reader could tell apart by looking is already handled.
pub fn repeat_carries_new_information(reason: HeldReason) -> bool {
    match reason {
        HeldReason::StateStalled | HeldReason::DriveStalled => true,
        HeldReason::Escalate
        | HeldReason::ReviewLimit
        | HeldReason::CiLimit
        | HeldReason::RebaseLimit
        | HeldReason::LaneStalled
        | HeldReason::FixStalled
        | HeldReason::RoutingUnaccountable
        | HeldReason::GateUnreadable
        | HeldReason::WorkerBlocked
        | HeldReason::WorkerUnresumable
        | HeldReason::CapRefused
        | HeldReason::CapFull
        | HeldReason::Messaged
        // #2811 S5b. `false`, and the question the doc above poses answers
        // itself here: this line carries a provider name and a remedy, and
        // no interpolated quantity at all. A repeat renders identically
        // BECAUSE it means the same thing — the account is still out of
        // budget — so re-announcing it would be the duplicate-line problem
        // #3040 N1 exists to stop, on the one hold that by construction
        // arrives N times at once.
        | HeldReason::ProviderLimit => false,
    }
}

/// **A stable digest of a notice, for [`hold_key`]** — FNV-1a, written here
/// rather than reached for.
///
/// `DefaultHasher` is the obvious call and is wrong for this: the key is
/// PERSISTED in `review_drives.json`, and std makes no promise that its hash is
/// stable across processes or builds, so a restart could silently re-announce
/// or (worse, if it collided differently) silently suppress. This is fifteen
/// lines, deterministic by construction, and depends on nothing — which also
/// keeps `src-tauri` clear of the getrandom family CLAUDE.md bans.
///
/// It is a digest and not a checksum: collisions are possible and their cost is
/// bounded at one suppressed line, the same cost the key already carries when
/// two holds genuinely are identical.
fn notice_digest(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// **What makes two holds the same hold** (#3040 N1): the reason, the head it
/// fired at, the counters the drive had spent — and a digest of the LINE the
/// drive is about to send.
///
/// **The line is in the key because the structured fields are not the whole of
/// what a notice says** (rev-final round 3). `held_notice` interpolates facts no
/// tuple here models: `HeldFacts::refusal` on `worker-unresumable`,
/// `cap-refused` and `cap-full` — which is #1961's whole point, the observation
/// rather than the diagnosis — and those three reasons spend no counter and need
/// no push, so head and counters are exactly the two things that do NOT move
/// across such a repeat. Keyed on the tuple alone, a hand-back that failed with
/// `unknown block "worker-adv"` and then with `Invalid session ID` announced
/// once and audited the second, and a `cap-refused` repeat suppressed the
/// CURRENT live-delegate roster — the list its own remedy tells the
/// orchestrator to pick a kill target from. The tick already knows the
/// difference (`rd_handback` renders `second time: {why}` when it does NOT),
/// so the key was the only reader that could not see it.
///
/// **Enumerating the interpolations instead would be the same bug with a longer
/// list.** #3040 N1's first attempt exempted the two reasons whose line carries
/// a duration; this finding is the same class one field over. A digest of the
/// rendered line generalises over every interpolation there is and over every
/// one a sixteenth reason adds, with no list to keep in step.
///
/// The structured fields stay beside it, unhashed, because the audit row and a
/// human reading `review_drives.json` want a key they can read — and because a
/// digest alone would make a prose edit to a notice look like a new hold on
/// every parked drive at once, where the fields say plainly that nothing about
/// the DRIVE changed.
///
/// One string rather than a tuple so the entry persists it as one JSON value an
/// older build round-trips through `extra` untouched (§11.2).
pub fn hold_key(reason: HeldReason, head: &str, counters: &Counters, notice: &str) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{:016x}",
        reason.as_str(),
        head,
        counters.review_rounds,
        counters.ci_attempts,
        counters.rebase_attempts,
        counters.body_only_grace,
        notice_digest(notice),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **`hold_key` stays reason-blind, so the exception cannot migrate into
    /// it** (rev-std round 2 premortem).
    ///
    /// The suppression is decided in two places that must stay separate: the
    /// key says whether two holds are the same hold, and
    /// [`repeat_carries_new_information`] says whether sameness implies
    /// silence. Fold the exception into the KEY instead — a nonce, a clock, a
    /// per-reason discriminator for the two — and every one of this module's
    /// other tests still passes: the cap-repeat test still dedups, the
    /// time-bound test still announces, and it announces for the wrong reason,
    /// with the mutation surface moved somewhere nothing reads.
    ///
    /// So this pins the property that makes the split real: the key is a pure
    /// function of what it is handed. Two calls with identical inputs are
    /// identical for EVERY reason, the two exceptions included — and the
    /// discriminating half is that a key still VARIES on each input it is
    /// documented to carry, or "always equal" would satisfy the first half.
    #[test]
    fn the_hold_key_is_a_pure_function_of_its_inputs_for_every_reason() {
        let head_a = "aa11bb22cc33dd44";
        let head_b = "bb22cc33dd44ee55";
        let c0 = Counters::default();
        let c1 = Counters { review_rounds: 1, ..Counters::default() };
        let n0 = "HELD — something";
        let n1 = "HELD — something else";
        for r in HeldReason::ALL {
            assert_eq!(
                hold_key(r, head_a, &c0, n0),
                hold_key(r, head_a, &c0, n0),
                "{}: the key must not carry a nonce or a clock — the exemption lives in \
                 repeat_carries_new_information, never here",
                r.as_str()
            );
            assert_ne!(
                hold_key(r, head_a, &c0, n0),
                hold_key(r, head_b, &c0, n0),
                "{}: …and it must still vary with the head, or the first assertion is \
                 satisfied by a constant",
                r.as_str()
            );
            assert_ne!(
                hold_key(r, head_a, &c0, n0),
                hold_key(r, head_a, &c1, n0),
                "{}: …and with the counters spent",
                r.as_str()
            );
            // **The line itself** (rev-final round 3). Everything a notice
            // interpolates that no field above models — the refusal above all —
            // reaches the key only through this, so a key that ignored the
            // notice would suppress a hold whose diagnosis had changed.
            assert_ne!(
                hold_key(r, head_a, &c0, n0),
                hold_key(r, head_a, &c0, n1),
                "{}: …and with the LINE, which is the only term that can see a changed \
                 refusal, a moved roster, or anything a future arm interpolates",
                r.as_str()
            );
        }
        // The digest is content-addressed and not positional: two different
        // strings of the same length must not collide by construction, and the
        // same string must digest the same way twice. (A digest CAN collide;
        // what this forbids is a stub that ignores its input or hashes only a
        // length.)
        assert_ne!(notice_digest("abcd"), notice_digest("abce"));
        assert_eq!(notice_digest("abcd"), notice_digest("abcd"));
        assert_ne!(notice_digest(""), notice_digest("a"));

        // The surviving exemption, stated as a SET rather than as a list of
        // calls: exactly the two rounded-duration reasons, so a sixteenth
        // reason folded in silently fails here as well as at the match.
        //
        // Sorted on both sides deliberately. The set is the property; the
        // order is `HeldReason::ALL`'s, which puts `drive-stalled` first and is
        // no part of what this asserts — pinning it would make a reordering of
        // that array read as an exemption changing.
        let mut exempt: Vec<&str> = HeldReason::ALL
            .into_iter()
            .filter(|r| repeat_carries_new_information(*r))
            .map(|r| r.as_str())
            .collect();
        exempt.sort_unstable();
        let mut want = vec!["state-stalled", "drive-stalled"];
        want.sort_unstable();
        assert_eq!(exempt, want, "{exempt:?}");
    }

    /// **A repeat whose REFUSAL changed is a different hold** (rev-final round
    /// 3) — the finding that the duration exemption was the right class on the
    /// wrong axis.
    ///
    /// `worker-unresumable`, `cap-refused` and `cap-full` interpolate
    /// `HeldFacts::refusal`, which is #1961's observation rather than a
    /// diagnosis: `unknown block "worker-adv"` and `Invalid session ID` send an
    /// orchestrator to different places. None of them spends a counter and none
    /// needs a push, so head and counters are exactly what does NOT move across
    /// such a repeat — and before the key digested the line, the second
    /// diagnosis reached no pane at all.
    ///
    /// **The tuple half is asserted equal**, or this test would pass under a key
    /// that happened to differ for some other reason and would prove nothing
    /// about the digest.
    #[test]
    fn a_repeat_whose_refusal_changed_is_not_the_same_hold() {
        let head = "aa11bb22cc33dd44";
        let c = Counters::default();
        for r in [HeldReason::WorkerUnresumable, HeldReason::CapRefused, HeldReason::CapFull] {
            let facts = |refusal: &str| crate::rddrive::HeldFacts {
                head: head.to_string(),
                refusal: refusal.to_string(),
                ..crate::rddrive::HeldFacts::default()
            };
            let n1 = crate::rddrive::held_notice(1758, r, &facts("unknown block \"worker-adv\""));
            let n2 = crate::rddrive::held_notice(1758, r, &facts("Invalid session ID"));
            assert_ne!(n1, n2, "{}: the fixture's premise — the LINE differs", r.as_str());

            let k1 = hold_key(r, head, &c, &n1);
            let k2 = hold_key(r, head, &c, &n2);
            assert_ne!(
                k1, k2,
                "{}: a changed refusal must be a changed key, or the second diagnosis is \
                 suppressed and the orchestrator is sent after the first one",
                r.as_str()
            );
            // …and the discriminator: everything the OLD key looked at is
            // identical across the pair, so the digest is what did the work.
            let tuple = |k: &str| k.rsplit_once('|').map(|(head, _)| head.to_string());
            assert_eq!(
                tuple(&k1),
                tuple(&k2),
                "{}: same reason, same head, same counters — this pair is exactly the one \
                 the tuple-only key could not tell apart",
                r.as_str()
            );
        }
    }

    // ── §2.1 the closed state enum ──────────────────────────────────────────

    #[test]
    fn the_state_enum_is_the_notes_seven_states() {
        // §2.1: "Four working states, one parked state, and two terminals."
        assert_eq!(DriveState::ALL.len(), 7);
        let live: Vec<&str> = DriveState::ALL
            .iter()
            .filter(|s| s.is_live())
            .map(|s| s.as_str())
            .collect();
        assert_eq!(live, vec!["ci-wait", "review-wait", "fix-wait", "gate-check"]);
        let parked: Vec<&str> = DriveState::ALL
            .iter()
            .filter(|s| s.is_parked())
            .map(|s| s.as_str())
            .collect();
        assert_eq!(parked, vec!["held"]);
        let terminal: Vec<&str> = DriveState::ALL
            .iter()
            .filter(|s| s.is_terminal())
            .map(|s| s.as_str())
            .collect();
        // The load-bearing half: `held` is NOT in this list. Pruning a parked
        // entry would silently grant three fresh review rounds (§5.2).
        assert_eq!(terminal, vec!["satisfied", "cancelled"]);
    }

    #[test]
    fn every_state_round_trips_through_as_str_and_parse() {
        for s in DriveState::ALL {
            assert_eq!(DriveState::parse(s.as_str()), Some(s), "{}", s.as_str());
        }
    }

    #[test]
    fn as_str_is_the_same_spelling_serde_writes() {
        // Two separate code paths: §5.2 persists the serde one while §5.4
        // audits the `as_str` one. Drift between them would write a file this
        // build's own `parse` refuses.
        for s in DriveState::ALL {
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(json, format!("\"{}\"", s.as_str()));
            let back: DriveState = serde_json::from_str(&json).unwrap();
            assert_eq!(back, s);
        }
        for r in HeldReason::ALL {
            let json = serde_json::to_string(&r).unwrap();
            assert_eq!(json, format!("\"{}\"", r.as_str()));
            let back: HeldReason = serde_json::from_str(&json).unwrap();
            assert_eq!(back, r);
        }
    }

    #[test]
    fn an_unknown_state_word_is_refused_never_coerced() {
        // There is no fallback variant to coerce it to, which is the point:
        // every available default either resumes a drive somebody stopped or
        // abandons one still running (§2.1).
        for bad in ["", "  ", "HELD", "landed", "queued", "unknown", "ci_wait"] {
            assert_eq!(DriveState::parse(bad), None, "{bad:?} must not parse");
        }
        // ...while a real one still does, so the loop above is not vacuous.
        assert_eq!(DriveState::parse(" held "), Some(DriveState::Held));
    }

    #[test]
    fn the_held_reasons_are_the_notes_sixteen() {
        assert_eq!(HeldReason::ALL.len(), 16);
        // §2.2: "There are **eighteen**" exits back to the LLM orchestrator —
        // the sixteen holds plus `satisfied` and `cancelled`. #2811 S5b added
        // `provider-limit`; the test NAME carries the count too, so a reason
        // added without touching the note fails here twice over.
        let exits =
            HeldReason::ALL.len() + DriveState::ALL.iter().filter(|s| s.is_terminal()).count();
        assert_eq!(exits, 18);
        // The two cap reasons are DIFFERENT exits, and nothing else here would
        // notice them collapsing into one spelling: `ALL` would still hold
        // sixteen entries and every one of them would still round-trip.
        assert_ne!(
            HeldReason::CapFull.as_str(),
            HeldReason::CapRefused.as_str(),
            "a lane starved by the cap and a hand-back refused by it are separate exits"
        );
        for r in HeldReason::ALL {
            assert_eq!(HeldReason::parse(r.as_str()), Some(r), "{}", r.as_str());
        }
        for bad in ["", "held", "stalled", "REVIEW-LIMIT", "review_limit", "messages"] {
            assert_eq!(HeldReason::parse(bad), None, "{bad:?} must not parse");
        }
    }

    // ── §2.1 the enumerated transition table ────────────────────────────────

    /// The twenty legal `(from, to)` pairs §2.1's thirteen arc rows name.
    /// Written out as data so the test below can assert the machine's legal set
    /// is *exactly* this: "get all 13, and add none the note does not name" is
    /// two claims, and a table of only-the-legal-ones would check one.
    fn expected_legal_pairs() -> Vec<(DriveState, DriveState)> {
        use DriveState::*;
        vec![
            (CiWait, ReviewWait),     // 2
            (CiWait, FixWait),        // 3
            (GateCheck, FixWait),     // 3, CONFLICTING at gate-check (#2311)
            (ReviewWait, GateCheck),  // 4
            (ReviewWait, FixWait),    // 5
            (ReviewWait, CiWait),     // 6
            (FixWait, CiWait),        // 7
            (FixWait, ReviewWait),    // 8
            (GateCheck, Satisfied),   // 9
            (GateCheck, CiWait),      // 10
            (Held, CiWait),           // 11
            (CiWait, Held),           // 12, over the four working/gate states
            (ReviewWait, Held),       // 12
            (FixWait, Held),          // 12
            (GateCheck, Held),        // 12
            (CiWait, Cancelled),      // 13, over all five non-terminals
            (ReviewWait, Cancelled),  // 13
            (FixWait, Cancelled),     // 13
            (GateCheck, Cancelled),   // 13
            (Held, Cancelled),        // 13
        ]
    }

    #[test]
    fn the_transition_table_is_exactly_the_notes_arcs() {
        let expected = expected_legal_pairs();
        assert_eq!(expected.len(), 20, "13 arc rows are 20 (from, to) pairs");
        let mut actual = Vec::new();
        for from in DriveState::ALL {
            for to in DriveState::ALL {
                if transition(from, to).is_ok() {
                    actual.push((from, to));
                }
            }
        }
        // Both directions, so neither a missing arc nor an invented one passes.
        for pair in &expected {
            assert!(
                actual.contains(pair),
                "{} -> {} is a note arc and was refused",
                pair.0.as_str(),
                pair.1.as_str()
            );
        }
        for pair in &actual {
            assert!(
                expected.contains(pair),
                "{} -> {} is legal and the note names no such arc",
                pair.0.as_str(),
                pair.1.as_str()
            );
        }
        assert_eq!(actual.len(), 20);
    }

    #[test]
    fn a_pair_the_table_does_not_name_is_refused_not_defaulted() {
        use DriveState::*;
        // A sample of the thirty refusals, each chosen because a fallthrough
        // implementation would silently accept it.
        for (from, to) in [
            (CiWait, GateCheck),     // skipping review entirely
            (CiWait, Satisfied),     // green CI is not a satisfied gate
            (ReviewWait, Satisfied), // a lane pass is not the gate's answer
            (FixWait, GateCheck),
            (FixWait, Satisfied),
            (Held, Held), // §2.1: `held` has exactly two outgoing arcs
            (Held, ReviewWait),
            (Held, FixWait),
            (Held, GateCheck),
            (Held, Satisfied),
            (Satisfied, CiWait), // terminal
            (Cancelled, CiWait),
            (Satisfied, Cancelled),
            (Cancelled, Satisfied),
        ] {
            let err = transition(from, to).unwrap_err();
            assert_eq!(err, InvalidTransition { from, to });
        }
    }

    #[test]
    fn a_self_transition_is_not_a_transition() {
        // Advancing to lane k+1 leaves the entry in `review-wait` and writes
        // the lane index (§2.1), which is why there is no review-wait arm.
        for s in DriveState::ALL {
            assert!(transition(s, s).is_err(), "{} -> itself", s.as_str());
        }
    }

    #[test]
    fn held_is_parked_with_exactly_two_ways_out() {
        let out: Vec<&str> = DriveState::ALL
            .iter()
            .filter(|to| transition(DriveState::Held, **to).is_ok())
            .map(|to| to.as_str())
            .collect();
        assert_eq!(out, vec!["ci-wait", "cancelled"]);
    }

    #[test]
    fn a_terminal_state_has_no_outgoing_arc_at_all() {
        for from in DriveState::ALL.iter().filter(|s| s.is_terminal()) {
            for to in DriveState::ALL {
                assert!(
                    transition(*from, to).is_err(),
                    "{} -> {}",
                    from.as_str(),
                    to.as_str()
                );
            }
        }
    }

    #[test]
    fn an_invalid_transition_names_both_ends() {
        let err = transition(DriveState::Held, DriveState::Satisfied).unwrap_err();
        assert_eq!(
            err.to_string(),
            "held -> satisfied is not a legal review-drive transition"
        );
    }
}
