//! The MCP spawn surface, and the harvested #105 persona parser.
//!
//! One module of the `workflow` integration-test target (`main.rs`),
//! split out of the former single-file `tests/workflow.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ─────────────────────────── the MCP spawn surface ──────────────────────────

pub(crate) fn orch_caller(reg: &OrchRegistry, group: &GroupId) -> Caller {
    let o = reg.spawn_agent(group, Role::Orchestrator, "orch", "", false, None).unwrap();
    Caller { agent_id: o.id, group: group.clone(), role: Role::Orchestrator, role_hint: None }
}

#[test]
fn mcp_spawn_rejects_an_unknown_kind_instead_of_making_it_a_worker() {
    let (reg, _d) = test_registry();
    let repo = Repo::new().git_init(); // the explicit worker spawn below cuts a real worktree (#338)
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let caller = orch_caller(&reg, &g.id);

    let call = |args: Value| {
        dispatch(&reg, &caller, "tools/call", &json!({ "name": "spawn_agent", "arguments": args })).unwrap()
    };

    // The pre-#222 parser was `_ => Role::Worker`: this call would have produced
    // an agent with a worktree and write access.
    let out = call(json!({ "kind": "revieweer", "task": "t" }));
    assert_eq!(out["isError"], json!(true), "an unknown kind must be an error");
    let text = out["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("unknown kind"), "{text}");
    assert_eq!(
        reg.list_agents(&g.id).as_array().unwrap().len(),
        1,
        "the rejected spawn must not have created an agent"
    );

    // ...and #544 closed the omission half of the same door: there IS no
    // documented default any more. A fresh spawn naming neither `kind` nor
    // `block` used to come back a worker; it is now refused, and the refusal
    // says what to pass. (`spawn_agent_never_defaults_to_the_privileged_class`
    // in tests/orchestration/ is the dedicated pin.)
    let out = call(json!({ "task": "t" }));
    assert_eq!(out["isError"], json!(true), "an omitted kind must be an error too");
    let text = out["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("#544") && text.contains("kind"), "{text}");
    assert_eq!(
        reg.list_agents(&g.id).as_array().unwrap().len(),
        1,
        "the refused spawn must not have created an agent either"
    );

    // An explicit kind still spawns the class it names.
    let out = call(json!({ "kind": "worker", "task": "t" }));
    assert_eq!(out["isError"], json!(false));
    assert!(out["content"][0]["text"].as_str().unwrap().contains("block worker"));
}

#[test]
fn mcp_spawn_can_name_a_block_and_the_block_decides_the_class() {
    let (reg, _d) = test_registry();
    // git_init(): the reviewer spawns below go through the MCP tool, and a
    // reviewer spawn's worktree now defaults on too (#359).
    let repo = Repo::new().workflow(FOCUSED_REVIEW).agent_file(
        "worker.md",
        "---\ndescription: repo worker\n---\nBranch first.",
    ).git_init();
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let caller = orch_caller(&reg, &g.id);
    let call = |args: Value| {
        dispatch(&reg, &caller, "tools/call", &json!({ "name": "spawn_agent", "arguments": args })).unwrap()
    };

    // Two reviewers from one roster — the feature in one assertion.
    for block in ["rev-security", "rev-tests"] {
        let out = call(json!({ "block": block, "task": "review the PR" }));
        assert_eq!(out["isError"], json!(false), "{:?}", out["content"][0]["text"]);
        assert!(out["content"][0]["text"].as_str().unwrap().contains(&format!("block {block}")));
    }
    let roster = reg.list_agents(&g.id);
    let blocks: Vec<&str> = roster
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["block"].as_str())
        .filter(|b| b.starts_with("rev-"))
        .collect();
    assert_eq!(blocks.len(), 2, "two distinct reviewer agents: {blocks:?}");

    // The block's kind wins over a `kind` the caller also passed — the roster is
    // authoritative about capability, not the caller.
    let out = call(json!({ "block": "rev-security", "kind": "worker", "task": "t" }));
    assert_eq!(out["isError"], json!(false));
    assert!(
        out["content"][0]["text"].as_str().unwrap().contains("Reviewer"),
        "the block's class must win: {:?}", out["content"][0]["text"]
    );

    // An unknown block is named as such, with the roster listed.
    let out = call(json!({ "block": "rev-ghost", "task": "t" }));
    assert_eq!(out["isError"], json!(true));
    let text = out["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("unknown block") && text.contains("rev-security"), "{text}");
}

#[test]
fn mcp_spawn_refuses_kind_orchestrator() {
    // The regression a review caught. Pre-#222 the kind parser ended in
    // `_ => Role::Worker`, so `kind: "orchestrator"` was swallowed by the
    // catch-all and quietly became a worker. Making unknown kinds an ERROR (the
    // right fix) removed that accident — and `orchestrator` IS a kind loomux can
    // name, so it started resolving.
    //
    // That is a privilege escalation, not a cosmetic bug: an orchestrator-kind
    // spawn skips the live-agent cap AND the spawn-rate backstop (both sit inside
    // `if role != Role::Orchestrator`), and its `Caller.role` passes
    // `require_orchestrator` — so it gets spawn_agent, kill_agent, set_state. An
    // orchestrator calling this in a loop would fork-bomb the machine with
    // fully-privileged panes. The tool's JSON-schema `enum` is advertisement; it
    // is never enforced against incoming args. This is the enforcement.
    let (reg, _d) = test_registry();
    let repo = Repo::new().git_init(); // the loop below spawns a real worker (#338)
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let caller = orch_caller(&reg, &g.id);
    let before = reg.list_agents(&g.id).as_array().unwrap().len();

    let out = dispatch(
        &reg,
        &caller,
        "tools/call",
        &json!({ "name": "spawn_agent", "arguments": { "kind": "orchestrator", "task": "t" } }),
    )
    .unwrap();
    assert_eq!(out["isError"], json!(true), "kind: orchestrator must be refused");
    let text = out["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("worker | reviewer | planner"), "{text}");
    assert_eq!(
        reg.list_agents(&g.id).as_array().unwrap().len(),
        before,
        "no second orchestrator may exist, not even briefly"
    );

    // The three delegate kinds still work.
    for kind in ["worker", "reviewer", "planner"] {
        let out = dispatch(
            &reg,
            &caller,
            "tools/call",
            &json!({ "name": "spawn_agent", "arguments": { "kind": kind, "task": "t" } }),
        )
        .unwrap();
        assert_eq!(out["isError"], json!(false), "{kind} must still spawn");
    }
}

#[test]
fn an_orchestrator_block_cannot_be_spawned_as_a_delegate() {
    // A group has exactly one orchestrator, minted at launch. Without this a
    // workflow file could declare a second `kind: orchestrator` block — which is
    // exempt from the live-agent cap and holds the privileged MCP tool set — and
    // an orchestrator could spawn itself a peer.
    let (reg, _d) = test_registry();
    let repo = Repo::new().workflow(
        "version: 1\nblocks:\n  - id: orch2\n    kind: orchestrator\n  - id: worker\n    kind: worker\n",
    );
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let err = reg
        .spawn_agent_ex(&g.id, Role::Worker, Some("orch2".into()), "", "t", false, None, None, None, None, None)
        .unwrap_err();
    assert!(err.contains("orchestrator block"), "{err}");
}

/// Drive one `spawn_agent` call and assert it was refused with a message that
/// says what a manager IS, and that no pane was opened. Split out so the two
/// refusal ROUTES below get one test each: they are separate guards, and a
/// single test would report only whichever failed first.
fn assert_manager_spawn_refused(reg: &OrchRegistry, caller: &Caller, group: &GroupId, args: Value) {
    let before = reg.list_agents(group).as_array().unwrap().len();
    let out = dispatch(reg, caller, "tools/call", &json!({ "name": "spawn_agent", "arguments": args }))
        .unwrap();
    assert_eq!(out["isError"], json!(true), "{args} must be refused, got {out}");
    let text = out["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("manager") && text.contains("human"),
        "the refusal must say what a manager IS, not just no: {text}"
    );
    assert_eq!(
        reg.list_agents(group).as_array().unwrap().len(),
        before,
        "no manager pane may exist, not even briefly"
    );
}

#[test]
fn every_block_the_orchestrator_is_told_to_spawn_is_one_spawn_agent_accepts() {
    // #1161 review B1. Two surfaces tell the orchestrator which blocks it may
    // open — its KICKOFF (`roster_note`) and its INSTRUCTION FILE
    // (`workflow_section`'s `{{BLOCKS}}`, under "Your delegates" followed by
    // "Spawn by block, not by kind") — and `spawn_agent` decides which it
    // actually accepts. Those are three renderings of one membership rule, and
    // before this they were two spellings of it: both lists filtered
    // `kind != Orchestrator`, which stopped meaning "spawnable" the moment a
    // second unspawnable class existed. A declared manager was listed as a
    // delegate by a slice that refuses to spawn one — a contradiction the
    // orchestrator reads on every turn, re-grounding included.
    //
    // So this pins the AGREEMENT rather than either side, in BOTH directions:
    // advertised ⇒ accepted, and accepted ⇒ advertised. Asserting only the
    // first would pass on a roster that advertised nothing at all.
    let (reg, _d) = test_registry();
    let repo = Repo::new().git_init().workflow(
        "version: 1\nblocks:\n\
         \x20 - id: worker\n    kind: worker\n\
         \x20 - id: rev\n    kind: reviewer\n\
         \x20 - id: plan\n    kind: planner\n\
         \x20 - id: manager\n    kind: manager\n",
    );
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    // The instruction file is read HERE — from the group render, before any
    // spawn — and that ordering is load-bearing rather than incidental.
    //
    // `spawn_agent_ex` re-renders the spawned block's instruction file from its
    // OWN, shorter var list (`REPO`/`GROUP_ID`/`MAX_AGENTS`/the three
    // `*_MODEL`s/`HOLD_LABEL`), and `render_template` leaves an unlisted key
    // LITERAL — so a spawn rewrites the file with `{{WORKFLOW}}` intact and the
    // whole workflow section, delegate list included, gone. That is a real
    // pre-existing defect (this repo's own live group dir carries literal
    // `{{ADVISOR_CONSULT_NOTE}}` and `{{LOCKS}}` in a delegate's file today),
    // it is NOT this slice's, and reading around it here is deliberate: this
    // test's subject is whether the two surfaces AGREE with the tool, and
    // measuring a file that a separate bug has blanked would make it pass for
    // the wrong reason — a vacuous green rather than a red about B1.
    let file = instructions_lf(&reg, &g.id, "orchestrator.md");
    assert!(
        file.contains("Your delegates:"),
        "the workflow section must be in the file this test reads, or it is measuring nothing: {file}"
    );

    // ONE orchestrator, used as both the kickoff's subject and the MCP caller:
    // the surfaces and the tool must be read against the same agent, or the
    // comparison is between different groups' answers.
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let caller =
        Caller { agent_id: o.id.clone(), group: g.id.clone(), role: Role::Orchestrator, role_hint: None };
    let kickoff = reg.kickoff_prompt(&o, &g, "", None);

    // `clamped()` synthesizes the orchestrator block, so this roster carries all
    // five classes — which makes the orchestrator block itself a second row in
    // the same pin, and the pre-existing half non-vacuous.
    let ids: Vec<String> = g.guardrails.blocks.iter().map(|b| b.id.clone()).collect();
    assert!(ids.iter().any(|i| i == "orchestrator"), "the roster must carry all five classes: {ids:?}");
    assert!(ids.iter().any(|i| i == "manager"), "{ids:?}");

    let mut disagreements: Vec<String> = Vec::new();
    for id in &ids {
        // The rows are `  - <id> (kind, cli, model)` and `- **`<id>`** — …`, so
        // the id followed by its opening delimiter is what "listed" means. A
        // bare `contains(id)` would match the prose around the list.
        let in_kickoff = kickoff.contains(&format!("- {id} ("));
        let in_file = file.contains(&format!("**`{id}`**"));
        let out = dispatch(
            &reg,
            &caller,
            "tools/call",
            &json!({ "name": "spawn_agent", "arguments": { "block": id, "task": "t" } }),
        )
        .unwrap();
        let accepted = out["isError"] == json!(false);
        if in_kickoff != accepted {
            disagreements.push(format!(
                "{id}: kickoff roster says spawnable={in_kickoff}, spawn_agent says {accepted} \
                 ({})",
                out["content"][0]["text"].as_str().unwrap_or("")
            ));
        }
        if in_file != accepted {
            disagreements.push(format!(
                "{id}: the orchestrator's instruction file says spawnable={in_file}, \
                 spawn_agent says {accepted}"
            ));
        }
    }
    assert!(disagreements.is_empty(), "a surface and the tool disagree:\n{}", disagreements.join("\n"));

    // Non-vacuity: the loop above is satisfied by "nothing is advertised and
    // nothing is accepted". Say what the answers actually are.
    assert!(kickoff.contains("- worker ("), "a worker block IS a delegate and must be listed: {kickoff}");
    assert!(!kickoff.contains("- manager ("), "the manager must NOT be listed as a delegate: {kickoff}");
    assert!(file.contains("**`worker`**"), "{file}");
    assert!(!file.contains("**`manager`**"), "nor in the instruction file: {file}");
}

#[test]
fn an_orchestrator_may_not_spawn_a_manager_by_kind() {
    // #1161. The manager is the human's own interface, opened for them rather
    // than spawned by the one agent it exists to relay TO — so `spawn_agent`
    // refuses it exactly as it refuses `kind: "orchestrator"`.
    let (reg, _d) = test_registry();
    let repo = Repo::new().git_init().workflow(WITH_MANAGER);
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let caller = orch_caller(&reg, &g.id);

    assert_manager_spawn_refused(&reg, &caller, &g.id, json!({ "kind": "manager", "task": "t" }));

    // The control: the ordinary delegate spawn on the SAME roster still works,
    // so the refusal is about the manager and not about this workflow.
    let out = dispatch(
        &reg,
        &caller,
        "tools/call",
        &json!({ "name": "spawn_agent", "arguments": { "block": "worker", "task": "t" } }),
    )
    .unwrap();
    assert_eq!(out["isError"], json!(false), "a worker block must still spawn: {out}");
}

#[test]
fn an_orchestrator_may_not_spawn_a_manager_by_naming_its_block() {
    // The route that would make the `kind` refusal decorative, and it is a
    // SEPARATE guard rather than the same one reached twice: a named `block:`
    // carries its own kind and that kind WINS over `kind:`, so a roster
    // declaring a manager hands the orchestrator a second spelling of the same
    // spawn — one that needs no `kind` argument at all, and one a check reading
    // only `kind` waves straight through.
    let (reg, _d) = test_registry();
    let repo = Repo::new().git_init().workflow(WITH_MANAGER);
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let caller = orch_caller(&reg, &g.id);

    assert_manager_spawn_refused(&reg, &caller, &g.id, json!({ "block": "manager", "task": "t" }));
    // ...including the spelling that pairs it with a legal `kind`, which is
    // what a `kind`-only guard would accept while opening a manager pane.
    assert_manager_spawn_refused(
        &reg,
        &caller,
        &g.id,
        json!({ "kind": "worker", "block": "manager", "task": "t" }),
    );
}

#[test]
fn the_orchestrator_kickoff_lists_a_declared_roster_and_says_edges_are_advisory() {
    // An orchestrator cannot spawn a block it doesn't know exists.
    let (reg, _d) = test_registry();
    let repo = Repo::new().workflow(FOCUSED_REVIEW).agent_file(
        "worker.md",
        "---\ndescription: repo worker\n---\nBranch first.",
    );
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let k = reg.kickoff_prompt(&o, &g, "", None);

    assert!(k.contains("rev-security (reviewer, claude, opus)"), "the roster must be listed: {k}");
    assert!(k.contains("rev-tests (reviewer, claude, sonnet)"));
    assert!(k.contains("has a persona"));
    assert!(k.contains("block:"), "it must be told HOW to spawn one: {k}");
    assert!(
        k.contains("ADVISORY"),
        "edges are the declared happy path, not a schedule — the orchestrator still routes: {k}"
    );
}

// ────────────────── the harvested #105 persona parser ───────────────────────

#[test]
fn copilot_agent_files_parse_with_folded_descriptions_and_native_keys() {
    // Harvested from PR #105. The parser must digest a REAL copilot agent file:
    // folded (`>`) descriptions whose continuation lines contain colons, `---`
    // separators inside the body, and copilot-native keys loomux doesn't own.
    let text = "---\nname: sempkg\ndescription: >\n  Version-accurate code research agent.\n  \
                Use when: exploring an unfamiliar dependency.\ntools: [agent, search, read]\n\
                agents: [\"*\"]\n---\n\n# sempkg\n\nYou are a research assistant.\n\n---\n\n## Workflow\nmore body\n";
    let p = profiles::parse_profile("sempkg.agent", text).unwrap();
    assert_eq!(p.name, "sempkg", "the `.agent` suffix is dropped from the stem");
    assert!(p.description.starts_with("Version-accurate code research agent."));
    assert!(p.description.contains("Use when: exploring"), "a colon in a folded value is text, not a key");
    assert!(p.instructions.contains("## Workflow"), "a `---` inside the body must not truncate it");
    assert_eq!(p.copilot_agent.as_deref(), Some("sempkg"), "--agent defaults to the persona name");
    assert!(p.model.is_none(), "copilot-native keys must not bleed into loomux fields");
    assert_eq!(p.mode, ProfileMode::Append, "mode defaults to append");

    // `mode: replace` (any case); anything unrecognized stays append — the safe
    // default, because an addendum cannot strip the built-in contract.
    assert_eq!(
        profiles::parse_profile("w", "---\nmode: Replace\n---\nBody.").unwrap().mode,
        ProfileMode::Replace
    );
    assert_eq!(
        profiles::parse_profile("w", "---\nmode: nonsense\n---\nBody.").unwrap().mode,
        ProfileMode::Append
    );

    // `allow:` patterns are sanitized before they can reach a shell line.
    let p = profiles::parse_profile("w", "---\nallow: Bash(make:*), bad\"quote\n---\nBody.").unwrap();
    assert_eq!(p.allow, vec!["Bash(make:*)", "badquote"]);

    // Not agent definitions.
    assert!(profiles::parse_profile("readme", "# just a doc").is_none());
    assert!(profiles::parse_profile("empty", "---\ndescription: x\n---\n\n").is_none(), "no body, no persona");
}

#[test]
fn discovery_reads_github_agents_and_only_that_directory_feeds_copilots_native_flag() {
    let repo = Repo::new()
        .agent_file("worker.md", "---\ndescription: repo worker\n---\nBranch first.")
        .agent_file("reviewer.agent.md", "---\ndescription: repo reviewer\n---\nBe strict.")
        .agent_file("notes.txt", "not a persona")
        .agent_file("no-front.md", "no frontmatter here");
    let found = profiles::discover_profiles(&repo.path());
    let names: Vec<&str> = found.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["reviewer", "worker"], "sorted by name; non-personas skipped");
    assert!(profiles::find_named(&found, "worker").is_some());
    assert!(profiles::discover_profiles("C:/definitely/not/a/repo").is_empty(), "a missing dir is not an error");

    // Copilot's `--agent` resolves names against `.github/agents/` — and ONLY a
    // file there can be named by it. A persona kept anywhere else is a loomux
    // concept that copilot has never heard of.
    assert!(profiles::is_copilot_native(".github/agents/worker.md"));
    assert!(profiles::is_copilot_native(".github\\agents\\worker.md"), "windows separators too");
    assert!(!profiles::is_copilot_native(".loomux/personas/worker.md"));
    assert!(!profiles::is_copilot_native("docs/worker.md"));
}
