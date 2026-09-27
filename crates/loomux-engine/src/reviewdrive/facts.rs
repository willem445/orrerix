//! §2.4's injected facts: [`DriveFacts`], [`LaneFact`], [`DriveStep`], and the
//! lane predicates [`decide`] asks those facts through.
//!
//! Part of the review driver's pure core, split out of the former single-file
//! `reviewdrive.rs` by #3498 P7 as a pure move. The module map is in `mod.rs`;
//! the design note is `docs/design/review-driver.md`.

use super::*;

// ── §2.4 the decision, over injected facts ──────────────────────────────────

/// What one observation of a driven PR's checks and mergeability concluded.
///
/// A closed vocabulary rather than `mqdriver`'s own result types, and that is
/// the seam: S3 maps `mqdriver::resolve_pr_detailed` /
/// `notify::pr_mergeability_result` and `mqdriver::pr_ci_green_detailed` /
/// `notify::pr_checks_result` onto these five, and this module never learns
/// what a `gh` invocation looks like.
///
/// Note what is **not** here: no "assume green", and no arm that folds an
/// unanswered lookup in with an answered one. §8 is emphatic on that — a
/// rate-limited `gh` returns *promptly* with a non-zero exit, so it is not a
/// runner failure, but `BaseUnverifiable` is still an **unknown** rather than a
/// fact about the PR, and "unknown is never treated as safe".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CiObservation {
    /// Checks are still running, or none has reported yet — which
    /// `notify::pr_checks_result` already reads as pending rather than green.
    Pending,
    /// Every required check terminal, none failing.
    Green,
    /// Terminal with failures.
    Red,
    /// GitHub reports the PR `CONFLICTING`: there is no clean merge ref, so no
    /// check suite will ever exist for it. Never discoverable by waiting.
    Conflicting,
    /// orrerix could not tell. Back off; no transition, no notice, bounded by
    /// `drive_timeout_minutes`.
    Unknown,
}

/// What the driven worker did since the hand-back (§2.1's `fix-wait` row).
///
/// Sourced from the **intercepted** `report` (§7), which is keyed on the
/// calling agent and never on a `ref` string a delegate typed — a delegate that
/// could choose whether its report reaches the orchestrator by naming a PR
/// number is a delegate that can route around the orchestrator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerSignal {
    // **Only the WORKER produces one of these**, and the name is the contract
    // rather than a label: arc 8 is "`report(done)` with the head unchanged" and
    // `held(worker-blocked)` names a worker's session, so feeding a reviewer
    // lane's report in here moves the drive on a hand-back that never happened.
    // Both sides reach the `report` MCP arm and a reviewer's `approved` resolves
    // to the same `done` word, so the arm decides on `DrivenRole` — the role is
    // what makes this type's name true (§7).
    /// Nothing yet.
    Silent,
    /// `report(done)`.
    Done,
    /// `report(blocked)`.
    Blocked,
    /// The fix could not be handed back to this drive's worker — see
    /// [`HeldReason::WorkerUnresumable`] for the causes this covers.
    ///
    /// **This is not a drive-time check, and the note is honest about why.**
    /// §5.1: a full, well-shaped session id this group never recorded takes
    /// `resolve_session_ref`'s `is_full_session_id` passthrough arm and is
    /// *accepted* by `drive_review`, so its unresumability surfaces here, at
    /// the first hand-back, possibly hours on. Resolving is not the same as
    /// proving resumable, and v1 does not prove it.
    ///
    /// **It is also produced AFTER a hand-back that succeeded** (#1961): the
    /// registry raises it when the pane the drive resumed exits in `fix-wait`
    /// with nothing reported, which is a resume that "worked" and then died on
    /// `Invalid session ID`. Before that the drive waited a full fix timeout on
    /// a process that was already gone.
    Unresumable,
}

/// What re-reading the gate concluded (§2.1's `gate-check` row).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateOutcome {
    /// The tick did not evaluate the gate. The gate is read in `gate-check` and
    /// nowhere else, so every other state passes this.
    NotEvaluated,
    /// `evaluate_merge_gate` satisfied at the live head, including every
    /// declared `also:` condition.
    Satisfied,
    /// Not satisfied, for any reason.
    Unsatisfied,
    /// The gate file is present and could not be read (an I/O error). **Not**
    /// `gate-not-configured`, which means the file is genuinely absent and is a
    /// drive-time decline (§5.1) rather than a hold.
    Unreadable,
}

/// One lane's live reading: the block the gate named, the verdict file as
/// `workflow::parse_verdict_file` returned it (or `None` for no verdict yet),
/// and whether the pane this drive recorded for that lane is GONE.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneFact {
    pub block: BlockId,
    pub verdict: Option<ReviewVerdict>,
    /// **The recorded pane is positively [`AgentStatus::Dead`]** (#2163) — the
    /// lane-side twin of the `worker_exit` observation `fix-wait` has had since
    /// #1961, and read for the same reason: `review-wait` waits for a verdict
    /// that a dead pane can never produce.
    ///
    /// Before this the exit was observed only for the worker and only in
    /// `fix-wait` — this module's own comment said so, and gave the reason as
    /// "`review-wait` has `lane-stalled` for its own panes". That bound is
    /// `lane_timeout_minutes` (60 at stock knobs) measured from the brief, so a
    /// reviewer pane killed at minute twelve left the drive silent for
    /// forty-eight more with no rd-* row at all — measured on PR #2140, and
    /// reached on the driver's OWN advice, since a `cap-refused` notice tells
    /// an orchestrator to kill an idle delegate and a lane that has finished
    /// its turn is on that list.
    ///
    /// **`false` is "we could not check" as well as "it is alive"**, and the
    /// fail direction is deliberate: the same asymmetry
    /// [`DriveEntry::forget_dead_panes`] states, resolved the same way
    /// `rd_pane_exit` resolves it — only a positive `Dead` counts, so an
    /// emptied agent map (a restart) re-opens nothing.
    ///
    /// A lane with no record, or one whose record carries no pane, is `false`:
    /// there is no pane to be dead, and the lane is opened by the ordinary path.
    pub pane_dead: bool,
}

/// Everything [`decide`] is allowed to know. Read by the tick, immediately
/// before the call; nothing in here is read from the entry, and nothing is
/// fetched inside the decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveFacts {
    /// The clock, injected. [`decide`] never reads one.
    pub now_ms: u64,
    /// Whether the PR is open. `Some(false)` **only on a positive answer**:
    /// `mqloop::draft_pr_open` returns `None` for a lookup it could not
    /// complete, and its doc says reconcile treats that as "the world does not
    /// match", never as "probably fine". Cancelling a live drive on a rate
    /// limit that clears in minutes is the failure this distinction stops.
    pub pr_open: Option<bool>,
    /// The PR's live head.
    pub head: String,
    /// The PR body's digest as it stands, or `None` when the body could not be
    /// read — the same fact `body_drift` takes in rather than fetching (#791).
    pub body_digest: Option<String>,
    /// The lanes the gate requires **at this head**, in
    /// `RoutingDecision::required`'s order. `None` is `route_reviewers`
    /// returning `None`: the changed-file list could not be shown complete, so
    /// which reviewers are required is unknown.
    pub required_lanes: Option<Vec<LaneFact>>,
    pub ci: CiObservation,
    pub worker: WorkerSignal,
    pub gate: GateOutcome,
    /// A driven delegate called `message_orchestrator` since the last tick
    /// (§7). Its own line was delivered unchanged, by its own arm; this is the
    /// routing fact beside it.
    pub messaged: bool,
    /// **A pane this drive owns is stopped on a provider's spend/usage
    /// refusal** (#2811 S5b) — the [`crate::providerlimit`] provider id, or
    /// `None` when no owned pane is showing one.
    ///
    /// A DRIVE-level fact rather than a per-[`LaneFact`] one, and that is a
    /// departure from plan-2504's wording ("`LaneFact::provider_limited`")
    /// argued in `docs/design/review-driver.md`: a drive in `fix-wait` owns a
    /// WORKER pane and no open lane at all, and that is precisely a drive the
    /// hold must cover. A per-lane field cannot see it. The tick unions every
    /// pane the drive owns — lanes and worker alike — and reports the provider,
    /// which is also the key the notice is aggregated on.
    ///
    /// **`None` is "no owned pane is showing one", including "we could not
    /// look"**, and the fail direction is the same one `pr_open` takes: a drive
    /// is never held on a reading orrerix could not complete. The cost of a
    /// missed detection is the pre-#2811 sixty-minute lane stall; the cost of a
    /// false one is N drives parked on a provider that is fine.
    pub provider_limited: Option<String>,
    /// **This process restarted while the drive was in `fix-wait`, and the
    /// worker pane it was waiting on died with the previous one** (#2811 S10).
    ///
    /// Set by the restart reconcile and by nothing else, because the reconcile
    /// is the only place the fact is KNOWN. A pane missing from the agent map
    /// mid-session is the ambiguous reading [`LaneFact::pane_dead`] declines to
    /// make; a pane missing on the first tick after a restart is not ambiguous
    /// at all, because every pane dies with the process. So the reconcile marks
    /// the entry and the decision below reads the mark, rather than either of
    /// them re-deriving "is this pane gone" from a map that cannot tell the two
    /// apart.
    ///
    /// It is a fact about the PROCESS, not about the session: whether the
    /// recorded worker session can actually be resumed is what the hand-back
    /// itself discovers, and a hand-back that cannot reach it still lands on
    /// `held(worker-unresumable)` by the ordinary path.
    pub restart_handback: bool,
    /// **This process restarted while the drive was in `ci-wait` waiting on the
    /// receipts for a push the worker had already made, and that worker's pane
    /// died with the previous process** (#3225).
    ///
    /// The sibling of [`restart_handback`](DriveFacts::restart_handback) one
    /// state later, and set by the same two places for the same reason: a pane
    /// absent from the roster is only unambiguously GONE where every pane is —
    /// the restart reconcile, and a `drive_review` an orchestrator issued
    /// against a live drive.
    ///
    /// What it licenses is narrow. [`decide_fix_receipts`] waits for the
    /// pushing worker's `report(done)` before briefing a lane, because green is
    /// not the end of the round — the receipts still have to reach the body.
    /// That report can never arrive from a pane that no longer exists, so the
    /// wait runs out `fix_timeout_minutes` and parks `held(fix-stalled)`: a
    /// claim that a worker went silent, about a worker orrerix was restarted
    /// under. The push itself is durable and is already at the head CI just went
    /// green on, so the mark says to treat that push as the fix delivered and
    /// brief the lane at that head.
    ///
    /// **What it does not do is charge anything.** A restart is not a round —
    /// [`DriveStep::Rehandback`]'s argument, applied to the arc this one takes.
    pub restart_push_delivered: bool,
}

/// What the tick should do with one entry, this tick.
///
/// **At most one state advance per entry per tick** (§2.4) is a property of
/// this type, not a discipline the caller has to remember: there is exactly one
/// advancing variant and it names one arc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriveStep {
    /// Nothing to do. The tick backs off per §2.4's principle — back off after
    /// any tick whose next attempt would make the same external calls and reach
    /// the same answer.
    Wait,
    /// Stay in `review-wait` and brief lane `index` — a spawn, or a resume of
    /// the lane's recorded session. **Not a transition** (§2.1): advancing to
    /// lane *k+1* leaves the entry where it is and writes the lane index, which
    /// is why the table has no `review-wait -> review-wait` arm.
    ///
    /// `verify` is #2168 E2: this brief is a **body-verification delta** — every
    /// required lane has passed the code at this head and only the PR body has
    /// moved, so what this lane is asked for is the body as it stands. It
    /// travels on the step rather than being re-derived at the write because it
    /// becomes a grant on the recorded verdict, and a grant derived twice is a
    /// grant that can disagree with itself.
    ///
    /// `body_only` is #2509, and travels for the same reason: this brief is
    /// about the BODY of a revision whose CODE this drive has already reviewed
    /// to completion — the head has not moved, this lane was already briefed at
    /// it, and every required lane has ANSWERED at it. It is strictly weaker
    /// than `verify`, which it nests inside (`verify` needs every lane to have
    /// PASSED), and the gap between the two is the case #2509 exists for: a
    /// lane that recorded `fail` on the body. It becomes
    /// [`LaneRecord::briefed_body_only`], which is the ONLY thing the one-shot
    /// grace below is granted on.
    OpenLane { index: usize, verify: bool, body_only: bool },
    /// Take one arc, spending `bump` first when there is one.
    Advance {
        to: DriveState,
        held_reason: Option<HeldReason>,
        bump: Option<Counter>,
    },
    /// Hand the worker back again from `fix-wait`, **taking no arc and
    /// spending no counter** (#2811 S10).
    ///
    /// The drive is already in `fix-wait` and stays there: the previous
    /// process handed this worker back, and only the PANE was lost. Re-briefing
    /// the recorded session is the cheapest thing that makes the drive live
    /// again, and it is not a new round — nothing about the review or the CI
    /// changed, so charging `review_rounds` for it would bill INVARIANT 9 for a
    /// restart. That is why this is a variant of its own rather than an
    /// `Advance` into the state the entry is already in: `advance` pairs an arc
    /// with its cost by construction, and there is no arc here to pair.
    ///
    /// The tick re-stamps [`DriveEntry::restamp_fix_handback`] when the
    /// hand-back reaches the worker, so `held(fix-stalled)` is measured from
    /// the brief the worker can actually answer rather than from one delivered
    /// to a pane that no longer exists.
    Rehandback,
}

impl DriveStep {
    pub(super) fn to(to: DriveState) -> DriveStep {
        DriveStep::Advance { to, held_reason: None, bump: None }
    }

    pub(super) fn held(reason: HeldReason) -> DriveStep {
        DriveStep::Advance {
            to: DriveState::Held,
            held_reason: Some(reason),
            bump: None,
        }
    }

    pub(super) fn spend(to: DriveState, bump: Counter) -> DriveStep {
        DriveStep::Advance { to, held_reason: None, bump: Some(bump) }
    }
}

/// Whether a lane's recorded verdict was recorded **about the revision now on
/// the PR** — the `(head, digest)` key, asked of the verdict WORD-BLIND.
///
/// §2.1's first carried-over property is written for a `pass` — *"a `pass` bound
/// to an old head, or to an old body digest, is not a pass"* — and the binding
/// rule underneath it is not about `pass` at all: **a verdict is bound to the
/// revision it reviewed**, so once the head moves the lane owes a fresh one
/// whatever word it last said. Reading the currency test only on the `pass` side
/// is what let a `fail` recorded three commits ago stay authoritative, and
/// [`decide_review_wait`] re-route it as though a reviewer had just spoken
/// (#1871 B1).
///
/// Asked with [`ReviewVerdict`]'s own methods rather than with comparisons
/// written here, because §4 makes the driver a reader of the gate and never a
/// third implementation of it. Both halves of the #565 asymmetry come along for
/// free that way: [`ReviewVerdict::reviewed`] is false for an empty head, so an
/// unbound verdict reads as stale and fails closed; and
/// [`ReviewVerdict::body_changed`] answers `None` when the drift cannot be
/// *known* — a verdict with no digest, or a body that could not be read — which
/// is not `Some(true)` and so does not stale the verdict. "We could not check"
/// and "it changed" are different answers, and only one of them may re-open a
/// lane.
pub fn lane_verdict_is_current(
    verdict: Option<&ReviewVerdict>,
    head: &str,
    body_digest: Option<&str>,
) -> bool {
    let Some(v) = verdict else { return false };
    v.reviewed(head) && v.body_changed(body_digest) != Some(true)
}

/// Whether a lane's recorded verdict is a `pass` that still counts at this
/// `(head, digest)` — the currency rule above, plus the word.
///
/// Kept as its own function because [`first_stale_lane`] asks a different
/// question from [`decide_review_wait`]'s match: "has this lane's pass settled
/// the revision in front of us" versus "does this lane have anything to say
/// about it". A stale `fail` answers **no** to the first and **no** to the
/// second, and before #1871 only the first was asked.
pub fn lane_pass_is_current(
    verdict: Option<&ReviewVerdict>,
    head: &str,
    body_digest: Option<&str>,
) -> bool {
    verdict.is_some_and(|v| v.verdict == Verdict::Pass)
        && lane_verdict_is_current(verdict, head, body_digest)
}

/// Whether a lane's `pass` still **settles** this revision — the currency rule
/// above, or the #2168 E2 delegation: a required lane recorded a
/// body-verification pass covering the body as it stands, and this pass is
/// bound to the same head.
///
/// **`verified` is passed in rather than computed here** because it is a
/// property of the whole required set, not of one lane, and re-deriving it per
/// lane would make the answer depend on iteration order. [`first_stale_lane`]
/// computes it once.
///
/// The delegation is asked through the gate's own
/// [`ReviewVerdict::pass_covers_body`], not re-implemented, for §4's reason: a
/// driver that decided "this lane is settled" on a rule the gate does not share
/// would drive to `gate-check` and be refused there for ever.
///
/// **That claim is scoped to the DELEGATION arm, and the scoping is not
/// pedantry** (#2308 review 1, premortem 2). The first arm is deliberately NOT
/// the gate's rule: [`lane_pass_is_current`] reads an unknown digest — the
/// verdict carries none, or the body could not be read now — as *not drift*,
/// because one transient `gh` failure must not re-brief every open lane in the
/// group, while the gate refuses an empty digest outright (unknown may never
/// discharge a merge condition). So on a gate declaring `body-unchanged`, a pass
/// recorded during a body-read outage settles here and is refused there, and the
/// drive cycles `gate-check -> ci-wait -> review-wait` on unchanged facts.
///
/// **Pre-existing, bounded, and deliberately not closed here.** That divergence
/// predates #2168 E2 — it is the #565 asymmetry meeting the #791 one, and both
/// arms are right about their own question — and it is bounded by
/// `drive_timeout_minutes` into `held(drive-stalled)`, which is the same exit
/// §8's `also: [base-green]` row parks on. Closing it means deciding which
/// asymmetry yields, which is a change to the gate's contract rather than to
/// this function, and it wants its own slice. What this slice DOES pin is that
/// the two sides agree about the verification question itself, over every
/// crossing of it: `the_driver_and_the_gate_answer_the_verification_question_identically`.
fn lane_pass_settles(
    verdict: Option<&ReviewVerdict>,
    head: &str,
    body_digest: Option<&str>,
    verified: bool,
) -> bool {
    if lane_pass_is_current(verdict, head, body_digest) {
        return true;
    }
    verdict.is_some_and(|v| v.pass_covers_body(head, body_digest, verified))
}

/// Whether this lane is open for exactly the revision now on the PR: it was
/// briefed at this head **and** at this body digest.
///
/// **Both signals, read by one rule, in one place.** The failure this exists to
/// prevent is the repo's "a guard reads every one of its inputs by one rule"
/// class, and it is not hypothetical — the head-only version of this comparison
/// shipped in the first push of this slice and was caught by
/// `a_pass_whose_body_digest_moved_re_opens_that_lane`. A lane that already
/// answered `pass` at this head and whose body then moved reads, under a
/// head-only key, exactly like a lane still thinking: the driver waits on a
/// reviewer that has already spoken, and `lane-stalled` eventually reports a
/// stall that never happened. §8's body-changed row wants that lane re-briefed
/// with a body-only delta.
///
/// **An unknown live digest does not mismatch**, and neither does an unrecorded
/// briefed one. "We could not check" is not "it changed" — the same asymmetry
/// [`ReviewVerdict::body_changed`] encodes by answering `None` rather than
/// `Some(false)` — and the alternative is that one transient `gh` failure to
/// read a PR body re-briefs every open lane in the group.
pub fn lane_open_for(rec: &LaneRecord, head: &str, body_digest: Option<&str>) -> bool {
    if rec.briefed_head != head {
        return false;
    }
    match body_digest {
        Some(now) if !now.is_empty() && !rec.briefed_digest.is_empty() => {
            rec.briefed_digest == now
        }
        _ => true,
    }
}

/// The `lane-stalled` anchor [`DriveEntry::open_lane`] must be given for the
/// brief about to be sent — `now_ms` for an ordinary one, and the anchor
/// already on the record when this brief is only REPLACING a dead pane (#2163).
///
/// **The re-open must not re-arm the clock, and that is what bounds the
/// re-open.** `decide_review_wait` re-opens a lane whose recorded pane is
/// `Dead`, so a pane that dies on every spawn would be replaced on every tick
/// for as long as the drive lives if each replacement started the silence
/// timer over. Preserving the anchor makes `lane-stalled` reachable through the
/// loop: the lane parks at `lane_timeout_minutes` from the ORIGINAL brief,
/// naming itself, and the notice's own remedy (read that pane) is the right one.
///
/// **It is also the more honest reading of the field.** `spawned_ms` anchors a
/// SILENCE test — `decide_review_wait`'s stall arm exempts a lane that has
/// answered — and a pane dying and being replaced does not make a lane less
/// silent about the head it was asked about. Re-arming would be the
/// defeat-your-own-bound shape `decide`'s empty-head guard describes.
///
/// **Only for a replacement at the SAME REVISION, and the revision is the full
/// `(head, digest)` key** — [`lane_open_for`], the one place that comparison is
/// written. A lane with no record, or one whose recorded pane is alive (an
/// ordinary re-brief, a reuse fall-through), gets `now_ms` too: nothing about
/// those is a replacement.
///
/// **The digest half is not decoration, and keying on the head alone was an
/// asymmetry** (#2169 review 2, N1). This module treats `(head, digest)` as ONE
/// revision key — §5.2 says so in as many words, `lane_open_for` implements it,
/// and a verdict binds to both — so a head-only test here answered "same round"
/// for a case the rest of the module calls a new one: a lane briefed at
/// `(H, d1)` whose pane dies, followed by a BODY-ONLY fix moving the digest to
/// `d2`, re-opens with a reviewer that has read nothing and inherits the
/// original anchor. Fifty minutes in, that fresh reviewer had nine minutes
/// before `held(lane-stalled)` — while the *same* body-only fix under a LIVE
/// idle pane re-arms to `now` and gets the full window, which is the same
/// revision getting two different clocks depending on whether a pane happened
/// to die.
///
/// **And the bound does not need the digest case.** The runaway this exists to
/// stop is a pane that dies on every spawn, which loops on the tick's own clock
/// with nothing else moving; a digest only moves when a human or a worker edits
/// the PR body, so it cannot drive a loop. Widening the exception to a new
/// revision therefore costs the bound nothing.
///
/// An **unknown** live digest does not re-arm: `lane_open_for` reads "we could
/// not check" as still-open rather than as drift, which is the same asymmetry
/// [`ReviewVerdict::body_changed`] encodes and the fail-safe direction here —
/// one transient `gh` failure to read a PR body must not hand every dying lane
/// a fresh hour.
///
/// A recorded anchor of `0` is a pre-field row, which is "unset" rather than
/// "the epoch"; it takes `now_ms` for the same reason `fix_handback_ms == 0`
/// means ancient rather than unset elsewhere in this module.
pub fn lane_stall_anchor(
    rec: Option<&LaneRecord>,
    head: &str,
    body_digest: Option<&str>,
    pane_dead: bool,
    now_ms: u64,
) -> u64 {
    match rec {
        Some(r)
            if pane_dead
                && !head.is_empty()
                && lane_open_for(r, head, body_digest)
                && r.spawned_ms != 0 =>
        {
            r.spawned_ms
        }
        _ => now_ms,
    }
}

/// Whether one of the required lanes has recorded a **body-verification pass**
/// covering the body as it stands (#2168 E2) — the driver's read of
/// [`mergeq::body_verified_by_required`](crate::mergeq::body_verified_by_required),
/// over the same routed reviewer list the gate would use.
///
/// One definition, asked through [`crate::workflow::body_verified`], because a
/// driver that answered this differently from the gate would advance to
/// `gate-check` and be refused there on every tick until `state-stalled`.
pub fn body_is_verified(required: &[LaneFact], head: &str, body_digest: Option<&str>) -> bool {
    crate::workflow::body_verified(
        required.iter().filter_map(|l| l.verdict.as_ref()),
        head,
        body_digest,
    )
}

/// The first lane whose `pass` does not stand at this (head, digest) — where
/// arc 8 re-enters after a body-only fix (§8 row 5), and equally where
/// `review-wait` resumes when a digest moves under a recorded pass.
///
/// Returns `required.len()` when every lane's pass stands, which is the
/// "nothing left to review" answer arc 4 acts on.
///
/// **Since #2168 E2 a body-only move re-briefs ONE lane, not all of them.**
/// Before the verification pass exists, every lane's pass is stale at the new
/// digest and this returns the FIRST of them — which is the lane the gate's own
/// `reviewers:` order puts first, and the order the driver already briefs in.
/// Once that lane answers with a verification pass,
/// [`body_is_verified`] is true and every other lane's pass at the head it was
/// bound to settles again, so this returns `required.len()` and the drive goes
/// to `gate-check` rather than walking the rest of the list. That walk is the
/// re-record cascade #2168 measured; the argument for accepting the delegation
/// is on `mergeq::body_unchanged`.
///
/// **Which lane is "first" is the gate file's decision, not this module's.**
/// `required` arrives in `RoutingDecision::required` order, which is the
/// `reviewers:` list a repo wrote plus the routed additions — so a repo says
/// which lane it wants asked first by writing it first, and orrerix never
/// forms an opinion about what a lane costs (CLAUDE.md constraint 8).
pub fn first_stale_lane(required: &[LaneFact], head: &str, body_digest: Option<&str>) -> usize {
    let verified = body_is_verified(required, head, body_digest);
    required
        .iter()
        .position(|l| !lane_pass_settles(l.verdict.as_ref(), head, body_digest, verified))
        .unwrap_or(required.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── the two carried-over gate properties (§2.1) ─────────────────────────

    #[test]
    fn a_pass_bound_to_an_old_head_or_an_old_body_is_not_a_pass() {
        let v = |verdict, head: &str, digest: &str| ReviewVerdict {
            pr: 1,
            block: "rev-std".into(),
            agent_id: "rev-1".into(),
            verdict,
            head: head.into(),
            body_digest: digest.into(),
            verified_body: false,
            open_findings: None,
            summary: String::new(),
            ts_ms: 0,
        };
        let now = Some("d1");
        // The positive control first, so every refusal below is a difference.
        assert!(lane_pass_is_current(
            Some(&v(Verdict::Pass, "head-a", "d1")),
            "head-a",
            now
        ));
        // Bound to an old head.
        assert!(!lane_pass_is_current(
            Some(&v(Verdict::Pass, "head-OLD", "d1")),
            "head-a",
            now
        ));
        // Bound to an old body.
        assert!(!lane_pass_is_current(
            Some(&v(Verdict::Pass, "head-a", "OLD")),
            "head-a",
            now
        ));
        // Unbound: an empty head can never equal a real one, so it fails closed.
        assert!(!lane_pass_is_current(
            Some(&v(Verdict::Pass, "", "d1")),
            "head-a",
            now
        ));
        // Not a pass at all, and no verdict at all.
        assert!(!lane_pass_is_current(
            Some(&v(Verdict::Fail, "head-a", "d1")),
            "head-a",
            now
        ));
        assert!(!lane_pass_is_current(None, "head-a", now));
        // "We could not check" is not "it changed": a verdict with no digest,
        // or a body that could not be read, does not stale a pass. Only
        // `Some(true)` re-opens a lane.
        assert!(lane_pass_is_current(
            Some(&v(Verdict::Pass, "head-a", "")),
            "head-a",
            now
        ));
        assert!(lane_pass_is_current(
            Some(&v(Verdict::Pass, "head-a", "d1")),
            "head-a",
            None
        ));
    }

    #[test]
    fn the_re_entry_point_is_the_first_lane_whose_pass_does_not_stand() {
        // Arc 8's "re-enters at the first stale lane" (§8 row 5).
        let lanes = vec![
            lane_fact("rev-std", Some(Verdict::Pass), "head-a", "d1"),
            lane_fact("rev-final", Some(Verdict::Pass), "head-OLD", "d1"),
            lane_fact("rev-extra", None, "", ""),
        ];
        assert_eq!(first_stale_lane(&lanes, "head-a", Some("d1")), 1);
        // Every lane standing is the "nothing left to review" answer arc 4 acts
        // on, and it is the length rather than an index.
        let all_good = vec![lane_fact("rev-std", Some(Verdict::Pass), "head-a", "d1")];
        assert_eq!(first_stale_lane(&all_good, "head-a", Some("d1")), 1);
        assert_eq!(first_stale_lane(&[], "head-a", Some("d1")), 0);
    }

    #[test]
    fn a_body_verification_never_stands_in_for_a_pass_at_another_head() {
        // **#2168 E2's delegation is bounded by the head, and this is the row
        // that pins it on the DRIVER side.** Its own test rather than a row
        // appended to `the_re_entry_point_is_the_first_lane_whose_pass_does_not_stand`,
        // because that test's first assertion covers the same predicate: a
        // neuter reddens it there and panics before ever reaching this, so the
        // red would evidence the older assertion and say nothing about this one
        // (CLAUDE.md — a red evidences only the assertion it reached and moved).
        //
        // What it is for: `pass_covers_body`'s direct arm shipped comparing
        // digests without asking whether the pass was bound to the head that
        // would merge. The gate's own loop filters that case out one line
        // earlier, so only the driver could reach it — and there a `pass`
        // recorded against code the worker had already fixed read as settling
        // the revision in front of the drive, walking straight past the lane
        // #1871 B1 exists to re-open.
        let across_heads = vec![
            verified_lane_fact("rev-std", "head-a", "d2"),
            lane_fact("rev-final", Some(Verdict::Pass), "head-OLD", "d1"),
        ];
        assert_eq!(
            first_stale_lane(&across_heads, "head-a", Some("d2")),
            1,
            "a verification of the body may stand in for a pass at THIS head, never for one \
             recorded against code the branch has left"
        );
        // The positive control: the same two lanes with rev-final's pass bound
        // to the live head — so the row above is the head deciding, not a
        // delegation that never fires.
        let same_head = vec![
            verified_lane_fact("rev-std", "head-a", "d2"),
            lane_fact("rev-final", Some(Verdict::Pass), "head-a", "d1"),
        ];
        assert_eq!(first_stale_lane(&same_head, "head-a", Some("d2")), 2);
    }

    #[test]
    fn a_held_step_always_carries_a_reason_and_no_other_step_ever_does() {
        // The same invariant `advance` enforces, asserted over everything
        // `decide` can actually emit — so a new arm cannot ship a reasonless
        // hold, or a reason riding an arc that is not one.
        let limits = DriveLimits::default();
        let mut seen_held = 0;
        let mut seen_other = 0;
        for st in DriveState::ALL {
            let mut e = entry_at(st);
            e.head = "head-a".into();
            for ci in [
                CiObservation::Green,
                CiObservation::Red,
                CiObservation::Conflicting,
                CiObservation::Pending,
                CiObservation::Unknown,
            ] {
                for worker in [
                    WorkerSignal::Silent,
                    WorkerSignal::Done,
                    WorkerSignal::Blocked,
                    WorkerSignal::Unresumable,
                ] {
                    for gate in [
                        GateOutcome::NotEvaluated,
                        GateOutcome::Satisfied,
                        GateOutcome::Unsatisfied,
                        GateOutcome::Unreadable,
                    ] {
                        for required in [
                            None,
                            Some(vec![lane_fact("rev-std", Some(Verdict::Fail), "head-a", "d1")]),
                        ] {
                            let facts = DriveFacts {
                                ci,
                                worker,
                                gate,
                                required_lanes: required.clone(),
                                ..facts_at("head-a")
                            };
                            match decide(&e, &facts, &limits) {
                                DriveStep::Advance {
                                    to: DriveState::Held,
                                    held_reason,
                                    ..
                                } => {
                                    assert!(held_reason.is_some());
                                    seen_held += 1;
                                }
                                DriveStep::Advance { held_reason, .. } => {
                                    assert!(held_reason.is_none());
                                    seen_other += 1;
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
        // Positive controls: the sweep really did produce both shapes, so the
        // assertions above are not passing over an empty walk.
        assert!(seen_held > 0, "the sweep produced no hold at all");
        assert!(seen_other > 0, "the sweep produced no ordinary advance");
    }
}
