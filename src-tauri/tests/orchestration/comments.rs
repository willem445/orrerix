//! Post_issue_comment and the pane-level notice diet.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ───────── #2815: a planner can post its own plan (`post_issue_comment`) ─────────
//
// The defect these pin, stated as the mechanism rather than as "gh was denied":
// a planner's deliverable is a whole document, and no route existed for one
// through the CLI shell. Claude Code refuses to allow-match a Bash command over
// 10,000 characters and treats newlines as subcommand separators, so a
// multi-line `--body` matches no permission rule and `dontAsk` denies it —
// with `Bash(gh *)` allowed the whole time. Measured on plan-2332: a
// 21,610-character `gh issue comment` denied, the `--body-file -` heredoc form
// denied, every file-write fallback denied. A tool argument is a JSON payload
// rather than a command line, so it has neither limit.

/// A stand-in `gh` that answers immediately, records the body file it was
/// handed, and reports its first three arguments by folding them into the URL
/// it prints.
///
/// Both halves are what make the end-to-end claim checkable rather than
/// asserted about a helper: the URL carries the VERB the registry really
/// spawned (`issue comment 7`), and `body-seen.txt` carries the bytes `gh`
/// really received — which is the only way to show a 15,000-character plan
/// arrived whole. `gh issue comment` prints the comment URL last, and this
/// prints exactly one line, which is the shape `post_issue_comment` parses.
fn echoing_gh(dir: &Path, name: &str) -> std::path::PathBuf {
    let (file, script) = if cfg!(windows) {
        (
            format!("{name}.cmd"),
            "@echo off\r\n\
             if not \"%~5\"==\"\" copy /y \"%~5\" \"%~dp0body-seen.txt\" >NUL\r\n\
             if not \"%~5\"==\"\" echo %~5>>\"%~dp0paths-seen.txt\"\r\n\
             echo https://example.invalid/c/%~1-%~2-%~3\r\n"
                .to_string(),
        )
    } else {
        (
            name.to_string(),
            "#!/bin/sh\n\
             d=$(dirname \"$0\")\n\
             if [ -n \"$5\" ]; then cat \"$5\" > \"$d/body-seen.txt\"; echo \"$5\" >> \"$d/paths-seen.txt\"; fi\n\
             printf '%s\\n' \"https://example.invalid/c/$1-$2-$3\"\n"
                .to_string(),
        )
    };
    let path = dir.join(file);
    fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn post_comment(reg: &OrchRegistry, c: &Caller, args: Value) -> Result<String, String> {
    let r = dispatch(reg, c, "tools/call",
        &json!({ "name": "post_issue_comment", "arguments": args })).unwrap();
    let text = r["content"][0]["text"].as_str().unwrap().to_string();
    if r["isError"] == true { Err(text) } else { Ok(text) }
}

fn listed_tool_names(reg: &OrchRegistry, c: &Caller) -> Vec<String> {
    dispatch(reg, c, "tools/list", &Value::Null).unwrap()["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

/// The whole point: a plan far too long to be a shell command reaches `gh`
/// intact, and the planner gets the URL back.
///
/// **The body is 15,000 characters DELIBERATELY.** A short body would pass
/// under the broken world too — the denial being fixed is keyed on length, not
/// on content — so a fixture under the 10,000-character ceiling would be a test
/// of the plumbing that cannot fail for the reason this change exists. It is
/// also over half of Windows' own 32,767-character command-line cap, which is
/// what makes the `--body-file` staging (rather than a `--body` argv) the thing
/// under test: an earlier draft passed the body as an argument and this test
/// failed on Windows alone, with `Command` refusing an unescapable argument to a
/// batch shim ("batch file arguments are invalid").
#[test]
fn a_planner_posts_a_plan_longer_than_the_shell_could_carry_and_gets_the_url() {
    let _serial = capture_lock();
    let (reg, _d) = test_registry();
    let repo = tempfile::tempdir().unwrap();
    let g = reg
        .create_group(&repo.path().to_string_lossy(), Guardrails { max_agents: 8, ..rails() })
        .unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let planner = reg.spawn_agent(&g.id, Role::Planner, "plan", "plan #7", false, None).unwrap();
    let cp = reg.resolve_token(&planner.token).unwrap();

    reg.set_gh_exec_override(Some((echoing_gh(repo.path(), "echoing_gh"), Duration::from_secs(20))));

    let body = format!("## Plan\n\n{}\n\n- step one\n", "x".repeat(15_000));
    assert!(
        body.len() > 10_000 && body.contains('\n'),
        "the fixture must be BOTH over the 10,000-character ceiling and multi-line — those are \
         the two independent reasons the shell route is denied, and a fixture missing either \
         cannot witness this change"
    );
    let out = post_comment(&reg, &cp, json!({ "issue": 7, "body": body })).unwrap();

    assert!(out.contains("https://example.invalid/c/issue-comment-7"),
        "the URL must come back to the planner (it is `report`'s detail_url), and the argv the \
         registry really spawned must be `issue comment 7` — the fake folds its first three \
         arguments into the URL precisely so this is one assertion, not two claims: {out}");

    // The BYTES gh received, not merely that the call returned. This is the
    // assertion the change exists for: the whole plan crossed the process
    // boundary, which is what no argv on this platform could have carried.
    let seen = fs::read_to_string(repo.path().join("body-seen.txt"))
        .expect("gh must have been handed a --body-file it could read");
    assert_eq!(seen.trim_end_matches(['\r', '\n']), body.trim_end_matches(['\r', '\n']),
        "the plan must arrive whole and unmodified (seen {} bytes, sent {})", seen.len(), body.len());

    // The staging file is scratch, not an artifact: it must not survive the
    // call. **Scanned in the staging SUBDIRECTORY** (#3061 residual 1): the
    // bodies moved out of the group dir, and a scan left pointing at the old
    // location would still pass — while looking at a directory the staging path
    // can no longer reach. That is a specimen leaving its class, so the
    // assertion moves with it rather than being relaxed.
    let staging = reg.state_root().join(g.id.as_str()).join(loomux_lib::orchestration::COMMENT_BODY_DIR);
    assert!(staging.is_dir(), "the post must really have staged a body, in its own directory");
    let leftovers: Vec<String> = fs::read_dir(&staging)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("-comment-body-") && n.ends_with(".md"))
        .collect();
    assert!(leftovers.is_empty(), "the staged body must be cleaned up, found: {leftovers:?}");

    // The human's record of what a pane published, without reading the pane.
    let row = reg.audit_log(&g.id).into_iter()
        .find(|e| e.action == "issue-comment")
        .expect("a post must leave an `issue-comment` audit row");
    assert_eq!(row.actor, cp.agent_id, "attributed to the pane that posted");
    assert_eq!(row.detail["issue"], 7);
    assert_eq!(row.detail["bytes"], body.len(), "the size posted, so an empty-looking plan shows");
    assert!(row.detail["url"].as_str().unwrap().contains("example.invalid"),
        "and the URL, so the row is followable: {row:?}");

    reg.set_gh_exec_override(None);
}

/// **#3061 residual 1: a staged comment body cannot clobber a block's
/// instruction file**, because the two no longer share a directory.
///
/// The collision is real and silent. The group dir holds each roster block's
/// instructions as `<block id>.md` (`workflow::Block::instructions_file`), a
/// block id is operator-authored and only `sanitize_id`-checked, and the old
/// staging name was `<agent>-comment-body-<seq>.md` in that same directory —
/// with a process-wide sequence counter that starts at 0. So a workflow
/// declaring a block called `<agent>-comment-body-0` had that block's
/// instructions truncated and then DELETED by that agent's first post after a
/// restart, with nothing failing.
///
/// The decoy here is written under exactly the name the pre-fix code would have
/// chosen, so this test performs the collision rather than describing it.
#[test]
fn a_staged_comment_body_cannot_clobber_a_file_in_the_group_dir() {
    let _serial = capture_lock();
    let (reg, _d) = test_registry();
    let repo = tempfile::tempdir().unwrap();
    let g = reg
        .create_group(&repo.path().to_string_lossy(), Guardrails { max_agents: 8, ..rails() })
        .unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let planner = reg.spawn_agent(&g.id, Role::Planner, "plan", "plan #7", false, None).unwrap();
    let cp = reg.resolve_token(&planner.token).unwrap();
    reg.set_gh_exec_override(Some((
        echoing_gh(repo.path(), "echoing_gh"),
        Duration::from_secs(20),
    )));

    // The decoy, at the exact path the pre-fix code would have staged into: this
    // agent's id, sequence 0. `COMMENT_BODY_SEQ` is process-wide and this test
    // shares a process with others, so the name is built from a RANGE of the
    // sequence numbers a single post could take rather than from one guess —
    // and every one of them is written, so the collision cannot be missed by
    // landing one number away.
    let group_dir = reg.state_root().join(g.id.as_str());
    fs::create_dir_all(&group_dir).unwrap();
    let decoys: Vec<std::path::PathBuf> = (0..64u64)
        .map(|n| group_dir.join(format!("{}-comment-body-{n}.md", cp.agent_id)))
        .collect();
    for d in &decoys {
        fs::write(d, "THE BLOCK'S INSTRUCTIONS").unwrap();
    }

    post_comment(&reg, &cp, json!({ "issue": 7, "body": "a comment\n" })).unwrap();

    for d in &decoys {
        // `unwrap`, not a `Result` comparison: the pre-fix code DELETED the
        // colliding file after writing over it, so an unreadable decoy is the
        // defect too and must fail here rather than compare unequal.
        assert_eq!(
            fs::read_to_string(d)
                .unwrap_or_else(|e| panic!("a post removed a file in the group dir: {} ({e})", d.display())),
            "THE BLOCK'S INSTRUCTIONS",
            "a post overwrote a file in the group dir: {}",
            d.display()
        );
    }
    // The population control, and it is what makes the loop above a statement
    // about the SUBDIRECTORY rather than about a post that never staged
    // anything: the staging directory exists, which only a post creates.
    assert!(
        group_dir.join(loomux_lib::orchestration::COMMENT_BODY_DIR).is_dir(),
        "the post must really have staged a body, in its own directory"
    );

    reg.set_gh_exec_override(None);
}

/// **#3061 residual 3: an orphaned staging file is swept**, and one that could
/// still belong to an in-flight post is not.
///
/// Both directions, because a sweep that deleted eagerly would be a worse defect
/// than the litter it cleans — it would pull the `--body-file` out from under a
/// concurrent `gh`. The clock is the caller's, which is the only reason the
/// young half can be performed at all.
#[test]
fn the_staging_sweep_deletes_an_orphan_and_spares_a_live_one() {
    let dir = tempfile::tempdir().unwrap();
    let staging = dir.path().join(loomux_lib::orchestration::COMMENT_BODY_DIR);
    fs::create_dir_all(&staging).unwrap();
    let orphan = staging.join("a-7-comment-body-0.md");
    fs::write(&orphan, "left behind by a kill between the write and the remove").unwrap();

    // Young: a file this post could still be handing to `gh`.
    loomux_lib::orchestration::sweep_staged_comment_bodies(&staging, SystemTime::now());
    assert!(
        orphan.exists(),
        "a file young enough to belong to an in-flight post must never be swept"
    );

    // Old: nothing can still own it.
    let later = SystemTime::now()
        + loomux_lib::orchestration::STAGING_ORPHAN_AGE
        + Duration::from_secs(60);
    loomux_lib::orchestration::sweep_staged_comment_bodies(&staging, later);
    assert!(!orphan.exists(), "an orphan past the bound must be swept");

    // The bound is DERIVED from what can actually hold a staging file open, not
    // picked: a sweep window shorter than the `gh` timeout could race a live
    // post. If that timeout ever grows past this window, this fails rather than
    // the race arriving in production.
    assert!(
        loomux_lib::orchestration::STAGING_ORPHAN_AGE
            > loomux_lib::orchestration::GH_CAPTURE_TIMEOUT,
        "the sweep window must outlast the longest a `gh` child can hold a body file"
    );

    // A sweep over a directory that is not there is silence, not an error: it
    // runs on the way INTO a post, and must never be able to fail one.
    loomux_lib::orchestration::sweep_staged_comment_bodies(
        &dir.path().join("no-such-dir"),
        later,
    );
}

/// **#3061 residual 4: what comes back is a URL, or it says it is not one.**
///
/// The old code took the last non-empty line of `gh`'s stdout and returned it as
/// "the comment's URL" with nothing in between — so a `gh` that printed a
/// deprecation notice and no URL, or printed nothing at all, handed an agent a
/// non-address to quote into a report, and printing nothing handed it the empty
/// string.
///
/// The shape checked is an absolute http(s) URL, deliberately NOT the
/// `#issuecomment-` fragment github.com renders: Enterprise is a different host,
/// `gh`'s output shape is not a documented contract, and this file's own
/// `echoing_gh` prints a URL without that fragment — a check the harness failed
/// would be calibrated to one deployment rather than to the property.
#[test]
fn a_post_whose_gh_printed_no_url_says_so_instead_of_inventing_one() {
    // The predicate first, over both polarities, so the end-to-end assertion
    // below is about the WIRING rather than about the rule.
    for good in [
        "https://github.com/o/r/issues/7#issuecomment-1",
        "https://ghe.internal/o/r/issues/7",
        "http://example.invalid/c/1",
        "noise\nhttps://example.invalid/c/2",
    ] {
        assert!(loomux_lib::gh::comment_url(good).is_some(), "must read as a URL: {good:?}");
    }
    for bad in [
        "",
        "\n\n",
        "gh: a deprecation notice",
        "https://example.invalid/c/1 and then some prose",
        "ftp://example.invalid/c/1",
        "https://",
        "https://example.invalid/c/1\nan afterword",
    ] {
        assert!(loomux_lib::gh::comment_url(bad).is_none(), "must NOT read as a URL: {bad:?}");
    }

    let _serial = capture_lock();
    let (reg, _d) = test_registry();
    let repo = tempfile::tempdir().unwrap();
    let g = reg
        .create_group(&repo.path().to_string_lossy(), Guardrails { max_agents: 8, ..rails() })
        .unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let planner = reg.spawn_agent(&g.id, Role::Planner, "plan", "plan #7", false, None).unwrap();
    let cp = reg.resolve_token(&planner.token).unwrap();
    reg.set_gh_exec_override(Some((mute_gh(repo.path(), "mute_gh"), Duration::from_secs(20))));

    let out = post_comment(&reg, &cp, json!({ "issue": 7, "body": "a comment\n" }))
        .expect("the post SUCCEEDED — gh simply printed no URL, and saying it failed would be a \
                 false claim an agent could double-post on");
    assert!(
        out.contains(loomux_lib::orchestration::POSTED_URL_UNREADABLE),
        "the caller is told plainly, in something that cannot be mistaken for an address: {out}"
    );

    let row = reg
        .audit_log(&g.id)
        .into_iter()
        .find(|e| e.action == "issue-comment")
        .expect("a post that reached gh leaves a row whichever way it went");
    assert_eq!(row.detail["url_unreadable"], json!(true));
    assert!(
        row.detail["raw"].as_str().is_some_and(|r| r.contains("deprecation")),
        "and the row carries what gh actually printed, so a human can still find the comment: \
         {row:?}"
    );

    reg.set_gh_exec_override(None);
}

/// A stand-in `gh` that posts successfully and prints a banner instead of a URL
/// — [`echoing_gh`]'s twin for the one case that fixture cannot express.
fn mute_gh(dir: &Path, name: &str) -> std::path::PathBuf {
    let (file, script) = if cfg!(windows) {
        (format!("{name}.cmd"), "@echo off\r\necho gh: a deprecation notice\r\n".to_string())
    } else {
        (
            name.to_string(),
            "#!/bin/sh\nprintf '%s\\n' 'gh: a deprecation notice'\n".to_string(),
        )
    };
    let path = dir.join(file);
    fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// The grant and the refusal are ONE test, so neither half can pass vacuously:

/// a `post_issue_comment` that refused everybody would satisfy the reviewer
/// assertions alone, and one that refused nobody would satisfy the other three.
#[test]
fn post_issue_comment_is_granted_to_three_classes_and_refused_to_a_reviewer() {
    let _serial = capture_lock();
    let (reg, _d) = test_registry();
    let repo = tempfile::tempdir().unwrap();
    let g = reg
        .create_group(&repo.path().to_string_lossy(), Guardrails { max_agents: 8, ..rails() })
        .unwrap();
    reg.set_gh_exec_override(Some((echoing_gh(repo.path(), "echoing_gh"), Duration::from_secs(20))));

    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let worker = reg.spawn_agent(&g.id, Role::Worker, "w", "do #7", false, None).unwrap();
    let planner = reg.spawn_agent(&g.id, Role::Planner, "plan", "plan #7", false, None).unwrap();
    let reviewer = reg.spawn_agent(&g.id, Role::Reviewer, "rev", "review #7", false, None).unwrap();

    for (what, token) in [("orchestrator", &orch.token), ("worker", &worker.token),
                          ("planner", &planner.token)] {
        let c = reg.resolve_token(token).unwrap();
        assert!(listed_tool_names(&reg, &c).iter().any(|t| t == "post_issue_comment"),
            "a {what} must SEE the tool");
        post_comment(&reg, &c, json!({ "issue": 7, "body": "ok" }))
            .unwrap_or_else(|e| panic!("a {what} must be able to post: {e}"));
    }

    // A reviewer: absent from the listing AND refused on dispatch — the #243
    // double gate, whose cosmetic half a caller can walk straight past by
    // naming the tool it was never shown.
    let cr = reg.resolve_token(&reviewer.token).unwrap();
    assert!(!listed_tool_names(&reg, &cr).iter().any(|t| t == "post_issue_comment"),
        "a reviewer must not see the tool");
    let err = post_comment(&reg, &cr, json!({ "issue": 7, "body": "ok" })).unwrap_err();
    assert!(err.contains("permission denied"), "got: {err}");
    assert!(err.contains("review_verdict"),
        "and the refusal must name the route a reviewer DOES have, or it teaches nothing \
         (the PlannerDenied precedent: a class told only no learns nothing about why it is \
         stuck): {err}");
    // SHAPE, beside the content — the assertion above passes just as happily on a
    // message carrying the source's own indentation. A Rust `\` continuation drops
    // the newline and keeps the leading spaces, and no asserted substring straddles
    // the break, so the leak survives a fully green suite (#1457). This is the pin
    // that fails: the message is one paragraph, so it has neither a newline nor a
    // run of two spaces.
    assert!(!err.contains('\n') && !err.contains("  "),
        "a user-facing refusal is ONE paragraph — no newline, no double space: {err:?}");

    reg.set_gh_exec_override(None);
}

/// Comments-only is a property of the CODE, not an argument check — so this
/// pins BOTH argv builders and asserts the dangerous verbs are unreachable for
/// any input, not merely absent from a happy path.
///
/// Both are pinned because they are two different channels: the webview's
/// `--body` form and the MCP path's `--body-file` form. Pinning only the one
/// this change happens to use would leave the other free to drift into a shape
/// a body could steer.
#[test]
fn neither_comment_argv_has_a_verb_a_caller_can_reach() {
    use loomux_lib::gh::{comment_argv, comment_file_argv, reject_empty_comment};

    assert_eq!(
        comment_argv("issue", 7, "hi").unwrap(),
        vec!["issue", "comment", "7", "--body", "hi"],
        "the verb and subcommand are literals in position 0 and 1; only the number and the \
         body come from a caller"
    );
    assert_eq!(
        comment_file_argv("issue", 7, "/tmp/b.md"),
        vec!["issue", "comment", "7", "--body-file", "/tmp/b.md"],
        "and the file form differs from it in exactly one flag"
    );

    // Bodies chosen to be exactly what would escape if the body were ever
    // interpolated into a command line rather than passed as the VALUE of
    // `--body`: a leading `-` (read as a flag), and the verbs this tool must
    // never reach.
    for hostile in [
        "-not a flag",
        "--json state",
        "x\n--add-label wontfix",
        "close\nmerge\n--delete-branch",
        "$(gh pr merge 7)",
    ] {
        for argv in [comment_argv("issue", 7, hostile).unwrap(),
                     comment_file_argv("issue", 7, hostile)] {
            assert_eq!(argv.len(), 5, "a body can never ADD an argument: {argv:?}");
            assert_eq!(&argv[..3], &["issue", "comment", "7"],
                "and can never displace the three that name the operation: {argv:?}");
            assert_eq!(argv[4], hostile, "it arrives verbatim, as data: {argv:?}");
            for verb in ["edit", "close", "reopen", "merge", "create", "review", "delete"] {
                assert!(!argv[..4].iter().any(|a| a == verb),
                    "no input may put `{verb}` in the argv's command position: {argv:?}");
            }
        }
    }

    // Rejected before any spawn: `gh` with no body value opens an interactive
    // editor, which in an agent pane is a hang, not an error. The guard is
    // shared, so the file path enforces it through `reject_empty_comment`
    // rather than keeping a second copy that could drift.
    for empty in ["", "   ", "\n\t \r\n"] {
        assert_eq!(comment_argv("issue", 7, empty).unwrap_err(), "empty comment",
            "whitespace-only is empty too: {empty:?}");
        assert_eq!(reject_empty_comment(empty).unwrap_err(), "empty comment",
            "and the shared guard agrees: {empty:?}");
    }
    assert!(reject_empty_comment("x").is_ok(), "a non-empty body is not refused");
}

/// Two posts by ONE agent must not share a staging path (review round 1).
///
/// The race is real rather than theoretical: a CLI is free to issue independent
/// tool calls in a single message, so one pane can have two posts in flight. With
/// the path keyed on the agent id alone, the second write truncates the file the
/// first is still handing to `gh`, and the first post publishes the second's text
/// or a torn prefix of it — with nothing failing, which is what makes it worth a
/// test rather than a comment.
///
/// It is pinned on the OBSERVABLE consequence, not on the naming scheme: the fake
/// `gh` records the `--body-file` path it was actually handed, and the assertion
/// is that the two differ. A test that asserted the filename format would pass on
/// any scheme that merely looked unique.
#[test]
fn two_posts_by_one_agent_get_distinct_staging_files() {
    let _serial = capture_lock();
    let (reg, _d) = test_registry();
    let repo = tempfile::tempdir().unwrap();
    let g = reg
        .create_group(&repo.path().to_string_lossy(), Guardrails { max_agents: 8, ..rails() })
        .unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let planner = reg.spawn_agent(&g.id, Role::Planner, "plan", "plan #7", false, None).unwrap();
    let cp = reg.resolve_token(&planner.token).unwrap();
    reg.set_gh_exec_override(Some((echoing_gh(repo.path(), "echoing_gh"), Duration::from_secs(20))));

    post_comment(&reg, &cp, json!({ "issue": 7, "body": "first" })).unwrap();
    post_comment(&reg, &cp, json!({ "issue": 7, "body": "second" })).unwrap();

    let seen = fs::read_to_string(repo.path().join("paths-seen.txt")).unwrap();
    let paths: Vec<&str> = seen.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    assert_eq!(paths.len(), 2, "both posts must have reached gh: {paths:?}");
    assert_ne!(paths[0], paths[1],
        "the same agent's two posts must stage to DIFFERENT files, or one truncates the \
         other mid-flight and publishes the wrong text: {paths:?}");

    reg.set_gh_exec_override(None);
}

/// A post that FAILED still leaves an audit row, carrying the error (review
/// round 1).
///
/// Without this the audit is a record of successes only, and a failed post is
/// indistinguishable in the log from one that was never attempted — which is the
/// worse of the two for a human asking "did that pane publish anything?". It is
/// also what makes the claim on the tool description and the docs page true as
/// written rather than nearly true.
#[test]
fn a_failed_post_is_audited_too_and_says_why() {
    let _serial = capture_lock();
    let (reg, _d) = test_registry();
    let repo = tempfile::tempdir().unwrap();
    let g = reg
        .create_group(&repo.path().to_string_lossy(), Guardrails { max_agents: 8, ..rails() })
        .unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let planner = reg.spawn_agent(&g.id, Role::Planner, "plan", "plan #7", false, None).unwrap();
    let cp = reg.resolve_token(&planner.token).unwrap();

    // A `gh` that cannot be spawned at all: the failure is real and needs no
    // script, and it lands in the same place a network or auth failure would.
    reg.set_gh_exec_override(Some((
        repo.path().join("no-such-gh-binary"),
        Duration::from_secs(20),
    )));

    let err = post_comment(&reg, &cp, json!({ "issue": 7, "body": "a plan" })).unwrap_err();
    assert!(!err.is_empty(), "the caller must be told the post failed");

    let row = reg.audit_log(&g.id).into_iter()
        .find(|e| e.action == "issue-comment")
        .expect("a FAILED post must leave an issue-comment row too — absent reads as \
                 'never attempted', which is a different thing");
    assert_eq!(row.detail["issue"], 7);
    assert_eq!(row.detail["bytes"], 6, "the size it tried to post");
    assert!(row.detail.get("url").is_none(),
        "no URL on a failure — a row that carried an empty one would read as a post that \
         worked and lost its link: {row:?}");
    assert!(row.detail["error"].as_str().is_some_and(|e| !e.is_empty()),
        "and the reason, so the human need not re-run it to find out: {row:?}");

    // The staged body is cleaned up on the failing path too.
    let leftovers = fs::read_dir(reg.state_root().join(g.id.as_str()))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("-comment-body-") && n.ends_with(".md"))
        .count();
    assert_eq!(leftovers, 0, "a failed post must not leave its staging file behind");

    reg.set_gh_exec_override(None);
}

// ───────── #3040 N2: the pane-level notices go on a diet ─────────

/// **A completed planner's slot-free notice is AUDITED, not announced** (#3040
/// N2), and the audit row keeps the whole text.
///
/// The demotion rides #533-B's existing path — `audit_demoted_exit_notice`, the
/// `agent-exit-notice` action, `routed: "audit-only"` — rather than inventing a
/// second way to do the same thing. What makes this the clearest member of that
/// class is the ordering #203 guarantees: the planner's own `report(done)` is
/// the IMMEDIATELY preceding prompt in the recipient's pane, so the notice tells
/// the orchestrator a second time what it has just read. #3040's census found it
/// acted on zero times out of ten.
///
/// The pin is deliberately two-sided, because either half alone passes for the
/// wrong reason: "the pane got nothing" is satisfied by a notice that was
/// dropped, and "the audit row exists" is satisfied by a notice that was
/// delivered as well. Both, plus the full text, is what "read it on demand"
/// means.
#[test]
fn a_planner_exit_is_audited_not_announced() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let planner = reg
        .spawn_agent(&g.id, Role::Planner, "plan", "plan issue #7", false, None)
        .unwrap();
    // Paused so a delivery is queued-and-audited and therefore observable at
    // all: test mode has no pane to type into. The pause changes nothing
    // upstream of delivery, which is what makes it a probe rather than a
    // different code path — and it is what makes the absence assertion below
    // mean "nothing was sent", not "nothing could be seen".
    pause_with_pane(&reg, &g.id, &orch.id, 6209);

    let before = delivered_texts(&reg, &g.id).len();
    reg.close_completed_planner(&planner.id);

    // Half 1: the pane got nothing.
    let after = delivered_texts(&reg, &g.id);
    assert!(
        !after[before..].iter().any(|t| t.contains("posted its plan and exited")),
        "the slot-free notice reached the orchestrator's pane: {:?}",
        &after[before..]
    );
    assert!(
        !after[before..].iter().any(|t| t.contains("slot is free")),
        "…in any spelling: {:?}",
        &after[before..]
    );

    // Half 2: the audit row exists, WITH the full text — the control that this
    // is a demotion and not a deletion. Without it the assertions above would be
    // satisfied by a notice that had simply been dropped on the floor.
    let rows = audit_entries(&reg, &g.id, "agent-exit-notice");
    let row = rows
        .iter()
        .find(|e| e["detail"]["agent"] == json!(planner.id))
        .unwrap_or_else(|| panic!("no agent-exit-notice row for the planner: {rows:?}"));
    assert_eq!(row["detail"]["routed"], json!("audit-only"),
        "the row must say WHY it is on the audit log rather than in a pane: {row}");
    assert_eq!(row["detail"]["initiator"], json!("planner-completed"),
        "…and record what really caused the exit, not a borrowed initiator: {row}");
    let notice = row["detail"]["notice"].as_str().expect("the row carries the notice text");
    assert!(notice.contains("posted its plan and exited"), "the full text is kept: {notice}");
    assert!(notice.contains("its delegate slot is free."), "…to its last clause: {notice}");
    assert!(notice.contains(&planner.id), "…naming the pane, which is what a reader needs: {notice}");

    // And the roster half #533-B leans on: `list_agents` really does carry the
    // liveness, so "read it on demand" is not a euphemism.
    let roster = reg.list_agents(&g.id);
    let dead = roster
        .as_array()
        .expect("the roster is a JSON array")
        .iter()
        .any(|a| a["id"] == json!(planner.id) && a["status"] == json!("dead"));
    assert!(dead, "the roster must show the pane gone (#533-B's own argument): {roster:?}");
}

/// **A stall on a pane something already asked to exit is not news** (#3040 N2)
/// — it is suppressed, with the reason on the audit row.
///
/// The decision is routed through `exit_notice_route` rather than by listing
/// initiators here, so the two answers cannot drift: an EXIT whose notice #533-B
/// judged not worth the orchestrator's turn cannot have a STALL notice about the
/// same pane that is. The pane is still `Running` at this point — a kill request
/// is recorded before the pty catches up — which is exactly the window in which
/// the watchdog would otherwise fire about a pane that is on its way out.
#[test]
fn a_stall_on_a_pane_already_told_to_exit_is_suppressed_with_a_reason() {
    let (reg, _d, gid, wid) = watchdog_setup(5);
    reg.record_exit_initiator(&wid, ExitInitiator::Orchestrator);

    let notified = reg.watchdog_tick(FAR, &HashMap::new(), &HashMap::new());
    assert!(notified.is_empty(), "a pane on its way out must not be announced: {notified:?}");

    let rows = audit_entries(&reg, &gid, "watchdog-suppressed");
    assert_eq!(rows.len(), 1, "the suppression must be diagnosable, not silent: {rows:?}");
    assert_eq!(rows[0]["detail"]["agent"], json!(wid));
    assert_eq!(rows[0]["detail"]["why"], json!("exit-initiated"),
        "the row must say WHICH of the three reasons this was: {}", rows[0]);
    // No stall notice was audited either — the two rows are alternatives, and a
    // run that wrote both would mean the stall was announced after all.
    assert!(audit_entries(&reg, &gid, "watchdog-stall").is_empty(),
        "a suppressed stall must not also be audited as a delivered one");

    // The anti-nag latch is set on this path too: a suppression is spoken about
    // once per stall, exactly as a notice is. Without this the audit log grows a
    // row every 30-second tick for the whole life of the stall.
    let _ = reg.watchdog_tick(FAR + 60_000, &HashMap::new(), &HashMap::new());
    assert_eq!(audit_entries(&reg, &gid, "watchdog-suppressed").len(), 1,
        "one suppression per stall, not one per tick");
}

/// The CONTROL for the two suppression tests: an ordinary stall on a pane nobody
/// has asked to exit and no drive owns still announces, and is audited as a
/// stall rather than a suppression.
///
/// It is what makes the `is_empty()` assertions above mean something. A change
/// that suppressed EVERY stall — the failure mode a diet invites — satisfies
/// both of them and fails this one.
#[test]
fn a_stall_on_an_undriven_pane_still_announces() {
    let (reg, _d, gid, wid) = watchdog_setup(5);
    assert_eq!(reg.watchdog_tick(FAR, &HashMap::new(), &HashMap::new()), vec![wid.clone()],
        "an ordinary stall is still the orchestrator's business");

    // Observed on the audit row rather than through `delivered_texts`, because
    // `watchdog_setup` binds no pane: the `watchdog-stall` row is written on the
    // delivery path and nowhere else, so its presence is the same fact.
    assert_eq!(audit_entries(&reg, &gid, "watchdog-stall").len(), 1,
        "…and the delivery path really ran");
    assert!(audit_entries(&reg, &gid, "watchdog-suppressed").is_empty(),
        "nothing suppressed this one");
}

/// The pure decision, at every crossing of its two inputs (#3040 N2).
///
/// `watchdog_suppress_reason` is the only place the two new reasons are decided,
/// and the ORDER matters for more than tidiness: `is_driven` costs a file read
/// under another lock, so it must not be consulted once the cheaper answer has
/// already decided. That is asserted here by a closure that PANICS — the one
/// shape that can fail if the short-circuit is ever removed.
#[test]
fn the_watchdog_suppression_reason_covers_every_crossing_of_its_two_inputs() {
    use loomux_lib::orchestration::watchdog_suppress_reason as why;

    // Nobody asked for this pane to end, and no drive owns it: NEWS.
    assert_eq!(why(None, || false), None);
    // A drive owns it: the driver's own `lane-stalled` hold is the real signal.
    assert_eq!(why(None, || true), Some("driven-lane"));
    // Something in this process asked for it to end — and every initiator
    // `exit_notice_route` demotes is demoted here too, by construction rather
    // than by a second list.
    for init in [
        ExitInitiator::Orchestrator,
        ExitInitiator::IdleTimeout,
        ExitInitiator::DriverRelease,
        ExitInitiator::LeadExit,
        ExitInitiator::PlannerCompleted,
    ] {
        assert_eq!(why(Some(init), || panic!("is_driven must not be consulted once the initiator has decided")),
            Some("exit-initiated"), "{init:?}");
        assert_eq!(exit_notice_route(Some(init)), ExitNoticeRoute::AuditOnly,
            "fixture: {init:?} must be one exit_notice_route demotes, or this row proves nothing");
    }
}
