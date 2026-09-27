//! What the driver may release (#2501, #2811 S1): [`ReleaseReason`], the
//! release population, and [`releasable`].
//!
//! Part of the review driver's pure core, split out of the former single-file
//! `reviewdrive.rs` by #3498 P7 as a pure move. The module map is in `mod.rs`;
//! the design note is `docs/design/review-driver.md`.

use super::*;

/// Why a driven pane is no longer needed — the **closed set** of three, and the
/// whole of what #2501 and #2811 S1 narrowed §3.1 item 5 to.
///
/// The note's item 5 was a closed sentence ("the driver may never kill a pane")
/// and it named its own reopening condition: *"a later measurement shows drives
/// starving on panes nothing frees"*. #2501 is that measurement — 12 driven PRs,
/// 20 `rd-refused` rows on one of them, five `held(cap-refused)` exits, and about
/// 25 orchestrator wakes spent doing by hand exactly what these variants do. So
/// the item is narrowed rather than deleted, and the narrowing is this enum:
/// anything not spelled here is still a pane the driver may not touch. #2811 S1 is
/// the SECOND such measurement — 15 of 20 hand-backs in one 6.4-hour session
/// held a slot the whole round, 33 kills by hand — and it adds
/// [`DriveEnded`](ReleaseReason::DriveEnded) rather than loosening either of the
/// two that were already here.
///
/// **What makes exactly these safe is not that they are idle.** The idle
/// reaper's demotion argument ("no task in flight, nothing to lose") is half of
/// it; the other half is that in each case the pane's OUTPUT is already on
/// durable record — a verdict file the gate re-reads, or a `report` the drive has
/// consumed and acted on, or (at a terminal step) a drive with nothing left to
/// ask for at all — and the CONVERSATION survives, because the driver resumes
/// lanes and workers by session and has done since #2109. A release therefore
/// destroys nothing: not work, not a decision, not a reviewer's memory of the
/// PR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseReason {
    /// A reviewer lane whose verdict is recorded **at the revision now on the
    /// PR**. The lane has answered the only question this drive asks it, so
    /// there is nothing left in that pane for the drive to wait for; the next
    /// round resumes its session (§5.2's `session`, resolved through the record,
    /// the roster or the merged records).
    ///
    /// Word-blind, on [`lane_verdict_is_current`]'s own argument: a verdict is
    /// bound to the revision it reviewed, and what makes the pane finished is
    /// that it ANSWERED about this revision, not which word it chose.
    VerdictRecorded,
    /// The worker pane whose `report` this tick consumed. §7 intercepts it, arc
    /// 8 acts on it, and the hand-back that follows resumes
    /// [`DriveEntry::worker_session`] rather than the pane — so the pane's only
    /// remaining function was to hold a slot.
    ReportConsumed,
    /// **The drive is OVER** — this tick's step is `satisfied` or `cancelled`,
    /// and the worker pane is a slot held for a drive that will never ask it
    /// anything again (#2811 S1).
    ///
    /// It is a third variant rather than a reuse of [`ReportConsumed`] because
    /// the audit reason is a claim: a drive can reach a terminal step by a path
    /// on which no report was ever consumed (resumed out of
    /// `held(fix-stalled)`, then green), and labelling that release
    /// `report-consumed` would be a false row on the one surface §5.4 asks a
    /// reader to count from. The two variants also carry different SAFETY
    /// arguments: `ReportConsumed` rests on the report being on durable record,
    /// this one on there being nothing left to wait for at all.
    ///
    /// The guard that keeps it honest is [`DriveEntry::handback_outstanding`]:
    /// a drive cancelled while its worker is mid-round is still waiting on that
    /// worker, and releases nothing.
    DriveEnded,
    /// **A reviewer lane reviewing a head that is about to be rebased away**
    /// (#3176) — the PR is CONFLICTING and the lane has recorded nothing at this
    /// revision.
    ///
    /// The fourth variant, and the first whose safety argument is NOT the one
    /// the other three share. Their pane's output is already on durable record;
    /// this pane has produced none, and that is the whole point. A verdict
    /// recorded against a conflicted head binds to a commit the rebase is about
    /// to replace, so [`lane_verdict_is_current`] is false for it the moment the
    /// worker pushes: the round is spent and the drive re-briefs that lane at the
    /// new head anyway. What a release destroys here is therefore a review whose
    /// only possible product is a stale verdict — measured on #3150, which paid a
    /// whole `rev-std` pass exactly that way.
    ///
    /// It is a fourth WORD rather than a reuse for the reason
    /// [`DriveEnded`](ReleaseReason::DriveEnded) is one: a reason is a claim on
    /// the surface §5.4 asks a reader to count from, and a reader counting the
    /// releases that followed a finished review must be able to leave this one
    /// out.
    ///
    /// **It spends no counter and proposes no arc.** #2311's hoist already spends
    /// `rebase_attempts` for the conflict and hands the worker back;
    /// `review_rounds` is untouched here exactly as it is there, because no lane
    /// delivered any findings.
    ///
    /// A lane that has ALREADY answered at this head is deliberately left to
    /// [`VerdictRecorded`](ReleaseReason::VerdictRecorded) and to the ordinary
    /// stale-verdict handling: its pane is finished either way, and labelling
    /// that release `conflict` would be the false row this variant exists to
    /// avoid.
    Conflict,
}

impl ReleaseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ReleaseReason::VerdictRecorded => "verdict-recorded",
            ReleaseReason::ReportConsumed => "report-consumed",
            ReleaseReason::DriveEnded => "drive-ended",
            ReleaseReason::Conflict => "conflict",
        }
    }
}

/// One pane [`releasable`] says this drive no longer needs.
///
/// The pane id is deliberately NOT here. This crate is Tauri-free and cannot ask
/// whether a pane is alive, idle or typeable — those are the registry's facts,
/// injected the way [`DriveEntry::forget_dead_panes`] injects liveness — so what
/// this carries is the DRIVE's half of the decision (which side, and why) and
/// the caller reads the pane off the entry and applies its own half.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseCandidate {
    pub role: DrivenRole,
    pub reason: ReleaseReason,
    /// **Whether the caller's population includes the panes this drive was
    /// STARTED ON, and not only the ones it resumed into** (#3250) — see
    /// [`DriveEntry::founding_panes`].
    ///
    /// [`DriveEntry::owned_panes`] names what the drive itself opened or took
    /// over, and that set is EMPTY for a drive that never handed back — which
    /// is the common shape, not a corner: an orchestrator starts a drive on a
    /// worker that has already pushed and reported, every lane passes, and no
    /// hand-back is ever taken. Measured on PRs #3243 and #3248, where the
    /// audit log carries `rd-started` -> `rd-satisfied` with no `rd-handback`
    /// row at all and the worker pane the orchestrator named at
    /// `start_review_drive` was still alive and idle at the exit, holding the
    /// worktree an orchestrator then had to `kill_agent` by hand.
    ///
    /// True at a TERMINAL step and false everywhere else, and that bound is the
    /// argument, not caution. At `satisfied` and at `cancelled` alike the drive
    /// is over — its own notice tells the orchestrator the conversation resumes
    /// with `spawn_agent(resume:)` — and the terminal rule already kills the
    /// panes it OWNS at both, so the two exits differing in population was the
    /// same defect one state over rather than a deliberate narrowing (review
    /// round 1, finding 1). Mid-drive the pane may be one the orchestrator is
    /// still using, so [`ReleaseReason::ReportConsumed`] keeps the #3203
    /// population (every pane the drive OWNS on that session) and this stays
    /// false.
    ///
    /// **It is not a kill of everything on the session, and the first draft of
    /// this change was** (review round 1). The population is a RECORDED list —
    /// the panes that were live on the session when `drive_review` read it —
    /// so a pane the orchestrator opens or resumes while the drive runs is
    /// simply not in it, including one opened inside the single tick between
    /// `gate-check` and the exit, where a fresh pane reads as idle and the
    /// barrier would otherwise take it from whoever had just started speaking
    /// to it. [`admit_session_pane`] states the one further exclusion the
    /// caller applies: a pane another live drive's record names is that
    /// drive's to release, since `already-driven` is keyed on the PR and one
    /// worker session may legally back two driven PRs.
    ///
    /// And it widens only the POPULATION. Whether any one of those panes may go
    /// is still the caller's barrier — idle, alive, bound to a terminal, not a
    /// fixture role — asked per pane, so a busy pane is skipped here exactly as
    /// a busy owned one is.
    pub include_founding: bool,
}

/// **May a pane that is merely ON the drive's worker session join the terminal
/// release's population?** (#3250, narrowed in review round 1.)
///
/// [`DriveEntry::founding_panes`] is the population; this is the whole of what
/// joining it excludes, spelled as one pure predicate rather than as conditions
/// in the caller's loop — the caller can then only get it wrong by not calling
/// this, which is visible, instead of by drifting one condition, which is not.
///
/// Three reasons to answer `false`:
///
/// - **an empty id**, which names no pane.
/// - **`already`** — the panes the drive OWNS, already in the population. One
///   pane, one barrier question, one audit row.
/// - **`owned_elsewhere`** — the panes any OTHER live drive OWNS, which is that
///   drive's [`owned_panes`](DriveEntry::owned_panes) and nothing else: the
///   panes it opened, took over or superseded. `already-driven` is keyed on the
///   PR and not on the session, so one worker session may legally back two
///   driven PRs, and a pane the other drive owns is a pane it is going to speak
///   to again — the exact claim this release rests on, broken by the release
///   itself (review round 1, finding 1).
///
///   **It does NOT cover the other drive's own founding list, and that is a
///   stated residual rather than an oversight** (review round 2). Two drives
///   started on one session name the same founding pane, so each considers it
///   its own and whichever reaches a terminal step first releases it. Excluding
///   it on both sides would trade that for a pane neither drive ever releases —
///   the leak this issue is about — so the release is left where it is and the
///   consequence is written down: the other drive's next hand-back re-opens the
///   conversation with `spawn_agent(resume:)`, which is the recovery the whole
///   release rests on and is exactly what it would have done for a pane the
///   human had killed.
///
/// **Deliberately NOT a comparison of timestamps.** The first draft admitted a
/// session pane if it was older than the drive, which reads well and is
/// untestable: an agent's `started_ms` is the wall clock while a drive's is the
/// caller's injected `now`, so under any synthetic clock the two are on
/// different scales — the same divergence that kept `drive-stalled` from ever
/// firing in a test before #2811 B2. A recorded list answers the same question
/// with no clock in it at all.
pub fn admit_session_pane(
    agent: &str,
    already: &[String],
    owned_elsewhere: &[String],
) -> bool {
    !agent.trim().is_empty()
        && !already.iter().any(|a| a == agent)
        && !owned_elsewhere.iter().any(|a| a == agent)
}

/// **The terminal release's worker population: the panes this drive owns plus
/// the panes it was started on, oldest first** (#3250, review round 2).
///
/// The merge and the sort are here rather than in the caller's loop because the
/// ORDER is a claim two comments make and only the merged list can keep. Every
/// member of `owned` was minted by a hand-back, so each one post-dates the pane
/// the drive was started on: appending `founding` to it puts the audit rows
/// newest-first-then-oldest, the inverse of "the rows read as the history they
/// are". The caller supplies `started_ms` because a pane's age is the
/// registry's fact and this crate is Tauri-free — the same injection
/// [`DriveEntry::forget_dead_panes`] takes for liveness.
///
/// A pane the registry cannot date sorts LAST rather than first: it is a pane
/// that is already gone or was never on the roster, so the barrier will refuse
/// it, and putting it at the head would place a row that never happens in front
/// of ones that do. Ties break on the id, so two panes registered inside one
/// wall-clock millisecond cannot order differently between runs.
///
/// Only the founding half is filtered — `owned` is the drive's own record and
/// is already deduplicated by [`retain_panes`] — and the filtering is
/// [`admit_session_pane`], asked once per founding pane.
pub fn release_population(
    owned: Vec<String>,
    founding: &[String],
    owned_elsewhere: &[String],
    started_ms: &dyn Fn(&str) -> Option<u64>,
) -> Vec<String> {
    let mut out = owned;
    for a in founding {
        if admit_session_pane(a, &out, owned_elsewhere) {
            out.push(a.clone());
        }
    }
    out.sort_by_key(|a| (started_ms(a).unwrap_or(u64::MAX), a.clone()));
    out
}

/// **The panes this drive no longer needs, at this tick's facts and this tick's
/// step** (#2501) — the drive-side half of §3.1 item 5's narrowing.
///
/// Pure, and separate from [`decide`] rather than folded into it, because a
/// release is **not an arc**: it moves no state, spends no counter and appears
/// in no row of §2.1's table. It is the same shape as [`DriveStep::OpenLane`]
/// being "not a transition" — a side effect the tick performs beside the step,
/// not instead of one.
///
/// # The three conditions, and what each excludes
///
/// **1. The step must not PARK the drive.** An `Advance` into `held` releases
/// nothing, and that is not caution — it is what keeps §6's hold notices true: a
/// parked drive's notice says its panes are "still running" and that a
/// `drive_review` resume speaks to them again
/// ([`PaneStanding::Owned`](crate::rddrive::PaneStanding::Owned)), which is a
/// promise about panes the drive still means to use.
///
/// **A TERMINAL step is the opposite case and releases MORE, not less** (#2811 S1).
/// It used to be folded in with `held` here, on the reading that a terminal
/// notice "hands the panes to the orchestrator to dispose of". Measured, that
/// hand-off is a bill: the orchestrator killed the reporting worker in the same
/// second it started 4 of one session's 16 drives, and the next hand-back then
/// resumed the session into a FRESH pane — a spawn per round that a released
/// pane would not have needed. Nothing is being waited for at a terminal step by
/// definition, so both rules below are evaluated and their panes go; what the
/// notice then names is whatever the barrier refused, which is exactly the list
/// an orchestrator still has something to do about.
///
/// A lane released on an EARLIER tick is simply not in
/// [`DriveEntry::owned_panes`] any more, so the notice keeps naming exactly the
/// panes that are still there.
///
/// **This is a condition on the STEP, and the drive can still end the tick
/// parked despite it** (rev-final W1). The step is what `decide` proposed; the
/// tick's ARM can then refuse and park on its own — `Advance{to: FixWait}` whose
/// `rd_handback` cannot resume the worker becomes `held(worker-unresumable)` or
/// `held(cap-refused)` — and by then the release has happened, because the tick
/// performs it BEFORE the arm. Saying "a parking DRIVE releases nothing" would
/// be false there, so it is not said anywhere; what holds is the step.
///
/// **Nothing is lost by the narrower reading, and one thing is gained.** The
/// notices are built after the release, off the live record, so a hold's line
/// still names only panes that are actually there. And releasing before the arm
/// is what stops the commonest way that hold was reached at all: the freed slot
/// is available to `rd_handback`'s own spawn, so a drive whose lane has just
/// answered no longer hits the live-delegate cap handing the fix back.
///
/// **2. A lane's verdict must be CURRENT.** Asked with
/// [`lane_verdict_is_current`] — the same function `review-wait` decides with,
/// never a second implementation of it — over the lane facts this tick READ, so
/// a verdict recorded three commits ago releases nothing. A head that could not
/// be read is empty, `lane_verdict_is_current` is false for it, and the tick
/// releases nothing at all: "we could not check" is not "it has answered".
///
/// **3. The worker must have REPORTED into a drive that is waiting on it — or
/// the drive must be over.** `WorkerSignal::Done` and no other word: `Blocked`
/// is INVARIANT 3 territory and parks the drive for the orchestrator, which is a
/// pane a human is about to talk to. `Silent` has reported nothing at all. And
/// the state condition is what makes "the driver has consumed the report" a fact
/// rather than a description — the signal is one-shot, cleared when the arc it
/// fed is durable, so the tick that sees `Done` while a hand-back is
/// outstanding **and takes an arc on it** is the tick that consumed it. That last clause is load-bearing in `ci-wait`
/// and in `ci-wait` only: there a `Done` can arrive before the matrix settles,
/// [`decide_ci_wait`] answers `Wait`, and nothing has been consumed yet — so
/// the pane is kept for the tick that really does end the wait. In `fix-wait`
/// every `Done` takes an arc (2, or 7 when the head moved under it), so the
/// clause changes nothing there and #2501's rows are unmoved.
///
/// **"Waiting on it" is [`DriveEntry::handback_outstanding`], which since #2168
/// E1 spans TWO states** (#2811 S1). This condition used to read `fix-wait` alone,
/// and E1 moved the report's consumption into `ci-wait` for the ordinary
/// push-then-report ordering — so for every hand-back that actually pushed code,
/// the condition and its consumer never met. Measured on one 6.4-hour session:
/// all 5 releases were body-only fixes, the other 15 of 20 hand-backs held a
/// live-delegate slot through ci-wait and the next review round, and 33 of the
/// orchestrator's pane kills were doing this by hand. The predicate is asked
/// once, on `DriveEntry`, because [`DriveEntry::kickback_owed`] needs the same
/// answer and two spellings of it is how the first drift happened.
///
/// **At a terminal step the word is not read at all** and the reason is
/// [`ReleaseReason::DriveEnded`]: nothing is outstanding, so there is no report
/// to consume and no wait to end. What is checked there is the mirror image —
/// that a hand-back is NOT outstanding, so a `cancelled` that arrives while the
/// worker is mid-round leaves that pane alone.
///
/// # What this deliberately does NOT release
///
/// A lane that has been briefed and has not answered; a lane whose verdict binds
/// to an older revision; a worker that reported `blocked` or has said nothing;
/// a worker still owed a round by a drive that is being `cancelled` under it;
/// a pane belonging to a drive whose STEP parks it this tick (see condition 1
/// for why that is not the same as "a drive that parks"); and every pane in the
/// group that is not this drive's. The orchestrator's kill authority is
/// untouched by all of it — this narrows what the DRIVER may do, and adds
/// nothing anywhere else.
///
/// # The residual, stated because a narrowing must state one
///
/// The worker rule still fires on ONE tick — the arc out of the wait, from
/// either of its two states — and the caller's own half of the decision can
/// refuse it there: a pane still finishing its turn is not idle, and an idle
/// check that says "no" is not retried, because the next tick is no longer
/// waiting on that worker and the fact has expired. What #2811 S1 changes is only
/// WHICH tick that is, not that there is one. The refused case costs exactly
/// what it cost before #2501 (the pane holds its slot until the next hand-back
/// reuses it, per #1960) — and now, additionally, until the drive ENDS, where
/// the terminal rule asks once more without the report's help. So the failure
/// direction is the old behaviour, bounded one exit sooner than it used to be.
///
/// **The tool cancel is not a terminal STEP and is outside all of this.**
/// `cancel_review_drive` ends a drive without a tick, so no `decide` runs, no
/// `releasable` is asked, and its notice names every owned pane as
/// [`PaneStanding::Released`] exactly as it always did. That is deliberate
/// rather than an oversight: the orchestrator that called the tool is the party
/// disposing of the panes, it is awake in the turn that called it, and the
/// notice it gets back is the list. The measured cost this slice is about is
/// paid by drives the DRIVER ends while the orchestrator is doing something
/// else.
///
/// **A worker session two drives share is the one case where a release reaches a
/// pane that is not only this drive's**, and it is disclosed rather than guarded
/// against, because the guard would have to be a claim about another drive's
/// intentions. Nothing stops two `drive_review` calls on different PRs naming one
/// worker session, and after both hand back they name one pane; the `report(done)`
/// that arrives is consumed by whichever drive `rd_owner` matches, so exactly one
/// of them releases it. The other learns on its next tick — `rd_pane_exit` reads
/// the death and names the initiator (`ended by driver-release`), so it parks
/// `held(worker-unresumable)` with a truthful line instead of waiting out
/// `fix-stalled`. That is faster and more informative than the pre-#2501
/// behaviour on the same fixture, where the second drive sat ninety minutes for a
/// report it was never going to be handed; what it is NOT is free, and a repo
/// that drives two PRs off one worker session is already the #338/#359 shape the
/// exit notices warn about.
///
/// The lane rule retries instead of expiring — it is a standing property of the
/// tick's facts — but it is **bounded by which states read those facts**, and
/// that bound is worth naming rather than leaving to be discovered. `facts.
/// required_lanes` is fetched only in `review-wait` and `gate-check`; §2.4 pays
/// for the routed-file list in exactly the states that consult it and nowhere
/// else. So a lane whose pane was still writing when its verdict landed is
/// released on the next tick the drive spends in one of those two states —
/// immediately, while another required lane is outstanding, and otherwise on the
/// drive's next review round. A one-lane gate whose only lane was busy on the
/// tick that read its pass therefore keeps that pane to the exit, where §6's
/// notice names it for the orchestrator exactly as it always did.
pub fn releasable(
    entry: &DriveEntry,
    facts: &DriveFacts,
    step: &DriveStep,
) -> Vec<ReleaseCandidate> {
    // Condition 1, and since #2811 S1 it is about PARKING alone — which is not the
    // same as "a drive that parks": the arm can refuse and park after this has
    // answered. See the doc above (rev-final W1).
    let mut terminal = false;
    // **Whether this tick takes an ARC at all**, which is what makes "the driver
    // consumed the report" a fact rather than a description: `rdtick` clears the
    // one-shot signal on every arc and on nothing else, so a tick that merely
    // WAITS has consumed nothing. It is reachable with a `Done` in hand — green
    // has not landed yet, so `decide_ci_wait` waits — and releasing there would
    // free the pane one tick before the drive stopped needing it, on a claim
    // (`report-consumed`) that was not yet true.
    let mut advancing = false;
    if let DriveStep::Advance { to, .. } = step {
        if to.is_parked() {
            return Vec::new();
        }
        terminal = to.is_terminal();
        advancing = true;
    }
    let mut out: Vec<ReleaseCandidate> = Vec::new();
    // Condition 3, first, so the list reads worker-first exactly as
    // `owned_panes` does.
    // **The guard is "this drive has a worker side", not "this drive resumed a
    // pane"** (#3250). Keying the whole condition on `worker_agent` made the
    // terminal rule unreachable for every drive that never handed back — the
    // exact drives the rule is most needed on, since a drive that DID hand back
    // has usually released that pane on the report already. The session is what
    // a drive always has: `drive_review` refuses without one (§5.1).
    if !entry.worker_agent.is_empty() || !entry.worker_session.trim().is_empty() {
        if terminal {
            // The drive is over. The only thing that keeps its worker pane is a
            // round still outstanding — a `cancelled` that arrived while the
            // worker was mid-fix — and `handback_outstanding` is the same
            // question asked the same way one branch down.
            if !entry.handback_outstanding() {
                out.push(ReleaseCandidate {
                    role: DrivenRole::Worker,
                    reason: ReleaseReason::DriveEnded,
                    // Both terminal steps: see the field's own doc for why the
                    // two exits cannot differ here.
                    include_founding: true,
                });
            }
        } else if !entry.worker_agent.is_empty()
            && advancing
            && entry.handback_outstanding()
            && facts.worker == WorkerSignal::Done
        {
            out.push(ReleaseCandidate {
                role: DrivenRole::Worker,
                reason: ReleaseReason::ReportConsumed,
                include_founding: false,
            });
        }
    }
    // Condition 2. `required_lanes` is `None` in the states that do not read it
    // and when the routing could not be computed at all; both are "we do not
    // know which lanes are required", and neither is a licence to release one.
    let digest = facts.body_digest.as_deref();
    for l in facts.required_lanes.iter().flatten() {
        if !lane_verdict_is_current(l.verdict.as_ref(), &facts.head, digest) {
            continue;
        }
        // A lane with no record, or one whose record carries no pane, has
        // nothing to release — including one released on an earlier tick, whose
        // pane slot is empty precisely so this stays true.
        match entry.lane(&l.block) {
            Some(rec) if !rec.agent.trim().is_empty() => {}
            _ => continue,
        }
        out.push(ReleaseCandidate {
            role: DrivenRole::Lane(l.block.clone()),
            reason: ReleaseReason::VerdictRecorded,
            include_founding: false,
        });
    }
    // **Condition 4: the PR does not merge, so every open lane is reviewing a
    // head that is about to be rebased away** (#3176).
    //
    // Keyed on `facts.ci` and not on the STEP, and that is the difference
    // between a fix and a coin flip. #2311's hoist takes arc 3 on the first tick
    // that observes the conflict, and on that tick the lane's pane is whatever it
    // happened to be doing — mid-turn as often as not, which
    // `release_driven_pane`'s idle barrier refuses (§3.1 item 5). Asked of the
    // FACTS, the rule is a standing property the way condition 2's is: it is
    // re-asked on every later tick the drive spends waiting out the same
    // conflict, so the pane goes on the first tick it is between turns instead of
    // on the one tick that took the arc. It is bounded by the conflict itself —
    // the worker's rebase moves the head, mergeability clears, and the rule stops
    // matching — and by `rebase-limit` / `fix-stalled` under it.
    //
    // Conditions 1's exclusions still apply above: a step that PARKS the drive
    // returned empty, so a `held(rebase-limit)` conflict releases nothing and §6's
    // notice keeps naming panes that are really there. A TERMINAL step is excluded
    // here rather than there — `satisfied` cannot be reached on a conflict since
    // #2311, and at `cancelled` the panes are the orchestrator's to dispose of,
    // named on the way out.
    //
    // **The lane that has answered is not this variant's.** It is condition 2's
    // when the routing could be read, and the ordinary stale-verdict handling's
    // when it could not; either way its pane is finished, and a `conflict` row on
    // it would be the false claim [`ReleaseReason::Conflict`] exists to avoid. The
    // record is what answers, because `facts.required_lanes` is `None` in exactly
    // the states a conflicted PR is usually observed in — GitHub computes no
    // changed-file list for a head that does not merge, which is #2311's own
    // measurement — so a rule that could only read `facts` would fire nowhere it
    // mattered. `LaneRecord::at_head` is a record of what the drive READ rather
    // than a gate input, and it is read here only to DECLINE a release: it can
    // cost a slot, never a review.
    if !terminal && facts.ci == CiObservation::Conflicting {
        for l in &entry.lanes {
            if l.agent.trim().is_empty() {
                continue;
            }
            let role = DrivenRole::Lane(l.block.clone());
            if out.iter().any(|c| c.role == role) {
                continue;
            }
            let answered_here = l.last_verdict.is_some()
                && !l.at_head.is_empty()
                && l.at_head == facts.head;
            if answered_here {
                continue;
            }
            out.push(ReleaseCandidate {
                role,
                reason: ReleaseReason::Conflict,
                include_founding: false,
            });
        }
    }
    out
}

// SCRATCH ONLY (#3498 P7 red-before-green): never merged.
#[allow(dead_code)]
fn planted_for_scratch(c: &mut Counters) {
    fn kill_agent(_: &str) {}
    kill_agent("w-1");
    c.body_only_grace = true;
    let _ = Counter::BodyOnlyGrace;
    let _ = vec!["pr", "merge"];
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── #2501: what the driver may release ──────────────────────────────────

    /// A `review-wait` entry with one lane, briefed at `head`, running in pane
    /// `rev-1` on session `sess-lane`.
    fn lane_open_at(head: &str) -> DriveEntry {
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = head.to_string();
        e.open_lane("rev-std", "sess-lane", "rev-1", head, Some("d1"), 1_000, false, false);
        e
    }

    /// The step a live `review-wait` tick takes when it is simply waiting — the
    /// neutral one, so a release these tests observe is the LANE rule firing and
    /// not the step type.
    const LIVE: DriveStep = DriveStep::Wait;

    /// **The lane rule, both directions, on one fixture whose only moving part
    /// is the verdict's binding.**
    ///
    /// The two rows differ in the head the verdict is bound to and in nothing
    /// else — same entry, same pane, same live head — so a `releasable` that
    /// ignored currency and a `releasable` that released nothing are both red
    /// here. That is the discrimination CLAUDE.md's non-discriminating-fixture
    /// rule asks for: the candidate outputs must DIVERGE.
    #[test]
    fn a_lane_is_released_when_its_verdict_binds_to_the_live_revision_and_not_before() {
        for (label, at_head, digest, owed) in [
            ("answered about this revision", "h1", "d1", true),
            ("answered about an older head", "h0", "d1", false),
            ("answered before the body moved", "h1", "d0", false),
        ] {
            let e = lane_open_at("h1");
            let mut f = facts_at("h1");
            f.required_lanes =
                Some(vec![lane_fact("rev-std", Some(Verdict::Pass), at_head, digest)]);
            let got = releasable(&e, &f, &LIVE);
            assert_eq!(
                got.len(),
                usize::from(owed),
                "{label}: releasable said {got:?}"
            );
            if owed {
                assert_eq!(got[0].role, DrivenRole::Lane("rev-std".into()), "{label}");
                assert_eq!(got[0].reason, ReleaseReason::VerdictRecorded, "{label}");
            }
        }
    }

    /// **Word-blind, exactly as [`lane_verdict_is_current`] is.** A lane that
    /// said `fail` or `escalate` about this revision has answered, and what makes
    /// its pane finished is that it answered — not which word it chose.
    ///
    /// The negative control is in the row above and is the one that makes this
    /// mean something: currency, not the word, is what decides.
    #[test]
    fn the_lane_rule_reads_the_binding_and_never_the_word() {
        for word in [Verdict::Pass, Verdict::Fail, Verdict::Escalate] {
            let e = lane_open_at("h1");
            let mut f = facts_at("h1");
            f.required_lanes = Some(vec![lane_fact("rev-std", Some(word), "h1", "d1")]);
            assert_eq!(
                releasable(&e, &f, &LIVE).len(),
                1,
                "a lane that answered {:?} at this revision is finished with this round",
                word
            );
        }
    }

    /// **A lane with no pane on record releases nothing** — which is what makes
    /// the release idempotent rather than a loop that kills once and then keeps
    /// naming a candidate nothing can act on. Same facts as the positive row
    /// above; the only difference is that the pane slot is already empty.
    #[test]
    fn a_lane_whose_pane_is_already_released_is_not_a_candidate_again() {
        let mut e = lane_open_at("h1");
        let mut f = facts_at("h1");
        f.required_lanes = Some(vec![lane_fact("rev-std", Some(Verdict::Pass), "h1", "d1")]);
        assert_eq!(releasable(&e, &f, &LIVE).len(), 1, "the control: it is a candidate first");

        let freed = e.release_pane(&DrivenRole::Lane("rev-std".into()), "sess-lane");
        assert_eq!(freed.as_deref(), Some("rev-1"), "the pane it un-recorded");
        assert_eq!(
            e.lane("rev-std").map(|l| l.session.as_str()),
            Some("sess-lane"),
            "…and the conversation, which is the whole promise"
        );
        assert!(releasable(&e, &f, &LIVE).is_empty(), "not a candidate a second time");
    }

    /// **A step that PARKS the drive releases nothing**, which is what keeps
    /// §6's hold notices true: they say the panes they name are still running
    /// and that a `drive_review` resume speaks to them again.
    ///
    /// The live arc is the positive control — otherwise "released nothing" is
    /// indistinguishable from a fixture that was never releasable at all — and
    /// the TERMINAL test below is the other half of the discrimination this pair
    /// needs after #2811 S1: park and end now diverge, so a `releasable` that
    /// treated them alike (in either direction) is red in one of the two.
    #[test]
    fn a_parking_step_releases_nothing() {
        let mut f = facts_at("h1");
        f.required_lanes = Some(vec![lane_fact("rev-std", Some(Verdict::Pass), "h1", "d1")]);
        assert_eq!(
            releasable(&lane_open_at("h1"), &f, &LIVE).len(),
            1,
            "the positive control: these facts DO release under a live step"
        );
        let step = DriveStep::Advance {
            to: DriveState::Held,
            held_reason: Some(HeldReason::Escalate),
            bump: None,
        };
        assert!(
            releasable(&lane_open_at("h1"), &f, &step).is_empty(),
            "a step into held must release nothing"
        );
        let live = DriveStep::Advance {
            to: DriveState::GateCheck,
            held_reason: None,
            bump: None,
        };
        assert_eq!(
            releasable(&lane_open_at("h1"), &f, &live).len(),
            1,
            "…while an arc that leaves the drive live still does"
        );
    }

    /// **A TERMINAL step releases the lane whose verdict is current AND the
    /// worker, because there is nothing left to wait for** (#2811 S1).
    ///
    /// The pre-#2811 S1 behaviour is the failing side of every row: it released
    /// nothing at all here, so the orchestrator was handed the panes in the exit
    /// notice and killed them by hand — 33 kills in the measured session, and a
    /// fresh spawn on the next hand-back for want of a pane that was resumable
    /// all along.
    ///
    /// The last loop is the one guard on it: a `cancelled` that lands while the
    /// worker still owes this drive a round leaves that pane alone, so "the
    /// drive is over" is not read as "nobody is working".
    #[test]
    fn a_terminal_step_releases_the_lane_and_the_worker_it_is_finished_with() {
        for (label, to) in
            [("satisfied", DriveState::Satisfied), ("cancelled", DriveState::Cancelled)]
        {
            let mut e = lane_open_at("h1");
            e.advance(DriveState::GateCheck, None, None, 1_000).unwrap();
            e.record_worker_pane("w-1");
            let mut f = facts_at("h1");
            f.required_lanes = Some(vec![lane_fact("rev-std", Some(Verdict::Pass), "h1", "d1")]);
            let step = DriveStep::Advance { to, held_reason: None, bump: None };
            let got = releasable(&e, &f, &step);
            assert_eq!(got.len(), 2, "{label}: releasable said {got:?}");
            assert_eq!(
                got[0].role,
                DrivenRole::Worker,
                "{label}: worker-first, as owned_panes reads"
            );
            assert_eq!(got[0].reason, ReleaseReason::DriveEnded, "{label}");
            assert_eq!(got[1].role, DrivenRole::Lane("rev-std".into()), "{label}");
            assert_eq!(got[1].reason, ReleaseReason::VerdictRecorded, "{label}");
        }

        // A lane whose verdict does NOT bind here is still not released, so the
        // terminal rule widened WHEN condition 2 is asked and not what it says.
        let mut e = lane_open_at("h1");
        e.advance(DriveState::GateCheck, None, None, 1_000).unwrap();
        let mut f = facts_at("h1");
        f.required_lanes = Some(vec![lane_fact("rev-std", Some(Verdict::Pass), "h0", "d1")]);
        let step = DriveStep::Advance { to: DriveState::Satisfied, held_reason: None, bump: None };
        let got = releasable(&e, &f, &step);
        assert!(
            !got.iter().any(|c| matches!(c.role, DrivenRole::Lane(_))),
            "a stale verdict is stale at a terminal step too: {got:?}"
        );
        // …and what IS proposed here is the worker side, on the session alone
        // (#3250): this fixture never handed back, so `worker_agent` is empty
        // and the pre-#3250 rule proposed nothing at all — which is the defect,
        // measured as an idle worker pane surviving `rd-satisfied` on PRs #3243
        // and #3248. The caller's barrier still decides per pane.
        assert_eq!(
            got,
            vec![ReleaseCandidate {
                role: DrivenRole::Worker,
                reason: ReleaseReason::DriveEnded,
                include_founding: true,
            }],
            "the satisfied exit asks about the worker session it was started on"
        );

        // The guard: cancelled while the worker still owes this drive a round.
        for pushed in [false, true] {
            let mut e = entry_at(DriveState::FixWait);
            if pushed {
                e.advance(DriveState::CiWait, None, None, 1_000).unwrap();
                assert!(e.fix_pushed(), "arc 7 is what makes this ci-wait a wait on the worker");
            }
            e.head = "h1".to_string();
            e.record_worker_pane("w-1");
            let mut f = facts_at("h1");
            f.worker = WorkerSignal::Silent;
            let step =
                DriveStep::Advance { to: DriveState::Cancelled, held_reason: None, bump: None };
            assert!(
                releasable(&e, &f, &step).is_empty(),
                "pushed={pushed}: a worker mid-round is not released by the drive being cancelled"
            );
        }
    }

    /// **Every reason a founding pane is refused, and the one shape that is
    /// admitted** (#3250, review round 1).
    ///
    /// The rows differ in ONE input each from the admitted row, so a predicate
    /// that dropped any single condition is red here — the discrimination
    /// CLAUDE.md's non-discriminating-fixture rule asks for. The two exclusion
    /// rows are the ones that matter: without the first a pane is asked about
    /// twice and audited twice, and without the second this drive's exit kills
    /// a pane ANOTHER live drive is still going to speak to.
    #[test]
    fn a_founding_pane_joins_the_release_unless_it_is_owned_here_or_driven_elsewhere() {
        let owned = vec!["w-owned".to_string()];
        let elsewhere = vec!["w-other-drive".to_string()];
        for (label, agent, admit) in [
            ("the founding pane the orchestrator handed over", "w-founding", true),
            ("a pane the drive itself owns is already in", "w-owned", false),
            ("another live drive's pane is that drive's", "w-other-drive", false),
            ("no pane at all", "  ", false),
        ] {
            assert_eq!(admit_session_pane(agent, &owned, &elsewhere), admit, "{label}");
        }
    }

    /// **One population holding an owned pane AND a founding one, in the order
    /// the rows claim** (#3250, review round 2) — the fixture the sort has to
    /// have, because with only one of the two kinds any order is the right one.
    ///
    /// The owned pane is the YOUNGER of the two, which is not a fixture choice
    /// but the shape of the thing: `owned` is written by a hand-back and a
    /// hand-back happens after the drive was handed the founding pane. So the
    /// merge order and the answer DIVERGE — drop the sort and this reddens with
    /// the two swapped, which is the mutation the claim owes.
    ///
    /// The undatable pane is asserted in the same run for the same reason it
    /// sorts last: a change that made an unknown age sort FIRST would put a row
    /// that never happens at the head of the list.
    #[test]
    fn the_release_population_reads_oldest_first_across_owned_and_founding_panes() {
        let age = |a: &str| match a {
            "w-founding" => Some(100u64),
            "w-handback" => Some(900),
            "w-second-founding" => Some(300),
            _ => None,
        };
        let got = release_population(
            vec!["w-handback".to_string()],
            &["w-founding".to_string(), "w-second-founding".to_string()],
            &[],
            &age,
        );
        assert_eq!(
            got,
            vec![
                "w-founding".to_string(),
                "w-second-founding".to_string(),
                "w-handback".to_string()
            ],
            "the hand-back pane is the youngest and comes last, whatever order it was merged in"
        );

        let with_ghost = release_population(
            vec!["w-handback".to_string()],
            &["w-ghost".to_string(), "w-founding".to_string()],
            &[],
            &age,
        );
        assert_eq!(
            with_ghost,
            vec!["w-founding".to_string(), "w-handback".to_string(), "w-ghost".to_string()],
            "a pane the registry cannot date sorts last, not first"
        );

        assert_eq!(
            release_population(
                vec!["w-handback".to_string()],
                &["w-founding".to_string(), "w-handback".to_string(), String::new()],
                &["w-second-founding".to_string()],
                &age,
            ),
            vec!["w-founding".to_string(), "w-handback".to_string()],
            "and the filtering is unchanged: owned, elsewhere and empty are all refused"
        );
    }

    /// **The founding list is recorded TOTAL, deduped, and never files a pane
    /// the drive has since resumed into** (#3250, review round 1).
    ///
    /// Total rather than additive is the half a resume depends on: a second
    /// `drive_review` re-reads the session, and a founding pane that has since
    /// died must not survive by having been recorded once. The `worker_agent`
    /// exclusion is what keeps one pane out of two lists, which the caller's
    /// dedup would otherwise have to catch every time.
    #[test]
    fn founding_panes_are_recorded_total_deduped_and_never_alongside_the_current_pane() {
        let mut e = entry_at(DriveState::CiWait);
        e.record_founding_panes(vec!["w-1".into(), "w-2".into(), "w-1".into(), String::new()]);
        assert_eq!(e.founding_panes, vec!["w-1".to_string(), "w-2".to_string()], "deduped");

        e.record_worker_pane("w-2");
        e.record_founding_panes(vec!["w-1".into(), "w-2".into()]);
        assert_eq!(
            e.founding_panes,
            vec!["w-1".to_string()],
            "the pane the drive has resumed into is owned, not founding"
        );

        e.record_founding_panes(vec!["w-3".into()]);
        assert_eq!(
            e.founding_panes,
            vec!["w-3".to_string()],
            "a re-read REPLACES: a founding pane that has gone does not survive the resume"
        );

        e.forget_worker_panes();
        assert!(e.founding_panes.is_empty(), "a different session forgets these too");
    }

    /// **A dead founding pane is dropped by the same prune that bounds the
    /// superseded lists** (#3250) — and a live one is never evicted.
    ///
    /// The `changed` answer is asserted in both directions because it is what
    /// decides whether the tick WRITES: a prune that dropped a pane and
    /// reported `false` would leave the record claiming a pane that is gone
    /// until something else happened to write.
    #[test]
    fn a_dead_founding_pane_is_pruned_and_says_so() {
        let mut e = entry_at(DriveState::CiWait);
        e.record_founding_panes(vec!["w-live".into(), "w-dead".into()]);
        assert!(
            e.forget_dead_panes(&|a: &str| a != "w-dead"),
            "dropping a founding pane is a change the caller must persist"
        );
        assert_eq!(e.founding_panes, vec!["w-live".to_string()]);
        assert!(!e.forget_dead_panes(&|_: &str| true), "…and a prune that drops nothing is no change");
    }

    /// **The worker rule: a hand-back outstanding plus `Done`, and nothing
    /// else** (#2501, corrected by #2811 S1).
    ///
    /// Every other worker signal is a row here, because each is a different
    /// reason to keep the pane: `Blocked` parks the drive for the orchestrator,
    /// which is about to talk to that pane; `Silent` has said nothing;
    /// `Unresumable` has already gone.
    ///
    /// **The two `ci-wait` `Done` rows are the correction, and the arc-7 anchor
    /// is their only moving part** — same state, same signal, same pane.
    /// Before #2811 S1 this table said `(ci-wait, Done) => false` outright,
    /// reasoning that such a report was about a hand-back already consumed;
    /// #2168 E1 had moved the consumption there, so the row was describing the
    /// very tick that consumes it, and 15 of one session's 20 hand-backs paid a
    /// held slot for the whole round. The `ci-wait`-without-a-push row keeps the
    /// original claim, which is still true of it: no hand-back is outstanding
    /// there, so a `Done` is not this drive's to consume.
    #[test]
    fn the_worker_is_released_only_on_the_tick_that_consumes_its_done_report() {
        // The step a tick takes when it really does consume the report: arc 2.
        // `LIVE` (a bare `Wait`) is the wrong instrument for this table — a tick
        // that consumes nothing must release nothing, which the last row pins.
        let arc2 = DriveStep::Advance {
            to: DriveState::ReviewWait,
            held_reason: None,
            bump: None,
        };
        for (state, pushed, signal, owed) in [
            (DriveState::FixWait, false, WorkerSignal::Done, true),
            (DriveState::FixWait, false, WorkerSignal::Blocked, false),
            (DriveState::FixWait, false, WorkerSignal::Silent, false),
            (DriveState::FixWait, false, WorkerSignal::Unresumable, false),
            (DriveState::CiWait, true, WorkerSignal::Done, true),
            (DriveState::CiWait, true, WorkerSignal::Blocked, false),
            (DriveState::CiWait, true, WorkerSignal::Silent, false),
            (DriveState::CiWait, true, WorkerSignal::Unresumable, false),
            (DriveState::CiWait, false, WorkerSignal::Done, false),
            (DriveState::ReviewWait, false, WorkerSignal::Done, false),
        ] {
            // Walked through the arcs rather than stamped, so no fixture here
            // can encode a `fix_pushed_ms` the machine would not have written.
            let mut e = if pushed {
                let mut e = entry_at(DriveState::FixWait);
                e.advance(DriveState::CiWait, None, None, 1_000).unwrap();
                e
            } else {
                entry_at(state)
            };
            assert_eq!(e.state(), state, "the fixture is in the state the row names");
            assert_eq!(
                e.fix_pushed(),
                pushed,
                "…and carries the arc-7 anchor iff the row says so"
            );
            e.head = "h1".to_string();
            e.record_worker_pane("w-1");
            let mut f = facts_at("h1");
            f.worker = signal;
            let got = releasable(&e, &f, &arc2);
            assert_eq!(
                got.len(),
                usize::from(owed),
                "{}/pushed={pushed}/{signal:?} released {got:?}",
                state.as_str()
            );
            if owed {
                assert_eq!(got[0].role, DrivenRole::Worker);
                assert_eq!(got[0].reason, ReleaseReason::ReportConsumed);
            }
        }

        // **A tick that WAITS has consumed nothing**, and this is a real state
        // rather than a hypothetical: a worker that reports before the matrix
        // settles is a `Done` sitting in `ci-wait` while `decide_ci_wait`
        // answers `Wait`. The only difference from the positive row above is the
        // step, so the fixture discriminates on exactly the axis this pins.
        let mut e = entry_at(DriveState::FixWait);
        e.advance(DriveState::CiWait, None, None, 1_000).unwrap();
        e.head = "h1".to_string();
        e.record_worker_pane("w-1");
        let mut f = facts_at("h1");
        f.worker = WorkerSignal::Done;
        assert_eq!(releasable(&e, &f, &arc2).len(), 1, "the control: the arc DOES release");
        assert!(
            releasable(&e, &f, &LIVE).is_empty(),
            "a tick that takes no arc has consumed no report, so it releases nothing"
        );
    }

    /// **One predicate, two callers** (#2811 S1) — `kickback_owed` and
    /// `releasable` ask "is a hand-back outstanding" through
    /// [`DriveEntry::handback_outstanding`] and not through two spellings of it,
    /// which is how the #2811 S1 drift happened in the first place.
    ///
    /// Pinned as a property of the ENTRY over every live state, so a later edit
    /// that re-inlines either reading diverges from this table rather than from
    /// the other caller silently.
    #[test]
    fn a_handback_is_outstanding_in_fix_wait_and_in_a_pushed_ci_wait_only() {
        for state in DriveState::ALL {
            if state.is_terminal() || state.is_parked() {
                continue;
            }
            let e = entry_at(state);
            assert_eq!(
                e.handback_outstanding(),
                state == DriveState::FixWait,
                "{} without a push",
                state.as_str()
            );
        }
        let mut e = entry_at(DriveState::FixWait);
        e.advance(DriveState::CiWait, None, None, 1_000).unwrap();
        assert!(e.handback_outstanding(), "arc 7 keeps the wait alive in ci-wait");
        // Arc 2 ends it, and that is the tick the release rides.
        e.advance(DriveState::ReviewWait, None, None, 1_000).unwrap();
        assert!(!e.handback_outstanding(), "…and the arc out of it ends the wait");
    }

    /// **A release refuses when the conversation cannot be named.** Killing the
    /// pane of a lane whose session nothing can resolve would end the reviewer's
    /// memory of the PR, which is the one thing this mechanism promises to keep.
    ///
    /// The positive control is the second half: the same lane, with a session,
    /// releases — so the refusal is the empty session and not the fixture.
    #[test]
    fn a_release_refuses_a_lane_whose_session_cannot_be_named() {
        let mut e = entry_at(DriveState::ReviewWait);
        e.open_lane("rev-std", "", "rev-1", "h1", Some("d1"), 1_000, false, false);
        assert_eq!(
            e.release_pane(&DrivenRole::Lane("rev-std".into()), ""),
            None,
            "no session on the record and none resolved: the pane stays"
        );
        assert_eq!(
            e.lane("rev-std").map(|l| l.agent.as_str()),
            Some("rev-1"),
            "…and the record still names it, so the next tick can try again"
        );
        assert_eq!(
            e.release_pane(&DrivenRole::Lane("rev-std".into()), "sess-resolved"),
            Some("rev-1".to_string()),
            "the control: a resolved session releases"
        );
        assert_eq!(
            e.lane("rev-std").map(|l| l.session.as_str()),
            Some("sess-resolved"),
            "…and the resolved id is written onto the record, so the resume no longer \
             depends on the dead pane's own row"
        );
    }

    /// The worker's half of the same refusal, and the pane list that follows
    /// from it: a released pane is not in [`DriveEntry::owned_panes`], which is
    /// what keeps every exit notice's "still running" true.
    #[test]
    fn a_released_worker_leaves_the_owned_pane_list() {
        let mut e = entry_at(DriveState::FixWait);
        e.record_worker_pane("w-1");
        assert!(
            e.owned_panes().iter().any(|(a, r)| a == "w-1" && *r == DrivenRole::Worker),
            "the control: it is owned first"
        );
        assert_eq!(e.release_pane(&DrivenRole::Worker, ""), Some("w-1".to_string()));
        assert!(
            !e.owned_panes().iter().any(|(a, _)| a == "w-1"),
            "a released pane must not be named by a notice that says its panes are still running"
        );
        assert_eq!(
            e.release_pane(&DrivenRole::Worker, ""),
            None,
            "…and there is nothing left to release twice"
        );
    }

    /// `DriveEntry::new` records the worker SESSION, so the worker arm's
    /// fail-closed branch needs an entry that has none — the shape a drive
    /// started without one would have. Pinned because the branch is otherwise
    /// unreachable from the fixtures above and would read as dead code.
    #[test]
    fn a_worker_with_no_recorded_session_is_never_released() {
        let mut e = entry_at(DriveState::FixWait);
        e.worker_session = String::new();
        e.record_worker_pane("w-1");
        assert_eq!(
            e.release_pane(&DrivenRole::Worker, "sess-ignored"),
            None,
            "the worker's session is the entry's own; a caller may not supply one for it"
        );
    }
}
