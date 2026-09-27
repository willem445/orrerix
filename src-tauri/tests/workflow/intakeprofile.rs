//! Intake: schema and profile resolution, the hold label, persistence and the consent gate.
//!
//! One module of the `workflow` integration-test target (`main.rs`),
//! split out of the former single-file `tests/workflow.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ───────────────── intake: schema + profile resolution (#382 P1) ───────────
//
// The `intake:` block declares intake source + label vocabulary — the
// missing sibling of `gates:` ("where work comes from" beside "what gates
// it"). Three invariants this section defends:
//
// 1. **`deny_unknown_fields` makes any disable-spelling a hard parse error.**
//    `human_gate: false` (or any key this schema doesn't name) is not an
//    ignored line — it's a parse failure, at every nesting level the schema
//    offers. This is the CRITICAL invariant: there is no way to spell "skip
//    the human merge gate" through this file, by construction.
// 2. **A repo declaring nothing, or only part of an `intake:` block, resolves
//    to `builtin_intake_profile()`** — the golden-fixture dodge the plan
//    calls out for P2: the const and the parser are independent, so either
//    drifting is a visible test failure, not a render-against-itself
//    tautology.
// 3. **The resolved profile rides the SAME advanced-orchestrator consent gate
//    the roster override does** — a Fresh launch with the toggle on is the
//    only path that lets a repo's `intake:` block take effect; a resume is
//    pinned to what group.json already has, exactly like `blocks`.

#[test]
fn intake_schema_round_trips_from_yaml() {
    let yaml = r#"
version: 1
blocks:
  - id: worker
    kind: worker

intake:
  source: github-labels
  labels:
    ready: build-me
    investigate: look-only
    owned: mine
    prototype: demo-me
"#;
    let wf = workflow::parse_workflow(yaml).expect("a valid intake block must parse");
    assert_eq!(wf.intake.source, workflow::IntakeSource::GithubLabels);
    assert_eq!(wf.intake.ready, "build-me");
    assert_eq!(wf.intake.investigate, "look-only");
    assert_eq!(wf.intake.owned, "mine");
    assert_eq!(wf.intake.prototype, "demo-me");
}

#[test]
fn no_intake_block_resolves_to_the_builtin_default() {
    let wf =
        workflow::parse_workflow("version: 1\nblocks:\n  - id: worker\n    kind: worker\n").unwrap();
    assert_eq!(
        wf.intake,
        workflow::builtin_intake_profile(),
        "a file with no intake: block must resolve to the built-in profile, byte for byte"
    );
}

#[test]
fn a_declared_intake_block_can_override_one_label_and_inherit_the_rest() {
    let yaml = r#"
version: 1
blocks:
  - id: worker
    kind: worker

intake:
  labels:
    ready: build-this
"#;
    let wf = workflow::parse_workflow(yaml).unwrap();
    let builtin = workflow::builtin_intake_profile();
    assert_eq!(wf.intake.ready, "build-this", "the overridden label takes effect");
    assert_eq!(wf.intake.investigate, builtin.investigate, "an omitted label inherits the default");
    assert_eq!(wf.intake.owned, builtin.owned, "an omitted label inherits the default");
    assert_eq!(wf.intake.prototype, builtin.prototype, "an omitted label inherits the default");
    assert_eq!(wf.intake.source, workflow::IntakeSource::GithubLabels, "source omitted -> default");
}

#[test]
fn builtin_intake_profile_matches_todays_github_label_vocabulary() {
    // The plan's dodge for the golden self-reference trap: this const is
    // checked in independently of the parser (and, in P2, of the template
    // fixture) — so THIS pin is what makes renaming a default label string a
    // visible test failure rather than something the schema quietly accepts.
    let p = workflow::builtin_intake_profile();
    assert_eq!(p.source, workflow::IntakeSource::GithubLabels);
    assert_eq!(p.ready, "agent-ready");
    assert_eq!(p.investigate, "agent-investigation");
    assert_eq!(p.owned, "agent-managed");
    assert_eq!(p.prototype, "agent-prototype");
    assert_eq!(p.hold, "agent-hold", "the full-autonomy veto label (#778) is part of the built-in vocabulary");
}

// ── the hold label: the full-autonomy veto's spelling (#778) ───────────────
//
// Additive to the #382 P1 schema and defaulted, so every existing file and
// every existing group.json keeps working untouched — but it is the one label
// whose spelling is a **consent boundary** (the host poller excludes
// hold-labeled issues from full-autonomy eligibility), so a repo that renames
// it must have ITS spelling honored rather than a hardcoded const's.

#[test]
fn the_hold_label_can_be_overridden_and_is_inherited_when_omitted() {
    let yaml = "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
                intake:\n  labels:\n    hold: do-not-touch\n";
    let wf = workflow::parse_workflow(yaml).unwrap();
    assert_eq!(wf.intake.hold, "do-not-touch", "a repo's own veto-label spelling must take effect");
    assert_eq!(wf.intake.ready, workflow::builtin_intake_profile().ready, "the other labels still inherit");

    let plain = workflow::parse_workflow("version: 1\nblocks:\n  - id: worker\n    kind: worker\nintake:\n  labels:\n    ready: build-me\n").unwrap();
    assert_eq!(
        plain.intake.hold,
        workflow::builtin_intake_profile().hold,
        "an omitted hold: inherits the built-in default, like every other label field"
    );
}

#[test]
fn an_unusable_hold_label_is_rejected_not_rewritten() {
    // Same "reject, don't rewrite" rule the other label fields get — and it
    // matters more here: a silently-rewritten veto label would match nothing
    // in the repo, so every held issue would read as eligible.
    let yaml = "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
                intake:\n  labels:\n    hold: \"not a label\"\n";
    let errs = workflow::parse_workflow(yaml).unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("intake.labels.hold")),
        "the error must name the offending field: {errs:?}"
    );
}

#[test]
fn the_hold_label_round_trips_through_group_json() {
    let (reg, dir) = test_registry();
    let yaml = "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
                intake:\n  labels:\n    hold: custom-hold\n";
    let repo = Repo::new().workflow(yaml);
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    assert_eq!(g.guardrails.intake.hold, "custom-hold");

    let gj: Value = serde_json::from_str(
        &fs::read_to_string(reg.state_root().join(g.id.as_str()).join("group.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(gj["guardrails"]["intake"]["labels"]["hold"], "custom-hold");

    // A restart must resume on the same veto spelling — a poller that fell
    // back to `agent-hold` here would ignore every hold the human applied.
    let reg2 = relaunch_registry(dir.path());
    reg2.set_port(45999); // fake port, as everywhere in these tests
    let g2 = reg2.create_group(&repo.path(), rails()).unwrap();
    assert_eq!(g2.id, g.id, "the restart resumes the same group");
    assert_eq!(g2.guardrails.intake.hold, "custom-hold", "the veto spelling must survive a restart");
}

/// **The veto is only a veto if every surface that names it agrees (#778, rev
/// round 1 B1).** The poller honoring `intake.labels.hold` while the contract
/// and the UI hardcoded `agent-hold` was worse than not supporting the rename at
/// all: the orchestrator's triage plan is built from its OWN sweep, and its only
/// exclusion was a literal the repo no longer used — so a held issue landed in
/// the plan, the human's "go" covered it, and a vetoed issue got started.
///
/// Three surfaces, one fixture repo that renamed the veto, asserted together
/// because agreement is the property (any one of them alone still passes while
/// the veto is broken):
///
/// 1. the **contract** the orchestrator reads names the repo's spelling, and
///    does not mention the built-in anywhere;
/// 2. the **poller** honors it (`eligible_deltas` via the resolved profile);
/// 3. the **allow-list** permits writing it — the UI's one-click gesture.
#[test]
fn a_renamed_veto_reaches_the_contract_the_poller_and_the_allow_list_alike() {
    let (reg, _d) = test_registry();
    let repo = Repo::new().workflow(
        "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
         intake:\n  labels:\n    hold: do-not-touch\n",
    );
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    assert_eq!(g.guardrails.intake.hold, "do-not-touch", "fixture sanity");

    // 1. The contract. `instructions_lf` reads the file the orchestrator is
    //    actually pointed at, after substitution — not the template.
    let contract = instructions_lf(&reg, &g.id, "orchestrator.md");
    assert!(
        contract.contains("do-not-touch"),
        "the orchestrator contract must name THIS repo's veto: under full autonomy the contract \
         is the consent boundary, and an exclusion it cannot name is one it will not apply"
    );
    assert!(
        !contract.contains("agent-hold"),
        "and it must not also name the built-in: two spellings in one contract is worse than \
         one wrong spelling — the agent gets to choose which veto to believe"
    );
    assert!(!contract.contains("{{HOLD_LABEL}}"), "the placeholder must be substituted, not shipped");

    // 2. The poller. An issue carrying the repo's veto is not eligible; one
    //    carrying the BUILT-IN spelling is, because here that label means
    //    nothing.
    let raw_issue = |number: u64, title: &str, labels: &[&str]| intake::RawIssue {
        number,
        title: title.to_string(),
        labels: labels.iter().map(|s| s.to_string()).collect(),
    };
    let mut seen = HashSet::new();
    let issues = vec![
        raw_issue(1, "held by the human", &["do-not-touch"]),
        raw_issue(2, "not actually held", &["agent-hold"]),
        raw_issue(3, "plain", &[]),
    ];
    let eligible = intake::eligible_deltas(
        &mut seen,
        true,
        Some(intake::OpenIssueList { issues: &issues, complete: true }),
        &g.guardrails.intake.hold,
        &HashSet::new(),
    );
    let mut got: Vec<u64> = eligible.iter().map(|s| s.number).collect();
    got.sort_unstable();
    assert_eq!(got, vec![2, 3], "the repo's own spelling is the veto the poller honors");

    // 1b. The kickoff clause, which is the OTHER half of the contract: a fresh
    //     boot or resume has no toggle notice to have seen, so this clause is
    //     where it learns the veto's name. Same hardcode, same consequence.
    reg.set_autonomous(&g.id, true).unwrap();
    reg.set_full_autonomy(&g.id, true, "harden any bugs").unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let entry = reg.agent(&orch.id).unwrap();
    let info = reg.group(&g.id).unwrap();
    let kickoff = reg.kickoff_prompt(&entry, &info, "", None);
    assert!(
        kickoff.contains("do-not-touch is the absolute human veto"),
        "the kickoff clause must name THIS repo's veto: {kickoff}"
    );
    assert!(
        !kickoff.contains("agent-hold"),
        "and must not also name the built-in: {kickoff}"
    );

    // 1c. The group panel, via `orch_autonomy` (rev round 2). Its full-autonomy
    //     help and mode chip both INSTRUCT — "label X to hold it back" — so a
    //     panel naming the built-in tells the human of this repo to apply a
    //     label its own poller ignores. The panel has no workflow parser; it
    //     renders what this field says.
    let state = reg.autonomy_state(&g.id);
    assert_eq!(
        state["hold_label"].as_str(),
        Some("do-not-touch"),
        "orch_autonomy must report THIS group's veto spelling, or the panel instructs the human \
         to apply a label that holds nothing: {state}"
    );

    // 3. The seam the write side stands on, and #2663 narrowed WHICH side it
    //    holds up. `gh.rs` takes an `Option<&Guardrails>` now: a pane inside a
    //    group hands it one and the spelling comes from `guardrails.intake.hold`
    //    directly, so for THAT caller the two resolutions are one value and
    //    cannot disagree. A plain pane still passes `None`, and its arm still
    //    resolves the repo's `default` file — so this agreement is what the
    //    allow-list's correctness rests on for the no-group caller, which is the
    //    half a live group cannot cover. Pinned here because it is the one link
    //    the two sides' own tests cannot see between them. (The allow-list's own
    //    closed-ness is
    //    `a_resolved_hold_spelling_widens_the_allow_list_by_exactly_one_value`;
    //    the group-scoped half is
    //    `an_applied_workflow_renaming_the_hold_veto_moves_every_label_surface`
    //    in `tests/orchestration/`.)
    let from_file = workflow::load_workflow(&repo.path()).unwrap().unwrap().intake.hold;
    assert_eq!(
        from_file, g.guardrails.intake.hold,
        "the repo-file resolution a NO-GROUP gh.rs caller uses must equal the group's for a \
         group launched on that file — if these can differ, a plain pane's issues view writes \
         one spelling while this group's poller honors another"
    );
}

#[test]
fn a_group_json_predating_the_hold_label_resolves_to_the_builtin_veto() {
    // Migration guarantee, the same one `absent_intake_key_in_group_json_…`
    // gives the whole block: a group.json written before this field existed
    // has `intake.labels` with no `hold` key at all, and must resolve to
    // `agent-hold` rather than to an empty string that would match nothing.
    let (reg, _d) = test_registry();
    let g = reg.create_group(&Repo::new().path(), rails()).unwrap();
    let path = reg.state_root().join(g.id.as_str()).join("group.json");
    let mut gj: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    gj["guardrails"]["intake"]["labels"].as_object_mut().unwrap().remove("hold");
    fs::write(&path, serde_json::to_string_pretty(&gj).unwrap()).unwrap();

    let (_, persisted) = reg.load_group_file(&g.id).unwrap();
    assert_eq!(
        persisted.intake.hold,
        workflow::builtin_intake_profile().hold,
        "an absent hold key must resolve to the built-in veto label, never to nothing"
    );
}

/// **A hand-edited `group.json` cannot pin a flag-shaped veto** (#2663).
///
/// `guardrails.intake.hold` is the one path into this field that never met
/// `parse_workflow`, and `read_intake` used to decide it with a local
/// `sanitize_id` comparison. `sanitize_id` permits a leading `-` (it is the
/// block-id alphabet, and CLAUDE.md constraint 6 says so out loud), while the
/// parser's `sanitize_intake_label` refuses one — so the two rules disagreed on
/// exactly this value class, invisibly, for as long as the field reached prose
/// surfaces only.
///
/// #2663 gives it a `gh label create <name>` POSITIONAL, where a leading dash
/// is read by cobra as a flag. Both callers now ask
/// `workflow::usable_intake_label`, which is that same accept condition lifted
/// out of the parser rather than restated beside it.
///
/// The fallback is the BUILT-IN, per field, which is what `read_intake` already
/// did for every other unusable value — a rejection here must not leave the
/// group with no veto spelling at all.
#[test]
fn a_flag_shaped_hold_in_a_hand_edited_group_json_resolves_to_the_builtin_veto() {
    let (reg, _d) = test_registry();
    let g = reg.create_group(&Repo::new().path(), rails()).unwrap();
    let path = reg.state_root().join(g.id.as_str()).join("group.json");

    let write_hold = |hold: &str| {
        let mut gj: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        gj["guardrails"]["intake"]["labels"]["hold"] = json!(hold);
        fs::write(&path, serde_json::to_string_pretty(&gj).unwrap()).unwrap();
    };

    for evil in ["--force", "-x", "--"] {
        write_hold(evil);
        let (_, persisted) = reg.load_group_file(&g.id).unwrap();
        assert_eq!(
            persisted.intake.hold,
            workflow::builtin_intake_profile().hold,
            "{evil:?} is flag-shaped and must fall back to the built-in veto"
        );
    }

    // The positive control, through the SAME write-and-reload path: a rename
    // that is merely unusual survives it, so the loop above is refusing this
    // value class rather than refusing every hand-edited spelling.
    write_hold("do-not-touch");
    let (_, persisted) = reg.load_group_file(&g.id).unwrap();
    assert_eq!(persisted.intake.hold, "do-not-touch");
}

#[test]
fn unknown_intake_source_is_rejected_never_coerced() {
    let yaml = "version: 1\nblocks:\n  - id: worker\n    kind: worker\nintake:\n  source: gitlab\n";
    let errs = workflow::parse_workflow(yaml).unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("gitlab") && e.contains("github-labels")),
        "the error must name the bad value AND the allowed set: {errs:?}"
    );
}

#[test]
fn board_and_none_sources_parse_as_schema_reserved_but_unwired() {
    // Phase A designs board/none into the schema so the config contract never
    // churns when Phase B builds their runtime — they must parse cleanly
    // today even though nothing yet reads them (no P2/P4 wiring in this PR).
    for (src, want) in
        [("board", workflow::IntakeSource::Board), ("none", workflow::IntakeSource::None)]
    {
        let yaml = format!(
            "version: 1\nblocks:\n  - id: worker\n    kind: worker\nintake:\n  source: {src}\n"
        );
        let wf = workflow::parse_workflow(&yaml).unwrap();
        assert_eq!(wf.intake.source, want, "source: {src} must parse");
    }
}

#[test]
fn an_intake_label_with_unusable_characters_is_rejected_not_rewritten() {
    // Same "reject, don't rewrite" rule a block id gets: an author who wrote a
    // label with a space must see an error, not silently get a different
    // string their own repo's actual GitHub labels no longer match.
    let yaml = "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
                intake:\n  labels:\n    ready: \"not a label\"\n";
    let errs = workflow::parse_workflow(yaml).unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("intake.labels.ready")),
        "the error must name the offending field: {errs:?}"
    );
}

/// **A flag-shaped label is refused, which is what makes the argv claim true**
/// (rev-648 NB4). `sanitize_id`'s alphabet allows `-` freely, so `--force` and
/// `-x` passed it unchanged and became a resolved label — and the hold spelling
/// reaches `gh label create <name> …` as a POSITIONAL argument, where cobra
/// reads a leading dash as an unknown flag.
///
/// That was never an injection: nothing is executed and the create fails loudly.
/// But `gh.rs` justified its allow-list with "nothing shell-ish or `--flag`-shaped
/// can reach an argv through this door", and a safety claim that isn't true is
/// worth less than no claim — a later reader relies on it. Refusing the class
/// here is what makes it true at the boundary that states it.
#[test]
fn an_intake_label_may_not_begin_with_a_dash() {
    for bad in ["--force", "-x", "--", "-agent-hold"] {
        let yaml = format!(
            "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
             intake:\n  labels:\n    hold: \"{bad}\"\n"
        );
        let errs = workflow::parse_workflow(&yaml).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("intake.labels.hold")),
            "{bad:?} must be refused, naming the field: {errs:?}"
        );
        assert!(
            errs.iter().any(|e| e.contains("may not begin with")),
            "the error must say WHY, so an author can fix it: {errs:?}"
        );
    }

    // The rule is a LEADING dash only: the built-in vocabulary and every
    // plausible rename are interior-dashed, and refusing those would break the
    // default install.
    for good in ["agent-hold", "do-not-touch", "hold_me", "HOLD2", "a-"] {
        let yaml = format!(
            "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
             intake:\n  labels:\n    hold: \"{good}\"\n"
        );
        let wf = workflow::parse_workflow(&yaml)
            .unwrap_or_else(|e| panic!("{good:?} must still parse: {e:?}"));
        assert_eq!(wf.intake.hold, good);
    }

    // Every label field, not just the veto — a leading dash is nonsense for all
    // five, and the one that reaches an argv is not the only one that would.
    for field in ["ready", "investigate", "owned", "prototype"] {
        let yaml = format!(
            "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
             intake:\n  labels:\n    {field}: \"--force\"\n"
        );
        let errs = workflow::parse_workflow(&yaml).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains(&format!("intake.labels.{field}"))),
            "{field} must refuse a flag-shaped label too: {errs:?}"
        );
    }
}

#[test]
fn intake_human_gate_spelling_is_a_deny_unknown_fields_error() {
    // THE CRITICAL invariant. There is no spelling under `intake:` that
    // disables the human merge gate — the gate lives in the `gh` shim, keyed
    // to group markers, and is not reachable from this schema at all.
    // `deny_unknown_fields` on `RawIntake` is what turns any attempt at one
    // into a hard parse error rather than a silently ignored line.
    let yaml = "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
                intake:\n  source: github-labels\n  human_gate: false\n";
    let errs = workflow::parse_workflow(yaml).unwrap_err();
    assert!(
        !errs.is_empty(),
        "a human_gate: false spelling under intake: must be a parse error, not an ignored line"
    );
    assert!(
        errs.iter().any(|e| e.to_lowercase().contains("human_gate") || e.contains("unknown field")),
        "the error should point at the unrecognized key: {errs:?}"
    );
}

#[test]
fn intake_labels_human_gate_spelling_is_also_rejected() {
    // Same invariant, the nested spelling — `intake.labels.human_gate: false`
    // must be equally unreachable, not just the top level of `intake:`.
    let yaml = "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
                intake:\n  labels:\n    ready: agent-ready\n    human_gate: false\n";
    let errs = workflow::parse_workflow(yaml).unwrap_err();
    assert!(!errs.is_empty(), "a human_gate spelling inside intake.labels: must be rejected: {errs:?}");
}

#[test]
fn no_top_level_spelling_disables_the_human_gate_either() {
    // The invariant restated at the workflow root, alongside intake: and
    // gates:. RawWorkflow's own deny_unknown_fields already enforces this,
    // but the CRITICAL invariant asks for an explicit test naming it rather
    // than leaving it as an incidental pass-through of a pre-existing
    // property.
    let yaml = "version: 1\nblocks:\n  - id: worker\n    kind: worker\nhuman_gate: false\n";
    let errs = workflow::parse_workflow(yaml).unwrap_err();
    assert!(!errs.is_empty(), "a top-level human_gate: false must be a parse error: {errs:?}");
}

// ── persistence + the advanced-orchestrator consent gate (#382 P1/P3) ──────

#[test]
fn absent_intake_key_in_group_json_resolves_to_the_builtin_profile() {
    // Migration guarantee: a group.json written before this field existed —
    // or one where the repo declared nothing — rejoins on the built-in
    // default, byte for byte, exactly like #222's `blocks`.
    let (reg, _d) = test_registry();
    let repo = Repo::new();
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    assert_eq!(g.guardrails.intake, workflow::builtin_intake_profile());

    let (_, persisted) = reg.load_group_file(&g.id).unwrap();
    assert_eq!(persisted.intake, workflow::builtin_intake_profile());
}

#[test]
fn the_resolved_intake_profile_is_available_even_when_the_toggle_is_off() {
    // #382 plan §3: autonomous mode can run with the built-in roster, so a
    // consumer (the #332 host poller) must always have a profile to read —
    // not only when the advanced orchestrator is in play.
    let (reg, _d) = test_registry();
    let g = reg.create_group(&Repo::new().path(), plain_rails()).unwrap();
    assert_eq!(g.guardrails.intake, workflow::builtin_intake_profile());
}

#[test]
fn a_fresh_advanced_launch_with_a_declared_intake_overrides_the_builtin_profile() {
    let (reg, _d) = test_registry();
    let yaml = "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
                intake:\n  labels:\n    ready: build-this\n";
    let repo = Repo::new().workflow(yaml);
    let g = reg.create_group(&repo.path(), rails()).unwrap(); // advanced ON, Launch::Fresh
    assert_eq!(g.guardrails.intake.ready, "build-this");
    assert_eq!(
        g.guardrails.intake.investigate,
        workflow::builtin_intake_profile().investigate,
        "an omitted label still inherits the built-in default"
    );
}

#[test]
fn the_toggle_off_ignores_a_declared_intake_profile_entirely() {
    // Mirrors `the_toggle_off_ignores_a_declared_workflow_entirely` for the
    // roster: the repo declares a custom vocabulary, the human did not opt
    // in, and NONE of it may reach the group.
    let (reg, _d) = test_registry();
    let yaml = "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
                intake:\n  labels:\n    ready: totally-different-label\n";
    let repo = Repo::new().workflow(yaml);
    let g = reg.create_group(&repo.path(), plain_rails()).unwrap();
    assert_eq!(
        g.guardrails.intake,
        workflow::builtin_intake_profile(),
        "the toggle is off — the file's intake block must not reach the group"
    );
}

#[test]
fn intake_profile_round_trips_through_group_json() {
    let (reg, dir) = test_registry();
    let yaml = "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
                intake:\n  labels:\n    ready: custom-ready\n    owned: custom-owned\n";
    let repo = Repo::new().workflow(yaml);
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    assert_eq!(g.guardrails.intake.ready, "custom-ready");

    let gj: Value = serde_json::from_str(
        &fs::read_to_string(reg.state_root().join(g.id.as_str()).join("group.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(gj["guardrails"]["intake"]["labels"]["ready"], "custom-ready");
    assert_eq!(gj["guardrails"]["intake"]["labels"]["owned"], "custom-owned");
    assert_eq!(gj["guardrails"]["intake"]["source"], "github-labels");

    // A fresh registry (an app restart) reads it back identically — the
    // persisted profile round-trips unchanged, same shape as
    // `block_map_round_trips_through_group_json`.
    let reg2 = relaunch_registry(dir.path());
    reg2.set_port(45999);
    let g2 = reg2.create_group(&repo.path(), rails()).unwrap();
    assert_eq!(g2.id, g.id, "the restart resumes the same group");
    assert_eq!(g2.guardrails.intake, g.guardrails.intake, "the profile must round-trip unchanged");
}

#[test]
fn a_resumed_group_runs_the_intake_profile_it_was_launched_with_not_the_file_as_it_is_now() {
    // Mirrors the roster's resume-pin test (rev-11 F2): a `git pull` between
    // launch and resume must not be able to swap the intake vocabulary a
    // human never consented to under a session already running.
    let (reg, _d) = test_registry();
    let yaml = "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
                intake:\n  labels:\n    ready: launch-time-label\n";
    let repo = Repo::new().workflow(yaml);
    let launched = reg.create_group(&repo.path(), rails()).unwrap();
    assert_eq!(launched.guardrails.intake.ready, "launch-time-label");

    // The repo moves on after launch — a reviewer/label vocabulary the human
    // never saw.
    fs::write(
        Path::new(&repo.path()).join(".loomux").join("workflow.yml"),
        "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
         intake:\n  labels:\n    ready: post-launch-label\n",
    )
    .unwrap();

    let (repo_path, persisted) = reg.load_group_file(&launched.id).expect("group.json");
    let resumed =
        reg.create_group_ex(&repo_path, persisted, Launch::Resume).expect("a resume must not fail");

    assert_eq!(
        resumed.guardrails.intake.ready, "launch-time-label",
        "the resumed group must keep the profile its human approved, not the file as it is now"
    );
}

#[test]
fn intake_only_drift_is_audited_even_when_the_roster_is_unchanged() {
    // rev-26 NB2. `audit_workflow_drift` used to compare only `g.blocks` — a
    // repo that renamed its label vocabulary WITHOUT touching a single block
    // produced no `workflow-changed-since-launch` audit, even though a
    // roster change under identical circumstances would. Intake drifts
    // independently, and the human-visible drift notice must say so.
    let (reg, _d) = test_registry();
    let yaml = "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
                intake:\n  labels:\n    ready: launch-time-label\n";
    let repo = Repo::new().workflow(yaml);
    let launched = reg.create_group(&repo.path(), rails()).unwrap();
    assert_eq!(launched.guardrails.intake.ready, "launch-time-label");
    let launched_block_ids: Vec<&str> =
        launched.guardrails.blocks.iter().map(|b| b.id.as_str()).collect();

    // The repo edits ONLY the intake vocabulary — the roster is untouched.
    fs::write(
        Path::new(&repo.path()).join(".loomux").join("workflow.yml"),
        "version: 1\nblocks:\n  - id: worker\n    kind: worker\n\
         intake:\n  labels:\n    ready: post-launch-label\n",
    )
    .unwrap();

    let (repo_path, persisted) = reg.load_group_file(&launched.id).expect("group.json");
    let resumed =
        reg.create_group_ex(&repo_path, persisted, Launch::Resume).expect("a resume must not fail");

    // The roster itself did not move...
    let resumed_block_ids: Vec<&str> =
        resumed.guardrails.blocks.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(resumed_block_ids, launched_block_ids, "the roster is unchanged");

    // ...but the intake-only edit must still be audited as drift — a
    // blocks-only comparison would have stayed silent here, which is exactly
    // the gap this test exists to close.
    let drift: Value = audit_entries(&reg, &launched.id)
        .into_iter()
        .find(|v| v["action"] == "workflow-changed-since-launch")
        .expect("an intake-only edit must be audited even though the roster didn't move");
    assert_eq!(
        drift["detail"]["intake_running"]["labels"]["ready"], "launch-time-label",
        "the audit must say what's RUNNING"
    );
    assert_eq!(
        drift["detail"]["intake_on_disk"]["labels"]["ready"], "post-launch-label",
        "...and what the file now says"
    );
}
