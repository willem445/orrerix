//! Forking a session, fork lineage, scratch worktree cleanup and the suggestion-only input box.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// #3318 — forking a session. F1: the claude fork line. F2: codex, pi and
// opencode lines, the builder's loud refusal for a CLI with no seam (#3331
// item 2), and the `fork_session` tool / `fork_agent` registry method.
// ---------------------------------------------------------------------------

const FORK_PARENT: &str = "11111111-2222-3333-4444-555555555555";
const FORK_CHILD: &str = "99999999-8888-7777-6666-555555555555";

/// One fork-test line: the string form for `cli`, with every other input held
/// at one value so two calls differ only in `session`/`resume`/`fork_of`.
fn fork_test_line(
    reg: &OrchRegistry,
    cli: &str,
    session: Option<&str>,
    resume: bool,
    fork_of: Option<&str>,
) -> Result<String, String> {
    reg.build_agent_command_ex(
        cli, "sonnet", workflow::ModelKnobs::default(), false, Path::new("C:/x/orrerix-w-1.config.toml"),
        None, Path::new("C:/data/group"), Path::new("C:/repo"), session, resume, Containment::None,
        &PersonaInject::default(), Role::Worker, None, fork_of,
    )
}

/// [`fork_test_line`]'s argv twin.
fn fork_test_argv(
    reg: &OrchRegistry,
    cli: &str,
    session: Option<&str>,
    resume: bool,
    fork_of: Option<&str>,
) -> Result<Vec<String>, String> {
    reg.build_agent_argv_ex(
        cli, "sonnet", workflow::ModelKnobs::default(), false, Path::new("C:/x/orrerix-w-1.config.toml"),
        None, Path::new("C:/data/group"), Path::new("C:/repo"), session, resume, Containment::None,
        &PersonaInject::default(), Role::Worker, None, fork_of,
    )
}

/// The fork line is the RESUME line plus exactly the documented token, and it
/// names the parent with `--resume` while pre-minting the child with
/// `--session-id` (`CLAUDE_FORK_PREMINTS_CHILD_ID`, live check L1 on #3318).
///
/// Asserted as a DIFFERENCE against the resume line rather than against a
/// literal: a fork must never be able to change the model, the permission mode,
/// the allow list or anything else a resume already got right, and a literal
/// would pass just as well if it had.
#[test]
fn a_claude_fork_line_is_the_resume_line_plus_exactly_the_fork_token() {
    let (reg, _d) = test_registry();
    let resume = fork_test_line(&reg, "claude", Some(FORK_PARENT), true, None).unwrap();
    let fork = fork_test_line(&reg, "claude", Some(FORK_CHILD), false, Some(FORK_PARENT)).unwrap();

    // The parent is still the session being opened, and the child's id is
    // pre-minted ahead of it.
    assert!(
        fork.starts_with(&format!("claude --session-id {FORK_CHILD} --resume {FORK_PARENT} ")),
        "fork line: {fork}"
    );
    // ...and the ONLY other difference from the resume line is the fork token.
    let normalised = fork
        .replace(
            &format!("--session-id {FORK_CHILD} --resume {FORK_PARENT}"),
            &format!("--resume {FORK_PARENT}"),
        )
        .replace(" --fork-session", "");
    assert_eq!(normalised, resume, "a fork must differ from a resume in the fork token alone");
    // The token is emitted once, and it is the documented spelling.
    assert_eq!(fork.matches("--fork-session").count(), 1, "fork line: {fork}");
}

/// **The constant is what decides, and this is the test that makes the
/// documented flip real.** `CLAUDE_FORK_PREMINTS_CHILD_ID` is compile-time, so
/// the counterfactual cannot be executed at runtime — but the assertion can
/// read the SAME constant the builder reads (through claude's row, since F2)
/// and require the line to agree with it. Flip the constant and the line
/// follows, and this still passes; flip it while the builder ignores it and
/// this goes red.
#[test]
fn the_claude_fork_arm_is_selected_by_the_constant_not_by_the_caller() {
    let (reg, _d) = test_registry();
    // The caller offers a child id in BOTH runs. What changes the answer is the
    // constant, so offering one and not getting it is the learned arm working.
    let line = fork_test_line(&reg, "claude", Some(FORK_CHILD), false, Some(FORK_PARENT)).unwrap();
    let argv = fork_test_argv(&reg, "claude", Some(FORK_CHILD), false, Some(FORK_PARENT)).unwrap();

    // The row carries the constant; the builder reads the row. Pinned so the
    // row cannot quietly stop carrying it.
    assert_eq!(
        cli_caps("claude").unwrap().fork.premints_child(),
        CLAUDE_FORK_PREMINTS_CHILD_ID,
        "claude's row must carry the L1 constant as its premints_child"
    );
    if CLAUDE_FORK_PREMINTS_CHILD_ID {
        assert!(
            line.starts_with(&format!("claude --session-id {FORK_CHILD} --resume {FORK_PARENT} ")),
            "pre-mint arm: {line}"
        );
        assert_eq!(
            argv.iter().take(5).cloned().collect::<Vec<_>>(),
            vec!["claude", "--session-id", FORK_CHILD, "--resume", FORK_PARENT]
        );
    } else {
        assert!(line.starts_with(&format!("claude --resume {FORK_PARENT} ")), "learned arm: {line}");
        assert!(
            !line.contains("--session-id"),
            "the learned arm pre-mints nothing, even when the caller offers an id: {line}"
        );
        assert!(!argv.iter().any(|t| t == "--session-id"), "{argv:?}");
    }

    // Either way the parent is named and the fork token is emitted once — the
    // half of the shape the constant does NOT get to change.
    assert!(line.contains(&format!("--resume {FORK_PARENT}")), "{line}");
    assert_eq!(line.matches("--fork-session").count(), 1, "{line}");
}

/// #3318 F2 — **opencode**: `--session <parent> --fork`. The fork line is the
/// resume line with exactly one token added, directly after the session it
/// forks ("use with `--continue` or `--session`"), and it carries no child id
/// even when offered one: opencode has no flag that could name it.
#[test]
fn an_opencode_fork_line_is_the_resume_line_plus_exactly_the_fork_token() {
    let (reg, _d) = test_registry();
    let resume = fork_test_line(&reg, "opencode", Some(FORK_PARENT), true, None).unwrap();
    let fork = fork_test_line(&reg, "opencode", Some(FORK_CHILD), false, Some(FORK_PARENT)).unwrap();
    assert_eq!(
        fork,
        resume.replacen(
            &format!("--session {FORK_PARENT}"),
            &format!("--session {FORK_PARENT} --fork"),
            1
        ),
        "an opencode fork is its resume line plus `--fork` after the session, and nothing else"
    );
    assert_ne!(fork, resume, "positive control: the two lines really differ");
    assert!(!fork.contains(FORK_CHILD), "opencode cannot pre-mint a child: {fork}");
}

/// #3318 F2 — **codex**: `fork <parent>` in the slot `resume <parent>` holds,
/// last on the line after every root option. Everything before it — `-C`, the
/// profile, the model — is the resume line's, byte for byte.
#[test]
fn a_codex_fork_line_is_the_resume_line_with_fork_in_the_subcommand_slot() {
    let (reg, _d) = test_registry();
    let resume = fork_test_line(&reg, "codex", Some(FORK_PARENT), true, None).unwrap();
    let fork = fork_test_line(&reg, "codex", Some(FORK_CHILD), false, Some(FORK_PARENT)).unwrap();
    assert!(resume.ends_with(&format!(" resume {FORK_PARENT}")), "the premise: {resume}");
    assert!(fork.ends_with(&format!(" fork {FORK_PARENT}")), "fork line: {fork}");
    assert_eq!(
        fork.strip_suffix(&format!(" fork {FORK_PARENT}")),
        resume.strip_suffix(&format!(" resume {FORK_PARENT}")),
        "a codex fork differs from its resume in the subcommand word alone"
    );
    assert!(!fork.contains(FORK_CHILD), "codex cannot pre-mint a child: {fork}");
    assert!(!fork.contains(" resume "), "a fork line resumes nothing: {fork}");
}

/// #3318 F2 — **pi**: `--session-id <child> --fork <parent>`. The installed pi
/// (0.85.1) accepts that pair and writes the child under the given id, so the
/// fork line is the child's FRESH line plus exactly `--fork <parent>`, placed
/// beside the id it composes with.
#[test]
fn a_pi_fork_line_is_the_childs_line_plus_exactly_fork_parent() {
    let (reg, _d) = test_registry();
    let fresh = fork_test_line(&reg, "pi", Some(FORK_CHILD), false, None).unwrap();
    let fork = fork_test_line(&reg, "pi", Some(FORK_CHILD), false, Some(FORK_PARENT)).unwrap();
    assert_eq!(
        fork,
        fresh.replacen(
            &format!("--session-id {FORK_CHILD}"),
            &format!("--session-id {FORK_CHILD} --fork {FORK_PARENT}"),
            1
        ),
        "a pi fork is the child's own line plus `--fork <parent>`, and nothing else"
    );
    assert_ne!(fork, fresh, "positive control: the two lines really differ");
    assert_eq!(fork.matches("--session-id").count(), 1, "one id flag, naming the child: {fork}");
}

/// The two spawn forms agree on the fork exactly as they agree on everything
/// else — the string line tokenizes to the argv vector — for every CLI with a
/// seam, since each spells its fork in a different place on the line.
#[test]
fn every_fork_line_tokenizes_to_its_argv_form() {
    let (reg, _d) = test_registry();
    let mut checked = 0;
    for cli in ["claude", "codex", "opencode", "pi"] {
        let line = fork_test_line(&reg, cli, Some(FORK_CHILD), false, Some(FORK_PARENT)).unwrap();
        let argv = fork_test_argv(&reg, cli, Some(FORK_CHILD), false, Some(FORK_PARENT)).unwrap();
        assert_eq!(shell_tokenize(&line), argv, "{cli}: argv must equal the tokenized fork line\n  line: {line}");
        assert!(argv.iter().any(|t| t == FORK_PARENT), "{cli}: the parent is on the line: {argv:?}");
        checked += 1;
    }
    assert_eq!(checked, 4);
    // Position, not just presence, for claude: the token must sit AFTER the
    // last `--allowedTools` value, never between that flag and its values (#610).
    let argv = fork_test_argv(&reg, "claude", Some(FORK_CHILD), false, Some(FORK_PARENT)).unwrap();
    let allow = argv.iter().position(|t| t == "--allowedTools").expect("allow flag");
    let forked = argv.iter().position(|t| t == "--fork-session").expect("fork flag");
    assert!(forked > allow, "fork token must not sever the allow list: {argv:?}");
}

/// **#3331 item 2 — the builder REFUSES a fork it cannot spell, in the row's
/// own words**, on both forms. F1's builder dropped `fork_of` silently here and
/// returned the plain line, which `fork_session` would have launched as though
/// it were a fork.
///
/// Each refused row carries its own control: the SAME call with `fork_of: None`
/// builds a line, so the `Err` is caused by the fork request and not by
/// anything else about the CLI. `hermes` is an unrecognized CLI and is in the
/// list on purpose — it falls through to claude's arm, and that arm must not
/// fork on its behalf.
#[test]
fn a_cli_with_no_fork_seam_is_refused_by_the_builder_in_its_own_words() {
    let (reg, _d) = test_registry();
    for cli in ["copilot", "gemini", "hermes"] {
        let want = fork_refusal(cli).unwrap_or_else(|| panic!("{cli} has no seam"));
        let line = fork_test_line(&reg, cli, Some(FORK_PARENT), true, Some(FORK_PARENT));
        assert_eq!(line, Err(want.clone()), "{cli}: the string builder refuses with the row's note");
        let argv = fork_test_argv(&reg, cli, Some(FORK_PARENT), true, Some(FORK_PARENT));
        assert_eq!(argv, Err(want), "{cli}: the argv builder refuses identically");
        assert!(
            fork_test_line(&reg, cli, Some(FORK_PARENT), true, None).is_ok(),
            "{cli}: control — without a fork request the same call builds a line"
        );
    }
    // Positive control: a CLI WITH a seam is not refused by the same call.
    assert!(fork_test_line(&reg, "claude", Some(FORK_CHILD), false, Some(FORK_PARENT)).is_ok());
}

/// `fork_refusal` is the predicate a fork GESTURE asks, and it quotes the row's
/// own note so a rejected fork says what is missing rather than "unsupported".
#[test]
fn a_cli_with_no_fork_seam_is_refused_with_its_own_note() {
    // The four CLIs that can fork are the positive controls and they come
    // FIRST: a refusal test whose subject refuses everything passes just as
    // well broken.
    for cli in ["claude", "codex", "opencode", "pi"] {
        assert_eq!(fork_refusal(cli), None, "{cli} has a fork seam (#3318 F2)");
    }
    for cli in ["copilot", "gemini"] {
        let refusal = fork_refusal(cli).unwrap_or_else(|| panic!("{cli} must be refused"));
        assert!(refusal.contains(cli), "the refusal names the CLI: {refusal}");
        let ForkSeam::None(note) = cli_caps(cli).expect("row").fork else {
            panic!("{cli} has a fork seam but no argv fork exists for it");
        };
        assert!(refusal.contains(note), "the refusal quotes the row's note: {refusal}");
    }
    // An unknown CLI is refused too, rather than falling through to claude's arm.
    let unknown = fork_refusal("hermes").expect("an unknown CLI is refused");
    assert!(unknown.contains("hermes"), "{unknown}");
}

/// Every row states its fork position, and every `None` row explains it — the
/// same "empty is a claim, not an omission" rule `effort_note` already carries.
#[test]
fn every_cli_row_states_its_fork_position() {
    for caps in CLI_CAPS {
        match caps.fork {
            ForkSeam::Flag { flag, .. } | ForkSeam::ParentFlag(flag) => assert!(
                flag.starts_with("--") && flag.len() > 2,
                "{}: a fork flag must be a real token",
                caps.cli
            ),
            ForkSeam::Subcommand(word) => assert!(
                !word.starts_with('-') && !word.is_empty(),
                "{}: a fork subcommand is a word, not a flag",
                caps.cli
            ),
            ForkSeam::None(note) => assert!(
                note.len() > 20,
                "{}: a None fork row must explain itself",
                caps.cli
            ),
        }
    }
    // The set F2 wires, stated rather than left to be noticed in review: a
    // widened or narrowed table changes this list WITH the row.
    let mut wired: Vec<&str> = CLI_CAPS.iter().filter(|c| c.fork.token().is_some()).map(|c| c.cli).collect();
    wired.sort();
    assert_eq!(wired, vec!["claude", "codex", "opencode", "pi"]);
    // …and exactly the CLIs that can name the child up front pre-mint one: pi
    // always, claude per L1. codex and opencode never can.
    for (cli, premints) in [("pi", true), ("codex", false), ("opencode", false)] {
        assert_eq!(cli_caps(cli).unwrap().fork.premints_child(), premints, "{cli}");
    }
}

// ---------- `fork_session` / `fork_agent` (#3318 F2) ----------

/// A group on a REAL git repo with an orchestrator and one worker working in a
/// worktree on `feat/src` — the fork tests' common source. Returns
/// `(registry, dir, repo, group, orchestrator caller, worker)`.
fn fork_fixture(max_agents: u32) -> (OrchRegistry, tempfile::TempDir, RealRepo, GroupId, Caller, AgentEntry) {
    let (reg, dir) = test_registry();
    let repo = real_repo();
    let path = repo.repo.to_string_lossy().replace('\\', "/");
    let g = reg.create_group(&path, Guardrails { max_agents, ..rails() }).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let worker = reg
        .spawn_agent(&g.id, Role::Worker, "src", "the original task", true, Some("feat/src".into()))
        .unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();
    (reg, dir, repo, g.id, co, worker)
}

/// The commit a branch points at in `repo`.
fn branch_head(repo: &Path, branch: &str) -> String {
    let out = std::process::Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", branch])
        .output()
        .expect("git");
    assert!(out.status.success(), "rev-parse {branch}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// **The happy path, end to end through the MCP tool**: the fork is a new
/// delegate of the SOURCE's block, launched on the vendor's fork of the
/// source's session, in its own worktree cut from the source's branch, with
/// its parent recorded on the roster (in memory AND in `agents.json`) and ONE
/// `agent-fork` audit row naming both sides.
#[test]
fn fork_session_forks_a_worker_into_its_own_worktree_off_the_sources_branch() {
    let (reg, _d, repo, gid, co, src) = fork_fixture(3);
    let parent = src.session_id.clone().expect("claude pre-mints the source's session");

    let out = q_call(&reg, &co, "fork_session", json!({ "agent": src.id, "task": "try the other approach" }));
    assert_ne!(out["isError"], json!(true), "fork_session refused: {}", q_text(&out));

    // The audit row names the fork; the registry answers for it.
    let rows = audit_entries(&reg, &gid, "agent-fork");
    assert_eq!(rows.len(), 1, "one agent-fork row: {rows:?}");
    let fork_id = rows[0]["detail"]["agent"].as_str().expect("the row names the fork").to_string();
    let fork = &reg.agent(&fork_id).expect("the fork is on the roster");
    assert!(q_text(&out).contains(&fork_id), "the tool's reply names the fork: {}", q_text(&out));
    assert_eq!(fork.forked_from.as_deref(), Some(parent.as_str()), "the roster records the parent SESSION");
    assert_eq!(fork.block, src.block, "a fork inherits its source's block");
    assert_eq!(fork.role, Role::Worker);
    assert_ne!(fork.id, src.id, "a fork is a NEW agent");
    let child = fork.session_id.clone().expect("claude's fork pre-mints its child (L1 arm)");
    assert_ne!(child, parent, "the child is not the parent's session");

    // The launch line is claude's fork of the parent, naming the child.
    let req = reg.spawn_request_for_test(&fork.id).expect("a spawn request was built");
    assert!(
        req.command.contains(&format!("--session-id {child} --resume {parent} ")),
        "fork line: {}",
        req.command
    );
    assert!(req.command.contains("--fork-session"), "fork line: {}", req.command);

    // Its own worktree, cut from where the source is.
    assert_ne!(fork.cwd, src.cwd, "a worker fork never shares its source's checkout");
    let fork_branch = fork.branch.clone().expect("a worktree fork records its branch");
    assert_eq!(
        branch_head(&repo.repo, &fork_branch),
        branch_head(&repo.repo, "feat/src"),
        "the fork's branch starts at the source's branch head"
    );

    // Durable provenance.
    let roster = fs::read_to_string(reg.state_root().join(gid.as_str()).join("agents.json")).unwrap();
    let records: Vec<AgentRecord> = serde_json::from_str(&roster).unwrap();
    let rec = records.iter().find(|r| r.id == fork.id).expect("the fork's roster row");
    assert_eq!(rec.forked_from.as_deref(), Some(parent.as_str()));

    // The audit row, both sides named.
    let d = &rows[0]["detail"];
    assert_eq!(d["agent"], json!(fork.id));
    assert_eq!(d["parent_agent"], json!(src.id));
    assert_eq!(d["parent_session"], json!(parent));
    assert_eq!(d["child_session"], json!(child));
    assert_eq!(d["base"], json!("feat/src"));
    assert_eq!(d["requested_by"], json!(co.agent_id));
}

/// A fork is an ordinary delegate: it takes a slot under the live cap, and a
/// full group refuses it with the cap's own refusal, not a fork-specific one.
#[test]
fn a_fork_counts_against_the_live_agent_cap() {
    let (reg, _d, _repo, gid, co, src) = fork_fixture(1);
    let err = reg
        .fork_agent(&gid, &co.agent_id, &src.id, "", None, None, "")
        .expect_err("the source already holds the only slot");
    assert!(loomux_lib::orchestration::is_live_cap_refusal(&err), "refused by the CAP, not by something else: {err}");
    assert_eq!(audit_count(&reg, &gid, "agent-fork"), 0, "nothing was forked");
}

/// Every refusal `fork_agent` owns, each asserting the sentence ITS check
/// produces — so a refusal that fired for the wrong reason cannot stand in for
/// the right one. The last row is the control: the same source, forked
/// properly, is admitted.
#[test]
fn fork_session_refuses_each_source_it_cannot_fork_and_says_why() {
    let (reg, _d, _repo, gid, co, src) = fork_fixture(4);
    let refuse = |args: Value| -> String {
        let out = q_call(&reg, &co, "fork_session", args);
        assert_eq!(out["isError"], json!(true), "must be refused: {}", q_text(&out));
        q_text(&out)
    };
    // The orchestrator itself — a fork inherits the block, and there is one.
    let own = refuse(json!({ "agent": co.agent_id }));
    assert!(own.contains("not a delegate anyone may open a second of"), "{own}");
    // An id the group does not hold — the membership wording, leaking nothing.
    let unknown = refuse(json!({ "agent": "w-404" }));
    assert!(unknown.contains("unknown agent: w-404"), "{unknown}");
    // `worktree: false` for a worker — #338/#359's rule, applied to a fork.
    let shared = refuse(json!({ "agent": src.id, "worktree": false }));
    assert!(shared.contains("dedicated worktree"), "{shared}");
    assert_eq!(audit_count(&reg, &gid, "agent-fork"), 0, "no refusal forked anything");

    // Control: the same worker, forked without the refused argument.
    let ok = q_call(&reg, &co, "fork_session", json!({ "agent": src.id }));
    assert_ne!(ok["isError"], json!(true), "the control must be admitted: {}", q_text(&ok));
    assert_eq!(audit_count(&reg, &gid, "agent-fork"), 1);
}

/// A CLI with no fork seam is refused at the SOURCE, in the row's own words —
/// before any worktree is cut or config minted. copilot is the case #3331
/// names: its `--resume=` line needs `resume`, which a fork does not pass, so
/// F1's silent drop would have opened a FRESH copilot session and called it a
/// fork.
#[test]
fn a_fork_of_a_copilot_source_is_refused_with_its_rows_note() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", Guardrails { agent_cli: "copilot".into(), ..rails() }).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "", false, None).unwrap();
    reg.set_session_for_test(&w.id, "copilot-session-1");
    let err = reg.fork_agent(&g.id, &orch.id, &w.id, "", None, None, "").expect_err("copilot cannot fork");
    assert_eq!(err, fork_refusal("copilot").unwrap(), "the refusal IS the row's note, verbatim");
    assert_eq!(reg.list_agents(&g.id).as_array().unwrap().len(), 2, "nothing was spawned");
}

/// A codex fork's child is the vendor's to mint: the roster records NO session
/// for it (the store watcher learns it later), and the line forks the parent
/// in the subcommand slot. The opposite of the claude/pi pre-mint, pinned so
/// the two cannot be confused — a minted id the line never carried would give
/// the roster a session the pane is not running under.
///
/// The same source first proves the no-session refusal: a codex pane's id is
/// learned after boot, so a fork asked for before that has nothing to fork —
/// and the second call, once the id is known, is that refusal's control.
#[test]
fn a_codex_fork_mints_no_child_id_and_forks_in_the_subcommand_slot() {
    let (reg, dir) = test_registry();
    reg.set_codex_home_override(dir.path().join("codex-home"));
    let repo = real_repo();
    let path = repo.repo.to_string_lossy().replace('\\', "/");
    let g = reg.create_group(&path, Guardrails { agent_cli: "codex".into(), max_agents: 3, ..rails() }).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg
        .spawn_agent(&g.id, Role::Worker, "w", "", true, Some("feat/codex-src".into()))
        .unwrap();
    assert_eq!(w.session_id, None, "the premise: codex has not told anyone its id yet");

    let early = reg.fork_agent(&g.id, &orch.id, &w.id, "", None, None, "").expect_err("nothing to fork");
    assert!(early.contains("no recorded session yet"), "{early}");
    assert_eq!(audit_count(&reg, &g.id, "agent-fork"), 0);

    reg.set_session_for_test(&w.id, "thread-parent");
    let fork = reg.fork_agent(&g.id, &orch.id, &w.id, "look", None, None, "").expect("codex forks");
    assert_eq!(fork.session_id, None, "codex mints its own child id; loomux records none");
    assert_eq!(fork.forked_from.as_deref(), Some("thread-parent"));
    let req = reg.spawn_request_for_test(&fork.id).expect("a spawn request");
    assert!(req.command.ends_with(" fork thread-parent"), "codex fork line: {}", req.command);
    let rows = audit_entries(&reg, &g.id, "agent-fork");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["detail"]["child_session"], json!(null), "the child's id arrives later");
}

/// The fork's first turn names the three things that changed at the fork —
/// its NEW identity, its workspace and its task — and carries a delivery id of
/// its OWN: the parent's is in the copied history already acted on, so a fork
/// reading the parent's id as its own would take its brief for a duplicate.
#[test]
fn a_forks_first_turn_names_its_new_identity_and_carries_its_own_delivery_id() {
    let gid = GroupId::parse("g-1").unwrap();
    let fork = loomux_lib::orchestration::ForkSpawn {
        parent_agent: "w-3".into(),
        parent_name: "src".into(),
        parent_session: FORK_PARENT.into(),
        requested_by: "orch-1".into(),
    };
    let text = loomux_lib::orchestration::fork_kickoff_prompt(
        &gid, "w-7", "src (fork)", &fork, Path::new("C:/g/worker.md"),
        "Your working directory is a dedicated git worktree at C:/wt already checked out on branch 'agent/w-7'.",
        "try the other approach",
    );
    assert!(text.contains("You are a FORK"), "{text}");
    assert!(text.contains("You are now agent w-7"), "the new identity: {text}");
    assert!(text.contains(FORK_PARENT), "names the session it was forked from: {text}");
    assert!(text.contains("agent/w-7"), "the workspace note: {text}");
    assert!(text.contains("Your task:\ntry the other approach"), "the task: {text}");
    // The delivery id is the FORK's — literal format, never derived from code.
    assert!(text.contains("Delivery id: g-1/w-7/k1"), "its own delivery id: {text}");
    assert!(!text.contains("g-1/w-3/k1"), "never the parent's delivery id: {text}");

    // The idle arm: no task means wait, not carry on with the parent's work.
    let idle = loomux_lib::orchestration::fork_kickoff_prompt(
        &gid, "w-7", "src (fork)", &fork, Path::new("C:/g/worker.md"), "", "  ",
    );
    assert!(idle.contains("You have no task yet"), "{idle}");
    assert!(!idle.contains("Your task:"), "{idle}");
}

/// A worker cannot fork anything — `fork_session` is gated like `spawn_agent`
/// — and an orchestrator cannot fork another group's agent (same wording as an
/// unknown one, so nothing leaks).
#[test]
fn fork_session_is_for_spawners_and_never_crosses_a_group() {
    let (reg, _d, co, cw) = setup_mcp();
    let denied = q_call(&reg, &cw, "fork_session", json!({ "agent": co.agent_id }));
    assert_eq!(denied["isError"], json!(true));
    assert!(q_text(&denied).contains("or a lead pane"), "{}", q_text(&denied));

    let other = reg.create_group("C:/tmp/other", rails()).unwrap();
    let theirs = reg.spawn_agent(&other.id, Role::Planner, "p", "", false, None).unwrap();
    let crossed = q_call(&reg, &co, "fork_session", json!({ "agent": theirs.id }));
    assert_eq!(crossed["isError"], json!(true));
    assert!(
        q_text(&crossed).contains(&format!("unknown agent: {}", theirs.id)),
        "the membership wording, identical to an id that exists nowhere: {}",
        q_text(&crossed)
    );
    assert_eq!(audit_count(&reg, &other.id, "agent-fork"), 0);
}

/// The roster field is additive in both directions: a pre-F2 row decodes to
/// `None`, and a row that is not a fork never GAINS the key on a rewrite.
#[test]
fn forked_from_is_additive_on_the_durable_roster() {
    let old = r#"{"id":"w-1","role":"worker","name":"w","session":"s","cwd":"C:/r","status":"running","updated_ms":1}"#;
    let rec: AgentRecord = serde_json::from_str(old).unwrap();
    assert_eq!(rec.forked_from, None, "a pre-F2 row reads as not-a-fork");
    let back = serde_json::to_string(&rec).unwrap();
    assert!(!back.contains("forked_from"), "a non-fork row does not gain the key: {back}");
    let fork = AgentRecord { forked_from: Some("p".into()), ..rec };
    assert!(serde_json::to_string(&fork).unwrap().contains(r#""forked_from":"p""#));
}

/// **An unreadable review-drive record refuses the fork** — "I could not look"
/// is not "nothing owns this pane", the same fail-closed reading `kill_agent`
/// takes on the same file. The control is the same fork once the record is
/// readable again (removed), admitted.
#[test]
fn an_unreadable_drive_record_refuses_a_fork_rather_than_assuming_no_drive() {
    let (reg, _d, _repo, gid, co, src) = fork_fixture(3);
    let record = reg.state_root().join(gid.as_str()).join("review_drives.json");
    fs::write(&record, "{ this is not a drive record").unwrap();
    let err = reg
        .fork_agent(&gid, &co.agent_id, &src.id, "", None, None, "")
        .expect_err("an unreadable record is not evidence that nothing owns the pane");
    assert!(err.contains("could not read this group's review-drive record"), "{err}");
    assert_eq!(audit_count(&reg, &gid, "agent-fork"), 0);

    fs::remove_file(&record).unwrap();
    reg.fork_agent(&gid, &co.agent_id, &src.id, "", None, None, "")
        .unwrap_or_else(|e| panic!("the control — no record at all — is admitted: {e}"));
}

/// **A reviewer's workspace note names where its worktree was ACTUALLY cut
/// from** (review round 1, rev-std 2). It said "cut fresh from the default
/// branch" unconditionally — false for every reviewer FORK, which `fork_agent`
/// cuts from its source's branch, and for a `spawn_agent(kind: "reviewer",
/// base:)`. The no-base arm is the control: it still says the default branch.
#[test]
fn a_reviewers_workspace_note_names_the_branch_it_was_really_cut_from() {
    let based = loomux_lib::orchestration::reviewer_worktree_note("C:/wt", "agent/rev-4", Some("feat/src"));
    assert!(based.contains("cut fresh from branch 'feat/src'"), "{based}");
    assert!(!based.contains("the default branch"), "a based worktree is not cut from the default branch: {based}");
    let plain = loomux_lib::orchestration::reviewer_worktree_note("C:/wt", "agent/rev-4", None);
    assert!(plain.contains("cut fresh from the default branch"), "{plain}");
    // Everything else the note says is the same in both — it moved one fact.
    assert_eq!(
        based.replace("branch 'feat/src'", "the default branch"),
        plain,
        "only the origin differs between the two notes"
    );
    // One paragraph (the `\` continuations survived): no newline, no ten-space run.
    for note in [&based, &plain] {
        assert!(!note.contains('\n') && !note.contains("          "), "{note}");
    }
}

// ---------------------------------------------------------------------------
// #3368 — a fork's name and its parent, as the session browser reads them.
// ---------------------------------------------------------------------------

/// **`fork_session(name)` names the fork's pane.** The name is the agent's
/// roster name and the spawn request's — which the frontend opens the pane
/// under and records as the session's pane name, so it is what the header and
/// the session browser show. The control is the same fork with no name, which
/// takes the documented default.
#[test]
fn fork_session_name_is_the_forks_pane_name() {
    let (reg, _d, _repo, _gid, co, src) = fork_fixture(4);
    let out = q_call(&reg, &co, "fork_session", json!({ "agent": src.id, "name": "spike: retry" }));
    assert_ne!(out["isError"], json!(true), "fork_session refused: {}", q_text(&out));
    let named = reg
        .list_agents(&co.group)
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == json!("spike: retry"))
        .and_then(|a| a["id"].as_str().map(str::to_string))
        .expect("a fork named as asked is on the roster");
    let req = reg.spawn_request_for_test(&named).expect("a spawn request was built");
    assert_eq!(req.name, "spike: retry", "the pane opens under the asked-for name");

    let fork = reg.fork_agent(&co.group, &co.agent_id, &src.id, "", None, None, "").unwrap();
    assert_eq!(fork.name, format!("{} (fork)", src.name), "no name asked for: the default");
}

/// **`session_roles` names a delegate fork's parent** (#3368): the session
/// browser's fork tree reads the roster's `forked_from` through this, and a
/// row that dropped it would put a delegate fork at the top level as though
/// it were its own conversation. The SOURCE's row is the control: not a fork,
/// so `None` — the field is the pointer, not a constant.
#[test]
fn session_roles_carry_a_delegate_forks_parent_session() {
    let (reg, _d, _repo, _gid, co, src) = fork_fixture(4);
    let parent = src.session_id.clone().expect("claude pre-mints the source's session");
    let fork = reg.fork_agent(&co.group, &co.agent_id, &src.id, "", None, None, "").unwrap();
    let child = fork.session_id.clone().expect("claude's fork pre-mints its child (L1 arm)");
    let roles = reg.session_roles();
    let row = roles.iter().find(|r| r.session_id == child).expect("the fork's session has a role row");
    assert_eq!(row.forked_from.as_deref(), Some(parent.as_str()));
    let source_row = roles.iter().find(|r| r.session_id == parent).expect("the source's row");
    assert_eq!(source_row.forked_from, None, "the source is not a fork");
    // What the frontend actually receives: the key is on the wire.
    let wire = serde_json::to_value(row).unwrap();
    assert_eq!(wire["forked_from"], json!(parent));
}

// ───────── #3443: a reviewer's scratch worktree goes with its pane ─────────

/// `git <args>` run in `dir`: (succeeded, stdout).
fn git_in(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git must be installed for this test");
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned())
}

/// How many worktrees `repo` has REGISTERED, its main checkout included —
/// git's own answer, so a directory deleted behind git's back still counts.
fn registered_worktrees(repo: &Path) -> usize {
    let (ok, out) = git_in(repo, &["worktree", "list", "--porcelain"]);
    assert!(ok, "git worktree list failed in {}", repo.display());
    out.lines().filter(|l| l.starts_with("worktree ")).count()
}

fn local_branch_exists(repo: &Path, branch: &str) -> bool {
    git_in(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).0
}

fn scratch_rows(reg: &OrchRegistry, group: &GroupId, action: &str) -> Vec<serde_json::Value> {
    reg.audit_log(group).into_iter().filter(|e| e.action == action).map(|e| e.detail).collect()
}

/// End `agent_id` the way every kill of a pty pane ends: the pty exits and the
/// waiter reports it. `kill_agent`, a human's pane close and the idle reaper
/// all arrive here; a headless test has no pty to kill, so it drives the exit.
fn end_pane(reg: &OrchRegistry, agent_id: &str, pty: u32) {
    reg.set_pty_for_test(agent_id, pty);
    reg.on_pty_exit(pty, Some(0), "", 0, true);
    assert_eq!(
        reg.agent(agent_id).map(|a| a.status),
        Some(AgentStatus::Dead),
        "the fixture's premise: {agent_id} is dead"
    );
}

/// **A killed reviewer leaves no registered worktree and no branch** (#3443).
///
/// The controls before the kill pin that the spawn really cut both, so the
/// assertions after it are about the reclaim and not about a spawn that never
/// cut anything. The reviewer detaches first, as `gh pr checkout --detach`
/// leaves it in real use — so the branch is checked out nowhere when it goes.
#[test]
fn a_killed_reviewer_leaves_no_worktree_and_no_branch_behind() {
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let rev = reg.spawn_agent(&g.id, Role::Reviewer, "rev", "review #1", true, None).unwrap();
    let branch = rev.branch.clone().expect("a reviewer that cut a worktree records its branch");
    assert!(Path::new(&rev.cwd).is_dir(), "control: the spawn cut a worktree at {}", rev.cwd);
    assert_eq!(registered_worktrees(repo.path()), 2, "control: the main checkout and the reviewer's");
    assert!(local_branch_exists(repo.path(), &branch), "control: the spawn cut {branch}");
    assert!(git_in(Path::new(&rev.cwd), &["checkout", "-q", "--detach"]).0);

    end_pane(&reg, &rev.id, 34431);

    assert!(!Path::new(&rev.cwd).exists(), "the reviewer's worktree directory must be gone");
    assert_eq!(registered_worktrees(repo.path()), 1, "…and no longer registered with git");
    assert!(!local_branch_exists(repo.path(), &branch), "…and its branch {branch} deleted");
    let rows = scratch_rows(&reg, &g.id, "reviewer-worktree-removed");
    assert_eq!(rows.len(), 1, "one audit row for the one reclaim: {rows:?}");
    assert_eq!(rows[0]["agent"], json!(rev.id));
    assert_eq!(rows[0]["branch"], json!(branch));
    assert_eq!(rows[0]["branch_deleted"], json!(true), "{rows:?}");
}

/// **A worker's worktree is never removed by the reclaim** (#3443) — not when
/// the worker dies, and not when a REVIEWER that was running in it dies. The
/// second is the one way the roster can put a reviewer at a worker's path: a
/// spawn or resume given that path as its `cwd`. That reviewer never cut the
/// worktree, so no reviewer record carries a branch for it.
///
/// The last reviewer is the positive control: same group, same repo, its own
/// worktree, and it IS reclaimed — so the silence above is the predicate
/// deciding, not a reclaim that never runs.
#[test]
fn a_workers_worktree_survives_its_death_and_a_reviewer_dying_in_it() {
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let w = reg
        .spawn_agent(&g.id, Role::Worker, "w", "t", true, Some("feat/keep".into()))
        .unwrap();
    let rev = reg
        .spawn_agent_ex(&g.id, Role::Reviewer, None, "rev", "t", false, None, None, None, Some(w.cwd.clone()), None)
        .unwrap();
    assert_eq!(rev.cwd, w.cwd, "the fixture's premise: the reviewer runs in the worker's worktree");

    end_pane(&reg, &rev.id, 34432);
    end_pane(&reg, &w.id, 34433);

    assert!(Path::new(&w.cwd).is_dir(), "a worker's worktree must survive: {}", w.cwd);
    assert_eq!(registered_worktrees(repo.path()), 2, "…and stay registered");
    assert!(local_branch_exists(repo.path(), "feat/keep"), "…and keep its branch");
    assert!(scratch_rows(&reg, &g.id, "reviewer-worktree-removed").is_empty());
    assert!(scratch_rows(&reg, &g.id, "reviewer-worktree-remove-failed").is_empty());

    let r2 = reg.spawn_agent(&g.id, Role::Reviewer, "rev2", "t", true, None).unwrap();
    assert_eq!(registered_worktrees(repo.path()), 3, "control: the second reviewer cut its own");
    end_pane(&reg, &r2.id, 34434);
    assert_eq!(
        registered_worktrees(repo.path()),
        2,
        "the control: a reviewer's OWN worktree in this same group is reclaimed"
    );
    assert!(Path::new(&w.cwd).is_dir(), "…and the worker's is still there after it");
}

/// **A removal git refuses is audited, and the pane still dies** (#3443).
///
/// A locked worktree is the portable stand-in for a Windows file lock: `git
/// worktree remove --force` refuses both, and what matters is the same — the
/// kill has already happened, the refusal is on the audit log with git's own
/// words, and the branch is left alone because its worktree is still there.
#[test]
fn a_reclaim_git_refuses_is_audited_and_does_not_block_the_kill() {
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let rev = reg.spawn_agent(&g.id, Role::Reviewer, "rev", "t", true, None).unwrap();
    let branch = rev.branch.clone().unwrap();
    assert!(git_in(repo.path(), &["worktree", "lock", "--reason", "held", &rev.cwd]).0);

    end_pane(&reg, &rev.id, 34435);

    let failed = scratch_rows(&reg, &g.id, "reviewer-worktree-remove-failed");
    assert_eq!(failed.len(), 1, "the refusal must be audited: {failed:?}");
    assert_eq!(failed[0]["agent"], json!(rev.id));
    assert!(
        !failed[0]["error"].as_str().unwrap_or_default().is_empty(),
        "the row carries git's own error: {failed:?}"
    );
    assert!(scratch_rows(&reg, &g.id, "reviewer-worktree-removed").is_empty());
    assert!(Path::new(&rev.cwd).is_dir(), "git refused, so the worktree is still there");
    assert!(local_branch_exists(repo.path(), &branch), "…and so is its branch");
}

/// **A resume of a reclaimed reviewer gets its worktree back at the same path**
/// (#3443) — and when THAT pane dies, the worktree goes again.
///
/// The same path because a CLI may key the session on it (Claude Code stores a
/// transcript under a directory named after the cwd). The second death is the
/// resumed-pane case of the predicate: the resumed pane records no branch of its
/// own, so the reclaim must find the cut on the ORIGINAL pane's record.
#[test]
fn a_resumed_reviewer_gets_its_scratch_worktree_back_and_gives_it_up_again() {
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();
    let rev = reg.spawn_agent(&g.id, Role::Reviewer, "rev", "t", true, None).unwrap();
    let (session, cwd, branch) = (rev.session_id.clone().unwrap(), rev.cwd.clone(), rev.branch.clone().unwrap());

    end_pane(&reg, &rev.id, 34436);
    assert!(!Path::new(&cwd).exists(), "control: the reclaim took the worktree");

    let resumed = dispatch(&reg, &co, "tools/call", &json!({
        "name": "spawn_agent",
        "arguments": { "kind": "reviewer", "resume_session": session, "task": "round 2" },
    }))
    .unwrap();
    assert_eq!(resumed["isError"], false, "the resume must not refuse: {resumed:?}");
    let back = reg
        .list_agents(&g.id)
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["session"] == json!(session) && a["status"] != json!("dead"))
        .cloned()
        .expect("the resumed pane");
    assert_eq!(back["cwd"], json!(cwd), "the resume runs at the SAME path its session ran in");
    assert!(Path::new(&cwd).is_dir(), "…which exists again");
    assert_eq!(registered_worktrees(repo.path()), 2, "…as a registered worktree");
    let recut = scratch_rows(&reg, &g.id, "reviewer-worktree-recut");
    assert_eq!(recut.len(), 1, "{recut:?}");
    assert_eq!(recut[0]["branch"], json!(branch));

    // The resumed pane records no branch of its own today (#3442 may give it
    // the resumed session's); either way the reclaim must find the cut.
    let back_id = back["id"].as_str().unwrap().to_string();
    end_pane(&reg, &back_id, 34437);
    assert_eq!(registered_worktrees(repo.path()), 1, "the resumed pane's death reclaims it again");
    assert!(!local_branch_exists(repo.path(), &branch), "…branch included");
}

/// **A worker's vanished worktree is never re-cut by the resume hook** (#3443):
/// a fresh cut from the default branch would hand a resumed worker a checkout
/// without its own work. The reviewer beside it is the control that the hook
/// does cut when it should.
#[test]
fn a_vanished_worker_worktree_is_not_re_cut_for_a_resume() {
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", true, Some("feat/gone".into())).unwrap();
    assert!(git_in(repo.path(), &["worktree", "remove", "--force", &w.cwd]).0);

    assert!(!reg.restore_reviewer_scratch_worktree(&g.id, &w.cwd), "a worker's path is not re-cut");
    assert!(!Path::new(&w.cwd).exists());
    assert_eq!(registered_worktrees(repo.path()), 1);
    assert!(scratch_rows(&reg, &g.id, "reviewer-worktree-recut").is_empty());

    let rev = reg.spawn_agent(&g.id, Role::Reviewer, "rev", "t", true, None).unwrap();
    end_pane(&reg, &rev.id, 34438);
    assert!(reg.restore_reviewer_scratch_worktree(&g.id, &rev.cwd), "control: a reviewer's is");
    assert!(Path::new(&rev.cwd).is_dir());
}

/// The pure predicate, rule by rule (#3443). Every "kept" row sits beside the
/// fixture it differs from by one field, so each rule is shown to be the one
/// deciding.
#[test]
fn reviewer_scratch_verdict_keeps_anything_it_cannot_prove_is_scratch() {
    use loomux_lib::orchestration::{reviewer_scratch_verdict as verdict, ScratchVerdict, WorkspaceClaim};
    let repo = "/r/repo";
    let wt = "/r/repo-worktrees/agent/rev-7";
    let claim = |id: &str, reviewer: bool, cwd: &str, branch: Option<&str>, live: bool| WorkspaceClaim {
        id: id.into(),
        reviewer,
        cwd: cwd.into(),
        branch: branch.map(str::to_string),
        live,
    };
    let cut = claim("rev-7", true, wt, Some("agent/rev-7"), false);
    let scratch = ScratchVerdict::Scratch { branch: "agent/rev-7".into() };

    assert_eq!(verdict(repo, wt, &[cut.clone()], Some("rev-7")), scratch, "the base case");
    assert_eq!(verdict(repo, repo, &[claim("rev-7", true, repo, Some("x"), false)], None),
        ScratchVerdict::NotScratch, "never the main clone");
    assert_eq!(verdict(repo, wt, &[claim("rev-7", true, wt, None, false)], None),
        ScratchVerdict::NotScratch, "no reviewer record carries a branch here — nothing was cut");
    assert_eq!(verdict(repo, wt, &[claim("w-1", false, wt, Some("agent/rev-7"), false)], None),
        ScratchVerdict::NotScratch, "a worker's cut is not a reviewer's");
    // A resumed reviewer: its own record has no branch, the original's does.
    assert_eq!(
        verdict(repo, wt, &[cut.clone(), claim("rev-9", true, wt, None, false)], Some("rev-9")),
        scratch,
        "a resumed pane's worktree is found through the original pane's record"
    );
    assert_eq!(verdict(repo, wt, &[cut.clone(), claim("w-1", false, wt, None, false)], None),
        ScratchVerdict::Kept("claimed-by-a-non-reviewer"), "a worker running in it owns it too");
    assert_eq!(
        verdict(repo, wt, &[cut.clone(), claim("w-1", false, "/r/elsewhere", Some("agent/rev-7"), false)], None),
        ScratchVerdict::Kept("claimed-by-a-non-reviewer"),
        "a worker whose branch shares the name owns the branch"
    );
    assert_eq!(verdict(repo, wt, &[cut.clone(), claim("rev-9", true, wt, None, true)], Some("rev-7")),
        ScratchVerdict::Kept("in-use-by-a-live-pane"), "a live pane is still using it");
    assert_eq!(
        verdict(repo, wt, &[claim("rev-7", true, wt, Some("agent/rev-7"), true)], Some("rev-7")),
        scratch,
        "the dying pane itself does not count as a live user"
    );
    assert_eq!(
        verdict(repo, wt, &[cut.clone(), claim("rev-8", true, wt, Some("agent/other"), false)], None),
        ScratchVerdict::Kept("ambiguous-branch"),
        "two cuts naming different branches for one path are not guessed between"
    );
}

/// **A reviewer's branch that carries a commit of its own is kept** (#3443) —
/// the worktree still goes, since it is scratch, but a commit that no other
/// ref holds would be lost with the branch. That is decided on content, not
/// on the branch's name: the same rule keeps an existing branch with unpushed
/// work that a spawn was handed by name.
#[test]
fn a_reclaimed_reviewers_branch_is_kept_when_a_commit_lives_only_on_it() {
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let rev = reg.spawn_agent(&g.id, Role::Reviewer, "rev", "t", true, None).unwrap();
    let branch = rev.branch.clone().unwrap();
    let wt = Path::new(&rev.cwd);
    assert!(git_in(wt, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "mine"]).0);

    end_pane(&reg, &rev.id, 34439);

    assert!(!wt.exists(), "the worktree is scratch and still goes");
    assert_eq!(registered_worktrees(repo.path()), 1);
    assert!(local_branch_exists(repo.path(), &branch), "the branch holds the only copy of a commit");
    let rows = scratch_rows(&reg, &g.id, "reviewer-worktree-removed");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["branch_deleted"], json!(false), "{rows:?}");
    assert_eq!(rows[0]["branch_kept"], json!("has-commits-no-other-ref-holds"), "{rows:?}");
}

/// **A session-browser resume of a reclaimed reviewer cuts its worktree again**
/// (#3443) — the third resume route, `resume_recorded_session`, beside the MCP
/// arm and the driver. It resolves the workspace synchronously before its
/// background spawn, so the re-cut is observable the moment it returns.
#[test]
fn a_session_browser_resume_of_a_reclaimed_reviewer_cuts_its_worktree_again() {
    use loomux_lib::orchestration::resume_recorded_session;
    use std::sync::Arc;
    let repo = real_repo();
    let dir = tempfile::tempdir().unwrap();
    let reg = Arc::new(relaunch_registry(dir.path()));
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let rev = reg.spawn_agent(&g.id, Role::Reviewer, "rev", "t", true, None).unwrap();
    let (sid, cwd) = (rev.session_id.clone().unwrap(), rev.cwd.clone());
    end_pane(&reg, &rev.id, 34440);
    assert!(!Path::new(&cwd).exists(), "control: the reclaim took the worktree");

    let out = resume_recorded_session(&reg, &sid, None, false);

    assert!(Path::new(&cwd).is_dir(), "the resume must cut the worktree again at {cwd}: {out:?}");
    assert!(out.is_ok(), "…and then resume into it: {out:?}");
    assert_eq!(registered_worktrees(repo.path()), 2, "as a registered worktree");
    assert_eq!(scratch_rows(&reg, &g.id, "reviewer-worktree-recut").len(), 1);
}

/// **Ending a group without "remove worktrees" keeps a reviewer's worktree**
/// (#3443): `end_group` goes around the reclaim, because its own flag is the
/// human's decision about every worktree in the group. The reviewer ended by
/// its own pane exit first is the control — the same group does reclaim.
#[test]
fn ending_a_group_without_cleanup_keeps_a_reviewers_worktree() {
    let repo = real_repo();
    let (reg, _d) = test_registry();
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let gone = reg.spawn_agent(&g.id, Role::Reviewer, "rev1", "t", true, None).unwrap();
    let kept = reg.spawn_agent(&g.id, Role::Reviewer, "rev2", "t", true, None).unwrap();
    end_pane(&reg, &gone.id, 34441);
    assert!(!Path::new(&gone.cwd).exists(), "control: a reviewer's own exit reclaims");

    reg.end_group(&g.id, false).unwrap();

    assert_eq!(reg.agent(&kept.id).map(|a| a.status), Some(AgentStatus::Dead));
    assert!(Path::new(&kept.cwd).is_dir(), "end_group(cleanup=false) must keep {}", kept.cwd);
    assert_eq!(registered_worktrees(repo.path()), 2, "the main checkout and the kept reviewer's");
    assert_eq!(scratch_rows(&reg, &g.id, "reviewer-worktree-removed").len(), 1, "only the control's");
}

// ---- #3426: an input box holding only the CLI's own suggestion is idle ----
//
// After a turn, Claude Code writes a guess at the next prompt into its empty
// input box as placeholder text, painted faint (chalk `dim`, SGR 2) with the
// block cursor in inverse over its first character while the terminal has
// focus. As TEXT that row is `❯ <content>`: a pointer-option row to the ring and
// to `pointer_rendered`, and a composer that is not empty to
// `idle_prompt_row_rendered`. Pane w-3078 was held thirty minutes on exactly
// that, with `idle_row:false` on every record, so #903's idle release never
// fired and the fifteen-minute override never collected an idle read.

/// One Claude Code screen after a finished turn: prose, the turn footer, the
/// composer between its two rules, and a faint statusline. `composer` is the
/// raw bytes of the input row AFTER the `❯ ` — the thing each test varies.
fn claude_idle_screen(prose: &str, composer: &str) -> Vec<u8> {
    let rule = "─".repeat(60);
    let row = format!("❯ {composer}");
    painted(&[
        prose,
        "",
        "✻ Worked for 27s · done 5:30 PM",
        "",
        rule.as_str(),
        row.as_str(),
        rule.as_str(),
        "  \x1b[2m⏵⏵ auto mode on (shift+tab to cycle)\x1b[22m",
    ])
}

const SUGGESTION_3426: &str = "main is green now — rebase onto origin/main and re-run CI";

/// The suggestion as a focused pane paints it: inverse first character (the
/// cursor), faint remainder.
fn suggestion_focused() -> String {
    let mut chars = SUGGESTION_3426.chars();
    let first = chars.next().unwrap();
    format!("\x1b[7m{first}\x1b[27m\x1b[2m{}\x1b[22m", chars.as_str())
}

/// …and as an unfocused one paints it: all faint, no cursor.
fn suggestion_unfocused() -> String {
    format!("\x1b[2m{SUGGESTION_3426}\x1b[22m")
}

/// Both readings from ONE raw stream, the way production's `question_sample`
/// takes them — the grid through `question_visible`, which is the change.
fn sample_3426(raw: &[u8]) -> QuestionSample {
    QuestionSample {
        ring: Some(raw[raw.len().saturating_sub(4096)..].to_vec()),
        visible: question_visible(raw, 100, 20),
    }
}

/// Run one poll of the production predicate; return (holding, witness).
fn poll_3426(raw: Vec<u8>) -> (bool, Option<QuestionWitnessed>) {
    let witness: QuestionWitness = Default::default();
    let pred = question_hold_predicate_sampled(
        move || sample_3426(&raw),
        None,
        Some(std::rc::Rc::clone(&witness)),
        Vec::new(),
    );
    let holding = pred();
    let seen = witness.borrow().clone();
    (holding, seen)
}

#[test]
fn an_idle_pane_whose_box_holds_only_the_cli_suggestion_takes_the_delivery() {
    let prose = "● Rebased #3414 onto origin/main and pushed; reporting once CI is green.";
    for (label, composer) in [("focused", suggestion_focused()), ("unfocused", suggestion_unfocused())] {
        let raw = claude_idle_screen(prose, &composer);

        // Positive control: the wedge is really in this fixture. The ring fires
        // on the suggestion row itself, and the pre-#3426 composition (plain
        // `render_visible`) holds on it.
        let control = raw.clone();
        let pred = question_hold_predicate_sampled(
            move || QuestionSample {
                ring: Some(control.clone()),
                visible: trustworthy_composition(
                    loomux_lib::orchestration::termgrid::render_visible(&control, 100, 20),
                ),
            },
            None,
            None,
            Vec::new(),
        );
        assert!(pred(), "{label}: control — the text-only composition must hold on the suggestion row");

        let (holding, seen) = poll_3426(raw);
        let seen = seen.unwrap_or_else(|| panic!("{label}: the ring must still match, or nothing was tested"));
        assert_eq!(seen.matched.signal, "pointer-option", "{label}: the suggestion row is what fired");
        assert!(
            !holding,
            "{label}: an idle pane showing only the CLI's suggestion must take the delivery: {seen:?}"
        );
        assert_eq!(seen.grid, GridEvidence::IdlePrompt, "{label}: released as an idle composer");
        assert!(
            seen.idle_row,
            "{label}: the override's own term must read the suggestion-only box as idle, \
             never as the human's typing"
        );
    }
}

#[test]
fn the_incidents_prose_match_is_released_by_a_suggestion_only_composer() {
    // w-3078's recorded match was `prose-permission-phrase` on prose still on
    // screen (`grid: still-rendered`) — the class #903's idle release exists
    // for. The grid must answer `IdlePrompt` for it with the suggestion in the
    // box, which is the release the suggestion was blocking.
    let prose = "● Rebased; waiting for your go-ahead before acting on the note.";
    let m = prompt_wait_match(prose).expect("control: the prose alone matches");
    assert_eq!(m.signal, "prose-permission-phrase");
    for composer in [suggestion_focused(), suggestion_unfocused()] {
        let raw = claude_idle_screen(prose, &composer);
        let visible = question_visible(&raw, 100, 20).expect("a populated screen composes");
        assert!(visible.contains("waiting for your"), "control: the matched prose is on screen");
        assert_eq!(
            grid_evidence_for(&m, Some(Composed::plain(&visible))),
            GridEvidence::IdlePrompt,
            "the suggestion must not stop the idle-composer release:\n{visible}"
        );
    }
}

#[test]
fn a_line_the_human_typed_still_holds() {
    let prose = "● Rebased #3414 onto origin/main and pushed; reporting once CI is green.";
    let rest: String = SUGGESTION_3426.chars().skip(1).collect();
    for (label, composer) in [
        // Typed at normal intensity, the cursor an inverse space after it.
        ("typed", format!("{SUGGESTION_3426}\x1b[7m \x1b[27m")),
        // Typed text beside faint text: one normal cell refuses the lot.
        ("mixed", format!("wait\x1b[2m {SUGGESTION_3426}\x1b[22m")),
        // An inverse first cell is allowed only because the rest is faint —
        // here the rest is typed.
        ("cursor-on-typed", format!("\x1b[7mm\x1b[27m{rest}")),
    ] {
        let (holding, seen) = poll_3426(claude_idle_screen(prose, &composer));
        let seen = seen.unwrap_or_else(|| panic!("{label}: the ring must match"));
        assert!(holding, "{label}: a line a human could have typed must still hold");
        assert!(!seen.idle_row, "{label}: and must not feed the override an idle read");
    }
}

#[test]
fn a_real_dialog_still_holds_with_a_faint_prompt_row_above_it() {
    // A permission dialog: its highlighted choice is painted at normal
    // intensity, so the lowest prompt-glyph row is the dialog's and nothing is
    // cleared. The faint `❯` row ABOVE it is transcript; clearing it too would
    // hand the override an "empty composer" over a live dialog.
    let raw = painted(&[
        "\x1b[2m❯ run the tests again\x1b[22m",
        "",
        "Bash command",
        "  npm test",
        "Do you want to proceed?",
        "❯ 1. Yes",
        "  2. No, and tell Claude what to do differently",
        "\x1b[2mEsc to cancel\x1b[22m",
    ]);
    let (holding, seen) = poll_3426(raw);
    let seen = seen.expect("the ring must match the dialog");
    assert!(holding, "a live dialog must hold: {seen:?}");
    assert!(!seen.idle_row, "and the override must not read an idle composer on it");

    let (holding, _) = poll_3426(FIX_CLAUDE_ASK.as_bytes().to_vec());
    assert!(holding, "AskUserQuestion (faint footer, no pointer glyph) must still hold");
}

#[test]
fn residual_a_faint_prompt_row_above_a_glyphless_dialog_reads_as_an_idle_composer() {
    // The residual `docs/design/question-gate-authorship.md` states, pinned so
    // the disclosure cannot go false silently. With no prompt glyph in the
    // dialog, the lowest glyph-led row is whatever faint row sits above it (here
    // a past prompt; a dim `$`/`>` hint line would do the same), and it is
    // cleared: the WEAK idle reading turns true. The gate itself still holds, on the
    // menu structure the strong reading requires to be absent.
    let raw = painted(&[
        "\x1b[2m❯ pick an auth approach\x1b[22m",
        "Which authentication approach should we use?",
        "\x1b[7m  1. OAuth 2.0 with PKCE  \x1b[27m",
        "   2. Personal access tokens",
        "\x1b[2mEnter to select · ↑↓ to navigate · Esc to cancel\x1b[22m",
    ]);
    let (holding, seen) = poll_3426(raw);
    let seen = seen.expect("the ring must match the footer");
    assert!(holding, "the strong reading still holds on the menu structure: {seen:?}");
    assert!(seen.idle_row, "the residual: the weak reading reads this as an idle composer");
}

// ---------------------------------------------------------------------------
