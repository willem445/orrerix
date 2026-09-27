//! Parsing and validation, `role_hint` as a marker rather than a capability class, and the `manager` class.
//!
//! One module of the `workflow` integration-test target (`main.rs`),
//! split out of the former single-file `tests/workflow.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ───────────────────────── schema: parse + validate ─────────────────────────

pub(crate) const FOCUSED_REVIEW: &str = r#"
version: 1
name: focused-review

blocks:
  - id: planner
    name: Planner
    kind: planner
    cli: claude
    model: opus

  - id: worker
    name: Worker
    kind: worker
    cli: copilot
    profile: .github/agents/worker.md

  - id: rev-security
    name: Security review
    kind: reviewer
    cli: claude
    model: opus
    prompt: |
      Review ONLY for security defects: injection, authz, secrets, path traversal.
      Ignore style and perf — other reviewers cover those.

  - id: rev-tests
    name: Test-quality review
    kind: reviewer
    cli: claude
    model: sonnet
    prompt: Review ONLY test quality. Flag tests that cannot fail.

edges:
  - { from: planner, to: worker }
  - { from: worker,  to: [rev-security, rev-tests] }

gates:
  merge:
    require: all-pass
    reviewers: [rev-security, rev-tests]
    also: [ci-green]
"#;

#[test]
fn schema_sketch_parses_into_blocks_edges_and_gates() {
    let wf = workflow::parse_workflow(FOCUSED_REVIEW).expect("the §4 schema sketch must parse");
    assert_eq!(wf.version, 1);
    assert_eq!(wf.name, "focused-review");

    // Identity is the id; the name is display-only. Both reviewers are the same
    // capability class but different agents — the entire point of the model.
    let ids: Vec<&str> = wf.blocks.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(ids, vec!["planner", "worker", "rev-security", "rev-tests"]);
    let sec = wf.block("rev-security").unwrap();
    assert_eq!(sec.kind, Role::Reviewer);
    assert_eq!(sec.name, "Security review");
    assert_eq!(sec.model, "opus");
    assert_eq!(wf.block("rev-tests").unwrap().model, "sonnet");
    assert!(
        sec.prompt.as_deref().unwrap().contains("path traversal"),
        "a block-scalar prompt keeps its body"
    );
    assert_eq!(
        wf.block("worker").unwrap().profile.as_deref(),
        Some(".github/agents/worker.md")
    );

    // `to:` accepts a scalar (single hand-off) or a list (fan-out).
    assert_eq!(wf.edges[0].to, vec!["worker"]);
    assert_eq!(wf.edges[1].to, vec!["rev-security", "rev-tests"]);

    let gate = wf.gates.get("merge").expect("the merge gate must parse");
    assert_eq!(gate.require, GateRequire::AllPass);
    assert_eq!(gate.reviewers, vec!["rev-security", "rev-tests"]);
    assert_eq!(gate.also, vec!["ci-green"]);
}

#[test]
fn unknown_kind_is_rejected_never_coerced_to_worker() {
    // THE bug this feature exists to not repeat. Pre-#222, `mcp.rs` and the
    // session-rejoin path both spelled the kind parse `_ => Role::Worker`, so a
    // typo'd kind silently produced an agent with a worktree and write access.
    // A capability class must never be guessed.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: rev\n    kind: revieweer\n",
    )
    .unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("unknown kind") && e.contains("revieweer")),
        "an unknown kind must be a named error, got: {errs:?}"
    );
    assert!(
        errs.iter().any(|e| e.contains("worker") && e.contains("reviewer")),
        "the error must list the classes that ARE allowed, got: {errs:?}"
    );
    // And nothing survives: there is no block to fall back on.
    assert!(workflow::kind_from_str("revieweer").is_none());
    assert!(workflow::kind_from_str("").is_none());
    // The four real ones still parse (case-insensitively).
    assert_eq!(workflow::kind_from_str("Reviewer"), Some(Role::Reviewer));
    assert_eq!(workflow::kind_from_str(" planner "), Some(Role::Planner));
}

#[test]
fn validation_catches_the_dangling_references_every_other_tool_ships_with() {
    // Unknown CLI.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: w\n    kind: worker\n    cli: goose\n",
    )
    .unwrap_err();
    assert!(errs.iter().any(|e| e.contains("unknown cli") && e.contains("goose")), "{errs:?}");

    // An edge pointing at a block that doesn't exist.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: w\n    kind: worker\nedges:\n  - { from: w, to: ghost }\n",
    )
    .unwrap_err();
    assert!(errs.iter().any(|e| e.contains("'to' names no block") && e.contains("ghost")), "{errs:?}");

    // A gate naming a reviewer that doesn't exist — unsatisfiable forever,
    // because nothing would ever record a verdict for it.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: w\n    kind: worker\ngates:\n  merge:\n    reviewers: [ghost]\n",
    )
    .unwrap_err();
    assert!(errs.iter().any(|e| e.contains("names no block") && e.contains("ghost")), "{errs:?}");

    // A gate naming a block that exists but isn't a reviewer — equally
    // unsatisfiable, and much easier to write by accident.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: w\n    kind: worker\ngates:\n  merge:\n    reviewers: [w]\n",
    )
    .unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("is a worker block, not a reviewer")),
        "a gate may only require reviewer verdicts: {errs:?}"
    );

    // A threshold no number of passes could reach.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: r\n    kind: reviewer\ngates:\n  merge:\n    require: threshold\n    threshold: 3\n    reviewers: [r]\n",
    )
    .unwrap_err();
    assert!(errs.iter().any(|e| e.contains("could never pass")), "{errs:?}");

    // A reviewer named twice in the same gate: undetected, this would inflate
    // `gate_need`/`recommend_capacity`'s minimum and let one PASS count twice
    // toward a `threshold: N` gate (#259). Rejected, consistent with a
    // duplicate block id, rather than silently deduped.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: r\n    kind: reviewer\ngates:\n  merge:\n    reviewers: [r, r]\n",
    )
    .unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("named more than once") && e.contains('r')),
        "{errs:?}"
    );

    // Duplicate ids: edges/gates would reference an ambiguous target.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: w\n    kind: worker\n  - id: w\n    kind: reviewer\n",
    )
    .unwrap_err();
    assert!(errs.iter().any(|e| e.contains("duplicate block id")), "{errs:?}");

    // A typo'd KEY is caught rather than silently ignored — the failure mode
    // Flowise/Langflow/Dify all ship with. `promt:` must not be a no-op.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: w\n    kind: worker\n    promt: hello\n",
    )
    .unwrap_err();
    assert!(!errs.is_empty(), "an unknown key must not be silently dropped");

    // Every problem is reported, not just the first: the human fixes the file in
    // one pass instead of playing whack-a-mole at spawn time.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: a\n    kind: nope\n  - id: b\n    kind: worker\n    cli: goose\n",
    )
    .unwrap_err();
    assert!(errs.len() >= 2, "validation reports every problem, got: {errs:?}");
}

#[test]
fn a_duplicate_driver_enabled_key_is_refused_not_last_one_wins() {
    // The pane's round-5 disclosure (#1869 review 6) leans on this bound: its
    // `enabled:`-line splice touches only the FIRST occurrence of a duplicated
    // key while the reader would keep the last, and the disclosure is safe ONLY
    // while serde refuses a duplicate field — an unloadable file cannot be made
    // wrong by the rewrite. This repo has swapped YAML crates once, so that
    // behaviour is pinned here rather than assumed: if the crate (or a serde
    // version) ever starts deduping silently, this goes red and the residual's
    // bound is gone with it.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: b\n    kind: worker\ndriver:\n  enabled: true\n  enabled: false\n",
    )
    .unwrap_err();
    assert!(
        !errs.is_empty(),
        "a duplicate driver enabled: key must refuse the file, not dedupe: {errs:?}"
    );

    // Positive control: the same file with ONE key parses — the refusal above is
    // the duplicate's, not the fixture's shape, and the assertion can tell the
    // two apart.
    assert!(workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: b\n    kind: worker\ndriver:\n  enabled: true\n",
    )
    .is_ok());
}

#[test]
fn a_workflow_file_can_never_grant_a_capability() {
    // The security spine (§2c/§2e). `kind` is the ONLY capability knob, and it
    // selects from a closed enum. There is no way to spell "a reviewer that can
    // push" or "a planner that can write".
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: p\n    kind: planner\n    read_only: false\n",
    )
    .unwrap_err();
    assert!(
        !errs.is_empty(),
        "`read_only: false` must not be an accepted key — it would be a capability grant"
    );

    // The class fully determines read-only-ness; nothing else can move it.
    assert!(Role::Planner.is_read_only());
    for r in [Role::Orchestrator, Role::Worker, Role::Reviewer] {
        assert!(!r.is_read_only());
    }

    // A `profile:` cannot escape the repo — the file's body is injected straight
    // into an agent's system prompt, so an escape would let a repo pull any file
    // on the operator's disk into an agent's context.
    //
    // These must be refused ON EVERY PLATFORM, which is the whole reason the
    // check runs on the string rather than deferring to `std::path`. A workflow
    // file is committed and shared between developers, so a `profile:` that is an
    // escape on Windows and an innocent relative path on Linux is precisely the
    // divergence to kill — and `std::path` on Unix will happily read
    // `C:/Windows/win.ini` as a directory called `C:`, and `..\..\x` as a single
    // filename.
    for escape in [
        "../../../../etc/passwd",
        "..\\..\\..\\Windows\\win.ini",
        "C:/Windows/win.ini",
        "c:\\Windows\\win.ini",
        "/etc/shadow",
        "\\\\server\\share\\x.md",
        ".github/agents/../../../../etc/passwd",
    ] {
        assert!(
            workflow::resolve_profile_path("/repo", escape).is_err(),
            "{escape:?} must be refused as a profile path on every platform"
        );
    }
    // The legitimate shape still resolves, with either separator.
    assert!(workflow::resolve_profile_path("/repo", ".github/agents/x.md").is_ok());
    assert!(workflow::resolve_profile_path("/repo", ".github\\agents\\x.md").is_ok());
}

#[test]
fn block_ids_names_and_personas_are_sanitized_before_any_shell_line() {
    // Ids reach a `--agent` flag and a file name; names reach a pane title.
    // `sanitize_model` is the precedent — strip, don't escape.
    assert_eq!(workflow::sanitize_id("rev-security_2"), Some("rev-security_2".into()));
    assert_eq!(workflow::sanitize_id("rev; rm -rf /"), Some("revrm-rf".into()));
    assert_eq!(workflow::sanitize_id("$(whoami)"), Some("whoami".into()));
    assert_eq!(workflow::sanitize_id("   "), None);

    // An id with disallowed characters is REJECTED at parse (not quietly
    // rewritten into something the author didn't write and can't reference).
    let errs =
        workflow::parse_workflow("version: 1\nblocks:\n  - id: 'rev sec'\n    kind: reviewer\n")
            .unwrap_err();
    assert!(errs.iter().any(|e| e.contains("not allowed")), "{errs:?}");

    // Control characters can't smuggle escape codes into a pane title.
    assert_eq!(workflow::sanitize_display("Sec\u{1b}[31m review\n"), "Sec[31m review");

    // `sanitize_persona`'s apostrophe mapping predates round #417 correction
    // 6 (it protected the single-quoted `--agents` shell token that
    // mechanism replaced with a generated file — see its own doc) but is
    // kept as defense-in-depth; still verified directly since nothing else
    // pins its behavior once no production call site's OUTPUT is asserted
    // against a raw apostrophe anymore.
    let s = workflow::sanitize_persona("don't run '; rm -rf /");
    assert!(!s.contains('\''), "the ASCII apostrophe must not survive: {s:?}");
    assert!(s.contains("don\u{2019}t"), "the word must still read as prose: {s:?}");
}

#[test]
fn a_block_name_cannot_break_the_generated_agent_files_yaml_frontmatter() {
    // `name:` is display text — `sanitize_display` only strips control
    // characters, so an apostrophe survives it, as it should. But the name is
    // ALSO the `description:` in the generated Claude agent file's YAML
    // frontmatter (round #417 correction 6, replacing the pre-round-6
    // `--agents` JSON payload this test used to check) — an unquoted colon
    // or double quote in a description would break the frontmatter's own
    // block-mapping parse. A block called `Bob's review: "the strict one"`
    // exercises an apostrophe (already neutralized by `sanitize_persona` at
    // resolution time) AND a colon-plus-double-quote (which only `yaml_
    // double_quoted`, applied at file-write time, protects against).
    let (reg, d) = test_registry();
    let repo = Repo::new().workflow(
        "version: 1\nblocks:\n  - id: rev\n    name: 'Bob''s review: \"the strict one\"'\n    kind: reviewer\n    prompt: Be strict.\n",
    );
    let g = reg.create_group(&repo.path(), rails()).unwrap();

    // The name keeps its apostrophe and quotes where it is only ever
    // displayed...
    assert_eq!(g.guardrails.block("rev").unwrap().name, "Bob's review: \"the strict one\"");

    // ...but the generated file's frontmatter still parses: exactly one
    // `description:` line, and the `---` closing delimiter is still found
    // (a real YAML corruption would either merge lines or eat the
    // delimiter).
    let (_cmd, argv, _k) = compile(&reg, &g, "rev");
    let handle = argv[argv.iter().position(|a| a == "--agent").unwrap() + 1].clone();
    let generated = fs::read_to_string(d.path().join("claude-agents").join(format!("{handle}.md"))).unwrap();
    let lines: Vec<&str> = generated.lines().collect();
    assert_eq!(lines[0], "---", "frontmatter must open cleanly: {generated}");
    assert!(lines[1].starts_with("name: "), "{generated}");
    assert_eq!(lines[3], "---", "frontmatter must close on line 4, not swallowed by an unescaped quote/colon: {generated}");
    assert_eq!(
        lines[2],
        "description: \"Bob\u{2019}s review: \\\"the strict one\\\"\"",
        "the apostrophe is already neutralized by sanitize_persona; the colon and quotes are escaped by yaml_double_quoted: {generated}"
    );
}

#[test]
fn a_verbose_persona_description_is_clamped_in_the_generated_agent_file() {
    // #502: Claude Code loads EVERY agent definition's `description` into the
    // session's agent roster and caps the AGGREGATE — the observed failure was
    // "Agent descriptions are over the 15.0k-token limit (~15.6k tokens)".
    // Description length is therefore a SHARED budget, not a per-file concern.
    // loomux's own descriptions are already terse (the block id), but a
    // repo-authored persona's `description:` is unbounded free text that flows
    // straight into the generated file — one repo's essay would spend every
    // session's budget. So it is clamped at the write.
    let (reg, d) = test_registry();
    let essay = "a very wordy persona description that keeps going and going ".repeat(40);
    let repo = Repo::new()
        .workflow(
            "version: 1\nblocks:\n  - id: chatty\n    kind: worker\n    cli: claude\n    profile: .github/agents/chatty.md\n",
        )
        .agent_file("chatty.md", &format!("---\nname: chatty\ndescription: {essay}\n---\nBe brief.\n"));
    let g = reg.create_group(&repo.path(), rails()).unwrap();

    let b = g.guardrails.block("chatty").unwrap();
    let cli = workflow::cli_of(b, &g.guardrails.agent_cli);
    let persona = reg.resolve_persona(&g, b).unwrap();
    assert!(
        persona.as_ref().is_some_and(|p| p.description.chars().count() > 1_000),
        "the persona itself must really carry an oversized description, or this pins nothing",
    );
    let instructions_body = instructions_lf(&reg, &g.id, &b.instructions_file());
    let contract = block_contract_text(&instructions_body, persona.as_ref());
    let inject = reg.persona_inject(&g.id, b, cli, persona.as_ref(), &contract);
    let handle = inject.claude_agent.clone().expect("a generated Claude agent file handle");

    let generated = fs::read_to_string(d.path().join("claude-agents").join(format!("{handle}.md"))).unwrap();
    let fm = parse_agent_frontmatter(&generated);
    let description = fm.get("description").expect("Claude requires `description`").clone();
    assert!(
        description.chars().count() <= 160,
        "an unbounded persona description must not reach the roster verbatim ({} chars): {description}",
        description.chars().count(),
    );
    assert!(
        description.ends_with("..."),
        "a clamped description must show it was cut rather than read as the whole thing: {description}",
    );
    assert!(
        description.starts_with("a very wordy persona description"),
        "the clamp keeps the FRONT of the description — the part that says what the agent is: {description}",
    );
    // The full persona TEXT is untouched: only the roster-facing description
    // is a shared budget, and the body is what the agent actually reads.
    assert!(generated.contains("Be brief."), "{generated}");
}

#[test]
fn a_quoted_allow_pattern_keeps_its_commas_and_braces() {
    // Coordination with #223 (the workflow pane, which hit a corruption bug on
    // exactly this shape). A real tool pattern contains commas and brackets:
    //   allow: ["Bash(gh pr view --json title,body)", "Read"]
    // Two things must hold. The YAML flow sequence must not be split on the
    // comma INSIDE the quoted scalar — and the pattern sanitizer must not strip
    // that comma either, because dropping it would not reject the pattern, it
    // would silently rewrite it to `--json titlebody`: a different, broken
    // command the agent is then pre-approved to run.
    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: w\n    kind: worker\n    prompt: Do the thing.\n\
         \x20   allow: [\"Bash(gh pr view --json title,body)\", \"Read\", \"Bash(gh pr list --json number,title)\"]\n",
    )
    .expect("a quoted flow sequence must parse");
    assert_eq!(
        wf.block("w").unwrap().allow,
        vec![
            "Bash(gh pr view --json title,body)",
            "Read",
            "Bash(gh pr list --json number,title)",
        ],
        "commas inside a quoted scalar are content, not separators — and must survive sanitization"
    );

    // ...and the pattern reaches the command line intact, still inside its own
    // double-quoted token (a comma is inert there in both PowerShell and sh).
    let (reg, _d) = test_registry();
    let repo = Repo::new().workflow(
        "version: 1\nblocks:\n  - id: w\n    kind: worker\n    prompt: Do the thing.\n\
         \x20   allow: [\"Bash(gh pr view --json title,body)\"]\n",
    );
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let (cmd, argv, _k) = compile(&reg, &g, "w");
    assert!(
        cmd.contains("\"Bash(gh pr view --json title,body)\""),
        "the pattern must reach the shell line intact: {cmd}"
    );
    assert!(
        argv.iter().any(|a| a == "Bash(gh pr view --json title,body)"),
        "...and be exactly one argv token: {argv:?}"
    );

    // The sanitizer still strips what could escape the double quotes it lands in.
    assert_eq!(
        profiles::sanitize_allow("Bash(gh pr view --json title,body)").as_deref(),
        Some("Bash(gh pr view --json title,body)")
    );
    for hostile in ["Read\"; rm -rf /", "x\"y", "$(whoami)", "a`b"] {
        let out = profiles::sanitize_allow(hostile).unwrap_or_default();
        assert!(
            !out.contains('"') && !out.contains('`') && !out.contains('$') && !out.contains(';'),
            "{hostile:?} must not keep a shell metacharacter, got {out:?}"
        );
    }
}

#[test]
fn an_authored_with_stamp_is_tolerated_and_preserved() {
    // #223's workflow pane stamps the loomux version that wrote the file.
    // `deny_unknown_fields` catches typo'd keys, so this one has to be declared
    // — and it must NEVER be a validation error, whatever it says: a file
    // authored by a newer (or older) loomux must still load.
    let wf = workflow::parse_workflow(
        "version: 1\nname: x\nauthored_with: loomux 0.9.0\nblocks:\n  - id: w\n    kind: worker\n",
    )
    .expect("authored_with must never fail validation");
    assert_eq!(wf.authored_with, "loomux 0.9.0", "and it must be preserved, not dropped");

    // Absent is fine (a hand-written file), and so is a value from a build that
    // doesn't exist yet.
    let wf = workflow::parse_workflow("version: 1\nblocks:\n  - id: w\n    kind: worker\n").unwrap();
    assert_eq!(wf.authored_with, "");
    assert!(workflow::parse_workflow(
        "version: 1\nauthored_with: loomux 99.0.0-from-the-future\nblocks:\n  - id: w\n    kind: worker\n"
    )
    .is_ok());

    // A group still launches from such a file, end to end.
    let (reg, _d) = test_registry();
    let repo = Repo::new().workflow(
        "version: 1\nauthored_with: loomux 0.9.0\nblocks:\n  - id: rev-sec\n    kind: reviewer\n    prompt: Security only.\n",
    );
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    assert!(g.guardrails.block("rev-sec").is_some(), "the roster must load, not fall back");
}

// ──── role_hint: a marker that is never a fifth capability CLASS (#250/#324) ──

#[test]
fn role_hint_requires_its_matching_capability_class() {
    // advisor -> planner, process -> worker. Anything else is a loud parse
    // error, never a silent fallback (the whole point of keeping this a
    // separate, validated field rather than free text).
    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: advisor\n    kind: planner\n    role_hint: advisor\n",
    )
    .expect("advisor on a planner-kind block must parse");
    assert_eq!(wf.block("advisor").unwrap().role_hint.as_deref(), Some("advisor"));

    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: proc\n    kind: worker\n    role_hint: process\n",
    )
    .expect("process on a worker-kind block must parse");
    assert_eq!(wf.block("proc").unwrap().role_hint.as_deref(), Some("process"));

    // The mismatched pairing is a NAMED error, not a silent drop or a coerced
    // kind — mirroring `unknown_kind_is_rejected_never_coerced_to_worker`.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: bad\n    kind: worker\n    role_hint: advisor\n",
    )
    .unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("role_hint") && e.contains("advisor") && e.contains("planner")),
        "advisor on a worker block must name the required kind: {errs:?}"
    );

    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: bad\n    kind: planner\n    role_hint: process\n",
    )
    .unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("role_hint") && e.contains("process") && e.contains("worker")),
        "process on a planner block must name the required kind: {errs:?}"
    );

    // liaison -> reviewer (#891). The human-facing pane rides the reviewer
    // class for its posture: read-only, persistent, board-reading.
    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: human\n    kind: reviewer\n    role_hint: liaison\n",
    )
    .expect("liaison on a reviewer-kind block must parse");
    assert_eq!(wf.block("human").unwrap().role_hint.as_deref(), Some("liaison"));

    // ...and on every OTHER kind it is the same named refusal, not a silent
    // drop. Checked on all three rather than one, because "requires reviewer"
    // is a claim about the whole set of kinds it is NOT allowed on.
    for wrong in ["worker", "planner", "orchestrator"] {
        let errs = workflow::parse_workflow(&format!(
            "version: 1\nblocks:\n  - id: bad\n    kind: {wrong}\n    role_hint: liaison\n"
        ))
        .unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("role_hint")
                && e.contains("liaison")
                && e.contains("reviewer")),
            "liaison on a {wrong} block must name the required kind: {errs:?}"
        );
    }

    // Case and surrounding space are normalized exactly like the other hints —
    // the TypeScript mirror (`roleHintRequires`) pins the same rule, and the
    // two must not disagree about what the real parser accepts.
    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: human\n    kind: reviewer\n    role_hint: \" Liaison \"\n",
    )
    .expect("a capitalized, padded liaison hint must parse like advisor/process do");
    assert_eq!(wf.block("human").unwrap().role_hint.as_deref(), Some("liaison"));

    // An unrecognized value is rejected exactly like an unrecognized `kind` —
    // never coerced to the nearest valid one.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: bad\n    kind: planner\n    role_hint: bogus\n",
    )
    .unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("unknown role_hint") && e.contains("bogus")),
        "{errs:?}"
    );

    // `deny_unknown_fields` still catches a typo'd key — role_hint is a
    // declared field, not a door that widens what else is accepted.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: w\n    kind: worker\n    role_hit: process\n",
    )
    .unwrap_err();
    assert!(!errs.is_empty(), "a typo'd role_hint key must not be silently ignored: {errs:?}");

    // Absent is None — today's behavior, byte for byte.
    let wf = workflow::parse_workflow("version: 1\nblocks:\n  - id: w\n    kind: worker\n").unwrap();
    assert_eq!(wf.block("w").unwrap().role_hint, None);
}

#[test]
fn role_hint_grants_no_capability_to_its_block() {
    // The one place review pushes hardest (plan §A2): role_hint must be
    // PROVEN inert w.r.t. capability, not just asserted. Two otherwise
    // identical planner blocks — one plain, one hinted `advisor` — must
    // compile to the identical deny-flag / persona-allow surface.
    let (reg, _d) = test_registry();
    let repo = Repo::new().workflow(
        "version: 1\nblocks:\n\
         \x20 - id: plain\n    kind: planner\n    prompt: Same prompt, no hint.\n\
         \x20 - id: advisor\n    kind: planner\n    role_hint: advisor\n    prompt: Same prompt, no hint.\n",
    );
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let plain = g.guardrails.block("plain").unwrap();
    let advisor = g.guardrails.block("advisor").unwrap();
    assert_eq!(advisor.role_hint.as_deref(), Some("advisor"));
    assert_eq!(plain.role_hint, None);

    // Same capability class -> same structural deny-flags; `is_read_only()`
    // keys off `kind` alone and role_hint cannot be the thing that moves it.
    assert_eq!(plain.kind, advisor.kind);
    assert!(advisor.kind.is_read_only());

    // The resolved persona's SECURITY-relevant fields (never the cosmetic
    // name/description, which legitimately differ with the block id) are
    // identical: role_hint cannot widen the allow list or change the
    // injection mode.
    let plain_persona = reg.resolve_persona(&g, plain).unwrap().expect("plain persona resolves");
    let advisor_persona = reg.resolve_persona(&g, advisor).unwrap().expect("advisor persona resolves");
    assert_eq!(plain_persona.text, advisor_persona.text);
    assert_eq!(plain_persona.allow, advisor_persona.allow);
    assert_eq!(plain_persona.mode, advisor_persona.mode);
    assert_eq!(plain_persona.copilot_native, advisor_persona.copilot_native);

    // ...and the actual compiled command lines carry the identical deny-tool
    // surface — the mechanical enforcement, not just the intermediate struct.
    // Normalize away the one legitimate difference (the block id, which rides
    // in the generated agent file's handle behind `--agent <handle>`) and the
    // rest — including `--disallowedTools Edit Write … "Bash(git commit *)"
    // "Bash(git push *)"` — must be byte-identical.
    let (cmd_plain, _argv_plain, _k) = compile(&reg, &g, "plain");
    let (cmd_advisor, _argv_advisor, _k2) = compile(&reg, &g, "advisor");
    assert!(cmd_plain.contains("--disallowedTools"), "a planner IS denied write tools — the comparison must not be vacuously equal: {cmd_plain}");
    let norm = |s: &str| s.replace("advisor", "BLOCK-ID").replace("plain", "BLOCK-ID");
    assert_eq!(
        norm(&cmd_plain),
        norm(&cmd_advisor),
        "the only difference between an advisor(planner) and a plain planner's command line \
         must be the block id itself:\n  plain:   {cmd_plain}\n  advisor: {cmd_advisor}"
    );
}

#[test]
fn the_liaison_hint_grants_its_reviewer_block_nothing_extra() {
    // #891's half of `role_hint_grants_no_capability_to_its_block`. The liaison
    // is the first hint whose whole point is a capability RULE, so "the hint
    // still grants nothing" needs proving here, not assuming: what the rule does
    // is take `review_verdict` AWAY (pinned in tests/orchestration/), and
    // everything structural must be identical to a plain reviewer's.
    //
    // Note the block ids are deliberately NOT the hint string: normalizing
    // "liaison" out of the command line would hide the very leak this test
    // exists to detect.
    let (reg, _d) = test_registry();
    let repo = Repo::new().workflow(
        "version: 1\nblocks:\n\
         \x20 - id: plain\n    kind: reviewer\n    prompt: Same prompt, no hint.\n\
         \x20 - id: hinted\n    kind: reviewer\n    role_hint: liaison\n    prompt: Same prompt, no hint.\n",
    );
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let plain = g.guardrails.block("plain").unwrap();
    let hinted = g.guardrails.block("hinted").unwrap();
    assert_eq!(hinted.role_hint.as_deref(), Some("liaison"));
    assert_eq!(plain.role_hint, None);
    assert_eq!(plain.kind, hinted.kind);

    // The resolved persona's SECURITY-relevant fields are identical: the hint
    // cannot widen the allow list or change the injection mode.
    let plain_persona = reg.resolve_persona(&g, plain).unwrap().expect("plain persona resolves");
    let hinted_persona = reg.resolve_persona(&g, hinted).unwrap().expect("hinted persona resolves");
    assert_eq!(plain_persona.text, hinted_persona.text);
    assert_eq!(plain_persona.allow, hinted_persona.allow);
    assert_eq!(plain_persona.mode, hinted_persona.mode);
    assert_eq!(plain_persona.copilot_native, hinted_persona.copilot_native);

    // ...and the compiled command lines carry the identical deny-tool surface.
    let (cmd_plain, _argv_plain, _k) = compile(&reg, &g, "plain");
    let (cmd_hinted, _argv_hinted, _k2) = compile(&reg, &g, "hinted");
    assert!(
        cmd_plain.contains("--disallowedTools"),
        "a reviewer IS denied write tools — the comparison must not be vacuously equal: {cmd_plain}"
    );
    let norm = |s: &str| s.replace("hinted", "BLOCK-ID").replace("plain", "BLOCK-ID");
    assert_eq!(
        norm(&cmd_plain),
        norm(&cmd_hinted),
        "the only difference between a liaison-hinted reviewer and a plain one's command line \
         must be the block id itself:\n  plain:  {cmd_plain}\n  hinted: {cmd_hinted}"
    );
}

#[test]
fn a_gate_may_not_name_a_liaison_as_one_of_its_reviewers() {
    // A liaison is reviewer-KIND, so it slips past the "not a reviewer" check —
    // but it can never record a verdict (#891), so a gate naming one would wait
    // forever on something no code path produces. That is the same permanently
    // unsatisfiable gate the worker case refuses, and it is refused at PARSE
    // rather than discovered later as a merge gate that never opens.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n\
         \x20 - id: worker\n    kind: worker\n\
         \x20 - id: human\n    kind: reviewer\n    role_hint: liaison\n\
         gates:\n  merge:\n    reviewers: [human]\n",
    )
    .unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("liaison") && e.contains("could never open")),
        "a gate naming a liaison must be refused, and say why: {errs:?}"
    );

    // The control: the SAME file with the hint removed parses clean. Without
    // this, the assertion above could be passing on any unrelated error in the
    // document rather than on the liaison rule.
    workflow::parse_workflow(
        "version: 1\nblocks:\n\
         \x20 - id: worker\n    kind: worker\n\
         \x20 - id: human\n    kind: reviewer\n\
         gates:\n  merge:\n    reviewers: [human]\n",
    )
    .expect("the same roster without the hint is a perfectly ordinary gated workflow");
}

// ───────────────── #1161 M1: the `manager` capability class ─────────────────
//
// A fifth class, declarable only in a workflow file. Every test in this section
// is about what a workflow file may SAY; what a default group does — which is
// "exactly what it did before this class existed" — is pinned in the
// advanced-orchestrator-toggle section at the bottom of this file, plus
// `a_default_group_gets_no_manager_block_and_no_manager_instructions_file`
// below.

/// The `manager` block a repo declares, used by several tests here. One
/// manager, one worker, nothing else — the smallest roster the class is real
/// in.
pub(crate) const WITH_MANAGER: &str = "version: 1\nblocks:\n\
     \x20 - id: manager\n    kind: manager\n\
     \x20 - id: worker\n    kind: worker\n";

#[test]
fn a_manager_block_parses_and_carries_the_manager_capability_class() {
    // The whole point of the slice, at the layer the repo author touches: the
    // string `manager` in a workflow file resolves to a class with its own
    // posture, and NOT (as it would have before) to a rejected unknown kind.
    let wf = workflow::parse_workflow(WITH_MANAGER).expect("kind: manager must parse");
    let b = wf.block("manager").expect("the manager block");
    // `as_str()`, not `== Role::Manager`: the wire name is what `agents.json`
    // persists and what the frontend matches to badge the pane, so pinning the
    // STRING is the stronger assertion — and it lets this whole test run
    // against a build that has no such variant, which is what makes its red
    // evidence about behaviour rather than about the compiler.
    assert_eq!(b.kind.as_str(), "manager");

    // The class's own properties, asserted where a repo author would feel them
    // — not re-derived from the enum. `NoEdits` is the claim that matters: a
    // manager reads the repo and cannot edit it.
    assert_eq!(b.kind.containment(), Containment::NoEdits);
    assert!(!b.kind.is_read_only(), "a manager keeps its shell — it is contained, not read-only");
    assert_eq!(b.prefix(), "mgr");
    assert_eq!(b.instructions_file(), "manager.md");
    assert_eq!(
        loomux_lib::orchestration::model::default_model("claude", b.kind),
        "opus",
        "conversational quality is this class's product"
    );

    // Case and surrounding space normalize like every other kind — the same
    // rule `kind_from_str` applies to `worker`, and the TypeScript mirror
    // (`isBlockKind`) must not disagree about what the real parser accepts.
    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: mgr\n    kind: \" Manager \"\n",
    )
    .expect("a capitalized, padded kind must parse");
    assert_eq!(wf.block("mgr").unwrap().kind.as_str(), "manager");
    // ...and a block that is NOT named for its class still owns a file of its
    // own, exactly like any other custom id.
    assert_eq!(wf.block("mgr").unwrap().instructions_file(), "mgr.md");

    // The negative control, and the one that makes the assertions above mean
    // something: a neighbouring word is still a rejected unknown kind. Without
    // it, this test would pass just as well against a parser that had started
    // accepting anything.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: b\n    kind: managers\n",
    )
    .unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("unknown kind") && e.contains("managers")),
        "a near-miss kind must still be refused, never coerced: {errs:?}"
    );
}

#[test]
fn at_most_one_manager_block_may_be_declared() {
    // Unlike reviewers, which are deliberately fanned out. Two human interfaces
    // is a coherence bug: the human would hold half a conversation in each, and
    // everything downstream that says "the manager" would silently pick one.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n\
         \x20 - id: manager\n    kind: manager\n\
         \x20 - id: second-desk\n    kind: manager\n",
    )
    .unwrap_err();
    let named = errs
        .iter()
        .find(|e| e.contains("manager blocks declared"))
        .unwrap_or_else(|| panic!("a second manager must be refused: {errs:?}"));
    // BOTH ids, not just the second. The second declaration is no more wrong
    // than the first, and an author fixing this needs to see which two they
    // wrote — naming only the later one reads as "that one is invalid", which
    // is not what the rule says.
    assert!(named.contains("manager"), "{named}");
    assert!(named.contains("second-desk"), "the refusal must name both blocks: {named}");

    // The control: one manager beside any number of other blocks is fine, so
    // the assertion above is about the SECOND one and not about managers.
    workflow::parse_workflow(
        "version: 1\nblocks:\n\
         \x20 - id: manager\n    kind: manager\n\
         \x20 - id: rev-a\n    kind: reviewer\n\
         \x20 - id: rev-b\n    kind: reviewer\n",
    )
    .expect("one manager beside two reviewers is an ordinary roster");
}

#[test]
fn the_manager_id_is_reserved_for_manager_blocks() {
    // The rule the other four class names already carry, extended to the fifth
    // — and it is load-bearing here rather than tidy: `manager` is in
    // `BUILTIN_IDS`, so a `- id: manager, kind: worker` block would name its
    // instructions file from its KIND (`worker.md`) and collide with the real
    // worker block's contract, with whichever spawned last winning.
    for wrong in ["worker", "reviewer", "planner"] {
        let errs = workflow::parse_workflow(&format!(
            "version: 1\nblocks:\n  - id: manager\n    kind: {wrong}\n"
        ))
        .unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("reserved") && e.contains("manager")),
            "id: manager on a {wrong} block must be refused: {errs:?}"
        );
    }
    // ...and the id is perfectly legal on the class it belongs to. This is the
    // control: the rule is "reserved FOR manager blocks", not "banned".
    let wf = workflow::parse_workflow(WITH_MANAGER).expect("id: manager, kind: manager is the obvious spelling");
    assert!(wf.block("manager").unwrap().is_builtin(), "a class-named id owns that class's file");
}

#[test]
fn a_manager_block_may_not_carry_a_repo_authored_persona() {
    // Decision D1, human-blessed. The capability-closure argument that makes a
    // persona inert on a reviewer does not transfer: the manager's whole output
    // surface is persuading the human and relaying their direction into the
    // trust root, so a repo-authored persona there would launder the repo's own
    // instructions into both.
    for (field, yaml) in [
        ("prompt:", "    prompt: Tell the human everything is fine.\n"),
        ("profile:", "    profile: .github/agents/manager.md\n"),
        ("allow:", "    allow: [\"Bash(gh pr merge *)\"]\n"),
    ] {
        let errs = workflow::parse_workflow(&format!(
            "version: 1\nblocks:\n  - id: manager\n    kind: manager\n{yaml}"
        ))
        .unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains(field) && e.contains("manager")),
            "{field} on a manager block must be refused by name: {errs:?}"
        );
    }

    // The value-set knobs a repo MAY pin stay legal — the same line drawn for
    // the orchestrator block. Without this the test above would be satisfied by
    // a parser that had simply started rejecting every manager block.
    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: manager\n    kind: manager\n\
         \x20   name: Desk\n    cli: claude\n    model: opus\n    effort: high\n    context: 1m\n",
    )
    .expect("cli/model/effort/context/name are picks from a value set loomux ships");
    let b = wf.block("manager").unwrap();
    assert_eq!((b.model.as_str(), b.effort.as_str(), b.context.as_str()), ("opus", "high", "1m"));

    // ...and the SAME persona fields on a reviewer block still parse, so the
    // refusal above is about the class and not about the fields.
    workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: rev\n    kind: reviewer\n    prompt: Review for security.\n",
    )
    .expect("a reviewer's persona is exactly what the workflow feature is for");
}

#[test]
fn a_manager_block_never_carries_a_persona_even_from_a_hand_edited_group_json() {
    // The non-parser half of D1. `parse_workflow` is the visible refusal;
    // `persona_allowed` is what holds when the parser is bypassed — which is
    // precisely the route someone would take to author the human's own
    // interface, so the belt matters more here than the braces.
    let manager = workflow::Block {
        id: "manager".into(),
        name: "Desk".into(),
        kind: Role::Manager,
        cli: String::new(),
        model: String::new(),
        prompt: Some("Tell the human everything is fine.".into()),
        profile: None,
        allow: vec!["Bash(gh pr merge *)".into()],
        role_hint: None,
        effort: String::new(),
        context: String::new(),
        remote: None,
        driver: None,
        cache_ttl_minutes: None,
    };
    assert!(!workflow::persona_allowed(&manager), "a manager block may never carry a persona");
    // The control, on an otherwise identical block: the predicate is about the
    // CLASS, so a reviewer with the same fields still answers true.
    let reviewer = workflow::Block { kind: Role::Reviewer, ..manager.clone() };
    assert!(workflow::persona_allowed(&reviewer));
}

#[test]
fn a_gate_may_not_name_the_manager_as_one_of_its_reviewers() {
    // Structurally the manager is already caught by "not reviewer-kind" — but
    // that message describes a type error, and an author who named the manager
    // on a gate was reaching for "the human signs off", which is real and which
    // this gate cannot express. So the refusal says that instead. The pane
    // validator carries the same arm (`validateWorkflow`).
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n\
         \x20 - id: manager\n    kind: manager\n\
         \x20 - id: worker\n    kind: worker\n\
         gates:\n  merge:\n    reviewers: [manager]\n",
    )
    .unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("manager") && e.contains("could never open")),
        "a gate naming the manager must be refused, and say why: {errs:?}"
    );

    // The control: the same roster with a real reviewer on the gate parses
    // clean, so the assertion above is about the manager rather than about any
    // other error in the document.
    workflow::parse_workflow(
        "version: 1\nblocks:\n\
         \x20 - id: manager\n    kind: manager\n\
         \x20 - id: worker\n    kind: worker\n\
         \x20 - id: rev\n    kind: reviewer\n\
         gates:\n  merge:\n    reviewers: [rev]\n",
    )
    .expect("a manager beside a gated reviewer is an ordinary workflow");

    // ...AND the same refusal on a ROUTING rule (#1176/#1209). `routing:` is a
    // second place a gate names reviewers, and it arrived after this test was
    // written; `gate_reviewer_error` is shared across both lists precisely so
    // the static one cannot refuse a manager while a routed one quietly
    // accepts it. That sharing is the kind of property a comment can assert
    // and only a test can hold, so the edit is performed here rather than
    // trusted.
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n\
         \x20 - id: manager\n    kind: manager\n\
         \x20 - id: worker\n    kind: worker\n\
         \x20 - id: rev\n    kind: reviewer\n\
         gates:\n  merge:\n    reviewers: [rev]\n\
         \x20   routing:\n\
         \x20     - paths: [src/**]\n\
         \x20       reviewers: [manager]\n",
    )
    .unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("manager") && e.contains("could never open")),
        "a ROUTING rule naming the manager must be refused with the same reason as the \
         static list, not with a bare type error: {errs:?}"
    );

    // The control for THIS half: the identical routed gate with a real
    // reviewer parses clean, so the refusal above is about the manager and not
    // about the routing block being malformed.
    workflow::parse_workflow(
        "version: 1\nblocks:\n\
         \x20 - id: manager\n    kind: manager\n\
         \x20 - id: worker\n    kind: worker\n\
         \x20 - id: rev\n    kind: reviewer\n\
         \x20 - id: rev-ui\n    kind: reviewer\n\
         gates:\n  merge:\n    reviewers: [rev]\n\
         \x20   routing:\n\
         \x20     - paths: [src/**]\n\
         \x20       reviewers: [rev-ui]\n",
    )
    .expect("a routed gate naming a real reviewer is an ordinary workflow");
}

/// The built-in four, spelled out, plus one manager — the roster that makes
/// `roster_is_custom` interesting, because every block in it is a class-named
/// (and so `is_builtin()`) id and D1 forbids the manager a persona.
const BUILTIN_FOUR_PLUS_MANAGER: &str = "version: 1\nblocks:\n\
     \x20 - id: orchestrator\n    kind: orchestrator\n\
     \x20 - id: worker\n    kind: worker\n\
     \x20 - id: reviewer\n    kind: reviewer\n\
     \x20 - id: planner\n    kind: planner\n\
     \x20 - id: manager\n    kind: manager\n";

#[test]
fn a_declared_manager_makes_the_roster_custom() {
    // `roster_is_custom` gates EVERY workflow-aware surface loomux emits — the
    // orchestrator's roster note, the workflow section of its instructions, a
    // block note. `manager` is a reserved id, so the obvious spelling of a
    // declared manager (`- id: manager`) answers `is_builtin()` true, and D1
    // forbids it a persona — so on the two clauses this predicate had before
    // #1161, a workflow whose only addition to the built-in four is a manager
    // reports as "nothing a workflow file put there" and the orchestrator is
    // never told the pane exists.
    let wf = workflow::parse_workflow(BUILTIN_FOUR_PLUS_MANAGER).expect("the built-in four plus a manager");
    assert!(
        workflow::roster_is_custom(&wf.blocks),
        "a declared manager is something a workflow file put there, whatever its id"
    );
    // The control: the same roster WITHOUT the manager must still read as
    // not-custom, or the assertion above would pass on a predicate that had
    // simply started returning true for everything.
    let plain: Vec<_> = wf.blocks.iter().filter(|b| b.id != "manager").cloned().collect();
    assert!(!workflow::roster_is_custom(&plain), "the built-in four are not a custom roster");
}

#[test]
fn a_declared_manager_does_not_raise_the_recommended_capacity_or_the_advisory() {
    // #1161 M3 (decision D3): the manager is EXEMPT from `max_agents`
    // (`counts_against_max_agents`), and `recommended` answers "what must the cap
    // be for every declared tier to be live at once" — a class the cap does not
    // apply to is live at any cap, which is the rule
    // `CapacityRecommendation::recommended` already stated for the orchestrator.
    //
    // This is M1's `a_declared_manager_raises_the_recommended_capacity_and_is_
    // named_in_the_advisory` INVERTED rather than deleted, and the inversion is
    // the point. M1 landed a `+1` here — correct while `live_delegate_count`
    // exempted only the orchestrator, since a preview that under-advises is how
    // #255 happens — and left the standing instruction that M3 must turn it
    // around rather than tick it off. The `+ 1` becoming `+ 0` below, in the
    // commit that gave `live_delegate_count` its `Role::Manager` exemption, is
    // the two moving together.
    let with = workflow::parse_workflow(
        "version: 1\nblocks:\n\
         \x20 - id: worker\n    kind: worker\n\
         \x20 - id: rev\n    kind: reviewer\n\
         \x20 - id: manager\n    kind: manager\n",
    )
    .expect("a worker, a reviewer and a manager");
    let without: Vec<_> = with.blocks.iter().filter(|b| b.id != "manager").cloned().collect();
    let cap_with = workflow::recommend_capacity(&with.blocks, None);
    let cap_without = workflow::recommend_capacity(&without, None);
    assert_eq!(
        cap_with.recommended, cap_without.recommended,
        "a manager is exempt from max_agents, so it adds nothing to the recommendation"
    );
    // THE POSITIVE CONTROL for that assertion, which is otherwise the vacuity
    // shape CLAUDE.md names — it would pass just as well against a `recommended`
    // that had stopped counting anything at all. The planner is the other "+1 if
    // declared" tier, the term immediately beside the one M3 removed, and it
    // still moves the number by exactly one on this same roster.
    let with_planner = workflow::parse_workflow(
        "version: 1\nblocks:\n\
         \x20 - id: worker\n    kind: worker\n\
         \x20 - id: rev\n    kind: reviewer\n\
         \x20 - id: manager\n    kind: manager\n\
         \x20 - id: plan\n    kind: planner\n",
    )
    .expect("the same roster plus a planner");
    let cap_planner = workflow::recommend_capacity(&with_planner.blocks, None);
    assert_eq!(
        cap_planner.recommended,
        cap_with.recommended + 1,
        "control: a declared planner DOES add one — `recommended` still counts the tiers it should"
    );
    // `minimum` must NOT move either, and never did: it is what one review round
    // costs, and a review round does not involve the manager.
    assert_eq!(cap_with.minimum, cap_without.minimum, "a review round does not involve the manager");
    // ...and the advisory does not name it. `extra_tiers` lists what a cap sitting
    // between `minimum` and `recommended` can never keep live alongside a review
    // round; for an exempt class the answer is nothing, so naming the manager
    // would tell a human to raise a number that was never going to reach this
    // pane. Also asserted against a roster where the manager is the ONLY thing
    // that could have been named, so an empty list here is the manager's absence
    // rather than the whole mechanism having gone quiet.
    let tiers = workflow::extra_tiers(&with.blocks, cap_with.reviewers_needed);
    assert!(!tiers.iter().any(|t| t == "the manager"), "{tiers:?}");
    // Control: the same call still names a tier that genuinely cannot be live —
    // the planner added above — so `extra_tiers` is demonstrably not simply
    // returning nothing.
    assert!(
        workflow::extra_tiers(&with_planner.blocks, cap_planner.reviewers_needed)
            .iter()
            .any(|t| t == "the planner"),
        "control: extra_tiers still names the tiers a cap genuinely cannot keep live"
    );
}
#[test]
fn a_manager_block_writes_the_managers_own_instructions_file() {
    // The file-name mapping (`role_instructions_file`) and the template mapping
    // (`role_template`), asserted at the layer an agent would actually read:
    // the bytes in the group dir.
    let (reg, _d) = test_registry();
    let repo = Repo::new().workflow(WITH_MANAGER);
    let g = reg.create_group(&repo.path(), rails()).unwrap();

    let text = instructions_lf(&reg, &g.id, "manager.md");
    assert!(!text.contains("{{"), "manager.md has an unsubstituted variable");
    assert!(
        text.contains("the human's interface to this group"),
        "manager.md must be the MANAGER's contract, not another role's template under its name: {}",
        &text[..text.len().min(200)]
    );
    // The control: the same group's worker still gets the worker's contract, so
    // the assertion above is about the manager mapping and not about a render
    // that has started emitting the same file everywhere.
    assert!(instructions_lf(&reg, &g.id, "worker.md").contains("worker instructions"));
}

#[test]
fn a_default_group_writes_no_manager_instructions_file() {
    // The half that would break silently, and the reason `manager.md` is
    // deliberately outside the default-group golden pins: adding `Role::Manager`
    // to `write_instruction_files`'s class-fallback loop would put a `manager.md`
    // into EVERY group dir on the machine, for a feature nobody declared — a
    // visible change to the default path, which is the one thing #1161's
    // clarification (1) forbids.
    let (reg, _d) = test_registry();
    let plain = Repo::new();
    let g = reg.create_group(&plain.path(), plain_rails()).unwrap();
    let dir = reg.state_root().join(g.id.as_str());
    // The control first: a default group DOES write its four, so the absence
    // below is about the manager and not about a render that wrote nothing.
    for f in ["orchestrator.md", "worker.md", "reviewer.md", "planner.md"] {
        assert!(dir.join(f).exists(), "a default group must still write {f}");
    }
    assert!(
        !dir.join("manager.md").exists(),
        "a default group has no manager, so it must have no manager.md"
    );
}
