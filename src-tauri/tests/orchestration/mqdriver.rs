//! The merge-queue driver on the production path, per-block model knobs and the poll path.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ---------- the merge queue's DRIVER, on the production path (#698) ----------

/// A scripted `MqRunner`: substring-matched canned replies, so the production
/// tick can be driven with no `git`, no `gh` and no network (constraint 3).
///
/// Separate from [`MqFake`] above, which answers one flat reply and is all the
/// reconcile seam needs. Driving a whole batch build needs a *sequence*, and
/// `tests/mergequeue.rs`'s richer fake cannot be shared across integration-test
/// targets — they are separate crates.
struct MqScript {
    replies: Vec<(&'static str, i32, String)>,
    calls: std::sync::Mutex<Vec<String>>,
}

impl MqScript {
    /// Every reply one clean batch build needs.
    fn batch_builder(head: &str, target_head: &str, merged: &str) -> MqScript {
        MqScript {
            replies: vec![
                // §7's live lookups.
                ("repo view", 0, "main\n".into()),
                (
                    "pr view 612",
                    0,
                    serde_json::json!({ "baseRefName": "integration", "headRefOid": head,
                                        "body": "b" })
                    .to_string(),
                ),
                // §4's mint check: this scratch name is free.
                ("ls-remote", 2, String::new()),
                // §8's construction.
                ("fetch", 0, String::new()),
                ("rev-parse refs/remotes/loomux-mq/target", 0, target_head.into()),
                ("rev-parse refs/remotes/loomux-mq/pr-612", 0, head.into()),
                ("worktree add", 0, String::new()),
                ("merge --no-ff", 0, String::new()),
                ("rev-parse HEAD", 0, merged.into()),
                // The create-only push and §5's draft PR.
                ("push --force-with-lease", 0, String::new()),
                ("pr create", 0, "https://github.com/o/r/pull/641\n".into()),
                // The batch's own checks, once it is in flight: an empty list is
                // PENDING, never success, so a second tick observes and does
                // nothing rather than landing an unjudged object.
                ("pr checks", 0, "[]".into()),
            ],
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn reply(&self, args: &[&str]) -> Result<loomux_lib::orchestration::mqdriver::CmdOut, String> {
        let joined = args.join(" ");
        self.calls.lock().unwrap().push(joined.clone());
        for (needle, code, stdout) in &self.replies {
            if joined.contains(needle) {
                return Ok(loomux_lib::orchestration::mqdriver::CmdOut {
                    code: Some(*code),
                    stdout: stdout.clone(),
                    stderr: String::new(),
                });
            }
        }
        panic!("MqScript: no canned reply for {joined:?}");
    }
}

impl loomux_lib::orchestration::mqdriver::MqRunner for MqScript {
    fn git(&self, args: &[&str]) -> Result<loomux_lib::orchestration::mqdriver::CmdOut, String> {
        self.reply(args)
    }
    fn gh(&self, args: &[&str]) -> Result<loomux_lib::orchestration::mqdriver::CmdOut, String> {
        self.reply(args)
    }
}

/// **#698, end to end through the production entry point.**
///
/// The batch pipeline had no caller. Every seam under it was green — the
/// planner, the scratch build, the create-only push, the draft-PR body, the
/// bisect splitter — while four approved PRs sat `queued` for 51 minutes and
/// the audit log carried four `mq-enqueued` lines and nothing after them. So
/// the assertion that matters is not "the driver works" but "**the poll loop
/// calls it**", and this test starts at `gh_poll_tick` for that reason: it is
/// the only shape that would have failed before the fix.
///
/// The runner is injected (`set_mq_runner_override`), which is the one thing
/// that lets the real entry point run without spawning `git` or `gh`.
#[test]
fn the_gh_poll_tick_drives_the_merge_queue_and_cuts_a_batch() {
    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const TARGET_HEAD: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const MERGED: &str = "dddddddddddddddddddddddddddddddddddddddd";

    let (reg, d) = test_registry();
    let repo = repo_with_merge_queue("mq-driver-tick", true);
    let g = reg.create_group(repo.to_str().unwrap(), advanced_rails()).unwrap();
    let gdir = d.path().join(g.id.as_str());

    // The gate the queue re-enforces (§6) — the same `merge_gate` file the shim
    // reads, never a second opinion — and one live pass against the PR's head.
    fs::write(gdir.join("merge_gate"), "require all-pass\nreviewer rev-a\n").unwrap();
    let vdir = gdir.join("verdicts").join("pr-612");
    fs::create_dir_all(&vdir).unwrap();
    fs::write(vdir.join("rev-a"), format!("pass\n{HEAD}\n0\nrev-1\n\napproved\n")).unwrap();

    // The state #698 observed: enqueued, eligible, and nothing draining it.
    let qfile = gdir.join("merge_queue.json");
    fs::write(
        &qfile,
        format!(
            r#"{{"version":1,"target":"integration","entries":[
                 {{"pr":612,"head":"{HEAD}","state":"queued","enqueued_ms":0}}]}}"#
        ),
    )
    .unwrap();

    let script = std::sync::Arc::new(MqScript::batch_builder(HEAD, TARGET_HEAD, MERGED));
    reg.set_mq_runner_override(Some(script.clone()));

    // The real unified poll tick, with an empty watch-result map — exactly what
    // `run_gh_poll_tick` hands it.
    let tick = reg.gh_poll_tick(1_000, &std::collections::HashMap::new());
    assert_eq!(
        tick.mq_serviced.as_deref(),
        Some(g.id.as_str()),
        "the poll loop must reach the merge-queue driver — the whole of #698"
    );

    // And the queue actually moved, on disk.
    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&qfile).unwrap()).unwrap();
    assert_eq!(
        after["entries"][0]["state"], "ci-wait",
        "the entry left `queued`: {after}"
    );
    assert_eq!(after["batch"]["prs"], serde_json::json!([612]), "{after}");
    assert_eq!(after["batch"]["draft_pr"], 641, "{after}");
    assert_eq!(after["batch"]["scratch_sha"], MERGED, "{after}");
    assert_eq!(after["batch"]["state"], "ci-wait", "{after}");

    // The audit log is the durable record, and #698's evidence was its
    // emptiness: four `mq-enqueued` lines and zero driver actions after them.
    let audit = reg.audit_log(&g.id);
    let actions: Vec<String> = audit.iter().map(|e| e.action.clone()).collect();
    for want in ["mq-batch-pushed", "mq-batch-built"] {
        assert!(actions.iter().any(|a| a == want), "expected {want}, saw {actions:?}");
    }

    // The construction really went through the seam — §4's create-only push
    // primitive and §5's draft PR, not some other path that happened to write
    // the same file.
    let calls = script.calls();
    assert!(
        calls.iter().any(|c| c.starts_with("push --force-with-lease=refs/heads/loomux/mq/")),
        "the scratch push must be create-only by primitive: {calls:?}"
    );
    assert!(calls.iter().any(|c| c.contains("pr create --draft")), "{calls:?}");

    // A second wake observes the batch it just cut rather than cutting another:
    // one in-flight batch per target (§4), and an empty check list is PENDING,
    // so nothing lands.
    let again = reg.gh_poll_tick(1_001, &std::collections::HashMap::new());
    assert_eq!(again.mq_serviced.as_deref(), Some(g.id.as_str()), "still the only due group");
    let after2: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&qfile).unwrap()).unwrap();
    assert_eq!(after2["batch"]["id"], after["batch"]["id"], "no second batch was cut: {after2}");

    reg.set_mq_runner_override(None);
}

/// **The product default is untouched** (§12): a group with no
/// `merge_queue.json` is never selected, so the driver spawns nothing and the
/// tick costs nothing — which is what makes it safe to run on every wake.
#[test]
fn the_driver_tick_skips_every_group_that_never_queued_anything() {
    let (reg, d) = test_registry();
    let repo = repo_with_merge_queue("mq-driver-idle", true);
    let g = reg.create_group(repo.to_str().unwrap(), advanced_rails()).unwrap();
    assert!(!d.path().join(g.id.as_str()).join("merge_queue.json").exists());

    // A runner that panics on any call: reaching it at all would be the defect.
    struct Never;
    impl loomux_lib::orchestration::mqdriver::MqRunner for Never {
        fn git(
            &self,
            args: &[&str],
        ) -> Result<loomux_lib::orchestration::mqdriver::CmdOut, String> {
            panic!("the driver must not run for a group that never queued anything: git {args:?}")
        }
        fn gh(&self, args: &[&str]) -> Result<loomux_lib::orchestration::mqdriver::CmdOut, String> {
            panic!("the driver must not run for a group that never queued anything: gh {args:?}")
        }
    }
    reg.set_mq_runner_override(Some(std::sync::Arc::new(Never)));

    let tick = reg.gh_poll_tick(1_000, &std::collections::HashMap::new());
    assert_eq!(tick.mq_serviced, None, "no group has a queue to drive");
    let audit = reg.audit_log(&g.id);
    assert!(
        !audit.iter().any(|e| e.action.starts_with("mq-")),
        "and no merge-queue audit noise: {:?}",
        audit.iter().map(|e| e.action.clone()).collect::<Vec<_>>()
    );
    reg.set_mq_runner_override(None);
}

/// **The queue is off unless the repo opted in** (§12), even with a state file
/// present — the block *and* the advanced-orchestrator toggle, the same pair
/// `merge_queue_enabled` requires of the three tools.
///
/// Without this, a group that had opted out of the whole workflow would still
/// have loomux pushing refs and opening PRs on its behalf.
#[test]
fn the_driver_tick_skips_a_group_whose_repo_did_not_opt_in() {
    let (reg, d) = test_registry();
    // Block ON, toggle OFF: the workflow file is not in force, so neither is
    // the queue.
    let repo = repo_with_merge_queue("mq-driver-optout", true);
    let g = reg.create_group(repo.to_str().unwrap(), rails()).unwrap();
    fs::write(
        d.path().join(g.id.as_str()).join("merge_queue.json"),
        r#"{"version":1,"target":"integration","entries":[
             {"pr":612,"head":"aaaaaaa","state":"queued","enqueued_ms":0}]}"#,
    )
    .unwrap();

    let tick = reg.gh_poll_tick(1_000, &std::collections::HashMap::new());
    assert_eq!(tick.mq_serviced, None, "a group that did not opt in is never driven");
}

// ─────────── per-block model knobs: the emit + clamp side (#687) ────────────

/// The capability table's #687 rows are the single source of truth the parser,
/// `clamped()` and the launcher all read, so its own invariants are worth
/// pinning: every row's values come from loomux's closed vocabulary, and every
/// row — including the ones with NO values — carries a reason.
///
/// The empty-with-a-reason rule is the one that matters. A CLI whose knob
/// loomux cannot deliver renders disabled in the launcher and refused by the
/// parser, and both surfaces quote this note. A row with an empty note would
/// degrade the honest "copilot reads effortLevel from ~/.copilot/settings.json"
/// into a bare "unsupported", which reads as loomux having forgotten.
#[test]
fn every_cli_row_explains_both_knobs() {
    for row in CLI_CAPS {
        for v in row.effort_levels {
            assert!(
                EFFORT_LEVELS.contains(v),
                "{}'s effort_levels names {v:?}, which is not in loomux's vocabulary",
                row.cli
            );
        }
        for v in row.context_variants {
            assert!(
                CONTEXT_VARIANTS.contains(v),
                "{}'s context_variants names {v:?}, which is not in loomux's vocabulary",
                row.cli
            );
        }
        assert!(!row.effort_note.is_empty(), "{} must say why its effort set is what it is", row.cli);
        assert!(!row.context_note.is_empty(), "{} must say why its context set is what it is", row.cli);
    }
    // The vendor facts this round rests on, asserted rather than assumed.
    //
    // EFFORT and CONTEXT are no longer the same list of CLIs, and that split is
    // the point of asserting them separately rather than as "only claude has a
    // seam": pi's --thinking is a session-scoped flag over a SUPERSET of
    // loomux's five levels (#2126), so two CLIs can set effort, while claude is
    // still alone on context because the [1m] suffix has no analogue anywhere
    // else.
    //
    // codex joins them with #2515 C1, and it is the one whose seam is NOT a
    // flag: `model_reasoning_effort` is a key in the profile file loomux
    // generates and selects with `-p`. The set is still all five, because
    // `ReasoningEffort::from_str` at the pinned tag maps `"max" => Max`
    // alongside `none`/`minimal`/`ultra`/`persistent` and a `Custom(String)`
    // catch-all — a strict superset of loomux's vocabulary. #2515's slice plan
    // claimed the opposite ("loomux's `max` is not deliverable"); the
    // correction is recorded on the issue, and this is where it is pinned, so
    // narrowing the row back has to argue with a test rather than with prose.
    for cli in ["claude", "pi", "codex"] {
        assert_eq!(
            cli_caps(cli).unwrap().effort_levels,
            EFFORT_LEVELS,
            "{cli} can be given every level loomux can ask for"
        );
    }
    assert_eq!(cli_caps("claude").unwrap().context_variants, CONTEXT_VARIANTS);
    for cli in ["copilot", "gemini", "opencode", "pi", "codex"] {
        assert!(
            cli_caps(cli).unwrap().context_variants.is_empty(),
            "{cli}'s context window is model-determined, not loomux-settable"
        );
    }
    for cli in ["copilot", "gemini", "opencode"] {
        assert!(
            cli_caps(cli).unwrap().effort_levels.is_empty(),
            "{cli} has no flag/env seam for effort"
        );
    }
}

/// `agent_cli_knobs`' wire shape (#687), which the launcher's selector reads to
/// decide enabled-vs-disabled-with-a-reason. Asserted on the pure body so no
/// tauri runtime is needed.
#[test]
fn agent_cli_knobs_reports_the_capability_row_verbatim() {
    let claude = cli_knobs_json("claude");
    assert_eq!(claude["known"], true);
    assert_eq!(claude["effort"]["values"], json!(["low", "medium", "high", "xhigh", "max"]));
    assert_eq!(claude["context"]["values"], json!(["1m"]));
    assert_eq!(claude["effort"]["note"], cli_caps("claude").unwrap().effort_note);

    // An unsupported knob is an EMPTY value set plus a non-empty reason — the
    // shape the launcher needs to render "disabled, because <vendor fact>"
    // instead of hiding the control.
    let copilot = cli_knobs_json("copilot");
    assert_eq!(copilot["known"], true);
    assert_eq!(copilot["effort"]["values"], json!([]));
    assert!(copilot["effort"]["note"].as_str().unwrap().contains("settings.json"));
    assert_eq!(copilot["context"]["values"], json!([]));
    assert!(!copilot["context"]["note"].as_str().unwrap().is_empty());

    // A CLI loomux has never evaluated: every knob off, and it says so rather
    // than pretending the claude answer applies.
    let unknown = cli_knobs_json("totally-unknown-cli");
    assert_eq!(unknown["known"], false);
    assert_eq!(unknown["effort"]["values"], json!([]));
    assert_eq!(unknown["context"]["values"], json!([]));
}

/// The silent half of the pair (`clamped_knob`): a knob that reached the roster
/// WITHOUT meeting the parser — a hand-edited `group.json` — is dropped to
/// empty against the block's **resolved** CLI, which is the piece the parser
/// structurally cannot do for a block that inherits the group default.
#[test]
fn clamped_drops_a_knob_the_resolved_cli_cannot_honor() {
    let block = |cli: &str, effort: &str, context: &str| workflow::Block {
        id: "w".into(),
        name: "w".into(),
        kind: Role::Worker,
        cli: cli.into(),
        model: String::new(),
        prompt: None,
        profile: None,
        allow: Vec::new(),
        role_hint: None,
        effort: effort.into(),
        context: context.into(),
        remote: None,
        driver: None,
        cache_ttl_minutes: None,
    };
    let resolve = |agent_cli: &str, b: workflow::Block| -> workflow::Block {
        let g = Guardrails {
            max_agents: 4,
            agent_cli: agent_cli.into(),
            blocks: vec![b],
            ..Guardrails::default()
        }
        .clamped();
        g.block("w").cloned().unwrap()
    };

    // Valid on claude: kept, normalized.
    let ok = resolve("claude", block("claude", " XHIGH ", "1M"));
    assert_eq!((ok.effort.as_str(), ok.context.as_str()), ("xhigh", "1m"));

    // Garbage: dropped to empty, never coerced to a neighbouring level.
    let junk = resolve("claude", block("claude", "banana", "9m"));
    assert_eq!((junk.effort.as_str(), junk.context.as_str()), ("", ""));

    // Valid VALUE, wrong CLI: still dropped — the vocabulary check is not the
    // capability check.
    let wrong = resolve("claude", block("copilot", "max", "1m"));
    assert_eq!((wrong.effort.as_str(), wrong.context.as_str()), ("", ""));

    // The inheriting block — the case the parser defers: the block names no
    // cli, so only `clamped()` knows it resolved to copilot.
    let inherited = resolve("copilot", block("", "max", "1m"));
    assert_eq!((inherited.effort.as_str(), inherited.context.as_str()), ("", ""));
    let inherited_ok = resolve("claude", block("", "max", "1m"));
    assert_eq!((inherited_ok.effort.as_str(), inherited_ok.context.as_str()), ("max", "1m"));

    // Absent stays absent — no default is invented on anyone's behalf.
    let none = resolve("claude", block("claude", "", ""));
    assert_eq!((none.effort.as_str(), none.context.as_str()), ("", ""));
}

/// The emit matrix (#687): claude gets `--effort <level>` and the `[1m]`-suffixed
/// model **only when set**, copilot and gemini get neither ever, and the two
/// spawn forms agree token for token.
#[test]
fn claude_emits_the_knobs_only_when_set_and_no_other_cli_ever_does() {
    let (reg, _d) = test_registry();
    let cfg = Path::new("C:/x/cfg.json");
    let gdir = Path::new("C:/data/group");
    let wd = Path::new("C:/repo");
    let knobs = |effort, context| workflow::ModelKnobs { effort, context };
    let line = |cli, model, k| {
        reg.build_agent_command_ex(
            cli, model, k, false, cfg, None, gdir, wd, None, false, Containment::None,
            &PersonaInject::default(), Role::Worker, None,
            None,
        ).unwrap()
    };
    let argv = |cli, model, k| {
        reg.build_agent_argv_ex(
            cli, model, k, false, cfg, None, gdir, wd, None, false, Containment::None,
            &PersonaInject::default(), Role::Worker, None,
            None,
        ).unwrap()
    };

    // Nothing pinned ⇒ the pre-#687 line, byte for byte (the `default()`
    // wrapper is the same call the whole existing suite makes).
    assert_eq!(
        line("claude", "sonnet", knobs("", "")),
        reg.build_agent_command(
            "claude", "sonnet", false, cfg, None, gdir, wd, None, false, Containment::None,
            &PersonaInject::default(),
        )
    );
    assert!(!line("claude", "sonnet", knobs("", "")).contains("--effort"));
    assert!(line("claude", "sonnet", knobs("", "")).contains("--model sonnet "));

    // Effort alone: the flag appears, the model is untouched.
    let e = line("claude", "sonnet", knobs("xhigh", ""));
    assert!(e.contains(" --effort xhigh"), "{e}");
    assert!(e.contains("--model sonnet "), "the model must not gain a suffix it wasn't given: {e}");

    // Context alone: the composed alias, QUOTED — `[1m]` is a glob pattern to a
    // POSIX shell — and no --effort.
    let c = line("claude", "sonnet", knobs("", "1m"));
    assert!(c.contains("--model \"sonnet[1m]\""), "{c}");
    assert!(!c.contains("--effort"), "{c}");
    // A full model name takes the same suffix (model-config §Extended context).
    assert!(line("claude", "claude-opus-4-8", knobs("", "1m")).contains("--model \"claude-opus-4-8[1m]\""));

    // Both, on both forms — and the argv token is UNQUOTED, because there is no
    // shell there to quote for.
    let both = line("claude", "opus", knobs("max", "1m"));
    assert!(both.contains("--model \"opus[1m]\"") && both.contains(" --effort max"), "{both}");
    let both_argv = argv("claude", "opus", knobs("max", "1m"));
    assert!(both_argv.contains(&"opus[1m]".to_string()), "{both_argv:?}");
    assert!(both_argv.windows(2).any(|w| w == ["--effort", "max"]), "{both_argv:?}");
    assert_eq!(shell_tokenize(&both), both_argv, "the two forms must not drift on the knobs");

    // copilot and gemini emit NEITHER, whatever they are handed — their seams
    // are a user-owned settings file and an interactive control, so there is no
    // flag to emit and inventing one would be a command line the CLI rejects.
    // (`clamped()` empties these long before a real spawn; this pins the emit
    // layer's own refusal, so the guarantee doesn't rest on the clamp alone.)
    for cli in ["copilot", "gemini"] {
        let l = line(cli, "auto", knobs("max", "1m"));
        assert!(!l.contains("--effort"), "{cli} must never be handed --effort: {l}");
        assert!(!l.contains("[1m]"), "{cli} must never be handed a [1m] model alias: {l}");
        assert!(l.contains("--model auto"), "{cli}'s model must be untouched: {l}");
        assert_eq!(shell_tokenize(&l), argv(cli, "auto", knobs("max", "1m")));
    }
}

/// #610's flag-placement rule, applied to the flag #687 adds: `--effort` must
/// land AFTER the last `--allowedTools` value, never between the flag and its
/// values — a flag emitted mid-list ends the list and demotes every pattern
/// after it to a stray positional argument.
///
/// This is the same property `claude_allow_patterns_are_not_severed_from_the_
/// allowedtools_flag` pins, re-run with the new flag present: that test fixes
/// the knobs at their defaults, so `--effort` never appears in it at all.
#[test]
fn the_effort_flag_does_not_sever_the_allowedtools_value_list() {
    let (reg, _d) = test_registry();
    let cfg = Path::new("C:/x/cfg.json");
    let hooks = Path::new("C:/x/cfg-hooks.json");
    let gdir = Path::new("C:/data/group");
    let wd = Path::new("C:/repo");
    let persona =
        PersonaInject { extra_allow: vec!["Bash(make:*)".into()], ..PersonaInject::default() };

    for containment in [Containment::None, Containment::NoEdits, Containment::ReadOnly] {
        // A read-only block never carries `extra_allow` (#222's capability
        // closure empties it) — pass it only where it can legitimately appear.
        let p = if containment == Containment::ReadOnly {
            PersonaInject::default()
        } else {
            persona.clone()
        };
        let mut want = vec!["mcp__orrerix", "Bash(git *)", "Bash(gh *)"];
        if containment != Containment::ReadOnly {
            want.push("Bash(make:*)");
        }
        for knobs in [
            workflow::ModelKnobs { effort: "max", context: "" },
            workflow::ModelKnobs { effort: "low", context: "1m" },
        ] {
            let line = reg.build_agent_command_ex(
                "claude", "opus", knobs, true, cfg, Some(hooks), gdir, wd, None, false,
                containment, &p, Role::Worker, None,
            None,
            ).unwrap();
            let argv_form = reg.build_agent_argv_ex(
                "claude", "opus", knobs, true, cfg, Some(hooks), gdir, wd, None, false,
                containment, &p, Role::Worker, None,
            None,
            ).unwrap();
            for (form, tokens) in [("command", shell_tokenize(&line)), ("argv", argv_form)] {
                let allowed = claude_allowed_tools_values(&tokens);
                for w in &want {
                    assert!(
                        allowed.iter().any(|t| t == w),
                        "#610/#687: {w:?} is missing from the {form} form's --allowedTools value \
                         list for {containment:?} with {knobs:?} — a flag was emitted BETWEEN the \
                         flag and its values, which terminates the list. Seen: {allowed:?}\n  \
                         line: {line}"
                    );
                }
                assert!(
                    !allowed.iter().any(|t| t == knobs.effort),
                    "the effort LEVEL must never land inside the allow list as a stray \
                     positional: {allowed:?}\n  line: {line}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// #743 S4 — the poll path: memoised hot reads, and the polled commands moving
// off the webview main thread. The census (#743 parts 1-2) ranked these as the
// app's #1 GUI-thread duty cycle while a human types.
// ---------------------------------------------------------------------------

/// #743 S4a: `orch_workflow_status` was in the group view's 2 s batch (since
/// #1608 the publisher makes the call, once a second per leased group), and
/// resolving the repo's default-branch NAME costs 2-4 blocking `git` spawns.
/// Inside the memo's window the polled read must serve the stored answer — and
/// the window must be a bound, not a freeze.
///
/// The name is display-only and already documented as unboundedly stale
/// (`git::default_branch_name` reads local refs, so it is only as fresh as
/// whatever last fetched), which is what makes memoising it free of correctness.
#[test]
fn workflow_status_memoises_the_default_branch_name_within_its_window() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(&repo)
            .args(args)
            .output()
            .expect("git must be installed for this test");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    fs::write(repo.join("f.txt"), "hi").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);
    // The resolution ladder prefers `main` over `master`, so start on `master`:
    // renaming to `main` later makes the LIVE answer change, which is the only
    // way to tell a served memo from a fresh resolution.
    git(&["branch", "-M", "master"]);

    let (reg, _d) = test_registry();
    let g = reg.create_group(repo.to_str().unwrap(), rails()).unwrap();

    assert_eq!(
        reg.workflow_status(&g.id)["default_branch"],
        "master",
        "the first read resolves live"
    );

    git(&["branch", "-M", "main"]);

    assert_eq!(
        reg.workflow_status(&g.id)["default_branch"],
        "master",
        "#743 S4a: inside DEFAULT_BRANCH_MAX_AGE the polled read must serve the memo instead \
         of spawning `git` 2-4 more times — the live answer is now `main`"
    );
    assert_eq!(
        reg.workflow_status_within(&g.id, Duration::ZERO)["default_branch"],
        "main",
        "the memo is a BOUNDED window, not a freeze: a zero window resolves live"
    );
    assert_eq!(
        reg.workflow_status(&g.id)["default_branch"],
        "main",
        "and a live resolution refreshes what the window then serves"
    );
}

/// Fixture: a claude session transcript under `<projects>/<encoded-cwd>/`, the
/// shape `compute_usage_snapshot`'s primary source reads.
fn write_usage_transcript(dir: &Path, sid: &str, input: u64, output: u64) {
    fs::create_dir_all(dir).unwrap();
    let text = format!(
        "{}\n{}\n",
        json!({"type":"user","message":{"content":"hi"}}),
        json!({"type":"assistant","message":{"id":"m1","model":"claude-opus-4-8",
            "usage":{"input_tokens":input,"output_tokens":output,
                     "cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}),
    );
    fs::write(dir.join(format!("{sid}.jsonl")), text).unwrap();
}

/// #743 S4b: `group_usage_live_within` is the heaviest thing on a cadence in
/// this app. Three callers asked for it inside the same ~2 s tick (group view,
/// tab bar, and `orch_autonomy`'s budget meter) until #1608 made the publisher
/// the only frontend-facing caller. Inside one window they must share ONE
/// computation; past it, the next caller recomputes.
#[test]
fn group_usage_serves_one_snapshot_per_window_and_recomputes_past_it() {
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg
        .spawn_agent(&g.id, Role::Worker, "w", "task", false, None)
        .unwrap();
    let sid = w.session_id.clone().expect("a claude worker gets a session id");
    let encoded = proj.path().join("C--tmp-repo");
    write_usage_transcript(&encoded, &sid, 1000, 500);

    // Far longer than any real poll gap, so the window — not the clock — is
    // what the assertions are about.
    let window = Duration::from_secs(3600);

    assert_eq!(
        reg.group_usage_within(&g.id, window)["lifetime_tokens"].as_u64(),
        Some(1500),
        "the first read computes"
    );

    // The agent keeps working: the live answer is now 3000 tokens.
    write_usage_transcript(&encoded, &sid, 2000, 1000);

    assert_eq!(
        reg.group_usage_within(&g.id, window)["lifetime_tokens"].as_u64(),
        Some(1500),
        "#743 S4b: a second polled caller inside the window must be served the stored \
         snapshot — not a second per-live-agent transcript scan and usage.json rewrite"
    );
    assert_eq!(
        reg.group_usage_within(&g.id, Duration::ZERO)["lifetime_tokens"].as_u64(),
        Some(3000),
        "the window is a BOUND, not a cache: a zero window recomputes"
    );
    assert_eq!(
        reg.group_usage(&g.id)["lifetime_tokens"].as_u64(),
        Some(3000),
        "and the public `group_usage` keeps its uncached contract — the MCP tool, the \
         autonomy anchor, and the budget enforcer all still read live"
    );
}

/// #1239: the polled usage path now resumes a per-transcript cursor instead of
/// re-parsing the file, so the regression it could introduce is STALENESS — an
/// agent whose spend stops moving because the cursor never advances past what
/// it read first. `tests/usage_cursor.rs` pins the reader's own contract (work
/// bound, resets, partial lines); this pins the freshness of the answer
/// `group_usage` actually serves, which is what a human reads off the group
/// view.
#[test]
fn group_usage_stays_fresh_across_a_transcript_append() {
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg
        .spawn_agent(&g.id, Role::Worker, "w", "task", false, None)
        .unwrap();
    let sid = w.session_id.clone().expect("a claude worker gets a session id");
    let encoded = proj.path().join("C--tmp-repo");
    write_usage_transcript(&encoded, &sid, 1000, 500);

    assert_eq!(
        reg.group_usage(&g.id)["lifetime_tokens"].as_u64(),
        Some(1500),
        "positive control: the first read finds and folds the transcript"
    );

    // The agent keeps working: one more assistant line APPENDED, exactly the
    // way Claude Code writes one — no rewrite of anything already there.
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(encoded.join(format!("{sid}.jsonl")))
        .unwrap();
    let next = json!({"type":"assistant","message":{"id":"m2","model":"claude-opus-4-8",
        "usage":{"input_tokens":30,"output_tokens":20,
                 "cache_creation_input_tokens":0,"cache_read_input_tokens":0}}});
    std::io::Write::write_all(&mut f, format!("{next}\n").as_bytes()).unwrap();
    drop(f);

    assert_eq!(
        reg.group_usage(&g.id)["lifetime_tokens"].as_u64(),
        Some(1550),
        "#1239: the appended line is folded onto the cursor. A cursor that never \
         advanced would still report 1500 here, and `upsert_usage_snapshot`'s \
         never-downgrade merge would then keep reporting it forever"
    );
}

/// #743 S4b: `mark_dead` captures an exiting agent's spend by writing
/// `usage.json` from OUTSIDE the usage computation, so it must drop the memo.
/// Without that, a killed agent keeps rendering as live in the group view until
/// the window expires.
#[test]
fn a_kill_snapshot_invalidates_the_polled_usage_memo() {
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg
        .spawn_agent(&g.id, Role::Worker, "w", "task", false, None)
        .unwrap();
    let sid = w.session_id.clone().unwrap();
    write_usage_transcript(&proj.path().join("C--tmp-repo"), &sid, 1000, 500);

    let window = Duration::from_secs(3600);
    let is_live = |v: &serde_json::Value| -> bool {
        v["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == w.id.as_str())
            .expect("the worker has a usage row")["live"]
            == true
    };

    assert!(
        is_live(&reg.group_usage_within(&g.id, window)),
        "the worker is live"
    );

    reg.mark_dead(&w.id, Some(0));

    assert!(
        !is_live(&reg.group_usage_within(&g.id, window)),
        "#743 S4b: the kill's own usage write must invalidate the memo, so the very next \
         polled read reports the agent dead instead of serving a snapshot taken while it \
         was still live"
    );
}


// ---------------------------------------------------------------------------
// #762 (F2 of #743) — concurrency pins for the two behaviour changes the
// dispatch conversion required. Both exist because moving these command bodies
// off the webview thread removed an ACCIDENTAL mutual exclusion: one thread ran
// every command, so no two could interleave. These pin what replaced it.
// ---------------------------------------------------------------------------

/// `solo_adopt`'s idempotence-by-pty must survive two callers at once, not just
/// two in sequence (the sibling test above covers sequence).
///
/// Its check-then-insert — read `by_pty`, mint an id, insert — is two lock
/// acquisitions with a mint between them, and until #762 the webview thread was
/// the only thing that stopped a second Connect gesture landing inside that
/// window. The failure it would produce is the silent kind: TWO delivery-only
/// identities for one pane, with both calls returning an id that looks fine.
///
/// The fix is a vacant-entry claim on `by_pty`, so the assertion is exact
/// rather than statistical: however the two threads interleave, the registry
/// must hold exactly one Solo agent for this pty and both callers must be told
/// the same id.
#[test]
fn concurrent_solo_adopts_of_one_pty_mint_exactly_one_identity() {
    let (reg, _d) = test_registry();
    let reg = std::sync::Arc::new(reg);
    // Several ptys, each adopted twice at once: one round would pass by luck
    // roughly as often as the race loses.
    for pty in 2000u32..2040 {
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let reg = std::sync::Arc::clone(&reg);
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait(); // start both inside the window, not one after the other
                    reg.solo_adopt(pty, "already running", "C:/tmp/x").unwrap()
                })
            })
            .collect();
        let ids: Vec<String> = handles
            .into_iter()
            .map(|h| h.join().unwrap()["agent_id"].as_str().unwrap().to_string())
            .collect();

        assert_eq!(
            ids[0], ids[1],
            "pty {pty}: two concurrent adopts returned different agent ids, so one pane now has \
             two identities — the check-then-insert lost its race (#762)"
        );
    }

    // One `solo-adopt` audit line per pty, and no more. The loser of the claim
    // returns before it audits, so this counts WINNERS — the direct observable
    // for "exactly one identity was minted per pane".
    let adopted = audit_count(&reg, solo_group_id(), "solo-adopt");
    assert_eq!(
        adopted, 40,
        "expected one adoption per pty across 40 ptys, got {adopted} — a second identity for one \
         pane is exactly what the check-then-insert used to allow (#762)"
    );
}

/// A consent marker and the in-memory set it mirrors must still agree once two
/// concurrent toggles have finished — the invariant #762 rev-260 B1 found
/// broken.
///
/// The marker file is load-time truth: `create_group`'s re-seed rebuilds the
/// gate from DISK. So "memory OFF, marker on disk" is not a cosmetic
/// disagreement — it restores merge authority the human explicitly withdrew, at
/// the next restart, audited as a benign `auto-merge-resumed`.
///
/// Reaching it takes no exotic timing: `set_auto_merge` released the set lock
/// between its reserve and its marker write, so a disable running inside that
/// window removed a marker that did not exist yet (`remove_marker` maps
/// NotFound to Ok), cleared the set, and let the enable recreate the file
/// behind it. The group panel does not disable the control while a call is in
/// flight, so a double-click emits exactly this enable/disable pair.
///
/// Asserted at quiescence, after both threads join, so the check is
/// deterministic even though which side wins is not: `marker_io` now spans
/// check-reserve-write, and agreement is guaranteed however the two order
/// themselves.
#[test]
fn concurrent_consent_toggles_leave_the_marker_and_memory_agreeing() {
    let (reg, _d, gid, _oid) = autonomous_setup();
    let marker = reg.state_root().join(gid.as_str()).join("auto_merge");
    let reg = std::sync::Arc::new(reg);

    for round in 0..200 {
        // Known state each round: gate off, no marker.
        reg.set_auto_merge(&gid, false).unwrap();
        assert!(!marker.is_file(), "round {round}: reset left a marker behind");

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = [true, false]
            .into_iter()
            .map(|on| {
                let reg = std::sync::Arc::clone(&reg);
                let gid = gid.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let _ = reg.set_auto_merge(&gid, on);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let in_memory = reg.is_auto_merge(&gid);
        let on_disk = marker.is_file();
        assert_eq!(
            in_memory, on_disk,
            "round {round}: auto-merge is {in_memory} in memory but {on_disk} on disk. The \
             re-seed at the next restart reads DISK, so a false-on-disk here hands back merge \
             authority the human turned off (#762 rev-260 B1)"
        );
    }
}
