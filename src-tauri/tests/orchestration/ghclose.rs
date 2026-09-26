//! The PR-close ownership gate.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ─────────────────── #2985: the PR-close ownership gate ───────────────────

#[test]
fn gh_close_action_detects_close_and_reopen_and_the_delete_branch_flag() {
    let a = |v: &[&str]| gh_close_action(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(a(&["pr", "close", "7"]), Some(("close".into(), false)));
    assert_eq!(a(&["pr", "close", "--delete-branch", "7"]), Some(("close".into(), true)));
    assert_eq!(a(&["pr", "close", "7", "-d"]), Some(("close".into(), true)), "-d is gh's own shorthand");
    assert_eq!(a(&["pr", "reopen", "7"]), Some(("reopen".into(), false)));
    // -R/--repo before or between the command tokens, exactly like the merge path.
    assert_eq!(a(&["-R", "o/r", "pr", "close", "7"]), Some(("close".into(), false)));
    assert_eq!(a(&["pr", "-R", "o/r", "close", "7"]), Some(("close".into(), false)));
    // `gh pr merge --delete-branch` is NOT this gate's business — it is the
    // orchestrator's documented post-merge step and stays under the merge gate.
    assert_eq!(a(&["pr", "merge", "--delete-branch", "7"]), None, "merge is not a close");
    assert_eq!(a(&["pr", "view", "7"]), None, "a read-only pr subcommand is not a close");
    assert_eq!(a(&["issue", "close", "7"]), None, "closing an ISSUE is not closing a PR");
    assert_eq!(a(&[]), None);
}

#[test]
fn a_branch_is_owned_only_by_itself_or_by_a_separated_descendant() {
    let o = gh_branch_is_owned;
    assert!(o("fix/2985-x", "fix/2985-x"), "your own branch");
    assert!(o("fix/2985-x-scratch2", "fix/2985-x"), "a `-` scratch branch beneath it");
    assert!(o("fix/2985-x/wip", "fix/2985-x"), "a `/` scratch branch beneath it");
    // THE NEGATIVE CONTROL this rule exists for. A BARE prefix test passes every
    // row above AND this one — which is another worker's branch. The incident
    // closed five PRs belonging to other workers, so a rule that cannot tell a
    // sibling from a descendant would be the same defect wearing a guard's name.
    assert!(!o("fix/2985-x", "fix/29"), "a bare prefix is NOT ownership: fix/29 owns nothing of fix/2985-x");
    assert!(!o("feat/other", "fix/2985-x"), "an unrelated branch");
    // THE RESIDUAL, pinned rather than glossed. The separator rule is about
    // where a prefix ENDS, not about who the branches belong to, so an agent
    // whose own branch is a strict prefix of another's *up to a separator* does
    // own it: `fix/2985` owns `fix/2985-other`. That is the accepted cost of
    // "same branch prefix" (issue #2985's own wording) rather than exact-match,
    // and it is bounded by what actually mints these names — orrerix cuts a
    // worker's branch from the ISSUE it is working, so two live workers whose
    // branches nest that way are two workers on one issue, which is the case
    // where the looser reading is wanted. Pinned as a passing row so that a
    // later narrowing to exact-match reddens here and has to argue for itself,
    // instead of silently changing what the design note claims.
    assert!(o("fix/2985-other", "fix/2985"), "a separated descendant is owned even when the parent is short — the accepted residual");
    // …and the bound on it: one character short of a separator is NOT ownership,
    // which is what keeps the row above a residual rather than a hole.
    assert!(!o("fix/2985other", "fix/2985"), "no separator, no ownership");
    // An empty own-branch owns NOTHING. Without this, every role with no branch
    // (orchestrator, planner, reviewer-without-worktree) would own every branch
    // in the repo by empty-prefix match — the widest possible failure, reached
    // by the commonest possible roster row.
    assert!(!o("fix/2985-x", ""), "no recorded branch owns nothing");
    assert!(!o("", "fix/2985-x"), "an unresolved head is owned by nobody");
    assert!(!o("", ""));
    // Not a prefix in the other direction either.
    assert!(!o("fix/2985", "fix/2985-x"), "the parent of your branch is not yours");
}

#[test]
fn gh_close_decision_refuses_a_close_the_caller_cannot_be_shown_to_own() {
    let d = gh_close_decision;
    // Not a close/reopen at all → the shim runs the real gh untouched. This is
    // the POSITIVE CONTROL for the whole gate: `gh pr view` must stay a
    // pass-through, or the guard has broken every agent's read path.
    assert_eq!(d(None, "worker", "fix/a", Some("fix/a")), GhCloseGate::PassThrough);
    // A reopen is never refused — it destroys nothing, and the incident's own
    // remediation was a reopen loop. Audited, not gated.
    assert_eq!(d(Some("reopen"), "worker", "fix/a", Some("fix/zzz")), GhCloseGate::AllowReopen);
    assert_eq!(d(Some("reopen"), "", "", None), GhCloseGate::AllowReopen, "a reopen needs no ownership proof at all");
    // The owner closes its own PR, with or without --delete-branch (which is
    // not an input to this decision: see `gh_close_decision`'s doc).
    assert_eq!(d(Some("close"), "worker", "fix/a", Some("fix/a")), GhCloseGate::AllowOwner);
    assert_eq!(d(Some("close"), "worker", "fix/a", Some("fix/a-scratch")), GhCloseGate::AllowOwner);
    // THE INCIDENT. A worker closing another worker's PR is refused.
    assert_eq!(d(Some("close"), "worker", "fix/a", Some("fix/b")), GhCloseGate::BlockNotOwner);
    assert_eq!(d(Some("close"), "reviewer", "", Some("fix/b")), GhCloseGate::BlockNotOwner, "a reviewer with no branch owns no PR");
    assert_eq!(d(Some("close"), "planner", "", Some("fix/b")), GhCloseGate::BlockNotOwner);
    // The orchestrator may close any PR in its group — its authority is over the
    // GROUP, not over a branch, so it does not even need the head ref resolved.
    assert_eq!(d(Some("close"), "orchestrator", "", Some("fix/b")), GhCloseGate::AllowOrchestrator);
    assert_eq!(d(Some("close"), "orchestrator", "", None), GhCloseGate::AllowOrchestrator);
    // Fail-closed on either half of the ownership question being unanswerable.
    assert_eq!(d(Some("close"), "worker", "fix/a", None), GhCloseGate::BlockUnverifiable, "gh could not tell us the head ref");
    assert_eq!(d(Some("close"), "worker", "fix/a", Some("")), GhCloseGate::BlockUnverifiable);
    assert_eq!(d(Some("close"), "", "", Some("fix/a")), GhCloseGate::BlockUnverifiable, "no roster row for this pane");
}

#[test]
fn the_close_refusals_each_render_as_one_paragraph_and_name_what_the_agent_needs() {
    // The house idiom (CLAUDE.md): a `\` line continuation strips the newline
    // AND the source indentation. A collapsed continuation leaves the run of
    // leading spaces with no newline at all, which no `.contains(…)` assertion
    // straddles — so the SHAPE is pinned beside the content.
    let one_para = |m: &str| !m.contains('\n') && !m.contains("          ");
    for (what, msg) in [
        ("refusal, no delete", gh_close_refusal("2942", "fix/other", "fix/mine", "w-2", false)),
        ("refusal, --delete-branch", gh_close_refusal("2942", "fix/other", "fix/mine", "w-2", true)),
        ("refusal, no own branch", gh_close_refusal("2942", "fix/other", "", "w-2", true)),
        ("refusal, owner unknown", gh_close_refusal("2942", "fix/other", "fix/mine", "", false)),
        ("unverifiable, no head", gh_close_unverifiable_refusal("2942", false)),
        ("unverifiable, no agent", gh_close_unverifiable_refusal("2942", true)),
    ] {
        assert!(one_para(&msg), "{what} must be one paragraph: {msg:?}");
    }
    // The three facts the incident's worker did not have. All of them, in the
    // message, or the agent has nothing to act on but a retry.
    let m = gh_close_refusal("2942", "fix/other", "fix/mine", "w-2", false);
    assert!(m.contains("#2942"), "names the PR: {m}");
    assert!(m.contains("'fix/other'"), "names the branch it refused: {m}");
    assert!(m.contains("your own branch is 'fix/mine'"), "names what the agent DOES own: {m}");
    // #2985 asks the refusal to name the PR's OWNER, not to guess at one.
    assert!(m.contains("it belongs to w-2"), "names the owning agent: {m}");
    assert!(!m.contains("another agent or to the human"), "the guess is gone: {m}");
    // &and when the roster owns that branch for nobody, it says THAT rather
    // than naming a guess — a branch whose agent has exited, or the human's.
    let u = gh_close_refusal("2942", "fix/other", "fix/mine", "", false);
    assert!(u.contains("no agent on this group's roster owns that branch"), "{u}");
    assert!(!u.contains("belongs to ."), "never an empty owner name: {u}");
    assert!(!m.contains("--delete-branch"), "no delete was asked for, so the message must not claim one: {m}");
    // …and when one WAS asked for, the message says so — that is the shape this
    // incident was one flag away from making unrecoverable.
    let d = gh_close_refusal("2942", "fix/other", "fix/mine", "w-2", true);
    assert!(d.contains("--delete-branch"), "a refused --delete-branch is named: {d}");
    // A pane with no branch is told that, rather than being told about an empty
    // string it would have to guess the meaning of.
    let n = gh_close_refusal("2942", "fix/other", "", "w-2", false);
    assert!(n.contains("no branch of its own recorded"), "{n}");
    assert!(!n.contains("branch is ''"), "never an empty-quoted branch: {n}");
    // The two unverifiable refusals are DIFFERENT sentences: an unresolvable
    // head ref and an unknown pane are different faults with different fixes,
    // and collapsing them would send the agent to debug the wrong one.
    assert_ne!(
        gh_close_unverifiable_refusal("2942", true),
        gh_close_unverifiable_refusal("2942", false),
        "the two unverifiable causes must not render identically"
    );
    assert!(gh_close_unverifiable_refusal("2942", false).contains("head branch"));
    assert!(gh_close_unverifiable_refusal("2942", true).contains("owner roster"));
}

#[test]
fn the_owner_roster_the_shim_reads_is_a_flat_projection_of_the_agent_records() {
    let r = |rows: &[(&str, &str, Option<&str>)]| {
        render_owner_roster(
            &rows
                .iter()
                .map(|(i, ro, b)| (i.to_string(), ro.to_string(), b.map(str::to_string)))
                .collect::<Vec<_>>(),
        )
    };
    assert_eq!(
        r(&[("w-1", "worker", Some("fix/a")), ("o-1", "orchestrator", None)]),
        "w-1 worker fix/a\no-1 orchestrator \n",
        "three space-separated fields per line, branch empty where there is none"
    );
    // A row whose fields would introduce a SECOND separator is dropped, not
    // written half-parsed: the shell reader splits on whitespace, so a branch
    // with a space in it would make the next field read as this one's. A
    // dropped row leaves that agent unidentifiable, and the gate then refuses —
    // the safe direction. (Real branches cannot contain a space; a hand-edited
    // `agents.json` can say anything, and this is the boundary that holds it.)
    assert_eq!(r(&[("w-1", "worker", Some("fix/a b"))]), "", "a whitespace-bearing branch is dropped");
    assert_eq!(r(&[("w 1", "worker", Some("fix/a"))]), "", "…and so is a whitespace-bearing id");
    assert_eq!(r(&[("w-1", "", Some("fix/a"))]), "", "a row with no role identifies nobody");
    assert_eq!(r(&[]), "", "an empty roster renders empty, not a stray newline");
    // A dropped row must not take its neighbours with it.
    assert_eq!(
        r(&[("w-1", "worker", Some("bad branch")), ("w-2", "worker", Some("fix/b"))]),
        "w-2 worker fix/b\n"
    );
}

/// A fake `gh` for the #2985 close gate: it answers `pr view --json
/// headRefName,number` from `$FAKE_HEAD`/`$FAKE_NUM` (the shim's one ownership
/// lookup) and logs every invocation, so a test can tell "the shim refused"
/// from "the shim allowed and the real gh ran".
///
/// An EMPTY `$FAKE_HEAD` exits NON-ZERO with no output, which is what a 404 /
/// no-such-PR / offline gh looks like to the shim — the fail-closed input.
fn write_fake_gh_head(root: &std::path::Path, log: &std::path::Path) -> std::path::PathBuf {
    let p = root.join("fakegh_head");
    std::fs::write(&p, format!(
        "#!/bin/sh\n\
         echo \"ARGS: $*\" >> \"{log}\"\n\
         if [ \"$1\" = \"pr\" ] && [ \"$2\" = \"view\" ] && [ -n \"$FAKE_HEAD\" ]; then\n\
         \x20 printf '%s %s\\n' \"$FAKE_HEAD\" \"$FAKE_NUM\"; exit 0\n\
         fi\n\
         if [ \"$1\" = \"pr\" ] && [ \"$2\" = \"view\" ]; then exit 1; fi\n\
         printf 'FAKE-GH-RAN\\n'; exit 0\n",
        log = log.display()
    )).unwrap();
    p
}

/// The #2985 close gate, executed as the REAL generated shim against a fake gh.
///
/// The four cases the guard is defined by — a refusal, the owner allowed, the
/// orchestrator allowed, and a plain `gh pr view` still passing through — plus
/// the audit rows, which are the half the incident had none of.
#[test]
fn gh_shim_harness_refuses_a_close_of_a_pr_the_caller_does_not_own() {
    use std::process::Command;
    if Command::new("sh").arg("-c").arg("exit 0").status().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("SKIP gh_shim_harness_refuses_a_close…: no POSIX sh");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    let log = root.join("gh.log");
    let fake = write_fake_gh_head(root, &log);
    let shim = root.join("gh");
    std::fs::write(&shim, gh_shim_sh(&fake.display().to_string(), &shim_paths())).unwrap();
    let _ = Command::new("sh").arg("-c").arg(format!("chmod +x '{}' '{}'", fake.display(), shim.display())).status();

    // The roster the backend projects out of `agents.json` — written here by the
    // very function that writes it in production, so this harness cannot pass
    // against a format the product does not emit.
    std::fs::write(
        group.join(OWNER_ROSTER_FILE),
        render_owner_roster(&[
            ("w-1".into(), "worker".into(), Some("fix/mine".into())),
            ("w-2".into(), "worker".into(), Some("fix/theirs".into())),
            ("o-1".into(), "orchestrator".into(), None),
            ("r-1".into(), "reviewer".into(), None),
        ]),
    )
    .unwrap();

    // (argv, agent id, the PR's head branch) → (exit ok, stderr)
    let run = |argv: &[&str], aid: &str, head: &str, num: &str| -> (bool, String) {
        let out = Command::new("sh").arg(&shim).args(argv)
            .env("LOOMUX_GROUP_DIR", &group)
            .env("LOOMUX_AGENT_ID", aid)
            .env("FAKE_HEAD", head).env("FAKE_NUM", num)
            .env("FAKE_BASE", "main").env("FAKE_DEFAULT", "main")
            .output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    let audit = || std::fs::read_to_string(group.join("audit.jsonl")).unwrap_or_default();
    let ran = || std::fs::read_to_string(&log).unwrap_or_default();

    // ── 1. THE INCIDENT. w-1 closes a PR whose head is w-2's branch. ──────────
    let (ok, err) = run(&["pr", "close", "2942"], "w-1", "fix/theirs", "2942");
    assert!(!ok, "closing another worker's PR must be refused");
    assert!(err.contains("#2942") && err.contains("'fix/theirs'"), "the refusal names the PR and the branch: {err}");
    assert!(!ran().contains("ARGS: pr close"), "the real gh must never have been reached: {}", ran());
    assert!(audit().contains("pr-close-blocked") && audit().contains("\"agent\":\"w-1\""),
        "the refusal is an audit row carrying the agent id: {}", audit());
    // …and with --delete-branch, which is the shape that would not have been
    // recoverable. Still refused, and the message says the delete was asked for.
    let (ok, err) = run(&["pr", "close", "--delete-branch", "2942"], "w-1", "fix/theirs", "2942");
    assert!(!ok, "--delete-branch on another worker's branch must be refused");
    assert!(err.contains("--delete-branch"), "{err}");

    // ── 2. THE OWNER is allowed — its own branch, and a scratch branch under it.
    std::fs::write(group.join("audit.jsonl"), b"").unwrap();
    std::fs::write(&log, b"").unwrap();
    let (ok, err) = run(&["pr", "close", "7"], "w-1", "fix/mine", "7");
    assert!(ok, "closing your OWN PR must still work: {err}");
    assert!(ran().contains("ARGS: pr close 7"), "the real gh ran: {}", ran());
    assert!(audit().contains("pr-close-allowed"), "an allowed close is a RECORD, not a silence: {}", audit());
    let (ok, err) = run(&["pr", "close", "--delete-branch", "8"], "w-1", "fix/mine-scratch2", "8");
    assert!(ok, "a scratch branch beneath your own is yours, --delete-branch included: {err}");
    // The sibling case, through the real shim: a bare prefix must not own it.
    let (ok, _) = run(&["pr", "close", "9"], "w-1", "fix/mine2", "9");
    assert!(!ok, "fix/mine must NOT own fix/mine2 — no separator, not a descendant");

    // ── 3. THE ORCHESTRATOR may close any PR in its group, still audited. ─────
    std::fs::write(group.join("audit.jsonl"), b"").unwrap();
    let (ok, err) = run(&["pr", "close", "--delete-branch", "2942"], "o-1", "fix/theirs", "2942");
    assert!(ok, "the orchestrator closes any PR in its group: {err}");
    assert!(audit().contains("pr-close-allowed") && audit().contains("\"role\":\"orchestrator\""),
        "…and it is audited with the role that allowed it: {}", audit());

    // ── 4. POSITIVE CONTROL: a plain read-only `gh pr view` is untouched. ─────
    // Without this the suite cannot tell a working gate from one that refuses
    // everything — and a shim that broke `gh pr view` would break every agent.
    std::fs::write(group.join("audit.jsonl"), b"").unwrap();
    std::fs::write(&log, b"").unwrap();
    let (ok, err) = run(&["pr", "view", "2942"], "w-1", "fix/theirs", "2942");
    assert!(ok, "gh pr view must pass through untouched: {err}");
    assert!(err.is_empty(), "…silently: {err}");
    assert!(audit().is_empty(), "a read is not an audit event: {}", audit());
    // `gh pr merge --delete-branch` is likewise not this gate's business — it is
    // the orchestrator's documented post-merge step. Whatever the MERGE gate
    // then decides about it, this gate must not have touched it: no close audit
    // row, and no close refusal on stderr. (Asserted on what THIS gate does,
    // not on the exit status, which belongs to the merge gate and is covered by
    // its own harness tests.)
    std::fs::write(group.join("audit.jsonl"), b"").unwrap();
    let (_, err) = run(&["pr", "merge", "--delete-branch", "2942"], "w-1", "fix/theirs", "2942");
    assert!(!audit().contains("pr-close"), "a merge is never a close: {}", audit());
    assert!(!err.contains("refusing to close"), "the close gate must not intercept a merge: {err}");

    // ── 5. A REOPEN is allowed and audited (#2985 §2). ───────────────────────
    std::fs::write(group.join("audit.jsonl"), b"").unwrap();
    let (ok, err) = run(&["pr", "reopen", "2942"], "w-1", "fix/theirs", "2942");
    assert!(ok, "a reopen destroys nothing and is never refused: {err}");
    assert!(audit().contains("pr-reopen") && audit().contains("\"agent\":\"w-1\""),
        "…but it IS attributed, which is what the incident's audit could not do: {}", audit());

    // ── 6. FAIL-CLOSED, both halves. ─────────────────────────────────────────
    // A pane the roster does not know.
    let (ok, err) = run(&["pr", "close", "7"], "ghost", "fix/mine", "7");
    assert!(!ok, "an unidentifiable caller cannot close anything");
    assert!(err.contains("owner roster"), "{err}");
    // A head ref gh cannot report (empty FAKE_HEAD → the fake exits non-zero).
    let (ok, err) = run(&["pr", "close", "7"], "w-1", "", "7");
    assert!(!ok, "an unresolvable head ref cannot be shown to be yours");
    assert!(err.contains("head branch"), "{err}");
    // And with the group dir unset — the evasion shape the merge gate refuses too.
    let out = Command::new("sh").arg(&shim).args(["pr", "close", "7"])
        .env_remove("LOOMUX_GROUP_DIR").env_remove("ORRERIX_GROUP_DIR")
        .env("LOOMUX_AGENT_ID", "w-1").env("FAKE_HEAD", "fix/mine").env("FAKE_NUM", "7")
        .output().unwrap();
    assert!(!out.status.success(), "a close this app cannot audit is a close it cannot allow");
}

/// The agent id of the one non-dead pane running `session`, off the roster the
/// orchestrator reads (#3442's fixture helper).
fn live_pane_on(reg: &OrchRegistry, group: &GroupId, session: &str) -> String {
    let panes: Vec<String> = reg
        .list_agents(group)
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["session"] == json!(session) && a["status"] != json!("dead"))
        .map(|a| a["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(panes.len(), 1, "exactly one live pane on session {session}: {panes:?}");
    panes[0].clone()
}

/// **#3442: a resumed worker closes its own scratch PR, and still nobody
/// else's.** The close gate (#2985/#3198) decides from the `agent_owners`
/// roster, and a pane opened by `spawn_agent(resume_session:)` used to be
/// written there with NO branch — so the gate owned nothing for it and
/// refused the worker's own `<branch>-scratchN` close. The review driver
/// resumes the worker on every hand-back, so that was every worker past
/// round 1.
///
/// Executed end to end: the worker is spawned and resumed through the real MCP
/// tool (worktree cut in a real repo), the roster is the file the backend
/// wrote, and the close runs through the real generated shim against a fake
/// gh. The close comes FIRST, so the red at the base is the refusal itself.
#[test]
fn a_resumed_worker_can_close_its_own_scratch_pr_and_nobody_elses() {
    use std::process::Command;
    if Command::new("sh").arg("-c").arg("exit 0").status().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("SKIP a_resumed_worker_can_close_its_own_scratch_pr…: no POSIX sh");
        return;
    }
    let (reg, _d) = test_registry();
    let repo = real_repo();
    let mut r = rails();
    r.max_agents = 8;
    let g = reg.create_group(&repo.path().to_string_lossy(), r).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();
    let call = |args: Value| {
        let out = dispatch(&reg, &co, "tools/call", &json!({ "name": "spawn_agent", "arguments": args }))
            .unwrap();
        assert_eq!(out["isError"], false, "{out:?}");
    };
    let session_of = |id: &str| reg.agent(id).unwrap().session_id.expect("claude mints a session id");

    // Two workers on two branches. `mine` is the one resumed; `theirs` stays
    // live, so its branch has a real owner on the roster.
    call(json!({ "kind": "worker", "branch": "fix/3442-mine", "task": "round 1" }));
    let mine = reg.list_agents(&g.id).as_array().unwrap().iter()
        .find(|a| a["role"] == "worker").map(|a| a["id"].as_str().unwrap().to_string()).unwrap();
    call(json!({ "kind": "worker", "branch": "fix/3442-theirs", "task": "other work" }));
    let sess = session_of(&mine);
    assert_eq!(
        reg.agent(&mine).unwrap().branch.as_deref(),
        Some("fix/3442-mine"),
        "fixture: the ORIGINAL pane records its branch — without that there is nothing to inherit"
    );

    // Round 2: the pane ends and the session is resumed the documented way —
    // resume_session and a follow-up, nothing else.
    reg.mark_dead(&mine, Some(0));
    call(json!({ "resume_session": sess, "task": "round 2 findings" }));
    let resumed = live_pane_on(&reg, &g.id, &sess);
    assert_ne!(resumed, mine, "the resume opened a NEW pane — that is the pane under test");

    // The shim, deciding from the roster the backend wrote for this group.
    let td = tempfile::tempdir().unwrap();
    let log = td.path().join("gh.log");
    let fake = write_fake_gh_head(td.path(), &log);
    let shim = td.path().join("gh");
    std::fs::write(&shim, gh_shim_sh(&fake.display().to_string(), &shim_paths())).unwrap();
    let _ = Command::new("sh").arg("-c").arg(format!("chmod +x '{}' '{}'", fake.display(), shim.display())).status();
    let gdir = reg.state_root().join(g.id.as_str());
    let close = |aid: &str, head: &str| -> (bool, String) {
        let out = Command::new("sh").arg(&shim).args(["pr", "close", "--delete-branch", "77"])
            .env("LOOMUX_GROUP_DIR", &gdir).env("LOOMUX_AGENT_ID", aid)
            .env("FAKE_HEAD", head).env("FAKE_NUM", "77")
            .output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
    };

    // ── THE DEFECT: the resumed worker closes its own scratch PR. ────────────
    let (ok, err) = close(&resumed, "fix/3442-mine-scratch1");
    assert!(
        ok,
        "a RESUMED worker must be able to close its own -scratchN PR — the gate refused it, \
         which is #3442 (the resumed pane has no recorded branch): {err}"
    );
    assert!(std::fs::read_to_string(&log).unwrap_or_default().contains("ARGS: pr close"),
        "the real gh ran");
    // …its own branch itself, likewise.
    let (ok, err) = close(&resumed, "fix/3442-mine");
    assert!(ok, "the resumed worker owns its own branch: {err}");

    // ── Still nobody else's. ─────────────────────────────────────────────────
    let (ok, err) = close(&resumed, "fix/3442-theirs");
    assert!(!ok, "a resumed worker must not close another worker's PR");
    assert!(err.contains("your own branch is 'fix/3442-mine'"), "refused as a pane WITH a branch: {err}");
    let (ok, _) = close(&resumed, "fix/3442-theirs-scratch1");
    assert!(!ok, "…nor another worker's scratch PR");
    // The separator rule survives inheritance: a bare prefix owns nothing.
    let (ok, _) = close(&resumed, "fix/3442-mine2");
    assert!(!ok, "fix/3442-mine must not own fix/3442-mine2");

    // What the gate read: the resumed pane's row carries the inherited branch.
    let roster = std::fs::read_to_string(gdir.join(OWNER_ROSTER_FILE)).unwrap();
    assert!(
        roster.lines().any(|l| l == format!("{resumed} worker fix/3442-mine")),
        "the resumed pane's owner-roster row names its session's branch: {roster}"
    );
    assert_eq!(reg.agent(&resumed).unwrap().branch.as_deref(), Some("fix/3442-mine"));

    // A SECOND resume inherits it too — the round-3 pane, resumed from a
    // session whose newest row is itself a resume.
    reg.mark_dead(&resumed, Some(0));
    call(json!({ "resume_session": sess, "task": "round 3 findings" }));
    let third = live_pane_on(&reg, &g.id, &sess);
    let (ok, err) = close(&third, "fix/3442-mine-scratch2");
    assert!(ok, "a resume of a resume still owns the session's branch: {err}");
}

/// **#3442's upgrade case: a newer roster row carrying `branch: None` does not
/// take the branch away.** Every resume written before this fix left such a row
/// — the session's minting row names the branch, every later row names nothing
/// — so a resume on a running install meets exactly this roster. The branch is
/// read off the session's FIRST row (`session_identity_record`); a
/// last-touched rule would read the legacy row and hand the pane nothing.
///
/// The legacy row is written by hand, with the newest `updated_ms` on the
/// roster, and that ordering is asserted as a pre-condition: without a newer
/// row that DISAGREES with the minting one, the first-row and newest-row rules
/// answer alike and this test could not tell them apart (review round 1, N1).
#[test]
fn a_resume_inherits_the_minting_rows_branch_past_a_newer_legacy_row() {
    let (reg, _d) = test_registry();
    let repo = real_repo();
    let mut r = rails();
    r.max_agents = 8;
    let g = reg.create_group(&repo.path().to_string_lossy(), r).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();
    let call = |args: Value| {
        let out = dispatch(&reg, &co, "tools/call", &json!({ "name": "spawn_agent", "arguments": args }))
            .unwrap();
        assert_eq!(out["isError"], false, "{out:?}");
    };

    call(json!({ "kind": "worker", "branch": "fix/3442-legacy", "task": "round 1" }));
    let mint = reg.list_agents(&g.id).as_array().unwrap().iter()
        .find(|a| a["role"] == "worker").map(|a| a["id"].as_str().unwrap().to_string()).unwrap();
    let sess = reg.agent(&mint).unwrap().session_id.expect("claude mints a session id");
    reg.mark_dead(&mint, Some(0));

    // The pre-#3442 resume's row: same session, same class, NO branch, and
    // touched after the minting row.
    let path = reg.state_root().join(g.id.as_str()).join("agents.json");
    let mut rows: Vec<Value> = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let newest = rows.iter().filter_map(|r| r["updated_ms"].as_u64()).max().unwrap();
    let mut legacy = rows.iter().find(|r| r["id"] == json!(mint)).cloned().expect("the minting row");
    assert_eq!(legacy["branch"], json!("fix/3442-legacy"), "fixture: the minting row names the branch");
    legacy["id"] = json!("w-legacy");
    legacy["branch"] = Value::Null;
    legacy["status"] = json!("dead");
    legacy["updated_ms"] = json!(newest + 60_000);
    rows.push(legacy);
    fs::write(&path, serde_json::to_string_pretty(&rows).unwrap()).unwrap();
    let on_disk: Vec<Value> = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let naming: Vec<&Value> = on_disk.iter().filter(|r| r["session"] == json!(sess)).collect();
    assert_eq!(naming.len(), 2, "fixture: two rows name the session");
    assert_eq!(
        (naming[0]["branch"].clone(), naming[1]["branch"].clone()),
        (json!("fix/3442-legacy"), Value::Null),
        "fixture: the FIRST row names the branch and the later one does not"
    );
    assert!(
        naming[1]["updated_ms"].as_u64() > naming[0]["updated_ms"].as_u64(),
        "fixture: the legacy row is the NEWEST, so a last-touched rule would pick it"
    );

    call(json!({ "resume_session": sess, "task": "round 2 findings" }));
    let resumed = live_pane_on(&reg, &g.id, &sess);
    assert_eq!(
        reg.agent(&resumed).unwrap().branch.as_deref(),
        Some("fix/3442-legacy"),
        "a resume must inherit the MINTING row's branch; a newer pre-#3442 row carrying none \
         must not strip it (that is every worker already resumed on a running install)"
    );
}

/// **#3442's fail-closed arms: a resume inherits a branch only where one was
/// recorded, and only as the class that recorded it.**
///
/// A resumed planner genuinely has no branch (a planner never gets one), so it
/// must still own NOTHING — the empty-`own` refusal the close gate keeps. And a
/// worker's session resumed as a REVIEWER by an explicit `kind` is not that
/// worker continuing its own work, so it inherits no branch either.
#[test]
fn a_resume_inherits_no_branch_where_none_was_recorded_or_the_class_changed() {
    use std::process::Command;
    let (reg, _d) = test_registry();
    let repo = real_repo();
    let mut r = rails();
    r.max_agents = 8;
    let g = reg.create_group(&repo.path().to_string_lossy(), r).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();
    let call = |args: Value| {
        let out = dispatch(&reg, &co, "tools/call", &json!({ "name": "spawn_agent", "arguments": args }))
            .unwrap();
        assert_eq!(out["isError"], false, "{out:?}");
    };
    let first_of = |role: &str| {
        reg.list_agents(&g.id).as_array().unwrap().iter()
            .find(|a| a["role"] == role).map(|a| a["id"].as_str().unwrap().to_string()).unwrap()
    };

    // ── A planner: no branch at mint, none on resume. ────────────────────────
    call(json!({ "kind": "planner", "task": "plan it" }));
    let planner = first_of("planner");
    assert_eq!(reg.agent(&planner).unwrap().branch, None, "fixture: a planner records no branch");
    let psess = reg.agent(&planner).unwrap().session_id.unwrap();
    reg.mark_dead(&planner, Some(0));
    call(json!({ "resume_session": psess, "task": "revise" }));
    let p2 = live_pane_on(&reg, &g.id, &psess);
    assert_eq!(reg.agent(&p2).unwrap().branch, None, "a resumed planner still has no branch");

    // ── A worker's session resumed as a reviewer. ────────────────────────────
    call(json!({ "kind": "worker", "branch": "fix/3442-w", "task": "work" }));
    let worker = first_of("worker");
    assert_eq!(
        reg.agent(&worker).unwrap().branch.as_deref(),
        Some("fix/3442-w"),
        "fixture: the minting worker HAS a branch, so the refusal below is the class check \
         and not an empty row"
    );
    let wsess = reg.agent(&worker).unwrap().session_id.unwrap();
    reg.mark_dead(&worker, Some(0));
    call(json!({ "kind": "reviewer", "resume_session": wsess, "task": "look at it" }));
    let rev = live_pane_on(&reg, &g.id, &wsess);
    assert_eq!(reg.agent(&rev).unwrap().role, Role::Reviewer, "fixture: the resume really changed class");
    assert_eq!(
        reg.agent(&rev).unwrap().branch,
        None,
        "a worker's session resumed as another class must not inherit the worker's branch"
    );

    // Both, through the gate: each owns nothing, and is told so.
    if Command::new("sh").arg("-c").arg("exit 0").status().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("SKIP the shim half of a_resume_inherits_no_branch…: no POSIX sh");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let log = td.path().join("gh.log");
    let fake = write_fake_gh_head(td.path(), &log);
    let shim = td.path().join("gh");
    std::fs::write(&shim, gh_shim_sh(&fake.display().to_string(), &shim_paths())).unwrap();
    let _ = Command::new("sh").arg("-c").arg(format!("chmod +x '{}' '{}'", fake.display(), shim.display())).status();
    let gdir = reg.state_root().join(g.id.as_str());
    for aid in [&p2, &rev] {
        let out = Command::new("sh").arg(&shim).args(["pr", "close", "77"])
            .env("LOOMUX_GROUP_DIR", &gdir).env("LOOMUX_AGENT_ID", aid.as_str())
            .env("FAKE_HEAD", "fix/3442-w-scratch1").env("FAKE_NUM", "77")
            .output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(!out.status.success(), "{aid} owns no branch and must not close fix/3442-w-scratch1");
        assert!(err.contains("no branch of its own recorded"), "refused as a branchless pane: {err}");
    }
}

/// The owner the refusal NAMES is the roster's most specific match, not its
/// first (#3206). Two rows can both match a head by the separated-descendant
/// rule — a `fix/team` holder and a `fix/team-alpha` holder both match head
/// `fix/team-alpha-2` — and a first-match lookup names whichever row comes
/// first in the roster, which need not be the roster's closest match.
/// "Closest", not "true owner": the roster cannot know who actually pushed a
/// descendant branch — the human can push one beneath another row's prefix —
/// so the longest match names the best the roster HAS, and the refusal
/// decision is the same either way (a non-owner's close is refused). This
/// pins the NAME: the LONGEST matching roster branch wins.
#[test]
fn gh_shim_names_the_longest_matching_roster_branch_as_the_pr_owner() {
    use std::process::Command;
    if Command::new("sh").arg("-c").arg("exit 0").status().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("SKIP gh_shim_names_the_longest_matching…: no POSIX sh");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    let log = root.join("gh.log");
    let fake = write_fake_gh_head(root, &log);
    let shim = root.join("gh");
    std::fs::write(&shim, gh_shim_sh(&fake.display().to_string(), &shim_paths())).unwrap();
    let _ = Command::new("sh").arg("-c").arg(format!("chmod +x '{}' '{}'", fake.display(), shim.display())).status();

    // TWO rows the descendant rule accepts for the same head: w-2's
    // `fix/team-alpha` is itself a separated descendant of w-1's `fix/team`,
    // so head `fix/team-alpha-2` matches BOTH. w-1 is written FIRST, so a
    // first-match lookup names w-1 — the wrong agent. The caller is w-9,
    // whose own branch owns nothing here, so the close is refused and the
    // message must name the owner.
    std::fs::write(
        group.join(OWNER_ROSTER_FILE),
        render_owner_roster(&[
            ("w-1".into(), "worker".into(), Some("fix/team".into())),
            ("w-2".into(), "worker".into(), Some("fix/team-alpha".into())),
            ("w-9".into(), "worker".into(), Some("fix/other".into())),
        ]),
    )
    .unwrap();

    let out = Command::new("sh").arg(&shim).args(["pr", "close", "2942"])
        .env("LOOMUX_GROUP_DIR", &group).env("LOOMUX_AGENT_ID", "w-9")
        .env("FAKE_HEAD", "fix/team-alpha-2").env("FAKE_NUM", "2942")
        .output().unwrap();
    assert!(!out.status.success(), "a non-owner's close is still refused");
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(err.contains("it belongs to w-2"), "the longest matching roster branch is the owner named: {err}");
    assert!(!err.contains("it belongs to w-1"), "the shorter prefix's row must not be named instead: {err}");
    // The decision half is untouched: still refused, still an audit row.
    let audit = std::fs::read_to_string(group.join("audit.jsonl")).unwrap_or_default();
    assert!(audit.contains("pr-close-blocked") && audit.contains("\"reason\":\"not-owner\""),
        "only the name changes, never the decision: {audit}");
}

/// The shim's refusal is the string Rust builds — executed, not inspected.
///
/// `gh_shim_close_gate` generates the sentence by CALLING `gh_close_refusal_with`
/// with the shell's own variable names, so the template cannot drift; this runs
/// the real script and compares its stderr to `gh_close_refusal`, which pins the
/// INTERPOLATION as well — that the shell fills `$c_pr`/`$c_head`/`$c_branch`
/// with the same three values the Rust function is given.
#[test]
fn the_close_refusal_the_shim_prints_is_the_one_rust_builds() {
    use std::process::Command;
    if Command::new("sh").arg("-c").arg("exit 0").status().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("SKIP the_close_refusal_the_shim_prints…: no POSIX sh");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    let log = root.join("gh.log");
    let fake = write_fake_gh_head(root, &log);
    let shim = root.join("gh");
    std::fs::write(&shim, gh_shim_sh(&fake.display().to_string(), &shim_paths())).unwrap();
    let _ = Command::new("sh").arg("-c").arg(format!("chmod +x '{}' '{}'", fake.display(), shim.display())).status();
    std::fs::write(
        group.join(OWNER_ROSTER_FILE),
        render_owner_roster(&[
            ("w-1".into(), "worker".into(), Some("fix/mine".into())),
            // w-2 owns fix/theirs, so the shim resolves an OWNER for the head
            // these rows refuse — the parity assertions below pass "w-2" to the
            // Rust builder, and a roster without this row would make the shim
            // say "nobody owns it" while Rust named an agent.
            ("w-2".into(), "worker".into(), Some("fix/theirs".into())),
            ("r-1".into(), "reviewer".into(), None),
        ]),
    )
    .unwrap();

    let stderr = |argv: &[&str], aid: &str, head: &str, num: &str| -> String {
        let out = Command::new("sh").arg(&shim).args(argv)
            .env("LOOMUX_GROUP_DIR", &group).env("LOOMUX_AGENT_ID", aid)
            .env("FAKE_HEAD", head).env("FAKE_NUM", num)
            .output().unwrap();
        String::from_utf8_lossy(&out.stderr).trim_end().to_string()
    };

    // A worker with a branch of its own, no --delete-branch.
    assert_eq!(
        stderr(&["pr", "close", "2942"], "w-1", "fix/theirs", "2942"),
        gh_close_refusal("2942", "fix/theirs", "fix/mine", "w-2", false)
    );
    // …with --delete-branch (the extra sentence is generated too).
    assert_eq!(
        stderr(&["pr", "close", "-d", "2942"], "w-1", "fix/theirs", "2942"),
        gh_close_refusal("2942", "fix/theirs", "fix/mine", "w-2", true)
    );
    // A role with NO branch of its own — the other own-clause arm.
    assert_eq!(
        stderr(&["pr", "close", "2942"], "r-1", "fix/theirs", "2942"),
        gh_close_refusal("2942", "fix/theirs", "", "w-2", false)
    );
    // Both unverifiable arms.
    assert_eq!(
        stderr(&["pr", "close", "2942"], "ghost", "fix/theirs", "2942"),
        gh_close_unverifiable_refusal("2942", true)
    );
    assert_eq!(
        stderr(&["pr", "close", "2942"], "w-1", "", "2942"),
        gh_close_unverifiable_refusal("2942", false)
    );
    // The PR the message names is the RESOLVED number, never the raw selector —
    // on the incident's own path the selector was a wrong number built by string
    // concatenation, and echoing it back would have confirmed the mistake.
    let m = stderr(&["pr", "close", "https://github.com/o/r/pull/2942"], "w-1", "fix/theirs", "2942");
    assert!(m.contains("#2942"), "the resolved number, not the URL: {m}");
    assert!(!m.contains("https://"), "{m}");
}

/// The Rust writer and the shell reader must agree about ONE path. A textual
/// pin, because they are two programs: nothing the compiler checks connects
/// `OWNER_ROSTER_FILE` to the string baked into the generated script.
#[test]
fn the_shim_reads_the_owner_roster_the_backend_writes() {
    let sh = gh_shim_sh("/usr/bin/gh", &shim_paths());
    let want = format!("\"$ORX_GD/{OWNER_ROSTER_FILE}\"");
    assert!(sh.contains(&want), "the shim must read {want}");
    // Non-vacuity: a `contains` on a name is only evidence if the gate is really
    // in the script. (An absence-only or name-only assertion passes just as well
    // against a shim that never grew the gate at all.)
    assert!(sh.contains("THE PR-CLOSE OWNERSHIP GATE"), "the gate is in the generated shim");
    assert!(sh.contains("pr-close-blocked") && sh.contains("pr-close-allowed") && sh.contains("pr-reopen"),
        "all three audit actions are emitted");
}

/// #2985 rev-std finding 1: the shim's shell positional scanner must consume a
/// `-c`/`--comment` VALUE, or the close gate decides ownership about the wrong PR.
///
/// The reviewer's reproduction, as a suite test. `gh pr close -c "7" 2942` was
/// scanned as `sel="7"` — the comment's value — so the gate resolved PR 7,
/// checked ownership of PR 7's head (`fix/mine`, which the caller DOES own),
/// allowed it, and the real gh then closed PR **2942**, which the caller does
/// not own. Fail-open, and the audit row recorded the wrong PR. The sibling
/// direction fails closed wrongly: `--comment "why" 2942` resolved nothing.
///
/// The fix is structural — the shell arms are generated from `GH_VALUE_FLAGS` —
/// so this test also asserts the GENERATED list covers every entry of that
/// const, which is what stops the next flag being added to one copy only.
#[test]
fn the_close_gate_resolves_the_pr_being_closed_not_a_comment_value() {
    use std::process::Command;
    if Command::new("sh").arg("-c").arg("exit 0").status().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("SKIP the_close_gate_resolves_the_pr_being_closed…: no POSIX sh");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    let log = root.join("gh.log");
    // A fake gh that resolves a DIFFERENT head per PR number, which is what makes
    // "which PR did the gate decide about" observable at all: PR 7 is the
    // caller's own, PR 2942 is another worker's.
    let fake = root.join("fakegh_bynum");
    std::fs::write(&fake, format!(
        "#!/bin/sh\n\
         echo \"ARGS: $*\" >> \"{log}\"\n\
         if [ \"$1\" = \"pr\" ] && [ \"$2\" = \"view\" ]; then\n\
         \x20 _n=\"\"\n\
         \x20 for a in \"$@\"; do case \"$a\" in [0-9]*) _n=$a ;; esac; done\n\
         \x20 case \"$_n\" in\n\
         \x20   7) printf 'fix/mine 7\\n'; exit 0 ;;\n\
         \x20   2942) printf 'fix/theirs 2942\\n'; exit 0 ;;\n\
         \x20   *) exit 1 ;;\n\
         \x20 esac\n\
         fi\n\
         printf 'FAKE-GH-RAN\\n'; exit 0\n",
        log = log.display()
    )).unwrap();
    let shim = root.join("gh");
    std::fs::write(&shim, gh_shim_sh(&fake.display().to_string(), &shim_paths())).unwrap();
    let _ = Command::new("sh").arg("-c").arg(format!("chmod +x '{}' '{}'", fake.display(), shim.display())).status();
    std::fs::write(
        group.join(OWNER_ROSTER_FILE),
        render_owner_roster(&[
            ("w-1".into(), "worker".into(), Some("fix/mine".into())),
            ("w-2".into(), "worker".into(), Some("fix/theirs".into())),
        ]),
    )
    .unwrap();

    let run = |argv: &[&str]| -> (bool, String, String) {
        std::fs::write(group.join("audit.jsonl"), b"").unwrap();
        let out = Command::new("sh").arg(&shim).args(argv)
            .env("LOOMUX_GROUP_DIR", &group).env("LOOMUX_AGENT_ID", "w-1")
            .output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            std::fs::read_to_string(group.join("audit.jsonl")).unwrap_or_default(),
        )
    };

    // The control: plainly closing 2942 is refused, and the audit says 2942.
    let (ok, err, audit) = run(&["pr", "close", "2942"]);
    assert!(!ok, "control: closing another worker's PR is refused");
    assert!(audit.contains("\"pr\":\"2942\""), "control audit names 2942: {audit}");
    assert!(err.contains("#2942"), "{err}");

    // THE DEFECT, in all the forms gh accepts for the flag. Each must still be
    // refused, and the audit must still say 2942 — the gate decided about the PR
    // being closed, not about the comment's text.
    for argv in [
        &["pr", "close", "-c", "7", "2942"][..],
        &["pr", "close", "--comment", "7", "2942"][..],
        &["pr", "close", "--comment=7", "2942"][..],
        &["pr", "close", "-c", "7", "--delete-branch", "2942"][..],
    ] {
        let (ok, err, audit) = run(argv);
        assert!(!ok, "a comment value must never be read as the PR selector: {argv:?}");
        assert!(audit.contains("\"pr\":\"2942\""),
            "the gate must decide about the PR being closed, not the comment: {argv:?} → {audit}");
        assert!(!audit.contains("\"pr\":\"7\""), "{argv:?} → {audit}");
        assert!(err.contains("#2942") && err.contains("fix/theirs"), "{argv:?} → {err}");
    }

    // The sibling direction: a legitimate close of one's OWN PR with a comment
    // must still be ALLOWED, not refused as unverifiable. Without this row the
    // test above would pass against a shim that refused everything.
    let (ok, err, audit) = run(&["pr", "close", "--comment", "done here", "7"]);
    assert!(ok, "closing your own PR with a comment must still work: {err}");
    assert!(audit.contains("pr-close-allowed") && audit.contains("\"pr\":\"7\""), "{audit}");

    // THE STRUCTURAL HALF. The shell arms are generated from `GH_VALUE_FLAGS`, so
    // assert the generated script really carries every entry — this is what stops
    // the next flag being added to the const and not to the shim, which is the
    // divergence that produced the defect above.
    let sh = gh_shim_sh("/usr/bin/gh", &shim_paths());
    for &flag in gh_value_flags() {
        if flag == "-R" || flag == "--repo" {
            continue; // their own capture arms, above the skip arm
        }
        assert!(sh.contains(&format!("{flag}|")) || sh.contains(&format!("{flag})")),
            "the generated shim's value-flag arm is missing {flag}");
        if flag.starts_with("--") {
            assert!(sh.contains(&format!("{flag}=*")), "the generated glued arm is missing {flag}=*");
        }
    }
    // Non-vacuity for that loop: a flag that is NOT in the const must not be
    // there either, or the assertions above would pass against a shim that
    // listed every string in the world.
    assert!(!sh.contains("--not-a-real-gh-flag"), "control");
}

/// #2985 rev-std finding 2: a git ref name may contain `"`, and this gate's own
/// second half is attribution — so an audit row that a branch name can FORGE
/// defeats the half that exists to stop the next orchestrator asking the human.
#[test]
fn a_quote_in_a_branch_name_cannot_forge_an_audit_row() {
    use std::process::Command;
    if Command::new("sh").arg("-c").arg("exit 0").status().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("SKIP a_quote_in_a_branch_name…: no POSIX sh");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    let log = root.join("gh.log");
    let fake = write_fake_gh_head(root, &log);
    let shim = root.join("gh");
    std::fs::write(&shim, gh_shim_sh(&fake.display().to_string(), &shim_paths())).unwrap();
    let _ = Command::new("sh").arg("-c").arg(format!("chmod +x '{}' '{}'", fake.display(), shim.display())).status();
    std::fs::write(
        group.join(OWNER_ROSTER_FILE),
        render_owner_roster(&[("w-1".into(), "worker".into(), Some("fix/mine".into()))]),
    )
    .unwrap();

    // The head ref the attacker controls: closing the JSON string and opening a
    // field of their own. If it reaches audit.jsonl raw, the row claims the
    // orchestrator did this.
    let hostile = r#"x","agent":"o-1","role":"orchestrator"#;
    let out = Command::new("sh").arg(&shim).args(["pr", "close", "2942"])
        .env("LOOMUX_GROUP_DIR", &group).env("LOOMUX_AGENT_ID", "w-1")
        .env("FAKE_HEAD", hostile).env("FAKE_NUM", "2942")
        .output().unwrap();
    assert!(!out.status.success(), "still refused");
    let audit = std::fs::read_to_string(group.join("audit.jsonl")).unwrap_or_default();

    // Every row is still ONE well-formed JSON object, and the agent it names is
    // the one that really called.
    assert!(!audit.is_empty(), "the refusal is audited at all (non-vacuity)");
    for line in audit.lines().filter(|l| !l.trim().is_empty()) {
        // The DETAIL object, parsed on its own rather than the whole row.
        //
        // Not a convenience: the row's `ts_ms` used to be written by
        // `date +%s%3N`, and `%3N` is a GNU extension that BSD `date` emits
        // LITERALLY - so on macOS every audit row either shim wrote before #3202
        // ended up with `"ts_ms":<seconds>3N`, which is not valid JSON. That was
        // a real, PRE-EXISTING defect (in the audit functions of both shims), it
        // was not what this test is about, and fixing it belonged in its own
        // change rather than riding in on a close-gate PR - so it was filed as
        // #3202 and scoped around here rather than silently absorbed. #3202 is
        // now fixed: every rendered shim timestamps with the self-launch shim's
        // all-digit fallback, pinned by
        // every_rendered_shim_timestamps_with_the_portable_ms_fallback. Parsing
        // the detail object still decides this test's question completely,
        // because every value the branch name could reach lives inside it.
        let detail = line
            .find("\"detail\":")
            .map(|i| &line[i + "\"detail\":".len()..])
            .and_then(|rest| rest.strip_suffix('}'))
            .unwrap_or_else(|| panic!("audit row has no detail object: {line}"));
        let v: serde_json::Value = serde_json::from_str(detail)
            .unwrap_or_else(|e| panic!("audit detail is not valid JSON ({e}): {detail}"));
        assert!(line.contains("\"actor\":\"gh-shim\""), "{line}");
        assert_eq!(v["agent"], "w-1", "the row must name the real caller: {line}");
        assert_ne!(v["role"], "orchestrator", "a branch name must not forge the role: {line}");
        // The forged field must not exist at all - not merely hold a wrong value.
        assert!(v.get("agent").is_some() && v["agent"] == "w-1", "{line}");
        assert_eq!(v.as_object().map(|o| o.keys().filter(|k| *k == "agent").count()), Some(1),
            "exactly one agent field, not a smuggled second one: {line}");
    }
    // The quote is gone from the recorded value rather than escaped into it.
    assert!(!audit.contains(r#"\""#), "no escaped quotes smuggled through: {audit}");

    // Non-vacuity: the SAME shim, with a harmless head, still records that head —
    // so the assertions above are about sanitising, not about a gate that
    // happens to write nothing.
    std::fs::write(group.join("audit.jsonl"), b"").unwrap();
    let _ = Command::new("sh").arg(&shim).args(["pr", "close", "2942"])
        .env("LOOMUX_GROUP_DIR", &group).env("LOOMUX_AGENT_ID", "w-1")
        .env("FAKE_HEAD", "fix/theirs").env("FAKE_NUM", "2942")
        .output().unwrap();
    let audit = std::fs::read_to_string(group.join("audit.jsonl")).unwrap_or_default();
    assert!(audit.contains("\"head\":\"fix/theirs\""), "a harmless head is recorded intact: {audit}");
}

/// #2985 rev-std finding 4 / the issue's own words: "refuse with the PR's owner
/// named". Through the real shim, so the roster lookup is exercised and not just
/// the sentence builder.
#[test]
fn the_refusal_names_the_agent_that_owns_the_branch() {
    use std::process::Command;
    if Command::new("sh").arg("-c").arg("exit 0").status().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("SKIP the_refusal_names_the_agent…: no POSIX sh");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    let log = root.join("gh.log");
    let fake = write_fake_gh_head(root, &log);
    let shim = root.join("gh");
    std::fs::write(&shim, gh_shim_sh(&fake.display().to_string(), &shim_paths())).unwrap();
    let _ = Command::new("sh").arg("-c").arg(format!("chmod +x '{}' '{}'", fake.display(), shim.display())).status();
    std::fs::write(
        group.join(OWNER_ROSTER_FILE),
        render_owner_roster(&[
            ("w-1".into(), "worker".into(), Some("fix/mine".into())),
            ("w-2".into(), "worker".into(), Some("fix/theirs".into())),
        ]),
    )
    .unwrap();
    let stderr = |head: &str| -> String {
        let out = Command::new("sh").arg(&shim).args(["pr", "close", "2942"])
            .env("LOOMUX_GROUP_DIR", &group).env("LOOMUX_AGENT_ID", "w-1")
            .env("FAKE_HEAD", head).env("FAKE_NUM", "2942")
            .output().unwrap();
        String::from_utf8_lossy(&out.stderr).trim_end().to_string()
    };

    // The roster owns it: name the agent.
    let m = stderr("fix/theirs");
    assert!(m.contains("it belongs to w-2"), "names the owning agent: {m}");
    assert_eq!(m, gh_close_refusal("2942", "fix/theirs", "fix/mine", "w-2", false), "parity with Rust");
    // A scratch branch BENEATH another agent's is owned by that agent too — the
    // owner lookup uses the same rule the decision does, not an exact match.
    let m = stderr("fix/theirs-scratch1");
    assert!(m.contains("it belongs to w-2"), "the owner rule matches descendants too: {m}");
    // Nobody on the roster owns it: say so rather than name a guess.
    let m = stderr("release/v9");
    assert!(m.contains("no agent on this group's roster owns that branch"), "{m}");
    assert_eq!(m, gh_close_refusal("2942", "release/v9", "fix/mine", "", false), "parity with Rust");
}

// ---------------------------------------------------------------------------
