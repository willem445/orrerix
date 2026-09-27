//! Verdicts and the merge gate, the capacity recommendation, gate satisfiability, and path-based routing.
//!
//! One module of the `workflow` integration-test target (`main.rs`),
//! split out of the former single-file `tests/workflow.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ───────── verdicts + the merge gate: the pure semantics (#222 / #197) ─────────
//
// The gate decision is pure (`evaluate_merge_gate`) so it can be pinned here in
// microseconds, and so the `gh` shim's shell mirror has a spec to agree with. The
// shell itself is executed end-to-end in tests/orchestration/.

pub(crate) fn gate(require: GateRequire, reviewers: &[&str], also: &[&str]) -> workflow::Gate {
    workflow::Gate {
        require,
        reviewers: reviewers.iter().map(|s| s.to_string()).collect(),
        also: also.iter().map(|s| s.to_string()).collect(),
        max_diff_lines: None,
        routing: Vec::new(),
    }
}

/// The revision every verdict below reviewed, unless it says otherwise.
const HEAD: &str = "a3f9c21";
/// The PR moved: the worker pushed after the reviews came in.
const NEW_HEAD: &str = "e1c4861";

/// Verdict records keyed by block. `(block, verdict, head-it-reviewed)`.
fn verdicts(
    pairs: &[(&str, workflow::Verdict, &str)],
) -> std::collections::BTreeMap<String, workflow::ReviewVerdict> {
    pairs
        .iter()
        .map(|(b, v, head)| {
            (
                b.to_string(),
                workflow::ReviewVerdict {
                    pr: 7,
                    block: b.to_string(),
                    agent_id: "rev-1".into(),
                    verdict: *v,
                    head: head.to_string(),
                    body_digest: String::new(),
                    verified_body: false,
                    open_findings: None,
                    summary: "…".into(),
                    ts_ms: 1,
                },
            )
        })
        .collect()
}

/// `evaluate_merge_gate` against the current head, which is `HEAD` unless a test
/// is exercising a re-push.
fn eval(
    g: &workflow::Gate,
    v: &std::collections::BTreeMap<String, workflow::ReviewVerdict>,
) -> workflow::GateOutcome {
    workflow::evaluate_merge_gate(g, v, Some(HEAD))
}

#[test]
fn all_pass_gate_refuses_while_one_named_verdict_is_outstanding() {
    // THE test. This is the #151 bug that produced #197: a PR merged on the FIRST
    // reviewer's approve while a second, dedicated review was still running — and
    // that second review found a real release-gate bypass (#196). One reviewer
    // still silent must mean the gate stays shut, however loudly the other approved.
    use workflow::{GateOutcome, Verdict};
    let g = gate(GateRequire::AllPass, &["rev-security", "rev-tests"], &[]);

    let one_in = eval(&g, &verdicts(&[("rev-security", Verdict::Pass, HEAD)]));
    assert_eq!(
        one_in,
        GateOutcome::Short {
            passes: 1,
            need: 2,
            outstanding: vec!["rev-tests".into()],
            stale: vec![]
        },
        "one reviewer's pass must NOT satisfy an all-pass gate while the other is still reviewing"
    );
    assert!(!one_in.satisfied());

    // Both in: satisfied — and only then.
    assert!(eval(
        &g,
        &verdicts(&[("rev-security", Verdict::Pass, HEAD), ("rev-tests", Verdict::Pass, HEAD)])
    )
    .satisfied());

    // Nobody in at all: shut, and it names who it is waiting for.
    match eval(&g, &verdicts(&[])) {
        GateOutcome::Short { passes: 0, need: 2, outstanding, .. } => {
            assert_eq!(outstanding, vec!["rev-security", "rev-tests"])
        }
        other => panic!("an empty verdict set must not satisfy a gate: {other:?}"),
    }

    // A verdict from a reviewer the gate does NOT name satisfies nothing — the gate
    // reads the *dispatched* reviewers, not the first approve that turns up (#197 A.2).
    assert!(!eval(&g, &verdicts(&[("rev-perf", Verdict::Pass, HEAD)])).satisfied());
}

#[test]
fn a_pass_does_not_survive_a_re_push() {
    // A verdict binds to a COMMIT, not to a PR number. Without that, the gate goes
    // green over code nobody reviewed: both reviewers pass #7, the worker pushes
    // "fixed lint" + "one more edge case", and the merge proceeds — satisfied to the
    // letter of #197 and violated in its spirit. GitHub's own review model dismisses
    // stale approvals for the same reason.
    use workflow::{GateOutcome, Verdict};
    let g = gate(GateRequire::AllPass, &["rev-security", "rev-tests"], &[]);
    let both_passed = verdicts(&[
        ("rev-security", Verdict::Pass, HEAD),
        ("rev-tests", Verdict::Pass, HEAD),
    ]);
    assert!(eval(&g, &both_passed).satisfied(), "as reviewed, the gate is satisfied");

    // The branch moves under them.
    assert_eq!(
        workflow::evaluate_merge_gate(&g, &both_passed, Some(NEW_HEAD)),
        GateOutcome::Short {
            passes: 0,
            need: 2,
            outstanding: vec![],
            stale: vec!["rev-security".into(), "rev-tests".into()],
        },
        "a pass reviewed at an earlier head must count as stale, not as a pass"
    );

    // Re-reviewing the new head clears it — one reviewer at a time.
    let refreshed = verdicts(&[
        ("rev-security", Verdict::Pass, NEW_HEAD),
        ("rev-tests", Verdict::Pass, HEAD),
    ]);
    match workflow::evaluate_merge_gate(&g, &refreshed, Some(NEW_HEAD)) {
        GateOutcome::Short { passes: 1, stale, .. } => assert_eq!(stale, vec!["rev-tests"]),
        other => panic!("one refreshed pass is not two: {other:?}"),
    }

    // A verdict loomux could not bind to a commit (empty head — gh unavailable at
    // record time) is stale too: it can never equal a real head. Fail closed.
    assert!(!eval(&g, &verdicts(&[
        ("rev-security", Verdict::Pass, ""),
        ("rev-tests", Verdict::Pass, ""),
    ]))
    .satisfied(), "an unbound verdict must not open a gate");

    // And if the head itself can't be resolved, there is no way to know what any
    // pass covers — refuse, rather than fall back to 'a pass is a pass'.
    assert_eq!(
        workflow::evaluate_merge_gate(&g, &both_passed, None),
        GateOutcome::UnknownRevision
    );

    // A BLOCKING verdict is revision-independent: "this PR has a defect" doesn't
    // stop being true because the author pushed more code. It still blocks.
    let stale_fail = verdicts(&[
        ("rev-security", Verdict::Pass, NEW_HEAD),
        ("rev-tests", Verdict::Fail, HEAD),
    ]);
    assert_eq!(
        workflow::evaluate_merge_gate(&g, &stale_fail, Some(NEW_HEAD)),
        GateOutcome::Blocked { blocking: vec!["rev-tests".into()] },
        "a fail against an older revision still refuses the merge until it is re-reviewed"
    );
}

#[test]
fn a_blocking_verdict_beats_any_number_of_passes() {
    // #197 A.3: "blockers beat approvals — first-to-report must never win." A fail
    // (and an escalate, which is a refusal to decide, not an approval) refuses the
    // merge whatever the others recorded and whatever the threshold says.
    use workflow::{GateOutcome, Verdict};
    for blocker in [Verdict::Fail, Verdict::Escalate] {
        assert!(blocker.is_blocking(), "{blocker:?} must refuse a merge");
        // Even against a threshold the passes already meet.
        let g = gate(GateRequire::Threshold(2), &["a", "b", "c"], &[]);
        let out = eval(
            &g,
            &verdicts(&[("a", Verdict::Pass, HEAD), ("b", Verdict::Pass, HEAD), ("c", blocker, HEAD)]),
        );
        assert_eq!(
            out,
            GateOutcome::Blocked { blocking: vec!["c".into()] },
            "two passes must not outvote a {blocker:?} — a disagreement resolves to do-not-merge"
        );
    }
}

#[test]
fn threshold_gate_needs_n_passes_and_all_pass_needs_everyone() {
    use workflow::{GateOutcome, Verdict};
    let g = gate(GateRequire::Threshold(2), &["a", "b", "c"], &[]);
    assert_eq!(workflow::gate_need(&g), 2);

    // One pass is short, and the outcome names who is still to report.
    assert_eq!(
        eval(&g, &verdicts(&[("a", Verdict::Pass, HEAD)])),
        GateOutcome::Short {
            passes: 1,
            need: 2,
            outstanding: vec!["b".into(), "c".into()],
            stale: vec![]
        }
    );
    // Two passes satisfy it: `threshold: 2` over three reviewers is the author
    // saying, in the file, that two are enough — it does not wait for the third.
    // (`all-pass`, the default, is the one that waits for everybody — above.)
    assert!(eval(&g, &verdicts(&[("a", Verdict::Pass, HEAD), ("b", Verdict::Pass, HEAD)]))
        .satisfied());
    // …but they must be passes for the code that would actually merge.
    assert!(!eval(&g, &verdicts(&[("a", Verdict::Pass, HEAD), ("b", Verdict::Pass, "0ldc0de")]))
        .satisfied(), "a threshold cannot be met with a stale pass");

    // The same two verdicts against an all-pass gate over the same three: still shut.
    let strict = gate(GateRequire::AllPass, &["a", "b", "c"], &[]);
    assert_eq!(workflow::gate_need(&strict), 3);
    assert!(!eval(&strict, &verdicts(&[("a", Verdict::Pass, HEAD), ("b", Verdict::Pass, HEAD)]))
        .satisfied());
}

// ───────── #255: max_agents recommendation, derived from roster + gate ─────────
//
// `recommend_capacity` is pure — pinned here, the same way `gate_need` and
// `evaluate_merge_gate` are above it. The wiring that records it in the
// `workflow-loaded` audit and warns below the minimum is exercised end to end
// in tests/orchestration/.

pub(crate) fn block(id: &str, kind: Role) -> workflow::Block {
    workflow::Block {
        id: id.into(),
        name: id.into(),
        kind,
        cli: String::new(),
        model: String::new(),
        prompt: None,
        profile: None,
        allow: vec![],
        role_hint: None,
        effort: String::new(),
        context: String::new(),
        remote: None,
        driver: None,
        cache_ttl_minutes: None,
    }
}

#[test]
fn capacity_minimum_is_gate_aware_not_just_a_reviewer_count() {
    // The same 5 reviewer blocks under two different gates: #255 explicitly asks
    // for the minimum to come from the GATE, not the block list — `threshold: 2`
    // needs far less live-at-once capacity than `all-pass` over the same five.
    let blocks = vec![
        block("worker", Role::Worker),
        block("rev-1", Role::Reviewer),
        block("rev-2", Role::Reviewer),
        block("rev-3", Role::Reviewer),
        block("rev-4", Role::Reviewer),
        block("rev-5", Role::Reviewer),
    ];
    let reviewers = ["rev-1", "rev-2", "rev-3", "rev-4", "rev-5"];

    let threshold = gate(GateRequire::Threshold(2), &reviewers, &[]);
    let rec = workflow::recommend_capacity(&blocks, Some(&threshold));
    assert_eq!(rec.minimum, 3, "threshold: 2 + 1 worker");
    assert_eq!(rec.recommended, 6, "1 worker + 5 reviewers, no planner block");
    assert_eq!(rec.reviewers_needed, 2, "the gate's own requirement, not the 5 declared reviewer blocks");

    let all_pass = gate(GateRequire::AllPass, &reviewers, &[]);
    let rec = workflow::recommend_capacity(&blocks, Some(&all_pass));
    assert_eq!(
        rec.minimum, 6,
        "all-pass over the same five reviewers needs every one of them live at once"
    );
    assert_eq!(rec.recommended, 6, "recommended follows the roster, not the gate — unchanged");
    assert_eq!(rec.reviewers_needed, 5);
}

#[test]
fn capacity_reviewers_needed_is_what_a_caller_must_describe_the_minimum_with() {
    // rev-1 B1 of the #255 review: a caller describing `minimum` must read
    // `reviewers_needed`, never recount reviewer BLOCKS — a threshold gate over
    // a subset makes those two numbers genuinely different, and reading the
    // wrong one is exactly the bug that shipped ("needs 5 reviewers + a worker
    // (minimum 3 live agents)" — 5 + 1 != 3).
    let blocks = vec![
        block("worker", Role::Worker),
        block("rev-1", Role::Reviewer),
        block("rev-2", Role::Reviewer),
        block("rev-3", Role::Reviewer),
        block("rev-4", Role::Reviewer),
        block("rev-5", Role::Reviewer),
    ];
    // The gate names only 2 of the 5 declared reviewer blocks.
    let g = gate(GateRequire::Threshold(2), &["rev-1", "rev-2"], &[]);
    let rec = workflow::recommend_capacity(&blocks, Some(&g));
    assert_eq!(rec.reviewers_needed, 2, "the gate's requirement, over the gate's own reviewers");
    assert_eq!(rec.minimum, 3, "2 (reviewers_needed) + 1 worker — NOT 5 (reviewer blocks) + 1");
    assert_eq!(rec.recommended, 6, "recommended still counts every declared reviewer block");
}

#[test]
fn capacity_recommended_counts_every_declared_tier_never_the_orchestrator() {
    // The #255 incident roster: orchestrator, planner, 2 worker tiers, 3
    // reviewers, all-pass. minimum (3 reviewers + 1 worker = 4) is exactly the
    // cap that thrashed for two hours — because recommended (every tier live at
    // once) is 6, not 4. This is the gap the feature exists to surface.
    let blocks = vec![
        block("orchestrator", Role::Orchestrator),
        block("planner", Role::Planner),
        block("worker-deep", Role::Worker),
        block("worker-quick", Role::Worker),
        block("rev-1", Role::Reviewer),
        block("rev-2", Role::Reviewer),
        block("rev-3", Role::Reviewer),
    ];
    let g = gate(GateRequire::AllPass, &["rev-1", "rev-2", "rev-3"], &[]);
    let rec = workflow::recommend_capacity(&blocks, Some(&g));
    assert_eq!(rec.minimum, 4);
    assert_eq!(
        rec.recommended, 6,
        "2 workers + 3 reviewers + 1 planner — the orchestrator is exempt from the cap and never counted"
    );
}

#[test]
fn capacity_with_no_declared_gate_falls_back_to_every_reviewer_block() {
    let blocks =
        vec![block("worker", Role::Worker), block("rev-1", Role::Reviewer), block("rev-2", Role::Reviewer)];
    let rec = workflow::recommend_capacity(&blocks, None);
    assert_eq!(rec.minimum, 3, "no gate to narrow the requirement: every reviewer, plus a worker");
    assert_eq!(rec.recommended, 3);
}

#[test]
fn capacity_with_no_worker_block_needs_no_worker_slot() {
    // A review-only workflow (no worker block at all) must not have a phantom
    // +1 forced into its minimum — there is nothing for that slot to run.
    let blocks = vec![block("rev-1", Role::Reviewer), block("rev-2", Role::Reviewer)];
    let g = gate(GateRequire::AllPass, &["rev-1", "rev-2"], &[]);
    let rec = workflow::recommend_capacity(&blocks, Some(&g));
    assert_eq!(rec.minimum, 2, "no worker block declared — nothing to add the +1 slot for");
    assert_eq!(rec.recommended, 2);
}

#[test]
fn extra_tiers_names_exactly_what_recommended_adds_over_minimum() {
    // The #255 incident roster again: minimum (4) budgets 1 worker + the 3
    // gated reviewers; recommended (6) adds the second worker tier and the
    // planner. Those two are exactly what a cap sitting between the two can
    // never keep live alongside a review round.
    let blocks = vec![
        block("orchestrator", Role::Orchestrator),
        block("planner", Role::Planner),
        block("worker-deep", Role::Worker),
        block("worker-quick", Role::Worker),
        block("rev-1", Role::Reviewer),
        block("rev-2", Role::Reviewer),
        block("rev-3", Role::Reviewer),
    ];
    let g = gate(GateRequire::AllPass, &["rev-1", "rev-2", "rev-3"], &[]);
    let rec = workflow::recommend_capacity(&blocks, Some(&g));
    let extras = workflow::extra_tiers(&blocks, rec.reviewers_needed);
    assert_eq!(extras, vec!["1 more worker tier".to_string(), "the planner".to_string()]);

    // An all-pass gate naming only a SUBSET of the declared reviewer blocks:
    // the ones outside the gate are "extra" too, exactly like an extra worker
    // tier — they still cannot merge-gate anything, but the roster budgets a
    // slot for them.
    let blocks2 = vec![
        block("worker", Role::Worker),
        block("rev-1", Role::Reviewer),
        block("rev-2", Role::Reviewer),
        block("rev-3", Role::Reviewer),
    ];
    let g2 = gate(GateRequire::AllPass, &["rev-1", "rev-2"], &[]);
    let rec2 = workflow::recommend_capacity(&blocks2, Some(&g2));
    assert_eq!(workflow::extra_tiers(&blocks2, rec2.reviewers_needed), vec!["1 more reviewer".to_string()]);

    // Exactly at the minimum (no planner, no second worker tier, gate needs
    // every declared reviewer): nothing is left over to name.
    let tight = vec![
        block("worker", Role::Worker),
        block("rev-1", Role::Reviewer),
        block("rev-2", Role::Reviewer),
    ];
    let g3 = gate(GateRequire::AllPass, &["rev-1", "rev-2"], &[]);
    let rec3 = workflow::recommend_capacity(&tight, Some(&g3));
    assert_eq!(rec3.minimum, rec3.recommended, "nothing beyond the minimum was declared");
    assert!(workflow::extra_tiers(&tight, rec3.reviewers_needed).is_empty());
}

#[test]
fn join_with_and_reads_like_english_at_every_list_length() {
    let s = |v: &[&str]| workflow::join_with_and(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(s(&[]), "");
    assert_eq!(s(&["the planner"]), "the planner");
    assert_eq!(s(&["the planner", "1 more worker tier"]), "the planner and 1 more worker tier");
    assert_eq!(
        s(&["the planner", "1 more worker tier", "2 more reviewers"]),
        "the planner, 1 more worker tier, and 2 more reviewers"
    );
}

#[test]
fn a_verdict_is_never_guessed_and_an_unreadable_one_is_not_a_pass() {
    use workflow::Verdict;
    assert_eq!(Verdict::parse("pass"), Some(Verdict::Pass));
    assert_eq!(Verdict::parse(" escalate \n"), Some(Verdict::Escalate), "trailing newline is file format, not content");
    // LOWERCASE-STRICT, and that is the whole point: the shim's `case "$v" in pass)`
    // is a shell case and cannot be case-insensitive, so if THIS half lowercased, a
    // hand-edited `PASS` would read as satisfied to the orchestrator while the shim
    // refused the merge — the two halves of one gate disagreeing about what a verdict
    // is. One token definition; both sides fail closed on anything else.
    for junk in ["PASS", "Pass", "approve", "lgtm", "yes", "true", "", "pass!", "ok"] {
        assert_eq!(Verdict::parse(junk), None, "{junk:?} must not parse as a verdict");
    }
    // So a verdict file whose first line isn't exactly a verdict word reads as *no
    // verdict*, which an all-pass gate treats as outstanding — never as a pass.
    assert!(workflow::parse_verdict_file(7, "rev-a", "PASS\na3f9c21\n1\nrev-1\nlgtm\n").is_none());
    assert!(workflow::parse_verdict_file(7, "rev-a", "").is_none());
}

#[test]
fn verdict_file_round_trips_with_its_attribution() {
    // The record is durable and ATTRIBUTED: which block recorded it, which agent
    // instance that was, when, and why. That is what makes it state rather than a
    // notification — #197's whole complaint about `report()`.
    let rec = workflow::ReviewVerdict {
        pr: 151,
        block: "rev-security".into(),
        agent_id: "rev-4".into(),
        verdict: workflow::Verdict::Fail,
        head: "a3f9c21".into(),
        body_digest: workflow::body_digest("## What\n\nA fix.\n"),
        verified_body: false,
        open_findings: None,
        summary: "release-gate bypass:\n  gh api can create a v* tag ref".into(),
        ts_ms: 1_720_000_000_000,
    };
    let text = workflow::verdict_file_text(&rec);
    assert!(
        text.starts_with("fail\na3f9c21\n"),
        "the verdict word is line 1 and the reviewed head line 2 — that IS the shim's read"
    );
    assert_eq!(
        text.lines().nth(4),
        Some(rec.body_digest.as_str()),
        "and the reviewed body's digest is line 5 — the shim's `head -n5 | tail -n1` (#565)"
    );
    let back = workflow::parse_verdict_file(151, "rev-security", &text).unwrap();
    assert_eq!(back, rec, "the record must survive the round trip, multi-line summary and all");

    // A head that isn't a commit id is stored EMPTY, and an empty head never equals
    // a real one — so it reads as stale rather than as "unbound, therefore fine".
    assert_eq!(workflow::sanitize_sha("not a sha; rm -rf /"), "");
    assert_eq!(workflow::sanitize_sha("  A3F9C21\n"), "a3f9c21", "normalized, so the shim's `case` compare agrees");
    assert!(!rec.reviewed(""), "an empty current head matches nothing");
    assert!(!workflow::ReviewVerdict { head: String::new(), ..rec.clone() }.reviewed("a3f9c21"),
        "an unbound verdict has reviewed no revision");
    assert!(rec.reviewed("a3f9c21"));

    // A control character in a summary would ride straight into a pane; newlines and
    // tabs are prose and survive.
    assert_eq!(workflow::sanitize_summary("bad\u{1b}[31m\tred\nline"), "bad[31m\tred\nline");
    assert_eq!(
        workflow::sanitize_summary(&"x".repeat(9000)).chars().count(),
        workflow::MAX_SUMMARY_CHARS
    );
}

#[test]
fn a_body_verification_mark_rides_line_5_and_a_reviewer_cannot_type_one() {
    // #2168 E2. The mark is what lets `body-unchanged` accept the passes this
    // verdict supersedes, so where it LIVES is the security property and not a
    // formatting choice: line 5 is the tool's, line 6 and below is the summary,
    // which is the one field a reviewer writes.
    let digest = workflow::body_digest("## What\n\nA fix.\n");
    let base = workflow::ReviewVerdict {
        pr: 2168,
        block: "rev-std".into(),
        agent_id: "rev-4".into(),
        verdict: workflow::Verdict::Pass,
        head: "a3f9c21".into(),
        body_digest: digest.clone(),
        verified_body: true,
        open_findings: None,
        summary: "body verified at this head".into(),
        ts_ms: 1_720_000_000_000,
    };
    let text = workflow::verdict_file_text(&base);
    assert_eq!(
        text.lines().nth(4),
        Some(format!("{digest} {}", workflow::VERIFIED_BODY_MARK).as_str()),
        "the mark rides line 5 AFTER the digest — both halves split the mark off and \
         run sanitize_digest over what remains"
    );
    assert_eq!(
        text.lines().nth(5),
        Some("body verified at this head"),
        "and the summary still starts on line 6, so nothing a reviewer wrote moved"
    );
    assert_eq!(workflow::parse_verdict_file(2168, "rev-std", &text).unwrap(), base, "round trip");

    // **The forgery this placement refuses.** A reviewer types the summary and
    // nothing else, and the summary starts on line 6 — so both shapes it could
    // reach for are read back as prose. The second is the strongest thing a
    // reviewer could construct, since it can compute the body's digest as
    // easily as orrerix can; the first is what a build that put the mark on a
    // line of its OWN would have swallowed.
    for forged_summary in [
        format!("{}\nreally", workflow::VERIFIED_BODY_MARK),
        format!("{digest} {}\nreally", workflow::VERIFIED_BODY_MARK),
    ] {
        let forger = workflow::ReviewVerdict {
            verified_body: false,
            summary: forged_summary.clone(),
            ..base.clone()
        };
        let forged = workflow::parse_verdict_file(
            2168,
            "rev-std",
            &workflow::verdict_file_text(&forger),
        )
        .unwrap();
        assert!(
            !forged.verified_body,
            "a reviewer that types the mark into its summary must not thereby grant itself \
             the delegation that opens `body-unchanged` for other lanes: {forged_summary:?}"
        );
        assert_eq!(
            forged, forger,
            "…and its summary survives verbatim rather than being eaten: {forged_summary:?}"
        );
    }

    // The mark never travels without a digest to qualify: what it asserts is
    // "the body THIS digest names was verified", and there is no such body when
    // the read failed.
    let no_digest = workflow::ReviewVerdict {
        body_digest: String::new(),
        verified_body: true,
        ..base.clone()
    };
    let text = workflow::verdict_file_text(&no_digest);
    assert_eq!(text.lines().nth(4), Some(""), "line 5 is empty, mark and all");
    assert!(!workflow::parse_verdict_file(2168, "rev-std", &text).unwrap().verified_body);

    // A pre-#565 file whose line 5 is prose reads exactly as it did before this
    // slice: the WHOLE line is offered to `sanitize_digest`, which refuses it,
    // and it stays in the summary. Splitting line 5 on the first space
    // unconditionally would have rewritten durable prose a human wrote.
    let legacy = workflow::parse_verdict_file(
        2168,
        "rev-std",
        "pass\na3f9c21\n1\nrev-4\nlooked fine to me\nsecond line\n",
    )
    .unwrap();
    assert_eq!(legacy.body_digest, "");
    assert!(!legacy.verified_body);
    assert_eq!(legacy.summary, "looked fine to me\nsecond line");
}

/// **#3367 item 5: `open_findings` rides line 4, and only a well-formed tail
/// there is a declaration.**
///
/// Line 4 because the three lines the `gh` shim reads — 1, 2 and 5 — must be
/// byte-for-byte what they were, and line 6 onward is the summary a reviewer
/// writes. The clean case skips the orchestrator's disposition on a declared
/// zero, so a zero this parser GUESSED would be a clean review nobody made:
/// every malformed or forged form below must read as no declaration at all.
#[test]
fn open_findings_rides_line_4_and_nothing_but_a_well_formed_tail_declares_it() {
    let digest = workflow::body_digest("## What\n\nA fix.\n");
    let rec = workflow::ReviewVerdict {
        pr: 3367,
        block: "rev-std".into(),
        agent_id: "rev-4".into(),
        verdict: workflow::Verdict::Pass,
        head: "a3f9c21".into(),
        body_digest: digest.clone(),
        verified_body: false,
        open_findings: Some(0),
        summary: "nothing left open\nsecond line".into(),
        ts_ms: 1_720_000_000_000,
    };
    let text = workflow::verdict_file_text(&rec);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "pass", "line 1 is the shim's, unchanged");
    assert_eq!(lines[1], "a3f9c21", "line 2 is the shim's, unchanged");
    assert_eq!(lines[3], format!("rev-4 {}0", workflow::OPEN_FINDINGS_KEY), "line 4 carries it");
    assert_eq!(lines[4], digest, "line 5 is the shim's, unchanged — the digest alone");
    let back = workflow::parse_verdict_file(3367, "rev-std", &text).unwrap();
    assert_eq!(back, rec, "the declaration round-trips, agent id and summary intact");

    // No declaration: the file is byte-for-byte the pre-#3367 shape.
    let none = workflow::ReviewVerdict { open_findings: None, ..rec.clone() };
    let none_text = workflow::verdict_file_text(&none);
    assert_eq!(none_text.lines().nth(3), Some("rev-4"));
    assert_eq!(workflow::parse_verdict_file(3367, "rev-std", &none_text).unwrap(), none);

    // A real count other than zero round-trips too.
    let three = workflow::ReviewVerdict { open_findings: Some(3), ..rec.clone() };
    assert_eq!(
        workflow::parse_verdict_file(3367, "rev-std", &workflow::verdict_file_text(&three))
            .unwrap()
            .open_findings,
        Some(3)
    );

    // Malformed tails are NOT declarations — never a guessed zero — and stay
    // on the id where a reader can see them.
    for bad in ["rev-4 open-findings=", "rev-4 open-findings=-1", "rev-4 open-findings=zero",
                "rev-4 open-findings=99999999999", "rev-4 open_findings=0"] {
        let t = format!("pass\na3f9c21\n1\n{bad}\n{digest}\nlgtm\n");
        let v = workflow::parse_verdict_file(3367, "rev-std", &t).unwrap();
        assert_eq!(v.open_findings, None, "{bad:?} must not declare anything");
        assert_eq!(v.agent_id, bad, "…and is left visible on the id");
    }

    // A reviewer typing the token into its SUMMARY declares nothing: line 6
    // onward is prose, and the parser never looks there for it.
    let forged = workflow::ReviewVerdict {
        open_findings: None,
        summary: format!("{}0\nrev-4 {}0", workflow::OPEN_FINDINGS_KEY, workflow::OPEN_FINDINGS_KEY),
        ..rec.clone()
    };
    let parsed =
        workflow::parse_verdict_file(3367, "rev-std", &workflow::verdict_file_text(&forged)).unwrap();
    assert_eq!(parsed.open_findings, None, "a summary cannot declare the count");
    assert_eq!(parsed, forged, "…and survives verbatim");

    // `list_verdicts` serialises the record: present when declared, ABSENT
    // (never `null`, never 0) when not — so an undeclared row is unchanged.
    let json = serde_json::to_value(&rec).unwrap();
    assert_eq!(json["open_findings"], serde_json::json!(0));
    assert!(serde_json::to_value(&none).unwrap().get("open_findings").is_none());
}

#[test]
fn the_gate_file_the_shim_reads_round_trips_and_carries_only_clean_tokens() {
    // The shim is a POSIX script that word-splits this file, so every token in it
    // must already be shell-inert: ids and conditions are *rejected* (never
    // rewritten) by the parser when they leave their alphabet — the contract #225
    // established at the parse boundary precisely so this consumer could assume it.
    let wf = workflow::parse_workflow(FOCUSED_REVIEW).unwrap();
    let g = wf.gates.get("merge").unwrap();
    let text = workflow::gate_file_text(g);
    assert!(text.contains("require all-pass\n"));
    assert!(text.contains("reviewer rev-security\n") && text.contains("reviewer rev-tests\n"));
    assert!(text.contains("also ci-green\n"));
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        assert!(
            line.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ')),
            "every token the shim word-splits must be shell-inert: {line:?}"
        );
    }
    assert_eq!(workflow::parse_gate_file(&text).as_ref(), Some(g), "round trip");

    // Threshold form.
    let t = gate(GateRequire::Threshold(2), &["a", "b", "c"], &[]);
    assert!(workflow::gate_file_text(&t).contains("require threshold 2\n"));
    assert_eq!(workflow::parse_gate_file(&workflow::gate_file_text(&t)), Some(t));

    // A gate file naming nobody is not a usable gate; a malformed threshold falls
    // back to the STRICTER all-pass, never to a number that lets something through.
    assert!(workflow::parse_gate_file("# empty\nrequire all-pass\n").is_none());
    assert_eq!(
        workflow::parse_gate_file("require threshold 0\nreviewer a\n").unwrap().require,
        GateRequire::AllPass
    );

    // A token that cannot be serialized safely POISONS the file — it is not silently
    // dropped. Dropping it would emit a *weaker* gate than the repo declared (a
    // reviewer just disappears, and the gate goes green one requirement short), and
    // every other fork in this feature chooses fail-closed on exactly that question.
    // Unreachable while the parse contract holds; this is what happens if it stops.
    let bad = gate(GateRequire::AllPass, &["rev ok", "rev-fine"], &["ci green"]);
    let poisoned = workflow::gate_file_text(&bad);
    assert!(poisoned.contains(workflow::POISON_KEY), "an unrepresentable token poisons the file");
    assert!(poisoned.contains("reviewer rev-fine"), "the representable ones still land");
    assert!(
        workflow::parse_gate_file(&poisoned).is_none(),
        "and neither half of the gate will read a poisoned file as a usable gate"
    );
    // Any line loomux cannot parse — poison, truncation, hand edit — makes the file
    // unusable rather than partially enforced. (The shim refuses on the same shapes;
    // `gh_shim_harness_refuses_a_truncated_or_malformed_gate_file` executes them.)
    assert!(workflow::parse_gate_file("require all-pass\nreviewer a\nsomething else\n").is_none());
    // Including an unrecognized RULE. `all-pass` is the strict one, so quietly falling
    // back to it would look safe — but it would mean enforcing a rule the file does not
    // state, and the shim would have to make the same lucky guess to agree. Refuse.
    assert!(workflow::parse_gate_file("require bogus\nreviewer a\n").is_none());
}

#[test]
fn the_small_batch_clause_parses_round_trips_and_refuses_a_limit_that_limits_nothing() {
    // #1174 A1. A STRUCTURED KEY, not an `also:` token: `also:` is a closed
    // vocabulary of parameterless conditions, and a threshold is a number.
    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: r\n    kind: reviewer\ngates:\n  merge:\n    reviewers: [r]\n    max_diff_lines: 800\n",
    )
    .expect("a declared limit parses");
    let g = wf.gates.get("merge").unwrap();
    assert_eq!(g.max_diff_lines, Some(800));

    // It reaches the shim, and comes back the same — the shim reads THIS file,
    // so a key that does not round-trip is a clause the shim never enforces.
    let text = workflow::gate_file_text(g);
    assert!(text.contains("max-diff-lines 800\n"), "{text}");
    assert_eq!(workflow::parse_gate_file(&text).as_ref(), Some(g), "round trip");

    // ABSENT = the feature off, and the gate file says nothing at all about it.
    let plain = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: r\n    kind: reviewer\ngates:\n  merge:\n    reviewers: [r]\n",
    )
    .unwrap();
    let plain = plain.gates.get("merge").unwrap();
    assert_eq!(plain.max_diff_lines, None);
    assert!(!workflow::gate_file_text(plain).contains("max-diff-lines"));

    // `0` is a PARSE ERROR, not "unlimited". A bound the repo wrote down must
    // never be read as the absence of one — the rule `threshold: 0` already
    // follows, and the reason both refuse rather than clamping.
    let err = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: r\n    kind: reviewer\ngates:\n  merge:\n    reviewers: [r]\n    max_diff_lines: 0\n",
    )
    .expect_err("0 must not load");
    assert!(
        err.iter().any(|e| e.contains("max_diff_lines")),
        "the error must name the key: {err:?}"
    );
    // A negative or fractional value never reaches that check: serde refuses the
    // whole file at `Option<u32>`, exactly as `threshold: -1` already does.
    for bad in ["-1", "1.5", "eight hundred"] {
        assert!(
            workflow::parse_workflow(&format!(
                "version: 1\nblocks:\n  - id: r\n    kind: reviewer\ngates:\n  merge:\n    reviewers: [r]\n    max_diff_lines: {bad}\n"
            ))
            .is_err(),
            "max_diff_lines: {bad} must not load"
        );
    }

    // A gate FILE carrying a number the shim could not use is MALFORMED — every
    // merge refused — never silently limitless. `require threshold 0` falls back
    // to the stricter all-pass; there is no stricter fallback for a size limit,
    // so the whole file goes unusable instead. Both halves choose the same
    // direction, which is the only property that matters here.
    for bad in ["max-diff-lines 0", "max-diff-lines abc", "max-diff-lines"] {
        assert!(
            workflow::parse_gate_file(&format!("require all-pass\nreviewer a\n{bad}\n")).is_none(),
            "{bad:?} must not read back as a usable gate"
        );
    }
}

#[test]
fn check_diff_size_is_the_one_definition_of_too_big() {
    // The pure spec the shim's shell mirrors and the merge queue re-runs (#1174).
    let with = |limit: Option<u32>| workflow::Gate {
        require: workflow::GateRequire::AllPass,
        reviewers: vec!["r".into()],
        also: vec![],
        max_diff_lines: limit,
        routing: Vec::new(),
    };
    use workflow::DiffSizeVerdict as V;
    assert_eq!(workflow::check_diff_size(&with(Some(800)), Some(799)), V::Ok);
    assert_eq!(workflow::check_diff_size(&with(Some(800)), Some(800)), V::Ok, "at the limit is within it — `max` means at most");
    assert_eq!(
        workflow::check_diff_size(&with(Some(800)), Some(801)),
        V::TooLarge { lines: 801, limit: 800 }
    );
    // Unknown REFUSES. This is the fail-closed half, and it is the one an
    // implementation drifts on: a size loomux could not read is not a small PR.
    assert_eq!(workflow::check_diff_size(&with(Some(800)), None), V::Unknown { limit: 800 });
    assert!(!V::Unknown { limit: 800 }.ok() && !V::TooLarge { lines: 1, limit: 0 }.ok());
    // …and with no limit declared, an unreadable size is STILL fine: a repo that
    // never asked for this must not start refusing merges because of it.
    assert_eq!(workflow::check_diff_size(&with(None), None), V::Ok);
    assert_eq!(workflow::check_diff_size(&with(None), Some(9_999_999)), V::Ok);
}

#[test]
fn an_also_condition_this_build_cannot_check_is_not_silently_ignored() {
    // A gate is a safety claim, so dropping a clause loomux doesn't understand would
    // turn a stricter-looking workflow file into a weaker one. An unknown condition
    // fails CLOSED in the shim (pinned in the shell, in tests/orchestration/); this
    // pins the classification the shim keys off.
    assert!(workflow::condition_supported("ci-green"));
    // #565's opt-in body-digest check is a condition, not a new config surface: a repo
    // that squash-merges declares it, one that merge-commits leaves it out.
    assert!(workflow::condition_supported("body-unchanged"));
    // #1174's stop-the-line clause: opt-in for the same reason `body-unchanged`
    // is — a repo with no CI would otherwise be refused every merge by a clause
    // it never asked for.
    assert!(workflow::condition_supported("base-green"));
    for unknown in [
        "no-live-agents-on-pr", "human-signoff", "ci_green", "CI-GREEN", "body_unchanged",
        "base_green", "BASE-GREEN", "basegreen",
    ] {
        assert!(!workflow::condition_supported(unknown), "{unknown:?} must not read as supported");
    }
    // The PARSER still accepts them — the file format is forward-compatible, and a
    // future build may know more conditions than this one. What it rejects is a
    // condition that is not a usable *name* at all. Enforcement is where the refusal
    // lives, because that is the only place that can fail closed.
    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: r\n    kind: reviewer\ngates:\n  merge:\n    reviewers: [r]\n    also: [no-live-agents-on-pr]\n",
    )
    .unwrap();
    assert_eq!(wf.gates["merge"].also, vec!["no-live-agents-on-pr"]);
}

// ───────── #316: gate satisfiability against the LIVE roster ─────────
//
// A gate's reviewer names are validated against the workflow file's OWN blocks at
// parse time (`the_repos_own_workflow_file_parses_clean_against_the_real_parser`
// below) — but the roster a group actually SPAWNS FROM can diverge from that: a
// broken/absent workflow.yml on a fresh launch keeps the group's last-known merge
// gate but resets `blocks` to the built-in four (registry/groups.rs `create_group`'s
// `merge-gate-retained` branch). The live incident behind #316: the gate named
// rev-orch/rev-ui/rev-tests, the registry offered only the built-in four, and
// `spawn_agent(block: "rev-orch")` failed with "unknown block" — the gate was
// unsatisfiable from inside the very session that armed it. `gate_missing_blocks`
// is the pure check that catches this at every arm point, independent of *why*
// the roster and the gate diverged.

#[test]
fn gate_missing_blocks_finds_every_reviewer_the_roster_cannot_spawn() {
    let g = gate(GateRequire::AllPass, &["rev-orch", "rev-ui", "rev-tests"], &[]);
    let builtin = workflow::builtin_roster("claude");
    assert_eq!(
        workflow::gate_missing_blocks(&g, &builtin),
        vec!["rev-orch".to_string(), "rev-ui".to_string(), "rev-tests".to_string()],
        "the built-in four-block roster can spawn none of the three named reviewers"
    );
}

#[test]
fn gate_missing_blocks_is_empty_against_the_roster_that_actually_declares_them() {
    let repo = repo_root();
    let wf = match workflow::load_workflow(&repo) {
        Ok(Some(wf)) => wf,
        other => panic!("loomux must ship its own parseable {}: {other:?}", workflow::workflow_path(&repo)),
    };
    // A file with no merge gate is valid (#3507), and then no reviewer is missing from it.
    let Some(gate) = wf.gates.get("merge") else { return };
    assert_eq!(
        workflow::gate_missing_blocks(gate, &wf.blocks),
        Vec::<String>::new(),
        "loomux's own dogfood roster declares every reviewer its own gate names"
    );
}

#[test]
fn gate_missing_blocks_reports_a_block_named_by_id_but_not_kind_reviewer() {
    // A workflow edited so `rev-tests` now belongs to a worker block: the gate
    // still names it, but no reviewer will ever answer to it — reported exactly
    // like an absent block, not silently matched by id alone.
    let g = gate(GateRequire::AllPass, &["rev-tests"], &[]);
    let blocks = vec![block("worker", Role::Worker), block("rev-tests", Role::Worker)];
    assert_eq!(
        workflow::gate_missing_blocks(&g, &blocks),
        vec!["rev-tests".to_string()],
        "an id that exists under the wrong kind is still unsatisfiable — kind must match, not just id"
    );
}

// ───────── path-based reviewer routing: the pure contract (#1176) ─────────
//
// Everything here is pure, so it pins the semantics in microseconds and gives
// the `gh` shim's POSIX mirror something to agree with. The shell itself is
// EXECUTED end-to-end in tests/orchestration/ — a shim/mirror agreement
// asserted only against source text is not an agreement, it is a comment.

/// A workflow declaring one worker, three reviewers and a merge gate whose
/// `gates.merge` body is `gate_body`.
fn routed_workflow(gate_body: &str) -> String {
    format!(
        "version: 1\nblocks:\n\
         \x20 - id: w\n    kind: worker\n\
         \x20 - id: rev-lead\n    kind: reviewer\n\
         \x20 - id: rev-ui\n    kind: reviewer\n\
         \x20 - id: rev-deps\n    kind: reviewer\n\
         gates:\n  merge:\n{gate_body}"
    )
}

/// The gate out of a workflow that must parse.
fn routed_gate(gate_body: &str) -> workflow::Gate {
    workflow::parse_workflow(&routed_workflow(gate_body))
        .unwrap_or_else(|e| panic!("must parse: {e:?}"))
        .gates
        .get("merge")
        .expect("a merge gate")
        .clone()
}

fn routed_errs(gate_body: &str) -> Vec<String> {
    workflow::parse_workflow(&routed_workflow(gate_body)).unwrap_err()
}

pub(crate) fn paths(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn the_glob_contract_is_star_only_star_crosses_slash_and_a_leading_doublestar_is_optional() {
    use workflow::glob_match as m;
    // Rule 1: `*` is any run of characters, INCLUDING `/`. Deliberately coarser
    // than gitignore — over-matching requires an extra lane, under-matching
    // skips one, and only one of those is survivable.
    assert!(m("src/**", "src/app.ts"));
    assert!(m("src/**", "src/deep/nested/app.ts"), "`*` crosses `/`: that IS the contract");
    assert!(m("src/*", "src/deep/nested/app.ts"), "one star and two are the same set");
    assert!(!m("src/**", "srcfoo"), "the `/` after src is a literal and must be there");
    assert!(!m("src/**", "docs/src/app.ts"), "anchored at the START");
    assert!(!m("src/**", "src"), "`src/**` needs something under src/");

    // Rule 3: a LEADING `**/` is optional — the one place `**` means more than
    // `*`. Without it `**/Cargo.toml` silently misses the root one, which is a
    // SKIPPED reviewer.
    assert!(m("**/Cargo.toml", "Cargo.toml"), "the leading `**/` is optional");
    assert!(m("**/Cargo.toml", "crates/loomux-engine/Cargo.toml"));
    assert!(!m("**/Cargo.toml", "Cargo.lock"));
    assert!(!m("**/Cargo.toml", "crates/Cargo.toml.bak"), "anchored at the END");

    // Rule 4, both ends, and the exact-literal case.
    assert!(m("package-lock.json", "package-lock.json"));
    assert!(!m("package-lock.json", "web/package-lock.json"), "no implicit `**/` — it is written");
    assert!(!m("package-lock.json", "package-lock.json.bak"));

    // `*` may match nothing at all.
    assert!(m("docs/*.md", "docs/.md"));
    assert!(m("*", ""));
    assert!(m("*.rs", "a.rs"));
    assert!(m("a*b*c", "abc"));
    assert!(m("a*b*c", "axxbyyc"));
    assert!(!m("a*b*c", "axxbyy"));

    // `?` is NOT a metacharacter — `sanitize_glob` refuses it, and if one ever
    // reached here it must be a literal rather than the one construct whose
    // meaning a shell `case` and a Rust matcher could not be shown to share.
    assert!(m("a?c", "a?c"));
    assert!(!m("a?c", "abc"));
}

#[test]
fn sanitize_glob_refuses_every_glob_that_could_never_fire_or_could_reach_the_shell() {
    use workflow::sanitize_glob as s;
    // The alphabet, unchanged by the filter — which is what `parse_workflow`
    // compares against, so "unchanged" is the whole acceptance test.
    for ok in ["src/**", "**/Cargo.toml", "package-lock.json", "a_b-c.d/*", "*"] {
        assert_eq!(s(ok).as_deref(), Some(ok), "{ok} is inside the alphabet");
    }
    // Outside the alphabet: the filter CHANGES them, which `parse_workflow`
    // reads as a refusal (reject, never rewrite — the #225 contract).
    for bad in ["src/[ab]*", "src/a\\*b", "a?c", "src/{a,b}", "src/a b", "a$b", "a;b"] {
        assert_ne!(s(bad).as_deref(), Some(bad), "{bad} must not survive unchanged");
    }
    // Shapes refused outright: each is a rule that could NEVER fire, and a rule
    // that never fires silently drops a reviewer the repo asked for.
    assert_eq!(s(""), None);
    assert_eq!(s("   "), None);
    assert_eq!(s("/src/**"), None, "GitHub reports changed paths repo-relative");
    assert_eq!(s("src/"), None, "every changed path names a file, never a directory");
    assert_eq!(s("../etc/**"), None);
    assert_eq!(s("a/../b"), None);
    // …but `..` INSIDE a segment is an ordinary filename.
    assert_eq!(s("a..b.txt").as_deref(), Some("a..b.txt"));
}

#[test]
fn route_reviewers_is_the_union_and_an_unaccountable_file_list_refuses() {
    let g = routed_gate(
        "    reviewers: [rev-lead]\n\
         \x20   routing:\n\
         \x20     - paths: [src/**]\n\
         \x20       reviewers: [rev-ui]\n\
         \x20     - paths: [\"**/Cargo.toml\", package-lock.json]\n\
         \x20       reviewers: [rev-deps]\n",
    );
    assert_eq!(g.routing.len(), 2);
    assert_eq!(g.routing[0].paths, paths(&["src/**"]));
    assert_eq!(g.routing[1].reviewers, paths(&["rev-deps"]));

    let decide = |files: &[&str]| {
        workflow::route_reviewers(&g, Some(&paths(files))).expect("a resolvable list")
    };

    // Nothing matched: the static list, untouched. Routing is ADDITIVE.
    let d = decide(&["docs/orchestration.md"]);
    assert_eq!(d.required, paths(&["rev-lead"]));
    assert!(d.fired.is_empty());

    // One rule matched → its lane is appended AFTER the static list, and the
    // ORDER is part of the contract because the shim appends in the same one.
    let d = decide(&["src/app.ts"]);
    assert_eq!(d.required, paths(&["rev-lead", "rev-ui"]));
    assert_eq!(d.fired.len(), 1);
    assert_eq!(d.fired[0].index, 1, "1-based, matching the position in the file");
    assert_eq!(
        d.fired[0].paths,
        paths(&["src/**"]),
        "the rule's OWN globs, not the one that happened to match"
    );

    // Both, in declaration order.
    let d = decide(&["Cargo.toml", "src/app.ts"]);
    assert_eq!(d.required, paths(&["rev-lead", "rev-ui", "rev-deps"]));
    assert_eq!(d.fired.iter().map(|f| f.index).collect::<Vec<_>>(), vec![1, 2]);

    // A rule matches on its SECOND glob just as well as its first.
    assert_eq!(decide(&["package-lock.json"]).required, paths(&["rev-lead", "rev-deps"]));

    // A complete answer that is empty is not the same as no answer.
    assert_eq!(decide(&[]).required, paths(&["rev-lead"]));

    // THE FAIL-CLOSED CASE. The unknown here is *which reviewers are required*,
    // so "no rule fired" would be a guess in favour of merging.
    assert!(workflow::route_reviewers(&g, None).is_none());

    // A gate with NO routing never looks at the list at all — the absent-config
    // no-op, which is what keeps a repo that never wrote the key on exactly the
    // path it was on before #1176.
    let plain = routed_gate("    reviewers: [rev-lead]\n");
    assert!(plain.routing.is_empty());
    assert_eq!(
        workflow::route_reviewers(&plain, None)
            .expect("no routing declared, so nothing to resolve")
            .required,
        paths(&["rev-lead"])
    );
}

#[test]
fn a_routed_reviewer_already_on_the_static_list_is_required_once_not_twice() {
    // A duplicate in `required` would let one PASS count twice under a
    // threshold and would inflate `gate_need` — the same integrity gap a
    // duplicate in `reviewers:` is refused for.
    let g = routed_gate(
        "    reviewers: [rev-lead, rev-ui]\n\
         \x20   routing:\n\
         \x20     - paths: [src/**]\n\
         \x20       reviewers: [rev-ui, rev-deps]\n",
    );
    let d = workflow::route_reviewers(&g, Some(&paths(&["src/a.ts"]))).unwrap();
    assert_eq!(d.required, paths(&["rev-lead", "rev-ui", "rev-deps"]));
    assert_eq!(workflow::gate_need(&d.gate(&g)), 3);
}

#[test]
fn the_effective_gate_carries_every_other_clause_forward_and_then_spends_routing() {
    // Routing resolves to a reviewer list and gets out of the way, so
    // `evaluate_merge_gate`/`gate_need`/the body-digest loop stay ONE
    // implementation each. An effective gate that still carried rules would be
    // an invitation to resolve them a second time, differently.
    let g = routed_gate(
        "    also: [ci-green]\n    max_diff_lines: 800\n    reviewers: [rev-lead]\n\
         \x20   routing:\n\
         \x20     - paths: [src/**]\n\
         \x20       reviewers: [rev-ui]\n",
    );
    let eff = workflow::route_reviewers(&g, Some(&paths(&["src/a.ts"]))).unwrap().gate(&g);
    assert_eq!(eff.reviewers, paths(&["rev-lead", "rev-ui"]));
    assert!(eff.routing.is_empty(), "spent");
    assert_eq!(eff.also, g.also, "every other clause is carried through untouched");
    assert_eq!(eff.require, g.require);
    assert_eq!(eff.max_diff_lines, g.max_diff_lines);
}

#[test]
fn routing_rules_are_validated_at_parse_and_every_refusal_is_loud() {
    let has = |errs: &[String], needle: &str| {
        assert!(errs.iter().any(|e| e.contains(needle)), "expected {needle:?} in {errs:?}");
    };

    // THE ONE THAT IS NOT OBVIOUS: routing + threshold. A threshold counts
    // passes over a fixed list; routing makes the list depend on the diff. Read
    // together, an added lane could SUPPLY one of the N passes instead of adding
    // one — so declaring a routing rule would make the gate EASIER to satisfy.
    let errs = routed_errs(
        "    require: threshold\n    threshold: 1\n    reviewers: [rev-lead, rev-ui]\n\
         \x20   routing:\n      - paths: [src/**]\n        reviewers: [rev-deps]\n",
    );
    has(&errs, "cannot both be declared");
    has(&errs, "all-pass");

    // A rule with no paths can never fire; one with no reviewers requires
    // nobody. Both are the same laxening wearing different clothes.
    has(
        &routed_errs(
            "    reviewers: [rev-lead]\n    routing:\n      - paths: []\n        reviewers: [rev-ui]\n",
        ),
        "declares no paths",
    );
    has(
        &routed_errs(
            "    reviewers: [rev-lead]\n    routing:\n      - paths: [src/**]\n        reviewers: []\n",
        ),
        "names no reviewers",
    );

    // The reviewer checks are the STATIC list's checks, from one definition —
    // so a routing rule cannot quietly accept what `reviewers:` refuses.
    has(
        &routed_errs(
            "    reviewers: [rev-lead]\n    routing:\n      - paths: [src/**]\n        reviewers: [ghost]\n",
        ),
        "names no block",
    );
    has(
        &routed_errs(
            "    reviewers: [rev-lead]\n    routing:\n      - paths: [src/**]\n        reviewers: [w]\n",
        ),
        "is a worker block, not a reviewer",
    );
    // …and the message points at the RULE, not merely at "a reviewer".
    has(
        &routed_errs(
            "    reviewers: [rev-lead]\n    routing:\n      - paths: [a/**]\n        reviewers: [rev-ui]\n      - paths: [b/**]\n        reviewers: [ghost]\n",
        ),
        "routing rule 2 reviewer",
    );

    // Duplicates, both halves.
    has(
        &routed_errs(
            "    reviewers: [rev-lead]\n    routing:\n      - paths: [src/**, src/**]\n        reviewers: [rev-ui]\n",
        ),
        "more than once",
    );
    has(
        &routed_errs(
            "    reviewers: [rev-lead]\n    routing:\n      - paths: [src/**]\n        reviewers: [rev-ui, rev-ui]\n",
        ),
        "more than once",
    );

    // A glob outside the alphabet, and two that could never fire.
    for bad_glob in ["src/[ab]*", "/src/**", "src/"] {
        has(
            &routed_errs(&format!(
                "    reviewers: [rev-lead]\n    routing:\n      - paths: [\"{bad_glob}\"]\n        reviewers: [rev-ui]\n"
            )),
            "not a usable path glob",
        );
    }

    // A misspelled key is a refusal, not a rule that silently routes nothing —
    // `deny_unknown_fields`, the same closed vocabulary every other section has.
    has(
        &routed_errs(
            "    reviewers: [rev-lead]\n    routing:\n      - path: [src/**]\n        reviewers: [rev-ui]\n",
        ),
        "unknown field",
    );

    // The caps are bounds on work the SHIM does on the merge path.
    let many: String = (0..workflow::ROUTING_RULES_MAX + 1)
        .map(|i| format!("      - paths: [d{i}/**]\n        reviewers: [rev-ui]\n"))
        .collect();
    has(
        &routed_errs(&format!("    reviewers: [rev-lead]\n    routing:\n{many}")),
        "routing rules — at most",
    );
    let wide: Vec<String> =
        (0..workflow::ROUTING_PATHS_MAX + 1).map(|i| format!("d{i}/**")).collect();
    has(
        &routed_errs(&format!(
            "    reviewers: [rev-lead]\n    routing:\n      - paths: [{}]\n        reviewers: [rev-ui]\n",
            wide.join(", ")
        )),
        "paths — at most",
    );
}

#[test]
fn the_gate_file_round_trips_routing_and_refuses_every_file_it_cannot_stitch() {
    let g = routed_gate(
        "    also: [ci-green]\n    reviewers: [rev-lead]\n\
         \x20   routing:\n\
         \x20     - paths: [src/**, index.html]\n\
         \x20       reviewers: [rev-ui]\n\
         \x20     - paths: [\"**/Cargo.toml\"]\n\
         \x20       reviewers: [rev-deps]\n",
    );
    let text = workflow::gate_file_text(&g);
    // Line-oriented `key value value`, because the reader is a POSIX
    // `while read -r k v w` with no arrays.
    assert!(text.contains("route-path 1 src/**\n"), "{text}");
    assert!(text.contains("route-path 1 index.html\n"), "{text}");
    assert!(text.contains("route-reviewer 1 rev-ui\n"), "{text}");
    assert!(text.contains("route-path 2 **/Cargo.toml\n"), "{text}");
    assert!(text.contains("route-reviewer 2 rev-deps\n"), "{text}");
    assert_eq!(workflow::parse_gate_file(&text).as_ref(), Some(&g), "round trip");

    // A gate with no routing writes no routing lines — the absent-config no-op
    // is visible in the file, not merely in the behaviour.
    let plain = routed_gate("    reviewers: [rev-lead]\n");
    assert!(!workflow::gate_file_text(&plain).contains("route-"));

    // Every unreadable file refuses OUTRIGHT. `parse_gate_file` returning None
    // is reported by both callers as "malformed — every merge refused", which is
    // the only safe reading: a routing line that is dropped is a required
    // reviewer that silently stops being required.
    let base = "require all-pass\nreviewer rev-lead\n";
    for bad in [
        "route-path 1 src/**\n",                            // paths, no reviewer
        "route-reviewer 1 rev-ui\n",                        // reviewer, no paths
        "route-path 1 src/**\nroute-reviewer 2 rev-ui\n",   // halves of different rules
        "route-path 2 src/**\nroute-reviewer 2 rev-ui\n",   // a gap: no rule 1
        "route-path 0 src/**\nroute-reviewer 0 rev-ui\n",   // rules are numbered from 1
        "route-path x src/**\nroute-reviewer x rev-ui\n",   // not a number
        // THE REWRITE CLASS, and the one that is not obvious: `sanitize_glob`
        // and `sanitize_id` FILTER rather than refuse, so reading them without
        // comparing turns `src/[ab]` into `src/ab` and `rev@ui` into `revui` —
        // not a refusal, a DIFFERENT RULE silently substituted for the one the
        // file carries. Both halves are present in each line below, so what
        // these pin is the rewrite and not a missing partner.
        "route-path 1 src/[ab]\nroute-reviewer 1 rev-ui\n", // outside the glob alphabet
        "route-path 1 src/a b\nroute-reviewer 1 rev-ui\n",  // a glob the writer could not have written
        "route-path 1 src/**\nroute-reviewer 1 rev@ui\n",   // outside the id alphabet
        "route-path 1 /src/**\nroute-reviewer 1 rev-ui\n",  // could never fire
        "route-path 1\nroute-reviewer 1 rev-ui\n",          // truncated line
        "route-path 1 src/**\nroute-reviewer 1\n",          // truncated line
    ] {
        assert!(
            workflow::parse_gate_file(&format!("{base}{bad}")).is_none(),
            "a gate file loomux cannot stitch must refuse every merge, not become a laxer gate: {bad:?}"
        );
    }
    // The pair `parse_workflow` refuses is refused HERE too, rather than
    // trusted to be impossible: this reader's job is to be the half that does
    // not assume the other half held.
    assert!(workflow::parse_gate_file(
        "require threshold 1\nreviewer rev-lead\nroute-path 1 src/**\nroute-reviewer 1 rev-ui\n"
    )
    .is_none());
}

#[test]
fn an_unrepresentable_routing_rule_poisons_the_gate_file_rather_than_vanishing_from_it() {
    // The #222 contract, extended to routing: a token that cannot be serialized
    // safely writes a line the shim cannot parse — which refuses every merge —
    // instead of disappearing and leaving a gate one requirement short.
    let with = |routing: Vec<workflow::RoutingRule>| workflow::Gate {
        require: GateRequire::AllPass,
        reviewers: vec!["rev-lead".into()],
        also: vec![],
        max_diff_lines: None,
        routing,
    };
    for bad in [
        workflow::RoutingRule { paths: paths(&["src/[ab]"]), reviewers: paths(&["rev-ui"]) },
        workflow::RoutingRule { paths: paths(&["src/**"]), reviewers: paths(&["rev ui"]) },
        workflow::RoutingRule { paths: vec![], reviewers: paths(&["rev-ui"]) },
        workflow::RoutingRule { paths: paths(&["src/**"]), reviewers: vec![] },
    ] {
        let g = with(vec![bad.clone()]);
        let text = workflow::gate_file_text(&g);
        assert!(text.contains(workflow::POISON_KEY), "must poison, not drop: {bad:?} -> {text}");
        assert!(workflow::parse_gate_file(&text).is_none(), "and the poison must refuse: {text}");
    }
    // …including the pair that has no honest reading.
    let g = workflow::Gate {
        require: GateRequire::Threshold(1),
        reviewers: vec!["rev-lead".into()],
        also: vec![],
        max_diff_lines: None,
        routing: vec![workflow::RoutingRule {
            paths: paths(&["src/**"]),
            reviewers: paths(&["rev-ui"]),
        }],
    };
    assert!(workflow::gate_file_text(&g).contains(workflow::POISON_KEY));
}

#[test]
fn the_changed_file_protocol_answers_completely_or_not_at_all() {
    use workflow::parse_routed_files as p;
    assert_eq!(p("ok\np src/a.ts\np Cargo.toml\n"), Some(paths(&["src/a.ts", "Cargo.toml"])));
    // A COMPLETE answer that happens to be empty — a PR that changed nothing —
    // is not the same statement as "loomux cannot say", and the difference is
    // the whole point of the `Option`.
    assert_eq!(p("ok\n"), Some(Vec::new()));
    // Paths with spaces survive: the prefix is the delimiter, not whitespace.
    assert_eq!(p("ok\np docs/a b.md\n"), Some(paths(&["docs/a b.md"])));

    // Everything else refuses. `unaccountable` is the reduction's own word for
    // a payload that cannot support the question — including the truncated file
    // list, which is the one that would otherwise fail OPEN.
    for bad in [
        "",
        "unaccountable",
        "unaccountable\n",
        "null\n",
        "ok\nsrc/a.ts\n",
        "ok\np \n",
        "ok\nq src/a.ts\n",
        "src/a.ts\n",
    ] {
        assert_eq!(p(bad), None, "{bad:?} is not a complete answer");
    }
}

#[test]
fn the_changed_file_reduction_is_one_definition_the_shim_can_carry() {
    let jq = workflow::ROUTING_FILES_JQ;
    // Interpolated into a SINGLE-QUOTED POSIX string in the `gh` shim, which is
    // what makes a plain substitution safe — the same property #1181 pinned for
    // the base-green reductions, and for the same reason: a future edit that
    // introduces a quote must be red here rather than a broken shim.
    assert!(!jq.contains('\''), "no single quote may appear in the reduction");
    assert!(!jq.contains('\n'), "and it must stay one line");
    // The truncation clause is the whole reason this is a reduction and not a
    // plain `.files[].path`: `gh pr view --json files` pages at 100 while
    // `changedFiles` counts them all, and for ROUTING a short list fails OPEN.
    assert!(jq.contains("changedFiles"), "{jq}");
    assert!(jq.contains("!="), "the count is compared for equality, not for `<`: {jq}");
    assert!(jq.contains("has(\"changedFiles\")"), "the shape is checked, not assumed: {jq}");
    assert!(jq.contains("has(\"files\")"), "{jq}");
    // …and the words it may answer in are the ones both readers know.
    assert!(jq.contains(&format!("\"{}\"", workflow::ROUTED_FILES_OK)), "{jq}");
    assert!(jq.contains(&format!("\"{}\"", workflow::ROUTED_FILES_PREFIX)), "{jq}");
}
