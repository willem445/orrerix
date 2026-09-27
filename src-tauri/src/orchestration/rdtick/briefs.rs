//! The text the driver sends (§5.5): the lane brief, the stop brief, the fix
//! brief and the grace clause, and the owed-notice edits (amend, the folded
//! auto-report, #3367).
//!
//! Design note: `docs/design/review-driver.md`. Part of `rdtick/`, the driver's
//! registry wiring, which `tests/reviewdrive.rs` scans whole (see `mod.rs`).

use super::*;

impl OrchRegistry {
    /// Render one lane's brief (§5.5), **sanitizing every interpolated value at
    /// this call site**.
    ///
    /// §5.5 makes that placement the difference between a pin and a decoration:
    /// "a test that sanitizes inside its own render harness asserts only that
    /// the two functions compose, and passes identically while the live call
    /// site hands `render_template` a raw job name". So [`rd_fact`] wraps every
    /// value here, and the hostile-value test calls this function.
    fn rd_lane_brief(
        &self,
        entry: &reviewdrive::DriveEntry,
        block: &str,
        brief: &RdBrief,
        limits: &reviewdrive::DriveLimits,
        verify: bool,
        scope: &str,
    ) -> String {
        let round = entry.counters.review_rounds.saturating_add(1).to_string();
        let max = limits.max_review_rounds.to_string();
        let head = rd_fact(&brief.head);
        let pr = brief.pr.to_string();
        // **The CI line states what this tick OBSERVED**, and it is rendered
        // rather than asserted because the driver had the fact in hand and the
        // templates were claiming green unconditionally.
        //
        // A lane is normally briefed out of `review-wait`, which `ci-wait`
        // reaches only on green — but arc 8 moves `fix-wait -> review-wait` on a
        // worker's `report(done)` at an unchanged head WITHOUT consulting
        // `facts.ci`, which is the "that failure was unrelated" turn. A brief
        // that told the reviewer the checks were green there was stating as fact
        // something this tick had just read as false. It cannot produce an
        // unsafe landing — `gate-check` re-evaluates `ci-green` through
        // `recheck_gate` — so it costs a misled reviewer and a wasted round,
        // which is exactly what a driven review is for saving.
        // **One paragraph, one line each.** A backslash-n plus the source
        // indent ships both into a reviewer's brief, and a `.contains` of any
        // single fragment passes straight over it because no asserted substring
        // straddles the break. Written without continuations at all so there is
        // nothing to collapse, and the SHAPE is pinned beside the content in
        // `a_lane_brief_is_one_paragraph_per_sentence`.
        let ci = match brief.ci {
            reviewdrive::CiObservation::Green => "This PR's checks are green at that head.",
            reviewdrive::CiObservation::Red => "This PR's checks are RED at that head. Review the change on its merits; the failure is the worker's to answer.",
            // **Unreachable through `decide` since #2311, and kept anyway.**
            // Arc 8 was the one route that briefed a lane on a conflicting PR;
            // mergeability is now read above the per-state logic, so that tick
            // hands the worker back instead — which is the better trade, since
            // reviewing a PR that must be rebased anyway spends a paid round on
            // a revision that will not survive. The arm stays because the match
            // is over a closed enum and a future arc could reach `review-wait`
            // without consulting mergeability again; what stops it coming back
            // to life unnoticed is `a_conflicting_pr_briefs_no_lane_at_all`,
            // which performs the counterfactual rather than describing it.
            reviewdrive::CiObservation::Conflicting => "This PR does not merge cleanly at that head. Review the change on its merits; the conflict is the worker's to answer.",
            // Pending and Unknown share one sentence on purpose: §8 says unknown
            // is never reported as a fact about the PR, and not-green-yet is the
            // only thing true of both.
            reviewdrive::CiObservation::Pending | reviewdrive::CiObservation::Unknown => "This PR's checks are not green at that head (orrerix could not read a settled result).",
        };
        // **#2168 E2's paragraph is decided by `verify` and by nothing else**
        // (#2308 review round 3, W1). It used to live inside the delta arm
        // below, under a further `rec.at_head == brief.head` — so a lane with
        // NO record, or one that had been briefed and not yet answered, was
        // stamped `briefed_verify` by `open_lane` and handed the ordinary
        // first-round brief. That path is not exotic: it is the
        // undriven-to-driven transition, which is a PR reviewed by hand and
        // then handed to `drive_review` after a body edit — the exact
        // situation #2168 is about. `rd_open_lane` renders this BEFORE
        // `open_lane` writes the record, so the record can never be the thing
        // that decides what the brief says about the grant the record is
        // about.
        //
        // Rendered once, here, and interpolated into whichever template the
        // lane's history selects: the grant and the sentence announcing it are
        // then one decision by construction rather than two that agree today.
        // `a_verification_brief_announces_itself_on_every_path_that_grants_it`
        // walks {record, no record} x {verify, not} and asserts the marker is
        // present exactly when the grant is.
        //
        // **What it asserts is a fact about the VERDICTS, not about the drive's
        // bookkeeping.** `decide_review_wait` sets `verify` only when every
        // required lane has a `pass` bound to this head, which is read from the
        // verdict files; a lane record's `at_head` is what this drive last
        // observed and can lag. Gating the sentence on the latter made the
        // announcement depend on something the claim does not rest on.
        let verification = if verify {
            // Stated as fact because it IS that precondition. The reviewer is
            // told what its pass will be taken to MEAN, because the gate
            // accepts the other lanes' passes on the strength of it — a grant
            // said out loud in the brief rather than only in a design note.
            //
            // **What it asks for is deliberately repo-neutral.** orrerix has no
            // idea what checking a body's receipts costs here, or what tool
            // does it; it points the reviewer at the contributor docs, which is
            // where a repo says (constraint 8).
            " What moved: the head has not moved and the PR body has, and every \
             required lane has already passed the code at this head. This is a \
             VERIFICATION-ONLY round: read the body as it stands, not the diff. \
             Check what it asserts against the tree at this head — figures, run \
             ids, SHAs, quoted passages — plus whatever this repo's contributor \
             docs tell a reviewer to run over a PR body. Your pass records that \
             the body as it stands is sound, and the merge gate accepts the other \
             lanes' passes on the strength of it instead of re-briefing them, so \
             a body defect you wave through is one nobody else will look at. A \
             finding about the code is still in scope if you see one; you are not \
             being asked to go looking for one."
        } else {
            ""
        };
        match entry.lane(block).filter(|l| !l.at_head.is_empty()) {
            // A lane that has answered before gets the delta — the line an
            // orchestrator typed by hand nine times on one PR.
            Some(rec) => {
                let prev_head = rd_fact(&rec.at_head);
                let prev =
                    rec.last_verdict.map(|v| v.as_str()).unwrap_or("unrecorded").to_string();
                let digest_state = if rec.briefed_digest.is_empty() || brief.body_digest.is_empty()
                {
                    // "Cannot tell" is not "changed" — the asymmetry
                    // `ReviewVerdict::body_changed` encodes.
                    "of unknown drift (orrerix could not compare the two digests)"
                } else if rec.briefed_digest == brief.body_digest {
                    "unchanged"
                } else {
                    "changed"
                };
                // **The mode is `scope`'s, not re-derived here** (#2508 review,
                // rev-std finding 2): `rd_lane_scope` already classified this
                // round from the same `(verify, rec.at_head, brief.head)` facts,
                // so the arm that picks the WHAT_MOVED text reads that line
                // instead of re-deriving `rec.at_head == brief.head` beside it —
                // two parallel matches over one predicate were one edit away
                // from a scope line contradicting the brief it rides on. In
                // this arm `scope` is `delta since …` or `body-only` by
                // construction (`whole-diff` implies no lane record, and the
                // `None` arm below renders the first-call template, which has
                // no WHAT_MOVED slot); the final `else` is the unchanged-head
                // body-only round, and the scope-mode test pins each mode to
                // its text so a future drift between the two reads goes red.
                let moved = if verify {
                    // The grant outranks the record: see the block above the
                    // `match`. `verification` opens with a space so it reads as
                    // an appended clause in the other arm; this slot wants it
                    // without one.
                    verification.trim_start().to_string()
                } else if scope.starts_with("scope: delta since ") {
                    // **What this brief does NOT claim.** orrerix does not
                    // compute the per-round delta: the driver's seam is
                    // `gh`-only by construction (§3.1 item 1, made structural in
                    // `RdRunner`), so it has no `git diff` to run. It names the
                    // two revisions and points at the command that answers the
                    // question exactly — facts it read plus an instruction,
                    // rather than a delta it invented.
                    format!(
                        "What moved: the head moved from {prev_head} to {head}. orrerix does not \
                         compute the per-round delta; `git diff {prev_head}..{head}` in your \
                         worktree does."
                    )
                } else {
                    // **What this half says changed at #2168 E1.** Until then
                    // the commonest cause of a body-only re-brief was the
                    // worker pasting its CI receipts after the checks settled —
                    // #1875's class, one re-record round on every code PR of
                    // that session — and a reviewer's right move was to skim.
                    // `decide_ci_wait` no longer briefs a lane at a head its
                    // worker pushed until that worker has reported the fix
                    // finished, so what reaches here after a hand-back is an
                    // edit somebody made on purpose, and the right move is to
                    // read it.
                    //
                    // **Scoped to "after a hand-back", which is the whole of
                    // what is provable here.** E1 gates the `ci-wait` arc on
                    // arc 7, so the claim holds for every revision this drive
                    // handed back; it does NOT hold for the drive's first pass
                    // over a head it never handed back, where the receipts race
                    // is unchanged. A flat sentence would be the wider claim,
                    // and this brief lands in a reviewer's pane as fact.
                    "What moved: the head has not moved and the PR body has. Re-read the \
                     body, not the diff — the body is what a squash merge commits, so text \
                     that moved there is text nobody has passed. After a hand-back the \
                     driver waits for the worker's report(done) before opening this lane, \
                     so a body move you see following one is a deliberate edit rather than \
                     CI receipts landing late."
                        .to_string()
                };
                render_template(
                    DRIVER_DELTA_TPL,
                    &[
                        ("PR", &pr),
                        ("HEAD", &head),
                        ("PREV_VERDICT", &rd_fact(&prev)),
                        ("PREV_HEAD", &prev_head),
                        ("PREV_DIGEST_STATE", digest_state),
                        ("WHAT_MOVED", &moved),
                        ("CI", ci),
                        ("ROUND", &round),
                        ("MAX_ROUNDS", &max),
                        ("SCOPE", scope),
                    ],
                )
            }
            None => {
                let prior: Vec<String> = brief
                    .lane_notices
                    .iter()
                    .take_while(|l| l.block != block)
                    .map(|l| {
                        format!(
                            "{} recorded {}",
                            rd_fact(&l.block),
                            l.verdict.as_str().to_uppercase()
                        )
                    })
                    .collect();
                let prior = if prior.is_empty() {
                    String::new()
                } else {
                    // How a final lane learns it is validating a review as well
                    // as the work, with no block name anywhere in the code —
                    // §4's sequenced-lane rule expressed as an ordered list.
                    format!(" Lanes before yours at this revision: {}.", prior.join("; "))
                };
                render_template(
                    DRIVER_REVIEW_TPL,
                    &[
                        ("PR", &pr),
                        ("HEAD", &head),
                        ("BASE", &rd_fact(&brief.base)),
                        ("LANES", &rd_fact(&brief.required.join(", "))),
                        ("LANE", &rd_fact(block)),
                        ("CI", ci),
                        ("ROUND", &round),
                        ("MAX_ROUNDS", &max),
                        ("PRIOR_LANES", &prior),
                        // #2308 W1: the arm that used to grant in silence. A
                        // lane with no record — the undriven-to-driven
                        // transition — is briefed HERE, and before this it was
                        // stamped `briefed_verify` without ever being told.
                        ("VERIFICATION", verification),
                        ("SCOPE", scope),
                    ],
                )
            }
        }
    }

    /// Replace `from` with `to` in the notice PR `pr`'s entry still OWES
    /// (#3367 item 5) — the clean enqueue's answer, learned after the notice was
    /// owed, replacing the clause that promised it. A text that no longer
    /// carries `from` gets `to` appended, so the answer is never lost to a
    /// wording mismatch.
    ///
    /// Its own short critical section, re-reading the file, for the flush's
    /// reason: the tick's lock has been released, so the entry is read as it
    /// now is. Nothing owed (a concurrent flush already delivered it, or the
    /// entry is gone) changes nothing — the `rd-clean` row still carries the
    /// answer — and a failed read or write is the same: the notice goes out
    /// with the clause that promised the submission, which names
    /// `merge_queue_status()` and was true when written and when read.
    fn rd_amend_owed_notice(&self, dir: &std::path::Path, pr: u64, from: &str, to: &str) {
        let _state_guard = self.rd_state_lock.lock_safe();
        let Ok(mut state) = reviewdrive::load_state(dir) else { return };
        let Some(n) = state.entry_mut(pr).and_then(|e| e.owed_notice.as_mut()) else { return };
        if n.text.contains(from) {
            n.text = n.text.replacen(from, to, 1);
        } else {
            n.text.push_str(to);
        }
        let _ = reviewdrive::store_state(dir, &state);
    }

    /// **The drive's FIRST notice carries the report that started it** (#3367
    /// item 2). `Option::take`, so exactly one notice carries it and every
    /// later one reads as it always did. A drive nobody auto-started has
    /// nothing to take, and its notice is returned unchanged.
    fn rd_fold_auto_report(entry: &mut reviewdrive::DriveEntry, notice: String) -> String {
        let report = entry.auto_report.take();
        Self::rd_fold_text(notice, report.as_deref())
    }

    /// [`rd_fold_auto_report`](Self::rd_fold_auto_report)'s text, without the
    /// take — for the one caller that must not consume the report until its
    /// line is known to have landed (a hold, delivered directly).
    fn rd_fold_text(notice: String, report: Option<&str>) -> String {
        match report {
            Some(r) => format!(
                "{notice} This drive was started by a worker's report(done), delivered here \
                 instead of on its own: {r}"
            ),
            None => notice,
        }
    }

    /// Clear the `auto_report` a DELIVERED hold notice carried (#3367 round-3
    /// residual) — see [`RdOut::report_folded`]. Re-reads under the lock, as
    /// [`rd_amend_owed_notice`](Self::rd_amend_owed_notice) does; a failed
    /// read or write leaves the report on the entry, which costs a repeat on
    /// the next notice and never a loss.
    fn rd_clear_auto_report(&self, dir: &std::path::Path, pr: u64) {
        let _state_guard = self.rd_state_lock.lock_safe();
        let Ok(mut state) = reviewdrive::load_state(dir) else { return };
        let Some(e) = state.entry_mut(pr) else { return };
        if e.auto_report.take().is_some() {
            let _ = reviewdrive::store_state(dir, &state);
        }
    }

    /// The sentence #2509's grace round owes the worker it hands back to.
    ///
    /// **Because the `{{ATTEMPT}}` numbers cannot say it.** They are
    /// `review_rounds` of `max_review_rounds`, and a grace round is the one
    /// hand-back where `review_rounds` has already stopped at the bound — so
    /// the brief would otherwise read "attempt 3 of 3" for the second time in
    /// a row, with no account of why there is a second one. A worker that
    /// cannot tell a grace from a bug in the counter has been told something
    /// false by omission.
    ///
    /// It is also where the worker learns the bound is now REALLY spent: the
    /// next blocking fail parks the drive whatever it is about.
    fn grace_clause(grace: bool) -> &'static str {
        if grace {
            " This is a GRACE round PAST the review bound: the last blocking \
             fail came back on a lane that had been re-briefed about the PR \
             body alone, at a head that had not moved. The attempt count below \
             still reads at the bound, and that is not a mistake — a grace \
             round is not a review round. It is granted once per drive and is \
             now spent, so the next blocking fail parks the drive whatever it \
             is about."
        } else {
            ""
        }
    }

    /// **What a busy reviewer lane is told when its PR stops merging** (#3176).
    ///
    /// One paragraph on one source line, for [`Self::rd_lane_brief`]'s reason: a
    /// newline plus the source indent ships both into a delegate's pane, and a
    /// `.contains` of any single fragment steps straight over it. The shape is
    /// pinned beside the content, as the lane briefs' is.
    ///
    /// **It deliberately does NOT reuse `rd_lane_brief`'s CONFLICTING arm.**
    /// That sentence ends *"Review the change on its merits; the conflict is the
    /// worker's to answer"* — an instruction to CARRY ON, which is the opposite
    /// of this one, and gluing a stop onto it would hand a reviewer a paragraph
    /// that contradicts itself. What the two share is the FACT, not the wording.
    ///
    /// It asks for a report, and that is the mechanism rather than politeness: a
    /// reviewer's report is what stamps `idle_since_ms`, which is what
    /// `release_driven_pane`'s barrier requires — so this line is what makes the
    /// pane releasable at all. The release then happens on an ordinary later
    /// tick through [`reviewdrive::ReleaseReason::Conflict`], with no second
    /// mechanism and no second decision.
    ///
    /// And it says the conversation survives, because it does: the next round
    /// resumes this same session against the rebased head, so a reviewer that
    /// drops what it is holding loses nothing it will not be asked for again.
    fn rd_lane_stop_brief(&self, brief: &RdBrief) -> String {
        format!(
            "STOP this review — orrerix is standing it down. PR #{} does not merge cleanly against {} at the head you were briefed on ({}), so that head is about to be rebased away and any verdict recorded against it goes stale the moment the worker pushes. Do not finish the review, do not record a verdict for this head, and do not report findings: call report with outcome done, and stop there. Nothing is lost — orrerix briefs you again against the rebased head, in this same conversation, and no review round is charged for this one.",
            brief.pr,
            rd_fact(&brief.base),
            rd_fact(&brief.head),
        )
    }

    /// Render the worker's hand-back brief (§5.5).
    ///
    /// `{{WHAT}}` is **loomux-authored text chosen from a closed set of three**,
    /// with facts orrerix read interpolated into it — never delegate- or
    /// repo-authored prose (§3.1 item 4). The three are the three ways a PR
    /// comes back: a lane's findings, a red run, and a conflict.
    fn rd_fix_brief(
        &self,
        entry: &reviewdrive::DriveEntry,
        brief: &RdBrief,
        limits: &reviewdrive::DriveLimits,
        grace: bool,
    ) -> String {
        let base = rd_fact(&brief.base);
        // #3367 item 1: a non-blocking round is not a FAIL, and the brief must
        // not say one was recorded. Read off the ENTRY rather than the step, so
        // the brief a restart re-sends (`Rehandback`) says the same thing the
        // first one did. The attempt figures are the SHARED bound's, because
        // that is the budget this round spent.
        if entry.nit_handback {
            let what = format!(
                "Review: request-changes. Every required lane PASSED, with non-blocking \
                 findings open ({}). The findings are on PR #{}. Address all of them, or \
                 answer on the PR why one is not a defect, then push. This is non-blocking \
                 round {} of {} that the driver runs on its own, and it counts toward the \
                 review bound below.",
                rd_fact(&rddrive::residual_text(&brief.lane_notices)),
                brief.pr,
                entry.nit_rounds,
                limits.fix_nonblocking_rounds,
            );
            return render_template(
                DRIVER_FIX_TPL,
                &[
                    ("PR", &brief.pr.to_string()),
                    ("HEAD", &rd_fact(&brief.head)),
                    ("BASE", &base),
                    ("WHAT", &what),
                    ("ATTEMPT", &entry.counters.review_rounds.to_string()),
                    ("MAX_ATTEMPTS", &limits.max_review_rounds.to_string()),
                ],
            );
        }
        let (what, attempt, max) = match brief.ci {
            reviewdrive::CiObservation::Conflicting => (
                format!(
                    "It is CONFLICTING against {base}. Rebase onto origin/{base}, resolve, and \
                     push."
                ),
                entry.counters.rebase_attempts,
                limits.max_rebase_attempts,
            ),
            reviewdrive::CiObservation::Red => (
                format!(
                    "CI is red at that head. Failing checks: {}. Read them with `gh pr checks \
                     {}`, fix, and push.",
                    rd_fact(&brief.failing_jobs.join(", ")),
                    brief.pr
                ),
                entry.counters.ci_attempts,
                limits.max_ci_attempts,
            ),
            _ => (
                format!(
                    "Review requested changes: {} recorded FAIL. The findings are on the PR. \
                     Address all of them, or answer on the PR why one is not a defect, then \
                     push.",
                    // The DECIDING lane, which is the one `decide_review_wait`
                    // actually acted on — not "the first lane with a `fail`".
                    // The two agree today, because the deciding lane is the
                    // first whose pass does not stand and a `fail` is what put
                    // it there. They agree by an argument rather than by
                    // construction, and naming the wrong lane in a hand-back
                    // sends a worker to the wrong review.
                    rd_fact(
                        &brief
                            .deciding_lane
                            .clone()
                            .unwrap_or_else(|| brief.failing_lane())
                    )
                ) + Self::grace_clause(grace),
                entry.counters.review_rounds,
                limits.max_review_rounds,
            ),
        };
        render_template(
            DRIVER_FIX_TPL,
            &[
                ("PR", &brief.pr.to_string()),
                ("HEAD", &rd_fact(&brief.head)),
                ("BASE", &base),
                ("WHAT", &what),
                ("ATTEMPT", &attempt.to_string()),
                ("MAX_ATTEMPTS", &max.to_string()),
            ],
        )
    }
}
