//! The default roster, generated agent files against each CLI's schema, the argv-length guard, persistence.
//!
//! One module of the `workflow` integration-test target (`main.rs`),
//! split out of the former single-file `tests/workflow.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ───────────────────── the default roster: nothing changed ──────────────────

#[test]
fn default_roster_command_lines_now_carry_the_durable_contract_via_a_generated_claude_agent_file() {
    // Formerly "THE regression pin (#222)": a repo with no `.loomux/
    // workflow.yml` used to get the byte-for-byte pre-#222 command line —
    // NO `--agents`/`--agent`/`--settings` at all, since those only ever
    // carried a REPO persona and a default-roster block has none.
    //
    // #416 deliberately changed that: the built-in role CONTRACT (mechanics +
    // class template — exactly the bytes written to the block's instructions
    // file) now rides the CLI's native system-prompt mechanism for EVERY
    // block, persona or not — closing a real gap (see docs/design/
    // orchestration.md's #416 note): compaction could dilute the contract
    // when it lived only in a "read this file" kickoff step.
    //
    // **Round #417 correction 6:** #416's original mechanism put the whole
    // contract inline in `--agents '<json>'`. A live demo hit Windows
    // CreateProcessW's hard 32,767-character command-line limit once the
    // contract (many KB) rode argv on every block, not just short repo
    // personas. The mechanism changed again — a loomux-generated
    // `~/.claude/agents/<handle>.md` FILE now carries the contract, and
    // `--agent <handle>` alone activates it — but the OUTCOME #416 promised
    // (the durable contract on the system-prompt layer, for every block) is
    // unchanged, and everything else about the command line (model,
    // permission-mode, allow/deny lists) is still pinned exactly.
    let (reg, d) = test_registry();
    let repo = Repo::new(); // no .loomux/ at all — the common case
    let g = reg.create_group(&repo.path(), rails()).unwrap();

    assert_eq!(g.guardrails.blocks.len(), 4, "the built-in roster is synthesized");
    for b in &g.guardrails.blocks {
        assert!(b.is_builtin(), "block {:?} is not a built-in", b.id);
        assert!(!b.has_persona(), "a built-in block must carry no persona");
    }

    let cfg = Path::new("C:/x/cfg.json");
    let gdir = Path::new("C:/data/group");
    let wd = Path::new("C:/repo");

    // Build each block exactly the way `spawn_agent_ex` does: resolve its
    // persona, read back the instructions file for `contract` (already
    // written by `create_group`'s `write_instruction_files`), compile it,
    // hand it to the command builder. Returns the shell-string command PLUS
    // the generated agent-file handle, so the test can check the contract's
    // CONTENT on disk, not just that the flag is present.
    let line = |block_id: &str, auto_ops: bool| -> (String, String) {
        let b = g.guardrails.block(block_id).unwrap();
        let cli = workflow::cli_of(b, &g.guardrails.agent_cli);
        let persona = reg.resolve_persona(&g, b).unwrap();
        assert!(persona.is_none(), "a default-roster block has no persona to compile");
        let instructions_body = instructions_lf(&reg, &g.id, &b.instructions_file());
        let contract = block_contract_text(&instructions_body, persona.as_ref());
        assert_eq!(contract, instructions_body, "no persona ⇒ contract IS the instructions body, unchanged");
        assert!(!contract.trim().is_empty(), "{block_id}'s contract must never be empty");
        let inject = reg.persona_inject(&g.id, b, cli, persona.as_ref(), &contract);
        assert!(inject.kickoff.is_none(), "claude never uses the kickoff fallback");
        assert!(inject.extra_allow.is_empty(), "no persona ⇒ no extra allow patterns");
        assert!(
            inject.claude_append_system_prompt_file.is_none(),
            "the generated-file path must succeed here — test_registry's override dir is writable"
        );
        assert_eq!(inject.contract_carrier, ContractCarrier::SystemLayerFull, "the contract must be durable for every claude block");
        let handle = inject.claude_agent.clone().expect("a generated Claude agent file handle");
        let cmd = reg.build_agent_command(
            cli,
            workflow::model_of(b, &g.guardrails.agent_cli),
            auto_ops,
            cfg,
            None,
            gdir,
            wd,
            None,
            false,
            b.kind.containment(),
            &inject,
        );
        let generated_path = d.path().join("claude-agents").join(format!("{handle}.md"));
        let generated = fs::read_to_string(&generated_path).expect("generated agent file must exist");
        // Unlike the pre-round-6 `--agents` payload, this is a FILE, not a
        // shell token — no apostrophe-mangling/ASCII-escaping, loomux's own
        // template prose keeps its real apostrophes verbatim.
        assert!(
            generated.contains(&contract),
            "{block_id}'s generated file must carry the contract verbatim: {generated}"
        );
        assert!(
            generated.starts_with(&format!("---\nname: {handle}\ndescription: \"{block_id}\"\n---\n")),
            "no persona ⇒ description falls back to the block id: {generated}"
        );
        (cmd, handle)
    };

    let expect = |cmd: &str, handle: &str, block_id: &str, model: &str, perm: &str, extra: &str| {
        let expected = format!(
            "claude --mcp-config \"C:/x/cfg.json\" --strict-mcp-config \
             --model {model} --permission-mode {perm} --add-dir \"C:/data/group\" \
             --allowedTools mcp__orrerix{extra} --agent {handle}"
        );
        assert_eq!(cmd, &expected, "{block_id}'s command line changed in an unexpected way");
        assert_eq!(handle, &format!("loomux-{}-{block_id}", g.id), "handle naming convention");
        assert!(cmd.len() < 500, "the command line must stay short now that the contract rides a file, not argv: {} chars", cmd.len());
    };

    let (cmd, handle) = line("worker", true);
    expect(&cmd, &handle, "worker", "sonnet", "auto", " \"Bash(git *)\" \"Bash(gh *)\"");
    let (cmd, handle) = line("reviewer", false);
    // #462: the built-in reviewer block's own command line — derived here from
    // `b.kind.containment()`, exactly as `spawn_agent_ex` derives it — now
    // carries the editing-tool denials. Everything else is untouched: still
    // `acceptEdits` (no promotion to unattended), still no pre-approved git/gh,
    // and no `Bash(git …)` denials, because the shell is the job.
    expect(&cmd, &handle, "reviewer", "sonnet", "acceptEdits", " --disallowedTools Edit Write NotebookEdit");
    let (cmd, handle) = line("planner", false);
    expect(
        &cmd, &handle, "planner", "opus", "dontAsk",
        // #448: `MultiEdit` dropped from CLAUDE_EDIT_DENY_TOOLS — it
        // matches no real Claude Code tool. #465: a read-only block runs
        // `dontAsk` (pre-approved tools only), not `auto` — see
        // `claude_effective_permission_mode`'s doc. A REVIEWER block stays on
        // `acceptEdits`/`auto` (#462): `dontAsk` would deny the shell it works through.
        " \"Bash(git *)\" \"Bash(gh *)\" --disallowedTools Edit Write NotebookEdit \
          \"Bash(git commit *)\" \"Bash(git push *)\"",
    );
    let (cmd, handle) = line("orchestrator", true);
    expect(&cmd, &handle, "orchestrator", "opus", "auto", " \"Bash(git *)\" \"Bash(gh *)\"");

    // Agent ids and instruction-file paths are unchanged too — they are in the
    // kickoff text the agent reads.
    let w = reg.spawn_agent(&g.id, Role::Worker, "", "t", false, None).unwrap();
    assert!(w.id.starts_with("w-"), "worker ids stay `w-N`, got {}", w.id);
    assert_eq!(w.block, "worker");
    assert_eq!(g.guardrails.block("worker").unwrap().instructions_file(), "worker.md");
    assert_eq!(g.guardrails.block("planner").unwrap().instructions_file(), "planner.md");
    let k = reg.kickoff_prompt(&w, &g, "note", None);
    assert!(k.contains("worker.md"), "the kickoff still points at worker.md");
    assert!(
        !k.contains("workflow.yml"),
        "a group with no workflow file must not be told about one: {k}"
    );
}

// ─── round 8: both CLIs' generated agent files against their real schema ───

/// Parse the frontmatter block (between the two `---` delimiters) of a
/// generated agent file as actual YAML — round 8 review: the pre-round-8
/// Copilot test coverage only ever checked a `starts_with("---\nname: ...")`
/// prefix, which happily passed a file missing `description:` entirely (the
/// live incident: Copilot's own `CustomAgentLoadFailedError: ...
/// description: Required`). A prefix check proves the file LOOKS like
/// frontmatter; only an actual parse proves every field a real loader would
/// require is really there.
pub(crate) fn parse_agent_frontmatter(generated: &str) -> std::collections::BTreeMap<String, String> {
    let mut parts = generated.splitn(3, "---\n");
    assert_eq!(parts.next(), Some(""), "must open with a bare --- line: {generated}");
    let frontmatter = parts.next().expect("a closing --- must follow: {generated}");
    serde_norway::from_str(frontmatter)
        .unwrap_or_else(|e| panic!("frontmatter did not parse as YAML: {e}\n{frontmatter}"))
}

#[test]
fn generated_agent_files_satisfy_each_clis_documented_required_frontmatter_fields() {
    // The exact incident shape, both CLIs, both reproduced via the SAME
    // no-persona default-roster path the live demo used: GitHub's custom-
    // agents-configuration reference (docs.github.com/en/copilot/reference/
    // custom-agents-configuration) requires `description` (not `name`,
    // which defaults to the filename); Claude's sub-agents doc
    // (code.claude.com/docs/en/sub-agents, "Supported frontmatter fields")
    // requires BOTH `name` and `description`. A generated file missing
    // either must never reach either CLI again.
    let (reg, d) = test_registry();

    // Claude side.
    let repo = Repo::new();
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let b = g.guardrails.block("worker").unwrap();
    let cli = workflow::cli_of(b, &g.guardrails.agent_cli);
    let instructions_body = instructions_lf(&reg, &g.id, &b.instructions_file());
    let contract = block_contract_text(&instructions_body, None);
    let inject = reg.persona_inject(&g.id, b, cli, None, &contract);
    let handle = inject.claude_agent.clone().expect("a generated Claude agent file handle");
    let generated = fs::read_to_string(d.path().join("claude-agents").join(format!("{handle}.md"))).unwrap();
    let fm = parse_agent_frontmatter(&generated);
    assert!(fm.get("name").is_some_and(|v| !v.is_empty()), "Claude requires `name`: {fm:?}");
    assert!(fm.get("description").is_some_and(|v| !v.is_empty()), "Claude requires `description`: {fm:?}");

    // Copilot side — this is the round-8 live-demo blocker's exact shape: a
    // default-roster (no persona) orchestrator block on the Copilot CLI.
    let g2 = reg.create_group(&repo.path(), Guardrails { agent_cli: "copilot".into(), ..rails() }).unwrap();
    let b2 = g2.guardrails.block_for(Role::Orchestrator).unwrap();
    let cli2 = workflow::cli_of(b2, &g2.guardrails.agent_cli);
    let instructions_body2 = instructions_lf(&reg, &g2.id, &b2.instructions_file());
    let contract2 = block_contract_text(&instructions_body2, None);
    let inject2 = reg.persona_inject(&g2.id, b2, cli2, None, &contract2);
    let handle2 = inject2.copilot_agent.clone().expect("a generated Copilot agent file handle");
    let generated2 = fs::read_to_string(d.path().join("copilot-agents").join(format!("{handle2}.agent.md"))).unwrap();
    let fm2 = parse_agent_frontmatter(&generated2);
    assert!(
        fm2.get("description").is_some_and(|v| !v.is_empty()),
        "Copilot requires `description` — a missing/empty one is exactly `CustomAgentLoadFailedError: \
         ... description: Required`, the round-8 live-demo blocker: {fm2:?}"
    );
    // `name` is documented optional for Copilot (defaults to the filename) —
    // loomux still sets it deliberately (harmless, gives a readable display
    // name), so pin that it's present too, not just tolerated if absent.
    assert!(fm2.get("name").is_some_and(|v| !v.is_empty()), "{fm2:?}");
}

// ─────── round #417 correction 6: the argv-length bug and its fix ───────

#[test]
fn a_thirty_kb_contract_still_produces_a_short_command_line() {
    // The regression this whole round fixes, reproduced directly: a live
    // demo hit Windows CreateProcessW's hard 32,767-character command-line
    // limit once the durable contract (#416, many KB of mechanics core +
    // template) rode `--agents` inline. A 30KB+ persona pins that the fix
    // holds regardless of payload size — the command line stays short no
    // matter how large the contract gets, because it never carries it.
    let (reg, d) = test_registry();
    let huge_persona = "x".repeat(30_000);
    let repo = Repo::new()
        .workflow("version: 1\nblocks:\n  - id: huge\n    kind: worker\n    cli: claude\n    profile: .github/agents/huge.md\n")
        .agent_file("huge.md", &format!("---\nname: huge\ndescription: A huge persona.\n---\n{huge_persona}"));
    let g = reg.create_group(&repo.path(), rails()).unwrap();

    let b = g.guardrails.block("huge").unwrap();
    let cli = workflow::cli_of(b, &g.guardrails.agent_cli);
    let persona = reg.resolve_persona(&g, b).unwrap();
    let instructions_body = instructions_lf(&reg, &g.id, &b.instructions_file());
    let contract = block_contract_text(&instructions_body, persona.as_ref());
    assert!(contract.len() > 30_000, "the contract itself must actually be huge: {} bytes", contract.len());
    let inject = reg.persona_inject(&g.id, b, cli, persona.as_ref(), &contract);
    let handle = inject.claude_agent.clone().expect("a generated Claude agent file handle");
    let cfg = Path::new("C:/x/cfg.json");
    let gdir = Path::new("C:/data/group");
    let wd = Path::new("C:/repo");
    let cmd = reg.build_agent_command(cli, "sonnet", false, cfg, None, gdir, wd, None, false, Containment::None, &inject);
    let argv = reg.build_agent_argv(cli, "sonnet", false, cfg, None, gdir, wd, None, false, Containment::None, &inject);

    assert!(cmd.len() < 1000, "the command line must stay short regardless of contract size: {} chars: {cmd}", cmd.len());
    assert!(command_line_length_guard(&argv).is_ok(), "the guard must never trip on the fixed path");

    let generated = fs::read_to_string(d.path().join("claude-agents").join(format!("{handle}.md"))).unwrap();
    assert!(generated.len() > 30_000, "the FULL contract must still reach the agent — just via a file, not argv: {} bytes", generated.len());
    assert!(generated.contains(&huge_persona));
}

#[test]
fn command_line_length_guard_fails_loudly_on_an_oversized_argument() {
    // Belt-and-braces (round #417 correction 6): a regression that puts a
    // large blob back on argv must fail LOUDLY, pre-spawn, naming the
    // oversized piece — never reproduce the unreadable CreateProcessW wall
    // the user's live demo hit.
    let ok = vec!["claude".to_string(), "--model".to_string(), "sonnet".to_string()];
    assert!(command_line_length_guard(&ok).is_ok());

    let oversized_single = vec!["claude".to_string(), "--agents".to_string(), "x".repeat(29_000)];
    let err = command_line_length_guard(&oversized_single).unwrap_err();
    assert!(err.contains("32,767"), "{err}");
    assert!(err.contains("argument #2"), "must name the offending argument by index: {err}");

    // Many small-but-not-individually-oversized arguments summing past the
    // limit must ALSO trip it — the guard checks the total, not just the
    // largest single token.
    let many_small: Vec<String> = std::iter::repeat("x".repeat(2_000)).take(20).collect();
    let err = command_line_length_guard(&many_small).unwrap_err();
    assert!(err.contains("32,767"), "{err}");
}

#[test]
fn claude_agent_file_write_failure_falls_back_to_append_system_prompt_file() {
    // Round #417 correction 6: when `~/.claude/agents` can't be created —
    // simulated here for REAL (a regular FILE already occupies the target
    // path, so `fs::create_dir_all` genuinely fails, not asserted from
    // reasoning alone) — `persona_inject` must fall back to `--append-
    // system-prompt-file` pointed at the group's own instructions file,
    // never silently lose the contract and never fall back to putting it
    // on argv either.
    let (reg, d) = test_registry();
    let blocked_path = d.path().join("claude-agents-blocked");
    fs::write(&blocked_path, b"not a directory").unwrap();
    reg.set_claude_agents_dir_override(blocked_path);
    let repo = Repo::new();
    let g = reg.create_group(&repo.path(), rails()).unwrap();

    let b = g.guardrails.block("worker").unwrap();
    let cli = workflow::cli_of(b, &g.guardrails.agent_cli);
    let persona = reg.resolve_persona(&g, b).unwrap();
    let instructions_body = instructions_lf(&reg, &g.id, &b.instructions_file());
    let contract = block_contract_text(&instructions_body, persona.as_ref());
    let inject = reg.persona_inject(&g.id, b, cli, persona.as_ref(), &contract);

    assert!(inject.claude_agent.is_none(), "the generated-file path must have failed");
    assert_eq!(inject.contract_carrier, ContractCarrier::SystemLayerFull, "still system-prompt-layer durable even in the fallback");
    let path = inject.claude_append_system_prompt_file.clone().expect("must fall back to --append-system-prompt-file");

    let cfg = Path::new("C:/x/cfg.json");
    let gdir = Path::new("C:/data/group");
    let wd = Path::new("C:/repo");
    let cmd = reg.build_agent_command(cli, "sonnet", false, cfg, None, gdir, wd, None, false, Containment::None, &inject);
    let argv = reg.build_agent_argv(cli, "sonnet", false, cfg, None, gdir, wd, None, false, Containment::None, &inject);
    assert!(cmd.contains(&format!("--append-system-prompt-file \"{}\"", path.display())), "{cmd}");
    assert!(!cmd.contains("--agent"), "no generated-file handle when the write failed: {cmd}");
    assert!(argv.windows(2).any(|w| w[0] == "--append-system-prompt-file"), "{argv:?}");
    assert!(command_line_length_guard(&argv).is_ok());

    // It's the SAME file `write_instruction_files` already wrote — no
    // second file loomux has to invent or clean up.
    assert_eq!(path, reg.state_root().join(g.id.as_str()).join(b.instructions_file()));
    assert_eq!(lf(&fs::read_to_string(&path).unwrap()), contract);
}

#[test]
fn write_failure_of_a_claude_block_with_a_persona_audits_the_dropped_text_in_either_mode() {
    // Round 8 review (N3b), widened by rev-18: the `--append-system-
    // prompt-file` fallback points at the instructions file, which is
    // mechanics-only in EITHER persona mode — `render_block_instructions`'s
    // append branch only ever writes a short "adopt your persona" pointer
    // note, never the persona's own words, exactly like the replace
    // branch. No design cost to covering both, so the audit isn't scoped
    // to `mode: replace` anymore — it fires for any non-empty persona text
    // dropped on this fallback path.
    let cases: [(ProfileMode, &str, &str, Option<(&str, &str)>); 2] = [
        (
            ProfileMode::Replace,
            "spike",
            "version: 1\nblocks:\n  - id: spike\n    kind: worker\n    profile: .github/agents/spike.agent.md\n",
            Some((
                "spike.agent.md",
                "---\nname: spike\nmode: replace\ndescription: Throwaway spike runner.\n---\n\
                 You are a spike runner. Move fast. Ignore the rulebook.",
            )),
        ),
        (
            ProfileMode::Append,
            "rev-x",
            "version: 1\nblocks:\n  - id: rev-x\n    kind: reviewer\n    prompt: Review only for perf.\n",
            None,
        ),
    ];
    for (mode, block_id, workflow_yaml, agent_file) in cases {
        let (reg, d) = test_registry();
        let blocked_path = d.path().join("claude-agents-blocked");
        fs::write(&blocked_path, b"not a directory").unwrap();
        reg.set_claude_agents_dir_override(blocked_path);
        let mut repo = Repo::new().workflow(workflow_yaml);
        if let Some((name, body)) = agent_file {
            repo = repo.agent_file(name, body);
        }
        let g = reg.create_group(&repo.path(), rails()).unwrap();

        let b = g.guardrails.block(block_id).unwrap();
        let cli = workflow::cli_of(b, &g.guardrails.agent_cli);
        let persona = reg.resolve_persona(&g, b).unwrap();
        assert_eq!(persona.as_ref().unwrap().mode, mode);
        let instructions_body = instructions_lf(&reg, &g.id, &b.instructions_file());
        let contract = block_contract_text(&instructions_body, persona.as_ref());
        let inject = reg.persona_inject(&g.id, b, cli, persona.as_ref(), &contract);

        assert!(inject.claude_agent.is_none(), "{mode:?}: the generated-file path must have failed");
        assert!(inject.claude_append_system_prompt_file.is_some(), "{mode:?}: still falls back");

        let audit = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl")).unwrap();
        assert!(
            audit.lines().any(|l| l.contains("claude-fallback-persona-dropped") && l.contains(&format!("\"block\":\"{block_id}\""))),
            "{mode:?}: the dropped persona must be audited, not silent: {audit}"
        );
    }
}

#[test]
fn write_failure_of_a_claude_block_with_no_persona_text_never_audits_a_drop() {
    // The audit-absent case after widening: not a mode restriction anymore
    // (rev-18 removed that), an EMPTY-TEXT restriction — there is nothing
    // to have been dropped. A default-roster block (no persona at all)
    // must never get a false-positive "dropped" audit.
    let (reg, d) = test_registry();
    let blocked_path = d.path().join("claude-agents-blocked");
    fs::write(&blocked_path, b"not a directory").unwrap();
    reg.set_claude_agents_dir_override(blocked_path);
    let repo = Repo::new(); // no .loomux/ at all — default roster, no persona
    let g = reg.create_group(&repo.path(), rails()).unwrap();

    let b = g.guardrails.block("worker").unwrap();
    let cli = workflow::cli_of(b, &g.guardrails.agent_cli);
    let persona = reg.resolve_persona(&g, b).unwrap();
    assert!(persona.is_none(), "default roster has no persona");
    let instructions_body = instructions_lf(&reg, &g.id, &b.instructions_file());
    let contract = block_contract_text(&instructions_body, persona.as_ref());
    let inject = reg.persona_inject(&g.id, b, cli, persona.as_ref(), &contract);
    assert!(inject.claude_agent.is_none(), "the generated-file path must have failed");
    assert!(inject.claude_append_system_prompt_file.is_some());

    let audit = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl")).unwrap();
    assert!(
        !audit.lines().any(|l| l.contains("claude-fallback-persona-dropped")),
        "nothing was dropped — there was no persona text to lose: {audit}"
    );
}

#[test]
fn the_compaction_self_check_clause_reaches_both_clis_generated_files_under_cap() {
    // Round 8 review (N3a): delivery-independent insurance against a
    // missed/delayed reinjection — both CLIs' generated files must
    // instruct the agent to re-read its instructions file after any
    // compaction, REGARDLESS of whether loomux's own notice arrives. Cheap
    // enough to never threaten Copilot's documented body cap.
    let (reg, d) = test_registry();
    let repo = Repo::new(); // no .loomux/ at all — default roster

    // Claude.
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let b = g.guardrails.block("worker").unwrap();
    let cli = workflow::cli_of(b, &g.guardrails.agent_cli);
    let instructions_body = instructions_lf(&reg, &g.id, &b.instructions_file());
    let contract = block_contract_text(&instructions_body, None);
    let inject = reg.persona_inject(&g.id, b, cli, None, &contract);
    let handle = inject.claude_agent.clone().expect("a generated Claude agent file handle");
    let generated = fs::read_to_string(d.path().join("claude-agents").join(format!("{handle}.md"))).unwrap();
    assert!(generated.contains("re-read") && generated.contains("worker.md"), "{generated}");
    assert!(generated.contains("even if no re-grounding notice arrives"), "{generated}");

    // Copilot.
    let g2 = reg.create_group(&repo.path(), Guardrails { agent_cli: "copilot".into(), ..rails() }).unwrap();
    let b2 = g2.guardrails.block_for(Role::Orchestrator).unwrap();
    let cli2 = workflow::cli_of(b2, &g2.guardrails.agent_cli);
    let instructions_body2 = instructions_lf(&reg, &g2.id, &b2.instructions_file());
    let contract2 = block_contract_text(&instructions_body2, None);
    let inject2 = reg.persona_inject(&g2.id, b2, cli2, None, &contract2);
    let handle2 = inject2.copilot_agent.clone().expect("a generated Copilot agent file handle");
    let generated2 = fs::read_to_string(d.path().join("copilot-agents").join(format!("{handle2}.agent.md"))).unwrap();
    assert!(generated2.contains("re-read") && generated2.contains("orchestrator.md"), "{generated2}");
    assert!(generated2.contains("even if no re-grounding notice arrives"), "{generated2}");
    let body_chars = {
        let mut parts = generated2.splitn(3, "---\n");
        parts.next();
        parts.next();
        parts.next().unwrap_or("").chars().count()
    };
    assert!(body_chars < 30_000, "the self-check clause must never threaten the documented cap: {body_chars} chars");
}

#[test]
fn a_broken_workflow_file_is_audited_and_skipped_never_fatal() {
    // A repo file must never be able to stop a group from launching. It is
    // audited (every error, not just the first) and the built-in roster stands.
    let (reg, _d) = test_registry();
    let repo = Repo::new().workflow("version: 1\nblocks:\n  - id: w\n    kind: not-a-kind\n");
    let g = reg.create_group(&repo.path(), rails()).expect("a broken workflow must not fail the launch");

    assert_eq!(g.guardrails.blocks.len(), 4, "the group falls back to the built-in roster");
    assert!(g.guardrails.block("worker").is_some());
    // ...and the agents still spawn.
    reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();

    let audit = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl")).unwrap();
    let invalid: Value = audit
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|v| v["action"] == "workflow-invalid")
        .expect("the validation failure must be audited");
    let errors = invalid["detail"]["errors"].as_array().unwrap();
    assert!(
        errors.iter().any(|e| e.as_str().unwrap().contains("unknown kind")),
        "the audit must say WHAT was wrong: {errors:?}"
    );

    // Unparseable YAML is the same story, not a panic.
    let repo2 = Repo::new().workflow("version: 1\nblocks: [ this is not: valid: yaml");
    let g2 = reg.create_group(&repo2.path(), rails()).unwrap();
    assert_eq!(g2.guardrails.blocks.len(), 4);
}

// ─────────────────────────── persistence round-trip ─────────────────────────

/// #1457 review, premortem 1. The invariant `remote: Some(_) => cli == "claude"
/// and the kind is one the orchestrator spawns` is established twice —
/// `parse_workflow` and `read_blocks` each derive it — and asserted nowhere.
///
/// `Guardrails::clamped` runs after both and is free to rewrite blocks: it
/// already writes `id`, `name`, `model`, `effort` and `context`, drops blocks on
/// two rules, and prepends a synthesized orchestrator. It happens not to touch
/// `cli`, which is true by reading and enforced by nothing. Add a `cli`
/// normalization there — a lowercase, or a fallback when `cli_caps` returns
/// `None`, both plausible as CLIs are added — and a block carrying `remote:`
/// comes out on a CLI whose session identity cannot survive the trip, with
/// nothing red to say so.
///
/// **The fixture makes the operands COLLIDE**, per the non-interference rule: a
/// pin whose subject `clamped` never touches holds under every implementation,
/// the one it forbids included.
///
/// Finding a field `clamped` really rewrites took a CI round. `model: OPUS` plus
/// `effort: high` came back byte-identical: `sanitize_model` accepts `OPUS` as
/// written, `clamped_knob` returns `high` unchanged for a CLI that honors it,
/// and `parse_workflow` refuses an effort the block's own `cli:` cannot honor —
/// so an unhonorable one cannot be smuggled past it either. The rewrite that
/// does happen is the one for an OMITTED `model:`: `clamped` resolves the empty
/// string to the kind's default for the resolved CLI. So the block below
/// declares no model, the control asserts `clamped` filled it in, and only then
/// do the pins claim the label and the CLI survived that same pass.
#[test]
fn clamping_a_roster_never_moves_a_remote_block_off_claude() {
    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n\
         \x20 - id: builder\n    kind: worker\n    cli: claude\n    remote: buildbox\n",
    )
    .expect("the fixture must parse");
    let before = wf.block("builder").unwrap().clone();
    assert_eq!(before.remote.as_deref(), Some("buildbox"));
    assert!(before.model.is_empty(), "the fixture declares no model");

    let rails = Guardrails { blocks: wf.blocks.clone(), ..rails() }.clamped();
    let after = rails.block("builder").expect("clamped must keep the block");

    // The collision control FIRST: this pass really did write this block's
    // fields, so the pins below are about a block `clamped` touched rather than
    // one it skipped.
    assert!(
        !after.model.is_empty(),
        "the fixture must be one clamped actually rewrites, or the pins below hold vacuously — \
         it should have resolved the empty model to the kind default, got {:?}",
        after.model
    );

    // …and the invariant survived it.
    assert_eq!(after.remote.as_deref(), Some("buildbox"), "clamping must not drop a remote label");
    assert_eq!(after.cli, "claude", "clamping must not move a remote block off claude");
    assert_eq!(after.kind, Role::Worker, "…nor change the kind the refusals were checked against");
}

/// #1457 review, premortem 2. `read_blocks` re-derives all three `remote:`
/// rules defensively and DROPS a label that fails any of them, silently —
/// there is no human at that layer to show a parse error to. That is the right
/// posture and it has one consequence worth a test of its own: a remote block
/// that loses its label is indistinguishable, everywhere downstream, from a
/// block that never had one.
///
/// **Tested through `load_group_file`, which is the one public caller of
/// `read_blocks`** — and getting there took two wrong turns worth recording,
/// because both were green-looking:
///
/// - `create_group` is `Launch::Fresh`, which RE-READS the repo's
///   `workflow.yml`. A reload through it re-parses the label out of the file,
///   so it would pass against a `blocks_json`/`read_blocks` pair that had
///   dropped the field entirely.
/// - `create_group_ex(.., Launch::Resume)` does not read `group.json` either:
///   per its own doc, "its caller loads `group.json` itself and hands the
///   persisted guardrails straight back in", so passing `rails()` hands it a
///   default roster and the block is not there at all.
///
/// `load_group_file` is that caller's loader, so it is where the persisted
/// roster actually comes from.
#[test]
fn a_remote_label_survives_a_group_json_round_trip_and_drops_when_it_should() {
    let (reg, _dir) = test_registry();
    let repo = Repo::new().workflow(
        "version: 1\nblocks:\n\
         \x20 - id: builder\n    kind: worker\n    cli: claude\n    remote: buildbox\n",
    );
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    assert_eq!(
        g.guardrails.block("builder").unwrap().remote.as_deref(),
        Some("buildbox"),
        "the parsed roster carries the label"
    );

    // It is on disk as a plain JSON string — no path, no interpolation.
    let path = reg.state_root().join(g.id.as_str()).join("group.json");
    let gj: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let on_disk = gj["guardrails"]["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == "builder")
        .unwrap()
        .clone();
    assert_eq!(on_disk["remote"], "buildbox");

    // …and it comes back out. This is the half that would otherwise go quietly
    // wrong: `blocks_json` rewrites the whole roster on every change, so a field
    // it forgot would vanish on the next resume with nothing to see.
    let (_, rails_back) = reg.load_group_file(&g.id).expect("the group file must load");
    assert_eq!(
        rails_back.block("builder").unwrap().remote.as_deref(),
        Some("buildbox"),
        "the label must survive the persistence round trip"
    );

    // The fail-closed half, on the one input `parse_workflow` never sees.
    // `read_blocks` compares the block's cli UNTRIMMED, so `"claude "` is not
    // claude: the label is dropped and the block comes back local. Anything
    // else would let a hand-edited group.json hold a remote label the parser
    // would have refused.
    let mut edited: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    for b in edited["guardrails"]["blocks"].as_array_mut().unwrap() {
        if b["id"] == "builder" {
            b["cli"] = Value::String("claude ".into());
        }
    }
    fs::write(&path, serde_json::to_string_pretty(&edited).unwrap()).unwrap();

    let (_, edited_back) = reg.load_group_file(&g.id).expect("the edited group file must still load");
    assert_eq!(
        edited_back.block("builder").unwrap().remote,
        None,
        "a group.json the parser would have refused must lose the label, not keep it"
    );
    // The control that keeps the line above from passing for the wrong reason:
    // the block is still THERE, still a worker, and only the label went. A
    // dropped block would satisfy that assertion just as well.
    assert_eq!(edited_back.block("builder").unwrap().kind, Role::Worker);

    // #1457 review, premortem 2: the MIGRATION case, which is every group on
    // every existing install and the one case that had no witness. A group.json
    // written before this field existed has no `remote` key at all — not a
    // null, simply absent — and `read_blocks` must read that as a local block
    // rather than tripping over it. Asserted with a fixture rather than left to
    // `as_str()` returning `None` on a missing key, which is true today and is
    // exactly the kind of true-by-reading that this PR keeps finding is not
    // true-by-test.
    // Built from the ORIGINAL `gj` — read at the top, before the fail-closed
    // stanza wrote `"cli": "claude "` into the file. Re-reading `path` here
    // instead would leave that edit in place, and `remote == None` would then
    // hold for three independent reasons — the missing key (the property under
    // test), `check_segment` on a hypothetical `Some("")`, and the cli no
    // longer being claude — with the assertion unable to tell them apart
    // (#1457 review N12). Same class as the collision control that fired on its
    // own fixture: an assertion that holds for a reason other than its own.
    let mut pre_1457: Value = gj.clone();
    for b in pre_1457["guardrails"]["blocks"].as_array_mut().unwrap() {
        b.as_object_mut().unwrap().remove("remote");
    }
    assert_eq!(
        pre_1457["guardrails"]["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == "builder")
            .unwrap()["cli"],
        "claude",
        "the migration fixture must carry a CLEAN cli, or the assertion below is confounded"
    );
    assert!(
        !serde_json::to_string(&pre_1457).unwrap().contains("remote"),
        "the fixture must really carry no remote key, or it is not a pre-#1457 file"
    );
    fs::write(&path, serde_json::to_string_pretty(&pre_1457).unwrap()).unwrap();

    let (_, migrated) = reg.load_group_file(&g.id).expect("a pre-#1457 group.json must still load");
    assert_eq!(
        migrated.block("builder").unwrap().remote,
        None,
        "a group.json predating the key is a LOCAL block"
    );
    assert_eq!(
        migrated.block("builder").unwrap().kind,
        Role::Worker,
        "…and the block survives the read, rather than being dropped for lacking a key"
    );
}

#[test]
fn block_map_round_trips_through_group_json() {
    let (reg, dir) = test_registry();
    let repo = Repo::new()
        .workflow(FOCUSED_REVIEW)
        .agent_file("worker.md", "---\ndescription: repo worker\n---\nBranch first, always.");
    let g = reg.create_group(&repo.path(), rails()).unwrap();

    // The declared roster replaced the built-in one — plus the orchestrator
    // block loomux always guarantees (the file didn't declare one).
    let ids: Vec<&str> = g.guardrails.blocks.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(ids, vec!["orchestrator", "planner", "worker", "rev-security", "rev-tests"]);

    // It is on disk in group.json...
    let gj: Value = serde_json::from_str(
        &fs::read_to_string(reg.state_root().join(g.id.as_str()).join("group.json")).unwrap(),
    )
    .unwrap();
    let blocks = gj["guardrails"]["blocks"].as_array().unwrap();
    assert_eq!(blocks.len(), 5);
    let sec = blocks.iter().find(|b| b["id"] == "rev-security").unwrap();
    assert_eq!(sec["kind"], "reviewer");
    assert_eq!(sec["model"], "opus");
    assert!(sec["prompt"].as_str().unwrap().contains("path traversal"));

    // ...and a fresh registry (an app restart) reads it back identically. Note
    // this reload does NOT re-read the repo — it is the persisted roster that
    // must round-trip.
    let reg2 = relaunch_registry(dir.path());
    reg2.set_port(45999);
    let g2 = reg2.create_group(&repo.path(), rails()).unwrap();
    assert_eq!(g2.id, g.id, "the restart resumes the same group");
    assert_eq!(g2.guardrails.blocks, g.guardrails.blocks, "the roster must round-trip unchanged");

    // A rejoined agent comes back as its BLOCK, not merely its class: three
    // reviewers are three different agents.
    let rev = reg2.spawn_agent_ex(
        &g2.id, Role::Reviewer, Some("rev-security".into()), "", "t", false, None, None, None, None, None,
    )
    .unwrap();
    assert_eq!(rev.block, "rev-security");
    assert_eq!(rev.role, Role::Reviewer);
    let roster = reg2.list_agents(&g2.id);
    let row = roster.as_array().unwrap().iter().find(|a| a["id"] == rev.id.as_str()).unwrap();
    assert_eq!(row["block"], "rev-security", "the roster must expose block identity");
}

#[test]
fn role_hint_round_trips_through_group_json_too() {
    // The persisted roster (`blocks_json` / `read_blocks`) is a SEPARATE wire
    // format from workflow.yml, and role_hint must survive it too — or a
    // process-pro spawned after an app restart would silently lose its hint.
    let (reg, dir) = test_registry();
    let repo = Repo::new().workflow(
        "version: 1\nblocks:\n  - id: advisor\n    kind: planner\n    role_hint: advisor\n",
    );
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    assert_eq!(g.guardrails.block("advisor").unwrap().role_hint.as_deref(), Some("advisor"));

    let gj: Value = serde_json::from_str(
        &fs::read_to_string(reg.state_root().join(g.id.as_str()).join("group.json")).unwrap(),
    )
    .unwrap();
    let blocks = gj["guardrails"]["blocks"].as_array().unwrap();
    let advisor = blocks.iter().find(|b| b["id"] == "advisor").unwrap();
    assert_eq!(advisor["role_hint"], "advisor", "role_hint must be persisted, not dropped");

    let reg2 = relaunch_registry(dir.path());
    reg2.set_port(45999);
    let g2 = reg2.create_group(&repo.path(), rails()).unwrap();
    assert_eq!(
        g2.guardrails.block("advisor").unwrap().role_hint.as_deref(),
        Some("advisor"),
        "a restart must not silently drop the hint"
    );

    // Defense in depth: a HAND-EDITED group.json (never met the parser) whose
    // role_hint no longer matches its own kind must not smuggle the mismatch
    // back in on load — the same silent-drop `read_blocks` already applies to
    // an unrecognized `kind`, since there is no human to show a parse error
    // to at this layer.
    let repo3 = Repo::new();
    let g3 = reg.create_group(&repo3.path(), rails()).unwrap();
    fs::write(
        reg.state_root().join(g3.id.as_str()).join("group.json"),
        serde_json::to_string_pretty(&json!({
            "group_id": g3.id,
            "repo": repo3.path(),
            "created_ms": 1_700_000_000_000u64,
            "guardrails": {
                "max_agents": 6,
                "agent_cli": "claude",
                "blocks": [{
                    "id": "sneaky", "name": "sneaky", "kind": "worker",
                    "cli": "", "model": "", "prompt": null, "profile": null,
                    "allow": [], "role_hint": "advisor",
                }],
            },
        }))
        .unwrap(),
    )
    .unwrap();
    let (_, persisted) = reg.load_group_file(&g3.id).expect("must still load");
    assert_eq!(
        persisted.block("sneaky").unwrap().role_hint, None,
        "a persisted role_hint whose kind doesn't match must be dropped, not resurrected"
    );
}

#[test]
fn a_pre_block_group_json_still_loads() {
    // Back-compat: a group.json written by 0.8.0 has the eight flat per-role
    // fields and no `blocks` array. It must rejoin with exactly the CLIs and
    // models it was launched with — silently reverting a copilot reviewer to
    // claude would be a live behavior change on upgrade.
    let (reg, _d) = test_registry();
    let repo = Repo::new();
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let path = reg.state_root().join(g.id.as_str()).join("group.json");

    fs::write(
        &path,
        serde_json::to_string_pretty(&json!({
            "group_id": g.id,
            "repo": repo.path(),
            "created_ms": 1_700_000_000_000u64,
            "guardrails": {
                "max_agents": 5,
                "agent_cli": "claude",
                "orchestrator_cli": "", "worker_cli": "", "reviewer_cli": "copilot", "planner_cli": "",
                "worker_model": "sonnet", "reviewer_model": "auto",
                "orchestrator_model": "opus", "planner_model": "opus",
                "auto_ops": true,
            },
        }))
        .unwrap(),
    )
    .unwrap();

    // `load_group_file` is the migration seam — it is what the orchestrator
    // session-rejoin path reads to rebuild a group's identity from disk with no
    // launcher form in sight. THAT is where a lost per-role CLI would show up as
    // a copilot reviewer silently coming back as claude.
    let (repo_path, persisted) =
        reg.load_group_file(&g.id).expect("a 0.8.0 group.json must still load");
    assert_eq!(repo_path, repo.path());
    let persisted = persisted.clamped();
    assert_eq!(persisted.blocks.len(), 4, "the legacy flat fields become the 4-block roster");
    assert_eq!(persisted.cli_for(Role::Reviewer), "copilot", "the legacy per-role CLI survives");
    assert_eq!(persisted.cli_for(Role::Worker), "claude", "an empty per-role CLI still inherits");
    assert_eq!(persisted.model_for(Role::Worker), "sonnet");
    assert_eq!(persisted.model_for(Role::Reviewer), "auto");
    assert_eq!(persisted.max_agents, 5);
    for kind in [Role::Orchestrator, Role::Worker, Role::Reviewer, Role::Planner] {
        assert!(persisted.block_for(kind).is_some(), "{kind:?} must have a block after migration");
    }

    // And the persisted cap still wins over the launcher default on a relaunch.
    let reg2 = relaunch_registry(&reg.state_root());
    reg2.set_port(45999);
    let g2 = reg2.create_group(&repo.path(), Guardrails { max_agents: 2, ..rails() }).unwrap();
    assert_eq!(g2.id, g.id);
    assert_eq!(g2.guardrails.max_agents, 5, "the persisted cap still wins on resume");
}

#[test]
fn the_four_class_names_are_reserved_ids_for_their_own_class() {
    // A block's instruction file is `<id>.md`, and the built-in roster's ids ARE
    // the class names — which is what keeps `worker.md` byte-identical. That
    // coupling has to be enforced, or `- id: planner, kind: reviewer` writes its
    // contract to the file the REAL reviewer reads, and whichever agent spawned
    // last wins. (`- id: orchestrator, kind: worker` breaks a second way: the
    // roster then has no orchestrator *kind*, so one is synthesized with the id
    // `orchestrator` — a duplicate that makes the repo's own block unreachable.)
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: planner\n    kind: reviewer\n    prompt: Review.\n",
    )
    .unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("reserved for planner blocks")),
        "a built-in id must be reserved for its own class: {errs:?}"
    );
    assert!(workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: orchestrator\n    kind: worker\n"
    )
    .is_err());

    // Using a class name for its OWN class is fine — that is the built-in roster.
    assert!(workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: reviewer\n    kind: reviewer\n    prompt: Be strict.\n"
    )
    .is_ok());

    // Defence in depth: a hand-edited group.json never meets the parser, so
    // `clamped()` drops the same shape (and any duplicate id) silently.
    let g = Guardrails {
        agent_cli: "claude".into(),
        blocks: vec![
            workflow::Block {
                id: "planner".into(),
                name: "sneaky".into(),
                kind: Role::Reviewer, // id says planner, kind says reviewer
                cli: String::new(),
                model: String::new(),
                prompt: Some("Review.".into()),
                profile: None,
                allow: vec![],
                role_hint: None,
                effort: String::new(),
                context: String::new(),
                remote: None,
                driver: None,
                cache_ttl_minutes: None,
            },
            workflow::Block {
                id: "worker".into(),
                name: "worker".into(),
                kind: Role::Worker,
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
            },
            workflow::Block {
                id: "worker".into(), // duplicate
                name: "worker two".into(),
                kind: Role::Worker,
                cli: String::new(),
                model: String::new(),
                prompt: Some("I am the impostor.".into()),
                profile: None,
                allow: vec![],
                role_hint: None,
                effort: String::new(),
                context: String::new(),
                remote: None,
                driver: None,
                cache_ttl_minutes: None,
            },
        ],
        ..Guardrails::default()
    }
    .clamped();

    let ids: Vec<&str> = g.blocks.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(ids, vec!["orchestrator", "worker"], "mismatched and duplicate ids are dropped");
    assert!(
        !g.block("worker").unwrap().has_persona(),
        "the FIRST worker wins the id; the duplicate cannot smuggle in a persona"
    );
    // Every id maps to exactly one file, and every file to one block.
    let files: Vec<String> = g.blocks.iter().map(|b| b.instructions_file()).collect();
    let unique: std::collections::HashSet<&String> = files.iter().collect();
    assert_eq!(files.len(), unique.len(), "no two blocks may share an instructions file: {files:?}");
}

#[test]
fn a_review_only_workflow_says_so_instead_of_silently_opening_no_workers() {
    // The launcher's "initial workers" count assumes a worker block exists. A
    // review-only workflow has none — and every initial spawn would then fail
    // with "declares no worker block", leaving the human with zero panes and
    // nothing but an audit line to explain it.
    let (reg, _d) = test_registry();
    let repo = Repo::new().workflow(
        "version: 1\nblocks:\n  - id: rev-sec\n    kind: reviewer\n    prompt: Security only.\n\
         \x20 - id: rev-perf\n    kind: reviewer\n    prompt: Perf only.\n",
    );
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    assert!(g.guardrails.block_for(Role::Worker).is_none(), "the roster really has no worker");

    // Asking for a worker names the gap plainly rather than guessing a class.
    let err = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap_err();
    assert!(err.contains("declares no worker block"), "{err}");

    // ...and the reviewers it DOES declare spawn fine.
    for block in ["rev-sec", "rev-perf"] {
        reg.spawn_agent_ex(
            &g.id, Role::Reviewer, Some(block.into()), "", "t", false, None, None, None, None, None,
        )
        .unwrap();
    }
}

#[test]
fn a_session_recorded_against_a_since_renamed_block_still_rejoins() {
    // A reviewer ran as `rev-security`; the workflow file was later edited to
    // rename that block. Resuming the old session must not be an error — losing
    // the persona is a downgrade, but losing the SESSION is data loss, and the
    // human has no other way to reach it. It degrades to the class default.
    //
    // (`spawn_agent_ex` stays strict about an unknown block id on purpose: for an
    // orchestrator's `spawn_agent(block:)`, a typo should be an error. The
    // rejoin path is where "stale" and "wrong" are distinguishable.)
    let (reg, _d) = test_registry();
    let repo = Repo::new().workflow(
        "version: 1\nblocks:\n  - id: rev-security\n    kind: reviewer\n    prompt: Security only.\n",
    );
    let g = reg.create_group(&repo.path(), rails()).unwrap();

    // A stale id errors on the strict path...
    let err = reg
        .spawn_agent_ex(
            &g.id, Role::Reviewer, Some("rev-gone".into()), "", "t", false, None, None, None, None, None,
        )
        .unwrap_err();
    assert!(err.contains("unknown block"), "{err}");

    // ...but the class default is always reachable, which is what the rejoin
    // falls back to.
    let r = reg
        .spawn_agent_ex(&g.id, Role::Reviewer, None, "", "t", false, None, None, None, None, None)
        .unwrap();
    assert_eq!(r.block, "rev-security", "the class default is the only reviewer block");
    assert_eq!(r.role, Role::Reviewer);
}

#[test]
fn copilot_native_agent_is_refused_when_the_handle_names_a_different_file() {
    // `--agent` takes a NAME, and a persona's name comes from its frontmatter,
    // not its path. So `.github/agents/security-review.md` can declare
    // `name: worker` — and loomux would kind-check the security-review file while
    // Copilot went and loaded the *worker* persona, with the audit line insisting
    // all was well. Only take the native path when the handle unambiguously names
    // the file loomux actually read.
    let (reg, _d) = test_registry();
    let repo = Repo::new()
        .workflow(
            "version: 1\nblocks:\n  - id: rev-security\n    kind: reviewer\n    cli: copilot\n\
             \x20   profile: .github/agents/security-review.md\n",
        )
        // The name says `worker`, but the file is the security review.
        .agent_file(
            "security-review.md",
            "---\nname: worker\ndescription: Security review.\n---\nReview for injection and authz holes.",
        )
        .agent_file(
            "worker.md",
            "---\nname: worker\ndescription: The worker.\n---\nBranch, commit, open a PR.",
        );
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let (cmd, argv, kickoff) = compile(&reg, &g, "rev-security");

    // #416: the ambiguous handle still must NOT reach copilot's NATIVE flag
    // (that would load the wrong file — the reasoning above is unchanged),
    // but the persona loomux actually read now reaches the CLI via a
    // loomux-GENERATED wrapper file (a unique handle loomux invents itself,
    // so there is no ambiguity to exploit) instead of falling all the way
    // back to a kickoff-only paste.
    let handle = argv[argv.iter().position(|a| a == "--agent").unwrap() + 1].clone();
    assert_ne!(handle, "worker", "must never resolve to the wrong file's name: {cmd}");
    assert!(kickoff.is_none(), "the generated wrapper carries it now; no kickoff fallback needed");
    let generated_path = _d.path().join("copilot-agents").join(format!("{handle}.agent.md"));
    let generated_text = fs::read_to_string(&generated_path).expect("generated wrapper must exist");
    assert!(
        generated_text.contains("injection and authz"),
        "the persona loomux actually read is delivered, not the file `worker` would ambiguously resolve to: {generated_text}"
    );
    let audit = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl")).unwrap();
    assert!(audit.lines().any(|l| l.contains("copilot-agent-handle-ambiguous")), "and it is audited");

    // The unambiguous case still takes the native path.
    let repo = Repo::new()
        .workflow(
            "version: 1\nblocks:\n  - id: rev-security\n    kind: reviewer\n    cli: copilot\n\
             \x20   profile: .github/agents/security-review.md\n",
        )
        .agent_file(
            "security-review.md",
            "---\nname: security-review\ndescription: Security review.\n---\nReview for injection.",
        );
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let (cmd, _argv, kickoff) = compile(&reg, &g, "rev-security");
    assert!(cmd.contains("--agent security-review"), "{cmd}");
    assert!(kickoff.is_none(), "the native flag carries it — nothing to inject");
}
