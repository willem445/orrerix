//! Loomux's own `.orrerix/workflow.yml` against the real parser, and what a block's `model:` is worth.
//!
//! One module of the `workflow` integration-test target (`main.rs`),
//! split out of the former single-file `tests/workflow.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ────────── loomux's own workflow, and what a block's model: is worth ─────────
//
// The repo dogfoods the feature (#222): `.orrerix/workflow.yml` at the root declares
// loomux's own roster, and the tests below are what keep that file honest. They check
// that the roster is VALID — it loads through the real parser, every CLI can host its
// class, every persona loads, every gate reviewer exists, and every block launches on
// what it declares — and never WHAT it declares (#3507): which cli, model or effort a
// block runs, and which blocks exist, are the operator's to edit without turning main red.
// The pane's half of the same pin lives in `test/workflowdogfood.test.ts`.

/// The loomux repo root (the crate's manifest dir is `src-tauri/`).
pub(crate) fn repo_root() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("src-tauri always has a parent")
        .to_string_lossy()
        .replace('\\', "/")
}

#[test]
fn the_repos_own_workflow_file_parses_clean_against_the_real_parser() {
    // Schema drift in CI, forever. A workflow file is only worth shipping if the
    // engine that runs it accepts it — and this asserts that against the REAL parser
    // and the REAL persona loader, not a copy of them.
    let repo = repo_root();
    let wf = match workflow::load_workflow(&repo) {
        Ok(Some(wf)) => wf,
        Ok(None) => panic!("the repo must ship its own {}", workflow::workflow_path(&repo)),
        Err(errors) => panic!("loomux's own workflow file does not validate: {errors:#?}"),
    };

    // The roster can do the work at all — and nothing here pins WHICH blocks do it, or on
    // what cli, model or effort (#3507): those are the operator's to edit, and the
    // end-to-end pin below derives them from this file rather than restating them.
    assert!(wf.blocks.iter().any(|b| b.kind == Role::Worker), "a usable roster needs a worker block");
    assert!(wf.blocks.iter().any(|b| b.kind == Role::Reviewer), "a usable roster needs a reviewer block");

    // And every one of them is a class this CLI may actually host — the containment
    // gate the parser itself consults, re-asserted here against the real file so a
    // future roster cannot pair a class with a CLI that cannot contain it.
    for b in wf.blocks.iter().filter(|b| !b.cli.is_empty()) {
        loomux_lib::orchestration::cli_can_host(&b.cli, b.kind)
            .unwrap_or_else(|e| panic!("{}: {e}", b.id));
    }

    let adv = workflow::parse_workflow(
        "version: 1
blocks:
  - id: helper
    kind: planner
    role_hint: advisor
",
    )
    .unwrap();
    assert_eq!(adv.blocks[0].role_hint.as_deref(), Some("advisor"), "planner+advisor stays the legal pairing");

    // No `merge_queue:` value is pinned (#3507): arming or un-arming the queue is the
    // operator's one-line edit, and the parser already refuses an out-of-range setting.
    // The product DEFAULT stays off, pinned by
    // `an_absent_merge_queue_block_means_the_feature_is_off` on a synthetic specimen.

    for b in &wf.blocks {
        let Some(rel) = b.profile.as_deref() else { continue };
        // The persona file exists, has frontmatter and a body, and declares the SAME
        // capability class as the block using it — the compatibility check that stops a
        // reviewer persona from being pointed at by a worker block (and vice versa).
        // Neither its `mode:` nor its location is pinned (#3507): `append` and `replace`
        // are both valid for any block, and a persona outside `.github/agents/` is valid
        // too — a copilot block then gets a kickoff paste rather than the native handle.
        let p = profiles::load_block_profile(&repo, rel, b.kind)
            .unwrap_or_else(|e| panic!("{}: {e}", b.id));
        // What IS owed, wherever the file lives in Copilot's own convention: the native
        // `--agent <name>` a `cli: copilot` block would get must resolve back,
        // unambiguously, to the file just read — or copilot would load a different persona.
        if profiles::is_copilot_native(rel) {
            let handle = p.copilot_agent.as_deref().unwrap_or(&p.name);
            assert!(
                profiles::handle_resolves_to(&repo, handle, rel),
                "{}: `copilot --agent {handle}` must load {rel} and nothing else",
                b.id
            );
        }
    }

    // Nothing the roster normalization drops: `clamped()` re-enforces the reserved-id
    // rule and id uniqueness on rosters that never met the parser, and a block silently
    // dropped there would be a delegate the human saw in the preview and never got.
    let ids: Vec<String> = wf.blocks.iter().map(|b| b.id.clone()).collect();
    let clamped = Guardrails { blocks: wf.blocks.clone(), ..rails() }.clamped();
    assert_eq!(clamped.blocks.iter().map(|b| b.id.clone()).collect::<Vec<_>>(), ids);

    // Everything below is about the merge gate, and a file with no gate is valid (#3507):
    // no verdict is then required of anyone, so there is nothing for it to hold.
    let Some(gate) = wf.gates.get("merge") else { return };
    // A BARE `spawn_agent(kind: "reviewer")` lands on an every-round lane. Block ORDER is the
    // operator's (#3507), but `block_for` takes the FIRST reviewing block, so a lane the gate's
    // static list does not name — one routing adds only on some paths — moved first would make
    // the default review the wrong lane on every PR with nothing else red. Membership, never a
    // name or a position: asked through the real resolver, on the roster a group would run.
    if !gate.reviewers.is_empty() {
        let bare = clamped
            .block_for(Role::Reviewer)
            .expect("a roster whose gate names reviewers resolves a bare reviewer spawn");
        assert!(
            gate.reviewers.contains(&bare.id),
            "a bare reviewer spawn resolves to {:?}, which the gate does not require every round ({:?}) — \
             put a gated lane first",
            bare.id,
            gate.reviewers
        );
    }
    // No `require:` value is pinned: under ALL-PASS every named reviewer must speak, so
    // the need is the static list's length — the property, stated for whichever rule the
    // file chose. (#1176 refuses `routing:` beside `require: threshold` at parse.)
    if gate.require == GateRequire::AllPass {
        assert_eq!(
            workflow::gate_need(gate),
            gate.reviewers.len() as u32,
            "every named reviewer must have to speak — abstention is a pass, so a threshold would let \
             the lanes that didn't review it open the gate ahead of the lane that must"
        );
    }
    // The GENERIC safety property, same shape as the frontend dogfood pin — and it is
    // NAMEDNESS, not reachability, so say so rather than overclaim. Stated over a UNION
    // because that is what #1176 made the required set: every declared reviewer-kind
    // block must be named by `gates.merge.reviewers` OR by at least one `routing:`
    // rule. A lane named by NEITHER can never enter the required set at all — it would
    // sit in the roster looking wired while the gate opened without it, which is exactly
    // what "an abstention is a pass" makes dangerous. A lane required only on some paths
    // is the routing working, not a hole.
    //
    // What namedness does NOT catch, found in review (rev-final round 1, N4) rather than
    // by the author: a rule whose `paths:` match nothing still NAMES its reviewer, so the
    // lane is named and required on no PR — the same end state from the other side. The
    // `must be able to FIRE` block below is the partial close, with its own residual.
    let declared: Vec<&str> =
        wf.blocks.iter().filter(|b| b.kind == Role::Reviewer).map(|b| b.id.as_str()).collect();
    let reachable: std::collections::BTreeSet<&str> = gate
        .reviewers
        .iter()
        .map(String::as_str)
        .chain(gate.routing.iter().flat_map(|r| r.reviewers.iter().map(String::as_str)))
        .collect();
    let unnamed: Vec<&str> = declared.iter().copied().filter(|id| !reachable.contains(id)).collect();
    assert!(
        unnamed.is_empty(),
        "every declared reviewer lane must be reachable by the gate; these are named by nothing: {unnamed:?}"
    );
    // Every reviewer the gate can require — static or routed — is a reviewer block that
    // actually exists. A rule naming a worker, or a block renamed out from under it,
    // could never open.
    for r in gate.reviewers.iter().chain(gate.routing.iter().flat_map(|rule| rule.reviewers.iter())) {
        assert_eq!(wf.block(r).map(|b| b.kind), Some(Role::Reviewer), "gate reviewer {r}");
    }
    // ROUTING RULES MUST BE ABLE TO FIRE — the partial close on the gap named above, and
    // partial for a reason worth stating rather than leaving for the next reader. Deciding
    // full reachability ("does this glob match a file a PR could touch") means running the
    // repo's tracked-file list through `glob_match`; what is checkable without either is
    // that a glob ROOTED at a literal path is rooted at one that EXISTS. That is exactly
    // the shape a directory rename or a typo produces, which is the arrival route the
    // review's premortem named (`src/**` narrowed to `src/orchestration/**` during a
    // refactor that moved it).
    let literal_root = |glob: &str| -> Option<String> {
        match glob.find(|c| c == '*' || c == '?' || c == '[') {
            None => Some(glob.to_string()),
            Some(w) => {
                let upto = &glob[..w];
                match upto.rfind('/') {
                    Some(i) if i > 0 => Some(upto[..i].to_string()),
                    _ => None,
                }
            }
        }
    };
    // The check's own POSITIVE CONTROL: the exact shape review found (a rooted glob naming
    // a directory that does not exist) must be one this check would refuse. Without it the
    // loop below passes just as well when `literal_root` returns None for everything and
    // nothing is ever verified.
    assert_eq!(literal_root("zzz-no-such-dir/**").as_deref(), Some("zzz-no-such-dir"));
    assert!(
        !Path::new(&repo).join("zzz-no-such-dir").exists(),
        "the control's negative arm: a bogus root really is absent, so the loop below has teeth"
    );

    // No floor on how many routing paths the real file carries, per rule or in all: a file
    // with no routing, or a rule made only of unrooted globs (`**/*.md`, which can fire), is
    // valid (#3507). The instrument's own teeth are the control above and the residual below,
    // both on literal strings rather than on the file's values.
    for (i, rule) in gate.routing.iter().enumerate() {
        for p in &rule.paths {
            let Some(root) = literal_root(p) else { continue };
            assert!(
                Path::new(&repo).join(&root).exists(),
                "routing[{i}] path {p:?} is rooted at {root:?}, which does not exist — the rule can never fire"
            );
        }
    }

    // THE RESIDUAL, PERFORMED rather than merely disclosed (CLAUDE.md's escape-hatch
    // rule): a glob whose literal root EXISTS but which matches no file slips through.
    // `src/**/*.zzz` roots at `src`, which is there, so the check passes it while the rule
    // can still never fire. This verifies the ROOT, not a match; closing that last step
    // means running the tracked-file list through `glob_match`. An unrooted glob is not
    // checked at all.
    assert_eq!(literal_root("**/nope.zzz"), None, "an unrooted glob has no root to check…");
    assert_eq!(
        literal_root("src/**/*.zzz").as_deref(),
        Some("src"),
        "…but a rooted-yet-unmatchable glob IS checked, and passes — the real blind spot"
    );
    assert!(Path::new(&repo).join("src").exists(), "…because its root really does exist, which is all this check asks");

    // And every `also:` condition is one THIS build can check. An unknown condition is
    // not ignored — it fails closed and refuses every merge — so shipping one in the
    // repo's own file would mean loomux could never merge its own PRs.
    for c in &gate.also {
        assert!(
            workflow::condition_supported(c),
            "{c:?} would refuse every merge: this build can only check {:?}",
            workflow::KNOWN_CONDITIONS
        );
    }
}

#[test]
fn the_checklist_reviewer_persona_carries_the_question_set() {
    // #1292 PR B: `rev-lead.md`'s "Questions every review answers" section is five
    // fixed headings the review body must carry. This loads the REAL persona through
    // the REAL profile loader, so a heading dropped from the file (accidentally, or by
    // a rename that drifts from PR A's product-side `## Premortem` spelling) reddens
    // here instead of silently thinning the review.
    //
    // BOUND TO THE FILE, NOT TO ROSTER MEMBERSHIP. The persona is pinned on the checked-in
    // file itself rather than looked up through `wf.block("rev-lead")`, because whether the
    // live roster declares `rev-lead` is the operator's call (#3507) and a lookup would
    // panic the day it does not. What that buys is checkable from the repo alone: the
    // persona's contract never silently drifts while no roster points at it.
    // These headings are `rev-lead.md`'s contract specifically — they are NOT asserted
    // of any other reviewer persona, which was never written to carry them.
    let repo = repo_root();
    let rel = ".github/agents/rev-lead.md";
    let p = profiles::load_block_profile(&repo, rel, Role::Reviewer)
        .unwrap_or_else(|e| panic!("rev-lead: {e}"));
    let body = flat(&p.instructions);

    for heading in [
        "## premortem",
        "## resource envelope",
        "## design alternative",
        "## misuse",
        "## operational futures",
    ] {
        pinned(
            "rev-lead.md",
            &body,
            heading,
            "one of the five fixed headings every review body must carry (#1292 PR B)",
        );
    }

    // The headings alone are decorative without the rule that makes them load-bearing:
    // an empty/"n/a" section under one of the five must be a finding, not a pass. Round-2
    // review (N2) named this clause unpinned — deleting it would drop CI to green while
    // silently permitting a rubber-stamped review. Pinned separately from the headings
    // above because it is a distinct sentence, not a sixth heading.
    pinned(
        "rev-lead.md",
        &body,
        "an empty or \"n/a\" section is a finding against the review, not a pass",
        "the enforcement rule that makes the five headings load-bearing rather than \
         decorative (#1292 PR B, review round 2 N2)",
    );
}

#[test]
fn the_cheap_review_lanes_carry_the_rules_that_make_them_safe() {
    // #1388's sibling of the question-set pin above, and for the same reason: these
    // three personas are INSTRUMENTS, and every safety property the gate argument
    // rests on lives as a sentence in a markdown file. A rule deleted from one of
    // them costs nothing at compile time and silently converts a lane from "fails
    // only on a quotable absence" into "fails on whatever a small model felt".
    //
    // The four defects #1388 found by running these checklists against its own PR
    // were all FALSE BLOCKS — a lane refusing a healthy change — which is the one
    // direction `all-pass` over four lanes is not safe in. Hence a pin, not a
    // comment.
    //
    // BOUND TO THE FILES, NOT TO ROSTER MEMBERSHIP, for the same reason as the pin
    // above: a roster's reviewer lanes need not be fixed-checklist instruments at all, so
    // deriving the population from whatever roster is live would assert these rules of
    // personas they were never written for. The checklist personas are checked in, so
    // their contracts stay pinned whether or not any roster points at them.
    //
    // Every `pinned(...)` rule below is byte-identical to what it was — the per-persona
    // rules are the whole point and none of them moved. The POPULATION CONTROL is the one
    // thing that did: a literal list makes `assert_eq!(lanes.len(), 3)` a sentence that
    // cannot fail, so it is re-earned from the directory instead, immediately below.
    // (Said precisely because an earlier draft of this comment said "every assertion",
    // which this test's own next commit then falsified — rev-final round 1, N1.)
    let repo = repo_root();
    let lanes = [".github/agents/qr-evidence.md", ".github/agents/qr-tests.md", ".github/agents/qr-constraints.md"];

    // POPULATION CONTROL. Binding the population to a literal list is what a
    // roster-derived filter used to do for free, so the control has to be re-earned
    // rather than restated: `assert_eq!(lanes.len(), 3)` against a fixed array is a
    // sentence that cannot fail. This asks the DIRECTORY instead — the list above must
    // be exactly the `qr-*.md` personas that exist — so a fourth checklist lane added
    // later, or one renamed or deleted, reddens here on the round it lands instead of
    // silently sitting outside the loop. The count is then checked at the VERIFIED site
    // below as well, since an empty or shrunken list would otherwise sail through the
    // loop and certify nothing.
    let mut on_disk: Vec<String> = fs::read_dir(Path::new(&repo).join(".github/agents"))
        .expect("the persona directory")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("qr-") && n.ends_with(".md"))
        .map(|n| format!(".github/agents/{n}"))
        .collect();
    on_disk.sort();
    let mut declared_lanes: Vec<String> = lanes.iter().map(|r| (*r).to_string()).collect();
    declared_lanes.sort();
    assert_eq!(
        declared_lanes, on_disk,
        "the checklist personas this test covers must be exactly the qr-*.md files that exist — \
         add a new one to the list in the commit that adds the file"
    );
    assert!(!on_disk.is_empty(), "…and there must be some, or the loop below certifies nothing");

    let mut verified = 0usize;
    for rel in &lanes {
        let p = profiles::load_block_profile(&repo, rel, Role::Reviewer)
            .unwrap_or_else(|e| panic!("{rel}: {e}"));
        let body = flat(&p.instructions);
        let label = (*rel).to_string();

        // The three rules EVERY lane owes, whatever it checks.
        pinned(
            &label,
            &body,
            "anything not on your checklist is silent",
            "the rule that stops a small model volunteering judgment it was never asked for — \
             without it a lane starts reporting opinions rev-lead already owns (#1388)",
        );
        pinned(
            &label,
            &body,
            "never review design, architecture, naming, wording, style, or formatting",
            "the explicit scope floor: these lanes are instruments, and design review is \
             rev-lead's alone (#1388)",
        );
        pinned(
            &label,
            &body,
            "when in doubt, `escalate`",
            "the tiebreak that keeps an undecidable check off the FAIL path, where it would \
             read as a defect the author has to answer for (#1388)",
        );

        // The FAIL rule itself — the load-bearing half of the all-pass safety argument.
        // Two spellings, because qr-constraints' checks are sweeps whose failure is a
        // line PRESENT rather than an artifact absent; both say the same thing, that a
        // FAIL must be quotable.
        let fail_rule = if rel.ends_with("qr-constraints.md") {
            "fail means a line you can quote"
        } else {
            "fail means absence of a named artifact you can quote"
        };
        pinned(
            &label,
            &body,
            fail_rule,
            "the whole reason all-pass over four lanes is safe: a lane may only refuse on \
             something it can paste, never on judgment (#1388)",
        );

        verified += 1;
    }
    assert_eq!(verified, lanes.len(), "every lane found must also have been checked");

    // qr-constraints alone owes the zero-shaped-sweep rules: five of its six checks
    // succeed by printing NOTHING, and an uncontrolled zero is byte-identical to a
    // grep that never worked. CLAUDE.md states this for the repo; a small model will
    // not infer it, so it has to be in the prompt — and therefore pinned.
    let qcrel = ".github/agents/qr-constraints.md";
    let qcp =
        profiles::load_block_profile(&repo, qcrel, Role::Reviewer).expect("qr-constraints persona");
    let qcbody = flat(&qcp.instructions);
    pinned(
        ".github/agents/qr-constraints.md",
        &qcbody,
        "never record `pass` off an uncontrolled zero",
        "the positive-control rule: without it a broken grep reads as a clean sweep, which \
         is the one way this lane can silently certify nothing (#1388)",
    );
    pinned(
        ".github/agents/qr-constraints.md",
        &qcbody,
        "do not pipe a sweep through `| wc -l`",
        "the specific form that discards grep's exit code and turns a broken command into a \
         confident 0 (CLAUDE.md's zero-shaped-sweep rule, #1388)",
    );
}

#[test]
fn the_repos_own_workflow_runs_every_block_on_what_it_declares() {
    // The end-to-end dogfood pin: the REAL file, through the REAL load + clamp, into the
    // command line loomux would actually run. It pins NO roster value (#3507): the
    // expected cli, model and effort of every block are read off the parsed file itself,
    // so an operator edit that swaps a block to another valid cli or model keeps this
    // green, while a launch path that drops, flattens or rewrites what the file declared
    // turns it red — whatever the file happens to declare today.
    //
    // It also covers model SHAPE without a shape rule of its own: `clamped()` runs every
    // block model through `sanitize_model`, which strips anything outside its alphabet,
    // so a declared id that would not survive the launch line arrives changed and fails
    // the equality below.
    let (reg, _d) = test_registry();
    let repo = repo_root();
    let wf = workflow::load_workflow(&repo).unwrap().unwrap();
    // The launcher's per-role picks name a SENTINEL model no real file declares, so
    // "declared model honored" and "flattened to the launcher pick" can never produce the
    // same argv. That is what retired #689's weaker converged-block case: it existed only
    // because the real file could declare the launcher's own pick, and a sentinel pick
    // cannot converge with any block.
    const PICK: &str = "launcher-pick-sentinel";
    assert!(wf.blocks.iter().all(|b| b.model != PICK), "no block may declare the sentinel, or it stops distinguishing");
    let picks = workflow::default_roster(&[
        (Role::Orchestrator, "claude", PICK),
        (Role::Worker, "claude", PICK),
        (Role::Reviewer, "claude", PICK),
        (Role::Planner, "claude", PICK),
    ]);
    let g = reg.create_group(&repo, Guardrails { blocks: picks, ..rails() }).unwrap();

    // `<flag> value` in either emitted form, where `<flag>` is one of the KNOB's own
    // spellings: the model rides `--model` (claude, copilot, gemini, opencode, pi) or `-m`
    // (codex); the effort rides `--effort` (claude) or `--thinking` (pi). Bound to the knob,
    // so an effort that happens to equal some other flag's value cannot satisfy it (#3510
    // review). A new CLI spelling reddens this on the PRODUCT change that adds it — never on
    // a workflow edit, which is the only thing #3507 promises stays green.
    const MODEL_FLAGS: &[&str] = &["--model", "-m"];
    const EFFORT_FLAGS: &[&str] = &["--effort", "--thinking"];
    fn flag_carries(toks: &[&str], flags: &[&str], value: &str, context: &str) -> bool {
        toks.windows(2).any(|w| {
            let v = w[1].trim_matches('"');
            // A declared `context:` rides claude's model token as a suffix
            // (`claude_model_arg`: `opus[1m]`), so the model is then a prefix, not the token.
            flags.contains(&w[0]) && (v == value || (!context.is_empty() && v.starts_with(&format!("{value}["))))
        })
    }
    // The binding's own POSITIVE CONTROL: a value carried by the WRONG knob's flag is not
    // carriage. Without it, a predicate that ignored `flags` would pass every row below.
    assert!(flag_carries(&["x", "--effort", "high"], EFFORT_FLAGS, "high", ""), "control: the right flag carries it");
    assert!(!flag_carries(&["x", "--model", "high"], EFFORT_FLAGS, "high", ""), "control: another knob's flag does not");

    let mut compiled = 0usize;
    for block in wf.blocks.iter().filter(|b| b.kind != Role::Orchestrator) {
        // The orchestrator is excluded because `compile` mirrors `spawn_agent_ex`, and the
        // orchestrator is never spawned through it — it is the launcher's own pane.
        let (cmd, argv, _kickoff) = compile(&reg, &g, &block.id);
        let cmd_toks: Vec<&str> = cmd.split_whitespace().collect();
        let argv_toks: Vec<&str> = argv.iter().map(String::as_str).collect();
        let id = block.id.as_str();

        // Expected values come from the PARSED FILE (with the parser's own inherit rule for
        // an absent field), never from the group the registry built out of it.
        let cli = workflow::cli_of(block, &g.guardrails.agent_cli);
        assert!(cmd.starts_with(&format!("{cli} ")), "{id}: the declared cli {cli:?} must launch: {cmd}");

        let model = workflow::model_of(block, &g.guardrails.agent_cli);
        if !model.is_empty() {
            assert!(flag_carries(&cmd_toks, MODEL_FLAGS, model, &block.context), "{id}: declared model {model:?} must reach the command line unchanged: {cmd}");
            assert!(flag_carries(&argv_toks, MODEL_FLAGS, model, &block.context), "{id}: …and the argv path must agree: {argv:?}");
        }
        assert!(!cmd.contains(PICK), "{id}: a launcher per-role pick must never flatten a declared block: {cmd}");

        // codex is the one effort-capable CLI whose level is a PROFILE key, never a flag
        // (`a_codex_effort_knob_rides_the_profile_and_an_empty_one_emits_no_key`,
        // tests/codexharness.rs) — a carriage behaviour, not an identity, so it is gated here.
        if !block.effort.is_empty() && cli != "codex" {
            assert!(flag_carries(&cmd_toks, EFFORT_FLAGS, &block.effort, ""), "{id}: declared effort {:?} must reach the command line: {cmd}", block.effort);
            assert!(flag_carries(&argv_toks, EFFORT_FLAGS, &block.effort, ""), "{id}: …and the argv path must agree: {argv:?}");
        }
        compiled += 1;
    }
    // POPULATION CONTROL: the loop compiled every delegate the file declares, and at least one.
    assert!(compiled > 0, "the file declares no delegate to compile");
    assert_eq!(compiled, wf.blocks.iter().filter(|b| b.kind != Role::Orchestrator).count());
}

#[test]
fn a_declared_block_model_survives_both_clis_and_a_resume() {
    let (reg, _d) = test_registry();
    let repo = Repo::new().workflow(
        "version: 1\nblocks:\n\
         \x20 - id: quick\n    kind: worker\n    cli: claude\n    model: haiku\n    prompt: Small, clearly-directed edits only.\n\
         \x20 - id: cheap-copilot\n    kind: reviewer\n    cli: copilot\n    model: claude-haiku-4.5\n    prompt: Review only for typos.\n\
         \x20 - id: inherits\n    kind: reviewer\n    cli: claude\n",
    );
    // The launcher's per-role picks say OPUS for reviewers — deliberately NOT the class
    // default (`sonnet`), so the two candidate semantics for an undeclared block model
    // actually diverge below. With `rails()`'s empty roster the pick *was* the class
    // default, and the `inherits` assertion passed under either rule: a pin that could
    // not fail on the very claim the design note calls the surprising one (rev-14 F3).
    let picks = workflow::default_roster(&[
        (Role::Orchestrator, "claude", "opus"),
        (Role::Worker, "claude", "opus"),
        (Role::Reviewer, "claude", "opus"),
        (Role::Planner, "claude", "opus"),
    ]);
    let g = reg.create_group(&repo.path(), Guardrails { blocks: picks, ..rails() }).unwrap();

    // A tier reaches the flag on BOTH CLIs — the model is a block property, not a
    // claude one, and `sanitize_model` keeps a dotted vendor id like the ones copilot
    // takes (`claude-haiku-4.5`) intact rather than filtering it down to something else.
    assert!(compile(&reg, &g, "quick").0.contains("--model haiku"));
    assert!(compile(&reg, &g, "cheap-copilot").0.contains("--model claude-haiku-4.5"));

    // A block that declares NO model takes its class default *for its own CLI* — NOT the
    // launcher's per-role pick, which here says opus. The file is the roster, so an
    // undeclared field resolves from the block, not from a launcher form the file never
    // saw. (Nothing is silent about it: the launcher's roster preview runs this same
    // load+clamp and shows the human the resolved model of every block before they hit
    // Create.) Both halves are asserted: the rule that holds, and the one that doesn't.
    let (inherits, _, _) = compile(&reg, &g, "inherits");
    assert!(inherits.contains("--model sonnet"), "the class default must win: {inherits}");
    assert!(
        !inherits.contains("--model opus"),
        "a declared block must never inherit the launcher's per-role model: {inherits}"
    );

    // The tier is durable: a resumed group must not come back one model tier up.
    let (_repo, persisted) = reg.load_group_file(&g.id).expect("group.json");
    assert_eq!(persisted.block("quick").unwrap().model, "haiku");
    assert_eq!(persisted.block("cheap-copilot").unwrap().model, "claude-haiku-4.5");
}

#[test]
fn the_builtin_roster_still_honors_the_launchers_per_role_models() {
    // The other half of "a guardrail is a launcher default": with the advanced
    // orchestrator OFF, the per-role picks are the ONLY thing that decides a model —
    // even in this repo, which now ships a workflow file declaring otherwise. If a
    // declared block could reach a toggle-off group, the compatibility promise (and
    // the consent argument the toggle exists for) would both be false.
    let (reg, _d) = test_registry();
    let picks = workflow::default_roster(&[
        (Role::Orchestrator, "claude", "opus"),
        (Role::Worker, "claude", "opus"),    // deliberately NOT the class default
        (Role::Reviewer, "claude", "haiku"), // ditto
        (Role::Planner, "claude", "opus"),
    ]);
    let g = reg
        .create_group(&repo_root(), Guardrails { blocks: picks, ..plain_rails() })
        .unwrap();

    assert_eq!(
        g.guardrails.blocks.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(),
        ["orchestrator", "worker", "reviewer", "planner"],
        "the toggle is off — the repo's own workflow file must not be read at all"
    );
    let (worker, _, kickoff) = compile(&reg, &g, "worker");
    assert!(worker.contains("--model opus"), "the launcher's worker pick decides: {worker}");
    // #416: --agent DOES appear (the built-in worker contract, same as any
    // default-roster block) — "a toggle-off group has no personas" now means
    // no REPO text reaches it, not that the flag is absent. Round #417
    // correction 6: a generated file's handle, not the bare block id.
    assert!(worker.contains(&format!("--agent loomux-{}-worker", g.id)), "the built-in contract still rides the system prompt: {worker}");
    assert!(kickoff.is_none(), "a toggle-off group has no persona to fall back to a kickoff for");
    let (reviewer, _, _) = compile(&reg, &g, "reviewer");
    assert!(reviewer.contains("--model haiku"), "the launcher's reviewer pick decides: {reviewer}");
}
