//! What a reviewer's verdict states about the findings it left open
//! (#3367): the stated counts, the clean predicates, and the driver's own
//! non-blocking round.
//!
//! Part of the review driver's pure core, split out of the former single-file
//! `reviewdrive.rs` by #3498 P7 as a pure move. The module map is in `mod.rs`;
//! the design note is `docs/design/review-driver.md`.

use super::*;

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

#[cfg(test)]
mod tests {
    use super::*;

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
