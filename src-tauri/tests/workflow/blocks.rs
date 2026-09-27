//! The declarative blocks: `merge_queue:`, `driver:`, per-block knobs, `resources:`, and `board:`.
//!
//! One module of the `workflow` integration-test target (`main.rs`),
//! split out of the former single-file `tests/workflow.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ───────── #581 §11.2: the `merge_queue:` block ─────────
//
// Policy for the bisecting merge queue (`docs/design/merge-queue.md`), parsed
// here beside `gates:`. The engine is `orchestration::mergeq`; this file only
// ever pins what the FILE means, which is the half a repo author can get wrong.

#[test]
fn an_absent_merge_queue_block_means_the_feature_is_off() {
    // §12's reversal mechanism, and the reason this is safe to land ahead of
    // the driver: no block, no queue, and the parsed policy is the product
    // default rather than anything the file influenced.
    let wf = workflow::parse_workflow("version: 1\nblocks:\n  - id: worker\n    kind: worker\n")
        .unwrap();
    assert_eq!(wf.merge_queue, workflow::MergeQueuePolicy::default());
    assert!(!wf.merge_queue.enabled, "the product default is OFF (§12)");
    assert_eq!(wf.merge_queue.max_batch, 3);
    assert_eq!(wf.merge_queue.checks_timeout_minutes, 60);
    // The specimen is deliberately SYNTHETIC: the repo's own file arms the queue
    // (a human choice, pinned in the parses-clean test) — using it here would
    // silently convert this test of the product DEFAULT into a test of this
    // repo's current choice.
}

#[test]
fn a_declared_merge_queue_block_fills_in_the_defaults_it_omits() {
    // A repo that wants the queue writes one line; it does not have to restate
    // policy it is happy with — same shape as `intake:`'s per-label fallback.
    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: worker\n    kind: worker\nmerge_queue:\n  enabled: true\n",
    )
    .unwrap();
    assert!(wf.merge_queue.enabled);
    assert_eq!(wf.merge_queue.max_batch, 3);
    assert_eq!(wf.merge_queue.checks_timeout_minutes, 60);

    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
         merge_queue:\n  enabled: true\n  max_batch: 5\n  checks_timeout_minutes: 90\n",
    )
    .unwrap();
    assert_eq!(wf.merge_queue.max_batch, 5);
    assert_eq!(wf.merge_queue.checks_timeout_minutes, 90);
}

#[test]
fn the_checks_timeout_is_clamped_by_the_notify_ttl_clamp_itself() {
    // §5's backstop is the same quantity a notify watch's TTL is — a bounded
    // wait on a PR's checks — so it takes the one definition's bounds (5..240)
    // rather than a second copy that can drift. §11.2: "clamped like the notify
    // TTLs".
    let of = |v: &str| {
        workflow::parse_workflow(&format!(
            "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
             merge_queue:\n  enabled: true\n  checks_timeout_minutes: {v}\n"
        ))
        .unwrap()
        .merge_queue
        .checks_timeout_minutes
    };
    assert_eq!(of("0"), 5, "an unbounded-in-effect wait is exactly what §5 forbids");
    assert_eq!(of("1"), 5);
    assert_eq!(of("30"), 30);
    assert_eq!(of("240"), 240);
    assert_eq!(of("99999"), 240);
}

#[test]
fn max_batch_zero_is_a_loud_error_rather_than_a_silent_default() {
    // §11.2: a malformed block never degrades to defaults, "because a queue
    // running on silently-substituted policy is a queue nobody can reason
    // about". Same posture the sibling `gates:` block takes on a `threshold`
    // that could never be satisfied.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
         merge_queue:\n  enabled: true\n  max_batch: 0\n",
    )
    .unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("merge_queue.max_batch") && e.contains("at least 1")),
        "max_batch: 0 must name itself in the error, got: {errs:?}"
    );
}

#[test]
fn the_merge_queue_block_can_never_name_a_branch_or_widen_anything() {
    // The capability-closure rule, applied to the newest block. `RawMergeQueue`
    // is `deny_unknown_fields`, so there is no spelling of "land on main", no
    // `human_gate: false`, and no key at all that this build does not
    // recognize — an attempt is a hard parse error, not an ignored line. §4 is
    // why nothing NEEDS to name a branch: the target comes from the first
    // enqueued PR's live base, and §7's default-branch refusals re-resolve live
    // at enqueue, batch build AND landing.
    for line in ["target: main", "human_gate: false", "auto_merge: true", "max_bacth: 3"] {
        let errs = workflow::parse_workflow(&format!(
            "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
             merge_queue:\n  enabled: true\n  {line}\n"
        ))
        .unwrap_err();
        assert!(!errs.is_empty(), "{line:?} must not be tolerated inside merge_queue:");
    }
    // And a mistyped block name is not silently ignored either — `RawWorkflow`
    // is `deny_unknown_fields` too, which is also why ADDING this key breaks
    // the file for builds predating slice C (§11.2, documented deliberately).
    assert!(workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: worker\n    kind: worker\nmerge_qeue:\n  enabled: true\n"
    )
    .is_err());
    // A value of the wrong type fails the whole file rather than resolving to
    // the default — policy fails loud (§11.2).
    assert!(workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: worker\n    kind: worker\nmerge_queue:\n  enabled: 3\n"
    )
    .is_err());
}

// ───────── #1778 §5.3: the `driver:` block ─────────
//
// Policy for the engine-driven review-loop driver (`docs/design/review-driver.md`),
// parsed beside `merge_queue:`. This file only ever pins what the FILE means,
// which is the half a repo author can get wrong; the driver's own core is
// `reviewdrive`, and this suite never drives anything.

#[test]
fn an_absent_driver_block_means_the_feature_is_off() {
    // §9's reversal mechanism: no block, no driver, and the parsed policy is
    // the product default rather than anything the file influenced. The
    // specimen is deliberately SYNTHETIC — this repo's own file does not arm
    // the driver, and if it ever does, that is a choice to pin beside the
    // roster, not here.
    let wf = workflow::parse_workflow("version: 1\nblocks:\n  - id: worker\n    kind: worker\n")
        .unwrap();
    assert_eq!(wf.driver, workflow::DriverPolicy::default());
    assert!(!wf.driver.enabled, "the product default is OFF (§9)");
    assert_eq!(wf.driver.max_review_rounds, 3, "INVARIANT 9's rounds (§2.3)");
    assert_eq!(wf.driver.max_ci_attempts, 3, "INVARIANT 9's CI attempts (§2.3)");
    assert_eq!(wf.driver.max_rebase_attempts, 1, "INVARIANT 9's one rebase (§2.3)");
    assert_eq!(wf.driver.lane_timeout_minutes, 60);
    assert_eq!(wf.driver.fix_timeout_minutes, 60);
    // #2110: twelve hours, and off the notify-TTL family whose ceiling this
    // used to borrow. It is the BACKSTOP now, under `reviewdrive`’s per-state
    // bounds, and a backstop measured in the same hours as the waits beneath
    // it is the one clock a drive making steady progress can still trip.
    assert_eq!(wf.driver.drive_timeout_minutes, 720);
}

#[test]
fn a_declared_driver_block_fills_in_the_defaults_it_omits() {
    // A repo that wants the driver writes one line; it does not have to restate
    // policy it is happy with — same shape as `merge_queue:`'s defaults.
    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: worker\n    kind: worker\ndriver:\n  enabled: true\n",
    )
    .unwrap();
    assert!(wf.driver.enabled);
    assert_eq!(wf.driver.max_review_rounds, 3);
    assert_eq!(wf.driver.max_ci_attempts, 3);
    assert_eq!(wf.driver.max_rebase_attempts, 1);
    assert_eq!(wf.driver.lane_timeout_minutes, 60);
    assert_eq!(wf.driver.fix_timeout_minutes, 60);
    assert_eq!(wf.driver.drive_timeout_minutes, 720);

    // …and every default is overridable, including in the TIGHTER direction —
    // which is the only direction §2.3 allows a repo file to move the counters.
    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
         driver:\n  enabled: true\n  max_review_rounds: 2\n  max_ci_attempts: 1\n  max_rebase_attempts: 0\n",
    )
    .unwrap();
    assert_eq!(wf.driver.max_review_rounds, 2);
    assert_eq!(wf.driver.max_ci_attempts, 1);
    assert_eq!(wf.driver.max_rebase_attempts, 0, "0 is legal: a repo may refuse the driver any rebase");
}

#[test]
fn driver_counters_accept_their_closed_range_edges() {
    // Each counter's lower and upper bound, accepted — the edges §2.3's closed
    // ranges are made of. Values OUTSIDE the range are refused, not clamped;
    // that is the next test's subject.
    let rounds = |v: &str| {
        workflow::parse_workflow(&format!(
            "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
             driver:\n  enabled: true\n  max_review_rounds: {v}\n"
        ))
        .unwrap()
        .driver
        .max_review_rounds
    };
    assert_eq!(rounds("1"), 1);
    assert_eq!(rounds("3"), 3);

    let ci = |v: &str| {
        workflow::parse_workflow(&format!(
            "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
             driver:\n  enabled: true\n  max_ci_attempts: {v}\n"
        ))
        .unwrap()
        .driver
        .max_ci_attempts
    };
    assert_eq!(ci("1"), 1);
    assert_eq!(ci("3"), 3);

    let rebase = |v: &str| {
        workflow::parse_workflow(&format!(
            "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
             driver:\n  enabled: true\n  max_rebase_attempts: {v}\n"
        ))
        .unwrap()
        .driver
        .max_rebase_attempts
    };
    assert_eq!(rebase("0"), 0);
    assert_eq!(rebase("1"), 1);
}

#[test]
fn driver_counters_refuse_out_of_range_values_instead_of_clamping() {
    // §2.3: the driver block clamps TOWARD INVARIANT 9, never away from it —
    // and a value outside the closed range is refused the way
    // `merge_queue.max_batch: 0` is refused, loudly and naming the field,
    // never silently pulled to the bound. A driver running on
    // silently-substituted policy is a driver nobody can reason about.
    let errs_of = |body: &str| {
        workflow::parse_workflow(&format!(
            "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
             driver:\n  enabled: true\n{body}"
        ))
        .unwrap_err()
    };
    for (field, bad, why) in [
        ("max_review_rounds", "0", "below"),
        ("max_review_rounds", "4", "above"),
        ("max_ci_attempts", "0", "below"),
        ("max_ci_attempts", "4", "above"),
        ("max_rebase_attempts", "2", "above"),
    ] {
        let errs = errs_of(&format!("  {field}: {bad}\n"));
        assert!(
            errs.iter()
                .any(|e| e.contains(&format!("driver.{field}")) && e.contains("must be")),
            "{field}: {bad} ({why} the range) must be refused naming the field, got: {errs:?}"
        );
    }
}

#[test]
fn driver_timeouts_are_clamped_by_the_notify_ttl_clamp_itself() {
    // §5.3: "clamped like the notify TTLs" — the same quantity as
    // `merge_queue.checks_timeout_minutes` (a bounded wait on a fallible
    // signal), so the same one definition bounds the two per-wait backstops
    // rather than a second copy that can drift.
    //
    // **`drive_timeout_minutes` left that family in #2110** and is checked
    // below on its own range. It stopped being the same quantity: the other
    // two bound ONE wait on ONE fallible signal, which is what the notify TTL
    // is, and this one is the last-resort bound over a whole drive with four
    // per-state clocks beneath it.
    let of = |field: &str, v: &str, want: u32| {
        let wf = workflow::parse_workflow(&format!(
            "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
             driver:\n  enabled: true\n  {field}: {v}\n"
        ))
        .unwrap();
        let got = match field {
            "lane_timeout_minutes" => wf.driver.lane_timeout_minutes,
            "fix_timeout_minutes" => wf.driver.fix_timeout_minutes,
            _ => wf.driver.drive_timeout_minutes,
        };
        assert_eq!(got, want, "{field}: {v} must land on {want}");
    };
    for field in ["lane_timeout_minutes", "fix_timeout_minutes"] {
        of(field, "0", 5);
        of(field, "1", 5);
        of(field, "30", 30);
        of(field, "240", 240);
        of(field, "99999", 240);
    }

    // The backstop's own range (#2110): the same floor, a ceiling of one day,
    // and a default of twelve hours. The 240 row is what pins the two ranges
    // apart — it is the OLD ceiling, so it clamps for the two fields above and
    // passes through untouched here, and an implementation that quietly left
    // this field on the notify clamp fails on the row below it rather than on
    // a value nobody would notice.
    of("drive_timeout_minutes", "0", 5);
    of("drive_timeout_minutes", "1", 5);
    of("drive_timeout_minutes", "240", 240);
    of("drive_timeout_minutes", "600", 600);
    of("drive_timeout_minutes", "1440", 1440);
    of("drive_timeout_minutes", "99999", 1440);
}

#[test]
fn the_driver_block_can_never_target_a_pr_or_widen_anything() {
    // The capability-closure rule, applied to the newest block. `RawDriver` is
    // `deny_unknown_fields`, so there is no spelling of "drive PR 12", no
    // `auto: true`, and no key at all this build does not recognize — §3.2's
    // two-key rule lives here: this block can only ENABLE, and no drive exists
    // until an orchestrator makes its own role-gated `drive_review` call
    // naming one PR. A mistyped field name is refused with the same
    // no-spelling-exists force.
    for line in ["pr: 12", "auto: true", "lanes: 3", "max_revew_rounds: 3"] {
        let errs = workflow::parse_workflow(&format!(
            "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
             driver:\n  enabled: true\n  {line}\n"
        ))
        .unwrap_err();
        assert!(!errs.is_empty(), "{line:?} must not be tolerated inside driver:");
    }
    // And a mistyped block name is not silently ignored either — `RawWorkflow`
    // is `deny_unknown_fields` too, which is also why ADDING this key breaks
    // the file for builds predating #1778: §5.3 restates merge-queue §11.2's
    // forward-compat property on purpose (the older build fails the parse of
    // the WHOLE file, down the loud `workflow-invalid` path — never a warning
    // that leaves the rest of the policy half-loaded).
    assert!(workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: worker\n    kind: worker\ndriverr:\n  enabled: true\n"
    )
    .is_err());
    // A value of the wrong type fails the whole file rather than resolving to
    // the default — policy fails loud (§5.3).
    assert!(workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: worker\n    kind: worker\ndriver:\n  enabled: 3\n"
    )
    .is_err());
}

// ───────────────── per-block model knobs: effort: / context: (#687) ─────────

/// The happy path, and the whole back-compat guarantee alongside it: a declared
/// knob survives parsing normalized, and an ABSENT one is the empty string —
/// which every emit path reads as "say nothing".
#[test]
fn block_effort_and_context_parse_as_closed_enums() {
    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n\
         \x20 - id: w\n    kind: worker\n    cli: claude\n    effort: xhigh\n    context: 1m\n",
    )
    .unwrap();
    let w = wf.block("w").unwrap();
    assert_eq!(w.effort, "xhigh");
    assert_eq!(w.context, "1m");

    // Normalized, not rejected, on case/whitespace — the `role_hint` shape.
    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: w\n    kind: worker\n    cli: claude\n    effort: \"  MAX \"\n",
    )
    .unwrap();
    assert_eq!(wf.block("w").unwrap().effort, "max");

    // Absent = empty = today's behavior, byte for byte.
    let wf = workflow::parse_workflow("version: 1\nblocks:\n  - id: w\n    kind: worker\n").unwrap();
    let w = wf.block("w").unwrap();
    assert_eq!((w.effort.as_str(), w.context.as_str()), ("", ""));

    // Every level in loomux's vocabulary is accepted on claude — a pin on the
    // vocabulary itself, so dropping one from `EFFORT_LEVELS` reddens here and
    // not only in the caps table's own test.
    for level in EFFORT_LEVELS {
        let wf = workflow::parse_workflow(&format!(
            "version: 1\nblocks:\n  - id: w\n    kind: worker\n    cli: claude\n    effort: {level}\n"
        ))
        .unwrap();
        assert_eq!(wf.block("w").unwrap().effort, *level);
    }
}

/// The refusal side, which is the half that matters: a knob loomux cannot
/// deliver is a **loud parse error**, never a silent no-op. The failure this
/// closes is the quiet one — a human writes `effort: xhigh` on a copilot block,
/// the file loads, and nothing anywhere ever tells them their reviewer is
/// thinking exactly as hard as it was before.
#[test]
fn a_knob_that_is_unknown_or_undeliverable_is_a_loud_parse_error() {
    // (1) Outside loomux's closed vocabulary — never coerced to a neighbour.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: w\n    kind: worker\n    cli: claude\n    effort: banana\n",
    )
    .unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("unknown effort \"banana\"")
            && e.contains("must be one of low, medium, high, xhigh, max")),
        "the error must name the value AND the vocabulary: {errs:?}"
    );
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: w\n    kind: worker\n    cli: claude\n    context: 2m\n",
    )
    .unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("unknown context \"2m\"") && e.contains("must be one of 1m")),
        "{errs:?}"
    );

    // (2) In the vocabulary, but not deliverable on THIS cli — and the error
    // quotes the vendor fact from the CLI's own capability row rather than
    // saying "unsupported".
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: w\n    kind: worker\n    cli: copilot\n    context: 1m\n",
    )
    .unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("cli \"copilot\" cannot set context \"1m\"")
            && e.contains(cli_caps("copilot").unwrap().context_note)),
        "the refusal must carry copilot's own reason: {errs:?}"
    );
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: r\n    kind: reviewer\n    cli: gemini\n    effort: high\n",
    )
    .unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("cli \"gemini\" cannot set effort \"high\"")
            && e.contains(cli_caps("gemini").unwrap().effort_note)),
        "{errs:?}"
    );
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: w\n    kind: worker\n    cli: copilot\n    effort: low\n",
    )
    .unwrap_err();
    assert!(errs.iter().any(|e| e.contains("cli \"copilot\" cannot set effort \"low\"")), "{errs:?}");

    // (3) A block that inherits the group's CLI defers the cli half — the
    // group default is not known at parse time (the same deferral
    // `cli_can_host` makes), and `Guardrails::clamped` re-checks it at spawn.
    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: w\n    kind: worker\n    effort: high\n",
    )
    .unwrap();
    assert_eq!(wf.block("w").unwrap().effort, "high");

    // (4) `deny_unknown_fields` still catches a typo'd key — the new keys are
    // declared fields, not a door that widens what else is accepted.
    assert!(workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: w\n    kind: worker\n    efort: high\n"
    )
    .is_err());
}

/// #782 — the refusal has to be an AUTHORING RAIL, not just a verdict.
///
/// A human gets the launcher, which greys an undeliverable knob out with its
/// reason attached; an agent writing `.loomux/workflow.yml` gets this string
/// and nothing else. So it must locate the mistake (which block, which knob,
/// which value), explain it in the vendor's own terms, AND say what to do
/// instead — otherwise the author's next move is a guess between deleting the
/// key and rewriting the block's `cli:`.
///
/// The remedy is derived from `CLI_CAPS`, never written per-CLI (CLAUDE.md
/// constraint 8): this asserts the suggested CLI list IS the set of spawnable
/// rows carrying that value, so wiring a knob on another CLI updates the
/// message with no test edit and no source edit.
#[test]
fn an_undeliverable_knob_names_the_block_the_value_the_reason_and_the_fix() {
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n\
         \x20 - id: orchestrator\n    kind: orchestrator\n    cli: claude\n\
         \x20 - id: rev-ui\n    kind: reviewer\n    cli: copilot\n    effort: xhigh\n",
    )
    .unwrap_err();
    let e = errs
        .iter()
        .find(|e| e.contains("effort"))
        .unwrap_or_else(|| panic!("no effort error at all: {errs:?}"));

    // WHERE: the block's index AND its id — a file with several copilot blocks
    // must not leave the author bisecting to find which one is meant.
    assert!(e.contains("blocks[1]") && e.contains("(rev-ui)"), "must locate the block: {e}");
    // WHAT: the knob and the exact value that was refused.
    assert!(e.contains("cannot set effort \"xhigh\""), "must name knob and value: {e}");
    // WHY: the vendor fact from copilot's own capability row, not "unsupported".
    assert!(e.contains(cli_caps("copilot").unwrap().effort_note), "must carry the reason: {e}");
    // HOW: both escapes, and the CLI list derived from CLI_CAPS itself.
    let can: Vec<&str> = CLI_CAPS
        .iter()
        .filter(|c| c.orchestration && c.effort_levels.contains(&"xhigh"))
        .map(|c| c.cli)
        .collect();
    assert!(!can.is_empty(), "the fixture assumes some spawnable cli can set effort");
    assert!(e.contains("drop the key"), "must offer the delete-it escape: {e}");
    assert!(
        e.contains(&can.join(", ")),
        "must offer the move-it escape, listing exactly the caps-derived CLIs {can:?}: {e}"
    );

    // A knob NO spawnable CLI can deliver must not invent an alternative — it
    // says so and offers only the escape that exists. `context: 1m` is that
    // case today (claude alone carries it), so this drives the branch by
    // asserting the message tracks the caps table rather than a literal.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: w\n    kind: worker\n    cli: gemini\n    context: 1m\n",
    )
    .unwrap_err();
    let e = errs.iter().find(|e| e.contains("context")).unwrap();
    let can: Vec<&str> = CLI_CAPS
        .iter()
        .filter(|c| c.orchestration && c.context_variants.contains(&"1m"))
        .map(|c| c.cli)
        .collect();
    if can.is_empty() {
        assert!(e.contains("no cli loomux spawns can set"), "{e}");
    } else {
        assert!(e.contains(&can.join(", ")), "{e}");
    }
    assert!(e.contains("drop the key"), "{e}");
}

/// #782 — a workflow file loomux refuses must never WEDGE a group.
///
/// The failure mode is the whole reason the knob check is allowed to be loud:
/// `load_workflow` returns the errors, the launcher's preview reports
/// `valid: false` with them attached, and a launch falls back to the built-in
/// roster. If a broken file could instead block a spawn, "loud" would mean
/// "unusable" and the check would have to be silent.
#[test]
fn a_refused_workflow_file_reports_errors_and_never_blocks_a_launch() {
    let repo = Repo::new().workflow(
        "version: 1\nblocks:\n  - id: w\n    kind: worker\n    cli: copilot\n    effort: high\n",
    );
    // Refused, WITH the reason — not `Ok` carrying half-parsed blocks, and not
    // a panic that would take the caller down with it.
    let errs = match workflow::load_workflow(&repo.path()) {
        Err(errs) => errs,
        other => panic!("a knob copilot cannot deliver must refuse the load: {other:?}"),
    };
    assert!(
        errs.iter().any(|e| e.contains("cannot set effort") && e.contains("drop the key")),
        "the refusal must carry the authoring rail out to the caller: {errs:?}"
    );

    // And the roster a launch then runs is the built-in one — synthesized
    // exactly as a repo with NO workflow file gets, so a refused file reads as
    // "no workflow", never as "no agents".
    let fallback = workflow::builtin_roster("copilot");
    assert!(
        !workflow::roster_is_custom(&fallback),
        "the fallback must be the built-in roster: {fallback:?}"
    );
    assert!(
        fallback.iter().any(|b| b.kind == Role::Orchestrator)
            && fallback.iter().any(|b| b.kind == Role::Worker),
        "the fallback roster must still be able to run work: {fallback:?}"
    );
}

/// The orchestrator block's pin list widens by exactly two value-set picks
/// (#687) — and by nothing else. This is the deliberate design change the PR
/// argues for in `docs/design/workflows.md`: a level from a closed enum authors
/// no text and pre-approves no tool, so it opens no injection seam into the
/// trust root, while `prompt:`/`profile:`/`allow:` stay refused.
#[test]
fn an_orchestrator_block_may_pin_effort_and_context_but_still_not_a_persona() {
    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n\
         \x20 - id: orchestrator\n    kind: orchestrator\n    cli: claude\n\
         \x20   model: opus\n    effort: max\n    context: 1m\n",
    )
    .unwrap();
    let o = wf.block("orchestrator").unwrap();
    assert_eq!((o.effort.as_str(), o.context.as_str()), ("max", "1m"));

    // The refusals the widening must NOT have loosened.
    for line in
        ["prompt: rewrite your contract", "profile: .github/agents/o.md", "allow: [\"Bash(x *)\"]"]
    {
        let errs = workflow::parse_workflow(&format!(
            "version: 1\nblocks:\n  - id: orchestrator\n    kind: orchestrator\n    {line}\n"
        ))
        .unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("loomux's trust root")),
            "{line:?} must still be refused on the orchestrator block: {errs:?}"
        );
    }
    // An invalid knob is refused on the orchestrator too — pinnable is not
    // unvalidated.
    assert!(workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: orchestrator\n    kind: orchestrator\n    cli: copilot\n    effort: max\n"
    )
    .is_err());
}

/// The persisted roster (`blocks_json` / `read_blocks`) is a SEPARATE wire
/// format from workflow.yml, so the knobs must survive it too — otherwise a
/// group resumed after an app restart would silently drop back to the CLI's
/// default thinking level. Mirrors `role_hint_round_trips_through_group_json_too`.
#[test]
fn block_knobs_round_trip_through_group_json_too() {
    let (reg, dir) = test_registry();
    let repo = Repo::new().workflow(
        "version: 1\nblocks:\n\
         \x20 - id: deep\n    kind: worker\n    cli: claude\n    effort: xhigh\n    context: 1m\n",
    );
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let deep = g.guardrails.block("deep").unwrap();
    assert_eq!((deep.effort.as_str(), deep.context.as_str()), ("xhigh", "1m"));

    let gj: Value = serde_json::from_str(
        &fs::read_to_string(reg.state_root().join(g.id.as_str()).join("group.json")).unwrap(),
    )
    .unwrap();
    let blocks = gj["guardrails"]["blocks"].as_array().unwrap();
    let persisted = blocks.iter().find(|b| b["id"] == "deep").unwrap();
    assert_eq!(persisted["effort"], "xhigh", "effort must be persisted, not dropped");
    assert_eq!(persisted["context"], "1m", "context must be persisted, not dropped");
    // The block that pinned nothing (loomux's guaranteed orchestrator) persists
    // as empty — the shape a pre-#687 group.json has by omission.
    let orch = blocks.iter().find(|b| b["id"] == "orchestrator").unwrap();
    assert_eq!((orch["effort"].as_str(), orch["context"].as_str()), (Some(""), Some("")));

    // `load_group_file` is the MIGRATION SEAM — what the orchestrator's
    // session-rejoin path reads to rebuild a group from disk with no launcher
    // form in sight (the same seam `a_pre_block_group_json_still_loads` drives).
    // A relaunch would not prove this: `create_group_ex` re-reads the workflow
    // FILE on a fresh launch, so it would be testing the parser twice.
    let (_, persisted_rails) = reg.load_group_file(&g.id).expect("group.json must load");
    let back = persisted_rails.block("deep").unwrap();
    assert_eq!((back.effort.as_str(), back.context.as_str()), ("xhigh", "1m"));

    let reg2 = relaunch_registry(dir.path());
    let g2 = reg2.create_group(&repo.path(), rails()).unwrap();
    assert_eq!(g2.guardrails.blocks, g.guardrails.blocks, "the roster must round-trip unchanged");
}

/// Back-compat as a test rather than as a hope: a `group.json` written before
/// #687 carries NO `effort`/`context` keys at all, and must come back off disk
/// with the knobs empty — i.e. running the command line it was launched with —
/// rather than with anything invented.
///
/// Driven through `load_group_file` + `clamped()`, which is the pair every
/// rejoin path actually runs (`create_group_ex` clamps at its top, and the
/// session-resume path reaches it through `create_orchestration_group`). The
/// specimen is hand-written rather than produced by this build, because every
/// file this build writes HAS the keys — a stripped-then-reloaded file would
/// stop being a pre-#687 specimen the moment `blocks_json` touched it.
#[test]
fn a_group_json_predating_the_knobs_loads_with_none() {
    let (reg, _d) = test_registry();
    let repo = Repo::new();
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let path = reg.state_root().join(g.id.as_str()).join("group.json");

    // Sanity first: this build DOES write both keys, so the specimen below is
    // genuinely the older shape and not just today's shape spelled differently.
    let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let first = &written["guardrails"]["blocks"][0];
    assert!(first.get("effort").is_some() && first.get("context").is_some());

    fs::write(
        &path,
        serde_json::to_string_pretty(&json!({
            "group_id": g.id,
            "repo": repo.path(),
            "created_ms": 1_700_000_000_000u64,
            "guardrails": {
                "max_agents": 6,
                "agent_cli": "claude",
                "blocks": [
                    // The pre-#687 shape: no effort/context keys at all.
                    { "id": "worker", "name": "worker", "kind": "worker",
                      "cli": "", "model": "", "prompt": null, "profile": null,
                      "allow": [], "role_hint": null },
                    // A hand edit that never met the parser: one knob loomux can
                    // honor, one it cannot. `clamped()` is the only thing standing
                    // between these and a spawn command line.
                    { "id": "deep", "name": "deep", "kind": "worker",
                      "cli": "claude", "model": "opus", "prompt": null, "profile": null,
                      "allow": [], "role_hint": null,
                      "effort": "max", "context": "9m" },
                ],
            },
        }))
        .unwrap(),
    )
    .unwrap();

    let (_, persisted) = reg.load_group_file(&g.id).expect("a pre-#687 group.json must still load");
    let worker = persisted.block("worker").unwrap();
    assert_eq!(
        (worker.effort.as_str(), worker.context.as_str()),
        ("", ""),
        "absent keys must read as no knob, never as an invented default"
    );

    let resolved = persisted.clamped();
    let worker = resolved.block("worker").unwrap();
    assert_eq!((worker.effort.as_str(), worker.context.as_str()), ("", ""));
    let deep = resolved.block("deep").unwrap();
    assert_eq!(deep.effort, "max", "a knob the resolved cli CAN honor survives the load");
    assert_eq!(deep.context, "", "a knob outside the vocabulary is dropped, not carried to argv");
}

// ───────── #858: the `resources:` block (named lock resources) ─────────
//
// What a repo declares as scarce, and what it may not declare. The engine is
// `orchestration::locks`; this file only ever pins what the FILE means, which
// is the half a repo author can get wrong.

#[test]
fn an_absent_resources_block_means_no_locks_at_all() {
    // The reversal mechanism and the "byte-for-byte unchanged" claim in one:
    // no block, no resources — and `mcp::tool_defs` keys the whole lock tool
    // surface off exactly this emptiness.
    let wf = workflow::parse_workflow("version: 1\nblocks:\n  - id: worker\n    kind: worker\n")
        .unwrap();
    assert!(wf.resources.is_empty());
}

#[test]
fn a_declared_resource_fills_in_the_defaults_it_omits() {
    // A repo that wants a mutex writes the name and nothing else — same shape
    // as `intake:`'s per-label fallback and `merge_queue:`'s.
    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: worker\n    kind: worker\nresources:\n  build: {}\n",
    )
    .unwrap();
    let p = wf.resources.get("build").expect("declared");
    assert_eq!(p.slots, workflow::RESOURCE_SLOTS_DEFAULT);
    assert_eq!(*p, workflow::ResourcePolicy::default());
    assert_eq!(p.slots, 1, "the useful default is a mutex; a semaphore is opt-in");
    assert_eq!(p.max_hold_minutes, workflow::RESOURCE_MAX_HOLD_MINUTES_DEFAULT);

    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
         resources:\n  build:\n    slots: 1\n    max_hold_minutes: 45\n  gpu:\n    slots: 2\n",
    )
    .unwrap();
    assert_eq!(wf.resources["build"].max_hold_minutes, 45);
    assert_eq!(wf.resources["gpu"].slots, 2);
    assert_eq!(
        wf.resources["gpu"].max_hold_minutes,
        workflow::RESOURCE_MAX_HOLD_MINUTES_DEFAULT,
        "one resource's explicit policy does not become another's"
    );
}

/// Every bad number is a hard ERROR, never a silent substitution — the
/// `merge_queue.max_batch` posture. A repo that wrote `slots: 0` believes its
/// builds are serialized; quietly handing it the default would leave that
/// belief in place while the behaviour changed underneath it.
#[test]
fn an_unusable_resource_policy_is_refused_rather_than_defaulted() {
    let cases: [(&str, &str, &str); 4] = [
        ("slots: 0", "resources.build.slots", "at least 1"),
        ("slots: 65", "resources.build.slots", "maximum of 64"),
        ("max_hold_minutes: 0", "resources.build.max_hold_minutes", "at least 1"),
        ("max_hold_minutes: 481", "resources.build.max_hold_minutes", "maximum of 480"),
    ];
    for (line, key, phrase) in cases {
        let errs = workflow::parse_workflow(&format!(
            "version: 1\nblocks:\n  - id: worker\n    kind: worker\nresources:\n  build:\n    {line}\n"
        ))
        .unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains(key) && e.contains(phrase)),
            "{line} must be refused naming {key}/{phrase}, got {errs:?}"
        );
    }
}

/// A name is REJECTED, never rewritten — the `blocks[].id` rule. An author who
/// wrote `heavy build` must not end up with a resource called `heavybuild`
/// that the `acquire_lock` call in their own worker brief cannot name.
#[test]
fn a_resource_name_outside_the_identifier_alphabet_is_rejected_not_sanitized() {
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: worker\n    kind: worker\nresources:\n  \"heavy build\": {}\n",
    )
    .unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("heavy build") && e.contains("not allowed")),
        "{errs:?}"
    );

    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: worker\n    kind: worker\nresources:\n  \"!!\": {}\n",
    )
    .unwrap_err();
    assert!(errs.iter().any(|e| e.contains("no usable characters")), "{errs:?}");
}

/// `deny_unknown_fields` on `RawResource`: a repo cannot smuggle a key this
/// build does not understand past the parse. Policy fails LOUD — the same
/// asymmetry `merge_queue:` states against machine-authored state.
#[test]
fn an_unknown_key_inside_a_resource_fails_the_whole_parse() {
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
         resources:\n  build:\n    slots: 1\n    exclusive_to: orchestrator\n",
    )
    .unwrap_err();
    assert!(errs.iter().any(|e| e.contains("exclusive_to")), "{errs:?}");
}

/// The cap on how many resources one repo may declare — every name is folded
/// into the `acquire_lock` description that every agent in the group reads, so
/// this bounds a per-agent context cost.
#[test]
fn more_resources_than_the_cap_are_refused() {
    let mut body = String::from("version: 1\nblocks:\n  - id: worker\n    kind: worker\nresources:\n");
    for i in 0..=workflow::RESOURCES_MAX {
        body.push_str(&format!("  r{i}: {{}}\n"));
    }
    let errs = workflow::parse_workflow(&body).unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("resources:") && e.contains("at most")),
        "{errs:?}"
    );
}

// ───────────────── board: per-status WIP limits (#1175 / #1170 A2) ─────────

/// A workflow with `body` appended, parsed.
fn parse_board(body: &str) -> Result<workflow::Workflow, Vec<String>> {
    workflow::parse_workflow(&format!(
        "version: 1\nblocks:\n  - id: worker\n    kind: worker\n{body}"
    ))
}

#[test]
fn an_absent_board_block_declares_no_caps_at_all() {
    // The opt-in guarantee: no `board:` and the feature is off, byte-for-byte —
    // the posture `merge_queue:` and `resources:` take. Asserted against the
    // `Default` the seam reads rather than against a hand-written expectation,
    // so "off" cannot come to mean two things.
    let wf = parse_board("").unwrap();
    assert_eq!(wf.board, workflow::BoardPolicy::default());
    assert!(wf.board.wip.is_empty(), "no caps");
    assert!(!wf.board.enforce, "and warn is the product default, not enforce");
    // A declared block with no caps is the same thing: `board:` on its own is
    // YAML null, which is "never declared", and an empty `wip:` mapping is a
    // repo that wrote the block and no limits.
    assert_eq!(parse_board("board:\n").unwrap().board, workflow::BoardPolicy::default());
}

#[test]
fn declared_caps_parse_by_their_wire_status_names() {
    let wf = parse_board(
        "board:\n  wip:\n    in-progress: 4\n    review: 3\n    human-testing: 2\n  enforce: true\n",
    )
    .unwrap();
    assert_eq!(wf.board.wip.get("in-progress"), Some(&4));
    assert_eq!(wf.board.wip.get("review"), Some(&3));
    // The two hyphenated statuses are the ones a struct field cannot spell, so
    // they are the ones a `rename` could silently get wrong — and a cap keyed
    // `human_testing` would match no board status and cap nothing, in silence.
    assert_eq!(wf.board.wip.get("human-testing"), Some(&2));
    assert_eq!(wf.board.wip.len(), 3, "a status the file omitted has NO cap, not a default one");
    assert!(wf.board.enforce);
}

#[test]
fn a_zero_cap_is_a_loud_error_rather_than_a_silent_default() {
    // Same posture as `merge_queue.max_batch: 0`: a repo that wrote `review: 0`
    // believes something about how its board paces, and quietly handing it "no
    // limit" would leave that belief in place while the behaviour went the
    // other way. Under `enforce` a 0 would additionally wedge the status shut.
    let errs = parse_board("board:\n  wip:\n    review: 0\n").unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("board.wip.review") && e.contains("at least 1")),
        "review: 0 must name itself in the error, got: {errs:?}"
    );
}

#[test]
fn a_misspelt_status_is_refused_and_the_error_names_the_ones_that_exist() {
    // This is the whole argument for a CLOSED struct over a `BTreeMap<String,
    // u32>`: an open key namespace cannot tell a typo from a status a newer
    // loomux might have, so `in-porgress: 4` would declare a limit on nothing,
    // in silence, for the lifetime of the file.
    let errs = parse_board("board:\n  wip:\n    in-porgress: 4\n").unwrap_err();
    let joined = errs.join(" ");
    assert!(joined.contains("in-porgress"), "the error names what was written: {errs:?}");
    assert!(
        joined.contains("in-progress") && joined.contains("review"),
        "…and what it could have written — serde's own unknown-field message is the \
         status list, which is why this module writes no check of its own: {errs:?}"
    );
}

#[test]
fn done_is_not_a_cappable_status() {
    // `done` is terminal and it is the relief valve: every other cap is
    // relieved by work reaching it, so a limit there would refuse the very
    // transition that unblocks the board. The wire struct simply has no field
    // for it, which makes this a parse error rather than a rule to remember.
    let errs = parse_board("board:\n  wip:\n    done: 3\n").unwrap_err();
    assert!(errs.join(" ").contains("done"), "the refusal names the key: {errs:?}");
    assert_eq!(
        workflow::WIP_UNCAPPABLE_STATUS, "done",
        "the constant the docs, the error path and this test all read"
    );
    // Not a blanket refusal of everything, though — the negative control that
    // keeps the assertion above from passing on a parser that rejects any cap.
    assert!(parse_board("board:\n  wip:\n    review: 3\n").is_ok());
}

#[test]
fn every_task_status_except_done_can_carry_a_cap() {
    // The drift pin. `RawWip`'s field list is a second copy of `TASK_STATUSES`,
    // which lives in `src-tauri` — on the other side of an arrow the engine
    // crate may not point back along — so it cannot be derived. Both
    // directions: a status that gained no field would arrive silently
    // uncappable, and a field naming no status would cap nothing.
    let declared: std::collections::BTreeSet<String> =
        workflow::workflow_schema_keys().remove("board.wip").expect("the wip section").into_iter().collect();
    let expected: std::collections::BTreeSet<String> = loomux_lib::orchestration::TASK_STATUSES
        .iter()
        .filter(|s| **s != workflow::WIP_UNCAPPABLE_STATUS)
        .map(|s| s.to_string())
        .collect();
    assert_eq!(
        declared, expected,
        "board.wip's fields must be exactly TASK_STATUSES minus {}",
        workflow::WIP_UNCAPPABLE_STATUS
    );
}

#[test]
fn the_board_block_can_never_widen_anything() {
    // The capability-closure rule, applied to the newest block. `RawBoard` and
    // `RawWip` are both `deny_unknown_fields`, so there is no spelling of
    // "merge without the gate", no key naming a branch, an agent or a program,
    // and no key at all this build does not recognize — an attempt is a hard
    // parse error, not an ignored line.
    for line in ["human_gate: false", "auto_merge: true", "reviewers: [w]", "enfroce: true"] {
        assert!(
            parse_board(&format!("board:\n  wip:\n    review: 2\n  {line}\n")).is_err(),
            "{line:?} must not be tolerated inside board:"
        );
    }
    // A mistyped block name is not silently ignored either.
    assert!(parse_board("bord:\n  wip:\n    review: 2\n").is_err());
    // Nor is a value of the wrong type — policy fails loud.
    assert!(parse_board("board:\n  wip:\n    review: soon\n").is_err());
    assert!(parse_board("board:\n  enforce: 3\n").is_err());
}
