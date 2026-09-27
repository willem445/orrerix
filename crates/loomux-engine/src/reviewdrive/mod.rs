//! The engine-driven review-loop driver: the pure core (#1778 S1).
//!
//! Design note: `docs/design/review-driver.md`. That note is the spec, and this
//! module is the half of it that has no I/O in it at all — the state machine
//! (§2.1), the persisted shape (§5.2), the counters (§2.3) and the decision
//! `rd_driver_tick` makes once its facts are in hand (§2.4). The tick itself,
//! the `gh` reads behind those facts, the spawns, the notices and the audit
//! lines are S3's, in `src-tauri`; the MCP tools (§5.1) are S4's; the `driver:`
//! block (§5.3) is S2's, in `workflow.rs`.
//!
//! **Why the split is drawn here and not somewhere more convenient.** Every
//! decision this feature makes is a function of facts orrerix read a moment
//! earlier — the PR's head, its checks, a verdict file, a clock. Putting the
//! decision in the same function as the reads makes it testable only through a
//! fake `gh`, which is how a state machine ends up pinned by its plumbing. So
//! [`decide`] takes the facts as an argument, reads no clock, spawns no child
//! and touches no file, and the whole of §2.1's table is exercised by building
//! a [`DriveFacts`] and asserting a [`DriveStep`].
//!
//! # What this module is NOT
//!
//! It is not a second reader of the merge gate. §4 of the note is explicit that
//! a third implementation of the gate decision is a defect rather than an
//! optimization, so the gate's answer, the routed lane list and each lane's
//! verdict arrive here as **facts the existing parsers produced** —
//! [`ReviewVerdict`] itself is what a [`LaneFact`] carries, and the staleness
//! questions are asked with that type's own [`ReviewVerdict::reviewed`] and
//! [`ReviewVerdict::body_changed`] rather than with a comparison written here.
//!
//! # Where this module knowingly goes beyond the note
//!
//! Three persisted fields exist that §5.2's example entry does not show, and
//! they are called out here rather than left for a reader to notice as drift.
//! §2.2 bounds two waits — `held(lane-stalled)` is "no verdict inside
//! `lane_timeout_minutes`", `held(fix-stalled)` was "neither pushed nor reported
//! inside `fix_timeout_minutes`" until #2168 E1 gave it a second site with its own
//! anchor — and the shape in §5.2 carries no timestamp
//! from which either could be measured, while §2.4 resumes a drive from disk
//! across a restart, so an in-memory clock cannot carry them either. A bound
//! with no anchor is not a bound. So:
//!
//! - [`LaneRecord::spawned_ms`] — when this lane's delegate was last spawned or
//!   resumed. The `lane-stalled` anchor.
//! - [`LaneRecord::briefed_head`] and [`LaneRecord::briefed_digest`] — the
//!   revision that lane was last briefed at, as **one key**, the same
//!   `(head, digest)` the gate binds a verdict to. §2.1 re-briefs a lane whose
//!   `pass` went stale, and without this the driver cannot tell a lane already
//!   re-opened at the live revision (wait for it) from one that still needs
//!   re-opening (brief it) — it would re-brief every tick. The head alone is
//!   not the key; [`lane_open_for`] carries why, and the defect it names is one
//!   this slice actually shipped and CI caught.
//! - [`DriveEntry::fix_handback_ms`] — when the drive last entered `fix-wait`.
//!   The `fix-stalled` anchor **in `fix-wait`**; the hand-back is the moment
//!   that wait began. #2168 E1 gives that hold a second site, in `ci-wait`,
//!   whose wait began at the PUSH rather than at the hand-back and so is
//!   measured from `state_since_ms` — see [`decide_fix_receipts`].
//! - [`DriveEntry::fix_kickback_ms`] — when the drive last answered a worker's
//!   `report(progress)` in that worker's own pane (#1959). Not a timeout
//!   anchor: it is compared against `fix_handback_ms`, which makes the budget
//!   one answer per hand-back and renews it with no reset to remember.
//! - [`DriveEntry::fix_pushed_ms`] — when the worker last pushed onto a head
//!   this drive handed back for; `None` unless `ci-wait` was entered by arc 7
//!   (#2168 E1). The `fix-stalled` anchor in THAT state, and the one clock here
//!   that a non-arc also writes: `note_fix_push` re-stamps it when a further
//!   push lands mid-wait, which is exactly what `state_since_ms` cannot do,
//!   since `transition` refuses a `ci-wait` -> `ci-wait` self-arc and so leaves
//!   the state clock on the FIRST push.
//!
//! `drive-stalled` needs none of these: it is the drive's **age**,
//! `now - started_ms`, so it keeps §5.2's own `started_ms`.
//!
//! **§2.2 used to forbid a general "when did the state last change" stamp, and
//! #2110 adds exactly that field on purpose** —
//! [`DriveEntry::state_since_ms`], written on every arc. The ban was never
//! about the stamp; it was about a drive whose ONLY bound is one, and its
//! worked example is §8's `also: [base-green]` row: that drive advances
//! `gate-check` → `ci-wait` on every wake, resets any per-state clock forever,
//! and would sit on a red default branch in silence. So the age is kept, as the
//! backstop that cycler falls through to, and the per-state clocks are added
//! above it. Both, not either.
//!
//! What forced the addition is the other half of the same question. Two drives
//! were parked `drive-stalled` at four hours, and neither was stalled: one was
//! mid-round with CI green at a new head, the other had spent three of those
//! hours unable to spawn a lane at all because another drive held every slot.
//! An age cannot tell those from paralysis, because every drive's age grows at
//! the same rate whatever it is doing — and the reason the *first* clock in
//! this struct was an age is that a bound with no anchor is not a bound, not
//! that an age was the right measure. See [`state_bound_ms`], and
//! [`DriveEntry::starved_total_ms`] for the time both clocks now exclude.
//!
//! - [`DriveEntry::state_since_ms`] — the `state-stalled` anchor.
//! - [`DriveEntry::starved_total_ms`], [`DriveEntry::starved_state_ms`] — what
//!   the drive spent unable to spawn, which neither clock charges it for.
//! - [`DriveEntry::held_from`], [`DriveEntry::held_after_ms`] — what the drive
//!   was doing when a bound fired, so a resume is a decision rather than a
//!   reflex.
//!
//! # One seam here is not a contract
//!
//! [`decide`] and the fact types it consumes are a **slice author's seam**, not
//! a §-backed public contract: the note specifies the states, the arcs, the
//! counters and the file, and says nothing about how the tick's decision is
//! factored out of its reads. Do not go looking for the section — there isn't
//! one. What that means for a later change is that this shape may be reworked
//! on its own merits, where [`DriveState`], [`transition`] and
//! [`ReviewDrivesState`] may not: those three are the note's, and changing one
//! changes `docs/design/review-driver.md` first.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::workflow::{BlockId, ReviewVerdict, Verdict, DRIVER_DRIVE_TIMEOUT_DEFAULT_MIN};

mod states;
mod counters;
mod store;
mod bounds;
mod entry;
mod facts;
mod decision;

pub use states::*;
pub use counters::*;
pub use store::*;
pub use bounds::*;
pub use entry::*;
pub use facts::*;
pub use decision::*;

/// What one reviewer's verdict SUMMARY says about the findings it left open
/// (#3367 item 1): a count per class, or `None` where the summary does not
/// state one unambiguously.
///
/// **Parsed from prose, and fail-safe in exactly one direction.** The
/// driver has never parsed findings out of a summary — [`crate::rddrive`]'s
/// satisfied notice says so — and it does not start pretending to here. What
/// this reads is a COUNT the reviewer stated (`0 blocking`, `blocking: 0`,
/// `3 non-blocking`, `no blocking findings`), because `reviewer.md` asks every
/// reviewer to state one; a summary that states none, or states two different
/// ones, is `None`, and `None` is "unknown", which never licenses a hand-back.
/// A reviewer that writes an unexpected phrasing costs the orchestrator the
/// wake it would have had anyway, and never costs the PR a round nobody
/// dispositioned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatedFindings {
    pub blocking: Option<u32>,
    pub non_blocking: Option<u32>,
}

/// A count word as a reviewer writes one: digits, or the small English numbers
/// verdict summaries actually use ("no blocking", "Two non-blocking notes").
fn count_word(tok: &str) -> Option<u32> {
    if let Ok(n) = tok.parse::<u32>() {
        return Some(n);
    }
    let n = match tok {
        "no" | "zero" | "none" => 0,
        "one" => 1,
        "two" => 2,
        "three" => 3,
        "four" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "eight" => 8,
        "nine" => 9,
        "ten" => 10,
        _ => return None,
    };
    Some(n)
}

/// Read [`StatedFindings`] out of one verdict summary.
///
/// The grammar is two shapes and nothing else, both matched on whole tokens:
/// `<count> <label>` and `<label>: <count>`, where the label is `blocking` /
/// `blocker(s)` or their `non-` forms (`non blocking` and `nonblocking` are
/// normalised to `non-blocking` first, so a `blocking` token can never be the
/// second half of a non-blocking one). A count is a whole token — `round-2
/// blocking finding fixed` is not a count of two, because `round-2` is not a
/// count word. Every count stated for a class must agree; two different ones
/// (`1 blocking … fixed; 0 blocking now`) make that class `None`.
pub fn stated_findings(summary: &str) -> StatedFindings {
    let (blocking, non_blocking) = stated_counts(summary);
    let agreed = |v: &[u32]| -> Option<u32> {
        let first = *v.first()?;
        v.iter().all(|n| *n == first).then_some(first)
    };
    StatedFindings { blocking: agreed(&blocking), non_blocking: agreed(&non_blocking) }
}

/// EVERY count a summary states, per class — `(blocking, non_blocking)` —
/// before [`stated_findings`] asks whether they agree (#3388 review round 2).
///
/// Its own function because the clean case needs the question the agreement
/// hides: "did this reviewer state ANY non-zero count?" A summary reading
/// `1 blocking finding from round 1 fixed; 0 blocking now` agrees on nothing,
/// so [`stated_findings`] answers `None` for it — correct for a count, and
/// exactly the case a veto keyed on that answer would miss.
pub fn stated_counts(summary: &str) -> (Vec<u32>, Vec<u32>) {
    let norm = summary
        .to_lowercase()
        .replace("non blocking", "non-blocking")
        .replace("nonblocking", "non-blocking")
        .replace("non blocker", "non-blocker");
    let toks: Vec<(&str, bool)> = norm
        .split_whitespace()
        .map(|raw| {
            let core = raw.trim_matches(|c: char| !(c.is_alphanumeric() || c == '-'));
            (core, raw.ends_with(':'))
        })
        .collect();
    let label = |core: &str| -> Option<bool> {
        match core {
            "blocking" | "blocker" | "blockers" => Some(true),
            "non-blocking" | "non-blocker" | "non-blockers" => Some(false),
            _ => None,
        }
    };
    let mut blocking: Vec<u32> = Vec::new();
    let mut non_blocking: Vec<u32> = Vec::new();
    for (i, (core, colon)) in toks.iter().enumerate() {
        let Some(is_blocking) = label(core) else { continue };
        // ONE count per label, and a colon label takes the count AFTER it: in
        // `blocking: 0, non-blocking: 2` the `0,` before `non-blocking:` is the
        // previous clause's, and reading it too would make a well-stated
        // summary disagree with itself.
        let after = if *colon { toks.get(i + 1).and_then(|t| count_word(t.0)) } else { None };
        let n = after.or_else(|| i.checked_sub(1).and_then(|j| count_word(toks[j].0)));
        if let Some(n) = n {
            if is_blocking {
                blocking.push(n);
            } else {
                non_blocking.push(n);
            }
        }
    }
    (blocking, non_blocking)
}

/// **What one PASS verdict leaves open — the reviewer's structured declaration
/// first, the summary parser as its fallback** (#3367 item 5).
///
/// `open_findings` is the explicit form of the number [`stated_findings`]
/// reads out of prose, so there is ONE count with two sources rather than two
/// counts: where the reviewer declared it, it wins; where it did not, the
/// parser answers exactly as it did before this field existed.
///
/// A declaration is a TOTAL, blocking and non-blocking together. On a `pass`
/// nothing is blocking by the verdict's own definition (`reviewer.md`: an
/// approval with findings open is only ever one with non-blocking ones), so an
/// unstated blocking count reads as `0` here and the rest of the total is
/// non-blocking. A summary that STATES a blocking count larger than the
/// declared total contradicts it, and a contradiction is unknown on both
/// classes — the fail-safe direction, since unknown never licenses a hand-back
/// and never makes a lane clean.
///
/// Called only for passes: [`lane_residuals`] and the notice helpers filter to
/// `Verdict::Pass` before they ask.
pub fn verdict_findings(summary: &str, open_findings: Option<u32>) -> StatedFindings {
    let stated = stated_findings(summary);
    let Some(total) = open_findings else { return stated };
    let blocking = stated.blocking.unwrap_or(0);
    if blocking > total {
        return StatedFindings::default();
    }
    StatedFindings { blocking: Some(blocking), non_blocking: Some(total - blocking) }
}

/// **This pass declared `open_findings: 0` and its summary states no open
/// finding of either class** (#3367 item 5) — one lane's half of the clean
/// case.
///
/// The declaration is REQUIRED: a lane that omitted it is not clean, however
/// its summary reads, because "the parser found `0 blocking, 0 non-blocking`"
/// is prose and the clean case skips the orchestrator's disposition entirely.
///
/// **And the summary has a veto over it, in BOTH classes** (#3388 review round
/// 2, a policy call made blocking): any non-zero count the summary states —
/// `1 blocking`, or `2 non-blocking` — makes the lane not clean, whatever
/// was declared. "Clean" means nothing is left to disposition (INVARIANT 3),
/// and a reviewer stating open nits has left something, however it filled in
/// the field. The veto reads [`stated_counts`], every count stated, never the
/// agreed one: `1 blocking … fixed; 0 blocking now` agrees on nothing, and a
/// veto keyed on agreement would let it through. The price is disclosed rather
/// than avoided: a summary whose multi-round prose still carries round one's
/// count loses the shortcut and wakes the orchestrator, which is the wake it
/// would have had anyway.
///
/// This is a veto on the CLEAN flag only. [`verdict_findings`] still reads the
/// declaration first for the non-blocking round, so a declared `0` still ends
/// the nit loop for that lane — a lane vetoed here is simply satisfied the
/// ordinary way, with its residual the orchestrator's to disposition.
pub fn declared_clean(summary: &str, open_findings: Option<u32>) -> bool {
    let (blocking, non_blocking) = stated_counts(summary);
    open_findings == Some(0) && blocking.iter().chain(&non_blocking).all(|n| *n == 0)
}

/// **Every required lane PASSED at `head`, each declaring `open_findings: 0`**
/// (#3367 item 5).
///
/// Positive on every axis, like [`residual_is_nonblocking_only`]: an empty
/// lane list, an empty head, a stale pass, a lane with no verdict, a `fail` or
/// an `escalate` all answer `false`, and `false` is simply the ordinary
/// `GATE SATISFIED` — nothing is refused, only the shortcut is not taken.
pub fn lanes_are_clean(lanes: &[LaneFact], head: &str) -> bool {
    !head.is_empty()
        && !lanes.is_empty()
        && lanes.iter().all(|l| {
            l.verdict.as_ref().is_some_and(|v| {
                v.verdict == Verdict::Pass
                    && v.reviewed(head)
                    && declared_clean(&v.summary, v.open_findings)
            })
        })
}

/// **The clean case**: the gate is satisfied at the live head, CI is green, and
/// [`lanes_are_clean`] (#3367 item 5).
///
/// CI is asked here explicitly rather than inherited from the gate, because a
/// gate need not declare `also: [ci-green]` and the brief's "clean" includes a
/// green CI whatever this repo's gate says. What the answer changes is the
/// satisfied exit's ROUTE — an enqueue where the merge queue is on, a flagged
/// notice where it is not — never whether the drive is satisfied.
pub fn gate_is_clean(facts: &DriveFacts) -> bool {
    facts.gate == GateOutcome::Satisfied
        && facts.ci == CiObservation::Green
        && facts.required_lanes.as_deref().is_some_and(|l| lanes_are_clean(l, &facts.head))
}

/// One required lane's residual, as a non-blocking round or a satisfied
/// notice reports it (#3367 item 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneResidual {
    pub block: BlockId,
    /// `Some` only for a `pass` bound to the live head — a stale verdict, a
    /// `fail`, an `escalate` or no verdict at all states nothing this round can
    /// act on, and reads as all-unknown.
    pub findings: StatedFindings,
}

/// Every required lane's [`LaneResidual`] at `head`, in the gate's order.
pub fn lane_residuals(lanes: &[LaneFact], head: &str) -> Vec<LaneResidual> {
    lanes
        .iter()
        .map(|l| LaneResidual {
            block: l.block.clone(),
            findings: l
                .verdict
                .as_ref()
                .filter(|v| v.verdict == Verdict::Pass && v.reviewed(head))
                // #3367 item 5: the declaration first, the parser as fallback —
                // which is also what makes `open_findings: 0` END the nit loop
                // for this lane: it states zero non-blocking, so it can never be
                // the lane `residual_is_nonblocking_only` needs above zero.
                .map(|v| verdict_findings(&v.summary, v.open_findings))
                .unwrap_or_default(),
        })
        .collect()
}

/// **Every lane stated zero blocking, and at least one stated a positive
/// non-blocking count** — the one residual a non-blocking round may be
/// spent on (#3367 item 1).
///
/// Both halves are POSITIVE statements. A lane that did not say how many
/// blocking findings it left is unknown, and unknown wakes the orchestrator —
/// the fail-safe the brief names. A lane that stated no non-blocking count is
/// fine as long as some other lane stated one: the hand-back says "address all
/// the findings on the PR", so the worker reads the PR, not this count.
pub fn residual_is_nonblocking_only(residuals: &[LaneResidual]) -> bool {
    !residuals.is_empty()
        && residuals.iter().all(|r| r.findings.blocking == Some(0))
        && residuals.iter().any(|r| r.findings.non_blocking.is_some_and(|n| n > 0))
}

/// **May the driver hand a satisfied gate back to the worker, on its own, for
/// the non-blocking findings still open?** (#3367 item 1)
///
/// Every answer that is not a positive yes is arc 9 — the orchestrator is
/// woken with GATE SATISFIED, which is today's behaviour. The yes needs all
/// four:
///
/// 1. **The repo asked for it and has rounds left**: `nit_rounds <
///    fix_nonblocking_rounds`, which is false at the default `0`.
/// 2. **INVARIANT 9's bound has a round left** — the SHARED one,
///    check-before-bump through [`counter_exhausted`], because the round this
///    spends is a review round (see [`Counter::NonblockingRound`]).
/// 3. **The revision moved since the last such round.** A gate satisfied again
///    at the head AND body the last round was handed back at is a worker that
///    changed nothing — it answered the findings on the PR instead — and a
///    second round would hand back the identical list. An unreadable digest at
///    an unchanged head counts as unchanged: "we could not check" never buys a
///    round.
/// 4. **Every required lane positively stated zero blocking, and some lane a
///    positive non-blocking count** — [`residual_is_nonblocking_only`].
pub fn nonblocking_round_applies(
    entry: &DriveEntry,
    facts: &DriveFacts,
    limits: &DriveLimits,
) -> bool {
    let limits = limits.clamped();
    if entry.nit_rounds >= limits.fix_nonblocking_rounds {
        return false;
    }
    if counter_exhausted(entry.counters.review_rounds, limits.max_review_rounds) {
        return false;
    }
    if !entry.nit_head.is_empty() && entry.nit_head == facts.head {
        let moved = match facts.body_digest.as_deref() {
            Some(d) => !d.is_empty() && !entry.nit_digest.is_empty() && d != entry.nit_digest,
            None => false,
        };
        if !moved {
            return false;
        }
    }
    let Some(lanes) = facts.required_lanes.as_deref() else { return false };
    residual_is_nonblocking_only(&lane_residuals(lanes, &facts.head))
}

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

    // ── #3367 item 1: the driver's own non-blocking round ───────────────────

    /// A `pass` at `head` whose summary is `summary` — the one input the
    /// non-blocking round reads that the other lane fixtures leave empty.
    fn nit_lane(block: &str, head: &str, summary: &str) -> LaneFact {
        let mut l = lane_fact(block, Some(Verdict::Pass), head, "d1");
        if let Some(v) = l.verdict.as_mut() {
            v.summary = summary.to_string();
        }
        l
    }

    /// A `gate-check` drive whose gate has just answered satisfied over `lanes`.
    fn satisfied_over(lanes: Vec<LaneFact>) -> (DriveEntry, DriveFacts) {
        let mut e = entry_at(DriveState::GateCheck);
        e.head = "head-a".into();
        let f = DriveFacts {
            required_lanes: Some(lanes),
            gate: GateOutcome::Satisfied,
            ..facts_at("head-a")
        };
        (e, f)
    }

    fn nit_limits(n: u32) -> DriveLimits {
        DriveLimits::default().with_fix_nonblocking_rounds(n)
    }

    const NIT_ROUND: DriveStep = DriveStep::Advance {
        to: DriveState::FixWait,
        held_reason: None,
        bump: Some(Counter::NonblockingRound),
    };

    /// **The grammar, over the phrasings this repo's reviewers actually
    /// write** — the first five rows are verdict summaries recorded in this
    /// group's own `verdicts/` directory, cut at the clause that matters.
    #[test]
    fn a_stated_finding_count_is_read_and_anything_ambiguous_is_unknown() {
        let s = |t: &str| stated_findings(t);
        let both = |b, n| StatedFindings { blocking: b, non_blocking: n };
        // Recorded summaries.
        assert_eq!(s("Completeness traced. 0 blocking; 3 non-blocking: the ..."), both(Some(0), Some(3)));
        assert_eq!(s("PASS. Two non-blocking notes, not routed"), both(None, Some(2)));
        assert_eq!(s("rev-final, whole-diff at 5a56b2fd. No blocking finding."), both(Some(0), None));
        assert_eq!(s("whole diff: 0 blocking findings. Round-2 fix verified"), both(Some(0), None));
        assert_eq!(s("Round-3 blocking finding fixed: the paragraph now says"), both(None, None));
        // The colon form, and the normalised non- spellings.
        assert_eq!(s("blocking: 0, non-blocking: 2"), both(Some(0), Some(2)));
        assert_eq!(s("0 blockers, 1 non blocking"), both(Some(0), Some(1)));
        assert_eq!(s("zero blocking, 4 nonblocking"), both(Some(0), Some(4)));
        // A `blocking` token is never the tail of a non-blocking one: this
        // states NO blocking count, not a count of three.
        assert_eq!(s("3 non-blocking"), both(None, Some(3)));
        // A count is a whole token: `round-6` is not six.
        assert_eq!(s("Both round-6 blocking findings fixed"), both(None, None));
        // Two DIFFERENT counts for one class are unknown, never the smaller.
        assert_eq!(s("1 blocking finding from round 1 fixed; 0 blocking now"), both(None, None));
        // The same count twice is still that count.
        assert_eq!(s("0 blocking. Summary: 0 blocking, 2 non-blocking"), both(Some(0), Some(2)));
        assert_eq!(s(""), both(None, None));
    }

    /// **Default `0` is today's behaviour**: the most loop-shaped residual
    /// there is still wakes the orchestrator.
    #[test]
    fn at_the_default_a_nonblocking_residual_still_satisfies() {
        let (e, f) =
            satisfied_over(vec![nit_lane("rev-std", "head-a", "0 blocking; 2 non-blocking")]);
        assert_eq!(decide(&e, &f, &DriveLimits::default()), DriveStep::to(DriveState::Satisfied));
        // The positive control, so the assertion above is about the default and
        // not about a residual the round could never have taken.
        assert_eq!(decide(&e, &f, &nit_limits(1)), NIT_ROUND);
    }

    /// **Every wake the brief names, one row each, beside the one residual that
    /// loops** — so a predicate that looped on any of them reddens its own row.
    #[test]
    fn a_nonblocking_round_is_taken_only_on_a_positively_stated_nit_residual() {
        let l = nit_limits(2);
        let sat = DriveStep::to(DriveState::Satisfied);
        let case = |lanes: Vec<LaneFact>| {
            let (e, f) = satisfied_over(lanes);
            decide(&e, &f, &l)
        };
        // Loops: every lane says 0 blocking, one says 2 non-blocking, the other
        // states no non-blocking count at all.
        assert_eq!(
            case(vec![
                nit_lane("rev-std", "head-a", "0 blocking; 2 non-blocking"),
                nit_lane("rev-final", "head-a", "No blocking finding."),
            ]),
            NIT_ROUND
        );
        // Wakes: zero open findings anywhere.
        assert_eq!(case(vec![nit_lane("rev-std", "head-a", "0 blocking, 0 non-blocking")]), sat);
        // Wakes: a lane that did not state its blocking count (fail-safe).
        assert_eq!(
            case(vec![
                nit_lane("rev-std", "head-a", "0 blocking; 2 non-blocking"),
                nit_lane("rev-final", "head-a", "Two non-blocking notes"),
            ]),
            sat
        );
        // Wakes: a finding a lane labels blocking, even on a pass.
        assert_eq!(case(vec![nit_lane("rev-std", "head-a", "1 blocking; 2 non-blocking")]), sat);
        // Wakes: the counting pass is bound to an OLD head, so it states nothing
        // about this revision.
        assert_eq!(case(vec![nit_lane("rev-std", "head-old", "0 blocking; 2 non-blocking")]), sat);
    }

    /// **The bound is SHARED and never exceeded**: a drive with its INVARIANT 9
    /// review rounds spent wakes even with nit rounds left, and a drive with
    /// its nit rounds spent wakes even with review rounds left.
    #[test]
    fn the_nonblocking_round_spends_the_shared_bound_and_its_own() {
        let lanes = vec![nit_lane("rev-std", "head-a", "0 blocking; 2 non-blocking")];
        let (mut e, f) = satisfied_over(lanes.clone());
        e.counters.review_rounds = MAX_ROUNDS_CEILING;
        assert_eq!(decide(&e, &f, &nit_limits(3)), DriveStep::to(DriveState::Satisfied));
        e.counters.review_rounds = MAX_ROUNDS_CEILING - 1;
        assert_eq!(decide(&e, &f, &nit_limits(3)), NIT_ROUND, "one review round left: taken");

        let (mut e, f) = satisfied_over(lanes);
        e.nit_rounds = 1;
        assert_eq!(decide(&e, &f, &nit_limits(1)), DriveStep::to(DriveState::Satisfied));
        assert_eq!(decide(&e, &f, &nit_limits(2)), NIT_ROUND, "control: one nit round left");

        // `advance` pays for it on BOTH counters, in the one arm.
        let (mut e, _) = satisfied_over(Vec::new());
        e.advance(DriveState::FixWait, None, Some(Counter::NonblockingRound), 3_000).unwrap();
        assert_eq!((e.counters.review_rounds, e.nit_rounds), (1, 1));
        assert!(e.nit_handback, "the fix-wait it entered knows it is a nit round");
        // …and every OTHER arc into fix-wait says it is not one.
        e.advance(DriveState::CiWait, None, None, 4_000).unwrap();
        e.advance(DriveState::FixWait, None, Some(Counter::CiAttempts), 5_000).unwrap();
        assert!(!e.nit_handback, "a red-CI hand-back after a nit round is not a nit round");
        // A repo cannot widen it past the ceiling either.
        assert_eq!(nit_limits(9).fix_nonblocking_rounds, MAX_ROUNDS_CEILING);
    }

    /// **A worker that changed nothing does not buy a second identical round.**
    #[test]
    fn a_gate_satisfied_again_at_the_handed_back_revision_wakes() {
        let lanes = vec![nit_lane("rev-std", "head-a", "0 blocking; 2 non-blocking")];
        let (mut e, mut f) = satisfied_over(lanes);
        e.nit_rounds = 1;
        e.nit_head = "head-a".into();
        e.nit_digest = "d1".into();
        assert_eq!(decide(&e, &f, &nit_limits(3)), DriveStep::to(DriveState::Satisfied));
        // An unreadable body at the same head is not a move.
        f.body_digest = None;
        assert_eq!(decide(&e, &f, &nit_limits(3)), DriveStep::to(DriveState::Satisfied));
        // A body edit at the same head IS one (the worker answered in the body).
        f.body_digest = Some("d2".into());
        assert_eq!(decide(&e, &f, &nit_limits(3)), NIT_ROUND);
        // …and so is a push.
        let (mut e2, f2) =
            satisfied_over(vec![nit_lane("rev-std", "head-b", "0 blocking; 1 non-blocking")]);
        e2.nit_rounds = 1;
        e2.nit_head = "head-a".into();
        e2.nit_digest = "d1".into();
        let f2 = DriveFacts { head: "head-b".into(), ..f2 };
        assert_eq!(decide(&e2, &f2, &nit_limits(3)), NIT_ROUND);
    }

    /// **An entry written before #3367 parses, and reads as no rounds run.**
    #[test]
    fn a_pre_3367_entry_reads_as_no_nonblocking_rounds_and_round_trips_them() {
        let e = entry_at(DriveState::CiWait);
        let old = serde_json::to_string(&e).unwrap();
        assert!(!old.contains("nit_rounds"), "absent is the resting shape: {old}");
        let back: DriveEntry = serde_json::from_str(&old).unwrap();
        assert_eq!(
            (back.nit_rounds, back.nit_handback, back.auto_report.as_deref()),
            (0, false, None)
        );
        let mut e = e;
        e.nit_rounds = 2;
        e.auto_report = Some("w-7 reports done".into());
        let back: DriveEntry = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back.nit_rounds, 2);
        assert_eq!(back.auto_report.as_deref(), Some("w-7 reports done"));
    }

    // ── #3367 item 5: `open_findings` and the clean case ─────────────────────

    /// A passing lane that DECLARED `open_findings` — the structured count.
    fn declared_lane(block: &str, head: &str, open: Option<u32>, summary: &str) -> LaneFact {
        let mut l = nit_lane(block, head, summary);
        if let Some(v) = l.verdict.as_mut() {
            v.open_findings = open;
        }
        l
    }

    /// **The declaration is read first, and the parser is its fallback** —
    /// one count with two sources, never two counts.
    #[test]
    fn a_declared_open_findings_count_wins_and_the_parser_is_its_fallback() {
        let both = |b, n| StatedFindings { blocking: b, non_blocking: n };
        // No declaration: exactly the parser, as before item 5.
        assert_eq!(verdict_findings("0 blocking; 2 non-blocking", None), both(Some(0), Some(2)));
        assert_eq!(verdict_findings("looks good", None), both(None, None));
        // A declaration over silent prose: a pass's total is all non-blocking.
        assert_eq!(verdict_findings("looks good", Some(2)), both(Some(0), Some(2)));
        assert_eq!(verdict_findings("looks good", Some(0)), both(Some(0), Some(0)));
        // A declaration over DISAGREEING prose: the declaration wins.
        assert_eq!(verdict_findings("0 blocking; 3 non-blocking", Some(1)), both(Some(0), Some(1)));
        // A stated blocking count inside the total is honoured, not zeroed.
        assert_eq!(verdict_findings("1 blocking", Some(3)), both(Some(1), Some(2)));
        // …and one ABOVE the total is a contradiction, which is unknown.
        assert_eq!(verdict_findings("1 blocking", Some(0)), both(None, None));
        // `declared_clean` needs the declaration itself — prose alone never.
        assert!(declared_clean("looks good", Some(0)));
        assert!(!declared_clean("0 blocking, 0 non-blocking", None), "an omission is never 0");
        assert!(!declared_clean("1 blocking", Some(0)), "the summary keeps a veto");
        assert!(!declared_clean("", Some(1)));
    }

    /// **`open_findings: 0` ends the nit loop for that lane** (item 5's
    /// interaction with item 1). Both rows reverse what the parser alone
    /// decides, which is what makes them discriminating: the declared zero
    /// beats a summary still carrying round one's `2 non-blocking`, and a
    /// declared `2` licenses the round a summary with no counts could not.
    #[test]
    fn a_declared_zero_ends_the_nit_loop_and_a_declared_count_can_start_it() {
        let sat = DriveStep::to(DriveState::Satisfied);
        let round = |lanes: Vec<LaneFact>| {
            let (e, f) = satisfied_over(lanes);
            decide(&e, &f, &nit_limits(2))
        };
        let stale_prose = "Round 1: 0 blocking; 2 non-blocking. Round 2: both fixed.";
        // The control: the parser alone loops on this summary.
        assert_eq!(round(vec![declared_lane("rev-std", "head-a", None, stale_prose)]), NIT_ROUND);
        // The declaration ends it.
        assert_eq!(round(vec![declared_lane("rev-std", "head-a", Some(0), stale_prose)]), sat);
        // One lane at 0, the other still declaring nits: the loop continues —
        // for the lane that has something open, which is the only one that can.
        assert_eq!(
            round(vec![
                declared_lane("rev-std", "head-a", Some(0), stale_prose),
                declared_lane("rev-final", "head-a", Some(1), "one nit left"),
            ]),
            NIT_ROUND
        );
        // A declared count with prose that states none: the parser alone woke.
        assert_eq!(round(vec![declared_lane("rev-std", "head-a", None, "one nit left")]), sat);
        assert_eq!(round(vec![declared_lane("rev-std", "head-a", Some(1), "one nit left")]), NIT_ROUND);
    }

    /// **A stated non-zero count vetoes a declared zero, in either class**
    /// (#3388 review round 2). "Clean" means nothing is left to disposition, so
    /// `open_findings: 0` beside `2 non-blocking` is not clean — and nor is a
    /// summary whose counts DISAGREE, which `stated_findings` reads as unknown
    /// and a veto keyed on that reading would let through. The positive rows
    /// are the control: a veto that refused everything would fail them.
    #[test]
    fn a_stated_nonzero_count_in_either_class_vetoes_a_declared_zero() {
        // Vetoed.
        for s in [
            "0 blocking; 2 non-blocking",
            "Two non-blocking notes, not routed",
            "non-blocking: 1",
            "1 blocking finding from round 1 fixed; 0 blocking now",
            "0 non-blocking now; 3 non-blocking in round 1",
        ] {
            assert!(!declared_clean(s, Some(0)), "{s:?} states an open finding: not clean");
        }
        // Clean: no count at all, or every count zero.
        for s in ["lgtm", "0 blocking, 0 non-blocking", "No blocking finding. zero non-blocking"] {
            assert!(declared_clean(s, Some(0)), "{s:?} states nothing open: clean");
        }
        // The nit loop is untouched: the declaration still wins there, so a
        // declared 0 ends the loop even where the clean flag is vetoed.
        assert_eq!(
            verdict_findings("0 blocking; 2 non-blocking", Some(0)),
            StatedFindings { blocking: Some(0), non_blocking: Some(0) }
        );
    }

    /// **The clean case is positive on every axis**: one row per thing that
    /// must hold, each flipped alone against the one base case that IS clean.
    #[test]
    fn the_clean_case_needs_every_lane_declaring_zero_at_the_head_with_ci_green() {
        let clean_lanes = || {
            vec![
                declared_lane("rev-std", "head-a", Some(0), "nothing left"),
                declared_lane("rev-final", "head-a", Some(0), "0 blocking, 0 non-blocking"),
            ]
        };
        let facts = |lanes: Vec<LaneFact>| {
            let (_, f) = satisfied_over(lanes);
            DriveFacts { ci: CiObservation::Green, ..f }
        };
        // The positive control: without it every row below could be a
        // predicate that answers false for everything.
        assert!(gate_is_clean(&facts(clean_lanes())));

        let mut rows = 0;
        let mut flip = |what: &str, f: DriveFacts| {
            assert!(!gate_is_clean(&f), "{what} must not be clean");
            rows += 1;
        };
        // A lane that omitted the field — though its prose says zero.
        let mut l = clean_lanes();
        l[1].verdict.as_mut().unwrap().open_findings = None;
        flip("an omitted declaration", facts(l));
        // A lane declaring a nit.
        let mut l = clean_lanes();
        l[0].verdict.as_mut().unwrap().open_findings = Some(1);
        flip("a declared open finding", facts(l));
        // A declaration the summary contradicts.
        let mut l = clean_lanes();
        l[0].verdict.as_mut().unwrap().summary = "1 blocking".into();
        flip("a contradicted zero", facts(l));
        // …in the NON-blocking class too (#3388 review round 2).
        let mut l = clean_lanes();
        l[0].verdict.as_mut().unwrap().summary = "0 blocking; 2 non-blocking".into();
        flip("a zero beside stated nits", facts(l));
        // A zero declared at an OLD head says nothing about this one.
        let mut l = clean_lanes();
        l[0].verdict.as_mut().unwrap().head = "head-old".into();
        flip("a stale pass", facts(l));
        // Not a pass.
        let mut l = clean_lanes();
        l[0].verdict.as_mut().unwrap().verdict = Verdict::Escalate;
        flip("an escalate", facts(l));
        // A required lane with no verdict at all.
        let mut l = clean_lanes();
        l[1].verdict = None;
        flip("an unrecorded lane", facts(l));
        // No lanes: nothing was reviewed, so nothing is clean.
        flip("an empty lane list", facts(Vec::new()));
        // CI not green.
        flip("CI pending", DriveFacts { ci: CiObservation::Pending, ..facts(clean_lanes()) });
        // The gate not satisfied.
        flip(
            "an unsatisfied gate",
            DriveFacts { gate: GateOutcome::Unsatisfied, ..facts(clean_lanes()) },
        );
        // Routing unaccountable.
        flip("unknown lanes", DriveFacts { required_lanes: None, ..facts(clean_lanes()) });
        assert_eq!(rows, 11, "every axis has its row");

        // And the clean case is not a nit round: nothing is open to hand back.
        let (e, f) = satisfied_over(clean_lanes());
        assert_eq!(decide(&e, &f, &nit_limits(3)), DriveStep::to(DriveState::Satisfied));
    }
}
