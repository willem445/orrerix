//! A persona's `tools:` filter and loomux's MCP tools (#802), and `contract_carrier`.
//!
//! One module of the `workflow` integration-test target (`main.rs`),
//! split out of the former single-file `tests/workflow.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ───── #802: a persona's `tools:` filter must not strip loomux's MCP tools ─────
//
// The live failure, three rounds deep: every custom-workflow copilot delegate
// came up with the loomux MCP server listed and none of its tools usable, while
// the built-in roster on the same machine was fine. Copilot's custom-agent
// `tools:` frontmatter is a FILTER over built-in AND MCP tools (custom-agents
// configuration reference, *Tools processing*), and a `profile:` pointing at a
// `.github/agents/*.md` that carries one is the ONLY thing a workflow block adds
// to the `--agent` target. loomux's own generated file has never had the key, so
// it inherits the documented all-tools default — hence the split.

/// A copilot block whose `profile:` names `.github/agents/<file>`.
fn copilot_profile_workflow(block: &str, file: &str) -> String {
    format!(
        "version: 1\nblocks:\n  - id: {block}\n    kind: worker\n    cli: copilot\n\
         \x20   profile: .github/agents/{file}\n"
    )
}

#[test]
fn copilot_persona_tools_list_without_loomux_is_repaired_by_a_generated_copy() {
    let (reg, d) = test_registry();
    let repo = Repo::new()
        .workflow(&copilot_profile_workflow("w", "scoped.md"))
        .agent_file(
            "scoped.md",
            "---\nname: scoped\ndescription: A scoped worker.\ntools: [\"read\", \"edit\", \"execute\"]\n---\nBranch, then open a PR.",
        );
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let (cmd, argv, _kickoff) = compile(&reg, &g, "w");

    // The whole defect in one assertion: `--agent scoped` hands copilot a file
    // whose `tools:` list filters every loomux tool out of the delegate. It must
    // name loomux's OWN copy instead.
    let handle = argv[argv.iter().position(|a| a == "--agent").unwrap() + 1].clone();
    assert_ne!(
        handle, "scoped",
        "a persona whose tools: list drops loomux must not be launched as-is — that delegate \
         cannot report (#802): {cmd}"
    );

    let generated = fs::read_to_string(d.path().join("copilot-agents").join(format!("{handle}.agent.md")))
        .expect("the generated copy must exist");
    // Valid YAML frontmatter, not just a substring that looks right — the same
    // bar `generated_agent_files_satisfy_each_clis_documented_required_
    // frontmatter_fields` set after `CustomAgentLoadFailedError`.
    let front: serde_norway::Value = {
        let mut parts = generated.splitn(3, "---\n");
        assert_eq!(parts.next(), Some(""), "must open with a bare --- line: {generated}");
        serde_norway::from_str(parts.next().expect("a closing --- must follow"))
            .unwrap_or_else(|e| panic!("frontmatter did not parse as YAML: {e}\n{generated}"))
    };
    let tools: Vec<String> = front["tools"]
        .as_sequence()
        .unwrap_or_else(|| panic!("the copy must carry a tools: sequence: {generated}"))
        .iter()
        .map(|t| t.as_str().unwrap().to_string())
        .collect();
    assert!(
        tools.contains(&"orrerix/*".to_string()),
        "the documented server-wildcard grant must be present: {tools:?}"
    );
    // The user's own scoping intent survives verbatim — loomux widens by exactly
    // one server, it does not hand the delegate every tool in the CLI.
    for kept in ["read", "edit", "execute"] {
        assert!(tools.contains(&kept.to_string()), "the persona's own entries must survive: {tools:?}");
    }
    assert!(
        generated.contains("Branch, then open a PR."),
        "and the persona text still reaches the agent: {generated}"
    );

    let audit = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl")).unwrap();
    assert!(
        audit.lines().any(|l| l.contains("copilot-persona-tools-gap")),
        "the class that cost #802 three rounds must never be silent again: {audit}"
    );
}

#[test]
fn loomux_repairs_an_omission_but_never_a_deliberate_narrowing() {
    // rev-lead N2. "Preserved, not widened" has to be true of the code, not just
    // of the sentence — so the two lists that state a deliberate NARROWING are
    // reported and left exactly as written, never widened into a grant the user
    // did not give. Paired against the omission case in the same test, so it
    // cannot pass by the repair being broken in general.
    let case = |tools: &str| -> (String, String) {
        let (reg, _d) = test_registry();
        let repo = Repo::new().workflow(&copilot_profile_workflow("w", "p.md")).agent_file(
            "p.md",
            &format!("---\nname: p\ndescription: A worker.\ntools: {tools}\n---\nDo the work."),
        );
        let g = reg.create_group(&repo.path(), rails()).unwrap();
        let (cmd, _argv, _k) = compile(&reg, &g, "w");
        let audit = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl")).unwrap();
        let line = audit
            .lines()
            .find(|l| l.contains("copilot-persona-tools-gap"))
            .unwrap_or_else(|| panic!("every gap is reported, repaired or not: {audit}"))
            .to_string();
        (cmd, line)
    };

    // An OMISSION — nobody writes this meaning "and the MCP server must not work".
    // Repaired: `--agent` moves off the user's own handle.
    let (cmd, line) = case("[\"read\", \"edit\"]");
    assert!(!cmd.contains("--agent p "), "an omission is repaired: {cmd}");
    assert!(line.contains("re-pointed"), "{line}");

    // A DECISION: `tools: []` is documented as "disables all tools".
    let (cmd, line) = case("[]");
    assert!(
        cmd.contains("--agent p"),
        "an explicit empty list is left exactly as written — this app does not overrule \"no tools\" \
         into \"none except its own\": {cmd}"
    );
    assert!(line.contains("deliberate no-tools decision"), "and says why: {line}");

    // A DECISION: the server is scoped per-tool on purpose.
    let (cmd, line) = case("[\"read\", \"orrerix/report\"]");
    assert!(
        cmd.contains("--agent p"),
        "a per-tool scope is left as written — widening it to orrerix/* would be this app granting \
         itself more than it was given: {cmd}"
    );
    assert!(line.contains("per-tool"), "and says why: {line}");

    // THE SAME DECISION, WRITTEN BEFORE THE FLAG DAY (rev-967 B1). This is the
    // row that is red without `scopes_mcp_server_per_tool`'s legacy arm, and it
    // is the whole reason that arm exists.
    //
    // #1153 phase 3 renamed the MCP server, and a persona file in somebody's
    // repo still says `loomux/report`. The author's decision did not change
    // when our server's name did: they asked for exactly one tool. Reading the
    // stale spelling as "never mentions the server" made this an OMISSION, and
    // the repair path then appended the full-server grant — this app widening a
    // narrowing nobody widened, which is what #222's capability closure forbids
    // and what `tools_gap_refusal`'s own doc promises never happens.
    //
    // Pinned NEXT TO the current-spelling case rather than replacing it. An
    // earlier revision of this PR moved this test's specimen to
    // `orrerix/report`, which left CI green across a live behaviour change
    // because the only witness had stopped being a member of the class at risk.
    let (cmd, line) = case("[\"read\", \"loomux/report\"]");
    assert!(
        cmd.contains("--agent p"),
        "a per-tool scope written before the rename is still a DECISION — repairing it would \
         hand the delegate every tool on the server where its author named one: {cmd}"
    );
    assert!(line.contains("per-tool"), "and it is reported as one: {line}");
    assert!(
        !line.contains("re-pointed"),
        "the repair path must not have run at all — running it IS the widening: {line}"
    );
    assert!(
        line.contains("\"names_server_as\":\"loomux\""),
        "and the record says which spelling the file used, so a human can act on it: {line}"
    );

    // The NEGATIVE CONTROL for that arm, and the asymmetry it protects. A stale
    // WHOLE-server grant is deliberately NOT a per-tool scope: `loomux/*` asks
    // for the whole server, so the repair gives exactly that under the name the
    // server actually has. Keeping it native instead would hand the delegate a
    // filter matching nothing — no orchestration tools at all — which is the
    // regression a well-meaning "accept both spellings everywhere" produces.
    // Argued in `grants_loomux_tools`'s doc; asserted here so the argument
    // cannot be undone by someone tidying the two predicates into one.
    for whole_server in ["[\"read\", \"loomux/*\"]", "[\"read\", \"loomux\"]"] {
        let (cmd, line) = case(whole_server);
        assert!(
            !cmd.contains("--agent p "),
            "{whole_server}: a stale whole-server grant is a GAP on purpose — the repair spells \
             the author's own intent the way the server is spelled now: {cmd}"
        );
        assert!(line.contains("re-pointed"), "{whole_server}: {line}");
    }
}

/// rev-967 B1, the human-facing half: a persona whose scope stopped matching
/// because the SERVER was renamed must be told that, in those words.
///
/// From inside the file nothing looks wrong — the `tools:` line is right there,
/// naming a server, scoping it to a tool. The only thing that changed is a name
/// the author never chose and cannot see from their own repo, so a warning that
/// merely says "does not grant the MCP server" sends them looking for a typo
/// they did not make.
#[test]
fn a_persona_scoping_the_pre_rename_server_is_told_the_name_is_what_went_stale() {
    let (reg, _d) = test_registry();
    let repo = Repo::new().workflow(&copilot_profile_workflow("w", "p.md")).agent_file(
        "p.md",
        "---\nname: p\ndescription: A worker.\ntools: [\"read\", \"loomux/report\"]\n---\nDo the work.",
    );
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let b = g.guardrails.block("w").unwrap();
    let persona = reg.resolve_persona(&g, b).unwrap_or(None).expect("the persona resolves");

    assert!(
        persona.scopes_mcp_server_per_tool(),
        "the pre-rename spelling must read as a per-tool scope, or the repair path widens it"
    );
    assert_eq!(
        persona.mcp_server_named_in_tools(),
        Some("loomux"),
        "and the file's own spelling is what gets reported back"
    );

    let warning = copilot_tools_gap_warning("w", &persona, ToolsGapAction::KeptNativeForPerToolScope);
    assert!(
        warning.contains("PRE-RENAME server `loomux`"),
        "the warning must name the stale spelling: {warning}"
    );
    assert!(
        warning.contains("orrerix"),
        "…and the current one, or the reader still cannot act on it: {warning}"
    );

    // The negative control: a persona scoping the CURRENT server per-tool is a
    // plain partial grant and must NOT be told its name went stale.
    let (reg2, _d2) = test_registry();
    let repo2 = Repo::new().workflow(&copilot_profile_workflow("w", "p.md")).agent_file(
        "p.md",
        "---\nname: p\ndescription: A worker.\ntools: [\"read\", \"orrerix/report\"]\n---\nDo the work.",
    );
    let g2 = reg2.create_group(&repo2.path(), rails()).unwrap();
    let b2 = g2.guardrails.block("w").unwrap();
    let current = reg2.resolve_persona(&g2, b2).unwrap_or(None).expect("the persona resolves");
    let warning2 =
        copilot_tools_gap_warning("w", &current, ToolsGapAction::KeptNativeForPerToolScope);
    assert!(
        !warning2.contains("PRE-RENAME"),
        "a current-spelling scope is not a stale name, and saying so would send a human editing \
         a file that is already right: {warning2}"
    );
    assert!(
        warning2.contains("grants some orrerix tools but not all"),
        "it gets the partial-grant wording instead: {warning2}"
    );

    // rev-967 N7. The two cases must not promise the same thing. The tail is
    // per-ACTION, so it lands right after the sentence above — and for a stale
    // spelling that sentence has just said the scope matches nothing, which
    // makes "CAN CALL ONLY the tools that list names" read as though some of
    // them still work. The set is empty.
    assert!(
        warning.contains("CAN CALL NONE OF THEM"),
        "a stale scope names tools on a server this app no longer declares, so the delegate \
         can call nothing — the warning must not imply a working subset: {warning}"
    );
    assert!(
        !warning.contains("CAN CALL ONLY"),
        "…and must not carry the current-spelling promise as well: {warning}"
    );
    assert!(
        warning2.contains("CAN CALL ONLY the orrerix tools that list names"),
        "while a CURRENT-spelling per-tool scope really does leave a working subset, and saying \
         it can call none would send a human editing a file that is already right: {warning2}"
    );
}

#[test]
fn copilot_persona_that_grants_loomux_or_declares_no_tools_keeps_the_native_path() {
    // The repair must not over-trigger: an unfiltered persona (the common case,
    // and every file in this repo's own `.github/agents/`) is untouched, and so
    // is one that already grants the server — by `*`, by `orrerix/*`, or by the
    // bare argv spelling the CLI's own `--allow-tool` uses.
    //
    // rev-lead N3: this was a guard that had never been observed red, i.e. a
    // coverage *claim*. It is differential now — each granting list is paired
    // below with the SAME list minus the grant, which must come out the other
    // way. A regression to "always native" fails the twin; a regression to
    // "always repair" fails these; and a `grants_mcp_server` stuck at either
    // constant fails one side or the other. No mutation run needed, and the
    // coverage lives in CI forever rather than in a cited log line.
    for tools_line in ["", "tools: [\"*\"]\n", "tools: [\"read\", \"orrerix/*\"]\n", "tools: [\"read\", \"orrerix\"]\n"] {
        let (reg, d) = test_registry();
        let repo = Repo::new().workflow(&copilot_profile_workflow("w", "open.md")).agent_file(
            "open.md",
            &format!("---\nname: open\ndescription: An open worker.\n{tools_line}---\nDo the work."),
        );
        let g = reg.create_group(&repo.path(), rails()).unwrap();
        let (cmd, _argv, kickoff) = compile(&reg, &g, "w");
        assert!(
            cmd.contains("--agent open"),
            "tools_line {tools_line:?} grants loomux (or filters nothing), so #222's native path \
             is unchanged: {cmd}"
        );
        assert!(kickoff.is_none(), "the native flag carries it: {tools_line:?}");
        // The generated-copy directory is keyed by a handle loomux mints, so
        // assert on the directory rather than guessing the name: nothing at all
        // should have been written for a persona that needed no repair.
        let copies = d.path().join("copilot-agents");
        let wrote_any = fs::read_dir(&copies).map(|mut e| e.next().is_some()).unwrap_or(false);
        assert!(!wrote_any, "loomux wrote a copy it did not need: {tools_line:?}");
        let audit = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl")).unwrap();
        assert!(
            !audit.lines().any(|l| l.contains("copilot-persona-tools-gap")),
            "a false-positive warning would train the human to ignore the real one: {tools_line:?}\n{audit}"
        );

        // THE TWIN (rev-lead N3). The same fixture with the loomux grant struck
        // out must come out the OTHER way. Without this, every assertion above
        // would still pass if the repair had been disabled outright, and the
        // test would be certifying nothing.
        let stripped = tools_line.replace("\"orrerix/*\"", "\"search\"").replace("\"orrerix\"", "\"search\"");
        let stripped = if tools_line.is_empty() { "tools: [\"read\"]\n".to_string() } else { stripped.replace("[\"*\"]", "[\"read\"]") };
        let (reg2, d2) = test_registry();
        let repo2 = Repo::new().workflow(&copilot_profile_workflow("w", "open.md")).agent_file(
            "open.md",
            &format!("---\nname: open\ndescription: An open worker.\n{stripped}---\nDo the work."),
        );
        let g2 = reg2.create_group(&repo2.path(), rails()).unwrap();
        let (cmd2, _argv2, _k2) = compile(&reg2, &g2, "w");
        assert!(
            !cmd2.contains("--agent open "),
            "twin of {tools_line:?} ({stripped:?}) grants loomux nothing, so it MUST be repaired — \
             if this passes natively the assertions above prove nothing: {cmd2}"
        );
        let wrote_any2 = fs::read_dir(d2.path().join("copilot-agents"))
            .map(|mut e| e.next().is_some())
            .unwrap_or(false);
        assert!(wrote_any2, "twin of {tools_line:?} must get the stand-in the granting case does not");
    }
}

#[test]
fn a_non_resolving_handle_never_reaches_agent_even_on_the_tools_gap_refusal_path() {
    // rev-lead B1. The refusal arm sets `--agent` to the persona's OWN
    // frontmatter name, and every other site that does so is guarded by
    // `copilot_native` — i.e. by `handle_resolves_to`, whose entire job is
    // stopping loomux from kind-checking one file and launching another. A
    // refusal is not an exemption from that.
    //
    // The fixture is the exact substitution `handle_resolves_to` exists to
    // prevent, wearing a tools gap and its own `mcp-servers:` — the combination
    // that reached the unguarded arm: `security-review.md` declares
    // `name: worker`, so `--agent worker` would load the WORKER persona instead.
    //
    // Two independent things now stop it, and this test states the observable
    // property both produce rather than either mechanism: the gap check is
    // gated on `copilot_native`, so a non-resolving handle never enters the
    // refusal arm at all; and the arm itself re-checks `copilot_native` before
    // naming a persona's own handle. The assertion below is what a user would
    // see, so it holds if either defence is later loosened — which a test
    // written against the arm's internals would not.
    let (reg, d) = test_registry();
    let repo = Repo::new()
        .workflow(&copilot_profile_workflow("w", "security-review.md"))
        .agent_file(
            "security-review.md",
            "---\nname: worker\ndescription: Security review.\ntools: [\"read\"]\n\
             mcp-servers:\n  custom-mcp:\n    command: node\n---\nReview for injection and authz holes.",
        )
        .agent_file("worker.md", "---\nname: worker\ndescription: The worker.\n---\nBranch, commit, open a PR.");
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let (cmd, argv, _kickoff) = compile(&reg, &g, "w");

    let handle = argv[argv.iter().position(|a| a == "--agent").unwrap() + 1].clone();
    assert_ne!(
        handle, "worker",
        "a refusal path must not emit --agent for a handle that resolves to a DIFFERENT file — \
         that is the trust substitution handle_resolves_to exists to prevent: {cmd}"
    );
    // ...and the persona loomux actually read is still what reaches the agent,
    // via the generated copy — never silently dropped by the refusal.
    let generated = fs::read_to_string(d.path().join("copilot-agents").join(format!("{handle}.agent.md")))
        .expect("the non-native persona still gets its generated file");
    assert!(
        generated.contains("injection and authz"),
        "the file loomux read is the one delivered, not the one `worker` would name: {generated}"
    );
    // The narrowing, pinned from the other side: a non-native persona's `tools:`
    // was never in force (Copilot never loads its file), so there is no gap to
    // warn about and none is claimed...
    let audit = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl")).unwrap();
    assert!(
        !audit.lines().any(|l| l.contains("copilot-persona-tools-gap")),
        "a non-native persona has no tools gap to report: {audit}"
    );
    // ...and its list is NOT reproduced into the copy, which would narrow the
    // block for the first time (rev-lead N1).
    assert!(
        !generated.contains("tools:"),
        "the generated copy for a non-native persona must be unchanged by #802 — no tools: key, \
         so Copilot's documented all-tools default still applies: {generated}"
    );
}

#[test]
fn a_repaired_stand_in_carries_every_other_frontmatter_key_verbatim() {
    // rev-lead's copy-faithfulness finding. The stand-in replaces a file Copilot
    // would otherwise have loaded whole, so a key loomux has no opinion about
    // must survive the substitution. `model:` is the one with teeth — the
    // reference documents it as the model the agent executes on, so dropping it
    // would change the persona's model as a side effect of a permissions fix.
    let (reg, d) = test_registry();
    let repo = Repo::new().workflow(&copilot_profile_workflow("w", "rich.md")).agent_file(
        "rich.md",
        "---\nname: rich\ndescription: A richly configured worker.\ntools: [\"read\"]\n\
         model: claude-sonnet-4.5\ninfer: false\ndisable-model-invocation: true\n---\nDo the work.",
    );
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let (_cmd, argv, _kickoff) = compile(&reg, &g, "w");
    let handle = argv[argv.iter().position(|a| a == "--agent").unwrap() + 1].clone();
    let generated =
        fs::read_to_string(d.path().join("copilot-agents").join(format!("{handle}.agent.md"))).unwrap();

    let front: serde_norway::Value = {
        let mut parts = generated.splitn(3, "---\n");
        parts.next();
        serde_norway::from_str(parts.next().unwrap())
            .unwrap_or_else(|e| panic!("stand-in frontmatter must still be valid YAML: {e}\n{generated}"))
    };
    assert_eq!(
        front["model"].as_str(),
        Some("claude-sonnet-4.5"),
        "a persona's model: must not vanish into a permissions fix: {generated}"
    );
    assert_eq!(front["infer"].as_bool(), Some(false), "{generated}");
    assert_eq!(front["disable-model-invocation"].as_bool(), Some(true), "{generated}");
    // The three loomux re-authors are loomux's, not the user's: `name` must be
    // the handle Copilot resolves this file by, and the user's `name: rich` must
    // NOT survive alongside it (two files claiming one handle is the ambiguity
    // `handle_resolves_to` refuses elsewhere).
    assert_eq!(front["name"].as_str(), Some(handle.as_str()), "{generated}");
    assert!(front["description"].as_str().unwrap().contains("orrerix"), "{generated}");
}

#[test]
fn copilot_persona_declaring_its_own_mcp_servers_is_warned_never_rewritten() {
    // The one case loomux must NOT repair. A generated copy models loomux's own
    // server and nothing else, so re-pointing `--agent` at it would silently
    // delete the servers the user declared — trading #802's missing-tools bug
    // for a different one. Warn loudly, launch native, let a human fix the file.
    let (reg, _d) = test_registry();
    let repo = Repo::new().workflow(&copilot_profile_workflow("w", "byo.md")).agent_file(
        "byo.md",
        "---\nname: byo\ndescription: Brings its own MCP.\ntools: [\"read\", \"custom-mcp/tool-1\"]\n\
         mcp-servers:\n  custom-mcp:\n    type: local\n    command: node\n---\nUse the custom server.",
    );
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let (cmd, _argv, _kickoff) = compile(&reg, &g, "w");
    assert!(
        cmd.contains("--agent byo"),
        "dropping a user's own mcp-servers to fix a tools gap is not a trade loomux may make: {cmd}"
    );
    let audit = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl")).unwrap();
    let line = audit
        .lines()
        .find(|l| l.contains("copilot-persona-tools-gap"))
        .unwrap_or_else(|| panic!("the unrepaired gap is the MOST important one to say out loud: {audit}"));
    assert!(
        line.contains("mcp-servers"),
        "the audit must say WHY it was left alone, or the next reader re-opens #802: {line}"
    );
}

#[test]
fn mcp_spawn_reply_says_when_a_persona_tools_list_stripped_the_loomux_server() {
    // The detection half, at the surface that matters: whoever asked for the
    // spawn is the party that can fix the file. #802 survived two doc-grounded
    // fixes because nothing said this out loud at spawn time — an agent that
    // cannot call loomux looks, from inside its own pane, exactly like loomux
    // being broken.
    let (reg, _d) = test_registry();
    let repo = Repo::new()
        .workflow(&copilot_profile_workflow("w", "scoped.md"))
        .agent_file(
            "scoped.md",
            "---\nname: scoped\ndescription: A scoped worker.\ntools: [\"read\"]\n---\nDo the work.",
        )
        .git_init();
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let caller = orch_caller(&reg, &g.id);
    let out = dispatch(
        &reg,
        &caller,
        "tools/call",
        &json!({ "name": "spawn_agent", "arguments": { "block": "w", "task": "t" } }),
    )
    .unwrap();
    assert_eq!(out["isError"], json!(false), "{:?}", out["content"][0]["text"]);
    let text = out["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("orrerix/*"),
        "the reply must carry the exact line to add to the persona file: {text}"
    );
    assert!(text.contains("scoped"), "...and name the persona to add it to: {text}");
}


#[test]
fn a_tools_frontmatter_is_read_in_every_yaml_shape_a_real_agent_file_uses() {
    // Detection is only as good as the parse behind it. All three shapes are
    // valid YAML and all three appear in real `.github/agents/*.md` files; a
    // reader that handled only the flow sequence would silently score a block
    // list as "no tools:" — i.e. as "grants everything" — which is precisely the
    // false negative #802 cannot afford a second time.
    let flow = profiles::parse_profile("a", "---\nname: a\ndescription: d\ntools: [read, edit]\n---\nbody").unwrap();
    let quoted = profiles::parse_profile("a", "---\nname: a\ndescription: d\ntools: [\"read\", \"edit\"]\n---\nbody").unwrap();
    let scalar = profiles::parse_profile("a", "---\nname: a\ndescription: d\ntools: read, edit\n---\nbody").unwrap();
    let block = profiles::parse_profile("a", "---\nname: a\ndescription: d\ntools:\n  - read\n  - edit\n---\nbody").unwrap();
    for (label, p) in [("flow", &flow), ("quoted", &quoted), ("scalar", &scalar), ("block", &block)] {
        assert_eq!(
            p.tools.as_deref(),
            Some(["read".to_string(), "edit".to_string()].as_slice()),
            "{label} shape must read as the same two tools"
        );
        assert!(!p.grants_mcp_server("orrerix"), "{label}: neither entry grants the server");
        assert!(!p.mentions_mcp_server("orrerix"), "{label}: nor mentions it");
    }

    // Absent vs. empty are DIFFERENT, and the difference is a capability:
    // absent is copilot's documented all-tools default, empty is its documented
    // "disables all tools". Collapsing them into a plain Vec would make the
    // empty list read as "grants everything" and hide the worst case of all.
    let absent = profiles::parse_profile("a", "---\nname: a\ndescription: d\n---\nbody").unwrap();
    assert_eq!(absent.tools, None);
    assert!(absent.grants_mcp_server("orrerix"), "no filter means every tool, the server's included");
    let empty = profiles::parse_profile("a", "---\nname: a\ndescription: d\ntools: []\n---\nbody").unwrap();
    assert_eq!(empty.tools.as_deref(), Some::<&[String]>(&[]));
    assert!(!empty.grants_mcp_server("orrerix"), "an explicit empty list disables everything");

    // A per-tool grant is *mentioned* but not a full grant — the warning says
    // something different in that case, and it has to be able to tell.
    let partial = profiles::parse_profile("a", "---\nname: a\ndescription: d\ntools: [\"orrerix/report\"]\n---\nbody").unwrap();
    assert!(!partial.grants_mcp_server("orrerix"));
    assert!(partial.mentions_mcp_server("orrerix"));

    // `mcp-servers:` presence is what blocks the rewrite; a file without it must
    // never be mistaken for one that has it.
    let byo = profiles::parse_profile(
        "a",
        "---\nname: a\ndescription: d\nmcp-servers:\n  custom:\n    command: node\n---\nbody",
    )
    .unwrap();
    assert!(byo.has_mcp_servers);
    assert!(!absent.has_mcp_servers);
}

// ───── #417 correction round 5, promoted to an enum in round 8: contract_carrier ─────

#[test]
fn contract_carrier_is_kickoff_only_for_an_unambiguous_copilot_native_persona() {
    // `compact_reinjection_notice`'s three-way shape choice after a compact
    // depends entirely on this being right: `KickoffOnly` means nothing
    // loomux-authored survives on the system-prompt layer, so a real
    // compaction must fall back to embedding the full contract. This is the
    // ONE documented #416 residual gap — the exact fixture
    // `copilot_native_agent_is_refused_when_the_handle_names_a_different_
    // file` uses for its own unambiguous-native case.
    let (reg, _d) = test_registry();
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
    let b = g.guardrails.block("rev-security").unwrap();
    let cli = workflow::cli_of(b, &g.guardrails.agent_cli);
    let persona = reg.resolve_persona(&g, b).unwrap();
    assert!(persona.as_ref().is_some_and(|p| p.copilot_native), "must resolve natively for this fixture");
    let instructions_body = instructions_lf(&reg, &g.id, &b.instructions_file());
    let contract = block_contract_text(&instructions_body, persona.as_ref());
    let inject = reg.persona_inject(&g.id, b, cli, persona.as_ref(), &contract);
    assert!(inject.copilot_agent.is_some(), "the native --agent flag is still emitted");
    assert_eq!(
        inject.contract_carrier,
        ContractCarrier::KickoffOnly,
        "a native persona's OWN file rides --agent, never loomux's contract"
    );
}

#[test]
fn contract_carrier_is_system_layer_core_for_the_copilot_generated_wrapper_and_full_for_claude() {
    // rev-16 review (N2), round 8: the generated-wrapper path (default
    // roster, no persona at all here) carries a SLIM composition, not the
    // full contract — but it is NOT `KickoffOnly` either. It is its own
    // real, third state: durable, load-bearing (identity + the
    // non-negotiable mechanics core), just incomplete relative to the full
    // role template. Collapsing it into `KickoffOnly` (the bool-era
    // behavior right after round 8's own B1 fix) made every Copilot
    // compaction pay for a full verbose re-embed — exactly the cost a
    // compaction is supposed to reclaim.
    let (reg, _d) = test_registry();
    let repo = Repo::new(); // no .loomux/ at all — default roster
    let g = reg.create_group(&repo.path(), Guardrails { agent_cli: "copilot".into(), ..rails() }).unwrap();
    let b = g.guardrails.block_for(Role::Worker).unwrap();
    let cli = workflow::cli_of(b, &g.guardrails.agent_cli);
    let persona = reg.resolve_persona(&g, b).unwrap();
    assert!(persona.is_none(), "default roster has no persona");
    let instructions_body = instructions_lf(&reg, &g.id, &b.instructions_file());
    let contract = block_contract_text(&instructions_body, persona.as_ref());
    let inject = reg.persona_inject(&g.id, b, cli, persona.as_ref(), &contract);
    assert!(inject.copilot_agent.is_some(), "the generated wrapper's handle is still emitted");
    assert_eq!(
        inject.contract_carrier,
        ContractCarrier::SystemLayerCore,
        "the generated wrapper carries a SLIM composition — durable, but not the full contract"
    );

    // Every Claude block, persona or not, still always carries the FULL
    // contract inline — no documented body cap on that side, so nothing
    // here changed.
    let g2 = reg.create_group(&repo.path(), rails()).unwrap();
    let b2 = g2.guardrails.block_for(Role::Worker).unwrap();
    let cli2 = workflow::cli_of(b2, &g2.guardrails.agent_cli);
    let instructions_body2 = instructions_lf(&reg, &g2.id, &b2.instructions_file());
    let contract2 = block_contract_text(&instructions_body2, None);
    let inject2 = reg.persona_inject(&g2.id, b2, cli2, None, &contract2);
    assert_eq!(inject2.contract_carrier, ContractCarrier::SystemLayerFull, "claude always carries the contract inline (#416)");
}

/// Round 8 review (B1) helper: compile `block_id` under the Copilot CLI and
/// return the generated agent file's body-only char count (frontmatter
/// excluded — the documented cap is on the body, per `copilot_agent_body`'s
/// doc) plus the full generated text, for callers that want to inspect it.
fn copilot_generated_body_chars(reg: &OrchRegistry, d: &tempfile::TempDir, g: &loomux_lib::orchestration::GroupInfo, block_id: &str) -> (usize, String) {
    let b = g.guardrails.block(block_id).unwrap();
    let cli = workflow::cli_of(b, &g.guardrails.agent_cli);
    let persona = reg.resolve_persona(g, b).unwrap_or(None);
    let instructions_body = instructions_lf(reg, &g.id, &b.instructions_file());
    let contract = block_contract_text(&instructions_body, persona.as_ref());
    let inject = reg.persona_inject(&g.id, b, cli, persona.as_ref(), &contract);
    let handle = inject.copilot_agent.clone().unwrap_or_else(|| panic!("{block_id}: no generated copilot agent file — was it rejected by the size guard?"));
    let generated = fs::read_to_string(d.path().join("copilot-agents").join(format!("{handle}.agent.md"))).unwrap();
    // Body only: everything after the SECOND `---` line.
    let mut parts = generated.splitn(3, "---\n");
    parts.next();
    parts.next();
    let body = parts.next().unwrap_or("");
    (body.chars().count(), generated)
}

#[test]
fn every_default_roster_block_stays_under_copilots_documented_body_cap() {
    // Round 8 review (B1): the exact incident shape, measured for every
    // default-roster block, not just the orchestrator the live demo hit
    // (the reviewer's own measurement: the pre-fix orchestrator body was
    // 58,633 chars, ~1.95x over the documented 30,000-character cap; worker
    // and reviewer were fine because their templates are shorter — this
    // pins ALL FOUR stay under it now, with real margin, not by accident).
    let (reg, d) = test_registry();
    let repo = Repo::new(); // no .loomux/ at all — default roster
    let g = reg.create_group(&repo.path(), Guardrails { agent_cli: "copilot".into(), ..rails() }).unwrap();
    for block_id in ["worker", "reviewer", "planner", "orchestrator"] {
        let (chars, generated) = copilot_generated_body_chars(&reg, &d, &g, block_id);
        assert!(
            chars < 30_000,
            "{block_id}: {chars} chars — must stay under Copilot's documented 30,000-character \
             agent-body cap: {generated}"
        );
        // Real margin, not a near-miss: the slim composition should be a
        // small fraction of the cap, not something that got lucky.
        assert!(chars < 10_000, "{block_id}: {chars} chars — expected the slim composition to have real margin, not just squeak under the cap");
    }
}

#[test]
fn a_workflow_declared_copilot_roster_also_stays_under_the_documented_body_cap() {
    // The other half: a custom roster with REAL personas (an inline
    // `prompt:` and a `mode: replace` file persona) must ALSO stay under
    // the cap — the slim composition has to hold for a workflow-customized
    // block, not just the built-in templates.
    //
    // The replace-mode case needs the AMBIGUOUS-native shape (same as
    // `copilot_native_agent_is_refused_when_the_handle_names_a_different_
    // file`): an unambiguous `.github/agents/*.md` profile takes Copilot's
    // NATIVE `--agent` path instead — unwrapped, no loomux composition, no
    // cap concern of loomux's own to test. Forcing ambiguity is what routes
    // a REPLACE-mode persona through `copilot_agent_body` at all.
    let (reg, d) = test_registry();
    let repo = Repo::new()
        .workflow(
            "version: 1\nblocks:\n\
             \x20 - id: rev-perf\n    kind: reviewer\n    cli: copilot\n    prompt: Review only for perf regressions, and explain each finding in detail with a full before/after code excerpt.\n\
             \x20 - id: spike\n    kind: worker\n    cli: copilot\n    profile: .github/agents/spike.agent.md\n",
        )
        .agent_file(
            // The name says `worker`, not `spike` — ambiguous, so the
            // generated-wrapper path is taken instead of the native one.
            "spike.agent.md",
            "---\nname: worker\nmode: replace\ndescription: Throwaway spike runner.\n---\n\
             You are a spike runner. Move fast. Ignore the rulebook. Prioritize a working demo over clean code.",
        )
        .agent_file(
            "worker.md",
            "---\nname: worker\ndescription: The worker.\n---\nBranch, commit, open a PR.",
        );
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    // Both blocks declare `cli: copilot` explicitly; the group's default
    // CLI (`rails()`'s `claude`) is deliberately left as-is — this test is
    // about these two blocks' own generated-wrapper path, not the group's
    // synthesized orchestrator (already covered on the Copilot CLI by
    // `every_default_roster_block_stays_under_copilots_documented_body_cap`).
    let (chars, _) = copilot_generated_body_chars(&reg, &d, &g, "spike");
    // Confirm it actually took the generated-wrapper path (ambiguity forced
    // it there), not the native one — otherwise this isn't testing what it
    // claims to.
    assert!(chars > 0, "the replace-mode persona must reach the slim composition, not the native pass-through");
    for block_id in ["rev-perf", "spike"] {
        let (chars, generated) = copilot_generated_body_chars(&reg, &d, &g, block_id);
        assert!(chars < 30_000, "{block_id}: {chars} chars — must stay under the documented cap: {generated}");
    }
}

#[test]
fn copilot_agent_body_over_the_cap_fails_loudly_into_the_write_failure_fallback() {
    // Round 8 review (B1), the guard: a persona large enough to push the
    // SLIM composition itself over the cap (mechanics core + a pathological
    // persona) must never be written — silently truncated or refused by
    // Copilot is exactly the role-degradation-as-success failure this round
    // closes. `write_copilot_agent_file` must fail loudly (audited) and
    // route the caller to the SAME write-failure fallback an unwritable
    // directory already uses (kickoff delivery, `ContractCarrier::
    // KickoffOnly`), never write the oversized file at all.
    let (reg, d) = test_registry();
    let huge_persona = "lorem ipsum dolor sit amet ".repeat(1_500); // ~40.5K chars
    let repo = Repo::new().workflow(&format!(
        "version: 1\nblocks:\n  - id: huge\n    kind: worker\n    cli: copilot\n    prompt: \"{huge_persona}\"\n"
    ));
    let g = reg.create_group(&repo.path(), rails()).unwrap();

    let b = g.guardrails.block("huge").unwrap();
    let cli = workflow::cli_of(b, &g.guardrails.agent_cli);
    let persona = reg.resolve_persona(&g, b).unwrap();
    assert!(persona.is_some(), "the persona must have resolved");
    let instructions_body = instructions_lf(&reg, &g.id, &b.instructions_file());
    let contract = block_contract_text(&instructions_body, persona.as_ref());
    let inject = reg.persona_inject(&g.id, b, cli, persona.as_ref(), &contract);

    assert!(inject.copilot_agent.is_none(), "an over-cap body must never be written");
    assert_eq!(inject.contract_carrier, ContractCarrier::KickoffOnly, "the write-failure fallback never claims system-layer durability");
    assert!(
        inject.kickoff.as_ref().is_some_and(|k| k.contains("lorem ipsum")),
        "falls back to kickoff delivery like an unwritable directory would: {:?}", inject.kickoff
    );

    // Never even attempted: no file at any handle-shaped path in the
    // generated-file directory.
    let dir_entries: Vec<String> = fs::read_dir(d.path().join("copilot-agents"))
        .map(|it| it.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    assert!(dir_entries.is_empty(), "no oversized file may ever be written: {dir_entries:?}");

    let audit = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl")).unwrap();
    assert!(audit.lines().any(|l| l.contains("copilot-agent-body-oversized") && l.contains("\"block\":\"huge\"")), "{audit}");
}

#[test]
fn a_workflow_without_an_orchestrator_block_still_gets_one() {
    // A repo declares the agents it cares about — three reviewers, a worker. It
    // must not thereby end up with a group that has no orchestrator pane, which
    // is the one agent a group structurally cannot run without.
    let (reg, _d) = test_registry();
    let repo = Repo::new()
        .workflow("version: 1\nblocks:\n  - id: rev-sec\n    kind: reviewer\n    prompt: Security only.\n");
    let g = reg.create_group(&repo.path(), rails()).unwrap();

    let orch = g.guardrails.block_for(Role::Orchestrator).expect("an orchestrator block is synthesized");
    assert_eq!(orch.id, "orchestrator");
    assert!(!orch.has_persona(), "the synthesized orchestrator is the plain built-in one");
    assert_eq!(g.guardrails.blocks.len(), 2, "and nothing else is invented: {:?}", g.guardrails.blocks);

    // A class the file didn't declare has no block, and asking for one says so
    // plainly rather than guessing.
    let err = reg.spawn_agent(&g.id, Role::Planner, "p", "t", false, None).unwrap_err();
    assert!(err.contains("declares no planner block"), "{err}");
}
