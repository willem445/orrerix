//! §2.4 the decision: [`decide`] and its per-state arms, over facts it is
//! handed rather than reads.
//!
//! Part of the review driver's pure core, split out of the former single-file
//! `reviewdrive.rs` by #3498 P7 as a pure move. The module map is in `mod.rs`;
//! the design note is `docs/design/review-driver.md`.

use super::*;

/// **The decision.** One entry, one tick, one step — pure, and that purity is
/// what makes S3's integration testable: no I/O, no `gh`, no clock read, no
/// file. Everything it knows is in `facts`.
///
/// # The order the conditions are asked in
///
/// Precedence is a decision the note does not spell out arc by arc, so it is
/// argued here rather than left to the reading order of a `match`:
///
/// 1. **A terminal or parked entry yields [`DriveStep::Wait`] immediately.**
///    §2.1's `held` row reads "nothing; the tick does not advance it" — a
///    parked drive is left for `drive_review` or `cancel_review_drive`, and if
///    the tick could move one, `held` would not be a park.
/// 2. **A positively-closed PR cancels**, before anything else can hold it.
///    §8: `cancelled`, and only on a positive answer. A PR that is gone has no
///    hold worth reporting, and `cancelled` says more than `drive-stalled`.
/// 3. **`messaged` holds next.** The delegate's own words are already in the
///    orchestrator's pane (§7 — `message_orchestrator` is never intercepted),
///    so this hold is the routing fact that explains them. Holding for anything
///    else here would leave that message beside a hold that does not account
///    for it.
/// 4. **The drive's age holds next, and it must outrank the per-state logic**
///    — this is the one ordering §8 forces rather than merely permits. Its
///    `also: [base-green]` row parks a drive on a red default branch by cycling
///    `gate-check` -> `ci-wait` on every wake; that drive *always* has an
///    advance available, so an age check that ran after the per-state logic
///    would never be reached and the drive would cycle forever. The bound is
///    what keeps stopping the line from being silent. Since #2110 it is the
///    BACKSTOP rather than the working bound, and the reason it still outranks
///    everything below is unchanged: it is the one bound the base-green cycler
///    cannot reset.
/// 5. **Then time in the current state** (#2110) — [`state_bound_ms`], reset by
///    every transition, which is what an orchestrator actually wants bounded.
///    Below the age for the reason argued at the line itself: both are past
///    only for a drive resumed out of a very long park, where the twelve-hour
///    figure is the more important of the two.
/// 6. **A CONFLICTING PR takes arc 3, from every state that can leave for
///    `fix-wait`** (#2311) — argued at the line itself. It is a fact about the
///    PR rather than about the wait, so it outranks each state's own reading:
///    at `gate-check` the gate's own `satisfied`, and at `review-wait` the
///    routing question, which cannot be answered for a conflicted head at all
///    (no changed-file list) and therefore reports a CONSEQUENCE of the
///    conflict as if it were an independent fact. Below the bounds above,
///    which are about the drive rather than the PR.
/// 7. Then the state's own logic.
pub fn decide(entry: &DriveEntry, facts: &DriveFacts, limits: &DriveLimits) -> DriveStep {
    let state = entry.state();
    if state.is_terminal() || state.is_parked() {
        return DriveStep::Wait;
    }
    if facts.pr_open == Some(false) {
        return DriveStep::to(DriveState::Cancelled);
    }
    if facts.messaged {
        return DriveStep::held(HeldReason::Messaged);
    }
    // **A provider outage outranks every bound below it** (#2811 S5b), and the
    // placement is the whole of "no lane timeout and no round spent".
    //
    // Everything from the age backstop down is measuring a WAIT. When the
    // account behind this drive's panes is out of budget those waits are not
    // measuring anything about the drive: the panes are not slow, they are
    // stopped, and no bound the repo can configure makes a vendor's billing
    // resolve sooner. Letting `drive-stalled`, `state-stalled`, `lane-stalled`
    // or `fix-stalled` answer first would report a timeout whose remedy ("read
    // that pane") does not work, an hour late, once per affected drive — which
    // is exactly the measured behaviour #2811 was filed for.
    //
    // Above them, and below `messaged`, because a delegate that actually spoke
    // has said something specific and this has not. It spends no counter for
    // the same reason it outranks the bounds: a drive that survived an outage
    // must not come out of it looking like one that had burned its budget.
    if let Some(provider) = facts.provider_limited.as_deref() {
        if !provider.trim().is_empty() {
            return DriveStep::held(HeldReason::ProviderLimit);
        }
    }
    // **The bounds are clamped HERE, on the values actually read.** §2.3's
    // ranges are a capability boundary, not input hygiene: a repo's `driver:`
    // block may run a tighter loop than INVARIANT 9 and may never run a looser
    // one, because `docs/design/workflows.md`'s closure is that a workflow file
    // selects from what loomux permits and never widens it. S2 clamps as it
    // parses; this clamps again, and the second is not redundant, because this
    // is a `pub fn` over a plain value type that any caller in any crate can
    // reach without going through S2's parser. A boundary that holds only when
    // the expected caller is upstream is not a boundary.
    let limits = &limits.clamped();
    if entry.bounded_age_ms(facts.now_ms) >= minutes_ms(limits.drive_timeout_minutes) {
        return DriveStep::held(HeldReason::DriveStalled);
    }
    // **5a. Then time in THIS state** (#2110) — the bound that does the work,
    // with the age above it as the backstop it falls through to.
    //
    // Below the age deliberately, and the ordering is not the one it looks like
    // it should be. The state bounds are smaller, so in the ordinary run of
    // events one of them fires first *in time* and this line is never reached
    // with both past. Where both ARE past, the drive has just been resumed out
    // of a long park — arc 11 re-stamps `started_ms` and `state_since_ms`
    // together, so the only way to be past the twelve-hour backstop at all is a
    // drive that has genuinely run that long, and that fact outranks whichever
    // state it happens to be sitting in. Putting this first would also reopen
    // §8's `also: [base-green]` row from the other end: that drive resets this
    // clock on every wake, so it must reach the age check, and an ordering that
    // let a per-state bound answer first for some other drive is one more thing
    // to keep true.
    //
    // `required_lanes` is the gate's list at this head; `None` is a routing
    // answer this tick could not produce, and that drive holds
    // `routing-unaccountable` in `decide_review_wait` below on this same tick.
    let lanes = facts.required_lanes.as_deref().map_or(0, |l| l.len());
    if let Some(bound) = state_bound_ms(state, limits, lanes) {
        if entry.state_elapsed_ms(facts.now_ms) >= bound {
            return DriveStep::held(HeldReason::StateStalled);
        }
    }
    // **An unresolved head is not a head, and acting on one is an unbounded
    // spawn loop.** Two of the four states below read `facts.head`, and the
    // guard is here rather than in each of them because it is the *drive* that
    // has no revision to act on, not those two states in particular:
    // `review-wait` compares it against the recorded head and keys a lane brief
    // on it, `fix-wait` compares it for arc 7. `ci-wait` and `gate-check` never
    // read it at all — they would be harmless to dispatch, and returning `Wait`
    // for them too is the conservative reading of "orrerix could not resolve
    // this PR's head this tick", not a claim that they would misbehave.
    // `review-wait` is the dangerous one: the arc-6 guard skips itself when
    // `facts.head` is empty, so the tick falls through to `first_stale_lane`,
    // where `ReviewVerdict::reviewed("")` is false for every real verdict head,
    // and then to `lane_open_for`, which refuses every record briefed at a real
    // head — yielding `OpenLane{0}` on EVERY tick. Worse, each brief re-arms
    // that lane's `spawned_ms`, so `lane-stalled` can never fire and the loop
    // defeats the very bound meant to catch it. §8's posture settles it: an
    // unknown is never a fact, and the drive stays bounded by `drive-stalled`
    // above, which needs no head at all.
    if facts.head.is_empty() {
        return DriveStep::Wait;
    }
    // **6. A CONFLICTING PR takes arc 3 wherever it is observed** (#2311).
    //
    // Read here rather than inside a state, because a conflict is a fact about
    // the PR and not about what the drive happens to be waiting for, and every
    // state that read it separately was a state that could answer something
    // else first:
    //
    // - `gate-check` read `facts.required_lanes` and `facts.gate` — and
    //   nothing about MERGEABILITY, which is the whole of the defect. It was
    //   never a state that read one input: routing has answered
    //   `routing-unaccountable` here since v1. So a base that moved while the
    //   lanes reviewed reached `satisfied` with every lane passed and the PR
    //   unmergeable — #2942, where `rev-final` passed carrying
    //   `mergeable:CONFLICTING` in its own summary and the cost (a hand rebase,
    //   a re-drive at `rounds_already_spent 3`, two fresh whole-diff lanes, ten
    //   minutes of cap starvation, three orchestrator turns) was paid outside
    //   the driver.
    // - `review-wait` asks `route_reviewers` FIRST, and routing needs the
    //   changed-file list, which GitHub does not compute for a conflicted head.
    //   So the same conflict parked the drive `held(routing-unaccountable)` —
    //   a hold whose notice says *which reviewers are required is unknown* for
    //   a PR whose real problem is that it does not merge, and whose remedy
    //   (`drive_review` again) reproduces it. Measured on #3118.
    //
    // Above the routing check for exactly that reason: routing being
    // unaccountable is a CONSEQUENCE of the conflict there, not an independent
    // fact, and the honest report is the one naming the cause. It stays BELOW
    // the bounds and the `messaged` hold above — those are about the drive
    // rather than the PR, and a drive already past its clock is not made young
    // by a rebase — and below the empty-head guard, which is "we could not read
    // this PR at all". That ordering is pinned by
    // `the_bounds_and_the_messaged_hold_outrank_a_conflict`, because a
    // precedence stated in a comment and asserted nowhere is a claim about the
    // order of two `if`s that any edit can silently reverse.
    //
    // **`fix-wait` is the one state excluded, and the exclusion is the
    // `state != DriveState::FixWait` clause below — an opinion, argued here.**
    // The arc table is the BACKSTOP, not the mechanism: `(fix-wait, fix-wait)`
    // is not a legal transition, so without this clause `decide` would propose
    // a step `take` refuses, and the drive would audit `invalid-transition`
    // every tick instead of waiting. The opinion is that a rebase hand-back is
    // already outstanding there and the worker's own signals are what that
    // state waits on: spending a second `rebase_attempts` on the conflict the
    // worker was just asked to fix would park `rebase-limit` before the worker
    // had a chance to push.
    //
    // `Pending`/`Unknown` are NOT conflicts: §8's posture is that an unknown is
    // never a fact about the PR, so a mergeability orrerix could not read is
    // not evidence that a rebase is owed.
    if facts.ci == CiObservation::Conflicting && state != DriveState::FixWait {
        return if counter_exhausted(entry.counters.rebase_attempts, limits.max_rebase_attempts) {
            DriveStep::held(HeldReason::RebaseLimit)
        } else {
            DriveStep::spend(DriveState::FixWait, Counter::RebaseAttempts)
        };
    }
    match state {
        DriveState::CiWait => decide_ci_wait(entry, facts, limits),
        DriveState::ReviewWait => decide_review_wait(entry, facts, limits),
        DriveState::FixWait => decide_fix_wait(entry, facts, limits),
        DriveState::GateCheck => decide_gate_check(entry, facts, limits),
        // Both returned above; repeated here because the enum is closed and a
        // catch-all arm is exactly what §2.1 forbids.
        DriveState::Held | DriveState::Satisfied | DriveState::Cancelled => DriveStep::Wait,
    }
}

fn decide_ci_wait(entry: &DriveEntry, facts: &DriveFacts, limits: &DriveLimits) -> DriveStep {
    match facts.ci {
        // **Arc 2, and since #2168 E1 green is not on its own enough when the
        // head arrived by arc 7.** See [`decide_fix_receipts`] for the whole
        // argument; the guard is here, on the Green arm alone, because it is
        // the only arm that briefs a reviewer. Red and CONFLICTING hand the
        // worker back through arc 3, where `decide_fix_wait`'s own ladder
        // already answers every worker signal, and `Pending`/`Unknown` wait as
        // they always did.
        CiObservation::Green if entry.fix_pushed() => decide_fix_receipts(entry, facts, limits),
        // Arc 2.
        CiObservation::Green => DriveStep::to(DriveState::ReviewWait),
        // Arc 3, spending a CI attempt — or parking, when the budget is gone.
        CiObservation::Red => {
            if counter_exhausted(entry.counters.ci_attempts, limits.max_ci_attempts) {
                DriveStep::held(HeldReason::CiLimit)
            } else {
                DriveStep::spend(DriveState::FixWait, Counter::CiAttempts)
            }
        }
        // Arc 3 again — **answered by [`decide`] before this function is
        // reached** (#2311), which is why it is not a second ladder here: a
        // conflict is a fact about the PR, so every state that can act on one
        // acts through the same rule, with the same counter and the same
        // `held(rebase-limit)`. Kept as an explicit arm rather than folded
        // into the catch-all because the enum is closed and §2.1 forbids a
        // catch-all; `Wait` is what it degrades to if a caller ever reaches
        // this function directly, which is the conservative direction.
        //
        // `Pending`/`Unknown` are neither an answer nor a reason to move. §8:
        // unknown is never treated as safe, and it is never treated as a fact
        // about the PR either.
        CiObservation::Conflicting | CiObservation::Pending | CiObservation::Unknown => {
            DriveStep::Wait
        }
    }
}

/// Green at a head the worker pushed under arc 7: **arc 2 waits for that
/// worker's `report(done)` first** (#2168 E1, closing #1875's class).
///
/// # What green alone does not mean
///
/// A green matrix says the checks settled; it does not say the ROUND is over.
/// This repo's worker persona forbids a `report(done)` before the whole matrix
/// has been re-read, and the PR body's CI receipts — run ids, per-platform
/// conclusions, the head they were measured at — cannot be written until that
/// same moment. So the sequence a green observation sits in the middle of is
/// fixed: push, checks settle, worker reads them, worker edits the body, worker
/// reports. A lane briefed on the green brief is briefed at digest d1 and
/// re-briefed at d2 the moment the receipts land, because
/// [`first_stale_lane`] re-reads the `(head, digest)` key every tick and a
/// `pass` recorded at d1 does not stand at d2. #1875's measurement is that
/// every code PR of that session paid at least one such round, and #1870 is the
/// fully instrumented one: `pass` at digest `bbff76b8`, 0 findings, the CI
/// section filled, `BODY CHANGED SINCE PASS`, gate blocked, re-record — with
/// the head never having moved and not one line of code having changed.
///
/// The digest rule itself is not weakened here and must not be: the body
/// becomes the squash commit message, so a `pass` recorded against different
/// text really has approved something else. What changes is only WHEN the lane
/// is briefed — after the revision has stopped moving rather than during.
///
/// # The two shapes this deliberately is not
///
/// #1875 offers three candidate fixes and two are refused. A **digest
/// carve-out** — a fenced evidence region excluded from the digest — needs that
/// region to be genuinely un-claim-bearing, and a CI section that also carries
/// prose is back where it started; it would also make `body-unchanged` a
/// weaker condition than the `gh` shim's, which §4 forbids. **The engine
/// writing the CI section itself** puts a second author on the PR body beside
/// the worker, which is a wire change and a new class of §3.1 action, for
/// receipts the worker already produces.
///
/// # The worker ladder is `decide_fix_wait`'s, not a second one
///
/// The four signals are read in the same order and answered with the same
/// holds, because this is the same wait for the same worker on the same
/// hand-back — only its location moved. Reading them by a different rule here
/// is the asymmetry `CLAUDE.md` names ("a guard reads every one of its inputs
/// by one rule"), and the two that would be dropped are the two that matter
/// most: a `Blocked` worker is INVARIANT 3 territory and a dead resumed pane is
/// `worker-unresumable`, and letting either fall through to the timeout below
/// would report both as `fix-stalled` — a claim that the worker went silent,
/// about a worker that said something.
///
/// # The bound, and why it is `fix_timeout_minutes` measured from the push
///
/// A silent worker must not hold the drive for ever, and the wait is the same
/// wait §2.2 already bounds — so it takes the same knob. The anchor is
/// [`DriveEntry::fix_pushed_ms`], the LATEST push in this `ci-wait` stay. Not
/// `fix_handback_ms`, which predates the push and would charge the worker for
/// the time it spent doing the work it was asked to do; and not
/// `state_since_ms`, which is the FIRST push, so a follow-up commit late in the
/// window would get the remainder of a window rather than one (rev-final round
/// 2, premortem 2).
/// [`state_bound_ms`]'s `ci-wait` arm ADDS the same knob to its constant so
/// that `held(state-stalled)` cannot preempt this hold on a repo that raised
/// it — the property [`HeldReason::StateStalled`] states, and the reason that
/// arm is an add rather than a max is argued there.
///
/// **`fix_timeout_minutes` cannot push that bound past the backstop**, which is
/// why this arm needs no counterpart to [`state_bound_ms`]'s `review-wait`
/// residual: `parse_workflow` runs the knob through `clamp_expires_minutes`, so
/// the largest value a workflow file can produce is 240 minutes and the sum
/// tops out at 330 against a twelve-hour `drive_timeout_minutes`. The engine
/// deliberately does not re-clamp the timeouts ([`DriveLimits::clamped`] takes
/// only the counters), so a caller building limits directly can still exceed
/// it — and there the age answers first, exactly as `review-wait`'s residual
/// describes. Pinned at the ceiling by
/// `the_ci_wait_bound_funds_the_receipts_wait_instead_of_preempting_it`.
///
/// # Two residuals, both bounded and both recoverable by the resume
///
/// **A worker pane that DIES here.** #1961's exit read is asked only in
/// `fix-wait` (`rdtick`'s `worker_exit`), so a pane killed while the drive
/// waits in `ci-wait` is not seen as `Unresumable` on the next tick; that drive
/// waits out `fix_timeout_minutes` and parks `held(fix-stalled)`, which is
/// bounded and named but is the slower notice. Disclosed rather than closed:
/// widening the exit read is an S3 change to what a tick OBSERVES, and this
/// slice changes what `decide` does with what it is already given.
///
/// **#3225 closed the RESTART half of that, and only that half.** A pane that
/// died because the process did is not ambiguous — every pane did — so the
/// reconcile marks the entry and the `Silent` arm below treats the recorded
/// push as the fix delivered. A pane killed mid-session while orrerix keeps
/// running is still the slower notice, for the reason above: nothing in
/// `ci-wait` observes that exit, and an absent agent mid-session is "we could
/// not check" rather than "it is gone".
///
/// **A worker that pushes and reports inside ONE tick window.** Both facts
/// reach the same tick, [`decide_fix_wait`]'s arc 7 outranks the report on
/// purpose ("the code moved and CI is what has to answer next"), and `rdtick`
/// clears the signal on every arc — so the report is spent on the arc and this
/// state waits for one that will not come, to `held(fix-stalled)`. It is not
/// closed here because a `WorkerSignal` is a word and not a timestamp: the same
/// `Done` beside a moved head is equally consistent with *reported, then
/// pushed*, where the report is about the pre-push tree and honouring it would
/// brief a lane over exactly the unfinished revision #1875 is about. Failing
/// toward a bounded hold rather than toward that defect is the direction this
/// slice takes everywhere, and the ordering is uncommon here by construction —
/// the worker persona forbids `report(done)` before the matrix is re-read, and
/// the matrix takes twenty to thirty minutes against a thirty-second tick.
///
/// **Both clear on the first tick after `drive_review`**, which is what §2.2
/// requires of a hold whose cause is a wait: arc 11 re-enters `ci-wait` from
/// `held`, so [`DriveEntry::advance`] assigns [`DriveEntry::fix_pushed_ms`] false
/// and the next green takes arc 2 with nothing further asked of the worker.
/// Pinned by `a_resume_out_of_fix_stalled_briefs_on_the_next_green`.
fn decide_fix_receipts(entry: &DriveEntry, facts: &DriveFacts, limits: &DriveLimits) -> DriveStep {
    match facts.worker {
        // Nothing to hand back to. Checked first, exactly as in
        // `decide_fix_wait`: every arm below presumes a worker that can be
        // reached.
        WorkerSignal::Unresumable => DriveStep::held(HeldReason::WorkerUnresumable),
        // INVARIANT 3 territory — a blocked worker is the orchestrator's call.
        WorkerSignal::Blocked => DriveStep::held(HeldReason::WorkerBlocked),
        // Arc 2 at last: the revision has stopped moving, so the lane can be
        // briefed at a `(head, digest)` that will still be current when it
        // records.
        WorkerSignal::Done => DriveStep::to(DriveState::ReviewWait),
        // **#3225: the pane whose report this wait is for died with the
        // previous process, so the push it already made IS the fix delivered.**
        //
        // Above the timeout below rather than beside it, and the difference is
        // the whole issue: the timeout's exit is `held(fix-stalled)`, which says
        // a worker went silent, and the worker did not — orrerix was restarted
        // under it. The push is durable, it is at the head CI has just gone
        // green on, and the only thing missing is a `report` no pane can send.
        //
        // What the lane loses by being briefed here is the CI receipts the
        // worker would have written into the body first — so the lane is briefed
        // at digest `d1` and re-briefed at `d2` if a human or a later worker
        // fills them in, which is exactly the round [`decide_fix_receipts`]'s
        // own header describes and is bounded by the same `(head, digest)` key.
        // That is one re-brief; the alternative measured on #3225's incident was
        // a whole `fix_timeout_minutes` and an orchestrator turn.
        //
        // **Only under `Silent`**: a `Done`, a `Blocked` or an `Unresumable`
        // signal is something this drive was actually told, and the arms above
        // answer each of them. The mark cannot manufacture one.
        WorkerSignal::Silent if facts.restart_push_delivered => {
            DriveStep::to(DriveState::ReviewWait)
        }
        WorkerSignal::Silent => {
            // **The LATEST push in this `ci-wait` stay**, which is what
            // `fix_pushed_ms` exists to carry — see that field, and
            // `note_fix_push` for the re-stamp. `state_since_ms` would be the
            // first one, and a worker that pushed a follow-up commit late in
            // the window would get the remainder rather than a window.
            //
            // The fallback is unreachable through `decide`: this function is
            // called from one place, under a guard that already asked
            // `fix_pushed()`. It is `state_since_ms` rather than a panic or a
            // zero because those are the two ways a fallback goes wrong — a
            // zero clears every timeout, which is the false-park direction the
            // field doc argues against, and an unwind out of the poll thread
            // takes the fleet's watches down with it.
            let since = entry.fix_pushed_ms.unwrap_or(entry.state_since_ms);
            if facts.now_ms.saturating_sub(since) >= minutes_ms(limits.fix_timeout_minutes) {
                DriveStep::held(HeldReason::FixStalled)
            } else {
                DriveStep::Wait
            }
        }
    }
}

fn decide_review_wait(entry: &DriveEntry, facts: &DriveFacts, limits: &DriveLimits) -> DriveStep {
    let Some(required) = facts.required_lanes.as_deref() else {
        return DriveStep::held(HeldReason::RoutingUnaccountable);
    };
    // Arc 6: the head moved under a lane. Checked before the verdicts are read,
    // because a verdict that lands at the old head is answered by the binding
    // rules, not by this state — a `fail` there still routes when the drive
    // comes back through `review-wait`, and a `pass` there is already stale.
    if !facts.head.is_empty() && entry.head != facts.head {
        return DriveStep::to(DriveState::CiWait);
    }
    let digest = facts.body_digest.as_deref();
    // Arc 4's precondition, and equally the re-entry point when a digest moved
    // under a recorded pass (§8's body-changed row): the first lane whose pass
    // does not stand here.
    let k = first_stale_lane(required, &facts.head, digest);
    if k >= required.len() {
        return DriveStep::to(DriveState::GateCheck);
    }
    let lane = &required[k];
    // **Is the brief this state may be about to send a body-verification delta?**
    // (#2168 E2.)
    //
    // The precondition is the whole of what makes the delegation honest, so it
    // is spelled as one `all` over the required set rather than inferred from
    // the fact that `k` happened to be stale: EVERY required lane has recorded a
    // `pass` bound to the head that would merge. Nothing about the CODE is
    // outstanding, so the only thing that can have staled lane `k` above is the
    // body — and what the lane is about to be asked for is the body as it
    // stands.
    //
    // A readable live digest is required too (`digest.is_some()`), because the
    // grant this writes is meaningless without one: the verdict's own digest is
    // what a gate later compares. "We could not read the body" grants nothing,
    // the same direction `lane_open_for` and `ReviewVerdict::body_changed` take
    // one question over.
    //
    // Deliberately NOT conditioned on the digest having moved for lane `k`. It
    // has — a lane whose pass is bound to this head reaches `k` only when
    // `lane_pass_settles` said no, and at this point `body_is_verified` is false
    // (or `first_stale_lane` would have returned `required.len()`), so the digest
    // is the only axis left. Re-deriving that here would be a second
    // implementation of `first_stale_lane`'s answer, and the two could drift.
    let verify = digest.is_some()
        && required.iter().all(|l| {
            l.verdict.as_ref().is_some_and(|v| {
                v.verdict == Verdict::Pass && v.reviewed(&facts.head)
            })
        });
    // **Is this brief about the BODY of a revision whose CODE is already fully
    // reviewed?** (#2509.)
    //
    // One conjunct weaker than `verify` above and one stronger, and both
    // differences are load-bearing.
    //
    // Weaker: every required lane must have ANSWERED at this head, not passed
    // at it. That gap IS the case #2509 was filed for — a lane that recorded
    // `fail` on the PR body, whose worker then moved the body and not the head,
    // which `verify` can never see because that lane's word is not `pass`.
    // Measured on PR #2397, which reached `held(review-limit)` on two body
    // sentences with the code green and unmoved throughout.
    //
    // Stronger: THIS lane must already have been briefed at this head. Without
    // it the very first brief at a head qualifies — every lane can be bound to
    // a head the moment they have all spoken — and the grace would be granted
    // for a fail on code nobody had reviewed twice. `briefed_head` is what says
    // this is a RE-brief, and a re-brief at an unchanged head can only be about
    // the body: `lane_open_for` is what routed us here, and at an equal head the
    // digest is the only axis it has left.
    //
    // A readable live digest is required for `verify`'s reason: what this
    // grants is read back against an exact `(briefed_head, briefed_digest)`
    // pair, and a brief whose revision cannot be pinned grants nothing.
    let body_only = digest.is_some()
        && entry
            .lane(&lane.block)
            .is_some_and(|r| !r.briefed_head.is_empty() && r.briefed_head == facts.head)
        && required
            .iter()
            .all(|l| l.verdict.as_ref().is_some_and(|v| v.reviewed(&facts.head)));
    // **A verdict decides only if it was recorded about THIS revision**, and the
    // currency test is asked here rather than inside the arms so no future word
    // can be added below without it (#1871 B1).
    //
    // The defect this closes: a lane's `fail` recorded at the head the worker has
    // since fixed stayed authoritative for ever. The drive took arc 7 out of
    // `fix-wait` on the new head, went green, came back here — and read the SAME
    // stale `fail`, spent a review round on it, and handed the worker back its
    // own already-addressed findings as "attempt 2". Three passes reached the
    // bound with no re-review having happened at all. Nothing re-opened the lane
    // because nothing below ever reached [`lane_open_for`]: the `Fail` arm
    // answered first, and it answered from a commit that no longer described the
    // PR.
    //
    // **Word-blind on purpose.** `escalate` is the same shape — an escalation of
    // a revision that no longer exists is not a judgment anyone is being asked
    // for — and so is a `pass`, which [`first_stale_lane`] has already filtered
    // for currency before this line is reached. Treating a stale verdict as
    // ABSENT is what puts the lane back on the `Some(Verdict::Pass) | None` arm,
    // where [`lane_open_for`] decides between re-briefing it and waiting for a
    // brief already out at this revision — which is the re-open the head change
    // owed and never got.
    let current = lane
        .verdict
        .as_ref()
        .filter(|v| lane_verdict_is_current(Some(v), &facts.head, digest));
    match current.map(|v| v.verdict) {
        // A lane recorded `escalate` at this revision: an LLM judgment call, and
        // §3 says the driver never makes one.
        Some(Verdict::Escalate) => DriveStep::held(HeldReason::Escalate),
        // Arc 5, spending a review round — or parking, when the budget is gone,
        // unless #2509's one-shot grace answers first.
        Some(Verdict::Fail) => {
            if counter_exhausted(entry.counters.review_rounds, limits.max_review_rounds) {
                // **The grace is asked ONLY at the bound**, which is what makes
                // it a grace rather than a discount. Below the bound the
                // ordinary round is spent and `body_only_grace` is untouched, so
                // a drive that never reaches the bound never consumes it — and a
                // drive that reaches the bound on a code fail never gets it.
                if body_only_grace_applies(entry, &lane.block, &facts.head, digest) {
                    DriveStep::spend(DriveState::FixWait, Counter::BodyOnlyGrace)
                } else {
                    DriveStep::held(HeldReason::ReviewLimit)
                }
            } else {
                DriveStep::spend(DriveState::FixWait, Counter::ReviewRounds)
            }
        }
        // Either no verdict bound to this revision, or a `pass` that no longer
        // stands here — which are the same thing to this state: the lane is
        // outstanding and must be briefed for this revision. Whether it
        // *already* was is what [`lane_open_for`] answers, on the full
        // (head, digest) key.
        Some(Verdict::Pass) | None => {
            match entry.lane(&lane.block) {
                // **The stall clock is keyed on the HEAD, not on the full
                // (head, digest) key** (#2109).
                //
                // It used to hang off the arm below, so it was asked only of a
                // lane that was still open for this exact revision — and a body
                // edit under a silent reviewer moved the digest, dropped through
                // to a re-brief, and re-armed `spawned_ms`. A reviewer that had
                // said nothing for fifty-nine minutes was given another hour by
                // an edit it never read, which is the defeat-your-own-bound shape
                // `decide`'s empty-head guard describes one screen up.
                //
                // #2109 makes that reachable rather than theoretical: the
                // re-brief it produced is now REFUSED while that lane's pane is
                // live, so without this the drive would retry the refusal until
                // `state-stalled` at the `review-wait` bound, and then
                // `drive-stalled` — four hours and twelve on a one-lane gate at
                // stock knobs, on notices that name no lane (#2110 renumbered
                // those two; neither names one).
                //
                // **`at_head` is what makes this a silence test rather than a
                // stopwatch** (#2109 review 1). The first version keyed on
                // `(briefed_head, spawned_ms)` alone, and that pair cannot see
                // whether the reviewer ANSWERED — which is the very distinction
                // [`LaneRecord::at_head`] exists to carry. A lane that recorded
                // `fail` at this head, whose worker then made a BODY-ONLY fix,
                // returns here through arc 8 at the same head with a moved
                // digest: the stale verdict reads as absent, this arm is
                // reached, and `now - spawned_ms` is review time PLUS fix time.
                // Past sixty minutes of that sum the drive parked
                // `held(lane-stalled)` on a reviewer that had never been
                // silent — and stuck, since only `open_lane` writes
                // `spawned_ms`, so a `drive_review` resume re-decided on
                // unchanged facts and re-held.
                //
                // A lane that has answered about THIS revision is therefore
                // exempt, whatever the body has done since; what it is owed is
                // the delta re-brief below, which resets both `at_head` and the
                // clock. Only a lane asked about this head and still silent past
                // its timeout stalls.
                Some(rec)
                    if rec.briefed_head == facts.head
                        && rec.at_head != facts.head
                        && facts.now_ms.saturating_sub(rec.spawned_ms)
                            >= minutes_ms(limits.lane_timeout_minutes) =>
                {
                    DriveStep::held(HeldReason::LaneStalled)
                }
                // **A lane whose pane is GONE is not a lane to wait for**
                // (#2163). `lane_open_for` answers "was this lane asked about
                // this revision", which stays true forever after the pane that
                // was asked has exited — so a killed reviewer left this state
                // waiting on a verdict nothing could produce, with `lane-stalled`
                // an hour away and not one rd-* row in between.
                //
                // Read AFTER the stall arm, and the ordering is the bound. A
                // pane that dies on every spawn would otherwise be re-opened for
                // ever: this arm answers first on every tick, so the stall arm
                // would never be evaluated. Below it, and with the re-open
                // PRESERVING `spawned_ms` (see [`lane_stall_anchor`]), a lane
                // whose panes keep dying still reaches `lane_timeout_minutes`
                // from the ORIGINAL brief and parks `held(lane-stalled)` naming
                // it — which is the true statement about that lane anyway: it
                // has been silent about this head for an hour.
                //
                // What the re-open costs is one pane on a session the lane
                // already has, which `rd_lane_session` resolves from the record,
                // the roster or the merged records; a session that no longer
                // resolves falls to the existing `rd-lane-resume-failed` →
                // fresh-spawn path.
                Some(rec) if lane_open_for(rec, &facts.head, digest) && !lane.pane_dead => {
                    DriveStep::Wait
                }
                // **The cap's starvation is reported before another spawn is
                // proposed** (#2109). The tick stamps
                // `cap_starved_since_ms` on the first lane spawn the
                // live-delegate cap refuses and leaves it alone on the rest, so
                // this reads the duration of one refusal RUN, not the age of the
                // last tick.
                //
                // Proposing `OpenLane` anyway would be harmless — the spawn
                // would be refused again and the entry re-stamped with nothing —
                // and that is exactly what made the measured incident invisible:
                // 37 identical `rd-refused` rows, `lanes: []`, no notice, three
                // hours. §2.2's exits are the only thing an orchestrator reads,
                // so a condition that needs an orchestrator has to become one.
                _ if entry.cap_starved_for(facts.now_ms).is_some_and(|d| d >= CAP_HOLD_MS) => {
                    DriveStep::held(HeldReason::CapFull)
                }
                _ => DriveStep::OpenLane { index: k, verify, body_only },
            }
        }
    }
}

fn decide_fix_wait(entry: &DriveEntry, facts: &DriveFacts, limits: &DriveLimits) -> DriveStep {
    match facts.worker {
        // Nothing to hand back to. Checked first: every other arm here presumes
        // a worker that can be reached.
        //
        // **Except on the first tick after a restart** (#2811 S10). The tick
        // derives `Unresumable` here from the worker PANE having exited, and
        // after a restart every pane has exited — so the signal is true of the
        // process rather than of the session, and holding on it would park every
        // resumable fix-wait drive the moment orrerix came back up. Falling
        // through to the re-hand-back below is what tells the two apart: it
        // resumes the recorded session, and a session that genuinely cannot be
        // resumed makes `rd_handback` fail, which lands on this same hold by the
        // ordinary path one tick later. The probe is the answer, not a guess
        // about it.
        WorkerSignal::Unresumable if !facts.restart_handback => {
            return DriveStep::held(HeldReason::WorkerUnresumable)
        }
        WorkerSignal::Unresumable => {}
        // INVARIANT 3 territory — a blocked worker is the orchestrator's call.
        WorkerSignal::Blocked => return DriveStep::held(HeldReason::WorkerBlocked),
        WorkerSignal::Done | WorkerSignal::Silent => {}
    }
    // Arc 7: the worker pushed. Outranks `report(done)` deliberately — if both
    // happened, the code moved and CI is what has to answer next.
    if !facts.head.is_empty() && entry.head != facts.head {
        return DriveStep::to(DriveState::CiWait);
    }
    // Arc 8: `report(done)` with the head unchanged — a body-only fix.
    if facts.worker == WorkerSignal::Done {
        return DriveStep::to(DriveState::ReviewWait);
    }
    // #2811 S10, BELOW arcs 7 and 8 and ABOVE the stall. Below them because a
    // worker that pushed or reported before the process went down has already
    // answered, and re-briefing it would ask again for work that is done. Above
    // the stall because `fix_handback_ms` predates the restart: most of the gap
    // it measures is downtime in which no worker could have answered, so
    // `held(fix-stalled)` — a claim about a WORKER being silent — would be
    // false. Nothing is charged either way; see [`DriveStep::Rehandback`].
    if facts.restart_handback {
        return DriveStep::Rehandback;
    }
    if facts.now_ms.saturating_sub(entry.fix_handback_ms) >= minutes_ms(limits.fix_timeout_minutes)
    {
        return DriveStep::held(HeldReason::FixStalled);
    }
    DriveStep::Wait
}

fn decide_gate_check(entry: &DriveEntry, facts: &DriveFacts, limits: &DriveLimits) -> DriveStep {
    // §4: `route_reviewers` returning `None` is `held(routing-unaccountable)`
    // from every state that reads it, **`gate-check` included**. This is the
    // one degradation whose absence would be a security defect rather than an
    // inconvenience: the unknown thing is *which reviewers are required*, so
    // guessing "no rule fired" is guessing in favour of merging, and the gate
    // would answer allowed on a reviewer list nobody could compute — §3.1's
    // "a bypass with better telemetry".
    if facts.required_lanes.is_none() {
        return DriveStep::held(HeldReason::RoutingUnaccountable);
    }
    match facts.gate {
        GateOutcome::Unreadable => DriveStep::held(HeldReason::GateUnreadable),
        // #3367 item 1: a satisfied gate whose lanes left only NON-blocking
        // findings goes back to the worker instead of waking the
        // orchestrator — arc 3's `(gate-check, fix-wait)` pair, so the arc
        // table is unchanged, told apart from the conflict hand-back by the
        // counter it spends. Every precondition, the shared bound included,
        // is in [`nonblocking_round_applies`]; anything it cannot establish
        // falls through to arc 9, which is today's behaviour.
        GateOutcome::Satisfied if nonblocking_round_applies(entry, facts, limits) => {
            DriveStep::spend(DriveState::FixWait, Counter::NonblockingRound)
        }
        // Arc 9.
        GateOutcome::Satisfied => DriveStep::to(DriveState::Satisfied),
        // Arc 10, which is deliberately wider than "stale".
        GateOutcome::Unsatisfied => DriveStep::to(DriveState::CiWait),
        // The tick reached `gate-check` without evaluating the gate. Nothing is
        // known, so nothing moves — and in particular this is not `satisfied`.
        GateOutcome::NotEvaluated => DriveStep::Wait,
    }
}

pub(super) fn minutes_ms(minutes: u64) -> u64 {
    minutes.saturating_mul(60_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── §2.4 the decision ───────────────────────────────────────────────────

    fn facts_at(head: &str) -> DriveFacts {
        DriveFacts {
            now_ms: 2_000,
            pr_open: Some(true),
            head: head.to_string(),
            body_digest: Some("d1".to_string()),
            required_lanes: Some(Vec::new()),
            ci: CiObservation::Pending,
            worker: WorkerSignal::Silent,
            gate: GateOutcome::NotEvaluated,
            messaged: false,
            provider_limited: None,
            restart_handback: false,
            restart_push_delivered: false,
        }
    }

    fn lane_fact(block: &str, v: Option<Verdict>, at_head: &str, digest: &str) -> LaneFact {
        LaneFact {
            block: block.to_string(),
            verdict: v.map(|verdict| ReviewVerdict {
                pr: 1758,
                block: block.to_string(),
                agent_id: "rev-1".into(),
                verdict,
                head: at_head.to_string(),
                body_digest: digest.to_string(),
                verified_body: false,
                open_findings: None,
                summary: String::new(),
                ts_ms: 0,
            }),
            // The overwhelmingly common reading, so it is the helper's default
            // and `pane_dead` is set by the one test that is about it (#2163).
            pane_dead: false,
        }
    }

    /// [`lane_fact`] whose `pass` is the driver's body-VERIFICATION delta
    /// (#2168 E2) — the mark `review_verdict` writes for a lane the driver
    /// briefed because only the body had moved.
    fn verified_lane_fact(block: &str, at_head: &str, digest: &str) -> LaneFact {
        let mut l = lane_fact(block, Some(Verdict::Pass), at_head, digest);
        if let Some(v) = l.verdict.as_mut() {
            v.verified_body = true;
        }
        l
    }

    #[test]
    fn the_tick_never_advances_a_parked_or_terminal_drive() {
        // §2.1's `held` row: "nothing; the tick does not advance it". If the
        // tick could move one, `held` would not be a park — and the two tools
        // would not be its only ways out.
        let limits = DriveLimits::default();
        for st in [DriveState::Held, DriveState::Satisfied, DriveState::Cancelled] {
            let e = entry_at(st);
            // Facts that would move any working state, so this is not passing
            // because there was nothing to do.
            let facts = DriveFacts {
                ci: CiObservation::Green,
                gate: GateOutcome::Satisfied,
                worker: WorkerSignal::Blocked,
                messaged: true,
                now_ms: 10_000_000_000,
                ..facts_at("head-a")
            };
            assert_eq!(decide(&e, &facts, &limits), DriveStep::Wait, "{}", st.as_str());
        }
    }

    #[test]
    fn only_a_positive_answer_cancels_a_drive() {
        // §8: cancelling a live drive on a rate limit that clears in minutes is
        // the failure this distinction exists to stop. `None` is "the world
        // does not match", never "probably fine".
        let limits = DriveLimits::default();
        let e = entry_at(DriveState::CiWait);
        let closed = DriveFacts { pr_open: Some(false), ..facts_at("head-a") };
        assert_eq!(decide(&e, &closed, &limits), DriveStep::to(DriveState::Cancelled));
        let unknown = DriveFacts { pr_open: None, ..facts_at("head-a") };
        assert_eq!(decide(&e, &unknown, &limits), DriveStep::Wait);
    }

    #[test]
    fn a_delegates_message_parks_the_drive() {
        // §7: `message_orchestrator` is never intercepted, so the delegate's
        // own line is already in the pane; this hold is the routing fact.
        let limits = DriveLimits::default();
        let e = entry_at(DriveState::ReviewWait);
        let facts = DriveFacts { messaged: true, ..facts_at("head-a") };
        assert_eq!(decide(&e, &facts, &limits), DriveStep::held(HeldReason::Messaged));
    }

    #[test]
    fn the_age_bound_outranks_an_available_advance() {
        // §8's `also: [base-green]` row is the worked example: that drive
        // cycles gate-check -> ci-wait on EVERY wake, so it always has an
        // advance available. An age check that ran after the per-state logic
        // would never be reached and the drive would cycle forever — which is
        // exactly the silent park the bound exists to prevent.
        let limits = DriveLimits::default();
        let e = entry_at(DriveState::GateCheck);
        let cycling = DriveFacts {
            gate: GateOutcome::Unsatisfied,
            ..facts_at("head-a")
        };
        // Young: it takes arc 10, as that row describes.
        assert_eq!(
            decide(&e, &cycling, &limits),
            DriveStep::to(DriveState::CiWait)
        );
        // Aged past the bound: parked, despite that arc still being available.
        let aged = DriveFacts {
            now_ms: e.started_ms + minutes_ms(limits.drive_timeout_minutes),
            ..cycling
        };
        assert_eq!(
            decide(&e, &aged, &limits),
            DriveStep::held(HeldReason::DriveStalled)
        );
    }

    #[test]
    fn ci_wait_reads_its_four_answers_and_waits_on_the_two_unknowns() {
        let limits = DriveLimits::default();
        let e = entry_at(DriveState::CiWait);
        let with = |ci| decide(&e, &DriveFacts { ci, ..facts_at("head-a") }, &limits);
        // Arc 2.
        assert_eq!(with(CiObservation::Green), DriveStep::to(DriveState::ReviewWait));
        // Arc 3, on each of its two counters.
        assert_eq!(
            with(CiObservation::Red),
            DriveStep::spend(DriveState::FixWait, Counter::CiAttempts)
        );
        assert_eq!(
            with(CiObservation::Conflicting),
            DriveStep::spend(DriveState::FixWait, Counter::RebaseAttempts)
        );
        // Neither an answer nor a reason to move: "unknown is never treated as
        // safe", and it is never treated as a fact about the PR either.
        assert_eq!(with(CiObservation::Pending), DriveStep::Wait);
        assert_eq!(with(CiObservation::Unknown), DriveStep::Wait);
    }

    // ── #2168 E1: green at a pushed head is not on its own an arc ───────────

    /// A drive that reached `ci-wait` the way arc 7 does: hand back, then the
    /// worker pushes. Walked through the real arcs rather than assembled, so a
    /// fixture cannot encode a `fix_pushed_ms` the machine would not stamp.
    fn pushed_fix_entry() -> DriveEntry {
        let mut e = entry_at(DriveState::ReviewWait);
        // Arc 5, the hand-back — this is what stamps `fix_handback_ms`.
        e.advance(DriveState::FixWait, None, Some(Counter::ReviewRounds), 10_000).unwrap();
        // Arc 7, the push. `state_since_ms` is now this moment, and it is the
        // anchor the receipts wait is measured from.
        e.advance(DriveState::CiWait, None, None, 100_000).unwrap();
        e
    }

    /// **The defect, and the test that is red without the guard** (#1875,
    /// #2168 E1). A worker fills the PR body's CI section only once the checks
    /// have settled, so a lane briefed on green alone is briefed at a digest
    /// the worker is about to move — and the `pass` it records is stale before
    /// it is written. #1870 is the measured instance: `pass` at digest
    /// `bbff76b8`, 0 findings, the CI section filled, `BODY CHANGED SINCE
    /// PASS`, gate blocked, re-record, with the head never having moved.
    #[test]
    fn green_at_a_pushed_head_waits_for_the_workers_report_before_it_briefs_a_lane() {
        let limits = DriveLimits::default();
        let e = pushed_fix_entry();
        assert!(e.fix_pushed(), "the pre-state: arc 7 is what this test is about");
        let green_and_silent = DriveFacts {
            ci: CiObservation::Green,
            worker: WorkerSignal::Silent,
            ..facts_at("head-b")
        };
        assert_eq!(
            decide(&e, &green_and_silent, &limits),
            DriveStep::Wait,
            "green is the checks settling, not the round ending — the worker has not \
             reported, so the body may still move under whatever lane this briefs"
        );
        // And the arc it is waiting FOR, so this is not "refuse everything".
        let green_and_done = DriveFacts { worker: WorkerSignal::Done, ..green_and_silent };
        assert_eq!(
            decide(&e, &green_and_done, &limits),
            DriveStep::to(DriveState::ReviewWait),
            "arc 2 fires once the revision has stopped moving"
        );
    }

    /// The negative control, and the property the brief calls out by name: a
    /// drive that never handed anything back advances on green as it always
    /// did. `drive_review` is called after the worker has already reported, so
    /// there is no second report to wait for and waiting would park every first
    /// drive on a signal nobody was asked for.
    #[test]
    fn green_on_a_head_this_drive_never_handed_back_still_advances_on_its_own() {
        let limits = DriveLimits::default();
        for e in [entry_at(DriveState::CiWait), gate_check_recycled_entry()] {
            assert!(!e.fix_pushed(), "neither of these reached `ci-wait` by arc 7");
            let facts = DriveFacts {
                ci: CiObservation::Green,
                worker: WorkerSignal::Silent,
                ..facts_at("head-a")
            };
            assert_eq!(
                decide(&e, &facts, &limits),
                DriveStep::to(DriveState::ReviewWait),
                "a drive with no outstanding hand-back has no report to wait for"
            );
        }
    }

    /// A drive that went out to `gate-check` and came back by arc 10 — one of
    /// the three non-arc-7 ways into `ci-wait`, and the one that would still be
    /// carrying the flag if `advance` merely SET it instead of assigning it.
    fn gate_check_recycled_entry() -> DriveEntry {
        let mut e = pushed_fix_entry();
        e.advance(DriveState::ReviewWait, None, None, 200_000).unwrap();
        e.advance(DriveState::GateCheck, None, None, 200_000).unwrap();
        // Arc 10: not satisfied, back to `ci-wait`.
        e.advance(DriveState::CiWait, None, None, 200_000).unwrap();
        e
    }

    /// `fix_pushed_ms` is assigned on every arc, not set on one — pinned across
    /// each way into and out of `ci-wait`, because the failure mode of a set is
    /// invisible: the flag survives, the drive waits for a `report(done)` its
    /// worker was never asked for, and it parks `fix-stalled` an hour later
    /// naming a worker that did nothing wrong.
    #[test]
    fn only_arc_seven_marks_a_head_as_one_the_worker_pushed() {
        // Arc 1: the creation.
        assert!(!DriveEntry::new(1, "s", "o", Counters::default(), 0).fix_pushed());
        // Arc 7 sets it…
        let mut e = pushed_fix_entry();
        assert!(e.fix_pushed(), "arc 7");
        // …arc 2 out of `ci-wait` clears it…
        e.advance(DriveState::ReviewWait, None, None, 110_000).unwrap();
        assert!(!e.fix_pushed(), "arc 2 leaves `ci-wait`, so the push is spent");
        // …and arc 6 back INTO `ci-wait` does not re-set it.
        e.advance(DriveState::CiWait, None, None, 120_000).unwrap();
        assert!(!e.fix_pushed(), "arc 6 is a push this drive did not hand back for");
        // Arc 10, the same question from `gate-check`.
        assert!(!gate_check_recycled_entry().fix_pushed(), "arc 10");
        // Arc 11, a resume out of a park — the drive that was waiting on a
        // report before it was held is not still waiting on one after.
        let mut held = pushed_fix_entry();
        held.advance(DriveState::Held, Some(HeldReason::FixStalled), None, 300_000).unwrap();
        held.advance(DriveState::CiWait, None, None, 400_000).unwrap();
        assert!(!held.fix_pushed(), "arc 11");
    }

    /// The bound, and **which clock it is measured on**. `fix_handback_ms` is
    /// the hand-back, which predates the push: measured from there, a worker
    /// that spent fifty-nine minutes writing the fix would get one minute to
    /// read a green matrix and report. The anchor is `state_since_ms`, which
    /// is the LATEST push in this `ci-wait` stay, which is what `fix_pushed_ms`
    /// carries and `state_since_ms` cannot.
    #[test]
    fn a_silent_worker_on_a_pushed_head_parks_fix_stalled_a_fix_timeout_after_the_push() {
        let limits = DriveLimits::default();
        let e = pushed_fix_entry();
        assert!(
            e.state_since_ms > e.fix_handback_ms,
            "the fixture's whole point: the two anchors are 90s apart, so a test that \
             read the wrong one cannot pass by coincidence"
        );
        let at = |now_ms| {
            decide(
                &e,
                &DriveFacts {
                    now_ms,
                    ci: CiObservation::Green,
                    worker: WorkerSignal::Silent,
                    ..facts_at("head-b")
                },
                &limits,
            )
        };
        let timeout = minutes_ms(limits.fix_timeout_minutes);
        assert_eq!(at(e.state_since_ms + timeout - 1), DriveStep::Wait, "one ms inside");
        assert_eq!(
            at(e.state_since_ms + timeout),
            DriveStep::held(HeldReason::FixStalled),
            "and the bound itself"
        );
        // The discriminator: at a `now` that is past the timeout measured from
        // the HAND-BACK but not from the push, the drive is still waiting.
        assert_eq!(
            at(e.fix_handback_ms + timeout),
            DriveStep::Wait,
            "measured from `fix_handback_ms` this would already have parked"
        );
    }

    /// The other two worker signals, answered by their own names rather than
    /// waited out. Letting either fall through to the timeout would report a
    /// worker that SAID something as one that went silent — and would cost an
    /// hour before saying even that.
    #[test]
    fn the_pushed_head_wait_reads_every_worker_signal_by_the_same_rule_fix_wait_does() {
        let limits = DriveLimits::default();
        let e = pushed_fix_entry();
        for (worker, want) in [
            (WorkerSignal::Blocked, HeldReason::WorkerBlocked),
            (WorkerSignal::Unresumable, HeldReason::WorkerUnresumable),
        ] {
            let facts =
                DriveFacts { ci: CiObservation::Green, worker, ..facts_at("head-b") };
            assert_eq!(
                decide(&e, &facts, &limits),
                DriveStep::held(want),
                "{worker:?} is answered here exactly as `decide_fix_wait` answers it"
            );
            // …and still by its own name once the clock HAS run out, which is
            // the discriminator: read below the timeout instead of above it,
            // both of these would report `fix-stalled` — a claim that the
            // worker went silent, about a worker that said something.
            let expired = DriveFacts {
                now_ms: e.state_since_ms + minutes_ms(limits.fix_timeout_minutes) + 1,
                ..facts
            };
            assert_eq!(
                decide(&e, &expired, &limits),
                DriveStep::held(want),
                "{worker:?} is not reported as `fix-stalled` once the wait expires"
            );
        }
    }

    /// The guard is on the GREEN arm alone. A red or a conflicting matrix at a
    /// pushed head still hands the worker back through arc 3, where
    /// `decide_fix_wait`'s ladder answers, and the two unknowns wait as they
    /// always did. Without this, the receipts wait would swallow the arc that
    /// tells the worker its fix failed.
    #[test]
    fn a_pushed_head_that_is_not_green_takes_the_arcs_it_always_did() {
        let limits = DriveLimits::default();
        let e = pushed_fix_entry();
        let with = |ci| {
            decide(
                &e,
                &DriveFacts { ci, worker: WorkerSignal::Silent, ..facts_at("head-b") },
                &limits,
            )
        };
        assert_eq!(
            with(CiObservation::Red),
            DriveStep::spend(DriveState::FixWait, Counter::CiAttempts)
        );
        assert_eq!(
            with(CiObservation::Conflicting),
            DriveStep::spend(DriveState::FixWait, Counter::RebaseAttempts)
        );
        assert_eq!(with(CiObservation::Pending), DriveStep::Wait);
        assert_eq!(with(CiObservation::Unknown), DriveStep::Wait);
    }

    /// **`state-stalled` may not preempt `fix-stalled`, at any configuration**
    /// — the property `HeldReason::StateStalled` states, and the one a `max`
    /// would have broken. `decide` reads the state bound ABOVE the state's own
    /// logic, so at `fix_timeout_minutes` above `CI_WAIT_BOUND_MS` a bound
    /// equal to the knob would answer first on the very tick the receipts wait
    /// expires, and the hold naming the pane to read would never fire at all.
    ///
    /// The two knobs are the two sides of the constant: 60 (the stock value,
    /// under it) and 240 (over it, which is where a `max` and an add differ —
    /// and the CEILING, since `parse_workflow` runs this knob through
    /// `clamp_expires_minutes`, so no workflow file can ask for more). At that
    /// ceiling the sum is 330 minutes, comfortably inside the twelve-hour
    /// backstop, which is why this arm needs no counterpart to `review-wait`'s
    /// overtake residual — that one is unbounded in the LANE COUNT, and this
    /// one has a single clamped term.
    #[test]
    fn the_ci_wait_bound_funds_the_receipts_wait_instead_of_preempting_it() {
        for minutes in [60u64, 240] {
            let limits = DriveLimits { fix_timeout_minutes: minutes, ..DriveLimits::default() };
            let bound = state_bound_ms(DriveState::CiWait, &limits, 1).unwrap();
            assert_eq!(
                bound,
                CI_WAIT_BOUND_MS + minutes_ms(minutes),
                "the constant is the SLACK over the receipts wait, not a rival to it"
            );
            let e = pushed_fix_entry();
            let facts = DriveFacts {
                now_ms: e.state_since_ms + minutes_ms(minutes),
                ci: CiObservation::Green,
                worker: WorkerSignal::Silent,
                ..facts_at("head-b")
            };
            assert_eq!(
                decide(&e, &facts, &limits),
                DriveStep::held(HeldReason::FixStalled),
                "at fix_timeout_minutes={minutes} the wait-specific hold is what fires; \
                 under `max(constant, knob)` this is `state-stalled` at 240"
            );
            assert!(
                bound < minutes_ms(limits.drive_timeout_minutes),
                "…and the sum stays inside the backstop at the ceiling the workflow \
                 parser clamps to, so this arm owes no overtake residual"
            );
        }
    }

    /// **A follow-up push mid-wait gets a whole window, not the remainder of
    /// one** (rev-final round 2, premortem 2). `transition` refuses a `ci-wait`
    /// -> `ci-wait` self-arc, so nothing re-stamps `state_since_ms` and a
    /// `bool` bounded from it would run the wait from the FIRST push: a commit
    /// landing at minute 55 of a 60-minute knob would leave five minutes to run
    /// a fresh matrix and report.
    ///
    /// The three assertions are the defect, the fix and the bound it must not
    /// remove — a re-stamp that also pushed `state-stalled` out would be an
    /// unbounded suppression driven by a signal the drive does not control.
    #[test]
    fn a_second_push_inside_one_ci_wait_stay_re_anchors_the_receipts_wait() {
        let limits = DriveLimits::default();
        let timeout = minutes_ms(limits.fix_timeout_minutes);
        let mut e = pushed_fix_entry();
        let first = e.fix_pushed_ms.expect("arc 7 stamps the anchor");
        let silent = |e: &DriveEntry, now_ms| {
            decide(
                e,
                &DriveFacts {
                    now_ms,
                    ci: CiObservation::Green,
                    worker: WorkerSignal::Silent,
                    ..facts_at("head-c")
                },
                &limits,
            )
        };
        // Without the re-stamp this is the park — the defect, stated as the
        // pre-state so the fix below cannot pass vacuously.
        assert_eq!(
            silent(&e, first + timeout),
            DriveStep::held(HeldReason::FixStalled),
            "the pre-state: measured from the first push, the window is spent"
        );

        // The worker pushes again at minute 55, and the tick records it.
        let second = first + timeout - minutes_ms(5);
        e.note_fix_push(second);
        assert_eq!(e.state_since_ms, first, "the STATE clock is deliberately untouched");
        assert_eq!(
            silent(&e, first + timeout),
            DriveStep::Wait,
            "the same instant is now five minutes into a fresh window"
        );
        assert_eq!(
            silent(&e, second + timeout),
            DriveStep::held(HeldReason::FixStalled),
            "…and the fresh window is a whole one, not an unbounded reprieve"
        );

        // The bound the re-stamp may not remove: `state-stalled` measures from
        // `state_since_ms`, so a worker that pushed for ever still parks.
        let forever = first + state_bound_ms(DriveState::CiWait, &limits, 0).unwrap();
        e.note_fix_push(forever);
        assert_eq!(
            silent(&e, forever),
            DriveStep::held(HeldReason::StateStalled),
            "a drive that keeps pushing is still bounded by the state it is sitting in"
        );

        // And it re-stamps only what exists: a head move in a state that never
        // took arc 7 must not manufacture a wait.
        let mut fresh = entry_at(DriveState::CiWait);
        fresh.note_fix_push(9_999);
        assert!(!fresh.fix_pushed(), "note_fix_push is a re-stamp, never an entry point");
    }

    /// **A driven worker's `report(progress)` is answered wherever a hand-back
    /// is outstanding, which E1 makes two states** (rev-final round 2, finding
    /// 1). `kickback_owed` was scoped to `fix-wait` on the argument that "in
    /// any other state there is no hand-back to be waiting on and nothing the
    /// worker was asked for" — which E1 falsified for `ci-wait` on an arc-7
    /// head, where the worker has been handed a round and is being waited on
    /// for the very `report(done)` that line asks it to send.
    ///
    /// Left unswept, #1959 reappears one state over and worse: §7 consumes the
    /// report (its interception is keyed on the agent, not the state), the
    /// orchestrator's pane gets nothing, the worker's pane gets nothing, and a
    /// fix timeout later the hold says the driver heard nothing from a worker
    /// that spoke.
    ///
    /// The `review-wait` row is the control that keeps this from becoming
    /// "answer everywhere": that state has no outstanding hand-back.
    #[test]
    fn a_progress_report_is_owed_an_answer_in_both_states_that_are_waiting_on_the_worker() {
        // `fix-wait`, unchanged — and the pre-state for the arc-7 case below.
        let mut e = entry_at(DriveState::ReviewWait);
        e.advance(DriveState::FixWait, None, Some(Counter::ReviewRounds), 10_000).unwrap();
        assert!(e.kickback_owed(), "#1959's own state, unchanged");

        // Arc 7. The hand-back is still outstanding: the worker was asked to
        // push AND report, and it has done half of that.
        e.advance(DriveState::CiWait, None, None, 100_000).unwrap();
        assert!(e.fix_pushed());
        assert!(
            e.kickback_owed(),
            "the worker is still being waited on, for the report this line asks it to send"
        );

        // The budget is one per HAND-BACK and not one per state: `fix_handback_ms`
        // is not re-stamped by arc 7, so a worker answered before it pushed is
        // not answered again after.
        let mut answered = e.clone();
        answered.record_kickback(20_000);
        assert!(!answered.kickback_owed(), "same round, same answer, once");

        // The control: `ci-wait` on a head this drive never handed back for, and
        // `review-wait`, owe nobody anything.
        let first_drive = entry_at(DriveState::CiWait);
        assert!(!first_drive.fix_pushed());
        assert!(!first_drive.kickback_owed(), "no hand-back is outstanding here");
        let mut reviewing = e.clone();
        reviewing.advance(DriveState::ReviewWait, None, None, 200_000).unwrap();
        assert!(!reviewing.kickback_owed(), "and the lane's state owes the worker nothing");
    }

    /// **The two residuals `decide_fix_receipts` discloses are recoverable by
    /// the remedy their own notice prints**, which §2.2 requires of every hold
    /// whose cause is a wait — and which a hold on a signal that will never
    /// arrive would otherwise fail: a resume that only restarted the clocks
    /// would re-hold an hour later, for ever.
    ///
    /// This is the counterfactual for the disclosure, performed rather than
    /// argued (CLAUDE.md: a documented escape hatch is only pinned by a test
    /// that performs the edit). Arc 11 re-enters `ci-wait` from `held`, so
    /// `advance` assigns `fix_pushed_ms` None — it is the ASSIGNMENT, not a
    /// clock, that makes the resume work, and a `set` would leave the drive
    /// waiting on the same absent report.
    ///
    /// The pre-state is the hold itself, so the test cannot pass by resuming a
    /// drive that was never stuck.
    #[test]
    fn a_resume_out_of_fix_stalled_briefs_on_the_next_green() {
        let limits = DriveLimits::default();
        let mut e = pushed_fix_entry();
        let stalled = DriveFacts {
            now_ms: e.state_since_ms + minutes_ms(limits.fix_timeout_minutes),
            ci: CiObservation::Green,
            worker: WorkerSignal::Silent,
            ..facts_at("head-b")
        };
        assert_eq!(
            decide(&e, &stalled, &limits),
            DriveStep::held(HeldReason::FixStalled),
            "the pre-state: a drive really parked on the wait this test is about"
        );
        e.take(&DriveStep::held(HeldReason::FixStalled), stalled.now_ms).unwrap();

        // Arc 11: `drive_review` resumes it, hours later — a human takes their
        // time over a hold, so the resume must not depend on any clock.
        let resumed_at = stalled.now_ms + minutes_ms(300);
        e.advance(DriveState::CiWait, None, None, resumed_at).unwrap();
        assert!(!e.fix_pushed(), "the resume leaves no outstanding report to wait for");
        assert_eq!(
            decide(
                &e,
                &DriveFacts { now_ms: resumed_at + 1_000, ..stalled },
                &limits
            ),
            DriveStep::to(DriveState::ReviewWait),
            "the very next green briefs, with nothing further asked of a worker that \
             may have said its piece already"
        );
    }

    #[test]
    fn ci_attempts_park_at_their_bound() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::CiWait);
        e.counters.ci_attempts = limits.max_ci_attempts;
        let facts = DriveFacts { ci: CiObservation::Red, ..facts_at("head-a") };
        assert_eq!(decide(&e, &facts, &limits), DriveStep::held(HeldReason::CiLimit));
    }

    #[test]
    fn an_unaccountable_route_parks_from_every_state_that_reads_one() {
        // §4: never a guess. The unknown thing is *which reviewers are
        // required*, so guessing "no rule fired" is guessing in favour of
        // merging — and at `gate-check` that would be a false GATE SATISFIED
        // notice, which §3.1 names "a bypass with better telemetry".
        let limits = DriveLimits::default();
        for st in [DriveState::ReviewWait, DriveState::GateCheck] {
            let mut e = entry_at(st);
            e.head = "head-a".into();
            let facts = DriveFacts {
                required_lanes: None,
                // The gate would say SATISFIED, which is precisely what must
                // not win here.
                gate: GateOutcome::Satisfied,
                ..facts_at("head-a")
            };
            assert_eq!(
                decide(&e, &facts, &limits),
                DriveStep::held(HeldReason::RoutingUnaccountable),
                "{}",
                st.as_str()
            );
        }
    }

    #[test]
    fn review_wait_walks_its_lanes_in_the_gates_order() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        let two_lanes = |first: Option<Verdict>, second: Option<Verdict>| DriveFacts {
            required_lanes: Some(vec![
                lane_fact("rev-std", first, "head-a", "d1"),
                lane_fact("rev-final", second, "head-a", "d1"),
            ]),
            ..facts_at("head-a")
        };
        // Lane 1 not yet briefed: open it.
        assert_eq!(
            decide(&e, &two_lanes(None, None), &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: false }
        );
        // Lane 1 passed at this (head, digest): move to lane 2 — and note this
        // is NOT a transition, which is why the table has no review-wait arm.
        assert_eq!(
            decide(&e, &two_lanes(Some(Verdict::Pass), None), &limits),
            DriveStep::OpenLane { index: 1, verify: false, body_only: false }
        );
        // Both passed: arc 4.
        assert_eq!(
            decide(
                &e,
                &two_lanes(Some(Verdict::Pass), Some(Verdict::Pass)),
                &limits
            ),
            DriveStep::to(DriveState::GateCheck)
        );
        // Arc 5, and an escalate that §3 refuses to decide.
        assert_eq!(
            decide(&e, &two_lanes(Some(Verdict::Fail), None), &limits),
            DriveStep::spend(DriveState::FixWait, Counter::ReviewRounds)
        );
        assert_eq!(
            decide(&e, &two_lanes(Some(Verdict::Escalate), None), &limits),
            DriveStep::held(HeldReason::Escalate)
        );
    }

    /// **The residual the answered-lane exemption leaves — NARROWED by #2110,
    /// not closed** (#2109 review 2, premortem 1).
    ///
    /// One composition escapes every per-lane bound: a reviewer answers at this
    /// head, the body then moves, and that reviewer's pane goes BUSY on
    /// something else — a human re-tasking it, another drive taking it over. The
    /// re-brief the moved digest calls for cannot be delivered (the reuse arm
    /// needs an idle pane) and must not be spawned beside it (#2109's duplicate
    /// refusal, which #2162 narrowed to exactly this busy case), while
    /// `lane-stalled` is exempt because the lane did answer. So the drive
    /// retries and audits two rows a tick.
    ///
    /// **What #2110 changed is the exit, and only the exit.** #2109 disclosed
    /// this gap and said closing it needs a per-lane refusal clock, "which
    /// belongs with #2110's age work". #2110 did not build that clock: there is
    /// still nothing per-lane here, and the hold that ends this still names no
    /// lane. What it built is a per-STATE bound, and `review-wait` is a state,
    /// so the drive now leaves at the `review-wait` state bound as
    /// `held(state-stalled)` instead
    /// of at twelve as `held(drive-stalled)` — a fraction of the wait, and a
    /// notice that at least says which wait. The honest description of the
    /// residual is therefore *bounded per state, still not per lane*, and §8's
    /// row says exactly that.
    ///
    /// A disclosure is a claim like any other: without this test the suite pins
    /// only the arms that work, and that §8 row could go false with nothing red
    /// to say so. The three halves are the gap itself (past the LANE timeout,
    /// still proposing the re-brief), the non-vacuity control one tick short of
    /// the bound that now ends it, and the bound.
    ///
    /// **The age backstop is deliberately not asserted here any more**, and its
    /// absence is the finding rather than an omission: the narrower bound fires
    /// first, so from `review-wait` the twelve-hour clock is now unreachable for
    /// this composition. Asserting it would be asserting a path the code no
    /// longer takes.
    #[test]
    fn an_answered_lane_whose_re_brief_is_refused_is_bounded_by_the_review_wait_state_bound() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        e.open_lane("rev-std", "sess", "rev-4", "head-a", Some("d1"), 1_000, false, false);
        assert!(e.record_verdict_seen("rev-std", Verdict::Fail, "head-a"));
        let facts = |now: u64| DriveFacts {
            now_ms: now,
            body_digest: Some("d2".to_string()),
            required_lanes: Some(vec![lane_fact("rev-std", Some(Verdict::Fail), "head-a", "d1")]),
            ..facts_at("head-a")
        };

        // The gap: twice the LANE timeout and still inside the state bound, so
        // this keeps proposing the re-brief the tick will refuse. `lane-stalled`
        // never fires here, by design, and nothing per-lane replaces it.
        let past_lane = 1_000 + minutes_ms(limits.lane_timeout_minutes) * 2;
        assert_eq!(
            decide(&e, &facts(past_lane), &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: true },
            "the exemption really does leave this composition unbounded PER LANE — if this \
             ever becomes a lane-named hold, the §8 row disclosing the gap is what needs \
             rewriting"
        );

        // The bound that now limits it, and its own non-vacuity control one tick
        // short. Asked of `state_bound_ms` rather than spelled here, because
        // this test is about WHICH bound ends the composition; the bound's own
        // value is pinned against literals by
        // `the_review_wait_floor_exceeds_the_sequential_gate_it_covers`, so the
        // number is not built from the code under test without a witness.
        let bound = state_bound_ms(DriveState::ReviewWait, &limits, 1).unwrap();
        assert!(
            bound > minutes_ms(limits.lane_timeout_minutes),
            "the lane timeout must not be what fires, or the assertions below are \
             about the wrong bound"
        );
        assert!(
            bound < minutes_ms(limits.drive_timeout_minutes),
            "…and neither must the backstop, which outranks it in `decide`"
        );
        assert_eq!(
            decide(&e, &facts(1_000 + bound - 1), &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: true },
            "…still inside the state bound, so the hold below is that bound and not a \
             coincidence"
        );
        assert_eq!(
            decide(&e, &facts(1_000 + bound), &limits),
            DriveStep::held(HeldReason::StateStalled),
            "…and time in `review-wait` is what ends it now, where before #2110 nothing \
             but the twelve-hour age could"
        );
    }

    /// **#2153: a re-drive carries the CONVERSATION and nothing that describes a
    /// revision.**
    ///
    /// Every field is asserted, and that is deliberate rather than exhaustive
    /// for its own sake: this record is what the next drive's first tick decides
    /// from, and each field it must NOT carry produces a different wrong answer.
    /// A carried `briefed_head`/`briefed_digest` makes `lane_open_for` true, so
    /// the first tick waits on a brief nobody sent; a carried `last_verdict` and
    /// `at_head` make `lane_has_answered` true, so the delta template describes
    /// a round that is over and the duplicate guard reads the previous drive's
    /// pane as holding this one; a carried `spawned_ms` starts `lane-stalled`
    /// counting from a brief in another drive's lifetime; and a carried `agent`
    /// makes a pane the new drive never spoke to its CURRENT one.
    #[test]
    fn a_reseeded_lane_keeps_its_session_and_its_owned_panes_and_no_claim_about_a_revision() {
        let mut e = entry_at(DriveState::ReviewWait);
        e.open_lane("rev-std", "sess-1", "rev-4", "head-a", Some("d1"), 1_000, false, false);
        e.open_lane("rev-std", "sess-1", "rev-9", "head-a", Some("d1"), 2_000, false, false);
        assert!(e.record_verdict_seen("rev-std", Verdict::Fail, "head-a"));
        let before = e.lane("rev-std").expect("the fixture's own premise").clone();
        assert_eq!(before.prior_agents, vec!["rev-4".to_string()], "the fixture: one supersede");

        let after = before.reseeded("sess-resolved");
        assert_eq!(after.block, "rev-std", "the lane is the same lane");
        assert_eq!(
            after.session, "sess-resolved",
            "and it carries the session the CALLER resolved, not the one on the record — a \
             lane on a CLI that mints its session after boot has `\"\"` here for its whole \
             life, and seeding off this field would drop exactly those lanes (#2109)"
        );
        assert_eq!(
            after.agent, "",
            "the previous drive's pane is not this drive's CURRENT one — leaving it here \
             would have the duplicate guard read it as holding this round and `pane_dead` \
             read its death as this drive's"
        );
        assert_eq!(
            after.prior_agents,
            vec!["rev-4".to_string(), "rev-9".to_string()],
            "…but both panes are still OWNED, oldest first, so a reviewer finishing the \
             previous round is intercepted rather than reporting as if undriven (§7)"
        );
        assert_eq!(after.last_verdict, None, "the previous drive's answer is not this one's");
        assert_eq!(after.at_head, "", "…and nothing may claim this lane has answered here");
        assert_eq!(after.briefed_head, "", "…nor that it has been asked");
        assert_eq!(after.briefed_digest, "");
        assert_eq!(after.spawned_ms, 0, "a lane that has not been briefed has not been silent");
    }

    /// **#2163: a lane whose pane is GONE is re-opened, not waited for — and
    /// the stall arm still outranks that.**
    ///
    /// `lane_open_for` answers "was this lane ASKED about this revision", which
    /// stays true for ever after the pane that was asked has exited. So a killed
    /// reviewer left `review-wait` waiting on a verdict nothing could produce
    /// until `lane-stalled` an hour later — measured on PR #2140 as 25+ minutes
    /// with no rd-* row at all, and reached on the driver's own advice.
    ///
    /// **The live arm is the control, and it is not decoration**: without it an
    /// implementation that re-opened on every tick regardless of the pane would
    /// pass the dead arm and destroy the wait this state is made of.
    ///
    /// **The ordering arm is the BOUND.** A pane that dies on every spawn must
    /// not be replaced for ever; the stall arm is read first, so a lane whose
    /// panes keep dying reaches `lane_timeout_minutes` — from the ORIGINAL
    /// brief, which is what [`lane_stall_anchor`] preserves — and parks
    /// `held(lane-stalled)` naming itself. Asserting it here is what makes that
    /// bound a property rather than a hope.
    #[test]
    fn a_lane_whose_pane_died_is_re_opened_and_a_live_one_is_still_waited_for() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        e.open_lane("rev-std", "sess", "rev-4", "head-a", Some("d1"), 1_000, false, false);
        let facts = |dead: bool, now: u64| DriveFacts {
            now_ms: now,
            required_lanes: Some(vec![LaneFact {
                pane_dead: dead,
                ..lane_fact("rev-std", None, "", "")
            }]),
            ..facts_at("head-a")
        };

        assert_eq!(
            decide(&e, &facts(false, 2_000), &limits),
            DriveStep::Wait,
            "the control: a lane open at this revision with a LIVE pane is one to wait for, \
             and re-opening it would be a second reviewer per tick"
        );
        assert_eq!(
            decide(&e, &facts(true, 2_000), &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: false },
            "a lane whose recorded pane is gone is one to re-open — `lane_open_for` says it \
             was ASKED, which a dead pane cannot un-say and cannot answer"
        );

        // The bound on a pane that keeps dying: past the lane timeout the stall
        // arm answers first, and it is reached because the re-open preserves the
        // anchor rather than re-arming it.
        let stalled = 1_000 + minutes_ms(limits.lane_timeout_minutes);
        assert_eq!(
            decide(&e, &facts(true, stalled - 1), &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: false },
            "…one tick short, so the hold below is the timeout and not a coincidence"
        );
        assert_eq!(
            decide(&e, &facts(true, stalled), &limits),
            DriveStep::held(HeldReason::LaneStalled),
            "…and the silence bound still outranks the re-open, which is what stops a pane \
             that dies on every spawn from being replaced for ever"
        );
    }

    /// **#2163: replacing a dead pane keeps the silence clock where it was.**
    ///
    /// [`lane_stall_anchor`] is what makes the bound in the test above
    /// REACHABLE: `open_lane` writes whatever anchor it is given, so a re-open
    /// that passed `now` would restart `lane-stalled` on every death and the
    /// loop would run until the drive's own age bound.
    ///
    /// Six rows, each killing an implementation the others let through. Always
    /// returning `now_ms` fails row 1; always returning the record's value fails
    /// rows 2, 3 and 5; ignoring the head fails row 2.
    ///
    /// **Rows 3 and 4 are #2169 review 2's N1, and the first version of this
    /// table did not have them** — that is the finding, not a footnote. Four
    /// rows read as discriminating while leaving the DIGEST half of the
    /// revision key untested, because the function took no digest at all: a
    /// head-only implementation passed every one of them. Row 3 is the case
    /// that exposed (a lane whose pane died, then a body-only fix moving the
    /// digest, inheriting an anchor a reviewer that has read nothing did not
    /// earn); row 4 is its own control, so "re-arm whenever the digest does not
    /// match" — which would re-arm on an unreadable body too — cannot pass row 3
    /// by being wrong in the other direction.
    ///
    /// A test that cannot express an axis is a test that does not pin it, and
    /// the missing axis was in the SIGNATURE rather than in the rows.
    #[test]
    fn a_dead_panes_replacement_inherits_the_stall_anchor_and_a_new_round_does_not() {
        let mut e = entry_at(DriveState::ReviewWait);
        e.open_lane("rev-std", "sess", "rev-4", "head-a", Some("d1"), 1_000, false, false);
        let rec = || e.lane("rev-std");

        let d1 = Some("d1");
        let observed = vec![
            ("dead pane, same revision", lane_stall_anchor(rec(), "head-a", d1, true, 9_000)),
            ("dead pane, new head", lane_stall_anchor(rec(), "head-b", d1, true, 9_000)),
            // The row #2169 review 2 (N1) showed was unpinned: same head, moved
            // DIGEST. A head-only implementation answers 1_000 here and passes
            // every other row in this table unchanged.
            ("dead pane, moved digest", lane_stall_anchor(rec(), "head-a", Some("d2"), true, 9_000)),
            // …and its own control: an UNKNOWN live digest is "we could not
            // check", not drift, so it still inherits. Without this row an
            // implementation that re-armed on any non-matching digest — `None`
            // included — passes the row above.
            ("dead pane, digest unknown", lane_stall_anchor(rec(), "head-a", None, true, 9_000)),
            ("live pane, same revision", lane_stall_anchor(rec(), "head-a", d1, false, 9_000)),
            ("no record at all", lane_stall_anchor(None, "head-a", d1, true, 9_000)),
        ];
        let expected = vec![
            // The replacement inherits the silence this lane has already spent.
            ("dead pane, same revision", 1_000),
            // A new revision is a new round, owed the full window — and the
            // revision is the FULL key, so either half moving is a new one.
            ("dead pane, new head", 9_000),
            ("dead pane, moved digest", 9_000),
            ("dead pane, digest unknown", 1_000),
            // Not a replacement — an ordinary re-brief re-arms, as it always has.
            ("live pane, same revision", 9_000),
            ("no record at all", 9_000),
        ];
        assert_eq!(
            observed, expected,
            "each row is (fixture, the `spawned_ms` the next brief must carry). A `9_000` on \
             row 1 hands a lane whose pane keeps dying a fresh hour on every death, so \
             `lane-stalled` never fires and nothing per-lane bounds the loop. A `1_000` on \
             the `new head`, `moved digest` or `live pane` rows stalls a lane that has not \
             been silent at all — and on `moved digest` it does so to a reviewer that has \
             read nothing, while the same body-only fix under a live pane gets a full window."
        );
    }

    /// **#2109 review 1, finding 1.** The stall clock is a SILENCE test, and a
    /// reviewer that answered is not silent however long the round has run.
    ///
    /// This is the arc-8 return the first version of that arm could not see. The
    /// lane records `fail` at `head-a`; the worker's fix is BODY-ONLY, so
    /// `report(done)` at an unchanged head returns the drive straight to
    /// `review-wait` with the digest moved; the stale verdict then reads as
    /// absent and this arm is reached with `now - spawned_ms` equal to review
    /// time PLUS fix time. Keyed on `(briefed_head, spawned_ms)` alone that sum
    /// crosses `lane_timeout_minutes` and parks the drive `lane-stalled` on a
    /// reviewer that answered promptly — and parks it STUCK, because only
    /// `open_lane` writes `spawned_ms`, so a resume re-decides on unchanged
    /// facts and re-holds. Its only exits were a head change, a cancel, or the
    /// age backstop.
    ///
    /// **The two halves differ in exactly one field**, which is what makes this
    /// a pin on `at_head` rather than on the timeout: same lane, same clock,
    /// same moved digest, same stale-verdict fact handed to `decide` — and only
    /// whether `record_verdict_seen` ever ran. Without the second half an
    /// implementation that simply deleted the stall arm would pass.
    #[test]
    fn a_lane_that_answered_at_this_head_is_not_stalled_by_the_fix_round_that_followed() {
        let limits = DriveLimits::default();
        let past = 1_000 + minutes_ms(limits.lane_timeout_minutes);
        // The gate's own fact for the lane: the `fail` it recorded, bound to the
        // head it reviewed and to the digest that has since moved. `decide`
        // reads this as ABSENT (`lane_verdict_is_current`), which is what puts
        // the lane back on the arm under test.
        let facts = DriveFacts {
            now_ms: past,
            body_digest: Some("d2".to_string()),
            required_lanes: Some(vec![lane_fact("rev-std", Some(Verdict::Fail), "head-a", "d1")]),
            ..facts_at("head-a")
        };

        let answered = {
            let mut e = entry_at(DriveState::ReviewWait);
            e.head = "head-a".into();
            e.open_lane("rev-std", "sess", "rev-4", "head-a", Some("d1"), 1_000, false, false);
            assert!(
                e.record_verdict_seen("rev-std", Verdict::Fail, "head-a"),
                "the fixture must actually record the answer, or this pins nothing"
            );
            e
        };
        assert_eq!(
            decide(&answered, &facts, &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: true },
            "a reviewer that ANSWERED at this head is not silent — arc 8 owes it the delta \
             re-brief, not a hold naming it as the thing that went quiet"
        );

        // The control: the identical entry that never answered. One field apart,
        // and it must still stall.
        let silent = {
            let mut e = entry_at(DriveState::ReviewWait);
            e.head = "head-a".into();
            e.open_lane("rev-std", "sess", "rev-4", "head-a", Some("d1"), 1_000, false, false);
            e
        };
        assert_eq!(
            decide(&silent, &facts, &limits),
            DriveStep::held(HeldReason::LaneStalled),
            "…while the lane that was asked and said nothing still stalls, which is the \
             whole point of keying the clock on the head"
        );
    }

    /// **#2109.** A lane's stall clock is keyed on the HEAD it was asked about,
    /// so a body edit under a SILENT reviewer cannot hand it another hour.
    ///
    /// The silence half of that key is pinned by the sibling above; every
    /// fixture here builds its record through `open_lane` alone, so `at_head`
    /// is empty and the lane has answered nothing.
    ///
    /// The two halves discriminate. The first is the pre-#2109 behaviour and is
    /// the control: a lane still open for this exact revision, past its timeout,
    /// has always been `lane-stalled`. The second is the one that moved — same
    /// lane, same silence, same clock, and only the DIGEST different — and under
    /// the old keying it read as a re-brief, which re-armed `spawned_ms` and
    /// bought the reviewer a fresh timeout it had done nothing to earn.
    ///
    /// The third is the negative control, and without it an implementation that
    /// simply held `lane-stalled` for every lane record would pass both halves.
    #[test]
    fn a_lanes_stall_clock_survives_a_body_edit_it_never_read() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        e.open_lane("rev-std", "sess", "rev-4", "head-a", Some("d1"), 1_000, false, false);
        let stale = 1_000 + minutes_ms(limits.lane_timeout_minutes);
        let facts = |now: u64, digest: &str| DriveFacts {
            now_ms: now,
            body_digest: Some(digest.to_string()),
            required_lanes: Some(vec![lane_fact("rev-std", None, "", "")]),
            ..facts_at("head-a")
        };

        assert_eq!(
            decide(&e, &facts(stale, "d1"), &limits),
            DriveStep::held(HeldReason::LaneStalled),
            "the control: a lane open at this revision and past its timeout has always stalled"
        );
        assert_eq!(
            decide(&e, &facts(stale, "d2"), &limits),
            DriveStep::held(HeldReason::LaneStalled),
            "and a body edit the silent reviewer never read must not re-arm its clock"
        );
        assert_eq!(
            decide(&e, &facts(stale - 1, "d2"), &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: false },
            "the negative control: inside the timeout a moved digest is still a re-brief"
        );
    }

    /// **#2109's third ask at the decision layer.** A drive whose lane the
    /// live-delegate cap keeps refusing must become one of §2.2's exits, and it
    /// must not become one on the first refusal.
    ///
    /// The three points are asserted together because each alone passes under an
    /// implementation that is wrong at another: an arm that never holds passes
    /// the first two, and an arm that holds on the stamp alone (no duration)
    /// passes the first and third.
    #[test]
    fn a_cap_that_keeps_refusing_a_lane_parks_the_drive_only_once_the_window_is_spent() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        let facts = |now: u64| DriveFacts {
            now_ms: now,
            required_lanes: Some(vec![lane_fact("rev-std", None, "", "")]),
            ..facts_at("head-a")
        };

        // The control: an un-starved drive proposes the spawn, so every
        // assertion below is about the starvation and not about a lane that
        // could never have opened.
        assert_eq!(decide(&e, &facts(10_000), &limits), DriveStep::OpenLane { index: 0, verify: false, body_only: false });

        assert!(e.note_cap_starvation(10_000), "the first refusal of a run stamps the clock");
        // One tick short of the window: still trying, still no orchestrator turn
        // spent on a condition that clears itself.
        assert_eq!(
            decide(&e, &facts(10_000 + CAP_HOLD_MS - 1), &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: false },
            "a cap that has not refused for the whole window is a back-off, not a hold"
        );
        assert_eq!(
            decide(&e, &facts(10_000 + CAP_HOLD_MS), &limits),
            DriveStep::held(HeldReason::CapFull),
            "a starvation that outlasts the window is one of §2.2's exits, not silence"
        );
    }

    /// The clock measures a RUN, and every way out of one clears it.
    ///
    /// The re-stamp half is the one with teeth: a tick that re-stamped on each
    /// refusal would make [`CAP_HOLD_MS`] unreachable, and the hold above would
    /// then be dead code that no test in this file could tell apart from a hold
    /// that fires — `decide` reads a stamp it is handed either way.
    #[test]
    fn the_starvation_clock_is_stamped_once_per_run_and_no_arc_carries_one_across() {
        let mut e = entry_at(DriveState::ReviewWait);
        assert_eq!(e.cap_starved_for(10_000), None, "a fresh entry is not starved");

        assert!(e.note_cap_starvation(10_000));
        assert!(!e.note_cap_starvation(20_000), "a second refusal in one run is not a new run");
        assert_eq!(
            e.cap_starved_for(20_000),
            Some(10_000),
            "the duration is measured from the FIRST refusal, not the newest tick"
        );

        assert!(e.clear_cap_starvation(20_000), "a lane that opens ends the run");
        assert_eq!(e.cap_starved_for(20_000), None);
        assert!(
            !e.clear_cap_starvation(20_000),
            "and clearing an unstarved entry changes nothing"
        );
        // #2110: what the run COST survives the clear, because that is the
        // quantity both age bounds subtract. A `clear` that only forgot the
        // anchor would leave the ten seconds charged to the drive that was
        // starved — which is the whole defect, one code path over from where
        // it was reported.
        assert_eq!(
            e.starved_ms(20_000),
            10_000,
            "ending a starvation run must bank what it cost, not discard it"
        );

        // An arc clears it too, including the arc into the hold it produces: a
        // resume must start the window over rather than re-hold on the next tick
        // from a stamp the previous starvation left behind.
        assert!(e.note_cap_starvation(30_000));
        e.advance(DriveState::Held, Some(HeldReason::CapFull), None, 30_000).unwrap();
        assert_eq!(
            e.cap_starved_for(30_000),
            None,
            "a drive that MOVED is not the drive that was stuck"
        );
    }

    /// **#2135's restart clear, and the ONE thing that distinguishes it from
    /// the tick's.** A starvation run cannot straddle a process boundary, and
    /// the interval it would otherwise be charged for is mostly orrerix's own
    /// downtime.
    ///
    /// The two halves are the same entry, the same stamp and the same clock,
    /// differing in exactly one call — so this pins that the restart clear
    /// charges NOTHING, and not merely that it forgets the anchor. An
    /// implementation spelled `clear_cap_starvation(now)` forgets the anchor
    /// perfectly and fails the second assertion, which is the point: charging
    /// the gap would FORGIVE orrerix's own downtime from both age bounds, and
    /// that downtime being CHARGED is the property #2117 disclosed and pinned
    /// (`orrerix_downtime_is_charged_to_the_state_it_spanned`).
    #[test]
    fn the_restart_clear_forgets_a_run_without_charging_the_downtime_it_spans() {
        let mut restarted = entry_at(DriveState::ReviewWait);
        assert!(
            !restarted.discard_cap_starvation_run(),
            "an entry with no run has none to drop, and says so"
        );
        assert!(restarted.note_cap_starvation(10_000));
        assert!(restarted.discard_cap_starvation_run(), "…and a stamped one says it dropped one");
        assert_eq!(restarted.cap_starved_for(2_000_000), None, "the run is over");
        assert_eq!(
            restarted.starved_ms(2_000_000),
            0,
            "and the half hour between the last tick of one process and the first of the next \
             is charged to the drive, never forgiven as a starvation nobody observed"
        );

        // The contrast, which is what makes the assertion above a discriminator
        // rather than a restatement: the TICK's clear, over the same stamp at
        // the same clock, banks every millisecond of it.
        let mut ticked = entry_at(DriveState::ReviewWait);
        assert!(ticked.note_cap_starvation(10_000));
        assert!(ticked.clear_cap_starvation(2_000_000));
        assert_eq!(
            ticked.starved_ms(2_000_000),
            1_990_000,
            "the tick's clear banks what the run cost — that IS the difference, and it is why \
             the restart clear is a separate function rather than a call of this one"
        );
    }

    /// **#2110's exclusion, at the decision itself.** A drive the live-delegate
    /// cap will not let spawn is neither progressing nor stalled, and no clock
    /// may charge it for the difference.
    ///
    /// **The two halves differ in exactly one call** — `note_cap_starvation` —
    /// so this pins the EXCLUSION and not the bound: same entry, same state,
    /// same head, same lane list, same clock, and only whether the cap had been
    /// refusing throughout. Without the control half an implementation that
    /// never holds `state-stalled` at all would pass it; without the starved
    /// half one that ignores starvation would.
    ///
    /// And the starved half parks on the CAP rather than on nothing, which is
    /// the outcome #2110 asks for: the orchestrator is told the one thing it
    /// can act on (free a slot), instead of being told a drive stalled when
    /// what happened is that it was never allowed to move.
    #[test]
    fn time_the_cap_refused_a_lane_advances_neither_age_bound() {
        let limits = DriveLimits::default();
        let facts = |now: u64| DriveFacts {
            now_ms: now,
            required_lanes: Some(vec![lane_fact("rev-std", None, "", "")]),
            ..facts_at("head-a")
        };
        // `review-wait`'s bound is FOUR hours here: the three-hour constant
        // plus one lane at the default sixty-minute timeout (#2117 review 2
        // made it an add rather than a max). Four hours therefore reaches it
        // exactly, and is well short of the twelve-hour backstop, so exactly
        // one time bound is in play and a red is attributable to it.
        let past = 1_000 + 4 * 60 * 60_000;

        let mut moving = entry_at(DriveState::ReviewWait);
        moving.head = "head-a".to_string();
        assert_eq!(
            decide(&moving, &facts(past), &limits),
            DriveStep::held(HeldReason::StateStalled),
            "a drive that really did sit four hours in `review-wait` must park"
        );

        let mut starved = entry_at(DriveState::ReviewWait);
        starved.head = "head-a".to_string();
        // One minute of ordinary life, THEN the cap. Not starved from the
        // instant it was created: both figures below would then read zero,
        // which is also what every arithmetic error produces, and they would
        // stop discriminating between an exclusion that works and one that
        // returns nothing.
        starved.note_cap_starvation(61_000);
        assert_eq!(
            decide(&starved, &facts(past), &limits),
            DriveStep::held(HeldReason::CapFull),
            "the cap held this drive still for four hours; charging that to its own state clock reports a stall, which is the false claim #2110 is about"
        );
        assert_eq!(
            starved.state_elapsed_ms(past),
            60_000,
            "the state clock must hold at the one minute this drive spent able to act"
        );
        assert_eq!(
            starved.bounded_age_ms(past),
            60_000,
            "and so must the age the backstop reads: it is the drive's own, not the cap's"
        );
        assert_eq!(
            starved.age_ms(past),
            past - 1_000,
            "…while the WALL age is untouched, which is what `since_ms` reports"
        );
    }

    /// **#1871 B1 at the decision layer.** A verdict decides only for the
    /// revision it reviewed, and the rule is word-blind.
    ///
    /// The same recorded `fail`, read at two heads: at its own it takes arc 5 and
    /// spends a round (the control — without it, an implementation that ignored
    /// every `fail` would pass); at the head the worker moved to it is absent, so
    /// the lane is re-opened. `escalate` is asserted beside it because it takes a
    /// different arc, and a fix that special-cased `Fail` alone would pass on one
    /// and fail on the other.
    ///
    /// `entry.head` is set to the LIVE head deliberately: arc 6 would otherwise
    /// answer first and this would be a test of arc 6, not of the binding rule.
    /// That is exactly the shape the drive is in after a real fix — the tick
    /// persists the head it resolved, so by the time `review-wait` is reached
    /// again the entry and the live head agree and only the VERDICT is stale.
    #[test]
    fn a_verdict_bound_to_an_older_head_decides_nothing_whatever_word_it_is() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-b".into();
        let at = |word, verdict_head| DriveFacts {
            required_lanes: Some(vec![lane_fact("rev-std", Some(word), verdict_head, "d1")]),
            ..facts_at("head-b")
        };
        // The control: bound to the head in front of the drive, each word decides.
        assert_eq!(
            decide(&e, &at(Verdict::Fail, "head-b"), &limits),
            DriveStep::spend(DriveState::FixWait, Counter::ReviewRounds)
        );
        assert_eq!(
            decide(&e, &at(Verdict::Escalate, "head-b"), &limits),
            DriveStep::held(HeldReason::Escalate)
        );
        // Bound to the head the worker fixed: absent, so the lane is re-opened.
        // Not `fix-wait`, and not a spent round — that loop reached INVARIANT 9's
        // bound in three passes with no re-review having happened.
        assert_eq!(
            decide(&e, &at(Verdict::Fail, "head-a"), &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: false },
            "a fail recorded against a commit that no longer describes the PR must not route"
        );
        assert_eq!(
            decide(&e, &at(Verdict::Escalate, "head-a"), &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: false },
            "…nor may a stale escalate park the drive on a judgment nobody is being asked for"
        );
        // The digest half of the same key: same head, body moved under it.
        let moved = DriveFacts {
            body_digest: Some("d2".into()),
            ..at(Verdict::Fail, "head-b")
        };
        assert_eq!(decide(&e, &moved, &limits), DriveStep::OpenLane { index: 0, verify: false, body_only: false });
        // …and "we could not check" is not "it changed", in this direction too.
        let unknown = DriveFacts { body_digest: None, ..at(Verdict::Fail, "head-b") };
        assert_eq!(
            decide(&e, &unknown, &limits),
            DriveStep::spend(DriveState::FixWait, Counter::ReviewRounds),
            "an unreadable body must not stale a verdict — one transient gh failure would \
             otherwise re-brief every open lane in the group"
        );
    }

    #[test]
    fn a_head_that_moved_under_a_lane_re_enters_ci_wait() {
        // Arc 6 / §8 row 4. The race is not designed away — it is the race the
        // verdict binding already exists to handle.
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        let facts = DriveFacts {
            required_lanes: Some(vec![lane_fact("rev-std", Some(Verdict::Pass), "head-a", "d1")]),
            ..facts_at("head-b")
        };
        assert_eq!(decide(&e, &facts, &limits), DriveStep::to(DriveState::CiWait));
    }

    #[test]
    fn a_lane_briefed_at_this_head_is_waited_for_then_bounded() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        e.open_lane("rev-std", "s1", "rev-1", "head-a", Some("d1"), 1_000, false, false);
        let facts = DriveFacts {
            required_lanes: Some(vec![lane_fact("rev-std", None, "", "")]),
            now_ms: 1_000 + minutes_ms(limits.lane_timeout_minutes) - 1,
            ..facts_at("head-a")
        };
        // Open and inside its wait: nothing to do, and in particular NOT a
        // re-brief on every tick.
        assert_eq!(decide(&e, &facts, &limits), DriveStep::Wait);
        // Past `lane_timeout_minutes` with no verdict: parked, naming the pane.
        let stalled = DriveFacts {
            now_ms: 1_000 + minutes_ms(limits.lane_timeout_minutes),
            ..facts
        };
        assert_eq!(
            decide(&e, &stalled, &limits),
            DriveStep::held(HeldReason::LaneStalled)
        );
    }

    #[test]
    fn a_lane_briefed_at_an_older_head_is_re_briefed_not_waited_for() {
        // This is what `briefed_head` exists to answer, and it is why that
        // field is not `at_head`: a lane whose brief predates the live head has
        // been asked nothing about this revision.
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-b".into();
        e.open_lane("rev-std", "s1", "rev-1", "head-a", Some("d1"), 1_000, false, false);
        let facts = DriveFacts {
            required_lanes: Some(vec![lane_fact("rev-std", None, "", "")]),
            ..facts_at("head-b")
        };
        assert_eq!(decide(&e, &facts, &limits), DriveStep::OpenLane { index: 0, verify: false, body_only: false });
    }

    #[test]
    fn a_pass_whose_body_digest_moved_re_opens_that_lane() {
        // §8's body-changed row: the (head, digest) key is re-read every tick,
        // so a moved digest with an unchanged head re-enters at the first stale
        // lane with a body-only delta brief.
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        e.open_lane("rev-std", "s1", "rev-1", "head-a", Some("OLD"), 1_000, false, false);
        let facts = DriveFacts {
            required_lanes: Some(vec![lane_fact("rev-std", Some(Verdict::Pass), "head-a", "OLD")]),
            body_digest: Some("NEW".into()),
            ..facts_at("head-a")
        };
        // …and `verify` is TRUE (#2168 E2): the single required lane has a pass
        // bound to this head, so nothing about the code is outstanding and the
        // brief about to go out is a body-verification delta.
        assert_eq!(decide(&e, &facts, &limits), DriveStep::OpenLane { index: 0, verify: true, body_only: true });
    }

    #[test]
    fn a_body_only_move_re_briefs_one_lane_and_the_verification_settles_the_rest() {
        // #2168 E2, the driver half. Two required lanes, both passed at
        // (head-a, d1); the worker edits the PR body and nothing else.
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        let with = |lanes: Vec<LaneFact>| DriveFacts {
            required_lanes: Some(lanes),
            body_digest: Some("d2".into()),
            ..facts_at("head-a")
        };
        let stale = vec![
            lane_fact("rev-std", Some(Verdict::Pass), "head-a", "d1"),
            lane_fact("rev-final", Some(Verdict::Pass), "head-a", "d1"),
        ];

        // ONE lane is briefed — the first in the gate's order — and the brief
        // is a verification delta, which is what makes the verdict it produces
        // able to discharge the clause for the other.
        assert_eq!(
            decide(&e, &with(stale.clone()), &limits),
            DriveStep::OpenLane { index: 0, verify: true, body_only: false }
        );

        // S3 sends it; the drive waits rather than moving on to lane 1. That is
        // the whole saving: before #2168 E2 the second lane's turn came next.
        e.open_lane("rev-std", "s1", "rev-1", "head-a", Some("d2"), 1_500, true, false);
        assert_eq!(decide(&e, &with(stale.clone()), &limits), DriveStep::Wait);

        // rev-std records the verification pass. rev-final's pass at d1 settles
        // again, so the drive goes to the gate instead of briefing lane 1.
        let answered = vec![
            verified_lane_fact("rev-std", "head-a", "d2"),
            lane_fact("rev-final", Some(Verdict::Pass), "head-a", "d1"),
        ];
        assert_eq!(
            decide(&e, &with(answered), &limits),
            DriveStep::to(DriveState::GateCheck)
        );

        // **The control, on the axis that carries the saving: WHICH step comes
        // back.** The same round with rev-std's pass recorded WITHOUT the mark
        // — an ordinary re-review that happens to sit at the current digest —
        // still owes lane 1 a brief, where the marked round above went straight
        // to `gate-check`. An implementation that accepted any newer pass would
        // answer `gate-check` here too, and would have weakened
        // `body-unchanged` for every repo, driver or no driver.
        //
        // The brief it gets is itself a verification round, and that is right
        // rather than a leak: every required lane has a `pass` bound to this
        // head, so nothing about the CODE is outstanding and what rev-final is
        // owed is the body. The grant it carries is simply not needed here —
        // rev-std's pass already sits at the current digest — which is why the
        // discriminator in this test is the index and not the flag.
        let unmarked = vec![
            lane_fact("rev-std", Some(Verdict::Pass), "head-a", "d2"),
            lane_fact("rev-final", Some(Verdict::Pass), "head-a", "d1"),
        ];
        assert_eq!(
            decide(&e, &with(unmarked), &limits),
            DriveStep::OpenLane { index: 1, verify: true, body_only: false }
        );

        // And the delegation is bounded by the HEAD it was granted at: once the
        // code moves, a body verification settles nothing and arc 6 takes the
        // drive back to `ci-wait`.
        let pushed = DriveFacts {
            required_lanes: Some(vec![
                verified_lane_fact("rev-std", "head-a", "d2"),
                lane_fact("rev-final", Some(Verdict::Pass), "head-a", "d1"),
            ]),
            body_digest: Some("d2".into()),
            ..facts_at("head-b")
        };
        assert_eq!(decide(&e, &pushed, &limits), DriveStep::to(DriveState::CiWait));
    }

    #[test]
    fn a_verification_delta_is_only_briefed_when_nothing_about_the_code_is_outstanding() {
        // #2168 E2's precondition, one crossing per way it can fail. The grant
        // the brief carries is what lets the gate stop asking the other lanes,
        // so it is issued only when EVERY required lane has a pass bound to the
        // head that would merge — anything else and this is an ordinary round.
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        let with = |lanes: Vec<LaneFact>, digest: Option<&str>| DriveFacts {
            required_lanes: Some(lanes),
            body_digest: digest.map(|d| d.to_string()),
            ..facts_at("head-a")
        };
        let std_pass = lane_fact("rev-std", Some(Verdict::Pass), "head-a", "d1");

        // A lane that has never answered: the code is still outstanding. Lane 0
        // is still the one briefed — its own pass staled with the digest — but
        // the brief is an ORDINARY one, and the verdict it produces will not
        // discharge anything for lane 1.
        assert_eq!(
            decide(
                &e,
                &with(vec![std_pass.clone(), lane_fact("rev-final", None, "", "")], Some("d2")),
                &limits
            ),
            DriveStep::OpenLane { index: 0, verify: false, body_only: false }
        );

        // A lane whose pass is bound to a head the branch has left reads as
        // absent (#1871 B1) — and "absent" is exactly what must not be granted
        // over, because the code that lane approved is not the code in front of
        // the drive.
        assert_eq!(
            decide(
                &e,
                &with(
                    vec![
                        std_pass.clone(),
                        lane_fact("rev-final", Some(Verdict::Pass), "head-OLD", "d1")
                    ],
                    Some("d2")
                ),
                &limits
            ),
            DriveStep::OpenLane { index: 0, verify: false, body_only: false }
        );

        // A body orrerix could not read grants nothing: the verdict this brief
        // produces would carry no digest, so there would be no body for the
        // mark to be ABOUT. (An unknown digest does not stale a pass either, so
        // lane 1 here is outstanding for its own reason — no verdict at all.)
        assert_eq!(
            decide(
                &e,
                &with(vec![std_pass.clone(), lane_fact("rev-final", None, "", "")], None),
                &limits
            ),
            DriveStep::OpenLane { index: 1, verify: false, body_only: false }
        );

        // The positive control, so the three rows above are the precondition
        // deciding and not a constant.
        assert_eq!(
            decide(
                &e,
                &with(
                    vec![
                        std_pass,
                        lane_fact("rev-final", Some(Verdict::Pass), "head-a", "d1")
                    ],
                    Some("d2")
                ),
                &limits
            ),
            DriveStep::OpenLane { index: 0, verify: true, body_only: false }
        );
    }

    #[test]
    fn a_re_brief_ends_the_loop_rather_than_repeating_it_every_tick() {
        // The other half of the fix above, and the failure a head-only key
        // would swap this one for: once the lane HAS been re-briefed at the new
        // digest, the very same stale `pass` must read as "asked, thinking" and
        // not re-brief again. The verdict file still holds the old pass — a
        // reviewer has not re-recorded yet — so nothing but the brief key can
        // tell these two ticks apart.
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        let facts = DriveFacts {
            required_lanes: Some(vec![lane_fact("rev-std", Some(Verdict::Pass), "head-a", "OLD")]),
            body_digest: Some("NEW".into()),
            ..facts_at("head-a")
        };
        e.open_lane("rev-std", "s1", "rev-1", "head-a", Some("OLD"), 1_000, false, false);
        assert_eq!(decide(&e, &facts, &limits), DriveStep::OpenLane { index: 0, verify: true, body_only: true });
        // S3 performs that brief; the drive now waits on the reviewer.
        e.open_lane("rev-std", "s1", "rev-1", "head-a", Some("NEW"), 1_500, true, true);
        assert_eq!(decide(&e, &facts, &limits), DriveStep::Wait);
    }

    #[test]
    fn a_lane_is_open_only_for_the_revision_it_was_asked_about() {
        // All four crossings of {briefed head matches} x {briefed digest
        // matches}, because this guard reads two signals and the defect it
        // shipped with was reading one. Three of the four must re-brief; a
        // guard that answered "open" on the head alone passes two of them.
        let rec = |head: &str, digest: &str| LaneRecord {
            block: "rev-std".into(),
            session: "s1".into(),
            agent: "rev-1".into(),
            prior_agents: Vec::new(),
            last_verdict: None,
            at_head: String::new(),
            briefed_head: head.into(),
            briefed_digest: digest.into(),
            stopped_head: String::new(),
            spawned_ms: 0,
            briefed_verify: false,
            briefed_body_only: false,
            extra: BTreeMap::new(),
        };
        let now = Some("d1");
        assert!(lane_open_for(&rec("head-a", "d1"), "head-a", now));
        assert!(!lane_open_for(&rec("head-a", "OLD"), "head-a", now));
        assert!(!lane_open_for(&rec("head-OLD", "d1"), "head-a", now));
        assert!(!lane_open_for(&rec("head-OLD", "OLD"), "head-a", now));
        // "We could not check" is not "it changed", in either direction — one
        // transient failure to read a PR body must not re-brief every open lane
        // in the group.
        assert!(lane_open_for(&rec("head-a", "d1"), "head-a", None));
        assert!(lane_open_for(&rec("head-a", "d1"), "head-a", Some("")));
        assert!(lane_open_for(&rec("head-a", ""), "head-a", now));
        // ...but an unknown digest never rescues a head that really did move.
        assert!(!lane_open_for(&rec("head-OLD", ""), "head-a", None));
    }

    #[test]
    fn fix_wait_takes_its_two_arcs_and_its_three_holds() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::FixWait);
        e.head = "head-a".into();
        assert_eq!(e.fix_handback_ms, 1_000);

        // Arc 7: the worker pushed.
        assert_eq!(
            decide(&e, &facts_at("head-b"), &limits),
            DriveStep::to(DriveState::CiWait)
        );
        // Arc 8: report(done) with the head unchanged — a body-only fix.
        let done_same_head = DriveFacts {
            worker: WorkerSignal::Done,
            ..facts_at("head-a")
        };
        assert_eq!(
            decide(&e, &done_same_head, &limits),
            DriveStep::to(DriveState::ReviewWait)
        );
        // A push outranks a `done` that arrived with it: the code moved, so CI
        // is what has to answer next.
        let done_and_pushed = DriveFacts {
            worker: WorkerSignal::Done,
            ..facts_at("head-b")
        };
        assert_eq!(
            decide(&e, &done_and_pushed, &limits),
            DriveStep::to(DriveState::CiWait)
        );
        // The three holds.
        for (signal, reason) in [
            (WorkerSignal::Blocked, HeldReason::WorkerBlocked),
            (WorkerSignal::Unresumable, HeldReason::WorkerUnresumable),
        ] {
            let facts = DriveFacts { worker: signal, ..facts_at("head-a") };
            assert_eq!(decide(&e, &facts, &limits), DriveStep::held(reason));
        }
        // Silent inside the wait, then bounded by it.
        let quiet = DriveFacts {
            now_ms: e.fix_handback_ms + minutes_ms(limits.fix_timeout_minutes) - 1,
            ..facts_at("head-a")
        };
        assert_eq!(decide(&e, &quiet, &limits), DriveStep::Wait);
        let stalled = DriveFacts {
            now_ms: e.fix_handback_ms + minutes_ms(limits.fix_timeout_minutes),
            ..facts_at("head-a")
        };
        assert_eq!(
            decide(&e, &stalled, &limits),
            DriveStep::held(HeldReason::FixStalled)
        );
    }

    #[test]
    fn an_unresumable_worker_is_a_hold_and_never_a_drive_time_refusal() {
        // §5.1 is deliberately honest about this: a full, well-shaped session
        // id this group never recorded takes `resolve_session_ref`'s
        // passthrough arm and is ACCEPTED by `drive_review`, so its
        // unresumability surfaces here, at the first hand-back, possibly hours
        // on. Nothing in this module claims to catch it earlier — resolving is
        // not the same as proving resumable.
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::FixWait);
        e.head = "head-a".into();
        let facts = DriveFacts {
            worker: WorkerSignal::Unresumable,
            ..facts_at("head-a")
        };
        assert_eq!(
            decide(&e, &facts, &limits),
            DriveStep::held(HeldReason::WorkerUnresumable)
        );
    }

    #[test]
    fn gate_check_answers_satisfied_only_when_the_gate_did() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::GateCheck);
        e.head = "head-a".into();
        let with = |gate| decide(&e, &DriveFacts { gate, ..facts_at("head-a") }, &limits);
        // Arc 9.
        assert_eq!(with(GateOutcome::Satisfied), DriveStep::to(DriveState::Satisfied));
        // Arc 10 — wider than "stale" on purpose.
        assert_eq!(with(GateOutcome::Unsatisfied), DriveStep::to(DriveState::CiWait));
        // Present and unreadable is a hold, NOT `gate-not-configured`.
        assert_eq!(
            with(GateOutcome::Unreadable),
            DriveStep::held(HeldReason::GateUnreadable)
        );
        // Nothing was evaluated, so nothing is known — and in particular this
        // is not `satisfied`.
        assert_eq!(with(GateOutcome::NotEvaluated), DriveStep::Wait);
    }

    /// **The bounds and the `messaged` hold outrank a conflict** — the ordering
    /// `decide` states in prose, asserted (#2311 review round 2).
    ///
    /// A precedence written in a comment and pinned nowhere is a claim about the
    /// order of two `if`s, and any edit reverses it silently: every one of these
    /// facts reaches `decide` on the same tick, so which answer comes back is
    /// decided by line order alone.
    ///
    /// The direction matters. A drive already past its clock is not made young
    /// by a rebase, and `message_orchestrator` is a delegate's own words already
    /// in the pane — reporting a rebase hand-back instead of either would be a
    /// notice that does not account for what actually stopped the drive.
    #[test]
    fn the_bounds_and_the_messaged_hold_outrank_a_conflict() {
        let limits = DriveLimits::default();
        let conflict = |e: &DriveEntry, f: DriveFacts| decide(e, &f, &limits);

        // The positive control FIRST, so every refusal below is a difference:
        // on these facts, minus the thing under test, the conflict wins.
        let mut fresh = entry_at(DriveState::GateCheck);
        fresh.head = "head-a".into();
        let base = || DriveFacts { ci: CiObservation::Conflicting, ..facts_at("head-a") };
        assert_eq!(
            conflict(&fresh, base()),
            DriveStep::spend(DriveState::FixWait, Counter::RebaseAttempts),
            "the control: a conflict on an otherwise healthy drive IS the rebase arc"
        );

        // 3. `messaged` outranks it.
        assert_eq!(
            conflict(&fresh, DriveFacts { messaged: true, ..base() }),
            DriveStep::held(HeldReason::Messaged)
        );

        // 2. A positively-closed PR outranks it — a PR that is gone has nothing
        //    to rebase onto.
        assert_eq!(
            conflict(&fresh, DriveFacts { pr_open: Some(false), ..base() }),
            DriveStep::to(DriveState::Cancelled)
        );

        // 4. The drive's AGE outranks it.
        let aged = DriveFacts {
            now_ms: 1_000 + minutes_ms(limits.drive_timeout_minutes),
            ..base()
        };
        assert_eq!(conflict(&fresh, aged), DriveStep::held(HeldReason::DriveStalled));

        // 5. Time in THIS state outranks it. Under the age bound, so the answer
        //    is this clock and not the backstop above.
        let bound = state_bound_ms(DriveState::GateCheck, &limits, 0).expect("gate-check is bounded");
        let stuck = DriveFacts { now_ms: 1_000 + bound, ..base() };
        assert!(
            bound < minutes_ms(limits.drive_timeout_minutes),
            "the fixture's premise: the state bound must fire before the age backstop"
        );
        assert_eq!(conflict(&fresh, stuck), DriveStep::held(HeldReason::StateStalled));

        // …and the empty-head guard, which is "we could not read this PR at
        // all" — below the bounds, still above the conflict.
        assert_eq!(
            conflict(&fresh, DriveFacts { head: String::new(), ..base() }),
            DriveStep::Wait
        );

        // A terminal or parked entry still yields `Wait`, conflict or not: the
        // tick does not move a drive `drive_review` owns.
        for st in [DriveState::Held, DriveState::Satisfied, DriveState::Cancelled] {
            let mut e = entry_at(st);
            e.head = "head-a".into();
            assert_eq!(conflict(&e, base()), DriveStep::Wait, "{}", st.as_str());
        }
    }

    #[test]
    fn a_conflicting_pr_takes_the_rebase_arc_from_every_state_that_can() {
        // #2311, widened past the plan's `gate-check` scope by the measurement
        // on #3118: routing needs a changed-file list, GitHub computes none for
        // a conflicted head, so `review-wait` answered `routing-unaccountable`
        // — a hold naming a CONSEQUENCE of the conflict, whose remedy
        // (`drive_review` again) reproduces it.
        let limits = DriveLimits::default();
        // Each state paired with the answer it gives on the SAME facts minus
        // the conflict — which is not one answer, because `ci-wait` never
        // reads routing at all. Written per state so the control is that
        // state's own strongest other answer rather than a shared shape.
        for (st, without_the_conflict) in [
            (DriveState::CiWait, DriveStep::Wait),
            (DriveState::ReviewWait, DriveStep::held(HeldReason::RoutingUnaccountable)),
            (DriveState::GateCheck, DriveStep::held(HeldReason::RoutingUnaccountable)),
        ] {
            let mut e = entry_at(st);
            e.head = "head-a".into();
            let facts = DriveFacts {
                ci: CiObservation::Conflicting,
                // Everything else set so that any state reading it would
                // answer something ELSE — the gate says SATISFIED and routing
                // says nothing at all — which is what makes the assertion below
                // a difference rather than a shape that holds either way.
                gate: GateOutcome::Satisfied,
                required_lanes: None,
                ..facts_at("head-a")
            };
            assert_eq!(
                decide(&e, &facts, &limits),
                DriveStep::spend(DriveState::FixWait, Counter::RebaseAttempts),
                "{}: a conflicting PR is a rebase hand-back, not this state's own answer",
                st.as_str()
            );
            // The control: with the mergeability the ONLY thing changed, each
            // state gives the answer that must not have won above.
            assert_eq!(
                decide(&e, &DriveFacts { ci: CiObservation::Pending, ..facts.clone() }, &limits),
                without_the_conflict,
                "{}: the control — without the conflict this state answers for itself",
                st.as_str()
            );
            // …and the second conflict parks, on the same counter `ci-wait` has
            // always spent (`counter_exhausted`'s ordering).
            let mut spent = e.clone();
            spent.counters.rebase_attempts = limits.max_rebase_attempts;
            assert_eq!(
                decide(&spent, &facts, &limits),
                DriveStep::held(HeldReason::RebaseLimit),
                "{}",
                st.as_str()
            );
            // The machine really accepts the arc: a step `decide` proposes and
            // `transition` refuses fails at runtime, on the degradation path.
            assert!(transition(st, DriveState::FixWait).is_ok(), "{}", st.as_str());
        }
    }

    #[test]
    fn fix_wait_is_the_one_state_a_conflict_does_not_divert() {
        // A rebase hand-back is already outstanding there — `(fix-wait,
        // fix-wait)` is not a transition, and spending a second
        // `rebase_attempts` on the conflict the worker was just asked to fix
        // would park `rebase-limit` before it could push.
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::FixWait);
        e.head = "head-a".into();
        let facts = DriveFacts { ci: CiObservation::Conflicting, ..facts_at("head-a") };
        assert_eq!(decide(&e, &facts, &limits), DriveStep::Wait);
        assert!(transition(DriveState::FixWait, DriveState::FixWait).is_err());
        // And the worker's own signals still decide the state, unchanged by the
        // mergeability beside them — the positive control for "this state was
        // reached at all".
        assert_eq!(
            decide(&e, &DriveFacts { worker: WorkerSignal::Done, ..facts }, &limits),
            DriveStep::to(DriveState::ReviewWait)
        );
    }

    #[test]
    fn an_unreadable_or_unevaluated_mergeability_is_never_a_conflict() {
        // §8: an unknown is never a fact about the PR. Each state still gives
        // its own answer, both ways, so neither reading is "it waits anyway".
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::GateCheck);
        e.head = "head-a".into();
        for ci in [CiObservation::Pending, CiObservation::Unknown, CiObservation::Green] {
            for (gate, want) in [
                (GateOutcome::Satisfied, DriveStep::to(DriveState::Satisfied)),
                (GateOutcome::Unsatisfied, DriveStep::to(DriveState::CiWait)),
                (GateOutcome::Unreadable, DriveStep::held(HeldReason::GateUnreadable)),
                (GateOutcome::NotEvaluated, DriveStep::Wait),
            ] {
                assert_eq!(
                    decide(&e, &DriveFacts { ci, gate, ..facts_at("head-a") }, &limits),
                    want,
                    "{ci:?} at gate-check must leave the gate to answer"
                );
            }
        }
    }
}
